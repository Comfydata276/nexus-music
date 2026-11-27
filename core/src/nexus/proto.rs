//! The backend's first words: where it is, its `hello`, and what counts as "no backend here".
//!
//! - **Where it is** (design 6.1): at the root of the music server's own origin, under
//!   [`ROOT`], whatever path the music server itself is under. Every address an app sends is
//!   one of the constants here: always with the slash after `nexus`, never with a query, and
//!   never ending in the name of a picture, a script or a style sheet (a proxy may send those
//!   somewhere else and keep the answer).
//! - **`hello`** (design 12.1): `GET /nexus/api/hello` answers
//!   `{"nexus":"0.1.0","id":"…","speaks":[1,1],"has":["plays"]}`. [`Hello::read`] reads it,
//!   checking every field before anything is made of it.
//! - **Known by its body, never by its status** (design 6.1, experiment E1): a music server
//!   left to answer under `/nexus/` sends a redirect to its own web app, or "method not
//!   allowed", not "not found". [`classify`] says "this is the backend" only for an answer
//!   whose body is a valid `hello`; anything else, whatever its status, is "no backend here".
//! - **The plain form of an address** (design 6.1, experiment E2): the proxy chooses where to
//!   send a request by the tidied path and hands on the untidied one, so the backend itself
//!   refuses, unread, any request target that [`plain`] does not pass. The apps never send
//!   such a thing.

use std::fmt;

use serde::Deserialize;

/// Everything of the backend's is under this, at the root of the music server's origin.
/// With the slash: without it a proxy's rule also catches `/nexusx`.
pub const ROOT: &str = "/nexus/";
/// The backend's calls are under this.
pub const API: &str = "/nexus/api/";
/// `GET`: what the backend is and what it does. Answers anyone and needs nothing.
pub const HELLO: &str = "/nexus/api/hello";

/// Every path the backend has, so that a test can hold each to the rules above.
pub const PATHS: &[&str] = &[HELLO];

/// What a backend that keeps plays says in [`Hello::has`].
pub const HAS_PLAYS: &str = "plays";

/// The most of an answer to `hello` that is read. A real `hello` is a tenth of this; a larger
/// answer is not the backend's.
pub const HELLO_MAX_BYTES: usize = 4096;
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

