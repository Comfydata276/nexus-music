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
use std::path::Path;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use slint::ComponentHandle as _;

use nexus_core::handover::{self, Clock, First, Here, Save, Then};
use nexus_core::step::{Answers, Change, Device, Found, Outcome, Plan, Take, Write, find, settle};
pub use nexus_core::sync::{HISTORY_LENGTH, PLAYLIST_NAME};

use crate::config::{Played, Settings};
use crate::controller::{Controller, Route, post, with};
use crate::subsonic::{ApiError, ApiResult, Client, PlayQueue, Song, Timed};

/// How long after a change before it's passed on, so a burst of changes goes as one.
const SETTLE: Duration = Duration::from_secs(3);
/// How often to look for changes made elsewhere.
const LOOK: Duration = Duration::from_secs(60);

// What an exchange decides is the library's (`nexus_core::step`), the same for every app:
// which playlist holds the record, what the server's answers mean, what to keep, take in and
// write, and in what order an app must do it. The numbered steps named below are its. What
// follows is this app's part: asking the server, the files, and (in `impl Controller`) the
// timers and showing what came back.

/// One thing at a time on its way to the server and back: an exchange, or a save of the
/// queue. **Something asked of the server that has been let go still counts as on its way
/// until it has answered.** Letting go means its result will not be applied; it does not
/// mean the next may start, because its write may be on its way still, and a second
/// exchange run across it would read the record from before that write and then see it
/// land. So the next waits until the earlier one's requests have all come back (they have
/// the time limits every request has).
#[derive(Default)]
pub struct Turn {
    out: std::cell::Cell<bool>,
    again: std::cell::Cell<bool>,
}

impl Turn {
    /// Asks to start. True if nothing is out: this now is. Otherwise it is noted that
    /// another turn is wanted when what is out comes back.
    #[must_use]
    pub fn begin(&self) -> bool {
        if self.out.replace(true) {
            self.again.set(true);
            return false;
        }
        true
    }

    /// Another turn is wanted after this one.
    pub fn again(&self) {
        self.again.set(true);
    }

    /// What was out has come back, whether or not anything is done with it: says whether
    /// another turn was wanted meanwhile.
    #[must_use]
    pub fn back(&self) -> bool {
        self.out.set(false);
        self.again.replace(false)
    }

    /// What is out is let go: it stays out until it comes back (`back`). Only the wish
    /// for another after it is forgotten; whoever wants one asks again.
    pub fn let_go(&self) {
        self.again.set(false);
    }
}

/// Whose the queue in the player is, and which queue a save was prepared for.
///
/// A save of the queue is prepared (the songs, the place) and then sent a moment later,
/// after a look at the server. **It is sent only if the queue is still the one it was
/// prepared for**: anything done to the queue here since, and the taking up of another
/// device's queue in its place, both make it another queue.
#[derive(Default)]
pub struct QueueOwner {
    /// Set while the queue is one taken up from another device and not yet touched here:
    /// that device's name and the time of its save (`nexus_core::handover::Here::taken`).
    taken: std::cell::RefCell<Option<String>>,
    revision: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// A save's note of the queue it was prepared for.
pub struct Prepared {
    was: u64,
    now: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Prepared {
    /// Whether the queue is still that one. Asked immediately before the save is sent.
    pub fn still(&self) -> bool {
        self.now.load(Ordering::SeqCst) == self.was
    }
}

impl QueueOwner {
    /// The queue changed here, or was played on: whatever it was, it is this PC's own now.
    pub fn changed_here(&self) {
        *self.taken.borrow_mut() = None;
        self.revision.fetch_add(1, Ordering::SeqCst);
    }

    /// Another device's queue was put in this one's place. It is that device's, and not
    /// this PC's to save, until something is done to it here.
    /// **It is a change to the queue for anything waiting to be saved**: a save prepared
    /// before it is for the queue that was replaced.
    pub fn taken_up(&self, from: String) {
        *self.taken.borrow_mut() = Some(from);
        self.revision.fetch_add(1, Ordering::SeqCst);
    }

    /// As it was left when the app last closed.
    pub fn restored(&self, taken: Option<String>) {
        *self.taken.borrow_mut() = taken;
    }

    pub fn taken(&self) -> Option<String> {
        self.taken.borrow().clone()
    }

