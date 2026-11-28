//! The backend's first words: where it is, its `hello`, and what counts as "no backend here".
//!
//! - **Where it is** (design 6.1): at the root of the music server's own origin, under
//!   [`ROOT`], whatever path the music server itself is under. Every address an app sends is
//!   one of the constants here: always with the slash after `nexus`, never with a query, and
//!   never ending in the name of a picture, a script or a style sheet (a proxy may send those
//!   somewhere else and keep the answer).
//! - **`hello`** (design 12.1): `GET /nexus/api/hello` answers
//!   `{"nexus":"0.2.0","id":"…","speaks":[1,1],"has":["accounts"]}`. [`Hello::read`] reads it,
//!   checking every byte and every field before anything is made of it.
//! - **Known by its status and its body together, never by a header** (design 6.1 and 12.1,
//!   as the lead has ruled; experiment E1): a music server left to answer under `/nexus/`
//!   sends a redirect to its own web app, or "method not allowed", not "not found".
//!   [`classify`] says "this is the backend" only for status 200 with a complete body that is
//!   a valid `hello`. Any other status is "no backend here" whatever the body; no header is
//!   required or trusted; a redirect is never followed.
//! - **The complete body or nothing**: a body is only ever read as a [`Body`], which can only
//!   be made by reading the transport's reader to its end under [`HELLO_MAX_BYTES`].
//! - **The plain form of an address** (design 6.1, experiment E2): the proxy chooses where to
//!   send a request by the tidied path and hands on the untidied one, so the backend itself
//!   refuses, unread, any request target that [`plain`] does not pass. The apps never send
//!   such a thing.
//!
//! **Wire rules this sets, beyond the design's words** (each a constant or a function here,
//! and each one that a later backend or app must keep, or the older side refuses):
//!
//! - A path of the backend's is at most [`TARGET_MAX_BYTES`], and each segment after
//!   `/nexus/` is spelled with lower-case ASCII letters, digits, `-` and `_` alone. So no
//!   path has a dot in it (`/nexus/v1.2` is refused), and none a `;`.
//! - A `hello` is at most [`HELLO_MAX_BYTES`], nested at most [`HELLO_MAX_DEPTH`] deep, with
//!   no key twice in any object of it. `has` holds at most [`HAS_MAX`] names, each of at most
//!   [`HAS_NAME_MAX_BYTES`], beginning with a lower-case letter and spelled with lower-case
//!   letters, digits and `_`: a name like `playlists-v2`, or a 33rd name, makes an older app
//!   refuse the whole `hello`.

use std::fmt;
use std::io::Read;

/// Everything of the backend's is under this, at the root of the music server's origin.
/// With the slash: without it a proxy's rule also catches `/nexusx`.
pub const ROOT: &str = "/nexus/";
/// The backend's calls are under this.
pub const API: &str = "/nexus/api/";
/// `GET`: what the backend is and what it does. Answers anyone and needs nothing.
pub const HELLO: &str = "/nexus/api/hello";

/// Every path the backend has, so that a test can hold each to the rules above.
pub const PATHS: &[&str] = &[HELLO];

/// What a backend that accepts account sign-in says in [`Hello::has`].
pub const HAS_ACCOUNTS: &str = "accounts";
/// What a backend that keeps plays says in [`Hello::has`].
pub const HAS_PLAYS: &str = "plays";

/// The most of an answer to `hello` that is read. A real `hello` is a tenth of this; a larger
/// answer is not the backend's.
pub const HELLO_MAX_BYTES: usize = 4096;
/// How deep a `hello` may nest arrays and objects, counting its own object as the first.
pub const HELLO_MAX_DEPTH: usize = 16;
/// The longest request target in plain form. Every path above is a fraction of this.
pub const TARGET_MAX_BYTES: usize = 256;
/// The most names `has` may carry, and the longest of them.
pub const HAS_MAX: usize = 32;
pub const HAS_NAME_MAX_BYTES: usize = 32;

// ---------------------------------------------------------------------------------------
// The plain form of a request target
// ---------------------------------------------------------------------------------------

/// Why a request target is not in plain form. Each is final: the request is refused unread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotPlain {
    /// Empty, or longer than [`TARGET_MAX_BYTES`].
    Length,
    /// A backslash, which some servers take for a slash.
    Backslash,
    /// A percent-escape: `%2f`, `%2e`, `%6e`, `%20`, `%00` and every other.
    Escape,
    /// A query (`?`) or a fragment (`#`). The backend's addresses have neither, ever.
    Query,
    /// Two slashes together, anywhere.
    DoubledSlash,
    /// A `.` or `..` segment, anywhere.
    DotSegment,
    /// Not beginning `/nexus/`: another path, another case, the absolute form, `*`.
    Outside,
    /// A byte in a segment that is not a lower-case letter, a digit, `-` or `_`. Every path
    /// of the backend's is spelled with those alone; so this is also what refuses a name with
    /// an extension (`app.js`, `x.png`), a `;`, a space, a control byte and anything beyond
    /// ASCII.
    Byte,
}

/// Whether `target`, a request target exactly as it arrived, is in plain form: it begins
/// `/nexus/`; it has no `.` or `..` segment, no percent-escape, no doubled slash, no
/// backslash, no query and no fragment; and each segment is spelled with lower-case ASCII
/// letters, digits, `-` and `_` alone. A slash may end it (`/nexus/` itself is in plain form).
///
/// The checks are made in the order of [`NotPlain`]'s variants, and the first that fails is
/// the reason given.
pub fn plain(target: &str) -> Result<(), NotPlain> {
    let bytes = target.as_bytes();
    if bytes.is_empty() || bytes.len() > TARGET_MAX_BYTES {
        return Err(NotPlain::Length);
    }
    if bytes.contains(&b'\\') {
        return Err(NotPlain::Backslash);
    }
    if bytes.contains(&b'%') {
        return Err(NotPlain::Escape);
    }
    if bytes.iter().any(|byte| matches!(byte, b'?' | b'#')) {
        return Err(NotPlain::Query);
    }
    if target.contains("//") {
        return Err(NotPlain::DoubledSlash);
    }
    if target.split('/').any(|segment| segment == "." || segment == "..") {
        return Err(NotPlain::DotSegment);
    }
    let Some(rest) = target.strip_prefix(ROOT) else {
        return Err(NotPlain::Outside);
    };
    let spelled = |byte: &u8| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'/');
    if !rest.as_bytes().iter().all(spelled) {
        return Err(NotPlain::Byte);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// hello
// ---------------------------------------------------------------------------------------

/// The backend's id: made at random when its store is first made, and the same for as long
/// as that store lasts (design 6.3). Not secret. Written as 32 lower-case hex digits.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BackendId([u8; 16]);

impl BackendId {
    /// The id made from 16 random bytes, which whoever makes it hands in.
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        BackendId(bytes)
    }

    /// The id written as 32 lower-case hex digits, and nothing else. Anything else is refused:
    /// upper case too, so that one id has one spelling.
    pub fn from_hex(text: &str) -> Option<Self> {
        let text = text.as_bytes();
        if text.len() != 32 {
            return None;
        }
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        let mut bytes = [0u8; 16];
        for (index, pair) in text.chunks_exact(2).enumerate() {
            bytes[index] = (digit(pair[0])? << 4) | digit(pair[1])?;
        }
        Some(BackendId(bytes))
    }

    pub fn to_hex(&self) -> String {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut text = String::with_capacity(32);
        for byte in self.0 {
            text.push(DIGITS[usize::from(byte >> 4)] as char);
            text.push(DIGITS[usize::from(byte & 15)] as char);
        }
        text
    }

    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for BackendId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BackendId({})", self.to_hex())
    }
}

