//! The play queue and transport: what plays next, shuffle and repeat, seeking, play counts
//! and lyrics. All on the UI thread; the audio itself is the engine's business.

use std::collections::HashSet;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use crate::audio::engine::{Event, TrackRequest};
use crate::config::{SavedQueue, Settings, VOLUME_LEVELS};
use crate::controller::{Controller, Route, with};
use crate::subsonic::{Lyrics, Song};
use crate::ui;

/// Start fetching the following track once this much of the current one remains.
const PRELOAD_SECONDS: f64 = 25.0;
/// Rows shown in the queue panel.
const QUEUE_ROWS: usize = 400;
/// Songs remembered for "Recently played".
const HISTORY_LENGTH: usize = 100;
/// File types that are always lossless. (M4A and CAF can be either; those are judged by
/// whether the server reports a bit depth, which lossy formats don't have.)
const LOSSLESS_SUFFIXES: [&str; 4] = ["flac", "wav", "aiff", "aif"];
/// File types the built-in decoders handle; anything else is converted by the server.
pub const NATIVE_SUFFIXES: [&str; 12] =
    ["mp3", "flac", "m4a", "aac", "ogg", "oga", "wav", "aiff", "aif", "mka", "caf", "mp4"];

pub struct Playback {
    pub queue: Vec<Song>,
    /// Indices into `queue`, in the order they play.
    pub order: Vec<usize>,
    /// Position of the current track within `order`.
    pub cursor: usize,
    pub context: String,
    pub shuffle: bool,
    pub repeat: u8,
    pub playing: bool,
    pub volume: f32,
    pub muted: bool,
    pub position: f64,
    token_counter: u64,
    /// Engine token of the current track; 0 when the engine has nothing loaded.
    current_token: u64,
    next_token: u64,
    next_cursor: usize,
    /// Where to start when the current track is first loaded (a restored session).
    resume_at: f64,
    /// The current track is being converted by the server, so seeking means reloading.
    transcoded: bool,
    listened: f64,
    last_tick: f64,
    scrobbled: bool,
    /// Track position when the queue was last written to disk.
    saved_at: f64,
    /// Songs whose original files the decoders rejected.
    needs_transcode: HashSet<String>,
    failures: u32,
    /// Similar songs are being fetched to continue after the queue.
    autoplay_loading: bool,
    /// The queue ran out while those songs were still on their way: start them on arrival.
    autoplay_resume: bool,
    /// The track that last triggered an early autoplay lookup, so each track asks only once.
    autoplay_asked: u64,
}

impl Playback {
    pub fn new(settings: &Settings) -> Playback {
        Playback {
            queue: Vec::new(),
            order: Vec::new(),
            cursor: 0,
            context: String::new(),
            shuffle: settings.shuffle,
            repeat: settings.repeat.min(2),
            playing: false,
            volume: settings.volume.clamp(0.0, 1.0),
            muted: settings.muted,
            position: 0.0,
            token_counter: 0,
            current_token: 0,
            next_token: 0,
            next_cursor: 0,
            resume_at: 0.0,
            transcoded: false,
            listened: 0.0,
            last_tick: 0.0,
            scrobbled: false,
            saved_at: 0.0,
            needs_transcode: HashSet::new(),
            failures: 0,
            autoplay_loading: false,
            autoplay_resume: false,
            autoplay_asked: 0,
        }
    }

    pub fn current(&self) -> Option<&Song> {
        self.order.get(self.cursor).and_then(|&i| self.queue.get(i))
    }

    /// The track that follows when the current one ends by itself.
    fn following(&self) -> Option<usize> {
        if self.order.is_empty() {
            None
        } else if self.repeat == 2 {
            Some(self.cursor)
        } else if self.cursor + 1 < self.order.len() {
            Some(self.cursor + 1)
        } else if self.repeat == 1 {
            Some(0)
        } else {
            None
        }
    }

    fn new_token(&mut self) -> u64 {
        self.token_counter += 1;
        self.token_counter
    }

    /// Puts the current track first and shuffles the rest behind it.
    fn shuffle_order(&mut self) {
        let Some(&current) = self.order.get(self.cursor) else { return };
        let mut rest: Vec<usize> = (0..self.queue.len()).filter(|&i| i != current).collect();
        fastrand::shuffle(&mut rest);
        self.order = std::iter::once(current).chain(rest).collect();
        self.cursor = 0;
    }
}

#[derive(Default)]
pub struct LyricsState {
    song_id: String,
    /// Start time of each line, for timed lyrics.
    times: Vec<f64>,
    active: i32,
}

impl Controller {
    // ---- starting playback ---------------------------------------------------------------------

    /// Replaces the queue and starts playing from `start`.
    pub fn play_songs(self: &Rc<Self>, songs: Vec<Song>, start: usize, context: &str, shuffle: bool) {
        // Offline, only what's downloaded can play: skip the rest.
        let (songs, start) = if self.is_offline() {
            let wanted = songs.get(start).map(|s| s.id.clone());
            let downloads = self.downloads.borrow();
            let kept: Vec<Song> = songs.into_iter().filter(|s| downloads.has(&s.id)).collect();
            let start = wanted.and_then(|id| kept.iter().position(|s| s.id == id)).unwrap_or(0);
            (kept, start)
        } else {
            (songs, start)
        };
        if songs.is_empty() {
            if self.is_offline() {
                self.toast("That isn't downloaded, so it can't play while you're offline");
            }
            return;
        }
        {
            let mut pb = self.playback.borrow_mut();
            if shuffle && !pb.shuffle {
                pb.shuffle = true;
                self.settings.borrow_mut().shuffle = true;
            }
            pb.order = (0..songs.len()).collect();
            // The Shuffle button means "surprise me", so it picks the opening track too.
            pb.cursor = if shuffle { fastrand::usize(..songs.len()) } else { start.min(songs.len() - 1) };
            pb.queue = songs;
            pb.context = context.to_string();
            if pb.shuffle {
                pb.shuffle_order();
            }
            pb.playing = true;
            pb.failures = 0;
            pb.autoplay_resume = false;
        }
        self.save_settings();
        self.start_current(0.0);
    }

