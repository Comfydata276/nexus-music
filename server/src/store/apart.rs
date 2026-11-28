//! The store kept apart (design 11, 12.3): one small file, `NEXUS_STATE/kept`, beside the
//! backend's id. **What must not go backwards when the data does**: the restore number; the
//! key that seals and the key answers are stamped with; the latest time the backend has acted
//! at; whether it has been set up; how far the data's changes had got (written at most once
//! a minute); and, for each account by its number, C3r's `Apart` (its state: exists or
//! deleted, life, revision, switched off, admin, count of passwords, whether a one-time code
//! is live, the music login's number; and each device's numbers and the session it may use).
//!
//! - **Written whole, every time**: to a file of its own, made lasting, then renamed over
//!   the last, and the folder made lasting. A cut leaves the old store or the new.
//! - **Read strictly**: every line in its place, and a check sum over all of them. Anything
//!   else is a store that cannot be read (12.4), and the program stops.
//! - **Holds no email and no session**: an account is its number, with a fingerprint of its
//!   email (so that its number is found again); a session is the number C3r gives it. The
//!   keys are secret: the file is the owner's alone to read (0600), and they are never
//!   written anywhere else.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use nexus_core::nexus::compare::{Pending, StoreView};
use nexus_core::nexus::kept::{AccountName, AccountState, AdminKind, Apart, ApartDevice, Leave, LeaveFor, MusicRecovery, Permit, SECRET_BYTES, StampKey, Status};
use nexus_core::nexus::proto::BackendId;
use sha2::{Digest, Sha256};

use crate::store::crash;

/// The file's name inside `NEXUS_STATE`.
pub const FILE: &str = "kept";
const STAGING: &str = "kept.new-";
const HEAD: &str = "nexus-music-server store kept apart 3";
/// The most a store may be: what [`read`] reads, and so what [`write`] ever publishes.
pub const FILE_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// The most a store may be after a write that adds an account or a device. The room above it
/// is kept for what takes leave away (an ending, a switch off, a deletion's tombstone), which
/// adds no record, so that nothing a capacity rule refuses is ever a revocation.
pub const GROWTH_MAX_BYTES: u64 = FILE_MAX_BYTES - 1024 * 1024;

/// One account: the fingerprint of its email, and C3r's record. The record's `name` is not
/// kept: it is filled from the data when the two are used together ([`Store::apart`]).
#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    pub name: [u8; 32],
    pub apart: Apart,
}

#[derive(Clone, PartialEq, Eq)]
pub struct Store {
    pub backend: BackendId,
    pub restore: u64,
    /// The latest time, in milliseconds on the backend's clock, it has acted at.
    pub high_water: u64,
    /// How far the data's changes had got when this was last written (12.3).
    pub mark: u64,
    /// When the mark was last written, on the backend's clock.
    pub mark_at: u64,
    key: [u8; SECRET_BYTES],
    seal: [u8; SECRET_BYTES],
    /// The backend has been set up (its first admin made, 8.4). Written here first.
    pub set_up: bool,
    /// The data has been made for this store. Until then no data is a first start cut short.
    pub data_made: bool,
    pub next_account: u64,
    pub next_device: u64,
    pub accounts: BTreeMap<u64, Account>,
    pub pending: Option<Pending>,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("restore", &self.restore)
            .field("high_water", &self.high_water)
            .field("mark", &self.mark)
            .field("set_up", &self.set_up)
            .field("accounts", &self.accounts.len())
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

/// The fingerprint by which the store knows an account's email: SHA-256 of a fixed prefix and
/// its bytes. (The design does not say how the store finds an account again by its email
/// once the data has lost its row; this does, and holds nothing else of it.)
pub fn name_fingerprint(name: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"Nexus Music account name v1\0");
    hash.update(name.as_bytes());
    hash.finalize().into()
}

/// The name a record carries until the data's is filled in: never a real email (`.invalid`).
pub fn placeholder(number: u64) -> Option<AccountName> {
    AccountName::from_canonical(format!("account-{number}@kept-apart.invalid"))
}

impl Store {
    pub fn new(backend: BackendId, key: [u8; SECRET_BYTES], seal: [u8; SECRET_BYTES], high_water: u64) -> Self {
        Store {
            backend,
            restore: 0,
            high_water,
            mark: 0,
            mark_at: high_water,
            key,
            seal,
            set_up: false,
            data_made: false,
            next_account: 1,
            next_device: 1,
            accounts: BTreeMap::new(),
            pending: None,
        }
    }

