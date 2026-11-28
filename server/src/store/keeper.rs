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

use nexus_core::nexus::account::{AccountId, DeviceId, Email, SECRET_BYTES, SessionId};
use nexus_core::nexus::diag;
use nexus_core::nexus::compare::{self, Contradiction, CopyIdentity, Halt, InPlace, NotPutBack, Pending, Resume};
use nexus_core::nexus::kept::{Account as Decided, AccountState, AdminAction, AdminTarget, Apart, Clock, DataChange, Effects, LiveCode, RestoreMode, Rows, StampKey, Startup, Status};
use nexus_core::nexus::proto::BackendId;

use crate::identity;
use crate::store::apart::{self, Account, LidarrKept, Reading, Store};
use nexus_core::nexus::admin::{InternalAddress, LidarrBinding, LidarrKeyCheck, SealedLidarrKey};
use crate::store::copies;
use crate::store::crash;
use crate::store::data::{self, Change, Data, Opened, why};
use crate::store::lock;
use crate::store::private;

/// How often, at most, the store hears how far the data's changes have got from a change the
/// data alone has (12.3).
pub const MARK_EVERY_MS: u64 = 60_000;

/// How many Diagnostics reports past their age one idle call removes, at most.
pub const EXPIRED_PER_IDLE: i64 = 500;

/// What a new account starts with ([`Keeper::make_account`]).
#[derive(Debug, Clone, Copy)]
pub enum First<'a> {
    Nothing,
    /// A one-time code's keyed fingerprint, and until when it is good.
    Code(&'a [u8; 32], u64),
    /// The first admin's password, as kept (set-up, 8.4).
    Password(&'a data::Password),
}

impl<'a> From<Option<(&'a [u8; 32], u64)>> for First<'a> {
    fn from(code: Option<(&'a [u8; 32], u64)>) -> Self {
        code.map_or(First::Nothing, |(code, until)| First::Code(code, until))
    }
}

/// The Lidarr key as [`Keeper::lidarr`] finds it: its binding now, and its seal if the store
/// holds one for exactly that binding.
#[derive(Debug)]
pub struct Lidarr {
    pub binding: Option<LidarrBinding>,
    pub sealed: Option<SealedLidarrKey>,
    pub(crate) has_saved_key: bool,
}

pub use crate::store::apart::RequestsSwitch;

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
    /// The latest time acted at, as the running backend has it (the lead's ruling of 28 Nov
    /// 13:59): written to the store with every step that changes a row, and otherwise once it
    /// is a minute ahead of what the store has (`Effects::clock_mark_due`).
    high_water: u64,
    /// The clock as loaded (`kept::Clock::after_load`): the store's time plus the minute it
    /// may lag by, a floor kept in this process alone and never written back (core-fu's M1).
    loaded: Clock,
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

/// The number of an account's live code, if one is live (its fingerprint is kept; any other goes).
fn live(code: Option<LiveCode>) -> Option<u64> {
    code.filter(|code| !code.used).map(|code| code.number)
}

const TAKE_AS_IS: &str = "run `nexus-music-server take-as-is` to take the data as it is: every session is ended, and, since nothing then says which passwords changed, every account waits for a new one-time code (`nexus-music-server recover <email>` makes one)";

/// Whose data is in `folder`, if there is a data file: read before anything of it is
/// changed (the review's finding 9).
fn whose(folder: &Path) -> Result<Option<BackendId>, Refused> {
    let place = data::place(folder);
    match fs::symlink_metadata(&place) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Refused(format!("The data at {} can't be read ({}).", place.display(), error.kind()))),
        Ok(_) => data::backend_of(&place).map(Some).map_err(Refused),
    }
}

fn another_backends(folder: &Path, theirs: BackendId, ours: BackendId) -> Refused {
    Refused(format!(
        "The data in {} is another backend's (its id is {}; this one's is {}). Nothing is served, and nothing was changed. Set NEXUS_DATA to this backend's data, or put back a copy of it.",
        folder.display(),
        theirs.to_hex(),
        ours.to_hex()
    ))
}

fn halted(halt: Halt, folder: &Path) -> Refused {
    Refused(match halt {
        Halt::CopyLost => format!(
            "A put-back was cut off, and the copy it was putting back is neither waiting beside the data nor in its place in {}. Nothing is served. Run `nexus-music-server put-back` with the copy again. (The data it was replacing is kept in {}.)",
            folder.display(),
            copies::folder(folder).display()
        ),
        Halt::NoData => format!(
            "A restore of the data was under way, and there is no data in {}. Nothing is served. Put a copy back with `nexus-music-server put-back`, or {TAKE_AS_IS}.",
            folder.display()
        ),
        Halt::NotTheCopy => format!(
            "A put-back was cut off, and the copy waiting beside the data, or the data now in {}, is not the copy it planned. Nothing is served and nothing was changed. If NEXUS_DATA names the wrong folder, set it right; otherwise put the copy back again.",
            folder.display()
        ),
        Halt::DataRestoreAhead => format!("The store kept apart does not belong with this data: {}. Nothing is served, and nothing was changed. {TAKE_AS_IS}.", contradiction(Contradiction::DataRestoreAhead)),
    })
}

/// A put-back's staged copy takes the data's place (12.4; the review's findings 2 to 5).
/// Only ever done once the data it replaces is kept and the put-back's plan is in the store.
/// First what SQLite wrote beside the data being replaced goes (so that none of it is ever
/// read as part of the copy), then the copy is renamed into place; each made lasting. Safe to
/// do again from any point: the copy waits staged until the rename.
fn put_in_place(folder: &Path) -> Result<(), Refused> {
    let failed = |error: std::io::Error| Refused(format!("The copy couldn't be put in place of the data ({}). Nothing is served; start again, or run the put-back again, and it is finished.", error.kind()));
    let place = data::place(folder);
    crash::point("put-back: begun").map_err(failed)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        match fs::remove_file(PathBuf::from(format!("{}{suffix}", place.display()))) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(failed(error)),
            _ => {}
        }
    }
    fs::File::open(folder).and_then(|folder| folder.sync_all()).map_err(failed)?;
    crash::point("put-back: beside gone").map_err(failed)?;
    fs::rename(data::incoming(folder), &place).map_err(failed)?;
    crash::point("put-back: in place").map_err(failed)?;
    fs::File::open(folder).and_then(|folder| folder.sync_all()).map_err(failed)
}