    /// Loads the track at the cursor into the engine.
    fn start_current(self: &Rc<Self>, at: f64) {
        let Some(client) = self.client() else { return };
        let (request, playing) = {
            let mut pb = self.playback.borrow_mut();
            let Some(song) = pb.current().cloned() else { return };
            let token = pb.new_token();
            pb.current_token = token;
            pb.next_token = 0;
            pb.position = at;
            pb.resume_at = 0.0;
            pb.listened = 0.0;
            pb.last_tick = at;
            pb.scrobbled = false;
            let request = self.request_for(&mut pb, &song, token, at);
            pb.transcoded = !request.ranged;
            (request, pb.playing)
        };
        self.engine.load(client, request);
        self.engine.set_playing(playing);
        self.ui.global::<ui::Player>().set_buffering(true);
        self.track_changed();
    }

    fn request_for(&self, pb: &mut Playback, song: &Song, token: u64, at: f64) -> TrackRequest {
        let client = self.client().expect("signed in");
        let settings = self.settings.borrow();
        let suffix = song.suffix.to_lowercase();
        let native = NATIVE_SUFFIXES.contains(&suffix.as_str()) && !pb.needs_transcode.contains(&song.id);
        let cap = settings.max_bitrate();
        // Whether this song sits beside another from the same album in the play order.
        let album_run = !song.album_id.is_empty()
            && pb.order.iter().position(|&i| pb.queue.get(i).is_some_and(|s| s.id == song.id)).is_some_and(|at| {
                let beside = |at: Option<usize>| {
                    at.and_then(|at| pb.order.get(at))
                        .and_then(|&i| pb.queue.get(i))
                        .is_some_and(|other| other.album_id == song.album_id)
                };
                beside(at.checked_sub(1)) || beside(Some(at + 1))
            });
        let gain = level_gain(&settings, song, album_run);
        let duration_secs = song.duration as f64;
        if !song.radio_url.is_empty() {
            // A live stream: no length, no seeking, and nothing to normalise against.
            let address = song.radio_url.to_lowercase();
            let suffix = ["aac", "ogg", "flac"].into_iter().find(|ext| address.contains(ext)).unwrap_or("mp3");
            return TrackRequest {
                token,
                local: None,
                live: true,
                url: song.radio_url.clone(),
                ranged: false,
                suffix: suffix.into(),
                gain: 1.0,
                start_secs: 0.0,
                base_secs: 0.0,
                duration_secs: 0.0,
            };
        }
        if let Some(path) = self.downloads.borrow().path_of(&song.id) {
            // A downloaded copy plays from disk, whatever the streaming quality is set to.
            let suffix = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            return TrackRequest {
                token,
                local: Some(path),
                live: false,
                url: String::new(),
                ranged: true,
                suffix,
                gain,
                start_secs: at,
                base_secs: 0.0,
                duration_secs,
            };
        }
        if native && cap == 0 {
            TrackRequest {
                token,
                local: None,
                live: false,
                url: client.stream_url(&song.id, 0, 0),
                ranged: true,
                suffix,
                gain,
                start_secs: at,
                base_secs: 0.0,
                duration_secs,
            }
        } else {
            // The server skips ahead for us; the stream then starts at `offset`.
            let offset = at.floor().max(0.0);
            TrackRequest {
                token,
                local: None,
                live: false,
                url: client.stream_url(&song.id, if cap == 0 { 320 } else { cap }, offset as u32),
                ranged: false,
                suffix: "mp3".into(),
                gain,
                start_secs: 0.0,
                base_secs: offset,
                duration_secs,
            }
        }
    }

    /// Tells the engine which track to run into next, so the handover is gapless.
    fn arm_next(self: &Rc<Self>) {
        let Some(client) = self.client() else { return };
        let request = {
            let mut pb = self.playback.borrow_mut();
            let Some(cursor) = pb.following() else { return };
            let Some(song) = pb.order.get(cursor).and_then(|&i| pb.queue.get(i)).cloned() else {
                return;
            };
            let token = pb.new_token();
            pb.next_token = token;
            pb.next_cursor = cursor;
            self.request_for(&mut pb, &song, token, 0.0)
        };
        self.engine.set_next(Some((client, request)));
    }

    /// Call after anything that changes what should play next.
    fn next_changed(self: &Rc<Self>) {
        let (armed, next_token, position) = {
            let pb = self.playback.borrow();
            (pb.next_token != 0, pb.next_token, pb.position)
        };
        if armed {
            if self.engine.decoding_token() == next_token {
                // The engine already ran on into the old next track; start this one over.
                self.start_current(position);
            } else {
                self.playback.borrow_mut().next_token = 0;
                self.engine.set_next(None);
            }
        }
        self.sync_queue();
    }

    // ---- transport -----------------------------------------------------------------------------

    pub fn toggle_play(self: &Rc<Self>) {
        let (loaded, resume_at) = {
            let mut pb = self.playback.borrow_mut();
            if pb.current().is_none() {
                return;
            }
            pb.playing = !pb.playing;
            (pb.current_token != 0, pb.resume_at)
        };
        let rejoin = {
            let pb = self.playback.borrow();
            pb.playing && pb.current().is_some_and(|s| !s.radio_url.is_empty())
        };
        if rejoin {
            // A radio station carries on while paused: resuming joins it as it is now.
            self.start_current(0.0);
        } else if loaded {
            self.engine.set_playing(self.playback.borrow().playing);
            self.sync_transport();
        } else {
            self.start_current(resume_at);
        }
        if !self.playback.borrow().playing {
            self.save_queue();
        }
    }

    /// Used by the media keys and the sleep timer.
    pub fn set_playing(self: &Rc<Self>, playing: bool) {
        if self.playback.borrow().playing != playing {
            self.toggle_play();
        }
    }

    pub fn next_track(self: &Rc<Self>) {
        {
            let mut pb = self.playback.borrow_mut();
            if pb.order.is_empty() {
                return;
            }
            if pb.cursor + 1 < pb.order.len() {
                pb.cursor += 1;
            } else {
                // Past the end: wrap to the top, and only keep playing if repeat is on.
                pb.cursor = 0;
                if pb.repeat == 0 {
                    pb.playing = false;
                }
            }
        }
        self.start_current(0.0);
    }

    pub fn previous_track(self: &Rc<Self>) {
        {
            let mut pb = self.playback.borrow_mut();
            if pb.order.is_empty() {
                return;
            }
            // Like a CD player: a few seconds in, "previous" restarts the track.
            if pb.position > 3.0 || pb.cursor == 0 {
                drop(pb);
                return self.seek(0.0);
            }
            pb.cursor -= 1;
        }
        self.start_current(0.0);
    }

