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

impl Pending {
    /// What the change is about. Changes to the same thing have to reach the server in order.
    fn id(&self) -> &str {
        match self {
            Pending::Star { id, .. } | Pending::Rating { id, .. } | Pending::Played { id, .. } => id,
        }
    }
}

pub fn load_pending(path: &std::path::Path) -> Vec<Pending> {
    std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

/// How a round of sending held changes went.
#[derive(Debug)]
struct Sent {
    /// How many the server dealt with.
    done: usize,
    /// The ones still to send, in the order they were made.
    kept: Vec<Pending>,
    /// The server stopped answering part-way.
    lost_touch: bool,
}

/// Sends held changes in order, keeping every one the server hasn't dealt with.
fn send_all(waiting: &[Pending], mut send: impl FnMut(&Pending) -> ApiResult<()>) -> Sent {
    let mut sent = Sent { done: 0, kept: Vec::new(), lost_touch: false };
    // Things with a change that was kept: later changes to them wait behind it.
    let mut held: Vec<&str> = Vec::new();
    let mut strange = 0;
    for (index, change) in waiting.iter().enumerate() {
        if held.contains(&change.id()) {
            sent.kept.push(change.clone());
            continue;
        }
        match send(change) {
            Ok(()) => (sent.done, strange) = (sent.done + 1, 0),
            // The server is there and said no (the song has gone, say): asking again won't help.
            Err(error @ ApiError::Server { .. }) if !error.sign_in_refused() => (sent.done, strange) = (sent.done + 1, 0),
            // Something in front of the server answered in its place (a proxy's error page):
            // keep the change and carry on, unless that's all that comes back.
            Err(ApiError::NotSubsonic(_)) if strange < 2 => {
                strange += 1;
                held.push(change.id());
                sent.kept.push(change.clone());
            }
            // No server to speak of: this one and the rest wait.
            Err(_) => {
                sent.kept.extend_from_slice(&waiting[index..]);
                sent.lost_touch = true;
                break;
            }
        }
    }
    sent
}

impl Controller {
    /// True when nothing should be expected of the server: offline mode is on, or the
    /// server isn't answering.
    pub fn is_offline(&self) -> bool {
        self.settings.borrow().offline || self.unreachable.get()
    }

    /// Writes down what's still to be sent. Changes on their way to the server stay on file
    /// until it has answered, so closing the app part-way can't lose them.
    fn save_pending(&self) {
        let Some(path) = self.pending_path() else { return };
        let (sending, pending) = (self.sending.borrow(), self.pending.borrow());
        let held: Vec<&Pending> = sending.iter().chain(pending.iter()).collect();
        if let Ok(json) = serde_json::to_vec(&held) {
            crate::config::write_atomically(&path, &json);
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
                // The server said no.
                Err(error @ ApiError::Server { .. }) if !error.sign_in_refused() => refused(ctl, error.to_string()),
                // It couldn't be asked: no connection, or a proxy answering in its place.
                Err(_) => {
                    ctl.pending.borrow_mut().push(change);
                    ctl.save_pending();
                    ctl.set_unreachable(true);
                }
            },
        );
    }

    /// Sends everything that was held back while offline.
    fn flush_pending(self: &Rc<Self>) {
        let Some(client) = self.client() else { return };
        // One round at a time: what a round is sending is in `sending` until it's over.
        if !self.sending.borrow().is_empty() || self.pending.borrow().is_empty() {
            return;
        }
        let waiting = std::mem::take(&mut *self.pending.borrow_mut());
        *self.sending.borrow_mut() = waiting.clone();
        let (account, path) = (self.account(), self.pending_path());
        self.api_pool.spawn(move || {
            let sent = send_all(&waiting, |change| change.send(&client));
            crate::controller::post(move |ctl| {
                if ctl.account() != account {
                    // Someone else has signed in meanwhile. What wasn't sent stays in the
                    // file of the account it was made under, ahead of anything there already.
                    if let Some(path) = path {
                        let mut held = sent.kept;
                        held.extend(load_pending(&path).into_iter().skip(waiting.len()));
                        if let Ok(json) = serde_json::to_vec(&held) {
                            crate::config::write_atomically(&path, &json);
                        }
                    }
                    return;
                }
                ctl.sending.borrow_mut().clear();
                // Anything changed while this was going on comes after what was kept.
                ctl.pending.borrow_mut().splice(0..0, sent.kept);
                ctl.save_pending();
                if sent.done > 0 {
                    ctl.invalidate("liked");
                }
                if sent.lost_touch && ctl.client().is_some() {
                    ctl.set_unreachable(true);
                }
            });
        });
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
                Err(error) if error.sign_in_refused() => ctl.leave("Your saved sign-in no longer works. Sign in again."),
                // No answer, an error from the server, or a proxy answering in its place: none
                // of those is a reason to sign out. Carry on with what's on this PC and keep
                // trying.
                Err(_) => ctl.set_unreachable(true),
            },
        );
    }

    /// A quick look at whether the server is still there, while music plays. Finding it gone
    /// starts the usual checks for its return; finding it there changes nothing.
    pub fn check_server(self: &Rc<Self>) {
        if self.is_offline() {
            return;
        }
        self.request(
            |client| client.ping().map(|_| ()),
            |ctl, answer| match answer {
                Ok(()) => {}
                Err(error) if error.sign_in_refused() => ctl.leave("Your saved sign-in no longer works. Sign in again."),
                Err(_) => ctl.set_unreachable(true),
            },
        );
    }

    /// The server is reachable: send what was held back, and top up downloaded playlists.
    fn came_online(self: &Rc<Self>) {
        self.flush_pending();
        self.covers.retry_failed(self);
        self.resume_waiting();
        self.sync_collections();
        self.look_elsewhere();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn star(id: &str, starred: bool) -> Pending {
        Pending::Star { kind: "song".into(), id: id.into(), starred }
    }

    fn ids(changes: &[Pending]) -> Vec<&str> {
        changes.iter().map(Pending::id).collect()
    }

    #[test]
    fn keeps_what_the_server_has_not_dealt_with() {
        let waiting = [star("a", true), star("b", true), star("c", true), star("d", true)];
        // Everything goes through.
        let sent = send_all(&waiting, |_| Ok(()));
        assert_eq!((sent.done, sent.kept.len(), sent.lost_touch), (4, 0, false));
        // The connection drops at the third: it and the rest wait, in order.
        let mut calls = 0;
        let sent = send_all(&waiting, |_| {
            calls += 1;
            if calls >= 3 { Err(ApiError::Network("gone".into())) } else { Ok(()) }
        });
        assert_eq!((sent.done, ids(&sent.kept), sent.lost_touch), (2, vec!["c", "d"], true));
        // The server refuses one for good: it isn't asked again, and the rest go.
        let sent = send_all(&waiting, |change| {
            if change.id() == "b" { Err(ApiError::Server { code: 70, message: "not found".into() }) } else { Ok(()) }
        });
        assert_eq!((sent.done, sent.kept.len()), (4, 0));
        // A sign-in the server no longer accepts refuses nothing in particular: all of it waits.
        let sent = send_all(&waiting, |_| Err(ApiError::Server { code: 40, message: "wrong password".into() }));
        assert_eq!((sent.done, sent.kept.len(), sent.lost_touch), (0, 4, true));
    }

    #[test]
    fn keeps_a_change_that_a_proxy_answered_for() {
        let page = || Err(ApiError::NotSubsonic("502".into()));
        // One odd answer: that change waits, and so does a later change to the same song.
        let waiting = [star("a", true), star("b", true), star("a", false), star("c", true)];
        let sent = send_all(&waiting, |change| if change.id() == "a" { page() } else { Ok(()) });
        assert_eq!((sent.done, ids(&sent.kept), sent.lost_touch), (2, vec!["a", "a"], false));
        // Nothing but error pages: stop asking and keep the lot.
        let waiting = [star("a", true), star("b", true), star("c", true), star("d", true)];
        let mut calls = 0;
        let sent = send_all(&waiting, |_| {
            calls += 1;
            page()
        });
        assert_eq!((sent.done, ids(&sent.kept), sent.lost_touch, calls), (0, vec!["a", "b", "c", "d"], true, 3));
    }
}
