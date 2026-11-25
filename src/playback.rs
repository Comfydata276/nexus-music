//! The play queue and transport: what plays next, shuffle and repeat, seeking, play counts
//! and lyrics. All on the UI thread; the audio itself is the engine's business.

use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, VecModel};

use nexus_core::levelling::in_album_order;
use nexus_core::queue::{Lineup, compare_lists, follow_list};

use crate::audio::engine::{Event, TrackRequest};
use crate::config::{SavedQueue, Settings, VOLUME_LEVELS};
use crate::controller::{Controller, Route, with};
use crate::jam::Remote;
use crate::subsonic::{ApiError, Lyrics, Song};
use crate::ui;

/// Start fetching the following track once this much of the current one remains.
const PRELOAD_SECONDS: f64 = 25.0;
/// Rows shown in the queue panel.
const QUEUE_ROWS: usize = 400;
/// How many of those Now Playing shows.
const UP_NEXT_ROWS: usize = 6;
use crate::sync::HISTORY_LENGTH;
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
    /// The page the queue was started from (its route key), and for each entry of `queue`
    /// its position in that page's song list. Used to mark the row that's playing: the
    /// same song in another list, or a second copy in this one, is a different row.
    pub source: String,
    pub origins: Vec<Option<usize>>,
    /// The ids of every song in that page's list, as last seen: how changes to it are noticed.
    pub source_list: Vec<String>,
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
    listened: f64,
    last_tick: f64,
    scrobbled: bool,
    /// Track position when the queue was last written to disk.
    saved_at: f64,
    /// Songs whose original files the decoders rejected.
    needs_transcode: HashSet<String>,
    failures: u32,
    /// How many times in a row the current song's stream has been asked for again after the
    /// connection failed, and whether it's now waiting for the server to come back.
    reconnects: u32,
    waiting_for_server: bool,
    /// Where the song had got to when last looked at, and for how many looks in a row (four
    /// a second) it hasn't moved although it's meant to be playing.
    seen_at: Option<f64>,
    stuck_ticks: u32,
    /// Similar songs are being fetched to continue after the queue.
    /// Songs whose kept copy was thrown away because it wouldn't play.
    dropped_kept: HashSet<String>,
    /// Songs whose downloaded copy wouldn't play this session: they're streamed instead.
    bad_downloads: HashSet<String>,
    autoplay_loading: bool,
    /// The song they're being found for. An answer for any other is out of date.
    autoplay_seed: String,
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
            source: String::new(),
            origins: Vec::new(),
            source_list: Vec::new(),
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
            listened: 0.0,
            last_tick: 0.0,
            scrobbled: false,
            saved_at: 0.0,
            needs_transcode: HashSet::new(),
            failures: 0,
            reconnects: 0,
            waiting_for_server: false,
            seen_at: None,
            stuck_ticks: 0,
            dropped_kept: HashSet::new(),
            bad_downloads: HashSet::new(),
            autoplay_loading: false,
            autoplay_seed: String::new(),
            autoplay_resume: false,
            autoplay_asked: 0,
        }
    }

    pub fn current(&self) -> Option<&Song> {
        self.order.get(self.cursor).and_then(|&i| self.queue.get(i))
    }

    /// Where the current track sits in the list of the page it was played from.
    pub fn current_origin(&self) -> Option<usize> {
        self.order.get(self.cursor).and_then(|&i| self.origins.get(i).copied().flatten())
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

    /// How many of the entries straight after the current one were queued by hand: the
    /// length of "Next in queue".
    pub fn queued_ahead(&self) -> usize {
        self.order.iter().skip(self.cursor + 1).take_while(|&&entry| self.queue.get(entry).is_some_and(|song| song.queued)).count()
    }

    /// Rearranges the order with `rearrange`, then puts what was queued by hand back where
    /// it was: straight after the current track, in the order it was queued.
    fn rearrange_keeping_queued(&mut self, rearrange: impl FnOnce(&mut Self)) {
        let ahead: Vec<usize> = self.order.iter().skip(self.cursor + 1).take(self.queued_ahead()).copied().collect();
        rearrange(self);
        let Some(&current) = self.order.get(self.cursor) else { return };
        if ahead.is_empty() {
            return;
        }
        self.order.retain(|entry| !ahead.contains(entry));
        self.cursor = self.order.iter().position(|&entry| entry == current).unwrap_or(0);
        for (place, entry) in ahead.into_iter().enumerate() {
            self.order.insert(self.cursor + 1 + place, entry);
        }
    }

    /// Takes the entry at this place in the playing order out of the queue.
    fn drop_at(&mut self, at: usize) {
        if at >= self.order.len() {
            return;
        }
        let removed = self.order.remove(at);
        self.queue.remove(removed);
        if removed < self.origins.len() {
            self.origins.remove(removed);
        }
        for index in self.order.iter_mut() {
            if *index > removed {
                *index -= 1;
            }
        }
        if at < self.cursor {
            self.cursor -= 1;
        }
    }

    /// Queues songs by hand: at the top of "Next in queue" with `play_next`, otherwise at
    /// the bottom of it.
    fn queue_up(&mut self, songs: Vec<Song>, play_next: bool) {
        let mut at = self.cursor + 1 + if play_next { 0 } else { self.queued_ahead() };
        for mut song in songs {
            song.queued = true;
            self.queue.push(song);
            self.order.insert(at, self.queue.len() - 1);
            at += 1;
        }
        self.origins.resize(self.queue.len(), None);
    }

    /// Moves the entry `from` places after the current track to `to` places after it, into
    /// "Next in queue" or out of it as `queued` says.
    fn move_ahead(&mut self, from: usize, to: usize, queued: bool) {
        let base = self.cursor + 1;
        let (from, to) = (base + from, base + to);
        if from >= self.order.len() || to >= self.order.len() {
            return;
        }
        let entry = self.order.remove(from);
        self.order.insert(to, entry);
        if let Some(song) = self.queue.get_mut(entry) {
            song.queued = queued;
        }
    }

    /// Empties part of what's to come: with `queued`, the songs queued by hand; otherwise
    /// the rest of the list behind them.
    fn clear_ahead(&mut self, queued: bool) {
        let ahead = self.queued_ahead();
        if queued {
            for _ in 0..ahead {
                self.drop_at(self.cursor + 1);
            }
        } else {
            while self.order.len() > self.cursor + 1 + ahead {
                self.drop_at(self.order.len() - 1);
            }
        }
    }

    /// Makes the entry `offset` places after the current track the current one. Songs
    /// queued by hand that it jumps over aren't lost: they stay next, after it. Returns
    /// whether there was such an entry.
    fn jump_ahead(&mut self, offset: usize) -> bool {
        let at = self.cursor + 1 + offset;
        if at >= self.order.len() {
            return false;
        }
        let ahead = self.queued_ahead();
        if offset < ahead {
            // One of the queued songs: it comes to the front of them.
            let entry = self.order.remove(at);
            self.cursor += 1;
            self.order.insert(self.cursor, entry);
        } else {
            let waiting: Vec<usize> = self.order.drain(self.cursor + 1..self.cursor + 1 + ahead).collect();
            self.cursor = at - ahead;
            for (place, entry) in waiting.into_iter().enumerate() {
                self.order.insert(self.cursor + 1 + place, entry);
            }
        }
        true
    }
}

/// Where songs about to be played came from: the page's route key, each song's position in
/// that page's list, and the ids of the whole list.
pub struct PlayFrom {
    pub key: String,
    pub rows: Vec<usize>,
    pub list: Vec<String>,
}

