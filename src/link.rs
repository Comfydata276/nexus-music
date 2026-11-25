//! Linking a person's devices the new way: the first slice. Two copies of the app signed
//! in to one account find each other through the relay's mailbox route, each shows what the
//! other is playing, and Pause on one pauses the other.
//!
//! It is **off** unless the environment says `NEXUS_MUSIC_LINK=1`, and has no setting. The
//! old linking (`jam.rs`) is beside it and unchanged.
//!
//! The rules are the library's (`nexus_core::link`, to `docs/linking-design.md`): the
//! secret and the look that reads it, the seal, the two messages, the rule for a command,
//! and what the asking side concludes. What is here is this app's own: asking the music
//! server and the relay, the clock, random numbers, the player, and the window.
//!
//! **The order of choices** is here too: a count of what has changed on this PC, kept so
//! that a command from another device can be judged against what is true here. It decides
//! nothing about what the person does on this PC, with linking on or off.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use nexus_core::link::frame::{self, ASK, Keys, TELL};
use nexus_core::link::message::{Ask, At, Tell, Why, read_ask, read_tell};
use nexus_core::link::rule::{Reservation, Waiting};
use nexus_core::link::secret::{self, Next, Secret, Stop, Verdict};
use nexus_core::link::steer::{Outcome, Peer, Tick, Told, Wait};
use nexus_core::link::{Asks, LOOK_AGAIN_MS, LOOK_EVERY_MS, Nonce, Place, VERSION, minute, secret_holds, text};
use nexus_core::step::Stated;
use serde_json::{Value, json};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::config;
use crate::controller::{Controller, post, with};
use crate::pool::Pool;
use crate::subsonic::Client;
use crate::ui;

/// Whether the new linking is on for this run of the app.
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("NEXUS_MUSIC_LINK").is_ok_and(|value| value == "1"))
}

/// Why a playlist of the person's own may not have this name, if it may not. Each feature
/// keeps a name to itself with its own words, and the new linking only where it is on:
/// with it off, nothing of it exists.
pub fn kept_name(name: &str) -> Option<&'static str> {
    kept_name_when(name, on())
}

fn kept_name_when(name: &str, linking: bool) -> Option<&'static str> {
    if nexus_core::step::reserved(name) {
        Some(nexus_core::step::RESERVED)
    } else if linking && secret::reserved(name) {
        Some(secret::RESERVED)
    } else {
        None
    }
}

/// Whether a playlist of this name is left out of every ordinary listing of playlists:
/// the one the app keeps devices in step through, always; and, where linking is on, the
/// one that holds the secret devices share. What is not listed is not opened, and a page
/// that is not opened is not written to the cache of pages on disk.
pub fn unlisted(name: &str) -> bool {
    unlisted_when(name, on())
}

fn unlisted_when(name: &str, linking: bool) -> bool {
    name == crate::sync::PLAYLIST_NAME || (linking && name == secret::PLAYLIST_NAME)
}

/// Whether a playlist's page may be written to the cache of pages on disk: never one of a
/// name the app keeps for itself, whose comment is not a description. Linking's is never
/// written whether or not linking is on in this copy: it may be on in another, and what
/// that one keeps there is a secret.
pub fn page_may_be_kept(playlist: &str) -> bool {
    playlist != crate::sync::PLAYLIST_NAME && playlist != secret::PLAYLIST_NAME
}

/// Milliseconds since the app started, on a clock that goes on counting while the PC
/// sleeps: the rule's minute, and the life of the secret, are read off it.
pub fn now_ms() -> u64 {
    static START: OnceLock<u64> = OnceLock::new();
    let now = awake_and_asleep_ms();
    now.saturating_sub(*START.get_or_init(|| now))
}

/// Time since the system started, sleep included.
#[cfg(windows)]
fn awake_and_asleep_ms() -> u64 {
    // SAFETY: takes nothing and returns a number.
    unsafe { windows::Win32::System::SystemInformation::GetTickCount64() }
}

/// Time since the system started, sleep included: what Linux calls its boot time.
#[cfg(not(windows))]
fn awake_and_asleep_ms() -> u64 {
    static OTHERWISE: OnceLock<std::time::Instant> = OnceLock::new();
    let read = || {
        let text = std::fs::read_to_string("/proc/uptime").ok()?;
        let seconds: f64 = text.split_whitespace().next()?.parse().ok()?;
        Some((seconds * 1000.0) as u64)
    };
    // (Elsewhere there is no such file. The app is not shipped for those systems.)
    read().unwrap_or_else(|| OTHERWISE.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64)
}

/// Bytes from the system's source of randomness, or `None` if it has none to give. Nothing
/// is ever made up another way.
fn random<const N: usize>() -> Option<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).ok()?;
    Some(bytes)
}

/// For a test only: how long a Pause from another device is held between being accepted
/// and being carried out, if `NEXUS_MUSIC_LINK_TEST_SLOW_PAUSE` gives a number of
/// milliseconds. With it, a run can make the secret change in between. Never set otherwise.
fn slow_pause() -> Option<Duration> {
    static WAIT: OnceLock<Option<Duration>> = OnceLock::new();
    *WAIT.get_or_init(|| std::env::var("NEXUS_MUSIC_LINK_TEST_SLOW_PAUSE").ok()?.parse().ok().map(Duration::from_millis))
}

/// Writes a line of what happened to the file named by `NEXUS_MUSIC_LINK_LOG`, if any: for
/// watching two copies from outside. Never a secret, a key or a token.
fn log(line: impl AsRef<str>) {
    use std::io::Write;
    static FILE: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    if let Some(path) = FILE.get_or_init(|| std::env::var_os("NEXUS_MUSIC_LINK_LOG").map(Into::into))
        && let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path)
    {
        let _ = writeln!(file, "{:>8} {}", now_ms(), line.as_ref());
    }
}

// ---- the order of choices ----------------------------------------------------------------
//
// It exists so that a command from another device is judged against what is true on this
// one, and for nothing else. Nothing the person does here is ever refused, delayed or
// dropped because of it: these calls only keep count.

/// How long a look-up for something to play is given before this PC stops counting as
/// busy on its account: longer than any one request to the music server is given.
const LOOK_UP_MS: u64 = 60_000;

/// What the player holds is being changed. What it does on the way is part of that one
/// change and is not counted again.
pub struct Changing<'a> {
    flag: &'a std::cell::Cell<bool>,
    outer: bool,
}

impl Drop for Changing<'_> {
    fn drop(&mut self) {
        self.flag.set(self.outer);
    }
}

impl Controller {
    /// What a command from another device would act on is about to change, by a press or
    /// by itself: the generation moves on, once for everything done until this is dropped.
    /// Called only once it is known that something will change: a press that changes
    /// nothing (Next with nothing to go to) takes no place.
    pub fn changing(&self) -> Changing<'_> {
        let outer = self.link_changing.replace(true);
        if !outer {
            self.link_rule.borrow_mut().moved();
        }
        Changing { flag: &self.link_changing, outer }
    }

    /// The same, for a change that is over as soon as it is made (the volume, a seek).
    pub fn changed(&self) {
        if !self.link_changing.get() {
            self.link_rule.borrow_mut().moved();
        }
    }

    /// Something to play has been chosen and its songs are still to be looked up: until
    /// `looked_up`, or the look-up's time is up, this PC is busy to other devices. To the
    /// person at it, nothing is different.
    pub fn looking_up(&self) -> Waiting {
        self.link_rule.borrow_mut().wait(now_ms(), LOOK_UP_MS)
    }

    /// The look-up is over, whatever it found.
    pub fn looked_up(&self, waiting: Waiting) {
        self.link_rule.borrow_mut().done(waiting);
    }
}

