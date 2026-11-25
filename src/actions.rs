//! Context menus, likes, and playlist management.

use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, VecModel, Model as _};

use crate::controller::{Controller, Route, with};
use crate::subsonic::Song;
use crate::ui;

/// What a context menu was opened on.
#[derive(Clone, Default)]
pub enum Target {
    #[default]
    None,
    Song {
        song: Song,
        /// Set when the row is in a playlist: its id and the row's position.
        playlist: Option<(String, usize)>,
        /// Set when the row is in the queue: how far after the current track.
        upcoming: Option<usize>,
    },
    /// Several songs chosen together.
    Songs {
        songs: Vec<Song>,
        /// Set when they're rows of a playlist: its id and the rows' positions.
        playlist: Option<(String, Vec<usize>)>,
    },
    Album { id: String, title: String },
    Artist { id: String, name: String },
    Playlist { id: String, name: String },
    /// An album on the Requests page: Lidarr's number for it, and its title.
    Request { id: i64, title: String },
}

impl Target {
    /// The route key of the page that lists exactly this target's songs, if there is one.
    fn route_key(&self) -> Option<String> {
        match self {
            Target::Album { id, .. } => Some(format!("album:{id}")),
            Target::Playlist { id, .. } => Some(format!("playlist:{id}")),
            _ => None,
        }
    }
}

#[derive(Default)]
pub struct MenuState {
    target: Target,
    /// The top-level entries, to return to from the playlist picker.
    root: Vec<ui::MenuEntry>,
    title: String,
}

pub enum PromptAction {
    /// Some of these songs are already in the playlist: add them all, or only the new ones.
    AddDuplicates { playlist_id: String, all: Vec<Song>, fresh: Vec<Song> },
    CreatePlaylist(Vec<Song>),
    /// The file or folder to import playlists from was typed in.
    ImportPath,
    /// A name for the equaliser as it's set now.
    SavePreset,
    /// A name for a new mix made to rules.
    NewMix,
    /// Something typed for one of a mix's rules: the mix, and which rule.
    RuleText { rule: u64, field: &'static str },
    /// The folder to keep downloads in was typed in.
    DownloadsFolder,
    /// The folder for songs kept as they play was typed in.
    SongsFolder,
    /// Ask Lidarr for an album (by its name for it), or for everything an import didn't find.
    RequestAlbum { album: String, title: String, song: String },
    AddArtist { artist: String, name: String },
    RequestMissing,
    RenamePlaylist(String),
    DeletePlaylist(String),
}

impl Controller {
    fn entry(&self, id: &str, label: &str, icon: slint::Image) -> ui::MenuEntry {
        ui::MenuEntry {
            id: id.into(),
            label: label.into(),
            icon,
            separator: false,
            danger: false,
            submenu: false,
        }
    }

    fn separator(&self) -> ui::MenuEntry {
        ui::MenuEntry { separator: true, ..Default::default() }
    }

    fn open_menu(&self, target: Target, title: &str, entries: Vec<ui::MenuEntry>, x: f32, y: f32) {
        let menu = self.ui.global::<ui::Menu>();
        menu.set_title(title.into());
        menu.set_entries(ModelRc::new(VecModel::from(entries.clone())));
        menu.set_x(x);
        menu.set_y(y);
        menu.set_open(true);
        *self.menu.borrow_mut() = MenuState { target, root: entries, title: title.to_string() };
    }

    fn close_menu(&self) {
        self.ui.global::<ui::Menu>().set_open(false);
    }

    /// Does what choosing `id` from the open menu would, if the menu offers it; either way
    /// the menu is closed. For keys that are shortcuts for a menu entry.
    pub fn act_on_menu(self: &Rc<Self>, id: &str) {
        let offered = self.ui.global::<ui::Menu>().get_entries().iter().any(|entry| entry.id == id);
        if offered {
            self.menu_activated(id);
        }
        self.close_menu();
    }

    /// An arrow key in a menu: moves to the next entry that way, round the ends.
    fn move_menu_cursor(&self, delta: i32) {
        let menu = self.ui.global::<ui::Menu>();
        let entries: Vec<ui::MenuEntry> = menu.get_entries().iter().collect();
        let count = entries.len() as i32;
        if !entries.iter().any(|entry| !entry.separator) {
            return;
        }
        // From no entry, down lands on the first and up on the last.
        let mut at = if menu.get_cursor() < 0 && delta < 0 { count } else { menu.get_cursor() };
        loop {
            at = (at + delta.signum()).rem_euclid(count);
            if !entries[at as usize].separator {
                break;
            }
        }
        let above: f32 = entries[..at as usize].iter().map(|entry| if entry.separator { 9.0 } else { 38.0 }).sum();
        menu.set_cursor(at);
        menu.set_cursor_y(4.0 + if menu.get_title().is_empty() { 0.0 } else { 34.0 } + above);
    }

    /// The user's rating for a song, 0 to 5, counting changes made in this session.
    pub fn rating_of(&self, song: &Song) -> u8 {
        self.ratings.borrow().get(&song.id).copied().unwrap_or(song.user_rating)
    }

    fn song_menu(&self, song: Song, playlist: Option<(String, usize)>, upcoming: Option<usize>, x: f32, y: f32) {
        // A radio station isn't a song in the library: none of these actions apply to it.
        // Nor is a row that only names a song an import couldn't find.
        if !song.radio_url.is_empty() || song.id.is_empty() {
            return;
        }
        let icons = self.ui.global::<ui::Icons>();
        let mut entries = vec![
            self.entry("play-next", "Play next", icons.get_list_start()),
            self.entry("add-queue", "Add to queue", icons.get_list_end()),
            self.separator(),
        ];
        let mut add = self.entry("add-to", "Add to playlist", icons.get_list_plus());
        add.submenu = true;
        entries.push(add);
        entries.push(if self.is_starred(&song) {
            self.entry("unstar", "Remove from Liked songs", icons.get_heart_fill())
        } else {
            self.entry("star", "Add to Liked songs", icons.get_heart())
        });
        let rating = self.rating_of(&song);
        let label = match rating {
            0 => "Rate".to_string(),
            1 => "Rated 1 star".to_string(),
            n => format!("Rated {n} stars"),
        };
        let mut rate = self.entry("rate", &label, icons.get_star());
        rate.submenu = true;
        entries.push(rate);
        let downloading = self.downloads.borrow().progress_of(&song.id) >= 0.0;
        entries.push(if self.downloads.borrow().has(&song.id) {
            self.entry("remove-download", "Remove download", icons.get_circle_arrow_down())
        } else if downloading {
            self.entry("cancel-download", "Cancel download", icons.get_x())
        } else {
            self.entry("download", "Download", icons.get_download())
        });
        entries.push(self.separator());
        if !song.album_id.is_empty() {
            entries.push(self.entry("go-album", "Go to album", icons.get_disc_album()));
        }
        // Everyone credited, each with a way to their page; or the one artist, if that's
        // all the server names.
        let credited: Vec<&crate::subsonic::Credit> = song.artists.iter().filter(|credit| !credit.id.is_empty()).collect();
        if credited.len() > 1 {
            for credit in credited {
                entries.push(self.entry(&format!("go-artist:{}", credit.id), &format!("Go to {}", credit.name), icons.get_user_round()));
            }
        } else if !song.artist_id.is_empty() {
            entries.push(self.entry("go-artist", "Go to artist", icons.get_user_round()));
        }
        entries.push(self.entry("details", "Song details", icons.get_info()));
        entries.push(self.entry("radio", "Start radio from this song", icons.get_radio()));
        if playlist.is_some() {
            entries.push(self.separator());
            entries.push(self.entry("remove-playlist", "Remove from this playlist", icons.get_trash_2()));
        }
        if upcoming.is_some() {
            entries.push(self.separator());
            entries.push(self.entry("remove-queue", "Remove from queue", icons.get_x()));
        }
        let title = song.title.clone();
        self.open_menu(Target::Song { song, playlist, upcoming }, &title, entries, x, y);
    }

    /// The menu for a song that isn't tied to the page on screen or the queue.
    pub fn open_song_menu(self: &Rc<Self>, song: Song, x: f32, y: f32) {
        self.song_menu(song, None, None, x, y);
    }

