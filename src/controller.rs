//! The UI-thread hub: owns the window, the server connection, navigation and the page on
//! screen. Playback lives in `playback.rs` and menus in `actions.rs`, as further `impl`s.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::diagnostics::{self, FaultPlace, FaultShape};
use crate::audio::engine::Engine;
use crate::audio::dsp::{EQ_BANDS, EQ_RANGE};
use crate::config::{self, EQ_PRESETS, Paths, QUALITIES, Settings, VOLUME_LEVELS};
use crate::covers::Covers;
use crate::pages::{self, CardData, Kind, PageData};
use crate::playback::Playback;
use crate::pool::Pool;
use crate::subsonic::{ApiResult, Client, Playlist, Song};
use crate::ui;

thread_local! {
    static CONTROLLER: RefCell<Option<Rc<Controller>>> = const { RefCell::new(None) };
}

/// Runs `f` with the controller. Only meaningful on the UI thread.
pub fn with<R>(f: impl FnOnce(&Rc<Controller>) -> R) -> Option<R> {
    let ctl = CONTROLLER.with(|slot| slot.borrow().clone());
    ctl.map(|ctl| f(&ctl))
}

/// Queues `f` to run on the UI thread, from any thread.
/// Said when the sign-in's token couldn't be kept (written down) on this PC.
const SIGN_IN_NOT_SAVED: &str = "Couldn't save your sign-in; you'll need to sign in next time.";

pub fn post(f: impl FnOnce(&Rc<Controller>) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        with(f);
    });
}

/// A change to a playlist that can be taken back: the playlist, and its songs before.
pub enum Undo {
    /// Songs were taken out; `places` is where they were, lowest first.
    Removed { playlist: String, before: Vec<Song>, places: Vec<usize> },
    /// The song at `from` was moved to `to`.
    Moved { playlist: String, before: Vec<Song>, from: usize, to: usize },
}

