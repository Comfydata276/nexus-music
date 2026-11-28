//! The store kept apart (design 8, 9.2): one small file, `NEXUS_STATE/kept`, beside the
//! backend's id. **What must not go backwards when the data does**: the restore number, the
//! key answers are stamped with, the latest time the backend has acted at, each account's
//! life and whether it is held, and each device's numbers and the sessions it may use.
//!
//! - **Written whole, every time**: to a file of its own, made lasting, then renamed over
//!   the last, and the folder made lasting. A cut leaves the old store or the new, never a
//!   mixture.
//! - **Read strictly**: every line in its place, and a check sum over all of them. Anything
//!   else is a store that cannot be read (11.2), and the program stops.
//! - **Holds no name and no session**: an account is its number, with a fingerprint of its
//!   name so that its life outlives its rows (11.1 step 4); a session is the number C3 gives
//!   it. The key is secret: the file is the owner's alone to read (0600), and the key is never
//!   written anywhere else.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use nexus_core::nexus::compare::{Pending, StoreAccount, StoreDevice, StoreView};
use nexus_core::nexus::kept::{Family, Permit, SECRET_BYTES, StampKey};
use nexus_core::nexus::proto::BackendId;
use sha2::{Digest, Sha256};

use crate::crash;

/// The file's name inside `NEXUS_STATE`.
pub const FILE: &str = "kept";
/// Where a write is made before it is renamed into place.
const STAGING: &str = "kept.new-";
/// The first line: what this file is, and the form it is written in.
const HEAD: &str = "nexus-music-server store kept apart 1";
/// The most the file may be. A device takes about 100 bytes, an account 120.
const FILE_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// One account: its fingerprint of a name, and the rest as `compare` reads it.
#[derive(Clone, PartialEq, Eq)]
pub struct Account {
    pub name: [u8; 32],
    pub kept: StoreAccount,
}

/// The whole store.
#[derive(Clone, PartialEq, Eq)]
pub struct Store {
    pub backend: BackendId,
    pub restore: u64,
    /// The latest time, in milliseconds on the backend's clock, the backend has acted at
    /// (C3's `Clock::high_water`).
    pub high_water: u64,
    key: [u8; SECRET_BYTES],
    /// Whether the data has been made for this store. Until then a store with no data is a
    /// first start cut short, not data lost.
    pub data_made: bool,
    pub next_account: u64,
    pub next_device: u64,
    pub accounts: BTreeMap<u64, Account>,
    pub devices: BTreeMap<u64, StoreDevice>,
    pub pending: Option<Pending>,
}

impl fmt::Debug for Store {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never the key, and no name's fingerprint.
        f.debug_struct("Store")
            .field("restore", &self.restore)
            .field("high_water", &self.high_water)
            .field("data_made", &self.data_made)
            .field("accounts", &self.accounts.len())
            .field("devices", &self.devices.len())
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

/// The fingerprint by which the store knows an account's name: SHA-256 of a fixed prefix and
/// the name's exact bytes. (The design does not say how the store keeps a life for a name
/// whose rows the data no longer has; this is what lets an account deleted and connected
/// again go on from its life, and holds nothing else of it.)
pub fn name_fingerprint(name: &str) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"Nexus Music account name v1\0");
    hash.update(name.as_bytes());
    hash.finalize().into()
}

impl Store {
    /// A new store, for a backend with no data yet.
    pub fn new(backend: BackendId, key: [u8; SECRET_BYTES], high_water: u64) -> Self {
        Store {
            backend,
            restore: 0,
            high_water,
            key,
            data_made: false,
            next_account: 1,
            next_device: 1,
            accounts: BTreeMap::new(),
            devices: BTreeMap::new(),
            pending: None,
        }
    }

    /// The key answers are stamped with.
    pub fn stamp_key(&self) -> StampKey {
        StampKey::new(self.key)
    }

    /// What `compare` reads.
    pub fn view(&self) -> StoreView {
        StoreView {
            restore: self.restore,
            accounts: self.accounts.iter().map(|(&number, account)| (number, account.kept)).collect(),
            devices: self.devices.clone(),
            pending: self.pending,
        }
    }

