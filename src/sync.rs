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

use nexus_core::record::{self, Read};
use nexus_core::step::{Answers, Change, Choices, Device, Found, Outcome, Plan, Stated, Write, find, settle};
use nexus_core::sync::{PLAYS_SHARED, merge_plays};
pub use nexus_core::sync::{HISTORY_LENGTH, PLAYLIST_NAME};

use crate::added::Notes;
use crate::config::Played;
use crate::controller::{Controller, Route, with};
use crate::subsonic::{ApiError, ApiResult, Client, PlayQueue, Song};

/// How long after a change before it's passed on, so a burst of changes goes as one.
const SETTLE: Duration = Duration::from_secs(3);
/// How often to look for changes made elsewhere.
const LOOK: Duration = Duration::from_secs(60);
/// The file, beside the settings, that holds the shared record's text as this version first
/// found it (see `Controller::exchanged`).
pub const RECORD_KEPT: &str = "shared-record-kept.json";

// What an exchange decides is the library's (`nexus_core::step`), the same for every app:
// which playlist holds the record, what the server's answers mean, what to keep, take in and
// write. What follows is this app's part of it: asking the server, and (in `impl Controller`)
// the timers, the files, and showing what came back.

/// What this PC holds, as an exchange begins.
#[derive(Debug, Clone, Default)]
pub struct Holds {
    pub username: String,
    pub choices: Choices,
    pub notes: Notes,
    pub history: Vec<(String, i64)>,
    pub plays: Vec<(String, i64)>,
    pub play_sources: Vec<(String, i64, String)>,
}

/// Reads what the server has (the list of playlists, then the one that holds the record) and
/// has the library decide what follows from it. Nothing is written.
pub fn look(client: &Client, holds: &Holds) -> ApiResult<Outcome> {
    let listed = client.playlists_stated()?;
    let (playlist, date, received) = match find(&listed.value, &holds.username) {
        Found::Ours(id) => {
            let answer = client.playlist_stated(&id)?;
            (Some(answer.value), answer.date, answer.received)
        }
        _ => (None, listed.date, listed.received),
    };
    let device = Device {
        username: &holds.username,
        choices: holds.choices.clone(),
        notes: holds.notes.clone(),
        history: holds.history.clone(),
        plays: holds.plays.clone(),
        play_sources: holds.play_sources.clone(),
    };
    Ok(settle(&device, &Answers { listed: &listed.value, playlist: playlist.as_ref(), date, received }))
}

/// Makes a write the library asked for: the playlist first, if there is none (asked for
/// once: if the text then can't be written, the next exchange finds the playlist empty and
/// fills it). Returns the playlist it went into.
pub fn send(client: &Client, write: &Write) -> ApiResult<String> {
    let id = match &write.into {
        Some(id) => id.clone(),
        None => client.create_playlist(PLAYLIST_NAME, &[])?.id,
    };
    if id.is_empty() {
        return Err(ApiError::NotSubsonic("The new playlist came back without an id.".into()));
    }
    client.set_playlist_comment(&id, &write.text)?;
    Ok(id)
}

/// The theme the record has, without changing anything, for a first look before the window
/// opens: the playlist it was found in, the theme and when it was chosen. `known` is the
/// playlist the record was in last time, which saves listing them all if it is still that:
/// still of that name, and still stated to be the account's own.
fn peek(client: &Client, known: &str) -> Option<(String, String, i64)> {
    let ours = |playlist: &Stated| playlist.name == PLAYLIST_NAME && matches!(find(std::slice::from_ref(playlist), client.username()), Found::Ours(_));
    let remembered = (!known.is_empty()).then(|| client.playlist_stated(known).ok()).flatten().map(|answer| answer.value).filter(ours);
    let playlist = match remembered {
        Some(playlist) => playlist,
        None => match find(&client.playlists_stated().ok()?.value, client.username()) {
            Found::Ours(id) => Some(client.playlist_stated(&id).ok()?.value).filter(ours)?,
            _ => return None,
        },
    };
    let shared = match record::read(playlist.comment.as_deref().unwrap_or("")) {
        Read::Record(record) => record.shared,
        Read::Later { shared: Some(shared), .. } => shared,
        _ => return None,
    };
    Some((playlist.id, shared.theme, shared.theme_at))
}

