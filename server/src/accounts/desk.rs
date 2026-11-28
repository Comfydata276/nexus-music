//! The one thread that writes (design 19.3), and the line of work waiting for it.
//!
//! The [`Keeper`] is the data and the store kept apart together, and is written by one thread
//! only. Every step that reads or writes them is handed to that thread as one piece of work,
//! and **waits in a line of a stated length** ([`WORK_WAITING_MAX`]); past it, the caller is
//! told "busy" at once, and nothing of the step is done.
//!
//! A piece of work that has been taken is done to its end even if whoever asked for it has
//! gone (the caller hung up, or its deadline passed): what a decision says is to be kept is
//! kept whatever it answered (12.3). Only the answer is dropped.
//!
//! Nothing here reads anything while a socket is written: an answer is handed back to the
//! request's own task, which writes it.

use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};

use tokio::sync::oneshot;

use crate::store::keeper::Keeper;

/// How many pieces of work may wait for the thread that writes, beyond the one it is doing.
/// As many as there are connections served at once ([`crate::bounds::CONNECTIONS_MAX`]): one
/// step per request, so a request is never turned away for the line while it has a place.
pub const WORK_WAITING_MAX: usize = crate::bounds::CONNECTIONS_MAX;

type Work = Box<dyn FnOnce(&mut Keeper) + Send>;

/// The line, and the thread at its end.
pub struct Desk {
    line: Option<SyncSender<Work>>,
    thread: Option<JoinHandle<Keeper>>,
}

/// The line was full, or the thread has stopped: nothing of the step was done.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Busy;

impl Desk {
    /// Hands `keeper` to a thread of its own, which does each piece of work in the order given.
    pub fn open(mut keeper: Keeper) -> std::io::Result<Desk> {
        let (line, work) = mpsc::sync_channel::<Work>(WORK_WAITING_MAX);
        let thread = thread::Builder::new().name("nexus-writer".into()).spawn(move || {
            while let Ok(step) = work.recv() {
                // A fault part-way through a step may leave the two stores between halves of a
                // change; the start's comparison finishes it. Until then nothing more is written.
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| step(&mut keeper))).is_err() {
                    eprintln!("A fault in a step of the thread that writes: nothing more is written until the backend is started again.");
                    break;
                }
            }
            keeper
        })?;
        Ok(Desk { line: Some(line), thread: Some(thread) })
    }

    /// Does `step` on the thread that writes, and gives back what it gave. Busy at once if the
    /// line is full.
    pub async fn ask<T: Send + 'static>(&self, step: impl FnOnce(&mut Keeper) -> T + Send + 'static) -> Result<T, Busy> {
        let (answer, answered) = oneshot::channel();
        let work: Work = Box::new(move |keeper| {
            // Done whether or not anyone is still waiting for the answer.
            let _ = answer.send(step(keeper));
        });
        match self.line.as_ref().ok_or(Busy)?.try_send(work) {
            Ok(()) => answered.await.map_err(|_| Busy),
            Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => Err(Busy),
        }
    }

    /// Lets the line empty, and gives the keeper back (to be closed).
    pub fn close(mut self) -> Option<Keeper> {
        self.line.take();
        self.thread.take()?.join().ok()
    }
}

impl Drop for Desk {
    fn drop(&mut self) {
        self.line.take();
        if let Some(thread) = self.thread.take() {
            if let Ok(keeper) = thread.join() {
                keeper.close();
            }
        }
    }
}
