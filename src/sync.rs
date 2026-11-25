//! Keeping devices in step.
//!
//! Two things are shared between copies of the app signed in to the same account:
//!
//! * The play queue and the place in it, through the server's own saved-queue feature.
//!   Whichever device changed it last has the say; a device that's playing keeps its own.
//! * Pins, each list's shuffle and sort choice, the theme, when songs were added to
//!   playlists, and Recently played. The server has nowhere made for such things, so they
//!   live as text in the comment of one playlist the app keeps for the purpose and doesn't
//!   list. Each item carries the time it last changed, and the newer one wins.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use slint::ComponentHandle as _;

pub use nexus_core::sync::{HISTORY_LENGTH, PLAYLIST_NAME};
use nexus_core::sync::{LONG_AGO, PLAYS_SHARED, Shared, VERSION, list_rank, merge, merge_history, merge_plays};

use crate::added::parse_time;
use crate::config::Played;
use crate::controller::{Controller, Route, with};
use crate::subsonic::{ApiError, ApiResult, Client, PlayQueue, Song};

/// How long after a change before it's passed on, so a burst of changes goes as one.
const SETTLE: Duration = Duration::from_secs(3);
/// How often to look for changes made elsewhere.
const LOOK: Duration = Duration::from_secs(60);
/// How much of a long queue is passed on: this many songs before the current one...
const QUEUE_BEHIND: usize = 50;
/// ...and this many from it onwards.
const QUEUE_AHEAD: usize = 450;

/// What an exchange with the server came back with.
pub struct Outcome {
    playlist: String,
    shared: Shared,
    /// The songs of `shared.history`, for the ones that could be found.
    songs: HashMap<String, Song>,
}

fn find_or_create(client: &Client) -> ApiResult<String> {
    let mine = client.username().to_lowercase();
    let found = client
        .playlists()?
        .into_iter()
        .filter(|p| p.name == PLAYLIST_NAME && (p.owner.is_empty() || p.owner.to_lowercase() == mine))
        .map(|p| p.id)
        .min();
    match found {
        Some(id) => Ok(id),
        None => Ok(client.create_playlist(PLAYLIST_NAME, &[])?.id),
    }
}

/// Reads what's on the server, combines it with what this device has, writes the result
/// back if that adds anything, and returns it.
fn exchange(client: &Client, known: Option<String>, local: Shared, mut songs: HashMap<String, Song>) -> ApiResult<Outcome> {
    let now = chrono::Utc::now().timestamp();
    let first = match known {
        Some(id) => client.playlist(&id),
        None => client.playlist(&find_or_create(client)?),
    };
    let playlist = match first {
        Ok(playlist) if playlist.name == PLAYLIST_NAME => playlist,
        // Renamed or deleted from another app: find it again, or start a new one.
        Ok(_) | Err(ApiError::Server { .. }) => client.playlist(&find_or_create(client)?)?,
        Err(e) => return Err(e),
    };
    let remote: Shared = serde_json::from_str(&playlist.comment).unwrap_or_default();
    if remote.nexus_music_sync > VERSION {
        // Written by a newer version of the app: read what's understood, change nothing.
        return Ok(Outcome { playlist: playlist.id, shared: remote, songs });
    }
    let merged = merge(&local, &remote, now);
    if merged != remote {
        let text = serde_json::to_string(&merged).unwrap_or_default();
        client.set_playlist_comment(&playlist.id, &text)?;
    }
    // Songs played elsewhere that this device hasn't met.
    for (id, _) in &merged.history {
        if !songs.contains_key(id) {
            if let Ok(song) = client.song(id) {
                songs.insert(id.clone(), song);
            }
        }
    }
    Ok(Outcome { playlist: playlist.id, shared: merged, songs })
}

/// Reads what's shared without changing anything, for a first look before the window opens.
fn peek(client: &Client, known: &str) -> Option<(String, Shared)> {
    let playlist = if known.is_empty() {
        let id = client.playlists().ok()?.into_iter().filter(|p| p.name == PLAYLIST_NAME).map(|p| p.id).min()?;
        client.playlist(&id).ok()?
    } else {
        client.playlist(known).ok()?
    };
    (playlist.name == PLAYLIST_NAME).then(|| (playlist.id, serde_json::from_str(&playlist.comment).unwrap_or_default()))
}