/// The backend's own version, as `MAJOR.MINOR.PATCH`, each any number from 0 to
/// `u32::MAX`. Shown, never used to decide anything: [`Speaks`] is what says whether two
/// sides understand each other. Every `Version` that can be made is written as text that
/// [`Version::parse`] reads back as the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// Exactly three numbers with a dot between each: digits only, no sign, no leading zero
    /// (but `0` itself), each no more than `u32::MAX`, nothing before or after.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let mut number = || -> Option<u32> {
            let part = parts.next()?;
            if part.is_empty() || part.len() > 10 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            if part.len() > 1 && part.starts_with('0') {
                return None;
            }
            part.parse().ok()
        };
        let version = Version { major: number()?, minor: number()?, patch: number()? };
        parts.next().is_none().then_some(version)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The lowest and highest number of the backend's words that a side understands (design
/// 12.1). Both at least 1, the lowest not above the highest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Speaks {
    low: u32,
    high: u32,
}

impl Speaks {
    pub fn new(low: u32, high: u32) -> Option<Self> {
        (low >= 1 && low <= high).then_some(Speaks { low, high })
    }

    pub fn low(&self) -> u32 {
        self.low
    }

    pub fn high(&self) -> u32 {
        self.high
    }

    /// Whether this side understands `number`.
    pub fn understands(&self, number: u32) -> bool {
        self.low <= number && number <= self.high
    }
}

/// What this version of the library speaks: the numbers it was written to.
pub const SPEAKS: Speaks = Speaks { low: 1, high: 1 };

/// The backend's answer to `hello`, every field checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub nexus: Version,
    pub id: BackendId,
    pub speaks: Speaks,
    /// What this backend does, by name. A name this version doesn't know is kept, and simply
    /// never offered (design 12.1: an app offers only what the backend has).
    has: Vec<String>,
    /// What Diagnostics starts as for a device whose person has not chosen (14.3): the
    /// admin's default for the whole server. Not secret. `None`: not said, which is "on".
    diagnostics: Option<bool>,
}

/// Why a body is not a valid `hello`. Each is "no backend here".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HelloWrong {
    /// Not one JSON object in UTF-8 (every byte of it, in fields this version passes over
    /// too); a key given twice in any object of it, known or not; nested deeper than
    /// [`HELLO_MAX_DEPTH`]; or a known field missing or of the wrong type.
    Form,
    /// `nexus` is not `MAJOR.MINOR.PATCH`.
    Version,
    /// `id` is not 32 lower-case hex digits.
    Id,
    /// `speaks` is not two numbers, each at least 1, the first not above the second.
    Speaks,
    /// `has` holds more than [`HAS_MAX`] names, a name given twice, or a name not spelled
    /// with lower-case letters, digits and `_`, beginning with a letter, of at most
    /// [`HAS_NAME_MAX_BYTES`].
    Has,
}

/// The known fields as they were found, before they are checked.
#[derive(Default)]
struct Fields {
    nexus: Option<String>,
    id: Option<String>,
    speaks: Option<Vec<u32>>,
    has: Option<Vec<String>>,
    diagnostics: Option<bool>,
}

/// Reads one object's keys, refusing any key given twice. Keys are compared as JSON reads
/// them, so `"id"` and `"id"` are the same key.
fn first_time<E: serde::de::Error>(seen: &mut Vec<String>, key: String) -> Result<(), E> {
    if seen.contains(&key) {
        return Err(E::custom("a key given twice"));
    }
    seen.push(key);
    Ok(())
}

/// The whole `hello` object: the four known fields read, every other passed over.
struct Top;

impl<'de> serde::de::Visitor<'de> for Top {
    type Value = Fields;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("one object")
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Fields, A::Error> {
        let mut fields = Fields::default();
        let mut seen = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "nexus" => fields.nexus = Some(map.next_value()?),
                "id" => fields.id = Some(map.next_value()?),
                "speaks" => fields.speaks = Some(map.next_value()?),
                "has" => fields.has = Some(map.next_value()?),
                "diagnostics" => fields.diagnostics = Some(map.next_value()?),
                _ => map.next_value_seed(PassOver(1))?,
            }
            first_time(&mut seen, key)?;
        }
        Ok(fields)
    }
}

/// A value this version doesn't know, passed over: read to its end, kept nowhere, held to
/// [`HELLO_MAX_DEPTH`] and to one of each key in each object inside it.
struct PassOver(usize);

impl<'de> serde::de::DeserializeSeed<'de> for PassOver {
    type Value = ();

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        if self.0 > HELLO_MAX_DEPTH {
            return Err(serde::de::Error::custom("nested too deep"));
        }
        deserializer.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for PassOver {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }

    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }

    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }

    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }

    fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }

    fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
        Ok(())
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        if self.0 >= HELLO_MAX_DEPTH { return Err(serde::de::Error::custom("nested too deep")); }
        while seq.next_element_seed(PassOver(self.0 + 1))?.is_some() {}
        Ok(())
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        if self.0 >= HELLO_MAX_DEPTH { return Err(serde::de::Error::custom("nested too deep")); }
        let mut seen = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            map.next_value_seed(PassOver(self.0 + 1))?;
            first_time(&mut seen, key)?;
        }
        Ok(())
    }
}

fn has_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= HAS_NAME_MAX_BYTES
        && bytes[0].is_ascii_lowercase()
        && bytes.iter().all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

fn has_list(has: Vec<String>) -> Option<Vec<String>> {
    if has.len() > HAS_MAX || !has.iter().all(|name| has_name(name)) {
        return None;
    }
    let mut seen: Vec<&str> = Vec::with_capacity(has.len());
    for name in &has {
        if seen.contains(&name.as_str()) {
            return None;
        }
        seen.push(name);
    }
    Some(has)
}

impl Hello {
    /// The stage-one answer of a backend with account sign-in enabled.
    pub fn for_accounts(nexus: Version, id: BackendId, speaks: Speaks) -> Self {
        Self { nexus, id, speaks, has: vec![HAS_ACCOUNTS.to_owned()], diagnostics: None }
    }

    /// A `hello` for the backend to send. `None` if `has` breaks the rules [`Hello::read`]
    /// holds an answer to, so that the backend cannot send what no app would take.
    pub fn new(nexus: Version, id: BackendId, speaks: Speaks, has: &[&str]) -> Option<Self> {
        let has = has_list(has.iter().map(|name| name.to_string()).collect())?;
        Some(Hello { nexus, id, speaks, has, diagnostics: None })
    }