    pub fn seek(self: &Rc<Self>, seconds: f64) {
        let (loaded, transcoded, token, duration) = {
            let mut pb = self.playback.borrow_mut();
            let Some(duration) = pb.current().map(|s| s.duration as f64) else { return };
            if pb.current().is_some_and(|s| !s.radio_url.is_empty()) {
                // A live stream has no earlier or later to go to.
                return;
            }
            let seconds = seconds.clamp(0.0, (duration - 1.0).max(0.0));
            pb.position = seconds;
            pb.last_tick = seconds;
            pb.resume_at = seconds;
            (pb.current_token != 0, pb.transcoded, pb.current_token, duration)
        };
        let seconds = seconds.clamp(0.0, (duration - 1.0).max(0.0));
        self.ui.global::<ui::Player>().set_position(seconds as f32);
        if !loaded {
            return;
        }
        if transcoded || self.engine.decoding_token() != token {
            self.start_current(seconds);
        } else {
            self.engine.seek(seconds);
        }
    }

    pub fn stop_playback(self: &Rc<Self>) {
        {
            let mut pb = self.playback.borrow_mut();
            pb.queue.clear();
            pb.order.clear();
            pb.cursor = 0;
            pb.playing = false;
            pb.current_token = 0;
            pb.next_token = 0;
            pb.position = 0.0;
        }
        self.engine.set_playing(false);
        self.engine.stop();
        self.track_changed();
    }

    fn set_volume(self: &Rc<Self>, volume: f32) {
        {
            let mut pb = self.playback.borrow_mut();
            pb.volume = volume.clamp(0.0, 1.0);
            pb.muted = false;
            let mut settings = self.settings.borrow_mut();
            settings.volume = pb.volume;
            settings.muted = false;
        }
        self.sync_transport();
    }

    fn toggle_shuffle(self: &Rc<Self>) {
        {
            let mut pb = self.playback.borrow_mut();
            pb.shuffle = !pb.shuffle;
            self.settings.borrow_mut().shuffle = pb.shuffle;
            if pb.shuffle {
                pb.shuffle_order();
            } else if let Some(&current) = pb.order.get(pb.cursor) {
                pb.order = (0..pb.queue.len()).collect();
                pb.cursor = current;
            }
        }
        self.save_settings();
        self.sync_transport();
        self.next_changed();
    }

    fn cycle_repeat(self: &Rc<Self>) {
        {
            let mut pb = self.playback.borrow_mut();
            pb.repeat = (pb.repeat + 1) % 3;
            self.settings.borrow_mut().repeat = pb.repeat;
        }
        self.save_settings();
        self.sync_transport();
        self.next_changed();
    }

    // ---- queue editing -------------------------------------------------------------------------

    /// Adds songs right after the current track, or at the end of the queue.
    pub fn enqueue(self: &Rc<Self>, songs: Vec<Song>, play_next: bool) {
        if songs.is_empty() {
            return;
        }
        let was_empty = self.playback.borrow().order.is_empty();
        if was_empty {
            // Nothing queued yet: this becomes the queue, ready to play.
            {
                let mut pb = self.playback.borrow_mut();
                pb.order = (0..songs.len()).collect();
                pb.queue = songs;
                pb.cursor = 0;
                pb.context.clear();
                pb.playing = false;
                pb.current_token = 0;
                pb.resume_at = 0.0;
                pb.position = 0.0;
            }
            self.track_changed();
            return;
        }
        {
            let mut pb = self.playback.borrow_mut();
            let mut at = if play_next { pb.cursor + 1 } else { pb.order.len() };
            for song in songs {
                pb.queue.push(song);
                let index = pb.queue.len() - 1;
                pb.order.insert(at, index);
                at += 1;
            }
        }
        self.next_changed();
    }

    /// Removes the entry `offset` places after the current track.
    pub fn remove_upcoming(self: &Rc<Self>, offset: usize) {
        {
            let mut pb = self.playback.borrow_mut();
            let at = pb.cursor + 1 + offset;
            if at >= pb.order.len() {
                return;
            }
            let removed = pb.order.remove(at);
            pb.queue.remove(removed);
            for index in pb.order.iter_mut() {
                if *index > removed {
                    *index -= 1;
                }
            }
        }
        self.next_changed();
    }

    /// Moves the entry `from` places after the current track to `to` places after it.
    fn move_upcoming(self: &Rc<Self>, from: usize, to: usize) {
        {
            let mut pb = self.playback.borrow_mut();
            let base = pb.cursor + 1;
            let (from, to) = (base + from, base + to);
            if from == to || from >= pb.order.len() || to >= pb.order.len() {
                return;
            }
            let entry = pb.order.remove(from);
            pb.order.insert(to, entry);
        }
        self.next_changed();
    }

    fn clear_upcoming(self: &Rc<Self>) {
        {
            let mut pb = self.playback.borrow_mut();
            let Some(song) = pb.current().cloned() else { return };
            pb.queue = vec![song];
            pb.order = vec![0];
            pb.cursor = 0;
        }
        self.next_changed();
    }

    fn jump_to_upcoming(self: &Rc<Self>, offset: usize) {
        {
            let mut pb = self.playback.borrow_mut();
            let at = pb.cursor + 1 + offset;
            if at >= pb.order.len() {
                return;
            }
            pb.cursor = at;
            pb.playing = true;
        }
        self.start_current(0.0);
    }

    // ---- engine events and the clock -----------------------------------------------------------