    pub fn prepared(&self) -> Prepared {
        Prepared { was: self.revision.load(Ordering::SeqCst), now: self.revision.clone() }
    }
}

/// Step 1: asks the server for the list of playlists and for the one that holds the record.
/// Nothing of what this PC holds is needed for it, and nothing is written.
pub fn read(client: &Client) -> ApiResult<Answers> {
    let listed = client.playlists_stated()?;
    let (playlist, date, received) = match find(&listed.value, client.username()) {
        Found::Ours(id) => {
            let answer = client.playlist_stated(&id)?;
            (Some(answer.value), answer.date, answer.received)
        }
        _ => (None, listed.date, listed.received),
    };
    Ok(Answers { listed: listed.value, playlist, date, received })
}

/// Step 7.3: makes a write the library asked for: the playlist first, if there is none
/// (asked for once: if the text then can't be written, the next exchange finds the playlist
/// empty and fills it). `still` is asked immediately before **each** request that changes
/// the server, the second as well as the first: whether it is still the same sign-in, with
/// sharing on and the app not shutting down. Returns the playlist the record went into;
/// `None` if `still` said no, and then nothing more was sent.
pub fn send(client: &Client, write: &Write, still: &dyn Fn() -> bool) -> ApiResult<Option<String>> {
    let id = match &write.into {
        Some(id) => id.clone(),
        None if still() => client.create_playlist(PLAYLIST_NAME, &[])?.id,
        None => return Ok(None),
    };
    if id.is_empty() {
        return Err(ApiError::NotSubsonic("The new playlist came back without an id.".into()));
    }
    if !still() {
        return Ok(None);
    }
    client.set_playlist_comment(&id, &write.text)?;
    Ok(Some(id))
}

/// The theme the record has, for a first look before the window opens, and when it was
/// chosen. Found exactly as an exchange finds the record (the same reads, the same
/// decision, for a device with nothing of its own to say); nothing is written.
fn peek(client: &Client) -> Option<(String, i64)> {
    let answers = read(client).ok()?;
    let device = Device { username: client.username().to_string(), ..Default::default() };
    match settle(&device, &answers) {
        Outcome::Go(plan) => Some((plan.take.agreed.theme, plan.take.agreed.theme_at)),
        Outcome::Stop(_) => None,
    }
}

/// The file, beside the settings, that holds an account's shared record as a version of the
/// app first found it. A later version's copy goes beside an earlier one's and never over
/// it: the earliest is the record from before any of them wrote. `account` is the account
/// as part of a file's name (`Settings::account_file`).
pub fn kept_file(settings: &Path, account: &str, version: &str) -> std::path::PathBuf {
    settings.with_file_name(format!("shared-record-kept-{account}-{version}.json"))
}

/// Step 7.1: before the first write this version makes to an account's record, the text as
/// it was found (the empty text too: an empty file) is put on the disk, so that a mistake
/// can be undone by hand. **The copy is its own proof.** If the file is there, the record
/// has been kept, and the file is never opened for writing again; if it is not, it is made,
/// only if it is still not there, and flushed. An error means it isn't there: then nothing
/// may be sent. Nothing else is asked or noted.
pub fn keep_first(settings: &Path, account: &str, version: &str, found: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let path = kept_file(settings, account, version);
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            let written = file.write_all(found.as_bytes()).and_then(|()| file.sync_all());
            if written.is_err() {
                // (This call's own file, made a moment ago and not what was found: it must
                // not pass for the copy.)
                drop(file);
                let _ = std::fs::remove_file(&path);
            }
            written
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_file() => Ok(()),
        Err(error) => Err(error),
    }
}

/// What the window is to be told after a step of an exchange has changed the settings.
#[derive(Debug, Default, PartialEq)]
pub struct Told {
    /// The pins, the theme, a list's choices are no longer as they were shown.
    pub pins: bool,
    pub theme: bool,
    pub lists: bool,
    /// The settings want saving.
    pub save: bool,
}

fn shown(settings: &Settings) -> (Vec<String>, String, HashMap<String, crate::config::ListPrefs>) {
    (settings.pinned_playlists.clone(), settings.theme.clone(), settings.lists.clone())
}

fn told(settings: &mut Settings, change: impl FnOnce(&mut Settings)) -> Told {
    let (before, was, known) = (shown(settings), settings.choices.clone(), settings.sync_playlist.clone());
    change(settings);
    settings.show();
    let now = shown(settings);
    Told { pins: now.0 != before.0, theme: now.1 != before.1, lists: now.2 != before.2, save: settings.choices != was || settings.sync_playlist != known }
}

/// Step 6, for the settings: what other devices shared is taken in.
pub fn take_in(settings: &mut Settings, plan: &Plan) -> Told {
    told(settings, |settings| {
        settings.choices.take(&plan.take);
        // (Remembered: the record is in this playlist.)
        if let Some(playlist) = &plan.playlist {
            settings.sync_playlist = playlist.clone();
        }
    })
}

/// Step 7.4, for the settings: the server has answered that it took the write.
pub fn written(settings: &mut Settings, write: &Write, playlist: &str) -> Told {
    told(settings, |settings| {
        settings.choices.written(write);
        settings.sync_playlist = playlist.to_string();
    })
}

/// Step 6, for Recently played: what this PC has **now** put together with what the plan
/// brought, each entry with its song. `songs` are any the server was asked for. `None` when
/// that is what this PC has already.
pub fn merged_history(live: &[Played], take: &Take, songs: &HashMap<String, Song>) -> Option<Vec<Played>> {
    let mine: Vec<(String, i64)> = live.iter().map(|played| (played.song.id.clone(), played.at)).collect();
    let merged = nexus_core::step::history(&mine, take);
    if merged == mine {
        return None;
    }
    let known = |id: &str| live.iter().find(|played| played.song.id == id).map(|played| &played.song).or_else(|| songs.get(id));
    Some(merged.into_iter().filter_map(|(id, at)| known(&id).map(|song| Played { song: song.clone(), at })).collect())
}

/// What asking the server for a song played on another device came to.
pub enum Looked {
    Found(Box<Song>),
    /// The server says there is no such song. Only this is remembered for the rest of the
    /// run, so that it isn't asked for at every exchange.
    Gone,
    /// It couldn't be had this time (the request timed out, the server failed in some
    /// other way): it is asked for again at the next exchange.
    NotNow,
}

pub fn looked(id: &str, answer: ApiResult<Song>) -> Looked {
    match answer {
        Ok(song) if song.id == id => Looked::Found(Box::new(song)),
        // (Subsonic's "the requested data was not found".)
        Err(ApiError::Server { code: 70, .. }) => Looked::Gone,
        _ => Looked::NotNow,
    }
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
        let session = self.session.load(Ordering::SeqCst);
        self.api_pool.spawn(move || {
            let Some((theme, at)) = peek(&client) else { return };
            post(move |ctl| ctl.theme_arrived(session, theme, at));
        });
    }

    fn theme_arrived(&self, session: u64, theme: String, at: i64) {
        if session != self.session.load(Ordering::SeqCst) || self.client().is_none() {
            return;
        }
        let told = {
            let mut settings = self.settings.borrow_mut();
            let known = crate::config::THEMES.iter().any(|t| t.name == theme);
            // (Agreed, as the record has it; a theme chosen here since still shows on top.)
            told(&mut settings, |settings| {
                if known {
                    settings.choices.theme_seen(&theme, at);
                }
            })
        };
        self.tell(&told);
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
        handover::device_name(&self.settings.borrow().device_id)
    }