impl Undo {
    /// The playlist the change was made to.
    pub fn playlist(&self) -> &str {
        match self {
            Undo::Removed { playlist, .. } | Undo::Moved { playlist, .. } => playlist,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    Home,
    Search(String),
    Album(String),
    Artist(String),
    Playlist(String),
    Liked,
    /// Every song in the library, newest first.
    Songs,
    /// Songs saved on this PC.
    Downloads,
    /// Internet radio stations set up on the server.
    Radio,
    /// Carries the index of the chosen sort order.
    Albums(usize),
    Artists,
    Genres,
    Genre(String),
    Settings,
    Lyrics,
    /// The screen about the song that's playing.
    NowPlaying,
    /// How the last import of playlists went.
    Import,
    /// One of the "Made for you" mixes, by its id.
    Mix(String),
    /// Albums that have been asked for and haven't arrived.
    Requests,
    /// What's been played most, over a stretch of time: `pages::stats` says which number
    /// stands for which.
    Stats(usize),
    /// An artist as the wider world knows them, by MusicBrainz's name for them: all their
    /// albums, in the library or not.
    OutsideArtist(String),
    /// An album that isn't in the library, by MusicBrainz's name for it.
    OutsideAlbum(String),
    /// The jam this PC has joined: someone else's queue.
    Jam,
}

impl Route {
    pub fn parse(text: &str) -> Option<Route> {
        let (head, rest) = text.split_once(':').unwrap_or((text, ""));
        Some(match head {
            "home" => Route::Home,
            "search" => Route::Search(rest.to_string()),
            "album" => Route::Album(rest.to_string()),
            "artist" => Route::Artist(rest.to_string()),
            "playlist" => Route::Playlist(rest.to_string()),
            "liked" => Route::Liked,
            "songs" => Route::Songs,
            "downloads" => Route::Downloads,
            "radio" => Route::Radio,
            "albums" => Route::Albums(rest.parse().unwrap_or(0)),
            "artists" => Route::Artists,
            "genres" => Route::Genres,
            "genre" => Route::Genre(rest.to_string()),
            "settings" => Route::Settings,
            "lyrics" => Route::Lyrics,
            "now-playing" => Route::NowPlaying,
            "import" => Route::Import,
            "mix" => Route::Mix(rest.to_string()),
            "requests" => Route::Requests,
            "stats" => Route::Stats(rest.parse().unwrap_or(0)),
            "outside-artist" => Route::OutsideArtist(rest.to_string()),
            "outside-album" => Route::OutsideAlbum(rest.to_string()),
            "jam" => Route::Jam,
            _ => return None,
        })
    }

    /// What the sidebar compares against to highlight the current section.
    fn key(&self) -> String {
        match self {
            Route::Home => "home".into(),
            Route::Search(_) => "search".into(),
            Route::Album(id) => format!("album:{id}"),
            Route::Artist(id) => format!("artist:{id}"),
            Route::Playlist(id) => format!("playlist:{id}"),
            Route::Liked => "liked".into(),
            Route::Songs => "songs".into(),
            Route::Downloads => "downloads".into(),
            Route::Radio => "radio".into(),
            Route::Albums(_) => "albums".into(),
            Route::Artists => "artists".into(),
            Route::Genres => "genres".into(),
            Route::Genre(name) => format!("genre:{name}"),
            Route::Settings => "settings".into(),
            Route::Lyrics => "lyrics".into(),
            Route::NowPlaying => "now-playing".into(),
            Route::Import => "import".into(),
            Route::Mix(id) => format!("mix:{id}"),
            Route::Requests => "requests".into(),
            Route::Stats(_) => "stats".into(),
            Route::OutsideArtist(id) => format!("outside-artist:{id}"),
            Route::OutsideAlbum(id) => format!("outside-album:{id}"),
            Route::Jam => "jam".into(),
        }
    }

    /// Identifies cached page data; distinct for anything that loads different content.
    pub fn cache_key(&self) -> String {
        match self {
            Route::Search(query) => format!("search:{query}"),
            Route::Albums(sort) => format!("albums:{sort}"),
            // (Not "stats:0": before 0.40 that was a different stretch of time.)
            Route::Stats(period) => format!("stats-{period}"),
            other => other.key(),
        }
    }

    fn page(&self) -> ui::Page {
        match self {
            Route::Home => ui::Page::Home,
            Route::Search(_) | Route::Import | Route::Requests | Route::Stats(_) => ui::Page::Search,
            Route::Album(_) | Route::Playlist(_) | Route::Liked | Route::Songs | Route::Downloads | Route::Radio | Route::Mix(_) => {
                ui::Page::Tracklist
            }
            Route::Artist(_) | Route::OutsideArtist(_) | Route::OutsideAlbum(_) => ui::Page::Artist,
            Route::Albums(_) | Route::Artists | Route::Genres | Route::Genre(_) => ui::Page::Grid,
            Route::Settings => ui::Page::Settings,
            Route::Lyrics => ui::Page::Lyrics,
            Route::NowPlaying => ui::Page::NowPlaying,
            Route::Jam => ui::Page::Jam,
        }
    }
}

/// Orders a song list can be shown in. The first is the list's own order.
pub const TRACK_SORTS: [&str; 6] = ["Custom order", "Title", "Artist", "Album", "Duration", "Date added"];
/// The position of "Date added" in `TRACK_SORTS`.
const SORT_ADDED: usize = 5;

/// How the song list on screen is narrowed and ordered.
#[derive(Default)]
pub struct TrackView {
    /// The page this applies to; moving to another page starts afresh.
    key: String,
    filter: String,
    sort: usize,
    descending: bool,
    /// For each row shown, the index of its song in the page's list.
    map: Vec<usize>,
}

impl TrackView {
    pub fn sort(&self) -> usize {
        self.sort
    }

    pub fn descending(&self) -> bool {
        self.descending
    }

    fn is_default(&self) -> bool {
        self.sort == 0 && self.filter.trim().is_empty()
    }

    /// Whether an order starts out reversed when first chosen: dates do, newest first.
    fn starts_descending(sort: usize) -> bool {
        sort == SORT_ADDED
    }

    /// Works out which songs to show, and in what order. `added` holds when each song was
    /// added to the list, where that's known.
    fn arrange(&mut self, songs: &[Song], added: &[i64]) {
        let needle = self.filter.trim().to_lowercase();
        let mut map: Vec<usize> = (0..songs.len())
            .filter(|&i| {
                let song = &songs[i];
                needle.is_empty()
                    || song.title.to_lowercase().contains(&needle)
                    || song.artist_label().to_lowercase().contains(&needle)
                    || song.album.to_lowercase().contains(&needle)
            })
            .collect();
        if self.sort != 0 {
            let key = |i: &usize| -> (String, i64) {
                let song = &songs[*i];
                match self.sort {
                    1 => (song.title.to_lowercase(), 0),
                    2 => (song.artist_label().to_lowercase(), 0),
                    3 => (song.album.to_lowercase(), (song.disc_number * 1000 + song.track) as i64),
                    4 => (String::new(), song.duration as i64),
                    _ => (String::new(), added.get(*i).copied().unwrap_or(0)),
                }
            };
            // A stable sort, so songs that tie keep the list's own order.
            map.sort_by_cached_key(key);
            if self.descending {
                map.reverse();
            }
        }
        self.map = map;
    }
}

/// Something offered under the search box.
#[derive(Clone)]
enum Suggestion {
    /// An artist or an album.
    Card(CardData),
    Song(Song),
}

struct HistoryEntry {
    route: Route,
    scroll: f32,
}

#[derive(Default)]
struct Nav {
    history: Vec<HistoryEntry>,
    index: usize,
    /// Bumped on every page change so late responses for an old page are dropped.
    generation: u64,
}

/// A cached page younger than this is shown as is. Older copies are still shown at once, and
/// checked against the server in the background.
const FRESH_FOR: Duration = Duration::from_secs(30);
/// Pages kept in memory before the oldest are dropped.
const MEMORY_PAGES: usize = 300;
/// Cached pages bigger than this aren't written to disk.
const DISK_PAGE_LIMIT: usize = 3 * 1024 * 1024;

pub struct Controller {
    pub ui: ui::MainWindow,
    pub paths: Paths,
    pub api_pool: Pool,
    /// For asking Lidarr to fetch things. One of those can take minutes, and mustn't hold
    /// up loading pages.
    requests_pool: Pool,
    /// For changes to playlists, one at a time and in the order they were made, so a later
    /// change is always applied to what an earlier one left.
    playlist_pool: Pool,
    /// The last change to a playlist that can still be taken back, and when it was made.
    pub undo: RefCell<Option<(Instant, Undo)>>,
    /// For looking things up in the copy of the library kept on this PC, which needs no
    /// network and so shouldn't wait behind anything that does.
    quick_pool: Pool,
    pub art_pool: Pool,
    pub download_pool: Pool,
    /// For talking to a jam's relay, which can be slow and mustn't hold up anything else.
    pub jam_pool: std::sync::Arc<Pool>,
    /// The jam being hosted or joined, if any (see `jam`).
    pub jam: RefCell<crate::jam::Jam>,
    /// The order of choices on this PC: what a command from another of the person's
    /// devices is judged against (see `link`).
    pub link_rule: RefCell<nexus_core::link::rule::Rule>,
    /// The choices under way in the player: who chose what is being done to it now, from
    /// which both that order and whose the queue is are worked out (see `ways`).
    pub ways: crate::ways::Choices,
    /// The new linking of a person's devices, where it is switched on (see `link`).
    pub link: RefCell<crate::link::Link>,
    /// Waits for typing in a jam's search box to pause.
    pub jam_timer: slint::Timer,
    pub downloads: RefCell<crate::downloads::Downloads>,
    pub covers: Covers,
    pub engine: Engine,
    pub settings: RefCell<Settings>,
    pub playback: RefCell<Playback>,
    pub playlists: RefCell<Vec<Playlist>>,
    /// What the page on screen was built from; the source for play and menu actions.
    pub page: RefCell<PageData>,
    pub tracks_model: Rc<VecModel<ui::Track>>,
    pub track_view: RefCell<TrackView>,
    /// When each of the page's songs was added to its list (0 where unknown); empty on
    /// pages that have no such date.
    page_added: RefCell<Vec<i64>>,
    /// The app's own note of when songs were added to playlists.
    pub added: RefCell<crate::added::Added>,
    /// Set just before `play_songs` by callers that know where the songs came from: the
    /// page's route key, and each song's position in that page's list.
    pub play_from: RefCell<Option<crate::playback::PlayFrom>>,
    /// Rows of the song list that are selected, and the row a Shift-click extends from.
    pub selection: RefCell<std::collections::BTreeSet<usize>>,
    selection_anchor: Cell<Option<usize>>,
    /// The row the keyboard is on.
    selection_cursor: Cell<Option<usize>>,
    pub queue_model: Rc<VecModel<ui::Track>>,
    /// The first few rows of the queue, for Now Playing's glance at what's next.
    pub up_next_model: Rc<VecModel<ui::Track>>,
    /// Which rows of the song list on screen show each song, so one song's rows can be
    /// brought up to date without going through the whole list. Emptied when the list is
    /// rebuilt and filled in again when next needed.
    pub track_places: RefCell<HashMap<String, Vec<usize>>>,
    /// The downloads are on their way to another folder; nothing new is started meanwhile.
    pub moving_downloads: Cell<bool>,
    /// The mix made to rules whose menu is open.
    pub menu_rule: Cell<Option<u64>>,
    /// Legacy device-sync notices. Diagnostics drains these as typed counts on the window.
    pub said: RefCell<std::collections::VecDeque<(i64, String)>>,
    /// Something to say once the window is up (the settings file was damaged, say).
    startup_note: RefCell<Option<String>>,
    /// What another device was playing more recently than anything was done here.
    /// What another device has been playing, on offer as "Continue here", with the server's
    /// clock beside this PC's as the read that found it measured it.
    pub elsewhere: RefCell<Option<(crate::subsonic::PlayQueue, nexus_core::time::Clock)>>,
    /// Songs that have been played, newest first, including the one playing now.
    pub history: RefCell<Vec<config::Played>>,
    /// Every play and when, for "Your stats".
    pub play_log: RefCell<crate::plays::PlayLog>,
    pub history_model: Rc<VecModel<ui::Track>>,
    pub grid_model: Rc<VecModel<ui::CardRow>>,
    pub grid_columns: Cell<usize>,
    pub grid_loading: Cell<bool>,
    /// Likes changed in this session, ahead of whatever cached responses still say.
    pub stars: RefCell<HashMap<String, bool>>,
    /// Star ratings changed in this session, likewise.
    pub ratings: RefCell<HashMap<String, u8>>,
    /// When the sleep timer will pause playback.
    pub sleep_at: Cell<Option<Instant>>,
    /// The sleep timer is set to pause when the current song ends.
    pub sleep_after_song: Cell<bool>,
    pub menu: RefCell<crate::actions::MenuState>,
    pub prompt: RefCell<Option<crate::actions::PromptAction>>,
    pub lyrics: RefCell<crate::playback::LyricsState>,
    /// Songs whose lyrics have been fetched ahead in this session.
    pub lyrics_asked: RefCell<std::collections::HashSet<String>>,
    #[cfg(windows)]
    pub media: RefCell<Option<crate::media::MediaKeys>>,
    client: RefCell<Option<Client>>,
    nav: RefCell<Nav>,
    cache: RefCell<HashMap<String, (Instant, PageData)>>,
    /// Pages being fetched ahead of a click, so a hover doesn't fetch the same thing twice.
    prefetching: RefCell<HashSet<String>>,
    toast_timer: slint::Timer,
    pub tick_timer: slint::Timer,
    update_timer: slint::Timer,
    /// Changes waiting to be sent to the server.
    pub pending: RefCell<Vec<crate::offline::Pending>>,
    /// The held changes being sent to the server right now.
    pub sending: RefCell<Vec<crate::offline::Pending>>,
    /// The server isn't answering.
    pub unreachable: Cell<bool>,
    pub reconnect_timer: slint::Timer,
    /// Waits before asking again for a song whose stream stopped arriving.
    pub retry_timer: slint::Timer,
    /// Sharing with other devices (see `sync`): the timers that pass changes on and look
    /// for others', whether an exchange is under way (and another is wanted after it), and
    /// the playlist that holds the shared data.
    /// Search: what's on offer under the search box, a count that tells a late answer from
    /// the current one, and the timer and flag for keeping the library copy up to date.
    /// How the last import of playlists went, as a page to show, and the songs it couldn't find.
    import_report: RefCell<Option<PageData>>,
    import_missing: RefCell<Vec<crate::import::Wanted>>,
    /// The same songs with the playlist each belongs in: (album, the song for that playlist).
    import_fills: RefCell<Vec<(String, crate::asked::Fill)>>,
    /// What's been asked for and is being watched for, and a count that paces the watching.
    pub asked: RefCell<crate::asked::AskedList>,
    asked_ticks: Cell<u32>,
    /// A note of the batch being asked for couldn't be written down; and whether that has
    /// been said (once per batch: see `bulk_event`).
    bulk_unnoted: Cell<(bool, bool)>,
    /// Counts ticks, to refresh the Requests page now and then while it's open.
    requests_ticks: Cell<u32>,
    /// The search on screen: everything it found, which kind of result it's narrowed to
    /// (see `pages::SEARCH_FILTERS`), and the words that choice belongs to.
    search_whole: RefCell<Option<PageData>>,
    search_filter: Cell<usize>,
    search_filter_query: RefCell<String>,
    /// Songs kept on this PC as they're played, what fetches the next few ahead of time,
    /// and counters that pace the two.
    pub song_cache: crate::songcache::SongCache,
    pub fetch_ahead: crate::songcache::Ahead,
    pub keep_ticks: Cell<u32>,
    pub keep_rounds: Cell<u32>,
    /// Counts checks of the connection to Lidarr, to tell a late answer from the current one.
    requests_check: Cell<u64>,
    /// The sign-in in front of Lidarr, once signed in to. The one copy is kept for as long
    /// as it's good, so its token is only ever renewed in one place.
    gate: RefCell<Option<std::sync::Arc<crate::gate::Session>>>,
    /// How many sign-in sessions have been begun: part of what a Lidarr connection is made of.
    sign_ins: Cell<u64>,
    /// Whether the key kept by 0.40.10 has been looked at, at this start; and the address it
    /// belongs to if it couldn't be bound then (see `keep::bind_old_key`).
    keys_looked_at: Cell<bool>,
    key_old_for: RefCell<Option<String>>,
    /// The same for the sign-in's token, and the service it belongs to.
    tokens_looked_at: Cell<bool>,
    token_old_for: RefCell<Option<String>>,
    suggestions: RefCell<Vec<Suggestion>>,
    preview_count: Cell<u64>,
    /// The same count, where the search thread can see it: a search that is no longer the
    /// newest when its turn comes isn't run.
    preview_latest: std::sync::Arc<std::sync::atomic::AtomicU64>,
    library_timer: slint::Timer,
    library_busy: Cell<bool>,
    pub sync_timer: slint::Timer,
    pub sync_look_timer: slint::Timer,
    pub queue_timer: slint::Timer,
    /// The exchange with the server, and the save of the queue: one of each at a time.
    pub exchange: crate::sync::Turn,
    pub queue_save: crate::sync::Turn,
    /// The server's clock minus this PC's, as the last exchange measured it; and what is
    /// said under the setting when this PC isn't sharing as it should (see `sync`).
    pub clock: Cell<nexus_core::time::Clock>,
    pub sync_note: RefCell<String>,
    /// Which sign-in, with sharing on, is current: a number that moves on at every sign-in
    /// and sign-out and whenever sharing is switched off. Whatever was asked of the server
    /// under an earlier number is dropped when it arrives, and a worker about to send
    /// looks at it once more.
    pub session: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Songs in other devices' Recently played that the server said it doesn't have, so
    /// they aren't asked for again this run.
    pub history_missing: RefCell<std::collections::HashSet<String>>,
    /// Whose the queue in the player is (another device's, taken up and not touched here
    /// since: kept with the queue's place between runs), and which queue a save was
    /// prepared for.
    pub queue_owner: crate::sync::QueueOwner,
    /// When the queue last changed here (or, for one taken from another device, there), and
    /// a fingerprint of it as last saved, to tell a real change from a save of the same thing.
    pub queue_stamp: Cell<i64>,
    /// The number of the queue that is in the player now, for a move of music that waits
    /// for the sound of the queue it put in and of no other (`link`). **It moves at one
    /// point, `Controller::whole_queue`, each time a whole queue is in the player in
    /// place of what was there**: something chosen to play, a queue from another device
    /// or the server, the queue brought back at a start, the player emptied. Changing a
    /// queue in place (adding, removing, moving, shuffling, following its list) keeps it.
    pub queue_number: Cell<u64>,
    pub queue_sig: Cell<u64>,
    /// The shape (see `config::queue_shape`) of the queue as last written to disk.
    pub queue_saved: Cell<u64>,
    /// While set, emptying the queue leaves the saved copy alone.
    pub keep_saved_queue: Cell<bool>,
    /// Writes files in the background.
    pub saver: crate::saver::Saver,
    /// Set while the queue is being put in place from a saved copy, which isn't a change.
    pub queue_quiet: Cell<bool>,
    /// Whether the sound device is currently held in exclusive mode.
    pub output_exclusive: Cell<bool>,
    /// The output devices last listed in the device menu: (id, name).
    pub devices: RefCell<Vec<(String, String)>>,
    /// The newer release found by the last check, if any.
    update: RefCell<Option<crate::update::Release>>,
    update_busy: Cell<bool>,
    #[cfg(windows)]
    tray: RefCell<Option<ui::Tray>>,
    /// The small player's window, once it's been opened, and whether it's showing.
    mini: RefCell<Option<ui::MiniPlayer>>,
    pub mini_open: Cell<bool>,
}

fn note_search_results(ticket: Option<crate::journal::SearchTicket>, page: &PageData) {
    if ticket.is_some() {
        let cards = page.lead.iter().chain(&page.shelves).map(|shelf| shelf.cards.len()).sum::<usize>();
        let library = page.songs.len() + cards;
        let outside = page.offers.len() + page.tail.iter().map(|shelf| shelf.cards.len()).sum::<usize>();
        crate::journal::search_results(ticket, library, Some(outside));
    }
}

impl Controller {
    pub fn install(ui: ui::MainWindow) -> Rc<Controller> {
        // Files flushed on this thread are counted and, with NEXUS_TRACE, printed.
        config::mark_window_thread();
        let paths = Paths::new();
        let (settings, settings_note) = Settings::load_checked(&paths.settings);
        // What writes killed part-way left beside the app's files, for the account signed
        // in: cleared now, once, before anything is written (`config::clear_leftovers`).
        {
            let stem = if settings.server.is_empty() { String::new() } else { account_stem(&settings.server, &settings.username) };
            let downloads = if settings.downloads_dir.is_empty() { paths.downloads.clone() } else { PathBuf::from(&settings.downloads_dir) };
            if let Some(dir) = paths.settings.parent() {
                config::clear_settings_leftovers(dir, &settings.account_file(), &stem);
            }
            if let Some(dir) = paths.crash_log.parent() {
                config::clear_data_leftovers(dir);
            }
            config::clear_downloads_leftovers(&downloads, &stem);
        }
        let play_log = {
            let recent: Vec<(String, i64)> = config::load_history(&paths.history).into_iter().map(|played| (played.song.id, played.at)).collect();
            crate::plays::PlayLog::load(&paths.plays, &recent)
        };
        // (Both, if both: each is said once, when the app has started.)
        let startup_note = match (settings_note, play_log.note()) {
            (Some(settings), Some(plays)) => Some(format!("{settings} {plays}")),
            (settings, plays) => settings.or(plays.map(str::to_string)),
        };
        let settings_songs_dir = settings.songs_dir.clone();
        crate::mixes::set_rules(settings.rule_mixes.clone());
        crate::audio::source::clean_stale(&paths.buffers);
        let engine = Engine::new(paths.buffers.clone(), |event| {
            post(move |ctl| ctl.on_engine_event(event));
        });
        let ctl = Rc::new(Controller {
            covers: Covers::new(paths.covers.clone()),
            api_pool: Pool::new("api", 4, false),
            requests_pool: Pool::new("requests", 2, false),
            playlist_pool: Pool::new("playlists", 1, false),
            undo: RefCell::default(),
            quick_pool: Pool::new("quick", 1, true),
            jam_pool: std::sync::Arc::new(Pool::new("jam", 4, false)),
            jam: RefCell::default(),
            link_rule: RefCell::default(),
            ways: Default::default(),
            link: RefCell::default(),
            jam_timer: slint::Timer::default(),
            download_pool: Pool::new("download", 2, false),
            // Nobody is signed in yet: whose downloads to show is known in `enter`.
            downloads: RefCell::new(crate::downloads::Downloads::load(
                if settings.downloads_dir.is_empty() { paths.downloads.clone() } else { PathBuf::from(&settings.downloads_dir) },
                "",
            )),
            art_pool: Pool::new("art", 6, true),
            engine,
            playback: RefCell::new(Playback::new(&settings)),
            settings: RefCell::new(settings),
            playlists: RefCell::default(),
            page: RefCell::default(),
            tracks_model: Rc::new(VecModel::default()),
            track_view: RefCell::default(),
            page_added: RefCell::default(),
            added: RefCell::new(crate::added::Added::load(paths.added.clone())),
            play_from: RefCell::new(None),
            selection: RefCell::default(),
            selection_anchor: Cell::new(None),
            selection_cursor: Cell::new(None),
            queue_model: Rc::new(VecModel::default()),
            up_next_model: Rc::new(VecModel::default()),
            track_places: RefCell::default(),
            moving_downloads: Cell::new(false),
            menu_rule: Cell::new(None),
            said: RefCell::default(),
            startup_note: RefCell::new(startup_note),
            elsewhere: RefCell::default(),
            play_log: RefCell::new(play_log),
            history: RefCell::new(config::load_history(&paths.history)),
            history_model: Rc::new(VecModel::default()),
            grid_model: Rc::new(VecModel::default()),
            grid_columns: Cell::new(5),
            grid_loading: Cell::new(false),
            stars: RefCell::default(),
            ratings: RefCell::default(),
            sleep_at: Cell::new(None),
            sleep_after_song: Cell::new(false),
            menu: RefCell::default(),
            prompt: RefCell::new(None),
            lyrics: RefCell::default(),
            lyrics_asked: RefCell::default(),
            #[cfg(windows)]
            media: RefCell::new(None),
            client: RefCell::new(None),
            nav: RefCell::default(),
            cache: RefCell::default(),
            prefetching: RefCell::default(),
            toast_timer: slint::Timer::default(),
            tick_timer: slint::Timer::default(),
            update_timer: slint::Timer::default(),
            pending: RefCell::default(),
            sending: RefCell::default(),
            unreachable: Cell::new(false),
            reconnect_timer: slint::Timer::default(),
            retry_timer: slint::Timer::default(),
            import_report: RefCell::new(None),
            import_missing: RefCell::default(),
            import_fills: RefCell::default(),
            asked: RefCell::new({
                crate::discover::set_place(paths.discover.clone());
                // Lyrics the app once fetched for itself; the server provides them now.
                let _ = std::fs::remove_dir_all(&paths.old_lyrics);
                crate::bulk::load(&paths.asking);
                crate::asked::AskedList::load(&paths.asked)
            }),
            asked_ticks: Cell::new(0),
            bulk_unnoted: Cell::new((false, false)),
            requests_ticks: Cell::new(0),
            search_whole: RefCell::new(None),
            search_filter: Cell::new(0),
            search_filter_query: RefCell::default(),
            song_cache: crate::songcache::SongCache::new({
                // Somewhere of your choosing, if you've chosen. Then what was kept in the
                // app's own folder is of no more use, and is cleared.
                if settings_songs_dir.is_empty() {
                    paths.songs.clone()
                } else {
                    for stale in std::fs::read_dir(&paths.songs).into_iter().flatten().flatten() {
                        let _ = std::fs::remove_file(stale.path());
                    }
                    let _ = std::fs::create_dir_all(&settings_songs_dir);
                    PathBuf::from(&settings_songs_dir)
                }
            }),
            fetch_ahead: crate::songcache::Ahead::new(),
            keep_ticks: Cell::new(0),
            keep_rounds: Cell::new(0),
            requests_check: Cell::new(0),
            gate: RefCell::new(None),
            sign_ins: Cell::new(0),
            keys_looked_at: Cell::new(false),
            key_old_for: RefCell::new(None),
            tokens_looked_at: Cell::new(false),
            token_old_for: RefCell::new(None),
            suggestions: RefCell::default(),
            preview_count: Cell::new(0),
            preview_latest: Default::default(),
            library_timer: slint::Timer::default(),
            library_busy: Cell::new(false),
            sync_timer: slint::Timer::default(),
            sync_look_timer: slint::Timer::default(),
            queue_timer: slint::Timer::default(),
            exchange: Default::default(),
            queue_save: Default::default(),
            clock: Cell::new(nexus_core::time::Clock::Unmeasured),
            sync_note: RefCell::default(),
            session: Default::default(),
            history_missing: RefCell::default(),
            queue_owner: Default::default(),
            queue_stamp: Cell::new(0),
            queue_number: Cell::new(0),
            queue_sig: Cell::new(crate::playback::queue_fingerprint(&Playback::new(&Settings::default()))),
            queue_quiet: Cell::new(false),
            queue_saved: Cell::new(0),
            keep_saved_queue: Cell::new(false),
            saver: crate::saver::Saver::new(),
            output_exclusive: Cell::new(false),
            devices: RefCell::default(),
            update: RefCell::new(None),
            update_busy: Cell::new(false),
            #[cfg(windows)]
            tray: RefCell::new(None),
            mini: RefCell::new(None),
            mini_open: Cell::new(false),
            paths,
            ui,
        });
        CONTROLLER.with(|slot| *slot.borrow_mut() = Some(ctl.clone()));
        ctl.wire();
        ctl.start();
        ctl.say_startup_notes();
        ctl
    }

    /// Says, once the window has had a moment to appear, anything that needs saying about
    /// how the app started: the settings file was damaged, or the app stopped by itself
    /// last time.
    fn say_startup_notes(self: &Rc<Self>) {
        // How much of the crash log has been mentioned already is kept beside it.
        let seen_file = config::crash_seen(&self.paths);
        let size = std::fs::metadata(&self.paths.crash_log).map(|meta| meta.len()).unwrap_or(0);
        let seen: u64 = std::fs::read_to_string(&seen_file).ok().and_then(|text| text.trim().parse().ok()).unwrap_or(0);
        let crashed = size > seen;
        if crashed {
            diagnostics::prior_crash(&self.paths.crash_log);
            // Whole or not at all: a number cut short would be taken for one that was seen.
            let _ = config::write_in_passing(&seen_file, size.to_string().as_bytes());
        }
        let note = self.startup_note.borrow_mut().take().or_else(|| {
            crashed.then(|| "The app stopped unexpectedly last time. “Save diagnostics” under Settings, About has what was noted.".to_string())
        });
        // (Said once ever, with whatever else is said now.)
        let links = config::no_links_note(&mut self.settings.borrow_mut().said_no_links);
        if links.is_some() {
            self.save_settings_in_passing();
        }
        let note = match (note, links) {
            (Some(note), Some(links)) => Some(format!("{note} {links}")),
            (note, links) => note.or(links.map(str::to_string)),
        };
        if let Some(note) = note {
            slint::Timer::single_shot(Duration::from_millis(1500), move || {
                with(|ctl| ctl.show_toast(note.into(), "", Duration::from_secs(10)));
            });
        }
    }

    /// Tells the person, once ever, that the settings folder's drive can't make hard links,
    /// the first time that is found out after the app has started (`config::no_links_note`).
    pub fn say_no_links_once(&self) {
        let note = config::no_links_note(&mut self.settings.borrow_mut().said_no_links);
        if let Some(note) = note {
            self.save_settings_in_passing();
            self.show_toast(note.into(), "", Duration::from_secs(10));
        }
    }

    /// Takes a copy of this session for export. The diagnostics worker builds the clean
    /// report, writes it and opens the file.
    pub fn save_diagnostics(&self) {
        crate::diagnostics::save(self);
    }

    pub fn client(&self) -> Option<Client> {
        self.client.borrow().clone()
    }

    /// Runs a server call on the worker pool and hands the result back on the UI thread.
    pub fn request<T: Send + 'static>(
        &self,
        job: impl FnOnce(&Client) -> ApiResult<T> + Send + 'static,
        done: impl FnOnce(&Rc<Controller>, ApiResult<T>) + Send + 'static,
    ) {
        let Some(client) = self.client() else { return };
        self.api_pool.spawn(move || {
            let result = job(&client);
            post(move |ctl| {
                // A reply for a session that has since signed out is of no use.
                if ctl.client().is_some() {
                    done(ctl, result);
                }
            });
        });
    }

    pub fn toast(&self, text: impl Into<SharedString>) {
        self.show_toast(text.into(), "", Duration::from_millis(3200));
    }

    /// A toast with "Undo" on it, shown for longer so there's time to press it.
    pub fn toast_with_undo(&self, text: impl Into<SharedString>) {
        self.show_toast(text.into(), "Undo", Duration::from_secs(8));
    }

    fn show_toast(&self, text: SharedString, action: &str, shown_for: Duration) {
        diagnostics::observe(diagnostics::Line::Note(diagnostics::Note::AppMessage));
        let toast = self.ui.global::<ui::Toast>();
        toast.set_text(text);
        toast.set_action(action.into());
        toast.set_visible(true);
        let weak = self.ui.as_weak();
        self.toast_timer.start(slint::TimerMode::SingleShot, shown_for, move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<ui::Toast>().set_visible(false);
            }
        });
    }

    /// Carries out a change to a playlist on the server. The playlist is looked at as it is
    /// there now, and the change applied to that, so songs added or removed somewhere else
    /// since the page loaded are neither lost nor mistaken for the ones meant. `expected` is
    /// what the page took the playlist to hold before the change.
    pub fn edit_playlist(self: &Rc<Self>, id: String, expected: Vec<String>, edit: crate::playlists::Edit) {
        use crate::playlists::{Outcome, carry_out};
        let Some(client) = self.client() else { return };
        self.playlist_pool.spawn(move || {
            let result = carry_out(&client, &id, &expected, &edit);
            post(move |ctl| {
                let key = format!("playlist:{id}");
                let showing = ctl.current_route() == Some(Route::Playlist(id.clone()));
                // The person's own change, taken by the server: what it added is dated, and
                // what it took out is told to the other devices. (Only what the change itself
                // did: whatever else the playlist gained meanwhile is not this PC's to date.)
                if let Ok(Outcome::Done { before, .. }) = &result {
                    if ctl.added.borrow_mut().edited(&id, before, &edit, chrono::Utc::now().timestamp()) {
                        ctl.share_soon();
                    }
                }
                match result {
                    // It had changed elsewhere: show how it stands now. Either way its length
                    // and running time on the page want bringing up to date.
                    Ok(Outcome::Done { .. }) if showing => ctl.refresh(),
                    Ok(Outcome::Done { .. }) => ctl.invalidate(&key),
                    // Nothing was sent. The page already shows the change, so it goes back to
                    // what the server has, and there's nothing to undo.
                    Ok(Outcome::HidesSongs) => {
                        let mut undo = ctl.undo.borrow_mut();
                        if undo.as_ref().is_some_and(|(_, undo)| undo.playlist() == id) {
                            *undo = None;
                        }
                        drop(undo);
                        ctl.invalidate(&key);
                        ctl.toast(crate::playlists::HIDES_SONGS);
                        if showing {
                            ctl.refresh();
                        }
                    }
                    Err(e) => {
                        ctl.invalidate(&key);
                        ctl.toast(format!("That didn't save. {e}"));
                        if showing {
                            ctl.refresh();
                        }
                    }
                }
            });
        });
    }

    /// Takes back the last change to a playlist, if it was made in the last minute.
    pub fn undo_last(self: &Rc<Self>) {
        let Some((at, undo)) = self.undo.borrow_mut().take() else { return };
        if at.elapsed() > Duration::from_secs(60) {
            return;
        }
        self.ui.global::<ui::Toast>().set_visible(false);
        let (playlist, restored, edit) = match undo {
            Undo::Removed { playlist, before, places } => {
                let ids: Vec<String> = before.iter().map(|song| song.id.clone()).collect();
                let edit = crate::playlists::restoring(&ids, &places);
                (playlist, before, edit)
            }
            Undo::Moved { playlist, before, from, to } => {
                // The list as it was after the move, and the move back.
                let mut ids: Vec<String> = before.iter().map(|song| song.id.clone()).collect();
                let moved = ids.remove(from);
                ids.insert(to, moved);
                let edit = crate::playlists::moving(&ids, to, from);
                (playlist, before, edit)
            }
        };
        // What the page (if it's still on this playlist) holds now is what the server is
        // expected to hold; then the page goes back to how it was.
        let showing = self.page.borrow().playlist.as_ref().is_some_and(|p| p.id == playlist);
        let expected: Vec<String> = if showing { self.page.borrow().songs.iter().map(|s| s.id.clone()).collect() } else { Vec::new() };
        if showing {
            {
                let mut page = self.page.borrow_mut();
                page.songs = restored;
                page.can_play = !page.songs.is_empty();
            }
            let songs = self.page.borrow().songs.clone();
            self.source_list_now(&format!("playlist:{playlist}"), &songs);
            self.rebuild_tracks();
            if let Some(route) = self.current_route() {
                let data = self.page.borrow().clone();
                self.remember(&route.cache_key(), Instant::now(), data);
            }
        }
        self.toast("Put back");
        self.edit_playlist(playlist, expected, edit);
    }

    /// Saves the settings after a change the person made: flushed to the disk.
    pub fn save_settings(&self) {
        self.settings.borrow().save(&self.paths.settings);
    }

    /// Saves the settings in passing, as an exchange or a new queue starting does.
    pub fn save_settings_in_passing(&self) {
        self.settings.borrow().save_in_passing(&self.paths.settings);
    }

    // ---- start-up and sign-in ----------------------------------------------------------------

    fn start(self: &Rc<Self>) {
        let app = self.ui.global::<ui::App>();
        app.set_app_version(env!("CARGO_PKG_VERSION").into());
        let settings = self.settings.borrow().clone();
        app.set_server(settings.server.as_str().into());
        app.set_username(settings.username.as_str().into());
        app.set_queue_open(settings.queue_panel);
        self.push_settings();
        self.apply_audio_settings();
        self.ui.global::<ui::View>().set_tracks(ModelRc::from(self.tracks_model.clone()));
        self.ui.global::<ui::View>().set_grid(ModelRc::from(self.grid_model.clone()));
        self.ui.global::<ui::Player>().set_queue(ModelRc::from(self.queue_model.clone()));
        self.ui.global::<ui::Player>().set_up_next(ModelRc::from(self.up_next_model.clone()));
        self.ui.global::<ui::Player>().set_history(ModelRc::from(self.history_model.clone()));
        self.sync_history();
        self.sync_transport();
        self.sync_offline();

        let (covers_dir, pages_dir) = (self.paths.covers.clone(), self.paths.pages.clone());
        self.api_pool.spawn(move || {
            crate::covers::trim_disk(&covers_dir, 400 * 1024 * 1024);
            for server in std::fs::read_dir(&pages_dir).into_iter().flatten().flatten() {
                crate::covers::trim_disk(&server.path(), 48 * 1024 * 1024);
            }
        });

        // Come back signed in when a password is on file.
        if !settings.server.is_empty() && !settings.username.is_empty() {
            if let Some(password) =
                config::load_password(&self.paths, &settings.server, &settings.username)
            {
                let client = Client::new(&settings.server, &settings.username, &password);
                self.enter(client, None);
                // Confirm the saved login still works (if not, go back to the sign-in
                // screen), and find out whether the server can be reached at all.
                self.check_connection();
            }
        }

        let weak = Rc::downgrade(self);
        self.tick_timer.start(slint::TimerMode::Repeated, Duration::from_millis(250), move || {
            if let Some(ctl) = weak.upgrade() {
                ctl.tick();
            }
        });

        self.sync_downloads();
        self.connect_requests();
        // Albums that were being asked for when the app last closed.
        self.run_bulk();
        self.apply_theme();
        // For looking at the window buttons on a development build, where there's no frame
        // to replace.
        #[cfg(not(windows))]
        app.set_custom_frame(std::env::var_os("NEXUS_SHOW_WINDOW_BUTTONS").is_some());
        // For looking at the layout in a window of a given size, e.g. `1000x650`.
        if let Some((width, height)) = std::env::var("NEXUS_WINDOW_SIZE").ok().and_then(|size| {
            let (width, height) = size.split_once('x')?;
            Some((width.parse::<f32>().ok()?, height.parse::<f32>().ok()?))
        }) {
            self.ui.window().set_size(slint::LogicalSize::new(width, height));
        }
        // The title bar can only be coloured once the window exists.
        #[cfg(windows)]
        slint::Timer::single_shot(Duration::from_millis(50), || {
            with(|ctl| ctl.apply_theme());
        });
        // Take over the title bar, once there is a window to take it over from.
        #[cfg(windows)]
        slint::Timer::single_shot(Duration::from_millis(60), || {
            with(|ctl| {
                let own = !ctl.settings.borrow().standard_title_bar;
                let installed = crate::frame::install(&ctl.ui, own);
                crate::frame::set_scale(ctl.ui.window().scale_factor());
                ctl.ui.global::<ui::App>().set_custom_frame(installed && own);
            });
        });
        // Likewise the buttons on the taskbar thumbnail.
        #[cfg(windows)]
        slint::Timer::single_shot(Duration::from_millis(400), || {
            with(|ctl| {
                crate::taskbar::install(&ctl.ui);
                ctl.sync_tray();
            });
        });
        self.start_shell();
        // Look for a new version shortly after opening, then a few times a day.
        self.ui.global::<ui::Settings>().set_can_self_update(crate::update::installed_copy());
        slint::Timer::single_shot(Duration::from_secs(5), || {
            with(|ctl| ctl.check_for_updates(false));
        });
        self.update_timer.start(slint::TimerMode::Repeated, Duration::from_secs(6 * 3600), || {
            with(|ctl| ctl.check_for_updates(false));
        });
    }

    // ---- window, tray and quitting ------------------------------------------------------------

    /// Sets up what happens when the window is closed, and the tray icon.
    fn start_shell(self: &Rc<Self>) {
        self.ui.window().on_close_requested(|| {
            with(|ctl| {
                if !ctl.stays_in_tray() {
                    ctl.quit();
                }
            });
            slint::CloseRequestResponse::HideWindow
        });
        #[cfg(windows)]
        if let Ok(tray) = ui::Tray::new() {
            tray.on_open(|| {
                with(|ctl| ctl.show_window());
            });
            tray.on_toggle_play(|| {
                with(|ctl| ctl.toggle_play());
            });
            tray.on_next(|| {
                with(|ctl| ctl.next_track());
            });
            tray.on_previous(|| {
                with(|ctl| ctl.previous_track());
            });
            tray.on_quit(|| {
                with(|ctl| ctl.quit());
            });
            *self.tray.borrow_mut() = Some(tray);
            self.sync_tray();
        }
    }

    /// Paints the interface, and the window's title bar, in the chosen theme's colours.
    pub fn apply_theme(&self) {
        let colors = self.settings.borrow().theme();
        let color = |rgb: u32| slint::Color::from_argb_encoded(0xff00_0000 | rgb);
        let paint = |theme: ui::Theme| {
            theme.set_base(color(colors.base));
            theme.set_panel(color(colors.panel));
            theme.set_raised(color(colors.raised));
            theme.set_overlay(color(colors.overlay));
            theme.set_text(color(colors.text));
            theme.set_muted(color(colors.muted));
            theme.set_faint(color(colors.faint));
            theme.set_accent(color(colors.accent));
            theme.set_accent_bright(color(colors.accent_bright));
            theme.set_on_accent(color(colors.on_accent));
            theme.set_warm(color(colors.warm));
        };
        paint(self.ui.global::<ui::Theme>());
        #[cfg(windows)]
        crate::instance::tint_title_bar(self.ui.window(), colors.base, colors.muted);
        // The small player is a window of its own, with its own copy of the colours.
        if let Some(mini) = self.mini.borrow().as_ref() {
            paint(mini.global::<ui::Theme>());
            #[cfg(windows)]
            crate::instance::tint_title_bar(mini.window(), colors.base, colors.muted);
        }
    }

    // ---- the small player ----------------------------------------------------------------------

    /// Swaps the window for the small player, which stays on top of other apps.
    pub fn open_mini(self: &Rc<Self>) {
        if self.mini.borrow().is_none() {
            let Ok(mini) = ui::MiniPlayer::new() else { return self.toast("The mini player couldn't be opened.") };
            mini.on_toggle(|| {
                with(|ctl| ctl.toggle_play());
            });
            mini.on_next(|| {
                with(|ctl| ctl.next_track());
            });
            mini.on_previous(|| {
                with(|ctl| ctl.previous_track());
            });
            mini.on_toggle_star(|| {
                with(|ctl| ctl.toggle_current_star());
            });
            mini.on_expand(|| {
                with(|ctl| ctl.close_mini());
            });
            // Closing it brings the full window back: the app isn't being quit.
            mini.window().on_close_requested(|| {
                with(|ctl| ctl.close_mini());
                slint::CloseRequestResponse::HideWindow
            });
            *self.mini.borrow_mut() = Some(mini);
            self.apply_theme();
        }
        self.sync_mini();
        let shown = self.mini.borrow().as_ref().is_some_and(|mini| mini.show().is_ok());
        if !shown {
            return self.toast("The mini player couldn't be opened.");
        }
        self.mini_open.set(true);
        // The colours of its title bar can only be set once it's on screen.
        self.apply_theme();
        self.ui.window().set_minimized(true);
    }

    /// Puts the small player away and brings the full window back.
    pub fn close_mini(self: &Rc<Self>) {
        if !self.mini_open.replace(false) {
            return;
        }
        if let Some(mini) = self.mini.borrow().as_ref() {
            let _ = mini.hide();
        }
        self.show_window();
    }

    /// Tells the small player what's playing and how far through it is. Called four times
    /// a second while it's open.
    pub fn sync_mini(&self) {
        let mini = self.mini.borrow();
        let Some(mini) = mini.as_ref() else { return };
        let pb = self.playback.borrow();
        let song = pb.current();
        mini.set_has_track(song.is_some());
        mini.set_playing(pb.playing);
        mini.set_song(song.map(|song| song.title.as_str()).unwrap_or_default().into());
        mini.set_artist(song.map(|song| song.artist_label()).unwrap_or_default().into());
        mini.set_starred(song.is_some_and(|song| self.is_starred(song)));
        let length = song.map(|song| song.duration as f64).unwrap_or(0.0);
        mini.set_progress(if length > 0.0 { (pb.position / length) as f32 } else { 0.0 });
        mini.set_elapsed(if song.is_some() { clock(pb.position as u32).into() } else { "".into() });
        mini.set_total(if length > 0.0 { clock(length as u32).into() } else { "".into() });
        // The picture arrives a moment after it's first asked for; asking again finds it.
        let cover = song.map(|song| song.cover_art.clone()).unwrap_or_default();
        mini.set_picture(if cover.is_empty() { slint::Image::default() } else { self.covers.get(self, &cover, 200) });
    }

    /// Whether closing the window should leave the app running in the notification area.
    fn stays_in_tray(&self) -> bool {
        #[cfg(windows)]
        let has_tray = self.tray.borrow().is_some();
        #[cfg(not(windows))]
        let has_tray = false;
        has_tray && self.settings.borrow().close_to_tray
    }

    /// Shows the tray icon only while it has a job: when closing hides the window to it.
    pub fn sync_tray(&self) {
        #[cfg(windows)]
        {
            let pb = self.playback.borrow();
            let liked = pb.current().is_some_and(|song| self.is_starred(song));
            crate::taskbar::update(pb.playing, pb.current().is_some(), liked);
        }
        #[cfg(windows)]
        if let Some(tray) = self.tray.borrow().as_ref() {
            let pb = self.playback.borrow();
            tray.set_shown(self.settings.borrow().close_to_tray);
            tray.set_playing(pb.playing);
            tray.set_has_track(pb.current().is_some());
            tray.set_now(
                pb.current()
                    .map(|song| format!("{} - {}", song.title, song.artist_label()))
                    .unwrap_or_default()
                    .into(),
            );
        }
    }

    pub(crate) fn diagnostics_window_visible(&self) -> bool {
        let window = self.ui.window();
        (window.is_visible() && !window.is_minimized()) || self.mini_open.get()
    }

    /// Brings the window back from the tray or from behind other apps.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn show_window(self: &Rc<Self>) {
        let _ = self.ui.show();
        self.ui.window().set_minimized(false);
        #[cfg(windows)]
        crate::instance::focus(&self.ui);
        // Coming back to the app: see what's happened on other devices meanwhile.
        self.look_elsewhere();
    }

    pub fn quit(self: &Rc<Self>) {
        // From the first moment of shutting down, nothing more is sent to keep in step: a
        // write about to be made is not, and an answer on its way is dropped.
        self.let_go();
        // A jam hosted here ends with the app; its guests are told so, not left waiting.
        self.stop_linking();
        // (The new linking ends with the sign-in, and a look still on its way with it.)
        self.link_stop();
        self.end_jam(true);
        self.save_queue();
        // (The settings are written once, waiting for the disk, as the app ends: `main`.)
        let _ = slint::quit_event_loop();
    }

    // ---- updates -------------------------------------------------------------------------------

    /// Asks GitHub whether a newer version exists. `manual` is a check the user asked for:
    /// it always runs, and reports problems instead of staying quiet.
    pub fn check_for_updates(self: &Rc<Self>, manual: bool) {
        // Offline mode means not going online for anything, unless asked to directly.
        if !manual && (!self.settings.borrow().check_updates || self.settings.borrow().offline) {
            return;
        }
        if self.update_busy.get() {
            return;
        }
        let page = self.ui.global::<ui::Settings>();
        if manual {
            page.set_update_status("Checking…".into());
        }
        self.api_pool.spawn(move || {
            let result = crate::update::check();
            post(move |ctl| {
                let page = ctl.ui.global::<ui::Settings>();
                if let Err(reason) = &result {
                    diagnostics::fault(FaultPlace::Updates, reason.fault_shape());
                }
                match result {
                    Ok(Some(release)) => {
                        page.set_update_download_failed(false);
                        page.set_update_version(release.version.as_str().into());
                        page.set_update_status(
                            format!("Version {} is available.", release.version).into(),
                        );
                        ctl.ui.global::<ui::App>().set_update_available(true);
                        // Mention each new version once; after that the dot on the cog is enough.
                        if ctl.settings.borrow().update_notified != release.version {
                            ctl.settings.borrow_mut().update_notified = release.version.clone();
                            ctl.save_settings_in_passing();
                            ctl.toast(format!(
                                "Nexus Music {} is available. Open Settings to update.",
                                release.version
                            ));
                        }
                        *ctl.update.borrow_mut() = Some(release);
                    }
                    Ok(None) => {
                        *ctl.update.borrow_mut() = None;
                        page.set_update_version("".into());
                        page.set_update_status("You have the latest version.".into());
                        ctl.ui.global::<ui::App>().set_update_available(false);
                    }
                    Err(reason) if manual || reason == crate::update::CheckError::Refused => {
                        page.set_update_status(format!("Couldn't check for updates. {reason}").into());
                    }
                    Err(_) => {}
                }
            });
        });
    }

    /// Downloads the new version's installer and hands over to it.
    fn install_update(self: &Rc<Self>) {
        let Some(release) = self.update.borrow().clone() else { return };
        let page = self.ui.global::<ui::Settings>();
        if page.get_update_download_failed() {
            return crate::update::open_in_browser(&release.page);
        }
        let installer = match (&release.installer, crate::update::installed_copy()) {
            (Some(installer), true) => installer.clone(),
            // A portable copy, or a release without an installer: show where to get it.
            _ => return crate::update::open_in_browser(&release.page),
        };
        if self.update_busy.replace(true) {
            return;
        }
        page.set_update_busy(true);
        page.set_update_status("Downloading the update…".into());
        std::thread::spawn(move || {
            let mut last = -1;
            let result = crate::update::download(&installer, |fraction| {
                let percent = (fraction * 100.0) as i32;
                if percent != last {
                    last = percent;
                    post(move |ctl| {
                        let text = format!("Downloading the update… {percent}%");
                        ctl.ui.global::<ui::Settings>().set_update_status(text.into());
                    });
                }
            });
            post(move |ctl| {
                let page = ctl.ui.global::<ui::Settings>();
                let path = match result {
                    Ok(path) => path,
                    Err(error) => {
                        diagnostics::fault(FaultPlace::Updates, error.shape);
                        ctl.update_busy.set(false);
                        page.set_update_busy(false);
                        page.set_update_download_failed(true);
                        page.set_update_status("The update couldn't be downloaded. Get it from the release page.".into());
                        return;
                    }
                };
                // The installer closes the app as it starts, maybe before the save at the
                // end of `main`: the settings are on the disk first, as 0.40.10 had them.
                ctl.save_settings();
                match crate::update::run_installer(&path) {
                    // The installer replaces the program and opens the new version.
                    Ok(()) => ctl.quit(),
                    Err(reason) => {
                        diagnostics::fault(FaultPlace::Updates, FaultShape::Unavailable);
                        ctl.update_busy.set(false);
                        page.set_update_busy(false);
                        // Don't leave the user stuck: the same file is one click away in a browser.
                        page.set_update_status(format!("Couldn't start the installer: {reason} The download page has been opened instead.").into());
                        if let Some(release) = ctl.update.borrow().as_ref() {
                            crate::update::open_in_browser(&release.page);
                        }
                    }
                }
            });
        });
    }

    fn sign_in(self: &Rc<Self>, server: String, username: String, password: String) {
        let app = self.ui.global::<ui::App>();
        if app.get_login_busy() {
            return;
        }
        app.set_login_busy(true);
        app.set_login_error("".into());
        let username = username.trim().to_string();
        self.api_pool.spawn(move || {
            // An address typed without "http://" or "https://" is tried the safe way first,
            // and the unsafe way only when it looks like a home-network one.
            let mut answer = Err(crate::subsonic::ApiError::Network("No address was given.".into()));
            let addresses = crate::subsonic::addresses_to_try(&server);
            for (index, address) in addresses.iter().enumerate() {
                let client = Client::new(address, &username, &password);
                // HTTPS was a guess: if nothing answers there soon, go on to plain HTTP
                // without waiting out the full time a real server is given.
                if index + 1 < addresses.len() && !client.answers_within(Duration::from_secs(5)) {
                    continue;
                }
                answer = client.ping().map(|info| (client, info));
                // Signed in, or the server is there and says no: either way that settles it.
                if answer.as_ref().err().is_none_or(|error| matches!(error, crate::subsonic::ApiError::Server { .. })) {
                    break;
                }
            }
            post(move |ctl| {
                let app = ctl.ui.global::<ui::App>();
                app.set_login_busy(false);
                if let Err(error) = &answer {
                    diagnostics::music_connection_failed(error);
                }
                match answer {
                    Ok((client, info)) => {
                        {
                            let mut settings = ctl.settings.borrow_mut();
                            settings.server = client.server().to_string();
                            settings.username = username.clone();
                        }
                        ctl.save_settings_in_passing();
                        if !config::save_password(&ctl.paths, client.server(), &username, &password) {
                            diagnostics::fault(FaultPlace::App, FaultShape::Write);
                            ctl.toast("Couldn't save your password; you'll need to sign in next time.");
                        }
                        ctl.covers.reset();
                        let open = crate::subsonic::unencrypted_beyond_home(client.server());
                        ctl.enter(client, Some(info.version));
                        // The server is plainly there: stop treating it as out of reach, and
                        // send whatever was being held for it.
                        ctl.check_connection();
                        if open {
                            ctl.show_toast(
                                "Signed in without encryption: over plain HTTP your sign-in and your music can be read on the way. If your server has an https:// address, sign in with that.".into(),
                                "",
                                Duration::from_secs(12),
                            );
                        }
                    }
                    // Only HTTPS was tried, because no scheme was typed: say so, and how to
                    // choose otherwise. Not when the server itself answered (a wrong
                    // password, say): then HTTPS worked, and the sentence would mislead.
                    Err(e) if crate::subsonic::https_only(&server) && !matches!(e, crate::subsonic::ApiError::Server { .. }) => app.set_login_error(
                        format!("{e}\nNexus Music tried HTTPS. If your server has no HTTPS, type http:// in front of the address.").into(),
                    ),
                    Err(e) => app.set_login_error(e.to_string().into()),
                }
            });
        });
    }

    /// Switches from the sign-in screen to the app proper.
    fn enter(self: &Rc<Self>, client: Client, version: Option<String>) {
        let app = self.ui.global::<ui::App>();
        app.set_server(client.server().into());
        app.set_username(client.username().into());
        app.set_server_version(version.unwrap_or_default().into());
        // Offline mode holds for whichever way this sign-in came: restored at start-up, or
        // typed in after signing out with it on. (Before the warm-up: nothing is sent.)
        client.set_offline(self.settings.borrow().offline);
        // Have connections open before the first clicks need them.
        let warm = client.clone();
        self.art_pool.spawn(move || warm.warm_up(6));
        *self.client.borrow_mut() = Some(client);
        self.load_account();
        app.set_signed_in(true);
        *self.nav.borrow_mut() = Nav::default();
        self.cache.borrow_mut().clear();
        self.restore_queue();
        self.reload_playlists();
        self.navigate(Route::Home);
        self.start_sync();
        self.asked.borrow_mut().tidy(chrono::Utc::now().timestamp());
        self.load_library();
        self.library_timer.start(slint::TimerMode::Repeated, Duration::from_secs(5 * 60), || {
            with(|ctl| ctl.load_library());
        });
    }

    /// Who is signed in, as a name that can be part of a file's name; empty when nobody is.
    pub fn account(&self) -> String {
        self.client().map(|client| account_stem(client.server(), client.username())).unwrap_or_default()
    }

    /// Where the changes held for the server are kept for whoever is signed in.
    pub fn pending_path(&self) -> Option<PathBuf> {
        let account = self.account();
        (!account.is_empty()).then(|| config::pending_file(&self.paths, &account))
    }

    /// Brings in what belongs to the account that has just signed in: its downloads, and
    /// the likes, ratings and plays still waiting to be sent for it. Each account has its
    /// own, so one person's never show up for, or get sent as, another.
    fn load_account(&self) {
        let account = self.account();
        let folder = self.downloads.borrow().folder().to_path_buf();
        *self.downloads.borrow_mut() = crate::downloads::Downloads::load(folder, &account);
        self.sending.borrow_mut().clear();
        let held = self.pending_path().map(|path| {
            // Before 0.29 there was one list for whoever was signed in.
            if !path.exists() && self.paths.pending.exists() {
                let _ = std::fs::rename(&self.paths.pending, &path);
            }
            crate::offline::load_pending(&path)
        });
        *self.pending.borrow_mut() = held.unwrap_or_default();
    }

    // ---- asking for albums that aren't in the library -------------------------------------------

    /// Sets up (or drops) the connection to Lidarr from what's in Settings.
    pub fn connect_requests(&self) {
        // Versions before 0.13.1 could keep a password for a sign-in in front of Lidarr.
        if config::load_secret(&self.paths, "lidarr-password").is_some() {
            config::save_secret(&self.paths, "lidarr-password", "");
        }
        let lidarr = {
            let settings = self.settings.borrow();
            let vault = self.vault();
            // At the start, a key kept by 0.40.10 is bound to the address configured then. If
            // that can't be done (its write failed, or the store couldn't be read), it's used,
            // unbound, for that address only, and tried again at the next start.
            if !self.keys_looked_at.replace(true) && crate::keep::bind_old_key(&*vault, &settings.lidarr_address) == crate::keep::Bind::NotYet {
                self.key_old_for.replace(Some(settings.lidarr_address.clone()));
            }
            let key = crate::keep::key(&*vault, &settings.lidarr_address, self.key_old_for.borrow().as_deref());
            // An address the app can't be sure of reaching as written isn't connected to.
            // (Read as 0.40.10 used it: a saved address isn't held to what's asked of a typed one.)
            let usable = nexus_core::lidarr::origin_saved(&settings.lidarr_address).is_ok();
            match (settings.requests && !settings.lidarr_address.trim().is_empty() && usable, key) {
                (true, Some(key)) => {
                    let gate = self.gate_session(&settings);
                    // What the connection is made of: when it changes, so does its number, and
                    // what was begun under the old one does nothing (see `lidarr::for_connection`).
                    let made_of = format!("{}\n{key}\n{}", crate::lidarr::tidy(&settings.lidarr_address), gate.as_ref().map_or(0, |_| self.sign_ins.get()));
                    Some((crate::lidarr::open(&settings.lidarr_address, &key, gate), made_of))
                }
                _ => None,
            }
        };
        self.ui.global::<ui::App>().set_requests(lidarr.is_some());
        crate::lidarr::set_singles(self.settings.borrow().request_singles);
        let (lidarr, made_of) = lidarr.map_or((None, String::new()), |(lidarr, made_of)| (Some(lidarr), made_of));
        crate::lidarr::set_current_made_of(lidarr, made_of);
    }

    /// The credential store, where Lidarr's key and the sign-in's token are kept (see `keep`).
    fn vault(&self) -> std::sync::Arc<dyn crate::keep::Vault> {
        std::sync::Arc::new(crate::keep::Store(self.paths.clone()))
    }

    /// The sign-in in front of Lidarr, picked up from what's on file if it isn't in use yet.
    fn gate_session(&self, settings: &config::Settings) -> Option<std::sync::Arc<crate::gate::Session>> {
        let found = crate::gate::Found { service: settings.lidarr_gate.clone(), client: settings.lidarr_gate_client.clone() };
        let trusted = !found.service.is_empty() && found.trusted(&settings.lidarr_address).is_ok();
        // At the start, and then only, a token kept by 0.40.10 is bound to the service saved
        // with it if that one is believed, and is otherwise left unused (see `keep`). If that
        // can't be done it's used, unbound, for that service only, tried again next start, and
        // said.
        if !self.tokens_looked_at.replace(true) && crate::keep::bind_old_token(&*self.vault(), &found.service, trusted, settings.lidarr_token_spent) == crate::keep::Bind::NotYet {
            self.token_old_for.replace(Some(found.service.clone()));
            self.toast(SIGN_IN_NOT_SAVED);
        }
        // One kept by an older version that isn't believed now is not used (see `Found::trusted`).
        if !trusted {
            self.gate.replace(None);
            return None;
        }
        if let Some(session) = self.gate.borrow().as_ref().filter(|session| *session.found() == found) {
            return Some(session.clone());
        }
        let token = crate::keep::token(&*self.vault(), &found.service, self.token_old_for.borrow().as_deref())?;
        let session = std::sync::Arc::new(self.new_gate(found, token));
        self.gate.replace(Some(session.clone()));
        Some(session)
    }

    /// A session for the sign-in at `found`, begun with `token`: its renewals are written only
    /// while the token kept is still the one it renewed (see `keep::renewals`).
    fn new_gate(&self, found: crate::gate::Found, token: String) -> crate::gate::Session {
        self.sign_ins.set(self.sign_ins.get() + 1);
        let keep = crate::keep::renewals(self.vault(), found.service.clone(), token.clone(), || post(|ctl| ctl.toast(SIGN_IN_NOT_SAVED)));
        crate::gate::session(found, token, keep)
    }

    /// Shows where the connection to Lidarr stands: `state` picks the marker and the one
    /// action on offer ("none", "checking", "connected", "sign-in", "waiting" or "error"),
    /// `text` says it in words.
    fn requests_state(&self, state: &str, text: &str) {
        let page = self.ui.global::<ui::Settings>();
        page.set_requests_state(state.into());
        page.set_requests_status(text.into());
    }

    /// "Lidarr is behind the sign-in at auth.example.org."
    fn behind_sign_in(&self) -> String {
        let gate = crate::gate::Found { service: self.settings.borrow().lidarr_gate.clone(), client: String::new() };
        nexus_core::gate::behind_sign_in(gate.host())
    }

    /// Saves where Lidarr is and its key, then connects: checks that it answers and, if a
    /// sign-in answers instead, goes on to that.
    fn save_requests(&self, address: &str, key: &str) {
        // An address with a name and password in it, or one the app might read otherwise than
        // its connections would, isn't kept: nor is the key typed with it.
        // (The address already saved, submitted with only a new key, is judged as it was saved.)
        if !address.trim().is_empty() {
            let saved = self.settings.borrow().lidarr_address.clone();
            if let Err(why) = nexus_core::lidarr::origin_entered(address, &saved) {
                self.push_settings();
                return self.requests_state("error", &why);
            }
        }
        let moved = crate::lidarr::tidy(&self.settings.borrow().lidarr_address) != crate::lidarr::tidy(address);
        // The key is kept with the origin it was typed for, before the address is saved: blank
        // leaves the key that's on file, unless the address now leads somewhere else (another
        // scheme, host or port). See `keep::change`.
        let old = self.settings.borrow().lidarr_address.clone();
        // A sign-in belongs to the address it was found in front of: let go of only once the
        // change is going through, and while that's still the address, so it's handed back
        // only where it was believed. A change that's refused keeps it.
        let mut let_go = || {
            if moved {
                self.drop_sign_in();
                let mut settings = self.settings.borrow_mut();
                settings.lidarr_gate.clear();
                settings.lidarr_gate_client.clear();
            }
        };
        let changed = crate::keep::change(&*self.vault(), &old, address.trim(), key.trim(), &mut let_go, &mut || {
            let mut settings = self.settings.borrow_mut();
            settings.lidarr_address = address.trim().to_string();
            settings.save_surely(&self.paths.settings)
        });
        let forgotten = match changed {
            crate::keep::Changed::Made { forgotten } => forgotten,
            crate::keep::Changed::Not(why) => {
                self.settings.borrow_mut().lidarr_address = old;
                self.connect_requests();
                self.push_settings();
                return self.requests_state("error", why);
            }
        };
        self.connect_requests();
        self.push_settings();
        if forgotten {
            return self.requests_state("none", nexus_core::lidarr::KEY_FORGOTTEN);
        }
        self.check_requests(true);
    }

    /// Asks Lidarr whether it's there and, if a sign-in answers instead, which one.
    /// `by_hand` when someone pressed Connect: then a sign-in that's needed is started
    /// straight away, not just offered.
    pub fn check_requests(&self, by_hand: bool) {
        let Some(lidarr) = crate::lidarr::current() else {
            // One kept by an older version that this one won't use says why.
            let address = self.settings.borrow().lidarr_address.clone();
            if let (false, Err(why)) = (address.trim().is_empty(), nexus_core::lidarr::origin_saved(&address)) {
                return self.requests_state("error", &why);
            }
            return self.requests_state("none", nexus_core::lidarr::NOT_CONNECTED);
        };
        let check = self.requests_check.get() + 1;
        self.requests_check.set(check);
        self.requests_state("checking", nexus_core::lidarr::CHECKING);
        let address = crate::lidarr::tidy(&self.settings.borrow().lidarr_address);
        self.api_pool.spawn(move || {
            let answer = lidarr.version();
            let gate = match &answer {
                Err(reason) if reason == crate::gate::NEEDED => {
                    Some(crate::gate::find(&crate::subsonic::make_agent_without_redirects(), &address))
                }
                _ => None,
            };
            post(move |ctl| {
                // Only the newest check counts.
                if ctl.requests_check.get() == check {
                    ctl.requests_checked(by_hand, answer, gate);
                }
            });
        });
    }

    fn requests_checked(&self, by_hand: bool, answer: Result<String, String>, gate: Option<Result<Option<crate::gate::Found>, String>>) {
        if let Some(Ok(Some(found))) = &gate {
            let known = {
                let settings = self.settings.borrow();
                settings.lidarr_gate == found.service && settings.lidarr_gate_client == found.client
            };
            if !known {
                self.drop_sign_in();
                {
                    let mut settings = self.settings.borrow_mut();
                    settings.lidarr_gate = found.service.clone();
                    settings.lidarr_gate_client = found.client.clone();
                }
                self.save_settings_in_passing();
                self.connect_requests();
            }
        }
        self.push_settings();
        let service = self.settings.borrow().lidarr_gate.clone();
        let signed_out = !service.is_empty() && crate::keep::token(&*self.vault(), &service, self.token_old_for.borrow().as_deref()).is_none();
        match (answer, gate) {
            (Ok(version), _) => self.requests_state("connected", &nexus_core::lidarr::connected(&version)),
            (Err(_), Some(Ok(Some(_)))) if by_hand => self.sign_in_requests(),
            (Err(_), Some(Ok(Some(_)))) => self.requests_state("sign-in", &self.behind_sign_in()),
            (Err(_), Some(Err(why))) => self.requests_state("error", &why),
            (Err(reason), _) => self.requests_state(if signed_out { "sign-in" } else { "error" }, &reason),
        }
    }

    /// Starts a sign-in to what's in front of Lidarr: opens it in the browser and waits.
    fn sign_in_requests(&self) {
        let found = {
            let settings = self.settings.borrow();
            crate::gate::Found { service: settings.lidarr_gate.clone(), client: settings.lidarr_gate_client.clone() }
        };
        if found.service.is_empty() {
            return;
        }
        if let Err(why) = found.trusted(&self.settings.borrow().lidarr_address) {
            return self.requests_state("error", &why);
        }
        let round = crate::gate::new_round();
        self.requests_check.set(self.requests_check.get() + 1);
        self.requests_state("waiting", &nexus_core::gate::finish_in_browser(found.host()));
        std::thread::spawn(move || {
            let agent = crate::subsonic::make_agent_without_redirects();
            let (result, browser) = match crate::gate::begin(&found) {
                Ok(attempt) => {
                    let link = attempt.link.clone();
                    post(move |_| crate::gate::open_link(&link));
                    attempt.finish(&agent, || !crate::gate::is_current(round))
                }
                Err(why) => (Err(why), None),
            };
            post(move |ctl| ctl.signed_in(round, found, result, browser));
        });
    }

    /// What a sign-in came to. `browser`, if it came back with a code, is told what the app
    /// decided: "You're signed in" only once the token is kept for the round still under way
    /// (a cancelled or replaced round, or a failed exchange, is told it didn't go through).
    fn signed_in(&self, round: u64, found: crate::gate::Found, result: Result<crate::gate::SignedIn, String>, browser: Option<crate::gate::Browser>) {
        use nexus_core::gate::Page;
        let tell = |page: Page| {
            if let Some(browser) = &browser {
                browser.answer(page);
            }
        };
        if !crate::gate::is_current(round) {
            return tell(Page::NotThrough);
        }
        match result {
            Ok(done) => {
                // From here this sign-in owns the token kept: whatever an earlier one still has
                // on its way writes nothing.
                if crate::keep::keep_token(&*self.vault(), &found.service, &done.token) {
                    // Whatever 0.40.10 kept is gone now (written over): nothing is spent.
                    self.settings.borrow_mut().lidarr_token_spent = false;
                    tell(Page::SignedIn);
                } else {
                    tell(Page::StillSigningIn);
                    self.toast(SIGN_IN_NOT_SAVED);
                }
                self.settings.borrow_mut().lidarr_signed_in = done.name;
                self.save_settings_in_passing();
                self.gate.replace(Some(std::sync::Arc::new(self.new_gate(found, done.token))));
                self.connect_requests();
                self.push_settings();
                self.check_requests(false);
            }
            Err(why) => {
                self.push_settings();
                self.requests_state("sign-in", &why);
            }
        }
    }

    /// Stops waiting for a sign-in that was started.
    fn cancel_sign_in(&self) {
        crate::gate::new_round();
        self.requests_state("sign-in", &self.behind_sign_in());
    }

    /// Forgets the sign-in in front of Lidarr and hands its token back to the service.
    fn drop_sign_in(&self) {
        crate::gate::new_round();
        let session = self.gate.replace(None);
        let service = self.settings.borrow().lidarr_gate.clone();
        // Whatever is kept goes; it's handed back only if it was made at this service, and that
        // one is believed (see `Found::trusted`): otherwise it's sent nowhere.
        let vault = self.vault();
        let (on_file, spent) = crate::keep::take_token_telling(&*vault, &service);
        // A token from 0.40.10, unbound, is never used again from here: not this run (it was
        // only ever used for the service saved at the start), nor after a restart, if it
        // couldn't be deleted (written down in the settings, at once).
        self.token_old_for.replace(None);
        if spent {
            self.settings.borrow_mut().lidarr_token_spent = true;
            self.save_settings();
        }
        let believed = crate::gate::Found { service: service.clone(), client: String::new() }.trusted(&self.settings.borrow().lidarr_address).is_ok();
        self.settings.borrow_mut().lidarr_signed_in.clear();
        if session.is_none() && on_file.is_none() {
            return;
        }
        self.api_pool.spawn(move || {
            // Closing waits out a renewal that's under way, so the token handed back is the
            // one that's good now.
            let token = session.map(|session| session.close()).filter(|token| !token.is_empty()).or(on_file);
            let Some(token) = token else { return };
            if believed {
                crate::gate::give_up_token(&crate::subsonic::make_agent_without_redirects(), &service, &token);
            }
            // Cleared again only if it's still this very token (its first clearing failed):
            // anything else there by now is a sign-in made meanwhile.
            crate::keep::clear_token_if(&*vault, &token);
        });
    }

    fn sign_out_requests(&self) {
        self.drop_sign_in();
        self.save_settings();
        self.connect_requests();
        self.push_settings();
        // Signed out of one that's refused, it's still said why: there's nothing to sign in to.
        let refused = {
            let settings = self.settings.borrow();
            crate::gate::Found { service: settings.lidarr_gate.clone(), client: String::new() }.trusted(&settings.lidarr_address).err()
        };
        match refused {
            Some(why) => self.requests_state("error", &why),
            None => self.requests_state("sign-in", &nexus_core::gate::signed_out(&self.behind_sign_in())),
        }
    }

    /// Asks whether to request an album that isn't in the library. `song` is the song that
    /// led here, if it was one: the album is what gets fetched, and that's said.
    pub fn prompt_request(self: &Rc<Self>, album: &str, title: &str, by: &str, song: &str, kind: &str) {
        let (heading, text) = match nexus_core::requests::asking(crate::lidarr::offers(), album, title, by, song, kind) {
            nexus_core::requests::Asking::Already(said) => return self.toast(format!("{said} See Requests in the sidebar.")),
            nexus_core::requests::Asking::Ask { heading, text } => (heading, text),
        };
        *self.prompt.borrow_mut() =
            Some(crate::actions::PromptAction::RequestAlbum { album: album.to_string(), title: title.to_string(), song: song.to_string() });
        self.open_prompt(&heading, &text, "Request", None, false);
    }

    /// Reloads the page on screen if it shows music that isn't in the library, after what's
    /// been asked for has changed.
    fn refresh_outside(self: &Rc<Self>) {
        self.invalidate("search:");
        self.invalidate("requests");
        self.invalidate("outside-");
        if let Some(route @ (Route::OutsideAlbum(_) | Route::OutsideArtist(_) | Route::Search(_))) = self.current_route() {
            let scroll = self.ui.global::<ui::View>().get_scroll_y();
            self.show(route, scroll, true, None);
        }
    }

    pub fn request_album(self: &Rc<Self>, album: String, title: String, song: String) {
        let Some(lidarr) = crate::lidarr::current() else { return };
        self.toast(nexus_core::requests::asking_for(&title));
        self.requests_pool.spawn(move || {
            // (Nothing on Windows stops a request, so "stopped" can't come of it.)
            let result = nexus_core::requests::request(&lidarr, crate::lidarr::offers(), &album, crate::bulk::way_back()).unwrap_or_else(|_| Err(nexus_core::http::STOPPED.into()));
            post(move |ctl| match result {
                Ok(artist) => {
                    // Noted, so it can be followed up when it arrives.
                    let now = chrono::Utc::now().timestamp();
                    let noted = ctl.asked.borrow_mut().add(nexus_core::requests::asked_for(&album, &title, &artist, &song, now));
                    // Marked once the note is kept and shared (see `asked_for`).
                    crate::lidarr::mark(&album, true);
                    ctl.refresh_outside();
                    ctl.toast(if noted { nexus_core::requests::asked_sentence(&title) } else { nexus_core::requests::asked_unnoted(&title, "computer") });
                }
                Err(reason) => ctl.toast(reason),
            });
        });
    }

    /// Plays a song that's suggested for the playlist on screen, by itself.
    fn play_suggestion(self: &Rc<Self>, song: &str) {
        let Some(found) = crate::search::current().and_then(|index| index.library.songs.iter().find(|known| known.id == song).cloned()) else { return };
        let title = found.title.clone();
        self.play_songs(vec![found], 0, &title, false);
    }

    /// Adds a suggested song to the playlist on screen. The row is ticked at once; the
    /// page then reloads with the song in the list and a fresh suggestion in its place.
    fn add_suggestion(self: &Rc<Self>, index: usize) {
        let (playlist, song) = {
            let mut page = self.page.borrow_mut();
            let Some(playlist) = page.playlist.as_ref().map(|playlist| playlist.id.clone()) else { return };
            let Some(offer) = page.offers.get_mut(index).filter(|offer| !offer.requested) else { return };
            offer.requested = true;
            (playlist, offer.song.clone())
        };
        let Some(found) = crate::search::current().and_then(|index| index.library.songs.iter().find(|known| known.id == song).cloned()) else { return };
        let view = self.ui.global::<ui::View>();
        let offers = view.get_offers();
        if let Some(mut row) = offers.row_data(index) {
            row.requested = true;
            offers.set_row_data(index, row);
        }
        self.add_to_playlist(playlist, vec![found]);
    }

    /// Asks whether to have Lidarr keep track of the artist whose page this is.
    fn prompt_add_artist(self: &Rc<Self>, artist: String) {
        let (name, albums) = {
            let page = self.page.borrow();
            let open = page
                .shelves
                .iter()
                .chain(&page.tail)
                .filter(|shelf| shelf.title == "Albums")
                .flat_map(|shelf| &shelf.cards)
                .filter(|card| card.kind == Kind::Request && !card.subtitle.ends_with("Requested") && !card.subtitle.ends_with("Arrived"))
                .count();
            (page.title.clone(), open)
        };
        if name.is_empty() || albums == 0 {
            return;
        }
        let (heading, text) = nexus_core::requests::adding(&name, albums);
        *self.prompt.borrow_mut() = Some(crate::actions::PromptAction::AddArtist { artist, name: name.clone() });
        self.open_prompt(&heading, &text, "Add artist", None, false);
    }

    pub fn add_artist(self: &Rc<Self>, artist: String, name: String) {
        let Some(lidarr) = crate::lidarr::current() else { return };
        self.toast(nexus_core::requests::adding_now(&name));
        self.requests_pool.spawn(move || {
            let result = lidarr.add_artist(&artist, &name, crate::bulk::way_back());
            post(move |ctl| match result {
                Ok(_) => {
                    ctl.refresh_outside();
                    crate::discover::forget(&artist);
                    ctl.toast(format!("{}; see Requests in the sidebar.", nexus_core::requests::added(&name)));
                }
                Err(reason) => ctl.toast(reason),
            });
        });
    }

    /// What to ask for to get the songs an import couldn't find: the album each is on, once,
    /// with the songs wanted from it and the playlists they go into.
    fn missing_wants(&self) -> Vec<crate::bulk::Want> {
        let name = |text: &str| crate::search::fold(text).split_whitespace().collect::<Vec<_>>().join(" ");
        let mut wants: Vec<crate::bulk::Want> = Vec::new();
        let mut places: HashMap<(String, String), usize> = HashMap::new();
        // The two lists run side by side: a song, and where it goes when it arrives.
        for (song, (_, fill)) in self.import_missing.borrow().iter().zip(self.import_fills.borrow().iter()) {
            let Some(artist) = song.artists.first().filter(|artist| !artist.trim().is_empty()) else { continue };
            let album = song.album.trim();
            // A song whose album the file doesn't name is found by the song itself.
            let key = (name(artist), if album.is_empty() { format!("\u{1}{}", name(&song.title)) } else { name(album) });
            let at = *places.entry(key).or_insert_with(|| {
                wants.push(crate::bulk::Want { artist: artist.clone(), album: album.to_string(), ..Default::default() });
                wants.len() - 1
            });
            if !wants[at].songs.contains(&song.title) {
                wants[at].songs.push(song.title.clone());
            }
            if !wants[at].fills.contains(fill) {
                wants[at].fills.push(fill.clone());
            }
        }
        wants
    }

    /// Offers to ask for the albums that the songs an import couldn't find are on.
    fn prompt_request_missing(self: &Rc<Self>) {
        let albums = self.missing_wants().len();
        if albums == 0 || crate::lidarr::current().is_none() {
            return;
        }
        if crate::bulk::running() {
            self.toast("Lidarr is still being asked for the last lot. See Requests.");
            return self.navigate(Route::Requests);
        }
        let songs = count(self.import_missing.borrow().len() as u32, "song");
        let many = count(albums as u32, "album");
        let (heading, text) = if crate::lidarr::singles() {
            (
                format!("Ask for {many} and singles?"),
                format!(
                    "The {songs} that weren't found came out on {albums} albums, EPs and singles. Each goes on Lidarr's list whole, as the playlist names it: a song listed by its single is asked for as that single. For that, an artist Lidarr only lists albums of is set to list their singles and EPs too. Nothing but what's asked for is fetched."
                ),
            )
        } else {
            (
                format!("Ask for {many}?"),
                format!(
                    "The {songs} that weren't found are on {many}. Each goes on Lidarr's list as a whole album. A song that only came out on a single or an EP is left out, because those are switched off in Settings."
                ),
            )
        };
        *self.prompt.borrow_mut() = Some(crate::actions::PromptAction::RequestMissing);
        self.open_prompt(&heading, &text, "Request them", None, false);
    }

    /// Asks for everything the last import didn't find. It's handed over as one batch, which
    /// is worked through in the background and shown on the Requests page (see `bulk`).
    pub fn request_missing(self: &Rc<Self>) {
        let wants = self.missing_wants();
        if wants.is_empty() || crate::lidarr::current().is_none() {
            return;
        }
        self.toast(format!("Asking Lidarr for {}. How it's going is shown here.", count(wants.len() as u32, "album")));
        crate::bulk::start(wants);
        self.run_bulk();
        self.invalidate("requests");
        self.navigate(Route::Requests);
    }

    /// Carries on with a batch of albums being asked for, if there is one and Lidarr is set up.
    pub fn run_bulk(self: &Rc<Self>) {
        let Some(lidarr) = crate::lidarr::current() else { return };
        if !crate::bulk::unfinished() {
            return;
        }
        self.bulk_unnoted.set((false, false));
        crate::bulk::run(lidarr, |event| post(move |ctl| ctl.bulk_event(event)));
    }

    fn bulk_event(self: &Rc<Self>, event: crate::bulk::Event) {
        use crate::bulk::Event;
        match event {
            Event::Changed => self.show_bulk_notice(),
            Event::Noted(notes) => {
                for note in &notes {
                    crate::lidarr::mark(&note.album, true);
                }
                if !self.asked.borrow_mut().add_all(notes) {
                    self.bulk_unnoted.set((true, self.bulk_unnoted.get().1));
                }
                self.invalidate("requests");
            }
            Event::Forgot(gone) => {
                let mut asked = self.asked.borrow_mut();
                for album in gone {
                    if !asked.forget(&album.album, &album.title, &album.artist, config::Durable::InPassing) {
                        self.bulk_unnoted.set((true, self.bulk_unnoted.get().1));
                    }
                    crate::lidarr::mark(&album.album, false);
                }
            }
            // Notes that couldn't be written down are said once for the batch, in place of its
            // summary (the Requests page has the counts).
            Event::Asked if self.bulk_unnoted.get() == (true, false) => {
                self.bulk_unnoted.set((true, true));
                self.toast(nexus_core::requests::asked_unnoted_batch("computer"));
                self.reload_requests();
            }
            Event::Asked => {
                let summary = crate::bulk::summary();
                if summary.listed > 0 {
                    let missed = summary.unknown + summary.failed;
                    let rest = if missed == 0 { String::new() } else { format!(" {missed} couldn't be asked for; see Requests.") };
                    self.toast(format!("{} on Lidarr's list.{rest}", if summary.listed == 1 { "1 album is".to_string() } else { format!("{} albums are", summary.listed) }));
                }
                self.reload_requests();
            }
            Event::Ended => {
                let summary = crate::bulk::summary();
                if !summary.stopped.is_empty() {
                    self.toast(format!("Stopped asking Lidarr. {}", summary.stopped));
                } else if summary.unknown + summary.failed == 0 {
                    // Nothing left to say about it.
                    crate::bulk::clear();
                }
                // The batch's notes, written in passing while it ran: flushed once, now.
                if !self.asked.borrow_mut().flush() {
                    self.bulk_unnoted.set((true, self.bulk_unnoted.get().1));
                }
                if self.bulk_unnoted.get() == (true, false) {
                    self.bulk_unnoted.set((true, true));
                    self.toast(nexus_core::requests::asked_unnoted_batch("computer"));
                }
                self.refresh_outside();
                self.reload_requests();
            }
        }
    }

    /// Shows how the batch stands, if the Requests page is the one on screen.
    fn show_bulk_notice(&self) {
        if self.current_route() != Some(Route::Requests) {
            return;
        }
        let (notice, action, other) = crate::bulk::summary().notice();
        let notice = pages::requests_notice(&notice);
        let view = self.ui.global::<ui::View>();
        view.set_notice(notice.as_str().into());
        view.set_notice_action(action.into());
        view.set_notice_other(other.into());
        let mut page = self.page.borrow_mut();
        page.notice = notice;
        page.notice_action = action.to_string();
        page.notice_other = other.to_string();
    }

    /// Draws the Requests page afresh if it's the one on screen.
    fn reload_requests(self: &Rc<Self>) {
        self.invalidate("requests");
        if self.current_route() == Some(Route::Requests) {
            let scroll = self.ui.global::<ui::View>().get_scroll_y();
            self.show(Route::Requests, scroll, true, None);
        }
    }

    /// A button beside what the Requests page says of a batch: stop it, carry on with it,
    /// have another go at what it couldn't ask for, or clear away what's left of it.
    fn bulk_action(self: &Rc<Self>, other: bool) {
        let action = {
            let page = self.page.borrow();
            if other { page.notice_other.clone() } else { page.notice_action.clone() }
        };
        match action.as_str() {
            "Stop" => {
                crate::bulk::stop();
                return self.toast("Stopping. What's already been asked for stays on Lidarr's list.");
            }
            "Carry on" => {
                if crate::lidarr::current().is_none() {
                    return self.toast("Requests are switched off. Turn them on in Settings.");
                }
                self.run_bulk();
            }
            "Try again" => {
                if crate::lidarr::current().is_none() {
                    return self.toast("Requests are switched off. Turn them on in Settings.");
                }
                if crate::bulk::again() {
                    self.run_bulk();
                }
            }
            _ => crate::bulk::clear(),
        }
        self.reload_requests();
    }

    /// Albums that were asked for have turned up in the library: says so, and puts the songs
    /// their playlists were missing into those playlists.
    fn requests_arrived(self: &Rc<Self>, arrivals: Vec<crate::asked::Arrival>) {
        let now = chrono::Utc::now().timestamp();
        let told: Vec<crate::asked::Asked> = {
            let mut asked = self.asked.borrow_mut();
            // By the note each was found for, as it's marked (see `asked::is_note`).
            let fresh = nexus_core::asked::news(&asked.items, &arrivals);
            asked.mark(&arrivals, now);
            fresh
        };
        // Arrivals that are news; the rest arrived before and are here to have their
        // playlists seen to again.
        let news = nexus_core::requests::arrived_news(&told);
        if !told.is_empty() {
            self.toast(format!("{news} See Requests."));
            self.refresh_outside();
            if self.current_route() == Some(Route::Requests) {
                self.show(Route::Requests, 0.0, true, None);
            }
        }
        // What to note once each one's songs are in their playlists.
        let settle: Vec<(i64, String, usize)> =
            arrivals.iter().filter(|arrival| !arrival.fills.is_empty()).map(|arrival| (arrival.at, arrival.title.clone(), arrival.missing)).collect();
        // The songs playlists were waiting for, gathered by playlist. A playlist that was
        // never made (none of its songs were found at the time) is made now.
        let mut by_list: Vec<(String, String, Vec<(String, i64)>)> = Vec::new();
        for (playlist, name, song, added) in arrivals.into_iter().flat_map(|arrival| arrival.fills) {
            let id = if playlist.is_empty() {
                self.playlists.borrow().iter().find(|known| known.name == name).map(|known| known.id.clone()).unwrap_or_default()
            } else {
                playlist
            };
            match by_list.iter_mut().find(|(known, known_name, _)| *known == id && *known_name == name) {
                Some((_, _, songs)) => songs.push((song, added)),
                None => by_list.push((id, name, vec![(song, added)])),
            }
        }
        if by_list.is_empty() {
            return;
        }
        self.request(
            move |client| {
                let mut done: Vec<(String, String, Vec<(String, i64)>)> = Vec::new();
                for (id, name, songs) in by_list {
                    let ids: Vec<String> = songs.iter().map(|(song, _)| song.clone()).collect();
                    let (id, songs) = if id.is_empty() {
                        (client.create_playlist(&name, &ids)?.id, songs)
                    } else {
                        // Not ones the playlist has already: gained some other way in the
                        // meantime, or put there by an earlier go at this that was cut short.
                        let have: HashSet<String> = client.playlist(&id)?.songs.into_iter().map(|song| song.id).collect();
                        let new: Vec<(String, i64)> = songs.into_iter().filter(|(song, _)| !have.contains(song)).collect();
                        if !new.is_empty() {
                            let ids: Vec<String> = new.iter().map(|(song, _)| song.clone()).collect();
                            client.add_to_playlist(&id, &ids)?;
                        }
                        (id, new)
                    };
                    done.push((id, name, songs));
                }
                Ok(done)
            },
            move |ctl, result| {
                // If a playlist couldn't be saved, these stay marked as waiting and are tried
                // again the next time the library is looked at.
                let Ok(done) = result else { return };
                for (at, title, missing) in &settle {
                    ctl.asked.borrow_mut().filled(*at, title, *missing);
                }
                let (mut songs, mut names) = (0, Vec::new());
                for (id, name, added) in &done {
                    // Dated as they were on the playlist they came from, where that's known.
                    let dated: Vec<(String, i64)> = added.iter().map(|(song, at)| (song.clone(), if *at > 0 { *at } else { now })).collect();
                    ctl.added.borrow_mut().record_at(id, &dated, false, now);
                    songs += added.len();
                    if !added.is_empty() {
                        names.push(name.clone());
                    }
                }
                ctl.share_soon();
                ctl.invalidate("playlist:");
                ctl.reload_playlists();
                if songs == 0 {
                    return;
                }
                let added = match names.as_slice() {
                    [one] => format!("Added {} to “{one}”.", count(songs as u32, "song")),
                    _ => format!("Added {} to {} playlists.", count(songs as u32, "song"), names.len()),
                };
                ctl.toast(if news.is_empty() { added } else { format!("{news} {added}") });
            },
        );
    }

    /// Takes an album off Lidarr's list (its number there), and forgets the note of it: the
    /// one with its MusicBrainz id, or its title and artist both (see `asked::forget`). It's
    /// no longer shown as asked for where it's offered.
    /// `connection` is the number of the connection the row was drawn under: if it's changed
    /// since, the row is another Lidarr's, nothing is sent, and the page is drawn afresh.
    pub fn cancel_request(self: &Rc<Self>, connection: u64, id: i64, album: String, title: String, artist: String) {
        let Some(lidarr) = crate::lidarr::for_connection(connection) else {
            self.invalidate("requests");
            if self.current_route() == Some(Route::Requests) {
                self.show(Route::Requests, 0.0, true, None);
            }
            return;
        };
        self.api_pool.spawn(move || {
            let result = lidarr.cancel(id);
            post(move |ctl| {
                match result {
                    Ok(()) => {
                        let noted = ctl.asked.borrow_mut().forget(&album, &title, &artist, config::Durable::Flushed);
                        crate::lidarr::mark(&album, false);
                        ctl.toast(if noted { nexus_core::requests::off_the_list(&title) } else { nexus_core::requests::off_the_list_unnoted(&title, "computer") });
                    }
                    Err(reason) => ctl.toast(reason),
                }
                ctl.invalidate("requests");
                ctl.invalidate("search:");
                // Its page and its artist's, kept from before, would still say "Requested".
                ctl.invalidate("outside-");
                if ctl.current_route() == Some(Route::Requests) {
                    ctl.show(Route::Requests, 0.0, true, None);
                }
            });
        });
    }

    /// Keeps the Requests page current while it's open. Called four times a second.
    pub fn tick_requests(self: &Rc<Self>) {
        // While something asked for is on its way, look for it in the library every minute
        // (a quick question to the server; the library is only read again if it has grown).
        let waited = self.asked_ticks.get() + 1;
        self.asked_ticks.set(waited);
        if waited % 240 == 0 && self.asked.borrow().waiting() && crate::lidarr::current().is_some() {
            self.load_library();
        }
        if self.current_route() != Some(Route::Requests) {
            return self.requests_ticks.set(0);
        }
        let ticks = self.requests_ticks.get() + 1;
        // Every half minute.
        if ticks >= 120 {
            self.requests_ticks.set(0);
            let scroll = self.ui.global::<ui::View>().get_scroll_y();
            self.show(Route::Requests, scroll, true, None);
        } else {
            self.requests_ticks.set(ticks);
        }
    }

    // ---- bringing playlists over from Spotify ---------------------------------------------------

    /// Asks which exported playlists to bring in, then brings them in.
    pub fn start_import(self: &Rc<Self>) {
        if self.is_offline() {
            return self.toast("Importing needs your server, which can't be reached right now.");
        }
        if crate::search::current().is_none() {
            return self.toast("Still fetching your library. Try again in a moment.");
        }
        #[cfg(windows)]
        if let Some(paths) = crate::picker::pick_exports(&self.ui) {
            if !paths.is_empty() {
                self.run_import(paths);
            }
            return;
        }
        // Without the system's file window: ask for the place by name.
        self.prompt_import_path();
    }

    pub fn run_import(self: &Rc<Self>, paths: Vec<std::path::PathBuf>) {
        let (Some(client), Some(index)) = (self.client(), crate::search::current()) else { return };
        self.toast("Importing your playlists…");
        self.api_pool.spawn(move || {
            let result = crate::import::run(&client, &index, &paths);
            // The songs that weren't found, as a file beside the ones that were chosen.
            let mut saved = None;
            if let Ok(reports) = &result {
                if reports.iter().any(|report| !report.missing.is_empty()) {
                    let folder = paths.first().map(|p| if p.is_dir() { p.clone() } else { p.parent().map(|d| d.to_path_buf()).unwrap_or_default() });
                    if let Some(file) = folder.map(|folder| folder.join("Nexus Music - not found.txt")) {
                        if std::fs::write(&file, crate::import::missing_text(reports)).is_ok() {
                            saved = Some(file);
                        }
                    }
                }
            }
            post(move |ctl| ctl.import_finished(result, saved));
        });
    }

    fn import_finished(self: &Rc<Self>, result: Result<Vec<crate::import::ListReport>, String>, saved: Option<std::path::PathBuf>) {
        let reports = match result {
            Ok(reports) => reports,
            Err(reason) => return self.toast(reason),
        };
        // When each song was added on Spotify becomes its "Date added" here.
        let now = chrono::Utc::now().timestamp();
        for report in reports.iter().filter(|report| !report.dated.is_empty()) {
            self.added.borrow_mut().record_at(&report.playlist, &report.dated, !report.existed, now);
        }
        self.share_soon();
        self.invalidate("playlist:");
        self.reload_playlists();

        let (total, found): (usize, usize) =
            reports.iter().fold((0, 0), |(total, found), report| (total + report.total, found + report.found));
        let cards: Vec<CardData> = reports
            .iter()
            .map(|report| CardData {
                kind: Kind::Playlist,
                id: report.playlist.clone(),
                title: report.name.clone(),
                subtitle: match (report.found, report.existed) {
                    (0, _) => format!("None of its {} found", count(report.total as u32, "song")),
                    (_, true) => format!("{} of {} found, {} added", report.found, report.total, report.added),
                    (_, false) => format!("{} of {} found", report.found, count(report.total as u32, "song")),
                },
                cover: String::new(),
            })
            .collect();
        // The songs that weren't found, shown as rows (they can't be played: there's no file).
        let missing: Vec<Song> = reports
            .iter()
            .flat_map(|report| report.missing.iter())
            .map(|wanted| Song {
                title: wanted.title.clone(),
                artist: wanted.artists.join(", "),
                album: wanted.album.clone(),
                duration: wanted.seconds,
                ..Default::default()
            })
            .collect();
        // With requests switched on, the albums those songs are on can be asked for.
        *self.import_missing.borrow_mut() = reports.iter().flat_map(|report| report.missing.iter().cloned()).collect();
        *self.import_fills.borrow_mut() = reports
            .iter()
            .flat_map(|report| {
                report.missing.iter().map(|wanted| {
                    let fill = crate::asked::Fill {
                        playlist: report.playlist.clone(),
                        playlist_name: report.name.clone(),
                        title: wanted.title.clone(),
                        artists: wanted.artists.clone(),
                        seconds: wanted.seconds,
                        added: wanted.added,
                    };
                    (wanted.album.clone(), fill)
                })
            })
            .collect();
        let askable = crate::lidarr::current().is_some() && !self.missing_wants().is_empty();
        let page = PageData {
            action: if askable { "Request the missing albums".into() } else { String::new() },
            title: "Imported from Spotify".into(),
            lead: vec![pages::ShelfData {
                title: format!("{} of {} found in your library", found, count(total as u32, "song")),
                route: String::new(),
                wrap: true,
                cards,
            }],
            tracks_title: match &saved {
                Some(file) => format!(
                    "Not in your library ({}). Also saved as “{}” beside your export.",
                    missing.len(),
                    file.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_default()
                ),
                None => format!("Not in your library ({})", missing.len()),
            },
            empty: if missing.is_empty() { "Every song was found.".into() } else { String::new() },
            songs: missing,
            show_album: true,
            ..Default::default()
        };
        *self.import_report.borrow_mut() = Some(page);
        if self.current_route() == Some(Route::Import) {
            self.show(Route::Import, 0.0, true, None);
        } else {
            self.navigate(Route::Import);
        }
    }

    // ---- search --------------------------------------------------------------------------------

    /// Readies the app's own copy of the library, which search runs on: from disk at first,
    /// then from the server whenever that says the library has changed.
    fn load_library(self: &Rc<Self>) {
        let (Some(client), Some(dir)) = (self.client(), self.pages_dir()) else { return };
        if self.library_busy.replace(true) {
            return;
        }
        // Offline, the copy on disk still serves; there's just no asking for a newer one.
        let online = !self.is_offline();
        self.api_pool.spawn(move || {
            let _ = std::fs::create_dir_all(&dir);
            let file = config::library_file(&dir);
            let mut renewed = false;
            if crate::search::current().is_none() {
                if let Some(library) = crate::search::Library::load(&file) {
                    crate::search::set_current(Some(crate::search::Index::new(library)));
                    renewed = true;
                }
            }
            let renew = || crate::search::renew(&client, crate::search::current().as_ref().map(|index| &index.library));
            if let Some(library) = online.then(renew).flatten() {
                library.save(&file);
                crate::search::set_current(Some(crate::search::Index::new(library)));
                renewed = true;
            }
            if renewed && online {
                // Which songs are liked, as of now.
                if let Ok(found) = client.starred() {
                    crate::search::set_liked(found.songs.into_iter().map(|song| song.id).collect());
                }
            }
            // Has anything that was asked for turned up?
            let now = chrono::Utc::now().timestamp();
            let arrived = crate::search::current().map(|index| crate::asked::arrivals(&index, &crate::asked::shared(), now)).unwrap_or_default();
            post(move |ctl| {
                ctl.library_busy.set(false);
                if !arrived.is_empty() {
                    ctl.requests_arrived(arrived);
                }
            });
        });
    }

    /// Looks up what's being typed in the search box and offers the best few matches.
    fn search_preview(self: &Rc<Self>, text: &str) {
        let query = text.trim().to_string();
        let count = self.preview_count.get() + 1;
        self.preview_count.set(count);
        self.preview_latest.store(count, std::sync::atomic::Ordering::Relaxed);
        if query.is_empty() {
            return self.close_preview();
        }
        // A search beyond the library is likely to follow: have the connection ready for it.
        if crate::lidarr::current().is_some() {
            crate::musicbrainz::warm();
        }
        let Some(client) = self.client() else { return };
        let show = move |found: Option<crate::search::Results>| {
            post(move |ctl| {
                if let (Some(found), true) = (found, ctl.preview_count.get() == count) {
                    ctl.show_preview(found);
                }
            });
        };
        match crate::search::current() {
            // The library is on this PC: search it on a thread kept for that, newest first,
            // passing over any search that more typing has already replaced.
            Some(index) => {
                let latest = self.preview_latest.clone();
                self.quick_pool.spawn(move || {
                    if latest.load(std::sync::atomic::Ordering::Relaxed) == count {
                        show(Some(index.search(&query, &crate::search::is_liked, 2, 2, 5)));
                    }
                });
            }
            // No copy of the library yet: the server's exact search will have to do.
            None => self.api_pool.spawn(move || {
                show(client.search(&query, 1, 2, 5).ok().map(|found| crate::search::Results {
                    artists: found.artists.iter().map(CardData::artist).collect(),
                    albums: found.albums.iter().map(CardData::album).collect(),
                    songs: found.songs,
                    artist_first: false,
                }))
            }),
        }
    }

    fn show_preview(&self, found: crate::search::Results) {
        let artists = found.artists.into_iter().map(Suggestion::Card);
        let albums = found.albums.into_iter().map(Suggestion::Card);
        let songs = found.songs.into_iter().map(Suggestion::Song);
        // An artist who matches what was typed leads; otherwise the songs do.
        let list: Vec<Suggestion> = if found.artist_first {
            artists.chain(songs).chain(albums).collect()
        } else {
            songs.chain(artists.take(1)).chain(albums).collect()
        };
        let rows: Vec<ui::Suggestion> = list
            .iter()
            .map(|suggestion| match suggestion {
                Suggestion::Card(card) => ui::Suggestion {
                    kind: if card.kind == Kind::Artist { 0 } else { 1 },
                    title: card.title.as_str().into(),
                    subtitle: match (card.kind, card.subtitle.is_empty()) {
                        (Kind::Artist, _) => "Artist".into(),
                        (_, true) => "Album".into(),
                        (_, false) => format!("Album • {}", card.subtitle).into(),
                    },
                    cover: card.cover.as_str().into(),
                    liked: false,
                },
                Suggestion::Song(song) => ui::Suggestion {
                    kind: 2,
                    title: song.title.as_str().into(),
                    subtitle: format!("Song • {}", song.artist_label()).into(),
                    cover: song.cover_art.as_str().into(),
                    liked: self.stars.borrow().get(&song.id).copied().unwrap_or_else(|| crate::search::is_liked(song)),
                },
            })
            .collect();
        let search = self.ui.global::<ui::Search>();
        search.set_highlight(-1);
        search.set_open(!rows.is_empty());
        search.set_items(ModelRc::new(VecModel::from(rows)));
        *self.suggestions.borrow_mut() = list;
    }

    fn close_preview(&self) {
        let search = self.ui.global::<ui::Search>();
        search.set_open(false);
        search.set_highlight(-1);
    }

    /// A row of the preview was chosen: go to the artist or album, or play the song.
    fn open_suggestion(self: &Rc<Self>, index: usize) {
        let Some(suggestion) = self.suggestions.borrow().get(index).cloned() else { return };
        self.close_preview();
        // The keyboard goes back to the player's shortcuts.
        let app = self.ui.global::<ui::App>();
        app.set_main_focus(app.get_main_focus().wrapping_add(1));
        let query = app.get_search_text();
        if !query.trim().is_empty() {
            crate::journal::begin_search(&query, false);
            crate::journal::search_results(
                crate::journal::search_ticket(&query), self.suggestions.borrow().len(), None,
            );
        }
        match suggestion {
            Suggestion::Card(card) => self.open_card(&card.to_ui()),
            // A song picked from a search starts its radio: the song, then ones like it.
            Suggestion::Song(song) => self.play_radio(song, &query),
        }
    }

    pub fn leave(self: &Rc<Self>, message: &str) {
        // Stopping empties the queue, and an empty queue isn't kept. Being turned away by the
        // server is no reason to lose it, so the saved copy stays for the next sign-in.
        // (Signing out by choice removes it first.)
        self.keep_saved_queue.set(true);
        self.stop_playback();
        self.keep_saved_queue.set(false);
        self.queue_saved.set(0);
        // What belongs to this account is on disk; none of it stays on screen or in hand.
        {
            let mut downloads = self.downloads.borrow_mut();
            downloads.save_if_due(true);
            let folder = downloads.folder().to_path_buf();
            *downloads = crate::downloads::Downloads::load(folder, "");
        }
        self.pending.borrow_mut().clear();
        self.sending.borrow_mut().clear();
        self.leave_jam();
        self.end_jam(false);
        self.stop_linking();
        // (The new linking ends with the sign-in, and a look still on its way with it.)
        self.link_stop();
        self.ui.global::<ui::Jam>().set_open(false);
        self.stop_sync();
        self.library_timer.stop();
        self.close_preview();
        crate::search::set_current(None);
        *self.client.borrow_mut() = None;
        self.cache.borrow_mut().clear();
        self.playlists.borrow_mut().clear();
        self.stars.borrow_mut().clear();
        let app = self.ui.global::<ui::App>();
        app.set_playlists(ModelRc::default());
        app.set_login_error(message.into());
        app.set_signed_in(false);
    }

    fn sign_out(self: &Rc<Self>) {
        let (server, username) = {
            let settings = self.settings.borrow();
            (settings.server.clone(), settings.username.clone())
        };
        config::delete_password(&self.paths, &server, &username);
        self.saver.remove(self.paths.queue.clone());
        self.saver.remove(self.paths.queue_place.clone());
        let _ = std::fs::remove_file(&self.paths.history);
        self.history.borrow_mut().clear();
        self.leave("");
        // Signing in again starts from whatever the server has.
        self.queue_stamp.set(0);
        self.settings.borrow_mut().sync_playlist.clear();
        self.save_settings();
    }

    // ---- navigation ---------------------------------------------------------------------------

    pub fn navigate(self: &Rc<Self>, route: Route) {
        self.navigate_with(route, None);
    }

    /// Navigates, drawing `hint` straight away if the page itself isn't cached yet.
    pub fn navigate_with(self: &Rc<Self>, route: Route, hint: Option<PageData>) {
        {
            let mut nav = self.nav.borrow_mut();
            let scroll = self.ui.global::<ui::View>().get_scroll_y();
            let at = nav.index;
            if let Some(current) = nav.history.get_mut(at) {
                if current.route == route {
                    return;
                }
                current.scroll = scroll;
                // Typing in the search box refines one history entry instead of adding many.
                let refining = matches!((&current.route, &route), (Route::Search(_), Route::Search(_)));
                if refining {
                    current.route = route.clone();
                    current.scroll = 0.0;
                } else {
                    nav.history.truncate(at + 1);
                    nav.history.push(HistoryEntry { route: route.clone(), scroll: 0.0 });
                    nav.index = nav.history.len() - 1;
                }
            } else {
                nav.history.push(HistoryEntry { route: route.clone(), scroll: 0.0 });
                nav.index = 0;
            }
        }
        self.show(route, 0.0, false, hint);
    }

    fn step(self: &Rc<Self>, delta: isize) {
        let (route, scroll) = {
            let mut nav = self.nav.borrow_mut();
            let target = nav.index as isize + delta;
            if target < 0 || target as usize >= nav.history.len() {
                return;
            }
            let scroll = self.ui.global::<ui::View>().get_scroll_y();
            let at = nav.index;
            nav.history[at].scroll = scroll;
            nav.index = target as usize;
            let entry = &nav.history[nav.index];
            (entry.route.clone(), entry.scroll)
        };
        self.show(route, scroll, false, None);
    }

    pub fn current_route(&self) -> Option<Route> {
        let nav = self.nav.borrow();
        nav.history.get(nav.index).map(|e| e.route.clone())
    }

    /// Reloads the page on screen from the server.
    pub fn refresh(self: &Rc<Self>) {
        if let Some(route) = self.current_route() {
            let scroll = self.ui.global::<ui::View>().get_scroll_y();
            self.show(route, scroll, true, None);
        }
        self.reload_playlists();
    }

    /// Drops cached copies of pages whose content an action just changed.
    pub fn invalidate(&self, prefix: &str) {
        self.cache.borrow_mut().retain(|key, _| !key.starts_with(prefix));
        let Some(dir) = self.pages_dir() else { return };
        let stem = file_stem(prefix);
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if entry.file_name().to_string_lossy().starts_with(&stem) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    // ---- the page cache ------------------------------------------------------------------------

    /// Folder for this server and account's cached pages.
    fn pages_dir(&self) -> Option<PathBuf> {
        let client = self.client()?;
        let who = format!("{}|{}", client.server(), client.username());
        Some(self.paths.pages.join(file_stem(&who)))
    }

    /// Where lyrics are kept for the server that's signed in to (see `lyrics`).
    pub fn lyrics_dir(&self) -> Option<PathBuf> {
        Some(config::lyrics_dir(&self.pages_dir()?))
    }

    /// A stored copy of a page and its age: from memory, or from disk on the first visit of
    /// a session.
    fn cached_page(&self, key: &str) -> Option<(Duration, PageData)> {
        if let Some((at, data)) = self.cache.borrow().get(key) {
            return Some((at.elapsed(), data.clone()));
        }
        let path = self.pages_dir()?.join(file_name(key));
        let (age, data) = read_page(&path)?;
        let stored = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
        self.remember(key, stored, data.clone());
        Some((age, data))
    }

    /// A long playlist's stored copy is megabytes of text, and reading it back takes long
    /// enough to notice. The first time one is wanted it's read away from this thread, and
    /// the page is shown as soon as it's in memory. Returns whether that's under way.
    fn read_big_page(self: &Rc<Self>, key: &str, route: &Route, scroll: f32, hint: Option<PageData>, generation: u64) -> bool {
        const BIG: u64 = 256 * 1024;
        if self.cache.borrow().contains_key(key) {
            return false;
        }
        let Some(path) = self.pages_dir().map(|dir| dir.join(file_name(key))) else { return false };
        if !std::fs::metadata(&path).is_ok_and(|meta| meta.len() > BIG) {
            return false;
        }
        let (key, route) = (key.to_string(), route.clone());
        self.quick_pool.spawn(move || {
            let read = read_page(&path);
            post(move |ctl| {
                match read {
                    Some((age, data)) => {
                        ctl.remember(&key, Instant::now().checked_sub(age).unwrap_or_else(Instant::now), data)
                    }
                    // Not a page any more: without it the next look goes to the server.
                    None => {
                        let _ = std::fs::remove_file(&path);
                    }
                }
                if ctl.nav.borrow().generation == generation {
                    ctl.show(route, scroll, false, hint);
                }
            });
        });
        true
    }

    pub fn remember(&self, key: &str, at: Instant, data: PageData) {
        let mut cache = self.cache.borrow_mut();
        if cache.len() >= MEMORY_PAGES {
            let mut ages: Vec<Instant> = cache.values().map(|(at, _)| *at).collect();
            ages.sort_unstable();
            let cutoff = ages[ages.len() / 2];
            cache.retain(|_, (at, _)| *at > cutoff);
        }
        cache.insert(key.to_string(), (at, data));
    }

    /// Fetches a page ahead of the click that would open it. Called when the pointer rests
    /// on something clickable.
    fn prefetch(self: &Rc<Self>, route: Route) {
        if !matches!(route, Route::Album(_) | Route::Artist(_) | Route::Playlist(_)) {
            return;
        }
        let key = route.cache_key();
        let (Some(client), Some(dir)) = (self.client(), self.pages_dir()) else { return };
        if self.cache.borrow().contains_key(&key)
            || dir.join(file_name(&key)).exists()
            || !self.prefetching.borrow_mut().insert(key.clone())
        {
            return;
        }
        self.api_pool.spawn(move || {
            let page = fetch_whole(&client, &route, &dir.join(file_name(&key)));
            post(move |ctl| {
                ctl.prefetching.borrow_mut().remove(&key);
                if let Ok(page) = page {
                    ctl.remember(&key, Instant::now(), page);
                }
            });
        });
    }

    pub(crate) fn show(self: &Rc<Self>, route: Route, scroll: f32, force: bool, hint: Option<PageData>) {
        let generation = {
            let mut nav = self.nav.borrow_mut();
            nav.generation += 1;
            nav.generation
        };
        let app = self.ui.global::<ui::App>();
        let (index, len) = {
            let nav = self.nav.borrow();
            (nav.index, nav.history.len())
        };
        app.set_can_go_back(index > 0);
        app.set_can_go_forward(index + 1 < len);
        app.set_route(route.key().into());
        app.set_page(route.page());
        if let Route::Search(query) = &route {
            if app.get_search_text() != query.as_str() {
                app.set_search_text(query.as_str().into());
            }
        } else {
            if !app.get_search_text().is_empty() {
                app.set_search_text("".into());
            }
            // Leaving search: give the keyboard back to the player shortcuts. (A refresh of
            // the page on screen isn't leaving anything, and mustn't interrupt typing.)
            if !force {
                app.set_main_focus(app.get_main_focus().wrapping_add(1));
            }
        }

        let view = self.ui.global::<ui::View>();
        match route {
            Route::Settings => {
                view.set_loading(false);
                view.set_error("".into());
                self.refresh_cache_size();
                // Show how the connection to Lidarr stands now, unless a sign-in is under way.
                if self.ui.global::<ui::Settings>().get_requests_state() != "waiting" {
                    self.check_requests(false);
                }
                self.check_jam();
                return;
            }
            Route::Lyrics | Route::NowPlaying => {
                view.set_loading(false);
                view.set_error("".into());
                self.load_lyrics();
                return;
            }
            Route::Jam => {
                view.set_loading(false);
                view.set_error("".into());
                return;
            }
            Route::Downloads => {
                self.show_downloads(scroll);
                return;
            }
            Route::Import => {
                let page = self.import_report.borrow().clone().unwrap_or_else(|| PageData {
                    title: "Import from Spotify".into(),
                    empty: "Nothing has been imported yet. Use the + beside Playlists in the sidebar.".into(),
                    ..Default::default()
                });
                self.apply_page(page, scroll);
                return;
            }
            _ => {}
        }

        let key = route.cache_key();
        if !force && self.read_big_page(&key, &route, scroll, hint.clone(), generation) {
            // Until it's read: what's already known of the page, or a blank panel.
            match hint {
                Some(hint) => self.apply_page(hint, 0.0),
                None => {
                    view.set_loading(true);
                    view.set_scroll_y(0.0);
                }
            }
            return;
        }
        let search_ticket = match &route {
            Route::Search(query) => crate::journal::search_ticket(query),
            _ => None,
        };
        let cached = if force { None } else { self.cached_page(&key) };
        // Something to draw immediately: a stored copy, or failing that the hint.
        let shown = match cached {
            Some((age, data)) => {
                note_search_results(search_ticket, &data);
                self.apply_page(data, scroll);
                if age < FRESH_FOR {
                    return;
                }
                true
            }
            None => {
                match hint {
                    Some(hint) if !force => self.apply_page(hint, 0.0),
                    // A refresh keeps what's on screen; a new page blanks the panel.
                    _ if force => {}
                    _ => {
                        view.set_loading(true);
                        view.set_scroll_y(0.0);
                    }
                }
                false
            }
        };
        view.set_error("".into());
        let (Some(client), Some(dir)) = (self.client(), self.pages_dir()) else {
            crate::journal::search_results(search_ticket, 0, None);
            return;
        };
        let file = dir.join(file_name(&key));
        self.api_pool.spawn(move || {
            let first = pages::load(&client, &route);
            let page = match first {
                Ok(page) => page,
                Err(e) => {
                    let network = matches!(e, crate::subsonic::ApiError::Network(_));
                    let offline_home = matches!(route, Route::Home) && network;
                    return post(move |ctl| {
                        crate::journal::search_results(search_ticket, 0, None);
                        if network && !ctl.settings.borrow().offline {
                            ctl.set_unreachable(true);
                        }
                        // A failed background check leaves the stored copy on screen.
                        if ctl.nav.borrow().generation == generation && !shown {
                            let view = ctl.ui.global::<ui::View>();
                            view.set_loading(false);
                            view.set_error(e.to_string().into());
                            // With the server out of reach, open on what can still be played.
                            if offline_home && !ctl.downloads.borrow().is_empty() {
                                diagnostics::music_connection_failed(&e);
                                ctl.navigate(Route::Downloads);
                                ctl.toast("Can't reach your server. Here are your downloads.");
                            }
                        }
                    });
                }
            };
            if !shown {
                // First paint as soon as the essentials are in; extras follow.
                let page = page.clone();
                let key = key.clone();
                post(move |ctl| {
                    ctl.remember(&key, Instant::now(), page.clone());
                    ctl.observe_playlist(&page);
                    ctl.source_list_now(&key, &page.songs);
                    if ctl.nav.borrow().generation == generation {
                        ctl.apply_page(page, scroll);
                    }
                });
            }
            // Extras that take a while are shown part by part as they come in.
            let mut publish = |part: PageData| {
                post(move |ctl| {
                    if ctl.nav.borrow().generation == generation {
                        ctl.apply_if_changed(part);
                    }
                });
            };
            let whole = pages::enrich(&client, &route, &page, &mut publish).unwrap_or(page);
            store_page(&file, &whole);
            post(move |ctl| {
                note_search_results(search_ticket, &whole);
                ctl.remember(&key, Instant::now(), whole.clone());
                ctl.observe_playlist(&whole);
                ctl.source_list_now(&key, &whole.songs);
                if ctl.nav.borrow().generation == generation {
                    ctl.apply_if_changed(whole);
                }
            });
        });
    }

    /// Narrows the search on screen to one kind of result (or back to all of them).
    fn set_search_filter(self: &Rc<Self>, filter: usize) {
        if self.search_filter.get() == filter {
            return;
        }
        self.search_filter.set(filter);
        let whole = self.search_whole.borrow().clone();
        if let Some(whole) = whole {
            self.apply_page(whole, 0.0);
        }
    }

    /// Replaces the page on screen with a newer copy, unless nothing differs.
    fn apply_if_changed(self: &Rc<Self>, data: PageData) {
        // A search on screen is a view of what was found: compare with that, not the view.
        if self.search_whole.borrow().as_ref() == Some(&data) {
            return;
        }
        {
            let current = self.page.borrow();
            if *current == data {
                return;
            }
            // The grid on screen has scrolled further than the first page just checked.
            let grown = current.grid.len() > data.grid.len() && current.grid.starts_with(&data.grid);
            if grown && !data.grid.is_empty() {
                return;
            }
        }
        let scroll = self.ui.global::<ui::View>().get_scroll_y();
        self.apply_page(data, scroll);
    }

    pub fn apply_page(self: &Rc<Self>, data: PageData, scroll: f32) {
        let view = self.ui.global::<ui::View>();
        // A search is shown through whichever of its chips is chosen; everything it found is
        // kept, so choosing another needs no new search.
        let data = match self.current_route() {
            Some(Route::Search(query)) if !query.is_empty() => {
                if *self.search_filter_query.borrow() != query {
                    self.search_filter.set(0);
                    self.search_filter_query.replace(query);
                }
                let shown = pages::search_view(&data, self.search_filter.get());
                self.search_whole.replace(Some(data));
                if view.get_kinds().row_count() == 0 {
                    view.set_kinds(ModelRc::new(VecModel::from(
                        pages::SEARCH_FILTERS.iter().map(|name| SharedString::from(*name)).collect::<Vec<_>>(),
                    )));
                }
                view.set_kind(self.search_filter.get() as i32);
                shown
            }
            Some(Route::Stats(period)) => {
                // The chips choose how far back the figures go.
                self.search_whole.replace(None);
                if view.get_kinds().row_count() != pages::STATS_PERIODS.len() {
                    view.set_kinds(ModelRc::new(VecModel::from(pages::STATS_PERIODS.iter().map(|name| SharedString::from(*name)).collect::<Vec<_>>())));
                }
                view.set_kind(period.min(pages::STATS_PERIODS.len() - 1) as i32);
                data
            }
            _ => {
                self.search_whole.replace(None);
                if view.get_kinds().row_count() > 0 {
                    view.set_kinds(ModelRc::default());
                }
                data
            }
        };
        // The finer choices under the chosen chip, where the page has any.
        let filters: Vec<SharedString> = data.filters.iter().map(|(name, _)| name.as_str().into()).collect();
        if filters.len() != view.get_sub_kinds().row_count() || filters.iter().zip(view.get_sub_kinds().iter()).any(|(a, b)| *a != b) {
            view.set_sub_kinds(ModelRc::new(VecModel::from(filters)));
        }
        view.set_sub_kind(data.filter as i32);
        view.set_stats(ModelRc::new(VecModel::from(
            data.stats.iter().map(|(value, label)| ui::Stat { value: value.as_str().into(), label: label.as_str().into() }).collect::<Vec<_>>(),
        )));
        view.set_tracks_route(data.tracks_route.as_str().into());
        view.set_offers_route(data.offers_route.as_str().into());
        view.set_label(data.label.as_str().into());
        view.set_title(data.title.as_str().into());
        view.set_subtitle(data.subtitle.as_str().into());
        view.set_subtitle_route(data.subtitle_route.as_str().into());
        view.set_meta(data.meta.as_str().into());
        view.set_about(data.about.as_str().into());
        view.set_cover(data.cover.as_str().into());
        view.set_cover_round(data.cover_round);
        view.set_liked_songs(data.liked_songs);
        view.set_can_play(data.can_play);
        view.set_has_menu(data.has_menu);
        view.set_notice(data.notice.as_str().into());
        view.set_notice_action(data.notice_action.as_str().into());
        view.set_notice_other(data.notice_other.as_str().into());
        view.set_pending(data.pending);
        view.set_can_star(data.star.is_some());
        view.set_starred(match &data.star {
            Some((_, id)) => self.stars.borrow().get(id).copied().unwrap_or(data.starred),
            None => false,
        });
        view.set_tracks_title(data.tracks_title.as_str().into());
        view.set_show_album(data.show_album);
        view.set_show_cover(data.show_cover);
        view.set_show_number(data.show_number);
        view.set_empty(data.empty.as_str().into());
        view.set_sorts(ModelRc::new(VecModel::from(
            data.sorts.iter().map(|s| SharedString::from(s.as_str())).collect::<Vec<_>>(),
        )));
        view.set_sort(data.sort as i32);

        // A different page starts with its list unfiltered and in its own order.
        let key = self.current_route().map(|route| route.cache_key()).unwrap_or_default();
        let same_view = self.track_view.borrow().key == key;
        if !same_view {
            // It opens in the order it was last left in. A playlist nobody has sorted yet
            // opens with the songs added most recently at the top.
            let (sort, descending) = self.list_sort(&key);
            *self.track_view.borrow_mut() = TrackView { key, sort, descending, ..Default::default() };
            view.set_filter("".into());
        }

        // Leave lists alone when their content didn't change, so a background refresh or the
        // arrival of a page's extras doesn't disturb what's being looked at.
        let (same_tracks, same_tiles, same_shelves, same_grid) = {
            let old = self.page.borrow();
            (
                same_view
                    && old.songs == data.songs
                    && old.album_numbers == data.album_numbers
                    && old.disc_sections == data.disc_sections
                    && self.tracks_model.row_count() == self.track_view.borrow().map.len(),
                old.tiles == data.tiles && !old.tiles.is_empty(),
                old.shelves == data.shelves && !old.shelves.is_empty(),
                old.grid == data.grid && !old.grid.is_empty(),
            )
        };
        if !same_tiles {
            view.set_tiles(cards_model(&data.tiles));
        }
        let shelves_model = |shelves: &[pages::ShelfData]| {
            ModelRc::new(VecModel::from(
                shelves
                    .iter()
                    .map(|shelf| ui::Shelf {
                        title: shelf.title.as_str().into(),
                        route: shelf.route.as_str().into(),
                        wrap: shelf.wrap,
                        cards: cards_model(&shelf.cards),
                    })
                    .collect::<Vec<_>>(),
            ))
        };
        if !same_shelves {
            view.set_shelves(shelves_model(&data.shelves));
        }
        view.set_action(data.action.as_str().into());
        view.set_offers_first(data.offers_first);
        view.set_offers_title(data.offers_title.as_str().into());
        view.set_offers_heading(data.offers_heading.as_str().into());
        view.set_offers_note(data.offers_note.as_str().into());
        if self.page.borrow().offers != data.offers || data.offers.is_empty() {
            view.set_offers(ModelRc::new(VecModel::from(
                data.offers
                    .iter()
                    .map(|offer| ui::Offer {
                        album: offer.album.as_str().into(),
                        album_title: offer.album_title.as_str().into(),
                        title: offer.title.as_str().into(),
                        artist: offer.artist.as_str().into(),
                        artist_id: offer.artist_id.as_str().into(),
                        artist_name: offer.artist_name.as_str().into(),
                        about: offer.about.as_str().into(),
                        cover: offer.cover.as_str().into(),
                        duration: if offer.seconds > 0 { clock(offer.seconds).into() } else { "".into() },
                        requested: offer.requested,
                        song: offer.song.as_str().into(),
                    })
                    .collect::<Vec<_>>(),
            )));
        }
        if self.page.borrow().tail != data.tail || data.tail.is_empty() {
            view.set_tail(shelves_model(&data.tail));
        }
        if self.page.borrow().lead != data.lead || data.lead.is_empty() {
            view.set_lead(shelves_model(&data.lead));
        }
        if !same_grid {
            self.grid_model.set_vec(grid_rows(&data.grid, self.grid_columns.get()));
        }
        self.grid_loading.set(false);
        *self.page.borrow_mut() = data;
        if !same_tracks {
            self.rebuild_tracks();
            self.top_up_page();
        } else {
            self.sync_track_view();
        }

        view.set_error("".into());
        view.set_loading(false);
        view.set_scroll_y(scroll);
    }

    /// Rebuilds the song rows from the page's songs, as filtered and sorted.
    pub fn rebuild_tracks(&self) {
        let rows = {
            let page = self.page.borrow();
            let added = self.added_dates(&page);
            self.track_view.borrow_mut().arrange(&page.songs, &added);
            *self.page_added.borrow_mut() = added;
            self.track_rows(&page)
        };
        let sections = rows.last().map(|row| row.sections_before).unwrap_or(0);
        self.ui.global::<ui::View>().set_track_sections(sections);
        self.tracks_model.set_vec(rows);
        self.track_places.borrow_mut().clear();
        self.set_cursor_row(None, false);
        // The rows are new, so nothing is selected any more.
        self.selection.borrow_mut().clear();
        self.selection_anchor.set(None);
        self.ui.global::<ui::View>().set_selection_count(0);
        self.sync_track_view();
    }

    // ---- selecting songs -----------------------------------------------------------------------

    /// Replaces the selection, updating only the rows whose state changed.
    fn set_selection(&self, rows: std::collections::BTreeSet<usize>) {
        let old = self.selection.replace(rows.clone());
        for &row in old.symmetric_difference(&rows) {
            if let Some(mut track) = self.tracks_model.row_data(row) {
                track.selected = rows.contains(&row);
                self.tracks_model.set_row_data(row, track);
            }
        }
        self.ui.global::<ui::View>().set_selection_count(rows.len() as i32);
    }

    /// A row was clicked: plain selects it alone, Ctrl adds or removes it, Shift selects
    /// everything from the last row clicked to this one.
    fn track_clicked(&self, row: usize, ctrl: bool, shift: bool) {
        let mut rows = self.selection.borrow().clone();
        match (shift, self.selection_anchor.get()) {
            (true, Some(anchor)) => {
                if !ctrl {
                    rows.clear();
                }
                rows.extend(row.min(anchor)..=row.max(anchor));
            }
            _ => {
                if ctrl {
                    if !rows.remove(&row) {
                        rows.insert(row);
                    }
                } else {
                    rows = [row].into();
                }
                self.selection_anchor.set(Some(row));
            }
        }
        self.set_cursor_row(Some(row), false);
        self.set_selection(rows);
        // The keyboard follows the click: out of the search box, if it was there, so the
        // arrow keys carry on from this row.
        let app = self.ui.global::<ui::App>();
        app.set_main_focus(app.get_main_focus().wrapping_add(1));
    }

    pub fn clear_selection(&self) {
        self.selection_anchor.set(None);
        self.set_cursor_row(None, false);
        self.set_selection(Default::default());
    }

    /// Notes which row the keyboard is on, and has it scrolled into view if asked.
    fn set_cursor_row(&self, row: Option<usize>, reveal: bool) {
        self.selection_cursor.set(row);
        let view = self.ui.global::<ui::View>();
        view.set_cursor_row(row.map_or(-1, |row| row as i32));
        if reveal {
            view.set_reveal_tick(view.get_reveal_tick().wrapping_add(1));
        }
    }

    /// An arrow key: moves the keyboard's row by `delta` and selects it, or with Shift held
    /// selects everything from where the selection started to it.
    fn move_selection(&self, delta: i32, extend: bool) {
        let count = self.tracks_model.row_count();
        if count == 0 {
            return;
        }
        let row = match self.selection_cursor.get() {
            Some(at) => (at as i64 + delta as i64).clamp(0, count as i64 - 1) as usize,
            // Nothing chosen yet: start at the top going down, at the bottom going up.
            None if delta > 0 => 0,
            None => count - 1,
        };
        let rows = match (extend, self.selection_anchor.get()) {
            (true, Some(anchor)) => (row.min(anchor)..=row.max(anchor)).collect(),
            _ => {
                self.selection_anchor.set(Some(row));
                [row].into()
            }
        };
        self.set_cursor_row(Some(row), true);
        self.set_selection(rows);
    }

    /// Delete: takes the selected songs out of the playlist on screen, the same as the menu's
    /// "Remove from this playlist".
    fn remove_selection(self: &Rc<Self>) {
        let Some(row) = self.selection_cursor.get() else { return };
        if self.page.borrow().playlist.is_none() {
            return;
        }
        self.open_track_menu(row, 0.0, 0.0);
        self.act_on_menu("remove-playlist");
    }

    /// The rows an action on `row` applies to: the whole selection if `row` is part of one,
    /// otherwise that row alone (which becomes the selection).
    pub fn rows_for(&self, row: usize) -> Vec<usize> {
        let selected = self.selection.borrow().clone();
        if selected.len() > 1 && selected.contains(&row) {
            return selected.into_iter().collect();
        }
        self.selection_anchor.set(Some(row));
        self.set_selection([row].into());
        vec![row]
    }

    /// The songs on these rows, with each one's position in the page's own list.
    pub fn songs_on(&self, rows: &[usize]) -> Vec<(usize, Song)> {
        let page = self.page.borrow();
        rows.iter()
            .filter_map(|&row| self.song_index(row))
            .filter_map(|index| page.songs.get(index).map(|song| (index, song.clone())))
            .collect()
    }

    /// Rows were dragged onto a playlist in the sidebar or onto the queue panel.
    fn rows_dropped(self: &Rc<Self>, row: usize, target: &str) {
        let songs: Vec<Song> = self.songs_on(&self.rows_for(row)).into_iter().map(|(_, song)| song).collect();
        let songs: Vec<Song> = songs.into_iter().filter(|s| s.radio_url.is_empty() && !s.id.is_empty()).collect();
        if songs.is_empty() {
            return;
        }
        if target == "queue" {
            let count = songs.len();
            self.enqueue(songs, false);
            self.toast(if count == 1 { "Added to queue".to_string() } else { format!("{count} songs added to queue") });
        } else if let Some(id) = target.strip_prefix("playlist:") {
            self.add_to_playlist(id.to_string(), songs);
        }
    }

    /// Shows the state of the filter and sort controls.
    fn sync_track_view(&self) {
        let page = self.page.borrow();
        let state = self.track_view.borrow();
        let view = self.ui.global::<ui::View>();
        let can_filter = self.is_own_list(&page);
        view.set_can_filter(can_filter && !page.songs.is_empty());
        // The same column says how often each song was played, on a page that counts plays.
        view.set_show_added(can_filter || !page.song_notes.is_empty());
        view.set_track_sort_index(state.sort as i32);
        view.set_track_sort(
            match (state.sort, page.playlist.is_some()) {
                (0, false) => "Default order",
                (sort, _) => TRACK_SORTS[sort.min(TRACK_SORTS.len() - 1)],
            }
            .into(),
        );
        view.set_track_sort_custom(state.sort == 0);
        view.set_track_sort_descending(state.descending);
        view.set_can_reorder(page.playlist.is_some() && state.is_default() && !page.pending);
        if !state.filter.trim().is_empty() && state.map.is_empty() {
            view.set_empty(format!("No songs here match “{}”.", state.filter.trim()).into());
        } else {
            view.set_empty(page.empty.as_str().into());
        }
        drop((page, state));
        self.sync_download_button();
        self.sync_playing();
        self.sync_page_shuffle();
    }

    /// Playlists, Liked songs and Downloads: lists of the user's own making, which can be
    /// filtered, show when each song was added, and remember how they were sorted.
    fn is_own_list(&self, page: &PageData) -> bool {
        page.playlist.is_some() || page.liked_songs || matches!(self.current_route(), Some(Route::Downloads | Route::Songs))
    }

    /// When each of the page's songs was added to it; empty if the page has no such date.
    fn added_dates(&self, page: &PageData) -> Vec<i64> {
        use crate::added::parse_time;
        if let Some(playlist) = &page.playlist {
            self.playlist_dates(&playlist.id, &page.songs)
        } else if page.liked_songs {
            page.songs.iter().map(|song| song.starred.as_deref().map(parse_time).unwrap_or(0)).collect()
        } else if matches!(self.current_route(), Some(Route::Downloads)) {
            let downloads = self.downloads.borrow();
            page.songs.iter().map(|song| downloads.added_at(&song.id)).collect()
        } else if matches!(self.current_route(), Some(Route::Songs)) {
            // When each came into the library.
            page.songs.iter().map(|song| crate::added::day_start(parse_time(&song.created))).collect()
        } else {
            Vec::new()
        }
    }

    /// When each song of a playlist was added to it: the app's own note where it has one,
    /// otherwise the day the song joined the library, the nearest thing the server knows.
    fn playlist_dates(&self, id: &str, songs: &[Song]) -> Vec<i64> {
        use crate::added::{day_start, parse_time};
        let noted = self.added.borrow().dates(id, songs);
        songs
            .iter()
            .zip(noted)
            .map(|(song, at)| if at > 0 { at } else { day_start(parse_time(&song.created)) })
            .collect()
    }

    /// Notes which songs a playlist has, as just read from the server, so this PC can show
    /// when it first saw the ones that turn up later. (For its own display: what is only
    /// noticed is never dated for the other devices.)
    fn observe_playlist(&self, page: &PageData) {
        if let Some(playlist) = &page.playlist {
            self.added.borrow_mut().notice(&playlist.id, &page.songs, chrono::Utc::now().timestamp());
        }
    }

    /// The order a list opens in (an index into `TRACK_SORTS`, and whether it's reversed):
    /// the one it was last left in. A playlist nobody has sorted yet opens with the songs
    /// added most recently at the top.
    fn list_sort(&self, key: &str) -> (usize, bool) {
        let prefs = self.list_prefs(key);
        if !prefs.sort_chosen && prefs.sort == 0 && (key.starts_with("playlist:") || key == "liked") {
            return (SORT_ADDED, true);
        }
        let sort = prefs.sort.min(TRACK_SORTS.len() - 1);
        (sort, sort != 0 && prefs.descending)
    }

    /// The order the page for the list `key` shows these songs in, as positions in `songs`.
    pub fn order_for(&self, key: &str, songs: &[Song]) -> Vec<usize> {
        if let Some(id) = key.strip_prefix("playlist:") {
            return self.playlist_order(id, songs);
        }
        let (sort, descending) = self.list_sort(key);
        let added: Vec<i64> = if key == "liked" {
            songs.iter().map(|song| song.starred.as_deref().map(crate::added::parse_time).unwrap_or(0)).collect()
        } else {
            Vec::new()
        };
        let mut view = TrackView { sort, descending, ..Default::default() };
        view.arrange(songs, &added);
        view.map
    }

    /// The order a playlist's page shows these songs in, as positions in `songs`. Playing
    /// the playlist from elsewhere (the sidebar, a menu) follows the same order.
    pub fn playlist_order(&self, id: &str, songs: &[Song]) -> Vec<usize> {
        self.added.borrow_mut().notice(id, songs, chrono::Utc::now().timestamp());
        let (sort, descending) = self.list_sort(&format!("playlist:{id}"));
        let mut view = TrackView { sort, descending, ..Default::default() };
        view.arrange(songs, &self.playlist_dates(id, songs));
        view.map
    }

    // ---- what's remembered about each list -----------------------------------------------------

    pub fn list_prefs(&self, key: &str) -> crate::config::ListPrefs {
        self.settings.borrow().lists.get(key).copied().unwrap_or_default()
    }

    /// The person chose whether the list `key` plays shuffled. (Kept as the change, and
    /// given its time when it is shared: see `sync::Controller::choose`.)
    pub fn choose_shuffle(&self, key: &str, on: bool) {
        self.choose_shuffle_saving(key, on, crate::config::Durable::Flushed);
    }

    /// `choose_shuffle`, for a choice that came from afar too: `InPassing` doesn't wait for
    /// the disk.
    pub fn choose_shuffle_saving(&self, key: &str, on: bool, durable: crate::config::Durable) {
        if !key.is_empty() {
            self.choose_saving(nexus_core::step::Change::Shuffle { key: key.to_string(), on }, durable);
        }
    }

    /// Shows the list on screen in this order, and remembers it for the next visit. The
    /// choice is saved first, and the list is drawn from it only once it has been: a sort
    /// that couldn't be saved leaves the list as it was, not in an order the settings
    /// don't have.
    fn sort_tracks(&self, sort: usize, descending: bool) {
        let key = self.track_view.borrow().key.clone();
        let own = self.is_own_list(&self.page.borrow());
        if own && !key.is_empty() && !self.choose(nexus_core::step::Change::Sort { key, sort, descending }) {
            return;
        }
        {
            let mut state = self.track_view.borrow_mut();
            state.sort = sort;
            state.descending = descending;
        }
        self.rebuild_tracks();
    }

    /// The choices for the list that is open have changed without the person choosing here
    /// (another device's sort has arrived): the list is drawn again in the order it now has.
    pub fn resort_page(&self) {
        let key = self.track_view.borrow().key.clone();
        if key.is_empty() || !self.is_own_list(&self.page.borrow()) {
            return;
        }
        let (sort, descending) = self.list_sort(&key);
        let same = {
            let state = self.track_view.borrow();
            (state.sort, state.descending) == (sort, descending)
        };
        if !same {
            {
                let mut state = self.track_view.borrow_mut();
                state.sort = sort;
                state.descending = descending;
            }
            self.rebuild_tracks();
        }
    }

    /// The page's songs in the order they're shown.
    pub fn visible_songs(&self) -> Vec<Song> {
        let page = self.page.borrow();
        let state = self.track_view.borrow();
        state.map.iter().filter_map(|&i| page.songs.get(i).cloned()).collect()
    }

    /// The position in the page's own list of the song shown on `row`.
    pub fn song_index(&self, row: usize) -> Option<usize> {
        self.track_view.borrow().map.get(row).copied()
    }

    fn set_track_filter(&self, text: &str) {
        if self.track_view.borrow().filter == text {
            return;
        }
        self.track_view.borrow_mut().filter = text.to_string();
        self.rebuild_tracks();
        self.ui.global::<ui::View>().set_scroll_y(0.0);
    }

    /// Picking the order already in use flips its direction.
    pub fn set_track_sort(&self, sort: usize) {
        let sort = sort.min(TRACK_SORTS.len() - 1);
        let descending = {
            let state = self.track_view.borrow();
            match (sort, state.sort == sort) {
                (0, _) => false,
                (_, true) => !state.descending,
                (_, false) => TrackView::starts_descending(sort),
            }
        };
        self.sort_tracks(sort, descending);
    }

    /// A column heading was clicked: sort by it, then the other way, then back to the
    /// list's own order.
    fn cycle_track_sort(&self, column: usize) {
        if column == 0 || column >= TRACK_SORTS.len() {
            return;
        }
        let (sort, descending) = {
            let state = self.track_view.borrow();
            let first = TrackView::starts_descending(column);
            if state.sort != column {
                (column, first)
            } else if state.descending == first {
                (column, !first)
            } else {
                (0, false)
            }
        };
        self.sort_tracks(sort, descending);
    }

    /// Moves a playlist's song from one row to another and saves the new order.
    fn move_playlist_track(self: &Rc<Self>, from: usize, to: usize) {
        if !self.track_view.borrow().is_default() {
            return;
        }
        let (id, before) = {
            let mut page = self.page.borrow_mut();
            let Some(id) = page.playlist.as_ref().map(|p| p.id.clone()) else { return };
            if from == to || from >= page.songs.len() || to >= page.songs.len() {
                return;
            }
            let before = page.songs.clone();
            let song = page.songs.remove(from);
            page.songs.insert(to, song);
            (id, before)
        };
        let ids: Vec<String> = before.iter().map(|song| song.id.clone()).collect();
        let edit = crate::playlists::moving(&ids, from, to);
        *self.undo.borrow_mut() = Some((Instant::now(), Undo::Moved { playlist: id.clone(), before, from, to }));
        // If the queue is playing from this playlist, it follows the new order.
        let songs = self.page.borrow().songs.clone();
        self.source_list_now(&format!("playlist:{id}"), &songs);
        self.rebuild_tracks();
        if let Some(route) = self.current_route() {
            let data = self.page.borrow().clone();
            self.remember(&route.cache_key(), Instant::now(), data);
        }
        self.toast_with_undo("Moved");
        self.edit_playlist(id, ids, edit);
    }

    /// Builds the interface rows for the page's songs, in the order the view has arranged.
    fn track_rows(&self, data: &PageData) -> Vec<ui::Track> {
        let state = self.track_view.borrow();
        // Disc headings only make sense while the album is in its own order.
        let multi_disc = data.disc_sections && state.sort == 0 && data.songs.iter().any(|s| s.disc_number > 1);
        let mut last_disc = 0;
        let mut sections = 0;
        let added = self.page_added.borrow();
        state
            .map
            .iter()
            .filter_map(|&i| data.songs.get(i).map(|song| (i, song)))
            .enumerate()
            .map(|(index, (i, song))| {
                let mut row = self.track_row(song);
                row.added = match data.song_notes.get(i) {
                    Some(note) => note.as_str().into(),
                    None => crate::added::date_label(added.get(i).copied().unwrap_or(0)).into(),
                };
                row.number = if data.album_numbers && song.track > 0 {
                    song.track.to_string().into()
                } else {
                    (index + 1).to_string().into()
                };
                if multi_disc && song.disc_number != last_disc {
                    last_disc = song.disc_number;
                    row.section = format!("Disc {}", song.disc_number.max(1)).into();
                    sections += 1;
                }
                row.sections_before = sections;
                row
            })
            .collect()
    }

    pub fn track_row(&self, song: &Song) -> ui::Track {
        ui::Track {
            id: song.id.as_str().into(),
            title: song.title.as_str().into(),
            artist: song.artist_label().into(),
            artist_id: song.artist_id.as_str().into(),
            album: song.album.as_str().into(),
            album_id: song.album_id.as_str().into(),
            cover: song.cover_art.as_str().into(),
            // A radio station has no length.
            duration: if song.duration == 0 { SharedString::default() } else { clock(song.duration).into() },
            added: SharedString::default(),
            number: SharedString::default(),
            starred: self.is_starred(song),
            section: SharedString::default(),
            sections_before: 0,
            playing: false,
            progress: self.downloads.borrow().progress_of(&song.id),
            by: SharedString::default(),
            selected: false,
            unavailable: self.is_offline() && !self.plays_offline(song),
            downloaded: self.downloads.borrow().has(&song.id),
        }
    }

    pub fn is_starred(&self, song: &Song) -> bool {
        self.stars.borrow().get(&song.id).copied().unwrap_or(song.starred.is_some())
    }

    // ---- library grid --------------------------------------------------------------------------

    fn set_columns(&self, columns: usize) {
        let columns = columns.max(1);
        if self.grid_columns.replace(columns) != columns {
            let rows = grid_rows(&self.page.borrow().grid, columns);
            self.grid_model.set_vec(rows);
        }
    }

    /// Fetches the next page of a long album list when the grid is scrolled near its end.
    fn load_more(self: &Rc<Self>) {
        let (list, offset) = {
            let page = self.page.borrow();
            match &page.more {
                Some(list) => (list.clone(), page.grid.len() as u32),
                None => return,
            }
        };
        if self.grid_loading.replace(true) {
            return;
        }
        let generation = self.nav.borrow().generation;
        self.request(
            move |client| client.album_list(&list, pages::GRID_PAGE, offset),
            move |ctl, result| {
                ctl.grid_loading.set(false);
                if ctl.nav.borrow().generation != generation {
                    return;
                }
                let Ok(albums) = result else { return };
                let mut page = ctl.page.borrow_mut();
                if (albums.len() as u32) < pages::GRID_PAGE {
                    page.more = None;
                }
                page.grid.extend(albums.iter().map(CardData::album));
                let rows = grid_rows(&page.grid, ctl.grid_columns.get());
                drop(page);
                // Rewrite only the rows that changed so the list keeps its place.
                let existing = ctl.grid_model.row_count();
                for (index, row) in rows.into_iter().enumerate().skip(existing.saturating_sub(1)) {
                    if index < existing {
                        ctl.grid_model.set_row_data(index, row);
                    } else {
                        ctl.grid_model.push(row);
                    }
                }
                // Keep the cached copy in step, so coming back shows everything loaded so far.
                if let Some(route) = ctl.current_route() {
                    let data = ctl.page.borrow().clone();
                    ctl.remember(&route.cache_key(), Instant::now(), data);
                }
            },
        );
    }

    // ---- sidebar -------------------------------------------------------------------------------

    pub fn reload_playlists(self: &Rc<Self>) {
        self.request(
            |client| client.playlists(),
            |ctl, result| {
                if let Ok(playlists) = result {
                    ctl.playlists_arrived(playlists);
                }
            },
        );
    }

    /// The list of playlists has been read from the server, by [`Self::reload_playlists`]
    /// or by a look at what other devices have changed (which reads it anyway).
    pub fn playlists_arrived(self: &Rc<Self>, mut playlists: Vec<Playlist>) {
        // The playlist the app keeps its shared data in isn't one to listen to.
        playlists.retain(|p| !crate::link::unlisted(&p.name));
        *self.playlists.borrow_mut() = playlists;
        self.show_playlists();
        self.reload_changed_playlist();
        self.follow_playing_playlist();
    }

    /// If the playlist the queue is playing from has gained or lost songs (added from another
    /// page, or on another device), fetches it so the queue can follow.
    fn follow_playing_playlist(self: &Rc<Self>) {
        let (key, known) = {
            let pb = self.playback.borrow();
            (pb.source.clone(), pb.source_list.len())
        };
        let Some(id) = key.strip_prefix("playlist:").map(str::to_string) else { return };
        // The page on screen looks after itself (see `reload_changed_playlist`).
        if known == 0 || self.current_route() == Some(Route::Playlist(id.clone())) {
            return;
        }
        let listed = self.playlists.borrow().iter().find(|p| p.id == id).map(|p| p.song_count as usize);
        if listed.is_some_and(|count| count != known) {
            self.request(
                move |client| client.playlist(&id),
                move |ctl, result| {
                    if let Ok(playlist) = result {
                        ctl.source_list_now(&key, &playlist.songs);
                    }
                },
            );
        }
    }

    /// If the playlist on screen no longer has as many songs as the server says (they were
    /// added or removed on another device), fetches it again.
    fn reload_changed_playlist(self: &Rc<Self>) {
        let Some(Route::Playlist(id)) = self.current_route() else { return };
        let stale = {
            let page = self.page.borrow();
            let shown = page.playlist.as_ref().is_some_and(|p| p.id == id) && !page.pending;
            let listed = self.playlists.borrow().iter().find(|p| p.id == id).map(|p| p.song_count as usize);
            shown && listed.is_some_and(|count| count != page.songs.len())
        };
        if stale {
            let scroll = self.ui.global::<ui::View>().get_scroll_y();
            self.show(Route::Playlist(id), scroll, true, None);
        }
    }

    /// Lists the playlists in the sidebar: pinned ones first, in the order they were pinned,
    /// then the rest by name.
    pub fn show_playlists(&self) {
        let pinned = self.settings.borrow().pinned_playlists.clone();
        let place = |id: &str| pinned.iter().position(|p| p == id);
        let mut playlists = self.playlists.borrow_mut();
        playlists.sort_by_key(|p| (place(&p.id).unwrap_or(usize::MAX), p.name.to_lowercase()));
        let rows: Vec<ui::PlaylistRef> = playlists
            .iter()
            .map(|p| ui::PlaylistRef {
                id: p.id.as_str().into(),
                name: p.name.as_str().into(),
                subtitle: count(p.song_count, "song").into(),
                cover: p.cover_art.as_str().into(),
                pinned: place(&p.id).is_some(),
            })
            .collect();
        // A search looks through them too.
        crate::search::set_playlists(
            playlists
                .iter()
                .map(|p| CardData {
                    kind: Kind::Playlist,
                    id: p.id.clone(),
                    title: p.name.clone(),
                    subtitle: count(p.song_count, "song"),
                    cover: p.cover_art.clone(),
                })
                .collect(),
        );
        // Left alone when nothing differs, so a routine check doesn't disturb the sidebar.
        let app = self.ui.global::<ui::App>();
        if app.get_playlists().iter().ne(rows.iter().cloned()) {
            app.set_playlists(ModelRc::new(VecModel::from(rows)));
        }
    }

    // ---- settings ------------------------------------------------------------------------------

    /// Copies the saved settings into the Settings page.
    pub fn push_settings(&self) {
        let settings = self.settings.borrow();
        let page = self.ui.global::<ui::Settings>();
        let strings = |items: Vec<&str>| {
            ModelRc::new(VecModel::from(items.into_iter().map(SharedString::from).collect::<Vec<_>>()))
        };
        page.set_qualities(ModelRc::new(VecModel::from(
            QUALITIES
                .iter()
                .map(|(title, detail, _)| ui::QualityOption { title: (*title).into(), detail: (*detail).into() })
                .collect::<Vec<_>>(),
        )));
        page.set_quality_names(strings(QUALITIES.iter().map(|q| q.0).collect()));
        let quality = QUALITIES.iter().position(|q| q.2 == settings.bitrate).unwrap_or(QUALITIES.len() - 1);
        page.set_quality(quality as i32);
        page.set_crossfade(settings.crossfade);
        page.set_gapless(settings.gapless);
        page.set_autoplay(settings.autoplay);
        page.set_normalize(settings.normalize);
        page.set_volume_levels(strings(VOLUME_LEVELS.iter().map(|l| l.0).collect()));
        page.set_volume_level(settings.volume_level as i32);
        page.set_level_modes(strings(config::LEVEL_MODES.to_vec()));
        page.set_level_mode(settings.level_mode.min(2) as i32);
        page.set_songs_folder(if settings.songs_dir.is_empty() { self.paths.songs.display().to_string() } else { settings.songs_dir.clone() }.into());
        page.set_songs_folder_custom(!settings.songs_dir.is_empty());
        page.set_untagged_levels(strings(config::UNTAGGED_LEVELS.iter().map(|level| level.0).collect()));
        page.set_untagged_level(config::UNTAGGED_LEVELS.iter().position(|level| (level.1 - settings.untagged_db).abs() < 0.1).unwrap_or(0) as i32);
        page.set_preamps(strings(config::PREAMPS.iter().map(|preamp| preamp.0).collect()));
        page.set_preamp(config::PREAMPS.iter().position(|preamp| (preamp.1 - settings.eq_preamp).abs() < 0.1).unwrap_or(1) as i32);
        page.set_mono(settings.mono);
        page.set_scrobble(settings.scrobble);
        page.set_equalizer(settings.equalizer);
        page.set_eq_gains(ModelRc::new(VecModel::from(settings.eq_gains.to_vec())));
        page.set_eq_labels(ModelRc::new(VecModel::from(
            EQ_BANDS
                .iter()
                .map(|hz| {
                    SharedString::from(if *hz >= 1000.0 {
                        format!("{} kHz", hz / 1000.0)
                    } else {
                        format!("{hz} Hz")
                    })
                })
                .collect::<Vec<_>>(),
        )));
        page.set_eq_preset(settings.eq_preset().into());
        page.set_output_device(
            if settings.output_device.is_empty() { "System default" } else { settings.output_device_name.as_str() }
                .into(),
        );
        page.set_exclusive(settings.exclusive);
        page.set_standard_title_bar(settings.standard_title_bar);
        page.set_themes(ModelRc::new(VecModel::from(
            config::THEMES
                .iter()
                .map(|t| ui::ThemeOption {
                    name: t.name.into(),
                    accent: slint::Color::from_argb_encoded(0xff00_0000 | t.accent_bright),
                    surface: slint::Color::from_argb_encoded(0xff00_0000 | t.panel),
                })
                .collect::<Vec<_>>(),
        )));
        page.set_theme(config::THEMES.iter().position(|t| t.name == settings.theme).unwrap_or(0) as i32);
        page.set_ui_scales(strings(config::UI_SCALES.iter().map(|s| s.0).collect()));
        page.set_cache_limits(strings(config::CACHE_LIMITS.iter().map(|limit| limit.0).collect()));
        page.set_cache_aheads(strings(config::CACHE_AHEAD_NAMES.to_vec()));
        page.set_cache_limits(strings(config::CACHE_LIMITS.iter().map(|limit| limit.0).collect()));
        page.set_cache_aheads(strings(config::CACHE_AHEAD.iter().map(|songs| if *songs == 0 { "Off" } else { ["", "1", "", "3", "", "5", "", "", "", "", "10"][*songs as usize] }).collect()));
        page.set_ui_scale(
            config::UI_SCALES.iter().position(|s| (s.1 - settings.ui_scale).abs() < 0.01).unwrap_or(1) as i32,
        );
        page.set_download_quality(
            QUALITIES.iter().position(|q| q.2 == settings.download_quality).unwrap_or(QUALITIES.len() - 1) as i32,
        );
        page.set_close_to_tray(settings.close_to_tray);
        page.set_check_updates(settings.check_updates);
        page.set_sync(settings.sync);
        page.set_sync_note(self.sync_note.borrow().as_str().into());
        page.set_cache_limit(config::CACHE_LIMITS.iter().position(|limit| limit.1 == settings.cache_limit_mb).unwrap_or(0) as i32);
        page.set_cache_ahead(config::CACHE_AHEAD.iter().position(|songs| *songs == settings.cache_ahead).unwrap_or(0) as i32);
        page.set_requests(settings.requests);
        page.set_request_singles(settings.request_singles);
        page.set_lidarr_address(settings.lidarr_address.as_str().into());
        page.set_lidarr_key_hint(crate::keep::key(&*self.vault(), &settings.lidarr_address, self.key_old_for.borrow().as_deref()).map(|key| crate::lidarr::mask(&key)).unwrap_or_default().into());
        let gate = crate::gate::Found { service: settings.lidarr_gate.clone(), client: String::new() };
        page.set_lidarr_gate(gate.host().into());
        page.set_lidarr_signed_in(settings.lidarr_signed_in.as_str().into());
        // "Signed in" only at a service that's believed (see `Found::trusted`); a token kept for
        // one that's refused is offered only to be signed out of, which sends it nowhere.
        let trusted = gate.trusted(&settings.lidarr_address).is_ok();
        let vault = self.vault();
        let usable = !settings.lidarr_gate.is_empty() && trusted && crate::keep::token(&*vault, &settings.lidarr_gate, self.token_old_for.borrow().as_deref()).is_some();
        page.set_lidarr_has_token(usable);
        page.set_lidarr_token_unused(!settings.lidarr_gate.is_empty() && !usable && crate::keep::any_kept(&*vault));
        drop(settings);
        self.push_jam();
    }

    /// Changes a setting, shows it, applies it to the audio and, when `save` is set, writes
    /// it to disk. Dragging a slider passes `save: false` until the drag ends.
    /// A slider's: saved, when its drag ends, in passing (a continuous control is no press).
    fn change_setting_slid(&self, save: bool, change: impl FnOnce(&mut Settings)) {
        self.change_setting(false, change);
        if save {
            self.save_settings_in_passing();
        }
    }

    fn change_setting(&self, save: bool, change: impl FnOnce(&mut Settings)) {
        change(&mut self.settings.borrow_mut());
        self.push_settings();
        self.apply_audio_settings();
        if save {
            self.save_settings();
        }
    }

    /// Plays through the device at `index` in the last device menu; `None` is the system default.
    pub fn choose_device(&self, index: Option<usize>) {
        let chosen = index.and_then(|i| self.devices.borrow().get(i).cloned()).unwrap_or_default();
        self.change_setting(true, |s| {
            s.output_device = chosen.0;
            s.output_device_name = chosen.1;
        });
    }

    fn refresh_cache_size(&self) {
        let (dir, pages) = (self.covers.dir().to_path_buf(), self.paths.pages.clone());
        let songs = self.song_cache.clone();
        self.api_pool.spawn(move || {
            let kept = songs.size() as f64 / (1024.0 * 1024.0);
            let kept = if kept >= 1024.0 { format!("{:.1} GB", kept / 1024.0) } else { format!("{kept:.0} MB") };
            post(move |ctl| ctl.ui.global::<ui::Settings>().set_kept_songs(kept.into()));
            let page_bytes: u64 = std::fs::read_dir(&pages)
                .into_iter()
                .flatten()
                .flatten()
                .map(|server| config::dir_size(&server.path()))
                .sum();
            let megabytes = (config::dir_size(&dir) + page_bytes) as f64 / (1024.0 * 1024.0);
            let text = if megabytes < 1.0 { "Under 1 MB".to_string() } else { format!("{megabytes:.0} MB") };
            post(move |ctl| ctl.ui.global::<ui::Settings>().set_cache_size(text.into()));
        });
        // How quick the server is to reach, which is what most waiting comes down to.
        self.ui.global::<ui::Settings>().set_connection("".into());
        self.request(
            |client| client.measure(),
            |ctl, result| {
                if let Ok((cold, warm)) = result {
                    let text = format!(
                        "Connecting takes {cold} ms; after that each request takes about {warm} ms."
                    );
                    ctl.ui.global::<ui::Settings>().set_connection(text.into());
                }
            },
        );
    }

    // ---- callbacks from the interface ----------------------------------------------------------

    fn wire(self: &Rc<Self>) {
        let app = self.ui.global::<ui::App>();
        app.on_sign_in(|server, username, password| {
            with(|ctl| ctl.sign_in(server.into(), username.into(), password.into()));
        });
        app.on_set_queue_open(|open| {
            with(|ctl| {
                ctl.ui.global::<ui::App>().set_queue_open(open);
                ctl.settings.borrow_mut().queue_panel = open;
                ctl.save_settings();
            });
        });
        app.on_window_minimize(|| {
            with(|ctl| ctl.ui.window().set_minimized(true));
        });
        app.on_window_maximize(|| {
            with(|ctl| {
                let window = ctl.ui.window();
                window.set_maximized(!window.is_maximized());
            });
        });
        app.on_window_close(|| {
            // The same as closing it any other way: to the tray, or quit, as set.
            with(|ctl| ctl.ui.window().dispatch_event(slint::platform::WindowEvent::CloseRequested));
        });
        app.on_titlebar_layout(|height, nav_right, search_left, search_right, controls_left| {
            #[cfg(windows)]
            with(|ctl| {
                let scale = ctl.ui.window().scale_factor();
                crate::frame::set_layout(height, nav_right, search_left, search_right, controls_left, scale);
            });
            #[cfg(not(windows))]
            let _ = (height, nav_right, search_left, search_right, controls_left);
        });
        app.on_sign_out(|| {
            with(|ctl| ctl.sign_out());
        });
        app.on_navigate(|route| {
            with(|ctl| {
                // "Show all" on a search's list chooses that kind's chip.
                if let Some(filter) = route.strip_prefix("filter:").and_then(|filter| filter.parse::<usize>().ok()) {
                    return ctl.set_search_filter(filter);
                }
                if let Some(route) = Route::parse(&route) {
                    ctl.navigate(route);
                }
            });
        });
        app.on_go_back(|| {
            with(|ctl| ctl.step(-1));
        });
        app.on_go_forward(|| {
            with(|ctl| ctl.step(1));
        });
        app.on_refresh(|| {
            with(|ctl| ctl.refresh());
        });
        app.on_undo(|| {
            with(|ctl| ctl.undo_last());
        });
        self.ui.global::<ui::Toast>().on_acted(|| {
            with(|ctl| ctl.undo_last());
        });
        let search = self.ui.global::<ui::Search>();
        search.on_preview(|text| {
            with(|ctl| ctl.search_preview(&text));
        });
        search.on_menu(|index, x, y| {
            with(|ctl| {
                let Some(suggestion) = ctl.suggestions.borrow().get(index.max(0) as usize).cloned() else { return };
                match suggestion {
                    Suggestion::Card(card) => ctl.open_card_menu(&card.to_ui(), x, y),
                    Suggestion::Song(song) => ctl.open_song_menu(song, x, y),
                }
            });
        });
        search.on_activate(|index| {
            with(|ctl| ctl.open_suggestion(index.max(0) as usize));
        });
        app.on_search(|text| {
            with(|ctl| {
                ctl.close_preview();
                let query = text.trim().to_string();
                // Clearing the box while browsing elsewhere shouldn't yank you to Search.
                let on_search = matches!(ctl.current_route(), Some(Route::Search(_)));
                if !query.is_empty() {
                    if crate::journal::recording() {
                        crate::journal::begin_search(&query, crate::lidarr::current().is_some());
                    }
                    if matches!(ctl.current_route(), Some(Route::Search(ref current)) if *current == query) {
                        if let Some(data) = ctl.search_whole.borrow().as_ref() {
                            note_search_results(crate::journal::search_ticket(&query), data);
                        }
                    }
                }
                if !query.is_empty() || on_search {
                    ctl.navigate(Route::Search(query));
                }
            });
        });
        app.on_new_playlist(|| {
            with(|ctl| ctl.prompt_new_playlist(Vec::new()));
        });
        app.on_playlist_menu(|x, y| {
            with(|ctl| ctl.open_playlist_menu(x, y));
        });

        let view = self.ui.global::<ui::View>();
        view.on_play(|| {
            with(|ctl| ctl.play_page());
        });
        view.on_toggle_shuffle(|| {
            with(|ctl| ctl.toggle_page_shuffle());
        });
        view.on_sort_column(|column| {
            with(|ctl| ctl.cycle_track_sort(column.max(0) as usize));
        });
        view.on_toggle_star(|| {
            with(|ctl| ctl.toggle_page_star());
        });
        view.on_header_menu(|x, y| {
            with(|ctl| ctl.open_header_menu(x, y));
        });
        view.on_track_activated(|index| {
            with(|ctl| ctl.play_page_track(index as usize));
        });
        view.on_notice_acted(|| {
            with(|ctl| {
                match ctl.current_route() {
                    // Downloads that failed.
                    Some(Route::Downloads) => ctl.retry_downloads(),
                    // A batch of albums being asked for.
                    Some(Route::Requests) => ctl.bulk_action(false),
                    _ => {}
                }
            });
        });
        view.on_notice_other_acted(|| {
            with(|ctl| {
                if ctl.current_route() == Some(Route::Requests) {
                    ctl.bulk_action(true);
                }
            });
        });
        view.on_move_selection(|delta, extend| {
            with(|ctl| ctl.move_selection(delta, extend));
        });
        view.on_activate_selection(|| {
            with(|ctl| {
                if let Some(row) = ctl.selection_cursor.get() {
                    ctl.play_page_track(row);
                }
            });
        });
        view.on_remove_selection(|| {
            with(|ctl| ctl.remove_selection());
        });
        view.on_track_moved(|index, delta| {
            with(|ctl| ctl.move_playlist_track(index.max(0) as usize, (index + delta).max(0) as usize));
        });
        view.on_track_clicked(|index, ctrl, shift| {
            with(|ctl| ctl.track_clicked(index.max(0) as usize, ctrl, shift));
        });
        view.on_rows_dropped(|index, target| {
            with(|ctl| ctl.rows_dropped(index.max(0) as usize, &target));
        });
        view.on_select_all(|| {
            with(|ctl| {
                let all = (0..ctl.tracks_model.row_count()).collect();
                ctl.set_selection(all);
            });
        });
        view.on_clear_selection(|| {
            with(|ctl| ctl.clear_selection());
        });
        view.on_download(|| {
            with(|ctl| ctl.toggle_page_download());
        });
        view.on_set_filter(|text| {
            with(|ctl| ctl.set_track_filter(&text));
        });
        view.on_choose_track_sort(|x, y| {
            with(|ctl| ctl.open_track_sort_menu(x, y));
        });
        view.on_track_star(|index| {
            with(|ctl| {
                let song = ctl
                    .song_index(index as usize)
                    .and_then(|i| ctl.page.borrow().songs.get(i).cloned());
                if let Some(song) = song {
                    ctl.toggle_song_star(&song);
                }
            });
        });
        view.on_track_menu(|index, x, y| {
            with(|ctl| ctl.open_track_menu(index as usize, x, y));
        });
        view.on_card_activated(|card| {
            with(|ctl| ctl.open_card(&card));
        });
        view.on_card_hovered(|card| {
            with(|ctl| {
                if let Some(route) = card_route(&card) {
                    ctl.prefetch(route);
                }
            });
        });
        view.on_card_play(|card| {
            with(|ctl| ctl.play_card(&card));
        });
        view.on_card_menu(|card, x, y| {
            with(|ctl| ctl.open_card_menu(&card, x, y));
        });
        view.on_set_sort(|index| {
            with(|ctl| {
                if let Some(Route::Albums(_)) = ctl.current_route() {
                    // Changing the order replaces this page instead of stacking history.
                    {
                        let mut nav = ctl.nav.borrow_mut();
                        let at = nav.index;
                        nav.history[at].route = Route::Albums(index as usize);
                        nav.history[at].scroll = 0.0;
                    }
                    ctl.show(Route::Albums(index as usize), 0.0, false, None);
                }
            });
        });
        view.on_columns_changed(|columns| {
            with(|ctl| ctl.set_columns(columns as usize));
        });
        view.on_near_end(|| {
            with(|ctl| ctl.load_more());
        });

        let covers = self.ui.global::<ui::Covers>();
        covers.on_get(|id, size, _rev| {
            with(|ctl| ctl.covers.get(ctl, &id, size)).unwrap_or_default()
        });
        covers.on_tint(|id, _rev| with(|ctl| ctl.covers.tint(&id)).unwrap_or_default());

        let settings = self.ui.global::<ui::Settings>();
        settings.on_set_quality(|index| {
            with(|ctl| {
                let bitrate = QUALITIES.get(index as usize).map(|q| q.2).unwrap_or(0);
                if ctl.settings.borrow().bitrate != bitrate {
                    ctl.change_setting(true, |s| s.bitrate = bitrate);
                    // Switch the song that's playing over to the new quality straight away.
                    ctl.reload_current();
                }
            });
        });
        settings.on_set_crossfade(|seconds, save| {
            with(|ctl| ctl.change_setting_slid(save, |s| s.crossfade = seconds.round().clamp(0.0, 12.0)));
        });
        settings.on_set_gapless(|on| {
            with(|ctl| ctl.change_setting(true, |s| s.gapless = on));
        });
        settings.on_set_autoplay(|on| {
            with(|ctl| ctl.change_setting(true, |s| s.autoplay = on));
        });
        settings.on_set_normalize(|on| {
            with(|ctl| {
                ctl.change_setting(true, |s| s.normalize = on);
                ctl.reload_current();
            });
        });
        settings.on_set_volume_level(|level| {
            with(|ctl| {
                ctl.change_setting(true, |s| s.volume_level = level.clamp(0, 2) as u8);
                ctl.reload_current();
            });
        });
        settings.on_set_level_mode(|mode| {
            with(|ctl| {
                ctl.change_setting(true, |s| s.level_mode = mode.clamp(0, 2) as u8);
                ctl.reload_current();
            });
        });
        settings.on_set_untagged_level(|index| {
            with(|ctl| {
                let (_, db) = config::UNTAGGED_LEVELS[(index.max(0) as usize).min(config::UNTAGGED_LEVELS.len() - 1)];
                ctl.change_setting(true, |s| s.untagged_db = db);
                // How loud a song plays is fixed when it loads.
                ctl.reload_current();
            });
        });
        settings.on_set_preamp(|index| {
            with(|ctl| {
                let (_, db) = config::PREAMPS[(index.max(0) as usize).min(config::PREAMPS.len() - 1)];
                ctl.change_setting(true, |s| s.eq_preamp = db);
            });
        });
        settings.on_set_mono(|on| {
            with(|ctl| ctl.change_setting(true, |s| s.mono = on));
        });
        settings.on_set_scrobble(|on| {
            with(|ctl| ctl.change_setting(true, |s| s.scrobble = on));
        });
        settings.on_set_equalizer(|on| {
            with(|ctl| ctl.change_setting(true, |s| s.equalizer = on));
        });
        settings.on_set_eq_band(|band, gain, save| {
            with(|ctl| {
                ctl.change_setting_slid(save, |s| {
                    if let Some(slot) = s.eq_gains.get_mut(band as usize) {
                        // Half-decibel steps are finer than anyone can hear and keep values tidy.
                        *slot = ((gain * 2.0).round() / 2.0).clamp(-EQ_RANGE, EQ_RANGE);
                    }
                });
            });
        });
        settings.on_reset_eq(|| {
            with(|ctl| ctl.change_setting(true, |s| s.eq_gains = [0.0; 6]));
        });
        settings.on_choose_preset(|x, y| {
            with(|ctl| ctl.open_preset_menu(x, y));
        });
        settings.on_choose_device(|x, y| {
            with(|ctl| ctl.open_device_menu(x, y));
        });
        settings.on_set_exclusive(|on| {
            with(|ctl| ctl.change_setting(true, |s| s.exclusive = on));
        });
        settings.on_set_standard_title_bar(|on| {
            with(|ctl| {
                ctl.change_setting(true, |s| s.standard_title_bar = on);
                #[cfg(windows)]
                {
                    crate::frame::set_enabled(!on);
                    ctl.ui.global::<ui::App>().set_custom_frame(!on);
                }
            });
        });
        settings.on_set_theme(|index| {
            with(|ctl| {
                let Some(theme) = config::THEMES.get(index as usize) else { return };
                if ctl.choose(nexus_core::step::Change::Theme(theme.name.to_string())) {
                    ctl.push_settings();
                    ctl.apply_theme();
                }
            });
        });
        settings.on_set_ui_scale(|index| {
            with(|ctl| {
                let Some((_, scale)) = config::UI_SCALES.get(index as usize) else { return };
                if (ctl.settings.borrow().ui_scale - scale).abs() > 0.01 {
                    ctl.change_setting(true, |s| s.ui_scale = *scale);
                    ctl.toast("The new size takes effect the next time Nexus Music starts");
                }
            });
        });
        settings.on_set_close_to_tray(|on| {
            with(|ctl| {
                ctl.change_setting(true, |s| s.close_to_tray = on);
                ctl.sync_tray();
            });
        });
        settings.on_set_requests(|on| {
            with(|ctl| {
                ctl.change_setting(true, |s| s.requests = on);
                ctl.connect_requests();
                if on {
                    ctl.check_requests(false);
                }
            });
        });
        settings.on_set_request_singles(|on| {
            with(|ctl| {
                ctl.change_setting(true, |s| s.request_singles = on);
                crate::lidarr::set_singles(on);
                ctl.invalidate("search:");
                ctl.invalidate("outside-");
            });
        });
        settings.on_save_requests(|address, key| {
            with(|ctl| ctl.save_requests(&address, &key));
        });
        settings.on_check_requests(|| {
            with(|ctl| ctl.check_requests(false));
        });
        settings.on_sign_in_requests(|| {
            with(|ctl| ctl.sign_in_requests());
        });
        settings.on_cancel_sign_in(|| {
            with(|ctl| ctl.cancel_sign_in());
        });
        settings.on_sign_out_requests(|| {
            with(|ctl| ctl.sign_out_requests());
        });
        view.on_action_clicked(|| {
            with(|ctl| match ctl.current_route() {
                Some(Route::Import) => ctl.prompt_request_missing(),
                Some(Route::OutsideAlbum(album)) => {
                    let (title, by) = {
                        let page = ctl.page.borrow();
                        (page.title.clone(), page.subtitle.clone())
                    };
                    ctl.prompt_request(&album, &title, &by, "", "");
                }
                Some(Route::OutsideArtist(artist)) => ctl.prompt_add_artist(artist),
                _ => {}
            });
        });
        view.on_set_kind(|index| {
            with(|ctl| match ctl.current_route() {
                Some(Route::Stats(period)) => {
                    if period != index.max(0) as usize {
                        ctl.navigate(Route::Stats(index.max(0) as usize));
                    }
                }
                _ => ctl.set_search_filter(index.max(0) as usize),
            });
        });
        view.on_set_sub_kind(|index| {
            with(|ctl| {
                let chosen = ctl.page.borrow().filters.get(index.max(0) as usize).map(|(_, period)| *period);
                if let (Some(period), Some(Route::Stats(now))) = (chosen, ctl.current_route()) {
                    if period != now {
                        ctl.navigate(Route::Stats(period));
                    }
                }
            });
        });
        view.on_offer_activated(|index| {
            with(|ctl| {
                let Some(offer) = ctl.page.borrow().offers.get(index as usize).cloned() else { return };
                if !offer.song.is_empty() {
                    // A song suggested for a playlist: hear it.
                    return ctl.play_suggestion(&offer.song);
                }
                if offer.album.is_empty() {
                    // One that couldn't be asked for by name: look for it by hand.
                    return ctl.navigate(Route::Search(format!("{} {}", offer.title, offer.artist)));
                }
                let hint = pages::hint(Kind::Request, &offer.album_title, &offer.artist, &offer.cover);
                ctl.navigate_with(Route::OutsideAlbum(offer.album), hint);
            });
        });
        view.on_offers_refresh(|| {
            with(|ctl| ctl.refresh());
        });
        view.on_offer_request(|index| {
            with(|ctl| {
                let Some(offer) = ctl.page.borrow().offers.get(index as usize).cloned() else { return };
                if !offer.song.is_empty() {
                    return ctl.add_suggestion(index as usize);
                }
                if offer.album.is_empty() {
                    return;
                }
                ctl.prompt_request(&offer.album, &offer.album_title, &offer.artist, &offer.title, if offer.kind.is_empty() { "Album" } else { &offer.kind });
            });
        });
        settings.on_import_spotify(|| {
            with(|ctl| ctl.start_import());
        });
        settings.on_set_sync(|on| {
            with(|ctl| {
                ctl.change_setting(true, |s| s.sync = on);
                ctl.sync_switched(on);
            });
        });
        settings.on_set_check_updates(|on| {
            with(|ctl| {
                ctl.change_setting(true, |s| s.check_updates = on);
                if on {
                    ctl.check_for_updates(false);
                }
            });
        });
        settings.on_check_updates_now(|| {
            with(|ctl| ctl.check_for_updates(true));
        });
        settings.on_install_update(|| {
            with(|ctl| ctl.install_update());
        });
        settings.on_set_download_quality(|index| {
            with(|ctl| {
                let bitrate = QUALITIES.get(index as usize).map(|q| q.2).unwrap_or(0);
                ctl.change_setting(true, |s| s.download_quality = bitrate);
            });
        });
        settings.on_set_offline(|on| {
            with(|ctl| {
                let changed = ctl.settings.borrow().offline != on;
                if changed && on {
                    diagnostics::offline_changed(true);
                }
                ctl.set_offline_mode(on);
                if changed && !on {
                    diagnostics::offline_changed(false);
                }
            });
        });
        settings.on_change_songs_folder(|| {
            with(|ctl| ctl.change_songs_folder());
        });
        settings.on_reset_songs_folder(|| {
            with(|ctl| ctl.set_songs_folder(None));
        });
        settings.on_change_downloads_folder(|| {
            with(|ctl| ctl.change_downloads_folder());
        });
        settings.on_reset_downloads_folder(|| {
            with(|ctl| {
                let folder = ctl.paths.downloads.clone();
                ctl.move_downloads(folder);
            });
        });
        settings.on_save_diagnostics(|| {
            with(|ctl| ctl.save_diagnostics());
        });
        settings.on_remove_downloads(|| {
            with(|ctl| ctl.remove_all_downloads());
        });
        settings.on_set_cache_limit(|index| {
            with(|ctl| {
                let megabytes = config::CACHE_LIMITS.get(index as usize).map_or(0, |limit| limit.1);
                ctl.change_setting(true, |s| s.cache_limit_mb = megabytes);
                if megabytes == 0 {
                    ctl.fetch_ahead.want(Vec::new());
                    ctl.song_cache.clear();
                } else {
                    let cache = ctl.song_cache.clone();
                    ctl.api_pool.spawn(move || cache.trim(megabytes as u64 * 1024 * 1024, &HashSet::new()));
                }
                ctl.refresh_cache_size();
            });
        });
        settings.on_set_cache_ahead(|index| {
            with(|ctl| {
                let songs = config::CACHE_AHEAD.get(index as usize).copied().unwrap_or(0);
                ctl.change_setting(true, |s| s.cache_ahead = songs);
                ctl.keep_ahead();
            });
        });
        settings.on_clear_cache(|| {
            with(|ctl| {
                ctl.covers.clear_disk();
                ctl.fetch_ahead.want(Vec::new());
                ctl.song_cache.clear();
                ctl.cache.borrow_mut().clear();
                let _ = std::fs::remove_dir_all(&ctl.paths.pages);
                let _ = std::fs::create_dir_all(&ctl.paths.pages);
                ctl.refresh_cache_size();
                ctl.toast("Cache cleared");
            });
        });

        self.wire_playback();
        self.wire_jam();
        self.wire_link();
        self.wire_actions();
    }

    fn open_card(self: &Rc<Self>, card: &ui::Card) {
        let Some(route) = card_route(card) else { return };
        // The card already says enough to draw the page's header before the server answers.
        let subtitle = match self.page.borrow().label.as_str() {
            // On an artist's page the cards are captioned with years, not the artist.
            "Artist" => self.page.borrow().title.clone(),
            _ if card.kind == ui::CardKind::Album => card.subtitle.to_string(),
            _ if card.kind == ui::CardKind::Request => {
                crate::lidarr::offered(&card.id).map(|offer| offer.artist).unwrap_or_default()
            }
            _ => String::new(),
        };
        let hint = pages::hint(Kind::from_ui(card.kind), &card.title, &subtitle, &card.cover);
        self.navigate_with(route, hint);
    }
}

