//! Context menus, likes, and playlist management.

use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, VecModel};

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
    Album { id: String, title: String },
    Artist { id: String, name: String },
    Playlist { id: String, name: String },
}

#[derive(Default)]
pub struct MenuState {
    target: Target,
    /// The top-level entries, to return to from the playlist picker.
    root: Vec<ui::MenuEntry>,
    title: String,
}

pub enum PromptAction {
    CreatePlaylist(Vec<Song>),
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

    fn song_menu(&self, song: Song, playlist: Option<(String, usize)>, upcoming: Option<usize>, x: f32, y: f32) {
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
        entries.push(self.separator());
        if !song.album_id.is_empty() {
            entries.push(self.entry("go-album", "Go to album", icons.get_disc_album()));
        }
        if !song.artist_id.is_empty() {
            entries.push(self.entry("go-artist", "Go to artist", icons.get_user_round()));
        }
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

    /// `row` is the row as shown, which a filter or sort can put anywhere in the list.
    pub fn open_track_menu(self: &Rc<Self>, row: usize, x: f32, y: f32) {
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
        let song = {
            let pb = self.playback.borrow();
            let at = (pb.cursor as i64 + 1 + index as i64) as usize;
            pb.order.get(at).and_then(|&i| pb.queue.get(i)).cloned()
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
            }
            Target::Playlist { .. } => {
                entries.push(self.separator());
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
            ui::CardKind::Genre => {}
        }
    }

    pub fn open_header_menu(self: &Rc<Self>, x: f32, y: f32) {
        let target = {
            let page = self.page.borrow();
            match (&page.playlist, self.current_route()) {
                (Some(p), _) => Target::Playlist { id: p.id.clone(), name: p.name.clone() },
                (None, Some(Route::Album(id))) => Target::Album { id, title: page.title.clone() },
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
            Target::None => {}
            Target::Song { song, .. } => then(self, vec![song]),
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
        let current = self.settings.borrow().eq_preset();
        let entries = crate::config::EQ_PRESETS
            .iter()
            .enumerate()
            .map(|(index, (name, _))| {
                let icon = if *name == current { icons.get_check() } else { slint::Image::default() };
                self.entry(&format!("eq:{index}"), name, icon)
            })
            .collect();
        self.open_menu(Target::None, "Presets", entries, x, y);
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
        if let Some(index) = id.strip_prefix("eq:").and_then(|i| i.parse().ok()) {
            self.close_menu();
            return self.choose_preset(index);
        }
        if let Some(index) = id.strip_prefix("sort:").and_then(|i| i.parse().ok()) {
            self.close_menu();
            return self.set_track_sort(index);
        }
        match id {
            "add-to" => return self.show_playlist_picker(),
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
            Target::Album { title, .. } => title.clone(),
            Target::Artist { name, .. } | Target::Playlist { name, .. } => name.clone(),
            Target::None => return,
        };
        match (id, &target) {
            ("play", _) => self.with_songs(target, move |ctl, songs| ctl.play_songs(songs, 0, &name, false)),
            ("play-next", _) => self.with_songs(target, |ctl, songs| {
                ctl.enqueue(songs, true);
                ctl.toast("Playing next");
            }),
            ("add-queue", _) => self.with_songs(target, |ctl, songs| {
                ctl.enqueue(songs, false);
                ctl.toast("Added to queue");
            }),
            ("pl-new", _) => self.with_songs(target, |ctl, songs| ctl.prompt_new_playlist(songs)),
            ("star", Target::Song { song, .. }) | ("unstar", Target::Song { song, .. }) => {
                self.toggle_song_star(song)
            }
            ("go-album", Target::Song { song, .. }) => self.navigate(Route::Album(song.album_id.clone())),
            ("go-artist", Target::Song { song, .. }) => {
                self.navigate(Route::Artist(song.artist_id.clone()))
            }
            ("radio", Target::Song { song, .. }) => self.start_radio(song.clone()),
            ("remove-playlist", Target::Song { playlist: Some((playlist, index)), .. }) => {
                self.remove_from_playlist(playlist.clone(), *index)
            }
            ("remove-queue", Target::Song { upcoming: Some(offset), .. }) => self.remove_upcoming(*offset),
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

    fn open_prompt(&self, title: &str, message: &str, confirm: &str, text: Option<&str>, danger: bool) {
        let prompt = self.ui.global::<ui::Prompt>();
        prompt.set_title(title.into());
        prompt.set_message(message.into());
        prompt.set_confirm_label(confirm.into());
        prompt.set_wants_text(text.is_some());
        prompt.set_text(text.unwrap_or_default().into());
        prompt.set_danger(danger);
        prompt.set_open(true);
    }

    pub fn prompt_new_playlist(self: &Rc<Self>, songs: Vec<Song>) {
        *self.prompt.borrow_mut() = Some(PromptAction::CreatePlaylist(songs));
        self.open_prompt("Name your playlist", "", "Create playlist", Some(""), false);
    }

    fn prompt_confirmed(self: &Rc<Self>, text: &str) {
        self.ui.global::<ui::Prompt>().set_open(false);
        let Some(action) = self.prompt.borrow_mut().take() else { return };
        let name = text.trim().to_string();
        match action {
            PromptAction::CreatePlaylist(songs) => {
                if name.is_empty() {
                    return;
                }
                let ids: Vec<String> = songs.iter().map(|s| s.id.clone()).collect();
                let label = name.clone();
                self.request(
                    move |client| client.create_playlist(&name, &ids),
                    move |ctl, result| match result {
                        Ok(_) => {
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

    fn add_to_playlist(self: &Rc<Self>, playlist_id: String, songs: Vec<Song>) {
        let name = self
            .playlists
            .borrow()
            .iter()
            .find(|p| p.id == playlist_id)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| "playlist".into());
        let ids: Vec<String> = songs.iter().map(|s| s.id.clone()).collect();
        let key = format!("playlist:{playlist_id}");
        self.request(
            move |client| client.add_to_playlist(&playlist_id, &ids),
            move |ctl, result| match result {
                Ok(()) => {
                    ctl.invalidate(&key);
                    ctl.toast(format!("Added to “{name}”"));
                    ctl.reload_playlists();
                }
                Err(e) => ctl.toast(e.to_string()),
            },
        );
    }

    fn remove_from_playlist(self: &Rc<Self>, playlist_id: String, index: usize) {
        let key = format!("playlist:{playlist_id}");
        let page = Route::Playlist(playlist_id.clone());
        self.request(
            move |client| client.remove_from_playlist(&playlist_id, index),
            move |ctl, result| match result {
                Ok(()) => {
                    ctl.invalidate(&key);
                    ctl.toast("Removed from playlist");
                    ctl.reload_playlists();
                    if ctl.current_route() == Some(page) {
                        ctl.refresh();
                    }
                }
                Err(e) => ctl.toast(e.to_string()),
            },
        );
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
        let target = match card.kind {
            ui::CardKind::Album => Target::Album { id, title: title.clone() },
            ui::CardKind::Playlist => Target::Playlist { id, name: title.clone() },
            ui::CardKind::Artist => Target::Artist { id, name: title.clone() },
            ui::CardKind::Genre => return,
        };
        self.with_songs(target, move |ctl, songs| ctl.play_songs(songs, 0, &title, false));
    }

    // ---- likes ---------------------------------------------------------------------------------

    pub fn toggle_song_star(self: &Rc<Self>, song: &Song) {
        let starred = !self.is_starred(song);
        self.set_star("id", song.id.clone(), starred);
        self.toast(if starred { "Added to Liked songs" } else { "Removed from Liked songs" });
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
        self.request(
            {
                let id = id.clone();
                move |client| client.set_starred(kind, &id, starred)
            },
            move |ctl, result| {
                if let Err(e) = result {
                    ctl.apply_star(kind, &id, !starred);
                    ctl.toast(format!("That didn't save. {e}"));
                }
            },
        );
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
        let prompt = self.ui.global::<ui::Prompt>();
        prompt.on_confirmed(|text| {
            with(|ctl| ctl.prompt_confirmed(&text));
        });
        prompt.on_cancelled(|| {
            with(|ctl| {
                ctl.prompt.borrow_mut().take();
                ctl.ui.global::<ui::Prompt>().set_open(false);
            });
        });
    }
}
