//! Asking for many albums at once.
//!
//! Bringing playlists over from Spotify can leave hundreds of songs the library doesn't
//! have, on as many albums by as many artists. Asking Lidarr for those one at a time, each
//! waiting for Lidarr to finish with the one before, would take hours and lose its place if
//! the app closed. So a batch is worked through in three steps, and written down as it goes:
//!
//! 1. Each album is looked up in Lidarr, several at a time. Playlists name albums loosely
//!    ("Supernatural (Remastered)"), and often name the single a song came out on, so the
//!    match is by what's left once the trimmings are gone, with the song itself as a way in
//!    when the name finds nothing.
//! 2. Each artist is dealt with in one go. One Lidarr doesn't have is added with exactly the
//!    releases wanted; Lidarr then reads the artist in, marks those and looks for them by
//!    itself, in its own time, whether or not this app is still open.
//! 3. As Lidarr finishes with each artist, what it ended up with is checked against what
//!    was asked, and put right if it differs.
//!
//! Nothing here limits how many albums a batch can hold. Lidarr reads in a few artists at a
//! time, so a very large batch is accepted in minutes and worked through by Lidarr over an
//! hour or more.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::asked::{Asked, Fill, same_artist};
use crate::lidarr::{Lidarr, Offer, Setup};
use crate::musicbrainz::words;

/// How many albums are looked up at once.
const LOOKUPS: usize = 3;
/// How many things in a row can go wrong before the batch stops to be carried on later:
/// that many together means Lidarr can't be reached, not that one album is awkward.
const GIVE_UP: usize = 5;
/// How often Lidarr is asked how it's getting on with the artists it was given.
const POLL: Duration = Duration::from_secs(5);
/// How long Lidarr must have had nothing under way for an artist before what it ended up
/// with is looked at: it does its reading-in as several jobs with a moment between them.
const QUIET: Duration = Duration::from_secs(10);
/// How long Lidarr's list of jobs can go without changing at all before the batch stops
/// waiting for it. (It reads artists in a few at a time and finishes most of them together
/// at the end, so a long wait for the first to finish means nothing.)
const STALLED: Duration = Duration::from_secs(30 * 60);
/// How long an artist Lidarr has finished with can go on listing nothing before it's given
/// up on.
const NOTHING_LISTED: Duration = Duration::from_secs(10 * 60);

pub const NOT_KNOWN: &str = "Lidarr doesn't know it by that name";
const NO_ANSWER: &str = "MusicBrainz was too busy to say what it came out on";
/// How long to wait before asking MusicBrainz again when it's too busy to answer, each time
/// it is. Its search is shared by everyone and turns callers away for a minute at a stretch.
const ASK_AGAIN: [u64; 3] = [5, 15, 30];

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub enum State {
    /// Not looked up yet.
    #[default]
    Waiting,
    /// Lidarr knows it; it hasn't been asked for yet.
    Found,
    /// Lidarr has been asked, and is still reading the artist in.
    Asked,
    /// On Lidarr's list.
    Done,
    /// Lidarr doesn't know it by the names given.
    Unknown,
    /// Lidarr turned it down.
    Failed,
}

/// One album to ask for, as a playlist named it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Want {
    pub artist: String,
    /// May be empty, if the playlist didn't say; then the songs are all there is to go on.
    pub album: String,
    /// The songs wanted from it, by title.
    pub songs: Vec<String>,
    /// Where those songs go once they arrive.
    pub fills: Vec<Fill>,
    pub state: State,
    /// What Lidarr calls it, once found.
    pub offer: Option<Offer>,
    /// Why it couldn't be asked for.
    pub reason: String,
    /// Lidarr's number for its artist, once asked.
    pub lidarr_artist: i64,
    /// It's been noted as asked for (see `asked`).
    pub noted: bool,
    /// It couldn't be asked for, for a reason that may have passed: worth another go.
    pub again: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Batch {
    wants: Vec<Want>,
    /// Artists whose "new releases" setting was turned off while their singles were read
    /// in: Lidarr's number for each, and what to put back.
    restore: Vec<(i64, String)>,
    /// Why the batch stopped before its end, if it did.
    stopped: String,
    /// Whether a Lidarr (named by its address), told which of a new artist's releases are
    /// wanted, marks those and no others. Every Lidarr this was tried against does. It's
    /// still watched for once per Lidarr before artists are added that way in number: one
    /// that marked everything instead would set about fetching whole discographies.
    proof: Option<(String, bool)>,
}

static BATCH: Mutex<Batch> = Mutex::new(Batch { wants: Vec::new(), restore: Vec::new(), stopped: String::new(), proof: None });
static PLACE: OnceLock<PathBuf> = OnceLock::new();
static RUNNING: AtomicBool = AtomicBool::new(false);
static STOPPING: AtomicBool = AtomicBool::new(false);
/// The first artist added in one go is being watched (see `Batch::proof`).
static WATCHING: AtomicBool = AtomicBool::new(false);

