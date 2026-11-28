//! The clean Diagnostics page, its one upload, and when to send it (§14).
//! The app supplies observations and time. Storage, transport and the device switch
//! itself belong to the caller.

use std::collections::{BTreeSet, VecDeque};
use std::fmt::{self, Write};

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::Serialize;
use serde_json::value::RawValue;

use super::account::{self, DeviceKind, Reply, ReplyResult, RequestHead, RequestId};
use super::events::{LossCounts, TIME_MAX_MS};
use super::proto::Version;
use super::wire::{self, BadMessage, Fields, WireBody};

pub use super::events::{Fault, FaultPlace, FaultShape, Wrong};

pub const MAX_ENTRIES: usize = 200;
pub const MAX_LINE_CHARS: usize = 300;
pub const CALL_BYTES: usize = 64 * 1024;
pub const MIN_GAP_MS: u64 = 10 * 60 * 1000;
pub const DAY_MS: u64 = 24 * 60 * 60 * 1000;
pub const MAX_PER_DAY: usize = 30;
pub const STORED_REPORTS: usize = 31;
pub const MAX_AGE_MS: u64 = 30 * DAY_MS;

// The design does not say what "a few minutes" means. Wait five minutes, coalescing
// faults during that wait without resetting its start.
pub const FAULT_WAIT_MS: u64 = 5 * 60 * 1000;

// The design does not say the call's path or JSON keys. The common authenticated
// envelope supplies the operation id (§12.2); the payload is just the displayed text.
pub const SEND_PATH: &str = "/nexus/api/diag/send";

/// The default for a device with no preference. Pass the server default applicable
/// to this device. Once the person chooses on or off, that choice wins every time.
pub fn enabled(device: Option<bool>, server_default: Option<bool>) -> bool {
    device.or(server_default).unwrap_or(true)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Due {
    Fault,
    Daily,
}

/// The caller keeps this per device, including across restarts. "Sent" means the
/// attempt started; a missing reply wastes only that attempt. Record before I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    started_ms: u64,
    fault_ms: Option<u64>,
    attempts: VecDeque<u64>,
}

impl Schedule {
    pub fn new(started_ms: u64) -> Option<Self> {
        Self::restore(started_ms, None, &[])
    }

    pub fn restore(started_ms: u64, fault_ms: Option<u64>, attempts: &[u64]) -> Option<Self> {
        if started_ms > TIME_MAX_MS
            || attempts.len() > MAX_PER_DAY
            || attempts
                .iter()
                .any(|&at| !(started_ms..=TIME_MAX_MS).contains(&at))
            || attempts.windows(2).any(|pair| match pair {
                [before, after] => !elapsed(*before, *after, MIN_GAP_MS),
                _ => true,
            })
            || fault_ms.is_some_and(|at| {
                !(attempts.last().copied().unwrap_or(started_ms)..=TIME_MAX_MS).contains(&at)
            })
        {
            return None;
        }
        Some(Self {
            started_ms,
            fault_ms,
            attempts: attempts.iter().copied().collect(),
        })
    }

    pub fn started_ms(&self) -> u64 {
        self.started_ms
    }

    pub fn fault_ms(&self) -> Option<u64> {
        self.fault_ms
    }

