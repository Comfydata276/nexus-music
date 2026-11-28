//! Songs saved on this PC, so they play without the server: the files, their index, and the
//! controller's side of downloading and removing them.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use slint::ComponentHandle;

use crate::diagnostics::{self, FaultPlace, FaultShape, Line, Note};
use crate::controller::{Controller, Route, post};
use crate::pages::{PageData, PlaySource};
use crate::playback::NATIVE_SUFFIXES;
use crate::subsonic::{Client, Song};
use crate::ui;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub song: Song,
    /// File name within the downloads folder.
    pub file: String,
    pub bytes: u64,
    /// When it was downloaded, in seconds since 1970; 0 for downloads from before this was kept.
    #[serde(default)]
    pub at: i64,
}

/// A song waiting to be fetched, or being fetched now.
struct Job {
    song: Song,
    /// Which go at this song this is. A song can be cancelled and asked for again while
    /// the first go is still winding down; what that one reports mustn't be taken for this.
    attempt: u64,
    /// How much has arrived, in thousandths.
    progress: Arc<AtomicU32>,
    /// Set to abandon the fetch.
    cancel: Arc<AtomicBool>,
    /// The music login it was asked for under (number, generation): held under an older one
    /// than the login in use when it reports, it is asked for again at once (W7c item 4).
    login: (u64, u64),
}

pub struct Downloads {
    dir: PathBuf,
    /// Oldest first.
    entries: Vec<Entry>,
    files: HashMap<String, String>,
    /// Playlists downloaded as a whole (their ids, or "liked" for Liked songs), so songs
    /// added to them later are downloaded too.
    collections: Vec<String>,
    /// In the order they'll be fetched.
    jobs: Vec<Job>,
    /// How far each song in `jobs` has got, by song id, so a row can be asked about without
    /// going through the list.
    under_way: HashMap<String, Arc<AtomicU32>>,
    /// When each downloaded song arrived, by song id.
    arrived: HashMap<String, i64>,
    /// The list of downloads has changed since it was last written to disk.
    unsaved: bool,
    /// The list was there and couldn't be read when this was loaded (and so is taken for
    /// none): a move never writes over it.
    unreadable: bool,
    /// Whose downloads these are (see `account`): each account has its own list, in the same
    /// folder, so signing in as someone else doesn't show the last person's.
    account: String,
    next_attempt: u64,
    /// Songs whose download the music server's refusal of the login stopped (design 6.3):
    /// not failed, but held, and asked for once more under a renewed login.
    held: Vec<Song>,
    /// Songs that couldn't be downloaded this session, and why, newest last.
    pub failed: Vec<(Song, String)>,
    saved_at: std::time::Instant,
    /// For the progress messages: how many songs the current batch has, and how many of
    /// them are done or failed.
    batch_total: usize,
    batch_done: usize,
    batch_failed: usize,
}

/// Whether `name` is what a copy across drives (`copy_whole`) left when it was stopped: a
/// listed song's name, then `.<process>.<count>.tmp`.
fn move_leftover(name: &str, entries: &[Entry]) -> bool {
    let Some(rest) = name.strip_suffix(".tmp") else { return false };
    let mut parts = rest.rsplitn(3, '.');
    let (Some(count), Some(process), Some(song)) = (parts.next(), parts.next(), parts.next()) else { return false };
    let digits = |part: &str| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit());
    digits(count) && digits(process) && entries.iter().any(|entry| entry.file == song)
}

/// Whether `name` is a download of the app's that was cut short (see `Downloads::load`).
fn partial_download(name: &str, entries: &[Entry]) -> bool {
    let Some(file) = name.strip_suffix(".part") else { return false };
    if entries.iter().any(|entry| entry.file == file) {
        return true;
    }
    let mut parts = file.rsplitn(3, '.');
    let (Some(attempt), Some(extension), Some(stem)) = (parts.next(), parts.next(), parts.next()) else { return false };
    let word = |part: &str, more: &str| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || more.contains(c));
    attempt.chars().all(|c| c.is_ascii_digit()) && !attempt.is_empty() && word(extension, "") && word(stem, "-_")
}

/// Where an account's downloads are (W7c item 5): the place chosen for that account under
/// Settings (`downloads_dirs`), else the one chosen for this PC (`downloads_dir`), else the
/// app's own; and inside it **always a folder of the account's own**, so that two accounts
/// with the same place never share a file name. The folder this PC had before (`legacy`,
/// carried over to one person) is that place itself, as 0.40 kept it; nobody signed in, too.
pub fn account_dir(settings: &crate::config::Settings, default: &Path, folder: &str, legacy: bool) -> PathBuf {
    let base = match settings.downloads_dirs.get(folder) {
        Some(chosen) => PathBuf::from(chosen),
        None if settings.downloads_dir.is_empty() => default.to_path_buf(),
        None => PathBuf::from(&settings.downloads_dir),
    };
    if folder.is_empty() || legacy { base } else { base.join(folder) }
}

/// Which songs an account has downloaded here, and which lists it keeps downloaded.
pub fn index_file(dir: &Path, account: &str) -> PathBuf {
    dir.join(format!("index-{account}.json"))
}

pub fn collections_file(dir: &Path, account: &str) -> PathBuf {
    dir.join(format!("collections-{account}.json"))
}

impl Downloads {
    /// Reads the downloads that belong to `account` (empty for nobody: signed out), claiming
    /// what 0.28 and before kept for whoever was signed in.
    pub fn load(dir: PathBuf, account: &str) -> Downloads {
        Self::load_for(dir, account, true)
    }