    /// How many records it holds: its accounts (tombstones included) and their devices.
    pub fn records(&self) -> usize {
        self.accounts.values().map(|account| 1 + account.apart.devices.len()).sum()
    }

    pub fn stamp_key(&self) -> StampKey {
        StampKey::new(self.key)
    }

    /// The key that seals (9.3). Its bytes are the caller's to wipe once used.
    pub fn seal_key(&self) -> [u8; SECRET_BYTES] {
        self.seal
    }

    /// An account's record, with the name `name` (the data's) in it.
    pub fn apart(&self, number: u64, name: Option<&AccountName>) -> Option<Apart> {
        let mut apart = self.accounts.get(&number)?.apart.clone();
        apart.account.name = name.cloned().or_else(|| placeholder(number))?;
        Some(apart)
    }

    /// What `compare` reads, with the data's names filled in.
    pub fn view(&self, names: &BTreeMap<u64, AccountName>) -> StoreView {
        StoreView {
            restore: self.restore,
            mark: self.mark,
            accounts: self.accounts.keys().filter_map(|&number| Some((number, self.apart(number, names.get(&number))?))).collect(),
            pending: self.pending.clone(),
        }
    }

    /// The account with this email that is not deleted, if the store has one.
    pub fn account_named(&self, name: &str) -> Option<u64> {
        let print = name_fingerprint(name);
        self.accounts.iter().find(|(_, account)| account.name == print && account.apart.account.status != Status::Deleted).map(|(&number, _)| number)
    }

    fn to_text(&self) -> String {
        let mut text = String::new();
        let mut line = |words: String| {
            text.push_str(&words);
            text.push('\n');
        };
        line(HEAD.to_string());
        line(format!("backend {}", self.backend.to_hex()));
        line(format!("restore {}", self.restore));
        line(format!("high-water {}", self.high_water));
        line(format!("mark {} at {}", self.mark, self.mark_at));
        line(format!("key {}", hex(&self.key)));
        line(format!("seal {}", hex(&self.seal)));
        line(format!("set-up {}", u8::from(self.set_up)));
        line(format!("data-made {}", u8::from(self.data_made)));
        line(format!("next-account {}", self.next_account));
        line(format!("next-device {}", self.next_device));
        for (number, account) in &self.accounts {
            let a = &account.apart.account;
            line(format!(
                "account {number} name {} restore {} life {} status {} passwords {} reset {} code {} admin {} music {} managed {} recovery {} held {} revision {}",
                hex(&account.name),
                a.restore,
                a.life,
                status_word(a.status),
                a.password_changes,
                u8::from(a.password_reset_required),
                u8::from(a.code_available),
                u8::from(a.admin),
                a.music_login,
                u8::from(a.music_managed),
                recovery_word(a.music_recovery),
                u8::from(a.history_held),
                a.revision
            ));
            for (id, device) in &account.apart.devices {
                line(format!(
                    "device {id} number {} last {} leaves {} permit {}",
                    device.number,
                    device.last_session,
                    device.last_leave,
                    device.permit.map_or_else(|| "-".to_string(), |permit| format!("{}:{}", permit.id, leave_text(permit.leave)))
                ));
            }
        }
        line(match &self.pending {
            None => "pending none".to_string(),
            Some(Pending::Change { accounts }) => {
                format!("pending change{}", accounts.iter().map(|account| format!(" {account}")).collect::<String>())
            }
            Some(Pending::Restore { restore }) => format!("pending restore {restore}"),
            Some(Pending::TakeAsIs { restore }) => format!("pending take-as-is {restore}"),
        });
        let sum = hex(Sha256::digest(text.as_bytes()).as_slice());
        text.push_str(&format!("sum {sum}\n"));
        text
    }

