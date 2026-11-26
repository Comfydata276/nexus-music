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
//!
//! The job's rules are here: what it asks Lidarr and MusicBrainz, in what order, how it
//! waits and when it gives up, and how the batch stands. Where it's written down, the thread
//! it runs on and the threads it looks albums up on are the app's (see `Desk`).

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::asked::{Asked, Fill};
use crate::lidarr::{Lidarr, Setup};
use crate::musicbrainz::MusicBrainz;
use crate::offer::{Offer, VARIOUS};
use crate::text::{same_artist, undecorated, words};

/// How many albums are looked up at once.
pub const LOOKUPS: usize = 3;
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

pub type Tell = dyn Fn(Event) + Send + Sync;

/// What the job needs of the app.
pub trait Desk: Send + Sync {
    /// Writes the batch down where it's kept between runs, or removes what's written there
    /// (`None`: there's nothing to keep). `flushed`: wait for the disk, as while the batch
    /// holds a way back (a power cut must not leave the file without it); otherwise in
    /// passing.
    fn keep(&self, batch: Option<Vec<u8>>, flushed: bool);
    /// Writes the batch down and says whether it is on the disk: for what must be kept
    /// before Lidarr is changed (the settings to put back).
    fn keep_checked(&self, batch: Vec<u8>) -> bool {
        self.keep(Some(batch), true);
        true
    }
    /// The time, in seconds since 1970: when something was noted as asked for.
    fn now(&self) -> i64;
    /// Whether singles and EPs are asked for as well as albums. Read once a run, when the
    /// looking up starts.
    fn singles(&self) -> bool;
    /// Runs `work` on `count` threads at once, and returns once every one has finished.
    /// Each goes on looking albums up until none are left, so running it once, on the
    /// calling thread, looks them all up one at a time; that's what this does unless the
    /// app says otherwise.
    ///
    /// One at a time is not quite the same as three. The look-ups stop for the batch to be
    /// carried on later once `GIVE_UP` (five) have failed in a row, counted across all of
    /// them, and any that succeeds starts the count again. With three at once, one that
    /// succeeds while the others are failing resets the count, so a Lidarr that fails most
    /// look-ups but not all is worked through; one at a time, five failures in a row stop
    /// the batch where three would have carried on. The Windows app runs three. An app
    /// that runs one should know it gives up sooner on a Lidarr like that; to give the same
    /// answers it must run `count` at once, and then each request must say which look-up it
    /// belongs to if the app needs to tell them apart, as they arrive on threads it started.
    fn together(&self, count: usize, work: &(dyn Fn() + Sync)) {
        let _ = count;
        work()
    }
}

/// The batch's file is where the way back from a change to Lidarr is written down: those of
/// the batch's own requests, and of one asked for by itself (`requests::request`).
impl crate::lidarr::WayBack for Job {
    fn turn(&self) -> Box<dyn crate::lidarr::Turn + '_> {
        Box::new(self.turn.lock().unwrap_or_else(|stuck| stuck.into_inner()))
    }

    /// Written down, surely (`Desk::keep_checked`), before Lidarr is changed. The caller holds
    /// the turn, so an entry found here was written by a step that finished: it is on the
    /// disk, and is that step's (`Ok(false)`); none is ever seen half made.
    fn keep(&self, artist: i64, before: &str) -> Result<bool, String> {
        if self.with(|batch| batch.restore.iter().any(|(known, _)| *known == artist)) {
            return Ok(false);
        }
        self.with(|batch| batch.restore.push((artist, before.to_string())));
        if self.save_checked() {
            return Ok(true);
        }
        self.with(|batch| batch.restore.retain(|known| *known != (artist, before.to_string())));
        Err("What Lidarr's settings would have to be put back to couldn't be written down on this PC first, so they weren't changed".into())
    }

    /// Crossed off: only the way back this request wrote, not one the batch had before it.
    fn put_back(&self, artist: i64, before: &str) {
        let crossed = self.with(|batch| {
            let had = batch.restore.len();
            batch.restore.retain(|known| *known != (artist, before.to_string()));
            batch.restore.len() != had
        });
        if crossed {
            self.save();
        }
    }
}

/// A batch of albums being asked for, and the job that works through it. The app has one.
pub struct Job {
    batch: Mutex<Batch>,
    running: AtomicBool,
    stopping: AtomicBool,
    /// The first artist added in one go is being watched (see `Batch::proof`).
    watching: AtomicBool,
    /// When the counts were last said and the batch last written down (see `progress`).
    told: Mutex<Option<(Instant, Instant)>>,
    desk: Box<dyn Desk>,
    /// One step of the way back at a time (`WayBack::turn`). Never taken by the window's
    /// thread.
    turn: Mutex<()>,
    /// Held by whoever writes the batch down, from taking the copy to writing it, so that a
    /// copy taken earlier is never written over one taken later; the batch itself is let go
    /// before the disk is touched (`save`).
    writing: Mutex<()>,
}

/// What a title comes down to for comparing: its words, without what's often tacked on.
fn key(title: &str) -> String {
    words(&undecorated(title)).join(" ")
}

fn same_want(a: &Want, b: &Want) -> bool {
    words(&a.artist) == words(&b.artist) && if a.album.is_empty() || b.album.is_empty() { a.album == b.album && a.songs == b.songs } else { key(&a.album) == key(&b.album) }
}

fn unfinished_in(batch: &Batch) -> bool {
    !batch.restore.is_empty() || batch.wants.iter().any(|want| matches!(want.state, State::Waiting | State::Found | State::Asked))
}

impl Job {
    pub fn new(desk: impl Desk + 'static) -> Job {
        Job {
            batch: Mutex::new(Batch::default()),
            running: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
            watching: AtomicBool::new(false),
            told: Mutex::new(None),
            desk: Box::new(desk),
            turn: Mutex::new(()),
            writing: Mutex::new(()),
        }
    }

    fn with<T>(&self, act: impl FnOnce(&mut Batch) -> T) -> T {
        act(&mut self.batch.lock().unwrap_or_else(|stuck| stuck.into_inner()))
    }

    /// Takes up the batch that was under way when the app last closed, as it was written down
    /// (see `Desk::keep`), and says whether it could be read. What can't be read is left
    /// alone: what to do with it, and what to tell the person, is the app's.
    pub fn load(&self, written: &[u8]) -> bool {
        let Ok(batch) = serde_json::from_slice::<Batch>(written) else { return false };
        self.with(|now| *now = batch);
        true
    }