    /// Whatever is on its way for the sign-in (or the setting) as it was is let go: an
    /// answer that arrives for it is dropped, and nothing of it is applied or sent. It is
    /// still on its way until it has come back, and the next waits for that (`Turn`). The
    /// app shutting down lets go too, from its first moment.
    pub fn let_go(&self) {
        self.session.fetch_add(1, Ordering::SeqCst);
        self.exchange.let_go();
        self.queue_save.let_go();
    }

    /// Starts keeping in step: now, and every so often from here on.
    pub fn start_sync(self: &Rc<Self>) {
        self.let_go();
        // (What is shown is the choices as this sign-in has them.)
        self.settings.borrow_mut().show();
        self.look_elsewhere();
        self.sync_look_timer.start(slint::TimerMode::Repeated, LOOK, || {
            with(|ctl| ctl.look_elsewhere());
        });
    }

    pub fn stop_sync(&self) {
        self.sync_look_timer.stop();
        self.sync_timer.stop();
        self.queue_timer.stop();
        self.let_go();
        self.clock.set(Clock::Unmeasured);
        self.sync_says("");
    }

    /// "Keep my devices in step" was switched: off, nothing already on its way is applied
    /// or sent; on, a look is made at once.
    pub fn sync_switched(self: &Rc<Self>, on: bool) {
        self.let_go();
        if on {
            self.look_elsewhere();
        } else {
            self.sync_timer.stop();
            self.queue_timer.stop();
            self.sync_says("");
        }
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
    /// list's shuffle or sort, the theme. It is kept as the change, made under the account
    /// signed in, to be made at the next exchange to whatever the server has then; and it
    /// is **on the disk before it is shown as made**. Returns whether it was made (the
    /// caller then draws it).
    pub fn choose(&self, change: Change) -> bool {
        let before = self.settings.borrow().choices.clone();
        {
            let mut settings = self.settings.borrow_mut();
            let account = settings.account();
            settings.choices.change(&account, change);
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

    /// Step 3: what this PC holds, now.
    fn device(&self) -> Device {
        let settings = self.settings.borrow();
        let playlists = self.playlists.borrow();
        let known: HashSet<&str> = playlists.iter().map(|p| p.id.as_str()).collect();
        // The newest plays by when they were played, wherever in the log they lie.
        let (plays, play_sources) = crate::plays::newest();
        Device {
            username: settings.username.clone(),
            account: settings.account(),
            choices: settings.choices.clone(),
            // Before the playlists have loaded there's no telling which still exist.
            notes: self.added.borrow().shared((!known.is_empty()).then_some(&known)),
            history: self.history.borrow().iter().map(|played| (played.song.id.clone(), played.at)).collect(),
            play_sources,
            plays,
        }
    }

    /// Step 1 of an exchange: the server is asked. Nothing this PC holds is looked at until
    /// the answers are back (`answered`), so whatever the person does meanwhile is in what
    /// is then decided.
    fn share_now(self: &Rc<Self>) {
        if !self.sync_on() {
            return;
        }
        let Some(client) = self.client() else { return };
        if !self.exchange.begin() {
            // One exchange at a time, one let go included: again when it is back.
            return;
        }
        let session = self.session.load(Ordering::SeqCst);
        self.api_pool.spawn(move || {
            let answers = read(&client);
            post(move |ctl| ctl.answered(session, client, answers));
        });
    }

    /// An exchange has finished, one way or another (applied, stopped, or let go and now
    /// back): the next may begin.
    fn exchange_over(self: &Rc<Self>) {
        if self.exchange.back() {
            self.share_soon();
        }
    }

    /// Draws whichever of the shared choices a step changed, and saves the settings if it
    /// changed them.
    fn tell(&self, told: &Told) {
        if told.save {
            self.save_settings();
        }
        if told.pins {
            self.show_playlists();
        }
        if told.theme {
            self.apply_theme();
            self.push_settings();
        }
        if told.lists {
            self.sync_page_shuffle();
            // A sort chosen on another device redraws the list that is open in that sort.
            self.resort_page();
        }
    }

    /// Steps 2 to 7 of an exchange: the server's answers are in hand. `client` is the
    /// connection of the sign-in that asked, and `session` says which that was: if it is
    /// no longer the one signed in, or sharing has been switched off, everything here is
    /// dropped.
    fn answered(self: &Rc<Self>, session: u64, client: Client, answers: ApiResult<Answers>) {
        // Step 2. (Let go: dropped. But it has come back, so the next may begin.)
        if session != self.session.load(Ordering::SeqCst) {
            return self.exchange_over();
        }
        if !self.sync_on() {
            return self.exchange_over();
        }
        let Ok(answers) = answers else { return self.exchange_over() };
        self.clock.set(nexus_core::step::clock(&answers));
        // Step 3: decided from what this PC holds now.
        let plan = match settle(&self.device(), &answers) {
            // Step 4.
            Outcome::Stop(stop) => {
                self.sync_says(stop.words());
                return self.exchange_over();
            }
            Outcome::Go(plan) => *plan,
        };
        // Step 5: plays counted on other devices go into this one's log, so the stats cover
        // them all; and before anything is written back that no longer has room for them.
        match self.play_log.borrow_mut().keep(&plan.keep.plays, &plan.keep.sources) {
            Ok(new) if new > 0 => self.invalidate("stats"),
            Ok(_) => {}
            Err(_) => {
                self.sync_says("What other devices shared couldn't be kept on this PC, so nothing was written. It will be asked for again.");
                return self.exchange_over();
            }
        }
        self.sync_says(plan.held.map(|held| held.words()).unwrap_or(""));
        // Step 6.
        let told = take_in(&mut self.settings.borrow_mut(), &plan);
        self.tell(&told);
        if self.added.borrow_mut().merge(&plan.take.notes) && matches!(self.current_route(), Some(Route::Playlist(_))) {
            // The dates on screen may have changed.
            self.rebuild_tracks();
        }
        // Recently played: songs played elsewhere that this PC hasn't met are asked for
        // (one the server says it doesn't have, once in a run), and the list is put
        // together when they are back.
        let wanted: Vec<String> = {
            let (history, missing) = (self.history.borrow(), self.history_missing.borrow());
            let known: HashSet<&str> = history.iter().map(|played| played.song.id.as_str()).collect();
            let mut wanted: Vec<String> = plan.take.history.iter().map(|(id, _)| id).filter(|id| !known.contains(id.as_str()) && !missing.contains(*id)).cloned().collect();
            wanted.dedup();
            wanted
        };
        if wanted.is_empty() {
            self.take_history(&plan.take, &HashMap::new());
        }
        // Step 7.
        let mut write = plan.write;
        if let Some(carries) = &write {
            // 7.1: the record as this version first found it is kept, or nothing is sent.
            let account = self.settings.borrow().account_file();
            if keep_first(&self.paths.settings, &account, env!("CARGO_PKG_VERSION"), &carries.found).is_err() {
                self.sync_says("A copy of the shared record couldn't be kept on this PC first, so nothing was written.");
                write = None;
            }
        }
        if let Some(carries) = &write {
            // 7.2: the plan must still fit the choices; and they are saved, as tried,
            // before the request is made.
            if !self.settings.borrow_mut().choices.sending(carries) {
                self.exchange.again();
                write = None;
            } else if !self.settings.borrow().save_surely(&self.paths.settings) {
                write = None;
            }
        }
        if write.is_none() && wanted.is_empty() {
            return self.exchange_over();
        }
        let live = self.session.clone();
        let take = plan.take;
        self.api_pool.spawn(move || {
            // 7.3: still the same sign-in, with sharing on, immediately before each request
            // that changes the server. And the server's answer to the write is taken note
            // of as soon as it has come, not after the songs below have been asked for.
            if let Some(write) = write {
                let went = send(&client, &write, &|| live.load(Ordering::SeqCst) == session).ok().flatten();
                post(move |ctl| ctl.wrote(session, &write, went));
            }
            let songs: Vec<(String, Looked)> = wanted.into_iter().map(|id| (id.clone(), looked(&id, client.song(&id)))).collect();
            post(move |ctl| ctl.finished(session, take, songs));
        });
    }

    /// Step 7.4: the server has answered that it took the write. Only now is what the write
    /// carried agreed, and its changes done with.
    fn wrote(self: &Rc<Self>, session: u64, write: &Write, went: Option<String>) {
        if session != self.session.load(Ordering::SeqCst) {
            return;
        }
        if let Some(playlist) = went {
            let told = written(&mut self.settings.borrow_mut(), write, &playlist);
            self.tell(&told);
        }
    }

    /// The end of an exchange that had something to send or to ask for.
    fn finished(self: &Rc<Self>, session: u64, take: Take, songs: Vec<(String, Looked)>) {
        if session != self.session.load(Ordering::SeqCst) {
            return self.exchange_over();
        }
        let mut found = HashMap::new();
        for (id, song) in songs {
            match song {
                Looked::Found(song) => drop(found.insert(id, *song)),
                Looked::Gone => drop(self.history_missing.borrow_mut().insert(id)),
                Looked::NotNow => {}
            }
        }
        self.take_history(&take, &found);
        self.exchange_over();
    }

    /// Recently played, with what other devices played: put together with what is here at
    /// this moment (a song started since the server was asked stays).
    fn take_history(self: &Rc<Self>, take: &Take, songs: &HashMap<String, Song>) {
        let merged = merged_history(&self.history.borrow(), take, songs);
        if let Some(history) = merged {
            crate::config::save_history(&self.paths.history, &history);
            *self.history.borrow_mut() = history;
            self.sync_history();
        }
    }

    // ---- the queue -----------------------------------------------------------------------------
    //
    // Whose queue is the newest is the library's to say (`nexus_core::handover`), and its
    // numbered steps are named below. This is the timers, the requests, and putting a queue
    // in the player.

    /// The queue changed here, or was played on: save it on the server once things have
    /// settled. (Whatever it was before, it is this PC's own now.)
    pub fn queue_changed_here(&self) {
        self.queue_owner.changed_here();
        if self.settings.borrow().sync {
            self.queue_timer.start(slint::TimerMode::SingleShot, SETTLE, || {
                with(|ctl| ctl.upload_queue());
            });
        }
    }

    /// This PC's side of a comparison of queues.
    fn here(&self) -> Here {
        let pb = self.playback.borrow();
        Here {
            device: self.device_name(),
            current: pb.current().map(|song| song.id.clone()),
            playing: pb.playing,
            stamp: self.queue_stamp.get(),
            taken: self.queue_owner.taken(),
        }
    }

    /// A save of this PC's queue: made **only if it is still the newest thing that
    /// happened**. Saves take turns. While a song plays the queue is this PC's to keep, and
    /// is saved without a look; otherwise the server's saved queue is read first, with the
    /// server's clock as that read gives it, and the library says whether another device's
    /// stands. Immediately before sending, the queue must still be the one this save was
    /// for, the sign-in the same and sharing on. A save that can't be made now is still
    /// owed: the next look at the server's queue finds this one newer, and comes back here.
    fn upload_queue(self: &Rc<Self>) {
        if !self.sync_on() {
            return;
        }
        let here = self.here();
        let saved = {
            let pb = self.playback.borrow();
            let in_order: Vec<&Song> = pb.order.iter().filter_map(|&i| pb.queue.get(i)).collect();
            handover::window(&in_order, pb.cursor, pb.position)
        };
        let (Some(saved), Some(client)) = (saved, self.client()) else { return };
        if !self.queue_save.begin() {
            return;
        }
        let (session, live) = (self.session.load(Ordering::SeqCst), self.session.clone());
        let prepared = self.queue_owner.prepared();
        self.api_pool.spawn(move || {
            // Steps 1 and 2.
            let (go, clock) = match handover::first(&here) {
                First::Save => (true, None),
                First::Read => match client.play_queue_timed() {
                    Ok(theirs) => {
                        let clock = Clock::measured(theirs.date, theirs.received);
                        (handover::save(&theirs.value, &here, clock) == Save::Save, Some(clock))
                    }
                    Err(_) => (false, None),
                },
            };
            // Step 3.
            if go && live.load(Ordering::SeqCst) == session && prepared.still() {
                let _ = client.save_play_queue(&here.device, &saved.ids, &saved.current, saved.position);
            }
            post(move |ctl| {
                // (Back, whether or not it was let go meanwhile: the next may begin.)
                let again = ctl.queue_save.back();
                if session != ctl.session.load(Ordering::SeqCst) {
                    return;
                }
                if let Some(clock) = clock {
                    ctl.clock.set(clock);
                }
                if again {
                    ctl.upload_queue();
                }
            });
        });
    }

    /// A look at the server's saved queue.
    pub fn check_server_queue(self: &Rc<Self>) {
        if !self.sync_on() {
            return;
        }
        let Some(client) = self.client() else { return };
        let session = self.session.load(Ordering::SeqCst);
        self.api_pool.spawn(move || {
            let Ok(theirs) = client.play_queue_timed() else { return };
            post(move |ctl| {
                if session == ctl.session.load(Ordering::SeqCst) && ctl.sync_on() {
                    ctl.looked_at_queue(theirs);
                }
            });
        });
    }

    /// What a look at the server's saved queue leads to: another device's offered as
    /// "Continue here" (it has been playing in the last half hour, and this PC is playing
    /// something of its own), or taken up, paused (this PC isn't playing, and nothing has
    /// been done here since); or this PC's own save, still owed.
    fn looked_at_queue(self: &Rc<Self>, theirs: Timed<PlayQueue>) {
        let clock = Clock::measured(theirs.date, theirs.received);
        self.clock.set(clock);
        let theirs = theirs.value;
        let looked = handover::look(&theirs, &self.here(), clock, chrono::Utc::now().timestamp());
        let label = looked.offer.and_then(|index| theirs.songs.get(index)).map(|song| format!("{} — {}", song.title, song.artist_label())).unwrap_or_default();
        // (Kept with the clock it was read by, for "Continue here".)
        *self.elsewhere.borrow_mut() = looked.offer.is_some().then(|| (theirs.clone(), clock));
        self.ui.global::<crate::ui::Player>().set_elsewhere(label.into());
        match looked.then {
            Then::TakeUp(place) => self.take_up(theirs, place, false),
            // (Unless a save is already on its way.)
            Then::Save if !self.queue_timer.running() => self.upload_queue(),
            Then::Save | Then::Nothing => {}
        }
    }

    /// Puts another device's queue in this one's place. It is that device's still, and not
    /// this PC's to save, until something is done to it here: which playing it is.
    fn take_up(self: &Rc<Self>, theirs: PlayQueue, place: handover::Place, play: bool) {
        if theirs.songs.is_empty() {
            return;
        }
        self.engine.set_playing(false);
        self.engine.stop();
        self.queue_stamp.set(place.stamp);
        self.queue_owner.taken_up(place.taken);
        self.replace_queue(theirs.songs, place.cursor, place.position);
        if play {
            self.toggle_play();
        }
    }

    /// Takes up what another device was playing, from where it had got to, and plays it.
    pub fn continue_here(self: &Rc<Self>) {
        let Some((theirs, clock)) = self.elsewhere.borrow_mut().take() else { return };
        self.ui.global::<crate::ui::Player>().set_elsewhere("".into());
        let place = handover::place(&theirs, clock);
        self.take_up(theirs, place, true);
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

    /// A PC signed in as the stand-in's account, with these changes made.
    fn pc(changes: &[Change]) -> Settings {
        let mut settings = Settings { server: "https://music.example.org".into(), username: "tester".into(), in_step: 1, ..Default::default() };
        for change in changes {
            settings.choices.change(&settings.account(), change.clone());
        }
        settings.show();
        settings
    }

    /// What `Controller::device` hands the library, for a PC with nothing but its settings.
    fn device(settings: &Settings) -> Device {
        Device { username: settings.username.clone(), account: settings.account(), choices: settings.choices.clone(), ..Default::default() }
    }

    /// Steps 1 and 3: the server is asked, and then the decision is made.
    fn decided(client: &Client, settings: &Settings) -> Outcome {
        settle(&device(settings), &read(client).unwrap())
    }

    fn plan(outcome: Outcome) -> Plan {
        match outcome {
            Outcome::Go(plan) => *plan,
            Outcome::Stop(stop) => panic!("stopped: {stop:?}"),
        }
    }

    fn pins(settings: &Settings) -> Vec<&str> {
        settings.pinned_playlists.iter().map(String::as_str).collect()
    }

    #[test]
    fn a_save_prepared_before_another_devices_queue_was_taken_up_is_not_sent() {
        let owner = QueueOwner::default();
        // A save is prepared: the songs and the place as they are, the server about to be
        // looked at. Nothing happens meanwhile: it may be sent.
        let save = owner.prepared();
        assert!(save.still());
        // Meanwhile a look at the server finds another device's newer queue, and it is put
        // in this one's place. The save is for a queue that is no longer in the player:
        // sent, it would put the old queue over the newer one, as the newest.
        owner.taken_up("NexusMusic-0badc0de@1700000000".into());
        assert!(!save.still(), "a save of the queue that was replaced would still be sent");
        assert_eq!(owner.taken().as_deref(), Some("NexusMusic-0badc0de@1700000000"));
        // Anything done to the queue here is the same to a save prepared before it; and
        // the queue is then this PC's own.
        let save = owner.prepared();
        owner.changed_here();
        assert!(!save.still() && owner.taken().is_none());
        // Putting back, at start-up, what was in the player is no change to anything.
        let save = owner.prepared();
        owner.restored(Some("NexusMusic-0badc0de@1700000000".into()));
        assert!(save.still() && owner.taken().is_some());
    }

    #[test]
    fn only_the_servers_no_such_song_is_remembered() {
        let song = Song { id: "s1".into(), ..Default::default() };
        assert!(matches!(looked("s1", Ok(song.clone())), Looked::Found(found) if found.id == "s1"));
        // The server's own "not found" is a definite no.
        assert!(matches!(looked("s1", Err(ApiError::Server { code: 70, message: "Song not found".into() })), Looked::Gone));
        // A request that timed out, an answer that wasn't the server's, a refusal for some
        // other reason, a different song altogether: none says the song is gone, and it is
        // asked for again at the next exchange, as 0.40.4 asked.
        for answer in [
            Err(ApiError::Network("timed out".into())),
            Err(ApiError::NotSubsonic("a proxy's page".into())),
            Err(ApiError::Server { code: 0, message: "a generic error".into() }),
            Err(ApiError::Server { code: 50, message: "not authorised".into() }),
            Ok(Song { id: "another".into(), ..Default::default() }),
        ] {
            assert!(matches!(looked("s1", answer), Looked::NotNow));
        }
    }

    #[test]
    fn what_was_let_go_is_still_on_its_way_until_it_has_answered() {
        let turn = Turn::default();
        assert!(turn.begin());
        // Sharing is switched off, and on again at once. The exchange that was let go may
        // have a write on its way still: the next does not start across it.
        turn.let_go();
        assert!(!turn.begin(), "a second exchange would run across the first's write");
        // It comes back (to be dropped), and the one that was wanted may begin.
        assert!(turn.back());
        assert!(turn.begin());
        // Let go with nothing wanted after it: nothing follows when it comes back, and
        // what was wanted before the letting go is forgotten with it.
        assert!(!turn.begin());
        turn.let_go();
        assert!(!turn.back());
        assert!(turn.begin() && !turn.back());
    }

    #[test]
    fn each_request_that_changes_the_server_is_asked_about_first() {
        let (client, stored) = navidrome();
        let write = plan(decided(&client, &pc(&[Change::Pin("p1".into())]))).write.unwrap();
        assert_eq!(write.into, None);
        let asked = |what: &str| stored.lock().unwrap().asked.iter().filter(|asked| *asked == what).count();
        // No longer the same sign-in, or sharing is off: nothing is sent.
        assert_eq!(send(&client, &write, &|| false).unwrap(), None);
        assert_eq!((asked("createPlaylist"), asked("updatePlaylist")), (0, 0));
        // It stopped being so while the playlist was being made: the record is not then
        // written into it.
        let times = std::cell::Cell::new(0);
        let once = || times.replace(times.get() + 1) == 0;
        assert_eq!(send(&client, &write, &once).unwrap(), None);
        assert_eq!((asked("createPlaylist"), asked("updatePlaylist"), times.get()), (1, 0, 2));
        assert!(stored.lock().unwrap().playlists.iter().all(|playlist| playlist.3.is_empty()));
    }

    #[test]
    fn an_exchange_through_the_apps_own_client() {
        let (client, stored) = navidrome();
        let comment = |stored: &Mutex<Stored>, id: &str| stored.lock().unwrap().playlists.iter().find(|playlist| playlist.0 == id).map(|playlist| playlist.3.clone()).unwrap_or_default();
        // Nothing on the server, and nothing to say: nothing is made.
        let looked = plan(decided(&client, &pc(&[])));
        assert!(looked.write.is_none() && looked.playlist.is_none());
        // With a pin, the playlist is made (once) and the record written into it.
        let write = plan(decided(&client, &pc(&[Change::Pin("p1".into())]))).write.unwrap();
        assert_eq!(write.into, None);
        let id = send(&client, &write, &|| true).unwrap().unwrap();
        assert!(comment(&stored, &id).contains(r#""pins":["p1"]"#));
        assert_eq!(stored.lock().unwrap().asked.iter().filter(|asked| *asked == "createPlaylist").count(), 1);
        // A second device finds it by its name and owner (among others' of that name, and
        // one whose owner the server doesn't state), and its pin goes in beside the first.
        {
            let mut stored = stored.lock().unwrap();
            stored.playlists.push(("a-guests".into(), PLAYLIST_NAME.into(), Some("guest".into()), "theirs".into()));
            stored.playlists.push(("a-whose".into(), PLAYLIST_NAME.into(), None, String::new()));
        }
        let second = plan(decided(&client, &pc(&[Change::Pin("p2".into())])));
        assert_eq!(second.playlist.as_deref(), Some(id.as_str()));
        assert_eq!(send(&client, &second.write.unwrap(), &|| true).unwrap().as_ref(), Some(&id));
        assert!(comment(&stored, &id).contains(r#""pins":["p1","p2"]"#));
        assert_eq!(comment(&stored, "a-guests"), "theirs");
        // The look before the window opens finds the theme there: in the playlist an
        // exchange would use, and no other. A higher-id playlist of the account's with
        // another theme in it is not looked at.
        stored.lock().unwrap().playlists.iter_mut().find(|playlist| playlist.0 == id).unwrap().3 = r#"{"nexus_music_sync":1,"theme":"Ocean","theme_at":77}"#.into();
        stored.lock().unwrap().playlists.push(("z-higher".into(), PLAYLIST_NAME.into(), Some("tester".into()), r#"{"nexus_music_sync":1,"theme":"Rose","theme_at":99}"#.into()));
        assert_eq!(peek(&client), Some(("Ocean".to_string(), 77)));
        // And not at all where an exchange would stop.
        stored.lock().unwrap().playlists.iter_mut().find(|playlist| playlist.0 == id).unwrap().3 = "somebody's note".into();
        assert_eq!(peek(&client), None);
    }

    #[test]
    fn what_the_server_does_not_say_and_what_its_clock_says() {
        let (client, stored) = navidrome();
        let pinned = pc(&[Change::Pin("p1".into())]);
        // The server's clock, from the Date on its answers, against this PC's: an hour out,
        // and nothing is to be written.
        let record = r#"{"nexus_music_sync":1,"pins":["theirs"],"pins_at":5}"#;
        stored.lock().unwrap().playlists.push(("r".into(), PLAYLIST_NAME.into(), Some("Tester".into()), record.into()));
        let hour_ago = chrono::Utc::now() - chrono::Duration::hours(1);
        stored.lock().unwrap().date = Some(hour_ago.format("%a, %d %b %Y %H:%M:%S GMT").to_string());
        let answers = read(&client).unwrap();
        assert!(matches!(nexus_core::step::clock(&answers), Clock::Offset(offset) if (-3_602..=-3_598).contains(&offset)));
        let looked = plan(settle(&device(&pinned), &answers));
        assert_eq!((looked.held, looked.write.is_none(), looked.take.agreed.pins.as_slice()), (Some(Held::ClockWrong), true, ["theirs".to_string()].as_slice()));
        // The right time, and it is.
        stored.lock().unwrap().date = Some(chrono::Utc::now().format("%a, %d %b %Y %H:%M:%S GMT").to_string());
        let looked = plan(decided(&client, &pinned));
        assert!(looked.held.is_none() && looked.write.is_some() && looked.offset.unwrap().abs() <= 2);
        // No date at all: nothing to compare with.
        stored.lock().unwrap().date = None;
        let answers = read(&client).unwrap();
        assert_eq!(nexus_core::step::clock(&answers), Clock::NoDate);
        assert!(plan(settle(&device(&pinned), &answers)).write.is_some());
        // The owner left out of the answers: the playlist is not taken for the account's.
        stored.lock().unwrap().playlists[0].2 = None;
        assert_eq!(decided(&client, &pinned), Outcome::Stop(Stop::OwnerNotStated));
        assert_eq!(peek(&client), None);
        assert_eq!(stored.lock().unwrap().playlists[0].3, record);
    }

    // ---- applying a plan to the settings: each kind, and a failure part-way ---------------------

    /// The server's record with these pins and this theme, as its answers state it.
    fn answers(pins: &[&str], theme: &str) -> Answers {
        let comment = format!(r#"{{"nexus_music_sync":1,"pins":{},"pins_at":50,"theme":"{theme}","theme_at":60,"lists":{{"liked":{{"sort":2,"sort_chosen":true,"at":70}}}}}}"#, serde_json::to_string(pins).unwrap());
        let stated = nexus_core::step::Stated { id: "r".into(), name: PLAYLIST_NAME.into(), owner: Some("tester".into()), comment: Some(comment) };
        Answers { listed: vec![stated.clone()], playlist: Some(stated), date: Some(1_800_000_000), received: 1_800_000_000 }
    }

    #[test]
    fn a_plan_with_nothing_to_write_is_taken_in_and_the_window_told_what_changed() {
        let mut settings = pc(&[]);
        let plan = plan(settle(&device(&settings), &answers(&["p1", "p2"], "Ocean")));
        assert!(plan.write.is_none());
        let told = take_in(&mut settings, &plan);
        assert_eq!(told, Told { pins: true, theme: true, lists: true, save: true });
        assert_eq!((pins(&settings), settings.theme.as_str(), settings.lists["liked"].sort, settings.sync_playlist.as_str()), (vec!["p1", "p2"], "Ocean", 2, "r"));
        // The same again changes nothing, and says so: nothing to draw, nothing to save.
        let plan = super::tests::plan(settle(&device(&settings), &answers(&["p1", "p2"], "Ocean")));
        assert_eq!(take_in(&mut settings, &plan), Told::default());
        // Only what changed is drawn: the pins, here.
        let plan = super::tests::plan(settle(&device(&settings), &{
            let mut answers = answers(&["p1"], "Ocean");
            let comment = answers.playlist.as_ref().unwrap().comment.clone().unwrap().replace(r#""pins_at":50"#, r#""pins_at":51"#);
            answers.playlist.as_mut().unwrap().comment = Some(comment.clone());
            answers.listed[0].comment = Some(comment);
            answers
        }));
        assert_eq!(take_in(&mut settings, &plan), Told { pins: true, theme: false, lists: false, save: true });
        // A theme this version doesn't have is agreed, and what is shown stays as it was.
        let mut newer = answers(&["p1"], "Neon");
        let comment = newer.listed[0].comment.clone().unwrap().replace(r#""pins_at":50"#, r#""pins_at":51"#).replace(r#""theme_at":60"#, r#""theme_at":61"#);
        (newer.listed[0].comment, newer.playlist.as_mut().unwrap().comment) = (Some(comment.clone()), Some(comment));
        let plan = super::tests::plan(settle(&device(&settings), &newer));
        let told = take_in(&mut settings, &plan);
        assert_eq!((told.theme, settings.theme.as_str(), settings.choices.agreed.theme.as_str()), (false, "Ocean", "Neon"));
    }

    #[test]
    fn a_plan_with_a_write_is_agreed_only_once_the_server_has_answered() {
        let mut settings = pc(&[Change::Pin("mine".into())]);
        assert_eq!(pins(&settings), ["mine"]);
        let plan = plan(settle(&device(&settings), &answers(&["p1"], "Ocean")));
        // Taken in: the server's pin shows, with this PC's still on top, and still waiting.
        let told = take_in(&mut settings, &plan);
        assert!(told.pins && told.save);
        assert_eq!((pins(&settings), settings.choices.waiting.len(), settings.choices.agreed.pins.len()), (vec!["p1", "mine"], 1, 1));
        // Sent: marked as tried. The answer never comes (the app is closed, the line drops):
        // nothing is agreed that the server may not have, and the change is still there.
        let write = plan.write.unwrap();
        assert!(settings.choices.sending(&write));
        let closed: Settings = serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!((closed.choices.agreed.pins.len(), closed.choices.waiting.len(), closed.choices.waiting[0].tried), (1, 1, true));
        assert_eq!(pins(&closed), ["p1", "mine"]);
        // The answer comes: what the write carried is agreed, nothing waits, and nothing on
        // the screen changes by it.
        let told = written(&mut settings, &write, "r");
        assert_eq!(told, Told { pins: false, theme: false, lists: false, save: true });
        assert_eq!((settings.choices.agreed.pins.len(), settings.choices.waiting.len()), (2, 0));
        assert_eq!(pins(&settings), ["p1", "mine"]);
    }

    #[test]
    fn a_plan_made_before_a_change_was_taken_back_is_not_sent() {
        // The reviewer's case, as the app's own steps: pin, decide, unpin, then come to send.
        let mut settings = pc(&[Change::Pin("c".into())]);
        let plan = plan(settle(&device(&settings), &answers(&["p1"], "Ocean")));
        take_in(&mut settings, &plan);
        let account = settings.account();
        settings.choices.change(&account, Change::Unpin("c".into()));
        settings.show();
        assert_eq!(pins(&settings), ["p1"]);
        assert!(!settings.choices.sending(&plan.write.unwrap()));
        assert!(settings.choices.waiting.is_empty() && settings.choices.agreed.pins == ["p1"]);
    }

    #[test]
    fn recently_played_is_put_together_with_what_is_here_when_it_is_taken_in() {
        let song = |id: &str| Song { id: id.into(), title: format!("Song {id}"), ..Default::default() };
        let played = |id: &str, at: i64| Played { song: song(id), at };
        let take = Take { history: vec![("theirs".into(), 150), ("x".into(), 100), ("gone".into(), 90)], ..Default::default() };
        // When the plan was made this PC had only x. Since then y has started here.
        let live = [played("y", 200), played("x", 100)];
        let songs = HashMap::from([("theirs".to_string(), song("theirs"))]);
        let merged = merged_history(&live, &take, &songs).unwrap();
        // y stays, at the top; theirs comes in with its song; one whose song couldn't be had
        // is left out, as it always was.
        assert_eq!(merged.iter().map(|played| (played.song.id.as_str(), played.at)).collect::<Vec<_>>(), [("y", 200), ("theirs", 150), ("x", 100)]);
        assert_eq!(merged[1].song.title, "Song theirs");
        // With nothing new from the others there is nothing to write.
        assert!(merged_history(&live, &Take { history: vec![("x".into(), 100)], ..Default::default() }, &HashMap::new()).is_none());
    }

    #[test]
    fn the_record_as_first_found_is_kept_before_anything_is_sent_and_never_written_again() {
        let dir = std::env::temp_dir().join(format!("nexus-kept-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
        let first = "shared-record-kept-tester-0.40.5.json";
        // The first write this version makes for the account: the text as found is there.
        keep_first(&settings, "tester", "0.40.5", "the record as 0.40.4 left it").unwrap();
        assert_eq!(read(first).as_deref(), Some("the record as 0.40.4 left it"));
        // The copy is its own proof: nothing else is written to say that it was kept, and
        // nothing else is asked. Its later writes (the answer to the first never came; the
        // record is by now this version's own text) leave the copy as it is.
        let files = || std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(files(), 1);
        keep_first(&settings, "tester", "0.40.5", "what this version wrote").unwrap();
        assert_eq!((read(first).as_deref(), files()), (Some("the record as 0.40.4 left it"), 1));
        // An empty record is kept as an empty file: a later write does not keep this
        // version's own text as "the original".
        keep_first(&settings, "another", "0.40.5", "").unwrap();
        keep_first(&settings, "another", "0.40.5", "what this version wrote").unwrap();
        assert_eq!(read("shared-record-kept-another-0.40.5.json").as_deref(), Some(""));
        // Each account has its own. A later version keeps what it finds, once, in a file of
        // its own: the copy an earlier version made is the one from before either of them
        // wrote, and is the one wanted when a mistake of the earlier version is found only
        // after the update, so it stays as it is.
        keep_first(&settings, "tester", "0.41.0", "as 0.40.5 left it").unwrap();
        assert_eq!(read("shared-record-kept-tester-0.41.0.json").as_deref(), Some("as 0.40.5 left it"));
        assert_eq!(read(first).as_deref(), Some("the record as 0.40.4 left it"));
        // Going back to the earlier version changes neither copy.
        keep_first(&settings, "tester", "0.40.5", "as 0.41.0 left it").unwrap();
        assert_eq!(read(first).as_deref(), Some("the record as 0.40.4 left it"));
        // If the copy can't be made, that is an error: nothing is sent on the strength of
        // it. (A folder stands where the file should be; then, no folder to put it in.)
        std::fs::create_dir_all(dir.join("shared-record-kept-blocked-0.40.5.json").join("in the way")).unwrap();
        assert!(keep_first(&settings, "blocked", "0.40.5", "x").is_err());
        assert!(keep_first(&dir.join("no such folder").join("settings.json"), "tester", "0.40.5", "x").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