fn card_route(card: &ui::Card) -> Option<Route> {
    if card.id.is_empty() {
        return None;
    }
    let id = card.id.to_string();
    Some(match card.kind {
        ui::CardKind::Album => Route::Album(id),
        ui::CardKind::Artist => Route::Artist(id),
        ui::CardKind::Playlist => Route::Playlist(id),
        ui::CardKind::Genre => Route::Genre(id),
        ui::CardKind::Mix => Route::Mix(id),
        ui::CardKind::Request => Route::OutsideAlbum(id),
        ui::CardKind::OutsideArtist => Route::OutsideArtist(id),
    })
}

/// Loads a page with its extras and writes it to the disk cache.
fn fetch_whole(client: &Client, route: &Route, file: &std::path::Path) -> ApiResult<PageData> {
    let page = pages::load(client, route)?;
    let whole = pages::enrich(client, route, &page, &mut |_| {}).unwrap_or(page);
    store_page(file, &whole);
    Ok(whole)
}

pub(crate) fn store_page(file: &std::path::Path, page: &PageData) {
    // A page made from what's already on this PC is quicker made again than read back.
    if page.local {
        return;
    }
    // The page of a playlist the app keeps for itself holds, as its comment, what the app
    // keeps there: for one of them that is a secret. It is never written to disk.
    if page.playlist.as_ref().is_some_and(|playlist| !crate::link::page_may_be_kept(&playlist.name, &playlist.comment)) {
        return;
    }
    let Ok(json) = serde_json::to_vec(page) else { return };
    if json.len() > DISK_PAGE_LIMIT {
        return;
    }
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, json);
}

