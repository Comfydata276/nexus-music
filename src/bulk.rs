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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, OnceLock};

use nexus_core::bulk::{Desk, Job};

use crate::lidarr::Lidarr;

pub use nexus_core::bulk::{Event, Summary, Want};

/// Where the batch is written down, and whether it may be: not while a file is there that
/// couldn't be read, or couldn't be moved aside (`take_up`).
struct Place {
    path: PathBuf,
    held: AtomicBool,
}

static PLACE: OnceLock<Place> = OnceLock::new();

impl Place {
    fn new(path: &Path) -> Place {
        Place { path: path.to_path_buf(), held: AtomicBool::new(false) }
    }

    /// Whole, or the batch as it was: in passing, or (while the batch holds a way back)
    /// flushed.
    fn keep(&self, json: Option<Vec<u8>>, flushed: bool) {
        if self.held.load(Ordering::SeqCst) {
            return;
        }
        match json {
            None => drop(std::fs::remove_file(&self.path)),
            Some(json) if flushed => drop(crate::config::write_surely(&self.path, &json)),
            Some(json) => drop(crate::config::write_in_passing(&self.path, &json)),
        }
    }

    /// What must be on the disk before Lidarr is changed: written, and said whether it is.
    fn keep_checked(&self, json: Vec<u8>) -> bool {
        !self.held.load(Ordering::SeqCst) && crate::config::write_surely(&self.path, &json).is_ok()
    }

    /// Takes up the batch written down here with `load`, and says what the person is to be
    /// told about it. **A file that can't be read is never taken for no batch:** one that
    /// can't be read just now is left as it is, nothing is written over it in this run, and
    /// it is read again when the app next starts. One that reads and isn't a batch (cut short
    /// by an earlier version, or by a power cut, or from a version that wrote it otherwise)
    /// is moved aside under a name of its own, and only then may a batch be written here; if
    /// it can't be moved, it is left, and nothing is written over it. An empty one, which is
    /// all such a cut leaves, is removed.
    fn take_up(&self, load: impl FnOnce(&[u8]) -> bool) -> String {
        let written = match std::fs::read(&self.path) {
            Ok(written) => written,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return String::new(),
            Err(_) => {
                self.held.store(true, Ordering::SeqCst);
                return NOT_READ.into();
            }
        };
        if load(&written) {
            return String::new();
        }
        if written.iter().all(u8::is_ascii_whitespace) {
            if std::fs::remove_file(&self.path).is_err() {
                self.held.store(true, Ordering::SeqCst);
                return NOT_READ.into();
            }
            return FOUND_EMPTY.into();
        }
        match crate::config::set_aside(&self.path) {
            Some(aside) => format!("{FOUND_DAMAGED} The file has been kept as {}.", aside.file_name().unwrap_or_default().to_string_lossy()),
            None => {
                self.held.store(true, Ordering::SeqCst);
                NOT_MOVED.into()
            }
        }
    }
}

/// The app's part in the job.
struct Windows;

impl Desk for Windows {
    fn keep(&self, json: Option<Vec<u8>>, flushed: bool) {
        if let Some(place) = PLACE.get() {
            place.keep(json, flushed);
        }
    }

