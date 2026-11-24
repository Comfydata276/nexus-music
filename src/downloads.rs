//! Songs saved on this PC, so they play without the server: the files, their index, and the
//! controller's side of downloading and removing them.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use serde::{Deserialize, Serialize};
use slint::ComponentHandle;

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
    /// Whose downloads these are (see `account`): each account has its own list, in the same
    /// folder, so signing in as someone else doesn't show the last person's.
    account: String,
    next_attempt: u64,
    /// Songs that couldn't be downloaded this session, and why, newest last.
    pub failed: Vec<(Song, String)>,
    saved_at: std::time::Instant,
    /// For the progress messages: how many songs the current batch has, and how many of
    /// them are done or failed.
    batch_total: usize,
    batch_done: usize,
    batch_failed: usize,
}

impl Downloads {
    /// Reads the downloads that belong to `account` (empty for nobody: signed out).
    pub fn load(dir: PathBuf, account: &str) -> Downloads {
        let _ = fs::create_dir_all(&dir);
        if !account.is_empty() {
            // Before 0.29 there was one list for whoever was signed in. It becomes the list
            // of the first account to sign in after updating.
            for name in ["index", "collections"] {
                let (old, new) = (dir.join(format!("{name}.json")), dir.join(format!("{name}-{account}.json")));
                if old.exists() && !new.exists() {
                    let _ = fs::rename(&old, &new);
                }
            }
        }
        let mut entries: Vec<Entry> = fs::read(dir.join(format!("index-{account}.json")))
            .ok()
            .filter(|_| !account.is_empty())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        // Trust the folder over the index: a file deleted by hand is simply not downloaded.
        entries.retain(|entry| dir.join(&entry.file).is_file());
        // Fetches that were cut short by the app closing.
        for stale in fs::read_dir(&dir).into_iter().flatten().flatten() {
            if stale.file_name().to_string_lossy().ends_with(".part") {
                let _ = fs::remove_file(stale.path());
            }
        }
        let files = entries.iter().map(|e| (e.song.id.clone(), e.file.clone())).collect();
        let arrived = entries.iter().map(|e| (e.song.id.clone(), arrival(&dir, e))).collect();
        let collections = fs::read(dir.join(format!("collections-{account}.json")))
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
            account: account.to_string(),
            next_attempt: 0,
            failed: Vec::new(),
            saved_at: std::time::Instant::now(),
            batch_total: 0,
            batch_done: 0,
            batch_failed: 0,
        }
    }

    fn save(&mut self) {
        if self.account.is_empty() {
            return;
        }
        if let Ok(json) = serde_json::to_vec(&self.entries) {
            crate::config::write_atomically(&self.dir.join(format!("index-{}.json", self.account)), &json);
        }
        self.unsaved = false;
        self.saved_at = std::time::Instant::now();
    }

    /// Writes the list of downloads if it has changed. While a batch is arriving that would
    /// be the whole list rewritten for every song, so it's done every couple of seconds
    /// instead, and once more when the batch is over (`now`).
    pub fn save_if_due(&mut self, now: bool) {
        if self.unsaved && (now || self.saved_at.elapsed() > std::time::Duration::from_secs(2)) {
            self.save();
        }
    }

    /// Remembers (or forgets) that a playlist is kept downloaded.
    fn keep_collection(&mut self, id: &str, keep: bool) {
        self.collections.retain(|c| c != id);
        if keep {
            self.collections.push(id.to_string());
        }
        if let (Ok(json), false) = (serde_json::to_vec(&self.collections), self.account.is_empty()) {
            crate::config::write_atomically(&self.dir.join(format!("collections-{}.json", self.account)), &json);
        }
    }

    /// The folder the downloads are in.
    pub fn folder(&self) -> &Path {
        &self.dir
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
            self.save();
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
    let original = playable && (quality == 0 || crate::playback::within_limit(&suffix, song.bit_rate, quality));
    if original { song.size } else { song.duration as u64 * if quality == 0 { 320 } else { quality } as u64 * 125 }
}

/// Moves everything in one folder into another: by renaming where that works (the same
/// drive), otherwise by copying and then removing. Returns how many files moved and how
/// many couldn't be. Files still arriving are left out.
pub fn move_folder(from: &Path, to: &Path) -> (usize, usize) {
    let (mut moved, mut stuck) = (0, 0);
    for entry in fs::read_dir(from).into_iter().flatten().flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if !path.is_file() || name.to_string_lossy().ends_with(".part") {
            continue;
        }
        let target = to.join(&name);
        let done = fs::rename(&path, &target).is_ok()
            || (fs::copy(&path, &target).is_ok_and(|copied| fs::metadata(&path).is_ok_and(|meta| meta.len() == copied)) && fs::remove_file(&path).is_ok());
        if done {
            moved += 1;
        } else {
            // Don't leave half a copy behind where the whole file would be looked for.
            if target.exists() && path.exists() {
                let _ = fs::remove_file(&target);
            }
            stuck += 1;
        }
    }
    (moved, stuck)
}