/// What the batch has to say to the rest of the app.
pub enum Event {
    /// The counts have moved.
    Changed,
    /// These are on Lidarr's list (or as good as): note them, to be followed up.
    Noted(Vec<Asked>),
    /// These turned out not to be: forget them, by title.
    Forgot(Vec<String>),
    /// Everything that could be asked for has been; Lidarr may still be reading artists in.
    Asked,
    /// The batch has ended, or stopped.
    Ended,
}

type Tell = dyn Fn(Event) + Send + Sync;

fn with<T>(act: impl FnOnce(&mut Batch) -> T) -> T {
    act(&mut BATCH.lock().unwrap_or_else(|stuck| stuck.into_inner()))
}

/// Reads the batch that was under way when the app last closed, if there was one.
pub fn load(path: &std::path::Path) {
    let _ = PLACE.set(path.to_path_buf());
    if let Some(batch) = std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice::<Batch>(&bytes).ok()) {
        with(|now| *now = batch);
    }
}

fn save() {
    let Some(path) = PLACE.get() else { return };
    let json = with(|batch| if batch.wants.is_empty() && batch.restore.is_empty() && batch.proof.is_none() { None } else { serde_json::to_vec(batch).ok() });
    match json {
        None => {
            let _ = std::fs::remove_file(path);
        }
        Some(json) => {
            let partial = path.with_extension("json.part");
            if std::fs::write(&partial, json).is_ok() {
                let _ = std::fs::rename(&partial, path);
            }
        }
    }
}

/// What a title comes down to for comparing: its words, without what's often tacked on.
fn key(title: &str) -> String {
    words(&crate::import::undecorated(title)).join(" ")
}

fn same_want(a: &Want, b: &Want) -> bool {
    words(&a.artist) == words(&b.artist) && if a.album.is_empty() || b.album.is_empty() { a.album == b.album && a.songs == b.songs } else { key(&a.album) == key(&b.album) }
}

/// Hands over albums to ask for. If a batch is still under way they join it; otherwise they
/// replace what's left of the last one.
pub fn start(wants: Vec<Want>) {
    with(|batch| {
        if !unfinished_in(batch) {
            batch.wants.clear();
            batch.stopped.clear();
        }
        for mut want in wants {
            match batch.wants.iter_mut().find(|known| same_want(known, &want)) {
                // Asked for again: its songs' playlists are added to what's known, and one
                // that came to nothing is tried afresh.
                Some(known) => {
                    for fill in want.fills {
                        if !known.fills.contains(&fill) {
                            known.fills.push(fill);
                        }
                    }
                    if matches!(known.state, State::Unknown | State::Failed) {
                        known.state = State::Waiting;
                        known.again = false;
                        known.reason.clear();
                    }
                }
                None => {
                    want.state = State::Waiting;
                    batch.wants.push(want);
                }
            }
        }
    });
    save();
}

fn unfinished_in(batch: &Batch) -> bool {
    !batch.restore.is_empty() || batch.wants.iter().any(|want| matches!(want.state, State::Waiting | State::Found | State::Asked))
}

/// Whether there's a batch with something still to do.
pub fn unfinished() -> bool {
    with(|batch| unfinished_in(batch))
}

pub fn running() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

/// Stops asking for more. What Lidarr has already been asked for stays asked for, and is
/// still checked.
pub fn stop() {
    STOPPING.store(true, Ordering::SeqCst);
}

fn stopping() -> bool {
    STOPPING.load(Ordering::SeqCst)
}

/// Has another go at what couldn't be asked for last time for a reason that may have passed.
/// Says whether there was anything to have another go at.
pub fn again() -> bool {
    if running() {
        return false;
    }
    let any = with(|batch| {
        let mut any = false;
        for want in batch.wants.iter_mut().filter(|want| want.again && matches!(want.state, State::Unknown | State::Failed)) {
            want.state = State::Waiting;
            want.again = false;
            want.reason.clear();
            any = true;
        }
        any
    });
    save();
    any
}

/// Forgets a batch that has ended: its results have been read.
pub fn clear() {
    if running() {
        return;
    }
    with(|batch| {
        batch.wants.clear();
        batch.stopped.clear();
    });
    save();
}

/// How a batch stands.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Summary {
    pub total: usize,
    /// Not looked up yet; found and not yet asked for.
    pub waiting: usize,
    pub found: usize,
    /// On Lidarr's list, counting those Lidarr is still reading the artist in for.
    pub listed: usize,
    /// How many artists Lidarr is still reading in.
    pub reading: usize,
    pub unknown: usize,
    pub failed: usize,
    /// How many of those are worth another go.
    pub again: usize,
    pub running: bool,
    /// Lidarr is being watched with the first artist, before the rest are handed over.
    pub watching: bool,
    pub stopped: String,
}

pub fn summary() -> Summary {
    with(|batch| {
        let count = |state: State| batch.wants.iter().filter(|want| want.state == state).count();
        let reading: HashSet<i64> = batch.wants.iter().filter(|want| want.state == State::Asked).map(|want| want.lidarr_artist).collect();
        Summary {
            total: batch.wants.len(),
            waiting: count(State::Waiting),
            found: count(State::Found),
            listed: count(State::Asked) + count(State::Done),
            reading: reading.len(),
            unknown: count(State::Unknown),
            failed: count(State::Failed),
            again: batch.wants.iter().filter(|want| want.again && matches!(want.state, State::Unknown | State::Failed)).count(),
            running: running(),
            watching: WATCHING.load(Ordering::SeqCst),
            stopped: batch.stopped.clone(),
        }
    })
}