    fn from_text(text: &str) -> Option<Self> {
        let body_end = text.rfind("sum ")?;
        let (body, sum) = text.split_at(body_end);
        let sum = sum.strip_prefix("sum ")?.strip_suffix('\n')?;
        if sum != hex(Sha256::digest(body.as_bytes()).as_slice()) {
            return None;
        }
        let mut lines = body.strip_suffix('\n')?.split('\n').peekable();
        if lines.next()? != HEAD {
            return None;
        }
        let mut next = || lines.next();
        let backend = BackendId::from_hex(field(next()?, "backend")?)?;
        let restore = number(field(next()?, "restore")?)?;
        let high_water = number(field(next()?, "high-water")?)?;
        let (mark, mark_at) = match field(next()?, "mark")?.split(' ').collect::<Vec<_>>().as_slice() {
            [mark, "at", at] => (number(mark)?, number(at)?),
            _ => return None,
        };
        let key = unhex(field(next()?, "key")?)?;
        let seal = unhex(field(next()?, "seal")?)?;
        let set_up = flag(field(next()?, "set-up")?)?;
        let data_made = flag(field(next()?, "data-made")?)?;
        let next_account = number(field(next()?, "next-account")?)?;
        let next_device = number(field(next()?, "next-device")?)?;
        drop(next);
        let mut store = Store { restore, high_water, mark, mark_at, set_up, data_made, next_account, next_device, ..Store::new(backend, key, seal, 0) };
        let mut line = lines.next()?;
        let mut devices_seen = std::collections::BTreeSet::new();
        while let Some(rest) = line.strip_prefix("account ") {
            let words: Vec<&str> = rest.split(' ').collect();
            let [n, "name", name, "restore", r, "life", life, "status", status, "passwords", passwords, "reset", reset, "code", code, "admin", admin, "music", music, "managed", managed, "recovery", recovery, "held", held, "revision", revision] =
                words.as_slice()
            else {
                return None;
            };
            let n = number(n)?;
            if n == 0 || n >= store.next_account || store.accounts.contains_key(&n) {
                return None;
            }
            let state = AccountState {
                backend,
                restore: number(r)?,
                name: placeholder(n)?,
                life: number(life)?,
                status: unstatus(status)?,
                password_changes: number(passwords)?,
                password_reset_required: flag(reset)?,
                code_available: flag(code)?,
                admin: flag(admin)?,
                music_login: number(music)?,
                music_managed: flag(managed)?,
                music_recovery: unrecovery(recovery)?,
                history_held: flag(held)?,
                revision: number(revision)?,
            };
            let mut apart = Apart { account: state, devices: BTreeMap::new() };
            line = lines.next()?;
            while let Some(rest) = line.strip_prefix("device ") {
                let words: Vec<&str> = rest.split(' ').collect();
                let [id, "number", device_number, "last", last, "leaves", leaves, "permit", permit] = words.as_slice() else { return None };
                let id = number(id)?;
                if id == 0 || id >= store.next_device || !devices_seen.insert(id) {
                    return None;
                }
                let permit = if *permit == "-" {
                    None
                } else {
                    let (session, leave) = permit.split_once(':')?;
                    Some(Permit { id: number(session)?, leave: unleave(leave)? })
                };
                apart.devices.insert(id, ApartDevice { number: number(device_number)?, last_session: number(last)?, last_leave: number(leaves)?, permit });
                line = lines.next()?;
            }
            store.accounts.insert(n, Account { name: unhex(name)?, apart });
        }
        let words: Vec<&str> = line.split(' ').collect();
        store.pending = match words.as_slice() {
            ["pending", "none"] => None,
            ["pending", "change", accounts @ ..] => Some(Pending::Change { accounts: accounts.iter().map(|account| number(account)).collect::<Option<Vec<u64>>>()? }),
            ["pending", "restore", restore] => Some(Pending::Restore { restore: number(restore)? }),
            ["pending", "take-as-is", restore] => Some(Pending::TakeAsIs { restore: number(restore)? }),
            _ => return None,
        };
        if lines.next().is_some() {
            return None;
        }
        Some(store)
    }
}

/// What reading the store found.
#[derive(Debug)]
pub enum Reading {
    /// There is no store.
    Missing,
    Found(Store),
    /// There is a file, and it is not a store this program can read: it is left as it is.
    Unreadable(String),
}

pub fn place(state: &Path) -> PathBuf {
    state.join(FILE)
}

