//! What's kept for Lidarr in the credential store: its API key. **It is kept with the origin
//! it belongs to, in one entry**: the scheme, host and port of the address the key was typed
//! for. A key is used only where the origin kept with it is the origin in use, so no order of
//! failed or interrupted writes (the settings file and the credential store are written
//! apart) can pair one address with another's key.
//!
//! A key kept by 0.40.10 has no origin. It is bound once, at the first start, to the address
//! then configured, which is what 0.40.10 would have used it for (and, until that one write is
//! made, used only there: changing the address binds or forgets it first, see `change`).

/// The credential store, as this needs it: a seam, so that the tests can make any write fail.
pub trait Vault: Send + Sync {
    fn read(&self, name: &str) -> Option<String>;
    /// Writes `value` under `name` (empty: forgets it), and says whether that was done.
    fn write(&self, name: &str, value: &str) -> bool;
}

/// The app's own: the Windows credential store (elsewhere a file only this user can read).
pub struct Store(pub crate::config::Paths);

impl Vault for Store {
    fn read(&self, name: &str) -> Option<String> {
        crate::config::load_secret(&self.0, name)
    }

    fn write(&self, name: &str, value: &str) -> bool {
        crate::config::save_secret(&self.0, name, value)
    }
}

const KEY: &str = "lidarr-key";
/// The first line of an entry that's kept with its origin; the second is the origin.
const BOUND: &str = "nexus-bound-1";

/// Said when the key couldn't be kept: the change isn't made. (The app's sentence for a
/// change that couldn't be saved.)
pub const NOT_SAVED: &str = "That couldn't be saved, so it hasn't been changed.";

#[derive(Debug, PartialEq)]
enum Kept {
    Nothing,
    /// Kept by 0.40.10, with no origin.
    Old(String),
    Bound { origin: String, secret: String },
}

fn read(vault: &dyn Vault, name: &str) -> Kept {
    match vault.read(name).filter(|value| !value.is_empty()) {
        None => Kept::Nothing,
        Some(value) => match value.split_once('\n') {
            Some((BOUND, rest)) => match rest.split_once('\n') {
                Some((origin, secret)) if !secret.is_empty() => Kept::Bound { origin: origin.to_string(), secret: secret.to_string() },
                _ => Kept::Nothing,
            },
            _ => Kept::Old(value),
        },
    }
}

/// The origin an address leads to, written out: "https://lidarr.example.org:443".
pub fn origin_of(address: &str) -> Option<String> {
    nexus_core::lidarr::origin(address).ok().map(|origin| format!("{}://{}:{}", origin.scheme, origin.host, origin.port))
}

fn bound(origin: &str, secret: &str) -> String {
    format!("{BOUND}\n{origin}\n{secret}")
}

// ---- the key ---------------------------------------------------------------------------------

/// The key for Lidarr at `address`, if one is kept for its origin.
pub fn key(vault: &dyn Vault, address: &str) -> Option<String> {
    let origin = origin_of(address)?;
    match read(vault, KEY) {
        Kept::Bound { origin: kept, secret } if kept == origin => Some(secret),
        // Not bound yet (its one write failed at the start): it belongs to the address in use,
        // as `change` binds or forgets it before that is ever changed.
        Kept::Old(secret) => Some(secret),
        _ => None,
    }
}

/// At the start: binds a key kept by 0.40.10 to the address configured now. Says whether
/// there was one, and it couldn't be bound.
pub fn bind_old_key(vault: &dyn Vault, address: &str) -> bool {
    match (read(vault, KEY), origin_of(address)) {
        (Kept::Old(secret), Some(origin)) => !vault.write(KEY, &bound(&origin, &secret)),
        _ => false,
    }
}