fn albums(n: usize) -> String {
    if n == 1 { "1 album".into() } else { format!("{n} albums") }
}

impl Summary {
    /// What to say about the batch at the top of the Requests page, and the names of what
    /// can be done about it: the first is the likelier. Empty where there's nothing.
    pub fn notice(&self) -> (String, &'static str, &'static str) {
        let (text, action) = self.said();
        match action {
            // Some of what couldn't be asked for might be now.
            "Clear" if self.again > 0 => (text, "Try again", "Clear"),
            _ => (text, action, ""),
        }
    }

    fn said(&self) -> (String, &'static str) {
        let missed = self.unknown + self.failed;
        let left = self.waiting + self.found;
        if self.total == 0 {
            (String::new(), "")
        } else if self.running && self.waiting > 0 {
            let looked = self.total - self.waiting;
            let not_found = if self.unknown == 0 { String::new() } else { format!(", {} not found", self.unknown) };
            (format!("Looking up {} in Lidarr: {looked} done{not_found}.", albums(self.total)), "Stop")
        } else if self.running && self.watching {
            (format!("Putting {} on Lidarr's list. Lidarr is reading in the first artist; the rest follow once that's been checked.", albums(self.found + self.listed)), "Stop")
        } else if self.running && self.found > 0 {
            (format!("Putting {} on Lidarr's list: {} done.", albums(self.found + self.listed), self.listed), "Stop")
        } else if self.running {
            let on_list = format!("{} {} on Lidarr's list.", albums(self.listed), if self.listed == 1 { "is" } else { "are" });
            match self.reading {
                0 => (on_list, ""),
                1 => (format!("{on_list} Lidarr is still reading in 1 artist."), ""),
                n => (format!("{on_list} Lidarr is still reading in {n} artists."), ""),
            }
        } else if !self.stopped.is_empty() && (left > 0 || self.reading > 0) {
            (format!("Stopped with {} of {} still to ask for. {}", left, albums(self.total), self.stopped), "Carry on")
        } else if missed > 0 {
            let rest = if missed == 1 { "The other one is".to_string() } else { format!("The other {missed} are") };
            (format!("{} of {} {} on Lidarr's list. {rest} listed below.", self.listed, albums(self.total), if self.listed == 1 { "is" } else { "are" }), "Clear")
        } else {
            (String::new(), "")
        }
    }
}

/// The albums of the batch that couldn't be asked for, with why.
pub fn missed() -> Vec<Want> {
    with(|batch| batch.wants.iter().filter(|want| matches!(want.state, State::Unknown | State::Failed)).cloned().collect())
}

// ---- finding ---------------------------------------------------------------------------------------

/// Of what Lidarr found, the release a playlist meant by this artist and album name: the
/// same title once the trimmings are off both, by that artist. Of several, one named
/// exactly as asked goes first, then an album before a single before an EP. Singles and EPs
/// only count if `singles` says so.
pub fn choose(found: &[Offer], artist: &str, album: &str, singles: bool) -> Option<Offer> {
    let wanted = key(album);
    let exact = words(album);
    let rank = |offer: &Offer| match offer.kind() {
        "Album" => 0,
        "Single" => 1,
        _ => 2,
    };
    found
        .iter()
        .filter(|offer| singles || offer.kind() == "Album")
        .filter(|offer| !wanted.is_empty() && key(&offer.title) == wanted && same_artist(&offer.artist, artist))
        .min_by_key(|offer| (words(&offer.title) != exact, rank(offer)))
        .cloned()
}

/// One question to Lidarr, asked again after a moment if it couldn't be answered.
fn twice<T>(ask: impl Fn() -> Result<T, String>) -> Result<T, String> {
    ask().or_else(|_| {
        std::thread::sleep(Duration::from_secs(2));
        ask()
    })
}

/// A search of Lidarr's, which it passes on to a service of its own that now and then is
/// too busy to answer. Lidarr then answers with an error, and is asked again a few times,
/// further apart. A Lidarr that can't be reached at all isn't waited on like that.
fn search(lidarr: &Lidarr, term: &str) -> Result<Vec<Offer>, String> {
    let mut waits = [2, 6, 15].iter();
    loop {
        match lidarr.find_any(term) {
            Ok(found) => return Ok(found),
            Err(reason) => {
                let answered = reason.starts_with("Lidarr answered");
                let Some(&wait) = waits.next().filter(|&&wait| !stopping() && (answered || wait == 2)) else { return Err(reason) };
                std::thread::sleep(Duration::from_secs(wait));
            }
        }
    }
}

/// How looking an album up went.
enum Lookup {
    Is(Offer),
    /// Lidarr doesn't know it.
    Not,
    /// It wasn't found by name, and what its songs came out on couldn't be asked.
    Unsure,
}