/// The backend's own version, as `MAJOR.MINOR.PATCH`. Shown, never used to decide anything:
/// [`Speaks`] is what says whether two sides understand each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    /// Exactly three numbers with a dot between each: digits only, no sign, no leading zero
    /// (but `0` itself), nothing before or after.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.split('.');
        let mut number = || -> Option<u32> {
            let part = parts.next()?;
            if part.is_empty() || part.len() > 9 || !part.bytes().all(|byte| byte.is_ascii_digit()) {
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
}

/// Why a body is not a valid `hello`. Each is "no backend here".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HelloWrong {
    /// Not one JSON object in UTF-8, or a known field missing, given twice, or of the wrong
    /// type.
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

/// A known field missing, given twice or of the wrong type fails here; a field this version
/// doesn't know is passed over.
#[derive(Deserialize)]
struct Wire {
    nexus: String,
    id: String,
    speaks: Vec<u32>,
    has: Vec<String>,
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
    /// A `hello` for the backend to send. `None` if `has` breaks the rules [`Hello::read`]
    /// holds an answer to, so that the backend cannot send what no app would take.
    pub fn new(nexus: Version, id: BackendId, speaks: Speaks, has: &[&str]) -> Option<Self> {
        let has = has_list(has.iter().map(|name| name.to_string()).collect())?;
        Some(Hello { nexus, id, speaks, has })
    }

    /// Reads a body that may be a `hello`: no more than [`HELLO_MAX_BYTES`], one JSON object,
    /// every known field present once and right, other fields passed over.
    pub fn read(body: &[u8]) -> Result<Self, HelloWrong> {
        if body.len() > HELLO_MAX_BYTES {
            return Err(HelloWrong::Form);
        }
        // One object, and nothing else: a list of the same values in the same order would
        // otherwise be read as the same fields.
        let first = body.iter().find(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'));
        if first != Some(&b'{') {
            return Err(HelloWrong::Form);
        }
        let wire: Wire = serde_json::from_slice(body).map_err(|_| HelloWrong::Form)?;
        let nexus = Version::parse(&wire.nexus).ok_or(HelloWrong::Version)?;
        let id = BackendId::from_hex(&wire.id).ok_or(HelloWrong::Id)?;
        let speaks = match wire.speaks[..] {
            [low, high] => Speaks::new(low, high).ok_or(HelloWrong::Speaks)?,
            _ => return Err(HelloWrong::Speaks),
        };
        let has = has_list(wire.has).ok_or(HelloWrong::Has)?;
        Ok(Hello { nexus, id, speaks, has })
    }

    /// The body the backend sends: what [`Hello::read`] reads back as this same `hello`.
    pub fn to_json(&self) -> String {
        serde_json::json!({
            "nexus": self.nexus.to_string(),
            "id": self.id.to_hex(),
            "speaks": [self.speaks.low, self.speaks.high],
            "has": self.has,
        })
        .to_string()
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

/// An answer to `GET /nexus/api/hello`, as the app's transport got it, **without following
/// any redirect**.
#[derive(Debug, Clone, Copy)]
pub struct Answer<'a> {
    pub status: u16,
    /// Every `Content-Type` the answer carried, in order (usually one, or none).
    pub content_types: &'a [&'a str],
    /// Whether it carried a `Location`.
    pub location: bool,
    /// The body after the transport has undone any compression. A transport that stops
    /// reading past [`HELLO_MAX_BYTES`] hands in what it has, or one byte more, and the
    /// answer is too large either way.
    pub body: &'a [u8],
}

/// Why an answer is not the backend's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoBackend {
    /// A 3xx, or a `Location`: a music server left to answer under `/nexus/` redirects to its
    /// own web app. A redirect is an answer, never a place to go.
    Redirect,
    /// Any status but 200: Navidrome's 405 for a method other than GET, a proxy's 404, 502,
    /// 403, and every other.
    Status(u16),
    /// Not one `Content-Type` of `application/json`: HTML, the music server's XML, a picture.
    NotJson,
    /// No body at all.
    Empty,
    /// A body larger than [`HELLO_MAX_BYTES`].
    TooLarge,
    /// JSON, but not a valid `hello`: the music server's own JSON, or a `hello` wrong in a
    /// known field.
    NotHello(HelloWrong),
}

/// What an answer to `hello` says is at the address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Backend(Hello),
    NoBackend(NoBackend),
}

/// Whether one `Content-Type` value is JSON: `application/json`, in any case, with nothing
/// after it but `charset=utf-8`.
fn json_type(value: &str) -> bool {
    let mut parts = value.split(';');
    let essence = parts.next().unwrap_or("").trim();
    if !essence.eq_ignore_ascii_case("application/json") {
        return false;
    }
    parts.all(|parameter| {
        let parameter = parameter.trim();
        parameter.is_empty() || parameter.eq_ignore_ascii_case("charset=utf-8")
    })
}

/// "This is the backend" for an answer of 200, with one JSON `Content-Type`, no `Location`,
/// and a body that [`Hello::read`] reads. "No backend here", with why, for everything else.
pub fn classify(answer: &Answer<'_>) -> Found {
    let refused = Found::NoBackend;
    if (300..400).contains(&answer.status) || answer.location {
        return refused(NoBackend::Redirect);
    }
    if answer.status != 200 {
        return refused(NoBackend::Status(answer.status));
    }
    match answer.content_types {
        [only] if json_type(only) => {}
        _ => return refused(NoBackend::NotJson),
    }
    if answer.body.is_empty() {
        return refused(NoBackend::Empty);
    }
    if answer.body.len() > HELLO_MAX_BYTES {
        return refused(NoBackend::TooLarge);
    }
    match Hello::read(answer.body) {
        Ok(hello) => Found::Backend(hello),
        Err(wrong) => refused(NoBackend::NotHello(wrong)),
    }
}

#[cfg(test)]
mod tests {
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

