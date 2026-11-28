//! Takes listening observations on the window thread. The journal worker owns the disk.

use std::cell::{Cell, RefCell};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use nexus_core::nexus::{
    account::AccountId,
    client::{Happened, JournalAccount, JournalDo, Saved}, events::{self, DeviceKind, EventId, Kind, LossCounts}, proto::BackendId,
};
use serde::{Deserialize, Serialize};
use slint::ComponentHandle;

pub(crate) type SearchTicket = EventId;

pub(crate) use events::{Began, Fault, FaultPlace, PlayedFrom, Sound, Stopped};

mod reader;
mod writer;

const QUEUE_LENGTH: usize = 256;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct Account {
    backend: [u8; 16],
    number: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Scope {
    account: Account,
    life: u64,
}

struct Observation {
    id: EventId,
    at: u64,
    kind: Kind,
}

enum Message {
    Event(Scope, Observation),
    Checkpoint(Scope, EventId, u32),
    Wake,
}

#[derive(Default)]
struct Counters {
    dropped: AtomicU64,
    line_full: AtomicU64,
    files_full: AtomicU64,
    refused: AtomicU64,
    closing: AtomicBool,
}

struct Service {
    send: SyncSender<Message>,
    finished: Mutex<Receiver<()>>,
    read: SyncSender<reader::Read>,
    update: mpsc::Sender<reader::ChangeRequest>,
    waiting: Arc<Mutex<Option<reader::Waiting>>>,
}

impl Service {
    fn change(&self, request: reader::ChangeRequest) {
        if self.update.send(request).is_ok() {
            // A full event line already wakes the writer.
            let _ = self.send.try_send(Message::Wake);
        }
    }
}

static SERVICE: OnceLock<Service> = OnceLock::new();
static COUNTERS: Counters = Counters {
    dropped: AtomicU64::new(0), line_full: AtomicU64::new(0),
    files_full: AtomicU64::new(0), refused: AtomicU64::new(0),
    closing: AtomicBool::new(false),
};

impl Counters {
    fn drop_event(&self) {
        let _ = self.dropped.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| Some(n.saturating_add(1)));
    }

    fn losses(&self) -> LossCounts {
        LossCounts {
            line_full: self.line_full.load(Ordering::Relaxed)
                .saturating_add(self.dropped.load(Ordering::Relaxed)),
            files_full: self.files_full.load(Ordering::Relaxed),
            refused: self.refused.load(Ordering::Relaxed),
        }
    }
}

thread_local! {
    static OPENED: Cell<bool> = const { Cell::new(false) };
    static GENERATION: Cell<u64> = const { Cell::new(0) };
    static SCOPE: Cell<Option<Scope>> = const { Cell::new(None) };
    static PLAY: RefCell<Play> = const { RefCell::new(Play::new()) };
    static SEARCH: RefCell<Vec<Search>> = const { RefCell::new(Vec::new()) };
}

pub(crate) fn install(root: PathBuf) {
    let (send, receive) = mpsc::sync_channel(QUEUE_LENGTH);
    let (done, finished) = mpsc::channel();
    let (read, reads) = mpsc::sync_channel::<reader::Read>(1);
    let (update, updates) = mpsc::channel();
    let waiting = Arc::new(Mutex::new(None));
    let writer_waiting = waiting.clone();
    let reader_root = root.clone();
    if std::thread::Builder::new().name("listening-reader".into()).spawn(move || {
        while let Ok(read) = reads.recv() {
            read.run(&reader_root);
        }
    }).is_err() {
        return;
    }
    if std::thread::Builder::new().name("listening-journal".into()).spawn(move || {
        writer::run(root, receive, updates, writer_waiting, &COUNTERS);
        let _ = done.send(());
    }).is_ok() {
        let _ = SERVICE.set(Service { send, finished: Mutex::new(finished), read, update, waiting });
    }
}

/// W7 publishes only its current authenticated saved state, on the window thread.
/// C10 removes the pending notice when the person has seen it.
#[allow(dead_code)] // The live publisher belongs to W7.
pub(crate) fn publish_account(saved: Option<&Saved>) {
    let scope = saved
        .filter(|saved| saved.session.is_some() && saved.notice.is_none())
        .and_then(|saved| Some(Scope::new(JournalAccount {
            backend_id: BackendId::from_hex(&saved.backend_id)?,
            account: saved.account,
        }, saved.life)));
    publish(scope);
}

