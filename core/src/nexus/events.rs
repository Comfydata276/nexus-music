//! Listening events, the device's journal, and the batch both sides understand (§12–13).
//! The caller owns the clock, random bytes, files, the authenticated account and device,
//! and the transaction that keeps an event or deletes all of a person's events.

use std::cmp::Ordering;
use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

pub use super::account::DeviceKind;
use super::account::{self, AccountId, DeviceId, Reply, ReplyResult, RequestHead};
use super::proto::{BackendId, Version};
use super::wire::{self, BadMessage, Fields, WireBody};

pub const FILE_BYTES: usize = 1024 * 1024;
pub const WAITING_BYTES: u64 = 64 * 1024 * 1024;
pub const TAKEN_BYTES: u64 = 16 * 1024 * 1024;
pub const EVENT_BYTES: usize = 2 * 1024;
pub const TEXT_CHARS: usize = 200;
pub const BATCH_BYTES: usize = 512 * 1024;
pub const BATCH_EVENTS: usize = 500;
pub const BATCH_REPLY_BYTES: usize = account::LIST_REPLY_MAX_BYTES;
pub const KEEP_MS: u64 = 8 * 24 * 60 * 60 * 1000;

// The design does not say the field limits or time encoding. Use UTC Unix milliseconds
// through year 9999, positive device order numbers, and millisecond positions. Allow up
// to seven days in a position or duration, also the bound on how much sound a start heard.
pub const TIME_MAX_MS: u64 = 253_402_300_799_999;
pub const PLAY_MAX_MS: u32 = 7 * 24 * 60 * 60 * 1000;
// The design does not say how to compare millisecond progress with a duration
// truncated to seconds by the music server. Its remaining fraction is at most 999 ms.
pub const STOP_TOLERANCE_MS: u32 = 999;

// The design does not say the JSON spelling or call paths. The caller wraps each payload
// in its authenticated request. Stats and deletion are separate calls.
pub const SEND_PATH: &str = "/nexus/api/events/send";
pub const DELETE_PATH: &str = "/nexus/api/events/delete";
pub const STATS_PATH: &str = "/nexus/api/events/stats";
pub const STATS_PAGE: usize = 50;
pub const STATS_REPLY_BYTES: usize = STATS_PAGE * (EVENT_BYTES + 64) + account::SMALL_REPLY_MAX_BYTES;

/// An opaque position in the account's recent plays, as given by the backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct StatsCursor(u64);

impl StatsCursor {
    pub fn new(number: u64) -> Option<Self> {
        (number > 0 && number <= i64::MAX as u64).then_some(Self(number))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl wire::Value for StatsCursor {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        Self::new(u64::read(raw)?).ok_or(BadMessage::Form)
    }
}

wire::object! {
    StatsRequest { head: RequestHead, after: Option<StatsCursor> }
}

impl wire::Message for StatsRequest {
    const MAX_BYTES: usize = account::SMALL_REQUEST_MAX_BYTES;
}

wire::object! {
    RecentPlay { device: DeviceId, event: Placed }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct StatsPage {
    pub starts: u64,
    pub recent: Vec<RecentPlay>,
    pub next: Option<StatsCursor>,
    /// The current revision of this session's account. Older servers omit it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_revision: Option<u64>,
}

impl wire::Value for StatsPage {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let fields = Fields::read(raw)?;
        let rows: Vec<&RawValue> = wire::plain_json(raw_field(&fields, "recent")?.get())?;
        if rows.len() > STATS_PAGE {
            return Err(BadMessage::Form);
        }
        let recent = rows.into_iter().map(RecentPlay::read).collect::<Result<Vec<_>, _>>()?;
        let page = Self {
            starts: fields.get("starts")?,
            recent,
            next: fields.get("next")?,
            account_revision: fields.0.get("account_revision").map(|raw| u64::read(raw)).transpose()?,
        };
        page.check()?;
        Ok(page)
    }

    fn check(&self) -> Result<(), BadMessage> {
        if self.recent.len() > STATS_PAGE
            || self.recent.len() as u64 > self.starts
            || (self.next.is_some() && self.recent.len() != STATS_PAGE)
            || self.recent.iter().any(|row| !matches!(row.event.record().kind(), Kind::Start { .. }))
        {
            return Err(BadMessage::Form);
        }
        Ok(())
    }
}

impl ReplyResult for StatsPage {
    const MAX_DONE_BYTES: usize = STATS_REPLY_BYTES;
}

pub type StatsReply = Reply<StatsPage>;

/// Read the complete stamped stats page. Refusals keep the account reply's small limit.
pub fn read_stats_reply(status: u16, whole_body: &[u8]) -> Result<StatsReply, BadMessage> {
    account::read_reply(status, whole_body)
}

/// The two self-delete POSTs share one scope, revision and action ID. The
/// app sends each once through its session driver and hands the whole answer back.
pub struct HistoryDeletion {
    head: RequestHead,
    account: AccountId,
    backend: BackendId,
    life: u64,
    revision: u64,
    request: account::RequestId,
}

#[derive(Debug, PartialEq, Eq)]
pub enum HistoryDeleteError {
    MissingRevision,
    Invalid,
    Refused(account::Refusal),
}

impl From<BadMessage> for HistoryDeleteError {
    fn from(_: BadMessage) -> Self {
        Self::Invalid
    }
}

impl HistoryDeletion {
    /// The stamp and current revision come from the same checked stats page.
    pub fn new(
        head: RequestHead, stamp: &account::Stamp, page: &StatsPage, request: account::RequestId,
    ) -> Result<Self, HistoryDeleteError> {
        if head.restore != stamp.restore {
            return Err(HistoryDeleteError::Invalid);
        }
        let revision = page.account_revision.ok_or(HistoryDeleteError::MissingRevision)?;
        Ok(Self { head, account: stamp.account, backend: stamp.backend, life: stamp.life, revision, request })
    }

