//! The one thread that writes (design 19.3), and the line of work waiting for it.
//!
//! The [`Keeper`] is the data and the store kept apart together, and is written by one thread
//! only. Every step that reads or writes them is handed to that thread as one piece of work,
//! and **waits in a line of a stated length**; past it, the caller is told "busy" at once, and
//! nothing of the step is done.
//!
//! **The line has two lanes** (the review of S8, C1). A stranger's sign-in, before any door has
//! been chosen for it (the account looked up, kept's check made), waits in a lane of its own,
//! [`STRANGER_WAITING_MAX`] long. Everything else waits in the other: a sign-in from a device
//! this backend has seen, a one-time code (already held to its own door of ten a second), and
//! every call made with a session, including the last step of one begun. The thread takes from
//! the second lane first, so a flood of strangers' sign-ins can fill only its own lane, and
//! never keeps a known device, a recovery code or a signed-in person's call waiting for more
//! than the one step under way.
//!
//! A piece of work whose caller has gone (hung up, or its deadline passed) **before it was
//! begun** is dropped: nothing of it has happened. One that has begun is done to its end even if
//! whoever asked for it has gone: what a decision says is to be kept is kept whatever it
//! answered (12.3). Only the answer is dropped.
//!
//! Nothing here reads anything while a socket is written: an answer is handed back to the
//! request's own task, which writes it.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};

use tokio::sync::oneshot;

use crate::store::keeper::Keeper;

/// How many pieces of work may wait in the lane of everything but strangers' sign-ins, beyond
/// the one being done. As many as there are connections served at once
/// ([`crate::bounds::CONNECTIONS_MAX`]): one step per request, so such a request is never
/// turned away for the line while it has a place.
pub const WORK_WAITING_MAX: usize = crate::bounds::CONNECTIONS_MAX;

/// How many strangers' sign-ins may wait for their first step at once. Small: each is a lookup
/// and a decision; more waiting would only be more waiting.
pub const STRANGER_WAITING_MAX: usize = 8;

/// Which lane a piece of work waits in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// A sign-in's first step from a device this backend has not seen, before any door.
    Stranger,
    /// Everything else.
    Kept,
}

type Work = Box<dyn FnOnce(&mut Keeper) + Send>;

struct Line {
    kept: VecDeque<Work>,
    stranger: VecDeque<Work>,
    open: bool,
}

struct Shared {
    line: Mutex<Line>,
    ready: Condvar,
}

/// The line, and the thread at its end.
pub struct Desk {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<Keeper>>,
}

/// The lane was full, or the thread has stopped: nothing of the step was done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

impl Desk {
    /// Hands `keeper` to a thread of its own, which does each piece of work in turn: the kept
    /// lane's first, then the strangers'.
    pub fn open(mut keeper: Keeper) -> std::io::Result<Desk> {
        let shared = Arc::new(Shared { line: Mutex::new(Line { kept: VecDeque::new(), stranger: VecDeque::new(), open: true }), ready: Condvar::new() });
        let taken = shared.clone();
        let thread = thread::Builder::new().name("nexus-writer".into()).spawn(move || {
            while let Some(step) = next(&taken) {
                // A fault part-way through a step may leave the two stores between halves of a
                // change; the start's comparison finishes it. Until then nothing more is written.
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| step(&mut keeper))).is_err() {
                    eprintln!("A fault in a step of the thread that writes: nothing more is written until the backend is started again.");
                    if let Ok(mut line) = taken.line.lock() {
                        line.open = false;
                        line.kept.clear();
                        line.stranger.clear();
                    }
                    break;
                }
            }
            keeper
        })?;
        Ok(Desk { shared, thread: Some(thread) })
    }

    /// Does `step` on the thread that writes, in the kept lane, and gives back what it gave.
    /// Busy at once if the lane is full.
    pub async fn ask<T: Send + 'static>(&self, step: impl FnOnce(&mut Keeper) -> T + Send + 'static) -> Result<T, Busy> {
        self.ask_in(Lane::Kept, step).await
    }

    /// The same, in `lane`.
    pub async fn ask_in<T: Send + 'static>(&self, lane: Lane, step: impl FnOnce(&mut Keeper) -> T + Send + 'static) -> Result<T, Busy> {
        let (answer, answered) = oneshot::channel();
        let work: Work = Box::new(move |keeper| {
            // Not begun, and nobody waits for it: nothing of it is done.
            if answer.is_closed() {
                return;
            }
            // Begun: done whether or not anyone is still waiting for the answer.
            let _ = answer.send(step(keeper));
        });
        {
            let mut line = self.shared.line.lock().map_err(|_| Busy)?;
            if !line.open {
                return Err(Busy);
            }
            let (waiting, max) = match lane {
                Lane::Kept => (&mut line.kept, WORK_WAITING_MAX),
                Lane::Stranger => (&mut line.stranger, STRANGER_WAITING_MAX),
            };
            if waiting.len() >= max {
                return Err(Busy);
            }
            waiting.push_back(work);
        }
        self.shared.ready.notify_one();
        answered.await.map_err(|_| Busy)
    }

    /// Lets the line empty, and gives the keeper back (to be closed).
    pub fn close(mut self) -> Option<Keeper> {
        self.shut();
        self.thread.take()?.join().ok()
    }

    fn shut(&self) {
        if let Ok(mut line) = self.shared.line.lock() {
            line.open = false;
        }
        self.shared.ready.notify_all();
    }
}