fn publish(scope: Option<Scope>) {
    if SCOPE.get() == scope {
        return;
    }
    stop(Stopped::Stopped);
    for prior in SEARCH.with_borrow_mut(std::mem::take) {
        finish_search(prior, 0, None);
    }
    PLAY.with_borrow_mut(|play| *play = Play::new());
    GENERATION.set(GENERATION.get().wrapping_add(1));
    SCOPE.set(scope);
    crate::controller::with(|ctl| {
        ctl.ui.global::<crate::ui::Settings>().set_nexus_records_listening(scope.is_some());
    });
    if let Some(service) = SERVICE.get() {
        service.change(reader::ChangeRequest {
            update: reader::Update::ScopeChanged(scope), finished: None,
        });
    }
    if scope.is_some() && !OPENED.replace(true) {
        note(|| {
            let version = nexus_core::nexus::proto::Version {
                major: env!("CARGO_PKG_VERSION_MAJOR").parse().ok()?,
                minor: env!("CARGO_PKG_VERSION_MINOR").parse().ok()?,
                patch: env!("CARGO_PKG_VERSION_PATCH").parse().ok()?,
            };
            Some(Kind::Open { version, device: DeviceKind::Pc })
        });
    }
}

impl Scope {
    fn new(account: JournalAccount, life: u64) -> Self {
        Self {
            account: Account {
                backend: *account.backend_id.as_bytes(),
                number: account.account.get(),
            },
            life,
        }
    }

    fn owner(self) -> Option<JournalAccount> {
        Some(JournalAccount {
            backend_id: BackendId::from_bytes(self.account.backend),
            account: AccountId::new(self.account.number)?,
        })
    }
}

/// The C10 machine's journal effects, on its adapter's window-thread turn.
/// W6 answers the Diagnostics read. The transport remains with the adapter.
/// A disk mutation returns a receipt. Resume the remaining C10 effects only
/// after that receipt arrives, without waiting on the window thread.
#[allow(dead_code)] // The live caller belongs to W7 and the sending slice.
pub(crate) fn handle_client(
    command: JournalDo,
    happened: impl FnOnce(Happened) + Send + 'static,
) -> Result<Option<Receiver<()>>, JournalDo> {
    use reader::{ChangeRequest, Update};
    use writer::Settlement;
    let update = match command {
        JournalDo::ReadEvents { read, account, life, from_sequence, max_events, max_bytes } => {
            let scope = Scope::new(account, life);
            let answered = move |events| happened(Happened::EventsRead { read, account, life, events });
            if let Some(service) = SERVICE.get().filter(|_| SCOPE.get() == Some(scope)) {
                let request = reader::Read {
                    scope, from: from_sequence, max_events, max_bytes, answered: Box::new(answered),
                };
                if let Err(TrySendError::Full(request) | TrySendError::Disconnected(request)) =
                    service.read.try_send(request)
                {
                    (request.answered)(Vec::new());
                }
            } else {
                answered(Vec::new());
            }
            return Ok(None);
        }
        JournalDo::ReadDiagnostics { .. } => return Err(command),
        JournalDo::Taken { account, life, ids, at_ms } =>
            Update::Settle(Scope::new(account, life), ids, Settlement::Taken(at_ms)),
        JournalDo::Refused { account, life, ids } =>
            Update::Settle(Scope::new(account, life), ids, Settlement::Refused(now_ms())),
        JournalDo::LifeMoved { account, life } => Update::LifeMoved(Scope::new(account, life)),
        JournalDo::ResendAll { backend_id } => Update::ResendAll(*backend_id.as_bytes()),
    };
    let (finished, receipt) = mpsc::channel();
    if let Some(service) = SERVICE.get() {
        service.change(ChangeRequest { update, finished: Some(finished) });
    }
    Ok(Some(receipt))
}

/// The client adapter drains this outside playback. The writer coalesces notices and
/// publishes only immutable counts. A previous sign-in's notice cannot start a round.
#[allow(dead_code)] // The live caller belongs to W7 and the sending slice.
pub(crate) fn take_waiting() -> Option<Happened> {
    let scope = SCOPE.get()?;
    let waiting = SERVICE.get()?.waiting.try_lock().ok()?.take()?;
    (waiting.scope == scope).then_some(Happened::EventsWaiting {
        account: scope.owner()?, life: scope.life, pending: waiting.pending, from_sequence: waiting.from,
    })
}

