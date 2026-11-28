//! The backend's two stores together (design 12.3, 12.4, 19.3): the data and the store kept
//! apart, opened, compared and brought together **before anything is served**; the steps
//! later slices call, each all or nothing and in the order 12.3 sets; and the owner's two
//! commands.
//!
//! **The order, every time (12.3, and C3r's `Effects`):**
//! 1. What takes leave away (each update's `apart_first`) is written to the store kept apart,
//!    with the latest time acted at and a note naming the accounts of the change under way.
//! 2. Then the data, in one step.
//! 3. Then the store again: what gives leave (`apart_last`), and the note cleared.
//!
//! A cut between any two is told from an older copy by the comparison at the next start
//! (`core::nexus::compare`), and finished. A step that fails part-way leaves the keeper
//! unable to write until it has brought the two together again (`heal`).
//!
//! **The seam with C3r is kept narrow:** this stores its `Apart` and `Rows` for each account,
//! loads an `Account` from them, and keeps what its `Effects` say, in order. Nothing here
//! decides who may do what; what a restore does to an account is `Account::restore`'s.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use nexus_core::nexus::compare::{self, Contradiction, Pending};
use nexus_core::nexus::kept::{
    Account as Decided, AccountName, AccountState, Apart, ApartDevice, Clock, DataChange, Effects, RestoreMode, Rows, SECRET_BYTES, StampKey, Startup, Status,
};
use nexus_core::nexus::proto::BackendId;

use crate::identity;
use crate::store::apart::{self, Account, Reading, Store};
use crate::store::copies;
use crate::store::data::{self, Change, Data, Opened, why};
use crate::store::lock;
use crate::store::private;

/// How often, at most, the store hears how far the data's changes have got from a change the
/// data alone has (12.3).
pub const MARK_EVERY_MS: u64 = 60_000;

/// Why the backend does not serve, or a command did nothing: a sentence for the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A step that did not happen, or did not finish: the caller answers "busy" or "nothing",
/// and nothing it was asked for is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed(pub String);

/// What the start found and did, for the log (19.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
    Made,
    Ready,
    /// A change cut between the two stores, finished (`FinishEnding`) or a grant the store
    /// never heard of dropped (`DiscardGrant`).
    Finished(Startup),
    /// The data was older than the store, or was put back, or taken as it is.
    Restored { restore: u64, accounts: usize, held: usize, from: u64 },
}

impl Started {
    /// The log's line. No email: accounts are counted.
    pub fn said(&self) -> String {
        match self {
            Started::Made => "The data and the store kept apart were made now.".into(),
            Started::Ready => "The data and the store kept apart were compared, and are the same age.".into(),
            Started::Finished(Startup::DiscardGrant) => {
                "The data and the store kept apart were compared: what was given in the data and not yet heard of in the store, cut off as the program stopped, was taken back.".into()
            }
            Started::Finished(_) => "The data and the store kept apart were compared: what the store had taken away and the data not yet, cut off as the program stopped, was finished in the data.".into(),
            Started::Restored { restore, accounts, held, from } => format!(
                "The data was found to be older than the store kept apart (its last step was at {from} ms), or was put back. It now has restore number {restore}; every session of every account was ended ({accounts} accounts), and what the store says was taken away since was taken away again; {held} accounts' listening data is held until each person answers."
            ),
        }
    }
}

/// The backend's two stores, open and agreeing. One program has them at a time (its locks),
/// and one thread writes through this (`&mut self`).
pub struct Keeper {
    state: PathBuf,
    folder: PathBuf,
    _lock: lock::Held,
    backend: BackendId,
    store: Store,
    data: Data,
    /// How far the data's changes have got, as last committed.
    changes: u64,
    broken: bool,
}

fn random_bytes<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|why| format!("No randomness to make a key with ({why})."))?;
    Ok(bytes)
}

fn store_failed(state: &Path, error: &std::io::Error) -> String {
    format!("The store kept apart couldn't be written in {} ({}).", state.display(), error.kind())
}

fn data_failed(error: &rusqlite::Error) -> String {
    format!("The data couldn't be written: {}.", why(error))
}

fn unreadable(error: &rusqlite::Error) -> Refused {
    Refused(format!("The data can't be read: {}.", why(error)))
}

fn contradiction(why: Contradiction) -> &'static str {
    match why {
        Contradiction::DataRestoreAhead => "the data has a restore number the store never gave",
        Contradiction::AccountUnknown => "the data has an account the store never made",
        Contradiction::AccountAhead => "the data has an account further on than the store",
        Contradiction::DeviceUnknown => "the data has a device the store never made",
        Contradiction::DeviceAhead => "the data has a device further on than the store",
        Contradiction::SessionNeverReserved => "the data has a session the store never made room for",
    }
}

const TAKE_AS_IS: &str = "run `nexus-music-server take-as-is` to take the data as it is: every session is ended, and every person signs in again";

struct Found {
    id: Option<BackendId>,
    store: Reading,
    data: Opened,
}

fn find(state: &Path, folder: &Path, now: u64) -> Result<Found, Refused> {
    apart::tidy(state);
    data::tidy(folder);
    copies::prune(folder);
    let id = identity::read(state).map_err(Refused)?;
    let store = apart::read(state);
    let data = Data::open(folder, now).map_err(Refused)?;
    Ok(Found { id, store, data })
}

