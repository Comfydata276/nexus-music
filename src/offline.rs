//! Working without the server: offline mode, noticing when the server can't be reached, and
//! holding on to likes, ratings and play counts until they can be sent.

use std::rc::Rc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use slint::ComponentHandle;

use crate::controller::{Controller, Route, with};
use crate::subsonic::{ApiError, ApiResult, Client};
use crate::ui;

/// Something to tell the server once it can be reached.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Pending {
    Star { kind: String, id: String, starred: bool },
    Rating { id: String, rating: u8 },
    /// A song that was played, and when (milliseconds since 1970).
    Played { id: String, at: i64 },
}

impl Pending {
    fn send(&self, client: &Client) -> ApiResult<()> {
        match self {
            Pending::Star { kind, id, starred } => client.set_starred(kind, id, *starred),
            Pending::Rating { id, rating } => client.set_rating(id, *rating),
            Pending::Played { id, at } => client.scrobble_at(id, *at),
        }
    }
}

pub fn load_pending(path: &std::path::Path) -> Vec<Pending> {
    std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

impl Controller {
    /// True when nothing should be expected of the server: offline mode is on, or the
    /// server isn't answering.
    pub fn is_offline(&self) -> bool {
        self.settings.borrow().offline || self.unreachable.get()
    }

    fn save_pending(&self) {
        if let Ok(json) = serde_json::to_vec(&*self.pending.borrow()) {
            let _ = std::fs::write(&self.paths.pending, json);
        }
    }

    /// Sends a change to the server, or keeps it for later when that isn't possible.
    /// `refused` runs if the server answers but says no.
    pub fn submit(self: &Rc<Self>, change: Pending, refused: impl FnOnce(&Rc<Controller>, String) + Send + 'static) {
        if self.is_offline() {
            self.pending.borrow_mut().push(change);
            return self.save_pending();
        }
        let sent = change.clone();
        self.request(
            move |client| sent.send(client),
            move |ctl, result| match result {
                Ok(()) => {}
                Err(ApiError::Network(_)) => {
                    ctl.pending.borrow_mut().push(change);
                    ctl.save_pending();
                    ctl.set_unreachable(true);
                }
                Err(e) => refused(ctl, e.to_string()),
            },
        );
    }

    /// Sends everything that was held back while offline.
    fn flush_pending(self: &Rc<Self>) {
        let waiting = std::mem::take(&mut *self.pending.borrow_mut());
        if waiting.is_empty() {
            return;
        }
        self.save_pending();
        self.request(
            move |client| {
                let mut unsent = Vec::new();
                for (index, change) in waiting.iter().enumerate() {
                    match change.send(client) {
                        // The server said no to this one; there's nothing to gain by retrying.
                        Ok(()) | Err(ApiError::Server { .. }) | Err(ApiError::NotSubsonic(_)) => {}
                        Err(ApiError::Network(_)) => {
                            unsent.extend_from_slice(&waiting[index..]);
                            break;
                        }
                    }
                }
                Ok((waiting.len() - unsent.len(), unsent))
            },
            |ctl, result| {
                let Ok((sent, unsent)) = result else { return };
                if !unsent.is_empty() {
                    ctl.pending.borrow_mut().splice(0..0, unsent);
                    ctl.save_pending();
                }
                if sent > 0 {
                    ctl.invalidate("liked");
                }
            },
        );
    }

    /// Shows whether the app is offline, and why.
    fn sync_offline(self: &Rc<Self>) {
        let manual = self.settings.borrow().offline;
        let app = self.ui.global::<ui::App>();
        app.set_offline(manual || self.unreachable.get());
        app.set_offline_label(if manual { "Offline mode" } else { "Can't reach server" }.into());
        self.ui.global::<ui::Settings>().set_offline(manual);
        // Songs that can't be played right now are shown dimmed.
        self.rebuild_tracks();
        self.sync_queue();
        self.sync_history();
    }

    /// Turns offline mode on or off.
    pub fn set_offline_mode(self: &Rc<Self>, on: bool) {
        if self.settings.borrow().offline == on {
            return;
        }
        self.settings.borrow_mut().offline = on;
        self.save_settings();
        if let Some(client) = self.client() {
            client.set_offline(on);
        }
        self.sync_offline();
        if on {
            self.toast("Offline mode: only downloaded songs will play");
            self.navigate(Route::Downloads);
        } else {
            // Find out whether the server is really there before assuming so.
            self.check_connection();
        }
    }

    /// Notes that the server has stopped, or started, answering.
    pub fn set_unreachable(self: &Rc<Self>, unreachable: bool) {
        if self.unreachable.replace(unreachable) == unreachable {
            return;
        }
        self.sync_offline();
        if unreachable {
            // Keep trying quietly until it's back.
            self.reconnect_timer.start(slint::TimerMode::Repeated, Duration::from_secs(15), || {
                with(|ctl| ctl.check_connection());
            });
        } else {
            self.reconnect_timer.stop();
            if !self.settings.borrow().offline {
                self.toast("Back online");
                self.refresh();
            }
        }
    }

    /// Asks the server whether it's there, and catches up with it if so.
    pub fn check_connection(self: &Rc<Self>) {
        if self.settings.borrow().offline {
            return;
        }
        self.request(
            |client| client.ping(),
            |ctl, result| match result {
                Ok(info) => {
                    ctl.ui.global::<ui::App>().set_server_version(info.version.into());
                    ctl.set_unreachable(false);
                    ctl.came_online();
                }
                Err(ApiError::Network(_)) => ctl.set_unreachable(true),
                Err(ApiError::Server { .. }) => ctl.leave("Your saved sign-in no longer works. Sign in again."),
                Err(_) => {}
            },
        );
    }

    /// The server is reachable: send what was held back, and top up downloaded playlists.
    fn came_online(self: &Rc<Self>) {
        self.flush_pending();
        self.sync_collections();
        self.look_elsewhere();
    }
}