pub(crate) fn finish() {
    publish(None);
    if let Some(service) = SERVICE.get() {
        COUNTERS.closing.store(true, Ordering::Release);
        let _ = service.send.try_send(Message::Wake);
        if let Ok(finished) = service.finished.lock() {
            let _ = finished.recv_timeout(Duration::from_secs(5));
        }
    }
}

pub(crate) fn losses() -> LossCounts {
    COUNTERS.losses()
}

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().try_into().unwrap_or_default()
}

fn new_id() -> Option<EventId> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).ok()?;
    Some(EventId::from_bytes(bytes))
}

fn enqueue(scope: Scope, observation: Observation) -> bool {
    let Some(service) = SERVICE.get() else {
        COUNTERS.drop_event();
        return false;
    };
    try_observe(&service.send, &COUNTERS, scope, observation)
}

fn try_observe(
    line: &SyncSender<Message>, counters: &Counters, scope: Scope, observation: Observation,
) -> bool {
    match line.try_send(Message::Event(scope, observation)) {
        Ok(()) => true,
        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
            counters.drop_event();
            false
        }
    }
}

fn note(kind: impl FnOnce() -> Option<Kind>) -> Option<EventId> {
    let scope = SCOPE.get()?;
    let kind = kind()?;
    let Some(id) = new_id() else {
        COUNTERS.drop_event();
        return None;
    };
    enqueue(scope, Observation { id, at: now_ms(), kind }).then_some(id)
}

fn ms(seconds: f64) -> u32 {
    (seconds * 1_000.0).round().clamp(0.0, f64::from(events::PLAY_MAX_MS)) as u32
}

struct Search {
    key: u64,
    words: String,
    id: EventId,
    at: u64,
    requests: Option<EventId>,
}

fn search_key(words: &str) -> u64 {
    let mut key = DefaultHasher::new();
    words.trim().hash(&mut key);
    key.finish()
}

pub(crate) fn begin_search(words: &str, requests: bool) {
    if SCOPE.get().is_none() {
        return;
    }
    let key = search_key(words);
    let words = events::clean_text(words);
    let Some(id) = new_id() else { return };
    let search = Search { key, words, id, at: now_ms(), requests: requests.then(new_id).flatten() };
    SEARCH.with_borrow_mut(|pending| {
        if pending.len() == QUEUE_LENGTH {
            finish_search(pending.remove(0), 0, None);
        }
        pending.push(search);
    });
}

pub(crate) fn search_ticket(words: &str) -> Option<EventId> {
    SCOPE.get()?;
    let key = search_key(words);
    SEARCH.with_borrow(|pending| pending.iter().rev().find(|search| search.key == key).map(|search| search.id))
}

pub(crate) fn search_results(ticket: Option<EventId>, library: usize, requests: Option<usize>) {
    let Some(ticket) = ticket else { return };
    SEARCH.with_borrow_mut(|pending| {
        let Some(key) = pending.iter().find(|search| search.id == ticket).map(|search| search.key) else {
            return;
        };
        for search in pending.extract_if(.., |search| search.key == key) {
            finish_search(search, library, requests);
        }
    });
}

pub(crate) fn search_origin(words: &str) -> Option<PlayedFrom> {
    SCOPE.get()?;
    let words = events::clean_text(words);
    (!words.trim().is_empty()).then_some(PlayedFrom::Search { words })
}

pub(crate) fn radio_origin(id: &str) -> Option<PlayedFrom> {
    SCOPE.get()?;
    Some(PlayedFrom::Radio { id: journal_id(id)? })
}

fn finish_search(search: Search, library: usize, requests: Option<usize>) {
    let Some(scope) = SCOPE.get() else { return };
    let found = library.try_into().unwrap_or(u32::MAX);
    let outside = search.requests.map(|id| (id, search.words.clone()));
    enqueue(scope, Observation { id: search.id, at: search.at, kind: Kind::Search {
        words: search.words,
        found, where_: events::SearchedIn::Library,
    }});
    if let Some((id, words)) = outside {
        enqueue(scope, Observation { id, at: search.at, kind: Kind::Search {
            words, found: requests.unwrap_or(0).try_into().unwrap_or(u32::MAX),
            where_: events::SearchedIn::Requests,
        }});
    }
}