    /// The menu for several selected songs.
    fn songs_menu(&self, songs: Vec<Song>, playlist: Option<(String, Vec<usize>)>, x: f32, y: f32) {
        let icons = self.ui.global::<ui::Icons>();
        let mut entries = vec![
            self.entry("play-next", "Play next", icons.get_list_start()),
            self.entry("add-queue", "Add to queue", icons.get_list_end()),
            self.separator(),
        ];
        let mut add = self.entry("add-to", "Add to playlist", icons.get_list_plus());
        add.submenu = true;
        entries.push(add);
        // Adding skips the ones already liked; once they all are, the offer is to remove them.
        entries.push(if songs.iter().all(|song| self.is_starred(song)) {
            self.entry("unstar-all", "Remove from Liked songs", icons.get_heart_fill())
        } else {
            self.entry("star-all", "Add to Liked songs", icons.get_heart())
        });
        // Offer whichever of these applies to at least one of the songs.
        let (saved, unsaved) = {
            let downloads = self.downloads.borrow();
            let saved = songs.iter().filter(|s| downloads.has(&s.id) || downloads.progress_of(&s.id) >= 0.0).count();
            (saved, songs.len() - saved)
        };
        if unsaved > 0 {
            entries.push(self.entry("download", "Download", icons.get_download()));
        }
        if saved > 0 {
            entries.push(self.entry("remove-download", "Remove downloads", icons.get_circle_arrow_down()));
        }
        if playlist.is_some() {
            entries.push(self.separator());
            entries.push(self.entry("remove-playlist", "Remove from this playlist", icons.get_trash_2()));
        }
        let title = format!("{} songs", songs.len());
        self.open_menu(Target::Songs { songs, playlist }, &title, entries, x, y);
    }

    /// `row` is the row as shown, which a filter or sort can put anywhere in the list.
    pub fn open_track_menu(self: &Rc<Self>, row: usize, x: f32, y: f32) {
        // On the Requests page a row is an album that's been asked for.
        let request = {
            let page = self.page.borrow();
            self.song_index(row).and_then(|index| Some((*page.request_ids.get(index)?, page.songs.get(index)?.title.clone())))
        };
        if let Some((id, title)) = request {
            let entries = vec![self.entry("cancel-request", "Take off the list", self.ui.global::<ui::Icons>().get_x())];
            return self.open_menu(Target::Request { id, title: title.clone() }, &title, entries, x, y);
        }
        // Right-clicking inside a selection acts on all of it.
        let rows = self.rows_for(row);
        if rows.len() > 1 {
            let chosen = self.songs_on(&rows);
            let playlist = self.page.borrow().playlist.as_ref().map(|p| {
                (p.id.clone(), chosen.iter().map(|(index, _)| *index).collect::<Vec<_>>())
            });
            let songs: Vec<Song> = chosen
                .into_iter()
                .map(|(_, song)| song)
                .filter(|s| s.radio_url.is_empty() && !s.id.is_empty())
                .collect();
            if songs.len() > 1 {
                return self.songs_menu(songs, playlist, x, y);
            }
        }
        let Some(index) = self.song_index(row) else { return };
        let (song, playlist) = {
            let page = self.page.borrow();
            let Some(song) = page.songs.get(index).cloned() else { return };
            (song, page.playlist.as_ref().map(|p| (p.id.clone(), index)))
        };
        self.song_menu(song, playlist, None, x, y);
    }

    /// `index` is the row in the queue panel, or -1 for the track that's playing.
    pub fn open_queue_menu(self: &Rc<Self>, index: i32, x: f32, y: f32) {
        // Controlling playback elsewhere, the queue on show is that one, not this PC's.
        let song = match self.remote_song(index) {
            Some(found) => found,
            None => {
                let pb = self.playback.borrow();
                let at = (pb.cursor as i64 + 1 + index as i64) as usize;
                pb.order.get(at).and_then(|&i| pb.queue.get(i)).cloned()
            }
        };
        if let Some(song) = song {
            self.song_menu(song, None, (index >= 0).then_some(index as usize), x, y);
        }
    }

    fn collection_menu(&self, target: Target, title: &str, x: f32, y: f32) {
        let icons = self.ui.global::<ui::Icons>();
        let mut entries = vec![
            self.entry("play", "Play", icons.get_play_fill()),
            self.entry("play-next", "Play next", icons.get_list_start()),
            self.entry("add-queue", "Add to queue", icons.get_list_end()),
        ];
        match &target {
            Target::Album { .. } => {
                entries.push(self.separator());
                let mut add = self.entry("add-to", "Add to playlist", icons.get_list_plus());
                add.submenu = true;
                entries.push(add);
                entries.push(self.entry("download", "Download", icons.get_download()));
            }
            Target::Playlist { id, .. } => {
                entries.push(self.separator());
                entries.push(if self.settings.borrow().pinned_playlists.contains(id) {
                    self.entry("unpin", "Unpin", icons.get_pin())
                } else {
                    self.entry("pin", "Pin to top", icons.get_pin())
                });
                entries.push(self.entry("download", "Download", icons.get_download()));
                entries.push(self.entry("export", "Export as CSV", icons.get_file_down()));
                entries.push(self.entry("rename", "Rename", icons.get_pencil()));
                let mut delete = self.entry("delete", "Delete playlist", icons.get_trash_2());
                delete.danger = true;
                entries.push(delete);
            }
            _ => {}
        }
        self.open_menu(target, title, entries, x, y);
    }

    pub fn open_card_menu(self: &Rc<Self>, card: &ui::Card, x: f32, y: f32) {
        let (id, title) = (card.id.to_string(), card.title.to_string());
        match card.kind {
            ui::CardKind::Album => self.collection_menu(Target::Album { id, title: title.clone() }, &title, x, y),
            ui::CardKind::Playlist => {
                self.collection_menu(Target::Playlist { id, name: title.clone() }, &title, x, y)
            }
            ui::CardKind::Artist => {
                self.collection_menu(Target::Artist { id, name: title.clone() }, &title, x, y)
            }
            ui::CardKind::Genre | ui::CardKind::Mix | ui::CardKind::Request | ui::CardKind::OutsideArtist => {}
        }
    }

    pub fn open_header_menu(self: &Rc<Self>, x: f32, y: f32) {
        let target = {
            let page = self.page.borrow();
            match (&page.playlist, self.current_route()) {
                (Some(p), _) => Target::Playlist { id: p.id.clone(), name: p.name.clone() },
                (None, Some(Route::Album(id))) => Target::Album { id, title: page.title.clone() },
                // A mix is made afresh each day and changes over time: it can be kept as a
                // playlist, and one made to rules of your own can have those rules changed.
                (None, Some(Route::Mix(id))) => {
                    let rule = id.strip_prefix("rule:").and_then(|id| id.parse::<u64>().ok());
                    let (songs, title) = (page.songs.clone(), page.title.clone());
                    drop(page);
                    return self.mix_menu(rule, songs, &title, x, y);
                }
                _ => return,
            }
        };
        let title = match &target {
            Target::Playlist { name, .. } => name.clone(),
            Target::Album { title, .. } => title.clone(),
            _ => String::new(),
        };
        self.collection_menu(target, &title, x, y);
    }

    fn show_rating_picker(&self) {
        let icons = self.ui.global::<ui::Icons>();
        let current = match &self.menu.borrow().target {
            Target::Song { song, .. } => self.rating_of(song),
            _ => 0,
        };
        let tick = |on: bool| if on { icons.get_check() } else { slint::Image::default() };
        let mut entries = vec![self.entry("back", "Back", icons.get_chevron_left()), self.separator()];
        for stars in (1..=5u8).rev() {
            let label = if stars == 1 { "1 star".to_string() } else { format!("{stars} stars") };
            entries.push(self.entry(&format!("rate:{stars}"), &label, tick(stars == current)));
        }
        entries.push(self.separator());
        entries.push(self.entry("rate:0", "No rating", tick(current == 0)));
        let menu = self.ui.global::<ui::Menu>();
        menu.set_title("Rate".into());
        menu.set_entries(ModelRc::new(VecModel::from(entries)));
    }