    pub fn attempts(&self) -> impl Iterator<Item = u64> + '_ {
        self.attempts.iter().copied()
    }

    pub fn note_fault(&mut self, at_ms: u64) {
        let latest = self.attempts.back().copied().unwrap_or(self.started_ms);
        if (latest..=TIME_MAX_MS).contains(&at_ms) {
            self.fault_ms = Some(self.fault_ms.map_or(at_ms, |was| was.min(at_ms)));
        }
    }

    /// The design does not say where a day begins. Use a rolling 24-hour cap, and a
    /// full day from first use or the last attempt for the normal timer. When the
    /// clock goes backwards or the app is idle, wait.
    pub fn due(&self, now_ms: u64, enabled: bool, in_use: bool) -> Option<Due> {
        if !enabled
            || !in_use
            || now_ms > TIME_MAX_MS
            || now_ms < self.started_ms
            || self.fault_ms.is_some_and(|at| at > now_ms)
            || self
                .attempts
                .back()
                .is_some_and(|&at| !elapsed(at, now_ms, MIN_GAP_MS))
        {
            return None;
        }
        if self.attempts.len() == MAX_PER_DAY
            && self
                .attempts
                .front()
                .is_some_and(|&at| !elapsed(at, now_ms, DAY_MS))
        {
            return None;
        }
        if self
            .fault_ms
            .is_some_and(|at| elapsed(at, now_ms, FAULT_WAIT_MS))
        {
            Some(Due::Fault)
        } else if elapsed(
            self.attempts.back().copied().unwrap_or(self.started_ms),
            now_ms,
            DAY_MS,
        ) {
            Some(Due::Daily)
        } else {
            None
        }
    }

    /// Consume one due attempt. A report sent by either timer covers the notes so far.
    pub fn take_due(&mut self, now_ms: u64, enabled: bool, in_use: bool) -> Option<Due> {
        let due = self.due(now_ms, enabled, in_use)?;
        self.attempts.retain(|&at| !elapsed(at, now_ms, DAY_MS));
        self.attempts.push_back(now_ms);
        self.fault_ms = None;
        Some(due)
    }
}

fn elapsed(from: u64, to: u64, ms: u64) -> bool {
    to.checked_sub(from).is_some_and(|age| age >= ms)
}

/// Indices of the reports the server can keep. Give their server receipt times,
/// latest first, for one device. Device or account deletion removes all of them.
pub fn reports_to_keep(received: &[u64], now_ms: u64) -> impl Iterator<Item = usize> {
    received
        .iter()
        .enumerate()
        .filter_map(move |(index, &at)| {
            (now_ms.checked_sub(at).is_some_and(|age| age <= MAX_AGE_MS)).then_some(index)
        })
        .take(STORED_REPORTS)
}

// The design does not say which device details and line shapes are safe to carry.
// Only the templates below are accepted. No caller-supplied name, address, path,
// package identifier, device model string, decoder name or build tag is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct About {
    pub version: Version,
    pub device: DeviceKind,
    /// Use zero for a component the OS omits. Android 14 is written as 14.0.0.
    pub os: Version,
    pub signed_in: bool,
    pub https: bool,
    pub offline: bool,
    /// Whether local network permission is held. PCs have no such setting.
    pub local_permission: Option<bool>,
    pub android_auto: Option<bool>,
    pub loss: LossCounts,
}

