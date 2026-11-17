//! The UI-thread hub: owns the window, the server connection, navigation and the page on
//! screen. Playback lives in `playback.rs` and menus in `actions.rs`, as further `impl`s.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

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
pub fn post(f: impl FnOnce(&Rc<Controller>) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || {
        with(f);
    });
}

#[derive(Debug, Clone, PartialEq)]
pub enum Route {
    Home,
    Search(String),
    Album(String),
    Artist(String),
    Playlist(String),
    Liked,
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
            "downloads" => Route::Downloads,
            "radio" => Route::Radio,
            "albums" => Route::Albums(rest.parse().unwrap_or(0)),
            "artists" => Route::Artists,
            "genres" => Route::Genres,
            "genre" => Route::Genre(rest.to_string()),
            "settings" => Route::Settings,
            "lyrics" => Route::Lyrics,
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
            Route::Downloads => "downloads".into(),
            Route::Radio => "radio".into(),
            Route::Albums(_) => "albums".into(),
            Route::Artists => "artists".into(),
            Route::Genres => "genres".into(),
            Route::Genre(name) => format!("genre:{name}"),
            Route::Settings => "settings".into(),
            Route::Lyrics => "lyrics".into(),
        }
    }

    /// Identifies cached page data; distinct for anything that loads different content.
    fn cache_key(&self) -> String {
        match self {
            Route::Search(query) => format!("search:{query}"),
            Route::Albums(sort) => format!("albums:{sort}"),
            other => other.key(),
        }
    }

    fn page(&self) -> ui::Page {
        match self {
            Route::Home => ui::Page::Home,
            Route::Search(_) => ui::Page::Search,
            Route::Album(_) | Route::Playlist(_) | Route::Liked | Route::Downloads | Route::Radio => {
                ui::Page::Tracklist
            }
            Route::Artist(_) => ui::Page::Artist,
            Route::Albums(_) | Route::Artists | Route::Genres | Route::Genre(_) => ui::Page::Grid,
            Route::Settings => ui::Page::Settings,
            Route::Lyrics => ui::Page::Lyrics,
        }
    }
}