/// Fetches one song's audio into the downloads folder. Runs on a worker thread. `quality`
/// is a bitrate cap in kbps, or 0 to keep the file as it is on the server.
fn fetch(
    client: &Client,
    dir: &Path,
    song: &Song,
    quality: u32,
    attempt: u64,
    progress: &AtomicU32,
    cancel: &AtomicBool,
) -> Result<Entry, String> {
    let suffix = song.suffix.to_lowercase();
    let playable = NATIVE_SUFFIXES.contains(&suffix.as_str());
    // The original is kept when lossless is asked for, or when it's already an MP3 within the
    // limit. Otherwise the server converts it, as it would for streaming. (Formats the app
    // can't decode itself are always converted.)
    let original = playable && (quality == 0 || crate::playback::within_limit(&suffix, song.bit_rate, quality));
    let rate = if quality == 0 { 320 } else { quality };
    let (url, extension, expected) = if original {
        (client.stream_url(&song.id, 0, 0), suffix, song.size)
    } else {
        (client.stream_url(&song.id, rate, 0), "mp3".to_string(), song.duration as u64 * rate as u64 * 125)
    };
    let stem: String = song.id.chars().filter(|c| c.is_ascii_alphanumeric() || "-_".contains(*c)).collect();
    if stem.is_empty() {
        return Err("unexpected song id".into());
    }
    let file = format!("{stem}.{extension}");
    // Each go has a file of its own to arrive in, so two goes at one song can't trip over
    // each other.
    let temp = dir.join(format!("{file}.{attempt}.part"));
    let (mut body, length) = client.open_stream(&url, None).map_err(|e| e.to_string())?;
    let expected = length.filter(|n| *n > 0).unwrap_or(expected).max(1);
    let mut out = fs::File::create(&temp).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 128 * 1024];
    let mut bytes = 0u64;
    let copied = loop {
        if cancel.load(Ordering::Relaxed) {
            break Err("cancelled".to_string());
        }
        match body.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                if let Err(e) = out.write_all(&buf[..n]) {
                    break Err(e.to_string());
                }
                bytes += n as u64;
                // Held just short of full until the file is in place.
                progress.store((bytes * 1000 / expected).min(990) as u32, Ordering::Relaxed);
            }
            Err(e) => break Err(e.to_string()),
        }
    };
    drop(out);
    let copied = copied.and_then(|()| match bytes {
        0 => Err("The server sent nothing.".to_string()),
        // Cancelled as the last of it arrived: it isn't wanted, whole or not.
        _ if cancel.load(Ordering::Relaxed) => Err("cancelled".to_string()),
        _ => Ok(()),
    });
    if let Err(reason) = copied {
        let _ = fs::remove_file(&temp);
        return Err(reason);
    }
    fs::rename(&temp, dir.join(&file)).map_err(|e| e.to_string())?;
    Ok(Entry { song: song.clone(), file, bytes, at: chrono::Utc::now().timestamp() })
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
        for (song, attempt, progress, cancel) in started {
            // Show it as on its way straight off.
            self.mark_download(&song.id);
            let (client, dir, covers) = (client.clone(), dir.clone(), covers.clone());
            self.download_pool.spawn(move || {
                let result = if cancel.load(Ordering::Relaxed) {
                    Err("cancelled".to_string())
                } else {
                    fetch(&client, &dir, &song, quality, attempt, &progress, &cancel)
                };
                if result.is_ok() && !song.cover_art.is_empty() {
                    // Have the artwork on disk too, for when the server can't be reached.
                    let _ = crate::covers::ensure_file(&client, &covers, &song.cover_art);
                }
                let id = song.id.clone();
                post(move |ctl| ctl.download_finished(&id, attempt, result));
            });
        }
        self.sync_downloads();
    }

    fn download_finished(self: &Rc<Self>, id: &str, attempt: u64, result: Result<Entry, String>) {
        let finished = {
            let mut downloads = self.downloads.borrow_mut();
            // No longer listed means this go was cancelled while it was fetching. (The song
            // may have been asked for again since: that go is listed, and left alone.)
            let wanted = downloads.jobs.iter().position(|job| job.song.id == id && job.attempt == attempt).map(|at| downloads.jobs.remove(at));
            if wanted.is_some() {
                downloads.under_way.remove(id);
            }
            match (result, wanted) {
                (Ok(entry), Some(_)) => {
                    downloads.batch_done += 1;
                    downloads.add(entry);
                }
                // A go that was cancelled finished anyway. Its file goes, unless the song has
                // been asked for again or downloaded since, when the file is that one's.
                (Ok(entry), None) => {
                    if !downloads.has(id) && !downloads.queued(id) {
                        let _ = fs::remove_file(downloads.dir.join(entry.file));
                    }
                }
                (Err(reason), Some(job)) => {
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
        self.mark_download(id);
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
        {
            let mut downloads = self.downloads.borrow_mut();
            for id in ids {
                downloads.remove(id);
            }
        }
        self.cancel_downloads(ids);
    }

    /// Moves the downloads into another folder and keeps them there from now on.
    pub fn move_downloads(self: &Rc<Self>, folder: PathBuf) {
        let from = self.downloads.borrow().dir.clone();
        if folder == from {
            return;
        }
        if !self.downloads.borrow().jobs.is_empty() || self.moving_downloads.get() {
            return self.toast("Wait for the downloads under way to finish, then choose the folder again.");
        }
        if fs::create_dir_all(&folder).is_err() || !folder.is_dir() {
            return self.toast(format!("{} can't be used as a folder.", folder.display()));
        }
        // Room for them all, unless it's the same drive (when nothing is copied). Renaming a
        // scrap of a file across tells which.
        let probe = from.join("move-test.part");
        let same_drive = fs::write(&probe, b"").is_ok() && fs::rename(&probe, folder.join("move-test.part")).is_ok();
        let _ = fs::remove_file(&probe);
        let _ = fs::remove_file(folder.join("move-test.part"));
        let needed = crate::config::dir_size(&from);
        if let Some(free) = free_space(&folder).filter(|free| !same_drive && needed + SPARE > *free) {
            return self.toast(format!(
                "Not enough room there: the downloads come to {}, and that drive has {} free. (Half a gigabyte is always left spare.)",
                size_label(needed),
                size_label(free)
            ));
        }
        self.downloads.borrow_mut().save_if_due(true);
        self.moving_downloads.set(true);
        self.toast(format!("Moving your downloads to {}…", folder.display()));
        std::thread::spawn(move || {
            let (moved, stuck) = move_folder(&from, &folder);
            post(move |ctl| {
                ctl.moving_downloads.set(false);
                // What couldn't be moved is still where it was: stay there with everything
                // else left behind, so nothing is lost sight of.
                if stuck > 0 {
                    let (back, _) = move_folder(&folder, &from);
                    let account = ctl.account();
                    *ctl.downloads.borrow_mut() = Downloads::load(from, &account);
                    ctl.toast(format!("{stuck} of {} files couldn't be moved, so the downloads are staying where they were.", moved + stuck));
                    let _ = back;
                } else {
                    ctl.settings.borrow_mut().downloads_dir =
                        if folder == ctl.paths.downloads { String::new() } else { folder.display().to_string() };
                    ctl.save_settings();
                    let account = ctl.account();
                    *ctl.downloads.borrow_mut() = Downloads::load(folder.clone(), &account);
                    ctl.toast(format!("Your downloads are now in {}", folder.display()));
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
        let collections = self.downloads.borrow().collections.clone();
        for id in collections {
            self.keep_downloaded(&id, false);
        }
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
        settings.set_downloads_folder_custom(downloads.dir != self.paths.downloads);
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

    #[test]
    fn moves_a_folder_of_downloads() {
        let (from, to) = (folder("from"), folder("to"));
        fs::write(from.join("a.flac"), b"one").unwrap();
        fs::write(from.join("index-someone.json"), b"[]").unwrap();
        fs::write(from.join("c.flac.7.part"), b"half").unwrap();
        fs::create_dir_all(from.join("a folder")).unwrap();
        assert_eq!(move_folder(&from, &to), (2, 0));
        assert_eq!(fs::read(to.join("a.flac")).unwrap(), b"one");
        assert!(to.join("index-someone.json").exists() && !from.join("a.flac").exists());
        // A file still arriving stays behind, and folders aren't touched.
        assert!(from.join("c.flac.7.part").exists() && from.join("a folder").exists() && !to.join("c.flac.7.part").exists());
        assert_eq!((size_label(340 * 1024 * 1024), size_label(12_400 * 1024 * 1024)), ("340 MB".to_string(), "12.1 GB".to_string()));
        let _ = fs::remove_dir_all(from);
        let _ = fs::remove_dir_all(to);
    }
}
