//! Windows media integration: keyboard media keys and the system's now-playing overlay.

use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig, SeekDirection};

use crate::controller::{Controller, post};
use crate::diagnostics::{self, Line, Note};
use crate::subsonic::Song;

pub struct MediaKeys {
    controls: MediaControls,
    /// The position last reported, to avoid chattering at the system four times a second.
    reported: f64,
}

impl Controller {
    /// Attaches to the window once it exists. Safe to call repeatedly.
    fn media_ready(self: &Rc<Self>) -> bool {
        if self.media.borrow().is_some() {
            return true;
        }
        let window = self.ui.window().window_handle();
        let Ok(handle) = window.window_handle() else { return false };
        let RawWindowHandle::Win32(win32) = handle.as_raw() else { return false };
        let config = PlatformConfig {
            display_name: "Nexus Music",
            dbus_name: "nexus_music",
            hwnd: Some(win32.hwnd.get() as *mut std::ffi::c_void),
        };
        let Ok(mut controls) = MediaControls::new(config) else { return false };
        let attached = controls.attach(|event| {
            post(move |ctl| match event {
                MediaControlEvent::Play => ctl.media_play(false),
                MediaControlEvent::Pause | MediaControlEvent::Stop => ctl.set_playing(false),
                MediaControlEvent::Toggle => ctl.media_play(true),
                MediaControlEvent::Next => ctl.next_track(),
                MediaControlEvent::Previous => ctl.previous_track(),
                MediaControlEvent::SetPosition(MediaPosition(at)) => ctl.seek(at.as_secs_f64()),
                MediaControlEvent::Seek(direction) => {
                    let now = ctl.playback.borrow().position;
                    let step = if direction == SeekDirection::Forward { 10.0 } else { -10.0 };
                    ctl.seek(now + step);
                }
                MediaControlEvent::SeekBy(direction, amount) => {
                    let now = ctl.playback.borrow().position;
                    let step = amount.as_secs_f64();
                    ctl.seek(if direction == SeekDirection::Forward { now + step } else { now - step });
                }
                _ => {}
            });
        });
        if attached.is_err() {
            return false;
        }
        *self.media.borrow_mut() = Some(MediaKeys { controls, reported: 0.0 });
        true
    }

    fn media_play(self: &Rc<Self>, toggle: bool) {
        let was_playing = {
            let playback = self.playback.borrow();
            if playback.current().is_none() {
                diagnostics::observe(Line::Note(Note::MediaButtonNothingToResume));
                return;
            }
            playback.playing
        };
        let can_play = self.client().is_some();
        if toggle {
            self.toggle_play_here();
        } else {
            self.set_playing_here(true);
        }
        if !was_playing && can_play && self.playback.borrow().playing {
            diagnostics::observe(Line::Note(Note::MediaButtonStarted));
        }
    }

    pub fn media_track(self: &Rc<Self>, song: Option<&Song>) {
        if !self.media_ready() {
            return;
        }
        let Some(song) = song else {
            if let Some(media) = self.media.borrow_mut().as_mut() {
                let _ = media.controls.set_playback(MediaPlayback::Stopped);
            }
            return;
        };
        self.media_metadata(song, None);
        // Artwork comes from the on-disk cache, fetched first if this cover hasn't been seen.
        if song.cover_art.is_empty() {
            return;
        }
        let Some(client) = self.client() else { return };
        let (dir, song) = (self.covers.dir().to_path_buf(), song.clone());
        self.art_pool.spawn(move || {
            if let Some(path) = crate::covers::ensure_file(&client, &dir, &song.cover_art) {
                post(move |ctl| {
                    let still_current =
                        ctl.playback.borrow().current().is_some_and(|current| current.id == song.id);
                    if still_current {
                        ctl.media_metadata(&song, Some(path));
                    }
                });
            }
        });
    }

    fn media_metadata(&self, song: &Song, cover: Option<PathBuf>) {
        let cover_url = cover.map(|path| format!("file://{}", path.display()));
        if let Some(media) = self.media.borrow_mut().as_mut() {
            let _ = media.controls.set_metadata(MediaMetadata {
                title: Some(&song.title),
                artist: Some(song.artist_label()),
                album: Some(&song.album),
                cover_url: cover_url.as_deref(),
                duration: (song.duration > 0).then(|| Duration::from_secs(song.duration as u64)),
            });
        }
    }

    pub fn media_playing(self: &Rc<Self>, playing: bool, position: f64) {
        let has_track = self.playback.borrow().current().is_some();
        if !has_track || !self.media_ready() {
            return;
        }
        if let Some(media) = self.media.borrow_mut().as_mut() {
            // (Whatever number it is handed: turning one that is too large into a length
            // of time stops the app.)
            let progress = Some(MediaPosition(nexus_core::queue::span(position)));
            media.reported = position;
            let _ = media.controls.set_playback(if playing {
                MediaPlayback::Playing { progress }
            } else {
                MediaPlayback::Paused { progress }
            });
        }
    }

    /// Keeps the system's progress bar roughly in step without updating it on every tick.
    pub fn media_position(self: &Rc<Self>, position: f64) {
        let stale = self.media.borrow().as_ref().is_some_and(|m| (position - m.reported).abs() >= 5.0);
        if stale {
            self.media_playing(true, position);
        }
    }
}