impl Controller {
    /// Finds out whether the theme was changed on another device. The window opens in the
    /// colours last used here without waiting for the answer; a newer choice is applied
    /// when it arrives, which with the server close by is before the window has been seen.
    pub fn preload_theme(&self) {
        let Some(client) = self.client() else { return };
        if !self.settings.borrow().sync || self.is_offline() {
            return;
        }
        let known = self.settings.borrow().sync_playlist.clone();
        self.api_pool.spawn(move || {
            let Some((playlist, theirs)) = peek(&client, &known) else { return };
            crate::controller::post(move |ctl| ctl.theme_arrived(playlist, theirs));
        });
    }

    fn theme_arrived(&self, playlist: String, theirs: Shared) {
        if self.client().is_none() {
            return;
        }
        let changed = {
            let mut settings = self.settings.borrow_mut();
            settings.sync_playlist = playlist.clone();
            let known = crate::config::THEMES.iter().any(|t| t.name == theirs.theme);
            let newer = known && (theirs.theme_at, &theirs.theme) > (settings.theme_at, &settings.theme);
            if newer {
                settings.theme = theirs.theme.clone();
                settings.theme_at = theirs.theme_at;
            }
            newer
        };
        *self.sync_playlist.borrow_mut() = Some(playlist);
        if changed {
            self.save_settings();
            self.apply_theme();
            self.push_settings();
        }
    }

    fn sync_on(&self) -> bool {
        self.settings.borrow().sync && self.client().is_some() && !self.is_offline()
    }

    /// The name this device gives when it saves the queue, so it can tell its own from others'.
    fn device_name(&self) -> String {
        if self.settings.borrow().device_id.is_empty() {
            self.settings.borrow_mut().device_id = format!("{:08x}", fastrand::u32(..));
            self.save_settings();
        }
        format!("NexusMusic-{}", self.settings.borrow().device_id)
    }

    /// Starts keeping in step: now, and every so often from here on.
    pub fn start_sync(self: &Rc<Self>) {
        self.sync_busy.set(false);
        if self.sync_playlist.borrow().is_none() {
            let known = self.settings.borrow().sync_playlist.clone();
            *self.sync_playlist.borrow_mut() = (!known.is_empty()).then_some(known);
        }
        self.look_elsewhere();
        self.sync_look_timer.start(slint::TimerMode::Repeated, LOOK, || {
            with(|ctl| ctl.look_elsewhere());
        });
    }

    pub fn stop_sync(&self) {
        self.sync_look_timer.stop();
        self.sync_timer.stop();
        self.queue_timer.stop();
        self.sync_busy.set(false);
        *self.sync_playlist.borrow_mut() = None;
    }

    /// Picks up whatever other devices have changed.
    pub fn look_elsewhere(self: &Rc<Self>) {
        if !self.sync_on() {
            return;
        }
        self.share_now();
        self.check_server_queue();
        // Playlists made or changed elsewhere.
        self.reload_playlists();
    }

    /// Something that's shared changed here: pass it on once things have settled.
    pub fn share_soon(&self) {
        if self.settings.borrow().sync {
            self.sync_timer.start(slint::TimerMode::SingleShot, SETTLE, || {
                with(|ctl| ctl.share_now());
            });
        }
    }

    /// What this device has to share.
    fn shared_here(&self) -> Shared {
        {
            // Choices from before sharing existed have no time on them: give them an old one.
            let mut settings = self.settings.borrow_mut();
            if settings.pins_at == 0 && !settings.pinned_playlists.is_empty() {
                settings.pins_at = LONG_AGO;
            }
            if settings.theme_at == 0 && settings.theme != crate::config::THEMES[0].name {
                settings.theme_at = LONG_AGO;
            }
            for prefs in settings.lists.values_mut().filter(|prefs| prefs.at == 0) {
                prefs.at = LONG_AGO;
            }
        }
        let settings = self.settings.borrow();
        let playlists = self.playlists.borrow();
        let known: HashSet<&str> = playlists.iter().map(|p| p.id.as_str()).collect();
        let plays = merge_plays(&crate::plays::shared().into_iter().rev().take(PLAYS_SHARED).collect::<Vec<_>>(), &[]);
        let play_sources = {
            let kept: HashSet<(&str, i64)> = plays.iter().map(|(song, at)| (song.as_str(), *at)).collect();
            crate::plays::sources().into_iter().filter(|(song, at, _)| kept.contains(&(song.as_str(), *at))).collect()
        };
        Shared {
            nexus_music_sync: VERSION,
            pins: settings.pinned_playlists.clone(),
            pins_at: settings.pins_at,
            theme: settings.theme.clone(),
            theme_at: settings.theme_at,
            lists: settings.lists.clone(),
            // Before the playlists have loaded there's no telling which still exist.
            added: self.added.borrow().shared((!known.is_empty()).then_some(&known)),
            history: self.history.borrow().iter().map(|played| (played.song.id.clone(), played.at)).collect(),
            play_sources,
            plays,
        }
    }