// ---- asking the music server -------------------------------------------------------------

/// What a look at the secret came to.
enum Looked {
    /// The secret a look read, the playlist it is in, and why a new one was written on the
    /// way, if one was.
    Secret { secret: Secret, id: String, wrote: Option<secret::Why> },
    Stopped(Stop),
    /// The server could not be asked, or kept undoing what was written. Nothing is
    /// concluded: what is held is neither renewed nor replaced.
    Failed,
    /// The record was found to be no good (public, another playlist's, not a record) and a
    /// new secret could not be written in its place: what is held is not to be used.
    Invalid,
    NoRandom,
}

/// Whether what a look found means the secret in use must not go on being used. That the
/// server could not be asked means nothing of the kind: the secret then runs out in its own
/// time. (A look that read a different secret ends the old session by beginning a new one.)
fn ends_session(looked: &Looked) -> bool {
    matches!(looked, Looked::Stopped(_) | Looked::Invalid)
}

/// What a look asks of the music server. `None` is "could not be asked, or did not answer".
trait Server {
    /// The playlists, as the server states them.
    fn list(&mut self) -> Option<Vec<Stated>>;
    /// One playlist, as the XML answer it came as.
    fn fetch(&mut self, id: &str) -> Option<String>;
    /// Makes the playlist the secret is kept in: its id.
    fn make(&mut self) -> Option<String>;
    /// Writes a comment into a playlist, and keeps it private.
    fn write(&mut self, id: &str, comment: &str) -> Option<()>;
    /// Thirty-two bytes from the system's source of randomness, if it has any to give.
    fn random(&mut self) -> Option<[u8; 32]>;
    /// Whether the sign-in this look is for is still the one. Asked before every request:
    /// once the person has signed out, the look makes no more of them.
    fn still(&self) -> bool;
}

/// The music server itself. Every call is a form POST, so neither the sign-in nor the
/// secret is in an address, and no redirect is followed.
struct Navidrome<'a> {
    client: &'a Client,
    agent: ureq::Agent,
    /// Set when the sign-in this look is for ends.
    over: Arc<AtomicBool>,
}

impl Server for Navidrome<'_> {
    fn list(&mut self) -> Option<Vec<Stated>> {
        let answer = self.client.link_call(&self.agent, "getPlaylists", &[]).ok()?;
        Some(serde_json::from_value(answer["playlists"]["playlist"].clone()).unwrap_or_default())
    }

    fn fetch(&mut self, id: &str) -> Option<String> {
        self.client.link_call_xml(&self.agent, "getPlaylist", &[("id", id)]).ok()
    }

    fn make(&mut self) -> Option<String> {
        let made = self.client.link_call(&self.agent, "createPlaylist", &[("name", secret::PLAYLIST_NAME)]).ok()?;
        made["playlist"]["id"].as_str().filter(|id| !id.is_empty()).map(str::to_string)
    }

    fn write(&mut self, id: &str, comment: &str) -> Option<()> {
        self.client.link_call(&self.agent, "updatePlaylist", &[("playlistId", id), ("comment", comment), ("public", "false")]).ok().map(drop)
    }

    fn random(&mut self) -> Option<[u8; 32]> {
        random()
    }

    fn still(&self) -> bool {
        !self.over.load(Ordering::Relaxed)
    }
}

/// One look (see `nexus_core::link::secret`). `over` is set if the sign-in it is for ends
/// while it is under way.
fn look(client: &Client, used: Option<String>, over: Arc<AtomicBool>) -> Looked {
    let mut server = Navidrome { client, agent: crate::subsonic::make_agent_without_redirects(), over };
    look_with(&mut server, client.username(), used)
}

/// One look, of whatever answers as the music server does.
///
/// **Evidence is not forgotten by a retry.** Once the record has been seen to be no good
/// (public, not a record, gone, or the playlist that was used no longer there), the look
/// can end only two ways: a later read shows a good record, which is then the one to use;
/// or the look says the record is no good ([`Looked::Invalid`]), which ends the session.
/// It never ends "the server could not be asked", which would leave the old secret in use
/// until its time ran out, on the strength of a request that happened to fail afterwards.
///
/// **It stops with the sign-in.** Before each request it asks whether the sign-in it is
/// for is still the one. If not, it makes no further request, and what it says is of no
/// account: its answer is the answer to a look nobody is waiting on.
fn look_with(server: &mut impl Server, username: &str, mut used: Option<String>) -> Looked {
    let mut wrote = None;
    // The record has been seen to be no good in this look.
    let mut no_good = false;
    let unasked = |no_good: bool| if no_good { Looked::Invalid } else { Looked::Failed };
    for _ in 0..secret::ROUNDS {
        if !server.still() {
            return Looked::Failed;
        }
        let Some(listed) = server.list() else { return unasked(no_good) };
        let (id, replace) = match secret::choose(&listed, username, used.as_deref()) {
            Next::Stop(stop) => return Looked::Stopped(stop),
            Next::Fetch { id, replace } => (id, replace),
            Next::Make => {
                // There is none. If one was in use, that one has gone.
                no_good |= used.is_some();
                if !server.still() {
                    return Looked::Failed;
                }
                let Some(id) = server.make() else { return unasked(no_good) };
                let Some(fresh) = server.random().map(Secret::new) else { return Looked::NoRandom };
                if !server.still() {
                    return Looked::Failed;
                }
                if server.write(&id, &fresh.comment()).is_none() {
                    return unasked(no_good);
                }
                log(format!("look: made the playlist {id} and wrote a secret into it"));
                used = Some(id);
                continue;
            }
        };
        // One answer, and the XML one: only it states whether the playlist is public, and
        // what it is, whose, and its comment are taken from the same answer.
        if !server.still() {
            return Looked::Failed;
        }
        let Some(fetched) = server.fetch(&id).as_deref().and_then(secret::fetched) else { return unasked(no_good) };
        match secret::judge(&id, replace, &fetched, username) {
            Verdict::Use(secret) => return Looked::Secret { secret, id, wrote },
            Verdict::Stop(stop) => return Looked::Stopped(stop),
            Verdict::Write(why) => {
                no_good = true;
                // A new secret into it, and it is kept private. The secret used afterwards
                // is the one the next round reads back, not this one.
                let Some(fresh) = server.random().map(Secret::new) else { return Looked::NoRandom };
                if !server.still() {
                    return Looked::Failed;
                }
                if server.write(&id, &fresh.comment()).is_none() {
                    return Looked::Invalid;
                }
                log(format!("look: wrote a new secret into {id} ({why:?})"));
                wrote = Some(why);
                used = Some(id);
            }
        }
    }
    // It kept being undone.
    unasked(no_good)
}

// ---- asking the relay --------------------------------------------------------------------

/// A connection to the relay's route for devices. The host key and a place's token go to
/// the relay's own address and nowhere else: no redirect is followed.
struct Wire {
    base: String,
    key: String,
    agent: ureq::Agent,
}

impl Wire {
    fn read(response: ureq::http::Response<ureq::Body>) -> (u16, Value) {
        (response.status().as_u16(), serde_json::from_reader(response.into_body().into_reader()).unwrap_or(Value::Null))
    }