pub(crate) fn fault(fault: Fault) {
    note(|| Some(Kind::Fault { fault, count: 1 }));
}

struct Active {
    start: EventId,
    position_ms: u32,
    duration_ms: u32,
    heard_ms: u32,
    heard_at_start: f64,
    paused: bool,
}

struct Play {
    active: Option<Active>,
    began: Began,
    sources: [(u64, Sound); 2],
}

impl Play {
    const fn new() -> Self {
        Self { active: None, began: Began::PressedHere, sources: [(0, Sound::Streamed); 2] }
    }
}

pub(crate) fn generation() -> u64 {
    GENERATION.get()
}

pub(crate) fn recording() -> bool {
    SCOPE.get().is_some()
}

pub(crate) fn beginning(began: Began) {
    if SCOPE.get().is_some() {
        PLAY.with_borrow_mut(|play| play.began = began);
    }
}

pub(crate) fn source(token: u64, sound: Sound, current: bool) {
    if SCOPE.get().is_some() {
        PLAY.with_borrow_mut(|play| play.sources[usize::from(!current)] = (token, sound));
    }
}

pub(crate) fn started(
    token: u64, song: &crate::subsonic::Song, route: &str, origin: Option<&PlayedFrom>, position: f64,
    heard_at_start: f64,
) {
    if SCOPE.get().is_none() {
        return;
    }
    // A reload for a seek, retry or changed setting is still this same listen.
    if PLAY.with_borrow(|play| play.active.is_some()) {
        return;
    }
    let (began, sound) = PLAY.with_borrow(|play| {
        let sound = play.sources.iter().find(|(saved, _)| *saved == token)
            .map_or(Sound::Streamed, |(_, sound)| *sound);
        (play.began, sound)
    });
    let position_ms = ms(position);
    let Some(song_id) = journal_id(&song.id) else { return };
    let from = origin.cloned().unwrap_or_else(|| played_from(route, song));
    if let Some(start) = note(|| Some(Kind::Start {
        song_id,
        title: events::clean_text(&song.title),
        artist: events::clean_text(song.artist_label()),
        album: events::clean_text(&song.album),
        from,
        began,
        sound,
        position_ms,
    })) {
        PLAY.with_borrow_mut(|play| play.active = Some(Active {
            start, position_ms, duration_ms: ms(f64::from(song.duration)), heard_ms: 0, heard_at_start, paused: false,
        }));
    }
}

fn journal_id(id: &str) -> Option<String> {
    // Do not clone an arbitrarily large music ID into the hand-off. Core validates
    // exact IDs and prepares the display strings once it builds the record.
    (id.chars().take(events::TEXT_CHARS + 1).count() <= events::TEXT_CHARS).then(|| id.to_owned())
}

fn played_from(route: &str, song: &crate::subsonic::Song) -> events::PlayedFrom {
    use events::PlayedFrom;
    if !song.radio_url.is_empty() {
        return journal_id(&song.id).map_or(PlayedFrom::Queue, |id| PlayedFrom::Radio { id });
    }
    if route == "liked" {
        return PlayedFrom::LikedSongs;
    }
    let Some((kind, id)) = route.split_once(':').filter(|(_, id)| !id.is_empty()) else {
        return PlayedFrom::Queue;
    };
    if kind == "search" {
        return search_origin(id).unwrap_or(PlayedFrom::Queue);
    }
    let Some(id) = journal_id(id) else { return PlayedFrom::Queue };
    match kind {
        "album" => PlayedFrom::Album { id },
        "playlist" => PlayedFrom::Playlist { id },
        "artist" => PlayedFrom::Artist { id },
        "mix" => PlayedFrom::Mix { id },
        "radio" => PlayedFrom::Radio { id },
        _ => PlayedFrom::Queue,
    }
}

pub(crate) fn sample(position: f64, heard: f64) {
    if SCOPE.get().is_some() {
        PLAY.with_borrow_mut(|play| {
            if let Some(active) = &mut play.active {
                active.position_ms = ms(position);
                active.heard_ms = ms(heard - active.heard_at_start);
            }
        });
    }
}

