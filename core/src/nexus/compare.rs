//! Design 12.3, 12.4: what the backend finds, each time it starts and before it answers
//! anything, when it compares its data with the store kept apart.
//!
//! The server (slice S3) reads both and hands them in: for each account, by the number S3
//! gives it, C3r's `Apart` from the store and `Rows` from the data. Nothing here opens either
//! or has a clock. Which sessions still work, and what a restore does to an account, are
//! C3r's (`kept::Account::load`, `Account::restore`); this says only whether the two are the
//! same age, whether a change was cut between them, or whether they cannot belong together.

use std::collections::BTreeMap;

use super::kept::{Apart, ApartDevice, Rows, Startup, Status, StoreComparison, at_start};

/// The one change the store has been told of first and the data may not yet have: written
/// with the store's first half of the change, cleared with its last.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pending {
    /// A decision's updates (or an account being made), for these accounts.
    Change { accounts: Vec<u64> },
    /// The data was found older, or a put-back's copy is in place: the new restore number
    /// is in the store, each device's number moved, and the data is being brought to it.
    Restore { restore: u64 },
    /// The owner's put-back, planned from the exact copy staged beside the data and written
    /// here whole (the new restore number, each device's number moved) before anything of
    /// the data in use was replaced: the staged copy may still be waiting to take its place.
    PutBack { restore: u64 },
    /// The owner took the data as it is.
    TakeAsIs { restore: u64 },
}

/// Its `Debug`, and [`DataView`]'s, count accounts: an account's state holds its email.
#[derive(Clone, PartialEq, Eq, Default)]
pub struct StoreView {
    pub restore: u64,
    /// How far the data's changes had got when the store last heard (12.3): written at most
    /// once a minute, so it is never ahead of the data, and may be behind it.
    pub mark: u64,
    pub accounts: BTreeMap<u64, Apart>,
    pub pending: Option<Pending>,
}

#[derive(Clone, PartialEq, Eq, Default)]
pub struct DataView {
    pub restore: u64,
    /// How far the data's changes have got: one more for every step written.
    pub changes: u64,
    pub accounts: BTreeMap<u64, Rows>,
}

impl std::fmt::Debug for StoreView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoreView")
            .field("restore", &self.restore)
            .field("mark", &self.mark)
            .field("accounts", &self.accounts.keys().collect::<Vec<_>>())
            .field("pending", &self.pending)
            .finish()
    }
}