fn not_this_backends(state: &Path) -> String {
    format!(
        "The store kept apart, {}, is not this backend's: it names another backend's id than {} does, or that file is missing. Nothing is served. If NEXUS_STATE names the wrong folder, set it right; if the store is lost, {TAKE_AS_IS}.",
        apart::place(state).display(),
        state.join(identity::FILE).display()
    )
}

impl Keeper {
    fn with(state: &Path, folder: &Path, held: lock::Held, store: Store, data: Data) -> Result<Keeper, Refused> {
        let changes = data.meta().map_err(|error| unreadable(&error))?.changes;
        Ok(Keeper { state: state.to_path_buf(), folder: folder.to_path_buf(), _lock: held, backend: store.backend, store, data, changes, broken: false })
    }

    /// Opens both stores in `state` and `folder` (`NEXUS_STATE`, `NEXUS_DATA`) at `now`, and
    /// compares them, **before anything is served** (12.3, 12.4).
    pub fn open(state: &Path, folder: &Path, now: u64) -> Result<(Keeper, Started), Refused> {
        let held = lock::hold(state, folder).map_err(Refused)?;
        private::at_start(state, folder).map_err(Refused)?;
        let found = find(state, folder, now)?;
        match (found.id, found.store, found.data) {
            (_, Reading::Unreadable(what), _) => Err(Refused(format!(
                "The store kept apart, {}, can't be read: {what}. Nothing is served, and it is left as it is. If it is lost, {TAKE_AS_IS}.",
                apart::place(state).display()
            ))),
            (_, Reading::Missing, Opened::Open(_)) => Err(Refused(format!(
                "The store kept apart, {}, is missing, and there is data. Nothing is served: without the store nothing says which sessions were ended, which accounts switched off or deleted, or which passwords changed. If NEXUS_STATE names the wrong folder, set it right. If the store is lost, {TAKE_AS_IS}.",
                apart::place(state).display()
            ))),
            (id, Reading::Missing, Opened::Absent) => {
                let id = match id {
                    Some(id) => id,
                    None => identity::read_or_make(state, random_bytes::<16>).map_err(Refused)?.0,
                };
                let store = Store::new(id, random_bytes::<SECRET_BYTES>().map_err(Refused)?, random_bytes::<SECRET_BYTES>().map_err(Refused)?, now);
                apart::write(state, &store).map_err(|error| Refused(store_failed(state, &error)))?;
                Self::first_data(state, folder, held, store, now)
            }
            (id, Reading::Found(store), Opened::Absent) => {
                if id != Some(store.backend) {
                    return Err(Refused(not_this_backends(state)));
                }
                if !store.data_made {
                    return Self::first_data(state, folder, held, store, now);
                }
                if matches!(store.pending, Some(Pending::Restore { .. })) {
                    return Err(Refused(format!(
                        "A put-back was cut off before the copy was in place: there is no data in {}. Run `nexus-music-server put-back` with the copy again. (The data it was replacing is in {}.)",
                        folder.display(),
                        copies::folder(folder).display()
                    )));
                }
                // 12.4, and the review: an emptied data folder with a store is never a first start.
                Err(Refused(format!(
                    "There is no data in {}, but the store kept apart says there was. Nothing is served. If NEXUS_DATA names the wrong folder, set it right. If the data is lost, put a copy back with `nexus-music-server put-back`, or {TAKE_AS_IS} (here: none, and every account is made again by the admin).",
                    folder.display()
                )))
            }
            (id, Reading::Found(store), Opened::Open(data)) => {
                let meta = data.meta().map_err(|error| unreadable(&error))?;
                if id != Some(store.backend) {
                    return Err(Refused(not_this_backends(state)));
                }
                if meta.backend != store.backend {
                    return Err(Refused(format!(
                        "The data in {} is another backend's (its id is {}; this one's is {}). Nothing is served, and nothing was changed. Set NEXUS_DATA to this backend's data, or put back a copy of it.",
                        folder.display(),
                        meta.backend.to_hex(),
                        store.backend.to_hex()
                    )));
                }
                // The clock is the store's alone (finding 7): every step of the data is
                // written at a time the store had already recorded, so the data's own time
                // is never taken for it.
                let mut keeper = Self::with(state, folder, held, store, data)?;
                if !keeper.store.data_made {
                    let view = keeper.data.view().map_err(|error| unreadable(&error))?;
                    if !view.accounts.is_empty() || view.restore != keeper.store.restore {
                        return Err(Refused(format!("The store kept apart says this backend's data was never made, and there is data. Nothing is served. {TAKE_AS_IS}.")));
                    }
                    let mut next = keeper.store.clone();
                    next.data_made = true;
                    keeper.write_store(next).map_err(|failed| Refused(failed.0))?;
                    return Ok((keeper, Started::Made));
                }
                let started = keeper.compare_and_bring_together(now)?;
                Ok((keeper, started))
            }
        }
    }

    fn first_data(state: &Path, folder: &Path, held: lock::Held, mut store: Store, now: u64) -> Result<(Keeper, Started), Refused> {
        let data = Data::create(folder, &store.backend, now).map_err(Refused)?;
        store.data_made = true;
        apart::write(state, &store).map_err(|error| Refused(store_failed(state, &error)))?;
        Ok((Self::with(state, folder, held, store, data)?, Started::Made))
    }

