//! Songs saved on this PC, so they play without the server: the files, their index, and the
//! controller's side of downloading and removing them.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

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

pub struct Downloads {
    dir: PathBuf,
    /// Oldest first.
    entries: Vec<Entry>,
    files: HashMap<String, String>,
    /// Songs waiting to be fetched or being fetched now.
    queued: HashSet<String>,
    /// For the progress message: how many of the current batch are done, and how many failed.
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
        Downloads { dir, entries, files, queued: HashSet::new(), batch_total: 0, batch_done: 0, batch_failed: 0 }
    }

    fn save(&self) {
        if let Ok(json) = serde_json::to_vec(&self.entries) {
            let temp = self.dir.join("index.tmp");
            if fs::write(&temp, json).is_ok() {
                let _ = fs::rename(&temp, self.dir.join("index.json"));
            }
        }
    }

    pub fn has(&self, id: &str) -> bool {
        self.files.contains_key(id)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Where a downloaded song's audio is, if it has been downloaded.
    pub fn path_of(&self, id: &str) -> Option<PathBuf> {
        self.files.get(id).map(|file| self.dir.join(file))
    }

    /// The downloaded songs, most recently added first.
    pub fn songs(&self) -> Vec<Song> {
        self.entries.iter().rev().map(|e| e.song.clone()).collect()
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

/// Fetches one song's audio into the downloads folder. Runs on a worker thread.
fn fetch(client: &Client, dir: &Path, song: &Song) -> Result<Entry, String> {
    let suffix = song.suffix.to_lowercase();
    // Formats the app can't decode itself are saved the way they're streamed: as MP3.
    let (url, extension) = if NATIVE_SUFFIXES.contains(&suffix.as_str()) {
        (client.stream_url(&song.id, 0, 0), suffix)
    } else {
        (client.stream_url(&song.id, 320, 0), "mp3".to_string())
    };
    let stem: String = song.id.chars().filter(|c| c.is_ascii_alphanumeric() || "-_".contains(*c)).collect();
    if stem.is_empty() {
        return Err("unexpected song id".into());
    }
    let file = format!("{stem}.{extension}");
    let temp = dir.join(format!("{file}.part"));
    let (mut body, _) = client.open_stream(&url, None).map_err(|e| e.to_string())?;
    let mut out = fs::File::create(&temp).map_err(|e| e.to_string())?;
    let mut buf = vec![0u8; 128 * 1024];
    let mut bytes = 0u64;
    let copied = loop {
        match body.read(&mut buf) {
            Ok(0) => break Ok(()),
            Ok(n) => {
                if let Err(e) = out.write_all(&buf[..n]) {
                    break Err(e.to_string());
                }
                bytes += n as u64;
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
        let Some(client) = self.client() else { return };
        let wanted: Vec<Song> = {
            let mut downloads = self.downloads.borrow_mut();
            let mut seen = HashSet::new();
            let wanted: Vec<Song> = songs
                .into_iter()
                .filter(|s| !downloads.has(&s.id) && !downloads.queued.contains(&s.id) && seen.insert(s.id.clone()))
                .collect();
            if downloads.queued.is_empty() {
                downloads.batch_total = 0;
                downloads.batch_done = 0;
                downloads.batch_failed = 0;
            }
            downloads.batch_total += wanted.len();
            downloads.queued.extend(wanted.iter().map(|s| s.id.clone()));
            wanted
        };
        if wanted.is_empty() {
            return self.toast("Already downloaded");
        }
        self.toast(match wanted.len() {
            1 => format!("Downloading “{}”", wanted[0].title),
            n => format!("Downloading {n} songs"),
        });
        let (dir, covers) = (self.downloads.borrow().dir.clone(), self.paths.covers.clone());
        for song in wanted {
            let (client, dir, covers) = (client.clone(), dir.clone(), covers.clone());
            self.download_pool.spawn(move || {
                let result = fetch(&client, &dir, &song);
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
            // Removed or cancelled while it was being fetched.
            let wanted = downloads.queued.remove(id);
            match result {
                Ok(entry) if wanted => {
                    downloads.batch_done += 1;
                    downloads.add(entry);
                }
                Ok(entry) => {
                    let _ = fs::remove_file(downloads.dir.join(entry.file));
                }
                Err(_) => downloads.batch_failed += 1,
            }
            downloads.queued.is_empty().then_some((downloads.batch_done, downloads.batch_failed))
        };
        self.mark_downloaded(id, self.downloads.borrow().has(id));
        if let Some((done, failed)) = finished {
            self.toast(match (done, failed) {
                (_, 0) if done == 1 => "Download finished".to_string(),
                (_, 0) => format!("{done} songs downloaded"),
                (0, _) => "Couldn't download. Check the connection to your server.".to_string(),
                _ => format!("{done} songs downloaded; {failed} couldn't be fetched"),
            });
        }
        self.sync_downloads();
    }

    /// Deletes these songs' saved copies and forgets any that are still on their way.
    pub fn remove_downloads(self: &Rc<Self>, ids: &[String]) {
        {
            let mut downloads = self.downloads.borrow_mut();
            for id in ids {
                downloads.queued.remove(id);
                downloads.remove(id);
            }
        }
        for id in ids {
            self.mark_downloaded(id, false);
        }
        self.sync_downloads();
    }

    pub fn remove_all_downloads(self: &Rc<Self>) {
        let ids: Vec<String> = self.downloads.borrow().entries.iter().map(|e| e.song.id.clone()).collect();
        let queued: Vec<String> = self.downloads.borrow().queued.iter().cloned().collect();
        self.remove_downloads(&[ids, queued].concat());
        self.toast("Downloads removed");
    }

    /// Shows or hides the "downloaded" mark on every row for this song.
    fn mark_downloaded(&self, id: &str, downloaded: bool) {
        for model in [&self.queue_model, &self.history_model, &self.tracks_model] {
            for row in 0..model.row_count() {
                if let Some(mut track) = model.row_data(row) {
                    if track.id == id && track.downloaded != downloaded {
                        track.downloaded = downloaded;
                        model.set_row_data(row, track);
                    }
                }
            }
        }
    }

    /// Refreshes everything that summarises the downloads: the sidebar line, Settings, the
    /// page's download button, and the Downloads page itself when it's showing.
    pub fn sync_downloads(self: &Rc<Self>) {
        let downloads = self.downloads.borrow();
        let status = if downloads.queued.is_empty() {
            String::new()
        } else {
            format!("Downloading {} of {}", (downloads.batch_done + downloads.batch_failed + 1).min(downloads.batch_total), downloads.batch_total)
        };
        self.ui.global::<ui::App>().set_download_status(status.into());
        self.ui.global::<ui::Settings>().set_downloads_summary(downloads.summary().into());
        drop(downloads);
        self.sync_download_button();
        if matches!(self.current_route(), Some(Route::Downloads)) && self.downloads.borrow().queued.is_empty() {
            let scroll = self.ui.global::<ui::View>().get_scroll_y();
            self.show_downloads(scroll);
        }
    }

    /// The download button on an album or playlist: 0 nothing saved, 1 in progress, 2 all saved.
    pub fn sync_download_button(&self) {
        let page = self.page.borrow();
        let downloads = self.downloads.borrow();
        let view = self.ui.global::<ui::View>();
        let offered = !page.songs.is_empty()
            && (page.playlist.is_some() || matches!(self.current_route(), Some(Route::Album(_)) | Some(Route::Liked)));
        view.set_can_download(offered);
        let saved = page.songs.iter().filter(|s| downloads.has(&s.id)).count();
        let busy = page.songs.iter().any(|s| downloads.queued.contains(&s.id));
        view.set_download_state(if busy { 1 } else if saved == page.songs.len() && saved > 0 { 2 } else { 0 });
    }

    /// The button was pressed: download what's missing, or remove it all if nothing is.
    pub fn toggle_page_download(self: &Rc<Self>) {
        let songs = self.page.borrow().songs.clone();
        let all_saved = {
            let downloads = self.downloads.borrow();
            songs.iter().all(|s| downloads.has(&s.id))
        };
        if all_saved {
            let ids: Vec<String> = songs.iter().map(|s| s.id.clone()).collect();
            self.remove_downloads(&ids);
            self.toast("Removed from downloads");
        } else {
            self.download_songs(songs);
        }
    }

    /// Builds the Downloads page from what's on disk; no server involved.
    pub fn show_downloads(self: &Rc<Self>, scroll: f32) {
        let (songs, summary) = {
            let downloads = self.downloads.borrow();
            (downloads.songs(), downloads.summary())
        };
        let page = PageData {
            label: "On this PC".into(),
            title: "Downloads".into(),
            meta: if songs.is_empty() { String::new() } else { summary },
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
