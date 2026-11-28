//! The app's Requests calls and what the Nexus server says back (§16.1).
//! The transport owns authentication, backend identity, and complete HTTP messages.
//! A change's request ID belongs to the action and must be reused if its begin is retried.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashSet, VecDeque};
use std::fmt;
use std::num::NonZeroU64;
use std::time::Duration;

use serde::{Deserialize, Serialize, de};
use serde_json::value::RawValue;

use super::account::{self, AccountId, Receipt};
use super::wire::{self, BadMessage, Fields, Value, WireBody};
pub use super::account::{RequestHead, RequestId};
use crate::lidarr::{self, ArtistOffer, Listed, Pending, RequestLidarr, Single, WayBack};
use crate::offer::Offer;

// the design does not say how to spell the path and JSON tags. One POST with a closed call tag.
pub const REQUESTS: &str = "/nexus/api/requests";
pub const PATHS: &[&str] = &[REQUESTS];
pub const CALL_MAX_BYTES: usize = wire::BODY_MAX_BYTES;
pub const SMALL_REPLY_MAX_BYTES: usize = account::SMALL_REPLY_MAX_BYTES;
// The lead's ruling on §7.1 and §16.1: keep the small ceiling for status, jobs and
// errors. A successful list has its own bounds and is refused whole when over them.
pub const LIST_REPLY_MAX_BYTES: usize = account::LIST_REPLY_MAX_BYTES;
pub const PAGE_MAX_ITEMS: usize = 500;
pub const NUMBER_PAGE_MAX_ITEMS: usize = 5000;
pub const ITEM_MAX_BYTES: usize = 1024;
// the design does not say how long snapshots stay live or how many to keep.
pub const SNAPSHOTS_PER_SESSION: usize = 4;
pub const SNAPSHOT_IDLE_SECONDS: u64 = 5 * 60;
pub const SEARCH_MAX_CHARS: usize = 200;
pub const BUSY_MAX_ARTISTS: usize = 500;
pub const BATCH_MAX_LINES: usize = 2000;
pub const RUNNING_PER_ACCOUNT: usize = 1;
pub const RUNNING_MAX: usize = 4;
pub const WAITING_MAX: usize = 16;
pub const KEPT_ALBUMS: usize = 500;
pub const KEPT_ARTISTS: usize = 200;
pub const OFFER_LIFETIME_SECONDS: i64 = 3600;

// the design does not say the total list or individual text field ceilings. Use
// Lidarr's existing 100 x 500 pending bound for any complete list. Every text field
// is subject to the combined per-item ceiling as well.
pub const LIST_MAX_ITEMS: usize = 50_000;
pub const NAME_MAX_BYTES: usize = ITEM_MAX_BYTES;
pub const ABOUT_MAX_BYTES: usize = ITEM_MAX_BYTES;
pub const COVER_MAX_BYTES: usize = ITEM_MAX_BYTES;
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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OfferName(pub MusicBrainzId);

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Search(String);

impl Search {
    pub fn new(text: &str) -> Option<Self> {
        valid_search(text).then(|| Self(text.to_owned()))
    }

    /// Keep the device's ordinary search usable while the wire stays strict.
    fn from_input(text: &str) -> Result<Option<Self>, String> {
        let text: String = text.chars().take(SEARCH_MAX_CHARS)
            .map(|ch| if ch.is_control() { ' ' } else { ch }).collect();
        if text.trim().is_empty() {
            return Ok(None);
        }
        Self::try_from(text).map(Some).map_err(|_| "Search by album or artist name, or use a valid MusicBrainz ID.".into())
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
        !reserved_lookup(text) || text.strip_prefix("lidarr:").is_some_and(is_mbid)
    }
}

fn reserved_lookup(text: &str) -> bool {
    text.trim_start().get(..7).is_some_and(|prefix| prefix.eq_ignore_ascii_case("lidarr:"))
}

fn valid_name_to_find(text: &str) -> bool {
    !text.trim().is_empty()
        && text.chars().take(SEARCH_MAX_CHARS + 1).count() <= SEARCH_MAX_CHARS
        && !text.chars().any(char::is_control)
}

/// Authentication stays in the transport header; use the same head as C7.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Message {
    pub head: RequestHead,
    pub call: Call,
}

impl Value for Message {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let fields = Fields::read(raw)?;
        Ok(Self { head: fields.get("head")?, call: fields.get("call")? })
    }

    fn check(&self) -> Result<(), BadMessage> {
        self.call.check().map_err(Into::into)
    }
}

/// the design does not say how to name a page's place. The backend assigns a
/// snapshot number. A continuation advances to the next offset in that same
/// snapshot. The first call has no cursor and starts from offset zero. Keep at
/// most [`SNAPSHOTS_PER_SESSION`] per session. Expire an idle snapshot after
/// [`SNAPSHOT_IDLE_SECONDS`] and answer [`Refusal::SearchAgain`] for an expired cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "(NonZeroU64, u32)", into = "(NonZeroU64, u32)")]
pub struct PageCursor {
    snapshot: NonZeroU64,
    offset: u32,
}

impl PageCursor {
    pub fn new(snapshot: NonZeroU64, offset: u32) -> Option<Self> {
        usize::try_from(offset).ok().filter(|&offset| (1..LIST_MAX_ITEMS).contains(&offset))?;
        Some(Self { snapshot, offset })
    }

    pub fn snapshot(self) -> NonZeroU64 { self.snapshot }

    pub fn offset(self) -> u32 { self.offset }
}

impl TryFrom<(NonZeroU64, u32)> for PageCursor {
    type Error = WireError;

    fn try_from((snapshot, offset): (NonZeroU64, u32)) -> Result<Self, WireError> {
        Self::new(snapshot, offset).ok_or(WireError::Bounds)
    }
}

impl From<PageCursor> for (NonZeroU64, u32) {
    fn from(cursor: PageCursor) -> Self { (cursor.snapshot, cursor.offset) }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "ask", rename_all = "snake_case")]
pub enum Call {
    Status,
    FindReleases { search: Search, after: Option<PageCursor> },
    FindAny { search: Search, after: Option<PageCursor> },
    FindArtists { search: Search, after: Option<PageCursor> },
    Tracked { after: Option<PageCursor> },
    ArtistNumbers { after: Option<PageCursor> },
    KnownArtist { artist: MusicBrainzId },
    AlbumsOf { artist: LidarrNumber, after: Option<PageCursor> },
    ReleasesOf { artist: LidarrNumber, after: Option<PageCursor> },
    Listed { artist: LidarrNumber, after: Option<PageCursor> },
    Busy { artists: Vec<LidarrNumber> },
    Pending { after: Option<PageCursor> },
    BatchLines { job: JobId, after: Option<PageCursor> },
    Begin { request_id: RequestId, change: Change },
    After { job: JobId },
}

impl Value for Call {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let fields = Fields::read(raw)?;
        let tag: String = member(&fields, "ask")?;
        let after = || member(&fields, "after");
        let search = || member(&fields, "search");
        let number = || member(&fields, "artist");
        let call = match tag.as_str() {
            "status" => Self::Status,
            "find_releases" => Self::FindReleases { search: search()?, after: after()? },
            "find_any" => Self::FindAny { search: search()?, after: after()? },
            "find_artists" => Self::FindArtists { search: search()?, after: after()? },
            "tracked" => Self::Tracked { after: after()? },
            "artist_numbers" => Self::ArtistNumbers { after: after()? },
            "known_artist" => Self::KnownArtist { artist: member(&fields, "artist")? },
            "albums_of" => Self::AlbumsOf { artist: number()?, after: after()? },
            "releases_of" => Self::ReleasesOf { artist: number()?, after: after()? },
            "listed" => Self::Listed { artist: number()?, after: after()? },
            "busy" => Self::Busy { artists: member(&fields, "artists")? },
            "pending" => Self::Pending { after: after()? },
            "batch_lines" => Self::BatchLines { job: member(&fields, "job")?, after: after()? },
            "begin" => Self::Begin { request_id: fields.get("request_id")?, change: object_json(raw_member(&fields, "change")?)? },
            "after" => Self::After { job: member(&fields, "job")? },
            _ => return Err(BadMessage::Form),
        };
        call.check().map_err(BadMessage::from)?;
        Ok(call)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum Change {
    Request { offer: OfferName },
    AddArtist { artist: MusicBrainzId },
    AddWanting { offers: Vec<OfferName> },
    Want { artist: LidarrNumber, albums: Vec<LidarrNumber> },
    ListSingles { artist: LidarrNumber, offer: OfferName },
    Cancel { album: LidarrNumber, release: MusicBrainzId },
    Batch {
        singles: bool,
        #[serde(deserialize_with = "objects")]
        lines: Vec<BatchLine>,
    },
}

/// A playlist line is deliberately just three names. It goes through bulk's lookup
/// rules on the server. It cannot carry a playlist ID, a Lidarr setting or an album body.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchLine {
    pub artist: String,
    pub album: String,
    pub song: String,
}

impl BatchLine {
    fn valid(&self) -> bool {
        // An imported playlist can omit the album. The released bulk lookup then
        // finds it via the song. Keep the field present and use an empty string.
        valid_name_to_find(&self.artist)
            && valid_name_to_find(&self.song)
            && (self.album.is_empty() || (valid_name_to_find(&self.album)
                // the design does not say whether a batch album title can select a
                // reserved lookup. Bulk prefixes the search with this title, as is
                // and undecorated. Only ordinary name searches belong here.
                && !reserved_lookup(&self.album)
                && !reserved_lookup(&crate::text::undecorated(&self.album))))
    }
}

fn within(count: usize, max: usize) -> Result<(), WireError> {
    require(count <= max)
}

fn require(valid: bool) -> Result<(), WireError> {
    if valid { Ok(()) } else { Err(WireError::Bounds) }
}

fn unique<T: Ord>(items: &[T]) -> bool {
    let mut seen = BTreeSet::new();
    items.iter().all(|item| seen.insert(item))
}

fn batch_offset(cursor: PageCursor) -> Option<usize> {
    usize::try_from(cursor.offset()).ok().filter(|offset| *offset < BATCH_MAX_LINES)
}

impl Call {
    fn check(&self) -> Result<(), WireError> {
        match self {
            Self::Busy { artists } => {
                within(artists.len(), BUSY_MAX_ARTISTS)?;
                require(unique(artists))
            }
            Self::BatchLines { after, .. } => {
                require(after.is_none_or(|cursor| batch_offset(cursor).is_some()))
            }
            Self::Begin { change, .. } => change.check(),
            _ => Ok(()),
        }
    }
}

impl wire::Message for Message {}

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
            Self::Batch { lines, .. } => {
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
    AskedByOthers,
    NotSignedIn,
    StartAgain,
    Malformed,
    Incompatible,
    TooLarge,
    LidarrUnavailable,
    LidarrRefused,
    NotAmongReleases,
    WouldNotKeep,
    NoLongerListed,
    LidarrNotSetUp,
    LidarrNoProfiles,
    LidarrSinglesProfileNoNumber,
    LidarrSoundtracksProfileNoNumber,
    LidarrBehindSignIn,
    LidarrSignInRejected,
    LidarrKeyRefused,
    LidarrUnrecognized,
    LidarrCouldNotRestore,
    JobNotFound,
    NotDealtWith,
}