    fn names(&self) -> Result<BTreeMap<u64, AccountName>, rusqlite::Error> {
        self.data.names()
    }

    fn compare_and_bring_together(&mut self, now: u64) -> Result<Started, Refused> {
        let names = self.names().map_err(|error| unreadable(&error))?;
        let view = self.data.view().map_err(|error| unreadable(&error))?;
        let found = compare::compare(&self.store.view(&names), &view);
        match found.startup(false) {
            Startup::Ready => {
                if self.store.pending.is_some() {
                    self.bring_up(now).map_err(|failed| Refused(failed.0))?;
                }
                Ok(Started::Ready)
            }
            plan @ (Startup::FinishEnding | Startup::DiscardGrant) => {
                self.bring_up(now).map_err(|failed| Refused(failed.0))?;
                Ok(Started::Finished(plan))
            }
            Startup::RestoreData if self.put_back_cut() => Err(Refused(format!(
                "A put-back was cut off before it was done, and the copy may or may not be in place. Nothing is served. Run `nexus-music-server put-back` with the copy again. (Any data it was replacing is kept in {}.)",
                copies::folder(&self.folder).display()
            ))),
            Startup::RestoreData => self.restore_data(now, RestoreMode::DataOnly),
            Startup::Stop | Startup::AdoptData => Err(Refused(format!(
                "The store kept apart does not belong with this data: {}. Nothing is served, and nothing was changed. If NEXUS_DATA or NEXUS_STATE names the wrong folder, set it right; otherwise {TAKE_AS_IS}.",
                found.contradiction.map_or("they disagree", contradiction)
            ))),
        }
    }

    fn put_back_cut(&self) -> bool {
        matches!(self.store.pending, Some(Pending::Restore { restore }) if self.store.restore.checked_add(1) == Some(restore))
    }

