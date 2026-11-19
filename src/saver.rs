//! Writes files in the background, in the order they were asked for.
//!
//! Saving the queue or a list of downloads means turning thousands of songs into text and
//! putting it on disk. Done on the UI thread that is a pause in everything on screen, so it
//! is handed to one thread here instead. When several versions of the same file are waiting,
//! only the newest is written.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

type Make = Box<dyn FnOnce() -> Option<Vec<u8>> + Send>;

enum Job {
    Write(PathBuf, Make),
    Remove(PathBuf),
    /// Answered once everything asked for before it has been done.
    Sync(Sender<()>),
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

    /// Waits until everything asked for so far is on disk. For when the app is closing, or
    /// a file is about to be read back.
    pub fn flush(&self) {
        let (done, wait) = mpsc::channel();
        if self.jobs.send(Job::Sync(done)).is_ok() {
            let _ = wait.recv_timeout(Duration::from_secs(5));
        }
    }
}

fn work(waiting: Receiver<Job>) {
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
                        crate::config::write_atomically(&path, &bytes);
                    }
                }
                Job::Remove(path) => {
                    let _ = std::fs::remove_file(path);
                }
                Job::Sync(done) => {
                    let _ = done.send(());
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
        saver.flush();
        assert_eq!(std::fs::read(&a).unwrap(), b"fourth");
        // The versions in between were never built, let alone written.
        assert_eq!(built.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(!b.exists());
        // A removal followed by a write leaves the file there.
        saver.remove(a.clone());
        saver.write(a.clone(), || Some(b"again".to_vec()));
        saver.flush();
        assert_eq!(std::fs::read(&a).unwrap(), b"again");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
