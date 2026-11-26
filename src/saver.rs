//! Writes the queue's two files in the background, in the order they were asked for.
//!
//! Saving the queue means turning thousands of songs into text and putting it on disk. Done
//! on the window's thread that is a pause in everything on screen, so it is handed to one
//! thread here instead. When several versions of the same file are waiting, only the newest
//! is written. Nothing else is written here: every other file is written where it changes,
//! by the thread that changes it (`docs/FILES.md`).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

type Make = Box<dyn FnOnce() -> Option<Vec<u8>> + Send>;

enum Job {
    Write(PathBuf, Make),
    Remove(PathBuf),
    /// Answered once everything asked for before it has been tried: with whether the last
    /// write of every file was done.
    Sync(Sender<bool>),
}

pub struct Saver {
    jobs: Sender<Job>,
}

impl Saver {
    pub fn new() -> Saver {
        let (jobs, waiting) = mpsc::channel();
        let _ = std::thread::Builder::new().name("saver".into()).spawn(move || work(waiting));
        Saver { jobs }
    }

    /// Replaces the file at `path` with what `make` returns. `make` runs on the saver's
    /// thread, so the work of building the contents is off the caller's hands too.
    pub fn write(&self, path: PathBuf, make: impl FnOnce() -> Option<Vec<u8>> + Send + 'static) {
        let _ = self.jobs.send(Job::Write(path, Box::new(make)));
    }

    pub fn remove(&self, path: PathBuf) {
        let _ = self.jobs.send(Job::Remove(path));
    }

    /// Waits until everything asked for so far has been tried, and says whether it is on the
    /// disk: false if the last write of any file failed (it is not said to be done), or if
    /// five seconds went by first. For when the app is closing, or a file is about to be read
    /// back.
    pub fn flush(&self) -> bool {
        let (done, wait) = mpsc::channel();
        self.jobs.send(Job::Sync(done)).is_ok() && wait.recv_timeout(Duration::from_secs(5)).unwrap_or(false)
    }
}

fn work(waiting: Receiver<Job>) {
    // The files whose last write failed: until a later one of each is done.
    let mut failed: HashSet<PathBuf> = HashSet::new();
    while let Ok(first) = waiting.recv() {
        // Take everything that's waiting, and drop what a later job for the same file undoes.
        let mut batch = vec![first];
        batch.extend(waiting.try_iter());
        let paths: Vec<Option<PathBuf>> = batch
            .iter()
            .map(|job| match job {
                Job::Write(path, _) | Job::Remove(path) => Some(path.clone()),
                Job::Sync(_) => None,
            })
            .collect();
        for (index, job) in batch.into_iter().enumerate() {
            let superseded = paths[index].is_some() && paths[index + 1..].contains(&paths[index]);
            match job {
                _ if superseded => {}
                Job::Write(path, make) => {
                    if let Some(bytes) = make() {
                        // In passing, as 0.40.10 wrote them (`config::Durable::InPassing`).
                        match crate::config::write_in_passing(&path, &bytes) {
                            Ok(()) => failed.remove(&path),
                            Err(_) => failed.insert(path),
                        };
                    }
                }
                Job::Remove(path) => {
                    match std::fs::remove_file(&path) {
                        Err(error) if error.kind() != std::io::ErrorKind::NotFound => failed.insert(path),
                        _ => failed.remove(&path),
                    };
                }
                Job::Sync(done) => {
                    let _ = done.send(failed.is_empty());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_the_newest_and_keeps_the_order() {
        let dir = std::env::temp_dir().join(format!("nexus-saver-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.json"), dir.join("b.json"));
        let saver = Saver::new();
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
        assert!(saver.flush());
        assert_eq!(std::fs::read(&a).unwrap(), b"fourth");
        // The versions in between were never built, let alone written.
        assert_eq!(built.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(!b.exists());
        // A removal followed by a write leaves the file there.
        saver.remove(a.clone());
        saver.write(a.clone(), || Some(b"again".to_vec()));
        assert!(saver.flush());
        assert_eq!(std::fs::read(&a).unwrap(), b"again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_that_failed_is_not_said_to_be_done() {
        let dir = std::env::temp_dir().join(format!("nexus-saver-failed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Its folder isn't there: it can't be written.
        let queue = dir.join("not yet").join("queue.json");
        let saver = Saver::new();
        saver.write(queue.clone(), || Some(b"[]".to_vec()));
        assert!(!saver.flush(), "said to be done when it was not");
        // A later write of it that is done: all done.
        std::fs::create_dir_all(queue.parent().unwrap()).unwrap();
        saver.write(queue.clone(), || Some(b"[1]".to_vec()));
        assert!(saver.flush());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