    /// The same; what 0.28 and before kept is claimed only when `legacy` (the folder this PC
    /// had before, carried over: the review's MUST 14).
    pub fn load_for(dir: PathBuf, account: &str, legacy: bool) -> Downloads {
        let _ = fs::create_dir_all(&dir);
        if !account.is_empty() && legacy {
            // Before 0.29 there was one list for whoever was signed in. It becomes the list
            // of the first account to sign in after updating.
            for name in ["index", "collections"] {
                let (old, new) = (dir.join(format!("{name}.json")), dir.join(format!("{name}-{account}.json")));
                if old.exists() && !new.exists() {
                    let _ = fs::rename(&old, &new);
                }
            }
        }
        let (mut entries, unreadable): (Vec<Entry>, bool) = match fs::read(index_file(&dir, account)) {
            _ if account.is_empty() => (Vec::new(), false),
            Ok(bytes) => serde_json::from_slice(&bytes).map_or((Vec::new(), true), |entries| (entries, false)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Vec::new(), false),
            Err(_) => (Vec::new(), true),
        };
        // Trust the folder over the index: a file deleted by hand is simply not downloaded.
        entries.retain(|entry| dir.join(&entry.file).is_file());
        // Fetches that were cut short by the app closing: only of the shapes the app writes
        // (`<id>.<extension>.<attempt>.part`, since 0.29), or a bare `<file>.part` (before)
        // for a file the list names. A browser's unfinished `….part` there is the person's.
        for stale in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let name = stale.file_name().to_string_lossy().into_owned();
            if (partial_download(&name, &entries) || move_leftover(&name, &entries)) && stale.file_type().is_ok_and(|kind| kind.is_file()) {
                let _ = fs::remove_file(stale.path());
            }
        }
        let files = entries.iter().map(|e| (e.song.id.clone(), e.file.clone())).collect();
        let arrived = entries.iter().map(|e| (e.song.id.clone(), arrival(&dir, e))).collect();
        let collections = fs::read(collections_file(&dir, account))
            .ok()
            .filter(|_| !account.is_empty())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Downloads {
            dir,
            entries,
            files,
            collections,
            jobs: Vec::new(),
            under_way: HashMap::new(),
            arrived,
            unsaved: false,
            unreadable,
            account: account.to_string(),
            next_attempt: 0,
            held: Vec::new(),
            failed: Vec::new(),
            saved_at: std::time::Instant::now(),
            batch_total: 0,
            batch_done: 0,
            batch_failed: 0,
        }
    }

    /// Writes the list: flushed when a batch is over, or after a removal (`Durable`); in
    /// passing while a batch is arriving.
    fn save(&mut self, durable: crate::config::Durable) {
        if self.account.is_empty() {
            return;
        }
        // Only a list that is on the disk is no longer due (and one that isn't is tried
        // again a couple of seconds later, or at the end of the batch).
        if let Ok(json) = serde_json::to_vec(&self.entries) {
            let written = crate::config::write_whole(&index_file(&self.dir, &self.account), &json, durable, &crate::config::REPLACING);
            if written.is_ok() {
                self.unsaved = false;
                // The list on the disk is whole again: it can be moved.
                self.unreadable = false;
            }
        }
        self.saved_at = std::time::Instant::now();
    }

    /// Whether the downloads can be moved, as their list stands: never one that couldn't be
    /// read (it would be written over, or left behind); and only once what it has that isn't
    /// written yet is (flushed: the person pressed Move). A list with nothing unwritten isn't
    /// written again, as 0.40.10 didn't.
    pub fn ready_to_move(&mut self) -> Result<(), String> {
        if self.unreadable {
            return Err(format!("The list of downloads in {} couldn't be read, so nothing was moved.", self.dir.display()));
        }
        if self.unsaved && !self.account.is_empty() {
            self.save(crate::config::Durable::Flushed);
            if self.unsaved {
                return Err("The list of downloads couldn't be saved first, so nothing was moved.".into());
            }
        }
        Ok(())
    }

    /// Writes the list of downloads if it has changed. While a batch is arriving that would
    /// be the whole list rewritten for every song, so it's done every couple of seconds
    /// instead, and once more when the batch is over (`now`).
    pub fn save_if_due(&mut self, now: bool) {
        // (A download finishing, a batch's end, quitting: in passing. A removal the person
        // asked for is saved, flushed, by `remove_downloads`.)
        if self.unsaved && (now || self.saved_at.elapsed() > std::time::Duration::from_secs(2)) {
            self.save(crate::config::Durable::InPassing);
        }
    }

    /// Remembers (or forgets) that a playlist is kept downloaded.
    /// Removes downloads the person asked to: the list saved once for all of them, flushed
    /// (it is their press).
    pub fn remove_many(&mut self, ids: &[String]) {
        for id in ids {
            self.remove(id);
        }
        if self.unsaved {
            self.save(crate::config::Durable::Flushed);
        }
    }

    /// Keeps no playlist downloaded any more: saved once (the person's "Remove all").
    fn forget_collections(&mut self) {
        if self.collections.is_empty() {
            return;
        }
        self.collections.clear();
        if let (Ok(json), false) = (serde_json::to_vec(&self.collections), self.account.is_empty()) {
            let _ = crate::config::write_surely(&collections_file(&self.dir, &self.account), &json);
        }
    }

    fn keep_collection(&mut self, id: &str, keep: bool) {
        self.collections.retain(|c| c != id);
        if keep {
            self.collections.push(id.to_string());
        }
        if let (Ok(json), false) = (serde_json::to_vec(&self.collections), self.account.is_empty()) {
            let _ = crate::config::write_surely(&collections_file(&self.dir, &self.account), &json);
        }
    }

    /// The folder the downloads are in.
    /// At an account's leaving: every download under way is cancelled (its worker stops at
    /// its next read, and its result is not taken).
    pub fn cancel_all(&mut self) {
        for job in self.jobs.drain(..) {
            job.cancel.store(true, Ordering::Relaxed);
        }
        self.under_way.clear();
        self.held.clear();
    }

    /// A song whose download a refusal stopped, under the login `login`: kept to be asked for
    /// again once under a renewed login; or, if the login in use (`current`) is already
    /// another one that may send (the download missed the renewal's edge), handed back to be
    /// asked for again at once (W7c item 4).
    fn hold(&mut self, song: Song, login: (u64, u64), current: Option<&Client>) -> Option<Song> {
        self.held.retain(|held| held.id != song.id);
        if current.is_some_and(|client| client.handed() != login && !client.refused() && !client.retired()) {
            return Some(song);
        }
        self.held.push(song);
        None
    }

    /// Cancelled: no longer asked for again under a renewed login either.
    fn forget_held(&mut self, ids: &[String]) {
        self.held.retain(|held| !ids.contains(&held.id));
    }

    /// What a refused login held, to be asked for again (once) under a renewed one.
    pub fn take_held(&mut self) -> Vec<Song> {
        std::mem::take(&mut self.held)
    }

    pub fn has(&self, id: &str) -> bool {
        self.files.contains_key(id)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether a song is waiting to be downloaded or on its way.
    fn queued(&self, id: &str) -> bool {
        self.under_way.contains_key(id)
    }

    /// How far a song's download has got, from 0 to 1; -1 if it isn't being downloaded.
    pub fn progress_of(&self, id: &str) -> f32 {
        self.under_way.get(id).map(|progress| progress.load(Ordering::Relaxed) as f32 / 1000.0).unwrap_or(-1.0)
    }

    /// When a song was downloaded, in seconds since 1970; 0 if it isn't (or isn't yet).
    /// The form a downloaded song was saved in: its file's extension, and its bit rate in
    /// kbps as near as the file's size and the song's length say.
    pub fn saved_as(&self, song: &Song) -> Option<(String, u32)> {
        let file = self.files.get(&song.id)?;
        let suffix = file.rsplit('.').next().unwrap_or_default().to_lowercase();
        let bytes = self.entries.iter().rev().find(|entry| entry.song.id == song.id).map_or(0, |entry| entry.bytes);
        let kbps = if song.duration > 0 { (bytes * 8 / song.duration as u64 / 1000) as u32 } else { 0 };
        Some((suffix, kbps))
    }

    /// One line about the songs that couldn't be downloaded, or nothing if they all could.
    pub fn trouble(&self) -> String {
        let Some((song, reason)) = self.failed.last() else { return String::new() };
        let which = match self.failed.len() {
            1 => format!("“{}”", song.title),
            2 => format!("“{}” and 1 other song", song.title),
            n => format!("“{}” and {} other songs", song.title, n - 1),
        };
        format!("{which} couldn't be downloaded. {reason}")
    }

    pub fn added_at(&self, id: &str) -> i64 {
        self.arrived.get(id).copied().unwrap_or(0)
    }

    /// Where a downloaded song's audio is, if it has been downloaded.
    pub fn path_of(&self, id: &str) -> Option<PathBuf> {
        self.files.get(id).map(|file| self.dir.join(file))
    }

    /// The songs on their way (in the order they'll arrive), then the downloaded ones, most
    /// recently added first.
    pub fn songs(&self) -> Vec<Song> {
        self.jobs.iter().map(|job| job.song.clone()).chain(self.entries.iter().rev().map(|e| e.song.clone())).collect()
    }

    pub fn downloaded_count(&self) -> usize {
        self.entries.len()
    }

    fn total_bytes(&self) -> u64 {
        self.entries.iter().map(|e| e.bytes).sum()
    }

    /// "12 songs, 340 MB"
    pub fn summary(&self) -> String {
        let megabytes = self.total_bytes() as f64 / (1024.0 * 1024.0);
        let size = if megabytes >= 1024.0 {
            format!("{:.1} GB", megabytes / 1024.0)
        } else {
            format!("{megabytes:.0} MB")
        };
        match self.entries.len() {
            0 => "Nothing downloaded".into(),
            1 => format!("1 song, {size}"),
            n => format!("{n} songs, {size}"),
        }
    }

    /// How far the whole batch has got, from 0 to 1, counting songs part-way through.
    fn batch_progress(&self) -> f32 {
        let partial: f32 = self.jobs.iter().map(|job| job.progress.load(Ordering::Relaxed) as f32 / 1000.0).sum();
        ((self.batch_done + self.batch_failed) as f32 + partial) / self.batch_total.max(1) as f32
    }

    fn add(&mut self, entry: Entry) {
        // Only a song downloaded a second time has an earlier entry to take out.
        if self.files.insert(entry.song.id.clone(), entry.file.clone()).is_some() {
            self.entries.retain(|e| e.song.id != entry.song.id);
        }
        self.arrived.insert(entry.song.id.clone(), arrival(&self.dir, &entry));
        self.entries.push(entry);
        self.unsaved = true;
    }

    fn remove(&mut self, id: &str) {
        if let Some(file) = self.files.remove(id) {
            let _ = fs::remove_file(self.dir.join(file));
            self.entries.retain(|e| e.song.id != id);
            self.arrived.remove(id);
            self.unsaved = true;
        }
    }
}

/// When a download arrived, in seconds since 1970; 0 if that can't be told.
fn arrival(dir: &std::path::Path, entry: &Entry) -> i64 {
    if entry.at > 0 {
        return entry.at;
    }
    // Older downloads: the file's own date says when it arrived.
    fs::metadata(dir.join(&entry.file))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

/// Room left on the drive after a batch of downloads, so they never fill it to the brim.
const SPARE: u64 = 500 * 1024 * 1024;

/// How much room is free on the drive a folder is on, if that can be told.
fn free_space(folder: &Path) -> Option<u64> {
    fs4::available_space(folder).ok()
}

/// "340 MB", "12.4 GB".
fn size_label(bytes: u64) -> String {
    let megabytes = bytes as f64 / (1024.0 * 1024.0);
    if megabytes >= 1024.0 { format!("{:.1} GB", megabytes / 1024.0) } else { format!("{megabytes:.0} MB") }
}

/// Whether a download keeps the file as the server has it, and how big it's expected to be.
fn expected_bytes(song: &Song, quality: u32) -> u64 {
    let suffix = song.suffix.to_lowercase();
    let playable = NATIVE_SUFFIXES.contains(&suffix.as_str());
    let original = playable && (quality == 0 || (suffix == "mp3" && song.bit_rate > 0 && song.bit_rate <= quality));
    if original { song.size } else { song.duration as u64 * if quality == 0 { 320 } else { quality } as u64 * 125 }
}

/// What moving the downloads moves: **decided once, before anything moves**. The lists that
/// read as the app's (`index-<account>.json`, a list of downloads, and the one from before
/// 0.29 without an account; and beside each, the same account's `collections-…json` if it
/// reads as a list of playlists), and the songs the lists of downloads name: each only if
/// its name is a plain file name (no folder in it, not `.` or `..`) and it is a file in the
/// folder. Nothing else there is the app's: the folder may be one of the person's own.
pub struct Plan {
    pub names: Vec<std::ffi::OsString>,
    /// How much the files come to, for the room check.
    pub bytes: u64,
}

/// `plan`, for the downloads of `account`: refused if their own list is there and can't be
/// read as the move is planned (it would be left out, and the move said to be done with the
/// songs still where they were). Another account's that can't be read is left out, as `plan`
/// leaves it.
pub fn plan_for(dir: &Path, account: &str) -> Result<Plan, String> {
    let list = index_file(dir, account);
    match fs::read(&list) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(bytes) if serde_json::from_slice::<Vec<Entry>>(&bytes).is_ok() => {}
        _ => return Err(format!("The list of downloads in {} couldn't be read, so nothing was moved.", dir.display())),
    }
    Ok(plan_of(dir, Some(account)))
}

