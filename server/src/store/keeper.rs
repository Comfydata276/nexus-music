//! The backend's two stores together (design 9.2, 11.2, 13.4): the data and the store kept
//! apart, opened, compared and brought together **before anything is served**; the steps
//! later slices call, each all or nothing and each in the order 9.2 sets; and the owner's two
//! commands.
//!
//! **The order, every time (9.2, and C3's `Effects`):**
//! 1. What takes leave away is written to the store kept apart, with a note of the change
//!    under way (`Pending`).
//! 2. Then the data, in one step.
//! 3. Then the store again: what gives leave, and the note cleared.
//!
//! A cut between any two leaves something the comparison at the next start tells from an
//! older copy, and finishes (`core::nexus::compare`). A step that fails part-way leaves the
//! keeper unable to write until it has brought the two together again (`heal`), so that
//! nothing is answered on stores that disagree.
//!
//! Nothing here decides who may do what: that is C3's (`kept`), and what the start does about
//! what it finds is `compare`'s and C3's (`at_start`, `restored_account`, `restore_device`).
//! This is the plumbing: files, SQLite, and the order of the two.

use std::fs;
use std::path::{Path, PathBuf};

use nexus_core::nexus::compare::{self, Contradiction, Pending, StoreAccount, StoreDevice};
use nexus_core::nexus::kept::{Apart, Clock, Denied, Device, Effects, Family, Fingerprint, Rows, SECRET_BYTES, StampKey, Scope, Startup, StoreComparison, at_start};
use nexus_core::nexus::proto::BackendId;

use crate::store::apart::{self, Account, Reading, Store};
use crate::store::copies;
use crate::store::data::{self, Change, Data, Opened, why};
use crate::identity;
use crate::store::lock;

/// Why the backend does not serve, or a command did nothing: a sentence for the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A step that did not happen, or did not finish: the caller answers "busy" or "not dealt
/// with", and nothing it was asked for is given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed(pub String);

/// What the start found and did, for the log (13.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Started {
    /// A first start: the store and the data made now.
    Made,
    /// The same age: nothing to do.
    Ready,
    /// A change cut between the two stores, finished in the data (`FinishEnding`, or
    /// `EndUnknownSession` for a grant the store never heard of).
    Finished(Startup),
    /// The data was older than the store, or was put back: a new restore number, every
    /// device's sessions ended, and the accounts held that were deleted since.
    Restored { restore: u64, devices: usize, held: usize, from: u64 },
}

impl Started {
    /// The log's line. No name: accounts are counted.
    pub fn said(&self) -> String {
        match self {
            Started::Made => "The data and the store kept apart were made now.".into(),
            Started::Ready => "The data and the store kept apart were compared, and are the same age.".into(),
            Started::Finished(Startup::EndUnknownSession) => {
                "The data and the store kept apart were compared: a session the store had not yet heard of, cut off as the program stopped, was ended.".into()
            }
            Started::Finished(_) => "The data and the store kept apart were compared: an ending the store had and the data not yet, cut off as the program stopped, was finished in the data.".into(),
            Started::Restored { restore, devices, held, from } => format!(
                "The data was found to be older than the store kept apart (its last step was at {from} ms), or was put back. It now has restore number {restore}; every device's sessions were ended ({devices} devices); {held} accounts' history is held until a device of each signs in and its person answers."
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
    /// A step failed part-way: nothing is written until the two have been brought together.
    broken: bool,
}

/// A change to a whole account (design 8.7, 8.10, 12.5).
#[derive(Debug, Clone, Copy)]
pub enum AccountChange<'a> {
    /// Switched off by the admin: every session ended, and none made until it is on again.
    SwitchOff,
    /// Switched on again: gives leave, so in the data first.
    SwitchOn,
    /// Deleted by the admin: everything of it goes, and no copy brings it back.
    Delete,
    /// Its password changed by its person (`Some`, and the device it was changed on kept), or
    /// reset by the admin (`None`: it waits for one to be chosen with a code). Either way the
    /// count moves, every one-time code it had is gone, and a copy from before holds a
    /// password that no longer works.
    Password(Option<&'a data::Password>),
}

/// One account, as a later slice is given it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountFound {
    pub number: u64,
    pub life: u64,
    pub held: bool,
}

fn random_bytes<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|why| format!("No randomness to make the store's key with ({why})."))?;
    Ok(bytes)
}

fn store_failed(state: &Path, error: &std::io::Error) -> String {
    format!("The store kept apart couldn't be written in {} ({}).", state.display(), error.kind())
}

fn data_failed(error: &rusqlite::Error) -> String {
    format!("The data couldn't be written: {}.", why(error))
}

fn contradiction(why: Contradiction) -> &'static str {
    match why {
        Contradiction::DataRestoreAhead => "the data has a restore number the store never gave",
        Contradiction::AccountUnknown => "the data has an account the store never made",
        Contradiction::AccountAhead => "the data has an account further on than the store",
        Contradiction::DeviceUnknown => "the data has a device the store never made",
        Contradiction::DeviceElsewhere => "the data has a device under another account than the store",
        Contradiction::DeviceAhead => "the data has a device further on than the store",
        Contradiction::SessionNeverReserved => "the data has a session the store never made room for",
    }
}

const TAKE_AS_IS: &str = "run `nexus-music-server take-as-is` to take the data as it is: every device will sign in again, and every account's history will be held until a device of it signs in and its person answers keep or delete";

/// What `open` found before deciding anything.
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