impl Refusal {
    /// Both apps render the same code. For an album request they already have
    /// the display title and artist. Neither value has to come from an error reply.
    pub fn sentence(self, wanted: Option<&Offer>) -> Cow<'static, str> {
        match (self, wanted) {
            (Self::NotAmongReleases, Some(offer)) => return lidarr::not_among_releases(offer).into(),
            (Self::WouldNotKeep, Some(offer)) => return lidarr::would_not_keep(offer).into(),
            (Self::LidarrCouldNotRestore, Some(offer)) => return lidarr::could_not_restore(offer).into(),
            _ => {}
        }
        Cow::Borrowed(match self {
            Self::SwitchedOff => "Requests are switched off for you.",
            Self::SearchAgain => "This offer has gone. Search again.",
            Self::Busy => "Requests are busy. Try again in a moment.",
            Self::NotAllowed => "You can't make that request.",
            Self::AskedByOthers => "Another account asked for this album. Ask an admin to take it off the list.",
            Self::NotSignedIn => "Sign in to Nexus Music again.",
            Self::StartAgain => "The server's data was put back. Sign in again.",
            Self::Malformed => "The request wasn't understood.",
            Self::Incompatible => "Update Nexus Music to make this request.",
            Self::TooLarge => "There's too much to show. Try a narrower search.",
            Self::LidarrUnavailable => "The Nexus server couldn't reach Lidarr.",
            Self::LidarrRefused => "Lidarr answered with an error. Try again in a moment.",
            Self::NotAmongReleases => "Lidarr doesn't list that album among the artist's releases.",
            Self::WouldNotKeep => "Lidarr wouldn't keep that album on its list. Try again in a minute.",
            Self::NoLongerListed => crate::requests::NO_LONGER_LISTED,
            Self::LidarrNotSetUp => lidarr::NO_MUSIC_FOLDER,
            Self::LidarrNoProfiles => lidarr::NO_PROFILES,
            Self::LidarrSinglesProfileNoNumber => lidarr::SINGLES_PROFILE_NO_NUMBER,
            Self::LidarrSoundtracksProfileNoNumber => lidarr::SOUNDTRACKS_PROFILE_NO_NUMBER,
            Self::LidarrBehindSignIn => lidarr::NEEDED,
            Self::LidarrSignInRejected => lidarr::SIGN_IN_REJECTED,
            Self::LidarrKeyRefused => lidarr::KEY_REFUSED,
            Self::LidarrUnrecognized => lidarr::UNRECOGNIZED_LIDARR,
            Self::LidarrCouldNotRestore => "The album was requested, but Lidarr's new releases setting couldn't be restored.",
            Self::JobNotFound => "The Nexus server no longer has that request.",
            Self::NotDealtWith => "The Nexus server couldn't deal with the request.",
        })
    }
}

