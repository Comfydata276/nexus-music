//! Playlist destinations belong to this PC's import. This queue holds only
//! outstanding playlist writes. The Requests page never draws history from it.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::asked::{Arrival, Asked};
use crate::app_nexus_calls::Context;

static WRITE: Mutex<()> = Mutex::new(());

pub(super) fn file(folder: &Path, context: Context) -> PathBuf {
    folder.join(format!("request-playlists-{}-{}-{}.json", context.backend.to_hex(), context.account.get(), context.restore))
}

fn read(file: &Path) -> std::io::Result<Vec<Asked>> {
    match std::fs::read(file) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn change(file: &Path, apply: impl FnOnce(&mut Vec<Asked>)) -> Result<(), String> {
    let _lock = WRITE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut items = read(file).map_err(|_| "Couldn't read the saved playlist requests.".to_string())?;
    apply(&mut items);
    let bytes = serde_json::to_vec(&items).map_err(|_| "Couldn't save the playlist requests.".to_string())?;
    crate::config::write_surely(file, &bytes).map_err(|_| "Couldn't save the playlist requests.".to_string())
}

pub(super) fn remember(file: &Path, notes: Vec<Asked>) -> Result<(), String> {
    if notes.is_empty() {
        return Ok(());
    }
    change(file, |items| {
        nexus_core::asked::tidy(items, chrono::Utc::now().timestamp());
        for note in notes {
            nexus_core::asked::put(items, note);
        }
    })
}

pub(crate) fn arrivals(folder: &Path, context: Context, index: &crate::search::Index, now: i64) -> Vec<Arrival> {
    let Ok(items) = read(&file(folder, context)) else { return Vec::new() };
    crate::asked::arrivals(index, &items, now)
}

pub(crate) fn settled(folder: PathBuf, context: Context, arrivals: Vec<Arrival>, now: i64) {
    std::thread::spawn(move || {
        let _ = change(&file(&folder, context), |items| {
            nexus_core::asked::mark(items, &arrivals, now);
            items.retain(|note| !arrivals.iter().any(|arrival| {
                arrival.missing == 0 && note.fills.len() == arrival.fills.len()
                    && nexus_core::asked::is_note(note, arrival)
            }));
        });
    });
}

pub(crate) fn cancel(folder: PathBuf, context: Context, album: String, title: String, artist: String) {
    std::thread::spawn(move || {
        let _ = change(&file(&folder, context), |items| {
            nexus_core::asked::forget(items, &album, &title, &artist);
        });
    });
}