impl Keeper {
    /// Opens both stores in `state` and `folder` (the settings' `NEXUS_STATE` and
    /// `NEXUS_DATA`) at `now`, and compares them, **before anything is served** (9.2, 11.2):
    /// either they agree, or are brought together, or it says what to do and nothing is served.
    pub fn open(state: &Path, folder: &Path, now: u64) -> Result<(Keeper, Started), Refused> {
        let held = lock::hold(state, folder).map_err(Refused)?;
        let found = find(state, folder, now)?;
        match (found.id, found.store, found.data) {
            (_, Reading::Unreadable(what), _) => Err(Refused(format!(
                "The store kept apart, {}, can't be read: {what}. Nothing is served, and it is left as it is. If it is lost, {TAKE_AS_IS}.",
                apart::place(state).display()
            ))),
            (_, Reading::Missing, Opened::Open(_)) => Err(Refused(format!(
                "The store kept apart, {}, is missing, and there is data. Nothing is served: without the store nothing says which sessions were ended or which histories deleted. If NEXUS_STATE names the wrong folder, set it right. If the store is lost, {TAKE_AS_IS}.",
                apart::place(state).display()
            ))),
            (id, Reading::Missing, Opened::Absent) => {
                // A first start (the backend's id may be there already, from the first slice).
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
                    return Err(Refused(Self::not_this_backends(state)));
                }
                if !store.data_made {
                    // A first start cut off before the data was made.
                    return Self::first_data(state, folder, held, store, now);
                }
                if matches!(store.pending, Some(Pending::Restore { .. })) {
                    return Err(Refused(format!(
                        "A put-back was cut off before the copy was in place: there is no data in {}. Run `nexus-music-server put-back` with the copy again. (The data it was replacing is in {}.)",
                        folder.display(),
                        copies::folder(folder).display()
                    )));
                }
                Err(Refused(format!(
                    "There is no data in {}, but the store kept apart says there was. Nothing is served. If NEXUS_DATA names the wrong folder, set it right. If the data is lost, put a copy back with `nexus-music-server put-back`, or {TAKE_AS_IS} (here: none, and every device signs in again).",
                    folder.display()
                )))
            }
            (id, Reading::Found(store), Opened::Open(data)) => {
                let meta = data.meta().map_err(|error| Refused(format!("The data can't be read: {}.", why(&error))))?;
                if id != Some(store.backend) {
                    return Err(Refused(Self::not_this_backends(state)));
                }
                if meta.backend != store.backend {
                    return Err(Refused(format!(
                        "The data in {} is another backend's (its id is {}; this one's is {}). Nothing is served, and nothing was changed. Set NEXUS_DATA to this backend's data, or put back a copy of it.",
                        folder.display(),
                        meta.backend.to_hex(),
                        store.backend.to_hex()
                    )));
                }
                let mut keeper = Keeper { state: state.to_path_buf(), folder: folder.to_path_buf(), _lock: held, backend: store.backend, store, data, broken: false };
                keeper.store.high_water = keeper.store.high_water.max(meta.written_at);
                if !keeper.store.data_made {
                    let view = keeper.data.view().map_err(|error| Refused(format!("The data can't be read: {}.", why(&error))))?;
                    if !view.accounts.is_empty() || !view.devices.is_empty() || view.restore != keeper.store.restore {
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

    fn not_this_backends(state: &Path) -> String {
        format!(
            "The store kept apart, {}, is not this backend's: it names another backend's id than {} does, or that file is missing. Nothing is served. If NEXUS_STATE names the wrong folder, set it right; if the store is lost, {TAKE_AS_IS}.",
            apart::place(state).display(),
            state.join(identity::FILE).display()
        )
    }

    fn first_data(state: &Path, folder: &Path, held: lock::Held, mut store: Store, now: u64) -> Result<(Keeper, Started), Refused> {
        let data = Data::create(folder, &store.backend, now).map_err(Refused)?;
        store.data_made = true;
        apart::write(state, &store).map_err(|error| Refused(store_failed(state, &error)))?;
        Ok((Keeper { state: state.to_path_buf(), folder: folder.to_path_buf(), _lock: held, backend: store.backend, store, data, broken: false }, Started::Made))
    }

    /// The comparison (9.2, 11.2) and what follows from it.
    fn compare_and_bring_together(&mut self, now: u64) -> Result<Started, Refused> {
        let view = self.data.view().map_err(|error| Refused(format!("The data can't be read: {}.", why(&error))))?;
        let found = compare::compare(&self.store.view(), &view);
        match found.startup(false) {
            Startup::Ready => {
                if self.store.pending.is_some() {
                    self.bring_up(&view).map_err(|failed| Refused(failed.0))?;
                }
                Ok(Started::Ready)
            }
            plan @ (Startup::FinishEnding | Startup::EndUnknownSession) => {
                self.bring_up(&view).map_err(|failed| Refused(failed.0))?;
                Ok(Started::Finished(plan))
            }
            Startup::RestoreData if self.put_back_cut() => Err(Refused(format!(
                "A put-back was cut off before it was done, and the copy may or may not be in place. Nothing is served. Run `nexus-music-server put-back` with the copy again. (Any data it was replacing is kept in {}.)",
                copies::folder(&self.folder).display()
            ))),
            Startup::RestoreData => self.restore_data(now, false),
            Startup::Stop | Startup::HoldAll => Err(Refused(format!(
                "The store kept apart does not belong with this data: {}. Nothing is served, and nothing was changed. If NEXUS_DATA or NEXUS_STATE names the wrong folder, set it right; otherwise {TAKE_AS_IS}.",
                found.contradiction.map_or("they disagree", contradiction)
            ))),
        }
    }

    /// Whether the store notes a put-back begun and not yet past its first step in the store
    /// (11.2 step 1): the copy may or may not have been put in place, so only the command,
    /// run again, can finish it.
    fn put_back_cut(&self) -> bool {
        matches!(self.store.pending, Some(Pending::Restore { restore }) if self.store.restore.checked_add(1) == Some(restore))
    }

    /// A device's half of C3's `Apart`, with the name the data gives its account.
    fn apart_of(&self, id: u64, device: &StoreDevice, name: &str) -> Apart {
        Apart {
            scope: Scope { backend: self.backend, restore: device.restore, account: name.to_string(), life: device.life, device: id, number: device.number },
            last_session: device.last_session,
            working: device.working,
            waiting: device.waiting,
        }
    }

    fn name_of(&self, account: u64) -> Result<String, Failed> {
        Ok(self.data.account_name(account).map_err(|error| Failed(data_failed(&error)))?.unwrap_or_default())
    }

    /// 9.2: "at the next start the data is brought up to it". The change the store names as
    /// under way is finished in the data, or what the data has of it that the store never
    /// heard of is dropped; then the note is cleared.
    fn bring_up(&mut self, view: &compare::DataView) -> Result<(), Failed> {
        let mut changes_rows: Vec<Rows> = Vec::new();
        // What survives in the store too: a permit whose session the data no longer loads is
        // dead (C3's load), and is dropped, so that the two agree. Nothing that works is taken.
        let mut permits: Vec<(u64, Rows)> = Vec::new();
        let mut remove = Vec::new();
        // What the store says was taken from an account and the data has not yet (a cut
        // change of a whole account: 12.4 step 3's rules, as for a copy).
        let again = compare::taken_again(&self.store.view(), view);
        let erase: Vec<u64> = again.iter().filter(|again| again.erase).map(|again| again.account).collect();
        // A deletion of listening data cut off: the data's life brought to the store's.
        let lives: Vec<(u64, u64)> = view
            .accounts
            .iter()
            .filter_map(|(&number, row)| {
                let kept = self.store.accounts.get(&number)?;
                let cut = matches!(self.store.pending, Some(Pending::Update { account, family: Some(Family::Account), .. }) if account == number);
                (cut && !kept.kept.deleted && row.life < kept.kept.life).then_some((number, kept.kept.life))
            })
            .collect();
        for (&id, row) in &view.devices {
            if erase.contains(&row.account) {
                continue;
            }
            let Some(kept) = self.store.devices.get(&id) else { continue };
            if kept.gone {
                remove.push(id);
                continue;
            }
            let name = self.name_of(row.account)?;
            let apart = self.apart_of(id, kept, &name);
            let rows = self.data.rows(&self.backend, id).map_err(|error| Failed(data_failed(&error)))?.ok_or_else(|| Failed("A device went from the data while it was read.".into()))?;
            let up = compare::brought_up(&apart, &rows);
            permits.push((id, up.clone()));
            if up != rows {
                changes_rows.push(up);
            }
        }
        let mut changes: Vec<Change<'_>> = erase.iter().map(|&account| Change::Erase { account }).collect();
        for again in again.iter().filter(|again| !again.erase) {
            if again.off {
                changes.push(Change::SwitchedOn { account: again.account, on: false });
            }
            if again.reset_password {
                let passwords = self.store.accounts.get(&again.account).map_or(0, |account| account.kept.passwords);
                changes.push(Change::Password { account: again.account, password: None, passwords });
                changes.push(Change::Codes { account: again.account });
            }
        }
        changes.extend(lives.iter().map(|&(account, life)| Change::Life { account, life }));
        changes.extend(remove.iter().map(|&device| Change::Remove { device }));
        changes.extend(changes_rows.iter().map(Change::Install));
        if !changes.is_empty() {
            self.data.apply(&changes, self.store.high_water).map_err(|error| Failed(data_failed(&error)))?;
        }
        let mut next = self.store.clone();
        for (id, up) in &permits {
            if let Some(entry) = next.devices.get_mut(id) {
                let permit = |session: &nexus_core::nexus::kept::Session| nexus_core::nexus::kept::Permit { id: session.id, delete_proven: session.delete_proven };
                entry.working = up.working.as_ref().map(permit);
                entry.waiting = up.waiting.as_ref().map(permit);
            }
        }
        match next.pending {
            Some(Pending::MakeAccount { account }) if !view.accounts.contains_key(&account) => {
                if let Some(entry) = next.accounts.get_mut(&account) {
                    entry.kept.present = false;
                }
            }
            Some(Pending::MakeDevice { device }) if !view.devices.contains_key(&device) => {
                if let Some(entry) = next.devices.get_mut(&device) {
                    entry.gone = true;
                }
            }
            _ => {}
        }
        next.pending = None;
        self.write_store(next)
    }

    /// 11.2 steps 1 to 3, still before answering anything: a new restore number in the store
    /// and in the data; every device's sessions ended (their numbers moved, in the store, by
    /// C3's `restore_device`); and every account the copy holds again after it was deleted,
    /// held. Done again from the start if cut off. `taken` marks the owner's take-as-is,
    /// which holds every account.
    fn restore_data(&mut self, now: u64, taken: bool) -> Result<Started, Refused> {
        let refused = |failed: Failed| Refused(failed.0);
        let view = self.data.view().map_err(|error| Refused(format!("The data can't be read: {}.", why(&error))))?;
        let meta = self.data.meta().map_err(|error| Refused(format!("The data can't be read: {}.", why(&error))))?;
        let new = match self.store.pending {
            Some(Pending::Restore { restore } | Pending::TakeAsIs { restore }) => restore,
            _ => self.store.restore.checked_add(1).ok_or_else(|| Refused("The restore number can go no higher. Nothing is served.".into()))?,
        };
        // In the store first: unless it was done already, by a start or a command cut off.
        if self.store.restore != new {
            // Taken as it is, every account was marked held as the store was made, and
            // `holds` keeps each held through its life.
            let holds = compare::holds(&self.store.view(), &view);
            let mut next = self.store.clone();
            let clock = Clock { now, high_water: next.high_water };
            let ids: Vec<u64> = next.devices.keys().copied().collect();
            for id in ids {
                let Some(kept) = next.devices.get(&id).copied() else { continue };
                if kept.gone {
                    continue;
                }
                if !view.devices.contains_key(&id) {
                    // Not in the copy: its rows are lost with it, and its id is not used again.
                    if let Some(entry) = next.devices.get_mut(&id) {
                        entry.gone = true;
                    }
                    continue;
                }
                let apart = self.apart_of(id, &kept, "");
                let mut device = Device::load(&apart, &Rows { scope: apart.scope.clone(), working: None, waiting: None });
                let mut scope = None;
                let answer = device
                    .decide(&apart.scope, clock, |turn| turn.restore_device(new))
                    .apply(|effects| {
                        scope = effects.update.as_ref().map(|update| update.apart_first.clone());
                        Ok::<_, Denied>(())
                    })
                    .and_then(|answer| answer);
                match (answer, scope) {
                    (Ok(()), Some(ended)) => {
                        if let Some(entry) = next.devices.get_mut(&id) {
                            entry.restore = ended.scope.restore;
                            entry.number = ended.scope.number;
                            entry.working = ended.working;
                            entry.waiting = ended.waiting;
                        }
                    }
                    _ => return Err(Refused("A device's number can go no higher, so its sessions can't be ended. Nothing is served.".into())),
                }
            }
            for (number, account) in next.accounts.iter_mut() {
                // The copy's rows are what the data now has (an account deleted since is
                // erased again, below).
                account.kept.present = !account.kept.deleted && view.accounts.contains_key(number);
            }
            for &(number, through) in &holds {
                if let Some(account) = next.accounts.get_mut(&number) {
                    account.kept.held = Some(through);
                }
            }
            next.restore = new;
            next.high_water = next.high_water.max(now);
            next.pending = Some(if taken { Pending::TakeAsIs { restore: new } } else { Pending::Restore { restore: new } });
            self.write_store(next).map_err(refused)?;
        }
        // Then the data, in one step: 11.2 / 12.4 steps 1 to 3.
        let again = compare::taken_again(&self.store.view(), &view);
        let passwords: Vec<(u64, u64)> = again.iter().map(|again| (again.account, self.store.accounts.get(&again.account).map_or(0, |account| account.kept.passwords))).collect();
        let mut changes = vec![Change::Restore { restore: new, at: now, from: (meta.restore != new).then_some(meta.written_at) }];
        for (again, &(_, count)) in again.iter().zip(&passwords) {
            if again.erase {
                changes.push(Change::Erase { account: again.account });
                continue;
            }
            if again.off {
                changes.push(Change::SwitchedOn { account: again.account, on: false });
            }
            if again.reset_password {
                changes.push(Change::Password { account: again.account, password: None, passwords: count });
                changes.push(Change::Codes { account: again.account });
            }
        }
        let erased: Vec<u64> = again.iter().filter(|again| again.erase).map(|again| again.account).collect();
        let mut devices = 0;
        for (&id, row) in &view.devices {
            if erased.contains(&row.account) {
                continue;
            }
            match self.store.devices.get(&id) {
                Some(kept) if !kept.gone => {
                    devices += 1;
                    changes.push(Change::Ended { device: id, number: kept.number });
                }
                _ => changes.push(Change::Remove { device: id }),
            }
        }
        self.data.apply(&changes, self.store.high_water).map_err(|error| Refused(data_failed(&error)))?;
        // Then the note is cleared.
        let held = self.store.accounts.values().filter(|account| account.kept.held.is_some_and(|through| through >= account.kept.life)).count();
        let mut next = self.store.clone();
        next.pending = None;
        self.write_store(next).map_err(refused)?;
        Ok(Started::Restored { restore: new, devices, held, from: meta.written_at })
    }

    /// Writes the store, and keeps it as written. On a failure nothing is kept in memory but
    /// what is on the disk.
    fn write_store(&mut self, next: Store) -> Result<(), Failed> {
        match apart::write(&self.state, &next) {
            Ok(()) => {
                self.store = next;
                Ok(())
            }
            Err(error) => {
                // A failure after the rename leaves the new store on the disk.
                if let Reading::Found(on_disk) = apart::read(&self.state) {
                    self.store = on_disk;
                }
                Err(Failed(store_failed(&self.state, &error)))
            }
        }
    }

    /// After a step failed part-way: the two compared and brought together again, as at a
    /// start. Until that succeeds, nothing is written.
    fn heal(&mut self) -> Result<(), Failed> {
        if !self.broken {
            return Ok(());
        }
        match apart::read(&self.state) {
            Reading::Found(store) => self.store = store,
            _ => return Err(Failed("The store kept apart can't be read back.".into())),
        }
        let view = self.data.view().map_err(|error| Failed(data_failed(&error)))?;
        match compare::compare(&self.store.view(), &view).startup(false) {
            Startup::Ready | Startup::FinishEnding | Startup::EndUnknownSession => {
                self.bring_up(&view)?;
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

    /// The restore number (9.2): every request names the one it holds.
    pub fn restore(&self) -> u64 {
        self.store.restore
    }

    /// The key answers are stamped with (9.2).
    pub fn stamp_key(&self) -> StampKey {
        self.store.stamp_key()
    }

    /// The clock for a decision at `now` (C3's `Clock`): the later of `now` and the latest time
    /// the backend has acted at, which outlasts a restart and a data folder put back (7.6).
    pub fn clock(&self, now: u64) -> Clock {
        Clock { now, high_water: self.store.high_water }
    }

    /// Whether this account's history is held (11.2 step 3).
    pub fn held(&self, account: u64) -> bool {
        self.store.accounts.get(&account).is_some_and(|account| account.kept.held.is_some_and(|through| through >= account.kept.life))
    }

    /// The account with this email (as tidied: C7's), if the data has it.
    pub fn find_account(&self, email: &str) -> Result<Option<AccountFound>, Failed> {
        Ok(self.data.account_named(email).map_err(|error| Failed(data_failed(&error)))?.map(|(number, life)| AccountFound { number, life, held: self.held(number) }))
    }

    /// An account made with this email (as tidied: C7's), or the one there is: in the store
    /// first (its number, never used again, and its life, which outlives its rows), then in
    /// the data, switched on and with no password yet.
    pub fn account(&mut self, email: &str, shown: &str, admin: bool, now: u64) -> Result<AccountFound, Failed> {
        self.heal()?;
        if let Some(found) = self.find_account(email)? {
            return Ok(found);
        }
        let mut next = self.store.clone();
        let number = match next.account_named(email) {
            Some(number) => number,
            None => {
                let number = next.next_account;
                next.next_account = number.checked_add(1).ok_or_else(|| Failed("No more accounts can be made.".into()))?;
                let kept = StoreAccount { life: 0, held: None, present: false, deleted: false, off: false, passwords: 0 };
                next.accounts.insert(number, Account { name: apart::name_fingerprint(email), kept });
                number
            }
        };
        let (life, passwords) = next.accounts.get(&number).map_or((0, 0), |account| (account.kept.life, account.kept.passwords));
        if let Some(account) = next.accounts.get_mut(&number) {
            account.kept.present = true;
            account.kept.off = false;
        }
        next.high_water = next.high_water.max(now);
        next.pending = Some(Pending::MakeAccount { account: number });
        self.write_store(next)?;
        if let Err(error) = self.data.apply(&[Change::Account { number, email, shown, life, passwords, admin }], self.store.high_water) {
            return self.broke(Failed(data_failed(&error)));
        }
        let mut next = self.store.clone();
        next.pending = None;
        if let Err(failed) = self.write_store(next) {
            return self.broke(failed);
        }
        Ok(AccountFound { number, life, held: self.held(number) })
    }

    /// A change to a whole account (design 8.7, 12.3, 12.5), each all or nothing: what it
    /// takes away in the store first, under a note; then the data in one step; then the note
    /// cleared. Every device of the account but `keep` has its session ended (C3's ending).
    pub fn change_account(&mut self, account: u64, change: AccountChange<'_>, keep: Option<u64>, now: u64) -> Result<(), Failed> {
        self.heal()?;
        let Some(kept) = self.store.accounts.get(&account).map(|account| account.kept).filter(|kept| kept.present && !kept.deleted) else {
            return Err(Failed("There is no such account.".into()));
        };
        let mut next = self.store.clone();
        next.high_water = next.high_water.max(now);
        let clock = Clock { now, high_water: next.high_water };
        let ending = !matches!(change, AccountChange::SwitchOn);
        let mut numbers = Vec::new();
        if ending {
            let ids: Vec<u64> = next.devices.iter().filter(|(id, device)| device.account == account && !device.gone && Some(**id) != keep).map(|(&id, _)| id).collect();
            for id in ids {
                let Some(device) = next.devices.get(&id).copied() else { continue };
                let apart = self.apart_of(id, &device, "");
                let mut loaded = Device::load(&apart, &Rows { scope: apart.scope.clone(), working: None, waiting: None });
                let mut ended = None;
                let answer = loaded
                    .decide(&apart.scope, clock, |turn| turn.end())
                    .apply(|effects| {
                        ended = effects.update.as_ref().map(|update| update.apart_first.clone());
                        Ok::<_, Denied>(())
                    })
                    .and_then(|answer| answer);
                let (Ok(()), Some(ended)) = (answer, ended) else {
                    return Err(Failed("A device's number can go no higher.".into()));
                };
                if let Some(entry) = next.devices.get_mut(&id) {
                    entry.number = ended.scope.number;
                    entry.working = ended.working;
                    entry.waiting = ended.waiting;
                    entry.gone = matches!(change, AccountChange::Delete);
                }
                numbers.push((id, ended.scope.number));
            }
        }
        let passwords = kept.passwords.checked_add(1).ok_or_else(|| Failed("The account's password can be changed no more.".into()))?;
        if let Some(entry) = next.accounts.get_mut(&account) {
            match change {
                AccountChange::SwitchOff => entry.kept.off = true,
                AccountChange::SwitchOn => {}
                AccountChange::Delete => {
                    entry.kept.deleted = true;
                    entry.kept.present = false;
                    entry.kept.held = None;
                }
                AccountChange::Password(_) => entry.kept.passwords = passwords,
            }
        }
        next.pending = Some(Pending::Account { account });
        self.write_store(next)?;
        let mut changes = match change {
            AccountChange::Delete => vec![Change::Erase { account }],
            AccountChange::SwitchOff => vec![Change::SwitchedOn { account, on: false }],
            AccountChange::SwitchOn => vec![Change::SwitchedOn { account, on: true }],
            AccountChange::Password(password) => vec![Change::Password { account, password, passwords }, Change::Codes { account }],
        };
        if ending && !matches!(change, AccountChange::Delete) {
            changes.push(Change::EndedAll { account, numbers: &numbers });
        }
        if let Err(error) = self.data.apply(&changes, self.store.high_water) {
            return self.broke(Failed(data_failed(&error)));
        }
        let mut next = self.store.clone();
        if let (AccountChange::SwitchOn, Some(entry)) = (change, next.accounts.get_mut(&account)) {
            // Gives leave: the store hears of it last.
            entry.kept.off = false;
        }
        next.pending = None;
        if let Err(failed) = self.write_store(next) {
            return self.broke(failed);
        }
        if matches!(change, AccountChange::Delete) {
            let _ = self.data.fold();
        }
        Ok(())
    }

    /// The device of `account` with this secret's fingerprint, made if there is none (7.5: a
    /// device is found by its secret, never by an id it claims): in the store first, so that
    /// its id is never given again, then in the data. Whether it was made now.
    pub fn device(&mut self, account: u64, secret: &Fingerprint, name: &str, kind: &str, version: &str, now: u64) -> Result<(u64, bool), Failed> {
        self.heal()?;
        if let Some(id) = self.data.device_with_secret(account, secret).map_err(|error| Failed(data_failed(&error)))? {
            return Ok((id, false));
        }
        let Some(kept) = self.store.accounts.get(&account).filter(|account| account.kept.present).map(|account| account.kept) else {
            return Err(Failed("There is no such account.".into()));
        };
        let mut next = self.store.clone();
        let id = next.next_device;
        next.next_device = id.checked_add(1).ok_or_else(|| Failed("No more devices can be made.".into()))?;
        next.devices.insert(id, StoreDevice { account, restore: next.restore, life: kept.life, number: 0, last_session: 0, working: None, waiting: None, gone: false });
        next.high_water = next.high_water.max(now);
        next.pending = Some(Pending::MakeDevice { device: id });
        self.write_store(next)?;
        let at = self.store.high_water;
        if let Err(error) = self.data.apply(&[Change::Device { id, account, secret, name, kind, version, at }], at) {
            return self.broke(Failed(data_failed(&error)));
        }
        let mut next = self.store.clone();
        next.pending = None;
        if let Err(failed) = self.write_store(next) {
            return self.broke(failed);
        }
        Ok((id, true))
    }

    /// A device as C3 decides with it: loaded from both stores (`Device::load`, under which a
    /// session known on one side only is dead), and the scope to decide under.
    /// The scope a device's next decision is made under: the store's, which is the
    /// authority (7.7). A device kept in memory between two decisions (a try begun and
    /// finished) is decided under this each time.
    pub fn authority(&self, id: u64) -> Result<Scope, Failed> {
        let Some(kept) = self.store.devices.get(&id).copied().filter(|device| !device.gone) else {
            return Err(Failed("There is no such device.".into()));
        };
        let name = self.name_of(kept.account)?;
        Ok(self.apart_of(id, &kept, &name).scope)
    }

    pub fn load(&mut self, id: u64) -> Result<(Device, Scope), Failed> {
        self.heal()?;
        let Some(kept) = self.store.devices.get(&id).copied().filter(|device| !device.gone) else {
            return Err(Failed("There is no such device.".into()));
        };
        let rows = self.data.rows(&self.backend, id).map_err(|error| Failed(data_failed(&error)))?.ok_or_else(|| Failed("There is no such device.".into()))?;
        let apart = self.apart_of(id, &kept, &rows.scope.account);
        Ok((Device::load(&apart, &rows), apart.scope))
    }

    /// Keeps what a decision says is to be kept (C3's `Effects`), **whatever it answered**,
    /// and before its answer is given: the time it acted at; and, if it changed anything,
    /// what it took away in the store first, then the data in one step, then what it gave in
    /// the store last. An ending of a whole account (a deletion) erases its history in the
    /// data's step (11.1). If this fails, the answer is not given and the device is loaded
    /// again before it is used.
    pub fn persist(&mut self, effects: &Effects) -> Result<(), Failed> {
        self.heal()?;
        let high_water = self.store.high_water.max(effects.high_water);
        let Some(update) = &effects.update else {
            if high_water > self.store.high_water {
                let mut next = self.store.clone();
                next.high_water = high_water;
                self.write_store(next)?;
            }
            return Ok(());
        };
        let id = update.previous.device;
        let Some(kept) = self.store.devices.get(&id).copied().filter(|device| !device.gone) else {
            return Err(Failed("There is no such device.".into()));
        };
        let current = self.apart_of(id, &kept, &update.previous.account).scope;
        if current != update.previous || update.previous.backend != self.backend {
            return Err(Failed("The device changed since it was loaded; it is loaded again.".into()));
        }
        let account = kept.account;
        let mut next = self.store.clone();
        next.high_water = high_water;
        let family = update.family;
        let others: Vec<u64> = next.devices.iter().filter(|(other, device)| **other != id && !device.gone).map(|(other, _)| *other).collect();
        if let Some(family @ (Family::Account | Family::Backend)) = family {
            for other in others {
                let Some(device) = next.devices.get(&other).copied() else { continue };
                if family == Family::Account && device.account != account {
                    continue;
                }
                let scope = self.apart_of(other, &device, &update.previous.account).scope;
                let ended = family.end_other(update, &scope).map_err(|_| Failed("A device's number can go no higher.".into()))?;
                if let (Some(ended), Some(entry)) = (ended, next.devices.get_mut(&other)) {
                    entry.restore = ended.restore;
                    entry.life = ended.life;
                    entry.number = ended.number;
                    entry.working = None;
                    entry.waiting = None;
                }
            }
        }
        let first = &update.apart_first;
        if let Some(entry) = next.devices.get_mut(&id) {
            entry.restore = first.scope.restore;
            entry.life = first.scope.life;
            entry.number = first.scope.number;
            entry.last_session = first.last_session;
            entry.working = first.working;
            entry.waiting = first.waiting;
        }
        match family {
            Some(Family::Account) => {
                // Listening data deleted (12.5): the life moves; the account stays.
                if let Some(entry) = next.accounts.get_mut(&account) {
                    entry.kept.life = first.scope.life;
                    entry.kept.held = None;
                }
            }
            Some(Family::Backend) => next.restore = first.scope.restore,
            _ => {}
        }
        next.pending = Some(Pending::Update { device: id, account, family });
        self.write_store(next)?;
        let at = self.store.high_water;
        let numbers: Vec<(u64, u64)> =
            self.store.devices.iter().filter(|(device, kept)| kept.account == account && **device != id && !kept.gone).map(|(&device, kept)| (device, kept.number)).collect();
        let result = match family {
            // The account's events go in this step, once they are kept (S12): their table
            // is the account's, and goes with its life.
            Some(Family::Account) => self.data.apply(
                &[Change::Life { account, life: first.scope.life }, Change::EndedAll { account, numbers: &numbers }, Change::Install(&update.data)],
                at,
            ),
            Some(Family::Backend) => {
                let mut changes = vec![Change::Restore { restore: first.scope.restore, at, from: None }];
                let numbers: Vec<(u64, u64)> = self.store.devices.iter().filter(|(_, device)| !device.gone).map(|(&device, kept)| (device, kept.number)).collect();
                changes.extend(numbers.iter().filter(|(device, _)| *device != id).map(|&(device, number)| Change::Ended { device, number }));
                changes.push(Change::Install(&update.data));
                self.data.apply(&changes, at)
            }
            _ => self.data.apply(&[Change::Install(&update.data)], at),
        };
        if let Err(error) = result {
            return self.broke(Failed(data_failed(&error)));
        }
        let mut next = self.store.clone();
        let last = &update.apart_last;
        if let Some(entry) = next.devices.get_mut(&id) {
            entry.last_session = last.last_session;
            entry.working = last.working;
            entry.waiting = last.waiting;
        }
        next.pending = None;
        if let Err(failed) = self.write_store(next) {
            return self.broke(failed);
        }
        if family == Some(Family::Account) {
            // What was deleted, out of the file SQLite writes ahead in, as soon as it can be.
            let _ = self.data.fold();
        }
        Ok(())
    }

    /// The person of a held account answered "keep" (11.2 step 3): the data's life brought to
    /// the store's (in the data first, as it gives leave), then the hold let go.
    pub fn keep_held(&mut self, account: u64) -> Result<(), Failed> {
        self.heal()?;
        let Some(kept) = self.store.accounts.get(&account).map(|account| account.kept).filter(|kept| kept.held.is_some()) else {
            return Ok(());
        };
        if let Err(error) = self.data.apply(&[Change::Life { account, life: kept.life }], self.store.high_water) {
            return self.broke(Failed(data_failed(&error)));
        }
        let mut next = self.store.clone();
        if let Some(entry) = next.accounts.get_mut(&account) {
            entry.kept.held = None;
        }
        if let Err(failed) = self.write_store(next) {
            return self.broke(failed);
        }
        Ok(())
    }

    /// Removes a device (stage 2's Remove; a device unheard for 400 days): gone in the store
    /// first, so that no copy brings it back, then from the data.
    pub fn remove_device(&mut self, id: u64) -> Result<(), Failed> {
        self.heal()?;
        if self.store.devices.get(&id).is_none_or(|device| device.gone) {
            return Ok(());
        }
        let mut next = self.store.clone();
        if let Some(entry) = next.devices.get_mut(&id) {
            entry.gone = true;
            entry.working = None;
            entry.waiting = None;
        }
        next.pending = Some(Pending::Remove { device: id });
        self.write_store(next)?;
        if let Err(error) = self.data.apply(&[Change::Remove { device: id }], self.store.high_water) {
            return self.broke(Failed(data_failed(&error)));
        }
        let mut next = self.store.clone();
        next.pending = None;
        if let Err(failed) = self.write_store(next) {
            return self.broke(failed);
        }
        Ok(())
    }

    /// The data, to read from (nothing is written through it).
    pub fn data(&self) -> &Data {
        &self.data
    }

    /// The data folder.
    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// Lets go of both stores, the data folded back into its one file.
    pub fn close(self) {
        self.data.close();
    }
}

// ---- The owner's two commands (13.2, 11.2) ----

/// What a command did, in plain words.
pub type Said = Vec<String>;

/// `put-back <copy>`: puts a copy of the data back (11.2), by the supported way. The copy is
/// checked first (its form, its pages, that it is this backend's); the data it replaces is
/// kept in `copies`; then, before anything is served, the data has a new restore number,
/// every device's sessions are ended, and every account deleted since the copy was made is
/// held. Refused while the backend serves.
pub fn put_back(state: &Path, folder: &Path, copy: &Path, now: u64) -> Result<Said, Refused> {
    let held = lock::hold(state, folder).map_err(Refused)?;
    apart::tidy(state);
    data::tidy(folder);
    let mut store = match apart::read(state) {
        Reading::Found(store) => store,
        Reading::Missing => {
            return Err(Refused(format!(
                "The store kept apart, {}, is missing, so a copy can't be put back this way: nothing would then say which histories were deleted. Nothing was changed. Put the copy at {} yourself and {TAKE_AS_IS}.",
                apart::place(state).display(),
                data::place(folder).display()
            )));
        }
        Reading::Unreadable(what) => {
            return Err(Refused(format!("The store kept apart, {}, can't be read: {what}. Nothing was changed.", apart::place(state).display())));
        }
    };
    if identity::read(state).map_err(Refused)? != Some(store.backend) {
        return Err(Refused(Keeper::not_this_backends(state)));
    }
    let source = data::open_to_read(copy, "The copy").map_err(Refused)?;
    let (backend, written_at) = data::identify(&source).map_err(|error| Refused(format!("The copy can't be read: {}.", why(&error))))?;
    if backend != store.backend {
        return Err(Refused(format!(
            "The copy is another backend's data (its id is {}; this one's is {}). Nothing was changed.",
            backend.to_hex(),
            store.backend.to_hex()
        )));
    }
    // A put-back cut off before is taken up again under the number it had.
    let new = match store.pending {
        Some(Pending::Restore { restore }) if store.restore.checked_add(1) == Some(restore) => restore,
        _ => store.restore.checked_add(1).ok_or_else(|| Refused("The restore number can go no higher. Nothing was changed.".into()))?,
    };
    // 1. The store first: a restore is under way (and done again from the start if cut off).
    store.pending = Some(Pending::Restore { restore: new });
    apart::write(state, &store).map_err(|error| Refused(store_failed(state, &error)))?;
    // 2. The data it replaces is kept, whole, with what SQLite had written ahead of it.
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
    // 3. The copy in its place, whole and lasting.
    copies::put(&source, &place).map_err(|why| Refused(format!("The copy couldn't be put in place: {why}. Run the put-back again.")))?;
    drop(source);
    // 4. Then, as at a start that finds the data older: 11.2's steps 1 to 3.
    let data = match Data::open(folder, now).map_err(Refused)? {
        Opened::Open(data) => data,
        Opened::Absent => return Err(Refused("The copy was not in place once put there. Run the put-back again.".into())),
    };
    let mut keeper = Keeper { state: state.to_path_buf(), folder: folder.to_path_buf(), _lock: held, backend: store.backend, store, data, broken: false };
    let started = keeper.restore_data(now, false)?;
    keeper.close();
    said.insert(0, format!("Put back {} (its data's last step was at {written_at} ms).", copy.display()));
    said.push(started.said());
    said.push("Start the backend again. Every device signs in again by itself, sends again whatever it holds as sent, and fetches from the beginning.".into());
    Ok(said)
}

/// `take-as-is`: takes the data as it is when the store kept apart is lost, cannot be read, or
/// does not belong with the data (11.2). That does 11.2's steps 1 and 2, and holds every
/// account's history until a device of each signs in and its person answers keep or delete,
/// since without the store nothing says which were deleted. With no data, it starts again
/// with none, keeping what the store has. Refused while the backend serves, and when the
/// store and the data agree.
pub fn take_as_is(state: &Path, folder: &Path, now: u64) -> Result<Said, Refused> {
    let held = lock::hold(state, folder).map_err(Refused)?;
    let found = find(state, folder, now)?;
    let mut said = Vec::new();
    match (found.id, found.store, found.data) {
        (_, Reading::Missing, Opened::Absent) => Err(Refused("There is neither data nor a store kept apart: there is nothing to take. Start the backend, and it makes both.".into())),
        (id, Reading::Found(mut store), Opened::Absent) => {
            if id != Some(store.backend) {
                return Err(Refused(Keeper::not_this_backends(state)));
            }
            // The data is lost: none is taken. Every device's rows went with it.
            let data = Data::create(folder, &store.backend, now).map_err(Refused)?;
            data.close();
            for device in store.devices.values_mut() {
                device.gone = true;
                device.working = None;
                device.waiting = None;
            }
            for account in store.accounts.values_mut() {
                account.kept.present = false;
            }
            store.data_made = true;
            // A new restore number, so that no device keeps how far it had fetched (9.2).
            let restore = store.restore.checked_add(1).ok_or_else(|| Refused("The restore number can go no higher.".into()))?;
            store.pending = Some(Pending::Restore { restore });
            apart::write(state, &store).map_err(|error| Refused(store_failed(state, &error)))?;
            let data = match Data::open(folder, now).map_err(Refused)? {
                Opened::Open(data) => data,
                Opened::Absent => return Err(Refused("The new data was not there once made. Run take-as-is again.".into())),
            };
            let mut keeper = Keeper { state: state.to_path_buf(), folder: folder.to_path_buf(), _lock: held, backend: store.backend, store, data, broken: false };
            let started = keeper.restore_data(now, false)?;
            let restore = keeper.restore();
            keeper.close();
            said.push(format!("There was no data: the backend starts again with none, under restore number {restore}. Accounts and devices sign in again; each account goes on from the life it had."));
            said.push(started.said());
            Ok(said)
        }
        (id, store, Opened::Open(data)) => {
            let meta = data.meta().map_err(|error| Refused(format!("The data can't be read: {}.", why(&error))))?;
            let view = data.view().map_err(|error| Refused(format!("The data can't be read: {}.", why(&error))))?;
            let old = match store {
                Reading::Found(store) => {
                    if id == Some(store.backend) && meta.backend == store.backend {
                        let found = compare::compare(&store.view(), &view);
                        if found.contradiction.is_none() && store.data_made {
                            return Err(Refused("The store kept apart is there and belongs with the data: there is nothing to take. Start the backend; it compares the two itself.".into()));
                        }
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
            if apart::place(state).exists() {
                let aside = apart::set_aside(state, now).map_err(|error| Refused(format!("The old store couldn't be put aside ({}). Nothing was changed.", error.kind())))?;
                said.push(format!("The old store kept apart is kept as {}.", aside.display()));
            }
            // A new store, from the data as it is; what a readable old one knew that only goes
            // up is kept (its restore number, lives, and the ids it gave).
            let old_restore = old.as_ref().map_or(0, |old| old.restore);
            let new = meta.restore.max(old_restore).checked_add(1).ok_or_else(|| Refused("The restore number can go no higher.".into()))?;
            // The key that seals is lost with the store, if it was not readable: what it sealed
            // is made anew or typed in again (12.4).
            let seal = match &old {
                Some(old) => old.seal_key(),
                None => random_bytes::<SECRET_BYTES>().map_err(Refused)?,
            };
            let mut store = Store::new(meta.backend, random_bytes::<SECRET_BYTES>().map_err(Refused)?, seal, now.max(meta.written_at).max(old.as_ref().map_or(0, |old| old.high_water)));
            store.data_made = true;
            store.restore = new.saturating_sub(1);
            store.next_account = view.accounts.keys().max().map_or(1, |n| n.saturating_add(1)).max(old.as_ref().map_or(1, |old| old.next_account));
            store.next_device = view.devices.keys().max().map_or(1, |n| n.saturating_add(1)).max(old.as_ref().map_or(1, |old| old.next_device));
            if let Some(old) = &old {
                for (&number, account) in &old.accounts {
                    if !view.accounts.contains_key(&number) {
                        store.accounts.insert(number, Account { name: account.name, kept: StoreAccount { present: false, held: None, ..account.kept } });
                    }
                }
                for (&id, device) in &old.devices {
                    if !view.devices.contains_key(&id) {
                        store.devices.insert(id, StoreDevice { gone: true, working: None, waiting: None, ..*device });
                    }
                }
            }
            for (&number, row) in &view.accounts {
                let name = data.account_name(number).map_err(|error| Refused(format!("The data can't be read: {}.", why(&error))))?.unwrap_or_default();
                let life = row.life.max(old.as_ref().and_then(|old| old.accounts.get(&number)).map_or(0, |account| account.kept.life));
                // 11.2: without the store nothing says which were deleted (C3's `HoldAll`).
                let held = (at_start(StoreComparison::StoreLost, true) == Startup::HoldAll).then_some(life);
                // 12.4: it resets nothing it cannot know of: on or off, and the count, as the data has them.
                let kept = StoreAccount { life, held, present: true, deleted: false, off: !row.on, passwords: row.passwords };
                store.accounts.insert(number, Account { name: apart::name_fingerprint(&name), kept });
            }
            for (&id, row) in &view.devices {
                let life = store.accounts.get(&row.account).map_or(0, |account| account.kept.life);
                let last = row.sessions.iter().map(|session| session.id).max().unwrap_or(0).max(old.as_ref().and_then(|old| old.devices.get(&id)).map_or(0, |device| device.last_session));
                let number = row.number.max(old.as_ref().and_then(|old| old.devices.get(&id)).map_or(0, |device| device.number));
                store.devices.insert(id, StoreDevice { account: row.account, restore: store.restore, life, number, last_session: last, working: None, waiting: None, gone: false });
            }
            store.pending = Some(Pending::TakeAsIs { restore: new });
            apart::write(state, &store).map_err(|error| Refused(store_failed(state, &error)))?;
            let mut keeper = Keeper { state: state.to_path_buf(), folder: folder.to_path_buf(), _lock: held, backend: store.backend, store, data, broken: false };
            let started = keeper.restore_data(now, true)?;
            keeper.close();
            said.insert(0, "Took the data as it is.".into());
            said.push(started.said());
            said.push("Start the backend again.".into());
            Ok(said)
        }
        (_, Reading::Unreadable(_), Opened::Absent) => Err(Refused("There is no data, and the store kept apart can't be read: there is nothing to take. Put a copy of the data back by hand into NEXUS_DATA and run this again.".into())),
    }
}