    /// Writes the batch down: a copy taken under the batch's lock, written after it is let go
    /// (the Requests page reads the batch on the window's thread, which never waits on a
    /// disk), under the writers' lock, so that a copy taken earlier is never written over one
    /// taken later. While it holds a way back, flushed.
    fn save(&self) {
        let _writing = self.writing.lock().unwrap_or_else(|stuck| stuck.into_inner());
        let (json, flushed) = self.with(|batch| {
            let json = if batch.wants.is_empty() && batch.restore.is_empty() && batch.proof.is_none() { None } else { serde_json::to_vec(batch).ok() };
            (json, !batch.restore.is_empty())
        });
        self.desk.keep(json, flushed);
    }

    /// Writes the batch down as `save` does, flushed, and says whether it is on the disk
    /// (`Desk::keep_checked`).
    fn save_checked(&self) -> bool {
        let _writing = self.writing.lock().unwrap_or_else(|stuck| stuck.into_inner());
        let json = self.with(|batch| serde_json::to_vec(batch).ok());
        json.is_some_and(|json| self.desk.keep_checked(json))
    }

    /// Hands over albums to ask for. If a batch is still under way they join it; otherwise they
    /// replace what's left of the last one.
    pub fn start(&self, wants: Vec<Want>) {
        self.with(|batch| {
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
        self.save();
    }

    /// Whether there's a batch with something still to do.
    pub fn unfinished(&self) -> bool {
        self.with(|batch| unfinished_in(batch))
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Stops asking for more. What Lidarr has already been asked for stays asked for, and is
    /// still checked.
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
    }

    fn stopping(&self) -> bool {
        self.stopping.load(Ordering::SeqCst)
    }

    /// Has another go at what couldn't be asked for last time for a reason that may have passed.
    /// Says whether there was anything to have another go at.
    pub fn again(&self) -> bool {
        if self.running() {
            return false;
        }
        let any = self.with(|batch| {
            let mut any = false;
            for want in batch.wants.iter_mut().filter(|want| want.again && matches!(want.state, State::Unknown | State::Failed)) {
                want.state = State::Waiting;
                want.again = false;
                want.reason.clear();
                any = true;
            }
            any
        });
        self.save();
        any
    }

    /// Forgets a batch that has ended: its results have been read.
    pub fn clear(&self) {
        if self.running() {
            return;
        }
        self.with(|batch| {
            batch.wants.clear();
            batch.stopped.clear();
        });
        self.save();
    }

    pub fn summary(&self) -> Summary {
        self.with(|batch| {
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
                running: self.running(),
                watching: self.watching.load(Ordering::SeqCst),
                stopped: batch.stopped.clone(),
            }
        })
    }

    /// The albums of the batch that couldn't be asked for, with why.
    pub fn missed(&self) -> Vec<Want> {
        self.with(|batch| batch.wants.iter().filter(|want| matches!(want.state, State::Unknown | State::Failed)).cloned().collect())
    }
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
        // A studio release before a soundtrack of the same name.
        .min_by_key(|offer| (words(&offer.title) != exact, offer.soundtrack, rank(offer)))
        .cloned()
}

/// One question to Lidarr, asked again after a moment if it couldn't be answered.
fn twice<T>(lidarr: &Lidarr, ask: impl Fn() -> Result<T, String>) -> Result<T, String> {
    ask().or_else(|_| {
        lidarr.pause(Duration::from_secs(2))?;
        ask()
    })
}

/// How looking an album up went.
enum Lookup {
    Is(Offer),
    /// Lidarr doesn't know it.
    Not,
    /// It wasn't found by name, and what its songs came out on couldn't be asked.
    Unsure,
}

impl Job {
    /// A search of Lidarr's, which it passes on to a service of its own that now and then is
    /// too busy to answer. Lidarr then answers with an error, and is asked again a few times,
    /// further apart. A Lidarr that can't be reached at all isn't waited on like that.
    fn search(&self, lidarr: &Lidarr, term: &str) -> Result<Vec<Offer>, String> {
        let mut waits = [2, 6, 15].iter();
        loop {
            match lidarr.find_any(term) {
                Ok(found) => return Ok(found),
                Err(reason) => {
                    let answered = reason.starts_with("Lidarr answered");
                    let Some(&wait) = waits.next().filter(|&&wait| !self.stopping() && (answered || wait == 2)) else { return Err(reason) };
                    lidarr.pause(Duration::from_secs(wait))?;
                }
            }
        }
    }

    /// What a song came out on, according to MusicBrainz, which is asked again after a while if
    /// it's too busy to say. An error if it stayed that way.
    fn release_of(&self, lidarr: &Lidarr, musicbrainz: &MusicBrainz, want: &Want, song: &str, singles: bool) -> Result<Option<String>, String> {
        let mut waits = ASK_AGAIN.iter();
        loop {
            match musicbrainz.release_with(&want.artist, song, &want.album, singles) {
                Ok(release) => return Ok(release),
                Err(reason) => {
                    let Some(&wait) = waits.next().filter(|_| !self.stopping()) else { return Err(reason) };
                    lidarr.pause(Duration::from_secs(wait))?;
                }
            }
        }
    }

    /// Finds an album by the name a playlist gives it, as Lidarr's search answers.
    fn by_name(&self, lidarr: &Lidarr, want: &Want, singles: bool) -> Result<Option<Offer>, String> {
        let found = self.search(lidarr, &format!("{} {}", want.album, want.artist))?;
        if let Some(offer) = choose(&found, &want.artist, &want.album, singles) {
            return Ok(Some(offer));
        }
        // Lidarr's search can be thrown by the trimmings; ask without them.
        let bare = undecorated(&want.album);
        if bare != want.album.trim() {
            let found = self.search(lidarr, &format!("{bare} {}", want.artist))?;
            return Ok(choose(&found, &want.artist, &want.album, singles));
        }
        Ok(None)
    }