/// What a song came out on, according to MusicBrainz, which is asked again after a while if
/// it's too busy to say. An error if it stayed that way.
fn release_of(want: &Want, song: &str, singles: bool) -> Result<Option<String>, String> {
    let mut waits = ASK_AGAIN.iter();
    loop {
        match crate::musicbrainz::release_with(&want.artist, song, &want.album, singles) {
            Ok(release) => return Ok(release),
            Err(reason) => {
                let Some(&wait) = waits.next().filter(|_| !stopping()) else { return Err(reason) };
                std::thread::sleep(Duration::from_secs(wait));
            }
        }
    }
}

/// Finds what Lidarr calls an album. An error only if Lidarr couldn't be asked.
fn find(lidarr: &Lidarr, want: &Want, singles: bool) -> Result<Lookup, String> {
    if !want.album.is_empty() {
        let found = search(lidarr, &format!("{} {}", want.album, want.artist))?;
        if let Some(offer) = choose(&found, &want.artist, &want.album, singles) {
            return Ok(Lookup::Is(offer));
        }
        // Lidarr's search can be thrown by the trimmings; ask without them.
        let bare = crate::import::undecorated(&want.album);
        if bare != want.album.trim() {
            let found = search(lidarr, &format!("{bare} {}", want.artist))?;
            if let Some(offer) = choose(&found, &want.artist, &want.album, singles) {
                return Ok(Lookup::Is(offer));
            }
        }
    }
    // By one of its songs: MusicBrainz says what the song came out on, by the name Lidarr
    // knows it by.
    let mut unsure = false;
    for song in want.songs.iter().take(2) {
        let release = match release_of(want, song, singles) {
            Ok(Some(release)) => release,
            Ok(None) => continue,
            Err(_) => {
                unsure = true;
                continue;
            }
        };
        let found = search(lidarr, &format!("lidarr:{release}"))?;
        if let Some(offer) = found.into_iter().find(|offer| offer.album == release && (singles || offer.kind() == "Album")) {
            return Ok(Lookup::Is(offer));
        }
    }
    Ok(if unsure { Lookup::Unsure } else { Lookup::Not })
}

// ---- the work ----------------------------------------------------------------------------------------