    fn get(&self, path: &str, token: &str, wait: Duration) -> Option<(u16, Value)> {
        let request = self.agent.get(format!("{}/{path}", self.base)).header("Authorization", &format!("Bearer {}", self.key)).header("X-Nexus-Link", token);
        request.config().timeout_global(Some(wait)).timeout_recv_response(Some(wait)).build().call().map(Wire::read).ok()
    }

    fn post(&self, path: &str, token: &str, body: &Value) -> Option<(u16, Value)> {
        let request = self.agent.post(format!("{}/{path}", self.base)).header("Content-Type", "application/json").header("Authorization", &format!("Bearer {}", self.key)).header("X-Nexus-Link", token);
        request.config().timeout_global(Some(Duration::from_secs(20))).build().send(body.to_string().as_bytes()).map(Wire::read).ok()
    }
}

/// Waits, unless told to stop meanwhile. Returns whether to go on.
fn pause_for(seconds: u64, stop: &AtomicBool) -> bool {
    for _ in 0..seconds * 10 {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !stop.load(Ordering::Relaxed)
}

/// Keeps this device's place in the room for as long as the session lasts: enters (under
/// a new place each time), collects what is put in its mailbox, and enters again if the
/// relay has forgotten it. Trying again is spaced out, up to a minute.
fn room_loop(wire: Arc<Wire>, room: String, serial: u64, stop: Arc<AtomicBool>) {
    // How long to leave it before trying again: longer each time, up to a minute.
    let wait = std::cell::Cell::new(2u64);
    let again = |stop: &AtomicBool| {
        let go_on = pause_for(wait.get(), stop);
        wait.set((wait.get() * 2).min(60));
        go_on
    };
    'entering: while !stop.load(Ordering::Relaxed) {
        match wire.get("api/hello", "", Duration::from_secs(15)) {
            Some((200, hello)) if hello["link"].as_u64() == Some(VERSION) && hello["host"].as_bool() == Some(true) => {}
            Some((200, _)) => {
                post(move |ctl| ctl.link_said(serial, "This relay needs updating before your devices can be linked."));
                if !pause_for(60, &stop) {
                    return;
                }
                continue;
            }
            Some((401, _)) => {
                post(move |ctl| ctl.link_said(serial, "The host link isn't right for this relay."));
                if !pause_for(60, &stop) {
                    return;
                }
                continue;
            }
            _ => {
                if !again(&stop) {
                    return;
                }
                continue;
            }
        }
        let Some(place) = random::<16>() else {
            return post(move |ctl| ctl.link_no_random());
        };
        let name = text::place(&place);
        let (token, mut version) = match wire.post(&format!("api/link/{room}/enter"), "", &json!({"place": name})) {
            Some((200, entered)) => {
                let Some(token) = entered["token"].as_str().map(str::to_string) else { continue };
                (token, entered["v"].as_u64().unwrap_or(0))
            }
            // (A place that is taken: so unlikely that it is simply tried again with another.)
            Some((409, _)) => continue,
            _ => {
                if !again(&stop) {
                    return;
                }
                continue;
            }
        };
        wait.set(2);
        log(format!("room: entered as {name}"));
        {
            let token = token.clone();
            post(move |ctl| ctl.link_entered(serial, place, token));
        }
        // The last frame handed to the window's thread. A new token counts from 1 again.
        let mut after = 0u64;
        loop {
            let answer = wire.get(&format!("api/link/{room}/poll?v={version}&after={after}"), &token, Duration::from_secs(40));
            if stop.load(Ordering::Relaxed) {
                let _ = wire.post(&format!("api/link/{room}/leave"), &token, &json!({}));
                return;
            }
            match answer {
                Some((200, heard)) => {
                    version = heard["v"].as_u64().unwrap_or(version);
                    let places: Vec<String> = heard["places"].as_array().into_iter().flatten().filter_map(|place| place.as_str().map(str::to_string)).collect();
                    let mut frames = Vec::new();
                    for frame in heard["frames"].as_array().into_iter().flatten() {
                        // One that was handed over before is sent again until it is said
                        // to have been dealt with: it is skipped.
                        let (Some(seq), Some(from), Some(sealed)) = (frame["seq"].as_u64(), frame["from"].as_str(), frame["frame"].as_str()) else { continue };
                        if seq > after {
                            after = seq;
                            frames.push((from.to_string(), sealed.to_string()));
                        }
                    }
                    post(move |ctl| ctl.link_heard(serial, places, frames));
                }
                // The relay has forgotten this place (it restarted, or the PC slept).
                Some((404, _)) => {
                    log("room: the relay has forgotten this place");
                    post(move |ctl| ctl.link_left(serial));
                    if !pause_for(1, &stop) {
                        return;
                    }
                    continue 'entering;
                }
                Some((401, _)) => {
                    if !pause_for(30, &stop) {
                        return;
                    }
                }
                Some((429 | 503, _)) => {
                    if !pause_for(2, &stop) {
                        return;
                    }
                }
                _ => {
                    if !pause_for(5, &stop) {
                        return;
                    }
                }
            }
        }
    }
}

// ---- what this device holds --------------------------------------------------------------

/// One sign-in with one secret. When either changes the session ends, and everything in
/// flight under it is dropped: its number no longer matches.
struct Session {
    serial: u64,
    secret: Secret,
    keys: Keys,
    /// When the look that read the secret began. Ten minutes on, the secret is not used.
    /// (Shared with what is queued to send under the session, which reads it when its turn
    /// comes: a look that renews the secret meanwhile renews it for that too.)
    since_ms: Arc<AtomicU64>,
    wire: Arc<Wire>,
    stop: Arc<AtomicBool>,
    /// This device's place in the room and the token for it, once it has entered.
    place: Option<(Place, String)>,
    /// The other places in the room, in the relay's order, and what is known of each.
    others: Vec<Place>,
    peers: HashMap<Place, Seen>,
}

impl Session {
    fn since(&self) -> u64 {
        self.since_ms.load(Ordering::Relaxed)
    }
}

/// Whether something queued to be sent under a session may go when its turn comes: only if
/// the session has not ended and its secret is still inside its time. Both, because a
/// session that has run out is no session, whether or not anything has ended it yet.
fn may_go(session_ended: bool, since_ms: u64, now_ms: u64) -> bool {
    !session_ended && secret_holds(since_ms, now_ms)
}

#[derive(Default)]
struct Seen {
    peer: Peer,
    /// When it was last looked at.
    looked_ms: Option<u64>,
}

#[derive(Default)]
pub struct Link {
    /// The sign-in all of this belongs to.
    account: String,
    /// The playlist the secret was last read from under this sign-in.
    used: Option<String>,
    /// A look is on its way: there is never more than one for a sign-in.
    looking: bool,
    /// Set when the sign-in ends, for the look that is on its way to see.
    look_over: Arc<AtomicBool>,
    /// Goes up when the sign-in changes, so the answer to an earlier look is dropped.
    looks: u64,
    /// When the last look finished.
    looked_ms: Option<u64>,
    session: Option<Session>,
    serial: u64,
    asks: Asks,
    /// What there is to say about linking, under the list.
    note: String,
    /// The system could not give random numbers: linking is off for this run.
    off: bool,
    pool: Option<Arc<Pool>>,
}

impl Link {
    fn pool(&mut self) -> Arc<Pool> {
        self.pool.get_or_insert_with(|| Arc::new(Pool::new("link", 2, false))).clone()
    }

