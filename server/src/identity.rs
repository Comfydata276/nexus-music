//! The backend's id (design 6.3): made at random the first time the program starts, kept in
//! a small file in the store kept apart (`NEXUS_STATE`, design 8 and 9.2), and the same for
//! as long as that file is. A device remembers it, and asks the person again if it changes.
//!
//! So the file is never written over. If it is there and is not an id, the program says so
//! and stops, and the owner decides.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use nexus_core::nexus::proto::BackendId;

/// The file's name inside the store kept apart.
pub const FILE: &str = "id";
/// Each start that makes an id writes it first to a file of its own, named for it, and links
/// that into place.
const STAGING: &str = "id.new-";
/// The most the file may be: an id, and a line's end or two.
const FILE_MAX_BYTES: u64 = 64;

fn place(state: &Path) -> PathBuf {
    state.join(FILE)
}

/// The id kept in `state`, if there is one yet.
pub fn read(state: &Path) -> Result<Option<BackendId>, String> {
    let path = place(state);
    let unreadable = |why: io::Error| format!("The backend's id at {} can't be read ({}).", path.display(), why.kind());
    match fs::metadata(&path) {
        Err(why) if why.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(why) => return Err(unreadable(why)),
        Ok(meta) if !meta.is_file() || meta.len() > FILE_MAX_BYTES => return Err(not_an_id(&path)),
        Ok(_) => {}
    }
    let bytes = fs::read(&path).map_err(unreadable)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| not_an_id(&path))?;
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);
    BackendId::from_hex(text).map(Some).ok_or_else(|| not_an_id(&path))
}

fn not_an_id(path: &Path) -> String {
    format!(
        "{} is not a backend id (32 lower-case hex digits). It is left as it is: put back the id it held, or remove it to have a new one made, and every device will then ask its person again.",
        path.display()
    )
}

/// The id kept in `state`, made there from `random` if there is none yet; and whether this
/// start made it.
///
/// Two starts at once each write their own staging file, and each links it into place only
/// where there is nothing yet, so one wins and neither replaces the other. Both then read back
/// what is on the disk and take that, so that both serve the one id there is.
pub fn read_or_make(state: &Path, random: impl FnOnce() -> Result<[u8; 16], String>) -> Result<(BackendId, bool), String> {
    if let Some(id) = read(state)? {
        return Ok((id, false));
    }
    let id = BackendId::from_bytes(random()?);
    // Named for the id this start made: no other start's has the same name.
    let staging = state.join(format!("{STAGING}{}", id.to_hex()));
    let failed = |why: io::Error| format!("The backend's id couldn't be written in {} ({}).", state.display(), why.kind());
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = options.open(&staging).and_then(|mut file| {
        file.write_all(format!("{}\n", id.to_hex()).as_bytes())?;
        file.sync_all()
    });
    if let Err(why) = written {
        let _ = fs::remove_file(&staging);
        return Err(failed(why));
    }
    // A link, not a rename: it fails if an id is there already, so one is never replaced.
    let linked = fs::hard_link(&staging, place(state));
    let _ = fs::remove_file(&staging);
    match linked {
        Ok(()) => {}
        Err(why) if why.kind() == io::ErrorKind::AlreadyExists => {}
        Err(why) => return Err(failed(why)),
    }
    // The folder's own record of the names, on the disk.
    #[cfg(unix)]
    fs::File::open(state).and_then(|folder| folder.sync_all()).map_err(failed)?;
    // What is there now is the id, whichever start put it there.
    let kept = read(state)?.ok_or_else(|| not_an_id(&place(state)))?;
    Ok((kept, kept == id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("nexus-server-identity-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn made_once_and_then_kept() {
        let state = folder("kept");
        assert_eq!(read(&state), Ok(None));
        let (made, new) = read_or_make(&state, || Ok([7; 16])).unwrap();
        assert!(new);
        assert_eq!(made, BackendId::from_bytes([7; 16]));
        let (again, new) = read_or_make(&state, || panic!("no second id is made")).unwrap();
        assert!(!new);
        assert_eq!(again, made);
        assert_eq!(fs::read_to_string(state.join(FILE)).unwrap(), format!("{}\n", made.to_hex()));
        assert!(fs::read_dir(&state).unwrap().all(|entry| entry.unwrap().file_name() == FILE));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(state.join(FILE)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn a_file_that_is_not_an_id_is_left_and_said() {
        let state = folder("wrong");
        for wrong in ["", "\n", "0123456789ABCDEF0123456789ABCDEF\n", "0123456789abcdef", "0123456789abcdef0123456789abcdef0\n", "x".repeat(200).as_str()] {
            fs::write(state.join(FILE), wrong).unwrap();
            let why = read_or_make(&state, || panic!("no id is made over a wrong one")).unwrap_err();
            assert!(why.contains("is not a backend id"), "{why}");
            assert_eq!(fs::read_to_string(state.join(FILE)).unwrap(), wrong);
        }
        // An id without a line's end, or with Windows', is still the id.
        for right in ["0123456789abcdef0123456789abcdef", "0123456789abcdef0123456789abcdef\r\n"] {
            fs::write(state.join(FILE), right).unwrap();
            assert_eq!(read(&state).unwrap().unwrap().to_hex(), "0123456789abcdef0123456789abcdef");
        }
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn a_staging_file_left_behind_is_never_the_id() {
        let state = folder("half");
        fs::write(state.join(format!("{STAGING}{}", "0".repeat(32))), "half").unwrap();
        let (made, new) = read_or_make(&state, || Ok([9; 16])).unwrap();
        assert!(new && made == BackendId::from_bytes([9; 16]));
        assert_eq!(read(&state).unwrap(), Some(made));
        fs::remove_dir_all(&state).unwrap();
    }

    #[test]
    fn starts_at_once_end_with_one_id_that_each_of_them_has() {
        for round in 0..40 {
            let state = folder(&format!("race-{round}"));
            let start = std::sync::Arc::new(std::sync::Barrier::new(8));
            let starts: Vec<_> = (0..8u8)
                .map(|n| {
                    let state = state.clone();
                    let start = start.clone();
                    std::thread::spawn(move || {
                        start.wait();
                        read_or_make(&state, || Ok([n + 1; 16]))
                    })
                })
                .collect();
            let had: Vec<(BackendId, bool)> = starts.into_iter().map(|start| start.join().unwrap().unwrap()).collect();
            let kept = read(&state).unwrap().unwrap();
            assert!(had.iter().all(|(id, _)| *id == kept), "round {round}: {had:?}, kept {kept:?}");
            assert_eq!(had.iter().filter(|(_, made)| *made).count(), 1, "round {round}");
            assert!(fs::read_dir(&state).unwrap().all(|entry| entry.unwrap().file_name() == FILE), "round {round}: staging left");
            fs::remove_dir_all(&state).unwrap();
        }
    }

    #[test]
    fn no_randomness_no_id() {
        let state = folder("random");
        assert_eq!(read_or_make(&state, || Err("no randomness".into())), Err("no randomness".into()));
        assert_eq!(read(&state), Ok(None));
        fs::remove_dir_all(&state).unwrap();
    }
}