/// Everything of the app's in `dir`, every account's (the move's own tests).
#[cfg(test)]
pub fn plan(dir: &Path) -> Plan {
    plan_of(dir, None)
}

/// `plan`, of `only` that account's list, its playlists kept downloaded and the files its
/// list names, when given (W7c item 5: a move acts on the signed-in account's alone).
fn plan_of(dir: &Path, only: Option<&str>) -> Plan {
    let plain = |name: &str| !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\\']) && Path::new(name).file_name().is_some_and(|only| only == name);
    let mut names: Vec<std::ffi::OsString> = Vec::new();
    for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let account = if name == "index.json" {
            Some(String::new())
        } else {
            name.strip_prefix("index-").and_then(|rest| rest.strip_suffix(".json")).map(str::to_string)
        };
        let Some(account) = account else { continue };
        if only.is_some_and(|only| only != account) {
            continue;
        }
        let Ok(entries) = fs::read(entry.path()).map(|bytes| serde_json::from_slice::<Vec<Entry>>(&bytes)) else { continue };
        let Ok(entries) = entries else { continue };
        names.push(entry.file_name());
        names.extend(entries.into_iter().map(|entry| entry.file).filter(|file| plain(file) && dir.join(file).is_file()).map(Into::into));
        let collections = if account.is_empty() { "collections.json".to_string() } else { format!("collections-{account}.json") };
        if fs::read(dir.join(&collections)).is_ok_and(|bytes| serde_json::from_slice::<Vec<String>>(&bytes).is_ok()) {
            names.push(collections.into());
        }
    }
    names.sort();
    names.dedup();
    let bytes = names.iter().filter_map(|name| fs::metadata(dir.join(name)).ok()).map(|meta| meta.len()).sum();
    Plan { names, bytes }
}

/// How a move of the downloads went.
#[derive(Debug, PartialEq, Eq)]
pub enum Moved {
    /// All of them are in the new folder.
    All(usize),
    /// So many of their names are had in the new folder already: nothing was moved.
    Taken(usize),
    /// So many of the `of` couldn't be moved, and every one that had moved came back.
    Back { stuck: usize, of: usize },
    /// Some couldn't be moved, and `left` of those that had couldn't be brought back: those
    /// are in the new folder, the rest in the old.
    Split { left: usize },
}

/// The steps a move takes for each file, given so that a test can have one fail, or have the
/// app stopped at one.
#[derive(Clone, Copy)]
pub struct MoveSteps {
    /// Renames a file, on the same drive.
    pub rename: fn(&Path, &Path) -> std::io::Result<()>,
    /// Makes sure what was copied is on the disk.
    pub flush: fn(&fs::File) -> std::io::Result<()>,
    /// The temporary name a copy is made under, beside where it goes.
    pub temp: fn(&Path) -> PathBuf,
}

pub const MOVING: MoveSteps = MoveSteps { rename: |from, to| fs::rename(from, to), flush: fs::File::sync_all, temp: crate::config::temp_for };

/// Moves the app's downloads (`plan`) from one folder into another. **A name of the plan had
/// in the new folder already stops the move before anything moves.** A file that can't be
/// moved has what this move moved put back, by the list this move kept: never by looking at
/// a folder. Each file is whole in one folder or the other throughout; a kill part-way leaves
/// them so, split, and is not resumed (see `docs/FILES.md`).
#[cfg(test)]
pub fn move_folder(from: &Path, to: &Path) -> Moved {
    move_folder_by(from, to, &plan(from), &MOVING)
}

pub fn move_folder_by(from: &Path, to: &Path, plan: &Plan, steps: &MoveSteps) -> Moved {
    let taken = plan.names.iter().filter(|name| fs::symlink_metadata(to.join(name)).is_ok()).count();
    if taken > 0 {
        return Moved::Taken(taken);
    }
    let mut moved: Vec<&std::ffi::OsString> = Vec::new();
    let mut stuck = 0;
    for name in &plan.names {
        if move_one(&from.join(name), &to.join(name), steps) {
            moved.push(name);
        } else {
            stuck += 1;
        }
    }
    if stuck == 0 {
        return Moved::All(moved.len());
    }
    let left = moved.iter().filter(|name| !move_one(&to.join(name), &from.join(name), steps)).count();
    if left == 0 { Moved::Back { stuck, of: plan.names.len() } } else { Moved::Split { left } }
}

/// Moves one file to a name nothing has: by renaming, or else (another drive) by copying.
fn move_one(from: &Path, to: &Path, steps: &MoveSteps) -> bool {
    // Looked for just before: the standard library's rename replaces what has the name, and
    // a file given that name in the few microseconds between is the one case this doesn't
    // guard against.
    if fs::symlink_metadata(to).is_ok() {
        return false;
    }
    (steps.rename)(from, to).is_ok() || copy_whole(from, to, steps)
}

/// Across drives a file arrives under its name whole or not at all: copied under a name of
/// the app's own beside it (`config::temp_for`), flushed, then given its name, and only then
/// the first removed. A copy that can't be finished goes; if the first can't be removed, the
/// copy goes too, and the file is where it was.
fn copy_whole(from: &Path, to: &Path, steps: &MoveSteps) -> bool {
    // A temporary name of its own: one that is taken (a file there, of whoever's) is passed
    // over for the next, and left as it is. Only what this copy made is ever removed.
    let made = (0..8).find_map(|_| {
        let temp = (steps.temp)(to);
        match fs::OpenOptions::new().write(true).create_new(true).open(&temp) {
            Ok(file) => Some(Ok((temp, file))),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
            Err(error) => Some(Err(error)),
        }
    });
    let Some(Ok((temp, mut target))) = made else { return false };
    let copied = (|| {
        let mut source = fs::File::open(from)?;
        let length = std::io::copy(&mut source, &mut target)?;
        (steps.flush)(&target)?;
        if fs::metadata(from)?.len() != length {
            return Err(std::io::Error::other("the copy came out a different length"));
        }
        Ok(())
    })();
    if copied.is_err() || fs::symlink_metadata(to).is_ok() || fs::rename(&temp, to).is_err() {
        let _ = fs::remove_file(&temp);
        return false;
    }
    if fs::remove_file(from).is_err() {
        let _ = fs::remove_file(to);
        return false;
    }
    true
}

/// Whether two folders are on one drive (a rename between them works), tried with a scrap of
/// a file under a name nothing has (`create_new`); only the file it made is removed.
pub fn same_drive(from: &Path, to: &Path) -> bool {
    let probe = crate::config::temp_for(&from.join("move-test"));
    let Some(name) = probe.file_name() else { return false };
    let there = to.join(name);
    if fs::OpenOptions::new().write(true).create_new(true).open(&probe).is_err() {
        return false;
    }
    let same = fs::symlink_metadata(&there).is_err() && fs::rename(&probe, &there).is_ok();
    let _ = fs::remove_file(if same { &there } else { &probe });
    same
}

/// Fetches one song's audio into the downloads folder. Runs on a worker thread. `quality`
/// is a bitrate cap in kbps, or 0 to keep the file as it is on the server.
#[derive(Debug)]
enum FetchFailure {
    Cancelled,
    /// The music server refused the login (or it was set aside): held, not failed.
    Held,
    Failed { shape: FaultShape, reason: String },
}

impl FetchFailure {
    fn failed(shape: FaultShape, reason: impl ToString) -> Self {
        Self::Failed { shape, reason: reason.to_string() }
    }
}