    /// The session, if `serial` is the one that is live **and its secret is inside its
    /// time**. Asked at every use of a session: when a look's answer would renew it, when
    /// something is sent, when a frame is opened, and at the moment the player is paused.
    /// A session that has run out is no session, whether or not the tick has got round to
    /// ending it.
    fn live(&self, serial: u64, now_ms: u64) -> Option<&Session> {
        self.session.as_ref().filter(|session| session.serial == serial && secret_holds(session.since(), now_ms))
    }

    fn live_mut(&mut self, serial: u64, now_ms: u64) -> Option<&mut Session> {
        self.session.as_mut().filter(|session| session.serial == serial && secret_holds(session.since(), now_ms))
    }

    /// Ends the session, if there is one: its thread stops, and whatever was queued to be
    /// sent under it is not sent. Says whether there was one.
    fn end_session(&mut self) -> bool {
        let Some(session) = self.session.take() else { return false };
        session.stop.store(true, Ordering::Relaxed);
        self.serial += 1;
        true
    }

    /// Everything of this sign-in's is over: the session, and any look at the secret that
    /// is still on its way, whose answer must start nothing.
    fn stop(&mut self) {
        self.end_session();
        self.look_over.store(true, Ordering::Relaxed);
        self.looks += 1;
        self.looking = false;
        self.looked_ms = None;
    }

    /// A look starts: the number its answer must show to be taken, and what it watches to
    /// see whether the sign-in it is for has ended.
    fn look_begins(&mut self) -> (u64, Arc<AtomicBool>) {
        self.looking = true;
        self.look_over = Arc::new(AtomicBool::new(false));
        (self.looks, self.look_over.clone())
    }

    /// Whether the answer to the look with this number is still wanted.
    fn expects(&self, looks: u64) -> bool {
        self.looks == looks
    }
}

/// Whether this copy should be linked with the person's other devices the new way: signed
/// in, a relay set up, random numbers to be had, and "Link my devices" on. That setting
/// means what it says for the old linking and the new alike: off is off for both.
fn wanted(signed_in: bool, relay: bool, link_my_devices: bool, no_random: bool) -> bool {
    signed_in && relay && link_my_devices && !no_random
}

/// One row for each device. A copy that has been started again is in the room twice for
/// a while: at its new place, and at its old one until the relay gives that up. It is
/// shown once, as its latest answer has it, where it was first in the list.
fn latest_of_each<T>(rows: Vec<([u8; 16], u64, T)>) -> Vec<T> {
    let mut kept: Vec<([u8; 16], u64, T)> = Vec::new();
    for (device, heard_ms, row) in rows {
        match kept.iter_mut().find(|(other, _, _)| *other == device) {
            Some(earlier) if heard_ms >= earlier.1 => *earlier = (device, heard_ms, row),
            Some(_) => {}
            None => kept.push((device, heard_ms, row)),
        }
    }
    kept.into_iter().map(|(_, _, row)| row).collect()
}

fn name_of(seen: &Seen) -> String {
    seen.peer.shown().map_or_else(|| "A device".to_string(), |shown| shown.tell.name.clone())
}

impl Controller {
    /// Called a few times a second: keeps the secret fresh, this device in the room, and
    /// what is known of the others up to date.
    pub fn link_tick(self: &Rc<Self>) {
        if !on() {
            return;
        }
        let now = now_ms();
        let account = self.settings.borrow().account();
        if self.link.borrow().account != account {
            // Another sign-in: nothing of the last one's carries over.
            self.link_stop();
            let mut link = self.link.borrow_mut();
            link.account = account;
            link.used = None;
        }
        let wanted = wanted(self.client().is_some(), self.can_host_jam(), self.settings.borrow().link_devices, self.link.borrow().off);
        if !wanted {
            self.link_stop();
            return self.link_show();
        }
        if self.link.borrow().session.as_ref().is_some_and(|session| !secret_holds(session.since(), now)) {
            log("session: the secret has not been read for ten minutes");
            self.link_end("Your devices can't be seen: the music server hasn't answered for ten minutes.");
        }
        let due = {
            let link = self.link.borrow();
            !link.looking && link.looked_ms.is_none_or(|at| now.saturating_sub(at) >= LOOK_AGAIN_MS)
        };
        if due {
            self.link_look(now);
        }
        self.link_peers(now);
        self.link_show();
    }

    /// Ends the session, if there is one: when the secret changes or runs out.
    fn link_end(&self, note: &str) {
        let mut link = self.link.borrow_mut();
        if link.end_session() {
            log("session: ended");
        }
        if !note.is_empty() {
            link.note = note.to_string();
        }
    }

    /// Ends everything of this sign-in's: at sign-out, when linking is no longer wanted,
    /// and when the account changes. A look still on its way is answered to nobody.
    pub fn link_stop(&self) {
        let mut link = self.link.borrow_mut();
        if link.session.is_some() {
            log("session: ended, with the sign-in it belonged to");
        }
        link.stop();
    }

    fn link_look(self: &Rc<Self>, now: u64) {
        let Some(client) = self.client() else { return };
        let (used, looks, pool) = {
            let mut link = self.link.borrow_mut();
            let (looks, over) = link.look_begins();
            (link.used.clone(), (looks, over), link.pool())
        };
        let (looks, over) = looks;
        pool.spawn(move || {
            let looked = look(&client, used, over);
            post(move |ctl| ctl.link_looked(looks, now, looked));
        });
    }

    /// A look has finished. `since` is when it began.
    fn link_looked(self: &Rc<Self>, looks: u64, since: u64, looked: Looked) {
        let mut forget = None;
        let begin = {
            let mut link = self.link.borrow_mut();
            // (Signed out since, or no longer wanted: the answer is nobody's.)
            if !link.expects(looks) {
                return;
            }
            link.looking = false;
            link.looked_ms = Some(now_ms());
            // A look that found the record is not one to use ends the session that used it
            // now, not when its ten minutes are up.
            if ends_session(&looked) && link.end_session() {
                log("session: ended, the record is no longer one to use");
            }
            match looked {
                Looked::Secret { secret, id, wrote } => {
                    // A page for that playlist written to disk before now (by a version that
                    // listed it) holds the secret: it goes.
                    forget = Some(format!("playlist:{id}"));
                    link.used = Some(id);
                    if wrote == Some(secret::Why::Public) {
                        link.note = "Your devices' link could be seen by others on the music server. It has been made private and reset.".into();
                    }
                    match link.session.as_mut().filter(|session| session.secret == secret && secret_holds(session.since(), now_ms())) {
                        Some(session) => {
                            session.since_ms.store(since, Ordering::Relaxed);
                            log("look: the secret is as it was");
                            None
                        }
                        None => Some(secret),
                    }
                }
                Looked::Stopped(stop) => {
                    link.note = match stop {
                        Stop::NotStated => "This music server doesn't say whose a playlist is or whether it is private, so devices can't be linked through it.",
                        Stop::Later => "Update Nexus Music on this PC to link it with your other devices.",
                        Stop::Mismatch => "The music server's answer about your devices' link didn't add up. It will be asked again.",
                        Stop::HasSongs => "You have a playlist named “Nexus Music link” with songs in it. Rename or delete it, and your devices can be linked.",
                    }
                    .into();
                    log(format!("look: stopped ({stop:?})"));
                    None
                }
                Looked::Failed => {
                    log("look: the music server could not be asked");
                    None
                }
                Looked::Invalid => {
                    link.note = "Your devices' link on the music server needs resetting, and couldn't be. It will be tried again.".into();
                    None
                }
                Looked::NoRandom => {
                    link.off = true;
                    link.note = "This PC couldn't make random numbers, so linking is off.".into();
                    None
                }
            }
        };
        if let Some(page) = forget {
            self.invalidate(&page);
        }
        if let Some(secret) = begin {
            self.link_begin(secret, since);
        }
    }