    pub fn grant(&self, password: account::Password) -> Result<WireBody, HistoryDeleteError> {
        Ok(account::encode(&account::GrantRequest {
            head: self.head(),
            change: account::GrantChange::DeleteListeningData {
                account: self.account, target_revision: self.revision, request_id: self.request,
            },
            password,
        })?)
    }

    pub fn spend(&self, status: u16, body: &[u8]) -> Result<WireBody, HistoryDeleteError> {
        let grant = match account::read_reply::<account::GrantIssued>(status, body)? {
            Reply::Done { result, receipt } if self.owns(&receipt.stamp) && receipt.stamp.life == self.life => result.grant,
            Reply::Refused(reason) => return Err(HistoryDeleteError::Refused(reason)),
            _ => return Err(HistoryDeleteError::Invalid),
        };
        Ok(account::encode(&account::DeleteListeningData {
            head: self.head(),
            request_id: self.request,
            change: account::DeleteListeningDataTarget { account: self.account, target_revision: self.revision },
            grant,
        })?)
    }

    pub fn confirmed_life(&self, status: u16, body: &[u8]) -> Result<u64, HistoryDeleteError> {
        match account::read_reply::<account::SignedOut>(status, body)? {
            Reply::Done { receipt, .. } if self.owns(&receipt.stamp) && receipt.stamp.life > self.life => Ok(receipt.stamp.life),
            Reply::Refused(reason) => Err(HistoryDeleteError::Refused(reason)),
            _ => Err(HistoryDeleteError::Invalid),
        }
    }

    fn head(&self) -> RequestHead {
        RequestHead { speaks: self.head.speaks, restore: self.head.restore }
    }

    fn owns(&self, stamp: &account::Stamp) -> bool {
        stamp.account == self.account && stamp.backend == self.backend && stamp.restore == self.head.restore
    }
}

/// The design does not say the id's text spelling. Use the backend id's 32 lower-case
/// hex digits, in its own type. Make the id once, before writing, and send it unchanged.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct EventId(BackendId);

impl EventId {
    pub fn from_bytes(bytes: [u8; 16]) -> Self {
        Self(BackendId::from_bytes(bytes))
    }

    pub fn from_hex(text: &str) -> Option<Self> {
        BackendId::from_hex(text).map(Self)
    }

    pub fn to_hex(self) -> String {
        self.0.to_hex()
    }
}

impl PartialOrd for EventId {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EventId {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.as_bytes().cmp(other.0.as_bytes())
    }
}

impl TryFrom<String> for EventId {
    type Error = &'static str;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::from_hex(&text).ok_or("invalid event id")
    }
}

impl From<EventId> for String {
    fn from(id: EventId) -> Self {
        id.to_hex()
    }
}

impl fmt::Debug for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("EventId(..)")
    }
}