    /// Finds what Lidarr calls an album. An error only if Lidarr couldn't be asked.
    fn find(&self, lidarr: &Lidarr, musicbrainz: &MusicBrainz, want: &Want, singles: bool) -> Result<Lookup, String> {
        // Lidarr's search fails outright for some words ("Thriller" is one), whatever else is
        // asked with them. That isn't the end of it: the album can still be found by its songs,
        // and looked up by the name that gives, which is a different question to Lidarr.
        let mut failed = None;
        if !want.album.is_empty() {
            match self.by_name(lidarr, want, singles) {
                Ok(Some(offer)) => return Ok(Lookup::Is(offer)),
                Ok(None) => {}
                Err(reason) if reason.starts_with("Lidarr answered") => failed = Some(reason),
                Err(reason) => return Err(reason),
            }
        }
        // By one of its songs: MusicBrainz says what the song came out on, by the name Lidarr
        // knows it by.
        let mut unsure = false;
        for song in want.songs.iter().take(2) {
            let release = match self.release_of(lidarr, musicbrainz, want, song, singles) {
                Ok(Some(release)) => release,
                Ok(None) => continue,
                Err(_) => {
                    unsure = true;
                    continue;
                }
            };
            let found = self.search(lidarr, &format!("lidarr:{release}"))?;
            if let Some(offer) = found.into_iter().find(|offer| offer.album == release && (singles || offer.kind() == "Album")) {
                return Ok(Lookup::Is(offer));
            }
        }
        match failed {
            Some(reason) => Err(reason),
            None => Ok(if unsure { Lookup::Unsure } else { Lookup::Not }),
        }
    }
}

// ---- the work ----------------------------------------------------------------------------------------

impl Job {
    /// Gets ready to work through the batch, if it isn't being worked through already: says
    /// whether it is to be. The app then calls `work_through` on a thread of its own, or
    /// `not_started` if it couldn't start one.
    pub fn begin(&self) -> bool {
        if self.running.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.stopping.store(false, Ordering::SeqCst);
        self.with(|batch| batch.stopped.clear());
        true
    }

    /// Works through the batch, start to end. Takes minutes: Lidarr is seen through every
    /// artist it was given.
    pub fn work_through(&self, lidarr: &Lidarr, musicbrainz: &MusicBrainz, tell: &Tell) {
        tell(Event::Changed);
        let outcome = self.work(lidarr, musicbrainz, tell);
        self.with(|batch| {
            if let Err(reason) = outcome {
                batch.stopped = reason;
            }
        });
        self.save();
        self.running.store(false, Ordering::SeqCst);
        tell(Event::Ended);
    }

    /// The app couldn't start the thread to work through the batch on.
    pub fn not_started(&self) {
        self.running.store(false, Ordering::SeqCst);
    }

    fn work(&self, lidarr: &Lidarr, musicbrainz: &MusicBrainz, tell: &Tell) -> Result<(), String> {
        let found = self.find_all(lidarr, musicbrainz, tell);
        let asked = found.and_then(|()| self.ask_all(lidarr, tell));
        if self.stopping() {
            // What hasn't been asked for won't be.
            self.with(|batch| batch.wants.retain(|want| !matches!(want.state, State::Waiting | State::Found)));
        }
        self.save();
        if asked.is_ok() {
            tell(Event::Asked);
        }
        // Whatever became of the asking, Lidarr is seen through the artists it was given.
        let settled = self.settle(lidarr, tell);
        asked.and(settled)
    }

    /// Says the counts have moved, and writes the batch down: neither more often than is useful.
    fn progress(&self, tell: &Tell) {
        let (say, write) = {
            let mut told = self.told.lock().unwrap_or_else(|stuck| stuck.into_inner());
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
            self.save();
        }
        if say {
            tell(Event::Changed);
        }
    }