    /// Every target in E2.2's table, and E1's that are not already in it: what reached the
    /// backend, and the verdict on what it would see. `Err` is refused unread.
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
        ("http://music.example.org/nexus/api/hello", As("/nexus/api/hello"), Err(NotPlain::DoubledSlash)),
        // With Cache Assets off these reach the backend; with it on, the music server and the
        // proxy's cache. No path of the backend's ends so.
        ("/nexus/app.js", AsSent, Err(NotPlain::Byte)),
        ("/nexus/x.png", AsSent, Err(NotPlain::Byte)),
        ("/nexus/?q=union+select+(1)", AsSent, Err(NotPlain::Query)),
        ("/nexus/?u=http://x", AsSent, Err(NotPlain::Query)),
        // Sent with POST, PUT, DELETE, OPTIONS and PATCH; the method is the backend's to
        // refuse, not the target's.
        ("/nexus/api/prove/begin", AsSent, Ok(())),
        // E1's, under a music server with no rule for `/nexus/`.
        ("/nexus/ws", AsSent, Ok(())),
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

    #[test]
    fn the_designs_hello_is_read_and_every_field_is_kept() {
        let hello = Hello::read(good().as_bytes()).unwrap();
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
        for has in [&[][..], &["plays"][..], &["plays", "queue", "a_2"][..]] {
            let hello = Hello::new(Version { major: 12, minor: 0, patch: 345 }, id, Speaks::new(1, 3).unwrap(), has).unwrap();
            let json = hello.to_json();
            assert!(json.len() < HELLO_MAX_BYTES);
            assert_eq!(Hello::read(json.as_bytes()), Ok(hello));
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
            r#"{{"later":{{"deep":[1,{{"x":null}}]}},"nexus":"0.1.0","id":{id},"speaks":[1,2],"has":["plays","queue"],"motd":"hi","n":1.5}}"#
        );
        let hello = Hello::read(with_more.as_bytes()).unwrap();
        assert_eq!(hello.has(), ["plays", "queue"]);
        // Whitespace about it is JSON's, and allowed.
        let spaced = format!(" \r\n\t{} \n", good());
        assert!(Hello::read(spaced.as_bytes()).is_ok());
        // `has` may be empty: a backend that does nothing yet.
        let empty = hello_json(r#""0.1.0""#, &id, "[1,1]", "[]");
        assert_eq!(Hello::read(empty.as_bytes()).unwrap().has(), [] as [String; 0]);
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
            (hello_json(r#""9999999999.0.0""#, &id, "[1,1]", "[]"), Version),
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
            assert_eq!(Hello::read(body.as_bytes()), Err(wrong), "{body}");
        }
        // Not UTF-8.
        let mut bytes = good().into_bytes();
        let at = bytes.iter().position(|&byte| byte == b'p').unwrap();
        bytes[at] = 0xff;
        assert_eq!(Hello::read(&bytes), Err(HelloWrong::Form));
        // Too large, however valid.
        let padded = format!(r#"{{"pad":"{}",{}"#, " ".repeat(HELLO_MAX_BYTES), &good()[1..]);
        assert_eq!(Hello::read(padded.as_bytes()), Err(HelloWrong::Form));
        // The most `has` may hold is allowed.
        let most = (0..HAS_MAX).map(|n| format!("\"n{n}\"")).collect::<Vec<_>>().join(",");
        assert!(Hello::read(hello_json(v, &id, "[1,1]", &format!("[{most}]")).as_bytes()).is_ok());
        let longest = format!("[\"{}\"]", "a".repeat(HAS_NAME_MAX_BYTES));
        assert!(Hello::read(hello_json(v, &id, "[1,1]", &longest).as_bytes()).is_ok());
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

    // ---- telling the backend from anything else -------------------------------------------

    const JSON: &[&str] = &["application/json"];

    fn answer<'a>(status: u16, content_types: &'a [&'a str], location: bool, body: &'a [u8]) -> Answer<'a> {
        Answer { status, content_types, location, body }
    }

    #[test]
    fn the_backends_answer_is_the_backend() {
        let body = good();
        for types in [JSON, &["application/json; charset=utf-8"][..], &["Application/JSON;charset=UTF-8"][..], &["application/json;"][..]] {
            match classify(&answer(200, types, false, body.as_bytes())) {
                Found::Backend(hello) => assert_eq!(hello.id.to_hex(), ID),
                other => panic!("{types:?}: {other:?}"),
            }
        }
    }

    #[test]
    fn what_the_music_server_and_the_proxy_answer_is_no_backend_whatever_the_status() {
        use NoBackend::*;
        let hello = good();
        let hello = hello.as_bytes();
        // What Navidrome 0.64.2 sent in E1 and E2, and what a proxy may.
        let found = b"<a href=\"/app/\">Found</a>.\n\n";
        let subsonic_json = br#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.64.2","openSubsonic":true}}"#;
        let subsonic_xml = br#"<?xml version="1.0" encoding="UTF-8"?><subsonic-response xmlns="http://subsonic.org/restapi" status="ok" version="1.16.1"></subsonic-response>"#;
        let html = b"<!doctype html><html><head><title>Navidrome</title></head><body></body></html>";
        let large = vec![b' '; HELLO_MAX_BYTES + 1];
        let cases: &[(Answer<'_>, NoBackend)] = &[
            // Navidrome's redirect to its web app, for a GET under `/nexus/`.
            (answer(302, &["text/html; charset=utf-8"], true, found), Redirect),
            // The proxy's 301 for `/nexus` without the slash.
            (answer(301, &["text/html"], true, b"<html>301</html>"), Redirect),
            // A redirect carrying the backend's own body is still a redirect.
            (answer(302, JSON, true, hello), Redirect),
            (answer(307, JSON, false, hello), Redirect),
            (answer(200, JSON, true, hello), Redirect),
            (answer(304, &[], false, b""), Redirect),
            // Navidrome's "method not allowed", empty.
            (answer(405, &[], false, b""), Status(405)),
            (answer(404, &["text/plain; charset=utf-8"], false, b"404 page not found\n"), Status(404)),
            (answer(400, &[], false, b""), Status(400)),
            (answer(403, &["text/html"], false, b"<html>403 Forbidden</html>"), Status(403)),
            (answer(413, &["text/html"], false, b"<html>413</html>"), Status(413)),
            (answer(502, &["text/html"], false, b"<html>502 Bad Gateway</html>"), Status(502)),
            // The backend's own body under any status but 200.
            (answer(201, JSON, false, hello), Status(201)),
            (answer(204, JSON, false, b""), Status(204)),
            (answer(404, JSON, false, hello), Status(404)),
            (answer(500, JSON, false, hello), Status(500)),
            (answer(0, JSON, false, hello), Status(0)),
            (answer(999, JSON, false, hello), Status(999)),
            // A 200 that is not JSON: HTML, Navidrome's XML, a picture, no type at all.
            (answer(200, &["text/html"], false, html), NotJson),
            (answer(200, &["application/xml"], false, subsonic_xml), NotJson),
            (answer(200, &["text/xml"], false, subsonic_xml), NotJson),
            (answer(200, &["image/png"], false, b"\x89PNG"), NotJson),
            (answer(200, &[], false, hello), NotJson),
            (answer(200, &["text/plain"], false, hello), NotJson),
            (answer(200, &["application/json", "application/json"], false, hello), NotJson),
            (answer(200, &["application/json; charset=latin-1"], false, hello), NotJson),
            (answer(200, &["application/jsonx"], false, hello), NotJson),
            (answer(200, &["application/problem+json"], false, hello), NotJson),
            // A 200 of JSON with nothing in it, or too much.
            (answer(200, JSON, false, b""), Empty),
            (answer(200, JSON, false, &large), TooLarge),
            // Navidrome's own JSON, and JSON that is nearly a `hello`.
            (answer(200, JSON, false, subsonic_json), NotHello(HelloWrong::Form)),
            (answer(200, JSON, false, b"{}"), NotHello(HelloWrong::Form)),
            (answer(200, JSON, false, b"null"), NotHello(HelloWrong::Form)),
            (answer(200, JSON, false, b"   "), NotHello(HelloWrong::Form)),
        ];
        for (answer, why) in cases {
            assert_eq!(classify(answer), Found::NoBackend(*why), "{answer:?}");
        }
        let wrong_id = good().replace(ID, &ID.to_uppercase());
        assert_eq!(
            classify(&answer(200, JSON, false, wrong_id.as_bytes())),
            Found::NoBackend(NoBackend::NotHello(HelloWrong::Id))
        );
    }
}