fn fetch(
    client: &Client,
    dir: &Path,
    song: &Song,
    quality: u32,
    attempt: u64,
    progress: &AtomicU32,
    cancel: &AtomicBool,
) -> Result<Fetched, FetchFailure> {
    let suffix = song.suffix.to_lowercase();
    let playable = NATIVE_SUFFIXES.contains(&suffix.as_str());
    // The original is kept when lossless is asked for, or when it's already an MP3 within the
    // limit. Otherwise the server converts it, as it would for streaming. (Formats the app
    // can't decode itself are always converted.)
    let original = playable && (quality == 0 || (suffix == "mp3" && song.bit_rate > 0 && song.bit_rate <= quality));
    let rate = if quality == 0 { 320 } else { quality };
    let (url, extension, expected) = if original {
        (client.stream_url(&song.id, 0, 0), suffix, song.size)
    } else {
        (client.stream_url(&song.id, rate, 0), "mp3".to_string(), song.duration as u64 * rate as u64 * 125)
    };
    let stem: String = song.id.chars().filter(|c| c.is_ascii_alphanumeric() || "-_".contains(*c)).collect();
    if stem.is_empty() {
        return Err(FetchFailure::failed(FaultShape::UnexpectedAnswer, "unexpected song id"));
    }
    let file = format!("{stem}.{extension}");
    // Each go has a file of its own to arrive in, never another's, whatever account or turn
    // it was asked for in (W7c item 5): numbered by this process, not by the list.
    static STAGED: AtomicU64 = AtomicU64::new(1);
    let _ = attempt;
    let temp = dir.join(format!("{file}.{}.part", STAGED.fetch_add(1, Ordering::Relaxed)));
    let (mut body, length) = client.open_stream(&url, None).map_err(|e| if client.refused() || client.retired() { FetchFailure::Held } else { FetchFailure::failed(diagnostics::connection_shape(&e), e) })?;
    let expected = length.filter(|n| *n > 0).unwrap_or(expected).max(1);
    let mut out = fs::File::create(&temp).map_err(|e| FetchFailure::failed(FaultShape::Write, e))?;
    let mut buf = vec![0u8; 128 * 1024];
    let mut bytes = 0u64;
    let copied = loop {
        if cancel.load(Ordering::Relaxed) {
            break Err(FetchFailure::Cancelled);
        }
        match body.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                if let Err(e) = out.write_all(&buf[..n]) {
                    break Err(FetchFailure::failed(FaultShape::Write, e));
                }
                bytes += n as u64;
                // Held just short of full until the file is in place.
                progress.store((bytes * 1000 / expected).min(990) as u32, Ordering::Relaxed);
            }
            Err(e) => break Err(FetchFailure::failed(FaultShape::Read, e)),
        }
    };
    drop(out);
    let copied = copied.and_then(|()| match bytes {
        0 => Err(FetchFailure::failed(FaultShape::UnexpectedAnswer, "The server sent nothing.")),
        // Cancelled as the last of it arrived: it isn't wanted, whole or not.
        _ if cancel.load(Ordering::Relaxed) => Err(FetchFailure::Cancelled),
        _ => Ok(()),
    });
    if let Err(reason) = copied {
        let _ = fs::remove_file(&temp);
        return Err(reason);
    }
    let created = publish(&temp, &dir.join(&file)).map_err(|e| FetchFailure::failed(FaultShape::Write, e))?;
    Ok(Fetched { entry: Entry { song: song.clone(), file, bytes, at: chrono::Utc::now().timestamp() }, created })
}

/// What a fetch left in the account's folder: the entry, and whether this go made its file
/// (`false`: a file of that name was there already, and was left as it was).
struct Fetched {
    entry: Entry,
    created: bool,
}

/// Gives the staged file its name, **never over a file that has it** (W7c item 5). Says
/// whether it did; if the name was taken, the staged file goes and the one there stays.
fn publish(staged: &Path, name: &Path) -> std::io::Result<bool> {
    match crate::config::place_new(staged, name) {
        Ok(()) => Ok(true),
        Err(_) if name.exists() => {
            let _ = fs::remove_file(staged);
            Ok(false)
        }
        // A drive that can't make a second name for a file: moved, unless the name is taken.
        Err(error) if error.kind() == std::io::ErrorKind::Unsupported => {
            fs::rename(staged, name)?;
            Ok(true)
        }
        Err(error) => {
            let _ = fs::remove_file(staged);
            Err(error)
        }
    }
}

impl Controller {
    /// Saves these songs on this PC. Ones already saved or on their way are skipped.
    pub fn download_songs(self: &Rc<Self>, songs: Vec<Song>) {
        self.download_songs_with(songs, false);
    }

    /// The id under which the page on screen is kept downloaded, if it's a playlist.
    fn page_collection(&self) -> Option<String> {
        let page = self.page.borrow();
        match &page.playlist {
            Some(playlist) => Some(playlist.id.clone()),
            None if page.liked_songs => Some("liked".into()),
            None => None,
        }
    }

    /// Marks a playlist as kept downloaded (or no longer).
    pub fn keep_downloaded(&self, id: &str, keep: bool) {
        self.downloads.borrow_mut().keep_collection(id, keep);
    }

    /// Downloads whatever has been added to the playlists that are kept downloaded.
    pub fn sync_collections(self: &Rc<Self>) {
        if self.is_offline() {
            return;
        }
        let collections = self.downloads.borrow().collections.clone();
        for id in collections {
            self.request(
                move |client| {
                    if id == "liked" { client.starred().map(|found| found.songs) } else { client.playlist(&id).map(|p| p.songs) }
                },
                |ctl, result| {
                    if let Ok(songs) = result {
                        ctl.download_songs_with(songs, true);
                    }
                },
            );
        }
    }

    /// Downloads these quietly: what a refusal held, asked for again.
    pub fn download_songs_with_quiet(self: &Rc<Self>, songs: Vec<Song>) {
        self.download_songs_with(songs, true);
    }

    /// `quiet` skips the messages: used when topping up in the background.
    fn download_songs_with(self: &Rc<Self>, songs: Vec<Song>, quiet: bool) {
        let Some(client) = self.client() else { return };
        if self.moving_downloads.get() {
            if !quiet {
                self.toast("The downloads are being moved to another folder. Try again when that's done.");
            }
            return;
        }
        let quality = self.settings.borrow().download_quality;
        let started: Vec<(Song, u64, Arc<AtomicU32>, Arc<AtomicBool>)> = {
            let mut downloads = self.downloads.borrow_mut();
            let mut seen = HashSet::new();
            let wanted: Vec<Song> = songs
                .into_iter()
                .filter(|s| {
                    s.radio_url.is_empty() && !downloads.has(&s.id) && !downloads.queued(&s.id) && seen.insert(s.id.clone())
                })
                .collect();
            // Is there room? Going by what the server says each file is, or for a smaller
            // copy what its length works out at, with something left over for the drive.
            let needed: u64 = wanted.iter().map(|song| expected_bytes(song, quality)).sum();
            if let Some(free) = free_space(&downloads.dir).filter(|free| needed + SPARE > *free) {
                drop(downloads);
                diagnostics::fault(FaultPlace::Downloads, FaultShape::Write);
                if !quiet {
                    self.toast(format!(
                        "Not enough room: that needs about {}, and the drive your downloads are on has {} free. (Half a gigabyte is always left spare.)",
                        size_label(needed),
                        size_label(free)
                    ));
                }
                return;
            }
            if downloads.jobs.is_empty() {
                downloads.batch_total = 0;
                downloads.batch_done = 0;
                downloads.batch_failed = 0;
            }
            downloads.batch_total += wanted.len();
            wanted
                .into_iter()
                .map(|song| {
                    downloads.next_attempt += 1;
                    let job = Job {
                        song: song.clone(),
                        attempt: downloads.next_attempt,
                        progress: Arc::new(AtomicU32::new(0)),
                        cancel: Arc::new(AtomicBool::new(false)),
                        login: client.handed(),
                    };
                    // Asked for again: it's no longer among the ones that failed.
                    downloads.failed.retain(|(failed, _)| failed.id != song.id);
                    let handles = (song, job.attempt, job.progress.clone(), job.cancel.clone());
                    downloads.under_way.insert(job.song.id.clone(), job.progress.clone());
                    downloads.jobs.push(job);
                    handles
                })
                .collect()
        };
        if started.is_empty() {
            if !quiet {
                self.toast("Already downloaded");
            }
            return;
        }
        if !quiet {
            self.toast(match started.len() {
                1 => format!("Downloading “{}”", started[0].0.title),
                n => format!("Downloading {n} songs"),
            });
        }
        let (dir, covers) = (self.downloads.borrow().dir.clone(), self.paths.covers.clone());
        // Whose these are: a result that comes back after the account has changed is not
        // taken for the next one's.
        let turn = self.turn.get();
        for (song, attempt, progress, cancel) in started {
            // Show it as on its way straight off.
            self.mark_download(&song.id);
            let (client, dir, covers) = (client.clone(), dir.clone(), covers.clone());
            let dir = dir.clone();
            self.download_pool.spawn(move || {
                let result = if cancel.load(Ordering::Relaxed) {
                    Err(FetchFailure::Cancelled)
                } else {
                    fetch(&client, &dir, &song, quality, attempt, &progress, &cancel)
                };
                if result.is_ok() && !song.cover_art.is_empty() {
                    // Have the artwork on disk too, for when the server can't be reached.
                    let _ = crate::covers::ensure_file(&client, &covers, &song.cover_art);
                }
                let id = song.id.clone();
                post(move |ctl| {
                    if ctl.turn.get() == turn {
                        ctl.download_finished(&id, attempt, result);
                    } else if let Ok(fetched) = result {
                        // Arrived for an account's turn that is over: the file goes, if this go
                        // made it and no list of that folder's has or wants it now.
                        ctl.drop_unwanted(&dir, fetched);
                    }
                });
            });
        }
        self.sync_downloads();
    }