    fn share_now(self: &Rc<Self>) {
        if !self.sync_on() {
            return;
        }
        if self.sync_busy.replace(true) {
            // One exchange at a time; go again when this one is back.
            self.sync_again.set(true);
            return;
        }
        let local = self.shared_here();
        let songs: HashMap<String, Song> =
            self.history.borrow().iter().map(|played| (played.song.id.clone(), played.song.clone())).collect();
        let known = self.sync_playlist.borrow().clone();
        self.request(
            move |client| exchange(client, known, local, songs),
            |ctl, result| {
                ctl.sync_busy.set(false);
                if let Ok(outcome) = result {
                    ctl.take_shared(outcome);
                }
                if ctl.sync_again.replace(false) {
                    ctl.share_soon();
                }
            },
        );
    }

    /// Takes in what came back from the server, where it's newer than what's here.
    fn take_shared(self: &Rc<Self>, outcome: Outcome) {
        let moved = self.settings.borrow().sync_playlist != outcome.playlist;
        *self.sync_playlist.borrow_mut() = Some(outcome.playlist.clone());
        let theirs = outcome.shared;
        let (mut pins, mut theme, mut lists) = (false, false, moved);
        {
            let mut settings = self.settings.borrow_mut();
            // Remembered, so the next start can look at it before the window opens.
            settings.sync_playlist = outcome.playlist;
            if (theirs.pins_at, &theirs.pins) > (settings.pins_at, &settings.pinned_playlists) {
                pins = settings.pinned_playlists != theirs.pins;
                settings.pinned_playlists = theirs.pins.clone();
                settings.pins_at = theirs.pins_at;
            }
            let known = crate::config::THEMES.iter().any(|t| t.name == theirs.theme);
            if known && (theirs.theme_at, &theirs.theme) > (settings.theme_at, &settings.theme) {
                theme = settings.theme != theirs.theme;
                settings.theme = theirs.theme.clone();
                settings.theme_at = theirs.theme_at;
            }
            for (key, prefs) in &theirs.lists {
                if settings.lists.get(key).is_none_or(|mine| list_rank(prefs) > list_rank(mine)) {
                    settings.lists.insert(key.clone(), *prefs);
                    lists = true;
                }
            }
        }
        if pins || theme || lists {
            self.save_settings();
        }
        if pins {
            self.show_playlists();
        }
        if theme {
            self.apply_theme();
            self.push_settings();
        }
        if lists {
            self.sync_page_shuffle();
        }
        if self.added.borrow_mut().merge(&theirs.added) && matches!(self.current_route(), Some(Route::Playlist(_))) {
            // The dates on screen may have changed.
            self.rebuild_tracks();
        }
        // Plays counted on other devices go into this one's log, so the stats cover them all.
        if self.play_log.borrow_mut().absorb(&theirs.plays, &theirs.play_sources) > 0 {
            self.invalidate("stats");
        }
        // Recently played: both devices' plays, in order.
        let mine: Vec<(String, i64)> =
            self.history.borrow().iter().map(|played| (played.song.id.clone(), played.at)).collect();
        let merged = merge_history(&mine, &theirs.history);
        if merged != mine {
            let mut known = outcome.songs;
            for played in self.history.borrow().iter() {
                known.entry(played.song.id.clone()).or_insert_with(|| played.song.clone());
            }
            let history: Vec<Played> = merged
                .into_iter()
                .filter_map(|(id, at)| known.get(&id).map(|song| Played { song: song.clone(), at }))
                .collect();
            crate::config::save_history(&self.paths.history, &history);
            *self.history.borrow_mut() = history;
            self.sync_history();
        }
    }

    // ---- the queue -----------------------------------------------------------------------------

    /// The queue changed here: save it on the server once things have settled.
    pub fn queue_changed_here(&self) {
        if self.settings.borrow().sync {
            self.queue_timer.start(slint::TimerMode::SingleShot, SETTLE, || {
                with(|ctl| ctl.upload_queue());
            });
        }
    }

