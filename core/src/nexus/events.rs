//! Listening events, the device's journal, and the batch both sides understand (§12–13).
//! The caller owns the clock, random bytes, files, the authenticated account and device,
//! and the transaction that keeps an event or deletes all of a person's events.

use std::collections::HashSet;
use std::fmt;

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

use super::proto::BackendId;

pub const FILE_BYTES: usize = 1024 * 1024;
pub const WAITING_BYTES: u64 = 64 * 1024 * 1024;
pub const BATCH_BYTES: usize = 1024 * 1024;
pub const BATCH_EVENTS: usize = 500;
pub const KEEP_MS: u64 = 8 * 24 * 60 * 60 * 1000;

// The design does not say the field limits or time encoding. Use UTC Unix milliseconds
// through year 9999, positive device order numbers, and millisecond positions. Allow up
// to seven days in a position or duration, also the bound on how much sound a start heard.
pub const TIME_MAX_MS: u64 = 253_402_300_799_999;
pub const PLAY_MAX_MS: u32 = 7 * 24 * 60 * 60 * 1000;
pub const MUSIC_ID_BYTES: usize = 256;
pub const NAME_BYTES: usize = 512;
pub const SEARCH_BYTES: usize = 512;
pub const VERSION_BYTES: usize = 64;

// The design does not say the JSON spelling or call paths. The caller wraps each payload
// in its authenticated request. Stats and deletion are separate calls.
pub const SEND_PATH: &str = "/nexus/api/events/send";
pub const DELETE_PATH: &str = "/nexus/api/events/delete";

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
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fault {
    pub place: FaultPlace,
    pub shape: FaultShape,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Pc,
    Phone,
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

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    Pause { start: EventId, position_ms: u32 },
    Resume { start: EventId, position_ms: u32 },
    Seek { start: EventId, from_ms: u32, to_ms: u32 },
    Search { words: String, found: u32, #[serde(rename = "where")] where_: SearchedIn },
    Open { version: String, device: DeviceKind },
    Fault { fault: Fault, count: u32 },
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub at_ms: u64,
    pub sequence: u64,
    pub event: Kind,
}

// Listening events deliberately carry names and search words. Format callers can safely
// log only the result of validation, never the body or these fields.
impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Event(..)")
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
    Status,
}

fn text_ok(text: &str, max: usize) -> bool {
    text.len() <= max && !text.chars().any(char::is_control)
}

fn nonempty(text: &str, max: usize) -> bool {
    text_ok(text, max) && !text.trim().is_empty()
}

fn version_ok(version: &str) -> bool {
    nonempty(version, VERSION_BYTES)
        && version.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'+'))
}

impl Event {
    pub fn check(&self) -> Result<(), Wrong> {
        let amount = |ms| ms <= PLAY_MAX_MS;
        let other = |id| id != self.id;
        let valid = match &self.event {
            Kind::Start { song_id, title, artist, album, from, position_ms, .. } => {
                let from_ok = match from {
                    PlayedFrom::Album { id } | PlayedFrom::Playlist { id }
                    | PlayedFrom::Artist { id } | PlayedFrom::Radio { id } | PlayedFrom::Mix { id } =>
                        nonempty(id, MUSIC_ID_BYTES),
                    PlayedFrom::Search { words } => nonempty(words, SEARCH_BYTES),
                    PlayedFrom::Queue | PlayedFrom::LikedSongs => true,
                };
                nonempty(song_id, MUSIC_ID_BYTES)
                    && [title, artist, album].iter().all(|s| text_ok(s, NAME_BYTES))
                    && from_ok && amount(*position_ms)
            }
            Kind::Stop { start, position_ms, duration_ms, heard_ms, why } => {
                other(*start)
                    && [position_ms, duration_ms, heard_ms].iter().all(|ms| ms.is_none_or(amount))
                    && (matches!(why, Stopped::Unknown)
                        || (position_ms.is_some() && duration_ms.is_some() && heard_ms.is_some()))
                    && position_ms.zip(*duration_ms).is_none_or(|(position, duration)| position <= duration)
            }
            Kind::Pause { start, position_ms } | Kind::Resume { start, position_ms } =>
                other(*start) && amount(*position_ms),
            Kind::Seek { start, from_ms, to_ms } => other(*start) && amount(*from_ms) && amount(*to_ms),
            Kind::Search { words, .. } => nonempty(words, SEARCH_BYTES),
            Kind::Open { version, .. } => version_ok(version),
            Kind::Fault { count, .. } => *count > 0,
        };
        if self.at_ms > TIME_MAX_MS || self.sequence == 0 || !valid {
            return Err(Wrong::Field);
        }
        Ok(())
    }