/// An account as the names of its lists of downloads and what waits to be sent have it.
pub(crate) fn account_stem(server: &str, username: &str) -> String {
    file_stem(&format!("{server}|{username}"))
}

/// A file-system-safe rendering of a cache key. Keys that differ only in characters this
/// replaces are told apart by the hash `file_name` appends.
fn file_stem(key: &str) -> String {
    key.chars()
        .take(80)
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}

/// Reads a stored page, and how long ago it was stored.
pub(crate) fn read_page(path: &std::path::Path) -> Option<(Duration, PageData)> {
    let data: PageData = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    // A page that was written before it was known not to be (by an earlier version, or
    // while its playlist had another name) holds what the app keeps in that playlist's
    // comment. It is not read: it goes.
    if data.playlist.as_ref().is_some_and(|playlist| !crate::link::page_may_be_kept(&playlist.name, &playlist.comment)) {
        let _ = std::fs::remove_file(path);
        return None;
    }
    let age = std::fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|at| at.elapsed().ok()).unwrap_or(Duration::MAX);
    Some((age, data))
}

fn file_name(key: &str) -> String {
    use md5::{Digest, Md5};
    let digest = Md5::digest(key.as_bytes());
    let hash: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
    format!("{}.{hash}.json", file_stem(key))
}