    fn find_all(&self, lidarr: &Lidarr, musicbrainz: &MusicBrainz, tell: &Tell) -> Result<(), String> {
        let todo: Vec<usize> = self.with(|batch| batch.wants.iter().enumerate().filter(|(_, want)| want.state == State::Waiting).map(|(at, _)| at).collect());
        if todo.is_empty() {
            return Ok(());
        }
        let singles = self.desk.singles();
        let (next, errors) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let last: Mutex<String> = Mutex::new(String::new());
        self.desk.together(LOOKUPS.min(todo.len()), &|| {
            while !self.stopping() && errors.load(Ordering::SeqCst) < GIVE_UP {
                let Some(&at) = todo.get(next.fetch_add(1, Ordering::SeqCst)) else { break };
                let want = self.with(|batch| batch.wants[at].clone());
                match self.find(lidarr, musicbrainz, &want, singles) {
                    Ok(found) => {
                        errors.store(0, Ordering::SeqCst);
                        self.with(|batch| {
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
                        self.with(|batch| batch.wants[at].reason = reason.trim_end_matches('.').to_string());
                        *last.lock().unwrap_or_else(|stuck| stuck.into_inner()) = reason;
                    }
                }
                self.progress(tell);
            }
        });
        if errors.load(Ordering::SeqCst) >= GIVE_UP {
            // Lidarr can't be reached: these are still to look up when it can.
            self.with(|batch| batch.wants.iter_mut().filter(|want| want.state == State::Waiting).for_each(|want| want.reason.clear()));
            return Err(last.into_inner().unwrap_or_else(|stuck| stuck.into_inner()));
        }
        if !self.stopping() {
            // One that couldn't be looked up while those around it could.
            self.with(|batch| {
                for want in batch.wants.iter_mut().filter(|want| want.state == State::Waiting && !want.reason.is_empty()) {
                    want.state = State::Failed;
                    want.again = true;
                }
            });
        }
        Ok(())
    }
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

impl Job {
    /// A note of something asked for, to be followed up when it arrives.
    fn note(&self, want: &Want, offer: &Offer) -> Asked {
        Asked { album: offer.album.clone(), title: offer.title.clone(), artist: offer.artist.clone(), at: self.desk.now(), fills: want.fills.clone(), ..Default::default() }
    }

    fn ask_all(&self, lidarr: &Lidarr, tell: &Tell) -> Result<(), String> {
        // By artist, in the order they come.
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        self.with(|batch| {
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
        let mut known = twice(lidarr, || lidarr.artist_numbers())?;
        let mut setup: Option<Setup> = None;
        let mut errors = 0;
        for (artist, places) in groups {
            if self.stopping() {
                break;
            }
            let offers: Vec<Offer> = self.with(|batch| places.iter().filter_map(|&at| batch.wants[at].offer.clone()).collect());
            // Whether this is an artist to watch Lidarr with (see `Batch::proof`).
            let mut unproven = false;
            let went = match known.get(&artist) {
                // "Various Artists", which soundtracks are credited to, isn't an artist whose
                // releases Lidarr can read in: its albums are put there one at a time.
                _ if artist == VARIOUS => Ok((
                    0,
                    offers
                        .iter()
                        .map(|offer| match lidarr.request(offer, self) {
                            Ok(()) => Went::Listed,
                            Err(reason) => Went::Failed(reason.trim_end_matches('.').to_string()),
                        })
                        .collect(),
                )),
                Some(&number) => self.ask_known(lidarr, number, &offers).map(|went| (number, went)),
                None => self.ask_new(lidarr, &mut setup, &offers).map(|(number, went, watch)| {
                    known.insert(artist.clone(), number);
                    unproven = watch;
                    (number, went)
                }),
            };
            let number = went.as_ref().map(|(number, _)| *number).unwrap_or_default();
            match went {
                Err(reason) => {
                    errors += 1;
                    self.with(|batch| places.iter().for_each(|&at| batch.wants[at].reason = reason.trim_end_matches('.').to_string()));
                    if errors >= GIVE_UP {
                        self.with(|batch| batch.wants.iter_mut().filter(|want| want.state == State::Found).for_each(|want| want.reason.clear()));
                        return Err(reason);
                    }
                }
                Ok((number, went)) => {
                    errors = 0;
                    let notes = self.with(|batch| {
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
                                notes.push(self.note(want, &offer));
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
                self.save();
                self.watching.store(true, Ordering::SeqCst);
                tell(Event::Changed);
                let seen = self.watch(lidarr, number, &offers);
                self.watching.store(false, Ordering::SeqCst);
                match seen {
                    Ok(Some(holds)) => {
                        self.with(|batch| batch.proof = Some((lidarr.address().to_string(), holds)));
                        self.save();
                    }
                    // Couldn't be told from this artist; the next is watched too.
                    Ok(None) => {}
                    // Lidarr never got round to them: the rest wait.
                    Err(reason) => return Err(reason),
                }
            }
            self.progress(tell);
        }
        if !self.stopping() {
            self.with(|batch| {
                for want in batch.wants.iter_mut().filter(|want| want.state == State::Found && !want.reason.is_empty()) {
                    want.state = State::Failed;
                    want.again = true;
                }
            });
        }
        Ok(())
    }

    /// Asks for releases of an artist Lidarr already has.
    fn ask_known(&self, lidarr: &Lidarr, artist: i64, offers: &[Offer]) -> Result<Vec<Went>, String> {
        let listed = twice(lidarr, || lidarr.listed(artist))?;
        // Lidarr may be reading the artist in still: their singles, for an earlier go at this,
        // or all of them, if they were added a moment before the app last closed. Then what
        // isn't listed yet may be soon, and is looked for again when Lidarr has finished.
        let mut reading = self.with(|batch| batch.restore.iter().any(|(known, _)| *known == artist));
        if !reading && offers.iter().any(|offer| !listed.iter().any(|known| known.album == offer.album)) {
            reading = listed.is_empty() || twice(lidarr, || lidarr.busy_with(&HashSet::from([artist])))?.contains(&artist);
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
                // Lidarr only lists the kinds of release an artist's profile names. A single, an
                // EP or a soundtrack may just need the artist moved to a profile that names those.
                // What it is to be put back to is on the disk before Lidarr is changed, or Lidarr
                // isn't changed. (If changing it fails after, what was written down stays: it
                // may have been changed all the same, and putting back what wasn't is nothing.)
                None if offer.kind() != "Album" || offer.soundtrack => match {
                    // One step of the way back (`WayBack::turn`).
                    let _turn = crate::lidarr::WayBack::turn(self);
                    lidarr.start_listing_singles_kept(artist, offer, self)
                } {
                    Ok(Some(_)) => {
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
    fn ask_new(&self, lidarr: &Lidarr, setup: &mut Option<Setup>, offers: &[Offer]) -> Result<(i64, Vec<Went>, bool), String> {
        let proof = self.with(|batch| batch.proof.clone()).filter(|(address, _)| address == lidarr.address()).map(|(_, holds)| holds);
        if proof == Some(false) {
            // The long way: one release at a time, each seen onto the list before the next.
            let went: Vec<Went> = offers
                .iter()
                .map(|offer| match lidarr.request(offer, self) {
                    Ok(()) => Went::Listed,
                    Err(reason) => Went::Failed(reason.trim_end_matches('.').to_string()),
                })
                .collect();
            let number = lidarr.artist_numbers()?.get(&offers[0].artist_id).copied().unwrap_or_default();
            return Ok((number, went, false));
        }
        let setup = match setup {
            Some(setup) => setup,
            None => setup.insert(twice(lidarr, || lidarr.setup())?),
        };
        let number = lidarr.add_wanting(setup, offers)?;
        Ok((number, offers.iter().map(|_| Went::Sent(true)).collect(), proof.is_none()))
    }

    /// Watches Lidarr read in the first artist added with a list of what's wanted, to see that
    /// it marks those releases and no others. `Some(true)` if it did; `Some(false)` if it marked
    /// more, which are then unmarked; `None` if that can't be told from this artist (they have
    /// nothing else to mark, or asking was stopped). An error if Lidarr never got round to them.
    fn watch(&self, lidarr: &Lidarr, artist: i64, offers: &[Offer]) -> Result<Option<bool>, String> {
        let mut moved = Instant::now();
        let mut jobs = 0;
        let mut quiet: Option<Instant> = None;
        let only = HashSet::from([artist]);
        while moved.elapsed() < STALLED {
            lidarr.pause(Duration::from_secs(3))?;
            if self.stopping() {
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
    fn settle(&self, lidarr: &Lidarr, tell: &Tell) -> Result<(), String> {
        let mut quiet: HashMap<i64, Instant> = HashMap::new();
        let mut empty: HashMap<i64, Instant> = HashMap::new();
        let mut moved = Instant::now();
        let mut jobs = 0;
        let mut errors = 0;
        loop {
            let waiting: HashSet<i64> = self.with(|batch| {
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
                        match self.finish(lidarr, artist, given_up, tell) {
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
            self.progress(tell);
            lidarr.pause(POLL)?;
        }
    }

    /// Checks one artist Lidarr has finished reading in. `Ok(false)` if Lidarr lists nothing
    /// for them yet and it's worth looking again later.
    fn finish(&self, lidarr: &Lidarr, artist: i64, given_up: bool, tell: &Tell) -> Result<bool, String> {
        let listed = lidarr.listed(artist)?;
        let places: Vec<(usize, Offer)> = self.with(|batch| {
            batch.wants.iter().enumerate().filter(|(_, want)| want.state == State::Asked && want.lidarr_artist == artist).filter_map(|(at, want)| Some((at, want.offer.clone()?))).collect()
        });
        if listed.is_empty() && !places.is_empty() && !given_up {
            return Ok(false);
        }
        let wanted: Vec<i64> =
            places.iter().filter_map(|(_, offer)| listed.iter().find(|known| known.album == offer.album)).filter(|known| !known.wanted && !known.have).map(|known| known.id).collect();
        lidarr.want(artist, &wanted)?;
        // Put back and crossed off, as one step of the way back (`WayBack::turn`).
        let turn = crate::lidarr::WayBack::turn(self);
        if let Some(before) = self.with(|batch| batch.restore.iter().find(|(known, _)| *known == artist).map(|(_, before)| before.clone())) {
            lidarr.set_new_releases(artist, &before)?;
            self.with(|batch| batch.restore.retain(|(known, _)| *known != artist));
        }
        drop(turn);
        let (notes, gone) = self.with(|batch| {
            let (mut notes, mut gone) = (Vec::new(), Vec::new());
            for (at, offer) in &places {
                let want = &mut batch.wants[*at];
                if listed.iter().any(|known| known.album == offer.album) {
                    want.state = State::Done;
                    if !want.noted {
                        want.noted = true;
                        notes.push(self.note(want, offer));
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
        self.save();
        if !notes.is_empty() {
            tell(Event::Noted(notes));
        }
        if !gone.is_empty() {
            tell(Event::Forgot(gone));
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::http::STOPPED;

    #[test]
    fn takes_a_studio_release_before_a_soundtrack_of_the_same_name() {
        let found = [Offer { soundtrack: true, ..offer("Purple Rain", "Prince", "") }, offer("Purple Rain", "Prince", "Single"), Offer { soundtrack: true, ..offer("Still Falling for You", "Ellie Goulding", "Single") }];
        assert_eq!(choose(&found, "Prince", "Purple Rain", true).map(|offer| (offer.kind, offer.soundtrack)), Some(("Single".into(), false)));
        // A soundtrack when that's all there is.
        assert_eq!(choose(&found, "Prince", "Purple Rain", false).map(|offer| offer.soundtrack), Some(true));
        assert_eq!(choose(&found, "Ellie Goulding", "Still Falling For You (From \"Bridget Jones's Baby\" Original Motion Picture Soundtrack)", true).map(|offer| offer.soundtrack), Some(true));
    }

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

    /// Keeps nothing, and asks for albums only.
    struct Nowhere;

    impl Desk for Nowhere {
        fn keep(&self, _: Option<Vec<u8>>, _: bool) {}

        fn now(&self) -> i64 {
            0
        }

        fn singles(&self) -> bool {
            false
        }
    }

    #[test]
    fn joins_a_batch_that_is_under_way() {
        let job = Job::new(Nowhere);
        let want = |artist: &str, album: &str| Want { artist: artist.into(), album: album.into(), songs: vec!["A song".into()], ..Default::default() };
        job.start(vec![want("Santana", "Supernatural (Remastered)"), want("Lady A", "Need You Now")]);
        assert_eq!((job.summary().total, job.summary().waiting), (2, 2));
        assert!(job.unfinished());
        // The same album again, and a new one: one more to do, not two.
        job.with(|batch| batch.wants[0].state = State::Unknown);
        job.start(vec![want("santana", "Supernatural"), want("Coldplay", "Parachutes")]);
        assert_eq!((job.summary().total, job.summary().waiting), (3, 3));
        // Once it's all done, the next lot replaces it.
        job.with(|batch| batch.wants.iter_mut().for_each(|want| want.state = State::Done));
        assert!(!job.unfinished());
        job.start(vec![want("Coldplay", "Parachutes")]);
        assert_eq!(job.summary().total, 1);
        job.clear();
        assert_eq!(job.summary().total, 0);
    }

    /// A Lidarr that has one artist with two albums, neither wanted yet, and knows nothing
    /// else. Every request is written down; nothing is waited for.
    struct Scripted {
        asked: Mutex<Vec<String>>,
        waits: Mutex<Vec<Duration>>,
    }

    impl crate::http::Http for Scripted {
        fn send(&self, request: &crate::http::Request) -> Result<crate::http::Response, crate::http::Failure> {
            use crate::http::Method;
            let (method, body) = match request.method {
                Method::Get => ("GET", ""),
                Method::Post(body) => ("POST", body),
                Method::Put(body) => ("PUT", body),
            };
            let path = request.url.trim_start_matches("http://lidarr.test/api/v1/");
            self.asked.lock().unwrap().push(format!("{method} {path} {body}"));
            let albums = r#"[{"id":11,"title":"Glass Orchard","foreignAlbumId":"al1","albumType":"Album","artistId":5,"monitored":false,"artist":{"artistName":"Quillon Vane","foreignArtistId":"ar1"}},
                             {"id":12,"title":"Paper Tides","foreignAlbumId":"al2","albumType":"Album","artistId":5,"monitored":false,"artist":{"artistName":"Quillon Vane","foreignArtistId":"ar1"}}]"#;
            let answer = match (method, path) {
                ("GET", path) if path.starts_with("album/lookup?term=") && path.contains("Quillon") => albums.to_string(),
                ("GET", "artist") => r#"[{"id":5,"artistName":"Quillon Vane","foreignArtistId":"ar1"}]"#.to_string(),
                ("GET", "album?artistId=5") => albums.to_string(),
                ("GET", "artist/5") => r#"{"id":5,"monitored":true}"#.to_string(),
                ("GET", _) => "[]".to_string(),
                _ => "{}".to_string(),
            };
            Ok(crate::http::Response::new(200, Vec::new(), move || Ok(answer)))
        }

        fn pause(&self, wait: Duration) -> Result<(), crate::http::Failure> {
            self.waits.lock().unwrap().push(wait);
            Ok(())
        }
    }

    /// Asks for albums only, and looks them up on as many threads as it's asked for.
    struct Threads;

    impl Desk for Threads {
        fn keep(&self, _: Option<Vec<u8>>, _: bool) {}

        fn now(&self) -> i64 {
            0
        }

        fn singles(&self) -> bool {
            false
        }

        fn together(&self, count: usize, work: &(dyn Fn() + Sync)) {
            std::thread::scope(|scope| {
                for _ in 0..count {
                    scope.spawn(work);
                }
            });
        }
    }

    /// Works through a batch of three, the first two of which Lidarr has.
    fn work_through(job: Job) -> (Vec<(State, String)>, Vec<String>, Vec<String>) {
        let http = Arc::new(Scripted { asked: Mutex::default(), waits: Mutex::default() });
        let lidarr = Lidarr::new("lidarr.test", "key", None, http.clone());
        let musicbrainz = MusicBrainz::new(http.clone(), crate::listenbrainz::ListenBrainz::new(http.clone()));
        let want = |artist: &str, album: &str| Want { artist: artist.into(), album: album.into(), ..Default::default() };
        job.start(vec![want("Quillon Vane", "Glass Orchard (Remastered)"), want("Quillon Vane", "Paper Tides"), want("Nobody", "Nothing")]);
        let noted: Arc<Mutex<Vec<String>>> = Arc::default();
        let told = noted.clone();
        assert!(job.begin() && !job.begin());
        job.work_through(&lidarr, &musicbrainz, &move |event| {
            if let Event::Noted(notes) = event {
                told.lock().unwrap().extend(notes.into_iter().map(|note| format!("{} {}", note.title, note.at)));
            }
        });
        assert!(!job.running() && http.waits.lock().unwrap().is_empty());
        let states = job.with(|batch| batch.wants.iter().map(|want| (want.state, want.reason.clone())).collect());
        let mut asked = http.asked.lock().unwrap().clone();
        // Three look-ups at once come back in any order; after them, one at a time.
        asked[..3].sort();
        let noted = noted.lock().unwrap().clone();
        (states, asked, noted)
    }

    #[test]
    fn looks_up_one_at_a_time_or_on_threads_alike() {
        let one = work_through(Job::new(Nowhere));
        let three = work_through(Job::new(Threads));
        assert_eq!(one, three);
        let (states, asked, noted) = one;
        assert_eq!(states, [(State::Done, String::new()), (State::Done, String::new()), (State::Unknown, NOT_KNOWN.to_string())]);
        assert_eq!(
            asked,
            [
                "GET album/lookup?term=Glass%20Orchard%20%28Remastered%29%20Quillon%20Vane ",
                "GET album/lookup?term=Nothing%20Nobody ",
                "GET album/lookup?term=Paper%20Tides%20Quillon%20Vane ",
                "GET artist ",
                "GET album?artistId=5 ",
                "GET artist/5 ",
                r#"PUT album/monitor {"albumIds":[11,12],"monitored":true}"#,
                r#"POST command {"albumIds":[11,12],"name":"AlbumSearch"}"#,
            ]
        );
        assert_eq!(noted, ["Glass Orchard 0", "Paper Tides 0"]);
    }

    /// A Lidarr that keeps one artist on a profile of albums only, and once their profile is
    /// changed is forever reading them in; asked over an app that has been told to stop, so
    /// every wait is cut short.
    struct Stopped(AtomicBool);

    impl crate::http::Http for Stopped {
        fn send(&self, request: &crate::http::Request) -> Result<crate::http::Response, crate::http::Failure> {
            let path = request.url.trim_start_matches("http://lidarr.test/api/v1/").to_string();
            if matches!(request.method, crate::http::Method::Put(_)) && path == "artist/5" {
                self.0.store(true, Ordering::SeqCst);
            }
            let entry = |item: &str, name: &str, allowed: bool| serde_json::json!({ item: { "id": 1, "name": name }, "allowed": allowed });
            let profile = |id: i64, name: &str, singles: bool| {
                serde_json::json!({ "id": id, "name": name,
                    "primaryAlbumTypes": [entry("albumType", "Album", true), entry("albumType", "Single", singles), entry("albumType", "EP", singles)],
                    "secondaryAlbumTypes": [entry("albumType", "Studio", true)],
                    "releaseStatuses": [entry("releaseStatus", "Official", true)] })
            };
            let answer = match path.as_str() {
                path if path.starts_with("album/lookup") => {
                    r#"[{"title":"A Single","foreignAlbumId":"al1","albumType":"Single","artist":{"artistName":"Someone","foreignArtistId":"ar1"}}]"#.to_string()
                }
                "artist" => r#"[{"id":5,"artistName":"Someone","foreignArtistId":"ar1"}]"#.to_string(),
                "album?artistId=5" => r#"[{"id":8,"title":"An Album","foreignAlbumId":"al0","albumType":"Album","monitored":true}]"#.to_string(),
                "metadataprofile" => serde_json::json!([profile(1, "Standard", false), profile(2, "Albums, EPs and singles", true)]).to_string(),
                "artist/5" => r#"{"id":5,"metadataProfileId":1,"monitorNewItems":"all","monitored":true}"#.to_string(),
                "command" if self.0.load(Ordering::SeqCst) => r#"[{"id":1,"name":"RefreshArtist","status":"started","body":{"artistIds":[5]}}]"#.to_string(),
                "command" => "[]".to_string(),
                _ => "{}".to_string(),
            };
            Ok(crate::http::Response::new(200, Vec::new(), move || Ok(answer)))
        }

        fn pause(&self, _: Duration) -> Result<(), crate::http::Failure> {
            Err(crate::http::Failure::Stopped)
        }
    }

    /// Keeps what's written down, and asks for singles too.
    struct Kept(Arc<Mutex<Option<Vec<u8>>>>);

    impl Desk for Kept {
        fn keep(&self, batch: Option<Vec<u8>>, _: bool) {
            *self.0.lock().unwrap() = batch;
        }

        fn now(&self) -> i64 {
            0
        }

        fn singles(&self) -> bool {
            true
        }
    }

    /// Writes down what's kept in passing, and can't make sure of anything on the disk.
    struct Unsure(Arc<Mutex<Option<Vec<u8>>>>);

    impl Desk for Unsure {
        fn keep(&self, batch: Option<Vec<u8>>, _: bool) {
            *self.0.lock().unwrap() = batch;
        }

        fn keep_checked(&self, _: Vec<u8>) -> bool {
            false
        }

        fn now(&self) -> i64 {
            0
        }

        fn singles(&self) -> bool {
            true
        }
    }

    #[test]
    fn the_batch_is_where_a_request_by_itself_writes_its_way_back_down() {
        // Asked for by itself (or one release at a time by the batch, when Lidarr can't be
        // told which releases are wanted): the batch's file has the way back before Lidarr is
        // changed, so a kill after the change leaves it there for the next run to put back.
        use crate::lidarr::tests::{KilledAfterChange, Stopping, single};
        let kept = Arc::new(Mutex::new(None));
        let job = Job::new(Kept(kept.clone()));
        let lidarr = Lidarr::new("lidarr.test", "key", None, Arc::new(KilledAfterChange(Stopping { asked: Default::default() })));
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| lidarr.request(&single(), &job))).is_err());
        let written: serde_json::Value = serde_json::from_slice(kept.lock().unwrap().as_ref().expect("nothing written down")).unwrap();
        assert_eq!(written["restore"], serde_json::json!([[5, "all"]]));
        // Not killed: put back, and crossed off.
        let kept = Arc::new(Mutex::new(None));
        let job = Job::new(Kept(kept.clone()));
        let lidarr = Lidarr::new("lidarr.test", "key", None, Arc::new(Stopping { asked: Default::default() }));
        let _ = lidarr.request(&single(), &job);
        assert!(job.with(|batch| batch.restore.is_empty()));
    }

    /// Notes, each time the batch is written down, whether the batch was held while it was,
    /// whether it was flushed, and how many wants the copy had.
    struct Held(Arc<Mutex<Vec<(bool, bool, usize)>>>, &'static std::sync::OnceLock<&'static Job>);

    impl Desk for Held {
        fn keep(&self, batch: Option<Vec<u8>>, flushed: bool) {
            // (A little while on the disk, so that writers overlap.)
            std::thread::sleep(Duration::from_millis(2));
            let held = self.1.get().is_some_and(|job| job.batch.try_lock().is_err());
            let wants = batch.and_then(|json| serde_json::from_slice::<serde_json::Value>(&json).ok()).map_or(0, |json| json["wants"].as_array().map_or(0, Vec::len));
            self.0.lock().unwrap().push((held, flushed, wants));
        }

        fn now(&self) -> i64 {
            0
        }

        fn singles(&self) -> bool {
            true
        }
    }

    #[test]
    fn the_batch_is_written_down_after_it_is_let_go_and_never_older_over_newer() {
        static JOB: std::sync::OnceLock<&'static Job> = std::sync::OnceLock::new();
        let noted = Arc::new(Mutex::new(Vec::new()));
        let job: &'static Job = Box::leak(Box::new(Job::new(Held(noted.clone(), &JOB))));
        JOB.set(job).ok().unwrap();
        // Eight threads, each adding a want and writing the batch down, at once.
        std::thread::scope(|scope| {
            for at in 0..8 {
                scope.spawn(move || {
                    job.start(vec![Want { artist: format!("Someone {at}"), album: format!("Album {at}"), ..Default::default() }]);
                    job.save();
                });
            }
        });
        let noted = noted.lock().unwrap().clone();
        // The batch was never held while the disk was being written to (the Requests page
        // reads it on the window's thread) ...
        assert!(noted.iter().all(|(held, _, _)| !held), "{noted:?}");
        // ... and no copy was written over one taken after it.
        assert!(noted.windows(2).all(|pair| pair[0].2 <= pair[1].2), "{noted:?}");
        assert_eq!(noted.last().map(|last| last.2), Some(8));
    }

    #[test]
    fn while_the_batch_holds_a_way_back_every_write_of_it_waits_for_the_disk() {
        static JOB: std::sync::OnceLock<&'static Job> = std::sync::OnceLock::new();
        let noted = Arc::new(Mutex::new(Vec::new()));
        let job: &'static Job = Box::leak(Box::new(Job::new(Held(noted.clone(), &JOB))));
        JOB.set(job).ok().unwrap();
        job.start(vec![Want { artist: "Someone".into(), album: "A Single".into(), ..Default::default() }]);
        assert!(!noted.lock().unwrap().last().unwrap().1, "with no way back, progress is written in passing");
        job.with(|batch| batch.restore.push((5, "all".into())));
        std::thread::scope(|scope| {
            scope.spawn(|| job.save());
            scope.spawn(|| job.save());
        });
        assert!(noted.lock().unwrap().iter().rev().take(2).all(|(_, flushed, _)| *flushed));
    }

    /// A Lidarr with one artist, whose "new releases" setting is kept (and whose profile, so
    /// that every request goes the way of a change, isn't), and a disk the batch is written
    /// to; and a note of every moment the setting was off with no way back on the disk.
    #[derive(Default)]
    struct World {
        setting: String,
        disk: Option<serde_json::Value>,
        broken: Vec<String>,
        /// How long a thread (by name) waits before each kind of step: "read", "off", "keep".
        waits: HashMap<(String, &'static str), u64>,
        /// The first checked write fails.
        first_fails: bool,
    }

    fn wait(world: &Arc<Mutex<World>>, step: &'static str) {
        let name = std::thread::current().name().unwrap_or("").to_string();
        let ms = world.lock().unwrap().waits.get(&(name, step)).copied().unwrap_or(0);
        std::thread::sleep(Duration::from_millis(ms));
    }

    fn kept_on_disk(world: &World) -> bool {
        world.disk.as_ref().is_some_and(|disk| disk["restore"].as_array().is_some_and(|restore| restore.iter().any(|entry| entry[0] == 5)))
    }

    struct Lid(Arc<Mutex<World>>);

    impl crate::http::Http for Lid {
        fn send(&self, request: &crate::http::Request) -> Result<crate::http::Response, crate::http::Failure> {
            use crate::http::Method;
            let (method, body) = match request.method {
                Method::Get => ("GET", ""),
                Method::Post(body) => ("POST", body),
                Method::Put(body) => ("PUT", body),
            };
            let path = request.url.trim_start_matches("http://lidarr.test/api/v1/").to_string();
            let entry = |item: &str, name: &str, allowed: bool| serde_json::json!({ item: { "id": 1, "name": name }, "allowed": allowed });
            let profile = |id: i64, name: &str, singles: bool| {
                serde_json::json!({ "id": id, "name": name,
                    "primaryAlbumTypes": [entry("albumType", "Album", true), entry("albumType", "Single", singles), entry("albumType", "EP", singles)],
                    "secondaryAlbumTypes": [entry("albumType", "Studio", true)],
                    "releaseStatuses": [entry("releaseStatus", "Official", true)] })
            };
            let answer = match (method, path.as_str()) {
                ("GET", "artist") => r#"[{"id":5,"artistName":"Someone","foreignArtistId":"ar1"}]"#.to_string(),
                ("GET", "metadataprofile") => serde_json::json!([profile(1, "Standard", false), profile(2, crate::lidarr::tests::SINGLES, true)]).to_string(),
                ("GET", "artist/5") => {
                    let setting = self.0.lock().unwrap().setting.clone();
                    wait(&self.0, "read");
                    serde_json::json!({ "id": 5, "metadataProfileId": 1, "monitorNewItems": setting, "monitored": true }).to_string()
                }
                ("PUT", "artist/5") => {
                    let off = body.contains(r#""monitorNewItems":"none""#);
                    if off {
                        wait(&self.0, "off");
                    }
                    let mut world = self.0.lock().unwrap();
                    if off && !kept_on_disk(&world) {
                        let who = std::thread::current().name().unwrap_or("").to_string();
                        world.broken.push(format!("{who} turned it off with no way back on the disk"));
                    }
                    world.setting = if off { "none".into() } else { "all".into() };
                    "{}".to_string()
                }
                ("GET", "album?artistId=5") => r#"[{"id":9,"title":"A Single","foreignAlbumId":"al1","albumType":"Single"}]"#.to_string(),
                ("GET", "command") => "[]".to_string(),
                _ => "{}".to_string(),
            };
            Ok(crate::http::Response::new(200, Vec::new(), move || Ok(answer)))
        }

        fn pause(&self, _: Duration) -> Result<(), crate::http::Failure> {
            Err(crate::http::Failure::Stopped)
        }
    }

    struct Disk(Arc<Mutex<World>>);

    impl Desk for Disk {
        fn keep(&self, batch: Option<Vec<u8>>, _: bool) {
            self.0.lock().unwrap().disk = batch.and_then(|json| serde_json::from_slice(&json).ok());
        }

        fn keep_checked(&self, batch: Vec<u8>) -> bool {
            wait(&self.0, "keep");
            let mut world = self.0.lock().unwrap();
            if std::mem::take(&mut world.first_fails) {
                return false;
            }
            world.disk = serde_json::from_slice(&batch).ok();
            true
        }

        fn now(&self) -> i64 {
            0
        }

        fn singles(&self) -> bool {
            true
        }
    }

    /// Two requests for one artist's single, by themselves, on threads of their own: A, and B
    /// a moment after.
    fn two_at_once(world: World) -> Arc<Mutex<World>> {
        let world = Arc::new(Mutex::new(World { setting: "all".into(), ..world }));
        let job = Job::new(Disk(world.clone()));
        let lidarr = Lidarr::new("lidarr.test", "key", None, Arc::new(Lid(world.clone())));
        let single = crate::lidarr::tests::single();
        std::thread::scope(|scope| {
            for (name, after) in [("A", 0), ("B", 30)] {
                let (job, lidarr, single) = (&job, &lidarr, single.clone());
                std::thread::Builder::new()
                    .name(name.into())
                    .spawn_scoped(scope, move || {
                        std::thread::sleep(Duration::from_millis(after));
                        let _ = lidarr.request(&single, job);
                    })
                    .unwrap();
            }
        });
        assert!(job.with(|batch| batch.restore.is_empty()), "{:?}", job.with(|batch| batch.restore.clone()));
        world
    }

    #[test]
    fn nobody_is_told_a_way_back_is_kept_that_isnt_on_the_disk() {
        // A's way back can't be written (and takes a while to fail); B, a moment after, must
        // not take A's entry, not yet on the disk and about to go, for its own.
        let waits = HashMap::from([(("A".to_string(), "keep"), 150)]);
        let world = two_at_once(World { waits, first_fails: true, ..Default::default() });
        let world = world.lock().unwrap();
        assert!(world.broken.is_empty(), "{:?}", world.broken);
        assert_eq!(world.setting, "all");
    }

    #[test]
    fn a_request_crosses_off_only_its_own_way_back_and_none_is_crossed_off_under_another() {
        // Both read "all". A turns it off, puts it back and crosses it off while B, which found
        // A's entry, is still on its way to turning it off: then B's change has no way back.
        let waits = HashMap::from([(("A".to_string(), "off"), 100), (("B".to_string(), "off"), 400)]);
        let world = two_at_once(World { waits, ..Default::default() });
        let world = world.lock().unwrap();
        assert!(world.broken.is_empty(), "{:?}", world.broken);
        assert_eq!(world.setting, "all");
        // A way back the batch had from before a request is the batch's: the request doesn't
        // cross it off.
        let world = Arc::new(Mutex::new(World { setting: "all".into(), ..Default::default() }));
        let job = Job::new(Disk(world.clone()));
        assert!(job.load(br#"{"wants":[],"restore":[[5,"all"]],"stopped":"","proof":null}"#));
        let lidarr = Lidarr::new("lidarr.test", "key", None, Arc::new(Lid(world.clone())));
        std::thread::scope(|scope| {
            scope.spawn(|| lidarr.request(&crate::lidarr::tests::single(), &job));
        });
        assert_eq!(job.with(|batch| batch.restore.clone()), [(5, "all".to_string())]);
    }

    #[test]
    fn a_setting_that_cant_be_written_down_first_is_not_changed_on_lidarr() {
        // The same single as below; but what is to be put back can't be made sure of on the
        // disk before Lidarr is changed: so Lidarr isn't, and the batch says why.
        let kept = Arc::new(Mutex::new(None));
        let job = Job::new(Unsure(kept.clone()));
        let changed = Arc::new(Stopped(AtomicBool::new(false)));
        let http: Arc<dyn crate::http::Http> = changed.clone();
        let lidarr = Lidarr::new("lidarr.test", "key", None, http.clone());
        let musicbrainz = MusicBrainz::new(http.clone(), crate::listenbrainz::ListenBrainz::new(http));
        job.start(vec![Want { artist: "Someone".into(), album: "A Single".into(), ..Default::default() }]);
        assert!(job.begin());
        job.work_through(&lidarr, &musicbrainz, &|_| {});
        assert!(!changed.0.load(Ordering::SeqCst), "Lidarr was changed with nothing written down to put it back");
        let written: serde_json::Value = serde_json::from_slice(kept.lock().unwrap().as_ref().unwrap()).unwrap();
        assert_eq!(written["restore"], serde_json::json!([]));
        assert_eq!(written["wants"][0]["state"], "Failed");
        assert!(written["wants"][0]["reason"].as_str().unwrap().contains("couldn't be written down"), "{}", written["wants"][0]["reason"]);
    }

    #[test]
    fn a_setting_to_put_back_is_kept_however_the_wait_ends() {
        // A single of an artist whose profile lists albums only: their "new releases" setting
        // is turned off while Lidarr reads their singles in. Every wait for Lidarr is cut
        // short; what's to be put back is written down, to be put back once Lidarr is done.
        let kept = Arc::new(Mutex::new(None));
        let job = Job::new(Kept(kept.clone()));
        let http: Arc<dyn crate::http::Http> = Arc::new(Stopped(AtomicBool::new(false)));
        let lidarr = Lidarr::new("lidarr.test", "key", None, http.clone());
        let musicbrainz = MusicBrainz::new(http.clone(), crate::listenbrainz::ListenBrainz::new(http));
        job.start(vec![Want { artist: "Someone".into(), album: "A Single".into(), ..Default::default() }]);
        assert!(job.begin());
        job.work_through(&lidarr, &musicbrainz, &|_| {});
        let written: serde_json::Value = serde_json::from_slice(kept.lock().unwrap().as_ref().unwrap()).unwrap();
        assert_eq!(written["restore"], serde_json::json!([[5, "all"]]));
        assert_eq!(written["stopped"], STOPPED);
        assert!(job.unfinished());
        // Read back, it is taken up; cut short, or empty, it is not, and the app is told so.
        let again = Job::new(Nowhere);
        let written = kept.lock().unwrap().clone().unwrap();
        assert!(!again.load(b"") && !again.load(&written[..written.len() / 2]) && !again.unfinished());
        assert!(again.load(&written) && again.unfinished());
    }
}
