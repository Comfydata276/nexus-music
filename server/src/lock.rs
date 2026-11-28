//! One program at a time on a backend's folders (design 13.4: one thread writes). The
//! backend as it serves, and each of the owner's commands, holds a lock on a file in each
//! folder for as long as it runs; the system lets go of it when the program ends, however it
//! ends. A second program finds either taken and stops, saying so, before it reads anything.
//!
//! The container's health check takes neither: it only asks the backend that holds them.

use std::fs::{self, File, TryLockError};
use std::path::Path;

/// The file's name in each folder. It holds nothing.
pub const FILE: &str = "lock";

/// Both locks, held until this is dropped.
#[derive(Debug)]
pub struct Held {
    _state: File,
    _data: File,
}

fn take(folder: &Path, name: &str) -> Result<File, String> {
    let path = folder.join(FILE);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|why| format!("{name} can't be used: the lock file {} can't be opened ({}).", path.display(), why.kind()))?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(format!(
            "{name} is in use by another Nexus server, or by one of its commands: one program at a time may use it. If the backend is serving, stop it first."
        )),
        Err(TryLockError::Error(why)) => Err(format!("{name} can't be locked ({}).", why.kind())),
    }
}

/// Takes the store's folder's lock, then the data folder's.
pub fn hold(state: &Path, data: &Path) -> Result<Held, String> {
    let state = take(state, "NEXUS_STATE")?;
    let data = take(data, "NEXUS_DATA")?;
    Ok(Held { _state: state, _data: data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_of_either_folder_is_refused_until_the_first_lets_go() {
        let root = std::env::temp_dir().join(format!("nexus-server-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for name in ["state", "data", "other-state", "other-data"] {
            fs::create_dir_all(root.join(name)).unwrap();
        }
        let first = hold(&root.join("state"), &root.join("data")).unwrap();
        let why = hold(&root.join("state"), &root.join("other-data")).unwrap_err();
        assert!(why.starts_with("NEXUS_STATE is in use"), "{why}");
        let why = hold(&root.join("other-state"), &root.join("data")).unwrap_err();
        assert!(why.starts_with("NEXUS_DATA is in use"), "{why}");
        drop(first);
        hold(&root.join("state"), &root.join("data")).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }
}
