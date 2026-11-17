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
use slint::{ComponentHandle, Model};

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
}

/// A song waiting to be fetched, or being fetched now.
struct Job {
    song: Song,
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
    /// For the progress messages: how many songs the current batch has, and how many of
    /// them are done or failed.
    batch_total: usize,
    batch_done: usize,
    batch_failed: usize,
}

impl Downloads {
    pub fn load(dir: PathBuf) -> Downloads {
        let _ = fs::create_dir_all(&dir);
        let mut entries: Vec<Entry> = fs::read(dir.join("index.json"))
            .ok()
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
        let collections = fs::read(dir.join("collections.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Downloads { dir, entries, files, collections, jobs: Vec::new(), batch_total: 0, batch_done: 0, batch_failed: 0 }
    }

    fn save(&self) {
        if let Ok(json) = serde_json::to_vec(&self.entries) {
            let temp = self.dir.join("index.tmp");
            if fs::write(&temp, json).is_ok() {
                let _ = fs::rename(&temp, self.dir.join("index.json"));
            }
        }
    }

    /// Remembers (or forgets) that a playlist is kept downloaded.
    fn keep_collection(&mut self, id: &str, keep: bool) {
        self.collections.retain(|c| c != id);
        if keep {
            self.collections.push(id.to_string());
        }
        if let Ok(json) = serde_json::to_vec(&self.collections) {
            let _ = fs::write(self.dir.join("collections.json"), json);
        }
    }

    pub fn has(&self, id: &str) -> bool {
        self.files.contains_key(id)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn job(&self, id: &str) -> Option<&Job> {
        self.jobs.iter().find(|job| job.song.id == id)
    }

    /// How far a song's download has got, from 0 to 1; -1 if it isn't being downloaded.
    pub fn progress_of(&self, id: &str) -> f32 {
        self.job(id).map(|job| job.progress.load(Ordering::Relaxed) as f32 / 1000.0).unwrap_or(-1.0)
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
        self.files.insert(entry.song.id.clone(), entry.file.clone());
        self.entries.retain(|e| e.song.id != entry.song.id);
        self.entries.push(entry);
        self.save();
    }

    fn remove(&mut self, id: &str) {
        if let Some(file) = self.files.remove(id) {
            let _ = fs::remove_file(self.dir.join(file));
            self.entries.retain(|e| e.song.id != id);
            self.save();
        }
    }
}

/// Fetches one song's audio into the downloads folder. Runs on a worker thread. `quality`
/// is a bitrate cap in kbps, or 0 to keep the file as it is on the server.
fn fetch(
    client: &Client,
    dir: &Path,
    song: &Song,
    quality: u32,
    progress: &AtomicU32,
    cancel: &AtomicBool,
) -> Result<Entry, String> {
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
        return Err("unexpected song id".into());
    }
    let file = format!("{stem}.{extension}");
    let temp = dir.join(format!("{file}.part"));
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
    if let Err(reason) = copied.and_then(|()| if bytes == 0 { Err("the server sent nothing".into()) } else { Ok(()) }) {
        let _ = fs::remove_file(&temp);
        return Err(reason);
    }
    fs::rename(&temp, dir.join(&file)).map_err(|e| e.to_string())?;
    Ok(Entry { song: song.clone(), file, bytes })
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
        let quality = self.settings.borrow().download_quality;
        let started: Vec<(Song, Arc<AtomicU32>, Arc<AtomicBool>)> = {
            let mut downloads = self.downloads.borrow_mut();
            let mut seen = HashSet::new();
            let wanted: Vec<Song> = songs
                .into_iter()
                .filter(|s| {
                    s.radio_url.is_empty() && !downloads.has(&s.id) && downloads.job(&s.id).is_none() && seen.insert(s.id.clone())
                })
                .collect();
            if downloads.jobs.is_empty() {
                downloads.batch_total = 0;
                downloads.batch_done = 0;
                downloads.batch_failed = 0;
            }
            downloads.batch_total += wanted.len();
            wanted
                .into_iter()
                .map(|song| {
                    let job = Job {
                        song: song.clone(),
                        progress: Arc::new(AtomicU32::new(0)),
                        cancel: Arc::new(AtomicBool::new(false)),
                    };
                    let handles = (song, job.progress.clone(), job.cancel.clone());
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
        for (song, progress, cancel) in started {
            // Show it as on its way straight off.
            self.mark_download(&song.id);
            let (client, dir, covers) = (client.clone(), dir.clone(), covers.clone());
            self.download_pool.spawn(move || {
                let result = if cancel.load(Ordering::Relaxed) {
                    Err("cancelled".to_string())
                } else {
                    fetch(&client, &dir, &song, quality, &progress, &cancel)
                };
                if result.is_ok() && !song.cover_art.is_empty() {
                    // Have the artwork on disk too, for when the server can't be reached.
                    let _ = crate::covers::ensure_file(&client, &covers, &song.cover_art);
                }
                let id = song.id.clone();
                post(move |ctl| ctl.download_finished(&id, result));
            });
        }
        self.sync_downloads();
    }

    fn download_finished(self: &Rc<Self>, id: &str, result: Result<Entry, String>) {
        let finished = {
            let mut downloads = self.downloads.borrow_mut();
            // No longer listed means it was cancelled while being fetched.
            let wanted = downloads.jobs.iter().position(|job| job.song.id == id).map(|at| downloads.jobs.remove(at));
            match (result, wanted) {
                (Ok(entry), Some(_)) => {
                    downloads.batch_done += 1;
                    downloads.add(entry);
                }
                (Ok(entry), None) => {
                    let _ = fs::remove_file(downloads.dir.join(entry.file));
                }
                (Err(_), Some(_)) => downloads.batch_failed += 1,
                (Err(_), None) => {}
            }
            (downloads.jobs.is_empty() && downloads.batch_total > 0)
                .then_some((downloads.batch_done, downloads.batch_failed))
        };
        self.mark_download(id);
        if let Some((done, failed)) = finished {
            self.downloads.borrow_mut().batch_total = 0;
            match (done, failed) {
                (0, 0) => {}
                (1, 0) => self.toast("Download finished"),
                (_, 0) => self.toast(format!("{done} songs downloaded")),
                (0, _) => self.toast("Couldn't download. Check the connection to your server."),
                _ => self.toast(format!("{done} songs downloaded; {failed} couldn't be fetched")),
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
        for model in [&self.queue_model, &self.history_model, &self.tracks_model] {
            for row in 0..model.row_count() {
                if let Some(mut track) = model.row_data(row) {
                    if track.id == id
                        && (track.downloaded != downloaded || track.progress != progress || track.unavailable != unavailable)
                    {
                        track.downloaded = downloaded;
                        track.progress = progress;
                        track.unavailable = unavailable;
                        model.set_row_data(row, track);
                    }
                }
            }
        }
    }

    /// Runs a few times a second while songs are downloading: moves the progress bars along.
    pub fn tick_downloads(self: &Rc<Self>) {
        let ids: Vec<String> = {
            let downloads = self.downloads.borrow();
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
        self.ui.global::<ui::Settings>().set_downloads_summary(downloads.summary().into());
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
        let busy = page.songs.iter().any(|s| downloads.job(&s.id).is_some());
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
                songs.iter().filter(|s| downloads.job(&s.id).is_some()).map(|s| s.id.clone()).collect();
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
        let page = PageData {
            label: "On this PC".into(),
            title: "Downloads".into(),
            meta,
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