    /// What must also go from the data with an account's state (C3r's): a password that may no
    /// longer be used, and codes that are no longer live.
    fn tidied<'a>(number: u64, state: &AccountState, changes: &mut Vec<Change<'a>>) {
        if state.password_reset_required {
            changes.push(Change::Password { account: number, password: None });
        }
        if !state.code_available {
            changes.push(Change::Codes { account: number });
        }
    }

    /// 12.3: "the data is brought up to it". Each account under the note (or every account)
    /// loaded by C3r from both stores, and both written as loaded: the store first (loading
    /// only ever takes away), then the data; then the note cleared.
    fn bring_up(&mut self, now: u64) -> Result<(), Failed> {
        let names = self.names().map_err(|error| Failed(data_failed(&error)))?;
        let meta = self.data.meta().map_err(|error| Failed(data_failed(&error)))?;
        let mut next = self.store.clone();
        let mut installs: Vec<(u64, Rows)> = Vec::new();
        let mut erase = Vec::new();
        for (&number, account) in &self.store.accounts {
            let rows = self.data.rows(&meta, number).map_err(|error| Failed(data_failed(&error)))?;
            if account.apart.account.status == Status::Deleted {
                if rows.is_some() {
                    erase.push(number);
                }
                continue;
            }
            let Some(rows) = rows else {
                // Made in the store first and never in the data: its number is not used again.
                if let Some(entry) = next.accounts.get_mut(&number) {
                    entry.apart.account.status = Status::Deleted;
                }
                continue;
            };
            let Some(record) = self.store.apart(number, names.get(&number)) else { continue };
            let loaded = Decided::load(&record, Some(&rows));
            if let Some(entry) = next.accounts.get_mut(&number) {
                entry.apart = loaded.apart();
            }
            let up = loaded.rows();
            if up != rows {
                installs.push((number, up));
            }
        }
        next.high_water = next.high_water.max(now);
        self.write_store(next)?;
        let at = self.store.high_water;
        let mut changes: Vec<Change<'_>> = erase.iter().map(|&account| Change::Erase { account }).collect();
        for (number, rows) in &installs {
            changes.push(Change::Install { number: *number, rows, at });
            Self::tidied(*number, &rows.account, &mut changes);
        }
        if !changes.is_empty() {
            self.changes = self.data.apply(&changes, at).map_err(|error| Failed(data_failed(&error)))?;
        }
        let mut next = self.store.clone();
        next.pending = None;
        next.mark = self.changes;
        next.mark_at = at;
        self.write_store(next)
    }

    /// 12.4 steps 1 to 4, still before answering anything: a new restore number; every
    /// session of every account ended, and what the store says was taken away since taken away
    /// again (each account by C3r's `Account::restore`); in the store first, then the data.
    /// Safe to do twice.
    fn restore_data(&mut self, now: u64, mode: RestoreMode) -> Result<Started, Refused> {
        let refused = |failed: Failed| Refused(failed.0);
        // Taking the data as it is, cut off and done again at a start, is still that.
        let mode = if matches!(self.store.pending, Some(Pending::TakeAsIs { .. })) { RestoreMode::Adopt } else { mode };
        let names = self.names().map_err(|error| unreadable(&error))?;
        let meta = self.data.meta().map_err(|error| unreadable(&error))?;
        let new = match self.store.pending {
            Some(Pending::Restore { restore } | Pending::TakeAsIs { restore }) => restore,
            _ => self.store.restore.checked_add(1).ok_or_else(|| Refused("The restore number can go no higher. Nothing is served.".into()))?,
        };
        if self.store.restore != new {
            let mut next = self.store.clone();
            next.high_water = next.high_water.max(now);
            let clock = Clock { now, high_water: next.high_water };
            for number in self.store.accounts.keys().copied().collect::<Vec<_>>() {
                let rows = self.data.rows(&meta, number).map_err(|error| unreadable(&error))?;
                let Some(record) = self.store.apart(number, names.get(&number)) else { continue };
                if record.account.status == Status::Deleted {
                    continue;
                }
                if rows.is_none() {
                    // Not in the copy: the account is lost with it, and its number not used again.
                    if let Some(entry) = next.accounts.get_mut(&number) {
                        entry.apart.account.status = Status::Deleted;
                    }
                    continue;
                }
                let mut first = None;
                let answer = Decided::restore(&record, rows.as_ref(), new, mode, clock).apply(|effects| {
                    first = effects.updates.first().map(|update| update.apart_first.clone());
                    Ok::<_, ()>(())
                });
                match (answer, first) {
                    (Ok(Ok(())), Some(first)) => {
                        if let Some(entry) = next.accounts.get_mut(&number) {
                            entry.apart = first;
                        }
                    }
                    _ => return Err(Refused("An account could not be brought to the new restore number (a number can go no higher). Nothing is served.".into())),
                }
            }
            next.restore = new;
            next.pending = Some(if mode == RestoreMode::Adopt { Pending::TakeAsIs { restore: new } } else { Pending::Restore { restore: new } });
            self.write_store(next).map_err(refused)?;
        }
        // Begun before and done again: the time it is done at reaches the store first.
        if now > self.store.high_water {
            let mut next = self.store.clone();
            next.high_water = now;
            self.write_store(next).map_err(refused)?;
        }
        // Then the data, in one step, at the time the store has.
        let at = self.store.high_water;
        let mut installs: Vec<(u64, Rows)> = Vec::new();
        let mut erase = Vec::new();
        for (&number, account) in &self.store.accounts {
            let rows = self.data.rows(&meta, number).map_err(|error| unreadable(&error))?;
            let Some(rows) = rows else { continue };
            if account.apart.account.status == Status::Deleted {
                erase.push(number);
                continue;
            }
            let Some(record) = self.store.apart(number, names.get(&number)) else { continue };
            installs.push((number, Decided::load(&record, Some(&rows)).rows()));
        }
        let mut changes = vec![Change::Restore { restore: new, at, from: (meta.restore != new).then_some(meta.written_at) }];
        changes.extend(erase.iter().map(|&account| Change::Erase { account }));
        for (number, rows) in &installs {
            changes.push(Change::Install { number: *number, rows, at });
            Self::tidied(*number, &rows.account, &mut changes);
        }
        self.changes = self.data.apply(&changes, at).map_err(|error| Refused(data_failed(&error)))?;
        let held = installs.iter().filter(|(_, rows)| rows.account.history_held).count();
        // Then the store as C3r loads the two now: they agree from here (what loading takes
        // away is only ever taken away).
        let mut next = self.store.clone();
        for (number, rows) in &installs {
            let name = Some(&rows.account.name);
            if let (Some(record), Some(entry)) = (self.store.apart(*number, name), next.accounts.get_mut(number)) {
                entry.apart = Decided::load(&record, Some(rows)).apart();
            }
        }
        next.pending = None;
        next.mark = self.changes;
        next.mark_at = at;
        self.write_store(next).map_err(refused)?;
        Ok(Started::Restored { restore: new, accounts: installs.len(), held, from: meta.written_at })
    }

    fn write_store(&mut self, next: Store) -> Result<(), Failed> {
        // Room above the growth limit is kept for revocations, which add no record.
        let limit = if next.records() > self.store.records() { apart::GROWTH_MAX_BYTES } else { apart::FILE_MAX_BYTES };
        match apart::write_within(&self.state, &next, limit) {
            Ok(()) => {
                self.store = next;
                Ok(())
            }
            Err(error) => {
                if let Reading::Found(on_disk) = apart::read(&self.state) {
                    self.store = on_disk;
                }
                Err(Failed(store_failed(&self.state, &error)))
            }
        }
    }

    fn heal(&mut self) -> Result<(), Failed> {
        if !self.broken {
            return Ok(());
        }
        match apart::read(&self.state) {
            Reading::Found(store) => self.store = store,
            _ => return Err(Failed("The store kept apart can't be read back.".into())),
        }
        self.changes = self.data.meta().map_err(|error| Failed(data_failed(&error)))?.changes;
        let names = self.names().map_err(|error| Failed(data_failed(&error)))?;
        let view = self.data.view().map_err(|error| Failed(data_failed(&error)))?;
        match compare::compare(&self.store.view(&names), &view).startup(false) {
            Startup::Ready | Startup::FinishEnding | Startup::DiscardGrant => {
                let now = self.store.high_water;
                self.bring_up(now)?;
                self.broken = false;
                Ok(())
            }
            // The design does not say what a running backend does if its data goes back under
            // it. It stops writing; the next start compares and acts.
            _ => Err(Failed("The data and the store kept apart disagree while the backend runs; it writes nothing until it is started again.".into())),
        }
    }

    fn broke<T>(&mut self, failed: Failed) -> Result<T, Failed> {
        self.broken = true;
        let _ = self.heal();
        Err(failed)
    }

    // ---- What later slices call ----

    pub fn backend(&self) -> BackendId {
        self.backend
    }

    /// The restore number (12.3): every request but `hello` and signing in names it.
    pub fn restore(&self) -> u64 {
        self.store.restore
    }

    pub fn stamp_key(&self) -> StampKey {
        self.store.stamp_key()
    }

    /// The key that seals (S9).
    pub fn seal_key(&self) -> [u8; SECRET_BYTES] {
        self.store.seal_key()
    }

    /// The clock for a decision at `now` (C3r's `Clock`).
    pub fn clock(&self, now: u64) -> Clock {
        Clock { now, high_water: self.store.high_water }
    }

    /// Whether the backend has been set up (8.4).
    pub fn set_up(&self) -> bool {
        self.store.set_up
    }

    /// The backend is set up: written to the store first, before the first admin is made.
    pub fn mark_set_up(&mut self, now: u64) -> Result<(), Failed> {
        self.heal()?;
        let mut next = self.store.clone();
        next.set_up = true;
        next.high_water = next.high_water.max(now);
        self.write_store(next)
    }

    /// The number of the account with this email (as tidied: C7's), if the data has it.
    pub fn account_named(&self, email: &str) -> Result<Option<u64>, Failed> {
        self.data.account_named(email).map_err(|error| Failed(data_failed(&error)))
    }

    /// An account as C3r decides with it, loaded from both stores.
    pub fn load(&mut self, number: u64) -> Result<Decided, Failed> {
        self.heal()?;
        let meta = self.data.meta().map_err(|error| Failed(data_failed(&error)))?;
        let rows = self.data.rows(&meta, number).map_err(|error| Failed(data_failed(&error)))?;
        let name = rows.as_ref().map(|rows| rows.account.name.clone());
        let record = self.store.apart(number, name.as_ref()).ok_or_else(|| Failed("There is no such account.".into()))?;
        Ok(Decided::load(&record, rows.as_ref()))
    }

    /// A device id never given before, for a sign-in that may make a device (C3r's `vacant`).
    /// It is made lasting with the store's next write.
    pub fn vacant_device(&mut self) -> u64 {
        let id = self.store.next_device;
        self.store.next_device = id.saturating_add(1);
        id
    }

    /// An account made (by the admin, 8.1): in the store first, then the data. It has no
    /// password; it waits for one to be chosen with the one-time code given (`code`, its
    /// fingerprint, good until `until`).
    pub fn make_account(&mut self, email: &str, shown: &str, admin: bool, code: Option<(&[u8; 32], u64)>, now: u64) -> Result<u64, Failed> {
        self.heal()?;
        let name = AccountName::from_canonical(email.to_string()).ok_or_else(|| Failed("That is not an email as the backend keeps one.".into()))?;
        if let Some(number) = self.account_named(email)? {
            return Ok(number);
        }
        let mut next = self.store.clone();
        let number = next.next_account;
        next.next_account = number.checked_add(1).ok_or_else(|| Failed("No more accounts can be made.".into()))?;
        // The design does not say what an account's first state is; C3r has no constructor.
        // Switched on, no password, a code live if one was given.
        let state = AccountState {
            backend: self.backend,
            restore: next.restore,
            name,
            life: 0,
            status: Status::On,
            password_changes: 0,
            password_reset_required: true,
            code_available: code.is_some(),
            admin,
            music_login: 0,
            music_managed: false,
            music_recovery: None,
            history_held: false,
            revision: 0,
        };
        next.accounts.insert(number, Account { name: apart::name_fingerprint(email), apart: Apart { account: state.clone(), devices: BTreeMap::new() } });
        next.high_water = next.high_water.max(now);
        next.mark = self.changes;
        next.pending = Some(Pending::Change { accounts: vec![number] });
        self.write_store(next)?;
        let at = self.store.high_water;
        let mut changes = vec![Change::Account { number, shown, state: &state }];
        if let Some((code, until)) = code {
            changes.push(Change::Code { account: number, code, made_at: at, until });
        }
        match self.data.apply(&changes, at) {
            Ok(changes) => self.changes = changes,
            Err(error) => return self.broke(Failed(data_failed(&error))),
        }
        let mut next = self.store.clone();
        next.pending = None;
        next.mark = self.changes;
        next.mark_at = at;
        if let Err(failed) = self.write_store(next) {
            return self.broke(failed);
        }
        Ok(number)
    }

    fn number_of(&self, update_name: &AccountName) -> Result<u64, Failed> {
        if let Some(number) = self.data.account_named(update_name.as_str()).map_err(|error| Failed(data_failed(&error)))? {
            return Ok(number);
        }
        self.store.account_named(update_name.as_str()).ok_or_else(|| Failed("A decision named an account there is not.".into()))
    }

    /// Keeps what a decision says is to be kept (C3r's `Effects`), **whatever it answered**,
    /// and before its answer is given, in 12.3's order. `extra` is what the caller adds to the
    /// data's step for the change (S8: a password's new fingerprint for `ChangePassword`, a
    /// code for `ResetPassword`; a device's description). If this fails, the answer is not
    /// given and the account is loaded again before it is used.
    pub fn persist(&mut self, effects: &Effects, extra: &[Change<'_>], now: u64) -> Result<(), Failed> {
        self.heal()?;
        let high_water = self.store.high_water.max(effects.high_water);
        if effects.updates.is_empty() {
            let due = now.saturating_sub(self.store.mark_at) >= MARK_EVERY_MS || now < self.store.mark_at;
            if high_water > self.store.high_water || (self.changes > self.store.mark && due) {
                let mut next = self.store.clone();
                next.high_water = high_water;
                if due {
                    next.mark = self.changes;
                    next.mark_at = now;
                }
                self.write_store(next)?;
            }
            return Ok(());
        }
        let mut numbers = Vec::new();
        for update in &effects.updates {
            numbers.push(self.number_of(&update.apart_first.account.name)?);
        }
        let mut next = self.store.clone();
        next.high_water = high_water;
        next.mark = self.changes;
        for (update, &number) in effects.updates.iter().zip(&numbers) {
            let entry = next.accounts.get_mut(&number).ok_or_else(|| Failed("A decision named an account there is not.".into()))?;
            entry.apart = update.apart_first.clone();
            if let Some(&highest) = update.apart_first.devices.keys().max() {
                next.next_device = next.next_device.max(highest.saturating_add(1));
            }
        }
        next.pending = Some(Pending::Change { accounts: numbers.clone() });
        self.write_store(next)?;
        let at = self.store.high_water;
        let mut changes: Vec<Change<'_>> = Vec::new();
        for (update, &number) in effects.updates.iter().zip(&numbers) {
            if update.change == DataChange::DeleteAccount || update.apart_first.account.status == Status::Deleted {
                changes.push(Change::Erase { account: number });
                continue;
            }
            changes.push(Change::Install { number, rows: &update.data, at });
            match update.change {
                DataChange::ResetPassword => changes.push(Change::Password { account: number, password: None }),
                DataChange::SpendCode => changes.push(Change::Codes { account: number }),
                // Its events go here once they are kept (S12).
                DataChange::EraseHistory => {}
                _ => {}
            }
            if !update.data.account.code_available {
                changes.push(Change::Codes { account: number });
            }
        }
        changes.extend(extra.iter().cloned());
        match self.data.apply(&changes, at) {
            Ok(changes) => self.changes = changes,
            Err(error) => return self.broke(Failed(data_failed(&error))),
        }
        let mut next = self.store.clone();
        for (update, &number) in effects.updates.iter().zip(&numbers) {
            if let Some(entry) = next.accounts.get_mut(&number) {
                if entry.apart.account.status != Status::Deleted {
                    entry.apart = update.apart_last.clone();
                }
            }
        }
        next.pending = None;
        next.mark = self.changes;
        next.mark_at = at;
        if let Err(failed) = self.write_store(next) {
            return self.broke(failed);
        }
        if effects.updates.iter().any(|update| update.change == DataChange::DeleteAccount) {
            let _ = self.data.fold();
        }
        Ok(())
    }

    /// A change the data alone keeps (a device last heard from), in one step; the store
    /// hears how far the data has got at most once a minute (12.3). It is no decision and
    /// moves no clock: the step is written at the time the store already has (finding 7),
    /// so that a restart finds the same clock the running backend had.
    pub fn heard(&mut self, device: u64, now: u64) -> Result<(), Failed> {
        self.heal()?;
        match self.data.apply(&[Change::Heard { device, at: now }], self.store.high_water) {
            Ok(changes) => self.changes = changes,
            Err(error) => return Err(Failed(data_failed(&error))),
        }
        let effects = Effects { high_water: self.store.high_water, updates: Vec::new() };
        self.persist(&effects, &[], now)
    }

    /// The data, to read from (nothing is written through it).
    pub fn data(&self) -> &Data {
        &self.data
    }

    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// When the store is next to hear how far the data's changes have got, if the data has
    /// changes it has not heard of: the one thread that writes calls [`Keeper::idle`] then,
    /// whether or not anything else is asked of it (12.3; the review's finding 6).
    pub fn mark_due(&self) -> Option<u64> {
        (self.changes > self.store.mark).then(|| self.store.mark_at.saturating_add(MARK_EVERY_MS))
    }

    /// In idle time: the store hears how far the data's changes have got, if it has not and
    /// a minute has passed since it last did (or the clock went back). It is no decision and
    /// moves no clock.
    pub fn idle(&mut self, now: u64) -> Result<(), Failed> {
        self.heal()?;
        let due = now.saturating_sub(self.store.mark_at) >= MARK_EVERY_MS || now < self.store.mark_at;
        if self.changes > self.store.mark && due {
            self.write_mark(now)?;
        }
        Ok(())
    }

    fn write_mark(&mut self, at: u64) -> Result<(), Failed> {
        let mut next = self.store.clone();
        next.mark = self.changes;
        next.mark_at = at;
        self.write_store(next)
    }

    /// Stops: the store hears how far the data's changes have got, whatever the minute, so
    /// that a copy from before the last of them is found older; then the data is folded
    /// back into its one file and closed.
    pub fn close(mut self) {
        if !self.broken && self.changes > self.store.mark {
            let at = self.store.mark_at.max(self.store.high_water);
            if let Err(failed) = self.write_mark(at) {
                eprintln!("As the backend stopped, {}", failed.0);
            }
        }
        self.data.close();
    }
}

