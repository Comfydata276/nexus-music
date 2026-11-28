//! §9.2 and §11.2: what the backend finds, each time it starts and before it answers
//! anything, when it compares its data with the store kept apart; and what it then does.
//!
//! The server (slice S3) reads both and hands them in as numbers. Nothing here opens either,
//! has a clock, or reads a name: an account and a device are their numbers.
//!
//! Which sessions still work is C3's rule (`kept::Device::load`). This is the part above it:
//! whether the data is the store's age or older (`kept::StoreComparison`), whether a write
//! was cut between the two stores, and whether the two cannot belong together at all. What
//! is then done is `kept::at_start` and `kept::restored_account`.

use std::collections::BTreeMap;

use super::kept::{Apart, Family, Permit, Restore, Rows, Startup, StoreComparison, at_start, restored_account};

/// One account as the store kept apart has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreAccount {
    /// Its life (§11.1). It only goes up, and outlives the account's rows in the data.
    pub life: u64,
    /// Held through this life (§11.2 step 3): nothing of it is given or taken until its
    /// person answers.
    pub held: Option<u64>,
    /// Whether the data has the account's row: set as it is made, cleared as it is deleted.
    pub present: bool,
    /// Deleted by the admin (12.5): its rows never come back from a copy.
    pub deleted: bool,
    /// Switched off (8.7): a copy that has it on is switched off again.
    pub off: bool,
    /// How many times its password has been changed or reset: a copy with an earlier count
    /// holds a password that must no longer work (12.4 step 3).
    pub passwords: u64,
}

/// One account as the data has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataAccount {
    pub life: u64,
    pub on: bool,
    pub passwords: u64,
}

/// One device as the store kept apart has it: its half of C3's `Apart`, by number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreDevice {
    pub account: u64,
    pub restore: u64,
    pub life: u64,
    pub number: u64,
    pub last_session: u64,
    pub working: Option<Permit>,
    pub waiting: Option<Permit>,
    /// Removed, or erased with its account's history: its id is never used again, and a copy
    /// that holds it does not bring it back.
    pub gone: bool,
}

impl StoreDevice {
    fn permits(&self) -> impl Iterator<Item = &Permit> {
        self.working.iter().chain(&self.waiting)
    }
}