/// The design does not say the fixed fault list. A producer chooses a place and shape
/// where it observes the failure. Raw exception messages never enter these types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultPlace {
    Playback,
    AudioOutput,
    Library,
    Downloads,
    Foreground,
    MusicServer,
    NexusServer,
    Journal,
    App,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaultShape {
    NameLookup,
    Connection,
    SecureConnection,
    Timeout,
    Permission,
    Refused,
    Unavailable,
    UnexpectedAnswer,
    Read,
    Write,
    Decode,
    NoSound,
    QueueFull,
    JournalFull,
    BorrowConflict,
    PoisonedLock,
    MissingValue,
    UnexpectedResultError,
    Bounds,
    Assertion,
    Overflow,
    NoPayload,
    Panic,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fault {
    pub place: FaultPlace,
    pub shape: FaultShape,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PlayedFrom {
    Album { id: String },
    Playlist { id: String },
    Artist { id: String },
    Radio { id: String },
    Mix { id: String },
    Search { words: String },
    Queue,
    LikedSongs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Began {
    PressedHere,
    NextInQueue,
    AddedByItself,
    AnotherDevice,
    Car,
    Jam,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Sound {
    Streamed,
    KeptCopy,
    Download,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Stopped {
    Finished,
    Next,
    Previous,
    AnotherChoice,
    Stopped,
    Error { fault: Fault },
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchedIn {
    Library,
    Requests,
}

#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    Start {
        song_id: String,
        title: String,
        artist: String,
        album: String,
        from: PlayedFrom,
        began: Began,
        sound: Sound,
        position_ms: u32,
    },
    Stop {
        start: EventId,
        position_ms: Option<u32>,
        duration_ms: Option<u32>,
        heard_ms: Option<u32>,
        why: Stopped,
    },
    Pause {
        start: EventId,
        position_ms: u32,
    },
    Resume {
        start: EventId,
        position_ms: u32,
    },
    Seek {
        start: EventId,
        from_ms: u32,
        to_ms: u32,
    },
    Search {
        words: String,
        found: u32,
        #[serde(rename = "where")]
        where_: SearchedIn,
    },
    Open {
        #[serde(serialize_with = "wire::write_version")]
        version: Version,
        device: DeviceKind,
    },
    Fault {
        fault: Fault,
        count: u32,
    },
}

fn raw_field<'a>(fields: &Fields<'a>, key: &str) -> Result<&'a RawValue, BadMessage> {
    fields.0.get(key).copied().ok_or(BadMessage::Form)
}

fn typed<T: serde::de::DeserializeOwned>(fields: &Fields<'_>, key: &str) -> Result<T, BadMessage> {
    wire::plain_json(raw_field(fields, key)?.get())
}

fn text(fields: &Fields<'_>, key: &str) -> Result<String, BadMessage> {
    wire::json_string(raw_field(fields, key)?).map(|value| value.to_string())
}

impl wire::Value for EventId {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        wire::read_text(raw, Self::from_hex)
    }
}

impl wire::Value for Fault {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let fields = Fields::read(raw)?;
        Ok(Self {
            place: typed(&fields, "place")?,
            shape: typed(&fields, "shape")?,
        })
    }
}

impl wire::Value for Stopped {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let fields = Fields::read(raw)?;
        if wire::json_string(raw_field(&fields, "kind")?)?.as_str() == "error" {
            return Ok(Self::Error {
                fault: fields.get("fault")?,
            });
        }
        wire::plain_json(raw.get())
    }
}

impl wire::Value for Kind {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let fields = Fields::read(raw)?;
        let tag = wire::json_string(raw_field(&fields, "kind")?)?;
        match tag.as_str() {
            "start" => {
                let from = raw_field(&fields, "from")?;
                Fields::read(from)?;
                Ok(Self::Start {
                    song_id: text(&fields, "song_id")?,
                    title: text(&fields, "title")?,
                    artist: text(&fields, "artist")?,
                    album: text(&fields, "album")?,
                    from: wire::plain_json(from.get())?,
                    began: typed(&fields, "began")?,
                    sound: typed(&fields, "sound")?,
                    position_ms: typed(&fields, "position_ms")?,
                })
            }
            "stop" => Ok(Self::Stop {
                start: fields.get("start")?,
                // Require all three keys, with null only for recovered stops.
                position_ms: typed(&fields, "position_ms")?,
                duration_ms: typed(&fields, "duration_ms")?,
                heard_ms: typed(&fields, "heard_ms")?,
                why: fields.get("why")?,
            }),
            "pause" => Ok(Self::Pause {
                start: fields.get("start")?,
                position_ms: typed(&fields, "position_ms")?,
            }),
            "resume" => Ok(Self::Resume {
                start: fields.get("start")?,
                position_ms: typed(&fields, "position_ms")?,
            }),
            "seek" => Ok(Self::Seek {
                start: fields.get("start")?,
                from_ms: typed(&fields, "from_ms")?,
                to_ms: typed(&fields, "to_ms")?,
            }),
            "search" => Ok(Self::Search {
                words: text(&fields, "words")?,
                found: typed(&fields, "found")?,
                where_: typed(&fields, "where")?,
            }),
            "open" => Ok(Self::Open {
                version: fields.get("version")?,
                device: fields.get("device")?,
            }),
            "fault" => Ok(Self::Fault {
                fault: fields.get("fault")?,
                count: typed(&fields, "count")?,
            }),
            _ => Err(BadMessage::Form),
        }
    }
}

/// What the app hands over at the moment an event happens. The writer has not
/// placed it in the device's order yet.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct Record {
    id: EventId,
    at_ms: u64,
    #[serde(rename = "event")]
    kind: Kind,
}

/// An event with its device order number, assigned by the journal's writer.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct Placed {
    #[serde(flatten)]
    record: Record,
    sequence: u64,
}

// These values carry names and search words. Neither Debug nor errors print them.
impl fmt::Debug for Record {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Record(..)")
    }
}

impl fmt::Debug for Placed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Placed(..)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wrong {
    Size,
    Form,
    Field,
    Count,
    RepeatedId,
    UnaskedId,
    /// The reply has no outcomes to apply. Reconcile the account before changing
    /// the local life or deleting files.
    LifeMoved { life: u64 },
    Status,
}

impl From<BadMessage> for Wrong {
    fn from(error: BadMessage) -> Self {
        match error {
            BadMessage::TooLarge => Self::Size,
            BadMessage::Status(_) => Self::Status,
            BadMessage::Form => Self::Form,
        }
    }
}

