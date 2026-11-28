//! The backend's own copies of its data (design 13.4): **one every night, made by SQLite's
//! own way of copying a database in use, seven kept**; and one made before the data's form is
//! changed, and one of the data a put-back replaced.
//!
//! - **In `NEXUS_DATA/copies`.** (The design does not say where. They are data: the store
//!   kept apart must hold nothing of the data, and the owner's own backup of the data folder
//!   then holds them too.)
//! - **Whole or not there:** each is made under a name of its own, checked by SQLite, made
//!   lasting, and only then given its name. A copy is one file, with nothing written ahead.
//! - **Made while the backend writes**, from a read of its own, in one step: SQLite gives the
//!   copy one moment of the data, with every step before it and none after.
//! - **Numbered as they are made**, and the seven with the highest numbers are kept, so that a
//!   clock set back can neither keep a copy from being made nor have the newest pruned.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::Connection;
use rusqlite::backup::{Backup, StepResult};

use crate::store::crash;
use crate::store::data::{self, why};

/// The folder's name inside `NEXUS_DATA`.
pub const FOLDER: &str = "copies";
/// How many nightly copies are kept.
pub const KEPT: usize = 7;
/// A night.
pub const EVERY_MS: u64 = 24 * 60 * 60 * 1000;
/// How often the program looks whether a copy is due.
pub const LOOK_EVERY: Duration = Duration::from_secs(10 * 60);
const MAKING: &str = ".making-";
const NIGHTLY: &str = "nightly-";

/// A nightly copy: its number, and the time it was made at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copy {
    pub number: u64,
    pub made_at: u64,
    pub path: PathBuf,
}

pub fn folder(data: &Path) -> PathBuf {
    data.join(FOLDER)
}

fn read_name(name: &str) -> Option<(u64, u64)> {
    let rest = name.strip_prefix(NIGHTLY)?.strip_suffix(".db")?;
    let (number, made_at) = rest.split_once('-')?;
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(number) || !digits(made_at) {
        return None;
    }
    Some((number.parse().ok()?, made_at.parse().ok()?))
}

/// The nightly copies there are, the newest (by number) last.
pub fn list(data: &Path) -> Vec<Copy> {
    let mut found: Vec<Copy> = fs::read_dir(folder(data))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let (number, made_at) = read_name(name.to_str()?)?;
            Some(Copy { number, made_at, path: entry.path() })
        })
        .collect();
    found.sort_by_key(|copy| copy.number);
    found
}

/// Whether a nightly copy is due at `now`: none yet; the newest made a night or more ago; or
/// the newest made at a time still to come (the clock was set back).
pub fn due(data: &Path, now: u64) -> bool {
    match list(data).last() {
        None => true,
        Some(newest) => now < newest.made_at || now - newest.made_at >= EVERY_MS,
    }
}

/// Copies the database open on `source` to `into`, whole: one step of SQLite's copy, checked,
/// written ahead in nothing, and lasting. Into a file of its own making name, then renamed.
fn copy(source: &Connection, into: &Path) -> Result<(), String> {
    let making = into.with_file_name(format!("{MAKING}{}-{}", std::process::id(), into.file_name().and_then(|name| name.to_str()).unwrap_or("copy")));
    let _ = fs::remove_file(&making);
    let made = (|| -> Result<(), String> {
        // Made empty and the owner's alone before SQLite writes a byte to it.
        crate::store::private::create(&making).map_err(|error| format!("{}", error.kind()))?;
        let mut target = Connection::open(&making).map_err(|error| why(&error))?;
        {
            let backup = Backup::new(source, &mut target).map_err(|error| why(&error))?;
            // All of it in one step: one moment of the data. (Copied in parts, a write between
            // two parts would start the copy again, for as long as writes kept coming.)
            match backup.step(-1).map_err(|error| why(&error))? {
                StepResult::Done => {}
                _ => return Err("the data was in use; it will be tried again".into()),
            }
        }
        let mode: String = target.query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0)).map_err(|error| why(&error))?;
        if !mode.eq_ignore_ascii_case("delete") {
            return Err("the copy could not be made one file".into());
        }
        let check: String = target.query_row("PRAGMA quick_check", [], |row| row.get(0)).map_err(|error| why(&error))?;
        if check != "ok" {
            return Err("SQLite found the copy damaged".into());
        }
        target.close().map_err(|(_, error)| why(&error))?;
        fs::File::open(&making).and_then(|file| file.sync_all()).map_err(|error| format!("{}", error.kind()))?;
        crash::point("copy: made").map_err(|error| format!("{}", error.kind()))?;
        fs::rename(&making, into).map_err(|error| format!("{}", error.kind()))?;
        crash::point("copy: renamed").map_err(|error| format!("{}", error.kind()))?;
        if let Some(parent) = into.parent() {
            fs::File::open(parent).and_then(|folder| folder.sync_all()).map_err(|error| format!("{}", error.kind()))?;
        }
        Ok(())
    })();
    if made.is_err() {
        let _ = fs::remove_file(&making);
    }
    made
}

fn make_folder(data: &Path) -> Result<PathBuf, String> {
    let copies = folder(data);
    let fresh = !copies.exists();
    crate::store::private::folder(&copies).map_err(|error| format!("The folder for copies, {}, can't be made ({}).", copies.display(), error.kind()))?;
    if fresh {
        // The folder's name in the data folder is lasting before anything is kept in it.
        fs::File::open(data).and_then(|folder| folder.sync_all()).map_err(|error| format!("The data folder couldn't be made lasting ({}).", error.kind()))?;
    }
    Ok(copies)
}

/// Makes the nightly copy of the data in `data`, from a read of its own, and prunes the
/// copies to the newest [`KEPT`]. Safe while the backend writes.
pub fn nightly(data: &Path, now: u64) -> Result<Copy, String> {
    let failed = |why: String| format!("The night's copy of the data couldn't be made: {why}.");
    let copies = make_folder(data).map_err(failed)?;
    let source = data::open_to_read(&data::place(data), "The data").map_err(failed)?;
    let number = list(data).last().map_or(1, |newest| newest.number.saturating_add(1));
    let path = copies.join(format!("{NIGHTLY}{number:06}-{now}.db"));
    copy(&source, &path).map_err(failed)?;
    prune(data);
    Ok(Copy { number, made_at: now, path })
}

/// Keeps the newest [`KEPT`] nightly copies, by number, and removes what a cut copy left.
pub fn prune(data: &Path) {
    let all = list(data);
    for old in all.iter().take(all.len().saturating_sub(KEPT)) {
        let _ = fs::remove_file(&old.path);
    }
    if let Ok(entries) = fs::read_dir(folder(data)) {
        for entry in entries.flatten() {
            if entry.file_name().to_str().is_some_and(|name| name.starts_with(MAKING)) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

/// The copy made before the data's form is changed (11.2). Where it is.
pub fn before_form(data: &Path, source: &Connection, form: i64, now: u64) -> Result<PathBuf, String> {
    let failed = |why: String| format!("The data's form was not changed: the copy made first couldn't be made ({why}).");
    let copies = make_folder(data).map_err(failed)?;
    let path = copies.join(format!("before-form-{}-{form}-{now}.db", form + 1));
    copy(source, &path).map_err(failed)?;
    Ok(path)
}

/// Copies `source` (a copy to be put back) to `into` (the data's own place, renamed into it
/// last): whole, checked and lasting.
pub fn put(source: &Connection, into: &Path) -> Result<(), String> {
    copy(source, into)
}