fn cards_model(cards: &[CardData]) -> ModelRc<ui::Card> {
    ModelRc::new(VecModel::from(cards.iter().map(CardData::to_ui).collect::<Vec<_>>()))
}

fn grid_rows(cards: &[CardData], columns: usize) -> Vec<ui::CardRow> {
    cards.chunks(columns.max(1)).map(|chunk| ui::CardRow { cards: cards_model(chunk) }).collect()
}

/// Formats seconds as m:ss, or h:mm:ss past an hour.
pub fn clock(seconds: u32) -> String {
    let (h, m, s) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

/// "1 song", "12 songs".
pub fn count(n: u32, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

/// A rough running time for a header: "32 min", "2 hr 5 min".
pub fn length(seconds: u32) -> String {
    let minutes = (seconds + 30) / 60;
    if minutes >= 60 {
        format!("{} hr {} min", minutes / 60, minutes % 60)
    } else {
        format!("{} min", minutes.max(1))
    }
}

impl Controller {
    /// Applies an equaliser preset chosen from the menu.
    pub fn choose_preset(&self, index: usize) {
        if let Some((_, gains)) = EQ_PRESETS.get(index) {
            self.change_setting(true, |s| {
                s.eq_gains = *gains;
                s.equalizer = true;
            });
        }
    }

    /// Applies one of your own saved equaliser curves.
    pub fn choose_saved_preset(&self, index: usize) {
        let saved = self.settings.borrow().eq_saved.get(index).map(|(_, gains)| *gains);
        if let Some(gains) = saved {
            self.change_setting(true, |s| {
                s.eq_gains = gains;
                s.equalizer = true;
            });
        }
    }

    /// Keeps the equaliser as it's set now under a name; the same name again replaces it.
    pub fn save_preset(&self, name: &str) {
        let name = name.trim().to_string();
        if name.is_empty() || EQ_PRESETS.iter().any(|(built_in, _)| *built_in == name) {
            return self.toast("That name is taken by one of the presets that come with the app.");
        }
        self.change_setting(true, |s| {
            let gains = s.eq_gains;
            s.eq_saved.retain(|(other, _)| *other != name);
            s.eq_saved.push((name.clone(), gains));
        });
        self.toast(format!("Saved “{name}”"));
    }

    pub fn delete_saved_preset(&self, index: usize) {
        self.change_setting(true, |s| {
            if index < s.eq_saved.len() {
                s.eq_saved.remove(index);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_page_cut_short_is_not_there_and_is_fetched_again() {
        let path = std::env::temp_dir().join(format!("nexus-page-{}.json", std::process::id()));
        let page = crate::pages::PageData { title: "Road Trip".into(), ..Default::default() };
        let whole = serde_json::to_vec(&page).unwrap();
        std::fs::write(&path, &whole).unwrap();
        assert_eq!(super::read_page(&path).map(|(_, read)| read), Some(page));
        // Written in place (it can be fetched again): cut short, or emptied, it is no page,
        // and the page is fetched as if none was kept.
        for cut in [&whole[..0], &whole[..whole.len() / 2], &whole[..whole.len() - 1]] {
            std::fs::write(&path, cut).unwrap();
            assert!(super::read_page(&path).is_none(), "{}", String::from_utf8_lossy(cut));
        }
        let _ = std::fs::remove_file(&path);
    }
}