/// The next piece of work, kept lane first; none once the line is shut and empty.
fn next(shared: &Shared) -> Option<Work> {
    let mut line = shared.line.lock().ok()?;
    loop {
        if let Some(work) = line.kept.pop_front().or_else(|| line.stranger.pop_front()) {
            return Some(work);
        }
        if !line.open {
            return None;
        }
        line = shared.ready.wait(line).ok()?;
    }
}

impl Drop for Desk {
    fn drop(&mut self) {
        self.shut();
        if let Some(thread) = self.thread.take() {
            if let Ok(keeper) = thread.join() {
                keeper.close();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Work in the kept lane is taken before strangers' work that was waiting longer, and the
    /// strangers' lane is full at its own bound while the kept lane still takes work.
    #[test]
    fn the_kept_lane_goes_first_and_strangers_fill_only_their_own() {
        let root = std::env::temp_dir().join(format!("nexus-desk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        let (keeper, _) = Keeper::open(&root.join("state"), &root.join("data"), 1).unwrap();
        let desk = Arc::new(Desk::open(keeper).unwrap());
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let order = Arc::new(Mutex::new(Vec::new()));
        runtime.block_on(async {
            // Hold the thread on one piece of work while the lanes fill.
            let (release, held) = std::sync::mpsc::channel::<()>();
            let holding = {
                let desk = desk.clone();
                tokio::spawn(async move { desk.ask(move |_| held.recv().ok()).await })
            };
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let mut strangers = Vec::new();
            for n in 0..STRANGER_WAITING_MAX {
                let (desk, order) = (desk.clone(), order.clone());
                strangers.push(tokio::spawn(async move { desk.ask_in(Lane::Stranger, move |_| order.lock().unwrap().push(format!("stranger {n}"))).await }));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            assert_eq!(desk.ask_in(Lane::Stranger, |_| ()).await, Err(Busy), "the strangers' lane is full");
            let kept = {
                let (desk, order) = (desk.clone(), order.clone());
                tokio::spawn(async move { desk.ask(move |_| order.lock().unwrap().push("kept".into())).await })
            };
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            release.send(()).unwrap();
            holding.await.unwrap().unwrap();
            kept.await.unwrap().unwrap();
            for stranger in strangers {
                stranger.await.unwrap().unwrap();
            }
        });
        assert_eq!(order.lock().unwrap().first().map(String::as_str), Some("kept"));
        drop(desk);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Work whose caller has gone before it was begun is not done.
    #[test]
    fn work_abandoned_before_it_begins_is_not_done() {
        let root = std::env::temp_dir().join(format!("nexus-desk-gone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        let (keeper, _) = Keeper::open(&root.join("state"), &root.join("data"), 1).unwrap();
        let desk = Arc::new(Desk::open(keeper).unwrap());
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let done = Arc::new(Mutex::new(false));
        runtime.block_on(async {
            let (release, held) = std::sync::mpsc::channel::<()>();
            let holding = {
                let desk = desk.clone();
                tokio::spawn(async move { desk.ask(move |_| held.recv().ok()).await })
            };
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let gone = {
                let (desk, done) = (desk.clone(), done.clone());
                tokio::spawn(async move { desk.ask_in(Lane::Stranger, move |_| *done.lock().unwrap() = true).await })
            };
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            gone.abort();
            let _ = gone.await;
            release.send(()).unwrap();
            holding.await.unwrap().unwrap();
            // Something after it, so that the thread has passed it.
            desk.ask(|_| ()).await.unwrap();
        });
        assert!(!*done.lock().unwrap());
        drop(desk);
        let _ = std::fs::remove_dir_all(&root);
    }
}