/// Classify an existing Lidarr error before reporting or logging the outcome.
/// `wanted` is the server's own offer, when the operation concerned one album.
/// The bulk worker can store the same reason without its final period.
pub fn refusal_for(error: &str, wanted: Option<&Offer>) -> Refusal {
    let error = if error.ends_with('.') { Cow::Borrowed(error) } else { Cow::Owned(format!("{error}.")) };
    match error.as_ref() {
        crate::requests::NO_LONGER_LISTED => Refusal::NoLongerListed,
        lidarr::NO_MUSIC_FOLDER => Refusal::LidarrNotSetUp,
        lidarr::NO_PROFILES => Refusal::LidarrNoProfiles,
        lidarr::SINGLES_PROFILE_NO_NUMBER => Refusal::LidarrSinglesProfileNoNumber,
        lidarr::SOUNDTRACKS_PROFILE_NO_NUMBER => Refusal::LidarrSoundtracksProfileNoNumber,
        lidarr::NEEDED => Refusal::LidarrBehindSignIn,
        lidarr::SIGN_IN_REJECTED => Refusal::LidarrSignInRejected,
        lidarr::KEY_REFUSED => Refusal::LidarrKeyRefused,
        lidarr::UNRECOGNIZED_LIDARR => Refusal::LidarrUnrecognized,
        error if error.starts_with("Couldn't reach Lidarr.") => Refusal::LidarrUnavailable,
        error if wanted.is_some_and(|offer| error == lidarr::not_among_releases(offer)) => Refusal::NotAmongReleases,
        error if wanted.is_some_and(|offer| error == lidarr::would_not_keep(offer)) => Refusal::WouldNotKeep,
        error if wanted.is_some_and(|offer| error == lidarr::could_not_restore(offer)) => Refusal::LidarrCouldNotRestore,
        _ => Refusal::LidarrRefused,
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "answer", rename_all = "snake_case")]
pub enum Answer {
    Status { lidarr: Availability },
    FoundReleases {
        #[serde(deserialize_with = "objects")]
        albums: Vec<AlbumView>,
        #[serde(deserialize_with = "objects")]
        singles: Vec<SingleView>,
        #[serde(deserialize_with = "Deserialize::deserialize")]
        next: Option<PageCursor>,
    },
    FoundAny {
        #[serde(deserialize_with = "objects")]
        albums: Vec<AlbumView>,
        #[serde(deserialize_with = "Deserialize::deserialize")]
        next: Option<PageCursor>,
    },
    FoundArtists {
        #[serde(deserialize_with = "objects")]
        artists: Vec<ArtistView>,
        #[serde(deserialize_with = "Deserialize::deserialize")]
        next: Option<PageCursor>,
    },
    Tracked { artists: Vec<MusicBrainzId>, #[serde(deserialize_with = "Deserialize::deserialize")] next: Option<PageCursor> },
    ArtistNumbers { artists: Vec<(MusicBrainzId, LidarrNumber)>, #[serde(deserialize_with = "Deserialize::deserialize")] next: Option<PageCursor> },
    KnownArtist {
        #[serde(deserialize_with = "Deserialize::deserialize")]
        artist: Option<(LidarrNumber, String)>,
    },
    AlbumsOf {
        #[serde(deserialize_with = "objects")]
        albums: Vec<ReleaseView>,
        #[serde(deserialize_with = "Deserialize::deserialize")]
        next: Option<PageCursor>,
    },
    ReleasesOf {
        #[serde(deserialize_with = "objects")]
        releases: Vec<ReleaseView>,
        #[serde(deserialize_with = "Deserialize::deserialize")]
        next: Option<PageCursor>,
    },
    Listed {
        #[serde(deserialize_with = "objects")]
        releases: Vec<ListedView>,
        #[serde(deserialize_with = "Deserialize::deserialize")]
        next: Option<PageCursor>,
    },
    /// the design does not say how to fit 500 64-bit numbers in a small answer.
    /// Return their zero-based positions in the call's vector.
    Busy { positions: Vec<u16>, stamp: u64 },
    Pending {
        #[serde(deserialize_with = "objects")]
        albums: Vec<PendingView>,
        #[serde(deserialize_with = "Deserialize::deserialize")]
        next: Option<PageCursor>,
    },
    BatchLines {
        #[serde(deserialize_with = "objects")]
        lines: Vec<BatchLineView>,
        #[serde(deserialize_with = "Deserialize::deserialize")]
        next: Option<PageCursor>,
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
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlbumView {
    pub album: OfferName,
    pub title: String,
    // the design does not say how to encode missing metadata from Lidarr. A release
    // listing can have no embedded artist; only an offer with an artist ID is actionable.
    #[serde(deserialize_with = "Deserialize::deserialize")]
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtistView {
    pub id: MusicBrainzId,
    pub name: String,
    pub about: String,
    pub cover: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SingleView {
    pub title: String,
    pub artist_id: MusicBrainzId,
    pub votes: i64,
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingView {
    pub id: LidarrNumber,
    pub album: MusicBrainzId,
    pub title: String,
    pub artist: String,
    /// Whether this authenticated account may take this row off Lidarr's list.
    pub removable: bool,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    pub progress: Option<u8>,
}

/// The backend reports progress against the original zero-based submitted index.
/// The app keeps its local playlist associations by that index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchLineView {
    pub index: u16,
    #[serde(deserialize_with = "object")]
    pub progress: BatchLineProgress,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BatchLineProgress {
    Waiting,
    Found { #[serde(deserialize_with = "object")] album: BatchAlbum },
    Asked { #[serde(deserialize_with = "object")] album: BatchAlbum },
    Done { #[serde(deserialize_with = "object")] album: BatchAlbum },
    NotFound,
    Failed { reason: Refusal },
}

/// The release found for the line, including what the app needs to match an arrival.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchAlbum {
    pub offer: OfferName,
    pub title: String,
    pub artist: String,
}

impl BatchLineView {
    fn valid(&self) -> bool {
        usize::from(self.index) < BATCH_MAX_LINES && match &self.progress {
            BatchLineProgress::Found { album } | BatchLineProgress::Asked { album }
            | BatchLineProgress::Done { album } => safe_text(&album.title, NAME_MAX_BYTES) && safe_text(&album.artist, NAME_MAX_BYTES),
            _ => true,
        }
    }
}

// Free text can contain a secret pasted into a search or returned by an upstream.
// Only the typed status and error codes are suitable for diagnostics.
macro_rules! redact_debug {
    ($($ty:ty),+ $(,)?) => {
        $(impl fmt::Debug for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct(stringify!($ty)).finish_non_exhaustive()
            }
        })+
    };
}

redact_debug!(Search, BatchLine, Answer, AlbumView, ArtistView, SingleView, PendingView, BatchAlbum);

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
    /// S13 applies this check to the current row before checking its owners and
    /// cancelling. The number alone may identify a different release now.
    pub fn check_cancel_target(&self, album: LidarrNumber, release: &MusicBrainzId) -> Result<(), Refusal> {
        if self.id == album && &self.album == release { Ok(()) } else { Err(Refusal::NoLongerListed) }
    }

    fn valid(&self) -> bool {
        safe_text(&self.title, NAME_MAX_BYTES) && safe_text(&self.artist, NAME_MAX_BYTES) && self.progress.is_none_or(|progress| progress <= 100)
    }
}

fn raw_member<'a>(fields: &Fields<'a>, key: &str) -> Result<&'a RawValue, BadMessage> {
    fields.0.get(key).copied().ok_or(BadMessage::Form)
}

fn member<T: de::DeserializeOwned>(fields: &Fields<'_>, key: &str) -> Result<T, BadMessage> {
    wire::plain_json(raw_member(fields, key)?.get())
}

fn check_item<T: Serialize>(item: &T) -> Result<(), WireError> {
    let bytes = serde_json::to_vec(item).map_err(|_| WireError::Form)?;
    within_body(bytes.len(), ITEM_MAX_BYTES)
}

fn check_items<T: Serialize>(items: &[T]) -> Result<(), WireError> {
    items.iter().try_for_each(check_item)
}

/// S13 converts Lidarr records individually before paging. An odd record, or one
/// whose encoded row exceeds the item ceiling, must not discard its neighbours.
pub fn lidarr_items<T: TryFrom<U> + Serialize, U>(items: impl IntoIterator<Item = U>) -> Vec<T> {
    items.into_iter().filter_map(|item| T::try_from(item).ok()).filter(|item| check_item(item).is_ok()).collect()
}

fn check_page(count: usize, limit: usize, next: Option<PageCursor>) -> Result<(), WireError> {
    within(count, limit)?;
    require(next.is_none_or(|next| count > 0 && usize::try_from(next.offset()).is_ok_and(|offset| offset >= count)))
}

impl Answer {
    fn check(&self) -> Result<(), WireError> {
        match self {
            Self::FoundReleases { albums, singles, next } => {
                check_page(albums.len().saturating_add(singles.len()), PAGE_MAX_ITEMS, *next)?;
                require(albums.iter().all(|album| album.kind == Kind::Album && album.valid())
                    && singles.iter().all(|single| safe_text(&single.title, NAME_MAX_BYTES) && single.votes >= 0))?;
                check_items(albums)?;
                check_items(singles)
            }
            Self::FoundAny { albums, next } => {
                check_page(albums.len(), PAGE_MAX_ITEMS, *next)?;
                require(albums.iter().all(AlbumView::valid))?;
                check_items(albums)
            }
            Self::FoundArtists { artists, next } => {
                check_page(artists.len(), PAGE_MAX_ITEMS, *next)?;
                require(artists.iter().all(ArtistView::valid))?;
                check_items(artists)
            }
            Self::Tracked { artists, next } => {
                check_page(artists.len(), PAGE_MAX_ITEMS, *next)?;
                require(unique(artists))
            }
            Self::ArtistNumbers { artists, next } => {
                check_page(artists.len(), NUMBER_PAGE_MAX_ITEMS, *next)?;
                require(unique(&artists.iter().map(|(id, _)| id).collect::<Vec<_>>()))
            }
            Self::KnownArtist { artist } => require(artist.as_ref().is_none_or(|(_, name)| safe_text(name, NAME_MAX_BYTES))),
            Self::AlbumsOf { albums, next } => {
                check_page(albums.len(), PAGE_MAX_ITEMS, *next)?;
                require(albums.iter().all(|release| release.album.kind == Kind::Album && release.album.valid()))?;
                check_items(albums)
            }
            Self::ReleasesOf { releases, next } => {
                check_page(releases.len(), PAGE_MAX_ITEMS, *next)?;
                require(releases.iter().all(|release| release.album.valid()))?;
                check_items(releases)
            }
            Self::Listed { releases, next } => check_page(releases.len(), PAGE_MAX_ITEMS, *next),
            Self::Busy { positions, .. } => {
                within(positions.len(), BUSY_MAX_ARTISTS)?;
                require(unique(positions) && positions.iter().all(|&at| usize::from(at) < BUSY_MAX_ARTISTS))
            }
            Self::Pending { albums, next } => {
                check_page(albums.len(), PAGE_MAX_ITEMS, *next)?;
                require(albums.iter().all(PendingView::valid))?;
                check_items(albums)
            }
            Self::BatchLines { lines, next } => {
                check_page(lines.len(), PAGE_MAX_ITEMS, *next)?;
                require(lines.iter().all(BatchLineView::valid)
                    && lines.windows(2).all(|rows| matches!(rows, [left, right] if left.index.checked_add(1) == Some(right.index)))
                    && next.is_none_or(|next| batch_offset(next).is_some_and(|offset| {
                        lines.last().is_some_and(|last| usize::from(last.index) + 1 == offset)
                    })))?;
                check_items(lines)
            }
            _ => Ok(()),
        }
    }

    fn list_fields(&self) -> &[&str] {
        match self {
            Self::FoundReleases { .. } => &["albums", "singles"],
            Self::FoundAny { .. } | Self::AlbumsOf { .. } | Self::Pending { .. } => &["albums"],
            Self::FoundArtists { .. } | Self::Tracked { .. } | Self::ArtistNumbers { .. } => &["artists"],
            Self::ReleasesOf { .. } | Self::Listed { .. } => &["releases"],
            Self::BatchLines { .. } => &["lines"],
            _ => &[],
        }
    }
}

impl wire::Value for Answer {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let answer: Self = object_json(raw)?;
        // Count unknown fields and whitespace inside each item's original JSON, too.
        let fields = Fields::read(raw)?;
        for key in answer.list_fields() {
            let items: Vec<&RawValue> = wire::plain_json(raw_member(&fields, key)?.get())?;
            if items.iter().any(|item| item.get().len() > ITEM_MAX_BYTES) {
                return Err(BadMessage::TooLarge);
            }
        }
        answer.check().map_err(BadMessage::from)?;
        Ok(answer)
    }
}

pub type Reply = account::Reply<Answer>;

impl account::ReplyResult for Answer {
    const MAX_DONE_BYTES: usize = LIST_REPLY_MAX_BYTES;

    fn agrees(&self, _: &Receipt) -> bool {
        self.check().is_ok()
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

impl From<BadMessage> for WireError {
    fn from(error: BadMessage) -> Self {
        match error {
            BadMessage::TooLarge => Self::TooLarge,
            BadMessage::Form => Self::Form,
            BadMessage::Status(_) => Self::Status,
        }
    }
}

impl From<WireError> for BadMessage {
    fn from(error: WireError) -> Self {
        match error {
            WireError::TooLarge => Self::TooLarge,
            _ => Self::Form,
        }
    }
}

/// The transport must hand in the complete body, with its declared length confirmed.
pub fn read_call(body: &[u8]) -> Result<Message, WireError> {
    wire::read(body).map_err(Into::into)
}

pub fn write_call(message: &Message) -> Result<WireBody, WireError> {
    message.call.check()?;
    wire::encode(message).map_err(Into::into)
}

pub fn read_reply(status: u16, body: &[u8]) -> Result<Reply, WireError> {
    let reply: Reply = account::read_reply(status, body)?;
    within_body(body.len(), reply_limit(&reply))?;
    Ok(reply)
}

pub fn write_reply(reply: &Reply) -> Result<WireBody, WireError> {
    if let Reply::Done { result, .. } = reply {
        result.check()?;
    }
    let bytes = wire::encode(reply)?;
    within_body(bytes.bytes().len(), reply_limit(reply))?;
    Ok(bytes)
}

fn reply_limit(reply: &Reply) -> usize {
    match reply {
        Reply::Done { result, .. } if !result.list_fields().is_empty() => LIST_REPLY_MAX_BYTES,
        _ => SMALL_REPLY_MAX_BYTES,
    }
}

fn within_body(size: usize, limit: usize) -> Result<(), WireError> {
    if size <= limit { Ok(()) } else { Err(WireError::TooLarge) }
}

fn object_json<T: de::DeserializeOwned>(raw: &RawValue) -> Result<T, BadMessage> {
    object(&mut serde_json::Deserializer::from_str(raw.get())).map_err(|_| BadMessage::Form)
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
/// for this album. Apply the switch first for every call. the design does not say
/// what happens if several accounts asked for it. A member may cancel only when
/// every recorded requester is that member, so the removal cannot affect another
/// account's request. An admin can cancel any album.
pub fn may_cancel(access: RequestsAccess, account: AccountId, admin: bool, requesters: &[AccountId]) -> Result<(), Refusal> {
    access.check()?;
    if admin {
        return Ok(());
    }
    if requesters.iter().any(|&requester| requester != account) {
        return Err(Refusal::AskedByOthers);
    }
    if requesters.is_empty() { Err(Refusal::NotAllowed) } else { Ok(()) }
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
    /// Record an offer from a successfully encoded answer, with any missing artist
    /// ID filled from backend records before making it actionable. Resolve and copy
    /// the offer into the queued job at begin so expiration cannot invalidate work
    /// already accepted. Time is monotonic seconds supplied by the server.
    pub fn remember_album(&mut self, album: AlbumView, now: i64) -> Result<(), WireError> {
        require(album.valid() && album.artist_id.is_some())?;
        check_items(std::slice::from_ref(&album))?;
        let id = album.album.0.clone();
        remember(&mut self.albums, KEPT_ALBUMS, id, album, now);
        Ok(())
    }

    pub fn remember_artist(&mut self, artist: ArtistView, now: i64) -> Result<(), WireError> {
        require(artist.valid())?;
        check_items(std::slice::from_ref(&artist))?;
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
        require(safe_text(&single.title, NAME_MAX_BYTES) && single.votes >= 0)?;
        Ok(Self { title: single.title, artist_id: single.artist_id.try_into()?, votes: single.votes })
    }
}

impl From<SingleView> for Single {
    fn from(view: SingleView) -> Self {
        Self { title: view.title, artist_id: view.artist_id.into(), votes: view.votes }
    }
}

impl TryFrom<(Offer, bool)> for ReleaseView {
    type Error = WireError;

    fn try_from((album, have): (Offer, bool)) -> Result<Self, WireError> {
        Ok(Self { album: album.try_into()?, have })
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

/// S13 fills `removable` from [`may_cancel`] for the authenticated account.
impl TryFrom<Pending> for PendingView {
    type Error = WireError;

    fn try_from(pending: Pending) -> Result<Self, WireError> {
        let view = Self { id: pending.id.try_into()?, album: pending.album.try_into()?, title: pending.title, artist: pending.artist, removable: false, progress: pending.progress };
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
    StillWorking(JobId),
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
            Self::Refused(reason) => f.write_str(&reason.sentence(None)),
            Self::StillWorking(_) => f.write_str("The Nexus server is still working on the request."),
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

/// A user stop may interrupt a read or a begin. Once a change is accepted,
/// the transport and wait bypass that operation's local stop while polling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallMode {
    Interruptible,
    FinishChange,
}

type SendCall<'a> = dyn Fn(&str, &[u8], CallMode) -> Result<TransportReply, TransportError> + Send + Sync + 'a;
type SeeReceipt<'a> = dyn Fn(&Receipt) -> Result<(), RequestError> + Send + Sync + 'a;
type Pause<'a> = dyn Fn(Duration, CallMode) -> Result<(), TransportError> + Send + Sync + 'a;
type NewId<'a> = dyn Fn() -> Result<RequestId, TransportError> + Send + Sync + 'a;

/// The app supplies a POST transport, a cancellable wait and a source of action IDs.
/// The transport pins the backend to the established origin and ID, supplies the
/// session independently of the message, and returns only complete bodies. Give
/// every authenticated reply's receipt to the app's session/stamp rules before using
/// its result (including for every page and every job poll). The callbacks must
/// honour [`CallMode::FinishChange`] per operation, even after a user stop.
pub struct BackendLidarr<'a> {
    head: RequestHead,
    send: Box<SendCall<'a>>,
    see_receipt: Box<SeeReceipt<'a>>,
    pause: Box<Pause<'a>>,
    new_id: Box<NewId<'a>>,
}

// the design does not say how often to poll. Leave the wait/cancellation to the
// app, with one second between asks while a job is working.
pub const POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_POLLS: usize = 300;
const TRANSIENT_POLL_RETRIES: usize = 3;

impl<'a> BackendLidarr<'a> {
    pub fn new(
        head: RequestHead,
        send: impl Fn(&str, &[u8], CallMode) -> Result<TransportReply, TransportError> + Send + Sync + 'a,
        see_receipt: impl Fn(&Receipt) -> Result<(), RequestError> + Send + Sync + 'a,
        pause: impl Fn(Duration, CallMode) -> Result<(), TransportError> + Send + Sync + 'a,
        new_id: impl Fn() -> Result<RequestId, TransportError> + Send + Sync + 'a,
    ) -> Self {
        Self { head, send: Box::new(send), see_receipt: Box::new(see_receipt), pause: Box::new(pause), new_id: Box::new(new_id) }
    }

    fn ask<T>(&self, call: Call, take: impl FnOnce(Answer) -> Option<T>) -> Result<T, RequestError> {
        self.ask_in_mode(call, CallMode::Interruptible, take)
    }

    fn ask_in_mode<T>(&self, call: Call, mode: CallMode, take: impl FnOnce(Answer) -> Option<T>) -> Result<T, RequestError> {
        let head = RequestHead { speaks: self.head.speaks, restore: self.head.restore };
        let body = write_call(&Message { head, call })?;
        let answer = (self.send)(REQUESTS, body.bytes(), mode)?;
        match read_reply(answer.status, &answer.body)? {
            Reply::Done { result, receipt } => {
                self.accept(&receipt)?;
                match result {
                    Answer::Refused { reason } => Err(RequestError::Refused(reason)),
                    answer => take(answer).ok_or(RequestError::UnexpectedAnswer),
                }
            }
            Reply::Refused(reason) => Err(account_refusal(&reason)),
            Reply::RefusedWithReceipt(refused) => {
                self.accept(refused.receipt())?;
                Err(account_refusal(refused.reason()))
            }
        }
    }

    fn accept(&self, receipt: &Receipt) -> Result<(), RequestError> {
        (self.see_receipt)(receipt)?;
        if receipt.stamp.restore != self.head.restore {
            return Err(RequestError::Refused(Refusal::StartAgain));
        }
        Ok(())
    }

    fn pages<T>(
        &self,
        call: impl Fn(Option<PageCursor>) -> Call,
        take: impl Fn(Answer) -> Option<(Vec<T>, Option<PageCursor>)>,
    ) -> Result<Vec<T>, RequestError> {
        let mut all = Vec::new();
        let mut after: Option<PageCursor> = None;
        loop {
            let (page, next) = self.ask(call(after), &take)?;
            let count = all.len().saturating_add(page.len());
            if count > LIST_MAX_ITEMS {
                return Err(WireError::Bounds.into());
            }
            if let Some(next) = next {
                if u32::try_from(count) != Ok(next.offset())
                    || after.is_some_and(|old| old.snapshot() != next.snapshot())
                {
                    return Err(RequestError::UnexpectedAnswer);
                }
            }
            all.extend(page);
            match next {
                None => return Ok(all),
                Some(next) => after = Some(next),
            }
        }
    }

    pub fn status(&self) -> Result<Availability, RequestError> {
        self.ask(Call::Status, |answer| match answer {
            Answer::Status { lidarr } => Some(lidarr),
            _ => None,
        })
    }

    fn album_rows(&self, artist: LidarrNumber) -> Result<Vec<ReleaseView>, RequestError> {
        self.pages(|after| Call::AlbumsOf { artist, after }, |answer| match answer {
            Answer::AlbumsOf { albums, next } => Some((albums, next)),
            _ => None,
        })
    }

    pub fn listed(&self, artist: LidarrNumber) -> Result<Vec<ListedView>, RequestError> {
        self.pages(|after| Call::Listed { artist, after }, |answer| match answer {
            Answer::Listed { releases, next } => Some((releases, next)),
            _ => None,
        })
    }

    pub fn artist_numbers(&self) -> Result<Vec<(MusicBrainzId, LidarrNumber)>, RequestError> {
        self.pages(|after| Call::ArtistNumbers { after }, |answer| match answer {
            Answer::ArtistNumbers { artists, next } => Some((artists, next)),
            _ => None,
        })
    }

    pub fn busy(&self, artists: Vec<LidarrNumber>) -> Result<(Vec<LidarrNumber>, u64), RequestError> {
        let (positions, stamp) = self.ask(Call::Busy { artists: artists.clone() }, |answer| match answer {
            Answer::Busy { positions, stamp } => Some((positions, stamp)),
            _ => None,
        })?;
        let busy = positions.into_iter().map(|at| artists.get(usize::from(at)).copied()).collect::<Option<Vec<_>>>();
        Ok((busy.ok_or(RequestError::UnexpectedAnswer)?, stamp))
    }

    /// Read this batch's rows by the job number returned by begin. S13 scopes the
    /// read to the authenticated account that submitted the batch.
    pub fn batch_lines(&self, job: JobId) -> Result<Vec<BatchLineView>, RequestError> {
        let lines = self.pages(|after| Call::BatchLines { job, after }, |answer| match answer {
            Answer::BatchLines { lines, next } => Some((lines, next)),
            _ => None,
        })?;
        if lines.iter().enumerate().any(|(index, line)| index != usize::from(line.index)) {
            return Err(RequestError::UnexpectedAnswer);
        }
        Ok(lines)
    }

    pub fn pending_rows(&self) -> Result<Vec<PendingView>, RequestError> {
        self.pages(|after| Call::Pending { after }, |answer| match answer {
            Answer::Pending { albums, next } => Some((albums, next)),
            _ => None,
        })
    }

    /// Use the pair the person saw, even if the pending list has refreshed.
    pub fn cancel_release(&self, album: LidarrNumber, release: MusicBrainzId) -> Result<(), RequestError> {
        self.change((self.new_id)()?, Change::Cancel { album, release }).map(drop)
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
        self.after_in_mode(job, CallMode::Interruptible)
    }

    fn after_in_mode(&self, job: JobId, mode: CallMode) -> Result<Progress, RequestError> {
        self.ask_in_mode(Call::After { job }, mode, |answer| match answer {
            Answer::Working => Some(Progress::Working),
            Answer::Done { result } => Some(Progress::Done(result)),
            Answer::Failed { reason } => Some(Progress::Failed(reason)),
            _ => None,
        })
    }

    /// Run an action through to its answer. A refused offer is refreshed once
    /// before any job begins. Transport loss while polling never resends a begin.
    /// A long-running batch can use begin/after and batch_lines directly.
    pub fn change(&self, request_id: RequestId, change: Change) -> Result<Completed, RequestError> {
        let job = match self.begin(request_id, change.clone()) {
            Err(RequestError::Refused(Refusal::SearchAgain)) => {
                self.refresh_offer(&change)?;
                let retry_id = (self.new_id)()?;
                if retry_id == request_id {
                    return Err(RequestError::UnexpectedAnswer);
                }
                self.begin(retry_id, change.clone())?
            }
            result => result?,
        };
        self.finish_change(job, &change, MAX_POLLS)
    }

    fn refresh_offer(&self, change: &Change) -> Result<(), RequestError> {
        let lookup = |id: &MusicBrainzId| Search(format!("lidarr:{}", id.as_str()));
        match change {
            Change::Request { offer } => self.ask(Call::FindAny { search: lookup(&offer.0), after: None }, |answer| {
                matches!(answer, Answer::FoundAny { .. }).then_some(())
            }),
            Change::AddArtist { artist } => self.ask(Call::FindArtists { search: lookup(artist), after: None }, |answer| {
                matches!(answer, Answer::FoundArtists { .. }).then_some(())
            }),
            _ => Err(RequestError::Refused(Refusal::SearchAgain)),
        }
    }

    fn finish_change(&self, job: JobId, change: &Change, polls: usize) -> Result<Completed, RequestError> {
        let mode = CallMode::FinishChange;
        let mut transient = 0;
        for _ in 0..polls {
            match self.after_in_mode(job, mode) {
                Ok(Progress::Working) => transient = 0,
                Ok(Progress::Failed(reason)) => return Err(RequestError::Refused(reason)),
                Ok(Progress::Done(result)) if result.answers(change) => return Ok(result),
                Ok(Progress::Done(_)) => return Err(RequestError::UnexpectedAnswer),
                Err(RequestError::Transport(TransportError::Stopped)) => {}
                Err(RequestError::Transport(TransportError::Unreachable)) if transient < TRANSIENT_POLL_RETRIES => transient += 1,
                Err(RequestError::Transport(TransportError::Unreachable)) => return Err(RequestError::StillWorking(job)),
                Err(error) => return Err(error),
            }
            // Even a local stop during this wait leaves the accepted job in flight.
            let _ = (self.pause)(POLL_INTERVAL, mode);
        }
        Err(RequestError::StillWorking(job))
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

fn account_refusal(reason: &account::Refusal) -> RequestError {
    let reason = match reason {
        account::Refusal::Busy { .. } => Refusal::Busy,
        account::Refusal::NotSignedIn(account::NotSignedIn::DataPutBack) => Refusal::StartAgain,
        account::Refusal::NotSignedIn(account::NotSignedIn::SessionEnded) => Refusal::NotSignedIn,
        account::Refusal::Malformed => Refusal::Malformed,
        account::Refusal::NotAllowed(account::NotAllowed::Incompatible { .. }) => Refusal::Incompatible,
        account::Refusal::NotAllowed(_) => Refusal::NotAllowed,
        account::Refusal::NotDealtWith => Refusal::NotDealtWith,
    };
    RequestError::Refused(reason)
}

impl RequestLidarr for BackendLidarr<'_> {
    fn find_releases(&self, term: &str) -> Result<(Vec<Offer>, Vec<Single>), String> {
        enum Found {
            Album(Offer),
            Single(Single),
        }
        let Some(search) = Search::from_input(term)? else { return Ok((Vec::new(), Vec::new())) };
        let all = self.pages(|after| Call::FindReleases { search: search.clone(), after }, |answer| match answer {
            Answer::FoundReleases { albums, singles, next } => {
                let found = albums.into_iter().map(|album| Found::Album(album.into()))
                    .chain(singles.into_iter().map(|single| Found::Single(single.into()))).collect();
                Some((found, next))
            }
            _ => None,
        }).map_err(|error| error.to_string())?;
        let (mut albums, mut singles) = (Vec::new(), Vec::new());
        for found in all {
            match found {
                Found::Album(album) => albums.push(album),
                Found::Single(single) => singles.push(single),
            }
        }
        Ok((albums, singles))
    }

    fn find_any(&self, term: &str) -> Result<Vec<Offer>, String> {
        let Some(search) = Search::from_input(term)? else { return Ok(Vec::new()) };
        self.pages(|after| Call::FindAny { search: search.clone(), after }, |answer| match answer {
            Answer::FoundAny { albums, next } => Some((albums, next)),
            _ => None,
        }).map(|items| items.into_iter().map(Offer::from).collect()).map_err(|error| error.to_string())
    }

    fn find_artists(&self, term: &str) -> Result<Vec<ArtistOffer>, String> {
        let Some(search) = Search::from_input(term)? else { return Ok(Vec::new()) };
        self.pages(|after| Call::FindArtists { search: search.clone(), after }, |answer| match answer {
            Answer::FoundArtists { artists, next } => Some((artists, next)),
            _ => None,
        }).map(|items| items.into_iter().map(ArtistOffer::from).collect()).map_err(|error| error.to_string())
    }

    fn tracked(&self) -> Result<HashSet<String>, String> {
        self.pages(|after| Call::Tracked { after }, |answer| match answer {
            Answer::Tracked { artists, next } => Some((artists, next)),
            _ => None,
        }).map(|items| items.into_iter().map(String::from).collect()).map_err(|error| error.to_string())
    }

    fn known_artist(&self, artist: &str) -> Result<Option<(i64, String)>, String> {
        let artist = MusicBrainzId::new(artist).ok_or_else(|| WireError::Bounds.to_string())?;
        self.ask(Call::KnownArtist { artist }, |answer| match answer {
            Answer::KnownArtist { artist } => Some(artist.map(|(number, name)| (number.into(), name))),
            _ => None,
        }).map_err(|error| error.to_string())
    }

    fn albums_of(&self, artist: i64, name: &str) -> Result<Vec<(Offer, bool)>, String> {
        let artist = LidarrNumber::try_from(artist).map_err(|error| error.to_string())?;
        self.album_rows(artist).map(|rows| named_releases(rows, name)).map_err(|error| error.to_string())
    }

    fn releases_of(&self, artist: i64, name: &str) -> Result<Vec<(Offer, bool)>, String> {
        let artist = LidarrNumber::try_from(artist).map_err(|error| error.to_string())?;
        self.pages(|after| Call::ReleasesOf { artist, after }, |answer| match answer {
            Answer::ReleasesOf { releases, next } => Some((releases, next)),
            _ => None,
        }).map(|rows| named_releases(rows, name)).map_err(|error| error.to_string())
    }

    fn add_artist(&self, artist: &str, _: &str, _: &dyn WayBack) -> Result<usize, String> {
        let artist = MusicBrainzId::new(artist).ok_or_else(|| Refusal::SearchAgain.sentence(None).into_owned())?;
        let request_id = (self.new_id)().map_err(|error| RequestError::from(error).to_string())?;
        match self.change(request_id, Change::AddArtist { artist }).map_err(|error| error.to_string())? {
            Completed::ArtistAdded { count } => usize::try_from(count).map_err(|_| WireError::Bounds.to_string()),
            _ => Err(RequestError::UnexpectedAnswer.to_string()),
        }
    }

    fn request(&self, wanted: &Offer, _: &dyn WayBack) -> Result<(), String> {
        let offer = MusicBrainzId::new(&wanted.album).map(OfferName).ok_or_else(|| Refusal::SearchAgain.sentence(None).into_owned())?;
        let request_id = (self.new_id)().map_err(|error| RequestError::from(error).to_string())?;
        // The backend resolves the name and owns the write-ahead recovery. None of
        // the local offer description or WayBack leaves this device.
        self.change(request_id, Change::Request { offer }).map(drop).map_err(|error| match error {
            RequestError::Refused(reason) => reason.sentence(Some(wanted)).into_owned(),
            _ => error.to_string(),
        })
    }

    fn pending(&self) -> Result<Vec<Pending>, String> {
        self.pending_rows().map(|rows| rows.into_iter().map(Pending::from).collect()).map_err(|error| error.to_string())
    }

    fn cancel(&self, id: i64) -> Result<(), String> {
        let id = LidarrNumber::try_from(id).map_err(|error| error.to_string())?;
        let row = self.pending_rows().map_err(|error| error.to_string())?
            .into_iter().find(|row| row.id == id)
            .ok_or_else(|| Refusal::NoLongerListed.sentence(None).into_owned())?;
        self.cancel_release(row.id, row.album).map_err(|error| error.to_string())
    }
}

fn named_releases(rows: Vec<ReleaseView>, name: &str) -> Vec<(Offer, bool)> {
    rows.into_iter().map(|release| {
        let mut album = Offer::from(release.album);
        album.artist = name.to_owned();
        (album, release.have)
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nexus::account::{Stamp, StampTag};
    use crate::nexus::proto::{BackendId, SPEAKS};
    use std::sync::{Arc, Mutex};

    fn id(number: u32) -> MusicBrainzId {
        MusicBrainzId::new(&format!("c9110000-0000-4000-a000-{number:012x}")).unwrap()
    }

    fn cursor(offset: u32) -> Option<PageCursor> { PageCursor::new(NonZeroU64::new(5).unwrap(), offset) }

    fn one() -> LidarrNumber { LidarrNumber::new(1).unwrap() }
    fn request_id() -> RequestId { RequestId::new(6).unwrap() }
    fn job() -> JobId { JobId(NonZeroU64::new(8).unwrap()) }

    fn message(call: Call) -> Message {
        Message { head: RequestHead { speaks: SPEAKS, restore: 2 }, call }
    }

    fn receipt() -> Receipt {
        Receipt {
            stamp: Stamp { backend: BackendId::from_bytes([9; 16]), restore: 2, account: AccountId::new(4).unwrap(), life: 3, change: 1, tag: StampTag::from_bytes([7; 32]) },
            music_login_count: 5,
        }
    }

    fn reply(result: Answer) -> Reply { Reply::Done { result, receipt: receipt() } }

    fn transport(answer: Answer) -> Result<TransportReply, TransportError> {
        let reply = reply(answer);
        Ok(TransportReply { status: reply.status(), body: write_reply(&reply).unwrap().bytes().to_vec() })
    }

    fn album(number: u32) -> AlbumView {
        AlbumView {
            album: OfferName(id(number)), title: "Étoiles et Poussière".into(), artist_id: Some(id(1)), artist: "Marrow Lake".into(),
            year: "2024".into(), cover: "https://art.example.test/album".into(), requested: false, kind: Kind::Album, soundtrack: false,
        }
    }

    fn artist(number: u32) -> ArtistView {
        ArtistView { id: id(number), name: "Marrow Lake".into(), about: "A band".into(), cover: String::new() }
    }

    fn batch_album() -> BatchAlbum {
        let album = album(2);
        BatchAlbum { offer: album.album, title: album.title, artist: album.artist }
    }

    #[test]
    fn every_call_round_trips_in_the_account_head() {
        let search = Search::new("Étoiles et Poussière").unwrap();
        let line = BatchLine { artist: "Marrow Lake".into(), album: String::new(), song: "At Dawn".into() };
        let offer = OfferName(id(2));
        let changes = [
            Change::Request { offer: offer.clone() },
            Change::AddArtist { artist: id(1) },
            Change::AddWanting { offers: vec![offer.clone()] },
            Change::Want { artist: one(), albums: vec![one()] },
            Change::ListSingles { artist: one(), offer },
            Change::Cancel { album: one(), release: id(2) },
            Change::Batch { singles: true, lines: vec![line] },
        ];
        let calls = [
            Call::Status, Call::FindReleases { search: search.clone(), after: None }, Call::FindAny { search: search.clone(), after: cursor(1) },
            Call::FindArtists { search, after: cursor(2) }, Call::Tracked { after: cursor(3) }, Call::ArtistNumbers { after: cursor(5_000) },
            Call::KnownArtist { artist: id(1) }, Call::AlbumsOf { artist: one(), after: cursor(4) }, Call::ReleasesOf { artist: one(), after: cursor(5) },
            Call::Listed { artist: one(), after: cursor(6) }, Call::Busy { artists: vec![one()] }, Call::Pending { after: cursor(7) }, Call::After { job: job() },
            Call::BatchLines { job: job(), after: cursor(8) },
        ];
        for call in calls.into_iter().chain(changes.into_iter().map(|change| Call::Begin { request_id: request_id(), change })) {
            let sent = message(call);
            assert_eq!(read_call(write_call(&sent).unwrap().bytes()), Ok(sent));
        }
        assert_eq!(crate::nexus::proto::plain(REQUESTS), Ok(()));
    }

    #[test]
    fn every_answer_round_trips_with_the_c7_stamp() {
        let album = album(2);
        let release = ReleaseView { album: album.clone(), have: true };
        let single = SingleView { title: "At Dawn".into(), artist_id: id(1), votes: i64::MAX };
        let pending = PendingView { id: one(), album: id(2), title: album.title.clone(), artist: album.artist.clone(), removable: true, progress: Some(100) };
        let done = [
            Completed::Requested, Completed::ArtistAdded { count: u32::MAX }, Completed::AddedWanting { artist: one() },
            Completed::Wanted, Completed::ListedSingles, Completed::Cancelled, Completed::Batch,
        ];
        let answers = [
            Answer::Status { lidarr: Availability::Ready },
            Answer::FoundReleases { albums: vec![album.clone()], singles: vec![single], next: None },
            Answer::FoundAny { albums: vec![album], next: cursor(1) },
            Answer::FoundArtists { artists: vec![artist(1)], next: None },
            Answer::Tracked { artists: vec![id(1)], next: None },
            Answer::ArtistNumbers { artists: vec![(id(1), LidarrNumber::new(i64::MAX).unwrap())], next: None },
            Answer::KnownArtist { artist: Some((one(), "Marrow Lake".into())) }, Answer::KnownArtist { artist: None },
            Answer::AlbumsOf { albums: vec![release.clone()], next: None }, Answer::ReleasesOf { releases: vec![release], next: None },
            Answer::Listed { releases: vec![ListedView { album: id(2), id: one(), wanted: true, have: false }], next: None },
            Answer::Busy { positions: vec![0, 499], stamp: u64::MAX },
            Answer::Pending { albums: vec![pending], next: None },
            Answer::BatchLines {
                lines: [
                    BatchLineProgress::Waiting,
                    BatchLineProgress::Found { album: batch_album() },
                    BatchLineProgress::Asked { album: batch_album() },
                    BatchLineProgress::Done { album: batch_album() },
                    BatchLineProgress::NotFound,
                    BatchLineProgress::Failed { reason: Refusal::WouldNotKeep },
                ].into_iter().enumerate().map(|(index, progress)| BatchLineView { index: index as u16, progress }).collect(),
                next: None,
            },
            Answer::Begun { job: job() }, Answer::Working,
            Answer::Failed { reason: Refusal::LidarrRefused }, Answer::Refused { reason: Refusal::SwitchedOff },
        ];
        let replies = answers.into_iter().chain(done.into_iter().map(|result| Answer::Done { result })).map(reply)
            .chain([
                Reply::Refused(account::Refusal::NotSignedIn(account::NotSignedIn::SessionEnded)),
                Reply::RefusedWithReceipt(account::RefusedWithReceipt::new(account::Refusal::Busy { retry_after_seconds: None }, receipt()).unwrap()),
            ]);
        for sent in replies {
            let bytes = write_reply(&sent).unwrap();
            assert_eq!(read_reply(sent.status(), bytes.bytes()), Ok(sent));
            assert_eq!(read_reply(302, bytes.bytes()), Err(WireError::Status));
        }
    }

    #[test]
    fn the_whole_tree_uses_the_shared_strict_reader() {
        fn read(call: &str) -> Result<Message, WireError> {
            read_call(format!(r#"{{"head":{{"speaks":[1,1],"restore":2}},"call":{call}}}"#).as_bytes())
        }
        assert_eq!(read(r#"{"ask":"status","future":{"band":["Étoiles"]}}"#), Ok(message(Call::Status)));
        for call in [
            r#"{"ask":"status","ask":"status"}"#,
            r#"{"ask":"status","future":{"word":0,"w\u006frd":1}}"#,
            r#"{"ask":"status","future":"\uD800"}"#,
            r#"{"ask":"find_any","after":null,"search":7}"#,
            r#"{"ask":"find_any","search":"Marrow Lake"}"#,
            r#"{"ask":"find_any","after":null,"search":"lidarr:one"}"#,
            r#"{"ask":"releases_of","after":null,"artist":-3}"#,
            r#"{"ask":"releases_of","after":null,"artist":0}"#,
            r#"{"ask":"releases_of","after":null,"artist":1.0}"#,
            r#"{"ask":"releases_of","after":null,"artist":"5"}"#,
            r#"{"ask":"tracked","after":[0,1]}"#,
            r#"{"ask":"tracked","after":[2,0]}"#,
            r#"{"ask":"tracked","after":[2,50000]}"#,
            r#"{"ask":"begin","request_id":0,"change":{"change":"cancel","album":5,"release":"c9110000-0000-4000-a000-000000000002"}}"#,
            r#"{"ask":"begin","request_id":1,"change":{"change":"cancel","album":5}}"#,
            r#"{"ask":"after","job":0}"#,
            r#"{"ask":"begin","request_id":1,"change":{"change":"batch","singles":true,"lines":[["B","A","S"]]}}"#,
            r#"{"ask":"begin","request_id":1,"change":{"change":"batch","lines":[{"artist":"B","album":"A","song":"S"}]}}"#,
            r#"{"ask":"begin","request_id":1,"change":{"change":"batch","singles":"yes","lines":[{"artist":"B","album":"A","song":"S"}]}}"#,
            r#"{"ask":"begin","request_id":1,"change":{"change":"request","offer":{"title":"At Dawn"}}}"#,
            r#"{"ask":"begin","request_id":1,"change":{"change":"set_new_releases","setting":"all"}}"#,
        ] {
            assert!(read(call).is_err(), "{call}");
        }
        let nested = |depth| format!(r#"{{"ask":"status","future":{}0{}}}"#, "[".repeat(depth), "]".repeat(depth));
        assert!(read(&nested(14)).is_ok());
        assert!(read(&nested(15)).is_err());
        let mut bad = br#"{"head":{"speaks":[1,1],"restore":2},"call":{"ask":"status"},"future":""#.to_vec();
        bad.extend_from_slice(&[0xff, b'"', b'}']);
        assert!(read_call(&bad).is_err());
        let good = write_call(&message(Call::Status)).unwrap();
        let mut tail = good.bytes().to_vec();
        tail.extend_from_slice(b"{}");
        assert!(read_call(&tail).is_err());
        assert!(read_call(good.bytes().get(..good.bytes().len() - 1).unwrap()).is_err());
        assert!(read_call(br#"{"head":[[1,1],2],"call":{"ask":"status"}}"#).is_err());

        let encoded = write_reply(&reply(Answer::Refused { reason: Refusal::Busy })).unwrap();
        let text = std::str::from_utf8(encoded.bytes()).unwrap();
        assert!(read_reply(200, text.replace(r#""busy""#, r#""surprise""#).as_bytes()).is_err());
        assert!(read_reply(200, text.replace(r#""busy""#, "7").as_bytes()).is_err());
        assert!(read_reply(200, text.replace(r#""busy""#, r#""busy","future":{"a":1,"a":2}"#).as_bytes()).is_err());
        assert!(read_reply(200, text.replace(r#""busy""#, r#""busy","future":{"a":1}"#).as_bytes()).is_ok());
    }

    #[test]
    fn messages_pages_and_each_list_item_have_their_own_limits() {
        fn padded(bytes: &WireBody, length: usize) -> Vec<u8> {
            let mut bytes = bytes.bytes().to_vec();
            bytes.resize(length, b' ');
            bytes
        }
        let call = write_call(&message(Call::Status)).unwrap();
        assert!(read_call(&padded(&call, CALL_MAX_BYTES)).is_ok());
        assert_eq!(read_call(&padded(&call, CALL_MAX_BYTES + 1)), Err(WireError::TooLarge));
        for answer in [Answer::Status { lidarr: Availability::Ready }, Answer::Working, Answer::Refused { reason: Refusal::Busy }] {
            let small = write_reply(&reply(answer)).unwrap();
            assert!(read_reply(200, &padded(&small, SMALL_REPLY_MAX_BYTES)).is_ok());
            assert_eq!(read_reply(200, &padded(&small, SMALL_REPLY_MAX_BYTES + 1)), Err(WireError::TooLarge));
        }
        let list = write_reply(&reply(Answer::Pending { albums: vec![], next: None })).unwrap();
        assert!(read_reply(200, &padded(&list, LIST_REPLY_MAX_BYTES)).is_ok());
        assert_eq!(read_reply(200, &padded(&list, LIST_REPLY_MAX_BYTES + 1)), Err(WireError::TooLarge));
        assert_eq!(write_reply(&reply(Answer::Tracked { artists: vec![], next: cursor(1) })).map(drop), Err(WireError::Bounds));

        let artists = (0..=PAGE_MAX_ITEMS).map(|at| id(at as u32)).collect::<Vec<_>>();
        assert!(write_reply(&reply(Answer::Tracked { artists: artists.iter().take(PAGE_MAX_ITEMS).cloned().collect(), next: cursor(PAGE_MAX_ITEMS as u32) })).is_ok());
        assert_eq!(write_reply(&reply(Answer::Tracked { artists, next: None })).map(drop), Err(WireError::Bounds));
        let artists = (0..=NUMBER_PAGE_MAX_ITEMS).map(|at| (id(at as u32), one())).collect::<Vec<_>>();
        assert!(write_reply(&reply(Answer::ArtistNumbers { artists: artists.iter().take(NUMBER_PAGE_MAX_ITEMS).cloned().collect(), next: cursor(NUMBER_PAGE_MAX_ITEMS as u32) })).is_ok());
        assert_eq!(write_reply(&reply(Answer::ArtistNumbers { artists, next: None })).map(drop), Err(WireError::Bounds));

        let mut album = album(1);
        album.title.clear();
        let base = serde_json::to_vec(&album).unwrap().len();
        album.title = "x".repeat(ITEM_MAX_BYTES - base);
        let exact = write_reply(&reply(Answer::FoundAny { albums: vec![album.clone()], next: None })).unwrap();
        assert!(read_reply(200, exact.bytes()).is_ok());
        let over = std::str::from_utf8(exact.bytes()).unwrap().replace(r#""soundtrack":false"#, r#""soundtrack":false,"future":0"#);
        assert_eq!(read_reply(200, over.as_bytes()), Err(WireError::TooLarge));
        album.title.push('x');
        assert_eq!(write_reply(&reply(Answer::FoundAny { albums: vec![album], next: None })).map(drop), Err(WireError::TooLarge));
    }

    #[test]
    fn selections_searches_and_batch_fields_are_bounded() {
        assert!(Search::new(&"🎵".repeat(SEARCH_MAX_CHARS)).is_some());
        for term in ["", "   ", "hi\u{0000}there", "lidarr:", "lidarr:44", " Lidarr:44", "lidarr:44?album=1"] {
            assert!(Search::new(term).is_none(), "{term}");
        }
        assert!(Search::new(&"🎵".repeat(SEARCH_MAX_CHARS + 1)).is_none());
        assert!(Search::new(&format!("lidarr:{}", id(1).as_str())).is_some());
        assert!(MusicBrainzId::new(&id(1).as_str().to_uppercase()).is_none());

        let musicians = (1..=BUSY_MAX_ARTISTS + 1).map(|n| LidarrNumber::new(n as i64).unwrap()).collect::<Vec<_>>();
        assert!(write_call(&message(Call::Busy { artists: musicians.iter().take(BUSY_MAX_ARTISTS).copied().collect() })).is_ok());
        assert_eq!(write_call(&message(Call::Busy { artists: musicians })).map(drop), Err(WireError::Bounds));
        let busy = reply(Answer::Busy { positions: (0..BUSY_MAX_ARTISTS as u16).collect(), stamp: u64::MAX });
        assert!(write_reply(&busy).unwrap().bytes().len() <= SMALL_REPLY_MAX_BYTES);

        let change = |change| write_call(&message(Call::Begin { request_id: request_id(), change })).map(drop);
        let line = BatchLine { artist: "Marrow Lake".into(), album: "".into(), song: "x".repeat(SEARCH_MAX_CHARS) };
        assert_eq!(change(Change::Batch { singles: true, lines: vec![line.clone(); BATCH_MAX_LINES] }), Ok(()));
        assert_eq!(change(Change::Batch { singles: true, lines: vec![line.clone(); BATCH_MAX_LINES + 1] }), Err(WireError::Bounds));
        assert_eq!(change(Change::Batch { singles: true, lines: vec![BatchLine { album: "a".repeat(SEARCH_MAX_CHARS + 1), ..line.clone() }] }), Err(WireError::Bounds));
        assert_eq!(change(Change::Batch { singles: true, lines: vec![BatchLine { album: "(mix) lidarr:7".into(), ..line.clone() }] }), Err(WireError::Bounds));
        assert_eq!(change(Change::Batch { singles: true, lines: vec![BatchLine { artist: "a".repeat(SEARCH_MAX_CHARS + 1), ..line }] }), Err(WireError::Bounds));
        assert_eq!(change(Change::Want { artist: one(), albums: vec![one(); 2] }), Err(WireError::Bounds));
        assert_eq!(change(Change::AddWanting { offers: vec![] }), Err(WireError::Bounds));
        assert_eq!(cursor(LIST_MAX_ITEMS as u32), None);
    }

    #[test]
    fn the_session_only_keeps_its_recent_offers_with_both_limits() {
        let mut book = SessionOffers::default();
        let other = SessionOffers::default();
        for at in 0..=KEPT_ALBUMS {
            book.remember_album(album(at as u32), 0).unwrap();
        }
        for at in 0..=KEPT_ARTISTS {
            book.remember_artist(artist(at as u32), 0).unwrap();
        }
        assert_eq!(book.album(&OfferName(id(0)), 0), Err(Refusal::SearchAgain));
        assert_eq!(book.artist(&id(0), 0), Err(Refusal::SearchAgain));
        assert_eq!(other.album(&OfferName(id(1)), 0), Err(Refusal::SearchAgain));
        assert_eq!(other.artist(&id(1), 0), Err(Refusal::SearchAgain));
        assert_eq!(book.album(&OfferName(id(1)), OFFER_LIFETIME_SECONDS - 1), Ok(&album(1)));
        assert_eq!(book.artist(&id(1), OFFER_LIFETIME_SECONDS), Err(Refusal::SearchAgain));
        assert_eq!(book.album(&OfferName(id(1)), i64::MAX), Err(Refusal::SearchAgain));

        book.remember_album(album(1), 10).unwrap();
        assert_eq!(book.album(&OfferName(id(1)), OFFER_LIFETIME_SECONDS), Ok(&album(1)));
        assert_eq!(book.album(&OfferName(id(1)), 9), Err(Refusal::SearchAgain));
        assert_eq!(book.album(&OfferName(id(1)), OFFER_LIFETIME_SECONDS + 10), Err(Refusal::SearchAgain));
        assert_eq!(book.remember_album(AlbumView { artist_id: None, ..album(1) }, 0), Err(WireError::Bounds));
    }

    #[test]
    fn only_the_requesting_account_or_an_admin_may_take_it_off() {
        let on = RequestsAccess::default();
        assert_eq!(on, RequestsAccess::On);
        let me = AccountId::new(1).unwrap();
        let other = AccountId::new(2).unwrap();
        assert_eq!(may_cancel(on, me, false, &[me]), Ok(()));
        assert_eq!(may_cancel(on, me, false, &[other]), Err(Refusal::AskedByOthers));
        assert_eq!(may_cancel(on, me, false, &[]), Err(Refusal::NotAllowed));
        assert_eq!(may_cancel(on, me, false, &[me, other]), Err(Refusal::AskedByOthers));
        assert_eq!(may_cancel(on, me, true, &[other]), Ok(()));
        assert_eq!(may_cancel(on, me, true, &[]), Ok(()));
        assert_eq!(may_cancel(RequestsAccess::Off, me, true, &[me]), Err(Refusal::SwitchedOff));
    }

    #[test]
    fn the_client_pages_and_waits_for_a_change_with_only_an_offer_name() {
        let steps = Mutex::new(Vec::new());
        let polls = Mutex::new(0usize);
        let client = BackendLidarr::new(
            message(Call::Status).head,
            |path, body, mode| {
                assert_eq!(path, REQUESTS);
                let message = read_call(body).unwrap();
                assert_eq!(message.head.restore, 2);
                let mut steps = steps.lock().unwrap();
                assert_eq!(mode, if matches!(message.call, Call::After { .. }) { CallMode::FinishChange } else { CallMode::Interruptible });
                let answer = match message.call {
                    Call::Tracked { after: None } => Answer::Tracked { artists: vec![id(1)], next: cursor(1) },
                    Call::Tracked { after } if after == cursor(1) => Answer::Tracked { artists: vec![id(2)], next: None },
                    Call::Begin { request_id: got, change: Change::Request { offer } } => {
                        assert_eq!(got, request_id());
                        assert_eq!(offer, OfferName(id(3)));
                        Answer::Begun { job: job() }
                    }
                    Call::After { job: got } => {
                        assert_eq!(got, job());
                        let mut polls = polls.lock().unwrap();
                        *polls += 1;
                        if *polls == 1 { Answer::Working } else { Answer::Done { result: Completed::Requested } }
                    }
                    call => panic!("unexpected {call:?}"),
                };
                steps.push(match &answer {
                    Answer::Tracked { .. } => "page", Answer::Begun { .. } => "begin",
                    Answer::Working => "working", Answer::Done { .. } => "done",
                    _ => panic!("unexpected answer"),
                });
                let body = write_reply(&reply(answer)).unwrap();
                Ok(TransportReply { status: 200, body: body.bytes().to_vec() })
            },
            |seen| { assert_eq!(*seen, receipt()); steps.lock().unwrap().push("stamp"); Ok(()) },
            |wait, mode| { assert_eq!(mode, CallMode::FinishChange); assert_eq!(wait, POLL_INTERVAL); steps.lock().unwrap().push("wait"); Ok(()) },
            || Ok(request_id()),
        );
        assert_eq!(client.tracked(), Ok(HashSet::from([id(1).into(), id(2).into()])));
        let offer = Offer { title: "tampered".into(), artist: "caller-supplied".into(), kind: "not a kind".into(), ..album(3).into() };
        assert_eq!(client.request(&offer, &crate::lidarr::tests::Noted::default()), Ok(()));
        assert_eq!(*steps.lock().unwrap(), ["page", "stamp", "page", "stamp", "begin", "stamp", "working", "stamp", "wait", "done", "stamp"]);
    }

    #[test]
    fn a_page_cannot_skip_a_place_or_move_to_another_snapshot() {
        let reply_with = |cursor_for_second_page| {
            BackendLidarr::new(
                message(Call::Status).head,
                move |_, body, _| {
                    let answer = match read_call(body).unwrap().call {
                        Call::Tracked { after: None } => Answer::Tracked { artists: vec![id(1)], next: cursor(1) },
                        Call::Tracked { after } if after == cursor(1) => Answer::Tracked { artists: vec![id(2)], next: cursor_for_second_page },
                        _ => panic!("an inconsistent cursor was followed"),
                    };
                    let bytes = write_reply(&reply(answer)).unwrap();
                    Ok(TransportReply { status: 200, body: bytes.bytes().to_vec() })
                },
                |_| Ok(()), |_, _| panic!("a read waited"), || panic!("a read wanted a change ID"),
            ).tracked()
        };
        let different = PageCursor::new(NonZeroU64::new(6).unwrap(), 2);
        let expected = Err(RequestError::UnexpectedAnswer.to_string());
        assert_eq!(reply_with(cursor(3)), expected);
        assert_eq!(reply_with(different), expected);
    }

    #[test]
    fn stamps_are_checked_and_a_local_stop_cannot_abandon_an_accepted_change() {
        let client = BackendLidarr::new(
            message(Call::Status).head,
            |_, body, _| {
                assert_eq!(read_call(body).unwrap().call, Call::Status);
                let mut receipt = receipt();
                receipt.stamp.restore = 3;
                let reply = Reply::Done { result: Answer::Status { lidarr: Availability::Ready }, receipt };
                Ok(TransportReply { status: 200, body: write_reply(&reply).unwrap().bytes().to_vec() })
            },
            |seen| { assert_eq!(seen.stamp.restore, 3); Ok(()) },
            |_, _| panic!("status waited"), || panic!("status wanted a change ID"),
        );
        assert_eq!(client.status(), Err(RequestError::Refused(Refusal::StartAgain)));

        let waits = Mutex::new(0);
        let client = BackendLidarr::new(
            message(Call::Status).head,
            |_, body, mode| {
                let answer = match read_call(body).unwrap().call {
                    Call::Begin { change: Change::Request { .. }, .. } => {
                        assert_eq!(mode, CallMode::Interruptible);
                        Answer::Begun { job: job() }
                    }
                    Call::After { job: got } if got == job() => {
                        assert_eq!(mode, CallMode::FinishChange);
                        if *waits.lock().unwrap() == 0 {
                            Answer::Working
                        } else {
                            Answer::Done { result: Completed::Requested }
                        }
                    }
                    _ => panic!("unexpected call"),
                };
                transport(answer)
            },
            |_| Ok(()),
            |_, mode| {
                assert_eq!(mode, CallMode::FinishChange);
                *waits.lock().unwrap() += 1;
                Err(TransportError::Stopped)
            },
            || Ok(request_id()),
        );
        assert_eq!(client.request(&album(3).into(), &crate::lidarr::tests::Noted::default()), Ok(()));
        assert_eq!(*waits.lock().unwrap(), 1);
    }

    #[test]
    fn an_unoffered_album_gets_one_lookup_and_one_fresh_begin() {
        for second_begin_fails in [false, true] {
            let trial = Mutex::new(0);
            let next_id = Mutex::new(6);
            let client = BackendLidarr::new(
                message(Call::Status).head,
                |_, body, _| {
                    let call = read_call(body).unwrap().call;
                    let mut trial = trial.lock().unwrap();
                    let answer = match (*trial, call) {
                        (0, Call::Begin { request_id, change: Change::Request { offer } }) => {
                            assert_eq!(request_id.get(), 6);
                            assert_eq!(offer, OfferName(id(3)));
                            Answer::Refused { reason: Refusal::SearchAgain }
                        }
                        (1, Call::FindAny { search, after: None }) => {
                            assert_eq!(search.as_str(), format!("lidarr:{}", id(3).as_str()));
                            Answer::FoundAny { albums: vec![album(3)], next: None }
                        }
                        (2, Call::Begin { request_id, change: Change::Request { offer } }) => {
                            assert_eq!(request_id.get(), 7);
                            assert_eq!(offer, OfferName(id(3)));
                            if second_begin_fails { Answer::Refused { reason: Refusal::SearchAgain } }
                            else { Answer::Begun { job: job() } }
                        }
                        (3, Call::After { job: got }) if got == job() && !second_begin_fails => {
                            Answer::Done { result: Completed::Requested }
                        }
                        _ => panic!("unexpected call"),
                    };
                    *trial += 1;
                    transport(answer)
                },
                |_| Ok(()), |_, _| panic!("a finished job waited"),
                || {
                    let mut id = next_id.lock().unwrap();
                    let request = RequestId::new(*id).unwrap();
                    *id += 1;
                    Ok(request)
                },
            );
            let result = client.request(&album(3).into(), &lidarr::tests::Noted::default());
            if second_begin_fails {
                assert_eq!(result, Err(Refusal::SearchAgain.sentence(None).into_owned()));
                assert_eq!(*trial.lock().unwrap(), 3);
            } else {
                assert_eq!(result, Ok(()));
                assert_eq!(*trial.lock().unwrap(), 4);
            }
        }
    }

    #[test]
    fn a_lost_poll_is_retried_and_wrong_completions_are_refused() {
        let polls = Mutex::new(0);
        let client = BackendLidarr::new(
            message(Call::Status).head,
            |_, body, mode| {
                assert_eq!(mode, CallMode::FinishChange);
                assert_eq!(read_call(body).unwrap().call, Call::After { job: job() });
                let mut polls = polls.lock().unwrap();
                *polls += 1;
                match *polls {
                    1 => Err(TransportError::Unreachable),
                    2 => transport(Answer::Done { result: Completed::Requested }),
                    3 => transport(Answer::Done { result: Completed::Cancelled }),
                    _ => transport(Answer::Working),
                }
            },
            |_| Ok(()), |_, _| Ok(()), || panic!("polling must not issue a new request ID"),
        );
        let change = Change::Request { offer: OfferName(id(2)) };
        assert_eq!(client.finish_change(job(), &change, 3), Ok(Completed::Requested));
        assert_eq!(client.finish_change(job(), &change, 3), Err(RequestError::UnexpectedAnswer));
        assert_eq!(client.finish_change(job(), &change, 3), Err(RequestError::StillWorking(job())));
        assert_eq!(*polls.lock().unwrap(), 6);
    }

    #[test]
    fn the_refusal_codes_round_trip_and_render_the_released_sentences() {
        let offer: Offer = album(2).into();
        let cases = [
            (lidarr::not_among_releases(&offer), Refusal::NotAmongReleases),
            (lidarr::would_not_keep(&offer), Refusal::WouldNotKeep),
            (lidarr::could_not_restore(&offer), Refusal::LidarrCouldNotRestore),
            (crate::requests::NO_LONGER_LISTED.into(), Refusal::NoLongerListed),
            (lidarr::NO_MUSIC_FOLDER.into(), Refusal::LidarrNotSetUp),
            (lidarr::NO_PROFILES.into(), Refusal::LidarrNoProfiles),
            (lidarr::SINGLES_PROFILE_NO_NUMBER.into(), Refusal::LidarrSinglesProfileNoNumber),
            (lidarr::SOUNDTRACKS_PROFILE_NO_NUMBER.into(), Refusal::LidarrSoundtracksProfileNoNumber),
            (lidarr::NEEDED.into(), Refusal::LidarrBehindSignIn),
            (lidarr::SIGN_IN_REJECTED.into(), Refusal::LidarrSignInRejected),
            (lidarr::KEY_REFUSED.into(), Refusal::LidarrKeyRefused),
            (lidarr::UNRECOGNIZED_LIDARR.into(), Refusal::LidarrUnrecognized),
        ];
        for (sentence, reason) in cases {
            assert_eq!(refusal_for(&sentence, Some(&offer)), reason);
            assert_eq!(refusal_for(sentence.trim_end_matches('.'), Some(&offer)), reason);
            assert_eq!(reason.sentence(Some(&offer)), sentence);
            let answer = reply(Answer::Failed { reason });
            let bytes = write_reply(&answer).unwrap();
            assert_eq!(read_reply(answer.status(), bytes.bytes()), Ok(answer));
        }
        let reason = refusal_for("Couldn't reach Lidarr. Private URL or header value.", Some(&offer));
        assert_eq!(reason, Refusal::LidarrUnavailable);
        let body = write_reply(&reply(Answer::Failed { reason })).unwrap();
        assert!(!std::str::from_utf8(body.bytes()).unwrap().contains("Private URL"));
        assert_eq!(refusal_for("Some upstream text", None), Refusal::LidarrRefused);
    }

    #[test]
    fn the_new_direct_trait_methods_make_the_released_calls() {
        use crate::http::{Failure, Http, Method, Request, Response};

        #[derive(Default)]
        struct Direct(Mutex<Vec<String>>);
        impl Http for Direct {
            fn send(&self, request: &Request) -> Result<Response, Failure> {
                let path = request.url.strip_prefix("http://lidarr.test/api/v1/").unwrap();
                self.0.lock().unwrap().push(path.into());
                let response = match (path, request.method) {
                    ("album?artistId=1", Method::Get) => r#"[{"title":"At Dawn","albumType":"Album"},{"title":"First Light","albumType":"EP"}]"#,
                    ("artist", Method::Get) => "[]",
                    ("rootfolder", Method::Get) => r#"[{"path":"/music","defaultQualityProfileId":1,"defaultMetadataProfileId":2}]"#,
                    ("artist", Method::Post(body)) => {
                        let body: serde_json::Value = serde_json::from_str(body).unwrap();
                        assert_eq!(body.get("artistName").and_then(serde_json::Value::as_str), Some("Marrow Lake"));
                        "{}"
                    }
                    ("album/monitor", Method::Put(body)) => {
                        assert_eq!(serde_json::from_str::<serde_json::Value>(body).unwrap(),
                            serde_json::json!({ "albumIds": [1], "monitored": false }));
                        "{}"
                    }
                    _ => panic!("unexpected Lidarr call"),
                };
                Ok(Response::new(200, Vec::new(), move || Ok(response.into())))
            }
        }
        let http = Arc::new(Direct::default());
        let lidarr = lidarr::Lidarr::new("http://lidarr.test", "key", None, http.clone());
        let client: &dyn RequestLidarr = &lidarr;
        let albums = client.albums_of(1, "Marrow Lake").unwrap();
        assert_eq!(albums.iter().map(|(album, _)| album.title.as_str()).collect::<Vec<_>>(), ["At Dawn"]);
        assert_eq!(client.add_artist(id(1).as_str(), "Marrow Lake", &lidarr::tests::Noted::default()), Ok(0));
        assert_eq!(client.cancel(1), Ok(()));
        assert_eq!(*http.0.lock().unwrap(), ["album?artistId=1", "artist", "rootfolder", "artist", "album/monitor"]);
    }

    #[test]
    fn the_new_remote_trait_methods_use_scoped_ids_and_local_display_names() {
        let trial = Mutex::new(0);
        let next_id = Mutex::new(10);
        let client = BackendLidarr::new(
            message(Call::Status).head,
            |_, body, _| {
                let call = read_call(body).unwrap().call;
                let mut trial = trial.lock().unwrap();
                let answer = match (*trial, call) {
                    (0, Call::AlbumsOf { artist, after: None }) if artist == one() => {
                        Answer::AlbumsOf { albums: vec![ReleaseView { album: album(2), have: true }], next: None }
                    }
                    (1, Call::Begin { change: Change::AddArtist { artist }, .. }) if artist == id(1) => {
                        Answer::Refused { reason: Refusal::SearchAgain }
                    }
                    (2, Call::FindArtists { search, after: None }) if search.as_str() == format!("lidarr:{}", id(1).as_str()) => {
                        Answer::FoundArtists { artists: vec![artist(1)], next: None }
                    }
                    (3, Call::Begin { change: Change::AddArtist { artist }, request_id }) if artist == id(1) && request_id.get() == 11 => {
                        Answer::Begun { job: job() }
                    }
                    (4, Call::After { .. }) => Answer::Done { result: Completed::ArtistAdded { count: 2 } },
                    (5, Call::Pending { after: None }) => Answer::Pending {
                        albums: vec![PendingView {
                            id: one(), album: id(2), title: "At Dawn".into(), artist: "Marrow Lake".into(),
                            removable: true, progress: None,
                        }], next: None,
                    },
                    (6, Call::Begin { change: Change::Cancel { album, release }, .. }) if album == one() && release == id(2) => {
                        Answer::Begun { job: job() }
                    }
                    (7, Call::After { .. }) => Answer::Done { result: Completed::Cancelled },
                    _ => panic!("unexpected call"),
                };
                *trial += 1;
                transport(answer)
            },
            |_| Ok(()), |_, _| panic!("jobs are immediately ready"),
            || {
                let mut id = next_id.lock().unwrap();
                let request = RequestId::new(*id).unwrap();
                *id += 1;
                Ok(request)
            },
        );
        let client: &dyn RequestLidarr = &client;
        let albums = client.albums_of(1, "Local display name").unwrap();
        assert_eq!(albums.iter().map(|(offer, have)| (offer.artist.as_str(), *have)).collect::<Vec<_>>(), [("Local display name", true)]);
        assert_eq!(client.add_artist(id(1).as_str(), "The caller's name", &lidarr::tests::Noted::default()), Ok(2));
        assert_eq!(client.cancel(1), Ok(()));
        assert_eq!(*trial.lock().unwrap(), 8);
    }

    #[test]
    fn pending_rows_check_both_ids_and_drop_only_bad_upstream_items() {
        let row = |number| Pending { id: number, album: id(number as u32).into(), title: "At Dawn".into(), artist: "Marrow Lake".into(), progress: Some(25) };
        let rows: Vec<PendingView> = lidarr_items([
            row(1),
            Pending { album: String::new(), ..row(2) },
            Pending { title: "too long".repeat(150), ..row(3) },
            row(4),
        ]);
        assert_eq!(rows.iter().map(|row| row.id.get()).collect::<Vec<_>>(), [1, 4]);
        let row = rows.first().unwrap();
        assert!(!row.removable);
        assert_eq!(row.check_cancel_target(one(), &id(1)), Ok(()));
        assert_eq!(row.check_cancel_target(one(), &id(4)), Err(Refusal::NoLongerListed));
        assert_eq!(row.check_cancel_target(LidarrNumber::new(4).unwrap(), &id(1)), Err(Refusal::NoLongerListed));
    }

    #[test]
    fn a_batch_has_one_preference_and_progress_in_original_line_order() {
        let client = BackendLidarr::new(
            message(Call::Status).head,
            |_, body, _| {
                let answer = match read_call(body).unwrap().call {
                    Call::Begin { change: Change::Batch { singles: false, lines }, .. } => {
                        assert_eq!(lines.iter().map(|line| line.song.as_str()).collect::<Vec<_>>(), ["At Dawn", "First Light"]);
                        Answer::Begun { job: job() }
                    }
                    Call::BatchLines { job: got, after: None } if got == job() => Answer::BatchLines {
                        lines: vec![BatchLineView { index: 0, progress: BatchLineProgress::Asked { album: batch_album() } }],
                        next: cursor(1),
                    },
                    Call::BatchLines { job: got, after } if got == job() && after == cursor(1) => Answer::BatchLines {
                        lines: vec![BatchLineView { index: 1, progress: BatchLineProgress::Failed { reason: Refusal::NoLongerListed } }],
                        next: None,
                    },
                    call => panic!("unexpected call {call:?}"),
                };
                transport(answer)
            },
            |_| Ok(()), |_, _| panic!("the progress read did not need to poll"), || Ok(request_id()),
        );
        let lines = ["At Dawn", "First Light"].into_iter().map(|song| BatchLine {
            artist: "Marrow Lake".into(), album: "".into(), song: song.into(),
        }).collect();
        let got = client.begin(request_id(), Change::Batch { singles: false, lines }).unwrap();
        assert_eq!(client.batch_lines(got).unwrap(), [
            BatchLineView { index: 0, progress: BatchLineProgress::Asked { album: batch_album() } },
            BatchLineView { index: 1, progress: BatchLineProgress::Failed { reason: Refusal::NoLongerListed } },
        ]);
        assert_eq!(write_call(&message(Call::BatchLines { job: got, after: cursor(2_000) })).map(drop), Err(WireError::Bounds));
        let bad = reply(Answer::BatchLines {
            lines: vec![BatchLineView { index: 2_000, progress: BatchLineProgress::Waiting }],
            next: None,
        });
        assert_eq!(write_reply(&bad).map(drop), Err(WireError::Bounds));
    }

    #[test]
    fn ordinary_search_input_is_tidied_on_the_device() {
        let calls = Mutex::new(0);
        let client = BackendLidarr::new(
            message(Call::Status).head,
            |_, body, _| {
                let Call::FindAny { search, after: None } = read_call(body).unwrap().call else { panic!("unexpected call") };
                *calls.lock().unwrap() += 1;
                assert_eq!(search.as_str(), format!("É 🎵{}", "x".repeat(197)));
                transport(Answer::FoundAny { albums: vec![], next: None })
            },
            |_| Ok(()), |_, _| panic!("lookup waited"), || panic!("lookup issued a change ID"),
        );
        assert_eq!(client.find_any(" \t\n\u{0000} "), Ok(Vec::new()));
        assert_eq!(client.find_releases(" \t\n "), Ok((Vec::new(), Vec::new())));
        assert_eq!(client.find_artists("\r"), Ok(Vec::new()));
        assert_eq!(client.find_any(&format!("É\t🎵{}", "x".repeat(300))), Ok(Vec::new()));
        assert_eq!(*calls.lock().unwrap(), 1);
    }

    #[test]
    fn the_standin_only_feeds_decoded_searches_to_the_existing_lidarr_address_builders() {
        use crate::http::{Failure, Http, Request, Response};
        use crate::lidarr::Lidarr;
        struct Record(Mutex<Vec<String>>);
        impl Http for Record {
            fn send(&self, request: &Request) -> Result<Response, Failure> {
                self.0.lock().unwrap().push(request.url.to_owned());
                Ok(Response::new(200, Vec::new(), || Ok("[]".into())))
            }
        }
        let http = Arc::new(Record(Mutex::new(Vec::new())));
        let lidarr = Lidarr::new("https://lidarr.example.test/local", "made-up-key", None, http.clone());
        let text = "Étoiles https://example.test/x?extra=1&next=/api#part %2F";
        let client = BackendLidarr::new(
            message(Call::Status).head,
            |path, body, mode| {
                assert_eq!(mode, CallMode::Interruptible);
                assert_eq!(path, REQUESTS);
                // An unknown caller-supplied URL/headers cannot survive the typed read.
                let mut body = std::str::from_utf8(body).unwrap().to_owned();
                body.pop();
                body.push_str(r#","url":"https://example.test/","headers":{"Authorization":"fake-token"}}"#);
                let answer = match read_call(body.as_bytes()).unwrap().call {
                    Call::FindReleases { search, after: None } => {
                        assert_eq!(lidarr.find_releases(search.as_str()), Ok((vec![], vec![])));
                        Answer::FoundReleases { albums: vec![], singles: vec![], next: None }
                    }
                    Call::FindAny { search, after: None } => {
                        assert_eq!(lidarr.find_any(search.as_str()), Ok(vec![]));
                        Answer::FoundAny { albums: vec![], next: None }
                    }
                    Call::FindArtists { search, after: None } => {
                        assert_eq!(lidarr.find_artists(search.as_str()), Ok(vec![]));
                        Answer::FoundArtists { artists: vec![], next: None }
                    }
                    call => panic!("unexpected {call:?}"),
                };
                Ok(TransportReply { status: 200, body: write_reply(&reply(answer)).unwrap().bytes().to_vec() })
            },
            |_| Ok(()), |_, _| panic!("lookup waited"), || panic!("lookup issued a change ID"),
        );
        assert!(client.find_releases(text).is_ok());
        assert!(client.find_any(text).is_ok());
        assert!(client.find_artists(text).is_ok());
        let urls = http.0.lock().unwrap();
        assert_eq!(urls.len(), 3);
        for (url, path) in urls.iter().zip(["album/lookup", "album/lookup", "artist/lookup"]) {
            let url = url::Url::parse(url).unwrap();
            assert_eq!(url.scheme(), "https");
            assert_eq!(url.host_str(), Some("lidarr.example.test"));
            assert_eq!(url.path(), format!("/local/api/v1/{path}"));
            assert_eq!(url.fragment(), None);
            assert_eq!(url.query_pairs().collect::<Vec<_>>(), vec![("term".into(), text.into())]);
        }
        assert!(client.find_any("lidarr:bad").is_err());
    }
}
