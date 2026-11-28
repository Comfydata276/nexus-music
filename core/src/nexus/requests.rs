//! The app's Requests calls and what the Nexus server says back (§16.1).
//! The transport owns authentication, backend identity, and complete HTTP messages.
//! A change's request ID belongs to the action and must be reused if its begin is retried.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fmt;
use std::io::{self, Write};
use std::num::NonZeroU64;
use std::time::Duration;

use serde::{Deserialize, Serialize, de};

use super::proto::{HELLO_MAX_DEPTH, Speaks};
use crate::lidarr::{ArtistOffer, Listed, Pending, RequestLidarr, Single, WayBack};
use crate::offer::Offer;

// the design does not say how to spell the path and JSON tags. One POST with a closed call tag.
pub const REQUESTS: &str = "/nexus/api/requests";
pub const PATHS: &[&str] = &[REQUESTS];
pub const CALL_MAX_BYTES: usize = 1024 * 1024;
pub const SMALL_REPLY_MAX_BYTES: usize = 4096;
// The lead's ruling on §7.1 and §16.1: keep the small ceiling for status, jobs and
// errors. A successful list has its own bounds and is refused whole when over them.
pub const LIST_REPLY_MAX_BYTES: usize = 1024 * 1024;
pub const SEARCH_MAX_CHARS: usize = 200;
pub const BUSY_MAX_ARTISTS: usize = 500;
pub const BATCH_MAX_LINES: usize = 2000;
pub const KEPT_ALBUMS: usize = 500;
pub const KEPT_ARTISTS: usize = 200;
pub const OFFER_LIFETIME_SECONDS: i64 = 3600;

// the design does not say the remaining field and count ceilings. Search results that
// make offers fit in a session's book. Other catalog lists fit within Lidarr's 100 x 500
// page ceiling, subject to the whole reply limit; no result is silently truncated.
pub const LIST_MAX_ITEMS: usize = 50_000;
pub const NAME_MAX_BYTES: usize = 1024;
pub const ABOUT_MAX_BYTES: usize = 1024;
pub const COVER_MAX_BYTES: usize = 2048;
pub const CHANGE_MAX_RELEASES: usize = KEPT_ALBUMS;

// the design does not say the integer widths. Lidarr's numbers are positive signed
// 64-bit integers, as in the released core; a begin receipt and job each use a nonzero u64.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct LidarrNumber(i64);

impl LidarrNumber {
    pub fn new(number: i64) -> Option<Self> {
        (number > 0).then_some(Self(number))
    }

    pub fn get(self) -> i64 {
        self.0
    }
}

impl TryFrom<i64> for LidarrNumber {
    type Error = WireError;

    fn try_from(number: i64) -> Result<Self, Self::Error> {
        Self::new(number).ok_or(WireError::Bounds)
    }
}

