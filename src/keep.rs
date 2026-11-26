//! What's kept for Lidarr in the credential store: its API key, and the token of the sign-in
//! in front of it. **Each is kept with the origin it belongs to, in one entry**: the scheme,
//! host and port of the address the key was typed for, or of the sign-in service the token was
//! made at. A secret is used only where the origin kept with it is the origin in use, so no
//! order of failed or interrupted writes (the settings file and the credential store are
//! written apart) can pair one place with another's secret.
//!
//! A secret kept by 0.40.10 has no origin. It is bound once, at the first start, to the place
//! then configured, which is what 0.40.10 would have used it for: the key to Lidarr's address
//! (and, until that one write is made, used only there: changing the address binds or forgets
//! it first, see `change`); the token to the saved sign-in service if that one is trusted
//! (`Found::trusted`), and otherwise left unused.
//!
//! The token has one more rule: **one sign-in owns it at a time.** A renewal writes only if
//! the entry still holds the token it renewed, and a sign-out's late tidying-up clears only the
//! token it handed back, so that an old sign-in can neither write over nor clear a new one.

use std::sync::{Arc, Mutex};

/// The credential store, as this needs it: a seam, so that the tests can make any write fail.
pub trait Vault: Send + Sync {
    /// What's kept under `name`: there (`Ok(Some)`), not there (`Ok(None)`), or couldn't be
    /// read (`Err`), which is not the same as not there.
    fn read(&self, name: &str) -> Result<Option<String>, ()>;
    /// Writes `value` under `name` (empty: forgets it), and says whether that was done.
    fn write(&self, name: &str, value: &str) -> bool;
}

/// The app's own: the Windows credential store (elsewhere a file only this user can read).
pub struct Store(pub crate::config::Paths);

impl Vault for Store {
    fn read(&self, name: &str) -> Result<Option<String>, ()> {
        crate::config::read_kept(&self.0, name)
    }

    fn write(&self, name: &str, value: &str) -> bool {
        crate::config::write_kept(&self.0, name, value)
    }
}

const KEY: &str = "lidarr-key";
const TOKEN: &str = "lidarr-token";
/// The first line of an entry that's kept with its origin; the second is the origin.
const BOUND: &str = "nexus-bound-1";

/// Said when the key couldn't be kept: the change isn't made. (The app's sentence for a
/// change that couldn't be saved.)
pub const NOT_SAVED: &str = "That couldn't be saved, so it hasn't been changed.";

#[derive(Debug, PartialEq)]
enum Kept {
    Nothing,
    /// Something may be kept, but the store couldn't say what: nothing may be taken for gone.
    Unreadable,
    /// Kept by 0.40.10, with no origin.
    Old(String),
    Bound { origin: String, secret: String },
}