/// Reads the store in `state`.
pub fn read(state: &Path) -> Reading {
    let path = place(state);
    let file = match fs::File::open(&path) {
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Reading::Missing,
        Err(why) => return Reading::Unreadable(format!("it can't be opened ({})", why.kind())),
        Ok(file) => file,
    };
    let mut bytes = Vec::new();
    if let Err(why) = file.take(FILE_MAX_BYTES + 1).read_to_end(&mut bytes) {
        return Reading::Unreadable(format!("it can't be read ({})", why.kind()));
    }
    if bytes.len() as u64 > FILE_MAX_BYTES {
        return Reading::Unreadable("it is larger than a store can be".into());
    }
    match std::str::from_utf8(&bytes).ok().and_then(Store::from_text) {
        Some(store) => Reading::Found(store),
        None => Reading::Unreadable("it is damaged, or is not a store this program wrote".into()),
    }
}

/// Writes `store` into `state`, lasting on the disk before this returns. The points it
/// passes, in order: `store: staged` (its own file written and lasting), `store: renamed`
/// (in place), `store: lasting` (the folder too). A store larger than [`read`] reads is
/// refused before anything is written, and the last one stays (the review's finding 10).
pub fn write(state: &Path, store: &Store) -> io::Result<()> {
    write_within(state, store, FILE_MAX_BYTES)
}

/// [`write`], refused if the store would be larger than `limit` (at most [`FILE_MAX_BYTES`]).
pub fn write_within(state: &Path, store: &Store, limit: u64) -> io::Result<()> {
    crash::point("store: before")?;
    let text = store.to_text();
    if text.len() as u64 > limit.min(FILE_MAX_BYTES) {
        return Err(io::Error::new(io::ErrorKind::StorageFull, "the store kept apart would be larger than it may be"));
    }
    let staging = state.join(format!("{STAGING}{}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = (|| {
        let mut file = options.open(&staging)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        crash::point("store: staged")?;
        fs::rename(&staging, place(state))
    })();
    if let Err(why) = written {
        let _ = fs::remove_file(&staging);
        return Err(why);
    }
    crash::point("store: renamed")?;
    fs::File::open(state)?.sync_all()?;
    crash::point("store: lasting")
}

/// Removes what a cut write left behind (only files of the store's own staging name). The
/// caller holds the folder's lock, so no other write is under way.
pub fn tidy(state: &Path) {
    if let Ok(entries) = fs::read_dir(state) {
        for entry in entries.flatten() {
            if entry.file_name().to_str().is_some_and(|name| name.starts_with(STAGING)) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// Puts an unreadable store aside, under a name that says so, so that a new one can be
/// made and the old is kept for whoever wants to look at it. Where it went.
pub fn set_aside(state: &Path, now: u64) -> io::Result<PathBuf> {
    let aside = state.join(format!("{FILE}.set-aside-{now}"));
    fs::rename(place(state), &aside)?;
    fs::File::open(state)?.sync_all()?;
    Ok(aside)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let bytes = text.as_bytes();
    if bytes.len() != 2 * N {
        return None;
    }
    let mut out = [0u8; N];
    for (slot, pair) in out.iter_mut().zip(bytes.chunks_exact(2)) {
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        let [high, low] = pair else { return None };
        *slot = digit(*high)? * 16 + digit(*low)?;
    }
    Some(out)
}

fn field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    line.strip_prefix(name)?.strip_prefix(' ')
}

/// A number written as this program writes one: digits, no sign, no leading zero.
fn number(text: &str) -> Option<u64> {
    let plain = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()) && (text == "0" || !text.starts_with('0'));
    if plain { text.parse().ok() } else { None }
}

fn flag(text: &str) -> Option<bool> {
    match text {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}







fn status_word(status: Status) -> &'static str {
    match status {
        Status::On => "on",
        Status::Off => "off",
        Status::Deleted => "deleted",
    }
}

fn unstatus(text: &str) -> Option<Status> {
    match text {
        "on" => Some(Status::On),
        "off" => Some(Status::Off),
        "deleted" => Some(Status::Deleted),
        _ => None,
    }
}

fn recovery_word(recovery: Option<MusicRecovery>) -> &'static str {
    match recovery {
        None => "-",
        Some(MusicRecovery::RotateManaged) => "rotate",
        Some(MusicRecovery::EnterAgain) => "enter",
    }
}