    pub fn on_engine_event(self: &Rc<Self>, event: Event) {
        match event {
            Event::Started(token) => {
                let advanced = {
                    let mut pb = self.playback.borrow_mut();
                    pb.failures = 0;
                    if token != 0 && token == pb.next_token {
                        // The engine rolled into the next track by itself.
                        pb.cursor = pb.next_cursor;
                        pb.current_token = token;
                        pb.next_token = 0;
                        pb.position = 0.0;
                        pb.listened = 0.0;
                        pb.last_tick = 0.0;
                        pb.scrobbled = false;
                        let cap = self.settings.borrow().max_bitrate();
                        let downloads = self.downloads.borrow();
                        pb.transcoded = pb.current().is_some_and(|s| {
                            !downloads.has(&s.id)
                                && (cap != 0
                                    || !NATIVE_SUFFIXES.contains(&s.suffix.to_lowercase().as_str())
                                    || pb.needs_transcode.contains(&s.id))
                        });
                        true
                    } else if token == pb.current_token {
                        false
                    } else {
                        return;
                    }
                };
                self.ui.global::<ui::Player>().set_buffering(false);
                if advanced {
                    self.track_changed();
                }
                self.record_played();
                self.report_now_playing();
            }
            Event::Finished => {
                let next = {
                    let mut pb = self.playback.borrow_mut();
                    if pb.current_token == 0 {
                        return;
                    }
                    // The sleep timer asked to stop here: line up what's next, paused.
                    if self.sleep_after_song.replace(false) {
                        pb.cursor = pb.following().unwrap_or(pb.cursor);
                        pb.playing = false;
                        drop(pb);
                        self.sync_sleep();
                        self.toast("Sleep timer: paused");
                        return self.start_current(0.0);
                    }
                    let radio = pb.current().is_some_and(|s| !s.radio_url.is_empty());
                    match pb.following() {
                        Some(cursor) => {
                            pb.cursor = cursor;
                            true
                        }
                        None if self.settings.borrow().autoplay && !radio => {
                            // Similar songs are on their way (or about to be asked for);
                            // carry on with them when they land.
                            pb.autoplay_resume = true;
                            drop(pb);
                            return self.load_autoplay();
                        }
                        None => {
                            // End of the queue: park on the first track, ready to go again.
                            pb.cursor = 0;
                            pb.playing = false;
                            pb.current_token = 0;
                            pb.position = 0.0;
                            pb.resume_at = 0.0;
                            false
                        }
                    }
                };
                if next {
                    self.start_current(0.0);
                } else {
                    self.engine.set_playing(false);
                    self.engine.stop();
                    self.track_changed();
                }
            }
            Event::StreamTitle { token, title } => {
                let station = {
                    let pb = self.playback.borrow();
                    pb.current().filter(|s| token == pb.current_token && !s.radio_url.is_empty()).cloned()
                };
                if let Some(station) = station {
                    // The station's name stays as the title; the song goes where the artist would.
                    let player = self.ui.global::<ui::Player>();
                    let mut track = player.get_track();
                    track.artist = if title.is_empty() { station.artist_label().into() } else { title.into() };
                    player.set_track(track);
                }
            }
            Event::Output { status, exclusive } => {
                self.ui.global::<ui::Settings>().set_output_status(status.into());
                self.output_exclusive.set(exclusive);
                self.show_format();
            }
            Event::Failed { token, message, unsupported } => {
                let (retry, title, position, give_up) = {
                    let mut pb = self.playback.borrow_mut();
                    if token != pb.current_token {
                        return;
                    }
                    let Some(song) = pb.current().cloned() else { return };
                    // A downloaded copy that won't play is discarded, and the song streamed.
                    if self.downloads.borrow().has(&song.id) {
                        let position = pb.position;
                        drop(pb);
                        self.remove_downloads(std::slice::from_ref(&song.id));
                        return self.start_current(position);
                    }
                    // First try having the server convert a file we can't decode.
                    let retry = unsupported && pb.needs_transcode.insert(song.id.clone());
                    if !retry {
                        pb.failures += 1;
                    }
                    (retry, song.title, pb.position, pb.failures >= 3 || pb.following().is_none())
                };
                if retry {
                    self.start_current(position);
                    return;
                }
                self.toast(format!("Couldn't play “{title}”. {message}"));
                if give_up {
                    let mut pb = self.playback.borrow_mut();
                    pb.playing = false;
                    pb.current_token = 0;
                    drop(pb);
                    self.engine.set_playing(false);
                    self.engine.stop();
                    self.sync_transport();
                } else {
                    self.next_track();
                }
            }
        }
    }

    /// Runs four times a second: moves the progress bar, counts listening time, queues up the
    /// next track and follows the lyrics.
    pub fn tick(self: &Rc<Self>) {
        self.check_sleep();
        // Keep the maximise button, and the title bar's sense of scale, in step with the window.
        let app = self.ui.global::<ui::App>();
        let maximized = self.ui.window().is_maximized();
        if app.get_maximized() != maximized {
            app.set_maximized(maximized);
        }
        #[cfg(windows)]
        crate::frame::set_scale(self.ui.window().scale_factor());
        let Some((token, seconds)) = self.engine.position() else { return };
        let lookahead;
        let (arm, submit, checkpoint) = {
            let mut pb = self.playback.borrow_mut();
            if token != pb.current_token || !pb.playing {
                return;
            }
            let Some(song) = pb.current().cloned() else { return };
            let duration = song.duration as f64;
            pb.position = if duration > 0.0 { seconds.min(duration) } else { seconds };
            let delta = seconds - pb.last_tick;
            if delta > 0.0 && delta < 2.0 {
                pb.listened += delta;
            }
            pb.last_tick = seconds;
            // A play counts after half the track or four minutes, whichever comes first.
            let threshold = if duration > 0.0 { (duration / 2.0).min(240.0) } else { 240.0 };
            let submit = (!pb.scrobbled && pb.listened >= threshold && song.radio_url.is_empty()).then(|| {
                pb.scrobbled = true;
                song.id.clone()
            });
            let near_end = duration > 0.0 && duration - seconds <= PRELOAD_SECONDS;
            let (seamless, autoplay) = {
                let settings = self.settings.borrow();
                (settings.gapless || settings.crossfade > 0.0, settings.autoplay)
            };
            // With gapless and crossfade both off, the next track simply loads when this one
            // has finished, which leaves a short natural pause.
            // (Nor does it run on when the sleep timer is waiting for this song to end.)
            let arm = pb.next_token == 0
                && near_end
                && seamless
                && pb.following().is_some()
                && !self.sleep_after_song.get();
            lookahead = near_end
                && autoplay
                && pb.following().is_none()
                && pb.autoplay_asked != pb.current_token;
            if lookahead {
                pb.autoplay_asked = pb.current_token;
            }
            // Note the position now and then, so a crash or power cut loses little.
            let checkpoint = (seconds - pb.saved_at).abs() >= 20.0;
            if checkpoint {
                pb.saved_at = seconds;
            }
            (arm, submit, checkpoint)
        };
        let position = self.playback.borrow().position;
        self.ui.global::<ui::Player>().set_position(position as f32);
        if arm {
            self.arm_next();
        }
        if lookahead {
            // The last track is nearly over: line up what plays after it.
            self.load_autoplay();
        }
        if checkpoint {
            self.save_queue();
        }
        if let Some(id) = submit {
            if self.settings.borrow().scrobble {
                // Sent now, or kept with its time and sent when the server is next reachable.
                let at = chrono::Utc::now().timestamp_millis();
                self.submit(crate::offline::Pending::Played { id, at }, |_, _| {});
            }
        }
        self.follow_lyrics(position);
        #[cfg(windows)]
        self.media_position(position);
    }