impl From<LidarrNumber> for i64 {
    fn from(number: LidarrNumber) -> Self {
        number.get()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RequestId(pub NonZeroU64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(pub NonZeroU64);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MusicBrainzId(String);

impl MusicBrainzId {
    // the design does not say whether to accept other spellings of a UUID. Keep the
    // canonical lowercase 8-4-4-4-12 spelling; accept all UUID versions.
    pub fn new(text: &str) -> Option<Self> {
        is_mbid(text).then(|| Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for MusicBrainzId {
    type Error = WireError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if !is_mbid(&text) {
            return Err(WireError::Bounds);
        }
        Ok(Self(text))
    }
}

impl From<MusicBrainzId> for String {
    fn from(id: MusicBrainzId) -> Self {
        id.0
    }
}

fn is_mbid(text: &str) -> bool {
    text.len() == 36 && text.bytes().enumerate().all(|(at, byte)| match at {
        8 | 13 | 18 | 23 => byte == b'-',
        _ => matches!(byte, b'0'..=b'9' | b'a'..=b'f'),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OfferName(pub MusicBrainzId);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Search(String);

impl Search {
    pub fn new(text: &str) -> Option<Self> {
        valid_search(text).then(|| Self(text.to_owned()))
    }

    /// Hand this to a Lidarr find method, which encodes it as one search value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Search {
    type Error = WireError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        if !valid_search(&text) {
            return Err(WireError::Bounds);
        }
        Ok(Self(text))
    }
}

impl From<Search> for String {
    fn from(search: Search) -> Self {
        search.0
    }
}

fn valid_search(text: &str) -> bool {
    // the design does not say what to do with surrounding spaces, control characters
    // or alternate spellings of the reserved prefix. Keep ordinary search text as typed,
    // refuse blank/control text, and allow the reserved lookup only in its exact form.
    valid_name_to_find(text) && {
        let reserved = text.trim_start().get(..7).is_some_and(|prefix| prefix.eq_ignore_ascii_case("lidarr:"));
        !reserved || text.strip_prefix("lidarr:").is_some_and(is_mbid)
    }
}

fn valid_name_to_find(text: &str) -> bool {
    !text.trim().is_empty()
        && text.chars().take(SEARCH_MAX_CHARS + 1).count() <= SEARCH_MAX_CHARS
        && !text.chars().any(char::is_control)
}

/// Authentication stays in the transport header. The head mirrors the account calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub speaks: [u32; 2],
    pub restore: u64,
}

impl Head {
    pub fn new(speaks: Speaks, restore: u64) -> Self {
        Self { speaks: [speaks.low(), speaks.high()], restore }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    #[serde(deserialize_with = "object")]
    pub head: Head,
    #[serde(deserialize_with = "object")]
    pub call: Call,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "ask", rename_all = "snake_case")]
pub enum Call {
    Status,
    FindReleases { search: Search },
    FindAny { search: Search },
    FindArtists { search: Search },
    Tracked,
    KnownArtist { artist: MusicBrainzId },
    AlbumsOf { artist: LidarrNumber },
    ReleasesOf { artist: LidarrNumber },
    Listed { artist: LidarrNumber },
    Busy { artists: Vec<LidarrNumber> },
    Pending,
    Begin {
        request_id: RequestId,
        #[serde(deserialize_with = "object")]
        change: Change,
    },
    After { job: JobId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum Change {
    Request { offer: OfferName },
    AddArtist { artist: MusicBrainzId },
    AddWanting { offers: Vec<OfferName> },
    Want { artist: LidarrNumber, albums: Vec<LidarrNumber> },
    ListSingles { artist: LidarrNumber, offer: OfferName },
    Cancel { album: LidarrNumber },
    Batch {
        #[serde(deserialize_with = "objects")]
        lines: Vec<BatchLine>,
    },
}

/// A playlist line is deliberately just three names. It goes through bulk's lookup
/// rules on the server. It cannot carry a playlist ID, a Lidarr setting or an album body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchLine {
    pub artist: String,
    pub album: String,
    pub song: String,
}

impl BatchLine {
    fn valid(&self) -> bool {
        valid_name_to_find(&self.artist) && valid_name_to_find(&self.album) && valid_name_to_find(&self.song)
    }
}

fn within(count: usize, max: usize) -> Result<(), WireError> {
    require(count <= max)
}

fn require(valid: bool) -> Result<(), WireError> {
    if valid { Ok(()) } else { Err(WireError::Bounds) }
}

fn unique<T: Eq + std::hash::Hash>(items: &[T]) -> bool {
    let mut seen = HashSet::with_capacity(items.len());
    items.iter().all(|item| seen.insert(item))
}

impl Message {
    fn check(&self) -> Result<(), WireError> {
        let [low, high] = self.head.speaks;
        require(Speaks::new(low, high).is_some())?;
        match &self.call {
            Call::Busy { artists } => {
                within(artists.len(), BUSY_MAX_ARTISTS)?;
                require(unique(artists))
            }
            Call::Begin { change, .. } => change.check(),
            _ => Ok(()),
        }
    }
}

impl Change {
    fn check(&self) -> Result<(), WireError> {
        // the design does not say whether to accept an empty change or repeated selections.
        // Refuse both. The batch preserves repeated lines, which can name the same song.
        match self {
            Self::AddWanting { offers } => {
                within(offers.len(), CHANGE_MAX_RELEASES)?;
                require(!offers.is_empty() && unique(offers))
            }
            Self::Want { albums, .. } => {
                within(albums.len(), CHANGE_MAX_RELEASES)?;
                require(!albums.is_empty() && unique(albums))
            }
            Self::Batch { lines } => {
                within(lines.len(), BATCH_MAX_LINES)?;
                require(!lines.is_empty() && lines.iter().all(BatchLine::valid))
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    Ready,
    Unconfigured,
    Unavailable,
}

/// Closed codes. No Lidarr error, HTTP error, credential or arbitrary sentence goes on
/// the wire. the design does not say how to label the other refusals. Keep separate
/// codes for the cases the screen can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    SwitchedOff,
    SearchAgain,
    Busy,
    NotAllowed,
    NotSignedIn,
    StartAgain,
    Malformed,
    Incompatible,
    TooLarge,
    LidarrUnavailable,
    LidarrRefused,
    JobNotFound,
    NotDealtWith,
}

impl Refusal {
    pub fn sentence(self) -> &'static str {
        match self {
            Self::SwitchedOff => "Requests are switched off for you.",
            Self::SearchAgain => "This offer has gone. Search again.",
            Self::Busy => "Requests are busy. Try again in a moment.",
            Self::NotAllowed => "You can't make that request.",
            Self::NotSignedIn => "Sign in to Nexus Music again.",
            Self::StartAgain => "The server's data was put back. Sign in again.",
            Self::Malformed => "The request wasn't understood.",
            Self::Incompatible => "Update Nexus Music to make this request.",
            Self::TooLarge => "There's too much to show. Try a narrower search.",
            Self::LidarrUnavailable => "The Nexus server couldn't reach Lidarr.",
            Self::LidarrRefused => "Lidarr answered with an error. Try again in a moment.",
            Self::JobNotFound => "The Nexus server no longer has that request.",
            Self::NotDealtWith => "The Nexus server couldn't deal with the request.",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "done", rename_all = "snake_case")]
pub enum Completed {
    Requested,
    ArtistAdded { count: u32 },
    AddedWanting { artist: LidarrNumber },
    Wanted,
    ListedSingles,
    Cancelled,
    Batch,
}

impl Completed {
    pub fn sentence(&self) -> &'static str {
        match self {
            Self::Requested => "The album is on Lidarr's list.",
            Self::ArtistAdded { .. } => "Lidarr is keeping track of the artist.",
            Self::AddedWanting { .. } | Self::Wanted => "The releases are on Lidarr's list.",
            Self::ListedSingles => "Lidarr has been asked to list the artist's singles.",
            Self::Cancelled => "The album is off Lidarr's list.",
            Self::Batch => "Lidarr has dealt with the batch.",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum Answer {
    Status { lidarr: Availability },
    FoundReleases {
        #[serde(deserialize_with = "objects")]
        albums: Vec<AlbumView>,
        #[serde(deserialize_with = "objects")]
        singles: Vec<SingleView>,
    },
    FoundAny {
        #[serde(deserialize_with = "objects")]
        albums: Vec<AlbumView>,
    },
    FoundArtists {
        #[serde(deserialize_with = "objects")]
        artists: Vec<ArtistView>,
    },
    Tracked { artists: Vec<MusicBrainzId> },
    KnownArtist { artist: Option<(LidarrNumber, String)> },
    AlbumsOf {
        #[serde(deserialize_with = "objects")]
        albums: Vec<ReleaseView>,
    },
    ReleasesOf {
        #[serde(deserialize_with = "objects")]
        releases: Vec<ReleaseView>,
    },
    Listed {
        #[serde(deserialize_with = "objects")]
        releases: Vec<ListedView>,
    },
    Busy { artists: Vec<LidarrNumber>, stamp: u64 },
    Pending {
        #[serde(deserialize_with = "objects")]
        albums: Vec<PendingView>,
    },
    Begun { job: JobId },
    Working,
    Done {
        #[serde(deserialize_with = "object")]
        result: Completed,
    },
    Failed { reason: Refusal },
    Refused { reason: Refusal },
}

/// MusicBrainz group ID doubles as the offer's name. A mutation must still find that
/// name in its own session's offers; the name conveys no authority by itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlbumView {
    pub album: OfferName,
    pub title: String,
    // the design does not say how to encode missing metadata from Lidarr. A release
    // listing can have no embedded artist; only an offer with an artist ID is actionable.
    pub artist_id: Option<MusicBrainzId>,
    pub artist: String,
    pub year: String,
    pub cover: String,
    pub requested: bool,
    pub kind: Kind,
    pub soundtrack: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Album,
    Ep,
    Single,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtistView {
    pub id: MusicBrainzId,
    pub name: String,
    pub about: String,
    pub cover: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SingleView {
    pub title: String,
    pub artist_id: MusicBrainzId,
    pub votes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseView {
    #[serde(deserialize_with = "object")]
    pub album: AlbumView,
    pub have: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedView {
    pub album: MusicBrainzId,
    pub id: LidarrNumber,
    pub wanted: bool,
    pub have: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingView {
    pub id: LidarrNumber,
    pub album: MusicBrainzId,
    pub title: String,
    pub artist: String,
    pub progress: Option<u8>,
}

fn safe_text(text: &str, max_bytes: usize) -> bool {
    text.len() <= max_bytes && !text.chars().any(char::is_control)
}

impl AlbumView {
    fn valid(&self) -> bool {
        safe_text(&self.title, NAME_MAX_BYTES)
            && safe_text(&self.artist, NAME_MAX_BYTES)
            && safe_text(&self.cover, COVER_MAX_BYTES)
            && (self.year.is_empty() || (self.year.len() == 4 && self.year.bytes().all(|byte| byte.is_ascii_digit())))
    }
}

impl ArtistView {
    fn valid(&self) -> bool {
        safe_text(&self.name, NAME_MAX_BYTES) && safe_text(&self.about, ABOUT_MAX_BYTES) && safe_text(&self.cover, COVER_MAX_BYTES)
    }
}

impl PendingView {
    fn valid(&self) -> bool {
        safe_text(&self.title, NAME_MAX_BYTES) && safe_text(&self.artist, NAME_MAX_BYTES) && self.progress.is_none_or(|progress| progress <= 100)
    }
}

impl Answer {
    fn check(&self) -> Result<(), WireError> {
        match self {
            Self::FoundReleases { albums, singles } => {
                within(albums.len(), KEPT_ALBUMS)?;
                within(singles.len(), KEPT_ALBUMS)?;
                require(albums.iter().all(AlbumView::valid) && singles.iter().all(|single| safe_text(&single.title, NAME_MAX_BYTES) && i64::try_from(single.votes).is_ok()))
            }
            Self::FoundAny { albums } => {
                within(albums.len(), KEPT_ALBUMS)?;
                require(albums.iter().all(AlbumView::valid))
            }
            Self::FoundArtists { artists } => {
                within(artists.len(), KEPT_ARTISTS)?;
                require(artists.iter().all(ArtistView::valid))
            }
            Self::Tracked { artists } => {
                within(artists.len(), LIST_MAX_ITEMS)?;
                require(unique(artists))
            }
            Self::KnownArtist { artist } => require(artist.as_ref().is_none_or(|(_, name)| safe_text(name, NAME_MAX_BYTES))),
            Self::AlbumsOf { albums } => {
                within(albums.len(), LIST_MAX_ITEMS)?;
                require(albums.iter().all(|release| release.album.kind == Kind::Album && release.album.valid()))
            }
            Self::ReleasesOf { releases } => {
                within(releases.len(), LIST_MAX_ITEMS)?;
                require(releases.iter().all(|release| release.album.valid()))
            }
            Self::Listed { releases } => within(releases.len(), LIST_MAX_ITEMS),
            Self::Busy { artists, .. } => {
                within(artists.len(), BUSY_MAX_ARTISTS)?;
                require(unique(artists))
            }
            Self::Pending { albums } => {
                within(albums.len(), LIST_MAX_ITEMS)?;
                require(albums.iter().all(PendingView::valid))
            }
            _ => Ok(()),
        }
    }

    fn limit(&self) -> usize {
        match self {
            Self::FoundReleases { .. } | Self::FoundAny { .. } | Self::FoundArtists { .. }
            | Self::Tracked { .. } | Self::AlbumsOf { .. } | Self::ReleasesOf { .. }
            | Self::Listed { .. } | Self::Busy { .. } | Self::Pending { .. } => LIST_REPLY_MAX_BYTES,
            _ => SMALL_REPLY_MAX_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    TooLarge,
    Form,
    Bounds,
    Status,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooLarge => "Requests message is too large.",
            Self::Form => "Requests message is malformed.",
            Self::Bounds => "Requests message has a field out of bounds.",
            Self::Status => "The Nexus server did not return a Requests answer.",
        })
    }
}

/// The transport must hand in the complete body, with its declared length confirmed.
/// A sliced prefix of a larger body must never be passed here.
pub fn read_call(body: &[u8]) -> Result<Message, WireError> {
    let message: Message = decode(body, CALL_MAX_BYTES)?;
    message.check()?;
    Ok(message)
}

pub fn write_call(message: &Message) -> Result<Vec<u8>, WireError> {
    message.check()?;
    encode(message, CALL_MAX_BYTES)
}

// the design does not say which HTTP status to use for each typed answer. Follow
// the account contract: read a typed answer only at 200; never follow a redirect.
pub const REPLY_STATUS: u16 = 200;

pub fn read_answer(status: u16, body: &[u8]) -> Result<Answer, WireError> {
    if status != REPLY_STATUS {
        return Err(WireError::Status);
    }
    let answer: Answer = decode(body, LIST_REPLY_MAX_BYTES)?;
    within_body(body.len(), answer.limit())?;
    answer.check()?;
    Ok(answer)
}

pub fn write_answer(answer: &Answer) -> Result<Vec<u8>, WireError> {
    answer.check()?;
    encode(answer, answer.limit())
}

fn within_body(size: usize, limit: usize) -> Result<(), WireError> {
    if size <= limit { Ok(()) } else { Err(WireError::TooLarge) }
}

fn decode<T: de::DeserializeOwned>(body: &[u8], limit: usize) -> Result<T, WireError> {
    within_body(body.len(), limit)?;
    let text = std::str::from_utf8(body).map_err(|_| WireError::Form)?;
    let mut input = serde_json::Deserializer::from_str(text);
    // Read the whole tree, including fields the typed reader will pass over.
    de::DeserializeSeed::deserialize(Json(0), &mut input).map_err(|_| WireError::Form)?;
    input.end().map_err(|_| WireError::Form)?;
    let mut input = serde_json::Deserializer::from_str(text);
    let value = object(&mut input).map_err(|_| WireError::Form)?;
    input.end().map_err(|_| WireError::Form)?;
    Ok(value)
}

fn encode(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, WireError> {
    struct Bounded {
        bytes: Vec<u8>,
        limit: usize,
        full: bool,
    }

    impl Write for Bounded {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                self.full = true;
                return Err(io::ErrorKind::FileTooLarge.into());
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    let mut out = Bounded { bytes: Vec::new(), limit, full: false };
    serde_json::to_writer(&mut out, value).map_err(|_| if out.full { WireError::TooLarge } else { WireError::Form })?;
    Ok(out.bytes)
}

/// A first pass, like hello's, checking the structure without keeping its values.
struct Json(usize);

impl<'de> de::DeserializeSeed<'de> for Json {
    type Value = ();

    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> de::Visitor<'de> for Json {
    type Value = ();

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<(), E> { Ok(()) }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> { Ok(()) }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> { Ok(()) }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> { Ok(()) }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> { Ok(()) }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> { Ok(()) }

    fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        if self.0 >= HELLO_MAX_DEPTH {
            return Err(de::Error::custom("nested too deep"));
        }
        while seq.next_element_seed(Json(self.0 + 1))?.is_some() {}
        Ok(())
    }

    fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> Result<(), A::Error> {
        if self.0 >= HELLO_MAX_DEPTH {
            return Err(de::Error::custom("nested too deep"));
        }
        let mut keys = BTreeSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(de::Error::custom("a key given twice"));
            }
            map.next_value_seed(Json(self.0 + 1))?;
        }
        Ok(())
    }
}

// Serde's struct reader also accepts an array of positional fields. Require JSON
// objects wherever an object is named in the protocol.
fn object<'de, D: de::Deserializer<'de>, T: Deserialize<'de>>(input: D) -> Result<T, D::Error> {
    struct ObjectVisitor<T>(std::marker::PhantomData<T>);

    impl<'de, T: Deserialize<'de>> de::Visitor<'de> for ObjectVisitor<T> {
        type Value = T;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a JSON object")
        }

        fn visit_map<A: de::MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
            T::deserialize(de::value::MapAccessDeserializer::new(map))
        }
    }

    input.deserialize_map(ObjectVisitor(std::marker::PhantomData))
}

fn objects<'de, D: de::Deserializer<'de>, T: Deserialize<'de>>(input: D) -> Result<Vec<T>, D::Error> {
    #[derive(Deserialize)]
    #[serde(transparent, bound(deserialize = "T: Deserialize<'de>"))]
    struct Object<T>(#[serde(deserialize_with = "object")] T);

    Vec::<Object<T>>::deserialize(input).map(|items| items.into_iter().map(|item| item.0).collect())
}

// --- Server rules --------------------------------------------------------------------------

/// The per-account switch. Authentication, including whether this is an account session
/// at all, is decided before applying it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestsAccess {
    On,
    Off,
}

impl Default for RequestsAccess {
    fn default() -> Self {
        Self::On
    }
}

impl RequestsAccess {
    pub fn check(self) -> Result<(), Refusal> {
        match self {
            Self::On => Ok(()),
            Self::Off => Err(Refusal::SwitchedOff),
        }
    }
}

/// The caller supplies the authenticated account and role and the ownership recorded
/// for this album. the design does not say who may cancel an unowned album or what
/// happens if several accounts asked for it. Admin may act regardless; a member must
/// appear among the album's recorded requesters. Apply the switch first for every call.
pub fn may_cancel<Account: Eq>(access: RequestsAccess, account: &Account, admin: bool, requesters: &[Account]) -> Result<(), Refusal> {
    access.check()?;
    if admin || requesters.contains(account) { Ok(()) } else { Err(Refusal::NotAllowed) }
}

/// One book per authenticated session, discarded when that session ends. S13 must
/// select this book by the authenticated session, never by data in a request.
#[derive(Default)]
pub struct SessionOffers {
    albums: VecDeque<Timed<AlbumView>>,
    artists: VecDeque<Timed<ArtistView>>,
}

struct Timed<T> {
    id: MusicBrainzId,
    offered_at: i64,
    view: T,
}

impl<T> Timed<T> {
    fn fresh(&self, now: i64) -> bool {
        now.checked_sub(self.offered_at).is_some_and(|age| (0..OFFER_LIFETIME_SECONDS).contains(&age))
    }
}

fn remember<T>(book: &mut VecDeque<Timed<T>>, max: usize, id: MusicBrainzId, view: T, now: i64) {
    // the design does not say how to handle repeats or the exact expiry boundary.
    // Reoffering renews and moves a name to the back; an hour old is already gone.
    book.retain(|old| old.id != id && old.fresh(now));
    if book.len() >= max {
        book.pop_front();
    }
    book.push_back(Timed { id, offered_at: now, view });
}

fn resolve<'a, T>(book: &'a VecDeque<Timed<T>>, id: &MusicBrainzId, now: i64) -> Result<&'a T, Refusal> {
    book.iter().find(|offer| &offer.id == id && offer.fresh(now)).map(|offer| &offer.view).ok_or(Refusal::SearchAgain)
}

impl SessionOffers {
    /// Record exactly what was returned in a successfully encoded answer, with any
    /// missing artist ID filled from backend records before making it actionable.
    /// Time is monotonic seconds supplied by the server.
    pub fn remember_album(&mut self, album: AlbumView, now: i64) -> Result<(), WireError> {
        require(album.valid() && album.artist_id.is_some())?;
        let id = album.album.0.clone();
        remember(&mut self.albums, KEPT_ALBUMS, id, album, now);
        Ok(())
    }

    pub fn remember_artist(&mut self, artist: ArtistView, now: i64) -> Result<(), WireError> {
        require(artist.valid())?;
        remember(&mut self.artists, KEPT_ARTISTS, artist.id.clone(), artist, now);
        Ok(())
    }

    pub fn album(&self, name: &OfferName, now: i64) -> Result<&AlbumView, Refusal> {
        resolve(&self.albums, &name.0, now)
    }

    pub fn artist(&self, id: &MusicBrainzId, now: i64) -> Result<&ArtistView, Refusal> {
        resolve(&self.artists, id, now)
    }
}

impl TryFrom<Offer> for AlbumView {
    type Error = WireError;

    fn try_from(offer: Offer) -> Result<Self, WireError> {
        let kind = match offer.kind() {
            "Album" => Kind::Album,
            "EP" => Kind::Ep,
            "Single" => Kind::Single,
            _ => return Err(WireError::Bounds),
        };
        let view = Self {
            album: OfferName(offer.album.try_into()?),
            title: offer.title,
            artist_id: if offer.artist_id.is_empty() { None } else { Some(offer.artist_id.try_into()?) },
            artist: offer.artist,
            year: offer.year,
            cover: offer.cover,
            requested: offer.requested,
            kind,
            soundtrack: offer.soundtrack,
        };
        require(view.valid())?;
        Ok(view)
    }
}

impl From<AlbumView> for Offer {
    fn from(view: AlbumView) -> Self {
        Self {
            album: view.album.0.into(),
            artist_id: view.artist_id.map(String::from).unwrap_or_default(),
            title: view.title,
            artist: view.artist,
            year: view.year,
            cover: view.cover,
            requested: view.requested,
            kind: match view.kind {
                Kind::Album => "",
                Kind::Ep => "EP",
                Kind::Single => "Single",
            }.into(),
            soundtrack: view.soundtrack,
        }
    }
}

impl TryFrom<ArtistOffer> for ArtistView {
    type Error = WireError;

    fn try_from(offer: ArtistOffer) -> Result<Self, WireError> {
        let view = Self { id: offer.id.try_into()?, name: offer.name, about: offer.about, cover: offer.cover };
        require(view.valid())?;
        Ok(view)
    }
}

impl From<ArtistView> for ArtistOffer {
    fn from(view: ArtistView) -> Self {
        Self { id: view.id.into(), name: view.name, about: view.about, cover: view.cover }
    }
}

impl TryFrom<Single> for SingleView {
    type Error = WireError;

    fn try_from(single: Single) -> Result<Self, WireError> {
        require(safe_text(&single.title, NAME_MAX_BYTES))?;
        Ok(Self { title: single.title, artist_id: single.artist_id.try_into()?, votes: u64::try_from(single.votes).map_err(|_| WireError::Bounds)? })
    }
}

impl From<SingleView> for Single {
    fn from(view: SingleView) -> Self {
        Self { title: view.title, artist_id: view.artist_id.into(), votes: i64::try_from(view.votes).unwrap_or(i64::MAX) }
    }
}

impl TryFrom<Listed> for ListedView {
    type Error = WireError;

    fn try_from(listed: Listed) -> Result<Self, WireError> {
        Ok(Self { album: listed.album.try_into()?, id: listed.id.try_into()?, wanted: listed.wanted, have: listed.have })
    }
}

impl From<ListedView> for Listed {
    fn from(view: ListedView) -> Self {
        Self { album: view.album.into(), id: view.id.into(), wanted: view.wanted, have: view.have }
    }
}

impl TryFrom<Pending> for PendingView {
    type Error = WireError;

    fn try_from(pending: Pending) -> Result<Self, WireError> {
        let view = Self { id: pending.id.try_into()?, album: pending.album.try_into()?, title: pending.title, artist: pending.artist, progress: pending.progress };
        require(view.valid())?;
        Ok(view)
    }
}

impl From<PendingView> for Pending {
    fn from(view: PendingView) -> Self {
        Self { id: view.id.into(), album: view.album.into(), title: view.title, artist: view.artist, progress: view.progress }
    }
}

// --- App stand-in --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    Stopped,
    Unreachable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestError {
    Wire(WireError),
    Transport(TransportError),
    Refused(Refusal),
    UnexpectedAnswer,
}

impl From<WireError> for RequestError {
    fn from(error: WireError) -> Self { Self::Wire(error) }
}

impl From<TransportError> for RequestError {
    fn from(error: TransportError) -> Self { Self::Transport(error) }
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Wire(error) => error.fmt(f),
            Self::Transport(TransportError::Stopped) => f.write_str(crate::http::STOPPED),
            Self::Transport(TransportError::Unreachable) => f.write_str("Couldn't reach the Nexus server."),
            Self::Refused(reason) => f.write_str(reason.sentence()),
            Self::UnexpectedAnswer => f.write_str("The Nexus server answered a different request."),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Working,
    Done(Completed),
    Failed(Refusal),
}

pub struct TransportReply {
    pub status: u16,
    pub body: Vec<u8>,
}

type SendCall<'a> = dyn Fn(&str, &[u8]) -> Result<TransportReply, TransportError> + Send + Sync + 'a;
type Pause<'a> = dyn Fn(Duration) -> Result<(), TransportError> + Send + Sync + 'a;
type NewId<'a> = dyn Fn() -> Result<RequestId, TransportError> + Send + Sync + 'a;

/// The app supplies a POST transport, a cancellable wait and a source of action IDs.
/// The transport pins the backend to the established origin and ID, supplies the
/// session independently of the message, and returns only complete bodies.
pub struct BackendLidarr<'a> {
    head: Head,
    send: Box<SendCall<'a>>,
    pause: Box<Pause<'a>>,
    new_id: Box<NewId<'a>>,
}

// the design does not say how often to poll. Leave the wait/cancellation to the
// app, with one second between asks while a job is working.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);

impl<'a> BackendLidarr<'a> {
    pub fn new(
        head: Head,
        send: impl Fn(&str, &[u8]) -> Result<TransportReply, TransportError> + Send + Sync + 'a,
        pause: impl Fn(Duration) -> Result<(), TransportError> + Send + Sync + 'a,
        new_id: impl Fn() -> Result<RequestId, TransportError> + Send + Sync + 'a,
    ) -> Self {
        Self { head, send: Box::new(send), pause: Box::new(pause), new_id: Box::new(new_id) }
    }

    fn ask<T>(&self, call: Call, take: impl FnOnce(Answer) -> Option<T>) -> Result<T, RequestError> {
        let body = write_call(&Message { head: self.head, call })?;
        let answer = (self.send)(REQUESTS, &body)?;
        match read_answer(answer.status, &answer.body)? {
            Answer::Refused { reason } => Err(RequestError::Refused(reason)),
            answer => take(answer).ok_or(RequestError::UnexpectedAnswer),
        }
    }

    pub fn status(&self) -> Result<Availability, RequestError> {
        self.ask(Call::Status, |answer| match answer {
            Answer::Status { lidarr } => Some(lidarr),
            _ => None,
        })
    }

    pub fn albums_of(&self, artist: LidarrNumber) -> Result<Vec<ReleaseView>, RequestError> {
        self.ask(Call::AlbumsOf { artist }, |answer| match answer {
            Answer::AlbumsOf { albums } => Some(albums),
            _ => None,
        })
    }

    pub fn listed(&self, artist: LidarrNumber) -> Result<Vec<ListedView>, RequestError> {
        self.ask(Call::Listed { artist }, |answer| match answer {
            Answer::Listed { releases } => Some(releases),
            _ => None,
        })
    }

    pub fn busy(&self, artists: Vec<LidarrNumber>) -> Result<(Vec<LidarrNumber>, u64), RequestError> {
        self.ask(Call::Busy { artists }, |answer| match answer {
            Answer::Busy { artists, stamp } => Some((artists, stamp)),
            _ => None,
        })
    }

    /// The same ID and change may be sent again after an ambiguous transport failure.
    /// The server must enforce the retry receipt and match its contents (§12.2).
    pub fn begin(&self, request_id: RequestId, change: Change) -> Result<JobId, RequestError> {
        self.ask(Call::Begin { request_id, change }, |answer| match answer {
            Answer::Begun { job } => Some(job),
            _ => None,
        })
    }

    pub fn after(&self, job: JobId) -> Result<Progress, RequestError> {
        self.ask(Call::After { job }, |answer| match answer {
            Answer::Working => Some(Progress::Working),
            Answer::Done { result } => Some(Progress::Done(result)),
            Answer::Failed { reason } => Some(Progress::Failed(reason)),
            _ => None,
        })
    }

    /// Run an action through to its answer. Callers that need to resume after losing
    /// a connection can keep their own request ID and use begin/after directly.
    pub fn change(&self, request_id: RequestId, change: Change) -> Result<Completed, RequestError> {
        let job = self.begin(request_id, change.clone())?;
        loop {
            match self.after(job)? {
                Progress::Working => (self.pause)(POLL_INTERVAL)?,
                Progress::Failed(reason) => return Err(RequestError::Refused(reason)),
                Progress::Done(result) => return if result.answers(&change) { Ok(result) } else { Err(RequestError::UnexpectedAnswer) },
            }
        }
    }
}

impl Completed {
    fn answers(&self, change: &Change) -> bool {
        matches!((self, change),
            (Self::Requested, Change::Request { .. })
            | (Self::ArtistAdded { .. }, Change::AddArtist { .. })
            | (Self::AddedWanting { .. }, Change::AddWanting { .. })
            | (Self::Wanted, Change::Want { .. })
            | (Self::ListedSingles, Change::ListSingles { .. })
            | (Self::Cancelled, Change::Cancel { .. })
            | (Self::Batch, Change::Batch { .. }))
    }
}

impl RequestLidarr for BackendLidarr<'_> {
    fn find_releases(&self, term: &str) -> Result<(Vec<Offer>, Vec<Single>), String> {
        let search = Search::new(term).ok_or_else(|| WireError::Bounds.to_string())?;
        self.ask(Call::FindReleases { search }, |answer| match answer {
            Answer::FoundReleases { albums, singles } => Some((albums.into_iter().map(Offer::from).collect(), singles.into_iter().map(Single::from).collect())),
            _ => None,
        }).map_err(|error| error.to_string())
    }

    fn find_any(&self, term: &str) -> Result<Vec<Offer>, String> {
        let search = Search::new(term).ok_or_else(|| WireError::Bounds.to_string())?;
        self.ask(Call::FindAny { search }, |answer| match answer {
            Answer::FoundAny { albums } => Some(albums.into_iter().map(Offer::from).collect()),
            _ => None,
        }).map_err(|error| error.to_string())
    }

    fn find_artists(&self, term: &str) -> Result<Vec<ArtistOffer>, String> {
        let search = Search::new(term).ok_or_else(|| WireError::Bounds.to_string())?;
        self.ask(Call::FindArtists { search }, |answer| match answer {
            Answer::FoundArtists { artists } => Some(artists.into_iter().map(ArtistOffer::from).collect()),
            _ => None,
        }).map_err(|error| error.to_string())
    }

    fn tracked(&self) -> Result<HashSet<String>, String> {
        self.ask(Call::Tracked, |answer| match answer {
            Answer::Tracked { artists } => Some(artists.into_iter().map(String::from).collect()),
            _ => None,
        }).map_err(|error| error.to_string())
    }

    fn known_artist(&self, artist: &str) -> Result<Option<(i64, String)>, String> {
        let artist = MusicBrainzId::new(artist).ok_or_else(|| WireError::Bounds.to_string())?;
        self.ask(Call::KnownArtist { artist }, |answer| match answer {
            Answer::KnownArtist { artist } => Some(artist.map(|(number, name)| (number.into(), name))),
            _ => None,
        }).map_err(|error| error.to_string())
    }

    fn releases_of(&self, artist: i64, name: &str) -> Result<Vec<(Offer, bool)>, String> {
        let artist = LidarrNumber::try_from(artist).map_err(|error| error.to_string())?;
        self.ask(Call::ReleasesOf { artist }, |answer| match answer {
            Answer::ReleasesOf { releases } => Some(releases.into_iter().map(|release| {
                let mut album = Offer::from(release.album);
                // Like the direct implementation, use the artist page's display name.
                album.artist = name.to_owned();
                (album, release.have)
            }).collect()),
            _ => None,
        }).map_err(|error| error.to_string())
    }

    fn request(&self, wanted: &Offer, _: &dyn WayBack) -> Result<(), String> {
        let offer = MusicBrainzId::new(&wanted.album).map(OfferName).ok_or_else(|| Refusal::SearchAgain.sentence().to_owned())?;
        let request_id = (self.new_id)().map_err(|error| RequestError::from(error).to_string())?;
        // The backend resolves the name and owns the write-ahead recovery. None of
        // the local offer description or WayBack leaves this device.
        self.change(request_id, Change::Request { offer }).map(drop).map_err(|error| error.to_string())
    }

    fn pending(&self) -> Result<Vec<Pending>, String> {
        self.ask(Call::Pending, |answer| match answer {
            Answer::Pending { albums } => Some(albums.into_iter().map(Pending::from).collect()),
            _ => None,
        }).map_err(|error| error.to_string())
    }
}