    fn download_finished(self: &Rc<Self>, id: &str, attempt: u64, result: Result<Fetched, FetchFailure>) {
        let mut again = None;
        let mut unwanted = None;
        let finished = {
            let mut downloads = self.downloads.borrow_mut();
            // No longer listed means this go was cancelled while it was fetching. (The song
            // may have been asked for again since: that go is listed, and left alone.)
            let wanted = downloads.jobs.iter().position(|job| job.song.id == id && job.attempt == attempt).map(|at| downloads.jobs.remove(at));
            if wanted.is_some() {
                downloads.under_way.remove(id);
            }
            match (result, wanted) {
                (Ok(fetched), Some(_)) => {
                    downloads.batch_done += 1;
                    downloads.add(fetched.entry);
                }
                // A go that was cancelled finished anyway. Its file goes, if this go made it
                // and the song has not been asked for again or downloaded since.
                (Ok(fetched), None) => unwanted = Some(fetched),
                (Err(FetchFailure::Held), Some(job)) => {
                    // Not failed: asked for again, once, when the login is renewed; at once if
                    // it already has been (it missed the renewal's edge: W7c item 4).
                    downloads.batch_total = downloads.batch_total.saturating_sub(1);
                    again = downloads.hold(job.song, job.login, self.client().as_ref());
                }
                (Err(failure), Some(job)) => {
                    let reason = match failure {
                        FetchFailure::Cancelled => "cancelled".into(),
                        FetchFailure::Held => unreachable!(),
                        FetchFailure::Failed { shape, reason } => {
                            diagnostics::fault(FaultPlace::Downloads, shape);
                            reason
                        }
                    };
                    downloads.batch_failed += 1;
                    downloads.failed.retain(|(failed, _)| failed.id != job.song.id);
                    downloads.failed.push((job.song, reason));
                }
                (Err(_), None) => {}
            }
            // The last of a batch: the list of downloads goes to disk now.
            let over = downloads.jobs.is_empty();
            downloads.save_if_due(over);
            (over && downloads.batch_total > 0).then_some((downloads.batch_done, downloads.batch_failed))
        };
        if let Some(fetched) = unwanted {
            let dir = self.downloads.borrow().dir.clone();
            self.drop_unwanted(&dir, fetched);
        }
        self.mark_download(id);
        if let Some(song) = again {
            self.download_songs_with(vec![song], true);
        }
        if let Some((done, failed)) = finished {
            self.downloads.borrow_mut().batch_total = 0;
            match (done, failed) {
                (0, 0) => {}
                (1, 0) => self.toast("Download finished"),
                (_, 0) => self.toast(format!("{done} songs downloaded")),
                (0, _) => self.toast(self.downloads.borrow().trouble()),
                _ => self.toast(format!("{done} songs downloaded; {failed} couldn't be. See Downloads.")),
            }
        }
        self.sync_downloads();
    }

    /// A download that came back and is not taken: its file goes **only if this go made it**
    /// and the list of that folder in use now neither has the song nor is fetching it (W7c
    /// item 5). Removed off the window's thread.
    fn drop_unwanted(&self, dir: &Path, fetched: Fetched) {
        if !fetched.created {
            return;
        }
        let id = &fetched.entry.song.id;
        let theirs = {
            let downloads = self.downloads.borrow();
            downloads.dir == dir && (downloads.has(id) || downloads.queued(id))
        };
        if !theirs {
            let path = dir.join(&fetched.entry.file);
            self.download_pool.spawn(move || {
                let _ = fs::remove_file(path);
            });
        }
    }

    /// Stops these songs' downloads, whether waiting or part-way through.
    pub fn cancel_downloads(self: &Rc<Self>, ids: &[String]) {
        {
            let mut downloads = self.downloads.borrow_mut();
            let before = downloads.jobs.len();
            downloads.jobs.retain(|job| {
                let cancelled = ids.contains(&job.song.id);
                if cancelled {
                    job.cancel.store(true, Ordering::Relaxed);
                }
                !cancelled
            });
            for id in ids {
                downloads.under_way.remove(id);
            }
            downloads.forget_held(ids);
            let over = downloads.jobs.is_empty();
            downloads.save_if_due(over);
            let cancelled = before - downloads.jobs.len();
            downloads.batch_total = downloads.batch_total.saturating_sub(cancelled);
            if downloads.jobs.is_empty() {
                downloads.batch_total = 0;
            }
        }
        for id in ids {
            self.mark_download(id);
        }
        self.sync_downloads();
    }

    /// Deletes these songs' saved copies, and stops any that are still on their way.
    pub fn remove_downloads(self: &Rc<Self>, ids: &[String]) {
        self.downloads.borrow_mut().remove_many(ids);
        self.cancel_downloads(ids);
    }

    /// Moves the signed-in person's downloads into `chosen` (a folder of their own inside it:
    /// `account_dir`) and keeps them there from now on. **Only theirs** (W7c item 5): their
    /// list, their playlists kept downloaded and the files their list names; every other
    /// account's stay where they are. What it comes to is kept for the account it was begun
    /// for, whoever is signed in when it ends; it is shown only if that account is still in.
    pub fn move_downloads(self: &Rc<Self>, chosen: PathBuf) {
        let owner = self.account();
        if owner.is_empty() {
            return;
        }
        let legacy = self.legacy_folder();
        let from = self.downloads.borrow().dir.clone();
        let folder = {
            let mut settings = self.settings.borrow().clone();
            settings.downloads_dirs.insert(owner.clone(), chosen.display().to_string());
            account_dir(&settings, &self.paths.downloads, &owner, legacy)
        };
        if folder == from || chosen == from {
            return;
        }
        if !self.downloads.borrow().jobs.is_empty() || self.moving_downloads.get() {
            return self.toast("Wait for the downloads under way to finish, then choose the folder again.");
        }
        if fs::create_dir_all(&folder).is_err() || !folder.is_dir() {
            diagnostics::fault(FaultPlace::Downloads, FaultShape::Write);
            return self.toast(format!("{} can't be used as a folder.", chosen.display()));
        }
        // The list as it stands, on the disk, before anything is decided from it.
        if let Err(said) = self.downloads.borrow_mut().ready_to_move() {
            diagnostics::fault(FaultPlace::Downloads, FaultShape::Write);
            return self.toast(said);
        }
        let plan = match plan_for(&from, &owner) {
            Ok(plan) => plan,
            Err(said) => {
                diagnostics::fault(FaultPlace::Downloads, FaultShape::Read);
                return self.toast(said);
            }
        };
        // Room for what moves, unless it's the same drive (when nothing is copied).
        if let Some(free) = free_space(&folder).filter(|free| !same_drive(&from, &folder) && plan.bytes + SPARE > *free) {
            diagnostics::fault(FaultPlace::Downloads, FaultShape::Write);
            return self.toast(format!(
                "Not enough room there: the downloads come to {}, and that drive has {} free. (Half a gigabyte is always left spare.)",
                size_label(plan.bytes),
                size_label(free)
            ));
        }
        self.moving_downloads.set(true);
        self.toast(format!("Moving your downloads to {}…", folder.display()));
        let turn = self.turn.get();
        std::thread::spawn(move || {
            let moved = move_folder_by(&from, &folder, &plan, &MOVING);
            post(move |ctl| {
                ctl.moving_downloads.set(false);
                // Still the turn the move was begun in: what it comes to is shown.
                let shown = ctl.turn.get() == turn && ctl.account() == owner;
                match moved {
                    Moved::All(_) => {
                        diagnostics::observe(Line::Note(Note::DownloadsFolderChanged));
                        // For the account it was begun for alone, whoever is in now.
                        ctl.settings.borrow_mut().downloads_dirs.insert(owner.clone(), chosen.display().to_string());
                        let saved = ctl.settings.borrow().save_surely(&ctl.paths.settings);
                        if !shown {
                            return;
                        }
                        *ctl.downloads.borrow_mut() = Downloads::load_for(folder.clone(), &owner, legacy);
                        if saved {
                            ctl.toast(format!("Your downloads are now in {}", folder.display()));
                        } else {
                            diagnostics::fault(FaultPlace::Downloads, FaultShape::Write);
                            ctl.toast(format!(
                                "Your downloads have moved to {}, but the setting that says so couldn't be saved. If they aren't shown after the app next starts, choose that folder again under Settings, Storage.",
                                folder.display()
                            ));
                        }
                    }
                    _ if !shown => return,
                    Moved::Taken(taken) => {
                        diagnostics::fault(FaultPlace::Downloads, FaultShape::Write);
                        ctl.toast(if taken == 1 {
                            format!("A file with the same name as one of your downloads is already in {}, so nothing was moved. Choose an empty folder, or move it out of that one first.", folder.display())
                        } else {
                            format!("{taken} files with the same names as your downloads are already in {}, so nothing was moved. Choose an empty folder, or move those out of it first.", folder.display())
                        });
                    }
                    Moved::Back { stuck, of } => {
                        diagnostics::fault(FaultPlace::Downloads, FaultShape::Write);
                        *ctl.downloads.borrow_mut() = Downloads::load_for(from, &owner, legacy);
                        ctl.toast(format!("{stuck} of {of} files couldn't be moved, so the downloads are staying where they were."));
                    }
                    Moved::Split { left } => {
                        diagnostics::fault(FaultPlace::Downloads, FaultShape::Write);
                        *ctl.downloads.borrow_mut() = Downloads::load_for(from.clone(), &owner, legacy);
                        ctl.toast(format!(
                            "Some files couldn't be moved, and {left} of those that had been couldn't be brought back: they are in {}. The rest are still in {}, where the app goes on keeping your downloads.",
                            folder.display(),
                            from.display()
                        ));
                    }
                }
                ctl.sync_downloads();
                ctl.rebuild_tracks();
            });
        });
    }

    /// Has another go at the songs that couldn't be downloaded.
    pub fn retry_downloads(self: &Rc<Self>) {
        let songs: Vec<Song> = self.downloads.borrow().failed.iter().map(|(song, _)| song.clone()).collect();
        if !songs.is_empty() {
            self.download_songs(songs);
        }
    }

    pub fn remove_all_downloads(self: &Rc<Self>) {
        let ids: Vec<String> = self.downloads.borrow().songs().into_iter().map(|song| song.id).collect();
        self.downloads.borrow_mut().forget_collections();
        self.remove_downloads(&ids);
        self.toast("Downloads removed");
    }