    fn report_now_playing(self: &Rc<Self>) {
        if !self.settings.borrow().scrobble {
            return;
        }
        let id = self.playback.borrow().current().filter(|s| s.radio_url.is_empty()).map(|s| s.id.clone());
        if let Some(id) = id {
            self.request(move |client| client.scrobble(&id, false), |_, _| {});
        }
    }

    // ---- autoplay -----------------------------------------------------------------------------

    /// Extends the queue with music like its last song. Safe to call repeatedly.
    fn load_autoplay(self: &Rc<Self>) {
        let seed = {
            let mut pb = self.playback.borrow_mut();
            if pb.autoplay_loading {
                return;
            }
            let Some(seed) = pb.order.last().and_then(|&i| pb.queue.get(i)).cloned() else { return };
            pb.autoplay_loading = true;
            seed
        };
        let known: HashSet<String> =
            self.playback.borrow().queue.iter().map(|s| s.id.clone()).collect();
        self.request(
            move |client| {
                // Best match first: songs the server considers similar. That needs Last.fm or
                // another source set up in Navidrome, so fall back to the same genre, then to
                // anything at all.
                let fresh = |songs: Vec<Song>| -> Vec<Song> {
                    songs.into_iter().filter(|s| !known.contains(&s.id)).collect()
                };
                let mut songs = fresh(client.similar_songs(&seed.id, 40).unwrap_or_default());
                if songs.len() < 5 && !seed.genre.is_empty() {
                    songs.extend(fresh(client.random_songs(30, Some(&seed.genre)).unwrap_or_default()));
                }
                if songs.len() < 5 {
                    songs.extend(fresh(client.random_songs(30, None)?));
                }
                let mut seen = HashSet::new();
                songs.retain(|s| seen.insert(s.id.clone()));
                songs.truncate(30);
                Ok(songs)
            },
            |ctl, result| ctl.autoplay_loaded(result.unwrap_or_default()),
        );
    }

    fn autoplay_loaded(self: &Rc<Self>, songs: Vec<Song>) {
        let (resume, found) = {
            let mut pb = self.playback.borrow_mut();
            pb.autoplay_loading = false;
            let resume = std::mem::take(&mut pb.autoplay_resume);
            // The queue may have been replaced while the request was out.
            let still_at_end = pb.following().is_none() && !pb.order.is_empty();
            if !songs.is_empty() && still_at_end {
                for song in songs {
                    pb.queue.push(song);
                    let index = pb.queue.len() - 1;
                    pb.order.push(index);
                }
                // What follows no longer comes from the album or playlist that was chosen.
                pb.context.clear();
                (resume, true)
            } else {
                (resume && still_at_end, false)
            }
        };
        if found {
            let context = self.playback.borrow().context.clone();
            self.ui.global::<ui::Player>().set_context(context.as_str().into());
            self.sync_queue();
            if resume {
                self.playback.borrow_mut().cursor += 1;
                self.start_current(0.0);
            }
        } else if resume {
            // Nothing to continue with: stop, parked on the first track.
            {
                let mut pb = self.playback.borrow_mut();
                pb.cursor = 0;
                pb.playing = false;
                pb.current_token = 0;
                pb.position = 0.0;
                pb.resume_at = 0.0;
            }
            self.engine.set_playing(false);
            self.engine.stop();
            self.track_changed();
        }
    }

    // ---- sleep timer ---------------------------------------------------------------------------

    /// Sets the sleep timer: a number of minutes, 0 for off, or `None` for "when this song ends".
    pub fn set_sleep(self: &Rc<Self>, minutes: Option<u32>) {
        self.sleep_after_song.set(minutes.is_none());
        self.sleep_at.set(
            minutes.filter(|m| *m > 0).map(|m| Instant::now() + Duration::from_secs(m as u64 * 60)),
        );
        if minutes.is_none() {
            // Don't let the engine run on into the next song.
            self.next_changed();
        }
        self.sync_sleep();
        self.toast(match minutes {
            None => "Music will pause when this song ends".to_string(),
            Some(0) => "Sleep timer off".to_string(),
            Some(m) => format!("Music will pause in {m} minutes"),
        });
    }

    fn check_sleep(self: &Rc<Self>) {
        let Some(at) = self.sleep_at.get() else { return };
        if Instant::now() >= at {
            self.sleep_at.set(None);
            self.set_playing(false);
            self.toast("Sleep timer: paused");
        }
        self.sync_sleep();
    }

    /// Shows how long the sleep timer has left.
    pub fn sync_sleep(&self) {
        let label = match (self.sleep_at.get(), self.sleep_after_song.get()) {
            (Some(at), _) => {
                let left = at.saturating_duration_since(Instant::now()).as_secs();
                format!("Sleep timer: {} min left", left.div_ceil(60).max(1))
            }
            (None, true) => "Sleep timer: pausing after this song".to_string(),
            (None, false) => String::new(),
        };
        let player = self.ui.global::<ui::Player>();
        if player.get_sleep() != label.as_str() {
            player.set_sleep(label.into());
        }
    }

    // ---- audio settings ------------------------------------------------------------------------

    /// Pushes the sound settings to the engine. They take effect on what's playing.
    pub fn apply_audio_settings(&self) {
        let settings = self.settings.borrow();
        self.engine.set_output(&settings.output_device, settings.exclusive);
        self.engine.set_crossfade(settings.crossfade);
        self.engine.set_mono(settings.mono);
        self.engine.set_equalizer(settings.equalizer, settings.eq_gains);
    }

