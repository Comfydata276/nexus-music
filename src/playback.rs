//! The play queue and transport: what plays next, shuffle and repeat, seeking, play counts
//! and lyrics. All on the UI thread; the audio itself is the engine's business.

use std::collections::HashSet;
use std::path::PathBuf;
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
    /// Songs whose kept copy was thrown away because it wouldn't play.
    dropped_kept: HashSet<String>,
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
            transcoded: false,
            listened: 0.0,
            last_tick: 0.0,
            scrobbled: false,
            saved_at: 0.0,
            needs_transcode: HashSet::new(),
            failures: 0,
            dropped_kept: HashSet::new(),
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
}

/// Where songs about to be played came from: the page's route key, each song's position in
/// that page's list, and the ids of the whole list.
pub struct PlayFrom {
    pub key: String,
    pub rows: Vec<usize>,
    pub list: Vec<String>,
}

/// Compares a list of song ids as it was with how it is now. Returns where each old entry
/// has gone (`None` if it's no longer there), and which of the new entries are arrivals.
/// A song that's in the list more than once is matched copy for copy.
fn compare_lists(old: &[String], new: &[String]) -> (Vec<Option<usize>>, Vec<usize>) {
    let mut places: std::collections::HashMap<&str, std::collections::VecDeque<usize>> = Default::default();
    for (index, id) in new.iter().enumerate() {
        places.entry(id.as_str()).or_default().push_back(index);
    }
    let moved: Vec<Option<usize>> =
        old.iter().map(|id| places.get_mut(id.as_str()).and_then(|at| at.pop_front())).collect();
    let mut arrivals: Vec<usize> = places.into_values().flatten().collect();
    arrivals.sort_unstable();
    (moved, arrivals)
}

/// The queue: its songs, where each sits in the list it came from, the order they play in
/// (as positions in `songs`), and which of those is playing.
#[derive(Debug, Clone, PartialEq)]
struct Lineup {
    songs: Vec<Song>,
    origins: Vec<Option<usize>>,
    order: Vec<usize>,
    cursor: usize,
}