impl std::fmt::Debug for DataView {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DataView")
            .field("restore", &self.restore)
            .field("changes", &self.changes)
            .field("accounts", &self.accounts.keys().collect::<Vec<_>>())
            .finish()
    }
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
    if let Some(Pending::Restore { restore } | Pending::PutBack { restore } | Pending::TakeAsIs { restore }) = store.pending {
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
        for (id, record) in &apart.devices {
            // A device the store permits a session for and the data has no row of: made in
            // the store first and its row not yet written, or an older copy. (A record with no
            // session is what is left of a device the data lost with an older copy: it keeps
            // its id from being given again, and says nothing of the data's age.)
            if record.permit.is_some() && !rows.devices.contains_key(id) {
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

/// What is done first, at a start and by either of the owner's commands alike, when the
/// store kept apart says a restore was under way (12.4: "if the program dies partway
/// through, it does the same again at the next start: every step is safe to do twice").
/// One decision for all three, so that a command run again after a cut never begins a second
/// restore, nor moves a number twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    /// Nothing under way: the start compares the two; a command begins its own work.
    Nothing,
    /// A put-back's copy is staged and its plan is in the store: put it in place of the
    /// data, then bring it to `restore`.
    PutInPlace { restore: u64 },
    /// The data in place is brought to `restore` (12.4 steps 1 to 5), adopted as it is if
    /// `adopt` (the owner's take-as-is).
    Restore { restore: u64, adopt: bool },
    /// Neither can go on alone.
    Stop(Halt),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Halt {
    /// A put-back was planned and its copy is neither staged nor in place: the owner puts
    /// the copy back again.
    CopyLost,
    /// A restore was under way and there is no data.
    NoData,
    /// The data has a restore number the store never gave.
    DataRestoreAhead,
}

/// [`Resume`], from what the store has under way (`pending`), whether a put-back's copy is
/// staged beside the data, and the restore number of the data in place, if there is any.
pub fn resume(pending: Option<&Pending>, staged: bool, data: Option<u64>) -> Resume {
    match (pending, data) {
        (None | Some(Pending::Change { .. }), _) => Resume::Nothing,
        (Some(&Pending::PutBack { restore }), _) if staged => Resume::PutInPlace { restore },
        (Some(Pending::PutBack { .. }), None) => Resume::Stop(Halt::CopyLost),
        (Some(Pending::Restore { .. } | Pending::TakeAsIs { .. }), None) => Resume::Stop(Halt::NoData),
        (Some(&(Pending::Restore { restore } | Pending::PutBack { restore } | Pending::TakeAsIs { restore })), Some(data)) if data > restore => {
            Resume::Stop(Halt::DataRestoreAhead)
        }
        (Some(&Pending::TakeAsIs { restore }), Some(_)) => Resume::Restore { restore, adopt: true },
        (Some(&(Pending::Restore { restore } | Pending::PutBack { restore })), Some(_)) => Resume::Restore { restore, adopt: false },
    }
}

/// Why a copy is not put back. Said before anything is changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotPutBack {
    /// A restore is under way: it is finished first ([`resume`]).
    UnderWay,
    /// The copy cannot be this store's data: it has what only the store moves first.
    Contradiction(Contradiction),
    /// The restore number can go no higher.
    Spent,
}

/// 12.4, the owner's put-back: whether `copy` (read from the exact file staged to take the
/// data's place) can be put back under this store, and the restore number it is put back
/// under. Asked before anything is recorded or replaced; what each account becomes is then
/// `kept::Account::restore`'s, and any refusal there also stops it with nothing changed.
pub fn put_back(store: &StoreView, copy: &DataView) -> Result<u64, NotPutBack> {
    if !matches!(store.pending, None | Some(Pending::Change { .. })) {
        return Err(NotPutBack::UnderWay);
    }
    let settled = StoreView { pending: None, ..store.clone() };
    if let Some(why) = compare(&settled, copy).contradiction {
        return Err(NotPutBack::Contradiction(why));
    }
    store.restore.checked_add(1).filter(|&new| new > copy.restore).ok_or(NotPutBack::Spent)
}

/// Whether `copy` is already the data in place, put back (or found older and restored) and
/// nothing written since: the data's restore is the store's, its changes are the store's
/// mark, and it was restored from a copy last written when `copy` was. Then the command run
/// again (killed after its last step, before it said so) changes nothing, rather than putting
/// the same copy back a second time and moving every number again (the review's finding 4).
pub fn already_put_back(store: &StoreView, data: &DataView, restored_from: Option<u64>, copy_written_at: u64) -> bool {
    store.pending.is_none() && store.restore > 0 && data.restore == store.restore && data.changes == store.mark && restored_from == Some(copy_written_at)
}

/// 12.4, the owner's take-as-is: whether there is anything to take. `store` is the store kept
/// apart only if it can be read and is this data's backend's, and was told its data was
/// made; a store under way is finished, not taken ([`resume`]).
pub fn needs_taking(store: Option<&StoreView>, data: &DataView) -> bool {
    match store {
        None => true,
        Some(store) if store.pending.is_some() && !matches!(store.pending, Some(Pending::Change { .. })) => false,
        Some(store) => compare(store, data).contradiction.is_some(),
    }
}

/// 12.4, the owner's take-as-is: an account's record in the store made again from its rows
/// in the data as it is.
///
/// - **With no authority** (the store lost, unreadable, or another backend's): nothing says
///   which passwords changed or which codes were used since, so none of them is trusted:
///   "every account then waits for a new one-time code". Its count of passwords moves one
///   on, so that the verifier the data holds no longer matches (`kept`'s fence) and is
///   removed with every code in the step that ends the sessions; it waits for a reset.
/// - **With `authority`**, a readable record of the same backend's store: what it took away
///   is kept. A tombstone stays a tombstone; switched off stays off; an admin's role taken
///   away stays away; each count at the higher of the two (so a password changed since, or a
///   life moved on, fences the data's as `kept` loads it); a device's numbers at the higher.
///
/// Every session ends anyway (the restore that follows): no permit is kept.
///
/// (Kept's to take: asked of C3r as `RestoreMode::Adopt` deciding this itself. Until then it
/// is here, in core, and the server only carries it out.)
pub fn recovery_baseline(rows: &Rows, authority: Option<&Apart>) -> Apart {
    let mut account = rows.account.clone();
    let device = |id: &u64, row: &super::kept::DeviceRow| {
        let session = row.session.as_ref();
        let record = ApartDevice {
            number: row.number,
            last_session: session.map_or(0, |session| session.id),
            last_leave: session.and_then(|session| session.leave).map_or(0, |leave| leave.fields().3),
            permit: None,
        };
        (*id, record)
    };
    let mut devices: std::collections::BTreeMap<u64, ApartDevice> = rows.devices.iter().map(|(id, row)| device(id, row)).collect();
    match authority {
        None => {
            account.password_changes = account.password_changes.saturating_add(1);
            account.password_reset_required = true;
            account.code_available = false;
        }
        Some(kept) => {
            let was = &kept.account;
            if was.status == Status::Deleted {
                account.status = Status::Deleted;
            } else if was.status == Status::Off {
                account.status = Status::Off;
            }
            account.admin &= was.admin;
            account.life = account.life.max(was.life);
            account.password_changes = account.password_changes.max(was.password_changes);
            account.revision = account.revision.max(was.revision);
            account.music_login = account.music_login.max(was.music_login);
            account.music_recovery = account.music_recovery.or(was.music_recovery);
            account.password_reset_required |= was.password_reset_required;
            account.code_available &= was.code_available;
            account.history_held |= was.history_held;
            for (id, record) in &kept.devices {
                let entry = devices.entry(*id).or_insert_with(|| ApartDevice { permit: None, ..record.clone() });
                entry.number = entry.number.max(record.number);
                entry.last_session = entry.last_session.max(record.last_session);
                entry.last_leave = entry.last_leave.max(record.last_leave);
            }
        }
    }
    Apart { account, devices }
}

#[cfg(test)]
mod tests;