pub(crate) fn checkpoint() {
    let (Some(scope), Some(service)) = (SCOPE.get(), SERVICE.get()) else { return };
    if let Some((id, position)) = PLAY.with_borrow(|play| play.active.as_ref()
        .map(|active| (active.start, active.position_ms))) {
        let _ = service.send.try_send(Message::Checkpoint(scope, id, position));
    }
}

pub(crate) fn pause(paused: bool) {
    if SCOPE.get().is_none() {
        return;
    }
    let kind = PLAY.with_borrow_mut(|play| {
        let active = play.active.as_mut()?;
        if active.paused == paused {
            return None;
        }
        active.paused = paused;
        let (start, position_ms) = (active.start, active.position_ms);
        Some(if paused { Kind::Pause { start, position_ms } } else { Kind::Resume { start, position_ms } })
    });
    if note(|| kind).is_some() && paused {
        checkpoint();
    }
}

pub(crate) fn seek(from: f64, to: f64) {
    if SCOPE.get().is_none() {
        return;
    }
    let kind = PLAY.with_borrow_mut(|play| {
        let active = play.active.as_mut()?;
        active.position_ms = ms(to);
        Some(Kind::Seek { start: active.start, from_ms: ms(from), to_ms: active.position_ms })
    });
    if note(|| kind).is_some() {
        checkpoint();
    }
}

pub(crate) fn stop(why: Stopped) {
    if SCOPE.get().is_none() {
        return;
    }
    let active = PLAY.with_borrow_mut(|play| play.active.take());
    note(|| active.map(|active| Kind::Stop {
        start: active.start,
        position_ms: Some(active.position_ms),
        duration_ms: Some(active.duration_ms),
        heard_ms: Some(active.heard_ms),
        why,
    }));
}

#[cfg(debug_assertions)]
pub(crate) fn test_account() {
    // Only the local walk's isolated debug profile can stand in for the W7 publication.
    if std::env::var_os("NEXUS_MUSIC_HOME").is_some()
        && std::env::var_os("NEXUS_TEST_LISTENING_JOURNAL").is_some()
    {
        publish(Some(Scope { account: Account { backend: [7; 16], number: 1 }, life: 1 }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlapping_searches_keep_their_own_result_counts_and_owner() {
        let root = std::env::temp_dir().join(crate::config::temp_for(std::path::Path::new("nexus-search-w8")));
        install(root.clone());
        let scope = Scope { account: Account { backend: [4; 16], number: 3 }, life: 7 };
        publish(Some(scope));
        begin_search("fictional first", false);
        let first = search_ticket("fictional first");
        begin_search("fictional second", true);
        let second = search_ticket("fictional second");
        search_results(second, 9, Some(2));
        search_results(first, 3, None);
        finish();

        let directory = writer::JournalDirectory::new(&root, scope.account);
        let mut found = Vec::new();
        for number in directory.numbers().unwrap() {
            let file = directory.read(number).unwrap().1.unwrap();
            assert_eq!(file.life, scope.life);
            for row in file.events {
                if let Kind::Search { words, found: count, where_ } = row.record().kind() {
                    found.push((words.clone(), *count, *where_));
                }
            }
        }
        let _ = std::fs::remove_dir_all(root);
        assert_eq!(found, [
            ("fictional second".into(), 9, events::SearchedIn::Library),
            ("fictional second".into(), 2, events::SearchedIn::Requests),
            ("fictional first".into(), 3, events::SearchedIn::Library),
        ]);
    }

    #[test]
    fn a_read_for_an_inactive_account_is_answered_empty() {
        let (send, receive) = mpsc::channel();
        let account = JournalAccount {
            backend_id: BackendId::from_bytes([5; 16]),
            account: AccountId::new(7).unwrap(),
        };
        assert!(handle_client(JournalDo::ReadEvents {
            read: 17, account, life: 2, from_sequence: 1, max_events: 5, max_bytes: 4096,
        }, move |event| send.send(event).unwrap()).is_ok());
        let Happened::EventsRead { read, account: owner, life, events } = receive.try_recv().unwrap() else {
            panic!("expected EventsRead");
        };
        assert_eq!((read, owner, life), (17, account, 2));
        assert!(events.is_empty());
    }
}