    /// Brings every row for this song up to date with whether it's downloaded or on its way.
    fn mark_download(&self, id: &str) {
        let (downloaded, progress) = {
            let downloads = self.downloads.borrow();
            (downloads.has(id), downloads.progress_of(id))
        };
        let unavailable = self.is_offline() && !downloaded;
        self.each_row_of(id, |track| {
            let changed = track.downloaded != downloaded || track.progress != progress || track.unavailable != unavailable;
            track.downloaded = downloaded;
            track.progress = progress;
            track.unavailable = unavailable;
            changed
        });
    }

    /// Runs a few times a second while songs are downloading: moves the progress bars along.
    pub fn tick_downloads(self: &Rc<Self>) {
        let ids: Vec<String> = {
            let mut downloads = self.downloads.borrow_mut();
            downloads.save_if_due(false);
            if downloads.jobs.is_empty() {
                return;
            }
            // Only the ones under way can have moved.
            downloads.jobs.iter().filter(|job| job.progress.load(Ordering::Relaxed) > 0).map(|job| job.song.id.clone()).collect()
        };
        for id in ids {
            self.mark_download(&id);
        }
        self.sync_download_button();
        self.sync_download_progress();
    }

    /// Shows the overall progress on the taskbar button.
    fn sync_download_progress(&self) {
        #[cfg(windows)]
        {
            let downloads = self.downloads.borrow();
            crate::taskbar::set_progress((!downloads.jobs.is_empty()).then(|| downloads.batch_progress()));
        }
    }

    /// Refreshes everything that summarises the downloads: the sidebar line, Settings, the
    /// page's download button, and the Downloads page itself when it's showing.
    pub fn sync_downloads(self: &Rc<Self>) {
        let downloads = self.downloads.borrow();
        let status = if downloads.jobs.is_empty() {
            String::new()
        } else {
            let at = (downloads.batch_done + downloads.batch_failed + 1).min(downloads.batch_total);
            format!("Downloading {at} of {}", downloads.batch_total)
        };
        self.ui.global::<ui::App>().set_download_status(status.into());
        let settings = self.ui.global::<ui::Settings>();
        settings.set_downloads_summary(downloads.summary().into());
        settings.set_downloads_folder(downloads.dir.display().to_string().into());
        settings.set_downloads_folder_custom(downloads.dir != account_dir(&crate::config::Settings::default(), &self.paths.downloads, &downloads.account, self.legacy_folder()));
        drop(downloads);
        self.sync_download_progress();
        if matches!(self.current_route(), Some(Route::Downloads)) {
            let scroll = self.ui.global::<ui::View>().get_scroll_y();
            self.show_downloads(scroll);
        } else {
            self.sync_download_button();
        }
    }

    /// The download button on an album or playlist: 0 nothing saved, 1 in progress, 2 all saved.
    pub fn sync_download_button(&self) {
        let page = self.page.borrow();
        let downloads = self.downloads.borrow();
        let view = self.ui.global::<ui::View>();
        if matches!(self.current_route(), Some(Route::Downloads)) {
            // On the Downloads page the button shows the whole batch, and cancels it.
            let busy = !downloads.jobs.is_empty();
            view.set_can_download(busy);
            view.set_download_state(if busy { 1 } else { 0 });
            view.set_download_done((downloads.batch_done + downloads.batch_failed) as i32);
            view.set_download_total(downloads.batch_total as i32);
            view.set_download_progress(downloads.batch_progress());
            return;
        }
        let offered = !page.songs.is_empty()
            && (page.playlist.is_some() || matches!(self.current_route(), Some(Route::Album(_)) | Some(Route::Liked)));
        view.set_can_download(offered);
        let saved = page.songs.iter().filter(|s| downloads.has(&s.id)).count();
        let partial: f32 = page.songs.iter().map(|s| downloads.progress_of(&s.id).max(0.0)).sum();
        let busy = page.songs.iter().any(|s| downloads.queued(&s.id));
        view.set_download_state(if busy { 1 } else if saved == page.songs.len() && saved > 0 { 2 } else { 0 });
        view.set_download_done(saved as i32);
        view.set_download_total(page.songs.len() as i32);
        view.set_download_progress((saved as f32 + partial) / page.songs.len().max(1) as f32);
    }

    /// The button was pressed: cancel what's under way, download what's missing, or remove it
    /// all if nothing is.
    pub fn toggle_page_download(self: &Rc<Self>) {
        if matches!(self.current_route(), Some(Route::Downloads)) {
            let ids: Vec<String> = self.downloads.borrow().jobs.iter().map(|job| job.song.id.clone()).collect();
            self.cancel_downloads(&ids);
            return self.toast("Downloads cancelled");
        }
        let songs = self.page.borrow().songs.clone();
        let collection = self.page_collection();
        let (all_saved, waiting) = {
            let downloads = self.downloads.borrow();
            let waiting: Vec<String> =
                songs.iter().filter(|s| downloads.queued(&s.id)).map(|s| s.id.clone()).collect();
            (songs.iter().all(|s| downloads.has(&s.id)), waiting)
        };
        if !waiting.is_empty() {
            if let Some(id) = &collection {
                self.keep_downloaded(id, false);
            }
            self.cancel_downloads(&waiting);
            self.toast("Download cancelled");
        } else if all_saved {
            let ids: Vec<String> = songs.iter().map(|s| s.id.clone()).collect();
            if let Some(id) = &collection {
                self.keep_downloaded(id, false);
            }
            self.remove_downloads(&ids);
            self.toast("Removed from downloads");
        } else {
            // A playlist downloaded as a whole stays that way as songs are added to it.
            if let Some(id) = &collection {
                self.keep_downloaded(id, true);
            }
            self.download_songs(songs);
        }
    }

    /// Called when a page has loaded: if it's a playlist that's kept downloaded, fetch any of
    /// its songs that aren't saved yet.
    pub fn top_up_page(self: &Rc<Self>) {
        let Some(id) = self.page_collection() else { return };
        if self.is_offline() || !self.downloads.borrow().collections.contains(&id) {
            return;
        }
        let songs = self.page.borrow().songs.clone();
        self.download_songs_with(songs, true);
    }