    /// Saves a song's rating on the server; 0 clears it.
    fn rate_song(self: &Rc<Self>, song: &Song, rating: u8) {
        let previous = self.rating_of(song);
        self.ratings.borrow_mut().insert(song.id.clone(), rating);
        let undo = song.id.clone();
        self.toast(match rating {
            0 => "Rating removed".to_string(),
            1 => "Rated 1 star".to_string(),
            n => format!("Rated {n} stars"),
        });
        self.submit(crate::offline::Pending::Rating { id: song.id.clone(), rating }, move |ctl, reason| {
            ctl.ratings.borrow_mut().insert(undo, previous);
            ctl.toast(format!("Couldn't save the rating. {reason}"));
        });
    }

    pub fn open_sleep_menu(&self, x: f32, y: f32) {
        let icons = self.ui.global::<ui::Icons>();
        let timed = self.sleep_at.get().is_some();
        let after_song = self.sleep_after_song.get();
        let tick = |on: bool| if on { icons.get_check() } else { slint::Image::default() };
        let mut entries = Vec::new();
        // With a timer running, the menu opens on how long it has left.
        if let Some(at) = self.sleep_at.get() {
            let minutes = at.saturating_duration_since(std::time::Instant::now()).as_secs().div_ceil(60).max(1);
            let label = if minutes == 1 { "1 minute left".to_string() } else { format!("{minutes} minutes left") };
            entries.push(self.entry("sleep:keep", &label, icons.get_moon()));
            entries.push(self.separator());
        }
        for minutes in [5u32, 10, 15, 30, 45, 60] {
            let label = if minutes == 60 { "1 hour".to_string() } else { format!("{minutes} minutes") };
            entries.push(self.entry(&format!("sleep:{minutes}"), &label, slint::Image::default()));
        }
        entries.push(self.entry("sleep:end", "End of this song", tick(after_song)));
        entries.push(self.separator());
        entries.push(self.entry("sleep:0", "Off", tick(!timed && !after_song)));
        // The menu opens above the button it belongs to.
        let height = entries.len() as f32 * 36.0 + 40.0;
        self.open_menu(Target::None, "Pause music after", entries, x, y - height);
    }

    fn show_playlist_picker(&self) {
        let icons = self.ui.global::<ui::Icons>();
        let mut entries = vec![
            self.entry("back", "Back", icons.get_chevron_left()),
            self.entry("pl-new", "New playlist", icons.get_plus()),
        ];
        let playlists = self.playlists.borrow();
        if !playlists.is_empty() {
            entries.push(self.separator());
        }
        for playlist in playlists.iter() {
            entries.push(self.entry(&format!("pl:{}", playlist.id), &playlist.name, icons.get_list_music()));
        }
        let menu = self.ui.global::<ui::Menu>();
        menu.set_title("Add to playlist".into());
        menu.set_entries(ModelRc::new(VecModel::from(entries)));
    }

    /// Fetches the songs a menu target stands for, then continues on the UI thread.
    fn with_songs(
        self: &Rc<Self>,
        target: Target,
        then: impl FnOnce(&Rc<Controller>, Vec<Song>) + Send + 'static,
    ) {
        let finish = |ctl: &Rc<Controller>, result: crate::subsonic::ApiResult<Vec<Song>>, then: Box<dyn FnOnce(&Rc<Controller>, Vec<Song>) + Send>| match result {
            Ok(songs) if songs.is_empty() => ctl.toast("There are no songs in that."),
            Ok(songs) => then(ctl, songs),
            Err(e) => ctl.toast(e.to_string()),
        };
        let then: Box<dyn FnOnce(&Rc<Controller>, Vec<Song>) + Send> = Box::new(then);
        match target {
            Target::None | Target::Request { .. } => {}
            Target::Song { song, .. } => then(self, vec![song]),
            Target::Songs { songs, .. } => then(self, songs),
            Target::Album { id, .. } => self.request(
                move |client| client.album(&id).map(|a| a.songs),
                move |ctl, result| finish(ctl, result, then),
            ),
            Target::Playlist { id, .. } => self.request(
                move |client| client.playlist(&id).map(|p| p.songs),
                move |ctl, result| finish(ctl, result, then),
            ),
            Target::Artist { id, .. } => self.request(
                move |client| {
                    let artist = client.artist(&id)?;
                    let mut songs = Vec::new();
                    for album in artist.albums.iter().take(40) {
                        songs.extend(client.album(&album.id)?.songs);
                    }
                    Ok(songs)
                },
                move |ctl, result| finish(ctl, result, then),
            ),
        }
    }

    /// The equaliser's preset list, shown with the same menu as everything else.
    pub fn open_preset_menu(&self, x: f32, y: f32) {
        let icons = self.ui.global::<ui::Icons>();
        let settings = self.settings.borrow();
        let current = settings.eq_preset();
        let tick = |name: &str| if name == current { icons.get_check() } else { slint::Image::default() };
        let mut entries: Vec<ui::MenuEntry> = crate::config::EQ_PRESETS
            .iter()
            .enumerate()
            .map(|(index, (name, _))| self.entry(&format!("eq:{index}"), name, tick(name)))
            .collect();
        // Your own, then keeping the curve as it is now, then removing the one in use.
        if !settings.eq_saved.is_empty() {
            entries.push(self.separator());
            entries.extend(settings.eq_saved.iter().enumerate().map(|(index, (name, _))| self.entry(&format!("eq-mine:{index}"), name, tick(name))));
        }
        entries.push(self.separator());
        entries.push(self.entry("eq-save", "Save as a preset", icons.get_list_plus()));
        if let Some((index, (name, _))) = settings.eq_saved.iter().enumerate().find(|(_, (name, _))| *name == current) {
            let mut delete = self.entry(&format!("eq-delete:{index}"), &format!("Delete “{name}”"), icons.get_trash_2());
            delete.danger = true;
            entries.push(delete);
        }
        drop(settings);
        self.open_menu(Target::None, "Presets", entries, x, y);
    }

    pub fn open_device_menu(&self, x: f32, y: f32) {
        let icons = self.ui.global::<ui::Icons>();
        let devices = crate::audio::engine::output_devices();
        let current = self.settings.borrow().output_device.clone();
        let tick = |on: bool| if on { icons.get_check() } else { slint::Image::default() };
        let mut entries = vec![self.entry("device:default", "System default", tick(current.is_empty()))];
        for (index, (id, name)) in devices.iter().enumerate() {
            entries.push(self.entry(&format!("device:{index}"), name, tick(*id == current)));
        }
        *self.devices.borrow_mut() = devices;
        self.open_menu(Target::None, "Play through", entries, x, y);
    }