fn unrecovery(text: &str) -> Option<Option<MusicRecovery>> {
    match text {
        "-" => Some(None),
        "rotate" => Some(Some(MusicRecovery::RotateManaged)),
        "enter" => Some(Some(MusicRecovery::EnterAgain)),
        _ => None,
    }
}

const ADMIN_KINDS: [(AdminKind, &str); 11] = [
    (AdminKind::CreateAccount, "create-account"),
    (AdminKind::ChangeAccountStatus, "account-status"),
    (AdminKind::DeleteAccount, "delete-account"),
    (AdminKind::ResetPassword, "reset-password"),
    (AdminKind::MakeAdmin, "make-admin"),
    (AdminKind::ForceOut, "force-out"),
    (AdminKind::ChangeMusicLogin, "music-login"),
    (AdminKind::SetMusicAddress, "music-address"),
    (AdminKind::SetDiagnosticsDefault, "diagnostics"),
    (AdminKind::SetRequests, "requests"),
    (AdminKind::SetLidarr, "lidarr"),
];

/// What leave is for, as one word (shared with the data's columns).
pub fn leave_for_word(purpose: LeaveFor) -> String {
    match purpose {
        LeaveFor::ChangePassword => "change-password".into(),
        LeaveFor::DeleteHistory => "delete-history".into(),
        LeaveFor::Admin(kind) => format!("admin-{}", ADMIN_KINDS.iter().find(|(k, _)| *k == kind).map_or("?", |(_, word)| word)),
    }
}

pub fn leave_for(word: &str) -> Option<LeaveFor> {
    match word {
        "change-password" => Some(LeaveFor::ChangePassword),
        "delete-history" => Some(LeaveFor::DeleteHistory),
        _ => {
            let kind = word.strip_prefix("admin-")?;
            ADMIN_KINDS.iter().find(|(_, w)| *w == kind).map(|(k, _)| LeaveFor::Admin(*k))
        }
    }
}

fn leave_text(leave: Option<Leave>) -> String {
    match leave {
        None => "-".into(),
        Some(leave) => {
            let (purpose, life, granted, serial) = leave.fields();
            format!("{},{life},{granted},{serial}", leave_for_word(purpose))
        }
    }
}