fn read(vault: &dyn Vault, name: &str) -> Kept {
    let Ok(value) = vault.read(name) else { return Kept::Unreadable };
    match value.filter(|value| !value.is_empty()) {
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

/// The origin an address leads to, written out: "https://lidarr.example.org:443". Read as a
/// saved address is (`origin_saved`): what's typed has been held to the rule before it's kept.
pub fn origin_of(address: &str) -> Option<String> {
    nexus_core::lidarr::origin_saved(address).ok().map(|origin| format!("{}://{}:{}", origin.scheme, origin.host, origin.port))
}

fn bound(origin: &str, secret: &str) -> String {
    format!("{BOUND}\n{origin}\n{secret}")
}

// ---- the key ---------------------------------------------------------------------------------

/// The key for Lidarr at `address`, if one is kept for its origin. `old_for` is the address
/// configured at this start if a key kept by 0.40.10 couldn't be bound to it then
/// (`bind_old_key`): that key, still unbound, is used for that address and no other.
pub fn key(vault: &dyn Vault, address: &str, old_for: Option<&str>) -> Option<String> {
    let origin = origin_of(address)?;
    match read(vault, KEY) {
        Kept::Bound { origin: kept, secret } if kept == origin => Some(secret),
        Kept::Old(secret) if old_for.and_then(origin_of).as_ref() == Some(&origin) => Some(secret),
        _ => None,
    }
}

/// How binding a key kept by 0.40.10 went, at the start.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Bind {
    /// Bound, or there was nothing to bind.
    Done,
    /// There's one, and it couldn't be bound (its write failed), or the store couldn't be read:
    /// it's used, unbound, only for the address configured now, and tried again next start.
    NotYet,
}

/// At the start: binds a key kept by 0.40.10 to the address configured now.
pub fn bind_old_key(vault: &dyn Vault, address: &str) -> Bind {
    match (read(vault, KEY), origin_of(address)) {
        (Kept::Old(secret), Some(origin)) if !vault.write(KEY, &bound(&origin, &secret)) => Bind::NotYet,
        (Kept::Unreadable, _) => Bind::NotYet,
        _ => Bind::Done,
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
/// `let_go` is what goes with the old address (the sign-in in front of it): done once the
/// change is going through, just before the address is saved, and not at all if it isn't.
pub fn change(vault: &dyn Vault, old: &str, new: &str, typed: &str, let_go: &mut dyn FnMut(), save_address: &mut dyn FnMut() -> bool) -> Changed {
    let mut forgotten = false;
    if !typed.is_empty() {
        let Some(origin) = origin_of(new) else { return Changed::Not(NOT_SAVED) };
        if !vault.write(KEY, &bound(&origin, typed)) {
            return Changed::Not(NOT_SAVED);
        }
    } else if origin_of(old) != origin_of(new) {
        match read(vault, KEY) {
            Kept::Nothing => {}
            // What's kept can't be read: it may be a key for the old address, which a change
            // now would leave behind for the new one. Not made.
            Kept::Unreadable => return Changed::Not(NOT_SAVED),
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
    let_go();
    if !save_address() {
        return Changed::Not(NOT_SAVED);
    }
    Changed::Made { forgotten }
}

// ---- the sign-in's token -------------------------------------------------------------------------

/// Every read and write of the token, together, so that "only if it still holds" is so.
static SLOT: Mutex<()> = Mutex::new(());

/// The token kept for the sign-in service at `service`, if it was made there. `old_for` is
/// the service saved at this start if a token kept by 0.40.10 couldn't be bound to it then
/// (`bind_old_token`): that token, still unbound, is used for that service and no other.
pub fn token(vault: &dyn Vault, service: &str, old_for: Option<&str>) -> Option<String> {
    let _slot = SLOT.lock().unwrap_or_else(|stuck| stuck.into_inner());
    match read(vault, TOKEN) {
        Kept::Old(secret) if old_for.and_then(origin_of).is_some() && old_for.and_then(origin_of) == origin_of(service) => Some(secret),
        _ => held(vault, service),
    }
}

fn held(vault: &dyn Vault, service: &str) -> Option<String> {
    let origin = origin_of(service)?;
    match read(vault, TOKEN) {
        Kept::Bound { origin: kept, secret } if kept == origin => Some(secret),
        _ => None,
    }
}

/// Whether what's kept is still `token`: bound to `service`, or kept by 0.40.10 unbound.
fn holds(vault: &dyn Vault, service: &str, token: &str) -> bool {
    match read(vault, TOKEN) {
        Kept::Bound { origin, secret } => Some(origin) == origin_of(service) && secret == token,
        Kept::Old(secret) => secret == token,
        _ => false,
    }
}

/// Whether any token is kept at all, used or not (one kept by 0.40.10 for a service that
/// isn't trusted, say: there to be cleared).
pub fn any_kept(vault: &dyn Vault) -> bool {
    read(vault, TOKEN) != Kept::Nothing
}

/// At the start, and then only: binds a token kept by 0.40.10 to the sign-in service saved
/// with it, if that one is `trusted`; otherwise it's left unused (and cleared at a sign-out,
/// sent nowhere). If binding can't be done (its write failed, or the store couldn't be read)
/// it's `Bind::NotYet`: the token is used, unbound, for that service only (`token`'s
/// `old_for`), and binding is tried again at the next start. It's never bound to a service
/// found or saved later: a change of service takes it away (`take_token`).
/// `spent`: a token kept by 0.40.10 that couldn't be taken away when it was to go
/// (`take_token_telling`): it isn't bound, or used.
pub fn bind_old_token(vault: &dyn Vault, service: &str, trusted: bool, spent: bool) -> Bind {
    let _slot = SLOT.lock().unwrap_or_else(|stuck| stuck.into_inner());
    if spent {
        return Bind::Done;
    }
    match (read(vault, TOKEN), origin_of(service), trusted) {
        (Kept::Old(secret), Some(origin), true) if !vault.write(TOKEN, &bound(&origin, &secret)) => Bind::NotYet,
        (Kept::Unreadable, _, true) => Bind::NotYet,
        _ => Bind::Done,
    }
}

/// Keeps the token of a sign-in just made at `service`: from now on it's the one kept.
pub fn keep_token(vault: &dyn Vault, service: &str, token: &str) -> bool {
    let _slot = SLOT.lock().unwrap_or_else(|stuck| stuck.into_inner());
    origin_of(service).is_some_and(|origin| vault.write(TOKEN, &bound(&origin, token)))
}

/// Takes the kept token away, whatever it is, and gives it if it was the one for `service`:
/// a sign-out, or a sign-in being let go of.
#[cfg(test)]
pub fn take_token(vault: &dyn Vault, service: &str) -> Option<String> {
    take_token_telling(vault, service).0
}

/// `take_token`, also saying whether what was kept was a token from 0.40.10, still unbound,
/// that couldn't be taken away: it's spent, and must be neither used nor bound from now on,
/// across a restart too (see `bind_old_token`'s `spent`).
pub fn take_token_telling(vault: &dyn Vault, service: &str) -> (Option<String>, bool) {
    let _slot = SLOT.lock().unwrap_or_else(|stuck| stuck.into_inner());
    let token = held(vault, service);
    let kept = read(vault, TOKEN);
    let taken = kept == Kept::Nothing || vault.write(TOKEN, "");
    (token, !taken && matches!(kept, Kept::Old(_)))
}

/// After a sign-out has handed `token` back: clears the entry only if it still holds that
/// very token (its first clearing failed, say). Anything else there is a later sign-in's.
pub fn clear_token_if(vault: &dyn Vault, token: &str) {
    let _slot = SLOT.lock().unwrap_or_else(|stuck| stuck.into_inner());
    if matches!(read(vault, TOKEN), Kept::Bound { secret, .. } | Kept::Old(secret) if secret == token) {
        let _ = vault.write(TOKEN, "");
    }
}

/// What a sign-in session at `service`, begun with `token`, does with each fresh token the
/// service gives it (every renewal), or "" when the service stopped honouring it: written only
/// if what's kept is still the token it renewed (bound, or kept by 0.40.10 and not yet bound).
/// One that's been signed out of, or replaced, writes and clears nothing. A write that fails is
/// said (`failed`).
pub fn renewals(vault: Arc<dyn Vault>, service: String, token: String, failed: impl Fn() + Send + Sync + 'static) -> impl Fn(&str) + Send + Sync + 'static {
    let last = Mutex::new(token);
    move |fresh: &str| {
        let mut last = last.lock().unwrap_or_else(|stuck| stuck.into_inner());
        let written = {
            let _slot = SLOT.lock().unwrap_or_else(|stuck| stuck.into_inner());
            if !holds(&*vault, &service, &last) {
                return;
            }
            match origin_of(&service) {
                Some(origin) if !fresh.is_empty() => vault.write(TOKEN, &bound(&origin, fresh)),
                _ => vault.write(TOKEN, ""),
            }
        };
        if written {
            *last = fresh.to_string();
        } else if !fresh.is_empty() {
            failed();
        }
    }
}

#[cfg(test)]
pub mod tests {
    use std::collections::HashMap;

    use super::*;

    /// A credential store in memory. `fail` names the writes (counted from 1) that fail;
    /// after `stop` writes nothing more is written, as if the app had been closed there.
    #[derive(Default)]
    pub struct Memory {
        pub kept: Mutex<HashMap<String, String>>,
        /// While set, nothing can be read.
        pub unreadable: std::sync::atomic::AtomicBool,
        pub writes: Mutex<usize>,
        pub fail: Vec<usize>,
        pub stop: Option<usize>,
    }

    impl Vault for Memory {
        fn read(&self, name: &str) -> Result<Option<String>, ()> {
            if self.unreadable.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(());
            }
            Ok(self.kept.lock().unwrap().get(name).cloned())
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
        assert_eq!(key(&vault, A, None).as_deref(), Some("key-a"));
        assert_eq!(key(&vault, "https://LIDARR.example.org:443/lidarr", None).as_deref(), Some("key-a"));
        assert_eq!(key(&vault, B, None), None);
        assert_eq!(key(&vault, C, None), None);
    }

    #[test]
    fn a_key_kept_by_0_40_10_is_bound_once_to_the_address_then_configured() {
        let vault = with(&[(KEY, "key-old")]);
        assert_eq!(bind_old_key(&vault, A), Bind::Done);
        assert_eq!(key(&vault, A, None).as_deref(), Some("key-old"));
        assert_eq!(key(&vault, C, None), None);
        // Once: bound, it isn't bound again to whatever is configured next.
        assert_eq!(bind_old_key(&vault, C), Bind::Done);
        assert_eq!((key(&vault, A, None).as_deref(), key(&vault, C, None)), (Some("key-old"), None));
        // Its one write failing: used, unbound, for the address configured at this start only.
        let failing = Memory { fail: vec![1], ..with(&[(KEY, "key-old")]) };
        assert_eq!(bind_old_key(&failing, A), Bind::NotYet);
        assert_eq!((key(&failing, A, Some(A)).as_deref(), key(&failing, C, Some(A))), (Some("key-old"), None));
    }

    /// What's on the disk after a change, read as the next start reads it: the address, and
    /// the key that would be sent to it.
    fn next_start(vault: &Memory, address: &str) -> (String, Option<String>) {
        let old_for = (bind_old_key(vault, address) == Bind::NotYet).then_some(address);
        (address.to_string(), key(vault, address, old_for))
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
                    let changed = change(&vault, A, C, typed, &mut || {}, &mut || {
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
            assert_eq!(change(&vault, A, C, "key-c", &mut || {}, &mut || false), Changed::Not(NOT_SAVED));
            assert_eq!(next_start(&vault, A).1, None);
        }
    }

    #[test]
    fn a_key_that_cant_be_kept_stops_the_change_and_is_said() {
        let vault = Memory { fail: vec![1], ..with(&[(KEY, &bound("https://lidarr.example.org:443", "key-a"))]) };
        let mut saved = false;
        assert_eq!(change(&vault, A, C, "key-c", &mut || {}, &mut || { saved = true; true }), Changed::Not(NOT_SAVED));
        assert!(!saved);
        assert_eq!(key(&vault, A, None).as_deref(), Some("key-a"));
        // An old key that can be neither bound nor let go of stops a change of origin.
        let vault = Memory { fail: vec![1, 2], ..with(&[(KEY, "key-a")]) };
        assert_eq!(change(&vault, A, C, "", &mut || {}, &mut || { saved = true; true }), Changed::Not(NOT_SAVED));
        // The same origin keeps the key, and changes nothing in the store.
        let vault = with(&[(KEY, &bound("https://lidarr.example.org:443", "key-a"))]);
        assert_eq!(change(&vault, A, "https://lidarr.example.org:443/lidarr", "", &mut || {}, &mut || true), Changed::Made { forgotten: false });
        assert_eq!(*vault.writes.lock().unwrap(), 0);
        assert_eq!(change(&vault, A, C, "", &mut || {}, &mut || true), Changed::Made { forgotten: true });
    }

    #[test]
    fn a_change_that_is_refused_lets_go_of_nothing() {
        // The kept key can't be read: the change is refused, and the sign-in in front of the
        // old address is kept.
        let vault = with(&[(KEY, "key-a")]);
        vault.unreadable.store(true, std::sync::atomic::Ordering::SeqCst);
        let mut let_go = false;
        assert_eq!(change(&vault, A, C, "", &mut || let_go = true, &mut || true), Changed::Not(NOT_SAVED));
        assert!(!let_go);
        // A key that can't be kept: the same.
        let vault = Memory { fail: vec![1], ..with(&[]) };
        assert_eq!(change(&vault, A, C, "key-c", &mut || let_go = true, &mut || true), Changed::Not(NOT_SAVED));
        assert!(!let_go);
        // Going through: let go of before the address is saved.
        let vault = with(&[(KEY, &bound("https://lidarr.example.org:443", "key-a"))]);
        let order = std::cell::RefCell::new(Vec::new());
        assert!(matches!(change(&vault, A, C, "", &mut || order.borrow_mut().push("let go"), &mut || { order.borrow_mut().push("saved"); true }), Changed::Made { .. }));
        assert_eq!(*order.borrow(), ["let go", "saved"]);
    }

    #[test]
    fn a_key_that_couldnt_be_read_is_not_taken_for_none() {
        use std::sync::atomic::Ordering;
        // A 0.40.10 key, and the first read at the start fails: nothing is bound.
        let vault = with(&[(KEY, "key-a")]);
        vault.unreadable.store(true, Ordering::SeqCst);
        assert_eq!(bind_old_key(&vault, A), Bind::NotYet);
        // The address is changed, no key typed, while the store still can't be read: not made,
        // and said as a change that couldn't be saved.
        let mut on_disk = A.to_string();
        assert_eq!(change(&vault, A, C, "", &mut || {}, &mut || { on_disk = C.to_string(); true }), Changed::Not(NOT_SAVED));
        assert_eq!(on_disk, A);
        // Readable again: the next start binds it to the address it was kept for, and C never
        // has it.
        vault.unreadable.store(false, Ordering::SeqCst);
        assert_eq!(next_start(&vault, &on_disk), (A.to_string(), Some("key-a".to_string())));
        assert_eq!(key(&vault, C, None), None);
        // A key typed with the new address needs nothing read: it replaces what's kept.
        vault.unreadable.store(true, Ordering::SeqCst);
        assert!(matches!(change(&vault, A, C, "key-c", &mut || {}, &mut || true), Changed::Made { .. }));
        vault.unreadable.store(false, Ordering::SeqCst);
        assert_eq!(next_start(&vault, C).1.as_deref(), Some("key-c"));
    }

    const S: &str = "https://auth.example.org";

    #[test]
    fn a_token_is_used_only_at_the_service_it_was_made_at() {
        let vault = with(&[]);
        assert!(keep_token(&vault, S, "t1"));
        assert_eq!(token(&vault, S, None).as_deref(), Some("t1"));
        assert_eq!(token(&vault, "https://elsewhere.example.org", None), None);
        assert_eq!(token(&vault, "http://auth.example.org", None), None);
        // One kept by 0.40.10 is bound to a trusted service, and is otherwise left unused.
        let vault = with(&[(TOKEN, "old")]);
        assert_eq!(token(&vault, S, None), None);
        bind_old_token(&vault, S, false, false);
        assert_eq!(token(&vault, S, None), None);
        assert!(any_kept(&vault));
        assert_eq!(vault.read(TOKEN), Ok(Some("old".to_string())));
        bind_old_token(&vault, S, true, false);
        assert_eq!(token(&vault, S, None).as_deref(), Some("old"));
    }

    #[test]
    fn a_0_40_10_sign_in_whose_binding_fails_locks_nobody_out() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        // Its binding write fails: used for the service saved with it, at this start, and no
        // other; and tried again at the next start.
        let vault: Arc<Memory> = Arc::new(Memory { fail: vec![1], ..with(&[(TOKEN, "old")]) });
        assert_eq!(bind_old_token(&*vault, S, true, false), Bind::NotYet);
        assert_eq!(token(&*vault, S, Some(S)).as_deref(), Some("old"));
        assert_eq!(token(&*vault, "https://found.later.example.org", Some(S)), None);
        // Its first renewal is written, bound to the service.
        let said = Arc::new(AtomicUsize::new(0));
        let told = said.clone();
        let renew = renewals(vault.clone(), S.into(), "old".into(), move || {
            told.fetch_add(1, Ordering::SeqCst);
        });
        renew("fresh");
        assert_eq!(token(&*vault, S, None).as_deref(), Some("fresh"));
        // A renewal whose write fails is said.
        let failing: Arc<Memory> = Arc::new(Memory { fail: vec![2], ..with(&[]) });
        keep_token(&*failing, S, "t1");
        let told = said.clone();
        let renew = renewals(failing.clone(), S.into(), "t1".into(), move || {
            told.fetch_add(1, Ordering::SeqCst);
        });
        renew("t2");
        assert_eq!(said.load(Ordering::SeqCst), 1);
        // The store couldn't be read at the start: not bound now, and never to a service found
        // later; the next start binds it to the service it was kept with.
        let vault = with(&[(TOKEN, "old")]);
        vault.unreadable.store(true, Ordering::SeqCst);
        assert_eq!(bind_old_token(&vault, S, true, false), Bind::NotYet);
        vault.unreadable.store(false, Ordering::SeqCst);
        assert_eq!(token(&vault, "https://found.later.example.org", Some(S)), None);
        assert_eq!(bind_old_token(&vault, S, true, false), Bind::Done);
        assert_eq!(token(&vault, S, None).as_deref(), Some("old"));
    }

    #[test]
    fn what_is_kept_is_written_as_bytes_and_a_0_40_10_entry_read_as_it_was() {
        // A refresh token as long as 0.40.10 could keep (1,270 characters of UTF-16 in 2,560
        // bytes, less its own room), bound: still within 2,560 bytes.
        let long = "r".repeat(1_270);
        let written = crate::config::kept_bytes(&bound("https://auth.example.org:443", &long));
        assert!(written.len() <= 2_560, "{} bytes", written.len());
        assert_eq!(crate::config::kept_text(&written).as_deref(), Some(bound("https://auth.example.org:443", &long).as_str()));
        // What 0.40.10 wrote on Windows: UTF-16 text, read back as it was.
        let old: Vec<u8> = "old token".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(crate::config::kept_text_windows(&old).as_deref(), Some("old token"));
        let bound_bytes = crate::config::kept_bytes(&bound("https://auth.example.org:443", "t"));
        assert_eq!(crate::config::kept_text_windows(&bound_bytes).as_deref(), Some(bound("https://auth.example.org:443", "t").as_str()));
    }

    #[test]
    fn a_renewal_writes_only_over_the_token_it_renewed() {
        let vault: Arc<Memory> = Arc::new(with(&[]));
        keep_token(&*vault, S, "t1");
        let renew = renewals(vault.clone(), S.into(), "t1".into(), || {});
        renew("t2");
        assert_eq!(token(&*vault, S, None).as_deref(), Some("t2"));
        // Signed out, and in again: the old session's late renewal writes nothing.
        assert_eq!(take_token(&*vault, S).as_deref(), Some("t2"));
        keep_token(&*vault, S, "new");
        renew("t3");
        assert_eq!(token(&*vault, S, None).as_deref(), Some("new"));
        // Nor does its being turned away clear the new one.
        renew("");
        assert_eq!(token(&*vault, S, None).as_deref(), Some("new"));
        // The new one's own renewals and ending are written.
        let renew = renewals(vault.clone(), S.into(), "new".into(), || {});
        renew("new2");
        assert_eq!(token(&*vault, S, None).as_deref(), Some("new2"));
        renew("");
        assert_eq!(token(&*vault, S, None), None);
    }

    #[test]
    fn a_sign_out_clears_only_the_sign_in_it_was_signing_out() {
        let vault = with(&[]);
        keep_token(&vault, S, "t1");
        let handed_back = take_token(&vault, S).unwrap();
        // A new sign-in meanwhile, while the old token's hand-back was slow.
        keep_token(&vault, S, "new");
        clear_token_if(&vault, &handed_back);
        assert_eq!(token(&vault, S, None).as_deref(), Some("new"));
        // Its own, still there (the first clearing failed): cleared.
        let vault = Memory { fail: vec![2], ..with(&[]) };
        keep_token(&vault, S, "t1");
        let handed_back = take_token(&vault, S).unwrap();
        assert_eq!(token(&vault, S, None).as_deref(), Some("t1"));
        clear_token_if(&vault, &handed_back);
        assert_eq!(token(&vault, S, None), None);
    }

    #[test]
    fn an_unbound_token_that_couldnt_be_taken_away_is_spent_across_a_restart() {
        // A 0.40.10 token, unbound; the service changes (or the person signs out), and it
        // can't be deleted.
        let vault = Memory { fail: vec![1], ..with(&[(TOKEN, "old")]) };
        let (handed, spent) = take_token_telling(&vault, S);
        assert_eq!((handed, spent), (None, true));
        assert_eq!(vault.read(TOKEN), Ok(Some("old".to_string())));
        // The next start, spent written down: it's neither bound to the service now saved, nor
        // used.
        let elsewhere = "https://auth.other.example.org";
        assert_eq!(bind_old_token(&vault, elsewhere, true, true), Bind::Done);
        assert_eq!(token(&vault, elsewhere, None), None);
        assert_eq!(vault.read(TOKEN), Ok(Some("old".to_string())));
        // A bound token taken away, or one taken away as asked, is no such thing.
        let vault = with(&[(TOKEN, "old")]);
        assert_eq!(take_token_telling(&vault, S), (None, false));
        let vault = Memory { fail: vec![2], ..with(&[]) };
        keep_token(&vault, S, "t1");
        assert_eq!(take_token_telling(&vault, S), (Some("t1".to_string()), false));
    }

    #[test]
    fn a_token_left_behind_by_a_change_of_service_is_never_sent_to_the_new_one() {
        // Moved from service A to B: B saved, and A's token not taken away (its write failed,
        // or the app was closed first). Under B it isn't there.
        let vault = Memory { fail: vec![2], ..with(&[]) };
        keep_token(&vault, S, "token-a");
        take_token(&vault, S);
        assert_eq!(token(&vault, "https://auth.other.example.org", None), None);
        assert_eq!(token(&vault, S, None).as_deref(), Some("token-a"));
        // Nor does a renewal of A's write it as B's.
        let vault: Arc<Memory> = Arc::new(with(&[]));
        keep_token(&*vault, S, "token-a");
        let renew = renewals(vault.clone(), "https://auth.other.example.org".into(), "token-a".into(), || {});
        renew("token-a2");
        assert_eq!(token(&*vault, "https://auth.other.example.org", None), None);
    }
}