fn bad_message(error: Wrong) -> BadMessage {
    if error == Wrong::Size {
        BadMessage::TooLarge
    } else {
        BadMessage::Form
    }
}

pub(super) fn text_ok(text: &str) -> bool {
    text.chars().take(TEXT_CHARS + 1).count() <= TEXT_CHARS
        && !text.chars().any(char::is_control)
}

fn nonempty(text: &str) -> bool {
    text_ok(text) && !text.trim().is_empty()
}

pub(super) fn disallowed_display_char(ch: char) -> bool {
    ch.is_control() || matches!(ch,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}'
        | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

fn clean_display_up_to(text: &str, max_chars: usize) -> String {
    let mut space = false;
    let mut cleaned: String = text.chars().filter_map(|ch| match ch {
        '\t' | '\n' | '\r' | '\u{2028}' | '\u{2029}' => Some(' '),
        ch if disallowed_display_char(ch) => None,
        ch => Some(ch),
    }).skip_while(|&ch| ch == ' ').filter(|&ch| {
        let repeated_space = std::mem::replace(&mut space, ch == ' ') && ch == ' ';
        !repeated_space
    }).take(max_chars).collect();
    if cleaned.ends_with(' ') {
        cleaned.pop();
    }
    cleaned
}

/// Clean display text under the caller's own length ceiling.
pub fn clean_display_text(text: &str) -> String {
    clean_display_up_to(text, usize::MAX)
}

/// Up to 200 cleaned Unicode scalar values of a journal display field.
/// Opaque song and source IDs do not pass through this function.
pub fn clean_text(text: &str) -> String {
    clean_display_up_to(text, TEXT_CHARS)
}

impl Kind {
    fn clean_display(&mut self, clean: impl Fn(&str) -> String) {
        let clean_words = |words: &mut String| {
            let cleaned = clean(words);
            // Keep the search and its outcome when all its text was removed.
            *words = if cleaned.trim().is_empty() && words.chars().any(disallowed_display_char) {
                String::new()
            } else {
                cleaned
            };
        };
        match self {
            Self::Start { title, artist, album, from, .. } => {
                for text in [title, artist, album] {
                    *text = clean(text);
                }
                if let PlayedFrom::Search { words } = from {
                    clean_words(words);
                }
            }
            Self::Search { words, .. } => clean_words(words),
            _ => {}
        }
    }
}

// Only quotes and backslashes are escaped in cleaned strings. Count them while
// removing characters so the playback path serializes the record at most once.
fn trim_start(kind: &mut Kind, mut over: usize) -> Result<(), Wrong> {
    let Kind::Start { title, artist, album, from, .. } = kind else {
        return Err(Wrong::Size);
    };
    let search = if let PlayedFrom::Search { words } = from {
        // Retain a non-whitespace character when the query has one.
        let minimum = words.char_indices().find(|(_, ch)| !ch.is_whitespace())
            .map_or(0, |(offset, ch)| offset + ch.len_utf8());
        Some((words, minimum))
    } else {
        None
    };
    let mut texts = [Some((title, 0)), Some((artist, 0)), Some((album, 0)), search];
    while over > 0 {
        let (text, _) = texts.iter_mut().flatten()
            .filter(|(text, minimum)| text.len() > *minimum)
            .max_by_key(|(text, _)| text.len())
            .ok_or(Wrong::Size)?;
        let ch = text.pop().ok_or(Wrong::Size)?;
        let removed = ch.len_utf8() + usize::from(matches!(ch, '"' | '\\'));
        over = over.saturating_sub(removed);
    }
    Ok(())
}

impl Record {
    pub fn new(id: EventId, at_ms: u64, mut kind: Kind) -> Result<Self, Wrong> {
        kind.clean_display(clean_text);
        if let Kind::Stop { position_ms: Some(position), duration_ms: Some(duration), .. } = &mut kind
            && *position <= PLAY_MAX_MS && *duration <= PLAY_MAX_MS
        {
            // An estimated duration can be short even after allowing for the
            // fractional second. Keep the stop reason and heard time.
            *position = (*position).min(duration.saturating_add(STOP_TOLERANCE_MS));
        }
        let record = Self { id, at_ms, kind };
        record.check()?;
        // The design does not say which display field to shorten when an event
        // is too large. Trim the longest by UTF-8 byte length, keeping IDs exact.
        // Reserve room for the biggest sequence so placement always fits.
        let mut largest = Placed { record, sequence: u64::MAX };
        let over = largest.encoded_len()?.saturating_sub(EVENT_BYTES);
        if over > 0 {
            trim_start(&mut largest.record.kind, over)?;
        }
        Ok(largest.record)
    }

    pub fn id(&self) -> EventId {
        self.id
    }

    pub fn at_ms(&self) -> u64 {
        self.at_ms
    }

    pub fn kind(&self) -> &Kind {
        &self.kind
    }

    pub fn place(self, sequence: u64) -> Result<Placed, Wrong> {
        if sequence == 0 {
            return Err(Wrong::Field);
        }
        Ok(Placed { record: self, sequence })
    }

    fn check(&self) -> Result<(), Wrong> {
        let amount = |ms| ms <= PLAY_MAX_MS;
        let other = |id| id != self.id;
        let words_ok = |words: &str| words.is_empty() || nonempty(words);
        let valid = match &self.kind {
            Kind::Start {
                song_id,
                title,
                artist,
                album,
                from,
                position_ms,
                ..
            } => {
                let from_ok = match from {
                    PlayedFrom::Album { id }
                    | PlayedFrom::Playlist { id }
                    | PlayedFrom::Artist { id }
                    | PlayedFrom::Radio { id }
                    | PlayedFrom::Mix { id } => nonempty(id),
                    PlayedFrom::Search { words } => words_ok(words),
                    PlayedFrom::Queue | PlayedFrom::LikedSongs => true,
                };
                nonempty(song_id)
                    && [title, artist, album].iter().all(|s| text_ok(s))
                    && from_ok
                    && amount(*position_ms)
            }
            Kind::Stop {
                start,
                position_ms,
                duration_ms,
                heard_ms,
                why,
            } => {
                other(*start)
                    && [position_ms, duration_ms, heard_ms]
                        .iter()
                        .all(|ms| ms.is_none_or(amount))
                    && (matches!(why, Stopped::Unknown)
                        || (position_ms.is_some() && duration_ms.is_some() && heard_ms.is_some()))
                    && position_ms
                        .zip(*duration_ms)
                        .is_none_or(|(position, duration)| {
                            position.saturating_sub(duration) <= STOP_TOLERANCE_MS
                        })
            }
            Kind::Pause { start, position_ms } | Kind::Resume { start, position_ms } => {
                other(*start) && amount(*position_ms)
            }
            Kind::Seek {
                start,
                from_ms,
                to_ms,
            } => other(*start) && amount(*from_ms) && amount(*to_ms),
            Kind::Search { words, .. } => words_ok(words),
            Kind::Open { .. } => true,
            Kind::Fault { count, .. } => *count > 0,
        };
        if self.at_ms > TIME_MAX_MS || !valid {
            return Err(Wrong::Field);
        }
        Ok(())
    }
}

impl Placed {
    pub fn record(&self) -> &Record {
        &self.record
    }

    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    fn encoded_len(&self) -> Result<usize, Wrong> {
        serde_json::to_vec(self).map(|bytes| bytes.len()).map_err(|_| Wrong::Form)
    }

    fn check_size(&self) -> Result<(), Wrong> {
        if self.encoded_len()? > EVENT_BYTES {
            return Err(Wrong::Size);
        }
        Ok(())
    }

    fn check(&self) -> Result<(), Wrong> {
        if self.sequence == 0 {
            return Err(Wrong::Field);
        }
        self.record.check()?;
        self.check_size()
    }

    fn parse_raw(raw: &RawValue) -> Result<Self, Wrong> {
        if raw.get().len() > EVENT_BYTES {
            return Err(Wrong::Size);
        }
        let fields = Fields::read(raw)?;
        Ok(Self {
            record: Record {
                id: fields.get("id")?,
                at_ms: fields.get("at_ms")?,
                kind: fields.get("event")?,
            },
            sequence: fields.get("sequence")?,
        })
    }

    fn from_raw(raw: &RawValue) -> Result<Self, Wrong> {
        let event = Self::parse_raw(raw)?;
        event.check()?;
        Ok(event)
    }

    fn from_upload(raw: &RawValue) -> Result<Self, Wrong> {
        let mut event = Self::parse_raw(raw)?;
        event.record.kind.clean_display(clean_display_text);
        event.check()?;
        Ok(event)
    }

    pub fn read(bytes: &[u8]) -> Result<Self, Wrong> {
        if bytes.len() > EVENT_BYTES {
            return Err(Wrong::Size);
        }
        wire::read(bytes).map_err(Into::into)
    }

    /// A JSON object followed by LF. A file can hold whole lines only.
    pub fn line(&self) -> Result<Vec<u8>, Wrong> {
        self.check()?;
        let mut bytes = serde_json::to_vec(self).map_err(|_| Wrong::Form)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

impl wire::Value for Placed {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        Self::from_raw(raw).map_err(bad_message)
    }
}

impl wire::Message for Placed {
    const MAX_BYTES: usize = EVENT_BYTES;
}

/// The next device order number after the last one assigned. The caller keeps the last
/// number even when old journal files are removed. On exhaustion the writer must stop.
pub fn next_sequence(last: u64) -> Option<u64> {
    last.checked_add(1)
}

/// Start ids left open in this device and life, in device order. Every event is read
/// first, so a stop written earlier in a different file also closes its start.
pub fn open_starts<'a>(events: impl IntoIterator<Item = &'a Placed>) -> Vec<EventId> {
    let mut starts = Vec::new();
    let mut stopped = BTreeSet::new();
    for event in events {
        match event.record.kind() {
            Kind::Start { .. } => starts.push((event.sequence, event.record.id)),
            Kind::Stop { start, .. } => { stopped.insert(*start); }
            _ => {}
        }
    }
    starts.sort_by_key(|(sequence, _)| *sequence);
    starts.into_iter().filter_map(|(_, id)| stopped.insert(id).then_some(id)).collect()
}

/// End an open start once. The caller supplies the saved queue's matching start id and
/// position, if it has them, and has first checked which starts remain open.
pub fn end_open_start(
    start: EventId,
    saved_position: Option<(EventId, u32)>,
    id: EventId,
    at_ms: u64,
) -> Result<Record, Wrong> {
    Record::new(id, at_ms, Kind::Stop {
        start,
        position_ms: saved_position.filter(|(saved, _)| *saved == start).map(|(_, ms)| ms),
        duration_ms: None,
        heard_ms: None,
        why: Stopped::Unknown,
    })
}

// The design does not say the journal's byte format. One JSON header gives the account
// life, and each subsequent LF-terminated JSON line gives one event. A torn tail is
// abandoned by rolling to a new file, so even an apparently complete tail lacking its
// LF can never become a committed line after a kill.
wire::object! { Header { format: u64, life: u64 } }
impl wire::Message for Header {}

// The design does not say how to recognize later file layouts. Number this layout.
pub const FILE_FORMAT: u64 = 1;

pub fn file_header(life: u64) -> Result<Vec<u8>, Wrong> {
    json_line(&Header { format: FILE_FORMAT, life })
}

fn json_line(value: &impl Serialize) -> Result<Vec<u8>, Wrong> {
    let mut bytes = serde_json::to_vec(value).map_err(|_| Wrong::Form)?;
    if bytes.len() >= FILE_BYTES {
        return Err(Wrong::Size);
    }
    bytes.push(b'\n');
    Ok(bytes)
}

pub struct JournalFile {
    pub life: u64,
    pub events: Vec<Placed>,
    pub skipped_lines: usize,
}

pub fn read_file(bytes: &[u8]) -> Result<JournalFile, Wrong> {
    if bytes.len() > FILE_BYTES {
        return Err(Wrong::Size);
    }
    let mut lines = bytes.split_inclusive(|b| *b == b'\n');
    let header = lines
        .next()
        .and_then(|line| line.strip_suffix(b"\n"))
        .ok_or(Wrong::Form)?;
    let header: Header = wire::read(header)?;
    if header.format != FILE_FORMAT {
        return Err(Wrong::Form);
    }
    let mut file = JournalFile {
        life: header.life,
        events: Vec::new(),
        skipped_lines: 0,
    };
    for line in lines {
        match line
            .strip_suffix(b"\n")
            .and_then(|line| Placed::read(line).ok())
        {
            Some(event) => file.events.push(event),
            None => file.skipped_lines += 1,
        }
    }
    Ok(file)
}

/// The writer supplies the trailing byte and byte length of a known journal file under
/// its own lock. If absent, torn, too full, or in another life, start a fresh file.
/// The bytes returned include the header only for a fresh file.
pub fn append(life: u64, tail: Option<FileTail>, event: &Placed) -> Result<Append, Wrong> {
    let line = event.line()?;
    if tail.is_some_and(|tail| {
        tail.life == life && tail.last_byte == b'\n' && tail.bytes > 0
            && tail.bytes <= FILE_BYTES - line.len()
    }) {
        return Ok(Append { new_file: false, bytes: line });
    }
    let mut bytes = file_header(life)?;
    if bytes.len() > FILE_BYTES - line.len() {
        return Err(Wrong::Size);
    }
    bytes.extend(line);
    Ok(Append { new_file: true, bytes })
}

pub struct FileTail {
    pub life: u64,
    pub bytes: usize,
    pub last_byte: u8,
}

pub struct Append {
    pub new_file: bool,
    pub bytes: Vec<u8>,
}

pub struct KeptFile {
    pub life: u64,
    pub bytes: u64,
    /// Valid event lines in the file, including events already answered.
    pub events: u64,
    pub pending: u64,
    pub final_at_ms: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RemoveFiles {
    /// Positions in the caller's oldest-first list.
    pub remove: Vec<usize>,
    /// Pending event lines lost to the waiting cap. Taken and refused lines are settled.
    pub dropped: u64,
}

/// The design does not say how terminal refusals affect retention. Treat a fully
/// answered file as settled, with the same age and byte limits as files all taken.
/// On restore use [`Outcome::should_send`] to mark taken events waiting again.
pub fn remove_files(files: &[KeptFile], life: u64, now_ms: u64) -> RemoveFiles {
    let mut waiting = 0u128;
    let mut taken = 0u128;
    for file in files.iter().filter(|file| file.life == life && file.events > 0) {
        if file.pending > 0 {
            waiting += u128::from(file.bytes);
        } else if !expired(file, now_ms) {
            taken += u128::from(file.bytes);
        }
    }
    let mut plan = RemoveFiles { remove: Vec::new(), dropped: 0 };
    for (index, file) in files.iter().enumerate() {
        if file.life != life || file.events == 0 {
            plan.remove.push(index);
        } else if file.pending == 0 {
            if expired(file, now_ms) {
                plan.remove.push(index);
            } else if taken > u128::from(TAKEN_BYTES) {
                taken -= u128::from(file.bytes);
                plan.remove.push(index);
            }
        } else if waiting > u128::from(WAITING_BYTES) {
            waiting -= u128::from(file.bytes);
            plan.remove.push(index);
            plan.dropped = plan.dropped.saturating_add(file.pending);
        }
    }
    plan
}

fn expired(file: &KeptFile, now_ms: u64) -> bool {
    file.final_at_ms.and_then(|at| now_ms.checked_sub(at)).is_some_and(|age| age >= KEEP_MS)
}

/// Device totals. Persist increments with the resulting journal or reply state,
/// so reading a refusal or removing a file twice does not add the loss twice.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LossCounts {
    pub line_full: u64,
    pub files_full: u64,
    pub refused: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loss {
    LineFull,
    FilesFull,
    Refused,
}

impl LossCounts {
    pub fn add(&mut self, loss: Loss, count: u64) {
        let total = match loss {
            Loss::LineFull => &mut self.line_full,
            Loss::FilesFull => &mut self.files_full,
            Loss::Refused => &mut self.refused,
        };
        *total = total.saturating_add(count);
    }
}

/// The scope comes from the authenticated account, never from the event body.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct EventKey {
    pub account: AccountId,
    pub id: EventId,
}

/// Delete every id owned by the signed-in account, across devices, in one transaction
/// after advancing its life in the store kept apart. No caller-supplied list limits it.
pub fn ids_to_delete<'a>(
    account: AccountId,
    keys: impl IntoIterator<Item = &'a EventKey>,
) -> impl Iterator<Item = EventId> {
    keys.into_iter().filter(move |key| key.account == account).map(|key| key.id)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Existing {
    None,
    SameDevice,
    AnotherDevice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intake {
    Insert,
    Answer(Outcome),
}

/// Decide under the same transaction that inserts by unique (account, id). An old
/// life never inserts, even after deletion has freed all ids. When the id is already
/// on this device, leave the first copy alone and acknowledge the retry. The design
/// does not say how to handle an id reused by another device of the same account;
/// refuse it so no device can replace or claim a different device's copy.
pub fn intake(life: u64, sent_life: u64, existing: Existing, room: bool) -> Intake {
    use Intake::{Answer, Insert};
    if life != sent_life {
        return Answer(Outcome::Refused);
    }
    match existing {
        Existing::SameDevice => Answer(Outcome::Taken),
        Existing::AnotherDevice => Answer(Outcome::Refused),
        Existing::None if room => Insert,
        Existing::None => Answer(Outcome::NotDealtWith),
    }
}

/// A nonempty batch of events under one account life. Authentication is in the
/// request header. C7's shared head carries the protocol and restore numbers.
/// §12.2 identifies changes here by each event's ID.
#[derive(Serialize)]
pub struct Batch {
    head: RequestHead,
    life: u64,
    events: Vec<Placed>,
}

impl Batch {
    pub fn head(&self) -> &RequestHead {
        &self.head
    }

    pub fn life(&self) -> u64 {
        self.life
    }

    pub fn events(&self) -> &[Placed] {
        &self.events
    }

    /// Take the first distinct ids that fit the complete request body, head included.
    pub fn prefix(
        head: RequestHead,
        life: u64,
        events: &[Placed],
        max_bytes: usize,
    ) -> Result<Self, Wrong> {
        let limit = max_bytes.min(BATCH_BYTES);
        let mut batch = Self {
            head,
            life,
            events: Vec::new(),
        };
        let mut bytes = serde_json::to_vec(&batch).map_err(|_| Wrong::Form)?.len();
        let mut seen = BTreeSet::new();
        for event in events {
            if !seen.insert(event.record.id) {
                continue;
            }
            event.check()?;
            let event_bytes = serde_json::to_vec(event).map_err(|_| Wrong::Form)?.len();
            bytes += event_bytes + usize::from(!batch.events.is_empty());
            if bytes > limit {
                break;
            }
            batch.events.push(event.clone());
            if batch.events.len() == BATCH_EVENTS {
                break;
            }
        }
        if batch.events.is_empty() {
            return Err(if events.is_empty() {
                Wrong::Count
            } else {
                Wrong::Size
            });
        }
        Ok(batch)
    }

    pub fn write(&self) -> Result<WireBody, Wrong> {
        wire::encode(self).map_err(Into::into)
    }
}

impl wire::Value for Batch {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let incoming = IncomingBatch::read_raw(raw).map_err(bad_message)?;
        let events = incoming
            .events
            .into_iter()
            .map(|(_, event)| event.map_err(bad_message))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            head: incoming.head,
            life: incoming.life,
            events,
        })
    }
}