    /// Restarts the current track where it is, to pick up a setting that's fixed when a
    /// track loads (streaming quality, loudness level).
    pub fn reload_current(self: &Rc<Self>) {
        let (loaded, position) = {
            let pb = self.playback.borrow();
            (pb.current_token != 0, pb.position)
        };
        if loaded {
            self.start_current(position);
        }
        self.show_format();
    }

    /// Describes the audio being played, for Settings: the file's own format and, when the
    /// server is converting it, what it's converted to.
    pub fn show_format(&self) {
        let text = match self.playback.borrow().current() {
            Some(song) if !song.radio_url.is_empty() => {
                let player = self.ui.global::<ui::Player>();
                player.set_format("Live stream".into());
                player.set_lossless(false);
                format!("“{}” is a live stream, played as the station sends it.", song.title)
            }
            Some(song) => {
                let mut parts = vec![song.suffix.to_uppercase()];
                if song.sampling_rate > 0 {
                    let khz = song.sampling_rate as f32 / 1000.0;
                    parts.push(if khz.fract() == 0.0 { format!("{khz:.0} kHz") } else { format!("{khz:.1} kHz") });
                }
                if song.bit_depth > 0 {
                    parts.push(format!("{}-bit", song.bit_depth));
                }
                if song.bit_rate > 0 {
                    parts.push(format!("{} kbps", song.bit_rate));
                }
                let source = parts.join(", ");
                // A downloaded copy plays as saved: the original, or MP3 for formats the app
                // can't decode.
                let saved = self.downloads.borrow().path_of(&song.id);
                let cap = if saved.is_some() { 0 } else { self.settings.borrow().max_bitrate() };
                let suffix = song.suffix.to_lowercase();
                let native = NATIVE_SUFFIXES.contains(&suffix.as_str())
                    && (saved.is_some() || !self.playback.borrow().needs_transcode.contains(&song.id));
                let title = &song.title;
                // The short form shown under the song in the player bar.
                let as_stored = native && (cap == 0 || (suffix == "mp3" && song.bit_rate <= cap));
                let lossless = as_stored
                    && (LOSSLESS_SUFFIXES.contains(&suffix.as_str()) || song.bit_depth > 0);
                let label = if as_stored {
                    let mut short = vec![song.suffix.to_uppercase()];
                    if lossless {
                        short.extend(parts.iter().skip(1).filter(|p| !p.ends_with("kbps")).cloned());
                    } else if song.bit_rate > 0 {
                        short.push(format!("{} kbps", song.bit_rate));
                    }
                    short.join(" · ")
                } else {
                    format!("MP3 · {} kbps", if cap == 0 { 320 } else { cap })
                };
                // Exclusive output of an untouched lossless file is what reaches the DAC.
                let label = if self.output_exclusive.get() { format!("{label} · Exclusive") } else { label };
                let player = self.ui.global::<ui::Player>();
                player.set_format(label.into());
                player.set_lossless(lossless);
                if cap > 0 && suffix == "mp3" && song.bit_rate <= cap {
                    // Already an MP3 within the limit: the server sends it untouched.
                    format!("“{title}” is playing as stored: {source}.")
                } else if cap > 0 {
                    format!("“{title}” is {source}, converted to {cap} kbps MP3 for streaming.")
                } else if !native {
                    format!(
                        "“{title}” is {source}. Nexus Music can't decode that format itself, so the server converts it to 320 kbps MP3."
                    )
                } else {
                    format!("“{title}” is playing as stored: {source}.")
                }
            }
            None => {
                self.ui.global::<ui::Player>().set_format("".into());
                String::new()
            }
        };
        self.ui.global::<ui::Settings>().set_now_format(text.into());
    }

    // ---- pushing state to the interface --------------------------------------------------------

    /// The current track changed (or there no longer is one).
    pub fn track_changed(self: &Rc<Self>) {
        let player = self.ui.global::<ui::Player>();
        let current = self.playback.borrow().current().cloned();
        match &current {
            Some(song) => {
                player.set_track(self.track_row(song));
                player.set_track_id(song.id.as_str().into());
                player.set_duration(song.duration as f32);
                player.set_has_track(true);
            }
            None => {
                player.set_track(ui::Track::default());
                player.set_track_id("".into());
                player.set_duration(0.0);
                player.set_has_track(false);
                player.set_buffering(false);
            }
        }
        player.set_position(self.playback.borrow().position as f32);
        player.set_context(self.playback.borrow().context.as_str().into());
        self.sync_transport();
        self.sync_queue();
        self.sync_history();
        self.save_queue();
        if matches!(self.current_route(), Some(Route::Lyrics | Route::NowPlaying)) {
            self.load_lyrics();
        }
        self.show_format();
        #[cfg(windows)]
        self.media_track(current.as_ref());
    }

    pub fn sync_transport(self: &Rc<Self>) {
        let pb = self.playback.borrow();
        let player = self.ui.global::<ui::Player>();
        player.set_playing(pb.playing);
        player.set_volume(pb.volume);
        player.set_muted(pb.muted);
        player.set_shuffle(pb.shuffle);
        player.set_repeat(pb.repeat as i32);
        // Slider travel is squared so the quiet end of the range has usable resolution.
        self.engine.set_volume(if pb.muted { 0.0 } else { pb.volume * pb.volume });
        let (playing, position) = (pb.playing, pb.position);
        drop(pb);
        self.sync_tray();
        #[cfg(windows)]
        self.media_playing(playing, position);
        #[cfg(not(windows))]
        let _ = (playing, position);
    }

    pub fn sync_queue(self: &Rc<Self>) {
        let pb = self.playback.borrow();
        let rows: Vec<ui::Track> = pb
            .order
            .iter()
            .skip(pb.cursor + 1)
            .take(QUEUE_ROWS)
            .filter_map(|&i| pb.queue.get(i))
            .map(|song| self.track_row(song))
            .collect();
        drop(pb);
        self.queue_model.set_vec(rows);
    }

    /// Applies a like or unlike to every copy of the song the player is showing.
    pub fn song_star_changed(self: &Rc<Self>, id: &str, starred: bool) {
        let player = self.ui.global::<ui::Player>();
        if player.get_track_id() == id {
            let mut track = player.get_track();
            track.starred = starred;
            player.set_track(track);
        }
        for model in [&self.queue_model, &self.history_model, &self.tracks_model] {
            for row in 0..model.row_count() {
                if let Some(mut track) = model.row_data(row) {
                    if track.id == id && track.starred != starred {
                        track.starred = starred;
                        model.set_row_data(row, track);
                    }
                }
            }
        }
    }