/// Works out the queue after the list it came from has changed.
///
/// Songs that left the list leave the queue, except the one playing. Songs that joined it
/// are added: in their place if the queue plays in the list's order, somewhere among what's
/// still to come if it's shuffled. Songs the list was reordered around take up the new
/// order. Songs that were never part of the list (added to the queue by hand) keep their
/// place after whatever they followed.
///
/// `moved` and `arrivals` are from `compare_lists`; `view` is the new list in the order its
/// page shows it; `song_at` gives the song at a position in the new list; `pick(n)` chooses
/// a number below `n`.
fn follow_list(
    lineup: Lineup,
    shuffled: bool,
    moved: &[Option<usize>],
    arrivals: &[usize],
    view: &[usize],
    song_at: &dyn Fn(usize) -> Option<Song>,
    pick: &mut dyn FnMut(usize) -> usize,
) -> Lineup {
    let Lineup { songs, origins, order, cursor } = lineup;
    let Some(&current) = order.get(cursor) else {
        return Lineup { songs, origins, order, cursor };
    };
    // Where each entry now sits in the list, and which entries go.
    let now: Vec<Option<usize>> =
        origins.iter().map(|origin| origin.and_then(|at| moved.get(at).copied().flatten())).collect();
    let gone = |entry: usize| origins[entry].is_some() && now[entry].is_none() && entry != current;
    // The arrangement to keep for entries that aren't in the list: the playing order, or
    // when shuffled the order underneath the shuffle.
    let base: Vec<usize> = if shuffled { (0..songs.len()).collect() } else { order.clone() };
    let mut followers: std::collections::HashMap<Option<usize>, Vec<usize>> = Default::default();
    let mut holder: std::collections::HashMap<usize, usize> = Default::default();
    let mut last = None;
    for &entry in base.iter().filter(|&&entry| !gone(entry)) {
        match now[entry] {
            Some(place) => {
                holder.insert(place, entry);
                last = Some(entry);
            }
            None => followers.entry(last).or_default().push(entry),
        }
    }
    enum Item {
        Kept(usize),
        Arrival(usize),
    }
    let mut items: Vec<Item> = followers.remove(&None).unwrap_or_default().into_iter().map(Item::Kept).collect();
    for &place in view {
        if let Some(&entry) = holder.get(&place) {
            items.push(Item::Kept(entry));
            items.extend(followers.remove(&Some(entry)).unwrap_or_default().into_iter().map(Item::Kept));
        } else if arrivals.binary_search(&place).is_ok() && song_at(place).is_some() {
            items.push(Item::Arrival(place));
        }
    }
    // The new set of songs, and where each old entry went in it.
    let mut new_songs = Vec::with_capacity(items.len());
    let mut new_origins = Vec::with_capacity(items.len());
    let mut went: Vec<Option<usize>> = vec![None; songs.len()];
    let mut added = Vec::new();
    for item in &items {
        match *item {
            Item::Kept(entry) => {
                went[entry] = Some(new_songs.len());
                new_songs.push(songs[entry].clone());
                new_origins.push(now[entry]);
            }
            Item::Arrival(place) => {
                added.push(new_songs.len());
                new_songs.extend(song_at(place));
                new_origins.push(Some(place));
            }
        }
    }
    let playing = went[current].unwrap_or(0);
    let (new_order, new_cursor) = if shuffled {
        let mut new_order: Vec<usize> = order.iter().filter_map(|&entry| went[entry]).collect();
        let at = new_order.iter().position(|&entry| entry == playing).unwrap_or(0);
        for entry in added {
            // Somewhere after the song that's playing.
            let slot = at + 1 + pick(new_order.len() - at);
            new_order.insert(slot.min(new_order.len()), entry);
        }
        (new_order, at)
    } else {
        ((0..new_songs.len()).collect(), playing)
    };
    Lineup { songs: new_songs, origins: new_origins, order: new_order, cursor: new_cursor }
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
                keep: None,
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
                keep: None,
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
                suffix: kept_as.to_string(),
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
                keep: kept,
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
                // Only a conversion that starts at the beginning is the whole song.
                keep: kept.filter(|_| offset == 0.0),
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
            crate::songcache::ready(&self.song_cache.file(&song.id, bitrate, kept_as))
        })
    }

    /// Has the next few songs fetched ahead of time, so the music carries on if the
    /// connection drops, and makes room among the kept songs when they outgrow their limit.
    /// Called every few seconds while something is playing.
    pub fn keep_ahead(self: &Rc<Self>) {
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
                    wanted.push(crate::songcache::Wanted { client: client.clone(), url: client.stream_url(&song.id, bitrate, 0), path });
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
        // Keeps an edited queue across a restart, and tells other devices about it.
        self.save_queue();
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
        let on = !self.playback.borrow().shuffle;
        self.set_shuffle(on);
    }

    /// Turns shuffle on or off for what's playing. The current song carries on: turning it
    /// on shuffles what follows; turning it off continues from this song in the list's order.
    fn set_shuffle(self: &Rc<Self>, on: bool) {
        let source = {
            let mut pb = self.playback.borrow_mut();
            if pb.shuffle != on {
                pb.shuffle = on;
                if on {
                    pb.shuffle_order();
                } else if let Some(&current) = pb.order.get(pb.cursor) {
                    pb.order = (0..pb.queue.len()).collect();
                    pb.cursor = current;
                }
            }
            pb.source.clone()
        };
        self.settings.borrow_mut().shuffle = on;
        self.save_settings();
        // The list being played remembers the choice for next time.
        self.update_list_prefs(&source, |prefs| prefs.shuffle = on);
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
            self.update_list_prefs(&key, |prefs| prefs.shuffle = on);
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
                pb.origins = vec![None; songs.len()];
                pb.source.clear();
                pb.source_list.clear();
            pb.source_list.clear();
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
            let entries = pb.queue.len();
            pb.origins.resize(entries, None);
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
            if removed < pb.origins.len() {
                pb.origins.remove(removed);
            }
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
            pb.origins = vec![pb.current_origin()];
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
                    // So is a copy kept from an earlier play (once: if the song still won't
                    // play after that, the copy wasn't what was wrong).
                    let (kept, _) = self.kept_file(&pb, &song);
                    if crate::songcache::ready(&kept) && pb.dropped_kept.insert(song.id.clone()) {
                        let position = pb.position;
                        drop(pb);
                        let _ = std::fs::remove_file(&kept);
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
        self.tick_downloads();
        // Every few seconds while there's a queue, see that the next songs are on hand.
        let keep_ticks = self.keep_ticks.get() + 1;
        self.keep_ticks.set(keep_ticks);
        if keep_ticks % 16 == 0 && self.playback.borrow().current_token != 0 {
            self.keep_ahead();
        }
        self.tick_requests();
        if self.mini_open.get() {
            self.sync_mini();
        }
        if keep_ticks % 8 == 0 {
            self.follow_lyrics_setting();
        }
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
            self.play_log.borrow_mut().add(&id, chrono::Utc::now().timestamp());
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
            // The like button on the taskbar thumbnail follows too.
            self.sync_tray();
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
            if history.first().is_some_and(|last| last.song.id == song.id) {
                return;
            }
            history.insert(0, crate::config::Played { song, at: chrono::Utc::now().timestamp() });
            history.truncate(HISTORY_LENGTH);
            crate::config::save_history(&self.paths.history, &history);
        }
        self.sync_history();
        self.share_soon();
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
            self.queue_stamp.set(chrono::Utc::now().timestamp());
            self.queue_changed_here();
        }
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
            source: pb.source.clone(),
            origins: pb.origins.clone(),
            stamp: self.queue_stamp.get(),
            source_list: pb.source_list.clone(),
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

    /// Has the lyrics worker ask or not, as the setting and the connection allow, and says
    /// in Settings how far through the library it's got.
    pub fn follow_lyrics_setting(&self) {
        let on = self.settings.borrow().online_lyrics;
        crate::lyrics::set_online(on && !self.is_offline());
        let (found, without, total) = crate::lyrics::progress();
        let status = if !on {
            String::new()
        } else if total == 0 {
            "Waiting for your library before looking any up.".to_string()
        } else if found + without >= total {
            format!("Lyrics are kept for {found} of your {total} songs. LRCLIB has none for the rest.")
        } else {
            format!("Lyrics are kept for {found} of your {total} songs so far, with {} still to look up.", total - found - without)
        };
        let page = self.ui.global::<ui::Settings>();
        if page.get_lyrics_status() != status.as_str() {
            page.set_lyrics_status(status.into());
        }
    }

    /// Looks for the current song's lyrics afresh, if they're on screen.
    pub fn reload_lyrics(self: &Rc<Self>) {
        self.lyrics.borrow_mut().song_id.clear();
        if matches!(self.current_route(), Some(Route::Lyrics | Route::NowPlaying)) {
            self.load_lyrics();
        }
    }

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
        let wanted = song.clone();
        self.request(
            move |client| {
                // The server's own first, if they're timed. Otherwise what's kept from
                // LRCLIB or can be had from it, and failing that whatever the server has.
                let server = client.lyrics(&wanted.id).ok().flatten();
                if server.as_ref().is_some_and(|lyrics| lyrics.synced) {
                    return Ok(server);
                }
                let timed = crate::lyrics::kept(&wanted.id).filter(|lyrics| lyrics.synced);
                Ok(timed.or(server).or_else(|| crate::lyrics::for_song(&wanted)))
            },
            move |ctl, result: crate::subsonic::ApiResult<Option<Lyrics>>| {
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
                            // Played from this page, though its rows aren't these songs.
                            ctl.play_from.replace(Some(PlayFrom { key, rows: Vec::new(), list: Vec::new() }));
                            ctl.play_songs(all, 0, &context, true)
                        }
                        Err(e) => ctl.toast(e.to_string()),
                    },
                );
            }
        }
    }
}