impl wire::Message for Batch {
    const MAX_BYTES: usize = BATCH_BYTES;
}

pub struct IncomingBatch {
    pub head: RequestHead,
    pub life: u64,
    pub events: Vec<(EventId, Result<Placed, Wrong>)>,
}

impl IncomingBatch {
    /// The design does not say how to answer a malformed entry with no usable id.
    /// Refuse that batch. An entry with a usable id and bad fields can be refused on
    /// its own. Clean display fields before validating each upload for storage.
    /// Bad JSON, including a duplicate key anywhere, refuses the whole body.
    pub fn read(bytes: &[u8]) -> Result<Self, Wrong> {
        wire::read_value(bytes, BATCH_BYTES).map_err(Into::into)
    }

    fn read_raw(raw: &RawValue) -> Result<Self, Wrong> {
        let fields = Fields::read(raw)?;
        let head = fields.get("head")?;
        let life = fields.get("life")?;
        let rows: Vec<&RawValue> = wire::plain_json(raw_field(&fields, "events")?.get())?;
        if rows.is_empty() || rows.len() > BATCH_EVENTS {
            return Err(Wrong::Count);
        }
        let mut events = Vec::with_capacity(rows.len());
        let mut seen = BTreeSet::new();
        for raw in rows {
            let id = Fields::read(raw)?.get("id")?;
            if !seen.insert(id) {
                return Err(Wrong::RepeatedId);
            }
            events.push((id, Placed::from_upload(raw)));
        }
        Ok(Self { head, life, events })
    }
}

