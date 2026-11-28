//! Takes listening observations on the window thread. The journal worker owns the disk.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use nexus_core::nexus::{
    client::Saved, events::{self, DeviceKind, EventId, Kind, LossCounts}, proto::BackendId,
};
use serde::{Deserialize, Serialize};

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
    counters: Arc<Counters>,
    finished: Mutex<Receiver<()>>,
    read: SyncSender<reader::Read>,
    update: mpsc::Sender<reader::Update>,
    waiting: Arc<Mutex<Option<reader::Waiting>>>,
}

static SERVICE: OnceLock<Service> = OnceLock::new();

thread_local! {
    static OPENED: Cell<bool> = const { Cell::new(false) };
    static GENERATION: Cell<u64> = const { Cell::new(0) };
    static SCOPE: Cell<Option<Scope>> = const { Cell::new(None) };
    static PLAY: RefCell<Play> = const { RefCell::new(Play::new()) };
    static SEARCH: RefCell<Option<Search>> = const { RefCell::new(None) };
}

pub(crate) fn install(root: PathBuf) {
    let (send, receive) = mpsc::sync_channel(QUEUE_LENGTH);
    let (done, finished) = mpsc::channel();
    let (read, reads) = mpsc::sync_channel::<reader::Read>(1);
    let (update, updates) = mpsc::channel();
    let waiting = Arc::new(Mutex::new(None));
    let writer_waiting = waiting.clone();
    let counters = Arc::new(Counters::default());
    let worker = counters.clone();
    let reader_root = root.clone();
    if std::thread::Builder::new().name("listening-reader".into()).spawn(move || {
        while let Ok(read) = reads.recv() {
            read.run(&reader_root);
        }
    }).is_err() {
        return;
    }
    if std::thread::Builder::new().name("listening-journal".into()).spawn(move || {
        writer::run(root, receive, updates, writer_waiting, worker);
        let _ = done.send(());
    }).is_ok() {
        let _ = SERVICE.set(Service { send, counters, finished: Mutex::new(finished), read, update, waiting });
    }
}

/// W7 publishes only its current authenticated saved state, on the window thread.
/// C10 removes the pending notice when the person has seen it.
#[allow(dead_code)] // The live publisher belongs to W7.
pub(crate) fn publish_account(saved: Option<&Saved>) {
    let scope = saved
        .filter(|saved| saved.session.is_some() && saved.notice.is_none())
        .and_then(|saved| Some(Scope {
            account: Account {
                backend: *BackendId::from_hex(&saved.backend_id)?.as_bytes(),
                number: saved.account.get(),
            },
            life: saved.life,
        }));
    publish(scope);
}

fn publish(scope: Option<Scope>) {
    if SCOPE.get() == scope {
        return;
    }
    stop(Stopped::Stopped);
    let prior = SEARCH.with_borrow_mut(Option::take);
    if let Some(prior) = prior {
        finish_search(prior, 0, None);
    }
    PLAY.with_borrow_mut(|play| *play = Play::new());
    GENERATION.set(GENERATION.get().wrapping_add(1));
    SCOPE.set(scope);
    if let Some(service) = SERVICE.get() {
        let _ = service.update.send(reader::Update::ScopeChanged(scope));
    }
    if scope.is_some() && !OPENED.replace(true) {
        note(|| {
            let version = nexus_core::nexus::proto::Version::parse(env!("CARGO_PKG_VERSION"))?;
            Some(Kind::Open { version, device: DeviceKind::Pc })
        });
    }
}

pub(crate) fn finish() {
    publish(None);
    if let Some(service) = SERVICE.get() {
        service.counters.closing.store(true, Ordering::Release);
        if let Ok(finished) = service.finished.lock() {
            let _ = finished.recv_timeout(Duration::from_secs(5));
        }
    }
}

pub(crate) fn losses() -> LossCounts {
    SERVICE.get().map_or_else(LossCounts::default, |service| {
        let counters = &service.counters;
        LossCounts {
            line_full: counters.line_full.load(Ordering::Relaxed)
                .saturating_add(counters.dropped.load(Ordering::Relaxed)),
            files_full: counters.files_full.load(Ordering::Relaxed),
            refused: counters.refused.load(Ordering::Relaxed),
        }
    })
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
    let Some(service) = SERVICE.get() else { return false };
    try_observe(&service.send, &service.counters, scope, observation)
}

fn try_observe(
    line: &SyncSender<Message>, counters: &Counters, scope: Scope, observation: Observation,
) -> bool {
    match line.try_send(Message::Event(scope, observation)) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            let _ = counters.dropped.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| Some(n.saturating_add(1)));
            false
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

fn note(kind: impl FnOnce() -> Option<Kind>) -> Option<EventId> {
    let scope = SCOPE.get()?;
    let kind = kind()?;
    let id = new_id()?;
    enqueue(scope, Observation { id, at: now_ms(), kind }).then_some(id)
}

fn ms(seconds: f64) -> u32 {
    (seconds * 1_000.0).round().clamp(0.0, f64::from(events::PLAY_MAX_MS)) as u32
}

struct Search {
    words: String,
    observation: Observation,
    requests: Option<EventId>,
}

pub(crate) fn begin_search(words: &str, requests: bool) {
    if SCOPE.get().is_none() {
        return;
    }
    let prior = SEARCH.with_borrow_mut(Option::take);
    if let Some(prior) = prior {
        finish_search(prior, 0, None);
    }
    let words = events::clean_text(words);
    let Some(id) = new_id() else { return };
    let observation = Observation { id, at: now_ms(), kind: Kind::Search {
        words: words.clone(), found: 0, where_: events::SearchedIn::Library,
    }};
    SEARCH.with_borrow_mut(|pending| {
        *pending = Some(Search { words, observation, requests: requests.then(new_id).flatten() });
    });
}

pub(crate) fn search_ticket(words: &str) -> Option<EventId> {
    SCOPE.get()?;
    SEARCH.with_borrow(|pending| {
        pending.as_ref().filter(|search| search.words == events::clean_text(words))
            .map(|search| search.observation.id)
    })
}

pub(crate) fn search_results(ticket: Option<EventId>, library: usize, requests: Option<usize>) {
    let Some(ticket) = ticket else { return };
    let search = SEARCH.with_borrow_mut(|pending| {
        if pending.as_ref().is_some_and(|search| search.observation.id == ticket) {
            pending.take()
        } else {
            None
        }
    });
    if let Some(search) = search {
        finish_search(search, library, requests);
    }
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

fn finish_search(mut search: Search, library: usize, requests: Option<usize>) {
    let Some(scope) = SCOPE.get() else { return };
    if let Kind::Search { found, .. } = &mut search.observation.kind {
        *found = library.try_into().unwrap_or(u32::MAX);
    }
    let at = search.observation.at;
    enqueue(scope, search.observation);
    if let Some(id) = search.requests {
        enqueue(scope, Observation { id, at, kind: Kind::Search {
            words: search.words, found: requests.unwrap_or(0).try_into().unwrap_or(u32::MAX),
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
            start, position_ms, duration_ms: ms(f64::from(song.duration)), heard_ms: 0, paused: false,
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
                active.heard_ms = ms(heard);
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