    fn upload_queue(self: &Rc<Self>) {
        if !self.sync_on() {
            return;
        }
        let device = self.device_name();
        let (ids, current, position) = {
            let pb = self.playback.borrow();
            let Some(current) = pb.current() else { return };
            // A radio station isn't something another device could pick up by id.
            if !current.radio_url.is_empty() {
                return;
            }
            let from = pb.cursor.saturating_sub(QUEUE_BEHIND);
            let ids: Vec<String> = pb
                .order
                .iter()
                .skip(from)
                .take(pb.cursor - from + QUEUE_AHEAD)
                .filter_map(|&i| pb.queue.get(i))
                .filter(|song| song.radio_url.is_empty())
                .map(|song| song.id.clone())
                .collect();
            (ids, current.id.clone(), (pb.position.max(0.0) * 1000.0) as u64)
        };
        self.request(move |client| client.save_play_queue(&device, &ids, &current, position), |_, _| {});
    }

    /// Asks the server for the saved queue, and takes it up if another device changed it
    /// after anything was last done here. A device that's playing keeps its own queue.
    pub fn check_server_queue(self: &Rc<Self>) {
        if !self.sync_on() {
            return;
        }
        self.request(
            |client| client.play_queue(),
            |ctl, result| {
                if let Ok(theirs) = result {
                    ctl.note_elsewhere(&theirs);
                    ctl.adopt_queue(theirs);
                }
            },
        );
    }

    /// Remembers what another device has been playing in the last half hour, so it can be
    /// offered as "Continue here". (A device that's paused takes up a newer queue without
    /// being asked; this is for one that's playing something of its own.)
    fn note_elsewhere(&self, theirs: &PlayQueue) {
        let age = chrono::Utc::now().timestamp() - parse_time(&theirs.changed);
        let same = self.playback.borrow().current().is_some_and(|song| song.id == theirs.current);
        let newer = !theirs.songs.is_empty() && theirs.changed_by != self.device_name() && (0..30 * 60).contains(&age) && !same;
        let label = newer
            .then(|| theirs.songs.iter().find(|song| song.id == theirs.current).or(theirs.songs.first()))
            .flatten()
            .map(|song| format!("{} — {}", song.title, song.artist_label()))
            .unwrap_or_default();
        *self.elsewhere.borrow_mut() = newer.then(|| theirs.clone());
        self.ui.global::<crate::ui::Player>().set_elsewhere(label.into());
    }

    /// Takes up what another device was playing, from where it had got to, and plays it.
    pub fn continue_here(self: &Rc<Self>) {
        let Some(theirs) = self.elsewhere.borrow_mut().take() else { return };
        self.ui.global::<crate::ui::Player>().set_elsewhere("".into());
        let cursor = theirs.songs.iter().position(|song| song.id == theirs.current).unwrap_or(0);
        let duration = theirs.songs[cursor].duration as f64;
        let position = theirs.position as f64 / 1000.0;
        let position = if duration > 0.0 && position >= duration - 1.0 { 0.0 } else { position.max(0.0) };
        self.engine.set_playing(false);
        self.engine.stop();
        self.queue_stamp.set(parse_time(&theirs.changed));
        self.replace_queue(theirs.songs, cursor, position);
        self.toggle_play();
    }

    fn adopt_queue(self: &Rc<Self>, theirs: PlayQueue) {
        let stamp = parse_time(&theirs.changed);
        if theirs.songs.is_empty() || theirs.changed_by == self.device_name() || stamp <= self.queue_stamp.get() {
            return;
        }
        if self.playback.borrow().playing {
            return;
        }
        // Taken up: there's nothing left to offer.
        self.elsewhere.borrow_mut().take();
        self.ui.global::<crate::ui::Player>().set_elsewhere("".into());
        let cursor = theirs.songs.iter().position(|song| song.id == theirs.current).unwrap_or(0);
        let duration = theirs.songs[cursor].duration as f64;
        let position = theirs.position as f64 / 1000.0;
        let position = if duration > 0.0 && position >= duration - 1.0 { 0.0 } else { position.max(0.0) };
        self.engine.set_playing(false);
        self.engine.stop();
        self.queue_stamp.set(stamp);
        self.replace_queue(theirs.songs, cursor, position);
    }
}