/// A fingerprint of the queue and the place in it, to tell a change from a save of the
/// same thing.
pub fn queue_fingerprint(pb: &Playback) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    pb.cursor.hash(&mut hasher);
    (pb.position as i64).hash(&mut hasher);
    pb.order.hash(&mut hasher);
    for song in &pb.queue {
        song.id.hash(&mut hasher);
    }
    hasher.finish()
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

    /// Replaces the queue and starts playing from `start`. `any_start` says no particular
    /// song was asked for, so a shuffled list may open with any of them.
    pub fn play_songs(self: &Rc<Self>, songs: Vec<Song>, start: usize, context: &str, any_start: bool) {
        if self.remote_play(&songs, start, context, any_start) {
            self.play_from.take();
            return;
        }
        // Where these songs came from, if the caller said (see `play_from`).
        let (source, origins, source_list) = match self.play_from.take() {
            Some(from) if from.rows.len() == songs.len() => {
                (from.key, from.rows.into_iter().map(Some).collect::<Vec<_>>(), from.list)
            }
            // The list is known, but not which of its rows each song is.
            Some(from) => (from.key, vec![None; songs.len()], Vec::new()),
            None => (String::new(), vec![None; songs.len()], Vec::new()),
        };
        // Offline, only what's downloaded can play: skip the rest.
        let (songs, origins, start) = if self.is_offline() {
            let downloads = self.downloads.borrow();
            let mut kept = Vec::new();
            let mut kept_origins = Vec::new();
            let mut kept_start = 0;
            for (index, (song, origin)) in songs.into_iter().zip(origins).enumerate() {
                if downloads.has(&song.id) || self.plays_offline(&song) {
                    if index == start {
                        kept_start = kept.len();
                    }
                    kept.push(song);
                    kept_origins.push(origin);
                }
            }
            (kept, kept_origins, kept_start)
        } else {
            (songs, origins, start)
        };
        if songs.is_empty() {
            if self.is_offline() {
                self.toast("That isn't downloaded, so it can't play while you're offline");
            }
            return;
        }
        let _changing = self.changing();
        // Each list remembers whether it plays shuffled. Songs that aren't from a list (a
        // search result, say) leave shuffle as it is.
        let listed = (!source.is_empty()).then(|| self.list_prefs(&source).shuffle);
        {
            let mut pb = self.playback.borrow_mut();
            let shuffle = listed.unwrap_or(pb.shuffle);
            pb.shuffle = shuffle;
            self.settings.borrow_mut().shuffle = shuffle;
            pb.order = (0..songs.len()).collect();
            pb.cursor =
                if shuffle && any_start { fastrand::usize(..songs.len()) } else { start.min(songs.len() - 1) };
            pb.queue = songs;
            pb.origins = origins;
            pb.source = source;
            pb.source_list = source_list;
            pb.context = context.to_string();
            // A new queue: nothing in it was queued by hand.
            for song in pb.queue.iter_mut() {
                song.queued = false;
            }
            if pb.shuffle {
                pb.shuffle_order();
            }
            pb.playing = true;
            pb.failures = 0;
            pb.autoplay_resume = false;
        }
        self.save_settings();
        self.start_current(0.0);
        self.sync_transport();
        self.sync_page_shuffle();
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
            pb.waiting_for_server = false;
            let request = self.request_for(&mut pb, &song, token, at);
            (request, pb.playing)
        };
        self.retry_timer.stop();
        self.engine.load(client, request);
        self.engine.set_playing(playing);
        let player = self.ui.global::<ui::Player>();
        player.set_buffering(true);
        player.set_reconnecting(false);
        self.track_changed();
    }

    /// Loads the track at the cursor again, at `at`: after a seek the engine can't do in
    /// place, a changed setting, or a retry. It's the same listen carrying on, so what has
    /// been heard still counts and the song isn't reported as played a second time.
    fn restart_current(self: &Rc<Self>, at: f64) {
        let (listened, scrobbled) = {
            let pb = self.playback.borrow();
            (pb.listened, pb.scrobbled)
        };
        self.start_current(at);
        let mut pb = self.playback.borrow_mut();
        pb.listened = listened;
        pb.scrobbled = scrobbled;
    }

    fn request_for(&self, pb: &mut Playback, song: &Song, token: u64, at: f64) -> TrackRequest {
        let client = self.client().expect("signed in");
        let settings = self.settings.borrow();
        let suffix = song.suffix.to_lowercase();
        let native = NATIVE_SUFFIXES.contains(&suffix.as_str()) && !pb.needs_transcode.contains(&song.id);
        let cap = settings.max_bitrate();
        let album_run = pb.order.iter().position(|&i| pb.queue.get(i).is_some_and(|s| s.id == song.id)).is_some_and(|at| {
            let neighbour = |at: Option<usize>| at.and_then(|at| pb.order.get(at)).and_then(|&i| pb.queue.get(i));
            in_album_order(song, neighbour(at.checked_sub(1)), neighbour(Some(at + 1)), pb.shuffle)
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
                keep: None,
                live: true,
                url: song.radio_url.clone(),
                ranged: false,
                size: 0,
                suffix: suffix.into(),
                gain: 1.0,
                start_secs: 0.0,
                base_secs: 0.0,
                duration_secs: 0.0,
                continues: false,
            };
        }
        if let Some(path) = self.downloads.borrow().path_of(&song.id).filter(|_| !pb.bad_downloads.contains(&song.id)) {
            // A downloaded copy plays from disk, whatever the streaming quality is set to.
            let suffix = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            return TrackRequest {
                token,
                local: Some(path),
                keep: None,
                live: false,
                url: String::new(),
                ranged: true,
                size: 0,
                suffix,
                gain,
                start_secs: at,
                base_secs: 0.0,
                duration_secs,
                continues: false,
            };
        }
        // A copy kept from an earlier play, or fetched ahead, plays from disk too.
        let (bitrate, kept_as) = stream_form(native, cap, &suffix);
        let kept = (settings.cache_limit_mb > 0).then(|| self.song_cache.file(&song.id, bitrate, kept_as));
        if let Some(path) = kept.as_ref().filter(|path| crate::songcache::ready(path)) {
            crate::songcache::touch(path);
            return TrackRequest {
                token,
                local: Some(path.clone()),
                keep: None,
                live: false,
                url: String::new(),
                ranged: true,
                size: 0,
                suffix: kept_as.to_string(),
                gain,
                start_secs: at,
                base_secs: 0.0,
                duration_secs,
                continues: false,
            };
        }
        if native && cap == 0 {
            TrackRequest {
                token,
                local: None,
                // The server says how long the file is, so nothing shorter is kept.
                keep: kept.map(|path| (path, 0)),
                live: false,
                url: client.stream_url(&song.id, 0, 0),
                ranged: true,
                size: 0,
                suffix,
                gain,
                start_secs: at,
                base_secs: 0.0,
                duration_secs,
                continues: false,
            }
        } else {
            // The server skips ahead for us; the stream then starts at `offset`.
            let offset = at.floor().max(0.0);
            TrackRequest {
                token,
                local: None,
                // Only a conversion that starts at the beginning is the whole song.
                keep: kept.filter(|_| offset == 0.0).map(|path| (path, least_bytes(song, bitrate))),
                live: false,
                url: client.stream_url(&song.id, if cap == 0 { 320 } else { cap }, offset as u32),
                ranged: false,
                // The server may send the file as it is instead: see `TrackRequest::size`.
                size: song.size,
                suffix: "mp3".into(),
                gain,
                start_secs: 0.0,
                base_secs: offset,
                duration_secs,
                continues: false,
            }
        }
    }

    /// Where a song would be kept, in the form it would be streamed in right now, and the
    /// bit rate to ask the server for (0 for the file as it is).
    fn kept_file(&self, pb: &Playback, song: &Song) -> (PathBuf, u32) {
        let suffix = song.suffix.to_lowercase();
        let native = NATIVE_SUFFIXES.contains(&suffix.as_str()) && !pb.needs_transcode.contains(&song.id);
        let (bitrate, kept_as) = stream_form(native, self.settings.borrow().max_bitrate(), &suffix);
        (self.song_cache.file(&song.id, bitrate, kept_as), bitrate)
    }

    /// Whether a song can play without the server: it's downloaded, or a copy has been kept.
    pub fn plays_offline(&self, song: &Song) -> bool {
        if self.downloads.borrow().has(&song.id) {
            return true;
        }
        let settings = self.settings.borrow();
        if settings.cache_limit_mb == 0 || song.id.is_empty() {
            return false;
        }
        let suffix = song.suffix.to_lowercase();
        let cap = settings.max_bitrate();
        [true, false].into_iter().any(|native| {
            let (bitrate, kept_as) = stream_form(native, cap, &suffix);
            self.song_cache.holds(&self.song_cache.file(&song.id, bitrate, kept_as))
        })
    }

    /// Has the next few songs fetched ahead of time, so the music carries on if the
    /// connection drops, and makes room among the kept songs when they outgrow their limit.
    /// Called every few seconds while something is playing.
    pub fn keep_ahead(self: &Rc<Self>) {
        self.fetch_lyrics_ahead();
        let (ahead, limit) = {
            let settings = self.settings.borrow();
            (settings.cache_ahead as usize, settings.cache_limit_mb as u64 * 1024 * 1024)
        };
        let Some(client) = self.client().filter(|_| limit > 0 && !self.is_offline()) else {
            return self.fetch_ahead.want(Vec::new());
        };
        let mut wanted = Vec::new();
        let mut in_use = HashSet::new();
        {
            let pb = self.playback.borrow();
            let downloads = self.downloads.borrow();
            let upcoming = pb.order.iter().skip(pb.cursor).take(ahead + 1).filter_map(|&i| pb.queue.get(i));
            for (place, song) in upcoming.enumerate() {
                if song.id.is_empty() || !song.radio_url.is_empty() || downloads.has(&song.id) {
                    continue;
                }
                let (path, bitrate) = self.kept_file(&pb, song);
                in_use.insert(path.clone());
                // The one playing is already streaming, and is kept when it has all arrived.
                if place > 0 && !crate::songcache::ready(&path) {
                    let least = if bitrate == 0 { 0 } else { least_bytes(song, bitrate) };
                    wanted.push(crate::songcache::Wanted { client: client.clone(), url: client.stream_url(&song.id, bitrate, 0), path, least });
                }
            }
        }
        self.fetch_ahead.want(wanted);
        // Now and then, make room. Off the main thread: it reads the whole folder.
        let rounds = self.keep_rounds.get() + 1;
        self.keep_rounds.set(rounds);
        if rounds % 8 == 1 {
            let cache = self.song_cache.clone();
            self.api_pool.spawn(move || cache.trim(limit, &in_use));
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
            // The next track of the album that's playing runs on as the album has it.
            let continues = pb.current().is_some_and(|now| {
                !now.album_id.is_empty()
                    && now.album_id == song.album_id
                    && now.disc_number == song.disc_number
                    && now.track > 0
                    && song.track == now.track + 1
            });
            let mut request = self.request_for(&mut pb, &song, token, 0.0);
            request.continues = continues;
            request
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
                self.restart_current(position);
            } else {
                self.playback.borrow_mut().next_token = 0;
                self.engine.set_next(None);
            }
        }
        self.sync_queue();
        // Keeps an edited queue across a restart, and tells other devices about it.
        self.save_queue();
    }

    // ---- transport -----------------------------------------------------------------------------

    pub fn toggle_play(self: &Rc<Self>) {
        if self.remote(Remote::Toggle) {
            return;
        }
        // (With nothing in the player the press changes nothing, and takes no place.)
        if self.playback.borrow().current().is_none() {
            return;
        }
        let _changing = self.changing();
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

    /// Pauses this PC's own player, whatever else this PC is steering: what a command from
    /// another of the person's devices does (see `link`). Unlike `set_playing`, it never
    /// goes on to a jam this PC has joined.
    pub fn pause_here(self: &Rc<Self>) {
        let was_playing = {
            let mut pb = self.playback.borrow_mut();
            if pb.current().is_none() {
                return;
            }
            std::mem::replace(&mut pb.playing, false)
        };
        if was_playing {
            self.engine.set_playing(false);
            self.sync_transport();
            self.save_queue();
        }
    }

    /// Likes or unlikes the song that's playing. Used by the taskbar's like button.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn toggle_current_star(self: &Rc<Self>) {
        let song = self.playback.borrow().current().cloned();
        if let Some(song) = song {
            self.toggle_song_star(&song);
        }
    }

    /// Used by the media keys and the sleep timer.
    pub fn set_playing(self: &Rc<Self>, playing: bool) {
        if self.remote(Remote::Play(playing)) {
            return;
        }
        if self.playback.borrow().playing != playing {
            self.toggle_play();
        }
    }

    pub fn next_track(self: &Rc<Self>) {
        if self.remote(Remote::Next) {
            return;
        }
        // (With nothing to go to the press changes nothing, and takes no place.)
        if self.playback.borrow().order.is_empty() {
            return;
        }
        let _changing = self.changing();
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
        if self.remote(Remote::Previous) {
            return;
        }
        if self.playback.borrow().order.is_empty() {
            return;
        }
        let _changing = self.changing();
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
        if self.remote(Remote::Seek(seconds)) {
            return;
        }
        let (loaded, token, duration) = {
            let mut pb = self.playback.borrow_mut();
            let Some(duration) = pb.current().map(|s| s.duration as f64) else { return };
            if pb.current().is_some_and(|s| !s.radio_url.is_empty()) {
                // A live stream has no earlier or later to go to.
                return;
            }
            let seconds = seconds.clamp(0.0, (duration - 1.0).max(0.0));
            self.changed();
            pb.position = seconds;
            pb.last_tick = seconds;
            pb.resume_at = seconds;
            (pb.current_token != 0, pb.current_token, duration)
        };
        let seconds = seconds.clamp(0.0, (duration - 1.0).max(0.0));
        self.ui.global::<ui::Player>().set_position(seconds as f32);
        if !loaded {
            return;
        }
        // The engine knows whether the track it has can be sought in where it is: a file, or
        // one the server sent as it is. A conversion is asked for again from there.
        if !self.engine.seeks_in_place(token) {
            self.restart_current(seconds);
        } else {
            self.engine.seek(token, seconds);
        }
    }

    pub fn stop_playback(self: &Rc<Self>) {
        let _changing = self.changing();
        {
            let mut pb = self.playback.borrow_mut();
            pb.queue.clear();
            pb.origins.clear();
            pb.source.clear();
            pb.source_list.clear();
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

    pub fn set_volume(self: &Rc<Self>, volume: f32) {
        if self.remote(Remote::Volume(volume)) {
            return;
        }
        self.changed();
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

    pub fn toggle_shuffle(self: &Rc<Self>) {
        if self.remote(Remote::Shuffle) {
            return;
        }
        let on = !self.playback.borrow().shuffle;
        self.set_shuffle(on);
    }

    /// Turns shuffle on or off for what's playing. The current song carries on: turning it
    /// on shuffles what follows; turning it off continues from this song in the list's order.
    pub fn set_shuffle(self: &Rc<Self>, on: bool) {
        self.changed();
        let source = {
            let mut pb = self.playback.borrow_mut();
            if pb.shuffle != on {
                pb.shuffle = on;
                if on {
                    pb.rearrange_keeping_queued(|pb| pb.shuffle_order());
                } else if let Some(&current) = pb.order.get(pb.cursor) {
                    pb.rearrange_keeping_queued(|pb| {
                        pb.order = (0..pb.queue.len()).collect();
                        pb.cursor = current;
                    });
                }
            }
            pb.source.clone()
        };
        self.settings.borrow_mut().shuffle = on;
        self.save_settings();
        // The list being played remembers the choice for next time.
        self.choose_shuffle(&source, on);
        self.sync_transport();
        self.next_changed();
        self.sync_page_shuffle();
    }

    /// The shuffle button on a page: sets whether that list plays shuffled, without starting
    /// it. If it's the list playing now, the queue changes to match.
    pub fn toggle_page_shuffle(self: &Rc<Self>) {
        let Some(key) = self.current_route().map(|route| route.cache_key()) else { return };
        let playing_this = self.playback.borrow().source == key;
        if playing_this {
            self.toggle_shuffle();
        } else {
            let on = !self.list_prefs(&key).shuffle;
            self.choose_shuffle(&key, on);
            self.sync_page_shuffle();
        }
    }

    /// Shows whether the list on screen plays shuffled.
    pub fn sync_page_shuffle(&self) {
        let key = self.current_route().map(|route| route.cache_key()).unwrap_or_default();
        let on = {
            let pb = self.playback.borrow();
            if !key.is_empty() && pb.source == key { pb.shuffle } else { self.list_prefs(&key).shuffle }
        };
        self.ui.global::<ui::View>().set_shuffle(on);
    }

    fn cycle_repeat(self: &Rc<Self>) {
        if self.remote(Remote::Repeat) {
            return;
        }
        let mode = (self.playback.borrow().repeat + 1) % 3;
        self.set_repeat(mode);
    }

    /// Sets repeat: 0 off, 1 the whole queue, 2 the current track.
    pub fn set_repeat(self: &Rc<Self>, mode: u8) {
        self.changed();
        {
            let mut pb = self.playback.borrow_mut();
            pb.repeat = mode.min(2);
            self.settings.borrow_mut().repeat = pb.repeat;
        }
        self.save_settings();
        self.sync_transport();
        self.next_changed();
    }

    // ---- queue editing -------------------------------------------------------------------------

    /// Queues songs by hand. They go into "Next in queue", which plays straight after the
    /// current track and ahead of the rest of whatever list is playing: at the top of it
    /// with `play_next`, otherwise at the bottom. A song that's already further down the
    /// list is queued all the same, and plays again when its turn in the list comes.
    pub fn enqueue(self: &Rc<Self>, songs: Vec<Song>, play_next: bool) {
        if songs.is_empty() {
            return;
        }
        if self.remote_add(&songs, play_next) {
            return;
        }
        self.changed();
        let was_empty = self.playback.borrow().order.is_empty();
        if was_empty {
            // Nothing queued yet: this becomes the queue, ready to play.
            {
                let mut pb = self.playback.borrow_mut();
                pb.order = (0..songs.len()).collect();
                pb.origins = vec![None; songs.len()];
                pb.source.clear();
                pb.source_list.clear();
                pb.queue = songs;
                for song in pb.queue.iter_mut() {
                    song.queued = false;
                }
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
        self.playback.borrow_mut().queue_up(songs, play_next);
        self.next_changed();
    }

    /// Removes the entry `offset` places after the current track.
    pub fn remove_upcoming(self: &Rc<Self>, offset: usize) {
        if self.remote(Remote::Remove(offset)) {
            return;
        }
        {
            let mut pb = self.playback.borrow_mut();
            let at = pb.cursor + 1 + offset;
            if at >= pb.order.len() {
                return;
            }
            self.changed();
            pb.drop_at(at);
        }
        self.next_changed();
    }

    /// Moves the entry `from` places after the current track to `to` places after it.
    /// `queued` says which part of the queue it lands in: "Next in queue" or the list.
    pub fn move_upcoming(self: &Rc<Self>, from: usize, to: usize, queued: bool) {
        if self.remote(Remote::Move(from, to, queued)) {
            return;
        }
        self.changed();
        self.playback.borrow_mut().move_ahead(from, to, queued);
        self.next_changed();
    }

    /// Empties part of what's to come: with `queued`, the songs queued by hand; otherwise
    /// the rest of the list behind them.
    pub fn clear_upcoming(self: &Rc<Self>, queued: bool) {
        if self.remote(Remote::Clear(queued)) {
            return;
        }
        if self.playback.borrow().current().is_none() {
            return;
        }
        self.changed();
        self.playback.borrow_mut().clear_ahead(queued);
        self.next_changed();
    }

    /// Plays the entry `offset` places after the current track now.
    pub fn jump_to_upcoming(self: &Rc<Self>, offset: usize) {
        if self.remote(Remote::Jump(offset)) {
            return;
        }
        {
            let mut pb = self.playback.borrow_mut();
            if !pb.jump_ahead(offset) {
                return;
            }
            pb.playing = true;
        }
        let _changing = self.changing();
        self.start_current(0.0);
    }

    /// Takes over a queue handed across from another device: these songs in this order,
    /// starting with the one at `index`, from `position` seconds into it. Songs marked as
    /// queued by hand stay so.
    pub fn load_queue(self: &Rc<Self>, songs: Vec<Song>, index: usize, position: f64, playing: bool, context: &str) {
        if songs.is_empty() {
            return;
        }
        let _changing = self.changing();
        self.engine.set_playing(false);
        self.engine.stop();
        let duration = songs.get(index).map_or(0.0, |song| song.duration as f64);
        // Handed over in a song's last moments, it starts from the top instead.
        let position = if duration > 0.0 && position >= duration - 1.0 { 0.0 } else { position.max(0.0) };
        self.replace_queue(songs, index, position);
        self.playback.borrow_mut().context = context.to_string();
        self.track_changed();
        if playing {
            self.toggle_play();
        } else {
            self.save_queue();
        }
    }

    // ---- engine events and the clock -----------------------------------------------------------

    pub fn on_engine_event(self: &Rc<Self>, event: Event) {
        match event {
            Event::Started(token) => {
                let advanced = {
                    let mut pb = self.playback.borrow_mut();
                    pb.failures = 0;
                    pb.reconnects = 0;
                    if token != 0 && token == pb.next_token {
                        // The engine rolled into the next track by itself.
                        self.changed();
                        pb.cursor = pb.next_cursor;
                        pb.current_token = token;
                        pb.next_token = 0;
                        pb.position = 0.0;
                        pb.listened = 0.0;
                        pb.last_tick = 0.0;
                        pb.scrobbled = false;
                        true
                    } else if token == pb.current_token {
                        false
                    } else {
                        return;
                    }
                };
                let player = self.ui.global::<ui::Player>();
                player.set_buffering(false);
                player.set_reconnecting(false);
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
                    // The song is over: whatever comes of that, what a command from another
                    // device would act on has changed (see `link`).
                    self.changed();
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
            Event::NoDevice => {
                // Playing on with nothing to play through would count songs nobody heard.
                let was_playing = std::mem::replace(&mut self.playback.borrow_mut().playing, false);
                if was_playing {
                    self.changed();
                    self.engine.set_playing(false);
                    self.sync_transport();
                    self.toast("No sound device found, so playback is paused.");
                }
            }
            Event::Failed { token, message, unsupported, network } => {
                let (retry, title, position) = {
                    let mut pb = self.playback.borrow_mut();
                    if token != pb.current_token {
                        return;
                    }
                    let Some(song) = pb.current().cloned() else { return };
                    if network {
                        // The stream stopped arriving. That's the connection, not the song.
                        let attempt = pb.reconnects;
                        pb.reconnects += 1;
                        drop(pb);
                        return self.reconnect(token, attempt, &song, message);
                    }
                    // A downloaded copy that won't play is left where it is (one failure may be
                    // nothing to do with the file), and the song is streamed for now.
                    if self.downloads.borrow().has(&song.id) && pb.bad_downloads.insert(song.id.clone()) {
                        let position = pb.position;
                        drop(pb);
                        self.toast(format!(
                            "The downloaded copy of “{}” wouldn't play, so it's being streamed. If that keeps happening, remove the download and fetch it again.",
                            song.title
                        ));
                        return self.restart_current(position);
                    }
                    // So is a copy kept from an earlier play (once: if the song still won't
                    // play after that, the copy wasn't what was wrong).
                    let (kept, _) = self.kept_file(&pb, &song);
                    if crate::songcache::ready(&kept) && pb.dropped_kept.insert(song.id.clone()) {
                        let position = pb.position;
                        drop(pb);
                        let _ = std::fs::remove_file(&kept);
                        return self.restart_current(position);
                    }
                    // First try having the server convert a file we can't decode.
                    let retry = unsupported && pb.needs_transcode.insert(song.id.clone());
                    (retry, song.title, pb.position)
                };
                if retry {
                    self.restart_current(position);
                    return;
                }
                self.pass_over(&title, &message);
            }
        }
    }

    /// Says so when a song that's meant to be playing hasn't moved for a second and a half:
    /// it's waiting for more of its stream to arrive.
    fn notice_stall(&self, position: Option<(u64, f64)>) {
        let stalled = {
            let mut pb = self.playback.borrow_mut();
            let at = position.filter(|(token, _)| *token == pb.current_token).map(|(_, seconds)| seconds);
            if !pb.playing || pb.current_token == 0 || (at.is_some() && at != pb.seen_at) {
                pb.stuck_ticks = 0;
            } else {
                pb.stuck_ticks += 1;
            }
            pb.seen_at = at;
            pb.stuck_ticks >= 6
        };
        let player = self.ui.global::<ui::Player>();
        if player.get_stalled() != stalled {
            player.set_stalled(stalled);
        }
    }

    /// The current song's stream stopped arriving. It's asked for again from where it had
    /// got to, a few times and a little further apart each time. If that doesn't bring it
    /// back and the server can't be reached, the song waits where it is and carries on when
    /// the server returns; if the server is there, the song is what's wrong and is passed over.
    fn reconnect(self: &Rc<Self>, token: u64, attempt: u32, song: &Song, message: String) {
        const WAITS: [u64; 4] = [1, 2, 4, 8];
        let player = self.ui.global::<ui::Player>();
        player.set_reconnecting(true);
        if let Some(&wait) = WAITS.get(attempt as usize) {
            self.retry_timer.start(slint::TimerMode::SingleShot, Duration::from_secs(wait), move || {
                with(|ctl| ctl.retry_current(token));
            });
            return;
        }
        let title = song.title.clone();
        if !song.radio_url.is_empty() {
            // A radio station is somewhere else on the internet, not the music server.
            return self.pass_over(&title, &message);
        }
        self.request(
            |client| client.ping().map(|_| ()),
            move |ctl, answer| {
                if ctl.playback.borrow().current_token != token {
                    return;
                }
                match answer {
                    Ok(()) | Err(ApiError::Server { .. }) => ctl.pass_over(&title, &message),
                    Err(_) => {
                        ctl.playback.borrow_mut().waiting_for_server = true;
                        ctl.set_unreachable(true);
                        ctl.toast("Lost the connection to the server. The music will carry on when it's back.");
                    }
                }
            },
        );
    }

    /// Asks for the current song's stream again, from where it had got to.
    fn retry_current(self: &Rc<Self>, token: u64) {
        let position = {
            let pb = self.playback.borrow();
            if pb.current_token != token {
                return;
            }
            pb.position
        };
        let radio = self.playback.borrow().current().is_some_and(|song| !song.radio_url.is_empty());
        self.restart_current(if radio { 0.0 } else { position });
        self.ui.global::<ui::Player>().set_reconnecting(true);
    }

    /// The server is back: carry on with a song that was waiting for it.
    pub fn resume_waiting(self: &Rc<Self>) {
        let position = {
            let mut pb = self.playback.borrow_mut();
            if !std::mem::take(&mut pb.waiting_for_server) || pb.current_token == 0 {
                return;
            }
            pb.reconnects = 0;
            pb.position
        };
        self.restart_current(position);
    }

    /// Gives up on the current song: says so, and moves to the next unless too many in a row
    /// have failed or there is no next.
    fn pass_over(self: &Rc<Self>, title: &str, message: &str) {
        let _changing = self.changing();
        self.ui.global::<ui::Player>().set_reconnecting(false);
        let give_up = {
            let mut pb = self.playback.borrow_mut();
            pb.failures += 1;
            pb.reconnects = 0;
            pb.failures >= 3 || pb.following().is_none()
        };
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

    /// Runs four times a second: moves the progress bar, counts listening time, queues up the
    /// next track and follows the lyrics.
    pub fn tick(self: &Rc<Self>) {
        self.check_sleep();
        self.jam_tick();
        self.tick_downloads();
        // Every few seconds while there's a queue, see that the next songs are on hand.
        let keep_ticks = self.keep_ticks.get() + 1;
        self.keep_ticks.set(keep_ticks);
        if keep_ticks % 16 == 0 && self.playback.borrow().current_token != 0 {
            self.keep_ahead();
        }
        // Once a minute while music plays, see that the server is still there, so a lost
        // connection shows before the songs kept on this PC run out.
        if keep_ticks % 240 == 0 && self.playback.borrow().playing {
            self.check_server();
        }
        self.tick_requests();
        if self.mini_open.get() {
            self.sync_mini();
        }
        // Keep the maximise button, and the title bar's sense of scale, in step with the window.
        let app = self.ui.global::<ui::App>();
        let maximized = self.ui.window().is_maximized();
        if app.get_maximized() != maximized {
            app.set_maximized(maximized);
        }
        #[cfg(windows)]
        crate::frame::set_scale(self.ui.window().scale_factor());
        let position = self.engine.position();
        self.notice_stall(position);
        let Some((token, seconds)) = position else { return };
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
                // A song playing as part of a playlist counts towards that playlist too; one
                // put in the queue by hand, or played from anywhere else, doesn't.
                let listed = pb.order.get(pb.cursor).and_then(|&at| pb.origins.get(at).copied().flatten()).is_some();
                let from = if listed && pb.source.starts_with("playlist:") { pb.source.clone() } else { String::new() };
                (song.id.clone(), from)
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
        if let Some((id, from)) = submit {
            self.play_log.borrow_mut().add(&id, chrono::Utc::now().timestamp(), &from);
            // The figures on "Your stats" have moved on.
            self.invalidate("stats");
            if let Some(route @ Route::Stats(_)) = self.current_route() {
                let scroll = self.ui.global::<ui::View>().get_scroll_y();
                self.show(route, scroll, true, None);
            }
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

    /// Plays a song straight away and lines up songs like it to follow: its radio. The song
    /// doesn't wait for the others to be found.
    pub fn play_radio(self: &Rc<Self>, song: Song) {
        let context = format!("{} radio", song.title);
        self.play_songs(vec![song], 0, &context, false);
        self.load_autoplay();
    }

    /// Extends the queue with music like its last song. Safe to call repeatedly.
    fn load_autoplay(self: &Rc<Self>) {
        let seed = {
            let mut pb = self.playback.borrow_mut();
            let Some(seed) = pb.order.last().and_then(|&i| pb.queue.get(i)).cloned() else { return };
            if pb.autoplay_loading && pb.autoplay_seed == seed.id {
                return;
            }
            pb.autoplay_loading = true;
            pb.autoplay_seed = seed.id.clone();
            seed
        };
        let known: HashSet<String> =
            self.playback.borrow().queue.iter().map(|s| s.id.clone()).collect();
        let asked_for = seed.id.clone();
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
            move |ctl, result| ctl.autoplay_loaded(&asked_for, result.unwrap_or_default()),
        );
    }

    fn autoplay_loaded(self: &Rc<Self>, seed: &str, songs: Vec<Song>) {
        let (resume, found) = {
            let mut pb = self.playback.borrow_mut();
            // Asked for a queue that has since been replaced, and asked again for the new one.
            if pb.autoplay_seed != seed {
                return;
            }
            pb.autoplay_loading = false;
            self.changed();
            let resume = std::mem::take(&mut pb.autoplay_resume);
            // The queue may have been replaced while the request was out.
            let still_at_end = pb.following().is_none()
                && pb.order.last().and_then(|&i| pb.queue.get(i)).is_some_and(|last| last.id == seed);
            if !songs.is_empty() && still_at_end {
                for song in songs {
                    pb.queue.push(song);
                    let index = pb.queue.len() - 1;
                    pb.order.push(index);
                }
                let entries = pb.queue.len();
                pb.origins.resize(entries, None);
                // What follows no longer comes from the album or playlist that was chosen. (A
                // radio stays a radio: more of the same is what it is.)
                if !pb.context.ends_with(" radio") {
                    pb.context.clear();
                }
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
        self.changed();
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
            let _changing = self.changing();
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
        self.engine.set_equalizer(settings.equalizer, settings.eq_gains, settings.eq_preamp_now());
    }

    /// Restarts the current track where it is, to pick up a setting that's fixed when a
    /// track loads (streaming quality, loudness level).
    pub fn reload_current(self: &Rc<Self>) {
        let (loaded, position) = {
            let pb = self.playback.borrow();
            (pb.current_token != 0, pb.position)
        };
        if loaded {
            self.restart_current(position);
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
                // A downloaded copy plays as it was saved, which is the original only if it
                // was downloaded at full quality: one saved at a lower quality is an MP3.
                let suffix = song.suffix.to_lowercase();
                let bad = self.playback.borrow().bad_downloads.contains(&song.id);
                let saved = self.downloads.borrow().saved_as(song).filter(|_| !bad);
                let converted = saved.as_ref().filter(|(kind, _)| *kind != suffix).map(|(_, kbps)| nearest_rate(*kbps));
                let cap = match (&saved, converted) {
                    (_, Some(rate)) => rate,
                    (Some(_), None) => 0,
                    (None, None) => self.settings.borrow().max_bitrate(),
                };
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
                } else if converted.is_some() {
                    format!("“{title}” is {source}. The copy downloaded to this PC is a {cap} kbps MP3, and that is what's playing.")
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
        {
            // The song that's playing has had its turn: it's no longer waiting in the queue.
            let mut pb = self.playback.borrow_mut();
            if let Some(&entry) = pb.order.get(pb.cursor) {
                if let Some(song) = pb.queue.get_mut(entry) {
                    song.queued = false;
                }
            }
        }
        if self.show_remote() {
            return;
        }
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
        self.sync_playing();
        self.save_queue();
        if matches!(self.current_route(), Some(Route::Lyrics | Route::NowPlaying)) {
            self.load_lyrics();
        }
        self.show_format();
        #[cfg(windows)]
        self.media_track(current.as_ref());
    }

    pub fn sync_transport(self: &Rc<Self>) {
        if self.show_remote() {
            return;
        }
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
        if self.show_remote() {
            return;
        }
        // In a jam hosted here, songs guests queued say who queued them.
        let added_by = self.jam.borrow().added_by();
        let pb = self.playback.borrow();
        let ahead = pb.queued_ahead();
        let rows: Vec<ui::Track> = pb
            .order
            .iter()
            .skip(pb.cursor + 1)
            .take(QUEUE_ROWS)
            .filter_map(|&i| pb.queue.get(i))
            .map(|song| {
                let mut row = self.track_row(song);
                if let Some(name) = added_by.get(&song.id).filter(|_| song.queued) {
                    row.by = name.as_str().into();
                }
                row
            })
            .collect();
        drop(pb);
        self.show_queue(rows, ahead);
    }

    /// Shows what's to come in the queue panel. The first `ahead` rows were queued by hand
    /// and are listed as "Next in queue"; the rest are what's left of the list being played.
    pub fn show_queue(&self, mut rows: Vec<ui::Track>, ahead: usize) {
        self.up_next_model.set_vec(rows.iter().take(UP_NEXT_ROWS).cloned().collect::<Vec<_>>());
        // Where one part ends and the other begins there's a heading, which takes a row of
        // its own in the list (a row with no song in it).
        let gap = if ahead > 0 && ahead < rows.len() { ahead as i32 } else { -1 };
        if gap >= 0 {
            rows.insert(ahead, ui::Track::default());
        }
        let player = self.ui.global::<ui::Player>();
        player.set_queued(ahead.min(rows.len()) as i32);
        player.set_queue_gap(gap);
        // The keyboard's row in the queue panel can't be past the end of it, or on the heading.
        let cursor = player.get_queue_cursor().min(rows.len() as i32 - 1);
        player.set_queue_cursor(if cursor == gap && gap >= 0 { cursor - 1 } else { cursor });
        self.queue_model.set_vec(rows);
    }

    /// Which entry after the current track a row of the queue panel shows; `None` for the
    /// heading between its two parts.
    fn queue_place(&self, row: i32) -> Option<usize> {
        let gap = self.ui.global::<ui::Player>().get_queue_gap();
        if row < 0 || row == gap {
            return None;
        }
        Some(if gap >= 0 && row > gap { row - 1 } else { row } as usize)
    }

    /// A row of the queue panel was moved `delta` rows (by dragging, or with the keyboard).
    /// The heading counts as a row: a song moved above it joins "Next in queue", and one
    /// moved below it goes back among the rest.
    fn queue_row_moved(self: &Rc<Self>, row: i32, delta: i32) {
        let player = self.ui.global::<ui::Player>();
        let (gap, queued) = (player.get_queue_gap(), player.get_queued());
        let target = row + delta;
        if row < 0 || target < 0 || row == gap {
            return;
        }
        let (from, to, joins) = if gap < 0 {
            // No heading: everything shown is of one kind.
            (row, target, queued > 0)
        } else if row < gap {
            if target < gap { (row, target, true) } else { (row, target - 1, false) }
        } else if target <= gap {
            (row - 1, target, true)
        } else {
            (row - 1, target - 1, false)
        };
        self.move_upcoming(from as usize, to as usize, joins);
    }

    /// Does `change` to every row showing this song: in the queue, Recently played and the
    /// song list on screen. `change` returns whether it altered the row.
    pub fn each_row_of(&self, id: &str, mut change: impl FnMut(&mut ui::Track) -> bool) {
        let mut apply = |model: &VecModel<ui::Track>, row: usize| {
            if let Some(mut track) = model.row_data(row).filter(|track| track.id == id) {
                if change(&mut track) {
                    model.set_row_data(row, track);
                }
            }
        };
        // These three are short.
        for model in [&self.queue_model, &self.up_next_model, &self.history_model] {
            for row in 0..model.row_count() {
                apply(model, row);
            }
        }
        // The list on screen can be thousands of rows, so where each song is in it is noted
        // the first time it's needed.
        let mut places = self.track_places.borrow_mut();
        if places.is_empty() && self.tracks_model.row_count() > 0 {
            for (row, track) in self.tracks_model.iter().enumerate() {
                places.entry(track.id.to_string()).or_default().push(row);
            }
        }
        for &row in places.get(id).into_iter().flatten() {
            apply(&self.tracks_model, row);
        }
    }

    /// Applies a like or unlike to every copy of the song the player is showing.
    pub fn song_star_changed(self: &Rc<Self>, id: &str, starred: bool) {
        let player = self.ui.global::<ui::Player>();
        if player.get_track_id() == id {
            let mut track = player.get_track();
            track.starred = starred;
            player.set_track(track);
            // The like button on the taskbar thumbnail follows too.
            self.sync_tray();
        }
        self.each_row_of(id, |track| std::mem::replace(&mut track.starred, starred) != starred);
    }

    // ---- recently played -----------------------------------------------------------------------

    /// Notes that the current track has started playing.
    fn record_played(self: &Rc<Self>) {
        let Some(song) = self.playback.borrow().current().cloned() else { return };
        {
            let mut history = self.history.borrow_mut();
            // Reloads of the same track (a seek, a quality change) aren't new plays.
            if history.first().is_some_and(|last| last.song.id == song.id) {
                return;
            }
            history.insert(0, crate::config::Played { song, at: chrono::Utc::now().timestamp() });
            history.truncate(HISTORY_LENGTH);
            crate::config::save_history(&self.paths.history, &history);
        }
        self.sync_history();
        self.share_soon();
        self.fetch_lyrics_ahead();
    }

    /// How many entries at the top of the history the panel leaves out: the song playing now
    /// is already shown in the player bar.
    fn history_hidden(&self) -> usize {
        let pb = self.playback.borrow();
        let history = self.history.borrow();
        match (pb.current(), history.first()) {
            (Some(current), Some(last)) if current.id == last.song.id => 1,
            _ => 0,
        }
    }

    pub fn sync_history(self: &Rc<Self>) {
        // The mixes on Home are built from this too.
        crate::mixes::set_plays(self.history.borrow().iter().map(|played| (played.song.id.clone(), played.at)).collect());
        let rows: Vec<ui::Track> = self
            .history
            .borrow()
            .iter()
            .skip(self.history_hidden())
            .map(|played| self.track_row(&played.song))
            .collect();
        self.history_model.set_vec(rows);
    }

    fn history_song(&self, row: usize) -> Option<Song> {
        self.history.borrow().get(row + self.history_hidden()).map(|played| played.song.clone())
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
        // Note when the queue last really changed, and pass the change on to other devices.
        let fingerprint = queue_fingerprint(&pb);
        if self.queue_sig.replace(fingerprint) != fingerprint && !self.queue_quiet.get() {
            // (By this PC's own clock. It is set beside the server's when it is compared
            // with the time of another device's save: see `nexus_core::handover`.)
            self.queue_stamp.set(chrono::Utc::now().timestamp());
            self.queue_changed_here();
        }
        if pb.queue.is_empty() {
            // Nothing to come back to, unless the saved copy is being held on to (see `leave`).
            if !self.keep_saved_queue.get() {
                self.queue_saved.set(0);
                self.saver.remove(self.paths.queue.clone());
                self.saver.remove(self.paths.queue_place.clone());
            }
            return;
        }
        // The queue itself is thousands of songs and changes when you change it. Where
        // playback has got to changes all the time and is a few numbers. So they are kept
        // apart: the queue is written only when it's different, and both away from this
        // thread.
        let shape = crate::config::queue_shape(&pb.queue, &pb.order, &pb.context, &pb.source, &pb.origins, &pb.source_list);
        let stamp = self.queue_stamp.get();
        if self.queue_saved.replace(shape) != shape {
            let saved = SavedQueue {
                songs: pb.queue.clone(),
                order: pb.order.clone(),
                cursor: pb.cursor,
                position: pb.position,
                context: pb.context.clone(),
                source: pb.source.clone(),
                origins: pb.origins.clone(),
                stamp,
                source_list: pb.source_list.clone(),
            };
            self.saver.write(self.paths.queue.clone(), move || serde_json::to_vec(&saved).ok());
        }
        let place = crate::config::QueuePlace { shape, cursor: pb.cursor, position: pb.position, stamp, taken: self.queue_owner.taken() };
        self.saver.write(self.paths.queue_place.clone(), move || serde_json::to_vec(&place).ok());
    }

    /// Brings back the queue from the last run, paused where it stopped.
    pub fn restore_queue(self: &Rc<Self>) {
        // Anything still on its way to disk gets there first.
        self.saver.flush();
        let Some(mut saved) = SavedQueue::load(&self.paths.queue) else { return };
        let shape =
            crate::config::queue_shape(&saved.songs, &saved.order, &saved.context, &saved.source, &saved.origins, &saved.source_list);
        // Where playback had got to, if that was noted for this same queue.
        let place = crate::config::QueuePlace::load(&self.paths.queue_place);
        if let Some(place) = place.filter(|place| place.shape == shape && place.cursor < saved.order.len()) {
            saved.cursor = place.cursor;
            saved.position = place.position;
            saved.stamp = saved.stamp.max(place.stamp);
            // A queue taken up from another device is still that device's after a restart.
            self.queue_owner.restored(place.taken);
        }
        self.queue_saved.set(shape);
        {
            let mut pb = self.playback.borrow_mut();
            if !pb.queue.is_empty() {
                return;
            }
            pb.origins = saved.origins;
            pb.origins.resize(saved.songs.len(), None);
            pb.source = saved.source;
            pb.source_list = saved.source_list;
            pb.queue = saved.songs;
            pb.order = saved.order;
            pb.cursor = saved.cursor;
            pb.context = saved.context;
            pb.position = saved.position;
            pb.resume_at = saved.position;
            pb.playing = false;
            pb.current_token = 0;
        }
        // A queue saved before changes were timed: the file's own date says when it was.
        let stamp = if saved.stamp > 0 {
            saved.stamp
        } else {
            std::fs::metadata(&self.paths.queue)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|since| since.as_secs() as i64)
                .unwrap_or(0)
        };
        self.queue_stamp.set(stamp);
        // Putting it back isn't a change to pass on.
        self.queue_quiet.set(true);
        self.track_changed();
        self.queue_quiet.set(false);
    }

    /// Puts a queue from another device in place of this one, paused at `position` seconds
    /// into the song at `cursor`.
    pub fn replace_queue(self: &Rc<Self>, songs: Vec<Song>, cursor: usize, position: f64) {
        if songs.is_empty() {
            return;
        }
        let _changing = self.changing();
        {
            let mut pb = self.playback.borrow_mut();
            pb.origins = vec![None; songs.len()];
            pb.source.clear();
            pb.source_list.clear();
            pb.order = (0..songs.len()).collect();
            pb.cursor = cursor.min(songs.len() - 1);
            pb.queue = songs;
            pb.context.clear();
            pb.position = position;
            pb.resume_at = position;
            pb.playing = false;
            pb.current_token = 0;
            pb.next_token = 0;
        }
        self.queue_quiet.set(true);
        self.track_changed();
        self.queue_quiet.set(false);
    }

    // ---- lyrics --------------------------------------------------------------------------------

    pub fn load_lyrics(self: &Rc<Self>) {
        let lyrics = self.ui.global::<ui::Lyrics>();
        let Some(song) = self.playback.borrow().current().cloned() else {
            *self.lyrics.borrow_mut() = LyricsState::default();
            lyrics.set_lines(ModelRc::default());
            lyrics.set_status("Play a song to see its lyrics".into());
            lyrics.set_note("".into());
            return;
        };
        if self.lyrics.borrow().song_id == song.id {
            return;
        }
        *self.lyrics.borrow_mut() = LyricsState { song_id: song.id.clone(), ..Default::default() };
        // What's kept from the last time the server was asked shows at once.
        let dir = self.lyrics_dir();
        let had = dir.as_ref().and_then(|dir| crate::lyrics::kept(dir, &song.id)).map(|kept| kept.lyrics);
        match &had {
            Some(kept) => self.show_lyrics(kept.clone()),
            None => {
                lyrics.set_lines(ModelRc::default());
                lyrics.set_status("Looking for lyrics…".into());
                lyrics.set_note("".into());
            }
        }
        // The server is asked all the same: for a first look, or in case they've changed.
        let id = song.id.clone();
        self.request(
            move |client| {
                let found = client.lyrics(&id)?;
                if let Some(dir) = &dir {
                    crate::lyrics::keep(dir, &id, &found);
                }
                Ok(found)
            },
            move |ctl, result| {
                if ctl.lyrics.borrow().song_id != song.id {
                    return;
                }
                match result {
                    // Only redrawn if they differ, so the page isn't disturbed for nothing.
                    Ok(found) => {
                        if had.as_ref() != Some(&found) {
                            ctl.show_lyrics(found);
                        }
                    }
                    // The server couldn't be asked: what's kept stays; with nothing kept, say so.
                    Err(_) => {
                        if had.is_none() {
                            ctl.show_lyrics(None);
                        }
                    }
                }
            },
        );
    }

    /// Fetches the lyrics of the song playing and the next few in the queue, so they're
    /// on hand before anyone opens them. Each song is asked about once a session; opening
    /// its lyrics asks again.
    pub fn fetch_lyrics_ahead(self: &Rc<Self>) {
        const AHEAD: usize = 5;
        const FRESH: std::time::Duration = std::time::Duration::from_secs(24 * 3600);
        if self.is_offline() {
            return;
        }
        let (Some(client), Some(dir)) = (self.client(), self.lyrics_dir()) else { return };
        let wanted: Vec<String> = {
            let pb = self.playback.borrow();
            let mut asked = self.lyrics_asked.borrow_mut();
            pb.order
                .iter()
                .skip(pb.cursor)
                .take(AHEAD + 1)
                .filter_map(|&i| pb.queue.get(i))
                .filter(|song| !song.id.is_empty() && song.radio_url.is_empty())
                .filter(|song| asked.insert(song.id.clone()))
                .map(|song| song.id.clone())
                .collect()
        };
        if wanted.is_empty() {
            return;
        }
        self.api_pool.spawn(move || {
            for id in wanted {
                // Asked lately and it had some: no need. One that had none is asked about
                // again, since lyrics get added to a library after the songs do.
                if crate::lyrics::kept(&dir, &id).is_some_and(|kept| kept.lyrics.is_some() && kept.age < FRESH) {
                    continue;
                }
                if let Ok(found) = client.lyrics(&id) {
                    crate::lyrics::keep(&dir, &id, &found);
                }
            }
        });
    }

    fn show_lyrics(self: &Rc<Self>, found: Option<Lyrics>) {
        let lyrics = self.ui.global::<ui::Lyrics>();
        let Some(found) = found else {
            // Whatever was on screen belonged to another song: take it away.
            {
                let mut state = self.lyrics.borrow_mut();
                state.times.clear();
                state.active = -1;
            }
            lyrics.set_lines(ModelRc::default());
            lyrics.set_timed(false);
            lyrics.set_active(-1);
            lyrics.set_status("No lyrics for this song".into());
            lyrics.set_note("Your music server doesn't have any for it.".into());
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
        player.on_seek_by(|seconds| {
            with(|ctl| {
                let (position, duration) = {
                    let pb = ctl.playback.borrow();
                    (pb.position, pb.current().map_or(0.0, |song| song.duration as f64))
                };
                // A radio station has no length to move about in.
                if duration > 0.0 {
                    ctl.seek((position + seconds as f64).clamp(0.0, (duration - 1.0).max(0.0)));
                }
            });
        });
        player.on_queue_remove(|index| {
            with(|ctl| {
                if let Some(place) = ctl.queue_place(index) {
                    ctl.remove_upcoming(place);
                }
            });
        });
        player.on_toggle_mute(|| {
            with(|ctl| {
                {
                    let mut pb = ctl.playback.borrow_mut();
                    ctl.changed();
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
        player.on_open_mini(|| {
            with(|ctl| ctl.open_mini());
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
            with(|ctl| {
                if let Some(place) = ctl.queue_place(index) {
                    ctl.jump_to_upcoming(place);
                }
            });
        });
        player.on_queue_menu(|index, x, y| {
            with(|ctl| {
                // -1 is the song that's playing.
                if index < 0 {
                    ctl.open_queue_menu(-1, x, y);
                } else if let Some(place) = ctl.queue_place(index) {
                    ctl.open_queue_menu(place as i32, x, y);
                }
            });
        });
        player.on_clear_queue(|queued| {
            with(|ctl| ctl.clear_upcoming(queued));
        });
        player.on_continue_here(|| {
            with(|ctl| ctl.continue_here());
        });
        player.on_save_queue(|| {
            with(|ctl| {
                // What's playing and everything after it, in the order it will play.
                let songs: Vec<Song> = {
                    let pb = ctl.playback.borrow();
                    pb.order.iter().skip(pb.cursor).filter_map(|&i| pb.queue.get(i)).filter(|s| s.radio_url.is_empty() && !s.id.is_empty()).cloned().collect()
                };
                if !songs.is_empty() {
                    ctl.prompt_new_playlist(songs);
                }
            });
        });
        player.on_queue_moved(|index, delta| {
            with(|ctl| ctl.queue_row_moved(index, delta));
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
        // A row that only names a song (one an import couldn't find) has nothing to play.
        if songs.get(index).is_some_and(|song| song.id.is_empty() && song.radio_url.is_empty()) {
            return;
        }
        // From a search, what's wanted is that song and then more like it, not whatever else
        // happened to match the words.
        if matches!(self.current_route(), Some(Route::Search(_))) {
            if let Some(song) = songs.get(index).cloned() {
                return self.play_radio(song);
            }
        }
        let context = self.page.borrow().context.clone();
        self.playing_from_page();
        if index < songs.len() {
            // Starting from a chosen song plays the list in order, even with shuffle on.
            self.play_songs_from(songs, index, &context);
        }
    }

    /// Notes that the next thing played is the page on screen, as it's shown, so its rows
    /// can be matched to the queue.
    fn playing_from_page(&self) {
        let key = self.current_route().map(|route| route.cache_key()).unwrap_or_default();
        let rows = (0..self.tracks_model.row_count()).filter_map(|row| self.song_index(row)).collect();
        let list = self.page.borrow().songs.iter().map(|song| song.id.clone()).collect();
        self.play_from.replace(Some(PlayFrom { key, rows, list }));
    }

    /// Marks the row that's playing, if the page on screen is where it was played from.
    pub fn sync_playing(&self) {
        let playing = {
            let pb = self.playback.borrow();
            let key = self.current_route().map(|route| route.cache_key()).unwrap_or_default();
            // Play buttons for the list the queue came from pause and resume it.
            let source = if pb.current().is_some() { pb.source.as_str() } else { "" };
            self.ui.global::<ui::App>().set_playing_source(source.into());
            self.ui.global::<ui::View>().set_playing_here(!source.is_empty() && source == key);
            if !pb.source.is_empty() && pb.source == key { pb.current_origin() } else { None }
        };
        for row in 0..self.tracks_model.row_count() {
            let Some(mut track) = self.tracks_model.row_data(row) else { continue };
            let is_playing = playing.is_some() && self.song_index(row) == playing;
            if track.playing != is_playing {
                track.playing = is_playing;
                self.tracks_model.set_row_data(row, track);
            }
        }
    }

    /// Tells the queue how the list `key` stands now. If that's the list the queue was
    /// started from and it has changed (songs added, removed or moved, here or on another
    /// device), the queue follows: see `follow_list`.
    pub fn source_list_now(self: &Rc<Self>, key: &str, songs: &[Song]) {
        let ids: Vec<String> = songs.iter().map(|song| song.id.clone()).collect();
        {
            let pb = self.playback.borrow();
            // (A queue from before lists were kept has nothing to compare with.)
            if key.is_empty() || pb.source != key || pb.queue.is_empty() || pb.source_list.is_empty() || pb.source_list == ids {
                return;
            }
        }
        let view = self.order_for(key, songs);
        self.changed();
        {
            let mut pb = self.playback.borrow_mut();
            let (moved, arrivals) = compare_lists(&pb.source_list, &ids);
            let lineup = Lineup {
                songs: std::mem::take(&mut pb.queue),
                origins: std::mem::take(&mut pb.origins),
                order: std::mem::take(&mut pb.order),
                cursor: pb.cursor,
            };
            let offline = self.is_offline();
            let downloads = self.downloads.borrow();
            // Offline, a song that isn't downloaded can't join the queue.
            let song_at = |place: usize| songs.get(place).filter(|song| !offline || downloads.has(&song.id)).cloned();
            let lineup = follow_list(lineup, pb.shuffle, &moved, &arrivals, &view, &song_at, &mut |n| fastrand::usize(..n.max(1)));
            pb.queue = lineup.songs;
            pb.origins = lineup.origins;
            pb.order = lineup.order;
            pb.cursor = lineup.cursor;
            pb.source_list = ids;
        }
        // What plays next may have changed; this also redraws and saves the queue.
        self.next_changed();
        self.sync_playing();
    }

    /// Plays `songs` beginning with the one at `index`; with shuffle on, that song goes first.
    fn play_songs_from(self: &Rc<Self>, songs: Vec<Song>, index: usize, context: &str) {
        self.play_songs(songs, index, context, false);
    }

    /// Whether the queue was started from the list with this route key.
    pub fn is_playing_from(&self, key: &str) -> bool {
        let pb = self.playback.borrow();
        !key.is_empty() && pb.source == key && pb.current().is_some()
    }

    /// The page's play button: plays its list from the top, or shuffled if the list is set
    /// to shuffle. If the list is already what's playing, it pauses or resumes instead.
    pub fn play_page(self: &Rc<Self>) {
        // If this page's list is what's playing, the button pauses and resumes it.
        let key = self.current_route().map(|route| route.cache_key()).unwrap_or_default();
        if self.is_playing_from(&key) {
            return self.toggle_play();
        }
        let (source, context) = {
            let page = self.page.borrow();
            (page.play.clone(), page.context.clone())
        };
        // A sorted or filtered list plays as it's shown.
        let songs = self.visible_songs();
        match source {
            crate::pages::PlaySource::Songs => {
                self.playing_from_page();
                self.play_songs(songs, 0, &context, true)
            }
            crate::pages::PlaySource::Albums(ids) => {
                let key = self.current_route().map(|route| route.cache_key()).unwrap_or_default();
                // Until the songs have been looked up, this PC is busy to other devices.
                let waiting = self.looking_up();
                self.request(
                    move |client| {
                        let mut all = Vec::new();
                        for id in ids.iter().take(40) {
                            all.extend(client.album(id)?.songs);
                        }
                        Ok(all)
                    },
                    move |ctl, result| match result {
                        Ok(all) => {
                            ctl.looked_up(waiting);
                            // Played from this page, though its rows aren't these songs.
                            ctl.play_from.replace(Some(PlayFrom { key, rows: Vec::new(), list: Vec::new() }));
                            ctl.play_songs(all, 0, &context, true)
                        }
                        Err(e) => {
                            ctl.looked_up(waiting);
                            ctl.toast(e.to_string())
                        }
                    },
                );
            }
        }
    }
}

/// The fewest bytes a whole converted song can come to: most of what its length works out at,
/// at the bit rate asked for or the file's own if that's lower. A conversion doesn't say how
/// long it will be, so one that ends well short of this was cut off, and isn't kept.
fn least_bytes(song: &Song, bitrate: u32) -> u64 {
    let rate = if song.bit_rate > 0 { bitrate.min(song.bit_rate) } else { bitrate };
    song.duration as u64 * rate as u64 * 125 * 4 / 5
}

/// The usual MP3 bit rate nearest to one worked out from a file's size.
fn nearest_rate(kbps: u32) -> u32 {
    [96u32, 128, 160, 192, 256, 320].into_iter().min_by_key(|rate| rate.abs_diff(kbps)).unwrap_or(320)
}

/// The form a song is streamed in: the bit rate to ask for (0 for the file as it is) and
/// the kind of file that comes back. `native` is whether the app can play the file as it is;
/// `cap` is the bit rate limit in Settings, 0 for none.
fn stream_form(native: bool, cap: u32, suffix: &str) -> (u32, &str) {
    if native && cap == 0 { (0, suffix) } else { (if cap == 0 { 320 } else { cap }, "mp3") }
}

/// The gain to apply to a song so it lands at the chosen loudness, from its ReplayGain tags.
/// `album_run` says the song is playing as part of its album (see `in_album_order`).
fn level_gain(settings: &Settings, song: &Song, album_run: bool) -> f32 {
    if !settings.normalize {
        return 1.0;
    }
    let (_, offset) = VOLUME_LEVELS[(settings.volume_level as usize).min(VOLUME_LEVELS.len() - 1)];
    // A song with no tags to go by is treated as if its tag said a set amount, about what a
    // typical song's does, so it doesn't jump out among tagged ones. Never turned up: nothing
    // says how much room its peaks leave. (0 leaves such songs exactly as they are.)
    let untagged = if settings.untagged_db < 0.0 { 10f32.powf((settings.untagged_db + offset) / 20.0).min(1.0) } else { 1.0 };
    let Some(tags) = song.replay_gain.as_ref() else { return untagged };
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
            None => return untagged,
        },
    };
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

    fn named(id: &str) -> Song {
        Song { id: id.to_string(), ..Default::default() }
    }

    #[test]
    fn turns_down_songs_that_have_no_levelling_tags() {
        let level = |settings: &Settings| level_gain(settings, &named("untagged"), false);
        let decibels = |gain: f32| 20.0 * gain.log10();
        // Treated as tagged "6 dB down" unless told otherwise: at the Quiet level (1 dB under
        // the tags' own reference) that's 7 dB down, at Normal (4 dB over it) 2 dB down.
        let mut settings = Settings { normalize: true, volume_level: 0, ..Default::default() };
        assert!((decibels(level(&settings)) + 7.0).abs() < 0.01);
        settings.volume_level = 1;
        assert!((decibels(level(&settings)) + 2.0).abs() < 0.01);
        settings.untagged_db = -9.0;
        assert!((decibels(level(&settings)) + 5.0).abs() < 0.01);
        // Never up: nothing says how much room the song's peaks leave.
        settings.untagged_db = -3.0;
        assert_eq!(level(&settings), 1.0);
        // "Leave" leaves them exactly as they are, at every level.
        settings.untagged_db = 0.0;
        settings.volume_level = 0;
        assert_eq!(level(&settings), 1.0);
        // With levelling off, nothing is touched.
        settings = Settings { normalize: false, untagged_db: -9.0, ..Default::default() };
        assert_eq!(level(&settings), 1.0);
        // A song with tags that say nothing about the track goes the same way as one without.
        let empty = Song { replay_gain: Some(ReplayGain { track_gain: None, album_gain: None, track_peak: None, album_peak: None }), ..Default::default() };
        let settings = Settings { normalize: true, volume_level: 1, ..Default::default() };
        assert!((decibels(level_gain(&settings, &empty, false)) + 2.0).abs() < 0.01);
    }

    /// A queue of these songs in this order, with the first one playing.
    fn queue_of(ids: &[&str]) -> Playback {
        let mut pb = Playback::new(&Settings::default());
        pb.queue = ids.iter().map(|id| named(id)).collect();
        pb.order = (0..ids.len()).collect();
        pb.origins = vec![None; ids.len()];
        pb
    }

    /// What's playing, then what's to come, with a star on the songs queued by hand.
    fn coming(pb: &Playback) -> String {
        let name = |&entry: &usize| format!("{}{}", pb.queue[entry].id, if pb.queue[entry].queued { "*" } else { "" });
        format!("{} | {}", pb.queue[pb.order[pb.cursor]].id, pb.order.iter().skip(pb.cursor + 1).map(name).collect::<Vec<_>>().join(" "))
    }

    #[test]
    fn songs_queued_by_hand_play_before_the_rest_of_the_list() {
        let mut pb = queue_of(&["a", "b", "c"]);
        pb.queue_up(vec![named("x")], false);
        assert_eq!(coming(&pb), "a | x* b c");
        // The next one queued goes behind the first, not in front of it...
        pb.queue_up(vec![named("y")], false);
        assert_eq!(coming(&pb), "a | x* y* b c");
        // ...unless it's to play next. A song that's already in the list is queued all the
        // same, and stays in the list too.
        pb.queue_up(vec![named("c")], true);
        assert_eq!(coming(&pb), "a | c* x* y* b c");
        assert_eq!(pb.queued_ahead(), 3);
        // Once the first of them is playing, two are left waiting.
        pb.cursor += 1;
        assert_eq!(pb.queued_ahead(), 2);
    }

    #[test]
    fn jumping_ahead_keeps_what_was_queued_by_hand() {
        let mut pb = queue_of(&["a", "b", "c", "d"]);
        pb.queue_up(vec![named("x"), named("y")], false);
        // To a song of the list: the queued songs wait behind it.
        assert!(pb.jump_ahead(3));
        assert_eq!(coming(&pb), "c | x* y* d");
        // To one of the queued songs: the others stay where they were.
        assert!(pb.jump_ahead(1));
        assert_eq!(coming(&pb), "y | x* d");
        assert!(!pb.jump_ahead(9));
    }

    #[test]
    fn moving_a_song_takes_it_into_the_queued_part_or_out_of_it() {
        let mut pb = queue_of(&["a", "b", "c"]);
        pb.queue_up(vec![named("x"), named("y")], false);
        pb.move_ahead(2, 0, true);
        assert_eq!(coming(&pb), "a | b* x* y* c");
        pb.move_ahead(1, 3, false);
        assert_eq!(coming(&pb), "a | b* y* c x");
        // Out of range: nothing happens.
        pb.move_ahead(1, 9, true);
        assert_eq!(coming(&pb), "a | b* y* c x");
    }

    #[test]
    fn clears_one_part_of_the_queue_and_leaves_the_other() {
        let mut pb = queue_of(&["a", "b", "c"]);
        pb.cursor = 1;
        pb.queue_up(vec![named("x"), named("y")], false);
        assert_eq!(coming(&pb), "b | x* y* c");
        let mut list = queue_of(&["a", "b", "c"]);
        list.cursor = 1;
        list.queue_up(vec![named("x"), named("y")], false);
        list.clear_ahead(false);
        assert_eq!(coming(&list), "b | x* y*");
        pb.clear_ahead(true);
        assert_eq!(coming(&pb), "b | c");
        // What was played before is still there to go back to.
        assert_eq!(pb.queue[pb.order[0]].id, "a");
    }

    #[test]
    fn shuffling_leaves_what_was_queued_by_hand_next() {
        let mut pb = queue_of(&["a", "b", "c", "d", "e", "f"]);
        pb.cursor = 2;
        pb.queue_up(vec![named("x"), named("y")], false);
        pb.rearrange_keeping_queued(|pb| pb.shuffle_order());
        assert!(coming(&pb).starts_with("c | x* y* "), "{}", coming(&pb));
        assert_eq!(pb.order.len(), 8);
        // And turning shuffle off again puts the list back in order around them.
        let current = pb.order[pb.cursor];
        pb.rearrange_keeping_queued(|pb| {
            pb.order = (0..pb.queue.len()).collect();
            pb.cursor = current;
        });
        assert_eq!(coming(&pb), "c | x* y* d e f");
    }

    #[test]
    fn dropping_an_entry_keeps_the_place_in_the_queue() {
        let mut pb = queue_of(&["a", "b", "c", "d"]);
        pb.cursor = 2;
        pb.drop_at(0);
        assert_eq!(coming(&pb), "c | d");
        pb.drop_at(2);
        assert_eq!(coming(&pb), "c | ");
        assert_eq!(pb.queue.len(), 2);
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