/// Whether version `a` is later than version `b` ("0.41.0" against "0.40.4"; anything
/// against none).
fn later_version(a: &str, b: &str) -> bool {
    let parts = |version: &str| version.split('.').map(|part| part.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    !a.is_empty() && (b.is_empty() || parts(a) > parts(b))
}

/// What came back from a look at the server, with the songs of Recently played that this PC
/// hadn't met.
pub struct Looked {
    outcome: Outcome,
    songs: HashMap<String, Song>,
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
            let Some((playlist, theme, at)) = peek(&client, &known) else { return };
            crate::controller::post(move |ctl| ctl.theme_arrived(playlist, theme, at));
        });
    }

    fn theme_arrived(&self, playlist: String, theme: String, at: i64) {
        if self.client().is_none() {
            return;
        }
        let changed = {
            let mut settings = self.settings.borrow_mut();
            settings.sync_playlist = playlist;
            let before = settings.theme.clone();
            // (Agreed, as the record has it; a theme chosen here since still shows on top.)
            if crate::config::THEMES.iter().any(|t| t.name == theme) && settings.choices.theme_seen(&theme, at) {
                settings.show();
            }
            settings.theme != before
        };
        self.save_settings();
        if changed {
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
        // (An answer for a session that has since signed out is never handed back, so what
        // was waiting on one is let go here.)
        self.sync_busy.set(false);
        self.queue_saving.set(false);
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
        self.queue_saving.set(false);
        self.queue_taken_up.set(false);
        self.clock_offset.set(None);
        self.sync_says("");
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

    /// The person changed one of the choices that are shared between devices: a pin, a
    /// list's shuffle or sort, the theme. It is kept as the change, to be made at the next
    /// exchange to whatever the server has then, and it is **on the disk before it is shown
    /// as made**. Returns whether it was made (the caller then draws it).
    pub fn choose(&self, change: Change) -> bool {
        let before = self.settings.borrow().choices.clone();
        {
            let mut settings = self.settings.borrow_mut();
            settings.choices.change(change);
            settings.show();
        }
        if !self.settings.borrow().save_surely(&self.paths.settings) {
            {
                let mut settings = self.settings.borrow_mut();
                settings.choices = before;
                settings.show();
            }
            self.toast("That couldn't be saved, so it hasn't been changed.");
            return false;
        }
        self.share_soon();
        true
    }

    /// What is said under "Keep my devices in step" when this PC isn't sharing as it should,
    /// and noted in the diagnostics when it changes. Empty when all is well.
    fn sync_says(&self, words: &str) {
        if *self.sync_note.borrow() == words {
            return;
        }
        *self.sync_note.borrow_mut() = words.to_string();
        if !words.is_empty() {
            let mut said = self.said.borrow_mut();
            said.push_back((chrono::Utc::now().timestamp(), format!("In step: {words}")));
            if said.len() > 40 {
                said.pop_front();
            }
        }
        self.ui.global::<crate::ui::Settings>().set_sync_note(words.into());
    }

    /// What this PC has to share.
    fn holds(&self) -> Holds {
        let settings = self.settings.borrow();
        let playlists = self.playlists.borrow();
        let known: HashSet<&str> = playlists.iter().map(|p| p.id.as_str()).collect();
        let plays = merge_plays(&crate::plays::shared().into_iter().rev().take(PLAYS_SHARED).collect::<Vec<_>>(), &[]);
        let play_sources = {
            let kept: HashSet<(&str, i64)> = plays.iter().map(|(song, at)| (song.as_str(), *at)).collect();
            crate::plays::sources().into_iter().filter(|(song, at, _)| kept.contains(&(song.as_str(), *at))).collect()
        };
        Holds {
            username: settings.username.clone(),
            choices: settings.choices.clone(),
            // Before the playlists have loaded there's no telling which still exist.
            notes: self.added.borrow().shared((!known.is_empty()).then_some(&known)),
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
        let holds = self.holds();
        let mut songs: HashMap<String, Song> =
            self.history.borrow().iter().map(|played| (played.song.id.clone(), played.song.clone())).collect();
        self.request(
            move |client| {
                let outcome = look(client, &holds)?;
                // Songs played elsewhere that this device hasn't met.
                if let Outcome::Go(plan) = &outcome {
                    for (id, _) in &plan.take.history {
                        if !songs.contains_key(id) {
                            if let Ok(song) = client.song(id) {
                                songs.insert(id.clone(), song);
                            }
                        }
                    }
                }
                Ok(Looked { outcome, songs })
            },
            |ctl, result| match result {
                Ok(looked) => ctl.exchanged(looked),
                Err(_) => ctl.exchange_over(),
            },
        );
    }

    /// An exchange has finished, one way or another: the next may begin.
    fn exchange_over(self: &Rc<Self>) {
        self.sync_busy.set(false);
        if self.sync_again.replace(false) {
            self.share_soon();
        }
    }

    /// The server has been read and the library has decided what follows. In the order it
    /// gives: the plays are kept, on the disk, or nothing more is done; what other devices
    /// shared is taken in; then the write, if there is one, is made, and only when the
    /// server has answered that it took it is what it carried counted as agreed.
    fn exchanged(self: &Rc<Self>, looked: Looked) {
        let plan = match looked.outcome {
            Outcome::Stop(stop) => {
                self.sync_says(stop.words());
                return self.exchange_over();
            }
            Outcome::Go(plan) => plan,
        };
        // Plays counted on other devices go into this one's log, so the stats cover them
        // all; and before anything is written back that no longer has room for them.
        match self.play_log.borrow_mut().keep(&plan.keep.plays, &plan.keep.sources) {
            Ok(new) if new > 0 => self.invalidate("stats"),
            Ok(_) => {}
            Err(_) => {
                self.sync_says("What other devices shared couldn't be kept on this PC, so nothing was written. It will be asked for again.");
                return self.exchange_over();
            }
        }
        self.clock_offset.set(plan.offset);
        self.sync_says(plan.held.map(|held| held.words()).unwrap_or(""));
        self.take_shared(&plan, looked.songs);
        let Some(write) = plan.write else { return self.exchange_over() };
        // The changes it carries are marked as tried, on the disk, before it is sent: if its
        // answer never comes they are made again at the next exchange.
        self.settings.borrow_mut().choices.sending(&write);
        if !self.settings.borrow().save_surely(&self.paths.settings) {
            return self.exchange_over();
        }
        // Before the first write this version ever makes to a record, the text it found is
        // kept beside the settings, so that a mistake can be undone by hand.
        let version = env!("CARGO_PKG_VERSION");
        let keep = !write.found.is_empty() && later_version(version, &self.settings.borrow().record_kept_by);
        let kept_at = self.paths.settings.with_file_name(RECORD_KEPT);
        self.request(
            move |client| {
                if keep && crate::config::write_surely(&kept_at, write.found.as_bytes()).is_err() {
                    return Err(ApiError::Network("The shared record couldn't be kept aside first.".into()));
                }
                send(client, &write).map(|playlist| (playlist, write, keep))
            },
            move |ctl, result| {
                if let Ok((playlist, write, kept)) = result {
                    let before = ctl.shown();
                    {
                        let mut settings = ctl.settings.borrow_mut();
                        settings.choices.written(&write);
                        settings.show();
                        settings.sync_playlist = playlist;
                        if kept {
                            settings.record_kept_by = version.to_string();
                        }
                    }
                    ctl.save_settings();
                    ctl.show_choices(before);
                }
                ctl.exchange_over();
            },
        );
    }

    /// The pins, the theme and the lists' choices as shown.
    fn shown(&self) -> (Vec<String>, String, HashMap<String, crate::config::ListPrefs>) {
        let settings = self.settings.borrow();
        (settings.pinned_playlists.clone(), settings.theme.clone(), settings.lists.clone())
    }

    /// Draws whichever of the shared choices are no longer as they were `before`.
    fn show_choices(self: &Rc<Self>, before: (Vec<String>, String, HashMap<String, crate::config::ListPrefs>)) {
        let now = self.shown();
        if now.0 != before.0 {
            self.show_playlists();
        }
        if now.1 != before.1 {
            self.apply_theme();
            self.push_settings();
        }
        if now.2 != before.2 {
            self.sync_page_shuffle();
        }
    }

    /// Takes in what other devices shared.
    fn take_shared(self: &Rc<Self>, plan: &Plan, songs: HashMap<String, Song>) {
        let before = self.shown();
        let changed = {
            let mut settings = self.settings.borrow_mut();
            let was = settings.choices.clone();
            settings.choices.take(&plan.take);
            settings.show();
            // Remembered, so the next start can look at it before the window opens.
            let moved = plan.playlist.as_ref().is_some_and(|playlist| *playlist != settings.sync_playlist);
            if let Some(playlist) = &plan.playlist {
                settings.sync_playlist = playlist.clone();
            }
            moved || settings.choices != was
        };
        if changed {
            self.save_settings();
        }
        self.show_choices(before);
        if self.added.borrow_mut().merge(&plan.take.notes) && matches!(self.current_route(), Some(Route::Playlist(_))) {
            // The dates on screen may have changed.
            self.rebuild_tracks();
        }
        // Recently played: both devices' plays, in order.
        let mine: Vec<(String, i64)> =
            self.history.borrow().iter().map(|played| (played.song.id.clone(), played.at)).collect();
        if plan.take.history != mine {
            let mut known = songs;
            for played in self.history.borrow().iter() {
                known.entry(played.song.id.clone()).or_insert_with(|| played.song.clone());
            }
            let history: Vec<Played> = plan
                .take
                .history
                .iter()
                .filter_map(|(id, at)| known.get(id).map(|song| Played { song: song.clone(), at: *at }))
                .collect();
            crate::config::save_history(&self.paths.history, &history);
            *self.history.borrow_mut() = history;
            self.sync_history();
        }
    }

    // ---- the queue -----------------------------------------------------------------------------
    //
    // Whose queue is the newest is the library's to say (`nexus_core::handover`): this is the
    // timers, the requests, and putting a queue in the player.

    /// The queue changed here, or was played on: save it on the server once things have
    /// settled. (Whatever it was before, it is this PC's own now.)
    pub fn queue_changed_here(&self) {
        self.queue_taken_up.set(false);
        if self.settings.borrow().sync {
            self.queue_timer.start(slint::TimerMode::SingleShot, SETTLE, || {
                with(|ctl| ctl.upload_queue());
            });
        }
    }

    /// The time on this PC's clock as the server's clock has it, for the queue's stamp: the
    /// time a saved queue changed is the server's, and the two are compared.
    pub fn server_now(&self) -> i64 {
        nexus_core::time::aligned(chrono::Utc::now().timestamp(), self.clock_offset.get())
    }

    /// Saves this PC's queue on the server, **if it is still the newest thing that
    /// happened**. Saves take turns; immediately before each the server's saved queue is
    /// read, and the library says whether another device's stands. A save that can't be
    /// made now (no server, the setting off, the clock wrong) is still owed: the next look
    /// at the server's queue finds this one newer, and comes back here.
    fn upload_queue(self: &Rc<Self>) {
        if !self.sync_on() {
            return;
        }
        if self.queue_saving.replace(true) {
            self.queue_again.set(true);
            return;
        }
        let device = self.device_name();
        let (saved, current, playing) = {
            let pb = self.playback.borrow();
            let in_order: Vec<&Song> = pb.order.iter().filter_map(|&i| pb.queue.get(i)).collect();
            (nexus_core::handover::window(&in_order, pb.cursor, pb.position), pb.current().map(|song| song.id.clone()), pb.playing)
        };
        let Some(saved) = saved else { return self.queue_saving.set(false) };
        let (stamp, taken_up, offset) = (self.queue_stamp.get(), self.queue_taken_up.get(), self.clock_offset.get());
        self.request(
            move |client| {
                use nexus_core::handover::{Here, Save, save};
                let theirs = client.play_queue()?;
                let here = Here { device: &device, current: current.as_deref(), playing, stamp, taken_up, offset };
                match save(&theirs, &here) {
                    Save::Save => client.save_play_queue(&device, &saved.ids, &saved.current, saved.position),
                    Save::NotNewest | Save::NotNow => Ok(()),
                }
            },
            |ctl, _| {
                ctl.queue_saving.set(false);
                if ctl.queue_again.replace(false) {
                    ctl.upload_queue();
                }
            },
        );
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
                    ctl.looked_at_queue(theirs);
                }
            },
        );
    }

    /// What a look at the server's saved queue leads to: another device's offered as
    /// "Continue here" (it has been playing in the last half hour, and this PC is playing
    /// something of its own), or taken up, paused (this PC isn't playing, and nothing has
    /// been done here since); or this PC's own save, still owed.
    fn looked_at_queue(self: &Rc<Self>, theirs: PlayQueue) {
        use nexus_core::handover::{Here, Then, look};
        let device = self.device_name();
        let looked = {
            let pb = self.playback.borrow();
            let here = Here {
                device: &device,
                current: pb.current().map(|song| song.id.as_str()),
                playing: pb.playing,
                stamp: self.queue_stamp.get(),
                taken_up: self.queue_taken_up.get(),
                offset: self.clock_offset.get(),
            };
            look(&theirs, &here, chrono::Utc::now().timestamp())
        };
        let label = looked.offer.and_then(|index| theirs.songs.get(index)).map(|song| format!("{} — {}", song.title, song.artist_label())).unwrap_or_default();
        *self.elsewhere.borrow_mut() = looked.offer.is_some().then(|| theirs.clone());
        self.ui.global::<crate::ui::Player>().set_elsewhere(label.into());
        match looked.then {
            Then::TakeUp(place) => {
                self.engine.set_playing(false);
                self.engine.stop();
                self.queue_stamp.set(place.stamp);
                self.replace_queue(theirs.songs, place.cursor, place.position);
                // It is that device's still, and not this PC's to save, until something is
                // done to it here.
                self.queue_taken_up.set(true);
            }
            // (Unless a save is already on its way.)
            Then::Save if !self.queue_timer.running() => self.upload_queue(),
            Then::Save | Then::Nothing => {}
        }
    }

    /// Takes up what another device was playing, from where it had got to, and plays it.
    pub fn continue_here(self: &Rc<Self>) {
        let Some(theirs) = self.elsewhere.borrow_mut().take() else { return };
        self.ui.global::<crate::ui::Player>().set_elsewhere("".into());
        if theirs.songs.is_empty() {
            return;
        }
        let place = nexus_core::handover::place(&theirs);
        self.engine.set_playing(false);
        self.engine.stop();
        self.queue_stamp.set(place.stamp);
        self.replace_queue(theirs.songs, place.cursor, place.position);
        self.toggle_play();
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::{Arc, Mutex};

    use nexus_core::step::{Held, Stop};

    use super::*;
    use crate::subsonic::fake;

    /// A stand-in Navidrome: its playlists (id, name, owner, comment), and the `Date` it puts
    /// on its answers. Like Navidrome, it leaves an empty comment out of an answer.
    #[derive(Default)]
    struct Stored {
        playlists: Vec<(String, String, Option<String>, String)>,
        date: Option<String>,
        asked: Vec<String>,
    }

    fn decoded(text: &str) -> String {
        let bytes = text.as_bytes();
        let (mut out, mut i) = (Vec::new(), 0);
        while i < bytes.len() {
            match bytes[i] {
                b'%' if i + 2 < bytes.len() => {
                    out.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap());
                    i += 2;
                }
                b'+' => out.push(b' '),
                other => out.push(other),
            }
            i += 1;
        }
        String::from_utf8(out).unwrap()
    }

    fn navidrome() -> (Client, Arc<Mutex<Stored>>) {
        let stored = Arc::new(Mutex::new(Stored::default()));
        let held = stored.clone();
        let address = fake::server(move |target, _, stream| {
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            let value = |name: &str| query.split('&').filter_map(|pair| pair.split_once('=')).find(|(key, _)| *key == name).map(|(_, value)| decoded(value));
            let mut stored = held.lock().unwrap();
            stored.asked.push(path.trim_start_matches("/rest/").to_string());
            let said = |(id, name, owner, comment): &(String, String, Option<String>, String)| {
                let owner = owner.as_ref().map(|owner| format!(r#","owner":"{owner}""#)).unwrap_or_default();
                let comment = if comment.is_empty() { String::new() } else { format!(r#","comment":{}"#, serde_json::to_string(comment).unwrap()) };
                format!(r#"{{"id":"{id}","name":"{name}"{owner}{comment}}}"#)
            };
            let fields = match path {
                "/rest/getPlaylists" => format!(r#""playlists":{{"playlist":[{}]}}"#, stored.playlists.iter().map(said).collect::<Vec<_>>().join(",")),
                "/rest/getPlaylist" => {
                    let id = value("id").unwrap_or_default();
                    stored.playlists.iter().find(|playlist| playlist.0 == id).map(|playlist| format!(r#""playlist":{}"#, said(playlist))).unwrap_or_default()
                }
                "/rest/createPlaylist" => {
                    let made = (format!("made{}", stored.playlists.len()), value("name").unwrap_or_default(), Some("tester".to_string()), String::new());
                    stored.playlists.push(made.clone());
                    format!(r#""playlist":{}"#, said(&made))
                }
                "/rest/updatePlaylist" => {
                    let (id, comment) = (value("playlistId").unwrap_or_default(), value("comment").unwrap_or_default());
                    if let Some(playlist) = stored.playlists.iter_mut().find(|playlist| playlist.0 == id) {
                        playlist.3 = comment;
                    }
                    String::new()
                }
                _ => String::new(),
            };
            let comma = if fields.is_empty() { "" } else { "," };
            let body = format!(r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1"{comma}{fields}}}}}"#);
            let date = stored.date.as_ref().map(|date| format!("Date: {date}\r\n")).unwrap_or_default();
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{date}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        });
        (Client::new(&address, "tester", "a password"), stored)
    }

    fn holds(changes: &[Change]) -> Holds {
        let mut holds = Holds { username: "tester".into(), ..Default::default() };
        for change in changes {
            holds.choices.change(change.clone());
        }
        holds
    }

    fn plan(outcome: Outcome) -> Plan {
        match outcome {
            Outcome::Go(plan) => *plan,
            Outcome::Stop(stop) => panic!("stopped: {stop:?}"),
        }
    }

    #[test]
    fn an_exchange_through_the_apps_own_client() {
        let (client, stored) = navidrome();
        let comment = |stored: &Mutex<Stored>, id: &str| stored.lock().unwrap().playlists.iter().find(|playlist| playlist.0 == id).map(|playlist| playlist.3.clone()).unwrap_or_default();
        // Nothing on the server, and nothing to say: nothing is made.
        let looked = plan(look(&client, &holds(&[])).unwrap());
        assert!(looked.write.is_none() && looked.playlist.is_none());
        // With a pin, the playlist is made (once) and the record written into it.
        let pinned = holds(&[Change::Pin("p1".into())]);
        let write = plan(look(&client, &pinned).unwrap()).write.unwrap();
        assert_eq!(write.into, None);
        let id = send(&client, &write).unwrap();
        let Read::Record(record) = record::read(&comment(&stored, &id)) else { panic!("not a record") };
        assert_eq!(record.shared.pins, ["p1"]);
        assert_eq!(stored.lock().unwrap().asked.iter().filter(|asked| *asked == "createPlaylist").count(), 1);
        // A second device finds it by its name and owner (among others' of that name, and
        // one whose owner the server doesn't state), and its pin goes in beside the first.
        {
            let mut stored = stored.lock().unwrap();
            stored.playlists.push(("a-guests".into(), PLAYLIST_NAME.into(), Some("guest".into()), "theirs".into()));
            stored.playlists.push(("a-whose".into(), PLAYLIST_NAME.into(), None, String::new()));
        }
        let second = plan(look(&client, &holds(&[Change::Pin("p2".into())])).unwrap());
        assert_eq!(second.playlist.as_deref(), Some(id.as_str()));
        assert_eq!(send(&client, &second.write.unwrap()).unwrap(), id);
        let Read::Record(record) = record::read(&comment(&stored, &id)) else { panic!("not a record") };
        assert_eq!(record.shared.pins, ["p1", "p2"]);
        assert_eq!(comment(&stored, "a-guests"), "theirs");
        // The look before the window opens finds the theme there, by the remembered playlist
        // or, when that is no longer the account's, by looking for it.
        stored.lock().unwrap().playlists.iter_mut().find(|playlist| playlist.0 == id).unwrap().3 = r#"{"nexus_music_sync":1,"theme":"Ocean","theme_at":77}"#.into();
        for known in [id.as_str(), "", "a-guests", "gone"] {
            assert_eq!(peek(&client, known), Some((id.clone(), "Ocean".to_string(), 77)), "{known:?}");
        }
    }

    #[test]
    fn what_the_server_does_not_say_and_what_its_clock_says() {
        let (client, stored) = navidrome();
        let pinned = holds(&[Change::Pin("p1".into())]);
        // The server's clock, from the Date on its answers, against this PC's: an hour out,
        // and nothing is to be written.
        let record = r#"{"nexus_music_sync":1,"pins":["theirs"],"pins_at":5}"#;
        stored.lock().unwrap().playlists.push(("r".into(), PLAYLIST_NAME.into(), Some("Tester".into()), record.into()));
        let hour_ago = chrono::Utc::now() - chrono::Duration::hours(1);
        stored.lock().unwrap().date = Some(hour_ago.format("%a, %d %b %Y %H:%M:%S GMT").to_string());
        let looked = plan(look(&client, &pinned).unwrap());
        assert!((-3_602..=-3_598).contains(&looked.offset.unwrap()), "{:?}", looked.offset);
        assert_eq!((looked.held, looked.write.is_none(), looked.take.agreed.pins.as_slice()), (Some(Held::ClockWrong), true, ["theirs".to_string()].as_slice()));
        // The right time, and it is.
        stored.lock().unwrap().date = Some(chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string());
        let looked = plan(look(&client, &pinned).unwrap());
        assert!(looked.held.is_none() && looked.write.is_some() && looked.offset.unwrap().abs() <= 2);
        // No date at all: nothing to compare with.
        stored.lock().unwrap().date = None;
        let looked = plan(look(&client, &pinned).unwrap());
        assert!(looked.offset.is_none() && looked.write.is_some());
        // The owner left out of the answers: the playlist is not taken for the account's.
        stored.lock().unwrap().playlists[0].2 = None;
        assert_eq!(look(&client, &pinned).unwrap(), Outcome::Stop(Stop::OwnerNotStated));
        assert_eq!(peek(&client, "r"), None);
        assert_eq!(stored.lock().unwrap().playlists[0].3, record);
    }

    #[test]
    fn a_later_version_is_told_from_an_earlier_one() {
        assert!(later_version("0.40.5", "0.40.4") && later_version("0.41.0", "0.40.12") && later_version("0.40.4", ""));
        assert!(!later_version("0.40.4", "0.40.4") && !later_version("0.40.4", "0.41.0") && !later_version("", "0.40.4"));
    }
}