    /// A secret that is not the one in use: a new session, and the end of the old one.
    fn link_begin(self: &Rc<Self>, secret: Secret, since: u64) {
        self.link_end("");
        let (address, Some(key)) = (self.settings.borrow().jam_relay.clone(), config::load_secret(&self.paths, "jam-key")) else { return };
        let wire = Arc::new(Wire { base: address.trim_end_matches('/').to_string(), key, agent: crate::subsonic::make_agent_without_redirects() });
        let keys = Keys::of(&secret);
        let (room, stop) = (keys.room().to_string(), Arc::new(AtomicBool::new(false)));
        let serial = {
            let mut link = self.link.borrow_mut();
            link.serial += 1;
            link.note.clear();
            link.session = Some(Session { serial: link.serial, secret, keys, since_ms: Arc::new(AtomicU64::new(since)), wire: wire.clone(), stop: stop.clone(), place: None, others: Vec::new(), peers: HashMap::new() });
            link.serial
        };
        log("session: begun with a secret a look read");
        let _ = std::thread::Builder::new().name("link-room".into()).spawn(move || room_loop(wire, room, serial, stop));
    }

    fn link_said(&self, serial: u64, note: &str) {
        let mut link = self.link.borrow_mut();
        if link.session.as_ref().is_some_and(|session| session.serial == serial) {
            link.note = note.to_string();
        }
    }

    fn link_no_random(&self) {
        self.link_end("This PC couldn't make random numbers, so linking is off.");
        self.link.borrow_mut().off = true;
    }

    fn link_entered(&self, serial: u64, place: Place, token: String) {
        let mut link = self.link.borrow_mut();
        if let Some(session) = link.session.as_mut().filter(|session| session.serial == serial) {
            // A new place: whatever was asked from the old one cannot be answered to it.
            session.place = Some((place, token));
            session.others.clear();
            session.peers.clear();
        }
    }

    fn link_left(&self, serial: u64) {
        let mut link = self.link.borrow_mut();
        if let Some(session) = link.session.as_mut().filter(|session| session.serial == serial) {
            session.place = None;
            session.others.clear();
            session.peers.clear();
        }
    }

    /// Seals a message for another place and sends it. Returns its nonce.
    fn link_send(&self, to: Place, kind: u8, message: String) -> Option<Nonce> {
        let Some(nonce) = random::<24>() else {
            self.link_no_random();
            return None;
        };
        let mut link = self.link.borrow_mut();
        let pool = link.pool();
        // Under the session that is live now, and only inside its time.
        let serial = link.session.as_ref()?.serial;
        let session = link.live(serial, now_ms())?;
        let (place, token) = session.place.clone()?;
        let sealed = frame::seal(&session.keys, kind, &nonce, &place, &to, message.as_bytes())?;
        let (wire, room, stop, since) = (session.wire.clone(), session.keys.room().to_string(), session.stop.clone(), session.since_ms.clone());
        pool.spawn(move || {
            // The session may have ended, or run out of time, while this waited its turn:
            // then it is not sent.
            if !may_go(stop.load(Ordering::Relaxed), since.load(Ordering::Relaxed), now_ms()) {
                return log("send: the session ended or ran out first, not sent");
            }
            // Sent once. Whatever comes of it, it is not sent again by itself.
            let status = wire.post(&format!("api/link/{room}/send"), &token, &json!({"to": text::place(&to), "frame": sealed})).map(|(status, _)| status);
            if status != Some(200) {
                log(format!("send: the relay answered {status:?}"));
            }
        });
        Some(nonce)
    }

    /// What this device says of itself.
    fn link_state(&self, re: Option<Nonce>, ok: Option<bool>, why: Option<Why>) -> Tell {
        let device = self.link_device();
        let name = self.this_device().1;
        let rule = self.link_rule.borrow();
        let pb = self.playback.borrow();
        let now = pb.current();
        Tell {
            re,
            ok,
            why,
            device,
            name,
            at: At { generation: rule.generation(), minute: minute(now_ms()) },
            busy: rule.busy(now_ms()),
            by: rule.by(),
            playing: pb.playing && now.is_some(),
            title: now.map(|song| song.title.clone()).unwrap_or_default(),
            artist: now.map(|song| song.artist_label().to_string()).unwrap_or_default(),
        }
    }

    /// This installation's name for itself among the person's devices: made once.
    fn link_device(&self) -> [u8; 16] {
        if let Some(id) = text::read_id(&self.settings.borrow().link_device) {
            return id;
        }
        // (Not secret, and only ever sent sealed: if the system has no random numbers to
        // give, nothing is sent at all, so what this falls back on is never seen.)
        let id = random::<16>().unwrap_or_default();
        self.settings.borrow_mut().link_device = text::base64(&id);
        self.save_settings();
        id
    }

    /// What the relay handed over: who is in the room, and what was in this device's mailbox.
    fn link_heard(self: &Rc<Self>, serial: u64, places: Vec<String>, frames: Vec<(String, String)>) {
        let mine = {
            let mut link = self.link.borrow_mut();
            let Some(session) = link.live_mut(serial, now_ms()) else { return };
            let Some((mine, _)) = session.place.clone() else { return };
            session.others = places.iter().filter_map(|place| text::read_place(place)).filter(|place| *place != mine).take(15).collect();
            let others = session.others.clone();
            session.peers.retain(|place, _| others.contains(place));
            mine
        };
        for (from, sealed) in frames {
            let Some(from) = text::read_place(&from) else { continue };
            let opened = {
                let link = self.link.borrow();
                // (Checked for each frame: the one before may have ended the session.)
                let Some(session) = link.live(serial, now_ms()) else { return };
                frame::open(&session.keys, &sealed, &from, &mine)
            };
            let Some(opened) = opened else {
                log(format!("frame from {}: does not open, dropped", text::place(&from)));
                continue;
            };
            match opened.kind {
                ASK => self.link_asked(serial, from, opened.nonce, &opened.message),
                TELL => self.link_told(serial, from, &opened.message),
                _ => {}
            }
        }
    }

    /// Another device asks something of this one.
    fn link_asked(self: &Rc<Self>, serial: u64, from: Place, nonce: Nonce, message: &[u8]) {
        let now = now_ms();
        if !self.link.borrow_mut().asks.allow(now) {
            return log("ask: more than this device answers in a second, dropped");
        }
        let who = text::place(&from);
        let answer = match read_ask(message) {
            None => {
                log(format!("ask from {who}: not one this version carries out"));
                self.link_state(Some(nonce), Some(false), Some(Why::Invalid))
            }
            Some(Ask::Look) => self.link_state(Some(nonce), None, None),
            Some(Ask::Pause { at }) => {
                // Checked as it arrives, before anything is done for it...
                let admitted = self.link_rule.borrow_mut().admit(at, minute(now), now);
                // (For a test only: the Pause is carried out a while after it is accepted,
                // so that something can be made to happen in between.)
                if let (Ok(_), Some(wait)) = (&admitted, slow_pause()) {
                    let ctl = self.clone();
                    log(format!("pause from {who}: accepted, and held {} ms before it is carried out (a test)", wait.as_millis()));
                    return slint::Timer::single_shot(wait, move || {
                        let answer = ctl.link_pause_now(serial, from, admitted, at, nonce);
                        ctl.link_answer(serial, from, answer);
                    });
                }
                self.link_pause_now(serial, from, admitted, at, nonce)
            }
        };
        self.link_answer(serial, from, answer);
    }