    fn keep_checked(&self, json: Vec<u8>) -> bool {
        PLACE.get().is_some_and(|place| place.keep_checked(json))
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

/// Where a request made by itself writes down the way back from a change to Lidarr before it
/// makes it: the batch's file, so that a run stopped before putting it back has it put back
/// when the batch is next carried on (`nexus_core::lidarr::WayBack`).
pub fn way_back() -> &'static dyn nexus_core::lidarr::WayBack {
    &*JOB
}

/// Reads the batch that was under way when the app last closed, if there was one.
pub fn load(path: &Path) {
    let place = PLACE.get_or_init(|| Place::new(path));
    let trouble = place.take_up(|written| JOB.load(written));
    if let Ok(mut said) = TROUBLE.write() {
        *said = trouble;
    }
}

/// What the Requests page says about the file the batch is kept in, if anything.
static TROUBLE: std::sync::RwLock<String> = std::sync::RwLock::new(String::new());

pub fn trouble() -> String {
    TROUBLE.read().map(|said| said.clone()).unwrap_or_default()
}

const FOUND_EMPTY: &str = "A batch of albums being asked for was found empty when the app started (it was stopped while saving it), so it hasn't been carried on. If it had turned off “new releases” for an artist while their singles were read in, that hasn't been put back: look at those artists in Lidarr.";
const FOUND_DAMAGED: &str = "A batch of albums being asked for couldn't be read back when the app started, so it hasn't been carried on. If it had turned off “new releases” for an artist while their singles were read in, that hasn't been put back: look at those artists in Lidarr.";
const NOT_READ: &str = "A batch of albums being asked for couldn't be read when the app started. It has been left as it is, and nothing is saved over it; it is read again when the app next starts. Until then, a new batch can be asked for, but nothing that would have to be put back on Lidarr is changed.";
const NOT_MOVED: &str = "A batch of albums being asked for couldn't be read back when the app started, and couldn't be moved aside. It has been left as it is, and nothing is saved over it; it is looked at again when the app next starts. Until then, a new batch can be asked for, but nothing that would have to be put back on Lidarr is changed.";

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nexus-asking-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names_in(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    const BATCH: &[u8] = br#"{"wants":[],"restore":[[5,"all"]],"stopped":"","proof":null}"#;

    #[test]
    fn a_batch_found_empty_or_damaged_is_said_and_kept_aside_under_a_name_of_its_own() {
        let dir = scratch("found");
        let place = Place::new(&dir.join("asking.json"));
        let job = Job::new(Windows);
        let read = |written: &[u8]| job.load(written);
        // Nothing written down: nothing to say.
        assert_eq!(place.take_up(read), "");
        // A batch, written down whole, is taken up.
        std::fs::write(&place.path, BATCH).unwrap();
        assert_eq!(place.take_up(read), "");
        assert!(job.unfinished(""));
        // What 0.40.10 could leave after a power cut: an empty file, or the batch cut short;
        // twice, and each kept under a name of its own, which the page names.
        std::fs::write(&place.path, "").unwrap();
        assert_eq!(place.take_up(read), FOUND_EMPTY);
        let mut said = Vec::new();
        for _ in 0..2 {
            std::fs::write(&place.path, &BATCH[..30]).unwrap();
            said.push(place.take_up(read));
        }
        let kept: Vec<String> = names_in(&dir).into_iter().filter(|name| name.starts_with("asking.damaged-")).collect();
        assert_eq!(kept.len(), 2);
        for said in &said {
            let named = kept.iter().find(|kept| said.contains(&format!("kept as {kept}."))).unwrap_or_else(|| panic!("{said}"));
            assert!(said.starts_with(FOUND_DAMAGED));
            assert_eq!(std::fs::read(dir.join(named)).unwrap(), &BATCH[..30]);
        }
        assert!(said[0] != said[1]);
        assert!(!place.path.exists() && !place.held.load(Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_batch_that_cant_be_read_is_left_and_nothing_is_saved_over_it() {
        let dir = scratch("unread");
        // (A folder where the file is read is a file that can't be read, on every system.)
        let place = Place::new(&dir.join("asking.json"));
        std::fs::create_dir(&place.path).unwrap();
        let job = Job::new(Windows);
        assert_eq!(place.take_up(|written| job.load(written)), NOT_READ);
        // Nothing is written over it, in passing or surely: what would need putting back on
        // Lidarr isn't changed.
        place.keep(Some(BATCH.to_vec()), false);
        assert!(!place.keep_checked(BATCH.to_vec()));
        assert!(place.path.is_dir());
        // A batch that reads, as before: written, and surely.
        std::fs::remove_dir(&place.path).unwrap();
        let place = Place::new(&dir.join("asking.json"));
        assert!(place.keep_checked(BATCH.to_vec()));
        assert_eq!(std::fs::read(&place.path).unwrap(), BATCH);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_batch_that_cant_be_moved_aside_is_left_and_nothing_is_saved_over_it() {
        let dir = scratch("unmoved");
        let place = Place::new(&dir.join("asking.json"));
        std::fs::write(&place.path, &BATCH[..30]).unwrap();
        let pinned = pin(&dir, &place.path);
        if crate::config::set_aside(&place.path).is_some() {
            // (Whoever runs this may move anything: nothing here can be pinned.)
            return;
        }
        let job = Job::new(Windows);
        assert_eq!(place.take_up(|written| job.load(written)), NOT_MOVED);
        place.keep(Some(BATCH.to_vec()), false);
        assert!(!place.keep_checked(BATCH.to_vec()));
        drop(pinned);
        assert_eq!(std::fs::read(&place.path).unwrap(), &BATCH[..30]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Keeps a file from being moved, until dropped: on Windows it is held open, shared for
    /// reading only; elsewhere its folder can't be written.
    struct Pinned(PathBuf, #[allow(dead_code)] Option<std::fs::File>);

    fn pin(dir: &Path, file: &Path) -> Pinned {
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            Pinned(dir.to_path_buf(), Some(std::fs::OpenOptions::new().read(true).share_mode(1).open(file).unwrap()))
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = file;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
            Pinned(dir.to_path_buf(), None)
        }
    }

    impl Drop for Pinned {
        fn drop(&mut self) {
            #[cfg(not(windows))]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
        }
    }
}

/// Hands over albums to ask for. If a batch is still under way they join it; otherwise they
/// replace what's left of the last one.
pub fn start(wants: Vec<Want>) {
    JOB.start(&here(), wants)
}

/// Whether there's a batch with something still to do with the Lidarr set up now. A way back
/// written for another Lidarr isn't: it is left for that one (`nexus_core::bulk::Job::unfinished`).
pub fn unfinished() -> bool {
    JOB.unfinished(&here())
}

/// The address of the Lidarr set up now, as its way back is written under (empty if none).
fn here() -> String {
    crate::lidarr::current().map(|lidarr| lidarr.address().to_string()).unwrap_or_default()
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