    // ---- recently played -----------------------------------------------------------------------

    /// Notes that the current track has started playing.
    fn record_played(self: &Rc<Self>) {
        let Some(song) = self.playback.borrow().current().cloned() else { return };
        {
            let mut history = self.history.borrow_mut();
            // Reloads of the same track (a seek, a quality change) aren't new plays.
            if history.first().is_some_and(|last| last.id == song.id) {
                return;
            }
            history.insert(0, song);
            history.truncate(HISTORY_LENGTH);
            crate::config::save_history(&self.paths.history, &history);
        }
        self.sync_history();
    }

    /// How many entries at the top of the history the panel leaves out: the song playing now
    /// is already shown in the player bar.
    fn history_hidden(&self) -> usize {
        let pb = self.playback.borrow();
        let history = self.history.borrow();
        match (pb.current(), history.first()) {
            (Some(current), Some(last)) if current.id == last.id => 1,
            _ => 0,
        }
    }

    pub fn sync_history(self: &Rc<Self>) {
        let rows: Vec<ui::Track> = self
            .history
            .borrow()
            .iter()
            .skip(self.history_hidden())
            .map(|song| self.track_row(song))
            .collect();
        self.history_model.set_vec(rows);
    }

    fn history_song(&self, row: usize) -> Option<Song> {
        self.history.borrow().get(row + self.history_hidden()).cloned()
    }

    /// Plays one song from the history by itself, as if it had been picked on its own: what
    /// follows it is up to autoplay.
    fn play_history(self: &Rc<Self>, row: usize) {
        if let Some(song) = self.history_song(row) {
            self.play_songs(vec![song], 0, "", false);
        }
    }

    // ---- remembering the queue between runs ----------------------------------------------------

    pub fn save_queue(&self) {
        let pb = self.playback.borrow();
        if pb.queue.is_empty() {
            let _ = std::fs::remove_file(&self.paths.queue);
            return;
        }
        SavedQueue {
            songs: pb.queue.clone(),
            order: pb.order.clone(),
            cursor: pb.cursor,
            position: pb.position,
            context: pb.context.clone(),
        }
        .save(&self.paths.queue);
    }

    /// Brings back the queue from the last run, paused where it stopped.
    pub fn restore_queue(self: &Rc<Self>) {
        let Some(saved) = SavedQueue::load(&self.paths.queue) else { return };
        {
            let mut pb = self.playback.borrow_mut();
            if !pb.queue.is_empty() {
                return;
            }
            pb.queue = saved.songs;
            pb.order = saved.order;
            pb.cursor = saved.cursor;
            pb.context = saved.context;
            pb.position = saved.position;
            pb.resume_at = saved.position;
            pb.playing = false;
            pb.current_token = 0;
        }
        self.track_changed();
    }

    // ---- lyrics --------------------------------------------------------------------------------

    pub fn load_lyrics(self: &Rc<Self>) {
        let lyrics = self.ui.global::<ui::Lyrics>();
        let Some(song) = self.playback.borrow().current().cloned() else {
            *self.lyrics.borrow_mut() = LyricsState::default();
            lyrics.set_lines(ModelRc::default());
            lyrics.set_status("Play a song to see its lyrics".into());
            return;
        };
        if self.lyrics.borrow().song_id == song.id {
            return;
        }
        *self.lyrics.borrow_mut() = LyricsState { song_id: song.id.clone(), ..Default::default() };
        lyrics.set_lines(ModelRc::default());
        lyrics.set_status("Looking for lyrics…".into());
        let id = song.id.clone();
        self.request(
            move |client| client.lyrics(&id),
            move |ctl, result| {
                if ctl.lyrics.borrow().song_id != song.id {
                    return;
                }
                ctl.show_lyrics(result.ok().flatten());
            },
        );
    }

    fn show_lyrics(self: &Rc<Self>, found: Option<Lyrics>) {
        let lyrics = self.ui.global::<ui::Lyrics>();
        let Some(found) = found else {
            lyrics.set_status("No lyrics for this song".into());
            return;
        };
        let timed = found.synced && found.line.iter().all(|l| l.start.is_some());
        let rows: Vec<ui::LyricLine> = found
            .line
            .iter()
            .map(|line| ui::LyricLine {
                text: line.value.trim().into(),
                time: line.start.map(|ms| ms as f32 / 1000.0).unwrap_or(-1.0),
            })
            .collect();
        {
            let mut state = self.lyrics.borrow_mut();
            state.times = if timed {
                found.line.iter().map(|l| l.start.unwrap_or(0) as f64 / 1000.0).collect()
            } else {
                Vec::new()
            };
            state.active = -1;
        }
        lyrics.set_timed(timed);
        lyrics.set_active(-1);
        lyrics.set_lines(ModelRc::new(VecModel::from(rows)));
        lyrics.set_status("".into());
    }

    fn follow_lyrics(&self, position: f64) {
        let mut state = self.lyrics.borrow_mut();
        if state.times.is_empty() {
            return;
        }
        // The last line whose start time has passed.
        let active = state.times.partition_point(|&t| t <= position + 0.15) as i32 - 1;
        if active != state.active {
            state.active = active;
            self.ui.global::<ui::Lyrics>().set_active(active);
        }
    }

    // ---- callbacks from the interface ----------------------------------------------------------