    /// The moment the player is changed for a Pause from another device: everything is
    /// checked again, under the session the Pause arrived in. Returns what to tell it.
    fn link_pause_now(self: &Rc<Self>, serial: u64, from: Place, admitted: Result<Reservation, Why>, at: At, nonce: Nonce) -> Tell {
        let who = text::place(&from);
        let done = admitted.and_then(|reservation| {
            let same_session = self.link.borrow().live(serial, now_ms()).is_some();
            self.link_rule.borrow_mut().finish(reservation, at, minute(now_ms()), same_session, nonce)?;
            self.pause_here();
            Ok(())
        });
        match done {
            Ok(()) => {
                log(format!("pause from {who} against generation {}: carried out", at.generation));
                self.link_state(Some(nonce), Some(true), None)
            }
            Err(why) => {
                log(format!("pause from {who} against generation {}: refused, {why:?} (this device is at {}{})", at.generation, self.link_rule.borrow().generation(), if self.link_rule.borrow().busy(now_ms()) { ", busy" } else { "" }));
                self.link_state(Some(nonce), Some(false), Some(why))
            }
        }
    }

    /// Answers an ask, if the session it arrived under is still the live one: an answer is
    /// not sent under another session, to a place that session has never heard of.
    fn link_answer(&self, serial: u64, to: Place, answer: Tell) {
        if self.link.borrow().live(serial, now_ms()).is_none() {
            return log("answer: the session it belonged to has ended, not sent");
        }
        self.link_send(to, TELL, answer.text());
    }

    /// Another device says something of itself.
    fn link_told(self: &Rc<Self>, serial: u64, from: Place, message: &[u8]) {
        let Some(tell) = read_tell(message) else {
            return log(format!("tell from {}: breaks its shape, dropped", text::place(&from)));
        };
        let (told, name) = {
            let mut link = self.link.borrow_mut();
            let Some(session) = link.live_mut(serial, now_ms()) else { return };
            let Some(seen) = session.peers.get_mut(&from) else { return };
            let told = seen.peer.told(tell);
            (told, name_of(seen))
        };
        match told {
            Told::Dropped => log(format!("tell from {name}: answers nothing waited on, dropped")),
            Told::Shown => {}
            Told::Settled(outcome) | Told::ShownAndSettled(outcome) => {
                self.link_outcome(&name, outcome);
                // The answer to a command does not change what is shown: look again.
                self.link_look_at(from, false);
            }
        }
    }

    fn link_outcome(&self, name: &str, outcome: Outcome) {
        let note = match outcome {
            Outcome::Done => format!("Paused {name}."),
            Outcome::Refused(Why::Changed) => format!("{name} changed before that reached it. Nothing was done."),
            Outcome::Refused(Why::Busy) => format!("{name} was in the middle of something. Nothing was done."),
            Outcome::Refused(Why::Late) => format!("That took too long to reach {name}. Nothing was done."),
            Outcome::Refused(Why::Invalid | Why::Cannot) => format!("{name} couldn't do that."),
            Outcome::NotReached => format!("That hadn't reached {name} when checked."),
            Outcome::Unknown => format!("{name} changed, or didn't answer. Check what is playing before you press again."),
        };
        log(format!("outcome: {note}"));
        self.link.borrow_mut().note = note;
    }

    /// Sends a look to a place. `checking` says it is the one look that asks what became of
    /// a command that has gone unanswered.
    fn link_look_at(&self, to: Place, checking: bool) {
        let Some(nonce) = self.link_send(to, ASK, Ask::Look.text()) else { return };
        let now = now_ms();
        let mut link = self.link.borrow_mut();
        if let Some(seen) = link.session.as_mut().and_then(|session| session.peers.get_mut(&to)) {
            seen.looked_ms = Some(now);
            if checking { seen.peer.checking(nonce, now) } else { seen.peer.looked(nonce, now) }
        }
    }

    /// Looks at each of the other devices every few seconds, and gives up waiting for the
    /// answer to a command when it has waited long enough.
    fn link_peers(&self, now: u64) {
        let mut to_look = Vec::new();
        let mut settled = Vec::new();
        {
            let mut link = self.link.borrow_mut();
            let Some(session) = link.session.as_mut().filter(|session| session.place.is_some()) else { return };
            for place in session.others.clone() {
                let seen = session.peers.entry(place).or_default();
                match seen.peer.tick(now) {
                    Tick::Check => to_look.push((place, true)),
                    Tick::Settled(outcome) => settled.push((name_of(seen), outcome)),
                    Tick::Nothing if seen.looked_ms.is_none_or(|at| now.saturating_sub(at) >= LOOK_EVERY_MS) => to_look.push((place, false)),
                    Tick::Nothing => {}
                }
            }
        }
        for (name, outcome) in settled {
            self.link_outcome(&name, outcome);
        }
        for (place, checking) in to_look {
            self.link_look_at(place, checking);
        }
    }

    /// Pause was pressed for another device.
    fn link_pause(self: &Rc<Self>, place: &str) {
        let Some(to) = text::read_place(place) else { return };
        let now = now_ms();
        let (may, name) = {
            let link = self.link.borrow();
            let Some(seen) = link.session.as_ref().and_then(|session| session.peers.get(&to)) else { return };
            (seen.peer.may_send(now), name_of(seen))
        };
        let at = match may {
            Ok(at) => at,
            Err(wait) => {
                self.link.borrow_mut().note = match wait {
                    Wait::Look => format!("Looking at {name} first. Press again in a moment."),
                    Wait::Busy => format!("{name} is in the middle of something. Press again in a moment."),
                    Wait::Unanswered => format!("Still waiting for {name} to answer."),
                };
                if wait != Wait::Unanswered {
                    self.link_look_at(to, false);
                }
                return self.link_show();
            }
        };
        let Some(nonce) = self.link_send(to, ASK, Ask::Pause { at }.text()) else { return };
        log(format!("pause: asked {name}, against generation {} in minute {}", at.generation, at.minute));
        let mut link = self.link.borrow_mut();
        link.note = format!("Asked {name} to pause…");
        if let Some(seen) = link.session.as_mut().and_then(|session| session.peers.get_mut(&to)) {
            seen.peer.sent(nonce, at, now);
        }
    }

