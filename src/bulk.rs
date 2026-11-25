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
//! The job's rules are in the library (`nexus_core::bulk`), for the Android app to use too.
//! This is the Windows app's end: the one job, the file it's written down in, the thread it
//! runs on, and the threads it looks albums up on.

use std::path::PathBuf;
use std::sync::{LazyLock, OnceLock};

use nexus_core::bulk::{Desk, Job};

use crate::lidarr::Lidarr;

pub use nexus_core::bulk::{Event, Summary, Want};

static PLACE: OnceLock<PathBuf> = OnceLock::new();

/// The app's part in the job.
struct Windows;

impl Desk for Windows {
    fn keep(&self, json: Option<Vec<u8>>) {
        let Some(path) = PLACE.get() else { return };
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

    fn now(&self) -> i64 {
        chrono::Utc::now().timestamp()
    }

    fn singles(&self) -> bool {
        crate::lidarr::singles()
    }

    /// Albums are looked up three at a time, each on a thread of its own.
    fn together(&self, count: usize, work: &(dyn Fn() + Sync)) {
        std::thread::scope(|scope| {
            for _ in 0..count {
                scope.spawn(work);
            }
        });
    }
}

static JOB: LazyLock<Job> = LazyLock::new(|| Job::new(Windows));

/// Reads the batch that was under way when the app last closed, if there was one.
pub fn load(path: &std::path::Path) {
    let _ = PLACE.set(path.to_path_buf());
    if let Ok(written) = std::fs::read(path) {
        JOB.load(&written);
    }
}

/// Hands over albums to ask for. If a batch is still under way they join it; otherwise they
/// replace what's left of the last one.
pub fn start(wants: Vec<Want>) {
    JOB.start(wants)
}

/// Whether there's a batch with something still to do.
pub fn unfinished() -> bool {
    JOB.unfinished()
}

pub fn running() -> bool {
    JOB.running()
}

/// Stops asking for more. What Lidarr has already been asked for stays asked for, and is
/// still checked.
pub fn stop() {
    JOB.stop()
}

/// Has another go at what couldn't be asked for last time for a reason that may have passed.
/// Says whether there was anything to have another go at.
pub fn again() -> bool {
    JOB.again()
}

/// Forgets a batch that has ended: its results have been read.
pub fn clear() {
    JOB.clear()
}

pub fn summary() -> Summary {
    JOB.summary()
}

/// The albums of the batch that couldn't be asked for, with why.
pub fn missed() -> Vec<Want> {
    JOB.missed()
}

/// Works through the batch on a thread of its own, if it isn't being worked through already.
pub fn run(lidarr: Lidarr, tell: impl Fn(Event) + Send + Sync + 'static) {
    if !JOB.begin() {
        return;
    }
    let started = std::thread::Builder::new().name("bulk-requests".into()).spawn(move || JOB.work_through(&lidarr, crate::musicbrainz::service(), &tell));
    if started.is_err() {
        JOB.not_started();
    }
}