macro_rules! words {
    ($name:ident { $($variant:ident => $label:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name { $($variant),+ }

        impl $name {
            fn text(self) -> &'static str {
                match self { $(Self::$variant => $label),+ }
            }

            fn read(text: &str) -> Option<Self> {
                match text { $($label => Some(Self::$variant),)+ _ => None }
            }
        }
    };
}

words!(Reach {
    None => "nothing reached",
    NameLookedUp => "name looked up",
    ConnectionOpened => "connection opened",
    MadeSecure => "made secure",
    Answered => "answered",
});
words!(Network {
    Wifi => "Wi-Fi",
    Mobile => "mobile",
    Ethernet => "Ethernet",
    Vpn => "VPN",
    Other => "other",
    Unknown => "unknown",
});
words!(Note {
    PlayerStarted => "Player started",
    ForegroundEntered => "Foreground entered",
    SoundMoved => "Audio output is moving",
    AndroidAutoAdmitted => "Android Auto connected",
    AndroidAutoRefused => "Android Auto connection refused",
    OtherControllerAdmitted => "Other controller connected",
    OtherControllerRefused => "Other controller connection refused",
    SignInDone => "Nexus sign-in done",
    SignInRefused => "Nexus sign-in refused",
    SignInTooManyTries => "Nexus sign-in had too many tries",
    MusicLoginAskedAgain => "Music login asked for again",
    StartAgain => "Nexus asked to start again",
    EventsSent => "Events sent",
    EventsRefused => "Events refused",
    AppMessage => "App message shown",
    DownloadsFolderChanged => "Downloads folder changed",
    MediaButtonStarted => "Media button started the player",
    MediaButtonNothingToResume => "Media button had nothing to resume",
    MediaButtonStartRefused => "Android refused to start the player from a media button",
    SharedLibraryUnavailable => "Shared library could not be loaded",
});
words!(SessionEnded {
    SignedOut => "signed out",
    ForcedOut => "forced out",
    Expired => "expired",
    PasswordChanged => "password changed",
    PasswordReset => "password reset",
    DataPutBack => "data put back",
    AccountSwitchedOff => "account switched off",
    AccountDeleted => "account deleted",
    Other => "other",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trace {
    pub reach: Reach,
    pub local_address: Option<bool>,
    pub local_permission: Option<bool>,
    pub network: Network,
}

impl Trace {
    fn read(text: &str) -> Option<Self> {
        let (reach, rest) = text.split_once(", local address ")?;
        let (local, rest) = rest.split_once(", local permission ")?;
        let (permission, network) = rest.split_once(", network ")?;
        Some(Self {
            reach: Reach::read(reach)?,
            local_address: read_maybe(local)?,
            local_permission: read_maybe(permission)?,
            network: Network::read(network)?,
        })
    }
}

impl fmt::Display for Trace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}, local address {}, local permission {}, network {}",
            self.reach.text(),
            maybe(self.local_address),
            maybe(self.local_permission),
            self.network.text()
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Line {
    Note(Note),
    Fault(Fault),
    SessionEnded(SessionEnded),
    ForegroundAfterMs(u32),
    // The design does not say how to carry the platform's reason. Keep only its code.
    DownloadJobStopped(u8),
    LastAttempt(Trace),
    LastFailure(Trace),
}

// The design does not say a timing observation's range. Cap it at one hour.
pub const MAX_FOREGROUND_MS: u32 = 60 * 60 * 1000;

const PLACES: &[(FaultPlace, &str)] = &[
    (FaultPlace::Playback, "playback"),
    (FaultPlace::AudioOutput, "audio output"),
    (FaultPlace::Library, "library"),
    (FaultPlace::Downloads, "downloads"),
    (FaultPlace::Foreground, "foreground"),
    (FaultPlace::MusicServer, "music server"),
    (FaultPlace::NexusServer, "Nexus server"),
    (FaultPlace::Journal, "journal"),
    (FaultPlace::App, "app"),
];

const SHAPES: &[(FaultShape, &str)] = &[
    (FaultShape::NameLookup, "name lookup"),
    (FaultShape::Connection, "connection"),
    (FaultShape::SecureConnection, "secure connection"),
    (FaultShape::Timeout, "timeout"),
    (FaultShape::Permission, "permission"),
    (FaultShape::Refused, "refused"),
    (FaultShape::Unavailable, "unavailable"),
    (FaultShape::UnexpectedAnswer, "unexpected answer"),
    (FaultShape::Read, "read"),
    (FaultShape::Write, "write"),
    (FaultShape::Decode, "decode"),
    (FaultShape::NoSound, "no sound"),
    (FaultShape::QueueFull, "queue full"),
    (FaultShape::JournalFull, "journal full"),
    (FaultShape::BorrowConflict, "borrow conflict"),
    (FaultShape::PoisonedLock, "poisoned lock"),
    (FaultShape::MissingValue, "missing value"),
    (FaultShape::UnexpectedResultError, "unexpected result error"),
    (FaultShape::Bounds, "out of bounds"),
    (FaultShape::Assertion, "assertion failed"),
    (FaultShape::Overflow, "overflow"),
    (FaultShape::NoPayload, "no panic payload"),
    (FaultShape::Panic, "panic"),
    (FaultShape::Other, "other"),
];

/// Classify the Rust panic payload using fixed markers. The result has no text field.
/// The design does not say how to recognize panic kinds. Unknown wording is a panic
/// without a more specific shape. The Windows hook writes "no message" for a
/// non-text payload.
pub fn classify_rust_panic(message: &str) -> FaultShape {
    if matches!(message, "no message" | "no payload") || message.chars().all(char::is_whitespace) {
        FaultShape::NoPayload
    } else if ["BorrowMutError", "BorrowError", "already borrowed", "already mutably borrowed"]
        .iter()
        .any(|marker| message.contains(marker))
    {
        FaultShape::BorrowConflict
    } else if message.contains("PoisonError") {
        FaultShape::PoisonedLock
    } else if message.contains("called `Option::unwrap()` on a `None` value") {
        FaultShape::MissingValue
    } else if message.contains("called `Result::unwrap()` on an `Err` value") {
        FaultShape::UnexpectedResultError
    } else if ["index out of bounds", "out of range for slice", "is not a char boundary", "slice index starts at"]
        .iter()
        .any(|marker| message.contains(marker))
    {
        FaultShape::Bounds
    } else if message.contains("assertion failed")
        || (message.contains("assertion `") && message.contains("` failed"))
    {
        FaultShape::Assertion
    } else if message.contains("with overflow") || message.contains("capacity overflow") {
        FaultShape::Overflow
    } else {
        FaultShape::Panic
    }
}

fn label<T: PartialEq>(table: &[(T, &'static str)], what: T) -> &'static str {
    table
        .iter()
        .find_map(|(key, label)| (*key == what).then_some(*label))
        .unwrap_or("other")
}

fn word<T: Copy>(table: &[(T, &str)], text: &str) -> Option<T> {
    table
        .iter()
        .find_map(|(key, label)| (*label == text).then_some(*key))
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Note(note) => f.write_str(note.text()),
            Self::Fault(fault) => write!(
                f,
                "Fault in {}: {}",
                label(PLACES, fault.place),
                label(SHAPES, fault.shape)
            ),
            Self::SessionEnded(why) => write!(f, "Nexus session ended: {}", why.text()),
            Self::ForegroundAfterMs(ms) => write!(f, "Foreground entered after {ms} ms"),
            Self::DownloadJobStopped(reason) => {
                write!(f, "Android stopped the download job (reason {reason})")
            }
            Self::LastAttempt(trace) => write!(f, "Nexus last attempt: {trace}"),
            Self::LastFailure(trace) => write!(f, "Nexus last failure: {trace}"),
        }
    }
}

impl Line {
    fn read(text: &str) -> Option<Self> {
        if let Some(note) = Note::read(text) {
            return Some(Self::Note(note));
        }
        if let Some(fault) = text.strip_prefix("Fault in ") {
            let (place, shape) = fault.split_once(": ")?;
            return Some(Self::Fault(Fault {
                place: word(PLACES, place)?,
                shape: word(SHAPES, shape)?,
            }));
        }
        if let Some(why) = text.strip_prefix("Nexus session ended: ") {
            return SessionEnded::read(why).map(Self::SessionEnded);
        }
        if let Some(ms) = text
            .strip_prefix("Foreground entered after ")
            .and_then(|ms| ms.strip_suffix(" ms"))
        {
            let ms = number(ms)?;
            return (ms <= MAX_FOREGROUND_MS).then_some(Self::ForegroundAfterMs(ms));
        }
        if let Some(reason) = text.strip_prefix("Android stopped the download job (reason ").and_then(|reason| reason.strip_suffix(')')) {
            return Some(Self::DownloadJobStopped(u8::try_from(number(reason)?).ok()?));
        }
        if let Some(trace) = text.strip_prefix("Nexus last attempt: ") {
            return Trace::read(trace).map(Self::LastAttempt);
        }
        text.strip_prefix("Nexus last failure: ")
            .and_then(Trace::read)
            .map(Self::LastFailure)
    }

    fn latest_slot(self) -> Option<LatestSlot> {
        match self {
            Self::LastAttempt(_) => Some(LatestSlot::Attempt),
            Self::LastFailure(_) => Some(LatestSlot::Failure),
            // The design does not say how to group timing values. Keep only the latest.
            Self::ForegroundAfterMs(_) => Some(LatestSlot::Foreground),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum LatestSlot {
    Attempt,
    Failure,
    Foreground,
}

fn number(text: &str) -> Option<u32> {
    u32::try_from(decimal(text)?).ok()
}

fn decimal(text: &str) -> Option<u64> {
    if text.is_empty() || text.len() > 20 || !text.bytes().all(|b| b.is_ascii_digit())
        || (text.len() > 1 && text.starts_with('0'))
    {
        return None;
    }
    text.parse().ok()
}

/// Match only a fixed template, after the phone's whitespace cleanup. Unknown raw
/// lines are dropped as a whole. Truncating unknown text could publish part of a secret.
pub fn clean_line(raw: &str) -> Option<Line> {
    if raw.len() > MAX_LINE_CHARS {
        return None;
    }
    let single_line = raw.replace(['\r', '\n', '\t'], " ");
    Line::read(single_line.trim())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub first_ms: u64,
    pub last_ms: u64,
    pub count: u32,
    pub line: Line,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Record {
    // In observation order, most recently seen last.
    entries: VecDeque<Entry>,
}

impl Record {
    pub fn entries(&self) -> &VecDeque<Entry> {
        &self.entries
    }

    pub fn note(&mut self, at_ms: u64, line: Line) {
        self.note_times(at_ms, line, 1);
    }

    pub fn note_times(&mut self, at_ms: u64, line: Line, count: u32) {
        if at_ms > TIME_MAX_MS
            || count == 0
            || matches!(line, Line::ForegroundAfterMs(ms) if ms > MAX_FOREGROUND_MS)
        {
            return;
        }
        let count = if line.latest_slot().is_some() {
            1
        } else {
            count
        };
        self.add(Entry {
            first_ms: at_ms,
            last_ms: at_ms,
            count,
            line,
        });
    }

    pub fn note_text(&mut self, at_ms: u64, raw: &str) {
        if let Some(line) = clean_line(raw) {
            self.note(at_ms, line);
        }
    }

    pub fn note_attempt(&mut self, at_ms: u64, trace: Trace, failed: bool) {
        self.note(at_ms, Line::LastAttempt(trace));
        if failed {
            self.note(at_ms, Line::LastFailure(trace));
        }
    }

    /// Apply observations made while the app read the earlier record from disk.
    pub fn merge_later(&mut self, later: Self) {
        for entry in later.entries {
            self.add(entry);
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    fn add(&mut self, mut entry: Entry) {
        let slot = entry.line.latest_slot();
        let index = self.entries.iter().position(|old| {
            if slot.is_some() {
                old.line.latest_slot() == slot
            } else {
                old.line == entry.line
            }
        });
        if let Some(old) = index.and_then(|index| self.entries.remove(index)) {
            if slot.is_none() {
                // The clock may move backwards. Keep the extreme times and observation order.
                entry.first_ms = entry.first_ms.min(old.first_ms);
                entry.last_ms = entry.last_ms.max(old.last_ms);
                entry.count = entry.count.saturating_add(old.count);
            }
        }
        self.entries.push_back(entry);
        if self.entries.len() > MAX_ENTRIES {
            // The design does not say how to keep both required traces when the record
            // fills. Reserve their places and age out the oldest other observation.
            if let Some(index) = self.entries.iter().position(|entry| {
                !matches!(
                    entry.line.latest_slot(),
                    Some(LatestSlot::Attempt | LatestSlot::Failure)
                )
            }) {
                self.entries.remove(index);
            }
        }
    }
}

const HEADING: &str = "What the player has met (times in UTC):";
const EMPTY: &str = "  (nothing yet)";
// Use the phone's whole-second display. The file/report can only restore this
// precision. In-memory observations keep the supplied millisecond times.
const DATE: &str = "%Y-%m-%d %H:%M:%S";
const LINE_FULL: &str = "Events dropped because the journal queue was full: ";
const FILES_FULL: &str = "Events lost because the waiting files were full: ";
const REFUSED_FOR_GOOD: &str = "Events refused for good: ";

fn yes(on: bool) -> &'static str {
    if on { "yes" } else { "no" }
}

fn maybe(on: Option<bool>) -> &'static str {
    on.map_or("unknown", yes)
}

fn read_yes(text: &str) -> Option<bool> {
    match text {
        "yes" => Some(true),
        "no" => Some(false),
        _ => None,
    }
}

fn read_maybe(text: &str) -> Option<Option<bool>> {
    if text == "unknown" {
        Some(None)
    } else {
        read_yes(text).map(Some)
    }
}

fn date(at: u64) -> Result<DateTime<Utc>, Wrong> {
    i64::try_from(at)
        .ok()
        .and_then(DateTime::from_timestamp_millis)
        .ok_or(Wrong::Field)
}

fn read_date(text: &str) -> Option<u64> {
    let at = NaiveDateTime::parse_from_str(text, DATE).ok()?.and_utc();
    let ms = u64::try_from(at.timestamp_millis()).ok()?;
    (ms <= TIME_MAX_MS).then_some(ms)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub about: About,
    pub record: Record,
}

impl Report {
    /// The design does not say the text's grammar. A canonical fixed-template report
    /// gives both apps one text to show, allows their record to be read back, and lets
    /// the server reject a text containing anything outside that vocabulary.
    pub fn text(&self) -> Result<String, Wrong> {
        let mut text = String::new();
        let a = self.about;
        let device = match a.device {
            DeviceKind::Pc => "PC",
            DeviceKind::Phone => "phone",
        };
        if a.device == DeviceKind::Pc && (a.android_auto.is_some() || a.local_permission.is_some())
        {
            return Err(Wrong::Field);
        }
        write!(text,
            "Nexus Music {} on {device} (OS {})\nSigned in: {}\nHTTPS: {}\nOffline: {}\nLocal network permission: {}\nAndroid Auto: {}\n{LINE_FULL}{}\n{FILES_FULL}{}\n{REFUSED_FOR_GOOD}{}\n\n{HEADING}",
            a.version, a.os, yes(a.signed_in), yes(a.https), yes(a.offline), maybe(a.local_permission), maybe(a.android_auto), a.loss.line_full, a.loss.files_full, a.loss.refused,
        ).map_err(|_| Wrong::Form)?;
        if self.record.entries.is_empty() {
            write!(text, "\n{EMPTY}").map_err(|_| Wrong::Form)?;
        }
        for entry in &self.record.entries {
            let last = date(entry.last_ms)?;
            let start = text.len();
            write!(text, "\n  {}  {}", last.format(DATE), entry.line).map_err(|_| Wrong::Form)?;
            if entry.count > 1 {
                let first = date(entry.first_ms)?;
                write!(
                    text,
                    "  ({} times since {})",
                    entry.count,
                    first.format(DATE)
                )
                .map_err(|_| Wrong::Form)?;
            }
            if text
                .get(start..)
                .is_none_or(|line| line.trim_start_matches('\n').chars().count() > MAX_LINE_CHARS)
            {
                return Err(Wrong::Size);
            }
        }
        Ok(text)
    }

    pub fn read(text: &str) -> Result<Self, Wrong> {
        if text.len() > CALL_BYTES {
            return Err(Wrong::Size);
        }
        let report = Self::parse(text).ok_or(Wrong::Form)?;
        if report.text()? != text {
            return Err(Wrong::Form);
        }
        Ok(report)
    }

    fn parse(text: &str) -> Option<Self> {
        let mut lines = text.split('\n');
        let (version, rest) = lines
            .next()?
            .strip_prefix("Nexus Music ")?
            .split_once(" on ")?;
        let (device, os) = rest.strip_suffix(')')?.split_once(" (OS ")?;
        let about = About {
            version: Version::parse(version)?,
            device: match device {
                "PC" => DeviceKind::Pc,
                "phone" => DeviceKind::Phone,
                _ => return None,
            },
            os: Version::parse(os)?,
            signed_in: read_yes(lines.next()?.strip_prefix("Signed in: ")?)?,
            https: read_yes(lines.next()?.strip_prefix("HTTPS: ")?)?,
            offline: read_yes(lines.next()?.strip_prefix("Offline: ")?)?,
            local_permission: read_maybe(
                lines.next()?.strip_prefix("Local network permission: ")?,
            )?,
            android_auto: read_maybe(lines.next()?.strip_prefix("Android Auto: ")?)?,
            loss: LossCounts {
                line_full: decimal(lines.next()?.strip_prefix(LINE_FULL)?)?,
                files_full: decimal(lines.next()?.strip_prefix(FILES_FULL)?)?,
                refused: decimal(lines.next()?.strip_prefix(REFUSED_FOR_GOOD)?)?,
            },
        };
        if lines.next()? != "" || lines.next()? != HEADING {
            return None;
        }
        let mut record = Record::default();
        let first = lines.next()?;
        if first == EMPTY {
            return lines.next().is_none().then_some(Self { about, record });
        }
        let mut seen = BTreeSet::new();
        let mut slots = BTreeSet::new();
        for line in std::iter::once(first).chain(lines) {
            if record.entries.len() == MAX_ENTRIES || line.len() > MAX_LINE_CHARS {
                return None;
            }
            let (last, rest) = line.strip_prefix("  ")?.split_once("  ")?;
            let last_ms = read_date(last)?;
            let (words, first_ms, count) =
                if let Some((words, count_and_first)) = rest.split_once("  (") {
                    let (count, first) = count_and_first
                        .strip_suffix(')')?
                        .split_once(" times since ")?;
                    (words, read_date(first)?, number(count)?)
                } else {
                    (rest, last_ms, 1)
                };
            let line = Line::read(words)?;
            if first_ms > last_ms || count == 0 || (count == 1 && first_ms != last_ms) {
                return None;
            }
            if let Some(slot) = line.latest_slot() {
                if !slots.insert(slot) || count != 1 {
                    return None;
                }
            }
            // Line's text is entirely fixed or a bounded number, and is the identity
            // used by the phone to count repeated observations.
            if !seen.insert(words) {
                return None;
            }
            record.entries.push_back(Entry {
                first_ms,
                last_ms,
                count,
                line,
            });
        }
        Some(Self { about, record })
    }
}

/// The one upload. The shared request head carries the protocol and restore
/// numbers. The request id identifies this write under §12.2.
#[derive(Serialize)]
pub struct Upload {
    pub head: RequestHead,
    pub request_id: RequestId,
    text: String,
}

impl Upload {
    pub fn new(head: RequestHead, request_id: RequestId, report: &Report) -> Result<Self, Wrong> {
        Ok(Self {
            head,
            request_id,
            text: report.text()?,
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn write(&self) -> Result<WireBody, Wrong> {
        wire::encode(self).map_err(Into::into)
    }

    pub fn read(whole_body: &[u8]) -> Result<Self, Wrong> {
        wire::read(whole_body).map_err(Into::into)
    }
}

impl wire::Value for Upload {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let fields = Fields::read(raw)?;
        let text = wire::json_string(fields.0.get("text").ok_or(BadMessage::Form)?)?;
        Report::read(&text).map_err(|_| BadMessage::Form)?;
        Ok(Self {
            head: fields.get("head")?,
            request_id: fields.get("request_id")?,
            text: text.to_string(),
        })
    }
}

impl wire::Message for Upload {
    const MAX_BYTES: usize = CALL_BYTES;
}

// The design does not say what goes in the successful result beyond the shared
// receipt. Use an empty object so the upload answer never returns a report.
wire::object! { Uploaded {} }
impl ReplyResult for Uploaded {}

pub type UploadReply = Reply<Uploaded>;

pub fn read_reply(status: u16, whole_body: &[u8]) -> Result<UploadReply, Wrong> {
    account::read_reply(status, whole_body).map_err(Into::into)
}

pub fn write_reply(reply: &UploadReply) -> Result<WireBody, Wrong> {
    wire::encode(reply).map_err(Into::into)
}

#[cfg(test)]
mod tests;