    /// The same `hello`, saying what Diagnostics starts as on a device whose person has not
    /// chosen (14.3: the admin's default for the whole server).
    pub fn with_diagnostics_default(self, on: bool) -> Self {
        Hello { diagnostics: Some(on), ..self }
    }

    /// What Diagnostics starts as on a device whose person has not chosen, if this backend
    /// said; `None` is "on" (`diag::enabled`). The device's own choice always wins.
    pub fn diagnostics_default(&self) -> Option<bool> {
        self.diagnostics
    }

    /// Reads a complete body as a `hello`: every byte of it UTF-8, one JSON object, no key
    /// given twice, every known field present and right, other fields passed over.
    pub fn read(body: &Body) -> Result<Self, HelloWrong> {
        let text = std::str::from_utf8(&body.0).map_err(|_| HelloWrong::Form)?;
        let mut deserializer = serde_json::Deserializer::from_str(text);
        let fields = serde::Deserializer::deserialize_map(&mut deserializer, Top).map_err(|_| HelloWrong::Form)?;
        deserializer.end().map_err(|_| HelloWrong::Form)?;
        let (Some(nexus), Some(id), Some(speaks), Some(has)) = (fields.nexus, fields.id, fields.speaks, fields.has) else {
            return Err(HelloWrong::Form);
        };
        let nexus = Version::parse(&nexus).ok_or(HelloWrong::Version)?;
        let id = BackendId::from_hex(&id).ok_or(HelloWrong::Id)?;
        let speaks = match speaks[..] {
            [low, high] => Speaks::new(low, high).ok_or(HelloWrong::Speaks)?,
            _ => return Err(HelloWrong::Speaks),
        };
        let has = has_list(has).ok_or(HelloWrong::Has)?;
        Ok(Hello { nexus, id, speaks, has, diagnostics: fields.diagnostics })
    }

    /// The body the backend sends: what [`Hello::read`] reads back as this same `hello`.
    pub fn to_json(&self) -> String {
        let mut hello = serde_json::json!({
            "nexus": self.nexus.to_string(),
            "id": self.id.to_hex(),
            "speaks": [self.speaks.low, self.speaks.high],
            "has": self.has,
        });
        if let (Some(on), Some(object)) = (self.diagnostics, hello.as_object_mut()) {
            object.insert("diagnostics".to_owned(), serde_json::Value::Bool(on));
        }
        hello.to_string()
    }

    /// What this backend does, by name, as it said.
    pub fn has(&self) -> &[String] {
        &self.has
    }

    /// Whether this backend says it does `name`.
    pub fn does(&self, name: &str) -> bool {
        self.has.iter().any(|have| have == name)
    }
}

// ---------------------------------------------------------------------------------------
// Telling the backend from anything else
// ---------------------------------------------------------------------------------------

/// The whole body of an answer, read to its end, and no larger than [`HELLO_MAX_BYTES`]. It
/// can only be made by [`Body::read`], so whatever holds one holds a body that was neither
/// cut short nor cut to fit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Body(Vec<u8>);

/// Why a body was not had whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BodyWrong {
    /// Larger than [`HELLO_MAX_BYTES`]: reading stopped at the first byte past it.
    TooLarge,
    /// Reading it failed before its end.
    Cut,
}

impl Body {
    /// Reads `source` to its end: the transport's own reader for the body, after it has undone
    /// any compression, and **not a copy of the body cut to a size**. Reads at most one byte
    /// past [`HELLO_MAX_BYTES`], so a body of any size costs no more than that.
    pub fn read(source: impl Read) -> Result<Self, BodyWrong> {
        let mut bytes = Vec::new();
        let limit = HELLO_MAX_BYTES as u64 + 1;
        source.take(limit).read_to_end(&mut bytes).map_err(|_| BodyWrong::Cut)?;
        if bytes.len() > HELLO_MAX_BYTES {
            return Err(BodyWrong::TooLarge);
        }
        Ok(Body(bytes))
    }

    pub fn bytes(&self) -> &[u8] {
        &self.0
    }
}

/// Why an answer is not the backend's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoBackend {
    /// A 3xx: a music server left to answer under `/nexus/` redirects to its own web app. A
    /// redirect is an answer, never a place to go.
    Redirect,
    /// Any other status but 200: Navidrome's 405 for a method other than GET, a proxy's 404,
    /// 502 or 403, and every other.
    Status(u16),
    /// A 200 with no body at all.
    Empty,
    /// A 200 with a body larger than [`HELLO_MAX_BYTES`].
    TooLarge,
    /// A 200 whose body could not be read to its end.
    Cut,
    /// A 200 whose body is not a valid `hello`: HTML, the music server's XML or JSON, or a
    /// `hello` wrong in a known field.
    NotHello(HelloWrong),
}

/// What an answer to `hello` says is at the address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Backend(Hello),
    NoBackend(NoBackend),
}

/// What an answer to `GET /nexus/api/hello` says is at the address, from its status and its
/// body alone (design 6.1 and 12.1, as the lead has ruled):
///
/// - **"This is the backend" only for status 200 with a complete body that is a valid
///   `hello`** ([`Hello::read`]).
/// - **Any other status is "no backend here", whatever the body**, and the body is not read.
/// - **No header is required or trusted**: not `Content-Type`, not `Location`.
/// - **A redirect is never followed**: the transport must hand in the 3xx itself, and it is
///   "no backend here".
///
/// `body` is the transport's reader for the body (see [`Body::read`]).
pub fn classify(status: u16, body: impl Read) -> Found {
    let refused = Found::NoBackend;
    if (300..400).contains(&status) {
        return refused(NoBackend::Redirect);
    }
    if status != 200 {
        return refused(NoBackend::Status(status));
    }
    let body = match Body::read(body) {
        Ok(body) => body,
        Err(BodyWrong::TooLarge) => return refused(NoBackend::TooLarge),
        Err(BodyWrong::Cut) => return refused(NoBackend::Cut),
    };
    if body.0.is_empty() {
        return refused(NoBackend::Empty);
    }
    match Hello::read(&body) {
        Ok(hello) => Found::Backend(hello),
        Err(wrong) => refused(NoBackend::NotHello(wrong)),
    }
}

// ---------------------------------------------------------------------------------------
// Where hello is asked
// ---------------------------------------------------------------------------------------

/// Ask at the root of the signed-in music server's canonical HTTPS origin. A base
/// in the backend's namespace cannot have a backend probe of its own. Neither the
/// music query nor its fragment travels to the backend.
pub fn hello_url(music_server: &str) -> Option<String> {
    let address = crate::origin::Address::parse(music_server).ok()?;
    if address.under_nexus() {
        return None;
    }
    Some(format!("{}{HELLO}", address.origin()))
}