    fn from_value(value: Value) -> Result<Self, Wrong> {
        let event = value.get("event").and_then(Value::as_object).ok_or(Wrong::Form)?;
        // The design does not say what to do with missing stop measurements. Require
        // their keys, with explicit null allowed only for a recovered stop. Serde would
        // otherwise treat a missing key the same as null.
        if event.get("kind").and_then(Value::as_str) == Some("stop")
            && ["position_ms", "duration_ms", "heard_ms"].iter().any(|key| !event.contains_key(*key))
        {
            return Err(Wrong::Form);
        }
        // These are JSON objects even where a serde struct could also read a sequence.
        if event.get("fault").is_some_and(|v| {
            event.get("kind").and_then(Value::as_str) == Some("fault") && !v.is_object()
        }) || event.get("why").and_then(|v| v.get("fault")).is_some_and(|v| {
            event.get("kind").and_then(Value::as_str) == Some("stop")
                && event.get("why").and_then(|why| why.get("kind")).and_then(Value::as_str) == Some("error")
                && !v.is_object()
        }) {
            return Err(Wrong::Form);
        }
        let event: Self = serde_json::from_value(value).map_err(|_| Wrong::Form)?;
        event.check()?;
        Ok(event)
    }

    pub fn read(bytes: &[u8]) -> Result<Self, Wrong> {
        Self::from_value(read_json(bytes, FILE_BYTES)?)
    }

    /// A JSON object followed by LF. A file can hold whole lines only.
    pub fn line(&self) -> Result<Vec<u8>, Wrong> {
        self.check()?;
        json_line(self)
    }
}

/// The next device order number after the last one assigned. The caller keeps the last
/// number even when old journal files are removed. On exhaustion the writer must stop.
pub fn next_sequence(last: u64) -> Option<u64> {
    last.checked_add(1)
}