    /// Shows the other devices and what each is doing.
    fn link_show(&self) {
        let now = now_ms();
        let link = self.link.borrow();
        let mut rows = Vec::new();
        if let Some(session) = &link.session {
            for place in &session.others {
                let Some(shown) = session.peers.get(place).and_then(|seen| seen.peer.shown()) else { continue };
                let tell = &shown.tell;
                let mut about = match (tell.busy, tell.title.is_empty(), tell.playing) {
                    (true, _, _) => "In the middle of something…".to_string(),
                    (_, true, _) => "Nothing playing".to_string(),
                    (_, _, true) => format!("Playing {}", tell.title),
                    (_, _, false) => format!("Paused on {}", tell.title),
                };
                let age = now.saturating_sub(shown.sent_ms) / 1000;
                if age >= 20 {
                    about.push_str(&format!(" (as of {age} seconds ago)"));
                }
                rows.push((tell.device, shown.sent_ms, ui::LinkedDevice { place: text::place(place).into(), name: tell.name.as_str().into(), about: about.into(), playing: tell.playing }));
            }
        }
        let list = latest_of_each(rows);
        let global = self.ui.global::<ui::Linked>();
        global.set_on(true);
        global.set_entered(link.session.as_ref().is_some_and(|session| session.place.is_some()));
        global.set_note(link.note.as_str().into());
        // (Only when it has changed: the list is redrawn a few times a second otherwise.)
        let before: Vec<ui::LinkedDevice> = slint::Model::iter(&global.get_list()).collect();
        if before != list {
            log(format!("shown: {}", if list.is_empty() { "no other devices".to_string() } else { list.iter().map(|device| format!("{}: {}", device.name, device.about)).collect::<Vec<_>>().join("; ") }));
            global.set_list(ModelRc::new(VecModel::from(list)));
        }
    }