#[cfg(test)]
mod tests {
    #[test]
    fn hello_is_asked_at_the_music_servers_origin_and_nowhere_else() {
        let asked = |server: &str| hello_url(server);
        let at = |origin: &str| Some(format!("{origin}/nexus/api/hello"));
        // The origin alone, whatever path the music server is under.
        assert_eq!(asked("https://music.example.org"), at("https://music.example.org"));
        assert_eq!(asked("https://music.example.org/"), at("https://music.example.org"));
        assert_eq!(asked("https://music.example.org/navidrome"), at("https://music.example.org"));
        assert_eq!(asked("https://music.example.org/navidrome/"), at("https://music.example.org"));
        assert_eq!(asked("https://music.example.org/a/b?x=1#y"), at("https://music.example.org"));
        assert_eq!(asked("https://music.example.org?x=1"), at("https://music.example.org"));
        // The old probe trimmed here; the design does not say to keep that exception.
        // The common reader refuses a space anywhere, including around a saved address.
        assert_eq!(asked("  https://music.example.org  "), None);
        // A port; an IPv4 and an IPv6 literal; case.
        assert_eq!(asked("https://music.example.org:4443/navidrome"), at("https://music.example.org:4443"));
        assert_eq!(asked("https://192.0.2.7:4443"), at("https://192.0.2.7:4443"));
        assert_eq!(asked("https://[2001:db8::1]"), at("https://[2001:db8::1]"));
        assert_eq!(asked("https://[2001:db8::1]:4443/music"), at("https://[2001:db8::1]:4443"));
        assert_eq!(asked("HTTPS://Music.Example.ORG"), at("https://music.example.org"));
        // Nothing is asked over plain HTTP, or of anything else.
        for server in [
            "http://music.example.org",
            "http://music.example.org:4533",
            "ftp://music.example.org",
            "music.example.org",
            "https://",
            "https:///navidrome",
            "https://name:secret@music.example.org",
            "https://name@music.example.org",
            "https://music.example.org:0",
            "https://music.example.org:65536",
            "https://music.example.org:",
            "https://music.example.org:44x3",
            "https://music.example.org:1:2",
            "https://[2001:db8::1",
            "https://[2001:db8::1]x",
            "https://[]",
            "https://2001:db8::1",
            "https://music example.org",
            "https://music.example.org\\evil",
            "https://music%2eexample.org",
            "https://music.example.org\u{7f}",
            // A music server that is itself under /nexus cannot have a backend.
            "https://music.example.org/nexus",
            "https://music.example.org/nexus/",
            "https://music.example.org/NEXUS/navidrome",
        ] {
            assert_eq!(asked(server), None, "{server:?}");
        }
        // An internationalised name in its ASCII form; not as typed. A malformed literal.
        assert_eq!(asked("https://xn--bcher-kva.example/music"), at("https://xn--bcher-kva.example"));
        assert_eq!(asked("https://mu_sic.example.org"), at("https://mu_sic.example.org"));
        assert_eq!(asked("https://[2001:0DB8::1]:443"), at("https://[2001:db8::1]"));
        for server in ["https://bücher.example/music", "https://[abc]", "https://[::g]", "https://music..example.org",
                       "https://-music.example.org", "https://music-.example.org", "https://music.example.org.", "https://[::ffff:192.0.2.10]", "https://[::ffff:c000:20a]"] {
            assert_eq!(asked(server), None, "{server:?}");
        }
        // Under a path that merely begins with the letters, it can.
        assert_eq!(asked("https://music.example.org/nexusmusic"), at("https://music.example.org"));
        // Every address made is in plain form at the backend.
        let made = asked("https://music.example.org:4443/navidrome").unwrap();
        assert_eq!(plain(made.trim_start_matches("https://music.example.org:4443")), Ok(()));
    }

    use super::*;

    // ---- the plain form -----------------------------------------------------------------

    /// Who answered a target in experiment E2's routing table (`backend-experiments-report`,
    /// E2.2, variant `plain`, and the `ws-cache-block` variant for the asset names).
    #[derive(Debug, Clone, Copy, PartialEq)]
    enum Reached {
        /// The stand-in backend, with the target exactly as sent.
        AsSent,
        /// The stand-in backend, with the target as the proxy re-wrote it.
        As(&'static str),
        /// The music server or the proxy itself: it never reaches the backend.
        Never,
    }
    use Reached::*;

    /// Every request target in E2.2's table, and E1's that are not already in it: what reached
    /// the backend, and the verdict on what it would see. `Err` is refused unread. (E2.2's rows
    /// that differ only in the method or the `Host` are routing by the proxy, which this
    /// function does not see; the target they all sent is here once.)
    ///
    /// E2 records where each target went, not a verdict; the verdicts are the report's change
    /// 9 ("the backend must itself refuse anything that is not already in normal form ... It
    /// must accept `//nexus/` and the absolute form only as NPM hands them on") and the
    /// design's 6.1. Targets that never reach the backend are still held to [`plain`], since
    /// an app must never send them and a caller that reaches the backend another way might.
    const E2: &[(&str, Reached, Result<(), NotPlain>)] = &[
        ("/nexus/", AsSent, Ok(())),
        ("/nexus/api/hello", AsSent, Ok(())),
        ("/nexus", Never, Err(NotPlain::Outside)), // the proxy's 301 to `/nexus/`
        ("/nexus/x/../api/hello", AsSent, Err(NotPlain::DotSegment)),
        ("/nexus/../rest/ping", Never, Err(NotPlain::DotSegment)),
        ("/nexus/%2e%2e/rest/ping", Never, Err(NotPlain::Escape)),
        ("/nexus/..%2frest/ping", Never, Err(NotPlain::Escape)),
        ("/nexus%2fapi/hello", AsSent, Err(NotPlain::Escape)),
        ("//nexus/api/hello", As("/nexus/api/hello"), Err(NotPlain::DoubledSlash)),
        ("/./nexus/api/hello", AsSent, Err(NotPlain::DotSegment)),
        ("/NEXUS/api/hello", Never, Err(NotPlain::Outside)),
        ("/Nexus/", Never, Err(NotPlain::Outside)),
        ("/%6eexus/api/hello", AsSent, Err(NotPlain::Escape)),
        ("/nexus%2F", AsSent, Err(NotPlain::Escape)),
        ("/nexusx", Never, Err(NotPlain::Outside)),
        ("/nexus.json", Never, Err(NotPlain::Outside)),
        // Passed by the five rules the design names; refused by the spelling every path of
        // the backend's has.
        ("/nexus/;x=1", AsSent, Err(NotPlain::Byte)),
        ("/nexus\\..\\rest\\ping", Never, Err(NotPlain::Backslash)),
        ("/nexus/%00", Never, Err(NotPlain::Escape)), // the proxy's 400
        ("/nexus/a%20b", AsSent, Err(NotPlain::Escape)),
        ("/nexus/api/hello?x=1&t=abc", AsSent, Err(NotPlain::Query)),
        ("/music/nexus/", Never, Err(NotPlain::Outside)),
        ("/rest/ping", Never, Err(NotPlain::Outside)),
        ("/rest/../nexus/api/hello", AsSent, Err(NotPlain::DotSegment)),
        ("/rest/%2e%2e/nexus/api/hello", AsSent, Err(NotPlain::Escape)),
        ("/app/../nexus/", AsSent, Err(NotPlain::DotSegment)),
        // The measured absolute form (E2.2), to the test set-up's own made-up name.
        ("http://music.test/nexus/api/hello", As("/nexus/api/hello"), Err(NotPlain::DoubledSlash)),
        // These four rows are the `plain` variant, with the host's Cache Assets and Block
        // Common Exploits off, as design 6.2 has them. With those boxes on (variant
        // `ws-cache-block`) none of the four reached the backend: the asset names went to the
        // music server and into the proxy's cache, and both queries got the proxy's 403. No
        // path of the backend's ends so, or has a query.
        ("/nexus/app.js", AsSent, Err(NotPlain::Byte)),
        ("/nexus/x.png", AsSent, Err(NotPlain::Byte)),
        ("/nexus/?q=union+select+(1)", AsSent, Err(NotPlain::Query)),
        ("/nexus/?u=http://x", AsSent, Err(NotPlain::Query)),
        // Sent with POST, PUT, DELETE, OPTIONS and PATCH; the method is the backend's to
        // refuse, not the target's.
        ("/nexus/api/prove/begin", AsSent, Ok(())),
        // E1's, under a music server with no rule for `/nexus/`: there they reached the music
        // server alone, so `Reached` is what E2's proxy, rule in place, did with each.
        ("/nexus/ws", AsSent, Ok(())),
        ("/NEXUS/", Never, Err(NotPlain::Outside)),
        ("//nexus/", As("/nexus/"), Err(NotPlain::DoubledSlash)),
        ("/nexus%2fapi", AsSent, Err(NotPlain::Escape)),
        ("/%6eexus/", AsSent, Err(NotPlain::Escape)),
        ("/app/nexus/", Never, Err(NotPlain::Outside)),
        ("/rest/nexus", Never, Err(NotPlain::Outside)),
        ("/api/nexus", Never, Err(NotPlain::Outside)),
        ("/share/nexus", Never, Err(NotPlain::Outside)),
    ];