/// Works through the batch on a thread of its own, if it isn't being worked through already.
pub fn run(lidarr: Lidarr, tell: impl Fn(Event) + Send + Sync + 'static) {
    if RUNNING.swap(true, Ordering::SeqCst) {
        return;
    }
    STOPPING.store(false, Ordering::SeqCst);
    with(|batch| batch.stopped.clear());
    let started = std::thread::Builder::new().name("bulk-requests".into()).spawn(move || {
        tell(Event::Changed);
        let outcome = work(&lidarr, &tell);
        with(|batch| {
            if let Err(reason) = outcome {
                batch.stopped = reason;
            }
        });
        save();
        RUNNING.store(false, Ordering::SeqCst);
        tell(Event::Ended);
    });
    if started.is_err() {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

fn work(lidarr: &Lidarr, tell: &Tell) -> Result<(), String> {
    let found = find_all(lidarr, tell);
    let asked = found.and_then(|()| ask_all(lidarr, tell));
    if stopping() {
        // What hasn't been asked for won't be.
        with(|batch| batch.wants.retain(|want| !matches!(want.state, State::Waiting | State::Found)));
    }
    save();
    if asked.is_ok() {
        tell(Event::Asked);
    }
    // Whatever became of the asking, Lidarr is seen through the artists it was given.
    let settled = settle(lidarr, tell);
    asked.and(settled)
}

static TOLD: Mutex<Option<(Instant, Instant)>> = Mutex::new(None);

/// Says the counts have moved, and writes the batch down: neither more often than is useful.
fn progress(tell: &Tell) {
    let (say, write) = {
        let mut told = TOLD.lock().unwrap_or_else(|stuck| stuck.into_inner());
        let now = Instant::now();
        let (said, wrote) = told.get_or_insert((now - Duration::from_secs(60), now));
        let say = said.elapsed() >= Duration::from_millis(400);
        let write = wrote.elapsed() >= Duration::from_secs(5);
        if say {
            *said = now;
        }
        if write {
            *wrote = now;
        }
        (say, write)
    };
    if write {
        save();
    }
    if say {
        tell(Event::Changed);
    }
}

fn find_all(lidarr: &Lidarr, tell: &Tell) -> Result<(), String> {
    let todo: Vec<usize> = with(|batch| batch.wants.iter().enumerate().filter(|(_, want)| want.state == State::Waiting).map(|(at, _)| at).collect());
    if todo.is_empty() {
        return Ok(());
    }
    let singles = crate::lidarr::singles();
    let (next, errors) = (AtomicUsize::new(0), AtomicUsize::new(0));
    let last: Mutex<String> = Mutex::new(String::new());
    std::thread::scope(|scope| {
        for _ in 0..LOOKUPS.min(todo.len()) {
            scope.spawn(|| {
                while !stopping() && errors.load(Ordering::SeqCst) < GIVE_UP {
                    let Some(&at) = todo.get(next.fetch_add(1, Ordering::SeqCst)) else { break };
                    let want = with(|batch| batch.wants[at].clone());
                    match find(lidarr, &want, singles) {
                        Ok(found) => {
                            errors.store(0, Ordering::SeqCst);
                            with(|batch| {
                                let want = &mut batch.wants[at];
                                want.reason.clear();
                                want.again = false;
                                match found {
                                    Lookup::Is(offer) => {
                                        want.state = State::Found;
                                        want.offer = Some(offer);
                                    }
                                    Lookup::Not => {
                                        want.state = State::Unknown;
                                        want.reason = NOT_KNOWN.into();
                                    }
                                    Lookup::Unsure => {
                                        want.state = State::Unknown;
                                        want.again = true;
                                        want.reason = if want.album.is_empty() { NO_ANSWER.into() } else { format!("{NOT_KNOWN}, and {NO_ANSWER}") };
                                    }
                                }
                            });
                        }
                        Err(reason) => {
                            errors.fetch_add(1, Ordering::SeqCst);
                            with(|batch| batch.wants[at].reason = reason.trim_end_matches('.').to_string());
                            *last.lock().unwrap_or_else(|stuck| stuck.into_inner()) = reason;
                        }
                    }
                    progress(tell);
                }
            });
        }
    });
    if errors.load(Ordering::SeqCst) >= GIVE_UP {
        // Lidarr can't be reached: these are still to look up when it can.
        with(|batch| batch.wants.iter_mut().filter(|want| want.state == State::Waiting).for_each(|want| want.reason.clear()));
        return Err(last.into_inner().unwrap_or_else(|stuck| stuck.into_inner()));
    }
    if !stopping() {
        // One that couldn't be looked up while those around it could.
        with(|batch| {
            for want in batch.wants.iter_mut().filter(|want| want.state == State::Waiting && !want.reason.is_empty()) {
                want.state = State::Failed;
                want.again = true;
            }
        });
    }
    Ok(())
}

/// How asking for one album went.
enum Went {
    /// It's on Lidarr's list.
    Listed,
    /// Lidarr has been asked and is reading the artist in; to be checked when it's done.
    /// With whether it can be counted as asked for already.
    Sent(bool),
    Failed(String),
}

fn not_listed(offer: &Offer) -> String {
    format!("Lidarr has {} but doesn't list “{}” among their releases", offer.artist, offer.title)
}

/// A note of something asked for, to be followed up when it arrives.
fn note(want: &Want, offer: &Offer) -> Asked {
    Asked { album: offer.album.clone(), title: offer.title.clone(), artist: offer.artist.clone(), at: chrono::Utc::now().timestamp(), fills: want.fills.clone(), ..Default::default() }
}

fn ask_all(lidarr: &Lidarr, tell: &Tell) -> Result<(), String> {
    // By artist, in the order they come.
    let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
    with(|batch| {
        for (at, want) in batch.wants.iter().enumerate().filter(|(_, want)| want.state == State::Found) {
            let Some(offer) = &want.offer else { continue };
            match groups.iter_mut().find(|(artist, _)| *artist == offer.artist_id) {
                Some((_, places)) => places.push(at),
                None => groups.push((offer.artist_id.clone(), vec![at])),
            }
        }
    });
    if groups.is_empty() {
        return Ok(());
    }
    let mut known = twice(|| lidarr.artist_numbers())?;
    let mut setup: Option<Setup> = None;
    let mut errors = 0;
    for (artist, places) in groups {
        if stopping() {
            break;
        }
        let offers: Vec<Offer> = with(|batch| places.iter().filter_map(|&at| batch.wants[at].offer.clone()).collect());
        // Whether this is an artist to watch Lidarr with (see `Batch::proof`).
        let mut unproven = false;
        let went = match known.get(&artist) {
            Some(&number) => ask_known(lidarr, number, &offers).map(|went| (number, went)),
            None => ask_new(lidarr, &mut setup, &offers).map(|(number, went, watch)| {
                known.insert(artist.clone(), number);
                unproven = watch;
                (number, went)
            }),
        };
        let number = went.as_ref().map(|(number, _)| *number).unwrap_or_default();
        match went {
            Err(reason) => {
                errors += 1;
                with(|batch| places.iter().for_each(|&at| batch.wants[at].reason = reason.trim_end_matches('.').to_string()));
                if errors >= GIVE_UP {
                    with(|batch| batch.wants.iter_mut().filter(|want| want.state == State::Found).for_each(|want| want.reason.clear()));
                    return Err(reason);
                }
            }
            Ok((number, went)) => {
                errors = 0;
                let notes = with(|batch| {
                    let mut notes = Vec::new();
                    for (&at, went) in places.iter().zip(went) {
                        let want = &mut batch.wants[at];
                        want.lidarr_artist = number;
                        want.reason.clear();
                        let counts = match went {
                            Went::Listed => {
                                want.state = State::Done;
                                true
                            }
                            Went::Sent(counts) => {
                                want.state = State::Asked;
                                counts
                            }
                            Went::Failed(reason) => {
                                want.state = State::Failed;
                                want.reason = reason;
                                false
                            }
                        };
                        if let (true, Some(offer)) = (counts, want.offer.clone()) {
                            want.noted = true;
                            notes.push(note(want, &offer));
                        }
                    }
                    notes
                });
                if !notes.is_empty() {
                    tell(Event::Noted(notes));
                }
            }
        }
        if unproven {
            // Written down first: the artist is with Lidarr whatever happens next.
            save();
            WATCHING.store(true, Ordering::SeqCst);
            tell(Event::Changed);
            let seen = watch(lidarr, number, &offers);
            WATCHING.store(false, Ordering::SeqCst);
            match seen {
                Ok(Some(holds)) => {
                    with(|batch| batch.proof = Some((lidarr.address().to_string(), holds)));
                    save();
                }
                // Couldn't be told from this artist; the next is watched too.
                Ok(None) => {}
                // Lidarr never got round to them: the rest wait.
                Err(reason) => return Err(reason),
            }
        }
        progress(tell);
    }
    if !stopping() {
        with(|batch| {
            for want in batch.wants.iter_mut().filter(|want| want.state == State::Found && !want.reason.is_empty()) {
                want.state = State::Failed;
                want.again = true;
            }
        });
    }
    Ok(())
}

/// Asks for releases of an artist Lidarr already has.
fn ask_known(lidarr: &Lidarr, artist: i64, offers: &[Offer]) -> Result<Vec<Went>, String> {
    let listed = twice(|| lidarr.listed(artist))?;
    // Lidarr may be reading the artist in still: their singles, for an earlier go at this,
    // or all of them, if they were added a moment before the app last closed. Then what
    // isn't listed yet may be soon, and is looked for again when Lidarr has finished.
    let mut reading = with(|batch| batch.restore.iter().any(|(known, _)| *known == artist));
    if !reading && offers.iter().any(|offer| !listed.iter().any(|known| known.album == offer.album)) {
        reading = listed.is_empty() || twice(|| lidarr.busy_with(&HashSet::from([artist])))?.contains(&artist);
    }
    let mut wanted = Vec::new();
    let mut went = Vec::new();
    for offer in offers {
        went.push(match listed.iter().find(|known| known.album == offer.album) {
            Some(known) => {
                if !known.wanted && !known.have {
                    wanted.push(known.id);
                }
                Went::Listed
            }
            None if reading => Went::Sent(false),
            // Lidarr only lists the kinds of release an artist's profile names. A single or
            // an EP may just need the artist moved to a profile that names those.
            None if offer.kind() != "Album" => match lidarr.start_listing_singles(artist, offer) {
                Ok(Some(before)) => {
                    with(|batch| batch.restore.push((artist, before)));
                    save();
                    reading = true;
                    Went::Sent(false)
                }
                Ok(None) => Went::Failed(not_listed(offer)),
                Err(reason) => Went::Failed(reason.trim_end_matches('.').to_string()),
            },
            None => Went::Failed(not_listed(offer)),
        });
    }
    lidarr.want(artist, &wanted)?;
    Ok(went)
}

/// Asks for releases of an artist Lidarr doesn't have yet. Returns Lidarr's number for
/// them, how asking for each release went, and whether Lidarr is to be watched with them.
fn ask_new(lidarr: &Lidarr, setup: &mut Option<Setup>, offers: &[Offer]) -> Result<(i64, Vec<Went>, bool), String> {
    let proof = with(|batch| batch.proof.clone()).filter(|(address, _)| address == lidarr.address()).map(|(_, holds)| holds);
    if proof == Some(false) {
        // The long way: one release at a time, each seen onto the list before the next.
        let went: Vec<Went> = offers
            .iter()
            .map(|offer| match lidarr.request(offer) {
                Ok(()) => Went::Listed,
                Err(reason) => Went::Failed(reason.trim_end_matches('.').to_string()),
            })
            .collect();
        let number = lidarr.artist_numbers()?.get(&offers[0].artist_id).copied().unwrap_or_default();
        return Ok((number, went, false));
    }
    let setup = match setup {
        Some(setup) => setup,
        None => setup.insert(twice(|| lidarr.setup())?),
    };
    let number = lidarr.add_wanting(setup, offers)?;
    Ok((number, offers.iter().map(|_| Went::Sent(true)).collect(), proof.is_none()))
}

/// Watches Lidarr read in the first artist added with a list of what's wanted, to see that
/// it marks those releases and no others. `Some(true)` if it did; `Some(false)` if it marked
/// more, which are then unmarked; `None` if that can't be told from this artist (they have
/// nothing else to mark, or asking was stopped). An error if Lidarr never got round to them.
fn watch(lidarr: &Lidarr, artist: i64, offers: &[Offer]) -> Result<Option<bool>, String> {
    let mut moved = Instant::now();
    let mut jobs = 0;
    let mut quiet: Option<Instant> = None;
    let only = HashSet::from([artist]);
    while moved.elapsed() < STALLED {
        std::thread::sleep(Duration::from_secs(3));
        if stopping() {
            return Ok(None);
        }
        match lidarr.busy(&only) {
            Ok((busy, _)) if busy.is_empty() => {
                if quiet.get_or_insert_with(Instant::now).elapsed() < QUIET {
                    continue;
                }
            }
            Ok((_, stamp)) => {
                // Still at it, or still getting to it.
                if stamp != jobs {
                    jobs = stamp;
                    moved = Instant::now();
                }
                quiet = None;
                continue;
            }
            Err(_) => {
                quiet = None;
                continue;
            }
        }
        let Ok(listed) = lidarr.listed(artist) else { continue };
        if listed.is_empty() {
            continue;
        }
        let more: Vec<i64> = listed.iter().filter(|known| known.wanted && !offers.iter().any(|offer| offer.album == known.album)).map(|known| known.id).collect();
        if !more.is_empty() {
            let _ = lidarr.unwant(&more);
            return Ok(Some(false));
        }
        return Ok((listed.len() > offers.len()).then_some(true));
    }
    Err("Lidarr hasn't read in the first artist it was given. The rest can be asked for once it has.".into())
}

/// Sees Lidarr through the artists it's reading in: as it finishes with each, checks that
/// what was asked for is on its list, and puts it there if it isn't.
fn settle(lidarr: &Lidarr, tell: &Tell) -> Result<(), String> {
    let mut quiet: HashMap<i64, Instant> = HashMap::new();
    let mut empty: HashMap<i64, Instant> = HashMap::new();
    let mut moved = Instant::now();
    let mut jobs = 0;
    let mut errors = 0;
    loop {
        let waiting: HashSet<i64> = with(|batch| {
            batch.wants.iter().filter(|want| want.state == State::Asked).map(|want| want.lidarr_artist).chain(batch.restore.iter().map(|(artist, _)| *artist)).collect()
        });
        if waiting.is_empty() {
            return Ok(());
        }
        match lidarr.busy(&waiting) {
            Err(reason) => {
                errors += 1;
                if errors >= GIVE_UP {
                    return Err(reason);
                }
            }
            Ok((busy, stamp)) => {
                errors = 0;
                if stamp != jobs {
                    jobs = stamp;
                    moved = Instant::now();
                }
                quiet.retain(|artist, _| !busy.contains(artist));
                for &artist in waiting.difference(&busy) {
                    if quiet.entry(artist).or_insert_with(Instant::now).elapsed() < QUIET {
                        continue;
                    }
                    let given_up = empty.get(&artist).is_some_and(|since| since.elapsed() > NOTHING_LISTED);
                    match finish(lidarr, artist, given_up, tell) {
                        Ok(true) => moved = Instant::now(),
                        Ok(false) => {
                            empty.entry(artist).or_insert_with(Instant::now);
                        }
                        Err(reason) => {
                            errors += 1;
                            if errors >= GIVE_UP {
                                return Err(reason);
                            }
                        }
                    }
                }
            }
        }
        if moved.elapsed() > STALLED {
            return Err("Lidarr hasn't finished reading in the artists it was given. What was asked for is with Lidarr.".into());
        }
        progress(tell);
        std::thread::sleep(POLL);
    }
}

/// Checks one artist Lidarr has finished reading in. `Ok(false)` if Lidarr lists nothing
/// for them yet and it's worth looking again later.
fn finish(lidarr: &Lidarr, artist: i64, given_up: bool, tell: &Tell) -> Result<bool, String> {
    let listed = lidarr.listed(artist)?;
    let places: Vec<(usize, Offer)> = with(|batch| {
        batch.wants.iter().enumerate().filter(|(_, want)| want.state == State::Asked && want.lidarr_artist == artist).filter_map(|(at, want)| Some((at, want.offer.clone()?))).collect()
    });
    if listed.is_empty() && !places.is_empty() && !given_up {
        return Ok(false);
    }
    let wanted: Vec<i64> =
        places.iter().filter_map(|(_, offer)| listed.iter().find(|known| known.album == offer.album)).filter(|known| !known.wanted && !known.have).map(|known| known.id).collect();
    lidarr.want(artist, &wanted)?;
    if let Some(before) = with(|batch| batch.restore.iter().find(|(known, _)| *known == artist).map(|(_, before)| before.clone())) {
        lidarr.set_new_releases(artist, &before)?;
        with(|batch| batch.restore.retain(|(known, _)| *known != artist));
    }
    let (notes, gone) = with(|batch| {
        let (mut notes, mut gone) = (Vec::new(), Vec::new());
        for (at, offer) in &places {
            let want = &mut batch.wants[*at];
            if listed.iter().any(|known| known.album == offer.album) {
                want.state = State::Done;
                if !want.noted {
                    want.noted = true;
                    notes.push(note(want, offer));
                }
            } else {
                want.state = State::Failed;
                want.reason = not_listed(offer);
                if want.noted {
                    want.noted = false;
                    gone.push(offer.title.clone());
                }
            }
        }
        (notes, gone)
    });
    save();
    if !notes.is_empty() {
        tell(Event::Noted(notes));
    }
    if !gone.is_empty() {
        tell(Event::Forgot(gone));
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(title: &str, artist: &str, kind: &str) -> Offer {
        Offer { album: format!("{title}/{artist}/{kind}"), title: title.into(), artist: artist.into(), kind: kind.into(), ..Default::default() }
    }

    #[test]
    fn picks_the_release_a_playlist_meant() {
        let found = [
            offer("Need You Now (Lady Antebellum)", "Cranius", "Single"),
            offer("Need You Now", "Lady A", "Single"),
            offer("Need You Now", "Lady A", ""),
            offer("Supernatural", "Santana", ""),
            offer("Supernatural", "Des’ree", ""),
            offer("“Heroes”", "David Bowie", ""),
            offer("Freakin’ Out", "Dexter and The Moonrocks", "Single"),
            offer("Written in the Stars", "Tinie Tempah", "Single"),
            offer("Written in the Stars", "Elton John", "Single"),
        ];
        let kind = |artist: &str, album: &str, singles: bool| choose(&found, artist, album, singles).map(|offer| (offer.artist, offer.kind));
        // The album before the single of the same name.
        assert_eq!(kind("Lady A", "Need You Now", true), Some(("Lady A".into(), String::new())));
        // What Spotify tacks on to a name doesn't matter, nor how quotes are written.
        assert_eq!(kind("Santana", "Supernatural (Remastered)", true), Some(("Santana".into(), String::new())));
        assert_eq!(kind("David Bowie", "\"Heroes\" (2017 Remaster)", true), Some(("David Bowie".into(), String::new())));
        assert_eq!(kind("Dexter and The Moonrocks", "Freakin' Out", true), Some(("Dexter and The Moonrocks".into(), "Single".into())));
        // A song a playlist lists by its single is asked for as that single...
        assert_eq!(kind("Tinie Tempah", "Written in the Stars (feat. Eric Turner)", true), Some(("Tinie Tempah".into(), "Single".into())));
        // ...unless singles are switched off.
        assert_eq!(kind("Tinie Tempah", "Written in the Stars (feat. Eric Turner)", false), None);
        // Someone else's album of the same name isn't it.
        assert_eq!(kind("Carlos Nobody", "Supernatural", true), None);
        assert_eq!(kind("Santana", "", true), None);
    }

    #[test]
    fn says_how_a_batch_stands() {
        let mut summary = Summary { total: 80, waiting: 43, unknown: 3, found: 34, running: true, ..Default::default() };
        assert_eq!(summary.notice(), ("Looking up 80 albums in Lidarr: 37 done, 3 not found.".to_string(), "Stop", ""));
        summary = Summary { total: 80, found: 37, listed: 40, unknown: 3, running: true, ..Default::default() };
        assert_eq!(summary.notice(), ("Putting 77 albums on Lidarr's list: 40 done.".to_string(), "Stop", ""));
        summary.watching = true;
        assert!(summary.notice().0.starts_with("Putting 77 albums on Lidarr's list. Lidarr is reading in the first artist"));
        summary = Summary { total: 80, listed: 77, reading: 12, unknown: 3, running: true, ..Default::default() };
        assert_eq!(summary.notice(), ("77 albums are on Lidarr's list. Lidarr is still reading in 12 artists.".to_string(), "", ""));
        summary = Summary { total: 80, listed: 77, unknown: 2, failed: 1, ..Default::default() };
        assert_eq!(summary.notice(), ("77 of 80 albums are on Lidarr's list. The other 3 are listed below.".to_string(), "Clear", ""));
        // One of them only because MusicBrainz was busy.
        summary.again = 1;
        assert_eq!((summary.notice().1, summary.notice().2), ("Try again", "Clear"));
        summary = Summary { total: 80, waiting: 40, listed: 40, stopped: "Couldn't reach Lidarr.".into(), ..Default::default() };
        assert_eq!(summary.notice(), ("Stopped with 40 of 80 albums still to ask for. Couldn't reach Lidarr.".to_string(), "Carry on", ""));
        assert_eq!(Summary { total: 80, listed: 80, ..Default::default() }.notice(), (String::new(), "", ""));
        assert_eq!(Summary::default().notice(), (String::new(), "", ""));
    }

    #[test]
    fn joins_a_batch_that_is_under_way() {
        // (The batch is shared by the whole program; this is the only test that fills it.)
        let want = |artist: &str, album: &str| Want { artist: artist.into(), album: album.into(), songs: vec!["A song".into()], ..Default::default() };
        start(vec![want("Santana", "Supernatural (Remastered)"), want("Lady A", "Need You Now")]);
        assert_eq!((summary().total, summary().waiting), (2, 2));
        assert!(unfinished());
        // The same album again, and a new one: one more to do, not two.
        with(|batch| batch.wants[0].state = State::Unknown);
        start(vec![want("santana", "Supernatural"), want("Coldplay", "Parachutes")]);
        assert_eq!((summary().total, summary().waiting), (3, 3));
        // Once it's all done, the next lot replaces it.
        with(|batch| batch.wants.iter_mut().for_each(|want| want.state = State::Done));
        assert!(!unfinished());
        start(vec![want("Coldplay", "Parachutes")]);
        assert_eq!(summary().total, 1);
        clear();
        assert_eq!(summary().total, 0);
    }
}
