//! Where the sign-in's secrets are kept (design 11): **Windows' Credential Manager**, and on
//! the other platforms (used only for development) files only this user can read, in the
//! same bytes, under the same limit, so that a test on either reads what Windows writes.
//!
//! **One format, written and read the same way everywhere.** Every record starts with
//! [`MARK`] and is UTF-8 after it. (0.40.10 and before wrote the store's passwords as UTF-16;
//! what `keep` binds for Lidarr starts with a mark of its own. Neither is read here, and
//! nothing here reads anything else as text.)
//!
//! **A record of any size up to [`RECORD_MAX`], put in place in one step.** One credential
//! holds at most [`BLOB_MAX`] bytes (Windows' `CRED_MAX_CREDENTIAL_BLOB_SIZE`). A record that
//! fits is that one credential. A larger one is written in parts first, each under a name
//! of its own that carries a number new to this write, and then its head, naming that
//! number, the parts' count, the length and a SHA-256 of the whole: **the head is the one
//! write that puts it in place**. A write cut short before the head leaves the record that
//! was there before, whole; parts no head names are swept away at the next start
//! ([`sweep`]). A head whose parts do not add up is not taken for anything: it is
//! [`Trouble::Garbled`].
//!
//! **What could not be read is never taken for what is not there** ([`Trouble`]): a store
//! that could not be asked is [`Trouble::Unreachable`]; bytes that are there and do not read
//! are [`Trouble::Garbled`]. Absent is `Ok(None)` alone.

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::config::Paths;

/// The most one credential holds.
pub const BLOB_MAX: usize = 2560;
/// The most a record may be: far more than the largest valid one (a `Saved` with a
/// 2,048-byte music address and a 2,048-byte notice is under 6 KiB).
pub const RECORD_MAX: usize = 64 * 1024;
/// What every record of this module's starts with.
pub const MARK: &[u8] = b"nexus-kept-1\n";
const WHOLE: &[u8] = b"W\n";
const PARTS: &[u8] = b"P\n";
/// What separates a record's name from its parts' numbers (never in a record's own name).
const PART: char = '~';

/// Why a record could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    /// The store could not be asked (or answered with an error): not the same as absent.
    Unreachable,
    /// Bytes are kept under the name and do not read as a record of this module's.
    Garbled,
}

/// What is kept under `name`: there (its text), not there, or why it could not be read.
pub fn read(paths: &Paths, name: &str) -> Result<Option<Zeroizing<String>>, Trouble> {
    let Some(head) = raw::get(paths, name).map_err(|()| Trouble::Unreachable)? else { return Ok(None) };
    let head = Zeroizing::new(head);
    let rest = head.strip_prefix(MARK).ok_or(Trouble::Garbled)?;
    if let Some(whole) = rest.strip_prefix(WHOLE) {
        return text(whole.to_vec()).map(Some);
    }
    let parts = Parts::read(rest.strip_prefix(PARTS).ok_or(Trouble::Garbled)?)?;
    let mut bytes = Zeroizing::new(Vec::with_capacity(parts.len));
    for index in 0..parts.count {
        match raw::get(paths, &part_name(name, &parts.generation, index)) {
            Ok(Some(part)) => bytes.extend_from_slice(&Zeroizing::new(part)),
            Ok(None) => return Err(Trouble::Garbled),
            Err(()) => return Err(Trouble::Unreachable),
        }
        if bytes.len() > parts.len {
            return Err(Trouble::Garbled);
        }
    }
    if bytes.len() != parts.len || hex(&Sha256::digest(&*bytes)) != parts.sha {
        return Err(Trouble::Garbled);
    }
    text(std::mem::take(&mut *bytes)).map(Some)
}