    #[test]
    fn every_target_the_experiments_tried_is_refused_or_passed_as_measured() {
        for &(target, reached, verdict) in E2 {
            assert_eq!(plain(target), verdict, "as sent: {target}");
            let arrived = match reached {
                AsSent => Some(target),
                As(arrived) => Some(arrived),
                Never => None,
            };
            if let Some(arrived) = arrived {
                let expected = if let As(_) = reached { Ok(()) } else { verdict };
                assert_eq!(plain(arrived), expected, "as it arrived: {arrived} (sent {target})");
            }
        }
    }

    #[test]
    fn nothing_that_reached_the_backend_untidied_is_passed_but_what_the_apps_send() {
        // Of everything that reached the stand-in as it was sent, only the backend's own
        // spellings pass.
        let passed: Vec<&str> = E2
            .iter()
            .filter(|(_, reached, _)| *reached == AsSent)
            .map(|(target, _, _)| *target)
            .filter(|target| plain(target).is_ok())
            .collect();
        assert_eq!(passed, ["/nexus/", "/nexus/api/hello", "/nexus/api/prove/begin", "/nexus/ws"]);
    }

    #[test]
    fn each_rule_on_its_own() {
        let cases: &[(&str, Result<(), NotPlain>)] = &[
            ("", Err(NotPlain::Length)),
            ("*", Err(NotPlain::Outside)),
            ("nexus/api/hello", Err(NotPlain::Outside)),
            ("/nexus/api/hello/", Ok(())),
            ("/nexus/api/plays/changes", Ok(())),
            ("/nexus/api/a-b_c9", Ok(())),
            ("/nexus/api//hello", Err(NotPlain::DoubledSlash)),
            ("/nexus/api/hello//", Err(NotPlain::DoubledSlash)),
            ("/nexus/.", Err(NotPlain::DotSegment)),
            ("/nexus/..", Err(NotPlain::DotSegment)),
            ("/nexus/api/./hello", Err(NotPlain::DotSegment)),
            ("/nexus/api/.hello", Err(NotPlain::Byte)),
            ("/nexus/api/hello.", Err(NotPlain::Byte)),
            ("/nexus/api/...", Err(NotPlain::Byte)),
            ("/nexus/api/hello.css", Err(NotPlain::Byte)),
            ("/nexus/api/Hello", Err(NotPlain::Byte)),
            ("/nexus/api/hel lo", Err(NotPlain::Byte)),
            ("/nexus/api/hel\tlo", Err(NotPlain::Byte)),
            ("/nexus/api/hel\0lo", Err(NotPlain::Byte)),
            ("/nexus/api/héllo", Err(NotPlain::Byte)),
            ("/nexus/api/hello;jsessionid=1", Err(NotPlain::Byte)),
            ("/nexus/api/hello?", Err(NotPlain::Query)),
            ("/nexus/api/hello#top", Err(NotPlain::Query)),
            ("/nexus/api/%68ello", Err(NotPlain::Escape)),
            ("/nexus/api/hello%", Err(NotPlain::Escape)),
            ("/nexus\\api\\hello", Err(NotPlain::Backslash)),
            ("/nexus/api/hello\\", Err(NotPlain::Backslash)),
            ("https://music.example.org/nexus/api/hello", Err(NotPlain::DoubledSlash)),
        ];
        for &(target, verdict) in cases {
            assert_eq!(plain(target), verdict, "{target:?}");
        }
        let longest = format!("{ROOT}{}", "a".repeat(TARGET_MAX_BYTES - ROOT.len()));
        assert_eq!(plain(&longest), Ok(()));
        assert_eq!(plain(&format!("{longest}a")), Err(NotPlain::Length));
    }

    #[test]
    fn the_backends_own_paths_are_in_plain_form_and_go_nowhere_else() {
        // The proxy's assets rule (`conf.d/include/assets.conf`, E2.2): a path ending in one
        // of these may be sent to the music server and kept for half an hour.
        const ASSETS: &[&str] = &[
            "css", "js", "jpg", "jpeg", "gif", "png", "webp", "woff", "woff2", "eot", "ttf",
            "svg", "ico", "map",
        ];
        assert!(ROOT.ends_with('/') && API.starts_with(ROOT));
        for path in PATHS.iter().chain([&ROOT, &API]) {
            assert_eq!(plain(path), Ok(()), "{path}");
            assert!(path.starts_with(ROOT) && !path.contains('?'), "{path}");
            for extension in ASSETS {
                assert!(!path.ends_with(&format!(".{extension}")), "{path}");
            }
        }
        assert!(PATHS.iter().all(|path| path.starts_with(API)));
    }

    // ---- hello ----------------------------------------------------------------------------

    const ID: &str = "0123456789abcdef0123456789abcdef";

