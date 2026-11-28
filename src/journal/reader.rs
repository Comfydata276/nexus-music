//! A separate reader can answer while the journal writer is waiting for the disk.

use std::path::Path;
use std::sync::mpsc::Sender;

use super::{Scope, events, writer::{JournalDirectory, Settlement}};
use events::{EventId, Placed};

pub(super) struct Read {
    pub(super) scope: Scope,
    pub(super) from: u64,
    pub(super) max_events: usize,
    pub(super) max_bytes: usize,
    pub(super) answered: Box<dyn FnOnce(Vec<Placed>) + Send>,
}

impl Read {
    pub(super) fn run(self, root: &Path) {
        let events = self.events(root).unwrap_or_default();
        (self.answered)(events);
    }

    fn events(&self, root: &Path) -> std::io::Result<Vec<Placed>> {
        let directory = JournalDirectory::new(root, self.scope.account);
        let limit = self.max_events.min(events::BATCH_EVENTS);
        let mut bytes_left = self.max_bytes.min(events::BATCH_BYTES);
        let mut pending = Vec::new();
        for number in directory.numbers()? {
            let Some(file) = directory.read(number)?.1 else { continue };
            if file.life != self.scope.life {
                continue;
            }
            let settled = directory.settled(number)?;
            if !directory.exists(number) {
                continue;
            }
            for event in file.events {
                if event.sequence() < self.from || settled.contains_key(&event.sequence()) {
                    continue;
                }
                if pending.len() == limit {
                    return Ok(pending);
                }
                let Ok(line) = event.line() else { continue };
                let Some(left) = bytes_left.checked_sub(line.len()) else { return Ok(pending) };
                bytes_left = left;
                pending.push(event);
            }
        }
        Ok(pending)
    }
}

pub(super) enum Update {
    ScopeChanged(Option<Scope>),
    Settle(Scope, Vec<EventId>, Settlement),
    LifeMoved(Scope),
    ResendAll([u8; 16]),
}

pub(super) struct ChangeRequest {
    pub(super) update: Update,
    pub(super) finished: Option<Sender<()>>,
}

#[derive(Clone, Copy)]
pub(super) struct Waiting {
    pub(super) scope: Scope,
    pub(super) pending: u64,
    pub(super) from: u64,
}