/// How changing Lidarr's address went.
#[derive(Debug, PartialEq)]
pub enum Changed {
    /// Made. `forgotten`: a key kept for the old address was let go of, the new address being
    /// another origin and no key typed with it.
    Made { forgotten: bool },
    /// Not made, and why: the key couldn't be kept, or the address couldn't be saved.
    Not(&'static str),
}

/// Changes Lidarr's address from `old` to `new`, with `typed` the key typed with it (empty:
/// none). The credential store is written first, then the address (`save_address`, which
/// says whether it's on the disk), so that whatever happens between them leaves no key
/// where it doesn't belong: a key typed is kept bound to the new address; with none typed and
/// the origin changed, the old key is bound to the old address (if it wasn't yet) and then
/// let go of. A write that fails stops the change, unless what's kept is bound already.
pub fn change(vault: &dyn Vault, old: &str, new: &str, typed: &str, save_address: &mut dyn FnMut() -> bool) -> Changed {
    let mut forgotten = false;
    if !typed.is_empty() {
        let Some(origin) = origin_of(new) else { return Changed::Not(NOT_SAVED) };
        if !vault.write(KEY, &bound(&origin, typed)) {
            return Changed::Not(NOT_SAVED);
        }
    } else if origin_of(old) != origin_of(new) {
        match read(vault, KEY) {
            Kept::Nothing => {}
            Kept::Bound { .. } => {
                forgotten = true;
                // (Bound to the old one: if it stays, it's never used for the new one.)
                let _ = vault.write(KEY, "");
            }
            Kept::Old(secret) => {
                forgotten = true;
                let tied = origin_of(old).is_some_and(|origin| vault.write(KEY, &bound(&origin, &secret)));
                if !vault.write(KEY, "") && !tied {
                    return Changed::Not(NOT_SAVED);
                }
            }
        }
    }
    if !save_address() {
        return Changed::Not(NOT_SAVED);
    }
    Changed::Made { forgotten }
}

#[cfg(test)]
pub mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    /// A credential store in memory. `fail` names the writes (counted from 1) that fail;
    /// after `stop` writes nothing more is written, as if the app had been closed there.
    #[derive(Default)]
    pub struct Memory {
        pub kept: Mutex<HashMap<String, String>>,
        pub writes: Mutex<usize>,
        pub fail: Vec<usize>,
        pub stop: Option<usize>,
    }

    impl Vault for Memory {
        fn read(&self, name: &str) -> Option<String> {
            self.kept.lock().unwrap().get(name).cloned()
        }

        fn write(&self, name: &str, value: &str) -> bool {
            let mut writes = self.writes.lock().unwrap();
            *writes += 1;
            if self.fail.contains(&*writes) {
                return false;
            }
            if self.stop.is_some_and(|stop| *writes > stop) {
                return true;
            }
            let mut kept = self.kept.lock().unwrap();
            if value.is_empty() {
                kept.remove(name);
            } else {
                kept.insert(name.to_string(), value.to_string());
            }
            true
        }
    }

    const A: &str = "https://lidarr.example.org";
    const B: &str = "http://lidarr.example.org";
    const C: &str = "https://other.example.org:8686";

    fn with(entries: &[(&str, &str)]) -> Memory {
        Memory { kept: Mutex::new(entries.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect()), ..Default::default() }
    }

    #[test]
    fn a_key_is_used_only_at_the_origin_it_was_kept_with() {
        let vault = with(&[(KEY, &bound("https://lidarr.example.org:443", "key-a"))]);
        assert_eq!(key(&vault, A).as_deref(), Some("key-a"));
        assert_eq!(key(&vault, "https://LIDARR.example.org:443/lidarr").as_deref(), Some("key-a"));
        assert_eq!(key(&vault, B), None);
        assert_eq!(key(&vault, C), None);
    }

    #[test]
    fn a_key_kept_by_0_40_10_is_bound_once_to_the_address_then_configured() {
        let vault = with(&[(KEY, "key-old")]);
        assert_eq!(key(&vault, A).as_deref(), Some("key-old"));
        assert!(!bind_old_key(&vault, A));
        assert_eq!(key(&vault, A).as_deref(), Some("key-old"));
        assert_eq!(key(&vault, C), None);
        // Once: bound, it isn't bound again to whatever is configured next.
        assert!(!bind_old_key(&vault, C));
        assert_eq!((key(&vault, A).as_deref(), key(&vault, C)), (Some("key-old"), None));
        // Its one write failing is said.
        let failing = Memory { fail: vec![1], ..with(&[(KEY, "key-old")]) };
        assert!(bind_old_key(&failing, A));
    }