/// Puts `value` in place under `name`, whole, in one step (see the module's notes); an empty
/// value forgets it. Says whether that was done: if not, what was there before is there
/// still, whole.
pub fn write(paths: &Paths, name: &str, value: &str) -> bool {
    if value.is_empty() {
        return forget(paths, name);
    }
    let bytes = value.as_bytes();
    if bytes.len() > RECORD_MAX || name.contains(PART) {
        return false;
    }
    let before = generation_of(paths, name);
    if MARK.len() + WHOLE.len() + bytes.len() <= BLOB_MAX {
        let mut head = Zeroizing::new(Vec::with_capacity(BLOB_MAX));
        head.extend_from_slice(MARK);
        head.extend_from_slice(WHOLE);
        head.extend_from_slice(bytes);
        if !raw::put(paths, name, &head) {
            return false;
        }
    } else {
        let generation = new_generation(before.as_deref());
        let chunks: Vec<&[u8]> = bytes.chunks(BLOB_MAX).collect();
        for (index, chunk) in chunks.iter().enumerate() {
            if !raw::put(paths, &part_name(name, &generation, index), chunk) {
                remove_parts(paths, name, &generation, index + 1);
                return false;
            }
        }
        let parts = Parts { generation: generation.clone(), count: chunks.len(), len: bytes.len(), sha: hex(&Sha256::digest(bytes)) };
        let mut head = MARK.to_vec();
        head.extend_from_slice(PARTS);
        head.extend_from_slice(parts.line().as_bytes());
        if !raw::put(paths, name, &head) {
            remove_parts(paths, name, &generation, chunks.len());
            return false;
        }
    }
    // The record before is replaced: its parts are no one's now.
    sweep_name(paths, name);
    true
}

/// Forgets what is kept under `name` (forgetting what is not there is done). Says whether
/// it was done.
pub fn forget(paths: &Paths, name: &str) -> bool {
    if !raw::delete(paths, name) {
        return false;
    }
    sweep_name(paths, name);
    true
}

/// At a start: the parts that no head names (a write cut short, a delete that failed) are
/// removed, for each of `names`. A head that could not be read keeps every part of its name.
pub fn sweep(paths: &Paths, names: &[&str]) {
    for name in names {
        sweep_name(paths, name);
    }
}

fn sweep_name(paths: &Paths, name: &str) {
    let Ok(all) = raw::names(paths) else { return };
    let current = match raw::get(paths, name) {
        Ok(None) => None,
        Ok(Some(head)) => match Zeroizing::new(head).strip_prefix(MARK).and_then(|rest| rest.strip_prefix(PARTS)).map(Parts::read) {
            Some(Ok(parts)) => Some(parts.generation),
            Some(Err(_)) => return,
            None => None,
        },
        Err(()) => return,
    };
    let prefix = format!("{name}{PART}");
    for other in all {
        let Some(rest) = other.strip_prefix(&prefix) else { continue };
        let generation = rest.split(PART).next().unwrap_or_default();
        if current.as_deref() != Some(generation) {
            let _ = raw::delete(paths, &other);
        }
    }
}

fn generation_of(paths: &Paths, name: &str) -> Option<String> {
    let head = Zeroizing::new(raw::get(paths, name).ok()??);
    Parts::read(head.strip_prefix(MARK)?.strip_prefix(PARTS)?).ok().map(|parts| parts.generation)
}

fn remove_parts(paths: &Paths, name: &str, generation: &str, count: usize) {
    for index in 0..count {
        let _ = raw::delete(paths, &part_name(name, generation, index));
    }
}

fn part_name(name: &str, generation: &str, index: usize) -> String {
    format!("{name}{PART}{generation}{PART}{index}")
}

/// A number for a write's parts that is not the one before's.
fn new_generation(before: Option<&str>) -> String {
    loop {
        let mut bytes = [0u8; 8];
        if getrandom::fill(&mut bytes).is_err() {
            bytes = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|time| time.as_nanos()).unwrap_or_default() as u64).to_le_bytes();
        }
        let generation = hex(&bytes);
        if before != Some(generation.as_str()) {
            return generation;
        }
    }
}