// ---- The owner's two commands (19.2, 12.4) ----

pub type Said = Vec<String>;

/// `put-back <copy>`: puts a copy of the data back by the supported way (12.4). The copy is
/// checked first; the data it replaces is kept in `copies`; then, before anything is served,
/// 12.4's steps. Refused while the backend serves.
pub fn put_back(state: &Path, folder: &Path, copy: &Path, now: u64) -> Result<Said, Refused> {
    let held = lock::hold(state, folder).map_err(Refused)?;
    private::at_start(state, folder).map_err(Refused)?;
    apart::tidy(state);
    data::tidy(folder);
    let mut store = match apart::read(state) {
        Reading::Found(store) => store,
        Reading::Missing => {
            return Err(Refused(format!(
                "The store kept apart, {}, is missing, so a copy can't be put back this way. Nothing was changed. Put the copy at {} yourself and {TAKE_AS_IS}.",
                apart::place(state).display(),
                data::place(folder).display()
            )));
        }
        Reading::Unreadable(what) => return Err(Refused(format!("The store kept apart, {}, can't be read: {what}. Nothing was changed.", apart::place(state).display()))),
    };
    if identity::read(state).map_err(Refused)? != Some(store.backend) {
        return Err(Refused(not_this_backends(state)));
    }
    let source = data::open_to_read(copy, "The copy").map_err(Refused)?;
    let (backend, written_at) = data::identify(&source).map_err(|error| Refused(format!("The copy can't be read: {}.", why(&error))))?;
    if backend != store.backend {
        return Err(Refused(format!("The copy is another backend's data (its id is {}; this one's is {}). Nothing was changed.", backend.to_hex(), store.backend.to_hex())));
    }
    let new = match store.pending {
        Some(Pending::Restore { restore }) if store.restore.checked_add(1) == Some(restore) => restore,
        _ => store.restore.checked_add(1).ok_or_else(|| Refused("The restore number can go no higher. Nothing was changed.".into()))?,
    };
    store.pending = Some(Pending::Restore { restore: new });
    apart::write(state, &store).map_err(|error| Refused(store_failed(state, &error)))?;
    let mut said = Vec::new();
    let place = data::place(folder);
    if place.exists() {
        if let Ok(Opened::Open(current)) = Data::open(folder, now) {
            current.close();
        }
        let copies_folder = copies::folder(folder);
        fs::create_dir_all(&copies_folder).map_err(|error| Refused(format!("The folder for copies can't be made ({}).", error.kind())))?;
        let aside = copies_folder.join(format!("replaced-{now}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let from = PathBuf::from(format!("{}{suffix}", place.display()));
            if from.exists() {
                fs::rename(&from, PathBuf::from(format!("{}{suffix}", aside.display())))
                    .map_err(|error| Refused(format!("The data being replaced couldn't be moved aside ({}). The put-back is not done: run it again.", error.kind())))?;
            }
        }
        fs::File::open(folder).and_then(|folder| folder.sync_all()).map_err(|error| Refused(format!("The data folder couldn't be made lasting ({}).", error.kind())))?;
        said.push(format!("The data it replaced is kept as {}.", aside.display()));
    }
    copies::put(&source, &place).map_err(|why| Refused(format!("The copy couldn't be put in place: {why}. Run the put-back again.")))?;
    drop(source);
    let data = match Data::open(folder, now).map_err(Refused)? {
        Opened::Open(data) => data,
        Opened::Absent => return Err(Refused("The copy was not in place once put there. Run the put-back again.".into())),
    };
    let mut keeper = Keeper::with(state, folder, held, store, data)?;
    let started = keeper.restore_data(now, RestoreMode::DataOnly)?;
    keeper.close();
    said.insert(0, format!("Put back {} (its data's last step was at {written_at} ms).", copy.display()));
    said.push(started.said());
    said.push("Start the backend again. Every person signs in again.".into());
    Ok(said)
}

/// `take-as-is`: takes the data as it is when the store kept apart is lost, cannot be read,
/// or does not belong with the data (12.4): a new restore number, every session ended, and
/// nothing reset that it cannot know of. With no data, it starts again with none (every
/// account made again by the admin), keeping what the store has. Refused while the backend
/// serves, and when the store and the data agree.
pub fn take_as_is(state: &Path, folder: &Path, now: u64) -> Result<Said, Refused> {
    let held = lock::hold(state, folder).map_err(Refused)?;
    private::at_start(state, folder).map_err(Refused)?;
    let found = find(state, folder, now)?;
    match (found.id, found.store, found.data) {
        (_, Reading::Missing, Opened::Absent) => Err(Refused("There is neither data nor a store kept apart: there is nothing to take. Start the backend, and it makes both.".into())),
        (_, Reading::Unreadable(_), Opened::Absent) => {
            Err(Refused("There is no data, and the store kept apart can't be read: there is nothing to take. Put a copy of the data back by hand into NEXUS_DATA and run this again.".into()))
        }
        (id, Reading::Found(mut store), Opened::Absent) => {
            if id != Some(store.backend) {
                return Err(Refused(not_this_backends(state)));
            }
            let data = Data::create(folder, &store.backend, now).map_err(Refused)?;
            store.data_made = true;
            let restore = store.restore.checked_add(1).ok_or_else(|| Refused("The restore number can go no higher.".into()))?;
            store.pending = Some(Pending::Restore { restore });
            apart::write(state, &store).map_err(|error| Refused(store_failed(state, &error)))?;
            let mut keeper = Keeper::with(state, folder, held, store, data)?;
            let started = keeper.restore_data(now, RestoreMode::DataOnly)?;
            keeper.close();
            Ok(vec![format!("There was no data: the backend starts again with none, under restore number {restore}. The admin makes each account again."), started.said()])
        }
        (id, store, Opened::Open(data)) => {
            let meta = data.meta().map_err(|error| unreadable(&error))?;
            let names = data.names().map_err(|error| unreadable(&error))?;
            let view = data.view().map_err(|error| unreadable(&error))?;
            let old = match store {
                Reading::Found(store) => {
                    if id == Some(store.backend) && meta.backend == store.backend && store.data_made && compare::compare(&store.view(&names), &view).contradiction.is_none() {
                        return Err(Refused("The store kept apart is there and belongs with the data: there is nothing to take. Start the backend; it compares the two itself.".into()));
                    }
                    Some(store)
                }
                _ => None,
            };
            if id.is_some_and(|id| id != meta.backend) {
                return Err(Refused(format!(
                    "The data is another backend's: its id is {}, and {} says {}. Nothing was changed. Set NEXUS_DATA and NEXUS_STATE to one backend's folders; if this data is the one to serve, remove that file first.",
                    meta.backend.to_hex(),
                    state.join(identity::FILE).display(),
                    id.map_or_else(String::new, |id| id.to_hex())
                )));
            }
            if id.is_none() {
                identity::read_or_make(state, || Ok(*meta.backend.as_bytes())).map_err(Refused)?;
            }
            let mut said = vec!["Took the data as it is.".to_string()];
            if apart::place(state).exists() {
                let aside = apart::set_aside(state, now).map_err(|error| Refused(format!("The old store couldn't be put aside ({}). Nothing was changed.", error.kind())))?;
                said.push(format!("The old store kept apart is kept as {}.", aside.display()));
            }
            // A new store from the data as it is ("an apart baseline rebuilt by S3 from data",
            // C3r): what a readable old one knew that only goes up is kept.
            let seal = match &old {
                Some(old) => old.seal_key(),
                // Lost with the store: what it sealed is made anew or typed in again (12.4).
                None => random_bytes::<SECRET_BYTES>().map_err(Refused)?,
            };
            let high_water = now.max(meta.written_at).max(old.as_ref().map_or(0, |old| old.high_water));
            let mut store = Store::new(meta.backend, random_bytes::<SECRET_BYTES>().map_err(Refused)?, seal, high_water);
            let old_restore = old.as_ref().map_or(0, |old| old.restore);
            let new = meta.restore.max(old_restore).checked_add(1).ok_or_else(|| Refused("The restore number can go no higher.".into()))?;
            store.restore = meta.restore;
            store.data_made = true;
            store.set_up = old.as_ref().is_some_and(|old| old.set_up) || view.accounts.values().any(|rows| rows.account.admin);
            store.mark = meta.changes;
            store.next_account = view.accounts.keys().max().map_or(1, |n| n.saturating_add(1)).max(old.as_ref().map_or(1, |old| old.next_account));
            let highest_device = view.accounts.values().flat_map(|rows| rows.devices.keys()).max().copied();
            store.next_device = highest_device.map_or(1, |n| n.saturating_add(1)).max(old.as_ref().map_or(1, |old| old.next_device));
            if let Some(old) = &old {
                for (&number, account) in &old.accounts {
                    if !view.accounts.contains_key(&number) {
                        let mut tombstone = account.clone();
                        tombstone.apart.account.status = Status::Deleted;
                        store.accounts.insert(number, tombstone);
                    }
                }
            }
            for (&number, rows) in &view.accounts {
                let devices = rows
                    .devices
                    .iter()
                    .map(|(&id, device)| {
                        let session = device.session.as_ref();
                        let record = ApartDevice {
                            number: device.number,
                            last_session: session.map_or(0, |session| session.id),
                            last_leave: session.and_then(|session| session.leave).map_or(0, |leave| leave.fields().3),
                            permit: None,
                        };
                        (id, record)
                    })
                    .collect();
                store.accounts.insert(number, Account { name: apart::name_fingerprint(rows.account.name.as_str()), apart: Apart { account: rows.account.clone(), devices } });
            }
            store.pending = Some(Pending::TakeAsIs { restore: new });
            apart::write(state, &store).map_err(|error| Refused(store_failed(state, &error)))?;
            let mut keeper = Keeper::with(state, folder, held, store, data)?;
            let started = keeper.restore_data(now, RestoreMode::Adopt)?;
            keeper.close();
            said.push(started.said());
            said.push("Start the backend again.".into());
            Ok(said)
        }
    }
}