    /// What's on the disk after a change, read as the next start reads it: the address, and
    /// the key that would be sent to it.
    fn next_start(vault: &Memory, address: &str) -> (String, Option<String>) {
        bind_old_key(vault, address);
        (address.to_string(), key(vault, address))
    }

    #[test]
    fn no_failed_or_interrupted_write_pairs_an_address_with_another_s_key() {
        // From each way a key can be kept, to an address of another origin with a key typed and
        // with none: every write failing in turn, and the app closed after each write.
        for kept in [bound("https://lidarr.example.org:443", "key-a"), "key-a".to_string()] {
            for typed in ["key-c", ""] {
                for (fail, stop) in [(vec![], None), (vec![1], None), (vec![2], None), (vec![1, 2], None), (vec![], Some(0)), (vec![], Some(1)), (vec![], Some(2))] {
                    let vault = Memory { fail: fail.clone(), stop, ..with(&[(KEY, &kept)]) };
                    let mut on_disk = A.to_string();
                    let saves = std::cell::Cell::new(0);
                    // The settings file: written after every write before it, or not at all if
                    // the app was closed before; and failing for the "fail" case 3.
                    let changed = change(&vault, A, C, typed, &mut || {
                        saves.set(saves.get() + 1);
                        if fail.contains(&3) || stop.is_some_and(|stop| *vault.writes.lock().unwrap() > stop) {
                            return false;
                        }
                        on_disk = C.to_string();
                        true
                    });
                    let (address, sent) = next_start(&vault, &on_disk);
                    let case = format!("kept {kept:?}, typed {typed:?}, failing {fail:?}, closed after {stop:?}: {changed:?}");
                    match sent.as_deref() {
                        None => {}
                        Some("key-a") => assert_eq!(address, A, "{case}"),
                        Some("key-c") => assert_eq!(address, C, "{case}"),
                        Some(other) => panic!("{other}: {case}"),
                    }
                    if matches!(changed, Changed::Made { .. }) && stop.is_none() {
                        assert_eq!(address, C, "{case}");
                    }
                }
            }
            // And the settings file failing: the change isn't made, and says so.
            let vault = with(&[(KEY, &kept)]);
            assert_eq!(change(&vault, A, C, "key-c", &mut || false), Changed::Not(NOT_SAVED));
            assert_eq!(next_start(&vault, A).1, None);
        }
    }

    #[test]
    fn a_key_that_cant_be_kept_stops_the_change_and_is_said() {
        let vault = Memory { fail: vec![1], ..with(&[(KEY, &bound("https://lidarr.example.org:443", "key-a"))]) };
        let mut saved = false;
        assert_eq!(change(&vault, A, C, "key-c", &mut || { saved = true; true }), Changed::Not(NOT_SAVED));
        assert!(!saved);
        assert_eq!(key(&vault, A).as_deref(), Some("key-a"));
        // An old key that can be neither bound nor let go of stops a change of origin.
        let vault = Memory { fail: vec![1, 2], ..with(&[(KEY, "key-a")]) };
        assert_eq!(change(&vault, A, C, "", &mut || { saved = true; true }), Changed::Not(NOT_SAVED));
        // The same origin keeps the key, and changes nothing in the store.
        let vault = with(&[(KEY, &bound("https://lidarr.example.org:443", "key-a"))]);
        assert_eq!(change(&vault, A, "https://lidarr.example.org:443/lidarr", "", &mut || true), Changed::Made { forgotten: false });
        assert_eq!(*vault.writes.lock().unwrap(), 0);
        assert_eq!(change(&vault, A, C, "", &mut || true), Changed::Made { forgotten: true });
    }
}