/// The gain to apply to a song so it lands at the chosen loudness, from its ReplayGain tags.
/// `album_run` says the song is playing alongside others from its album.
/// The form a song is streamed in: the bit rate to ask for (0 for the file as it is) and
/// the kind of file that comes back. `native` is whether the app can play the file as it is;
/// `cap` is the bit rate limit in Settings, 0 for none.
fn stream_form(native: bool, cap: u32, suffix: &str) -> (u32, &str) {
    if native && cap == 0 { (0, suffix) } else { (if cap == 0 { 320 } else { cap }, "mp3") }
}

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

    fn named(id: &str) -> Song {
        Song { id: id.to_string(), ..Default::default() }
    }

    /// A queue of these songs, each from the same place in its list, playing the one at `cursor`.
    fn lineup(ids: &[&str], cursor: usize) -> Lineup {
        Lineup {
            songs: ids.iter().map(|id| named(id)).collect(),
            origins: (0..ids.len()).map(Some).collect(),
            order: (0..ids.len()).collect(),
            cursor,
        }
    }

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| id.to_string()).collect()
    }

    /// The queue in playing order, with the song that's playing in brackets.
    fn playing_order(lineup: &Lineup) -> String {
        lineup
            .order
            .iter()
            .enumerate()
            .map(|(at, &entry)| {
                let id = &lineup.songs[entry].id;
                if at == lineup.cursor { format!("[{id}]") } else { id.clone() }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn follow(lineup: Lineup, shuffled: bool, old: &[&str], new: &[&str], view: Option<&[usize]>) -> Lineup {
        let (moved, arrivals) = compare_lists(&ids(old), &ids(new));
        let in_order: Vec<usize> = (0..new.len()).collect();
        let song_at = |place: usize| new.get(place).map(|id| named(id));
        // "Random" here means as late as possible, to keep the tests the same every run.
        follow_list(lineup, shuffled, &moved, &arrivals, view.unwrap_or(&in_order), &song_at, &mut |n| n - 1)
    }

    #[test]
    fn compares_lists_copy_for_copy() {
        let (moved, arrivals) = compare_lists(&ids(&["a", "b", "a", "c"]), &ids(&["b", "a", "d", "a", "a"]));
        assert_eq!(moved, [Some(1), Some(0), Some(3), None]);
        assert_eq!(arrivals, [2, 4]);
    }

    #[test]
    fn the_queue_follows_its_list() {
        let list = ["a", "b", "c", "d"];
        // Songs added to the list join the queue in their place; removed ones leave it.
        let after = follow(lineup(&list, 1), false, &list, &["a", "b", "x", "c", "y"], None);
        assert_eq!(playing_order(&after), "a [b] x c y");
        assert_eq!(after.origins, [Some(0), Some(1), Some(2), Some(3), Some(4)]);
        // The song that's playing stays even if it was removed, and what followed it still does.
        let after = follow(lineup(&list, 1), false, &list, &["a", "c", "d"], None);
        assert_eq!(playing_order(&after), "a [b] c d");
        assert_eq!(after.origins, [Some(0), None, Some(1), Some(2)]);
        // A reordered list is played in its new order from the current song on.
        let after = follow(lineup(&list, 1), false, &list, &["d", "b", "a", "c"], None);
        assert_eq!(playing_order(&after), "d [b] a c");
        // The page may show the list in another order (newest first, say): that's the order.
        // (The queue was started from that view, so its entries came from places 2, 1, 0.)
        let mut queue = lineup(&["c", "b", "a"], 0);
        queue.origins = vec![Some(2), Some(1), Some(0)];
        let after = follow(queue, false, &["a", "b", "c"], &["a", "b", "c", "x"], Some(&[3, 2, 1, 0]));
        assert_eq!(playing_order(&after), "x [c] b a");
    }

    #[test]
    fn songs_queued_by_hand_keep_their_place() {
        // "q" was added to play after "b"; it isn't part of the list.
        let mut queue = lineup(&["a", "b", "q", "c"], 1);
        queue.origins = vec![Some(0), Some(1), None, Some(2)];
        let after = follow(queue, false, &["a", "b", "c"], &["a", "b", "x", "c"], None);
        assert_eq!(playing_order(&after), "a [b] q x c");
        // A queue played from part of a list (a filtered view) doesn't gain the rest of it.
        let mut queue = lineup(&["b", "d"], 0);
        queue.origins = vec![Some(1), Some(3)];
        let after = follow(queue, false, &["a", "b", "c", "d"], &["a", "b", "c", "d", "e"], None);
        assert_eq!(playing_order(&after), "[b] d e");
    }

    #[test]
    fn a_shuffled_queue_takes_arrivals_among_what_is_to_come() {
        let list = ["a", "b", "c", "d"];
        let mut queue = lineup(&list, 1);
        queue.order = vec![2, 0, 3, 1];
        // Playing "a"; "c" has been played, "d" and "b" are to come. "b" is removed, "x" added.
        let after = follow(queue, true, &list, &["a", "x", "c", "d"], None);
        assert_eq!(playing_order(&after), "c [a] d x");
        // Underneath the shuffle the songs are in the list's order, for when shuffle is turned off.
        let titles: Vec<&str> = after.songs.iter().map(|song| song.id.as_str()).collect();
        assert_eq!(titles, ["a", "x", "c", "d"]);
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