    pub fn wire_link(self: &Rc<Self>) {
        self.ui.global::<ui::Linked>().on_pause(|place| {
            with(|ctl| ctl.link_pause(place.as_str()));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_playlists_the_app_keeps_for_itself_are_not_listed() {
        assert!(unlisted_when("Nexus Music sync", false) && unlisted_when("Nexus Music sync", true));
        // The one that holds the secret: where linking is on. With it off, a playlist of
        // that name is the person's own and is listed like any other.
        assert!(unlisted_when("Nexus Music link", true) && !unlisted_when("Nexus Music link", false));
        assert!(!unlisted_when("Morning mix", true));
    }

    /// The secret is the comment of a playlist, and a playlist's page holds its comment.
    #[test]
    fn a_page_for_a_playlist_the_app_keeps_for_itself_is_never_written_to_disk() {
        use crate::pages::PageData;
        use crate::subsonic::Playlist;
        let dir = std::env::temp_dir().join(format!("nexus-link-page-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let page = |name: &str| PageData { title: name.to_string(), playlist: Some(Playlist { id: "p".into(), name: name.into(), comment: r#"{"nexus_music_link":1,"secret":"ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8"}"#.into(), ..Default::default() }), ..Default::default() };
        for (name, kept) in [("Morning mix", true), ("Nexus Music link", false), ("Nexus Music sync", false)] {
            let file = dir.join(name);
            crate::controller::store_page(&file, &page(name));
            assert_eq!(file.exists(), kept, "{name}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A copy with a session that began with a look at `since`.
    fn linked(since: u64) -> Link {
        let secret = Secret::new([7; 32]);
        let wire = Arc::new(Wire { base: "http://127.0.0.1:9".into(), key: "a-key".into(), agent: crate::subsonic::make_agent_without_redirects() });
        let mut link = Link::default();
        link.serial = 1;
        link.session = Some(Session { serial: 1, keys: Keys::of(&secret), secret, since_ms: Arc::new(AtomicU64::new(since)), wire, stop: Arc::new(AtomicBool::new(false)), place: Some(([1; 16], "token".into())), others: Vec::new(), peers: HashMap::new() });
        link
    }

    /// Sign-out while the first look at the secret is still on its way: its answer must
    /// start nothing, and nothing queued under the old session is sent.
    #[test]
    fn signing_out_leaves_a_look_s_answer_and_queued_sends_nothing_to_act_on() {
        let mut link = linked(0);
        let (looks, _) = link.look_begins();
        let queued = link.session.as_ref().map(|session| session.stop.clone()).unwrap();
        assert!(link.expects(looks) && !queued.load(Ordering::Relaxed));
        link.stop();
        assert!(!link.expects(looks), "the look's answer is still wanted");
        assert!(queued.load(Ordering::Relaxed), "what was queued would still be sent");
        assert!(link.session.is_none() && !link.looking && link.looked_ms.is_none());
        // The next sign-in's look is its own.
        let (next, _) = link.look_begins();
        assert!(link.expects(next) && next != looks);
    }

    /// A session is the same session at every use: itself, and inside its time.
    #[test]
    fn a_session_is_live_only_as_itself_and_inside_its_ten_minutes() {
        let mut link = linked(1_000);
        assert!(link.live(1, 1_000).is_some() && link.live(1, 1_000 + nexus_core::link::SECRET_LIFE_MS - 1).is_some());
        // Ten minutes on, before any tick has ended it: a frame is not opened under it, a
        // Pause is not carried out under it, nothing is sent under it.
        assert!(link.live(1, 1_000 + nexus_core::link::SECRET_LIFE_MS).is_none(), "a session past its time is still used");
        assert!(link.live_mut(1, 1_000 + nexus_core::link::SECRET_LIFE_MS).is_none());
        // Another session's number is not this one, and an ended one is nobody's.
        assert!(link.live(2, 1_000).is_none());
        assert!(link.end_session() && !link.end_session());
        assert!(link.live(1, 1_000).is_none() && link.serial == 2);
    }

    /// A music server that answers from a script: what each fetch of the playlist gives
    /// (`None`: no answer), and whether listing and writing work.
    struct Scripted {
        fetches: Vec<Option<String>>,
        listed: Vec<Stated>,
        list_works: bool,
        write_works: bool,
        asked: Vec<&'static str>,
        /// The person signs out once this many requests have been made.
        signed_out_after: Option<usize>,
    }

    impl Scripted {
        fn with(fetches: Vec<Option<String>>) -> Scripted {
            let listed = vec![Stated { id: "k".into(), name: secret::PLAYLIST_NAME.into(), owner: Some("tester".into()), comment: None }];
            Scripted { fetches, listed, list_works: true, write_works: true, asked: Vec::new(), signed_out_after: None }
        }
    }

    impl Server for Scripted {
        fn list(&mut self) -> Option<Vec<Stated>> {
            self.asked.push("list");
            self.list_works.then(|| self.listed.clone())
        }
        fn fetch(&mut self, _id: &str) -> Option<String> {
            self.asked.push("fetch");
            if self.fetches.is_empty() { None } else { self.fetches.remove(0) }
        }
        fn make(&mut self) -> Option<String> {
            self.asked.push("make");
            Some("made".into())
        }
        fn write(&mut self, _id: &str, _comment: &str) -> Option<()> {
            self.asked.push("write");
            self.write_works.then_some(())
        }
        fn random(&mut self) -> Option<[u8; 32]> {
            Some([9; 32])
        }
        fn still(&self) -> bool {
            self.signed_out_after.is_none_or(|requests| self.asked.len() < requests)
        }
    }

    /// The playlist as Navidrome answers for it: the record, or somebody's note; private or not.
    fn playlist(comment: &str, public: bool) -> Option<String> {
        let comment = comment.replace('"', "&#34;");
        Some(format!(r#"<subsonic-response status="ok"><playlist id="k" name="Nexus Music link" comment="{comment}" songCount="0" public="{public}" owner="tester"></playlist></subsonic-response>"#))
    }

    const RECORD: &str = r#"{"nexus_music_link":1,"secret":"ICEiIyQlJicoKSorLC0uLzAxMjM0NTY3ODk6Ozw9Pj8"}"#;

    /// The reviewer's case: the record is found public, a new secret is written, and the
    /// read that should show it fails. The old secret must not live on.
    #[test]
    fn a_record_seen_to_be_no_good_is_not_forgotten_when_a_later_request_fails() {
        let found = |fetches: Vec<Option<String>>| {
            let mut server = Scripted::with(fetches);
            let looked = look_with(&mut server, "tester", Some("k".into()));
            (ends_session(&looked), matches!(looked, Looked::Secret { .. }), server.asked)
        };
        // Public; written; the read-back gets no answer.
        let (ends, _, asked) = found(vec![playlist(RECORD, true), None]);
        assert!(ends, "the old secret would live on until its time ran out");
        assert_eq!(asked, ["list", "fetch", "write", "list", "fetch"]);
        // The same when the list after the write gets no answer, when the answer is not one
        // about a playlist, and when what was written keeps not being there.
        let mut server = Scripted::with(vec![playlist(RECORD, true)]);
        server.listed.clear();
        assert!(ends_session(&look_with(&mut server, "tester", Some("gone".into()))) || server.asked.contains(&"make"));
        assert!(found(vec![playlist("somebody's note", false), Some("not an answer".into())]).0);
        assert!(found(vec![playlist(RECORD, true), playlist(RECORD, true), playlist(RECORD, true)]).0);
        // Unless a later read shows it good: then that is the secret to use.
        let (ends, good, _) = found(vec![playlist(RECORD, true), playlist(RECORD, false)]);
        assert!(!ends && good);
        // And with nothing seen to be wrong, not being able to ask concludes nothing.
        let (ends, good, asked) = found(vec![None]);
        assert!(!ends && !good && asked == ["list", "fetch"]);
        let mut server = Scripted::with(vec![]);
        server.list_works = false;
        assert!(!ends_session(&look_with(&mut server, "tester", Some("k".into()))));
        // A record found no good that cannot be written over is no good either.
        let mut server = Scripted::with(vec![playlist(RECORD, true)]);
        server.write_works = false;
        assert!(ends_session(&look_with(&mut server, "tester", Some("k".into()))));
    }

    /// The person signs out while a refresh is under way: the request in flight finishes,
    /// and the refresh makes no other.
    #[test]
    fn a_refresh_makes_no_request_after_the_sign_in_it_is_for_has_ended() {
        // A look that would take five requests: list, fetch (public), write, list, fetch.
        let whole = || Scripted::with(vec![playlist(RECORD, true), playlist(RECORD, false)]);
        let mut server = whole();
        look_with(&mut server, "tester", Some("k".into()));
        assert_eq!(server.asked, ["list", "fetch", "write", "list", "fetch"]);
        // Signed out during each of them in turn: that one is the last.
        for during in 1..=5 {
            let mut server = whole();
            server.signed_out_after = Some(during);
            let looked = look_with(&mut server, "tester", Some("k".into()));
            assert_eq!(server.asked.len(), during.min(5), "signed out during request {during}: {:?}", server.asked);
            if during < 5 {
                assert!(matches!(looked, Looked::Failed));
            }
        }
        // Signed out before it began, it asks nothing at all. And with a playlist to make.
        let mut server = whole();
        server.signed_out_after = Some(0);
        look_with(&mut server, "tester", None);
        assert!(server.asked.is_empty());
        let mut server = Scripted::with(vec![]);
        server.listed.clear();
        server.signed_out_after = Some(2);
        look_with(&mut server, "tester", None);
        assert_eq!(server.asked, ["list", "make"]);
    }

    /// The flag a look watches is the one the end of its sign-in sets, and the next look's
    /// is another.
    #[test]
    fn the_end_of_a_sign_in_is_seen_by_the_look_that_is_under_way() {
        let mut link = linked(0);
        let (_, over) = link.look_begins();
        assert!(!over.load(Ordering::Relaxed));
        link.stop();
        assert!(over.load(Ordering::Relaxed), "the look would go on asking the music server");
        let (_, next) = link.look_begins();
        assert!(!next.load(Ordering::Relaxed) && over.load(Ordering::Relaxed));
        // A session that merely ends (its secret changed or ran out) is not the sign-in ending.
        link.end_session();
        assert!(!next.load(Ordering::Relaxed));
    }

    /// What is queued to send goes only under a session that has not ended and is inside
    /// its ten minutes when its turn comes.
    #[test]
    fn something_queued_to_send_is_not_sent_once_the_session_has_ended_or_run_out() {
        let life = nexus_core::link::SECRET_LIFE_MS;
        assert!(may_go(false, 1_000, 1_000) && may_go(false, 1_000, 1_000 + life - 1));
        assert!(!may_go(true, 1_000, 1_000), "ended");
        assert!(!may_go(false, 1_000, 1_000 + life), "run out, and nothing has ended it yet");
        // A look that renews the secret while it waits renews it for what is queued too.
        let link = linked(1_000);
        let session = link.session.as_ref().unwrap();
        let (queued_since, queued_stop) = (session.since_ms.clone(), session.stop.clone());
        session.since_ms.store(1_000 + life, Ordering::Relaxed);
        assert!(may_go(queued_stop.load(Ordering::Relaxed), queued_since.load(Ordering::Relaxed), 1_500 + life));
    }

    /// A refresh that finds the record public and cannot repair it, not ours, changed in
    /// layout, or no longer stated, ends the session at once.
    #[test]
    fn a_look_that_finds_the_record_no_good_ends_the_session_at_once() {
        for found in [Looked::Invalid, Looked::Stopped(Stop::Mismatch), Looked::Stopped(Stop::NotStated), Looked::Stopped(Stop::Later), Looked::Stopped(Stop::HasSongs)] {
            assert!(ends_session(&found));
        }
        // Not being able to ask is no evidence of anything, and the same secret read again
        // is the session carrying on.
        assert!(!ends_session(&Looked::Failed));
        assert!(!ends_session(&Looked::Secret { secret: Secret::new([7; 32]), id: "k".into(), wrote: None }));
    }

    /// Seen in the run: a copy that was started again was listed twice, once at the place
    /// it had left, growing older by the second.
    #[test]
    fn a_device_that_was_started_again_is_shown_once_as_it_last_answered() {
        let (study, phone) = ([1; 16], [2; 16]);
        let rows = vec![(study, 1_000, "Study PC, playing, at its old place"), (phone, 4_000, "Phone"), (study, 30_000, "Study PC, paused, at its new place")];
        assert_eq!(latest_of_each(rows), vec!["Study PC, paused, at its new place", "Phone"]);
        // The older answer does not replace the newer whichever order the places come in.
        assert_eq!(latest_of_each(vec![(study, 30_000, "new"), (study, 1_000, "old")]), vec!["new"]);
        assert_eq!(latest_of_each(Vec::<([u8; 16], u64, &str)>::new()), Vec::<&str>::new());
    }

    #[test]
    fn link_my_devices_off_is_off_for_the_new_linking_too() {
        assert!(wanted(true, true, true, false));
        assert!(!wanted(true, true, false, false), "the setting is off");
        assert!(!wanted(false, true, true, false) && !wanted(true, false, true, false) && !wanted(true, true, true, true));
    }

    #[test]
    fn with_linking_off_its_playlist_s_name_is_anyone_s_to_use() {
        assert_eq!(kept_name_when("Nexus Music link", false), None);
        assert_eq!(kept_name_when("Nexus Music link", true), Some(secret::RESERVED));
        // The record's name is kept either way, with the record's own words.
        assert_eq!(kept_name_when("Nexus Music sync", false), Some(nexus_core::step::RESERVED));
        assert_eq!(kept_name_when("Nexus Music sync", true), Some(nexus_core::step::RESERVED));
        assert_eq!(kept_name_when("Morning mix", true), None);
    }
}