impl wire::Value for IncomingBatch {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        Self::read_raw(raw).map_err(bad_message)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Taken,
    Refused,
    NotDealtWith,
}

impl Outcome {
    /// Ordinary sending retries only unfinished events. "Start again" also replays
    /// taken events with their original ids. The design does not say which of "every
    /// event it still holds" and "refused, which is final" wins on restore. A refusal
    /// stays final.
    pub fn should_send(self, start_again: bool) -> bool {
        match self {
            Self::Taken => start_again,
            Self::Refused => false,
            Self::NotDealtWith => true,
        }
    }
}

impl wire::Value for Outcome {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        wire::plain_json(raw.get())
    }
}

wire::object! {
    #[derive(Clone, Copy)]
    Answered { id: EventId, outcome: Outcome }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct BatchAnswer {
    answers: Vec<Answered>,
}

impl BatchAnswer {
    pub fn new(answers: Vec<Answered>) -> Result<Self, Wrong> {
        if answers.len() > BATCH_EVENTS {
            return Err(Wrong::Count);
        }
        let mut seen = BTreeSet::new();
        if answers.iter().any(|answer| !seen.insert(answer.id)) {
            return Err(Wrong::RepeatedId);
        }
        Ok(Self { answers })
    }

    pub fn answers(&self) -> &[Answered] {
        &self.answers
    }

    /// A reply can be sparse and in any order. An absent id has no final answer.
    pub fn outcome(&self, id: EventId) -> Outcome {
        self.answers
            .iter()
            .find(|answer| answer.id == id)
            .map_or(Outcome::NotDealtWith, |a| a.outcome)
    }
}

impl wire::Value for BatchAnswer {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let fields = Fields::read(raw)?;
        let rows: Vec<&RawValue> = wire::plain_json(raw_field(&fields, "answers")?.get())?;
        if rows.len() > BATCH_EVENTS {
            return Err(BadMessage::Form);
        }
        let answers = rows
            .into_iter()
            .map(Answered::read)
            .collect::<Result<_, _>>()?;
        Self::new(answers).map_err(bad_message)
    }
}