    pub fn open_track_sort_menu(&self, x: f32, y: f32) {
        let icons = self.ui.global::<ui::Icons>();
        let (current, descending) = {
            let state = self.track_view.borrow();
            (state.sort(), state.descending())
        };
        let own_order = if self.page.borrow().playlist.is_some() { "Custom order" } else { "Default order" };
        let entries = crate::controller::TRACK_SORTS
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let icon = match (index == current, index, descending) {
                    (false, ..) => slint::Image::default(),
                    (true, 0, _) => icons.get_check(),
                    (true, _, false) => icons.get_arrow_up(),
                    (true, _, true) => icons.get_arrow_down(),
                };
                self.entry(&format!("sort:{index}"), if index == 0 { own_order } else { name }, icon)
            })
            .collect();
        self.open_menu(Target::None, "Sort by", entries, x, y);
    }

    fn menu_activated(self: &Rc<Self>, id: &str) {
        if let Some(artist) = id.strip_prefix("go-artist:") {
            self.close_menu();
            return self.navigate(Route::Artist(artist.to_string()));
        }
        if let Some(index) = id.strip_prefix("eq:").and_then(|i| i.parse().ok()) {
            self.close_menu();
            return self.choose_preset(index);
        }
        if let Some(index) = id.strip_prefix("eq-mine:").and_then(|i| i.parse().ok()) {
            self.close_menu();
            return self.choose_saved_preset(index);
        }
        if let Some(index) = id.strip_prefix("eq-delete:").and_then(|i| i.parse().ok()) {
            self.close_menu();
            return self.delete_saved_preset(index);
        }
        if id == "eq-save" {
            self.close_menu();
            *self.prompt.borrow_mut() = Some(PromptAction::SavePreset);
            return self.open_prompt("Name this preset", "Keeps the equaliser as it's set now.", "Save preset", Some(""), false);
        }
        if let Some(choice) = id.strip_prefix("sleep:") {
            self.close_menu();
            if choice == "keep" {
                // The time-left line is there to be read, not to change anything.
                return;
            }
            return self.set_sleep(if choice == "end" { None } else { Some(choice.parse().unwrap_or(0)) });
        }
        if let Some(rating) = id.strip_prefix("rate:").and_then(|r| r.parse::<u8>().ok()) {
            self.close_menu();
            let target = std::mem::take(&mut self.menu.borrow_mut().target);
            if let Target::Song { song, .. } = target {
                self.rate_song(&song, rating);
            }
            return;
        }
        if let Some(device) = id.strip_prefix("device:") {
            self.close_menu();
            return self.choose_device(device.parse().ok());
        }
        if let Some(index) = id.strip_prefix("sort:").and_then(|i| i.parse().ok()) {
            self.close_menu();
            return self.set_track_sort(index);
        }
        match id {
            "new-playlist" => {
                self.close_menu();
                return self.prompt_new_playlist(Vec::new());
            }
            "import-spotify" => {
                self.close_menu();
                return self.start_import();
            }
            "new-mix" => {
                self.close_menu();
                *self.prompt.borrow_mut() = Some(PromptAction::NewMix);
                return self.open_prompt(
                    "Name your mix",
                    "A mix picks songs from your library by rules you set: a genre, a rating, how long since you played them. You choose the rules next, from the mix's menu.",
                    "Create mix",
                    Some(""),
                    false,
                );
            }
            rule if rule.starts_with("rule") => return self.rule_chosen(rule),
            "add-to" => return self.show_playlist_picker(),
            "rate" => return self.show_rating_picker(),
            "back" => {
                let state = self.menu.borrow();
                let menu = self.ui.global::<ui::Menu>();
                menu.set_title(state.title.as_str().into());
                menu.set_entries(ModelRc::new(VecModel::from(state.root.clone())));
                return;
            }
            _ => {}
        }
        self.close_menu();
        let target = std::mem::take(&mut self.menu.borrow_mut().target);
        let name = match &target {
            Target::Song { song, .. } => song.title.clone(),
            Target::Songs { songs, .. } => format!("{} songs", songs.len()),
            Target::Album { title, .. } => title.clone(),
            Target::Artist { name, .. } | Target::Playlist { name, .. } => name.clone(),
            Target::Request { id: album, title } => {
                if id == "cancel-request" {
                    self.cancel_request(*album, title.clone());
                }
                return;
            }
            Target::None => return,
        };
        match (id, &target) {
            ("play", _) => self.play_target(target, name),
            ("play-next", _) => self.with_songs(target, |ctl, songs| {
                ctl.enqueue(songs, true);
                ctl.toast("Playing next");
            }),
            ("add-queue", _) => self.with_songs(target, |ctl, songs| {
                ctl.enqueue(songs, false);
                ctl.toast("Added to queue");
            }),
            ("pl-new", _) => self.with_songs(target, |ctl, songs| ctl.prompt_new_playlist(songs)),
            ("save-playlist", Target::Songs { songs, .. }) => self.prompt_new_playlist(songs.clone()),
            ("export", Target::Playlist { id, name }) => self.export_playlist(id.clone(), name.clone()),
            ("star", Target::Song { song, .. }) | ("unstar", Target::Song { song, .. }) => {
                self.toggle_song_star(song)
            }
            ("go-album", Target::Song { song, .. }) => self.navigate(Route::Album(song.album_id.clone())),
            ("details", Target::Song { song, .. }) => self.show_song_details(song),
            ("go-artist", Target::Song { song, .. }) => {
                self.navigate(Route::Artist(song.artist_id.clone()))
            }
            ("radio", Target::Song { song, .. }) => self.start_radio(song.clone()),
            ("remove-playlist", Target::Song { playlist: Some((playlist, index)), .. }) => {
                self.remove_from_playlist(playlist.clone(), *index)
            }
            ("remove-queue", Target::Song { upcoming: Some(offset), .. }) => self.remove_upcoming(*offset),
            ("star-all", Target::Songs { songs, .. }) | ("unstar-all", Target::Songs { songs, .. }) => {
                let like = id == "star-all";
                // Only the songs this changes anything for, each once.
                let mut ids: Vec<String> =
                    songs.iter().filter(|song| self.is_starred(song) != like).map(|song| song.id.clone()).collect();
                ids.sort_unstable();
                ids.dedup();
                let changed = ids.len();
                self.set_song_stars(ids, like);
                self.toast(match (changed, like) {
                    (1, true) => "1 song added to Liked songs".to_string(),
                    (n, true) => format!("{n} songs added to Liked songs"),
                    (1, false) => "1 song removed from Liked songs".to_string(),
                    (n, false) => format!("{n} songs removed from Liked songs"),
                });
            }
            ("remove-playlist", Target::Songs { playlist: Some((playlist, rows)), .. }) => {
                self.remove_rows_from_playlist(playlist.clone(), rows)
            }
            ("download", _) => {
                if let Target::Playlist { id, .. } = &target {
                    self.keep_downloaded(id, true);
                }
                self.with_songs(target, |ctl, songs| ctl.download_songs(songs))
            }
            ("remove-download", Target::Song { song, .. }) => {
                self.remove_downloads(std::slice::from_ref(&song.id));
                self.toast("Download removed");
            }
            ("pin", Target::Playlist { id, .. }) | ("unpin", Target::Playlist { id, .. }) => {
                {
                    let mut settings = self.settings.borrow_mut();
                    let was_pinned = settings.pinned_playlists.contains(id);
                    settings.pinned_playlists.retain(|p| p != id);
                    if !was_pinned {
                        settings.pinned_playlists.push(id.clone());
                    }
                    settings.pins_at = chrono::Utc::now().timestamp();
                }
                self.save_settings();
                self.show_playlists();
                self.share_soon();
            }
            ("cancel-download", Target::Song { song, .. }) => {
                self.cancel_downloads(std::slice::from_ref(&song.id));
                self.toast("Download cancelled");
            }
            ("remove-download", Target::Songs { songs, .. }) => {
                let ids: Vec<String> = songs.iter().map(|song| song.id.clone()).collect();
                self.remove_downloads(&ids);
                self.toast("Downloads removed");
            }
            ("rename", Target::Playlist { id, name }) => {
                *self.prompt.borrow_mut() = Some(PromptAction::RenamePlaylist(id.clone()));
                self.open_prompt("Rename playlist", "", "Save", Some(name), false);
            }
            ("delete", Target::Playlist { id, name }) => {
                *self.prompt.borrow_mut() = Some(PromptAction::DeletePlaylist(id.clone()));
                let message = format!("“{name}” will be deleted from your Navidrome server. This can't be undone.");
                self.open_prompt("Delete this playlist?", &message, "Delete playlist", None, true);
            }
            (other, _) => {
                if let Some(playlist_id) = other.strip_prefix("pl:") {
                    let playlist_id = playlist_id.to_string();
                    self.with_songs(target, move |ctl, songs| ctl.add_to_playlist(playlist_id, songs));
                }
            }
        }
    }

    // ---- prompts -------------------------------------------------------------------------------

    pub fn open_prompt(&self, title: &str, message: &str, confirm: &str, text: Option<&str>, danger: bool) {
        let prompt = self.ui.global::<ui::Prompt>();
        prompt.set_cancel_label("Cancel".into());
        prompt.set_title(title.into());
        prompt.set_message(message.into());
        prompt.set_confirm_label(confirm.into());
        prompt.set_wants_text(text.is_some());
        prompt.set_text(text.unwrap_or_default().into());
        prompt.set_danger(danger);
        prompt.set_open(true);
    }

    /// The menu on the + beside "Playlists".
    pub fn open_playlist_menu(&self, x: f32, y: f32) {
        let icons = self.ui.global::<ui::Icons>();
        let entries = vec![
            self.entry("new-playlist", "New playlist", icons.get_list_plus()),
            self.entry("new-mix", "New mix from rules", icons.get_list_music()),
            self.entry("import-spotify", "Import from Spotify", icons.get_download()),
        ];
        self.open_menu(Target::None, "", entries, x, y);
    }

    // ---- mixes made to rules -------------------------------------------------------------------

    /// The menu on a mix's page.
    fn mix_menu(&self, rule: Option<u64>, songs: Vec<Song>, title: &str, x: f32, y: f32) {
        let icons = self.ui.global::<ui::Icons>();
        let mut entries = Vec::new();
        if !songs.is_empty() {
            entries.push(self.entry("save-playlist", "Save as playlist", icons.get_list_plus()));
        }
        let rules = rule.and_then(|id| self.settings.borrow().rule_mixes.iter().find(|rules| rules.id == id).cloned());
        self.menu_rule.set(rules.as_ref().map(|rules| rules.id));
        if let Some(rules) = rules {
            use crate::mixes::{ADDED, ORDERS, PLAYED, SIZES};
            if !entries.is_empty() {
                entries.push(self.separator());
            }
            let pick = |index: u8, count: usize| (index as usize).min(count - 1);
            let years = match (rules.from_year, rules.to_year) {
                (0, 0) => "any".to_string(),
                (from, 0) => format!("from {from}"),
                (0, to) => format!("up to {to}"),
                (from, to) => format!("{from} to {to}"),
            };
            let stars = match rules.rating {
                0 => "any".to_string(),
                5 => "5 stars".to_string(),
                n => format!("{n} or more"),
            };
            let mut choice = |id: &str, label: String, submenu: bool| {
                let mut entry = self.entry(id, &label, slint::Image::default());
                entry.submenu = submenu;
                entries.push(entry);
            };
            choice("rule:genre", format!("Genre: {}", if rules.genre.trim().is_empty() { "any" } else { rules.genre.trim() }), false);
            choice("rule:rating", format!("Rating: {stars}"), true);
            choice("rule:liked", format!("Liked songs only: {}", if rules.liked { "yes" } else { "no" }), false);
            choice("rule:years", format!("Years: {years}"), false);
            choice("rule:played", format!("Last played: {}", PLAYED[pick(rules.played, PLAYED.len())].0.to_lowercase()), true);
            choice("rule:added", format!("Added: {}", ADDED[pick(rules.added, ADDED.len())].0.to_lowercase()), true);
            choice("rule:order", format!("Order: {}", ORDERS[pick(rules.order, ORDERS.len())].to_lowercase()), true);
            choice("rule:size", format!("Length: {} songs", SIZES[pick(rules.size, SIZES.len())]), true);
            entries.push(self.separator());
            entries.push(self.entry("rule:rename", "Rename", icons.get_pencil()));
            let mut delete = self.entry("rule:delete", "Delete mix", icons.get_trash_2());
            delete.danger = true;
            entries.push(delete);
        }
        if !entries.is_empty() {
            self.open_menu(Target::Songs { songs, playlist: None }, title, entries, x, y);
        }
    }

    /// Something was chosen from a rule mix's menu.
    fn rule_chosen(self: &Rc<Self>, id: &str) {
        use crate::mixes::{ADDED, ORDERS, PLAYED, SIZES};
        let Some(rule) = self.menu_rule.get() else { return self.close_menu() };
        let Some(rules) = self.settings.borrow().rule_mixes.iter().find(|rules| rules.id == rule).cloned() else {
            return self.close_menu();
        };
        // A choice from one of the lists below: "rule-set:<which>:<number>".
        if let Some((field, value)) = id.strip_prefix("rule-set:").and_then(|rest| rest.split_once(':')) {
            let value: u8 = value.parse().unwrap_or(0);
            self.close_menu();
            return self.change_rule(rule, |rules| match field {
                "rating" => rules.rating = value,
                "played" => rules.played = value,
                "added" => rules.added = value,
                "order" => rules.order = value,
                _ => rules.size = value,
            });
        }
        // A list of choices takes the menu's place, with the one in force ticked.
        let list = |title: &str, field: &str, current: u8, names: Vec<String>| {
            let icons = self.ui.global::<ui::Icons>();
            let mut entries = vec![self.entry("back", "Back", icons.get_chevron_left()), self.separator()];
            for (index, name) in names.iter().enumerate() {
                let icon = if index == current as usize { icons.get_check() } else { slint::Image::default() };
                entries.push(self.entry(&format!("rule-set:{field}:{index}"), name, icon));
            }
            let menu = self.ui.global::<ui::Menu>();
            menu.set_title(title.into());
            menu.set_entries(ModelRc::new(VecModel::from(entries)));
        };
        let ask = |field: &'static str, title: &str, message: &str, now: String| {
            self.close_menu();
            *self.prompt.borrow_mut() = Some(PromptAction::RuleText { rule, field });
            self.open_prompt(title, message, "Set", Some(&now), false);
        };
        match id {
            "rule:rating" => list("Rating", "rating", rules.rating, ["Any", "1 star or more", "2 or more", "3 or more", "4 or more", "5 stars"].map(String::from).to_vec()),
            "rule:played" => list("Last played", "played", rules.played, PLAYED.iter().map(|choice| choice.0.to_string()).collect()),
            "rule:added" => list("Added to the library", "added", rules.added, ADDED.iter().map(|choice| choice.0.to_string()).collect()),
            "rule:order" => list("Order", "order", rules.order, ORDERS.iter().map(|order| order.to_string()).collect()),
            "rule:size" => list("Length", "size", rules.size, SIZES.iter().map(|size| format!("{size} songs")).collect()),
            "rule:liked" => {
                self.close_menu();
                self.change_rule(rule, |rules| rules.liked = !rules.liked);
            }
            "rule:genre" => ask("genre", "Genre", "Part of a genre's name, such as “rock”. Leave it empty for any genre.", rules.genre.clone()),
            "rule:years" => {
                let now = match (rules.from_year, rules.to_year) {
                    (0, 0) => String::new(),
                    (from, 0) => format!("{from}-"),
                    (0, to) => format!("-{to}"),
                    (from, to) => format!("{from}-{to}"),
                };
                ask("years", "Years", "A year, or a range such as “1990-1999”, “2010-” or “-1985”. Leave it empty for any year.", now)
            }
            "rule:rename" => ask("name", "Rename the mix", "", rules.name.clone()),
            "rule:delete" => {
                self.close_menu();
                {
                    self.settings.borrow_mut().rule_mixes.retain(|rules| rules.id != rule);
                }
                self.rule_mixes_changed(rule);
                self.toast(format!("Deleted “{}”", rules.name));
                self.navigate(Route::Home);
            }
            _ => self.close_menu(),
        }
    }

    /// Something typed for a rule. An empty answer clears the rule (but not the name).
    fn set_rule_text(self: &Rc<Self>, rule: u64, field: &str, text: &str) {
        let text = text.trim().to_string();
        self.change_rule(rule, |rules| match field {
            "genre" => rules.genre = text.clone(),
            "name" => {
                if !text.is_empty() {
                    rules.name = text.clone();
                }
            }
            _ => {
                // "1990-1999", "2010-", "-1985" or one year.
                let year = |part: &str| part.trim().parse::<u32>().ok().filter(|year| (1000..=9999).contains(year)).unwrap_or(0);
                (rules.from_year, rules.to_year) = match text.split_once(['-', '–']) {
                    Some((from, to)) => (year(from), year(to)),
                    None => (year(&text), year(&text)),
                };
            }
        });
    }

    fn new_rule_mix(self: &Rc<Self>, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let id = chrono::Utc::now().timestamp_millis() as u64;
        self.settings.borrow_mut().rule_mixes.push(crate::mixes::Rules { id, name: name.to_string(), size: 1, ..Default::default() });
        self.rule_mixes_changed(id);
        self.navigate(Route::Mix(format!("rule:{id}")));
        self.toast("Set its rules from the “…” menu");
    }

    fn change_rule(self: &Rc<Self>, rule: u64, change: impl FnOnce(&mut crate::mixes::Rules)) {
        if let Some(rules) = self.settings.borrow_mut().rule_mixes.iter_mut().find(|rules| rules.id == rule) {
            change(rules);
        }
        self.rule_mixes_changed(rule);
    }

    /// The rules of a mix changed (or it was made or deleted): keeps them, and has the mix
    /// and the row on Home that lists it made again.
    fn rule_mixes_changed(self: &Rc<Self>, rule: u64) {
        self.save_settings();
        crate::mixes::set_rules(self.settings.borrow().rule_mixes.clone());
        self.invalidate(&format!("mix:rule:{rule}"));
        self.invalidate("home");
        if self.current_route() == Some(Route::Mix(format!("rule:{rule}"))) {
            self.refresh();
        }
    }

    /// Everything the server says about a song, laid out to be read.
    fn show_song_details(&self, song: &Song) {
        let mut lines: Vec<String> = Vec::new();
        let mut line = |name: &str, value: String| {
            if !value.is_empty() {
                lines.push(format!("{name}: {value}"));
            }
        };
        let number = |n: u32, unit: &str| if n > 0 { format!("{n}{unit}") } else { String::new() };
        let date = |text: &str| {
            let at = crate::added::parse_time(text);
            if at > 0 { chrono::DateTime::from_timestamp(at, 0).map(|at| at.format("%-d %B %Y").to_string()).unwrap_or_default() } else { String::new() }
        };
        let artists = if song.artists.len() > 1 {
            song.artists.iter().map(|credit| credit.name.as_str()).collect::<Vec<_>>().join(", ")
        } else {
            song.artist_label().to_string()
        };
        line(if song.artists.len() > 1 { "Artists" } else { "Artist" }, artists);
        line("Album", song.album.clone());
        line("Track", match (song.disc_number, song.track) {
            (_, 0) => String::new(),
            (disc, track) if disc > 1 => format!("{track}, disc {disc}"),
            (_, track) => track.to_string(),
        });
        line("Year", number(song.year, ""));
        line("Genre", song.genre.clone());
        line("Length", if song.duration > 0 { crate::controller::clock(song.duration) } else { String::new() });
        let mut format = vec![song.suffix.to_uppercase()];
        if song.sampling_rate > 0 {
            let khz = song.sampling_rate as f32 / 1000.0;
            format.push(if khz.fract() == 0.0 { format!("{khz:.0} kHz") } else { format!("{khz:.1} kHz") });
        }
        format.extend([number(song.bit_depth, "-bit"), number(song.bit_rate, " kbps")].into_iter().filter(|part| !part.is_empty()));
        line("Format", format.into_iter().filter(|part| !part.is_empty()).collect::<Vec<_>>().join(", "));
        line("Size", if song.size > 0 { format!("{:.1} MB", song.size as f64 / (1024.0 * 1024.0)) } else { String::new() });
        if let Some(tags) = &song.replay_gain {
            let db = |gain: Option<f32>| gain.map(|gain| format!("{gain:+.1} dB"));
            let parts: Vec<String> = [db(tags.track_gain).map(|g| format!("song {g}")), db(tags.album_gain).map(|g| format!("album {g}"))].into_iter().flatten().collect();
            line("Levelling tags", parts.join(", "));
        } else {
            line("Levelling tags", "none".into());
        }
        line("Played", match song.play_count {
            0 => "never".into(),
            1 => format!("once, on {}", date(&song.played)),
            n => format!("{n} times, last on {}", date(&song.played)),
        });
        line("Your rating", match self.rating_of(song) {
            0 => String::new(),
            1 => "1 star".into(),
            n => format!("{n} stars"),
        });
        line("Added to the library", date(&song.created));
        if let Some((kind, kbps)) = self.downloads.borrow().saved_as(song) {
            line("Downloaded as", format!("{}, about {kbps} kbps", kind.to_uppercase()));
        }
        self.close_menu();
        *self.prompt.borrow_mut() = None;
        let prompt = self.ui.global::<ui::Prompt>();
        self.open_prompt(&song.title, &lines.join("\n"), "Close", None, false);
        prompt.set_cancel_label("".into());
    }

    /// Asks which folder downloads should be kept in, then moves them there.
    pub fn change_downloads_folder(self: &Rc<Self>) {
        #[cfg(windows)]
        match crate::picker::pick_folder(&self.ui, "Choose a folder for downloads") {
            Some(Some(folder)) => return self.move_downloads(folder),
            Some(None) => return,
            None => {}
        }
        // Without the system's folder window: ask for the place by name.
        *self.prompt.borrow_mut() = Some(PromptAction::DownloadsFolder);
        self.open_prompt("Downloads folder", "Type the folder to keep downloads in.", "Move downloads", Some(""), false);
    }

    /// Asks which folder songs should be kept in as they play.
    pub fn change_songs_folder(self: &Rc<Self>) {
        #[cfg(windows)]
        match crate::picker::pick_folder(&self.ui, "Choose a folder for kept songs") {
            Some(Some(folder)) => return self.set_songs_folder(Some(folder)),
            Some(None) => return,
            None => {}
        }
        *self.prompt.borrow_mut() = Some(PromptAction::SongsFolder);
        self.open_prompt("Kept songs folder", "Type the folder to keep songs in as they play.", "Use this folder", Some(""), false);
    }

    /// Notes where songs are to be kept as they play (`None` for the app's own folder). The
    /// app goes on using the folder it started with until it's next opened; nothing is
    /// moved, since what's kept is only ever a copy of what's on the server.
    pub fn set_songs_folder(self: &Rc<Self>, folder: Option<std::path::PathBuf>) {
        if let Some(folder) = &folder {
            if std::fs::create_dir_all(folder).is_err() || !folder.is_dir() {
                return self.toast(format!("{} can't be used as a folder.", folder.display()));
            }
        }
        let text = folder.filter(|folder| *folder != self.paths.songs).map(|folder| folder.display().to_string()).unwrap_or_default();
        if self.settings.borrow().songs_dir == text {
            return;
        }
        self.settings.borrow_mut().songs_dir = text.clone();
        self.save_settings();
        self.push_settings();
        self.toast(if text.is_empty() {
            "Kept songs go back to the app's own folder the next time it starts.".to_string()
        } else {
            format!("Kept songs will go in {text} from the next time the app starts.")
        });
    }

    pub fn prompt_import_path(&self) {
        *self.prompt.borrow_mut() = Some(PromptAction::ImportPath);
        self.open_prompt(
            "Import from Spotify",
            "Type the folder your playlists were exported to, or the path of one file.",
            "Import",
            Some(""),
            false,
        );
    }

    pub fn prompt_new_playlist(self: &Rc<Self>, songs: Vec<Song>) {
        *self.prompt.borrow_mut() = Some(PromptAction::CreatePlaylist(songs));
        self.open_prompt("Name your playlist", "", "Create playlist", Some(""), false);
    }

    /// Puts the question away and gives the keyboard back to the app's shortcuts.
    fn close_prompt(&self) {
        self.ui.global::<ui::Prompt>().set_open(false);
        let app = self.ui.global::<ui::App>();
        app.set_main_focus(app.get_main_focus().wrapping_add(1));
    }

    fn prompt_confirmed(self: &Rc<Self>, text: &str) {
        self.close_prompt();
        let Some(action) = self.prompt.borrow_mut().take() else { return };
        let name = text.trim().to_string();
        match action {
            PromptAction::AddDuplicates { playlist_id, fresh, .. } => {
                if !fresh.is_empty() {
                    self.add_songs_to_playlist(playlist_id, fresh);
                }
            }
            PromptAction::RequestAlbum { album, title, song } => self.request_album(album, title, song),
            PromptAction::AddArtist { artist, name } => self.add_artist(artist, name),
            PromptAction::RequestMissing => self.request_missing(),
            PromptAction::ImportPath => {
                if !name.is_empty() {
                    self.run_import(vec![std::path::PathBuf::from(name.trim_matches('"'))]);
                }
            }
            PromptAction::SavePreset => self.save_preset(&name),
            PromptAction::NewMix => self.new_rule_mix(&name),
            PromptAction::RuleText { rule, field } => self.set_rule_text(rule, field, &name),
            PromptAction::SongsFolder => {
                if !name.is_empty() {
                    self.set_songs_folder(Some(std::path::PathBuf::from(name.trim_matches('"'))));
                }
            }
            PromptAction::DownloadsFolder => {
                if !name.is_empty() {
                    self.move_downloads(std::path::PathBuf::from(name.trim_matches('"')));
                }
            }
            PromptAction::CreatePlaylist(songs) => {
                if name.is_empty() {
                    return;
                }
                let ids: Vec<String> = songs.iter().map(|s| s.id.clone()).collect();
                let label = name.clone();
                self.request(
                    move |client| client.create_playlist(&name, &ids).map(|playlist| (playlist, ids)),
                    move |ctl, result| match result {
                        Ok((playlist, ids)) => {
                            ctl.added.borrow_mut().create(&playlist.id, &ids, chrono::Utc::now().timestamp());
                            ctl.share_soon();
                            ctl.toast(format!("Created “{label}”"));
                            ctl.reload_playlists();
                        }
                        Err(e) => ctl.toast(e.to_string()),
                    },
                );
            }
            PromptAction::RenamePlaylist(id) => {
                if name.is_empty() {
                    return;
                }
                let key = format!("playlist:{id}");
                let page = Route::Playlist(id.clone());
                self.request(
                    move |client| client.rename_playlist(&id, &name),
                    move |ctl, result| match result {
                        Ok(()) => {
                            ctl.invalidate(&key);
                            ctl.reload_playlists();
                            if ctl.current_route() == Some(page) {
                                ctl.refresh();
                            }
                        }
                        Err(e) => ctl.toast(e.to_string()),
                    },
                );
            }
            PromptAction::DeletePlaylist(id) => {
                let key = format!("playlist:{id}");
                let page = Route::Playlist(id.clone());
                self.request(
                    move |client| client.delete_playlist(&id),
                    move |ctl, result| match result {
                        Ok(()) => {
                            ctl.invalidate(&key);
                            ctl.update_list_prefs(&key, |prefs| *prefs = Default::default());
                            if let Route::Playlist(id) = &page {
                                ctl.added.borrow_mut().forget(id);
                            }
                            ctl.toast("Playlist deleted");
                            ctl.reload_playlists();
                            if ctl.current_route() == Some(page) {
                                ctl.navigate(Route::Home);
                            }
                        }
                        Err(e) => ctl.toast(e.to_string()),
                    },
                );
            }
        }
    }

    // ---- playlists -----------------------------------------------------------------------------

    /// Adds songs to a playlist, first asking what to do about any that are already in it.
    pub fn add_to_playlist(self: &Rc<Self>, playlist_id: String, songs: Vec<Song>) {
        let id = playlist_id.clone();
        self.request(
            move |client| client.playlist(&id),
            move |ctl, result| {
                // If the playlist can't be read, add without the check: the add itself will
                // report whatever is wrong.
                let Ok(playlist) = result else { return ctl.add_songs_to_playlist(playlist_id, songs) };
                // Note what's in it now, so the songs about to be added can be told apart.
                ctl.added.borrow_mut().notice(&playlist_id, &playlist.songs, chrono::Utc::now().timestamp());
                let present: std::collections::HashSet<&str> = playlist.songs.iter().map(|s| s.id.as_str()).collect();
                let fresh: Vec<Song> = songs.iter().filter(|s| !present.contains(s.id.as_str())).cloned().collect();
                let repeats = songs.len() - fresh.len();
                if repeats == 0 {
                    return ctl.add_songs_to_playlist(playlist_id, songs);
                }
                let name = &playlist.name;
                // The cautious answer is the highlighted one, as it is in Spotify.
                let (title, message, add, skip) = match (songs.len(), fresh.len()) {
                    (1, _) => ("Already added", format!("This is already in your “{name}” playlist."), "Add anyway", "Don't add"),
                    (_, 0) => ("Already added", format!("These are already in your “{name}” playlist."), "Add anyway", "Don't add"),
                    _ => (
                        "Some already added",
                        format!("{repeats} of these {} songs are already in your “{name}” playlist.", songs.len()),
                        "Add all",
                        "Add new ones",
                    ),
                };
                *ctl.prompt.borrow_mut() = Some(PromptAction::AddDuplicates { playlist_id, all: songs, fresh });
                ctl.open_prompt(title, &message, skip, None, false);
                ctl.ui.global::<ui::Prompt>().set_cancel_label(add.into());
            },
        );
    }

    fn add_songs_to_playlist(self: &Rc<Self>, playlist_id: String, songs: Vec<Song>) {
        let name = self
            .playlists
            .borrow()
            .iter()
            .find(|p| p.id == playlist_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "playlist".into());
        let ids: Vec<String> = songs.iter().map(|s| s.id.clone()).collect();
        let key = format!("playlist:{playlist_id}");
        let page = Route::Playlist(playlist_id.clone());
        self.request(
            move |client| client.add_to_playlist(&playlist_id, &ids).map(|()| (playlist_id, ids)),
            move |ctl, result| match result {
                Ok((playlist_id, ids)) => {
                    ctl.added.borrow_mut().record(&playlist_id, &ids, chrono::Utc::now().timestamp());
                    ctl.share_soon();
                    ctl.invalidate(&key);
                    ctl.toast(format!("Added to “{name}”"));
                    if ctl.current_route() == Some(page) {
                        // The playlist is on screen: show the new songs in it.
                        ctl.refresh();
                    } else {
                        ctl.reload_playlists();
                    }
                }
                Err(e) => ctl.toast(e.to_string()),
            },
        );
    }

    /// Takes songs out of the playlist on screen. They disappear at once; the server is told
    /// in the background, and the page is reloaded from it if that fails.
    fn remove_rows_from_playlist(self: &Rc<Self>, playlist_id: String, rows: &[usize]) {
        let mut rows: Vec<usize> = rows.to_vec();
        rows.sort_unstable();
        rows.dedup();
        let before = {
            let mut page = self.page.borrow_mut();
            if page.playlist.as_ref().is_none_or(|p| p.id != playlist_id) {
                return;
            }
            rows.retain(|&row| row < page.songs.len());
            let before = page.songs.clone();
            let mut index = 0;
            page.songs.retain(|_| {
                index += 1;
                rows.binary_search(&(index - 1)).is_err()
            });
            page.can_play = !page.songs.is_empty();
            before
        };
        if rows.is_empty() {
            return;
        }
        // Which songs go, said by the songs themselves and not by where they sit: the
        // playlist may have changed on the server since this page loaded.
        let ids: Vec<String> = before.iter().map(|song| song.id.clone()).collect();
        let edit = crate::playlists::Edit::Remove(rows.iter().map(|&row| crate::playlists::spot(&ids, row)).collect());
        *self.undo.borrow_mut() = Some((
            std::time::Instant::now(),
            crate::controller::Undo::Removed { playlist: playlist_id.clone(), before, places: rows.clone() },
        ));
        let key = format!("playlist:{playlist_id}");
        // If the queue is playing from this playlist, the songs leave it too.
        let songs = self.page.borrow().songs.clone();
        self.source_list_now(&key, &songs);
        self.rebuild_tracks();
        if let Some(route) = self.current_route() {
            let data = self.page.borrow().clone();
            self.remember(&route.cache_key(), std::time::Instant::now(), data);
        }
        self.toast_with_undo(if rows.len() == 1 {
            "Removed from the playlist".to_string()
        } else {
            format!("{} songs removed from the playlist", rows.len())
        });
        self.edit_playlist(playlist_id, ids, edit);
    }

    /// Writes a playlist out as a spreadsheet file: where the system's save window says, or
    /// (without one) into the app's own folder.
    fn export_playlist(self: &Rc<Self>, id: String, name: String) {
        let file = format!("{}.csv", crate::playlists::file_name(&name));
        #[cfg(windows)]
        let chosen = match crate::picker::pick_save(&self.ui, &file) {
            Some(None) => return,
            Some(path) => path,
            None => None,
        };
        #[cfg(not(windows))]
        let chosen: Option<std::path::PathBuf> = None;
        let path = chosen.unwrap_or_else(|| self.paths.exports.join(&file));
        self.request(
            move |client| client.playlist(&id).map(|playlist| (id, playlist)),
            move |ctl, result| {
                let (id, playlist) = match result {
                    Ok(found) => found,
                    Err(e) => return ctl.toast(format!("Couldn't export “{name}”. {e}")),
                };
                let added = ctl.added.borrow().dates(&id, &playlist.songs);
                let text = crate::playlists::csv(&playlist.songs, &added);
                if let Some(folder) = path.parent() {
                    let _ = std::fs::create_dir_all(folder);
                }
                // With a mark at the start that tells a spreadsheet the text is UTF-8.
                match std::fs::write(&path, format!("\u{feff}{text}")) {
                    Ok(()) => ctl.toast(format!("Exported “{name}” to {}", path.display())),
                    Err(e) => ctl.toast(format!("Couldn't write {}. {e}", path.display())),
                }
            },
        );
    }

    fn remove_from_playlist(self: &Rc<Self>, playlist_id: String, index: usize) {
        self.remove_rows_from_playlist(playlist_id, &[index]);
    }

    fn start_radio(self: &Rc<Self>, seed: Song) {
        let id = seed.id.clone();
        self.request(
            move |client| client.similar_songs(&id, 60),
            move |ctl, result| {
                let similar = result.unwrap_or_default();
                if similar.is_empty() {
                    ctl.toast("No similar songs found. Radio needs Last.fm set up in Navidrome.");
                    return;
                }
                let context = format!("{} radio", seed.title);
                let mut songs = vec![seed.clone()];
                songs.extend(similar.into_iter().filter(|s| s.id != seed.id));
                ctl.play_songs(songs, 0, &context, false);
            },
        );
    }

    pub fn play_card(self: &Rc<Self>, card: &ui::Card) {
        let (id, title) = (card.id.to_string(), card.title.to_string());
        // The play button on what's already playing pauses and resumes it.
        let key = match card.kind {
            ui::CardKind::Album => format!("album:{id}"),
            ui::CardKind::Playlist => format!("playlist:{id}"),
            ui::CardKind::Mix => format!("mix:{id}"),
            _ => String::new(),
        };
        if self.is_playing_from(&key) {
            return self.toggle_play();
        }
        let target = match card.kind {
            ui::CardKind::Album => Target::Album { id, title: title.clone() },
            ui::CardKind::Playlist => Target::Playlist { id, name: title.clone() },
            ui::CardKind::Artist => Target::Artist { id, name: title.clone() },
            ui::CardKind::Genre | ui::CardKind::OutsideArtist => return,
            // The button on an album that isn't in the library asks for it.
            ui::CardKind::Request => {
                let by = crate::lidarr::offered(&id).map(|offer| offer.artist).unwrap_or_default();
                return self.prompt_request(&id, &title, &by, "", "");
            }
            ui::CardKind::Mix => {
                // Made on the spot from the app's copy of the library.
                let Some(mix) = crate::search::current().and_then(|index| crate::mixes::one(&index, &id)) else { return };
                let list: Vec<String> = mix.songs.iter().map(|song| song.id.clone()).collect();
                let rows = (0..mix.songs.len()).collect();
                self.play_from.replace(Some(crate::playback::PlayFrom { key: format!("mix:{id}"), rows, list }));
                return self.play_songs(mix.songs, 0, &mix.title, true);
            }
        };
        self.play_target(target, title);
    }

    /// Plays an album, playlist or the like from the top (or shuffled, if it's set to be).
    fn play_target(self: &Rc<Self>, target: Target, name: String) {
        let source = target.route_key();
        let playlist = match &target {
            Target::Playlist { id, .. } => Some(id.clone()),
            _ => None,
        };
        self.with_songs(target, move |ctl, all| {
            // A playlist plays in the order its page shows it in.
            let rows: Vec<usize> = match &playlist {
                Some(id) => ctl.playlist_order(id, &all),
                None => (0..all.len()).collect(),
            };
            let songs: Vec<Song> = rows.iter().filter_map(|&row| all.get(row).cloned()).collect();
            let list = all.iter().map(|song| song.id.clone()).collect();
            ctl.play_from.replace(source.map(|key| crate::playback::PlayFrom { key, rows, list }));
            ctl.play_songs(songs, 0, &name, true)
        });
    }

    // ---- likes ---------------------------------------------------------------------------------

    pub fn toggle_song_star(self: &Rc<Self>, song: &Song) {
        if song.id.is_empty() {
            // A row that only names a song (one an import couldn't find), not one in the library.
            return;
        }
        if !song.radio_url.is_empty() {
            return self.toast("Radio stations can't be added to Liked songs");
        }
        let starred = !self.is_starred(song);
        self.set_song_stars(vec![song.id.clone()], starred);
        self.toast(if starred { "Added to Liked songs" } else { "Removed from Liked songs" });
    }

    /// Likes or unlikes these songs. Unliked songs leave the Liked songs page if it's showing.
    fn set_song_stars(self: &Rc<Self>, ids: Vec<String>, starred: bool) {
        for id in &ids {
            self.set_star("id", id.clone(), starred);
        }
        if starred || ids.is_empty() || !self.page.borrow().liked_songs {
            return;
        }
        let gone: std::collections::HashSet<&str> = ids.iter().map(|id| id.as_str()).collect();
        let (songs, meta) = {
            let mut page = self.page.borrow_mut();
            page.songs.retain(|song| !gone.contains(song.id.as_str()));
            page.can_play = !page.songs.is_empty();
            page.meta = crate::controller::count(page.songs.len() as u32, "song");
            (page.songs.clone(), page.meta.clone())
        };
        // If the queue is playing Liked songs, they leave it too.
        self.source_list_now("liked", &songs);
        let view = self.ui.global::<ui::View>();
        view.set_meta(meta.into());
        view.set_can_play(self.page.borrow().can_play);
        self.rebuild_tracks();
    }

    pub fn toggle_page_star(self: &Rc<Self>) {
        let Some((_, id)) = self.page.borrow().star.clone() else { return };
        let starred = !self.ui.global::<ui::View>().get_starred();
        self.set_star("albumId", id, starred);
        self.toast(if starred { "Added to liked albums" } else { "Removed from liked albums" });
    }

    fn apply_star(self: &Rc<Self>, kind: &str, id: &str, starred: bool) {
        self.stars.borrow_mut().insert(id.to_string(), starred);
        if kind == "id" {
            crate::search::note_liked(id, starred);
            self.song_star_changed(id, starred);
        } else if self.page.borrow().star.as_ref().is_some_and(|(_, page_id)| page_id == id) {
            self.ui.global::<ui::View>().set_starred(starred);
        }
    }

    fn set_star(self: &Rc<Self>, kind: &'static str, id: String, starred: bool) {
        self.apply_star(kind, &id, starred);
        // Lists of liked things are now out of date.
        self.invalidate("liked");
        self.invalidate("albums:");
        let change = crate::offline::Pending::Star { kind: kind.to_string(), id: id.clone(), starred };
        self.submit(change, move |ctl, reason| {
            ctl.apply_star(kind, &id, !starred);
            ctl.toast(format!("That didn't save. {reason}"));
            // A song taken off the Liked songs page is still liked after all: show it again.
            if ctl.page.borrow().liked_songs {
                ctl.refresh();
            }
        });
    }

    // ---- callbacks from the interface ----------------------------------------------------------

    pub fn wire_actions(self: &Rc<Self>) {
        let menu = self.ui.global::<ui::Menu>();
        menu.on_activated(|id| {
            with(|ctl| ctl.menu_activated(&id));
        });
        menu.on_dismissed(|| {
            with(|ctl| ctl.close_menu());
        });
        menu.on_move(|delta| {
            with(|ctl| ctl.move_menu_cursor(delta));
        });
        menu.on_accept(|| {
            with(|ctl| {
                let menu = ctl.ui.global::<ui::Menu>();
                let chosen = usize::try_from(menu.get_cursor()).ok().and_then(|at| menu.get_entries().row_data(at));
                if let Some(entry) = chosen.filter(|entry| !entry.separator) {
                    ctl.menu_activated(&entry.id);
                }
            });
        });
        let prompt = self.ui.global::<ui::Prompt>();
        prompt.on_confirmed(|text| {
            with(|ctl| ctl.prompt_confirmed(&text));
        });
        prompt.on_cancelled(|| {
            with(|ctl| {
                let action = ctl.prompt.borrow_mut().take();
                ctl.close_prompt();
                // For this question the second button is an answer, not a way out: add them
                // all, repeats included.
                if let Some(PromptAction::AddDuplicates { playlist_id, all, .. }) = action {
                    ctl.add_songs_to_playlist(playlist_id, all);
                }
            });
        });
        prompt.on_dismissed(|| {
            with(|ctl| {
                ctl.prompt.borrow_mut().take();
                ctl.close_prompt();
            });
        });
    }
}
