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

use serde::{Deserialize, Serialize};

use crate::added::{Notes, merge_notes, parse_time};
use crate::config::{ListPrefs, Played};
use crate::controller::{Controller, Route, with};
use crate::subsonic::{ApiError, ApiResult, Client, PlayQueue, Song};

/// The playlist whose comment holds the shared data.
pub const PLAYLIST_NAME: &str = "Nexus Music sync";
/// The layout of the shared data. A copy written by a later layout is left alone.
const VERSION: u32 = 1;
/// How long after a change before it's passed on, so a burst of changes goes as one.
const SETTLE: Duration = Duration::from_secs(3);
/// How often to look for changes made elsewhere.
const LOOK: Duration = Duration::from_secs(60);
/// Songs remembered for "Recently played".
pub const HISTORY_LENGTH: usize = 100;
/// How much of a long queue is passed on: this many songs before the current one...
const QUEUE_BEHIND: usize = 50;
/// ...and this many from it onwards.
const QUEUE_AHEAD: usize = 450;
/// Stands in for "some time ago" on choices made before sharing existed, so they're passed
/// on but lose to anything chosen since.
const LONG_AGO: i64 = 1;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Shared {
    /// Marks the text as this app's and says which layout it's in.
    pub nexus_music_sync: u32,
    pub pins: Vec<String>,
    pub pins_at: i64,
    pub theme: String,
    pub theme_at: i64,
    /// By route key, e.g. "playlist:abc".
    pub lists: HashMap<String, ListPrefs>,
    pub added: Notes,
    /// Song id and when it was played, newest first.
    pub history: Vec<(String, i64)>,
}

fn list_rank(prefs: &ListPrefs) -> (i64, bool, usize, bool, bool) {
    (prefs.at, prefs.shuffle, prefs.sort, prefs.descending, prefs.sort_chosen)
}

/// Puts plays from two devices into one list, newest first.
pub fn merge_history(a: &[(String, i64)], b: &[(String, i64)]) -> Vec<(String, i64)> {
    let mut all: Vec<(String, i64)> = a.iter().chain(b).cloned().collect();
    all.sort_by(|x, y| y.1.cmp(&x.1).then_with(|| x.0.cmp(&y.0)));
    all.dedup();
    // The same song twice running is one stretch of listening.
    all.dedup_by(|later, earlier| later.0 == earlier.0);
    all.truncate(HISTORY_LENGTH);
    all
}

/// Combines what two devices know. For each item the newer wins; equally new, the choice is
/// arbitrary but the same whichever way round they're given, so devices end up agreeing.
pub fn merge(a: &Shared, b: &Shared, now: i64) -> Shared {
    let pins_from_a = (a.pins_at, &a.pins) >= (b.pins_at, &b.pins);
    let theme_from_a = (a.theme_at, &a.theme) >= (b.theme_at, &b.theme);
    let mut lists = a.lists.clone();
    for (key, theirs) in &b.lists {
        if lists.get(key).is_none_or(|mine| list_rank(theirs) > list_rank(mine)) {
            lists.insert(key.clone(), *theirs);
        }
    }
    Shared {
        nexus_music_sync: VERSION,
        pins: if pins_from_a { a.pins.clone() } else { b.pins.clone() },
        pins_at: a.pins_at.max(b.pins_at),
        theme: if theme_from_a { a.theme.clone() } else { b.theme.clone() },
        theme_at: a.theme_at.max(b.theme_at),
        lists,
        added: merge_notes(&a.added, &b.added, now),
        history: merge_history(&a.history, &b.history),
    }
}

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

impl Controller {
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
        *self.sync_playlist.borrow_mut() = Some(outcome.playlist);
        let theirs = outcome.shared;
        let (mut pins, mut theme, mut lists) = (false, false, false);
        {
            let mut settings = self.settings.borrow_mut();
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
        if !self.sync_on() || self.playback.borrow().playing {
            return;
        }
        self.request(
            |client| client.play_queue(),
            |ctl, result| {
                if let Ok(theirs) = result {
                    ctl.adopt_queue(theirs);
                }
            },
        );
    }

    fn adopt_queue(self: &Rc<Self>, theirs: PlayQueue) {
        let stamp = parse_time(&theirs.changed);
        if theirs.songs.is_empty() || theirs.changed_by == self.device_name() || stamp <= self.queue_stamp.get() {
            return;
        }
        if self.playback.borrow().playing {
            return;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::added::Note;

    fn history(entries: &[(&str, i64)]) -> Vec<(String, i64)> {
        entries.iter().map(|(id, at)| (id.to_string(), *at)).collect()
    }

    #[test]
    fn plays_from_two_devices_interleave() {
        let here = history(&[("c", 30), ("a", 10)]);
        let there = history(&[("d", 40), ("b", 20), ("a", 10)]);
        let merged = merge_history(&here, &there);
        assert_eq!(merged, history(&[("d", 40), ("c", 30), ("b", 20), ("a", 10)]));
        assert_eq!(merge_history(&there, &here), merged);
        // The same song on both devices back to back is one entry.
        assert_eq!(merge_history(&history(&[("a", 12)]), &history(&[("a", 10)])), history(&[("a", 12)]));
    }

    #[test]
    fn the_newer_choice_wins_whichever_way_round() {
        let mut here = Shared { pins: vec!["p1".into()], pins_at: 100, theme: "Ember".into(), theme_at: 50, ..Default::default() };
        here.lists.insert("playlist:x".into(), ListPrefs { shuffle: true, at: 70, ..Default::default() });
        here.lists.insert("album:y".into(), ListPrefs { sort: 1, at: 10, ..Default::default() });
        here.added.entry("x".into()).or_default().insert("s".into(), Note { times: vec![5], at: 5 });
        let mut there = Shared { pins: vec!["p2".into(), "p3".into()], pins_at: 90, theme: "Ocean".into(), theme_at: 60, ..Default::default() };
        there.lists.insert("playlist:x".into(), ListPrefs { shuffle: false, at: 80, ..Default::default() });
        there.added.entry("x".into()).or_default().insert("s".into(), Note { times: vec![], at: 9 });

        let merged = merge(&here, &there, 100);
        assert_eq!(merged, merge(&there, &here, 100));
        assert_eq!(merged.pins, ["p1"]);
        assert_eq!(merged.theme, "Ocean");
        assert!(!merged.lists["playlist:x"].shuffle);
        assert_eq!(merged.lists["album:y"].sort, 1);
        assert!(merged.added["x"]["s"].times.is_empty());
        // Combining again with either side changes nothing: the devices have settled.
        assert_eq!(merge(&merged, &here, 100), merged);
        assert_eq!(merge(&merged, &there, 100), merged);
        // It survives being written out and read back.
        let text = serde_json::to_string(&merged).unwrap();
        assert_eq!(serde_json::from_str::<Shared>(&text).unwrap(), merged);
        // Text that isn't this app's reads as nothing shared yet.
        assert_eq!(serde_json::from_str::<Shared>("hello").unwrap_or_default(), Shared::default());
    }

    #[test]
    fn choices_as_old_as_each_other_still_settle() {
        let here = Shared { pins: vec!["a".into()], pins_at: LONG_AGO, ..Default::default() };
        let there = Shared { pins: vec!["b".into()], pins_at: LONG_AGO, ..Default::default() };
        assert_eq!(merge(&here, &there, 0).pins, merge(&there, &here, 0).pins);
    }
}