fn unleave(text: &str) -> Option<Option<Leave>> {
    if text == "-" {
        return Some(None);
    }
    let parts: Vec<&str> = text.split(',').collect();
    let [purpose, life, granted, serial] = parts.as_slice() else { return None };
    Some(Some(Leave::stored(leave_for(purpose)?, number(life)?, number(granted)?, number(serial)?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("nexus-server-apart-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn full() -> Store {
        let mut store = Store::new(BackendId::from_bytes([5; 16]), [9; 32], [8; 32], 1234);
        store.restore = 3;
        store.mark = 40;
        store.mark_at = 1200;
        store.set_up = true;
        store.data_made = true;
        store.next_account = 3;
        store.next_device = 8;
        let state = |number, status| AccountState {
            backend: store.backend,
            restore: 3,
            name: placeholder(number).unwrap(),
            life: 2,
            status,
            password_changes: 4,
            password_reset_required: true,
            code_available: true,
            admin: true,
            music_login: 6,
            music_managed: true,
            music_recovery: Some(MusicRecovery::EnterAgain),
            history_held: true,
            revision: 9,
        };
        let mut alice = Apart { account: state(1, Status::Off), devices: BTreeMap::new() };
        alice.devices.insert(7, ApartDevice { number: 4, last_session: 6, last_leave: 2, permit: Some(Permit { id: 6, leave: Some(Leave::stored(LeaveFor::Admin(AdminKind::ResetPassword), 2, 99, 2)) }) });
        alice.devices.insert(3, ApartDevice { number: 0, last_session: 0, last_leave: 0, permit: None });
        store.accounts.insert(1, Account { name: name_fingerprint("alice@example.org"), apart: alice });
        store.accounts.insert(2, Account { name: name_fingerprint("bob@example.org"), apart: Apart { account: state(2, Status::Deleted), devices: BTreeMap::new() } });
        store.pending = Some(Pending::Change { accounts: vec![1, 2] });
        store
    }

    #[test]
    fn what_is_written_is_read_back_the_same() {
        let state = folder("round");
        assert!(matches!(read(&state), Reading::Missing));
        for pending in [None, Some(Pending::Change { accounts: vec![] }), Some(Pending::Change { accounts: vec![1, 2] }), Some(Pending::Restore { restore: 4 }), Some(Pending::TakeAsIs { restore: 4 })] {
            let store = Store { pending: pending.clone(), ..full() };
            write(&state, &store).unwrap();
            let Reading::Found(back) = read(&state) else { panic!("not read back") };
            assert!(back == store, "{pending:?}");
        }
        for kind in ADMIN_KINDS {
            assert_eq!(leave_for(&leave_for_word(LeaveFor::Admin(kind.0))), Some(LeaveFor::Admin(kind.0)));
        }
        assert!(fs::read_dir(&state).unwrap().all(|entry| entry.unwrap().file_name() == FILE));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(place(&state)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn any_change_to_a_byte_makes_it_unreadable_and_it_is_left() {
        let state = folder("damaged");
        write(&state, &full()).unwrap();
        let good = fs::read(place(&state)).unwrap();
        for at in (0..good.len()).step_by(3) {
            let mut bad = good.clone();
            bad[at] ^= 0x01;
            fs::write(place(&state), &bad).unwrap();
            assert!(matches!(read(&state), Reading::Unreadable(_)), "byte {at}");
            assert_eq!(fs::read(place(&state)).unwrap(), bad);
        }
        for cut in (0..good.len()).step_by(7) {
            fs::write(place(&state), &good[..cut]).unwrap();
            assert!(matches!(read(&state), Reading::Unreadable(_)), "cut at {cut}");
        }
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn a_store_whose_lines_do_not_fit_together_is_unreadable() {
        let cases: Vec<(&str, Store)> = vec![
            ("an account past the next number", Store { next_account: 2, ..full() }),
            ("a device past the next number", Store { next_device: 7, ..full() }),
        ];
        let state = folder("misfit");
        for (name, store) in cases {
            fs::write(place(&state), store.to_text()).unwrap();
            assert!(matches!(read(&state), Reading::Unreadable(_)), "{name}");
        }
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn a_store_larger_than_its_reader_reads_is_never_written_and_room_is_kept_for_revocations() {
        let state = folder("bound");
        write(&state, &full()).unwrap();
        let mut big = full();
        let template = big.accounts[&2].clone();
        let mut number = 10;
        while (big.to_text().len() as u64) < GROWTH_MAX_BYTES + 1024 {
            for _ in 0..10_000 {
                big.accounts.insert(number, template.clone());
                number += 1;
            }
        }
        big.next_account = number;
        // Past the room left for growth: an account more is refused, the store as it was kept.
        let before = fs::read(place(&state)).unwrap();
        assert_eq!(write_within(&state, &big, GROWTH_MAX_BYTES).unwrap_err().kind(), io::ErrorKind::StorageFull);
        assert_eq!(fs::read(place(&state)).unwrap(), before);
        // What takes leave away, at the same size, is still written and read back.
        write(&state, &big).unwrap();
        assert!(matches!(read(&state), Reading::Found(_)));
        // Past what can be read: never published.
        while (big.to_text().len() as u64) <= FILE_MAX_BYTES {
            for _ in 0..10_000 {
                big.accounts.insert(number, template.clone());
                number += 1;
            }
        }
        big.next_account = number;
        let before = fs::read(place(&state)).unwrap();
        assert!(write(&state, &big).is_err());
        assert_eq!(fs::read(place(&state)).unwrap(), before);
        assert!(matches!(read(&state), Reading::Found(_)));
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn its_debug_says_nothing_of_the_keys_or_a_name() {
        let shown = format!("{:?}", full());
        assert!(!shown.contains("0909") && !shown.contains("0808") && !shown.contains(&hex(&name_fingerprint("alice@example.org"))), "{shown}");
    }

    #[test]
    fn a_staging_file_left_by_a_cut_write_is_tidied_and_never_read() {
        let state = folder("tidy");
        write(&state, &full()).unwrap();
        fs::write(state.join(format!("{STAGING}12345")), "half").unwrap();
        assert!(matches!(read(&state), Reading::Found(_)));
        tidy(&state);
        assert!(fs::read_dir(&state).unwrap().all(|entry| entry.unwrap().file_name() == FILE));
        fs::remove_dir_all(&state).unwrap();
    }
}