/// What a restore makes of the store (12.4 steps 1 to 4, in the store first): the new
/// restore number, and each account as `kept::Account::restore` decides it from the rows on
/// `rows` (the data in place, or the exact copy staged to take its place). Nothing written.
fn planned(store: &Store, rows: &rusqlite::Connection, new: u64, mode: RestoreMode, pending: Pending, clock: Clock) -> Result<Store, Refused> {
    let names = data::names_of(rows).map_err(|error| unreadable(&error))?;
    let meta = data::meta_of(rows).map_err(|error| unreadable(&error))?;
    let mut next = store.clone();
    for number in store.accounts.keys().copied().collect::<Vec<_>>() {
        let rows = data::rows_of(rows, &meta, number).map_err(|error| unreadable(&error))?;
        let Some(record) = store.apart(number, names.get(&number)) else { continue };
        if record.account.status == Status::Deleted {
            continue;
        }
        if rows.is_none() {
            // Not in the data: the account is lost with it, and its number not used again.
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
            _ => return Err(Refused(format!("Account {number} could not be brought to the new restore number (a number can go no higher). Nothing was changed."))),
        }
    }
    next.restore = new;
    next.pending = Some(pending);
    Ok(next)
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
        let (high_water, loaded) = (store.high_water, Clock::after_load(store.high_water, store.high_water));
        Ok(Keeper { state: state.to_path_buf(), folder: folder.to_path_buf(), _lock: held, backend: store.backend, store, data, changes, high_water, loaded, broken: false })
    }

    /// Opens both stores in `state` and `folder` (`NEXUS_STATE`, `NEXUS_DATA`) at `now`, and
    /// compares them, **before anything is served** (12.3, 12.4).
    pub fn open(state: &Path, folder: &Path, now: u64) -> Result<(Keeper, Started), Refused> {
        let held = lock::hold(state, folder).map_err(Refused)?;
        private::no_links(state, folder).map_err(Refused)?;
        let id = identity::read(state).map_err(Refused)?;
        let store = match apart::read(state) {
            Reading::Unreadable(what) => {
                return Err(Refused(format!(
                    "The store kept apart, {}, can't be read: {what}. Nothing is served, and it is left as it is. If it is lost, {TAKE_AS_IS}.",
                    apart::place(state).display()
                )));
            }
            Reading::Missing => {
                if whose(folder)?.is_some() {
                    return Err(Refused(format!(
                        "The store kept apart, {}, is missing, and there is data. Nothing is served: without the store nothing says which sessions were ended, which accounts switched off or deleted, or which passwords changed. If NEXUS_STATE names the wrong folder, set it right. If the store is lost, {TAKE_AS_IS}.",
                        apart::place(state).display()
                    )));
                }
                prepare(state, folder)?;
                let id = match id {
                    Some(id) => id,
                    None => identity::read_or_make(state, random_bytes::<16>).map_err(Refused)?.0,
                };
                let store = Store::new(id, random_bytes::<{ SECRET_BYTES }>().map_err(Refused)?, random_bytes::<{ SECRET_BYTES }>().map_err(Refused)?, now);
                apart::write(state, &store).map_err(|error| Refused(store_failed(state, &error)))?;
                return Self::first_data(state, folder, held, store, now);
            }
            Reading::Found(store) => store,
        };
        if id != Some(store.backend) {
            return Err(Refused(not_this_backends(state)));
        }
        // Whose the data is, before anything of it is changed (finding 9).
        if let Some(theirs) = whose(folder)?.filter(|&theirs| theirs != store.backend) {
            return Err(another_backends(folder, theirs, store.backend));
        }
        prepare(state, folder)?;
        // What was under way is finished first, the same way a command run again would (12.4).
        let held = match resumed(state, folder, held, store.clone(), now)? {
            Ok(done) => return Ok(done),
            Err(held) => held,
        };
        copies::prune(folder);
        let data = match Data::open(folder, now).map_err(Refused)? {
            Opened::Open(data) => data,
            Opened::Absent => {
                if !store.data_made {
                    return Self::first_data(state, folder, held, store, now);
                }
                // 12.4, and the review: an emptied data folder with a store is never a first start.
                return Err(Refused(format!(
                    "There is no data in {}, but the store kept apart says there was. Nothing is served. If NEXUS_DATA names the wrong folder, set it right. If the data is lost, put a copy back with `nexus-music-server put-back`, or {TAKE_AS_IS} (here: none, and every account is made again by the admin).",
                    folder.display()
                )));
            }
        };
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

    fn first_data(state: &Path, folder: &Path, held: lock::Held, mut store: Store, now: u64) -> Result<(Keeper, Started), Refused> {
        let data = Data::create(folder, &store.backend, now).map_err(Refused)?;
        store.data_made = true;
        apart::write(state, &store).map_err(|error| Refused(store_failed(state, &error)))?;
        Ok((Self::with(state, folder, held, store, data)?, Started::Made))
    }

    fn names(&self) -> Result<BTreeMap<u64, Email>, rusqlite::Error> {
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
            Startup::RestoreData => self.restore_data(now, RestoreMode::DataOnly),
            Startup::Stop | Startup::AdoptData => Err(Refused(format!(
                "The store kept apart does not belong with this data: {}. Nothing is served, and nothing was changed. If NEXUS_DATA or NEXUS_STATE names the wrong folder, set it right; otherwise {TAKE_AS_IS}.",
                found.contradiction.map_or("they disagree", contradiction)
            ))),
        }
    }

    /// What must also go from the data with an account's state (C3r's): a password that may no
    /// longer be used, and codes that are no longer live.
    fn tidied<'a>(number: u64, state: &AccountState, changes: &mut Vec<Change<'a>>) {
        if state.password_reset_required {
            changes.push(Change::Password { account: number, password: None });
        }
        changes.push(Change::Codes { account: number, keep: live(state.code) });
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
        // One restore number for one restore: the one under way, if one is (findings 3, 4).
        let new = match self.store.pending {
            Some(Pending::Restore { restore } | Pending::PutBack { restore, .. } | Pending::TakeAsIs { restore }) => restore,
            _ => self.store.restore.checked_add(1).ok_or_else(|| Refused("The restore number can go no higher. Nothing is served.".into()))?,
        };
        if self.store.restore != new {
            let pending = if mode == RestoreMode::Adopt { Pending::TakeAsIs { restore: new } } else { Pending::Restore { restore: new } };
            let next = planned(&self.store, self.data.connection(), new, mode, pending, self.clock(now))?;
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
            let name = Some(&rows.account.email);
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

    fn write_store(&mut self, mut next: Store) -> Result<(), Failed> {
        // Every write of the store carries the latest time acted at.
        next.high_water = next.high_water.max(self.high_water);
        // Room above the growth limit is kept for revocations, which add no record.
        let limit = if next.records() > self.store.records() { apart::GROWTH_MAX_BYTES } else { apart::FILE_MAX_BYTES };
        match apart::write_within(&self.state, &next, limit) {
            Ok(()) => {
                self.store = next;
                self.high_water = self.high_water.max(self.store.high_water);
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
        self.loaded.observe(self.high_water).observe(now)
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
        let name = rows.as_ref().map(|rows| rows.account.email.clone());
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

    /// A session id never given before (C3r's `FreshSession`): made lasting with the store's
    /// next write, which is the decision's first half, before the data can hold it.
    pub fn fresh_session(&mut self) -> Option<SessionId> {
        let id = SessionId::new(self.store.next_session)?;
        self.store.next_session = self.store.next_session.checked_add(1)?;
        Some(id)
    }

    /// An account made, in the store first, then the data: by the admin (8.1), with no
    /// password, waiting for one chosen with the one-time code given (`First::Code`: its
    /// keyed fingerprint, good until `until`); or at set-up (8.4), the first admin with the
    /// password chosen there (`First::Password`). An email the data has already is refused.
    pub fn make_account<'a>(&mut self, email: &str, shown: &str, admin: bool, first: impl Into<First<'a>>, now: u64) -> Result<u64, Failed> {
        let first = first.into();
        self.heal()?;
        let name = Email::new(email).filter(|tidied| tidied.as_str() == email).ok_or_else(|| Failed("That is not an email as the backend keeps one.".into()))?;
        if self.account_named(email)?.is_some() {
            return Err(Failed("There is an account with that email already.".into()));
        }
        let mut next = self.store.clone();
        let number = next.next_account;
        next.next_account = number.checked_add(1).ok_or_else(|| Failed("No more accounts can be made.".into()))?;
        let id = AccountId::new(number).ok_or_else(|| Failed("No more accounts can be made.".into()))?;
        // C3r's first state (`AccountState::new`): switched on, no password, a reset required, a
        // code live if one is given (as number 0, good until its `until`). The first admin made
        // at set-up with a password has that password, and no reset waiting.
        let code = match first {
            First::Code(_, until) => Some(LiveCode { number: 0, used: false, expires: until }),
            _ => None,
        };
        let mut state = AccountState::new(self.backend, next.restore, id, name, code);
        state.admin = admin;
        state.password_reset_required = !matches!(first, First::Password(_));
        next.accounts.insert(number, Account { name: apart::name_fingerprint(email), apart: Apart { account: state.clone(), devices: BTreeMap::new() } });
        next.high_water = next.high_water.max(now);
        next.mark = self.changes;
        next.pending = Some(Pending::Change { accounts: vec![number] });
        self.write_store(next)?;
        let at = self.store.high_water;
        let mut changes = vec![Change::Account { number, shown, state: &state }];
        match first {
            First::Nothing => {}
            First::Code(code, until) => changes.push(Change::Code { account: number, number: 0, code, made_at: at, until }),
            First::Password(password) => changes.push(Change::Password { account: number, password: Some(password) }),
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

    /// What of `extra` a decision's effects call for and cannot supply themselves (the review
    /// of S3b, B), chosen before anything is written:
    ///
    /// - **Removals are never taken from a caller**: a password or codes going are derived
    ///   from the effects alone (a reset, a code no longer live).
    /// - **Exactly one new verifier** for each password changed or code redeemed (the one
    ///   C3r was given, for a redemption); a second, or one for an account whose change does
    ///   not call for one while another's does, refuses the whole step.
    /// - **A code's keyed fingerprint** only as the live code the effects decided: its number
    ///   and its deadline; anything else, or a second, refuses the step.
    /// - **A device's description** only for a device that has a fresh session in the update.
    ///
    /// A verifier, code or description no update calls for at all (a decision refused: its
    /// effects are still kept, whatever it answered) is not kept.
    fn selected<'c>(&self, effects: &Effects, numbers: &[u64], extra: &[Change<'c>]) -> Result<Vec<Change<'c>>, Failed> {
        let refuse = |why: &str| Err(Failed(format!("{why}; nothing was kept.")));
        let update_of = |account: u64| effects.updates.iter().zip(numbers).find(|(_, number)| **number == account).map(|(update, _)| update);
        let calls_verifier = |change: &DataChange| matches!(change, DataChange::ChangePassword | DataChange::RedeemCode { .. });
        let any_verifier = effects.updates.iter().any(|update| calls_verifier(&update.change));
        let any_code = effects.updates.iter().any(|update| matches!(update.change, DataChange::ResetPassword));
        let mut kept = Vec::new();
        let (mut verifiers, mut codes) = (BTreeMap::<u64, usize>::new(), BTreeMap::<u64, usize>::new());
        for change in extra {
            match *change {
                Change::Password { password: None, .. } | Change::Codes { .. } => return refuse("A removal was given with a decision; what goes is the decision's own"),
                Change::Password { account, password: Some(password) } => match update_of(account).map(|update| &update.change) {
                    Some(DataChange::RedeemCode { verifier, .. }) if password.hash != *verifier.as_bytes() => return refuse("A code was redeemed with another verifier than the one checked"),
                    Some(called) if calls_verifier(called) => {
                        *verifiers.entry(account).or_default() += 1;
                        kept.push(change.clone());
                    }
                    _ if any_verifier => return refuse("A new password was given for an account whose change is not one"),
                    _ => {}
                },
                Change::Code { account, number, until, .. } => match update_of(account) {
                    Some(update) if matches!(update.change, DataChange::ResetPassword) => {
                        let live = update.data.account.code;
                        if live.is_none_or(|live| live.used || live.number != number || live.expires != until) {
                            return refuse("A code was given that is not the live code the reset decided");
                        }
                        *codes.entry(account).or_default() += 1;
                        kept.push(change.clone());
                    }
                    _ if any_code => return refuse("A code was given for an account that was not reset"),
                    _ => {}
                },
                Change::Describe { device, .. } => {
                    let fresh = DeviceId::new(device).is_some_and(|id| {
                        effects.updates.iter().zip(numbers).any(|(update, number)| {
                            let before = self.store.accounts.get(number).and_then(|account| account.apart.devices.get(&id)).and_then(|record| record.permit.as_ref()).map(|permit| permit.id);
                            update.data.devices.get(&id).and_then(|row| row.session.as_ref()).is_some_and(|session| Some(session.id) != before)
                        })
                    });
                    if fresh {
                        kept.push(change.clone());
                    }
                }
                _ => return refuse("A change was added to a decision that no decision adds to"),
            }
        }
        if verifiers.values().any(|&n| n > 1) || codes.values().any(|&n| n > 1) {
            return refuse("More than one new password or code was given for one account");
        }
        for (update, &number) in effects.updates.iter().zip(numbers) {
            if calls_verifier(&update.change) && verifiers.get(&number) != Some(&1) {
                return refuse("A password was changed or a code redeemed with no new password to keep");
            }
        }
        Ok(kept)
    }

    /// Keeps what a decision says is to be kept (C3r's `Effects`), **whatever it answered**,
    /// and before its answer is given, in 12.3's order. `extra` is what the caller adds to the
    /// data's step for the change (S8: a password's new fingerprint for `ChangePassword`, a
    /// code for `ResetPassword`; a device's description). If this fails, the answer is not
    /// given and the account is loaded again before it is used.
    pub fn persist(&mut self, effects: &Effects, extra: &[Change<'_>], now: u64) -> Result<(), Failed> {
        self.heal()?;
        self.high_water = self.high_water.max(effects.high_water);
        let high_water = self.high_water;
        if effects.updates.is_empty() {
            // No row changes: the clock goes to the store once it is a minute ahead of it,
            // the mark once its minute is out (12.3; the ruling of 13:59).
            let due = now.saturating_sub(self.store.mark_at) >= MARK_EVERY_MS || now < self.store.mark_at;
            let clock_due = effects.clock_mark_due(self.store.high_water);
            if clock_due || (self.changes > self.store.mark && due) {
                let mut next = self.store.clone();
                next.high_water = high_water;
                if due && self.changes > self.store.mark {
                    next.mark = self.changes;
                    next.mark_at = now;
                }
                self.write_store(next)?;
            }
            return Ok(());
        }
        let mut numbers = Vec::new();
        for update in &effects.updates {
            let number = update.apart_first.account.id.get();
            if !self.store.accounts.contains_key(&number) {
                return Err(Failed("A decision named an account there is not.".into()));
            }
            numbers.push(number);
        }
        let added = self.selected(effects, &numbers, extra)?;
        // What a music login cannot be kept without, said before anything is written: sealed
        // for its own account, this backend and the row's count (its internal address is in
        // the binding itself, core-fu2).
        // A switch for requests, or the Lidarr key, only as the successor of what the store
        // has now (S13): each is written in the FIRST store write, store first, and nowhere
        // else. The data carries only non-authoritative evidence of a check.
        for (update, &number) in effects.updates.iter().zip(&numbers) {
            match &update.change {
                DataChange::InstallMusicLogin(install) => {
                    let binding = &install.binding;
                    if binding.account != update.apart_first.account.id || binding.count != update.data.account.music_login || binding.backend != self.backend {
                        return Err(Failed("A music login was sealed for another account, backend or count; nothing was kept.".into()));
                    }
                }
                DataChange::SetRequestsEnabled { revision, .. } => {
                    if self.store.requests.get(&number).copied().unwrap_or_default().revision.checked_add(1) != Some(*revision) {
                        return Err(Failed("A switch for requests was set under another revision; nothing was kept.".into()));
                    }
                }
                DataChange::InstallLidarrKey { binding, .. } => {
                    let kept = &self.store.lidarr;
                    if binding.backend != self.backend || kept.address.as_deref() != Some(binding.address.as_str()) || kept.revision.checked_add(1) != Some(binding.revision) {
                        return Err(Failed("A Lidarr key was sealed for another backend, address or revision; nothing was kept.".into()));
                    }
                }
                DataChange::Keep
                | DataChange::ChangePassword
                | DataChange::ResetPassword
                | DataChange::OwnerRecovery { .. }
                | DataChange::RedeemCode { .. }
                | DataChange::EraseHistory
                | DataChange::DiscardCredentials
                | DataChange::DeleteAccount
                | DataChange::ApplyAdmin { .. } => {}
            }
        }
        let mut next = self.store.clone();
        next.high_water = high_water;
        next.mark = self.changes;
        for (update, &number) in effects.updates.iter().zip(&numbers) {
            let entry = next.accounts.get_mut(&number).ok_or_else(|| Failed("A decision named an account there is not.".into()))?;
            entry.apart = update.apart_first.clone();
            if let Some(&highest) = update.apart_first.devices.keys().max() {
                next.next_device = next.next_device.max(highest.get().saturating_add(1));
            }
            if let Some(highest) = update.apart_first.devices.values().map(|device| device.last_session).max() {
                next.next_session = next.next_session.max(highest.saturating_add(1));
            }
        }
        for (update, &number) in effects.updates.iter().zip(&numbers) {
            match &update.change {
                DataChange::SetRequestsEnabled { enabled, revision } => {
                    next.requests.insert(number, RequestsSwitch { enabled: *enabled, revision: *revision });
                }
                DataChange::InstallLidarrKey { binding, sealed, .. } => {
                    next.lidarr = LidarrKept { revision: binding.revision, address: Some(binding.address.as_str().to_string()), sealed: Some(sealed.as_bytes().to_vec()) };
                }
                _ => {}
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
            // Every effect has its arm: one the keeper does not know fails to compile (M3).
            match &update.change {
                DataChange::Keep | DataChange::DeleteAccount => {}
                DataChange::ResetPassword => changes.push(Change::Password { account: number, password: None }),
                // The new password is in `extra` (checked above): the verifier C3r was given.
                DataChange::RedeemCode { .. } | DataChange::ChangePassword => {}
                // The owner's recovery (8.4): no password, no old code, the new code's verifier
                // under the number kept gave it, the sessions gone (in `Install`), in one step.
                DataChange::OwnerRecovery { code_verifier } => {
                    changes.push(Change::Password { account: number, password: None });
                    changes.push(Change::Codes { account: number, keep: None });
                    if let Some(code) = update.data.account.code.filter(|code| !code.used) {
                        changes.push(Change::Code { account: number, number: code.number, code: code_verifier.as_bytes(), made_at: at, until: code.expires });
                    }
                }
                // A store lost (12.4): the verifier and every code go with the sessions.
                DataChange::DiscardCredentials => {
                    changes.push(Change::Password { account: number, password: None });
                    changes.push(Change::Codes { account: number, keep: None });
                }
                // The account's listening events go in this same step: its life moves on in
                // `Install`, and the data's trigger deletes them unless they are held (12.5).
                DataChange::EraseHistory => {}
                DataChange::InstallMusicLogin(install) => changes.push(Change::MusicLogin { account: number, install }),
                DataChange::ApplyAdmin { action, target } => match (action, target) {
                    (AdminAction::SetMusicAddress(address), AdminTarget::Backend) => changes.push(Change::MusicPublic { address: address.as_str() }),
                    (AdminAction::SetDiagnosticsDefault(on), AdminTarget::Backend) => changes.push(Change::DiagnosticsDefault { on: *on }),
                    // What these change is the account's state, in `Install` (and the store).
                    (AdminAction::SetOn(_) | AdminAction::DeleteAccount | AdminAction::ResetPassword | AdminAction::SetAdmin(_) | AdminAction::ForceOut(_) | AdminAction::MusicLoginChanged | AdminAction::CreateAccount, _) => {}
                    // Their own effects (`SetRequestsEnabled`, `InstallLidarrKey`): the store's.
                    (AdminAction::SetRequests { .. } | AdminAction::SetLidarr { .. }, _) => {}
                    (AdminAction::SetMusicAddress(_) | AdminAction::SetDiagnosticsDefault(_), AdminTarget::Account { .. } | AdminTarget::NewAccount(_)) => {}
                },
                // The switch and sealed key remain in the store kept apart.
                DataChange::SetRequestsEnabled { .. } => {}
                DataChange::InstallLidarrKey { sealed, check, .. } => {
                    let stamp = (*check == LidarrKeyCheck::Works).then(|| sealed.check_stamp());
                    changes.push(Change::LidarrChecked { stamp });
                }
            }
            changes.push(Change::Codes { account: number, keep: live(update.data.account.code) });
        }
        changes.extend(added);
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
        self.flush_clock()?;
        match self.data.apply(&[Change::Heard { device, at: now }], self.store.high_water) {
            Ok(changes) => self.changes = changes,
            Err(error) => return Err(Failed(data_failed(&error))),
        }
        let effects = Effects { high_water: self.store.high_water, updates: Vec::new() };
        self.persist(&effects, &[], now)
    }

    /// A step of another slice's rows (listening events, Diagnostics reports: data, never
    /// authority), through the one writer (S12/S7). `step` runs in one transaction; it says
    /// whether it changed anything. If it did, the step is counted (12.3's mark: the store
    /// hears of it within the minute, by `idle` if nothing else) and written at the time the
    /// store already has (finding 7: no step of data alone moves the clock); if not, nothing
    /// of it is kept. All of it or none. Nothing that takes leave away or gives it may be done
    /// here: that is `persist`'s, store first.
    pub fn write_data<T>(&mut self, now: u64, step: impl FnOnce(&rusqlite::Transaction<'_>) -> rusqlite::Result<(T, bool)>) -> Result<T, Failed> {
        self.heal()?;
        self.flush_clock()?;
        let (value, changed) = self.data.apply_with(self.store.high_water, step).map_err(|error| Failed(data_failed(&error)))?;
        if let Some(changes) = changed {
            self.changes = changes;
            let due = now.saturating_sub(self.store.mark_at) >= MARK_EVERY_MS || now < self.store.mark_at;
            if due {
                self.write_mark(now)?;
            }
        }
        Ok(value)
    }

    /// An account's switch for requests and its revision (S13), as the store kept apart has
    /// it: the authority a grant to change it is bound to.
    pub fn requests_switch(&self, account: u64) -> Result<RequestsSwitch, Failed> {
        if !self.store.accounts.contains_key(&account) {
            return Err(Failed("There is no such account.".into()));
        }
        Ok(self.store.requests.get(&account).copied().unwrap_or_default())
    }

    /// The Lidarr key's binding for `configured` (`NEXUS_LIDARR`, or none) and the sealed key
    /// if the store holds one for exactly that binding (S13). If the store last saw another
    /// address (changed, or removed), the key it holds is retired first, store first: its
    /// revision moves on and its seal goes, so that setting the address back never revives it.
    pub fn lidarr(&mut self, configured: Option<&InternalAddress>) -> Result<Lidarr, Failed> {
        self.heal()?;
        let seen = configured.map(|address| address.as_str().to_string());
        if self.store.lidarr.address != seen {
            let mut next = self.store.clone();
            next.lidarr = LidarrKept {
                revision: next.lidarr.revision.checked_add(1).ok_or_else(|| Failed("The Lidarr key's revision can go no higher.".into()))?,
                address: seen,
                sealed: None,
            };
            self.write_store(next)?;
        }
        let kept = &self.store.lidarr;
        let binding = configured.map(|address| LidarrBinding { backend: self.backend, address: address.clone(), revision: kept.revision });
        let sealed = kept.sealed.as_deref().filter(|_| binding.is_some()).and_then(SealedLidarrKey::from_bytes);
        let has_saved_key = binding.is_some() && kept.sealed.is_some();
        Ok(Lidarr { binding, sealed, has_saved_key })
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
        // Diagnostics reports past their age go, a bounded number at a time (14; S7's ask).
        let cutoff = self.clock(now).effective().saturating_sub(diag::MAX_AGE_MS);
        let (_, changed) = self
            .data
            .apply_with(self.store.high_water, |step| {
                let gone = step.execute(
                    "DELETE FROM diagnostic_reports WHERE row_id IN (SELECT row_id FROM diagnostic_reports WHERE received_ms < ?1 ORDER BY received_ms, row_id LIMIT ?2)",
                    rusqlite::params![i64::try_from(cutoff).unwrap_or(i64::MAX), EXPIRED_PER_IDLE],
                )?;
                Ok(((), gone > 0))
            })
            .map_err(|error| Failed(data_failed(&error)))?;
        if let Some(changes) = changed {
            self.changes = changes;
        }
        let due = now.saturating_sub(self.store.mark_at) >= MARK_EVERY_MS || now < self.store.mark_at;
        if self.changes > self.store.mark && due {
            self.write_mark(now)?;
        }
        Ok(())
    }

    /// Before a step that changes a row and is no decision (a device heard from, another
    /// slice's rows): the latest time acted at goes to the store first if the running backend
    /// is ahead of it (the review of S3b, 7: every step that changes a row carries the clock,
    /// store first). The step itself is written at that time; what it observed (when a device
    /// was heard from) is its own.
    fn flush_clock(&mut self) -> Result<(), Failed> {
        if self.high_water > self.store.high_water {
            let next = self.store.clone();
            self.write_store(next)?;
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

/// Once whose the data is has been read (finding 9): what this program left half-done is
/// tidied, and its files narrowed to the owner's alone (finding 8).
fn prepare(state: &Path, folder: &Path) -> Result<(), Refused> {
    private::at_start(state, folder).map_err(Refused)?;
    apart::tidy(state);
    data::tidy(folder);
    Ok(())
}

/// What was under way, finished first, by a start and by either command alike (12.4; the
/// review's finding 4 and the re-review's C: one decision, `compare::resume`, made from what
/// is only READ: the hash of a copy waiting beside the data, and the data in place as it is,
/// whose backend is checked before it is opened to write). `Err(held)` if nothing was.
fn resumed(state: &Path, folder: &Path, held: lock::Held, store: Store, now: u64) -> Result<Result<(Keeper, Started), lock::Held>, Refused> {
    let staged = match copies::hash(&data::incoming(folder)) {
        Ok(hash) => Some(hash),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(Refused(format!("The copy waiting beside the data can't be read ({}). Nothing was changed.", error.kind()))),
    };
    let in_place = match data::identity_of(&data::place(folder)).map_err(Refused)? {
        Some(found) if found.backend != store.backend => return Err(another_backends(folder, found.backend, store.backend)),
        Some(found) => Some(InPlace { restore: found.restore, changes: found.changes, written_at: found.written_at }),
        None => None,
    };
    let adopt = match compare::resume(store.pending.as_ref(), staged.as_ref(), in_place) {
        Resume::Nothing => {
            if staged.is_some() {
                // Staged by a put-back cut off before its plan was in the store: never used.
                let _ = fs::remove_file(data::incoming(folder));
            }
            return Ok(Err(held));
        }
        Resume::Stop(halt) => return Err(halted(halt, folder)),
        Resume::PutInPlace { .. } => {
            put_in_place(folder)?;
            false
        }
        Resume::Restore { adopt, .. } => {
            // Its name in place may not have been made lasting before the cut.
            fs::File::open(folder).and_then(|folder| folder.sync_all()).map_err(|error| Refused(format!("The data folder couldn't be made lasting ({}).", error.kind())))?;
            adopt
        }
    };
    let data = match Data::open(folder, now).map_err(Refused)? {
        Opened::Open(data) => data,
        Opened::Absent => return Err(halted(Halt::CopyLost, folder)),
    };
    let mut keeper = Keeper::with(state, folder, held, store, data)?;
    let started = keeper.restore_data(now, if adopt { RestoreMode::Adopt } else { RestoreMode::DataOnly })?;
    Ok(Ok((keeper, started)))
}

/// [`resumed`], for a command: what it said, or the locks back if nothing was under way.
fn finish_first(state: &Path, folder: &Path, held: lock::Held, store: Store, now: u64) -> Result<Result<Said, lock::Held>, Refused> {
    let adopt = matches!(store.pending, Some(Pending::TakeAsIs { .. }));
    match resumed(state, folder, held, store, now)? {
        Err(held) => Ok(Err(held)),
        Ok((keeper, started)) => {
            let restore = keeper.restore();
            keeper.close();
            Ok(Ok(vec![
                format!("A {} begun earlier was cut off; it was finished first, under restore number {restore}, and nothing else was done.", if adopt { "take-as-is" } else { "restore of the data" }),
                started.said(),
                "Run the command again if there is more to do; otherwise start the backend again. Every person signs in again.".into(),
            ]))
        }
    }
}

/// `put-back <copy>`: puts a copy of the data back by the supported way (12.4). Refused while
/// the backend serves. In this order, each step lasting before the next (the review's
/// findings 3 to 5):
///
/// 1. The copy is staged beside the data (SQLite's own copy, checked, the owner's alone).
/// 2. That exact staged file is read, and core says whether this store can take it and
///    under which restore number; each account's restore is planned by `kept`. **Any refusal
///    here, or before, changes nothing.**
/// 3. The data in use is kept, whole, as one checked file (or byte for byte if SQLite can't
///    read it).
/// 4. The plan goes to the store, whole: the new restore number and each device's number
///    moved. From here a start, or the command run again, finishes it (`compare::resume`).
/// 5. The staged copy takes the data's place; then 12.4's steps in the data.
pub fn put_back(state: &Path, folder: &Path, copy: &Path, now: u64) -> Result<Said, Refused> {
    let held = lock::hold(state, folder).map_err(Refused)?;
    private::no_links(state, folder).map_err(Refused)?;
    let store = match apart::read(state) {
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
    // The data in place is this backend's, or there is none: read before anything changes.
    if let Some(theirs) = whose(folder)?.filter(|&theirs| theirs != store.backend) {
        return Err(another_backends(folder, theirs, store.backend));
    }
    prepare(state, folder)?;
    let held = match finish_first(state, folder, held, store.clone(), now)? {
        Ok(said) => return Ok(said),
        Err(held) => held,
    };
    let nothing = |why: String| Refused(format!("{why} Nothing was changed."));
    // 1. Staged, from the copy named.
    let source = data::open_to_read(copy, "The copy").map_err(nothing)?;
    let (backend, written_at) = data::identify(&source).map_err(|error| nothing(format!("The copy can't be read: {}.", why(&error))))?;
    if backend != store.backend {
        return Err(nothing(format!("The copy is another backend's data (its id is {}; this one's is {}).", backend.to_hex(), store.backend.to_hex())));
    }
    let staged = copies::stage(&source, folder).map_err(|why| nothing(format!("The copy couldn't be staged beside the data: {why}.")))?;
    drop(source);
    let unstage = |refused: Refused| {
        let _ = fs::remove_file(&staged);
        refused
    };
    // A copy of an older form is brought forward before anything reads it with this
    // program's forms (the forms' steps; a newer one was refused when it was opened).
    data::bring_forward(&staged).map_err(|why| unstage(nothing(why)))?;
    // 2. The staged file itself, checked and planned by core before anything is recorded.
    let plan = (|| -> Result<Store, Refused> {
        // The very file staged: its hash, and its own backend id (read from it, not from the
        // copy it was made from).
        let hash = copies::hash(&staged).map_err(|error| nothing(format!("The staged copy can't be read ({}).", error.kind())))?;
        let staged_data = data::open_to_read(&staged, "The staged copy").map_err(nothing)?;
        let (backend, _) = data::identify(&staged_data).map_err(|error| nothing(format!("The copy can't be read: {}.", why(&error))))?;
        if backend != store.backend {
            return Err(nothing("The staged copy is another backend's data.".into()));
        }
        let meta = data::meta_of(&staged_data).map_err(|error| nothing(format!("The copy can't be read: {}.", why(&error))))?;
        let view = data::view_of(&staged_data).map_err(|error| nothing(format!("The copy can't be read: {}.", why(&error))))?;
        let names = data::names_of(&staged_data).map_err(|error| nothing(format!("The copy can't be read: {}.", why(&error))))?;
        let new = compare::put_back(&store.view(&names), &view).map_err(|refusal| {
            nothing(match refusal {
                NotPutBack::Contradiction(why) => format!("The copy can't be put back under this store kept apart: {}.", contradiction(why)),
                NotPutBack::Spent => "The restore number can go no higher.".into(),
                NotPutBack::UnderWay => "A restore is under way.".into(),
            })
        })?;
        let copy = CopyIdentity { hash, changes: meta.changes, written_at: meta.written_at };
        planned(&store, &staged_data, new, RestoreMode::DataOnly, Pending::PutBack { restore: new, copy }, Clock::after_load(now, store.high_water))
    })()
    .map_err(unstage)?;
    let new = plan.restore;
    let mut said = vec![format!("Put back {} (its data's last step was at {written_at} ms).", copy.display())];
    // 3. The data in use, kept whole, as one checked file, before anything of it is replaced;
    // if that can't be made, nothing is replaced (the re-review's 5).
    if data::place(folder).exists() {
        let kept = data::open_to_read(&data::place(folder), "The data in place")
            .and_then(|live| copies::keep_replaced(&live, folder, new, now))
            .map_err(|why| unstage(nothing(format!("The data being replaced couldn't be kept whole ({why}); if it is damaged, move it out of NEXUS_DATA yourself and put the copy back again."))))?;
        said.push(format!("The data it replaced is kept as {}.", kept.display()));
    }
    // 4. The plan in the store, whole.
    apart::write(state, &plan).map_err(|error| Refused(format!("{} Run the put-back again: it begins again, or finishes what was begun.", store_failed(state, &error))))?;
    // 5. In place, then 12.4's steps in the data.
    put_in_place(folder)?;
    let data = match Data::open(folder, now).map_err(Refused)? {
        Opened::Open(data) => data,
        Opened::Absent => return Err(halted(Halt::CopyLost, folder)),
    };
    let mut keeper = Keeper::with(state, folder, held, plan, data)?;
    let started = keeper.restore_data(now, RestoreMode::DataOnly)?;
    keeper.close();
    said.push(started.said());
    said.push("Start the backend again. Every person signs in again.".into());
    Ok(said)
}

/// `take-as-is`: takes the data as it is when the store kept apart is lost, cannot be read,
/// or does not belong with the data (12.4): a new restore number, every session ended. With
/// no store of this backend that can be read, nothing says which passwords changed or which
/// codes were used since, so **every account waits for a new one-time code** (12.4; the
/// review's finding 1). With one, what it took away is kept. With no data, it starts again
/// with none (every account made again by the admin), keeping what the store has. Refused
/// while the backend serves, and when the store and the data agree. A readable old store
/// stays in place until the new one is renamed over it whole (finding 2).
pub fn take_as_is(state: &Path, folder: &Path, now: u64) -> Result<Said, Refused> {
    let held = lock::hold(state, folder).map_err(Refused)?;
    private::no_links(state, folder).map_err(Refused)?;
    let id = identity::read(state).map_err(Refused)?;
    let read = apart::read(state);
    let theirs = whose(folder)?;
    // Whose the data is, before anything of it is changed (finding 9).
    if let (Some(id), Some(theirs)) = (id, theirs)
        && id != theirs
    {
        return Err(Refused(format!(
            "The data is another backend's: its id is {}, and {} says {}. Nothing was changed. Set NEXUS_DATA and NEXUS_STATE to one backend's folders; if this data is the one to serve, remove that file first.",
            theirs.to_hex(),
            state.join(identity::FILE).display(),
            id.to_hex()
        )));
    }
    prepare(state, folder)?;
    let Some(theirs) = theirs else {
        return match read {
            Reading::Missing => Err(Refused("There is neither data nor a store kept apart: there is nothing to take. Start the backend, and it makes both.".into())),
            Reading::Unreadable(_) => Err(Refused("There is no data, and the store kept apart can't be read: there is nothing to take. Put a copy of the data back by hand into NEXUS_DATA and run this again.".into())),
            Reading::Found(store) => {
                if id != Some(store.backend) {
                    return Err(Refused(not_this_backends(state)));
                }
                let held = match finish_first(state, folder, held, store.clone(), now)? {
                    Ok(said) => return Ok(said),
                    Err(held) => held,
                };
                let mut store = store;
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
        };
    };
    // The authority there still is: a readable store of this data's backend.
    let authority = match read {
        Reading::Found(store) if store.backend == theirs => Some(store),
        _ => None,
    };
    let held = match &authority {
        Some(store) if id == Some(theirs) && store.data_made => match finish_first(state, folder, held, store.clone(), now)? {
            Ok(said) => return Ok(said),
            Err(held) => held,
        },
        _ => held,
    };
    let data = match Data::open(folder, now).map_err(Refused)? {
        Opened::Open(data) => data,
        Opened::Absent => return Err(Refused("The data was not there once opened. Nothing was changed.".into())),
    };
    let meta = data.meta().map_err(|error| unreadable(&error))?;
    let names = data.names().map_err(|error| unreadable(&error))?;
    let view = data.view().map_err(|error| unreadable(&error))?;
    if id.is_some() && !compare::needs_taking(authority.as_ref().filter(|store| store.data_made).map(|store| store.view(&names)).as_ref(), &view) {
        return Err(Refused("The store kept apart is there and belongs with the data: there is nothing to take. Start the backend; it compares the two itself.".into()));
    }
    // A new store from the data as it is, planned whole before anything is written.
    let old = authority.as_ref();
    let seal = match old {
        Some(old) => old.seal_key(),
        // Lost with the store: what it sealed is made anew or typed in again (12.4).
        None => random_bytes::<SECRET_BYTES>().map_err(Refused)?,
    };
    let key = random_bytes::<SECRET_BYTES>().map_err(Refused)?;
    let high_water = now.max(meta.written_at).max(old.map_or(0, |old| old.high_water));
    let mut store = Store::new(meta.backend, key, seal, high_water);
    let new = meta.restore.max(old.map_or(0, |old| old.restore)).checked_add(1).ok_or_else(|| Refused("The restore number can go no higher. Nothing was changed.".into()))?;
    store.restore = meta.restore;
    store.data_made = true;
    store.set_up = old.is_some_and(|old| old.set_up) || view.accounts.values().any(|rows| rows.account.admin);
    store.mark = meta.changes;
    store.next_account = view.accounts.keys().max().map_or(1, |n| n.saturating_add(1)).max(old.map_or(1, |old| old.next_account));
    let highest_device = view.accounts.values().flat_map(|rows| rows.devices.keys()).chain(old.into_iter().flat_map(|old| old.accounts.values().flat_map(|account| account.apart.devices.keys()))).max().copied();
    store.next_device = highest_device.map_or(1, |n| n.get().saturating_add(1)).max(old.map_or(1, |old| old.next_device));
    let highest_session = view
        .accounts
        .values()
        .flat_map(|rows| rows.devices.values())
        .filter_map(|device| device.session.as_ref())
        .map(|session| session.id.get())
        .chain(old.into_iter().flat_map(|old| old.accounts.values().flat_map(|account| account.apart.devices.values().map(|device| device.last_session))))
        .max();
    store.next_session = highest_session.map_or(1, |n| n.saturating_add(1)).max(old.map_or(1, |old| old.next_session));
    if let Some(old) = old {
        // What only this store knows of requests: each account's switch, and the Lidarr key.
        store.requests = old.requests.clone();
        store.lidarr = old.lidarr.clone();
        for (&number, account) in &old.accounts {
            if !view.accounts.contains_key(&number) {
                let mut tombstone = account.clone();
                tombstone.apart.account.status = Status::Deleted;
                for device in tombstone.apart.devices.values_mut() {
                    device.permit = None;
                }
                store.accounts.insert(number, tombstone);
            }
        }
    }
    // Each account as kept recovers it from its rows and what a readable store of this
    // backend still says (`kept::Account::recover_from_lost_store`: its tombstones, switches
    // and counts kept; every verifier and code discarded; no device known until it signs in).
    let clock = Clock::after_load(now, store.high_water);
    for (&number, rows) in &view.accounts {
        let was = old.and_then(|old| old.apart(number, Some(&rows.account.email)));
        let mut first = None;
        let answer = Decided::recover_from_lost_store(rows, was.as_ref(), new, clock).apply(|effects| {
            first = effects.updates.first().map(|update| update.apart_first.clone());
            Ok::<_, ()>(())
        });
        let Some((Ok(()), apart)) = answer.ok().zip(first) else {
            return Err(Refused(format!("Account {number} could not be taken as it is (a number can go no higher, or its rows are not its own). Nothing was changed.")));
        };
        store.accounts.insert(number, Account { name: apart::name_fingerprint(rows.account.email.as_str()), apart });
    }
    store.restore = new;
    store.pending = Some(Pending::TakeAsIs { restore: new });
    let plan = store;
    let mut said = vec!["Took the data as it is.".to_string()];
    if old.is_none() {
        said.push("Nothing says which passwords changed or which one-time codes were used since the store kept apart was lost: every account waits for a new one-time code (the admin's first, by the recovery command).".into());
    }
    // The old store, kept as it is beside the new; the new is renamed over it whole.
    if apart::place(state).exists() {
        let aside = apart::keep_aside(state, now).map_err(|error| Refused(format!("The old store couldn't be kept aside ({}). Nothing was changed.", error.kind())))?;
        said.push(format!("The old store kept apart is kept as {}.", aside.display()));
    }
    if id.is_none() {
        identity::read_or_make(state, || Ok(*meta.backend.as_bytes())).map_err(Refused)?;
    }
    apart::write(state, &plan).map_err(|error| Refused(format!("{} Run take-as-is again.", store_failed(state, &error))))?;
    let mut keeper = Keeper::with(state, folder, held, plan, data)?;
    let started = keeper.restore_data(now, RestoreMode::Adopt)?;
    keeper.close();
    said.push(started.said());
    said.push("Start the backend again.".into());
    Ok(said)
}