    /// Builds the Downloads page from what's on disk and what's on its way; no server involved.
    pub fn show_downloads(self: &Rc<Self>, scroll: f32) {
        let (songs, summary, waiting) = {
            let downloads = self.downloads.borrow();
            (downloads.songs(), downloads.summary(), downloads.jobs.len())
        };
        let meta = match (songs.is_empty(), waiting) {
            (true, _) => String::new(),
            (false, 0) => summary,
            (false, 1) => format!("{summary}, and 1 more on its way"),
            (false, n) => format!("{summary}, and {n} more on their way"),
        };
        let trouble = self.downloads.borrow().trouble();
        let page = PageData {
            label: "On this PC".into(),
            title: "Downloads".into(),
            meta,
            notice_action: if trouble.is_empty() { String::new() } else { "Retry failed".into() },
            notice: trouble,
            cover: songs.first().map(|s| s.cover_art.clone()).unwrap_or_default(),
            can_play: !songs.is_empty(),
            show_album: true,
            show_cover: true,
            show_number: true,
            songs,
            context: "Downloads".into(),
            play: PlaySource::Songs,
            empty: "Songs you download are kept here, ready to play even when your server can't be reached. Use the download button on an album or playlist, or a song's menu.".into(),
            ..Default::default()
        };
        self.apply_page(page, scroll);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nexus-downloads-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(id: &str, file: &str, bytes: u64) -> Entry {
        Entry { song: Song { id: id.into(), title: id.to_uppercase(), suffix: "flac".into(), duration: 100, ..Default::default() }, file: file.into(), bytes, at: 5 }
    }

    /// W7c item 5: two accounts given the same place never share a file name (each has a
    /// folder of its own in it); the carried-over folder keeps 0.40's place. A file is never
    /// put over one that has its name. A move plans the signed-in account's files alone.
    #[test]
    fn downloads_are_each_accounts_own_whatever_place_is_chosen() {
        let place = folder("place");
        let default = folder("default");
        let mut settings = crate::config::Settings::default();
        for who in ["nexus-7-aa", "nexus-8-bb"] {
            settings.downloads_dirs.insert(who.into(), place.display().to_string());
        }
        let (a, b) = (account_dir(&settings, &default, "nexus-7-aa", false), account_dir(&settings, &default, "nexus-8-bb", false));
        assert_ne!(a, b);
        assert!(a.starts_with(&place) && b.starts_with(&place) && a != place && b != place);
        assert_eq!(account_dir(&settings, &default, "legacy-stem", true), default);
        assert_eq!(account_dir(&crate::config::Settings::default(), &default, "nexus-9-cc", false), default.join("nexus-9-cc"));
        // Published, never over what has the name: a late go leaves the file there as it was.
        fs::create_dir_all(&a).unwrap();
        fs::write(a.join("1.flac"), b"theirs").unwrap();
        fs::write(a.join("1.flac.5.part"), b"late").unwrap();
        assert!(!publish(&a.join("1.flac.5.part"), &a.join("1.flac")).unwrap());
        assert_eq!(fs::read(a.join("1.flac")).unwrap(), b"theirs");
        assert!(!a.join("1.flac.5.part").exists());
        fs::write(a.join("2.flac.6.part"), b"mine").unwrap();
        assert!(publish(&a.join("2.flac.6.part"), &a.join("2.flac")).unwrap());
        // A move of one account's: its list and files only.
        let shared = folder("shared");
        fs::write(index_file(&shared, "x"), list(&[entry("1", "1.flac", 6)])).unwrap();
        fs::write(index_file(&shared, "y"), list(&[entry("2", "2.flac", 4)])).unwrap();
        fs::write(shared.join("1.flac"), b"xxxxxx").unwrap();
        fs::write(shared.join("2.flac"), b"yyyy").unwrap();
        let plan = plan_for(&shared, "x").unwrap();
        let names: Vec<String> = plan.names.iter().map(|name| name.to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["1.flac".to_string(), "index-x.json".to_string()]);
        for dir in [place, default, shared] {
            let _ = fs::remove_dir_all(dir);
        }
    }

    /// W7c item 4: a download held under an older login than the one in use is handed back
    /// to be asked for at once; under the current one (or with nothing newer that may send)
    /// it waits for the renewal; cancelled, it is not asked for again.
    #[test]
    fn a_download_held_under_an_older_login_is_asked_for_again_at_once() {
        let dir = folder("held");
        let mut downloads = Downloads::load(dir.clone(), "a");
        let song = |id: &str| Song { id: id.into(), ..Song::default() };
        let current = Client::new("http://127.0.0.1:9", "a", "p");
        current.handed_as((2, 1));
        assert_eq!(downloads.hold(song("old"), (1, 1), Some(&current)).map(|song| song.id), Some("old".into()));
        assert!(downloads.hold(song("now"), (2, 1), Some(&current)).is_none());
        current.set_aside();
        assert!(downloads.hold(song("aside"), (1, 1), Some(&current)).is_none(), "nothing newer that may send: it waits");
        downloads.forget_held(&["now".to_string()]);
        let held: Vec<String> = downloads.take_held().into_iter().map(|song| song.id).collect();
        assert_eq!(held, vec!["aside".to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn each_account_has_its_own_list() {
        let dir = folder("accounts");
        // A list from before accounts had their own, with one song whose file is there.
        fs::write(dir.join("a.flac"), b"audio").unwrap();
        fs::write(dir.join("index.json"), serde_json::to_vec(&vec![entry("a", "a.flac", 5)]).unwrap()).unwrap();
        fs::write(dir.join("collections.json"), br#"["liked"]"#).unwrap();
        // Nobody signed in: nothing shown, and the old list is left for whoever signs in.
        assert!(Downloads::load(dir.clone(), "").is_empty());
        assert!(dir.join("index.json").exists());
        // The first to sign in gets it.
        let mut first = Downloads::load(dir.clone(), "server_ann");
        assert!(first.has("a") && first.collections == ["liked"]);
        assert!(!dir.join("index.json").exists() && dir.join("index-server_ann.json").exists());
        // Someone else starts with none, and what they download is theirs alone.
        let mut second = Downloads::load(dir.clone(), "server_bob");
        assert!(second.is_empty() && second.collections.is_empty());
        fs::write(dir.join("b.mp3"), b"audio").unwrap();
        second.add(entry("b", "b.mp3", 5));
        second.save_if_due(true);
        first.save_if_due(true);
        assert!(Downloads::load(dir.clone(), "server_bob").has("b"));
        assert!(!Downloads::load(dir.clone(), "server_ann").has("b"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn says_what_a_download_was_saved_as_and_what_went_wrong() {
        let dir = folder("saved");
        fs::write(dir.join("a.flac"), b"x").unwrap();
        fs::write(dir.join("b.mp3"), b"x").unwrap();
        let mut downloads = Downloads::load(dir.clone(), "someone");
        // A hundred seconds of audio in 1,600,000 bytes is 128 kbps.
        downloads.add(entry("a", "a.flac", 12_000_000));
        downloads.add(entry("b", "b.mp3", 1_600_000));
        let song = |id: &str| entry(id, "", 0).song;
        assert_eq!(downloads.saved_as(&song("a")), Some(("flac".to_string(), 960)));
        assert_eq!(downloads.saved_as(&song("b")), Some(("mp3".to_string(), 128)));
        assert_eq!(downloads.saved_as(&song("c")), None);

        assert_eq!(downloads.trouble(), "");
        downloads.failed.push((song("x"), "Can't reach the server.".into()));
        assert_eq!(downloads.trouble(), "“X” couldn't be downloaded. Can't reach the server.");
        downloads.failed.push((song("y"), "The disk is full.".into()));
        downloads.failed.push((song("z"), "The disk is full.".into()));
        assert_eq!(downloads.trouble(), "“Z” and 2 other songs couldn't be downloaded. The disk is full.");
        let _ = fs::remove_dir_all(dir);
    }

    fn list(entries: &[Entry]) -> Vec<u8> {
        serde_json::to_vec(entries).unwrap()
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        names
    }

    /// A downloads folder of the person's: the app's list and songs, and files of theirs.
    fn their_folder(name: &str) -> PathBuf {
        // (One down, so that what `..` names is a file of the test's own.)
        let from = folder(name).join("dl");
        fs::create_dir_all(from.join("sub")).unwrap();
        // Named in the list with a folder in them, and there: never followed.
        fs::write(from.join("../c.flac"), b"outside").unwrap();
        fs::write(from.join("sub/d.flac"), b"below").unwrap();
        fs::write(from.join("index-someone.json"), list(&[entry("a", "a.flac", 3), entry("b", "b.flac", 3), entry("c", "../c.flac", 3), entry("d", "sub/d.flac", 3)])).unwrap();
        fs::write(from.join("collections-someone.json"), br#"["liked"]"#).unwrap();
        fs::write(from.join("a.flac"), b"one").unwrap();
        fs::write(from.join("b.flac"), b"two").unwrap();
        // Theirs: named like the app's, and a list of strings with no list of downloads beside it.
        for (name, text) in [("index-tax.tmp", "theirs"), ("index-tax.json", "theirs too"), ("collections-tax.json", r#"["a list of theirs"]"#), ("notes.txt", "and theirs")] {
            fs::write(from.join(name), text).unwrap();
        }
        from
    }

    const THEIRS: [&str; 5] = ["collections-tax.json", "index-tax.json", "index-tax.tmp", "notes.txt", "sub"];
    const APPS: [&str; 4] = ["a.flac", "b.flac", "collections-someone.json", "index-someone.json"];

    #[test]
    fn the_move_is_decided_before_anything_moves_and_takes_only_the_apps_files() {
        let from = their_folder("plan");
        let planned: Vec<String> = plan(&from).names.into_iter().map(|name| name.to_string_lossy().into_owned()).collect();
        // A song named with a folder in it, or `..`, isn't followed; nor is a list of theirs.
        assert_eq!(planned, APPS);
        let to = folder("plan-to");
        assert_eq!(move_folder(&from, &to), Moved::All(4));
        assert_eq!(names(&to), APPS);
        assert_eq!(names(&from), THEIRS);
        let _ = fs::remove_dir_all(from.parent().unwrap());
        let _ = fs::remove_dir_all(to);
    }

    #[test]
    fn a_name_taken_in_the_new_folder_stops_the_move_before_it_starts() {
        // The new folder was a downloads folder once: its own list has the same name. Nothing
        // moves, and nothing in either folder is touched.
        let from = their_folder("taken");
        let to = folder("taken-to");
        fs::write(to.join("index-someone.json"), list(&[entry("z", "z.flac", 3)])).unwrap();
        fs::write(to.join("z.flac"), b"theirs, listed there").unwrap();
        let before = (names(&from), names(&to));
        assert_eq!(move_folder(&from, &to), Moved::Taken(1));
        assert_eq!((names(&from), names(&to)), before);
        let _ = fs::remove_dir_all(from.parent().unwrap());
        let _ = fs::remove_dir_all(to);
    }

    #[test]
    fn what_moved_is_put_back_by_the_moves_own_list_when_a_file_cant_move() {
        // b.flac can't be moved, by rename or by copy: what moved comes back, and what was in
        // the new folder before stays there.
        let from = their_folder("back");
        let to = folder("back-to");
        // The new folder's own: a list of another account's, and the song it names. Put back
        // by looking at that folder, they would be taken too.
        fs::write(to.join("index-other.json"), list(&[entry("t", "theirs.flac", 3)])).unwrap();
        fs::write(to.join("theirs.flac"), b"in the new folder already").unwrap();
        let stuck = MoveSteps {
            rename: |from, to| if from.ends_with("b.flac") { Err(std::io::Error::other("no")) } else { fs::rename(from, to) },
            flush: |_| Err(std::io::Error::other("no")),
            ..MOVING
        };
        let before = names(&from);
        assert_eq!(move_folder_by(&from, &to, &plan(&from), &stuck), Moved::Back { stuck: 1, of: 4 });
        assert_eq!(names(&from), before);
        assert_eq!(names(&to), ["index-other.json", "theirs.flac"]);
        // And one that can't come back either: said, with how many are left where.
        let split = MoveSteps {
            rename: |from, to| {
                let back = to.to_string_lossy().contains("split-from");
                if from.ends_with("b.flac") || (back && to.ends_with("a.flac")) { Err(std::io::Error::other("no")) } else { fs::rename(from, to) }
            },
            flush: |_| Err(std::io::Error::other("no")),
            ..MOVING
        };
        let (from, to) = (their_folder("split-from"), folder("split-to"));
        assert_eq!(move_folder_by(&from, &to, &plan(&from), &split), Moved::Split { left: 1 });
        assert!(to.join("a.flac").exists() && from.join("b.flac").exists() && from.join("index-someone.json").exists());
        for dir in ["back", "back-to", "split-from", "split-to"] {
            let _ = fs::remove_dir_all(folder(dir));
        }
    }

    #[test]
    fn across_drives_a_song_arrives_whole_or_not_at_all() {
        // Another drive: copied. Killed once the copy is written and before it has its name:
        // nothing at the song's name in the new folder, and the song whole in the old.
        let from = their_folder("copy");
        let to = folder("copy-to");
        let killed = MoveSteps { rename: |_, _| Err(std::io::Error::other("another drive")), flush: |_| panic!("killed"), ..MOVING };
        let planned = plan(&from);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| move_folder_by(&from, &to, &planned, &killed))).is_err());
        assert!(!to.join("a.flac").exists() && fs::read(from.join("a.flac")).unwrap() == b"one");
        // What it was copying into is there, under a temporary name of the app's, never the song's.
        assert!(!names(&to).is_empty() && names(&to).iter().all(|name| name.starts_with("a.flac.") && name.ends_with(".tmp")), "{:?}", names(&to));
        // Not killed: each copied whole, and gone from the old folder.
        let (from, to) = (their_folder("copy2"), folder("copy2-to"));
        let copying = MoveSteps { rename: |_, _| Err(std::io::Error::other("another drive")), ..MOVING };
        assert_eq!(move_folder_by(&from, &to, &plan(&from), &copying), Moved::All(4));
        assert_eq!(fs::read(to.join("a.flac")).unwrap(), b"one");
        assert_eq!(names(&from), THEIRS);
        for dir in ["copy", "copy-to", "copy2", "copy2-to"] {
            let _ = fs::remove_dir_all(folder(dir));
        }
    }

    #[test]
    fn a_temporary_name_that_is_taken_is_passed_over_and_left_as_it_is() {
        // Across drives, the first name a copy would be made under is had already, by a file
        // of whoever's: it isn't touched, the next is used, and the move goes through.
        thread_local!(static COUNT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) });
        let (from, to) = (their_folder("taken-temp"), folder("taken-temp-to"));
        fs::write(to.join("a.flac.999.0.tmp"), b"not the app's").unwrap();
        let counted = MoveSteps {
            rename: |_, _| Err(std::io::Error::other("another drive")),
            temp: |to| {
                let count = COUNT.get();
                COUNT.set(count + 1);
                to.with_file_name(format!("{}.999.{count}.tmp", to.file_name().unwrap().to_string_lossy()))
            },
            ..MOVING
        };
        assert_eq!(move_folder_by(&from, &to, &plan(&from), &counted), Moved::All(4));
        assert_eq!(fs::read(to.join("a.flac.999.0.tmp")).unwrap(), b"not the app's");
        assert_eq!(names(&to), ["a.flac", "a.flac.999.0.tmp", "b.flac", "collections-someone.json", "index-someone.json"]);
        let _ = fs::remove_dir_all(folder("taken-temp"));
        let _ = fs::remove_dir_all(to);
    }

    #[test]
    fn a_move_never_writes_over_a_list_it_couldnt_read_nor_rewrites_a_clean_one() {
        let dir = folder("unread-list");
        fs::write(dir.join("index-someone.json"), b"half a lis").unwrap();
        let mut downloads = Downloads::load(dir.clone(), "someone");
        assert!(downloads.ready_to_move().is_err_and(|said| said.contains("couldn't be read")));
        assert_eq!(fs::read(dir.join("index-someone.json")).unwrap(), b"half a lis");
        // A list that reads, with nothing unwritten: not written again (a rewrite would lose
        // the spaces it was written with here).
        let written = serde_json::to_vec_pretty(&[entry("a", "a.flac", 3)]).unwrap();
        fs::write(dir.join("index-someone.json"), &written).unwrap();
        fs::write(dir.join("a.flac"), b"one").unwrap();
        let mut downloads = Downloads::load(dir.clone(), "someone");
        assert!(downloads.ready_to_move().is_ok());
        assert_eq!(fs::read(dir.join("index-someone.json")).unwrap(), written);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_list_made_good_can_be_moved_and_one_that_cant_be_read_when_planned_cant() {
        // A damaged list, loaded; then a song arrives and the list is written whole again.
        let dir = folder("made-good");
        fs::write(dir.join("index-someone.json"), b"half a lis").unwrap();
        let mut downloads = Downloads::load(dir.clone(), "someone");
        fs::write(dir.join("a.flac"), b"one").unwrap();
        downloads.add(entry("a", "a.flac", 3));
        downloads.save_if_due(true);
        assert!(serde_json::from_slice::<Vec<Entry>>(&fs::read(dir.join("index-someone.json")).unwrap()).is_ok());
        assert_eq!(downloads.ready_to_move(), Ok(()), "a list written good since was still refused");
        assert_eq!(plan_for(&dir, "someone").map(|plan| plan.names.len()), Ok(2));
        // The list in use can't be read as the move is planned: refused, not "moved" with the
        // songs left where they are. Another account's that can't be read is left out, as before.
        fs::write(dir.join("index-other.json"), b"not a list").unwrap();
        assert_eq!(plan_for(&dir, "someone").map(|plan| plan.names.len()), Ok(2));
        fs::write(dir.join("index-someone.json"), b"half a lis").unwrap();
        assert!(plan_for(&dir, "someone").is_err_and(|said| said.contains("couldn't be read")));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn the_drive_is_told_without_touching_a_file_of_theirs() {
        let (from, to) = (folder("probe"), folder("probe-to"));
        fs::write(from.join("move-test.part"), "theirs").unwrap();
        fs::write(to.join("move-test.part"), "theirs too").unwrap();
        assert!(same_drive(&from, &to));
        assert_eq!((names(&from), names(&to)), (vec!["move-test.part".to_string()], vec!["move-test.part".to_string()]));
        assert_eq!((fs::read_to_string(from.join("move-test.part")).unwrap(), fs::read_to_string(to.join("move-test.part")).unwrap()), ("theirs".to_string(), "theirs too".to_string()));
        let _ = fs::remove_dir_all(from);
        let _ = fs::remove_dir_all(to);
    }

    #[test]
    fn only_the_apps_own_cut_short_downloads_are_cleared() {
        let dir = folder("partials");
        fs::write(dir.join("index-someone.json"), list(&[entry("a", "a.flac", 3)])).unwrap();
        fs::write(dir.join("a.flac"), b"one").unwrap();
        // And what a copy across drives left: only for a song the list names.
        for name in ["b-1_X.mp3.7.part", "a.flac.part", "video.mp4.part", "movie.2.part", "report.part", "two words.mp3.3.part", "a.flac.12.3.tmp", "b.flac.12.3.tmp", "a.flac.x.3.tmp"] {
            fs::write(dir.join(name), "x").unwrap();
        }
        let _ = Downloads::load(dir.clone(), "someone");
        assert_eq!(names(&dir), ["a.flac", "a.flac.x.3.tmp", "b.flac.12.3.tmp", "index-someone.json", "movie.2.part", "report.part", "two words.mp3.3.part", "video.mp4.part"]);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn on_the_windows_thread_only_a_press_waits_for_the_disk_and_once() {
        let dir = folder("window");
        crate::config::mark_window_thread();
        let flushes = crate::config::window_flushes;
        let mut downloads = Downloads::load(dir.clone(), "someone");
        let ids: Vec<String> = (0..20).map(|n| format!("s{n}")).collect();
        let before = flushes();
        // In passing: a batch arriving and ending, Recently played, the settings saved by an
        // exchange, when songs were added, the crash note's marker.
        for id in &ids {
            fs::write(dir.join(format!("{id}.flac")), b"x").unwrap();
            downloads.add(entry(id, &format!("{id}.flac"), 1));
            downloads.save_if_due(false);
        }
        downloads.save_if_due(true);
        crate::config::save_history(&dir.join("history.json"), &[]);
        crate::config::Settings::default().save_in_passing(&dir.join("settings.json"));
        crate::added::Added::load(dir.join("playlist-added.json")).record("p", &ids, 1_700_000_000);
        assert_eq!(flushes(), before, "a write in passing waited for the disk on the window's thread");
        // A press: twenty downloads removed at once, the list flushed once.
        downloads.remove_many(&ids);
        assert_eq!(flushes(), before + 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn moves_a_folder_of_downloads() {
        let (from, to) = (folder("from"), folder("to"));
        fs::write(from.join("a.flac"), b"one").unwrap();
        fs::write(from.join("index-someone.json"), serde_json::to_vec(&[entry("a", "a.flac", 3)]).unwrap()).unwrap();
        fs::write(from.join("c.flac.7.part"), b"half").unwrap();
        fs::create_dir_all(from.join("a folder")).unwrap();
        assert_eq!(move_folder(&from, &to), Moved::All(2));
        assert_eq!(fs::read(to.join("a.flac")).unwrap(), b"one");
        assert!(to.join("index-someone.json").exists() && !from.join("a.flac").exists());
        // A file still arriving stays behind, and folders aren't touched.
        assert!(from.join("c.flac.7.part").exists() && from.join("a folder").exists() && !to.join("c.flac.7.part").exists());
        assert_eq!((size_label(340 * 1024 * 1024), size_label(12_400 * 1024 * 1024)), ("340 MB".to_string(), "12.1 GB".to_string()));
        let _ = fs::remove_dir_all(from);
        let _ = fs::remove_dir_all(to);
    }
}