/// Start ids left open in this device and life, in device order. Every event is read
/// first, so a stop written earlier in a different file also closes its start.
pub fn open_starts<'a>(events: impl IntoIterator<Item = &'a Event>) -> Vec<EventId> {
    let mut starts = Vec::new();
    let mut stopped = HashSet::new();
    for event in events {
        match &event.event {
            Kind::Start { .. } => starts.push((event.sequence, event.id)),
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
    sequence: u64,
) -> Result<Event, Wrong> {
    let event = Event {
        id, at_ms, sequence,
        event: Kind::Stop {
            start,
            position_ms: saved_position.filter(|(saved, _)| *saved == start).map(|(_, ms)| ms),
            duration_ms: None,
            heard_ms: None,
            why: Stopped::Unknown,
        },
    };
    event.check()?;
    Ok(event)
}

// The design does not say the journal's byte format. One JSON header gives the account
// life, and each subsequent LF-terminated JSON line gives one event. A torn tail is
// abandoned by rolling to a new file, so even an apparently complete tail lacking its
// LF can never become a committed line after a kill.
#[derive(Serialize, Deserialize)]
struct Header {
    life: u64,
}

pub fn file_header(life: u64) -> Result<Vec<u8>, Wrong> {
    json_line(&Header { life })
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
    pub events: Vec<Event>,
    pub skipped_lines: usize,
}

pub fn read_file(bytes: &[u8]) -> Result<JournalFile, Wrong> {
    if bytes.len() > FILE_BYTES {
        return Err(Wrong::Size);
    }
    let mut lines = bytes.split_inclusive(|b| *b == b'\n');
    let header = lines.next().and_then(|line| line.strip_suffix(b"\n")).ok_or(Wrong::Form)?;
    let header: Header = serde_json::from_value(read_json(header, FILE_BYTES)?).map_err(|_| Wrong::Form)?;
    let mut file = JournalFile { life: header.life, events: Vec::new(), skipped_lines: 0 };
    for line in lines {
        match line.strip_suffix(b"\n").and_then(|line| Event::read(line).ok()) {
            Some(event) => file.events.push(event),
            None => file.skipped_lines += 1,
        }
    }
    Ok(file)
}

/// The writer supplies the trailing byte and byte length of a known journal file under
/// its own lock. If absent, torn, too full, or in another life, start a fresh file.
/// The bytes returned include the header only for a fresh file.
pub fn append(life: u64, tail: Option<FileTail>, event: &Event) -> Result<Append, Wrong> {
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
    pub pending: u64,
    pub final_at_ms: Option<u64>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RemoveFiles {
    /// Positions in the caller's oldest-first list.
    pub remove: Vec<usize>,
    /// Pending events lost to the waiting cap, to be reported as a fault.
    pub dropped: u64,
}

/// The design does not say how terminal refusals affect retention. They do not count as
/// waiting; retain a fully answered file for eight days after its last terminal answer.
/// The 64 MiB cap applies to waiting files, including a file partly taken. On restore
/// the caller marks taken events waiting again, leaving final refusals final.
pub fn remove_files(files: &[KeptFile], life: u64, now_ms: u64) -> RemoveFiles {
    let mut waiting: u128 = files.iter().filter(|file| file.life == life && file.pending > 0)
        .map(|file| u128::from(file.bytes)).sum();
    let mut plan = RemoveFiles { remove: Vec::new(), dropped: 0 };
    for (index, file) in files.iter().enumerate() {
        if file.life != life {
            plan.remove.push(index);
        } else if file.pending == 0 {
            if file.final_at_ms.and_then(|at| now_ms.checked_sub(at)).is_some_and(|age| age >= KEEP_MS) {
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

/// The scope comes from the authenticated account, never from the event body.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct EventKey<Account> {
    pub account: Account,
    pub id: EventId,
}

/// Delete every id owned by the signed-in account, across devices, in one transaction
/// after advancing its life in the store kept apart. No caller-supplied list limits it.
pub fn ids_to_delete<'a, Account: PartialEq + 'a>(
    account: &Account,
    keys: impl IntoIterator<Item = &'a EventKey<Account>>,
) -> impl Iterator<Item = EventId> {
    keys.into_iter().filter(move |key| &key.account == account).map(|key| key.id)
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

/// A nonempty batch of events of one account life. Make using the size of the wire
/// envelope available to the caller, so adding authentication cannot exceed 1 MiB.
pub struct Batch {
    life: u64,
    events: Vec<Event>,
}

impl Batch {
    pub fn life(&self) -> u64 {
        self.life
    }

    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// Take the longest prefix that fits. The byte limit includes this JSON object.
    pub fn prefix(life: u64, events: &[Event], max_bytes: usize) -> Result<Self, Wrong> {
        let limit = max_bytes.min(BATCH_BYTES);
        let mut bytes = format!(r#"{{"life":{life},"events":[]}}"#).len();
        let mut taken = Vec::new();
        let mut seen = HashSet::new();
        for event in events.iter().take(BATCH_EVENTS) {
            event.check()?;
            let event_bytes = serde_json::to_vec(event).map_err(|_| Wrong::Form)?.len();
            bytes += event_bytes + usize::from(!taken.is_empty());
            if bytes > limit {
                break;
            }
            if !seen.insert(event.id) {
                return Err(Wrong::RepeatedId);
            }
            taken.push(event.clone());
        }
        if taken.is_empty() {
            return Err(if events.is_empty() { Wrong::Count } else { Wrong::Size });
        }
        Ok(Self { life, events: taken })
    }

    pub fn write(&self) -> Result<Vec<u8>, Wrong> {
        #[derive(Serialize)]
        struct Wire<'a> {
            life: u64,
            events: &'a [Event],
        }
        serde_json::to_vec(&Wire { life: self.life, events: &self.events }).map_err(|_| Wrong::Form)
    }
}

pub struct IncomingBatch {
    pub life: u64,
    pub events: Vec<(EventId, Result<Event, Wrong>)>,
}

impl IncomingBatch {
    /// The design does not say how to answer a malformed entry with no usable id.
    /// Refuse that batch. An entry with a usable id and bad fields can be refused on
    /// its own. Bad JSON, including a duplicate key anywhere, refuses the whole body.
    pub fn read(bytes: &[u8]) -> Result<Self, Wrong> {
        let mut object = read_json(bytes, BATCH_BYTES)?;
        let life = object.get("life").and_then(Value::as_u64).ok_or(Wrong::Form)?;
        let Some(Value::Array(values)) = object.as_object_mut().and_then(|o| o.remove("events")) else {
            return Err(Wrong::Form);
        };
        if values.is_empty() || values.len() > BATCH_EVENTS {
            return Err(Wrong::Count);
        }
        let mut events = Vec::with_capacity(values.len());
        let mut seen = HashSet::new();
        for value in values {
            let id = value.get("id").and_then(Value::as_str).and_then(EventId::from_hex).ok_or(Wrong::Form)?;
            if !seen.insert(id) {
                return Err(Wrong::RepeatedId);
            }
            events.push((id, Event::from_value(value)));
        }
        Ok(Self { life, events })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Taken,
    Refused,
    NotDealtWith,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answered {
    pub id: EventId,
    pub outcome: Outcome,
}

#[derive(Debug, PartialEq, Eq)]
pub struct BatchAnswer(Vec<Answered>);

impl BatchAnswer {
    pub fn new(answers: Vec<Answered>) -> Result<Self, Wrong> {
        if answers.len() > BATCH_EVENTS {
            return Err(Wrong::Count);
        }
        let mut seen = HashSet::new();
        if answers.iter().any(|answer| !seen.insert(answer.id)) {
            return Err(Wrong::RepeatedId);
        }
        Ok(Self(answers))
    }

    pub fn read(status: u16, whole_body: &[u8], sent: &Batch) -> Result<Self, Wrong> {
        // The design does not say this call's exact success status. Only a fully
        // read 200 with this shape can acknowledge anything.
        if status != 200 {
            return Err(Wrong::Status);
        }
        let value = read_json(whole_body, BATCH_BYTES)?;
        let answers = value.get("answers").and_then(Value::as_array).ok_or(Wrong::Form)?;
        if answers.len() > BATCH_EVENTS {
            return Err(Wrong::Count);
        }
        let wanted: HashSet<_> = sent.events().iter().map(|event| event.id).collect();
        let mut decoded = Vec::with_capacity(answers.len());
        for answer in answers {
            if !answer.is_object() {
                return Err(Wrong::Form);
            }
            let answer: Answered = serde_json::from_value(answer.clone()).map_err(|_| Wrong::Form)?;
            if !wanted.contains(&answer.id) {
                return Err(Wrong::UnaskedId);
            }
            decoded.push(answer);
        }
        Self::new(decoded)
    }

    pub fn write(&self) -> Result<Vec<u8>, Wrong> {
        #[derive(Serialize)]
        struct Wire<'a> {
            answers: &'a [Answered],
        }
        serde_json::to_vec(&Wire { answers: &self.0 }).map_err(|_| Wrong::Form)
    }

    /// A reply can be sparse and in any order. An absent id has no final answer.
    pub fn outcome(&self, id: EventId) -> Outcome {
        self.0.iter().find(|answer| answer.id == id).map_or(Outcome::NotDealtWith, |a| a.outcome)
    }
}

/// Read one complete JSON object. The first pass checks every key and the depth even
/// in unknown fields, which the typed readers then ignore. Never return a serde error
/// to a caller: it can quote a field value.
pub(crate) fn read_json(bytes: &[u8], limit: usize) -> Result<Value, Wrong> {
    if bytes.len() > limit {
        return Err(Wrong::Size);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| Wrong::Form)?;
    let mut json = serde_json::Deserializer::from_str(text);
    let value = Json(0).deserialize(&mut json).map_err(|_| Wrong::Form)?;
    json.end().map_err(|_| Wrong::Form)?;
    if !value.is_object() {
        return Err(Wrong::Form);
    }
    Ok(value)
}

struct Json(usize);

impl<'de> DeserializeSeed<'de> for Json {
    type Value = Value;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Json {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        if self.0 >= super::proto::HELLO_MAX_DEPTH {
            return Err(de::Error::custom("nested too deep"));
        }
        let mut object = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            let entry = map.next_value_seed(Json(self.0 + 1))?;
            if object.insert(key, entry).is_some() {
                return Err(de::Error::custom("a key given twice"));
            }
        }
        Ok(Value::Object(object))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        if self.0 >= super::proto::HELLO_MAX_DEPTH {
            return Err(de::Error::custom("nested too deep"));
        }
        let mut array = Vec::new();
        while let Some(entry) = seq.next_element_seed(Json(self.0 + 1))? {
            array.push(entry);
        }
        Ok(Value::Array(array))
    }

    fn visit_str<E: de::Error>(self, text: &str) -> Result<Value, E> {
        Ok(Value::String(text.to_owned()))
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Number::from_f64(value).map(Value::Number).ok_or_else(|| de::Error::custom("not a JSON number"))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
}

#[cfg(test)]
mod tests;