/// The one change the store has been told of first and the data may not yet have (§9.2:
/// "if the program dies between the two, the store is ahead"). It is written with the
/// store's first half of the change and cleared with its last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    MakeAccount { account: u64 },
    /// A change to a whole account that takes leave away (switched off, deleted, its
    /// password changed or reset), with its devices' endings.
    Account { account: u64 },
    MakeDevice { device: u64 },
    /// A device's decision (C3's `Effects`): `family` as its `Update` says.
    Update { device: u64, account: u64, family: Option<Family> },
    Remove { device: u64 },
    /// The data was put back by the command, or found older: the new restore number is in
    /// the store, and the data is being brought to it.
    Restore { restore: u64 },
    /// The owner took the data as it is, with no store to compare it with.
    TakeAsIs { restore: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StoreView {
    pub restore: u64,
    pub accounts: BTreeMap<u64, StoreAccount>,
    pub devices: BTreeMap<u64, StoreDevice>,
    pub pending: Option<Pending>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataSession {
    pub id: u64,
    pub delete_proven: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDevice {
    pub account: u64,
    pub number: u64,
    pub sessions: Vec<DataSession>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DataView {
    pub restore: u64,
    pub accounts: BTreeMap<u64, DataAccount>,
    pub devices: BTreeMap<u64, DataDevice>,
}

/// Something no cut between the two stores, and no older copy of the data, can make: the
/// store is not this data's (§11.2's "cannot be read"). The owner is asked to take the data
/// as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contradiction {
    /// The data has a restore number the store never gave.
    DataRestoreAhead,
    /// The data has an account the store never made (accounts are made in the store first).
    AccountUnknown,
    /// The data has an account at a life, or a count of passwords, the store never reached.
    AccountAhead,
    /// The data has a device the store never made.
    DeviceUnknown,
    /// The data has a device under another account than the store gave it.
    DeviceElsewhere,
    /// The data has a device at a number the store never reached.
    DeviceAhead,
    /// The data has a session under a number the store never reserved.
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

/// §9.2, §11.2: the data against the store kept apart.
///
/// - **Older** whenever the data lacks anything the store says happened, unless it is the
///   one change [`Pending`] names and the data is exactly one step behind it.
/// - **A write cut between the two** (the `Pending` change): an ending, a deletion or a
///   removal the store has and the data not yet (`EndingRecorded`); or a grant the data has
///   and the store not yet (`GrantOnlyInData`).
/// - **Cannot be this data's store** ([`Contradiction`]): the data is ahead of the store in
///   something only the store moves first.
///
/// The design does not say how a cut write is told from an older copy. The store names the
/// change under way; anything else that differs is an older copy.
pub fn compare(store: &StoreView, data: &DataView) -> Found {
    if let Some(Pending::Restore { restore } | Pending::TakeAsIs { restore }) = store.pending {
        if data.restore > restore {
            return Found::stop(Contradiction::DataRestoreAhead);
        }
        // Done again from the start: it is the same whichever step it was cut at.
        return Found { comparison: StoreComparison::OlderCopy, contradiction: None };
    }
    if data.restore > store.restore {
        return Found::stop(Contradiction::DataRestoreAhead);
    }
    let pending_account = |account: u64| match store.pending {
        Some(Pending::MakeAccount { account: made } | Pending::Account { account: made }) => made == account,
        Some(Pending::Update { account: of, family: Some(Family::Account), .. }) => of == account,
        _ => false,
    };
    let pending_device = |device: u64, account: u64| {
        pending_account(account)
            || match store.pending {
                Some(Pending::MakeDevice { device: d } | Pending::Update { device: d, .. } | Pending::Remove { device: d }) => d == device,
                _ => false,
            }
    };
    let (mut older, mut ending, mut grant) = (data.restore < store.restore, false, false);
    for (&number, row) in &data.accounts {
        let Some(kept) = store.accounts.get(&number) else {
            return Found::stop(Contradiction::AccountUnknown);
        };
        let life = row.life;
        if life > kept.life || row.passwords > kept.passwords {
            return Found::stop(Contradiction::AccountAhead);
        }
        let cut = pending_account(number);
        if kept.deleted {
            if cut { ending = true } else { older = true }
            continue;
        }
        if row.passwords < kept.passwords {
            if cut && row.passwords.checked_add(1) == Some(kept.passwords) { ending = true } else { older = true }
        }
        // Switching off goes to the store first; switching on to the data first, under a note
        // in the store all the same. Cut either way, the data is on and the store off, and
        // the data is brought to the store: off.
        if row.on && kept.off {
            if cut { ending = true } else { older = true }
        }
        if !row.on && !kept.off {
            older = true;
        }
        if life < kept.life {
            if kept.held.is_some_and(|through| through >= kept.life) {
                // Held already: the copy's history waits for its person (§11.2 step 3).
            } else if pending_account(number) && life.checked_add(1) == Some(kept.life) {
                ending = true;
            } else {
                older = true;
            }
        }
    }
    for (number, kept) in &store.accounts {
        if kept.present && !data.accounts.contains_key(number) && !pending_account(*number) {
            older = true;
        }
    }
    for (&id, row) in &data.devices {
        let Some(kept) = store.devices.get(&id) else {
            return Found::stop(Contradiction::DeviceUnknown);
        };
        if row.account != kept.account {
            return Found::stop(Contradiction::DeviceElsewhere);
        }
        if row.number > kept.number {
            return Found::stop(Contradiction::DeviceAhead);
        }
        if row.sessions.iter().any(|session| session.id > kept.last_session) {
            return Found::stop(Contradiction::SessionNeverReserved);
        }
        let cut = pending_device(id, row.account);
        if kept.gone {
            if cut { ending = true } else { older = true }
            continue;
        }
        if row.number < kept.number {
            if cut && row.number.checked_add(1) == Some(kept.number) { ending = true } else { older = true }
            continue;
        }
        for session in &row.sessions {
            match kept.permits().find(|permit| permit.id == session.id) {
                None if !cut => older = true,
                // The grant this change reserved its number for, in the data and not yet in
                // the store.
                None if session.id == kept.last_session => grant = true,
                None => ending = true,
                Some(permit) if permit.delete_proven != session.delete_proven => {
                    if !cut {
                        older = true
                    } else if session.delete_proven.is_some() && permit.delete_proven.is_none() {
                        grant = true
                    } else {
                        ending = true
                    }
                }
                Some(_) => {}
            }
        }
        // A session the store still permits is never missing from data its own age: what
        // ends one is written to the store first.
        if kept.permits().any(|permit| !row.sessions.iter().any(|session| session.id == permit.id)) {
            older = true;
        }
    }
    for (id, kept) in &store.devices {
        if !kept.gone && !data.devices.contains_key(id) && !pending_device(*id, kept.account) {
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

/// §11.2 step 3, for an older copy: each account of the data whose history is to be held,
/// and through which life (`kept::restored_account`). An account the store no longer has
/// rows for is not here: there is nothing of it to hold.
pub fn holds(store: &StoreView, data: &DataView) -> Vec<(u64, u64)> {
    data.accounts
        .iter()
        .filter_map(|(&number, row)| {
            let life = row.life;
            let kept = store.accounts.get(&number)?;
            if kept.deleted {
                return None;
            }
            let already = kept.held.is_some_and(|through| through >= kept.life);
            match restored_account(kept.life, life, already) {
                Restore::HoldThrough(through) => Some((number, through)),
                Restore::Reprove => None,
            }
        })
        .collect()
}

/// What is taken away again from an account the data has (12.4 step 3), whether for an
/// older copy or a cut change: what the store says happened to it since.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TakenAgain {
    pub account: u64,
    /// Deleted since: everything of it goes again.
    pub erase: bool,
    /// Switched off since.
    pub off: bool,
    /// Its password changed since: the one the data holds must not work. It waits for the
    /// admin to reset it (8.4).
    pub reset_password: bool,
}

pub fn taken_again(store: &StoreView, data: &DataView) -> Vec<TakenAgain> {
    data.accounts
        .iter()
        .filter_map(|(&account, row)| {
            let kept = store.accounts.get(&account)?;
            let again = TakenAgain {
                account,
                erase: kept.deleted,
                off: !kept.deleted && kept.off && row.on,
                reset_password: !kept.deleted && row.passwords < kept.passwords,
            };
            (again.erase || again.off || again.reset_password).then_some(again)
        })
        .collect()
}

/// A device's rows brought up to the store (§9.2: "at the next start the data is brought up
/// to it"): the store's scope, and only the sessions the store still permits, in the places
/// it permits them. Loading the result gives the device that loading the rows as they were
/// gives (`kept::Device::load`, whose rule this follows: see the tests); it only leaves out
/// what would never be loaded.
///
/// (C3's own `Device::rows` would give this from a loaded device, were it public.)
pub fn brought_up(apart: &Apart, data: &Rows) -> Rows {
    let recover = |permit: Permit| {
        if apart.scope != data.scope || permit.id == 0 || permit.id > apart.last_session {
            return None;
        }
        let mut row = data.working.iter().chain(&data.waiting).find(|session| session.id == permit.id)?.clone();
        row.delete_proven = row.delete_proven.filter(|_| row.delete_proven == permit.delete_proven);
        Some(row)
    };
    let working = apart.working.and_then(recover);
    let waiting = match (&working, apart.waiting) {
        (Some(_), Some(waiting)) if Some(waiting.id) != apart.working.map(|permit| permit.id) => recover(waiting),
        _ => None,
    };
    Rows { scope: apart.scope.clone(), working, waiting }
}

#[cfg(test)]
mod tests;
