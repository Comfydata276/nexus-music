//! Writes files in the background, in the order they were asked for.
//!
//! A file written while nobody waits for the answer (the queue, Recently played, what waits
//! to be sent, the lists of downloads, a play added to the log, the settings saved in
//! passing) is handed to the one thread here, so that the window never waits for the disk:
//! a slow disk would otherwise be a pause in everything on screen, and a song starting
//! would wait on it. **One writer for each file**: a write whose answer someone does wait
//! for (`write_checked`, `append_checked`) is made here too, in its turn, and the caller
//! waits for that one answer.
//!
//! It tells the truth. A write that fails stays due and is tried again, in its turn, until
//! it is done or a newer one for the file replaces it; `flush` says whether everything is
//! done; and a file that has failed three times running is said once (`on_failing`).
//! When several versions of one file are waiting, only the newest is built and written.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

type Make = Box<dyn FnOnce() -> Option<Vec<u8>> + Send>;
/// How lines are added to the end of a file (the log of plays' own rule: `plays::append`).
pub type Add = fn(&Path, &str) -> std::io::Result<()>;

/// What is to be on the disk for a file.
enum What {
    /// The whole of it.
    Whole(Vec<u8>),
    /// Lines to add to its end, by the file's own rule.
    Lines(String, Add),
}

enum Job {
    Write(PathBuf, Make),
    Append(PathBuf, String, Add),
    Remove(PathBuf),
    /// Written in its turn, and the answer sent back: done, or not done and dropped.
    Checked(PathBuf, What, Sender<std::io::Result<()>>),
    /// Answered, once everything asked for before it has been tried, with whether all of
    /// it is done (or all of it for one file).
    Sync(Option<PathBuf>, Sender<bool>),
}

/// A write that has not been done yet.
struct Due {
    path: PathBuf,
    what: What,
}

/// How long a write that failed waits before it is tried again (sooner if more come).
const AGAIN: Duration = Duration::from_secs(5);
/// How many failures running before the person is told.
const TELL_AFTER: u32 = 3;

pub struct Saver {
    jobs: Sender<Job>,
}

static SAVER: LazyLock<Saver> = LazyLock::new(Saver::start);
static FAILING: OnceLock<fn(&Path)> = OnceLock::new();

/// The app's one saver.
pub fn saver() -> &'static Saver {
    &SAVER
}

/// What to do, once, when a file has failed to be written three times running: say so
/// where the person will see it. Set once, by the window.
pub fn on_failing(tell: fn(&Path)) {
    let _ = FAILING.set(tell);
}

impl Saver {
    fn start() -> Saver {
        let (jobs, waiting) = mpsc::channel();
        let _ = std::thread::Builder::new().name("saver".into()).spawn(move || work(waiting));
        Saver { jobs }
    }

    /// Replaces the file at `path` with what `make` returns. `make` runs on the saver's
    /// thread, so the work of building the contents is off the caller's hands too.
    pub fn write(&self, path: PathBuf, make: impl FnOnce() -> Option<Vec<u8>> + Send + 'static) {
        let _ = self.jobs.send(Job::Write(path, Box::new(make)));
    }

    /// Adds `text` to the end of the file at `path`, by `add`, after whatever was asked for
    /// that file before.
    pub fn append(&self, path: PathBuf, text: String, add: Add) {
        let _ = self.jobs.send(Job::Append(path, text, add));
    }

    pub fn remove(&self, path: PathBuf) {
        let _ = self.jobs.send(Job::Remove(path));
    }

    /// Writes the whole file now, in its turn, and says whether it is on the disk. One that
    /// isn't is dropped, not tried again: the caller goes on as if it had not been asked.
    pub fn write_checked(&self, path: PathBuf, bytes: Vec<u8>) -> std::io::Result<()> {
        self.checked(path, What::Whole(bytes))
    }

    /// Adds `text` (which may be nothing) to the end of the file, after everything asked
    /// for that file before, and says whether all of it is on the disk.
    pub fn append_checked(&self, path: PathBuf, text: String, add: Add) -> std::io::Result<()> {
        self.checked(path, What::Lines(text, add))
    }

    fn checked(&self, path: PathBuf, what: What) -> std::io::Result<()> {
        let (answer, wait) = mpsc::channel();
        self.jobs.send(Job::Checked(path, what, answer)).map_err(|_| std::io::Error::other("the saver has stopped"))?;
        wait.recv().unwrap_or_else(|_| Err(std::io::Error::other("the saver has stopped")))
    }

    /// Waits until everything asked for so far has been tried, and says whether it is all
    /// on the disk. For when the app is closing, or a file is about to be read back.
    pub fn flush(&self) -> bool {
        self.sync(None)
    }

    /// `flush`, saying whether everything asked for one file is on the disk.
    #[cfg(test)]
    pub fn flush_file(&self, path: &Path) -> bool {
        self.sync(Some(path.to_path_buf()))
    }