fn text(bytes: Vec<u8>) -> Result<Zeroizing<String>, Trouble> {
    String::from_utf8(bytes).map(Zeroizing::new).map_err(|error| {
        drop(Zeroizing::new(error.into_bytes()));
        Trouble::Garbled
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A head that names its parts.
struct Parts {
    generation: String,
    count: usize,
    len: usize,
    sha: String,
}

impl Parts {
    fn line(&self) -> String {
        format!("{}\n{}\n{}\n{}", self.generation, self.count, self.len, self.sha)
    }

    fn read(bytes: &[u8]) -> Result<Parts, Trouble> {
        let text = std::str::from_utf8(bytes).map_err(|_| Trouble::Garbled)?;
        let mut lines = text.split('\n');
        let (Some(generation), Some(count), Some(len), Some(sha), None) = (lines.next(), lines.next(), lines.next(), lines.next(), lines.next()) else {
            return Err(Trouble::Garbled);
        };
        let plain = |text: &str, size: usize| text.len() == size && text.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase());
        let (Ok(count), Ok(len)) = (count.parse::<usize>(), len.parse::<usize>()) else { return Err(Trouble::Garbled) };
        if !plain(generation, 16) || !plain(sha, 64) || len > RECORD_MAX || count != len.div_ceil(BLOB_MAX) || count == 0 {
            return Err(Trouble::Garbled);
        }
        Ok(Parts { generation: generation.to_string(), count, len, sha: sha.to_string() })
    }
}

/// What 0.40.13 and every version before it kept: the music server's password, as the
/// credential store's password, named `user@server` under the app's service. **Each such
/// entry is deleted at every start, by its name alone, whatever music server and user it is
/// for** (earlier accounts and servers too, and one a failed delete left last time); its
/// password is never read. What this module and `keep` keep (named `secret:…`) and every
/// other program's credentials are left alone. Says how many were deleted, and how many
/// could not be.
pub fn forget_legacy_passwords(paths: &Paths) -> (usize, usize) {
    raw::forget_legacy(paths)
}

#[cfg(windows)]
mod raw {
    //! Windows' Credential Manager: generic credentials named `secret:<name>` under the
    //! app's service (the same names `keep` and 0.40 used), written and read as bytes.
    use crate::config::Paths;

    const SERVICE: &str = "Nexus Music";
    /// What the credential store library names a generic credential: `<user>.<service>`.
    const SUFFIX: &str = ".Nexus Music";
    const OURS: &str = "secret:";

    fn entry(name: &str) -> Result<keyring::Entry, ()> {
        keyring::Entry::new(SERVICE, &format!("{OURS}{name}")).map_err(drop)
    }

    pub fn get(_paths: &Paths, name: &str) -> Result<Option<Vec<u8>>, ()> {
        match entry(name)?.get_secret() {
            Ok(bytes) => Ok(Some(bytes)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(()),
        }
    }

    pub fn put(_paths: &Paths, name: &str, bytes: &[u8]) -> bool {
        bytes.len() <= super::BLOB_MAX && entry(name).is_ok_and(|entry| entry.set_secret(bytes).is_ok())
    }

    pub fn delete(_paths: &Paths, name: &str) -> bool {
        entry(name).is_ok_and(|entry| matches!(entry.delete_credential(), Ok(()) | Err(keyring::Error::NoEntry)))
    }

    /// The names of every generic credential under the app's service, by the store's own
    /// listing: what is in them is not looked at.
    fn targets() -> Result<Vec<String>, ()> {
        use windows_sys::Win32::Foundation::{ERROR_NOT_FOUND, GetLastError};
        use windows_sys::Win32::Security::Credentials::{CRED_TYPE_GENERIC, CREDENTIALW, CredEnumerateW, CredFree};
        let mut count: u32 = 0;
        let mut list: *mut *mut CREDENTIALW = std::ptr::null_mut();
        // SAFETY: the out-pointers are valid; what the store hands back is freed below with
        // CredFree, and read only while held.
        if unsafe { CredEnumerateW(std::ptr::null(), 0, &mut count, &mut list) } == 0 {
            // SAFETY: plain call.
            return if unsafe { GetLastError() } == ERROR_NOT_FOUND { Ok(Vec::new()) } else { Err(()) };
        }
        let mut names = Vec::new();
        // SAFETY: `list` holds `count` valid pointers until CredFree.
        for &credential in unsafe { std::slice::from_raw_parts(list, count as usize) } {
            // SAFETY: each pointer is a valid CREDENTIALW; only its type and name are read.
            let credential = unsafe { &*credential };
            if credential.Type != CRED_TYPE_GENERIC || credential.TargetName.is_null() {
                continue;
            }
            // SAFETY: TargetName is a NUL-terminated wide string.
            let wide = unsafe {
                let mut len = 0;
                while *credential.TargetName.add(len) != 0 {
                    len += 1;
                }
                std::slice::from_raw_parts(credential.TargetName, len)
            };
            if let Ok(name) = String::from_utf16(wide)
                && name.ends_with(SUFFIX)
            {
                names.push(name);
            }
        }
        // SAFETY: what CredEnumerateW gave, once.
        unsafe { CredFree(list as *const std::ffi::c_void) };
        Ok(names)
    }

    pub fn names(_paths: &Paths) -> Result<Vec<String>, ()> {
        Ok(targets()?.into_iter().filter_map(|target| target.strip_suffix(SUFFIX)?.strip_prefix(OURS).map(str::to_string)).collect())
    }

    pub fn forget_legacy(_paths: &Paths) -> (usize, usize) {
        let Ok(targets) = targets() else { return (0, 1) };
        let (mut gone, mut left) = (0, 0);
        for target in targets {
            let Some(user) = target.strip_suffix(SUFFIX) else { continue };
            // 0.40 and before: `user@server`. Everything named `secret:` is this version's.
            if user.starts_with(OURS) || !user.contains('@') {
                continue;
            }
            match keyring::Entry::new(SERVICE, user).map(|entry| entry.delete_credential()) {
                Ok(Ok(()) | Err(keyring::Error::NoEntry)) => gone += 1,
                _ => left += 1,
            }
        }
        (gone, left)
    }
}

#[cfg(not(windows))]
mod raw {
    //! Elsewhere (development only): a file per credential, `secret-<name>` beside the
    //! settings, only this user's, renamed into place; held to Windows' limit.
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use crate::config::{self, Paths};

    const PREFIX: &str = "secret-";

    fn file(paths: &Paths, name: &str) -> PathBuf {
        paths.secret_file(&format!("{PREFIX}{name}"))
    }

    pub fn get(paths: &Paths, name: &str) -> Result<Option<Vec<u8>>, ()> {
        match fs::read(file(paths, name)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(()),
        }
    }

    pub fn put(paths: &Paths, name: &str, bytes: &[u8]) -> bool {
        if bytes.len() > super::BLOB_MAX || bytes.is_empty() || test_fails(name) {
            return false;
        }
        let file = file(paths, name);
        let temp = config::temp_for(&file);
        let written = fs::write(&temp, bytes).is_ok() && fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).is_ok() && fs::rename(&temp, &file).is_ok();
        if !written {
            let _ = fs::remove_file(&temp);
        }
        written
    }

    pub fn delete(paths: &Paths, name: &str) -> bool {
        if test_fails(name) {
            return false;
        }
        match fs::remove_file(file(paths, name)) {
            Ok(()) => true,
            Err(error) => error.kind() == std::io::ErrorKind::NotFound,
        }
    }

    pub fn names(paths: &Paths) -> Result<Vec<String>, ()> {
        let dir = file(paths, "x");
        let dir = dir.parent().ok_or(())?;
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(()),
        };
        Ok(entries.filter_map(|entry| entry.ok()?.file_name().into_string().ok()?.strip_prefix(PREFIX).map(str::to_string)).filter(|name| !name.contains(".tmp")).collect())
    }

    pub fn forget_legacy(paths: &Paths) -> (usize, usize) {
        match fs::remove_file(paths.secret_file("credential")) {
            Ok(()) => (1, 0),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (0, 0),
            Err(_) => (0, 1),
        }
    }

    /// For the tests and walks: writes and deletes of names starting with this fail, as a
    /// full or locked store would (`NEXUS_TEST_VAULT_FAIL=nexus-note`).
    fn test_fails(name: &str) -> bool {
        crate::nexus::test_var("NEXUS_TEST_VAULT_FAIL").is_some_and(|prefix| !prefix.is_empty() && name.starts_with(prefix.as_str()))
            || tests_fail(name)
    }

    #[cfg(test)]
    thread_local! {
        pub static FAIL: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    }

    #[cfg(test)]
    fn tests_fail(name: &str) -> bool {
        FAIL.with(|fail| fail.borrow().as_deref().is_some_and(|prefix| name.starts_with(prefix)))
    }

    #[cfg(not(test))]
    fn tests_fail(_: &str) -> bool {
        false
    }
}

#[cfg(test)]
pub fn fail_writes(prefix: Option<&str>) {
    #[cfg(not(windows))]
    raw::FAIL.with(|fail| *fail.borrow_mut() = prefix.map(str::to_string));
    let _ = prefix;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> (tempdir::Dir, Paths) {
        let dir = tempdir::Dir::new();
        let paths = Paths::in_dir(dir.path());
        (dir, paths)
    }

    mod tempdir {
        pub struct Dir(std::path::PathBuf);
        impl Dir {
            pub fn new() -> Dir {
                static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let path = std::env::temp_dir().join(format!("nexus-vault-{}-{}", std::process::id(), COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst)));
                std::fs::create_dir_all(&path).unwrap();
                Dir(path)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    /// What is written is read back as it was, small or large, in the bytes Windows keeps:
    /// the mark, then UTF-8. Nothing kept is over one credential's limit.
    #[test]
    fn a_record_of_any_valid_size_reads_back_as_written_in_windows_bytes() {
        let (_dir, paths) = paths();
        for size in [1, 100, BLOB_MAX - MARK.len() - WHOLE.len(), BLOB_MAX - MARK.len() - WHOLE.len() + 1, 6000, 3 * BLOB_MAX, RECORD_MAX] {
            // Two bytes to a character, so that a part's end falls inside one.
            let value = format!("{}{}", "é".repeat(size / 2), "x".repeat(size % 2));
            assert_eq!(value.len(), size);
            assert!(write(&paths, "nexus-sign-in", &value), "{size}");
            assert_eq!(read(&paths, "nexus-sign-in").unwrap().as_deref().map(String::as_str), Some(value.as_str()), "{size}");
            let head = raw::get(&paths, "nexus-sign-in").unwrap().unwrap();
            assert!(head.starts_with(MARK) && head.len() <= BLOB_MAX, "{size}");
            for name in raw::names(&paths).unwrap() {
                assert!(raw::get(&paths, &name).unwrap().unwrap().len() <= BLOB_MAX);
            }
        }
        // Each write leaves only its own parts.
        let parts = raw::names(&paths).unwrap().into_iter().filter(|name| name.contains(PART)).count();
        assert_eq!(parts, RECORD_MAX.div_ceil(BLOB_MAX));
        assert!(!write(&paths, "nexus-sign-in", &"x".repeat(RECORD_MAX + 1)));
        assert!(forget(&paths, "nexus-sign-in"));
        assert_eq!(read(&paths, "nexus-sign-in"), Ok(None));
        assert!(raw::names(&paths).unwrap().is_empty());
    }

    /// What 0.40.10 wrote (UTF-16) and what `keep` binds are not read as records; bytes with
    /// no mark are garbled, never absent; a head whose parts do not add up is garbled.
    #[test]
    fn what_does_not_read_is_garbled_never_absent() {
        let (_dir, paths) = paths();
        let utf16: Vec<u8> = "{\"a\":1}".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert!(raw::put(&paths, "nexus-device", &utf16));
        assert_eq!(read(&paths, "nexus-device"), Err(Trouble::Garbled));
        assert!(raw::put(&paths, "nexus-device", b"{\"bare\":\"utf-8\"}"));
        assert_eq!(read(&paths, "nexus-device"), Err(Trouble::Garbled));
        assert!(raw::put(&paths, "nexus-device", b"nexus-bound-1\nhttps://x\nt"));
        assert_eq!(read(&paths, "nexus-device"), Err(Trouble::Garbled));
        let big = "y".repeat(3 * BLOB_MAX);
        assert!(write(&paths, "nexus-sign-in", &big));
        let part = raw::names(&paths).unwrap().into_iter().find(|name| name.ends_with("~1")).unwrap();
        assert!(raw::put(&paths, &part, b"z"));
        assert_eq!(read(&paths, "nexus-sign-in"), Err(Trouble::Garbled));
        assert!(raw::delete(&paths, &part));
        assert_eq!(read(&paths, "nexus-sign-in"), Err(Trouble::Garbled));
    }

    /// A write cut short leaves the record before, whole; its stray parts go at the sweep.
    #[test]
    fn a_write_that_fails_leaves_what_was_there_and_its_parts_are_swept() {
        let (_dir, paths) = paths();
        let before = "b".repeat(2 * BLOB_MAX);
        assert!(write(&paths, "nexus-sign-in", &before));
        // The head's write fails (the store full): the parts written go, the record stays.
        let after = "a".repeat(5 * BLOB_MAX);
        fail_writes(Some("nexus-sign-in"));
        assert!(!write(&paths, "nexus-sign-in", &after));
        assert!(!forget(&paths, "nexus-sign-in"));
        fail_writes(None);
        assert_eq!(read(&paths, "nexus-sign-in").unwrap().as_deref().map(String::as_str), Some(before.as_str()));
        // Parts of a write killed before its head: swept, the record's own kept.
        assert!(raw::put(&paths, "nexus-sign-in~00000000000000aa~0", b"stray"));
        sweep(&paths, &["nexus-sign-in"]);
        assert!(raw::get(&paths, "nexus-sign-in~00000000000000aa~0").unwrap().is_none());
        assert_eq!(read(&paths, "nexus-sign-in").unwrap().as_deref().map(String::as_str), Some(before.as_str()));
    }
}