    pub fn wire_playback(self: &Rc<Self>) {
        let player = self.ui.global::<ui::Player>();
        player.on_toggle(|| {
            with(|ctl| ctl.toggle_play());
        });
        player.on_next(|| {
            with(|ctl| ctl.next_track());
        });
        player.on_previous(|| {
            with(|ctl| ctl.previous_track());
        });
        player.on_seek(|seconds| {
            with(|ctl| ctl.seek(seconds as f64));
        });
        player.on_set_volume(|volume| {
            with(|ctl| ctl.set_volume(volume));
        });
        player.on_toggle_mute(|| {
            with(|ctl| {
                {
                    let mut pb = ctl.playback.borrow_mut();
                    pb.muted = !pb.muted;
                    ctl.settings.borrow_mut().muted = pb.muted;
                }
                ctl.sync_transport();
            });
        });
        player.on_toggle_shuffle(|| {
            with(|ctl| ctl.toggle_shuffle());
        });
        player.on_cycle_repeat(|| {
            with(|ctl| ctl.cycle_repeat());
        });
        player.on_toggle_star(|| {
            with(|ctl| {
                let song = ctl.playback.borrow().current().cloned();
                if let Some(song) = song {
                    ctl.toggle_song_star(&song);
                }
            });
        });
        player.on_queue_activated(|index| {
            with(|ctl| ctl.jump_to_upcoming(index as usize));
        });
        player.on_queue_menu(|index, x, y| {
            with(|ctl| ctl.open_queue_menu(index, x, y));
        });
        player.on_clear_queue(|| {
            with(|ctl| ctl.clear_upcoming());
        });
        player.on_queue_moved(|index, delta| {
            with(|ctl| {
                let to = (index + delta).max(0) as usize;
                ctl.move_upcoming(index.max(0) as usize, to);
            });
        });
        player.on_choose_sleep(|x, y| {
            with(|ctl| ctl.open_sleep_menu(x, y));
        });
        player.on_history_activated(|index| {
            with(|ctl| ctl.play_history(index as usize));
        });
        player.on_history_menu(|index, x, y| {
            with(|ctl| {
                if let Some(song) = ctl.history_song(index as usize) {
                    ctl.open_song_menu(song, x, y);
                }
            });
        });
    }

    // ---- playing from the page on screen -------------------------------------------------------

    /// `index` is the row that was chosen, in the order the page is showing.
    pub fn play_page_track(self: &Rc<Self>, index: usize) {
        let songs = self.visible_songs();
        let context = self.page.borrow().context.clone();
        if index < songs.len() {
            // Starting from a chosen song plays the list in order, even with shuffle on.
            self.play_songs_from(songs, index, &context);
        }
    }

    /// Plays `songs` beginning with the one at `index`; with shuffle on, that song goes first.
    fn play_songs_from(self: &Rc<Self>, songs: Vec<Song>, index: usize, context: &str) {
        self.play_songs(songs, index, context, false);
    }

    pub fn play_page(self: &Rc<Self>, shuffle: bool) {
        let (source, context) = {
            let page = self.page.borrow();
            (page.play.clone(), page.context.clone())
        };
        // A sorted or filtered list plays as it's shown.
        let songs = self.visible_songs();
        match source {
            crate::pages::PlaySource::Songs => self.play_songs(songs, 0, &context, shuffle),
            crate::pages::PlaySource::Albums(ids) => {
                self.request(
                    move |client| {
                        let mut all = Vec::new();
                        for id in ids.iter().take(40) {
                            all.extend(client.album(id)?.songs);
                        }
                        Ok(all)
                    },
                    move |ctl, result| match result {
                        Ok(all) => ctl.play_songs(all, 0, &context, shuffle),
                        Err(e) => ctl.toast(e.to_string()),
                    },
                );
            }
        }
    }
}

/// The gain to apply to a song so it lands at the chosen loudness, from its ReplayGain tags.
/// `album_run` says the song is playing alongside others from its album.
fn level_gain(settings: &Settings, song: &Song, album_run: bool) -> f32 {
    if !settings.normalize {
        return 1.0;
    }
    let Some(tags) = song.replay_gain.as_ref() else { return 1.0 };
    // Album gain turns a whole album up or down by one amount, keeping its songs in
    // proportion; it falls back to the song's own value when the album one is missing.
    let by_album = match settings.level_mode {
        1 => false,
        2 => true,
        _ => album_run,
    };
    let (gain_db, peak) = match (by_album, tags.album_gain) {
        (true, Some(album_gain)) => (album_gain, tags.album_peak.or(tags.track_peak)),
        _ => match tags.track_gain {
            Some(track_gain) => (track_gain, tags.track_peak),
            None => return 1.0,
        },
    };
    let (_, offset) = VOLUME_LEVELS[(settings.volume_level as usize).min(VOLUME_LEVELS.len() - 1)];
    let gain = 10f32.powf((gain_db + offset) / 20.0);
    // Quiet and Normal never push a track's peaks past full scale. Loud may, by up to 3 dB,
    // and the limiter rounds those peaks off: that's the quality cost of the Loud setting.
    let peak = peak.filter(|p| *p > 0.0).unwrap_or(1.0);
    let headroom = if settings.volume_level >= 2 { 1.41 } else { 1.0 };
    gain.min(headroom / peak)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subsonic::ReplayGain;

    fn song(gain: f32, peak: f32) -> Song {
        Song {
            replay_gain: Some(ReplayGain {
                track_gain: Some(gain),
                album_gain: Some(gain - 5.0),
                track_peak: Some(peak),
                album_peak: Some(peak),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn loudness_levels_respect_peaks() {
        let mut settings = Settings { normalize: true, volume_level: 1, ..Default::default() };
        // A loud modern master is turned down: -8 dB of ReplayGain plus Normal's +4 dB.
        assert!((level_gain(&settings, &song(-8.0, 1.0), false) - 10f32.powf(-4.0 / 20.0)).abs() < 0.001);
        // A quiet track with full-scale peaks can't be raised without clipping.
        assert_eq!(level_gain(&settings, &song(3.0, 1.0), false), 1.0);
        // Unless Loud is chosen, which allows a little and leans on the limiter.
        settings.volume_level = 2;
        assert!((level_gain(&settings, &song(3.0, 1.0), false) - 1.41).abs() < 0.001);
        // Off means untouched, as does a file with no ReplayGain tag.
        settings.normalize = false;
        assert_eq!(level_gain(&settings, &song(-8.0, 1.0), false), 1.0);
        settings.normalize = true;
        assert_eq!(level_gain(&settings, &Song::default(), false), 1.0);
        // Playing through an album, Auto uses the album's value (5 dB lower in this fixture);
        // "Song" never does, and "Album" always does.
        settings.volume_level = 1;
        let album = 10f32.powf((-8.0 - 5.0 + 4.0) / 20.0);
        assert!((level_gain(&settings, &song(-8.0, 1.0), true) - album).abs() < 0.001);
        settings.level_mode = 1;
        assert!((level_gain(&settings, &song(-8.0, 1.0), true) - 10f32.powf(-4.0 / 20.0)).abs() < 0.001);
        settings.level_mode = 2;
        assert!((level_gain(&settings, &song(-8.0, 1.0), false) - album).abs() < 0.001);
    }
}