    fn sync(&self, only: Option<PathBuf>) -> bool {
        let (done, wait) = mpsc::channel();
        self.jobs.send(Job::Sync(only, done)).is_ok() && wait.recv_timeout(Duration::from_secs(5)).unwrap_or(false)
    }
}

fn work(waiting: Receiver<Job>) {
    let mut due: Vec<Due> = Vec::new();
    let mut failures: HashMap<PathBuf, u32> = HashMap::new();
    loop {
        let first = if due.is_empty() {
            match waiting.recv() {
                Ok(job) => Some(job),
                Err(_) => return,
            }
        } else {
            match waiting.recv_timeout(AGAIN) {
                Ok(job) => Some(job),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };
        // Take everything that's waiting, and drop what a later job for the same file undoes.
        let mut batch: Vec<Job> = first.into_iter().collect();
        batch.extend(waiting.try_iter());
        let replaces = |job: &Job| match job {
            Job::Write(path, _) | Job::Remove(path) => Some(path.clone()),
            _ => None,
        };
        let paths: Vec<Option<PathBuf>> = batch.iter().map(replaces).collect();
        for (index, job) in batch.into_iter().enumerate() {
            let superseded = paths[index].is_some() && paths[index + 1..].contains(&paths[index]);
            match job {
                _ if superseded => {}
                Job::Write(path, make) => {
                    if let Some(bytes) = make() {
                        due.retain(|due| due.path != path);
                        due.push(Due { path, what: What::Whole(bytes) });
                    }
                }
                Job::Append(path, text, add) => due.push(Due { path, what: What::Lines(text, add) }),
                Job::Remove(path) => {
                    due.retain(|due| due.path != path);
                    let _ = std::fs::remove_file(&path);
                }
                Job::Checked(path, what, answer) => {
                    let done = match what {
                        // Newer than anything due for the file: once it is on the disk, that is.
                        What::Whole(bytes) => crate::config::write_surely(&path, &bytes).inspect(|()| due.retain(|due| due.path != path)),
                        // After what is due for the file, in order.
                        What::Lines(text, add) => {
                            try_due(&mut due, &mut failures, Some(&path));
                            if due.iter().any(|due| due.path == path) {
                                Err(std::io::Error::other("what was to be written before it couldn't be"))
                            } else if text.is_empty() {
                                Ok(())
                            } else {
                                add(&path, &text)
                            }
                        }
                    };
                    note(&mut failures, &path, done.is_ok());
                    let _ = answer.send(done);
                }
                Job::Sync(only, done) => {
                    try_due(&mut due, &mut failures, None);
                    let _ = done.send(!due.iter().any(|due| only.as_ref().is_none_or(|only| due.path == *only)));
                }
            }
        }
        try_due(&mut due, &mut failures, None);
    }
}

/// Tries what is due (for one file, or all), in order. A file whose write fails has the
/// rest of its writes left for later too, so that they are never done out of turn.
fn try_due(due: &mut Vec<Due>, failures: &mut HashMap<PathBuf, u32>, only: Option<&Path>) {
    let mut stuck: Vec<PathBuf> = Vec::new();
    due.retain(|one| {
        if only.is_some_and(|only| one.path != only) || stuck.contains(&one.path) {
            return true;
        }
        let done = match &one.what {
            What::Whole(bytes) => crate::config::write_surely(&one.path, bytes),
            What::Lines(text, add) => add(&one.path, text),
        };
        note(failures, &one.path, done.is_ok());
        if done.is_err() {
            stuck.push(one.path.clone());
        }
        done.is_err()
    });
}

/// What a file of the app's is, in words for the person (`on_failing`).
pub fn describe(path: &Path) -> String {
    let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let what = match name.as_str() {
        "settings.json" => "Your settings",
        "history.json" => "Recently played",
        "queue.json" | "queue-place.json" => "The queue",
        "playlist-added.json" => "When songs were added to playlists",
        "asked.json" => "The list of albums asked for",
        "asking.json" => "The batch of albums being asked for",
        "plays.log" | "plays.log.new" => "Your plays",
        "crash.seen" => "A note of what has been said",
        _ if name.starts_with("pending-") => "Likes, ratings and plays waiting to be sent",
        _ if name.starts_with("index-") => "The list of downloaded songs",
        _ if name.starts_with("collections-") => "The playlists kept downloaded",
        _ => return name,
    };
    what.to_string()
}

/// Counts a file's failures running, and tells the person when there have been three.
fn note(failures: &mut HashMap<PathBuf, u32>, path: &Path, done: bool) {
    if done {
        failures.remove(path);
        return;
    }
    let count = failures.entry(path.to_path_buf()).or_insert(0);
    *count += 1;
    if *count == TELL_AFTER {
        if let Some(tell) = FAILING.get() {
            tell(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nexus-saver-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Adds lines to the end of a file, plainly.
    fn add(path: &Path, text: &str) -> std::io::Result<()> {
        use std::io::Write as _;
        std::fs::OpenOptions::new().create(true).append(true).open(path)?.write_all(text.as_bytes())
    }

    /// The files said to be failing (see `on_failing`), for the tests that make some fail.
    static TOLD: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

    fn told(path: &Path) -> usize {
        on_failing(|path| TOLD.lock().unwrap().push(path.to_path_buf()));
        TOLD.lock().unwrap().iter().filter(|told| *told == path).count()
    }

    #[test]
    fn writes_the_newest_and_keeps_the_order() {
        let dir = scratch("order");
        let (a, b) = (dir.join("a.json"), dir.join("b.json"));
        let saver = saver();
        // The first write holds the thread up long enough for the rest to queue behind it.
        saver.write(a.clone(), || {
            std::thread::sleep(Duration::from_millis(150));
            Some(b"first".to_vec())
        });
        let built = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        for text in ["second", "third", "fourth"] {
            let built = built.clone();
            saver.write(a.clone(), move || {
                built.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Some(text.as_bytes().to_vec())
            });
        }
        saver.write(b.clone(), || Some(b"kept".to_vec()));
        saver.remove(b.clone());
        assert!(saver.flush_file(&a));
        assert_eq!(std::fs::read(&a).unwrap(), b"fourth");
        // The versions in between were never built, let alone written.
        assert_eq!(built.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(!b.exists());
        // A removal followed by a write leaves the file there.
        saver.remove(a.clone());
        saver.write(a.clone(), || Some(b"again".to_vec()));
        assert!(saver.flush_file(&a));
        assert_eq!(std::fs::read(&a).unwrap(), b"again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_that_fails_stays_due_and_is_said_once_and_done_when_it_can_be() {
        let dir = scratch("fails");
        // Its folder isn't there: every try fails, until it is.
        let path = dir.join("not yet").join("history.json");
        let saver = saver();
        told(&path);
        saver.write(path.clone(), || Some(b"the newest".to_vec()));
        for _ in 0..4 {
            assert!(!saver.flush() && !saver.flush_file(&path), "said to be done when it was not");
        }
        // Said once, however many more times it fails.
        assert_eq!(told(&path), 1);
        // Tried again, it is done; and then it is said to be.
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert!(saver.flush_file(&path));
        assert_eq!(std::fs::read(&path).unwrap(), b"the newest");
        assert_eq!(told(&path), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_checked_write_says_whether_it_was_done_and_one_that_was_not_is_never_done_later() {
        let dir = scratch("checked");
        let path = dir.join("not yet").join("settings.json");
        let saver = saver();
        // In passing, while the folder isn't there: due. Then a checked one: not done, and
        // dropped, so that what its caller took back is never written later.
        saver.write(path.clone(), || Some(b"in passing".to_vec()));
        assert!(saver.write_checked(path.clone(), b"chosen".to_vec()).is_err());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert!(saver.flush_file(&path));
        assert_eq!(std::fs::read(&path).unwrap(), b"in passing");
        // Done, it replaces what was due for the file before it.
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        saver.write(path.clone(), || Some(b"older".to_vec()));
        assert!(!saver.flush_file(&path));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert!(saver.write_checked(path.clone(), b"chosen".to_vec()).is_ok());
        assert!(saver.flush_file(&path));
        assert_eq!(std::fs::read(&path).unwrap(), b"chosen");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lines_are_added_in_order_and_a_check_waits_for_those_before_it() {
        let dir = scratch("lines");
        let path = dir.join("not yet").join("plays.log");
        let saver = saver();
        saver.append(path.clone(), "x\n".into(), add);
        saver.append(path.clone(), "y\n".into(), add);
        // Those before it can't be added yet: nor is it, and it says so.
        assert!(saver.append_checked(path.clone(), "z\n".into(), add).is_err());
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        assert!(saver.append_checked(path.clone(), "w\n".into(), add).is_ok());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\ny\nw\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_written_in_passing_waits_for_the_disk_on_the_window_thread() {
        let dir = scratch("window");
        crate::config::mark_window_thread();
        let settings = dir.join("settings.json");
        crate::config::Settings { theme: "Violet".into(), ..Default::default() }.save(&settings);
        crate::config::save_history(&dir.join("history.json"), &[]);
        let mut asked = crate::asked::AskedList::load(&dir.join("asked.json"));
        asked.add(crate::asked::Asked { title: "Cradlesong".into(), artist: "Rob Thomas".into(), ..Default::default() });
        let mut added = crate::added::Added::load(dir.join("playlist-added.json"));
        added.record("p1", &["s1".to_string()], 1_700_000_000);
        assert_eq!(crate::config::window_flushes(), 0, "a file was flushed on the window's thread");
        // All of it is written, on the saver's thread.
        saver().flush();
        for name in ["settings.json", "history.json", "asked.json", "playlist-added.json"] {
            assert!(dir.join(name).exists(), "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