    fn hello_json(nexus: &str, id: &str, speaks: &str, has: &str) -> String {
        format!(r#"{{"nexus":{nexus},"id":{id},"speaks":{speaks},"has":{has}}}"#)
    }

    fn good() -> String {
        hello_json(r#""0.1.0""#, &format!("\"{ID}\""), "[1,1]", r#"["plays"]"#)
    }

    /// The design's `hello` with `extra` put in before its last brace, as bytes.
    fn good_with(extra: &[u8]) -> Vec<u8> {
        let mut bytes = good().into_bytes();
        let last = bytes.pop();
        assert_eq!(last, Some(b'}'));
        bytes.extend_from_slice(extra);
        bytes.push(b'}');
        bytes
    }

    fn read(bytes: &[u8]) -> Result<Hello, HelloWrong> {
        Hello::read(&Body::read(bytes).expect("a body within the bound"))
    }

    #[test]
    fn an_account_backends_hello_identifies_that_capability() {
        let version = Version::parse("0.2.0").unwrap();
        let id = BackendId::from_hex(ID).unwrap();
        let hello = Hello::for_accounts(version, id, SPEAKS);
        let body = Body::read(hello.to_json().as_bytes()).unwrap();
        let decoded = Hello::read(&body).unwrap();
        assert_eq!(decoded.has(), &[HAS_ACCOUNTS]);
        assert!(decoded.does(HAS_ACCOUNTS));
    }

    #[test]
    fn the_diagnostics_default_is_said_in_hello_and_absent_is_not_said() {
        let version = Version::parse("0.2.0").unwrap();
        let id = BackendId::from_hex(ID).unwrap();
        let plain = Hello::for_accounts(version, id, SPEAKS);
        assert_eq!(plain.diagnostics_default(), None);
        assert!(!plain.to_json().contains("diagnostics"));
        assert_eq!(read(plain.to_json().as_bytes()).unwrap().diagnostics_default(), None);
        for on in [true, false] {
            let said = Hello::for_accounts(version, id, SPEAKS).with_diagnostics_default(on);
            let back = read(said.to_json().as_bytes()).unwrap();
            assert_eq!(back.diagnostics_default(), Some(on));
            assert_eq!(back, said);
        }
        assert_eq!(read(&good_with(br#","diagnostics":false"#)).unwrap().diagnostics_default(), Some(false));
        // Not a yes or no, or said twice: not a hello.
        assert_eq!(read(&good_with(br#","diagnostics":"off""#)), Err(HelloWrong::Form));
        assert_eq!(read(&good_with(br#","diagnostics":true,"diagnostics":false"#)), Err(HelloWrong::Form));
    }

    #[test]
    fn the_designs_hello_is_read_and_every_field_is_kept() {
        let hello = read(good().as_bytes()).unwrap();
        assert_eq!(hello.nexus, Version { major: 0, minor: 1, patch: 0 });
        assert_eq!(hello.id.to_hex(), ID);
        assert_eq!((hello.speaks.low(), hello.speaks.high()), (1, 1));
        assert!(hello.speaks.understands(1) && !hello.speaks.understands(2));
        assert!(hello.does(HAS_PLAYS) && !hello.does("queue"));
        assert_eq!(hello.has(), ["plays"]);
    }

    #[test]
    fn what_the_backend_writes_is_read_back_the_same() {
        let id = BackendId::from_bytes([0xa5; 16]);
        let versions = [
            Version { major: 0, minor: 0, patch: 0 },
            Version { major: 12, minor: 0, patch: 345 },
            // Ten digits, in range.
            Version { major: 1_000_000_000, minor: 0, patch: 0 },
            Version { major: u32::MAX, minor: u32::MAX, patch: u32::MAX },
        ];
        for nexus in versions {
            assert_eq!(Version::parse(&nexus.to_string()), Some(nexus));
            for has in [&[][..], &["plays"][..], &["plays", "queue", "a_2"][..]] {
                let hello = Hello::new(nexus, id, Speaks::new(1, u32::MAX).unwrap(), has).unwrap();
                let json = hello.to_json();
                assert!(json.len() < HELLO_MAX_BYTES);
                assert_eq!(read(json.as_bytes()), Ok(hello));
            }
        }
        // And it will not write what no app would take.
        let version = Version { major: 0, minor: 1, patch: 0 };
        assert_eq!(Hello::new(version, id, SPEAKS, &["plays", "plays"]), None);
        assert_eq!(Hello::new(version, id, SPEAKS, &["Plays"]), None);
    }

    #[test]
    fn fields_it_does_not_know_are_passed_over() {
        let id = format!("\"{ID}\"");
        let with_more = format!(
            r#"{{"later":{{"deep":[1,{{"x":null}}]}},"nexus":"0.1.0","id":{id},"speaks":[1,2],"has":["plays","queue"],"motd":"hi","n":1.5,"t":true,"m":-3}}"#
        );
        let hello = read(with_more.as_bytes()).unwrap();
        assert_eq!(hello.has(), ["plays", "queue"]);
        // A later release, speaking only numbers this one doesn't: still the backend, so the
        // app can say it is too old (design 12.2).
        let later = hello_json(r#""2.1.0""#, &id, "[2,3]", r#"["plays","queue"]"#);
        let later = format!("{},\"words\":{{\"x\":[1]}}}}", &later[..later.len() - 1]);
        let hello = read(later.as_bytes()).unwrap();
        assert_eq!(hello.nexus, Version { major: 2, minor: 1, patch: 0 });
        assert!(!hello.speaks.understands(SPEAKS.high()));
        // Whitespace about it is JSON's, and allowed.
        let spaced = format!(" \r\n\t{} \n", good());
        assert!(read(spaced.as_bytes()).is_ok());
        // `has` may be empty: a backend that does nothing yet.
        let empty = hello_json(r#""0.1.0""#, &id, "[1,1]", "[]");
        assert_eq!(read(empty.as_bytes()).unwrap().has(), [] as [String; 0]);
        // Nested as deep as allowed.
        let deep = format!(r#","x":{}0{}"#, "[".repeat(HELLO_MAX_DEPTH - 1), "]".repeat(HELLO_MAX_DEPTH - 1));
        assert!(read(&good_with(deep.as_bytes())).is_ok());
    }

    #[test]
    fn every_byte_of_the_body_is_utf8_even_in_fields_passed_over() {
        // The review's case: a raw 0xFF in a string this version doesn't read.
        assert_eq!(read(&good_with(b",\"extra\":\"\xff\"")), Err(HelloWrong::Form));
        assert_eq!(read(&good_with(b",\"\xff\":1")), Err(HelloWrong::Form));
        assert_eq!(read(&good_with(b",\"extra\":[\"a\",{\"b\":\"\xc3\"}]")), Err(HelloWrong::Form));
        // And in a known one.
        let mut bytes = good().into_bytes();
        let at = bytes.iter().position(|&byte| byte == b'p').unwrap();
        bytes[at] = 0xff;
        assert_eq!(read(&bytes), Err(HelloWrong::Form));
        // UTF-8 that is right is no trouble.
        assert!(read(&good_with(",\"extra\":\"é ✓\"".as_bytes())).is_ok());
    }

    #[test]
    fn a_key_given_twice_is_refused_known_or_not() {
        // The review's case.
        assert_eq!(read(&good_with(b",\"extra\":0,\"extra\":1")), Err(HelloWrong::Form));
        assert_eq!(read(&good_with(b",\"extra\":0,\"extra\":0")), Err(HelloWrong::Form));
        // The same key, spelled with an escape.
        assert_eq!(read(&good_with(b",\"extra\":0,\"\\u0065xtra\":1")), Err(HelloWrong::Form));
        assert_eq!(read(&good_with(b",\"\\u0069d\":\"0123456789abcdef0123456789abcdef\"")), Err(HelloWrong::Form));
        // Inside a field passed over, at any depth.
        assert_eq!(read(&good_with(b",\"extra\":{\"a\":1,\"a\":2}")), Err(HelloWrong::Form));
        assert_eq!(read(&good_with(b",\"extra\":[{\"x\":{\"a\":1,\"a\":2}}]")), Err(HelloWrong::Form));
        // Different keys, and the same key in different objects, are no trouble.
        assert!(read(&good_with(b",\"extra\":0,\"extras\":1")).is_ok());
        assert!(read(&good_with(b",\"a\":{\"a\":{\"a\":1}},\"b\":[{\"a\":1},{\"a\":2}]")).is_ok());
    }

    #[test]
    fn a_body_nested_too_deep_is_refused() {
        let deep = format!(r#","x":{}0{}"#, "[".repeat(HELLO_MAX_DEPTH), "]".repeat(HELLO_MAX_DEPTH));
        assert_eq!(read(&good_with(deep.as_bytes())), Err(HelloWrong::Form));
        let deep = format!(r#","x":{}0{}"#, "{\"a\":".repeat(HELLO_MAX_DEPTH), "}".repeat(HELLO_MAX_DEPTH));
        assert_eq!(read(&good_with(deep.as_bytes())), Err(HelloWrong::Form));
        // The review's case: a thousand arrays about a 0.
        let deep = format!(r#","x":{}0{}"#, "[".repeat(1000), "]".repeat(1000));
        assert_eq!(read(&good_with(deep.as_bytes())), Err(HelloWrong::Form));
    }

    #[test]
    fn a_hello_wrong_in_any_known_field_is_refused() {
        use HelloWrong::*;
        let id = format!("\"{ID}\"");
        let v = r#""0.1.0""#;
        let cases: Vec<(String, HelloWrong)> = vec![
            // The whole of it.
            (String::new(), Form),
            ("null".into(), Form),
            ("[]".into(), Form),
            (format!(r#"["0.1.0",{id},[1,1],[]]"#), Form),
            (format!("{} x", good()), Form),
            (format!("{}{}", good(), good()), Form),
            (good().replacen('{', "{\u{feff}", 1), Form),
            (format!("\u{feff}{}", good()), Form),
            // Each field missing.
            (format!(r#"{{"id":{id},"speaks":[1,1],"has":[]}}"#), Form),
            (format!(r#"{{"nexus":{v},"speaks":[1,1],"has":[]}}"#), Form),
            (format!(r#"{{"nexus":{v},"id":{id},"has":[]}}"#), Form),
            (format!(r#"{{"nexus":{v},"id":{id},"speaks":[1,1]}}"#), Form),
            // Each field given twice, even the same.
            (format!(r#"{{"nexus":{v},"nexus":{v},"id":{id},"speaks":[1,1],"has":[]}}"#), Form),
            (format!(r#"{{"nexus":{v},"id":{id},"id":{id},"speaks":[1,1],"has":[]}}"#), Form),
            (format!(r#"{{"nexus":{v},"id":{id},"speaks":[1,1],"speaks":[1,1],"has":[]}}"#), Form),
            (format!(r#"{{"nexus":{v},"id":{id},"speaks":[1,1],"has":[],"has":[]}}"#), Form),
            // Each field of the wrong type.
            (hello_json("null", &id, "[1,1]", "[]"), Form),
            (hello_json("1", &id, "[1,1]", "[]"), Form),
            (hello_json(v, "null", "[1,1]", "[]"), Form),
            (hello_json(v, "12345", "[1,1]", "[]"), Form),
            (hello_json(v, &id, "null", "[]"), Form),
            (hello_json(v, &id, "1", "[]"), Form),
            (hello_json(v, &id, r#"{"low":1,"high":1}"#, "[]"), Form),
            (hello_json(v, &id, "[1.0,1]", "[]"), Form),
            (hello_json(v, &id, "[-1,1]", "[]"), Form),
            (hello_json(v, &id, r#"["1",1]"#, "[]"), Form),
            (hello_json(v, &id, "[1,4294967296]", "[]"), Form),
            (hello_json(v, &id, "[1,1]", "null"), Form),
            (hello_json(v, &id, "[1,1]", r#""plays""#), Form),
            (hello_json(v, &id, "[1,1]", "[1]"), Form),
            (hello_json(v, &id, "[1,1]", "[null]"), Form),
            // nexus.
            (hello_json(r#""""#, &id, "[1,1]", "[]"), Version),
            (hello_json(r#""1""#, &id, "[1,1]", "[]"), Version),
            (hello_json(r#""0.1""#, &id, "[1,1]", "[]"), Version),
            (hello_json(r#""0.1.0.0""#, &id, "[1,1]", "[]"), Version),
            (hello_json(r#""0.1.0-beta""#, &id, "[1,1]", "[]"), Version),
            (hello_json(r#""v0.1.0""#, &id, "[1,1]", "[]"), Version),
            (hello_json(r#"" 0.1.0""#, &id, "[1,1]", "[]"), Version),
            (hello_json(r#""01.1.0""#, &id, "[1,1]", "[]"), Version),
            (hello_json(r#""0.+1.0""#, &id, "[1,1]", "[]"), Version),
            (hello_json(r#""0..0""#, &id, "[1,1]", "[]"), Version),
            (hello_json(r#""4294967296.0.0""#, &id, "[1,1]", "[]"), Version),
            (hello_json(r#""0.0.99999999999""#, &id, "[1,1]", "[]"), Version),
            // id.
            (hello_json(v, r#""""#, "[1,1]", "[]"), Id),
            (hello_json(v, &format!("\"{}\"", &ID[..31]), "[1,1]", "[]"), Id),
            (hello_json(v, &format!("\"{ID}0\""), "[1,1]", "[]"), Id),
            (hello_json(v, &format!("\"{}\"", ID.to_uppercase()), "[1,1]", "[]"), Id),
            (hello_json(v, r#""0123456789abcdef0123456789abcdeg""#, "[1,1]", "[]"), Id),
            (hello_json(v, r#""0123456789abcdef-123456789abcdef""#, "[1,1]", "[]"), Id),
            (hello_json(v, &format!("\" {}\"", &ID[1..]), "[1,1]", "[]"), Id),
            // speaks.
            (hello_json(v, &id, "[]", "[]"), Speaks),
            (hello_json(v, &id, "[1]", "[]"), Speaks),
            (hello_json(v, &id, "[1,1,1]", "[]"), Speaks),
            (hello_json(v, &id, "[2,1]", "[]"), Speaks),
            (hello_json(v, &id, "[0,1]", "[]"), Speaks),
            (hello_json(v, &id, "[0,0]", "[]"), Speaks),
            // has.
            (hello_json(v, &id, "[1,1]", r#"["plays","plays"]"#), Has),
            (hello_json(v, &id, "[1,1]", r#"[""]"#), Has),
            (hello_json(v, &id, "[1,1]", r#"["Plays"]"#), Has),
            (hello_json(v, &id, "[1,1]", r#"["1plays"]"#), Has),
            (hello_json(v, &id, "[1,1]", r#"["pla ys"]"#), Has),
            (hello_json(v, &id, "[1,1]", r#"["pla-ys"]"#), Has),
            (hello_json(v, &id, "[1,1]", &format!("[\"{}\"]", "a".repeat(HAS_NAME_MAX_BYTES + 1))), Has),
            (
                hello_json(v, &id, "[1,1]", &format!("[{}]", (0..=HAS_MAX).map(|n| format!("\"n{n}\"")).collect::<Vec<_>>().join(","))),
                Has,
            ),
        ];
        for (body, wrong) in cases {
            assert_eq!(read(body.as_bytes()), Err(wrong), "{body}");
        }
        // The most `has` may hold is allowed.
        let most = (0..HAS_MAX).map(|n| format!("\"n{n}\"")).collect::<Vec<_>>().join(",");
        assert!(read(hello_json(v, &id, "[1,1]", &format!("[{most}]")).as_bytes()).is_ok());
        let longest = format!("[\"{}\"]", "a".repeat(HAS_NAME_MAX_BYTES));
        assert!(read(hello_json(v, &id, "[1,1]", &longest).as_bytes()).is_ok());
    }

    #[test]
    fn an_id_has_one_spelling() {
        let id = BackendId::from_hex(ID).unwrap();
        assert_eq!(id.to_hex(), ID);
        assert_eq!(BackendId::from_hex(&id.to_hex()), Some(id));
        assert_eq!(format!("{id:?}"), format!("BackendId({ID})"));
        let all = BackendId::from_bytes(std::array::from_fn(|n| (n * 17) as u8));
        assert_eq!(BackendId::from_hex(&all.to_hex()), Some(all));
        assert_eq!(BackendId::from_hex("ÿÿÿÿÿÿÿÿÿÿÿÿÿÿÿÿ"), None);
    }

    // ---- the complete body ----------------------------------------------------------------

    /// A valid `hello` padded with spaces to exactly `size` bytes.
    fn padded(size: usize) -> Vec<u8> {
        let mut bytes = good().into_bytes();
        bytes.resize(size, b' ');
        bytes
    }

    /// A reader that gives `bytes` a few at a time, then fails.
    struct Failing<'a>(&'a [u8]);

    impl std::io::Read for Failing<'_> {
        fn read(&mut self, into: &mut [u8]) -> std::io::Result<usize> {
            if self.0.is_empty() {
                return Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "cut"));
            }
            let n = into.len().min(self.0.len()).min(7);
            into[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }

    #[test]
    fn a_body_is_had_whole_or_not_at_all() {
        // At the bound, whole: the backend.
        let exactly = padded(HELLO_MAX_BYTES);
        assert_eq!(Body::read(&exactly[..]).map(|body| body.bytes().len()), Ok(HELLO_MAX_BYTES));
        assert!(matches!(classify(200, &exactly[..]), Found::Backend(_)));
        // The review's case: the same and one byte more is not, though its first 4,096 bytes
        // are a valid `hello`.
        let mut more = exactly.clone();
        more.push(b'x');
        assert_eq!(Body::read(&more[..]), Err(BodyWrong::TooLarge));
        assert_eq!(classify(200, &more[..]), Found::NoBackend(NoBackend::TooLarge));
        // However much more there is, no more than one byte past the bound is read.
        let mut huge = std::io::Read::chain(&exactly[..], std::io::repeat(b' '));
        assert_eq!(classify(200, &mut huge), Found::NoBackend(NoBackend::TooLarge));
        // A body cut off by a failed read is not had, even if what came is a valid `hello`.
        assert_eq!(Body::read(Failing(good().as_bytes())), Err(BodyWrong::Cut));
        assert_eq!(classify(200, Failing(good().as_bytes())), Found::NoBackend(NoBackend::Cut));
    }

    // ---- telling the backend from anything else -------------------------------------------

    /// A reader that must not be read: the body of an answer that is not 200.
    struct Untouched;

    impl std::io::Read for Untouched {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            panic!("the body of an answer that is not 200 was read");
        }
    }

    #[test]
    fn a_200_with_a_valid_hello_is_the_backend_whatever_its_headers() {
        // No header is an input at all: a `hello` without a Content-Type, or under any type,
        // is the backend. (So a captive portal's page labelled JSON is no backend either: its
        // body is not a `hello`.)
        match classify(200, good().as_bytes()) {
            Found::Backend(hello) => assert_eq!(hello.id.to_hex(), ID),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn any_other_status_is_no_backend_whatever_the_body() {
        use NoBackend::*;
        for status in [300, 301, 302, 303, 304, 307, 308, 399] {
            assert_eq!(classify(status, Untouched), Found::NoBackend(Redirect), "{status}");
        }
        for status in [0, 100, 101, 201, 202, 204, 206, 299, 400, 401, 403, 404, 405, 408, 413, 429, 500, 502, 503, 504, 999] {
            assert_eq!(classify(status, Untouched), Found::NoBackend(Status(status)), "{status}");
        }
        // And with the backend's own body, read or not: the review's 201 case.
        for status in [201, 302, 404, 500] {
            assert!(matches!(classify(status, good().as_bytes()), Found::NoBackend(_)), "{status}");
        }
    }

    #[test]
    fn what_the_music_server_and_the_proxy_answer_is_no_backend() {
        use NoBackend::*;
        // What Navidrome 0.64.2 sent in E1 and E2, and what a proxy or a portal may.
        let found: &[u8] = b"<a href=\"/app/\">Found</a>.\n\n";
        let subsonic_json: &[u8] = br#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.64.2","openSubsonic":true}}"#;
        let subsonic_xml: &[u8] = br#"<?xml version="1.0" encoding="UTF-8"?><subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"></subsonic-response>"#;
        let html: &[u8] = b"<!doctype html><html><head><title>Navidrome</title></head><body></body></html>";
        let large = vec![b' '; HELLO_MAX_BYTES + 1];
        let cases: &[(u16, &[u8], NoBackend)] = &[
            // Navidrome's redirect to its web app, for a GET under `/nexus/`.
            (302, found, Redirect),
            // The proxy's 301 for `/nexus` without the slash.
            (301, b"<html>301</html>", Redirect),
            // Navidrome's "method not allowed", empty.
            (405, b"", Status(405)),
            (404, b"404 page not found\n", Status(404)),
            (502, b"<html>502 Bad Gateway</html>", Status(502)),
            // A 200 that is not a `hello`.
            (200, html, NotHello(HelloWrong::Form)),
            (200, subsonic_xml, NotHello(HelloWrong::Form)),
            (200, subsonic_json, NotHello(HelloWrong::Form)),
            (200, b"{}", NotHello(HelloWrong::Form)),
            (200, br#"{"nexus":"0.1.0"}"#, NotHello(HelloWrong::Form)),
            (200, b"null", NotHello(HelloWrong::Form)),
            (200, b"   ", NotHello(HelloWrong::Form)),
            (200, b"\x89PNG\r\n", NotHello(HelloWrong::Form)),
            (200, b"", Empty),
            (200, &large, TooLarge),
        ];
        for &(status, body, why) in cases {
            assert_eq!(classify(status, body), Found::NoBackend(why), "{status} {:?}", String::from_utf8_lossy(body));
        }
        let wrong_id = good().replace(ID, &ID.to_uppercase());
        assert_eq!(classify(200, wrong_id.as_bytes()), Found::NoBackend(NotHello(HelloWrong::Id)));
    }
}
