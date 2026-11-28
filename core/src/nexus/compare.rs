//! Design 12.3, 12.4: what the backend finds, each time it starts and before it answers
//! anything, when it compares its data with the store kept apart.
//!
//! The server (slice S3) reads both and hands them in: for each account, by the number S3
//! gives it, C3r's `Apart` from the store and `Rows` from the data. Nothing here opens either
//! or has a clock. Which sessions still work, and what a restore does to an account, are
//! C3r's (`kept::Account::load`, `Account::restore`); this says only whether the two are the
//! same age, whether a change was cut between them, or whether they cannot belong together.

use std::collections::BTreeMap;

use super::kept::{Apart, Rows, Startup, Status, StoreComparison, at_start};

/// The one change the store has been told of first and the data may not yet have: written
/// with the store's first half of the change, cleared with its last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// A decision's updates (or an account being made), for these accounts.
    Change { accounts: Vec<u64> },
    /// The data was put back by the command, or found older: the new restore number is
    /// in the store, and the data is being brought to it.
    Restore { restore: u64 },
    /// The owner took the data as it is.
    TakeAsIs { restore: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StoreView {
    pub restore: u64,
    /// How far the data's changes had got when the store last heard (12.3): written at most
    /// once a minute, so it is never ahead of the data, and may be behind it.
    pub mark: u64,
    pub accounts: BTreeMap<u64, Apart>,
    pub pending: Option<Pending>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DataView {
    pub restore: u64,
    /// How far the data's changes have got: one more for every step written.
    pub changes: u64,
    pub accounts: BTreeMap<u64, Rows>,
}

/// Something no cut between the two stores, and no older copy of the data, can make: the
/// store is not this data's. The owner is asked to take the data as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contradiction {
    DataRestoreAhead,
    AccountUnknown,
    /// The data has an account further on (a life, a count of passwords, a revision, a music
    /// login) than the store, which moves first.
    AccountAhead,
    DeviceUnknown,
    DeviceAhead,
    SessionNeverReserved,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Found {
    pub comparison: StoreComparison,
    pub contradiction: Option<Contradiction>,
}

impl Found {
    fn stop(why: Contradiction) -> Self {
        Self { comparison: StoreComparison::StoreLost, contradiction: Some(why) }
    }

    /// What is done (`kept::at_start`). Only the owner's command adopts.
    pub fn startup(&self, owner_adopts: bool) -> Startup {
        at_start(self.comparison, owner_adopts)
    }
}

/// 12.3, 12.4: the data against the store kept apart.
///
/// - **Older** whenever the data lacks anything the store says happened (the store's mark of
///   how far the data had got included), unless it is in an account the store's note names
///   as under way.
/// - **A change cut between the two** (in an account the note names): the store ahead
///   (`EndingRecorded`), or the data holding a grant the store has not yet recognized
///   (`GrantOnlyInData`).
/// - **Cannot be this data's store**: the data ahead in anything only the store moves first.
///
/// The design does not say how a cut write is told from an older copy. The store names the
/// accounts of the change under way; anything else that differs is an older copy.
pub fn compare(store: &StoreView, data: &DataView) -> Found {
    if let Some(Pending::Restore { restore } | Pending::TakeAsIs { restore }) = store.pending {
        if data.restore > restore {
            return Found::stop(Contradiction::DataRestoreAhead);
        }
        return Found { comparison: StoreComparison::OlderCopy, contradiction: None };
    }
    if data.restore > store.restore {
        return Found::stop(Contradiction::DataRestoreAhead);
    }
    let under_way = |account: u64| matches!(&store.pending, Some(Pending::Change { accounts }) if accounts.contains(&account));
    let mut older = data.restore < store.restore || data.changes < store.mark;
    let (mut ending, mut grant) = (false, false);
    for (&number, rows) in &data.accounts {
        let Some(apart) = store.accounts.get(&number) else {
            return Found::stop(Contradiction::AccountUnknown);
        };
        let (kept, row) = (&apart.account, &rows.account);
        let cut = under_way(number);
        if kept.status == Status::Deleted {
            if cut { ending = true } else { older = true }
            continue;
        }
        if row.life > kept.life || row.password_changes > kept.password_changes || row.revision > kept.revision || row.music_login > kept.music_login {
            return Found::stop(Contradiction::AccountAhead);
        }
        if row != kept {
            if cut { ending = true } else { older = true }
        }
        for (&id, device) in &rows.devices {
            let Some(record) = apart.devices.get(&id) else {
                return Found::stop(Contradiction::DeviceUnknown);
            };
            if device.number > record.number {
                return Found::stop(Contradiction::DeviceAhead);
            }
            if device.session.as_ref().is_some_and(|session| session.id > record.last_session) {
                return Found::stop(Contradiction::SessionNeverReserved);
            }
            if device.number < record.number {
                if cut { ending = true } else { older = true }
                continue;
            }
            match (&device.session, record.permit) {
                (None, None) => {}
                (Some(session), Some(permit)) if session.id == permit.id && session.leave == permit.leave => {}
                // What the store still permits is never missing from data its own age:
                // what ends a session is written to the store first.
                (None, Some(_)) => older = true,
                _ if !cut => older = true,
                // A session, or leave, given in the data and not yet recognized in the store;
                // or one the store has ended and the data not yet.
                (Some(session), permit) => {
                    let given = permit.is_none() && session.id == record.last_session || permit.is_some_and(|p| p.id == session.id && session.leave.is_some());
                    if given { grant = true } else { ending = true }
                }
            }
        }
        for id in apart.devices.keys() {
            if !rows.devices.contains_key(id) {
                // A device made in the store first, its row not yet written; or an older copy.
                if cut { grant = true } else { older = true }
            }
        }
    }
    for (number, apart) in &store.accounts {
        if apart.account.status != Status::Deleted && !data.accounts.contains_key(number) && !under_way(*number) {
            older = true;
        }
    }
    let comparison = if older {
        StoreComparison::OlderCopy
    } else if ending {
        StoreComparison::EndingRecorded
    } else if grant {
        StoreComparison::GrantOnlyInData
    } else {
        StoreComparison::Matches
    };
    Found { comparison, contradiction: None }
}

#[cfg(test)]
mod tests;