/// Orders a song list can be shown in. The first is the list's own order.
pub const TRACK_SORTS: [&str; 5] = ["Custom order", "Title", "Artist", "Album", "Duration"];

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

    /// Works out which songs to show, and in what order.
    fn arrange(&mut self, songs: &[Song]) {
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
            let key = |i: &usize| -> (String, u32) {
                let song = &songs[*i];
                match self.sort {
                    1 => (song.title.to_lowercase(), 0),
                    2 => (song.artist_label().to_lowercase(), 0),
                    3 => (song.album.to_lowercase(), song.disc_number * 1000 + song.track),
                    _ => (String::new(), song.duration),
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
    pub art_pool: Pool,
    pub download_pool: Pool,
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
    pub queue_model: Rc<VecModel<ui::Track>>,
    /// Songs that have been played, newest first, including the one playing now.
    pub history: RefCell<Vec<Song>>,
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
    /// Whether the sound device is currently held in exclusive mode.
    pub output_exclusive: Cell<bool>,
    /// The output devices last listed in the device menu: (id, name).
    pub devices: RefCell<Vec<(String, String)>>,
    /// The newer release found by the last check, if any.
    update: RefCell<Option<crate::update::Release>>,
    update_busy: Cell<bool>,
    #[cfg(windows)]
    tray: RefCell<Option<ui::Tray>>,
}

impl Controller {
    pub fn install(ui: ui::MainWindow) -> Rc<Controller> {
        let paths = Paths::new();
        let settings = Settings::load(&paths.settings);
        crate::audio::source::clean_stale(&paths.buffers);
        let engine = Engine::new(paths.buffers.clone(), |event| {
            post(move |ctl| ctl.on_engine_event(event));
        });
        let ctl = Rc::new(Controller {
            covers: Covers::new(paths.covers.clone()),
            api_pool: Pool::new("api", 4, false),
            download_pool: Pool::new("download", 2, false),
            downloads: RefCell::new(crate::downloads::Downloads::load(paths.downloads.clone())),
            art_pool: Pool::new("art", 6, true),
            engine,
            playback: RefCell::new(Playback::new(&settings)),
            settings: RefCell::new(settings),
            playlists: RefCell::default(),
            page: RefCell::default(),
            tracks_model: Rc::new(VecModel::default()),
            track_view: RefCell::default(),
            queue_model: Rc::new(VecModel::default()),
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
            #[cfg(windows)]
            media: RefCell::new(None),
            client: RefCell::new(None),
            nav: RefCell::default(),
            cache: RefCell::default(),
            prefetching: RefCell::default(),
            toast_timer: slint::Timer::default(),
            tick_timer: slint::Timer::default(),
            update_timer: slint::Timer::default(),
            output_exclusive: Cell::new(false),
            devices: RefCell::default(),
            update: RefCell::new(None),
            update_busy: Cell::new(false),
            #[cfg(windows)]
            tray: RefCell::new(None),
            paths,
            ui,
        });
        CONTROLLER.with(|slot| *slot.borrow_mut() = Some(ctl.clone()));
        ctl.wire();
        ctl.start();
        ctl
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
        let toast = self.ui.global::<ui::Toast>();
        toast.set_text(text.into());
        toast.set_visible(true);
        let weak = self.ui.as_weak();
        self.toast_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(3200), move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<ui::Toast>().set_visible(false);
            }
        });
    }

    pub fn save_settings(&self) {
        self.settings.borrow().save(&self.paths.settings);
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
        self.ui.global::<ui::Player>().set_history(ModelRc::from(self.history_model.clone()));
        self.sync_history();
        self.sync_transport();

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
                // Confirm the saved login still works; if not, go back to the sign-in screen.
                self.request(
                    |client| client.ping(),
                    |ctl, result| match result {
                        Ok(info) => {
                            ctl.ui.global::<ui::App>().set_server_version(info.version.into());
                        }
                        Err(crate::subsonic::ApiError::Server { .. }) => {
                            ctl.leave("Your saved sign-in no longer works. Sign in again.");
                        }
                        Err(_) => {}
                    },
                );
            }
        }

        let weak = Rc::downgrade(self);
        self.tick_timer.start(slint::TimerMode::Repeated, Duration::from_millis(250), move || {
            if let Some(ctl) = weak.upgrade() {
                ctl.tick();
            }
        });

        self.sync_downloads();
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

    /// Brings the window back from the tray or from behind other apps.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn show_window(self: &Rc<Self>) {
        let _ = self.ui.show();
        self.ui.window().set_minimized(false);
        #[cfg(windows)]
        crate::instance::focus(&self.ui);
    }

    pub fn quit(&self) {
        self.save_queue();
        self.save_settings();
        let _ = slint::quit_event_loop();
    }

    // ---- updates -------------------------------------------------------------------------------

    /// Asks GitHub whether a newer version exists. `manual` is a check the user asked for:
    /// it always runs, and reports problems instead of staying quiet.
    pub fn check_for_updates(self: &Rc<Self>, manual: bool) {
        if !manual && !self.settings.borrow().check_updates {
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
                match result {
                    Ok(Some(release)) => {
                        page.set_update_version(release.version.as_str().into());
                        page.set_update_status(
                            format!("Version {} is available.", release.version).into(),
                        );
                        ctl.ui.global::<ui::App>().set_update_available(true);
                        // Mention each new version once; after that the dot on the cog is enough.
                        if ctl.settings.borrow().update_notified != release.version {
                            ctl.settings.borrow_mut().update_notified = release.version.clone();
                            ctl.save_settings();
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
                    Err(reason) if manual => {
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
        let installer = match (&release.installer, crate::update::installed_copy()) {
            (Some(installer), true) => installer.clone(),
            // A portable copy, or a release without an installer: show where to get it.
            _ => return crate::update::open_in_browser(&release.page),
        };
        if self.update_busy.replace(true) {
            return;
        }
        let page = self.ui.global::<ui::Settings>();
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
                let launched = result.and_then(|path| {
                    crate::update::run_installer(&path).map_err(|e| format!("Couldn't start the installer: {e}"))
                });
                match launched {
                    // The installer replaces the program and opens the new version.
                    Ok(()) => ctl.quit(),
                    Err(reason) => {
                        ctl.update_busy.set(false);
                        page.set_update_busy(false);
                        page.set_update_status(reason.into());
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
        let client = Client::new(&server, &username, &password);
        self.api_pool.spawn(move || {
            let result = client.ping();
            post(move |ctl| {
                let app = ctl.ui.global::<ui::App>();
                app.set_login_busy(false);
                match result {
                    Ok(info) => {
                        {
                            let mut settings = ctl.settings.borrow_mut();
                            settings.server = client.server().to_string();
                            settings.username = username.clone();
                        }
                        ctl.save_settings();
                        if !config::save_password(&ctl.paths, client.server(), &username, &password) {
                            ctl.toast("Couldn't save your password; you'll need to sign in next time.");
                        }
                        ctl.covers.reset();
                        ctl.enter(client, Some(info.version));
                    }
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
        if let Some(version) = version {
            app.set_server_version(version.into());
        }
        // Have connections open before the first clicks need them.
        let warm = client.clone();
        self.art_pool.spawn(move || warm.warm_up(6));
        *self.client.borrow_mut() = Some(client);
        app.set_signed_in(true);
        *self.nav.borrow_mut() = Nav::default();
        self.cache.borrow_mut().clear();
        self.restore_queue();
        self.reload_playlists();
        self.navigate(Route::Home);
    }

    fn leave(self: &Rc<Self>, message: &str) {
        self.stop_playback();
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
        let _ = std::fs::remove_file(&self.paths.queue);
        let _ = std::fs::remove_file(&self.paths.history);
        self.history.borrow_mut().clear();
        self.leave("");
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

    /// A stored copy of a page and its age: from memory, or from disk on the first visit of
    /// a session.
    fn cached_page(&self, key: &str) -> Option<(Duration, PageData)> {
        if let Some((at, data)) = self.cache.borrow().get(key) {
            return Some((at.elapsed(), data.clone()));
        }
        let path = self.pages_dir()?.join(file_name(key));
        let bytes = std::fs::read(&path).ok()?;
        let data: PageData = serde_json::from_slice(&bytes).ok()?;
        let age = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .unwrap_or(Duration::MAX);
        let stored = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
        self.remember(key, stored, data.clone());
        Some((age, data))
    }

    fn remember(&self, key: &str, at: Instant, data: PageData) {
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

    fn show(self: &Rc<Self>, route: Route, scroll: f32, force: bool, hint: Option<PageData>) {
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
            // Leaving search: give the keyboard back to the player shortcuts.
            app.set_main_focus(app.get_main_focus().wrapping_add(1));
        }

        let view = self.ui.global::<ui::View>();
        match route {
            Route::Settings => {
                view.set_loading(false);
                view.set_error("".into());
                self.refresh_cache_size();
                return;
            }
            Route::Lyrics => {
                view.set_loading(false);
                view.set_error("".into());
                self.load_lyrics();
                return;
            }
            Route::Downloads => {
                self.show_downloads(scroll);
                return;
            }
            _ => {}
        }

        let key = route.cache_key();
        let cached = if force { None } else { self.cached_page(&key) };
        // Something to draw immediately: a stored copy, or failing that the hint.
        let shown = match cached {
            Some((age, data)) => {
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
        let (Some(client), Some(dir)) = (self.client(), self.pages_dir()) else { return };
        let file = dir.join(file_name(&key));
        self.api_pool.spawn(move || {
            let first = pages::load(&client, &route);
            let page = match first {
                Ok(page) => page,
                Err(e) => {
                    let offline_home = matches!(route, Route::Home)
                        && matches!(e, crate::subsonic::ApiError::Network(_));
                    return post(move |ctl| {
                        // A failed background check leaves the stored copy on screen.
                        if ctl.nav.borrow().generation == generation && !shown {
                            let view = ctl.ui.global::<ui::View>();
                            view.set_loading(false);
                            view.set_error(e.to_string().into());
                            // With the server out of reach, open on what can still be played.
                            if offline_home && !ctl.downloads.borrow().is_empty() {
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
                    if ctl.nav.borrow().generation == generation {
                        ctl.apply_page(page, scroll);
                    }
                });
            }
            let whole = pages::enrich(&client, &route, &page).unwrap_or(page);
            store_page(&file, &whole);
            post(move |ctl| {
                ctl.remember(&key, Instant::now(), whole.clone());
                if ctl.nav.borrow().generation == generation {
                    ctl.apply_if_changed(whole);
                }
            });
        });
    }

    /// Replaces the page on screen with a newer copy, unless nothing differs.
    fn apply_if_changed(self: &Rc<Self>, data: PageData) {
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
            *self.track_view.borrow_mut() = TrackView { key, ..Default::default() };
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
        if !same_shelves {
            view.set_shelves(ModelRc::new(VecModel::from(
                data.shelves
                    .iter()
                    .map(|shelf| ui::Shelf {
                        title: shelf.title.as_str().into(),
                        route: shelf.route.as_str().into(),
                        wrap: shelf.wrap,
                        cards: cards_model(&shelf.cards),
                    })
                    .collect::<Vec<_>>(),
            )));
        }
        if !same_grid {
            self.grid_model.set_vec(grid_rows(&data.grid, self.grid_columns.get()));
        }
        self.grid_loading.set(false);
        *self.page.borrow_mut() = data;
        if !same_tracks {
            self.rebuild_tracks();
        } else {
            self.sync_track_view();
        }

        view.set_error("".into());
        view.set_loading(false);
        view.set_scroll_y(scroll);
    }

    /// Rebuilds the song rows from the page's songs, as filtered and sorted.
    fn rebuild_tracks(&self) {
        let rows = {
            let page = self.page.borrow();
            self.track_view.borrow_mut().arrange(&page.songs);
            self.track_rows(&page)
        };
        self.tracks_model.set_vec(rows);
        self.sync_track_view();
    }

    /// Shows the state of the filter and sort controls.
    fn sync_track_view(&self) {
        let page = self.page.borrow();
        let state = self.track_view.borrow();
        let view = self.ui.global::<ui::View>();
        let can_filter = page.playlist.is_some()
            || page.liked_songs
            || matches!(self.current_route(), Some(Route::Downloads));
        view.set_can_filter(can_filter && !page.songs.is_empty());
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
        {
            let mut state = self.track_view.borrow_mut();
            state.descending = sort != 0 && state.sort == sort && !state.descending;
            state.sort = sort.min(TRACK_SORTS.len() - 1);
        }
        self.rebuild_tracks();
    }

    /// Moves a playlist's song from one row to another and saves the new order.
    fn move_playlist_track(self: &Rc<Self>, from: usize, to: usize) {
        if !self.track_view.borrow().is_default() {
            return;
        }
        let (id, ids) = {
            let mut page = self.page.borrow_mut();
            let Some(id) = page.playlist.as_ref().map(|p| p.id.clone()) else { return };
            if from == to || from >= page.songs.len() || to >= page.songs.len() {
                return;
            }
            let song = page.songs.remove(from);
            page.songs.insert(to, song);
            (id, page.songs.iter().map(|s| s.id.clone()).collect::<Vec<_>>())
        };
        self.rebuild_tracks();
        if let Some(route) = self.current_route() {
            let data = self.page.borrow().clone();
            self.remember(&route.cache_key(), Instant::now(), data);
        }
        self.request(
            move |client| client.replace_playlist(&id, &ids),
            |ctl, result| {
                if let Err(e) = result {
                    ctl.toast(format!("Couldn't save the new order. {e}"));
                    ctl.refresh();
                }
            },
        );
    }

    /// Builds the interface rows for the page's songs, in the order the view has arranged.
    fn track_rows(&self, data: &PageData) -> Vec<ui::Track> {
        let multi_disc = data.disc_sections
            && data.songs.iter().any(|s| s.disc_number > 1);
        let mut last_disc = 0;
        let state = self.track_view.borrow();
        state
            .map
            .iter()
            .filter_map(|&i| data.songs.get(i))
            .enumerate()
            .map(|(index, song)| {
                let mut row = self.track_row(song);
                row.number = if data.album_numbers && song.track > 0 {
                    song.track.to_string().into()
                } else {
                    (index + 1).to_string().into()
                };
                if multi_disc && song.disc_number != last_disc {
                    last_disc = song.disc_number;
                    row.section = format!("Disc {}", song.disc_number.max(1)).into();
                }
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
            number: SharedString::default(),
            starred: self.is_starred(song),
            section: SharedString::default(),
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
                let Ok(mut playlists) = result else { return };
                playlists.sort_by_key(|p| p.name.to_lowercase());
                let rows: Vec<ui::PlaylistRef> = playlists
                    .iter()
                    .map(|p| ui::PlaylistRef {
                        id: p.id.as_str().into(),
                        name: p.name.as_str().into(),
                        subtitle: count(p.song_count, "song").into(),
                        cover: p.cover_art.as_str().into(),
                    })
                    .collect();
                ctl.ui.global::<ui::App>().set_playlists(ModelRc::new(VecModel::from(rows)));
                *ctl.playlists.borrow_mut() = playlists;
            },
        );
    }

    // ---- settings ------------------------------------------------------------------------------

    /// Copies the saved settings into the Settings page.
    fn push_settings(&self) {
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
        let quality = QUALITIES.iter().position(|q| q.2 == settings.bitrate).unwrap_or(QUALITIES.len() - 1);
        page.set_quality(quality as i32);
        page.set_crossfade(settings.crossfade);
        page.set_gapless(settings.gapless);
        page.set_autoplay(settings.autoplay);
        page.set_normalize(settings.normalize);
        page.set_volume_levels(strings(VOLUME_LEVELS.iter().map(|l| l.0).collect()));
        page.set_volume_level(settings.volume_level as i32);
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
        page.set_close_to_tray(settings.close_to_tray);
        page.set_check_updates(settings.check_updates);
    }

    /// Changes a setting, shows it, applies it to the audio and, when `save` is set, writes
    /// it to disk. Dragging a slider passes `save: false` until the drag ends.
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
        self.api_pool.spawn(move || {
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
        app.on_sign_out(|| {
            with(|ctl| ctl.sign_out());
        });
        app.on_navigate(|route| {
            with(|ctl| {
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
        app.on_search(|text| {
            with(|ctl| {
                let query = text.trim().to_string();
                // Clearing the box while browsing elsewhere shouldn't yank you to Search.
                let on_search = matches!(ctl.current_route(), Some(Route::Search(_)));
                if !query.is_empty() || on_search {
                    ctl.navigate(Route::Search(query));
                }
            });
        });
        app.on_new_playlist(|| {
            with(|ctl| ctl.prompt_new_playlist(Vec::new()));
        });

        let view = self.ui.global::<ui::View>();
        view.on_play(|shuffle| {
            with(|ctl| ctl.play_page(shuffle));
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
        view.on_track_moved(|index, delta| {
            with(|ctl| ctl.move_playlist_track(index.max(0) as usize, (index + delta).max(0) as usize));
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
            with(|ctl| ctl.change_setting(save, |s| s.crossfade = seconds.round().clamp(0.0, 12.0)));
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
                ctl.change_setting(save, |s| {
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
        settings.on_set_close_to_tray(|on| {
            with(|ctl| {
                ctl.change_setting(true, |s| s.close_to_tray = on);
                ctl.sync_tray();
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
        settings.on_remove_downloads(|| {
            with(|ctl| ctl.remove_all_downloads());
        });
        settings.on_clear_cache(|| {
            with(|ctl| {
                ctl.covers.clear_disk();
                ctl.cache.borrow_mut().clear();
                let _ = std::fs::remove_dir_all(&ctl.paths.pages);
                let _ = std::fs::create_dir_all(&ctl.paths.pages);
                ctl.refresh_cache_size();
                ctl.toast("Cache cleared");
            });
        });

        self.wire_playback();
        self.wire_actions();
    }

    fn open_card(self: &Rc<Self>, card: &ui::Card) {
        let Some(route) = card_route(card) else { return };
        // The card already says enough to draw the page's header before the server answers.
        let subtitle = match self.page.borrow().label.as_str() {
            // On an artist's page the cards are captioned with years, not the artist.
            "Artist" => self.page.borrow().title.clone(),
            _ if card.kind == ui::CardKind::Album => card.subtitle.to_string(),
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
    })
}

/// Loads a page with its extras and writes it to the disk cache.
fn fetch_whole(client: &Client, route: &Route, file: &std::path::Path) -> ApiResult<PageData> {
    let page = pages::load(client, route)?;
    let whole = pages::enrich(client, route, &page).unwrap_or(page);
    store_page(file, &whole);
    Ok(whole)
}

fn store_page(file: &std::path::Path, page: &PageData) {
    let Ok(json) = serde_json::to_vec(page) else { return };
    if json.len() > DISK_PAGE_LIMIT {
        return;
    }
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(file, json);
}

/// A file-system-safe rendering of a cache key. Keys that differ only in characters this
/// replaces are told apart by the hash `file_name` appends.
fn file_stem(key: &str) -> String {
    key.chars()
        .take(80)
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
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
}