impl ReplyResult for BatchAnswer {
    const MAX_DONE_BYTES: usize = BATCH_REPLY_BYTES;
}

pub type BatchReply = Reply<BatchAnswer>;

/// Use C7's envelope and stamp. Before the client applies the reply to its
/// journal, it also checks the receipt against its account, life and generation.
pub fn read_reply(status: u16, whole_body: &[u8], sent: &Batch) -> Result<BatchReply, Wrong> {
    if whole_body.len() > BATCH_REPLY_BYTES {
        return Err(Wrong::Size);
    }
    let reply = account::read_reply::<BatchAnswer>(status, whole_body)?;
    let receipt = match &reply {
        Reply::Done { receipt, .. } => Some(receipt),
        Reply::RefusedWithReceipt(context) => Some(context.receipt()),
        Reply::Refused(_) => None,
    };
    if let Some(life) = receipt.map(|receipt| receipt.stamp.life).filter(|&life| life != sent.life()) {
        return Err(Wrong::LifeMoved { life });
    }
    if let Reply::Done { result, .. } = &reply {
        let wanted: BTreeSet<_> = sent.events().iter().map(|event| event.record.id).collect();
        if result
            .answers
            .iter()
            .any(|answer| !wanted.contains(&answer.id))
        {
            return Err(Wrong::UnaskedId);
        }
    }
    Ok(reply)
}

pub fn write_reply(reply: &BatchReply) -> Result<WireBody, Wrong> {
    wire::encode(reply).map_err(Into::into)
}

#[cfg(test)]
mod tests;