    /// The account with this name's fingerprint, if the store has ever had it.
    pub fn account_named(&self, name: &str) -> Option<u64> {
        let print = name_fingerprint(name);
        self.accounts.iter().find(|(_, account)| account.name == print).map(|(&number, _)| number)
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
        line(format!("key {}", hex(&self.key)));
        line(format!("data-made {}", u8::from(self.data_made)));
        line(format!("next-account {}", self.next_account));
        line(format!("next-device {}", self.next_device));
        for (number, account) in &self.accounts {
            line(format!(
                "account {number} name {} life {} held {} present {}",
                hex(&account.name),
                account.kept.life,
                optional(account.kept.held),
                u8::from(account.kept.present)
            ));
        }
        for (id, device) in &self.devices {
            line(format!(
                "device {id} account {} restore {} life {} number {} last {} working {} waiting {} gone {}",
                device.account,
                device.restore,
                device.life,
                device.number,
                device.last_session,
                permit(device.working),
                permit(device.waiting),
                u8::from(device.gone)
            ));
        }
        line(match self.pending {
            None => "pending none".to_string(),
            Some(Pending::MakeAccount { account }) => format!("pending make-account {account}"),
            Some(Pending::MakeDevice { device }) => format!("pending make-device {device}"),
            Some(Pending::Update { device, account, family }) => format!("pending update {device} {account} {}", family_word(family)),
            Some(Pending::Remove { device }) => format!("pending remove {device}"),
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
        let mut lines = body.strip_suffix('\n')?.split('\n');
        let mut next = || lines.next();
        if next()? != HEAD {
            return None;
        }
        let backend = BackendId::from_hex(field(next()?, "backend")?)?;
        let restore = number(field(next()?, "restore")?)?;
        let high_water = number(field(next()?, "high-water")?)?;
        let key = unhex(field(next()?, "key")?)?;
        let data_made = flag(field(next()?, "data-made")?)?;
        let next_account = number(field(next()?, "next-account")?)?;
        let next_device = number(field(next()?, "next-device")?)?;
        let mut store = Store { backend, restore, high_water, key, data_made, next_account, next_device, ..Store::new(backend, key, 0) };
        let mut line = next()?;
        while let Some(rest) = line.strip_prefix("account ") {
            let words: Vec<&str> = rest.split(' ').collect();
            let [n, "name", name, "life", life, "held", held, "present", present] = words.as_slice() else { return None };
            let n = number(n)?;
            let account = Account { name: unhex(name)?, kept: StoreAccount { life: number(life)?, held: unoptional(held)?, present: flag(present)? } };
            if n == 0 || n >= store.next_account || store.accounts.insert(n, account).is_some() {
                return None;
            }
            line = next()?;
        }
        while let Some(rest) = line.strip_prefix("device ") {
            let words: Vec<&str> = rest.split(' ').collect();
            let [id, "account", account, "restore", restore, "life", life, "number", n, "last", last, "working", working, "waiting", waiting, "gone", gone] =
                words.as_slice()
            else {
                return None;
            };
            let id = number(id)?;
            let device = StoreDevice {
                account: number(account)?,
                restore: number(restore)?,
                life: number(life)?,
                number: number(n)?,
                last_session: number(last)?,
                working: unpermit(working)?,
                waiting: unpermit(waiting)?,
                gone: flag(gone)?,
            };
            if id == 0 || id >= store.next_device || !store.accounts.contains_key(&device.account) || store.devices.insert(id, device).is_some() {
                return None;
            }
            line = next()?;
        }
        let words: Vec<&str> = line.split(' ').collect();
        store.pending = match words.as_slice() {
            ["pending", "none"] => None,
            ["pending", "make-account", account] => Some(Pending::MakeAccount { account: number(account)? }),
            ["pending", "make-device", device] => Some(Pending::MakeDevice { device: number(device)? }),
            ["pending", "update", device, account, family] => {
                Some(Pending::Update { device: number(device)?, account: number(account)?, family: unfamily(family)? })
            }
            ["pending", "remove", device] => Some(Pending::Remove { device: number(device)? }),
            ["pending", "restore", restore] => Some(Pending::Restore { restore: number(restore)? }),
            ["pending", "take-as-is", restore] => Some(Pending::TakeAsIs { restore: number(restore)? }),
            _ => return None,
        };
        if next().is_some() {
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
/// (in place), `store: lasting` (the folder too).
pub fn write(state: &Path, store: &Store) -> io::Result<()> {
    crash::point("store: before")?;
    let staging = state.join(format!("{STAGING}{}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = (|| {
        let mut file = options.open(&staging)?;
        file.write_all(store.to_text().as_bytes())?;
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

fn optional(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), |value| value.to_string())
}

fn unoptional(text: &str) -> Option<Option<u64>> {
    if text == "-" { Some(None) } else { number(text).map(Some) }
}

fn permit(permit: Option<Permit>) -> String {
    match permit {
        None => "-".to_string(),
        Some(permit) => format!("{}:{}", permit.id, optional(permit.delete_proven)),
    }
}

fn unpermit(text: &str) -> Option<Option<Permit>> {
    if text == "-" {
        return Some(None);
    }
    let (id, delete_proven) = text.split_once(':')?;
    Some(Some(Permit { id: number(id)?, delete_proven: unoptional(delete_proven)? }))
}

fn family_word(family: Option<Family>) -> &'static str {
    match family {
        None => "none",
        Some(Family::Device) => "device",
        Some(Family::Account) => "account",
        Some(Family::Backend) => "backend",
    }
}

fn unfamily(text: &str) -> Option<Option<Family>> {
    match text {
        "none" => Some(None),
        "device" => Some(Some(Family::Device)),
        "account" => Some(Some(Family::Account)),
        "backend" => Some(Some(Family::Backend)),
        _ => None,
    }
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
        let mut store = Store::new(BackendId::from_bytes([5; 16]), [9; 32], 1234);
        store.restore = 3;
        store.data_made = true;
        store.next_account = 3;
        store.next_device = 8;
        store.accounts.insert(1, Account { name: name_fingerprint("alice"), kept: StoreAccount { life: 2, held: Some(2), present: true } });
        store.accounts.insert(2, Account { name: name_fingerprint("bob"), kept: StoreAccount { life: 0, held: None, present: false } });
        store.devices.insert(
            7,
            StoreDevice {
                account: 1,
                restore: 3,
                life: 2,
                number: 4,
                last_session: 6,
                working: Some(Permit { id: 5, delete_proven: Some(99) }),
                waiting: Some(Permit { id: 6, delete_proven: None }),
                gone: false,
            },
        );
        store.devices.insert(3, StoreDevice { account: 2, restore: 1, life: 0, number: 0, last_session: 0, working: None, waiting: None, gone: true });
        store.pending = Some(Pending::Update { device: 7, account: 1, family: Some(Family::Account) });
        store
    }

    #[test]
    fn what_is_written_is_read_back_the_same() {
        let state = folder("round");
        assert!(matches!(read(&state), Reading::Missing));
        for pending in [
            None,
            Some(Pending::MakeAccount { account: 1 }),
            Some(Pending::MakeDevice { device: 7 }),
            Some(Pending::Update { device: 7, account: 1, family: None }),
            Some(Pending::Update { device: 7, account: 1, family: Some(Family::Device) }),
            Some(Pending::Update { device: 7, account: 1, family: Some(Family::Backend) }),
            Some(Pending::Remove { device: 3 }),
            Some(Pending::Restore { restore: 4 }),
            Some(Pending::TakeAsIs { restore: 4 }),
        ] {
            let store = Store { pending, ..full() };
            write(&state, &store).unwrap();
            let Reading::Found(back) = read(&state) else { panic!("not read back") };
            assert!(back == store, "{pending:?}");
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
        for at in 0..good.len() {
            let mut bad = good.clone();
            bad[at] ^= 0x01;
            fs::write(place(&state), &bad).unwrap();
            assert!(matches!(read(&state), Reading::Unreadable(_)), "byte {at}");
            assert_eq!(fs::read(place(&state)).unwrap(), bad);
        }
        for cut in 0..good.len() {
            fs::write(place(&state), &good[..cut]).unwrap();
            assert!(matches!(read(&state), Reading::Unreadable(_)), "cut at {cut}");
        }
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn a_store_whose_lines_do_not_fit_together_is_unreadable() {
        // Each written with a right sum, so only the reading of its lines refuses it.
        let cases: Vec<(&str, Store)> = vec![
            ("an account past the next number", Store { next_account: 2, ..full() }),
            ("a device past the next number", Store { next_device: 7, ..full() }),
            ("a device of no account", {
                let mut store = full();
                store.accounts.remove(&2);
                store
            }),
            ("account number 0", {
                let mut store = full();
                let account = store.accounts.remove(&2).unwrap();
                store.accounts.insert(0, account);
                store
            }),
        ];
        let state = folder("misfit");
        for (name, store) in cases {
            fs::write(place(&state), store.to_text()).unwrap();
            assert!(matches!(read(&state), Reading::Unreadable(_)), "{name}");
        }
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn its_debug_says_nothing_of_the_key_or_a_name() {
        let shown = format!("{:?}", full());
        assert!(!shown.contains("0909") && !shown.contains(&hex(&name_fingerprint("alice"))), "{shown}");
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
