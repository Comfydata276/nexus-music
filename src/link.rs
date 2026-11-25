//! Linking a person's devices the new way. Copies of the app signed in to one account find
//! each other through the relay's mailbox route; each shows what the others are playing;
//! one can steer another (the player and the queue panel become the other's, and what is
//! pressed is asked of it), and music moves from one to another.
//!
//! It is **off** unless the environment says `NEXUS_MUSIC_LINK=1`, and has no setting. The
//! old linking (`jam.rs`) is beside it and unchanged; where the old steers a device by
//! joining its session, this asks the same things as sealed commands.
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

use nexus_core::link::command::{MOST_ADDED, id_ok};
use nexus_core::link::frame::{self, ASK, Keys, TELL};
use nexus_core::link::message::{Adopt, Ask, At, Command, Entry, Full, Handed, Tell, Want, Why, read_ask, read_tell};
use nexus_core::link::moving::{Adopted, Adopting, Gave, Giving, START_MS, Taking, Took};
use nexus_core::link::news::News;
use nexus_core::link::rule::{Reservation, Waiting};
use nexus_core::link::secret::{self, Next, Secret, Stop, Verdict};
use nexus_core::link::steer::{Outcome, Peer, Press, Tick, Told, Wait};
use nexus_core::link::{Asks, DeviceId, LOOK_AGAIN_MS, LOOK_EVERY_MS, LOOK_IDLE_MS, Nonce, Place, VERSION, WAIT_MS, minute, secret_holds, text};
use nexus_core::step::Stated;
use serde_json::{Value, json};
use slint::{ComponentHandle, ModelRc, VecModel};

use crate::config;
use crate::controller::{Controller, clock, post, with};
use crate::jam::Remote;
use crate::pool::Pool;
use crate::subsonic::{Client, Song};
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

    /// A command from another device is being carried out. It took its place in the order
    /// when the rule let it through (`Rule::finish`): what the player does for it, until
    /// this is dropped, is that one change and is not counted again.
    pub fn carrying_out(&self) -> Changing<'_> {
        Changing { flag: &self.link_changing, outer: self.link_changing.replace(true) }
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
fn look(client: &Client, used: Option<String>, over: Arc<AtomicBool>, reset: bool) -> Looked {
    let mut server = Navidrome { client, agent: crate::subsonic::make_agent_without_redirects(), over };
    look_with(&mut server, client.username(), used, reset)
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
///
/// **A look that resets** ("Reset linking") does everything a look does, and where it
/// would have used the secret it read, writes a new one in its place and reads that back.
/// It is not a different path to the playlist: the same answer is checked the same way
/// before anything is written, so a reset cannot write into a playlist a look would not use.
fn look_with(server: &mut impl Server, username: &str, mut used: Option<String>, mut reset: bool) -> Looked {
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
            Verdict::Use(_) if reset => {
                reset = false;
                let Some(fresh) = server.random().map(Secret::new) else { return Looked::NoRandom };
                if !server.still() {
                    return Looked::Failed;
                }
                // (If it cannot be written, the secret that is there stands, and the look
                // says only that the server could not be asked.)
                if server.write(&id, &fresh.comment()).is_none() {
                    return unasked(no_good);
                }
                log(format!("look: wrote a new secret into {id} (Reset)"));
                wrote = Some(secret::Why::Reset);
                used = Some(id);
                // The secret that was read is the record no longer: if the new one cannot
                // be read back, the look says so, and the old one is not used meanwhile.
                no_good = true;
            }
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

    /// Where a device is in the room now: the place that last answered as it. A copy that
    /// was started again is at two places for a while, and its latest answer is the one
    /// that counts.
    fn place_of(&self, device: &DeviceId) -> Option<Place> {
        let answered = |place: &Place| self.peers.get(place)?.peer.shown().filter(|shown| shown.tell.device == *device).map(|shown| shown.sent_ms);
        self.others.iter().filter_map(|place| answered(place).map(|at| (at, *place))).max_by_key(|(at, _)| *at).map(|(_, place)| place)
    }
}

/// Whether something queued to be sent under a session may go when its turn comes: only if
/// the session has not ended and its secret is still inside its time. Both, because a
/// session that has run out is no session, whether or not anything has ended it yet.
fn may_go(session_ended: bool, since_ms: u64, now_ms: u64) -> bool {
    !session_ended && secret_holds(since_ms, now_ms)
}

/// What was last asked of a device, so that what became of it can be said.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Out {
    /// Pause, from the list of devices.
    Pause,
    /// Something pressed while steering it: the screen shows what became of it.
    Steer,
    /// Songs to queue there, and how many.
    Add(usize),
    /// A queue to play there now, while steering it; and how many of its songs were left behind.
    PlayThere(usize),
    /// This PC's music, to carry on there; how many songs were sent, and how many left behind.
    Give(Giving, usize, usize),
    /// The pause that completes bringing its music here.
    TakePause,
}

#[derive(Default)]
struct Seen {
    peer: Peer,
    /// When it was last looked at.
    looked_ms: Option<u64>,
    /// What the command that is out was for. What waits behind it is a press made while
    /// steering, or songs to add.
    out: Option<Out>,
}

/// What the device being steered was last heard to be doing, as the player shows it; with
/// what has been asked of it since shown as done, until its own word on it arrives.
struct View {
    full: Full,
    playing: bool,
    /// When that was heard.
    heard_ms: u64,
}

/// One of the person's other devices is being steered from here: the player and the queue
/// panel are its, and what is pressed is asked of it.
struct Steering {
    device: DeviceId,
    name: String,
    view: Option<View>,
    /// Its songs as this PC's own library knows them, by id; `None` for one it doesn't
    /// have. Kept so each is looked up once.
    known: HashMap<String, Option<Song>>,
    /// Since when it has had no place in the room.
    lost_ms: Option<u64>,
}

/// Music is being brought here from another device.
enum Take {
    /// Its queue has been asked for.
    Asked { place: Place, nonce: Nonce, name: String, pressed_ms: u64 },
    /// Its queue is loaded here, and this PC's sound is awaited: `heard_from` is where in
    /// the song the sound was when it was first heard on it.
    Loaded { place: Place, name: String, taking: Taking, heard_from: Option<f64> },
}

/// A queue another device sent has been loaded here. It is answered when it sounds.
struct Adoption {
    serial: u64,
    from: Place,
    nonce: Nonce,
    adopting: Adopting,
    /// This PC is busy to other devices until then.
    waiting: Waiting,
    /// Where in the song the sound was when it was first heard on the queue.
    heard_from: Option<f64>,
}

/// How far the sound has to have moved on, in seconds, to count as having started.
const SOUNDS_AFTER: f64 = 0.2;

/// How long a device that was being steered may be gone from the room before the player
/// goes back to this PC.
const LOST_MS: u64 = 45_000;
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
    /// "Reset linking" was pressed: the next look writes a new secret.
    reset: bool,
    session: Option<Session>,
    serial: u64,
    asks: Asks,
    /// What there is to say about linking, under the list.
    note: String,
    /// The system could not give random numbers: linking is off for this run.
    off: bool,
    pool: Option<Arc<Pool>>,
    steering: Option<Steering>,
    take: Option<Take>,
    adoption: Option<Adoption>,
    news: News,
}

impl Link {
    fn pool(&mut self) -> Arc<Pool> {
        self.pool.get_or_insert_with(|| Arc::new(Pool::new("link", 2, false))).clone()
    }

    /// The session, if `serial` is the one that is live **and its secret is inside its
    /// time**. Asked at every use of a session: when a look's answer would renew it, when
    /// something is sent, when a frame is opened, and at the moment the player is changed.
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
        // A queue that was asked for under it will not be answered under another.
        if matches!(self.take, Some(Take::Asked { .. })) {
            self.take = None;
        }
        self.news.forget();
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
        self.reset = false;
        self.steering = None;
        self.take = None;
    }

    /// "Link my devices" was set. Off is off at once: everything of this sign-in's ends
    /// here, in the same step as the setting is saved, so that a frame already on its way
    /// to this thread finds no session to act under. Says whether anything was ended.
    fn setting(&mut self, link_my_devices: bool) -> bool {
        if link_my_devices {
            return false;
        }
        self.stop();
        true
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

    /// The place of the device being steered, if it has one in the room now.
    fn steered_place(&self) -> Option<Place> {
        self.session.as_ref()?.place_of(&self.steering.as_ref()?.device)
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

/// What a device is doing, in a few words, for the list.
fn about(tell: &Tell, age_s: u64) -> String {
    let mut about = match (tell.steering.is_empty(), tell.busy, tell.title.is_empty(), tell.playing) {
        (false, ..) => format!("Steering {}", tell.steering),
        (_, true, _, _) => "In the middle of something…".to_string(),
        (_, _, true, _) => "Nothing playing".to_string(),
        (_, _, _, true) => format!("Playing {}", tell.title),
        (_, _, _, false) => format!("Paused on {}", tell.title),
    };
    if age_s >= 50 {
        about.push_str(&format!(" (as of {age_s} seconds ago)"));
    }
    about
}

/// What a command needs found before it can be carried out.
enum Ready {
    Nothing,
    /// The entry of the queue it means, as it stands now.
    Entry(usize),
    /// The songs to queue.
    Songs(Vec<Song>),
    /// The queue to take, and where in it to start.
    Queue(Vec<Song>, usize),
}

enum Prepared {
    Ready(Ready),
    /// Songs this PC's copy of the library does not have: the music server is asked.
    Find(Vec<String>),
    Cannot,
}

/// The songs of a queue sent by another device, as this library knows them, and where in
/// them the one to start with is. Songs this library doesn't have are left out; the ones
/// straight after the first that were queued by hand stay so.
fn known_songs(ids: &[String], start: usize, queued: usize) -> (Vec<Song>, usize) {
    let Some(index) = crate::search::current() else { return (Vec::new(), 0) };
    let known: HashMap<&str, &Song> = index.library.songs.iter().map(|song| (song.id.as_str(), song)).collect();
    let (mut songs, mut begin) = (Vec::new(), 0);
    for (place, id) in ids.iter().enumerate() {
        if place == start {
            begin = songs.len();
        }
        if let Some(song) = known.get(id.as_str()) {
            songs.push(Song { queued: place > start && place <= start + queued, ..(*song).clone() });
        }
    }
    let begin = begin.min(songs.len().saturating_sub(1));
    (songs, begin)
}

fn entry_of(song: &Song) -> Entry {
    Entry { id: song.id.clone(), title: song.title.clone(), artist: song.artist_label().to_string(), album: song.album.clone(), duration: song.duration, queued: song.queued }
}

/// What the person is told when music was to move to another device.
fn gave_words(gave: Gave, name: &str, left: usize, sent: usize) -> String {
    match gave {
        Gave::Moved if left > 0 => format!("Playing on {name}. The next {sent} of {} songs were moved.", sent + left),
        Gave::Moved => format!("Playing on {name}."),
        Gave::StartedAndThisChanged => format!("{name} has started. This PC was given something else to do meanwhile, so it was left as it is: both may be playing."),
        Gave::StartedLate => format!("{name} has started, too late for this PC to stop by itself. Both are playing."),
        Gave::Stayed(Why::Busy) => format!("{name} was in the middle of something, so nothing was moved. Still playing here."),
        // (Before the queue reached it, or after it took the queue and before it sounded.)
        Gave::Stayed(Why::Changed) => format!("{name} was given something else to do before it started. Still playing here."),
        Gave::Stayed(Why::Late) => format!("That took too long to reach {name}, so nothing was moved. Still playing here."),
        Gave::Stayed(Why::Cannot | Why::Invalid) => format!("{name} couldn't play that. Still playing here."),
        Gave::NotReached => format!("That hadn't reached {name} when checked. Still playing here; {name} may yet start."),
        Gave::Unknown => format!("{name} didn't answer. Still playing here; check whether {name} started too."),
    }
}

/// What the person is told when something asked of another device was not done, or what
/// became of it is not known.
fn outcome_words(outcome: Outcome, name: &str) -> String {
    match outcome {
        Outcome::Done => String::new(),
        Outcome::Refused(Why::Changed) => format!("{name} changed before that reached it. Nothing was done."),
        Outcome::Refused(Why::Busy) => format!("{name} was in the middle of something. Nothing was done."),
        Outcome::Refused(Why::Late) => format!("That took too long to reach {name}. Nothing was done."),
        Outcome::Refused(Why::Invalid | Why::Cannot) => format!("{name} couldn't do that."),
        Outcome::NotReached => format!("That hadn't reached {name} when checked."),
        Outcome::Unknown => format!("{name} changed, or didn't answer. Check what is playing before you press again."),
    }
}

/// What a press shows in the player straight away, before the other device's own word on
/// it arrives.
fn shown_at_once(view: &mut View, command: &Command, now_ms: u64, position: f64) {
    let settle = |view: &mut View, playing: bool, at: f64| {
        view.playing = playing;
        view.full.position = at;
        view.heard_ms = now_ms;
    };
    match command {
        Command::Play => settle(view, true, position),
        Command::Pause => settle(view, false, position),
        Command::Seek { to } => settle(view, view.playing, *to),
        Command::Volume { to } => view.full.volume = *to,
        Command::Shuffle { on } => view.full.shuffle = *on,
        Command::Repeat { mode } => view.full.repeat = *mode,
        Command::Remove { at, .. } => {
            if (*at as usize) < view.full.queue.len() {
                view.full.queue.remove(*at as usize);
            }
        }
        Command::Move { from, to, queued, .. } => {
            let (from, to) = (*from as usize, *to as usize);
            if from < view.full.queue.len() && to < view.full.queue.len() {
                let mut moved = view.full.queue.remove(from);
                moved.queued = *queued;
                view.full.queue.insert(to, moved);
            }
        }
        _ => {}
    }
}

/// Where the song on the device being steered has got to: where it was when last heard,
/// plus the time since if it is playing.
fn position_now(view: &View, now_ms: u64) -> f64 {
    let duration = view.full.now.as_ref().map_or(0.0, |song| f64::from(song.duration));
    let since = if view.playing { now_ms.saturating_sub(view.heard_ms) as f64 / 1000.0 } else { 0.0 };
    (view.full.position + since).clamp(0.0, duration.max(0.0))
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
            !link.looking && (link.reset || link.looked_ms.is_none_or(|at| now.saturating_sub(at) >= LOOK_AGAIN_MS))
        };
        if due {
            self.link_look(now);
        }
        self.link_peers(now);
        self.link_moves(now);
        self.link_news(now);
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

    /// "Link my devices" was changed in Settings. Where the new linking is switched on,
    /// off takes effect at once, not at the next tick.
    pub fn link_setting(self: &Rc<Self>, link_my_devices: bool) {
        if !on() {
            return;
        }
        let was_steering = self.link.borrow().steering.is_some();
        if self.link.borrow_mut().setting(link_my_devices) {
            log("session: ended, \"Link my devices\" was switched off");
            self.link_back_here(was_steering);
            self.link_show();
        }
    }

    /// Ends everything of this sign-in's: at sign-out, when linking is no longer wanted,
    /// and when the account changes. A look still on its way is answered to nobody.
    pub fn link_stop(self: &Rc<Self>) {
        let was_steering = {
            let mut link = self.link.borrow_mut();
            if link.session.is_some() {
                log("session: ended, with the sign-in it belonged to");
            }
            let was_steering = link.steering.is_some();
            link.stop();
            was_steering
        };
        self.link_back_here(was_steering);
    }

    /// The player and the queue panel are this PC's own again, if they were another device's.
    fn link_back_here(self: &Rc<Self>, was_steering: bool) {
        if was_steering {
            self.show_joined();
            self.track_changed();
        }
    }

    fn link_look(self: &Rc<Self>, now: u64) {
        let Some(client) = self.client() else { return };
        let (used, looks, over, reset, pool) = {
            let mut link = self.link.borrow_mut();
            let (looks, over) = link.look_begins();
            (link.used.clone(), looks, over, std::mem::take(&mut link.reset), link.pool())
        };
        pool.spawn(move || {
            let looked = look(&client, used, over, reset);
            post(move |ctl| ctl.link_looked(looks, now, looked, reset));
        });
    }

    /// "Reset linking" was pressed: a new secret is written into the playlist at the next
    /// look, which starts as soon as the one on its way (if any) is over. Every device
    /// still signed in moves to it at its own next look.
    pub fn link_reset(self: &Rc<Self>) {
        if !on() {
            return;
        }
        {
            let mut link = self.link.borrow_mut();
            link.reset = true;
            link.note = "Resetting linking…".into();
        }
        log("reset: asked for");
        self.link_show();
    }

    /// A look has finished. `since` is when it began; `reset` is whether it was one that
    /// writes a new secret.
    fn link_looked(self: &Rc<Self>, looks: u64, since: u64, looked: Looked, reset: bool) {
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
            let reset_done = reset && matches!(&looked, Looked::Secret { wrote: Some(secret::Why::Reset), .. });
            // A reset that got as far as writing and no further has still ended the old secret.
            let reset_half_done = reset && matches!(&looked, Looked::Invalid);
            let begin = match looked {
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
                        None => Some((secret, reset_done)),
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
            };
            if reset_half_done {
                link.note = "Linking was reset, but the music server stopped answering before that could be confirmed. It will be asked again.".into();
            } else if reset && !reset_done {
                link.note = "Linking couldn't be reset: the music server didn't answer. Try again.".into();
            }
            begin
        };
        if let Some(page) = forget {
            self.invalidate(&page);
        }
        if let Some((secret, reset_done)) = begin {
            self.link_begin(secret, since);
            if reset_done {
                log("reset: a new secret was written and read back");
                // The devices this one remembers are on the old secret until their next look.
                self.link.borrow_mut().note = "Linking was reset. Your other devices rejoin by themselves within two minutes.".into();
            }
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
        link.news.forget();
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
        let steering = self.link.borrow().steering.as_ref().map(|steering| steering.name.clone()).unwrap_or_default();
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
            steering,
            full: None,
            queue: None,
        }
    }

    /// What a device steering this one shows: the song, where it has got to, what is to come.
    fn link_full(&self) -> Full {
        let pb = self.playback.borrow();
        let coming = pb.order.iter().skip(pb.cursor + 1).filter_map(|&index| pb.queue.get(index)).map(entry_of);
        Full::fitted(pb.position, pb.current().map(entry_of), coming, &pb.context, pb.shuffle, pb.repeat, f64::from(pb.volume))
    }

    /// What this device is playing and has to come, for another to carry on with: the
    /// current song first, then the rest in the order they'd play. A radio station is not
    /// something another device can carry on with.
    fn link_handed(&self) -> Handed {
        let pb = self.playback.borrow();
        if pb.current().is_none_or(|song| !song.radio_url.is_empty()) {
            return Handed::default();
        }
        let ids: Vec<String> = pb.order.iter().skip(pb.cursor).filter_map(|&index| pb.queue.get(index)).filter(|song| song.radio_url.is_empty()).map(|song| song.id.clone()).collect();
        Handed::fitted(&ids, pb.queued_ahead(), pb.position, &pb.context)
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

    // ---- what another device asks of this one ----------------------------------------------

    /// Another device asks something of this one.
    fn link_asked(self: &Rc<Self>, serial: u64, from: Place, nonce: Nonce, message: &[u8]) {
        let now = now_ms();
        if !self.link.borrow_mut().asks.allow(now) {
            return log("ask: more than this device answers in a second, dropped");
        }
        let who = text::place(&from);
        match read_ask(message) {
            None => {
                log(format!("ask from {who}: not one this version carries out"));
                self.link_answer(serial, from, self.link_state(Some(nonce), Some(false), Some(Why::Invalid)));
            }
            Some(Ask::Look(want)) => {
                let mut state = self.link_state(Some(nonce), None, None);
                match want {
                    Want::Brief => {}
                    Want::Full => state.full = Some(self.link_full()),
                    Want::Queue => state.queue = Some(self.link_handed()),
                }
                self.link_answer(serial, from, state);
            }
            Some(Ask::Do { command, at }) => self.link_commanded(serial, from, nonce, command, at),
        }
    }

    /// A command from another device, as it arrives: checked against the rule before any
    /// work is done for it, then whatever it needs is found, and then it is carried out.
    fn link_commanded(self: &Rc<Self>, serial: u64, from: Place, nonce: Nonce, command: Command, at: At) {
        let now = now_ms();
        let who = text::place(&from);
        // (The rule is let go of before anything is said of what it decided: saying it reads the rule.)
        let admitted = self.link_rule.borrow_mut().admit(at, minute(now), now);
        let reservation = match admitted {
            Ok(reservation) => reservation,
            Err(why) => return self.link_refused(serial, from, nonce, &command, at, why),
        };
        match self.link_prepare(&command) {
            Prepared::Cannot => {
                self.link_rule.borrow_mut().refuse(reservation);
                self.link_refused(serial, from, nonce, &command, at, Why::Cannot);
            }
            // (For a test only: a Pause is carried out a while after it is accepted, so that
            // something can be made to happen in between.)
            Prepared::Ready(ready) if command == Command::Pause && slow_pause().is_some() => {
                let (ctl, wait) = (self.clone(), slow_pause().unwrap_or_default());
                log(format!("pause from {who}: accepted, and held {} ms before it is carried out (a test)", wait.as_millis()));
                slint::Timer::single_shot(wait, move || ctl.link_finish(serial, from, nonce, command, at, reservation, ready));
            }
            Prepared::Ready(ready) => self.link_finish(serial, from, nonce, command, at, reservation, ready),
            Prepared::Find(ids) => {
                // The songs are looked up away from this thread; the command stays the one
                // that is open, and this PC is busy to other devices, until they are back.
                let (client, pool) = (self.client(), self.link.borrow_mut().pool());
                pool.spawn(move || {
                    let songs: Vec<Song> = ids.iter().filter_map(|id| crate::jam::find_song(id, client.as_ref())).filter(|song| song.radio_url.is_empty()).collect();
                    post(move |ctl| {
                        if songs.is_empty() {
                            ctl.link_rule.borrow_mut().refuse(reservation);
                            return ctl.link_refused(serial, from, nonce, &command, at, Why::Cannot);
                        }
                        ctl.link_finish(serial, from, nonce, command, at, reservation, Ready::Songs(songs));
                    });
                });
            }
        }
    }

    /// What a command needs found before it can be carried out, or that it cannot be.
    /// Nothing is changed here.
    fn link_prepare(&self, command: &Command) -> Prepared {
        let entry = |at: u64, id: &str| self.link_entry(at, id).map_or(Prepared::Cannot, |entry| Prepared::Ready(Ready::Entry(entry)));
        match command {
            Command::Play | Command::Pause | Command::Next | Command::Previous | Command::Seek { .. } | Command::Clear { .. } if self.playback.borrow().current().is_none() => Prepared::Cannot,
            Command::Jump { at, id } | Command::Remove { at, id } => entry(*at, id),
            Command::Move { from, id, .. } => entry(*from, id),
            Command::Add { ids, .. } => {
                let (songs, _) = known_songs(ids, 0, 0);
                if songs.len() == ids.len() {
                    Prepared::Ready(Ready::Songs(songs.into_iter().filter(|song| song.radio_url.is_empty()).collect()))
                } else {
                    Prepared::Find(ids.clone())
                }
            }
            // It checks it can play before anything is changed: a queue none of whose songs
            // this library has is not taken.
            Command::Adopt(adopt) => match known_songs(&adopt.songs, adopt.index, adopt.queued) {
                (songs, _) if songs.is_empty() => Prepared::Cannot,
                (songs, begin) => Prepared::Ready(Ready::Queue(songs, begin)),
            },
            _ => Prepared::Ready(Ready::Nothing),
        }
    }

    /// Which entry after the current song a command means: the one at `at`, if it is still
    /// the song its sender saw there, or else the nearest copy of that song.
    fn link_entry(&self, at: u64, id: &str) -> Option<usize> {
        let at = at as usize;
        let pb = self.playback.borrow();
        let ahead = || pb.order.iter().skip(pb.cursor + 1).filter_map(|&index| pb.queue.get(index));
        if ahead().nth(at).is_some_and(|song| song.id == id) {
            return Some(at);
        }
        ahead().enumerate().filter(|(_, song)| song.id == id).map(|(place, _)| place).min_by_key(|place| place.abs_diff(at))
    }

    /// The moment the player is changed for a command from another device: everything is
    /// checked again, under the session the command arrived in, and then it is carried out
    /// the way the same press on this PC would be.
    fn link_finish(self: &Rc<Self>, serial: u64, from: Place, nonce: Nonce, command: Command, at: At, reservation: Reservation, ready: Ready) {
        let who = text::place(&from);
        let now = now_ms();
        let same_session = self.link.borrow().live(serial, now).is_some();
        let finished = self.link_rule.borrow_mut().finish(reservation, at, minute(now), same_session, nonce);
        if let Err(why) = finished {
            return self.link_refused(serial, from, nonce, &command, at, why);
        }
        {
            // The command took its place in the order when it was finished: what the player
            // does for it is that one change, and is not counted again.
            let _carrying = self.carrying_out();
            self.link_apply(&command, ready);
        }
        log(format!("{} from {who} against generation {}: carried out", command.name(), at.generation));
        if matches!(command, Command::Adopt(_)) {
            // It answers "carried out" when its sound has started, and is busy until then.
            let mut rule = self.link_rule.borrow_mut();
            let waiting = rule.wait(now, START_MS + 5_000);
            let adoption = Adoption { serial, from, nonce, adopting: Adopting::begin(rule.generation(), now), waiting, heard_from: None };
            drop(rule);
            if let Some(earlier) = self.link.borrow_mut().adoption.replace(adoption) {
                self.link_rule.borrow_mut().done(earlier.waiting);
            }
            return;
        }
        self.link_answer(serial, from, self.link_state(Some(nonce), Some(true), None));
    }

    /// Does to this PC's own player what a command asks, as the same press here would.
    fn link_apply(self: &Rc<Self>, command: &Command, ready: Ready) {
        match (command, ready) {
            (Command::Play, _) => self.set_playing_here(true),
            (Command::Pause, _) => self.set_playing_here(false),
            (Command::Next, _) => self.next_track_here(),
            (Command::Previous, _) => self.previous_track_here(),
            (Command::Seek { to }, _) => self.seek_here(*to),
            (Command::Volume { to }, _) => self.set_volume_here(*to as f32),
            (Command::Jump { .. }, Ready::Entry(entry)) => self.jump_to_upcoming_here(entry),
            (Command::Remove { .. }, Ready::Entry(entry)) => self.remove_upcoming_here(entry),
            (Command::Move { to, queued, .. }, Ready::Entry(from)) => {
                let last = {
                    let pb = self.playback.borrow();
                    pb.order.len().saturating_sub(pb.cursor + 2)
                };
                self.move_upcoming_here(from, (*to as usize).min(last), *queued);
            }
            (Command::Clear { queued }, _) => self.clear_upcoming_here(*queued),
            (Command::Shuffle { on }, _) => self.set_shuffle(*on),
            (Command::Repeat { mode }, _) => self.set_repeat(*mode),
            (Command::Add { next, .. }, Ready::Songs(songs)) => self.enqueue_here(songs, *next),
            (Command::Adopt(adopt), Ready::Queue(songs, begin)) => self.load_queue_here(songs, begin, adopt.position, &adopt.context),
            _ => {}
        }
    }

    fn link_refused(&self, serial: u64, from: Place, nonce: Nonce, command: &Command, at: At, why: Why) {
        log(format!(
            "{} from {} against generation {}: refused, {why:?} (this device is at {}{})",
            command.name(),
            text::place(&from),
            at.generation,
            self.link_rule.borrow().generation(),
            if self.link_rule.borrow().busy(now_ms()) { ", busy" } else { "" }
        ));
        self.link_answer(serial, from, self.link_state(Some(nonce), Some(false), Some(why)));
    }

    /// Answers an ask, if the session it arrived under is still the live one: an answer is
    /// not sent under another session, to a place that session has never heard of.
    fn link_answer(&self, serial: u64, to: Place, answer: Tell) {
        if self.link.borrow().live(serial, now_ms()).is_none() {
            return log("answer: the session it belonged to has ended, not sent");
        }
        self.link_send(to, TELL, answer.text());
    }

    /// The moves that wait on this PC's own sound: a queue another device sent, which is
    /// answered when it sounds; and music being brought here, whose other end is paused
    /// when this one sounds. And the device being steered, if it has gone.
    fn link_moves(self: &Rc<Self>, now: u64) {
        // This PC's own sound has started on the queue a move is waiting on once it has
        // moved on from where it was first heard: the song being open is not yet sound.
        let sounding = {
            let at = self.sounding_at();
            let mut link = self.link.borrow_mut();
            let link = &mut *link;
            let heard = match (&mut link.adoption, &mut link.take) {
                (Some(adoption), _) => Some(&mut adoption.heard_from),
                (None, Some(Take::Loaded { heard_from, .. })) => Some(heard_from),
                _ => None,
            };
            match (heard, at) {
                (Some(heard), Some(at)) => at - *heard.get_or_insert(at) >= SOUNDS_AFTER,
                (Some(heard), None) => {
                    *heard = None;
                    false
                }
                (None, _) => false,
            }
        };
        let generation = self.link_rule.borrow().generation();
        // A queue that was sent here.
        let adopted = {
            let mut link = self.link.borrow_mut();
            let live = link.session.as_ref().map(|session| session.serial);
            let step = match &link.adoption {
                // (The session it came under has ended: there is nobody to answer.)
                Some(adoption) if live != Some(adoption.serial) => Some(None),
                Some(adoption) => adoption.adopting.poll(sounding, generation, now).map(Some),
                None => None,
            };
            step.and_then(|adopted| link.adoption.take().map(|adoption| (adoption, adopted)))
        };
        if let Some((adoption, adopted)) = adopted {
            let (ok, why) = match adopted {
                Some(Adopted::Started) => (true, None),
                Some(Adopted::Changed) => (false, Some(Why::Changed)),
                Some(Adopted::Failed) => {
                    // No sound came: it stops trying, and says so.
                    self.changed();
                    self.pause_here();
                    (false, Some(Why::Cannot))
                }
                None => (false, None),
            };
            self.link_rule.borrow_mut().done(adoption.waiting);
            if adopted.is_some() {
                log(format!("adopt from {}: {}", text::place(&adoption.from), if ok { "its sound has started" } else { "it did not start" }));
                self.link_answer(adoption.serial, adoption.from, self.link_state(Some(adoption.nonce), Some(ok), why));
            }
        }
        // Music being brought here.
        let ended = {
            let mut link = self.link.borrow_mut();
            let step = match &link.take {
                Some(Take::Loaded { taking, .. }) => taking.poll(sounding, generation, now).map(Some),
                // Its queue never came.
                Some(Take::Asked { pressed_ms, .. }) if now.saturating_sub(*pressed_ms) >= WAIT_MS => Some(None),
                _ => None,
            };
            step.and_then(|took| link.take.take().map(|take| (take, took)))
        };
        if let Some((Take::Asked { name, .. }, _)) = &ended {
            self.link_let_go();
            self.link_say(format!("{name} didn't answer, so nothing was moved. It may still be playing."));
        }
        if let Some((Take::Loaded { place, name, .. }, Some(took))) = ended {
            match took {
                Took::Pause(at) => {
                    // The other stops now that this one sounds: against the state its queue
                    // was taken from, so that if it has been given anything else to do
                    // since, it is left to it.
                    if let Some(nonce) = self.link_send(place, ASK, Ask::Do { command: Command::Pause, at }.text()) {
                        log(format!("take: this PC sounds; asked {name} to pause, against generation {}", at.generation));
                        if let Some(seen) = self.link.borrow_mut().session.as_mut().and_then(|session| session.peers.get_mut(&place)) {
                            seen.peer.sent(nonce, at, now);
                            seen.out = Some(Out::TakePause);
                        }
                    }
                }
                Took::ThisChanged => self.link_say(format!("Something else was chosen here, so {name} was left as it is: it is still playing.")),
                Took::NotStarted => self.link_say(format!("This PC didn't start in time, so {name} was left playing.")),
            }
        }
        // The device being steered.
        let lost = {
            let mut link = self.link.borrow_mut();
            let here = link.steered_place().is_some();
            match link.steering.as_mut() {
                Some(steering) if here => {
                    steering.lost_ms = None;
                    None
                }
                Some(steering) => {
                    let since = *steering.lost_ms.get_or_insert(now);
                    (now.saturating_sub(since) >= LOST_MS).then(|| steering.name.clone())
                }
                None => None,
            }
        };
        if let Some(name) = lost {
            self.link_let_go();
            self.link_say(format!("{name} can't be reached any more, so the player is back on this PC."));
        }
        // The progress bar moves by itself between one look and the next.
        let position = self.link.borrow().steering.as_ref().and_then(|steering| steering.view.as_ref()).map(|view| position_now(view, now));
        if let Some(position) = position {
            self.ui.global::<ui::Player>().set_position(position as f32);
        }
    }

    /// Tells the other devices, unasked, that what this one is doing has changed: so that
    /// they look now, not at their next turn. Bounded by the library (`link::news`).
    fn link_news(&self, now: u64) {
        use std::hash::{Hash, Hasher};
        let others = {
            let link = self.link.borrow();
            match link.session.as_ref().filter(|session| session.place.is_some()) {
                Some(session) => session.others.clone(),
                None => return,
            }
        };
        let state = self.link_state(None, None, None);
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (state.at.generation, state.busy, state.playing, &state.title, &state.artist, &state.name, &state.steering).hash(&mut hasher);
        if !self.link.borrow_mut().news.due(hasher.finish(), others.len(), now) {
            return;
        }
        let text = state.text();
        for place in others {
            self.link_send(place, TELL, text.clone());
        }
    }

    // ---- what this device asks of another --------------------------------------------------

    /// Another device says something of itself.
    fn link_told(self: &Rc<Self>, serial: u64, from: Place, message: &[u8]) {
        let Some(mut tell) = read_tell(message) else {
            return log(format!("tell from {}: breaks its shape, dropped", text::place(&from)));
        };
        let now = now_ms();
        if tell.news() {
            // News is a reason to look, and nothing more.
            let look = {
                let mut link = self.link.borrow_mut();
                let Some(session) = link.live_mut(serial, now) else { return };
                session.peers.get_mut(&from).is_some_and(|seen| seen.peer.news(now))
            };
            if look {
                self.link_look_at(from, false);
            }
            return;
        }
        // (The queue, if it carries one, is for the move that asked for it and is not kept.)
        let (handed, re, at, playing) = (tell.queue.take(), tell.re, tell.at, tell.playing);
        let (told, name, out) = {
            let mut link = self.link.borrow_mut();
            let Some(session) = link.live_mut(serial, now) else { return };
            let Some(seen) = session.peers.get_mut(&from) else { return };
            let told = seen.peer.told(tell);
            // (What it was for is kept until it is known to have been carried out: a refusal,
            // or "not known", can still be put right by a success that arrives later.)
            let out = seen.out;
            if matches!(told, Told::Settled(Outcome::Done) | Told::ShownAndSettled(Outcome::Done)) {
                seen.out = None;
            }
            (told, name_of(seen), out)
        };
        if matches!(told, Told::Shown | Told::ShownAndSettled(_)) {
            self.link_view(from);
            // The queue that was asked for, to bring its music here.
            let asked = matches!(&self.link.borrow().take, Some(Take::Asked { place, nonce, .. }) if *place == from && Some(*nonce) == re);
            if asked {
                self.link_take(from, handed, at, playing);
            }
        }
        match told {
            Told::Dropped => log(format!("tell from {name}: answers nothing waited on, dropped")),
            Told::Shown => {}
            Told::Settled(outcome) | Told::ShownAndSettled(outcome) => {
                self.link_outcome(from, &name, out, outcome);
                // What was pressed while it was unanswered goes next, if it was carried out.
                if outcome == Outcome::Done {
                    self.link_next(from);
                }
                // The answer to a command does not change what is shown: look again.
                self.link_look_at(from, false);
            }
        }
    }

    /// Says what became of something asked of another device.
    fn link_outcome(self: &Rc<Self>, place: Place, name: &str, out: Option<Out>, outcome: Outcome) {
        log(format!("outcome of {:?} to {name}: {outcome:?}", out.unwrap_or(Out::Steer)));
        let note = match (out.unwrap_or(Out::Steer), outcome) {
            (Out::Pause, Outcome::Done) => format!("Paused {name}."),
            (Out::Steer | Out::PlayThere(0), Outcome::Done) => String::new(),
            (Out::PlayThere(left), Outcome::Done) => format!("{name} was sent as many songs as fit; {left} were left out."),
            (Out::Add(1), Outcome::Done) => format!("Added to the queue on {name}"),
            (Out::Add(count), Outcome::Done) => format!("Added {count} songs to the queue on {name}"),
            (Out::Give(giving, sent, left), outcome) => {
                let gave = giving.ends(outcome, self.link_rule.borrow().generation(), now_ms());
                if gave == Gave::Moved {
                    // The other has started: this one stops, and the player is the other's.
                    self.set_playing_here(false);
                    self.link_steer(place);
                }
                gave_words(gave, name, left, sent)
            }
            (Out::TakePause, Outcome::Done) => format!("Playing here. {name} is paused."),
            (Out::TakePause, Outcome::Refused(_)) => format!("Playing here. {name} was given something else to do before it could be paused, so it may still be playing."),
            (Out::TakePause, _) => format!("Playing here. {name} didn't answer, so it may still be playing."),
            (_, outcome) => outcome_words(outcome, name),
        };
        if note.is_empty() {
            return;
        }
        log(format!("said: {note}"));
        self.link.borrow_mut().note = note.clone();
        // Under the list for a Pause pressed there; anything else is said where the person is looking.
        if out != Some(Out::Pause) {
            self.toast(note);
        }
    }

    /// Tells the person something about their devices: where they are looking, and under
    /// the list.
    fn link_say(&self, note: String) {
        log(format!("said: {note}"));
        self.link.borrow_mut().note = note.clone();
        self.toast(note);
    }

    /// Sends a look to a place. `checking` says it is the one look that asks what became of
    /// a command that has gone unanswered.
    fn link_look_at(&self, to: Place, checking: bool) {
        // The device being steered is asked for everything the player shows.
        let want = if self.link.borrow().steered_place() == Some(to) { Want::Full } else { Want::Brief };
        let Some(nonce) = self.link_send(to, ASK, Ask::Look(want).text()) else { return };
        let now = now_ms();
        let mut link = self.link.borrow_mut();
        if let Some(seen) = link.session.as_mut().and_then(|session| session.peers.get_mut(&to)) {
            seen.looked_ms = Some(now);
            if checking { seen.peer.checking(nonce, now) } else { seen.peer.looked(nonce, now) }
        }
    }

    /// Looks at each of the other devices from time to time (the one being steered every
    /// few seconds), and gives up waiting for the answer to a command when it has waited
    /// long enough.
    fn link_peers(self: &Rc<Self>, now: u64) {
        let mut to_look = Vec::new();
        let mut settled = Vec::new();
        {
            let mut link = self.link.borrow_mut();
            let steered = link.steered_place();
            let Some(session) = link.session.as_mut().filter(|session| session.place.is_some()) else { return };
            for place in session.others.clone() {
                let seen = session.peers.entry(place).or_default();
                let every = if steered == Some(place) { LOOK_EVERY_MS } else { LOOK_IDLE_MS };
                match seen.peer.tick(now) {
                    Tick::Check => to_look.push((place, true)),
                    Tick::Settled(outcome) => settled.push((place, name_of(seen), seen.out, outcome)),
                    Tick::Nothing if seen.looked_ms.is_none_or(|at| now.saturating_sub(at) >= every) => to_look.push((place, false)),
                    Tick::Nothing => {}
                }
            }
        }
        for (place, name, out, outcome) in settled {
            self.link_outcome(place, &name, out, outcome);
        }
        for (place, checking) in to_look {
            self.link_look_at(place, checking);
        }
    }

    /// Asks a device to do something: now, if nothing asked of it is unanswered and there
    /// is a fresh state to make it against; after the command that is out, if one is; or
    /// not at all.
    fn link_do(&self, to: Place, command: Command, out: Out) -> Press {
        let now = now_ms();
        let press = {
            let mut link = self.link.borrow_mut();
            let Some(seen) = link.session.as_mut().and_then(|session| session.peers.get_mut(&to)) else { return Press::No(Wait::Look) };
            seen.peer.press(&command, now)
        };
        let Press::Send(at) = press else { return press };
        let name = command.name();
        let Some(nonce) = self.link_send(to, ASK, Ask::Do { command, at }.text()) else { return Press::No(Wait::Look) };
        let mut link = self.link.borrow_mut();
        if let Some(seen) = link.session.as_mut().and_then(|session| session.peers.get_mut(&to)) {
            log(format!("{name}: asked {}, against generation {} in minute {}", name_of(seen), at.generation, at.minute));
            seen.peer.sent(nonce, at, now);
            seen.out = Some(out);
        }
        press
    }

    /// Sends the next press that was waiting for a device, now that the one before it has
    /// been carried out: against the state in that answer.
    fn link_next(&self, to: Place) {
        let now = now_ms();
        let next = self.link.borrow_mut().session.as_mut().and_then(|session| session.peers.get_mut(&to)).and_then(|seen| seen.peer.next(now));
        let Some((command, at)) = next else { return };
        let out = match &command {
            Command::Add { ids, .. } => Out::Add(ids.len()),
            _ => Out::Steer,
        };
        let name = command.name();
        let Some(nonce) = self.link_send(to, ASK, Ask::Do { command, at }.text()) else { return };
        let mut link = self.link.borrow_mut();
        if let Some(seen) = link.session.as_mut().and_then(|session| session.peers.get_mut(&to)) {
            log(format!("{name}: asked {} next, against generation {} in minute {}", name_of(seen), at.generation, at.minute));
            seen.peer.sent(nonce, at, now);
            seen.out = Some(out);
        }
    }

    /// Says why something could not be asked of a device just now, and looks at it.
    fn link_not_sent(&self, to: Place, name: &str, wait: Wait, aloud: bool) {
        let note = match wait {
            Wait::Look => format!("Looking at {name} first. Press again in a moment."),
            Wait::Busy => format!("{name} is in the middle of something. Press again in a moment."),
            Wait::Unanswered => format!("Still waiting for {name} to answer."),
        };
        if wait != Wait::Unanswered && self.link_name(&to).is_some() {
            self.link_look_at(to, false);
        }
        self.link.borrow_mut().note = note.clone();
        if aloud {
            self.toast(note);
        }
        self.link_show();
    }

    /// Whether something asked of a device has not been answered or given up on.
    fn link_unanswered(&self, place: &Place) -> bool {
        self.link.borrow().session.as_ref().and_then(|session| session.peers.get(place)).is_some_and(|seen| seen.peer.waiting())
    }

    fn link_name(&self, place: &Place) -> Option<String> {
        self.link.borrow().session.as_ref().and_then(|session| session.peers.get(place)).map(name_of)
    }

    /// Pause was pressed for another device, in the list.
    fn link_pause(self: &Rc<Self>, place: &str) {
        let Some(to) = text::read_place(place) else { return };
        let Some(name) = self.link_name(&to) else { return };
        if self.link_unanswered(&to) {
            return self.link_not_sent(to, &name, Wait::Unanswered, false);
        }
        match self.link_do(to, Command::Pause, Out::Pause) {
            Press::Send(_) | Press::Waits => self.link.borrow_mut().note = format!("Asked {name} to pause…"),
            Press::No(wait) => self.link_not_sent(to, &name, wait, false),
        }
    }

    // ---- steering another device from here ---------------------------------------------------

    /// The name of the device the player on this PC is steering, if it is steering one.
    pub fn link_steered(&self) -> Option<String> {
        self.link.try_borrow().ok()?.steering.as_ref().map(|steering| steering.name.clone())
    }

    /// From here the player and the queue panel are that device's.
    fn link_steer(self: &Rc<Self>, place: Place) {
        let begun = {
            let mut link = self.link.borrow_mut();
            let found = link.session.as_ref().and_then(|session| session.peers.get(&place)).and_then(|seen| seen.peer.shown()).map(|shown| (shown.tell.device, shown.tell.name.clone()));
            match found {
                Some((device, name)) => {
                    log(format!("steering {name}"));
                    link.steering = Some(Steering { device, name, view: None, known: HashMap::new(), lost_ms: None });
                    true
                }
                None => false,
            }
        };
        if begun {
            self.show_joined();
            self.track_changed();
            self.link_look_at(place, false);
        }
    }

    /// Stops steering, leaving the other device as it is. The player is this PC's own
    /// again. Says whether there was anything to stop.
    pub fn link_let_go(self: &Rc<Self>) -> bool {
        if !on() {
            return false;
        }
        let was = {
            let mut link = self.link.borrow_mut();
            if matches!(link.take, Some(Take::Asked { .. })) {
                link.take = None;
            }
            link.steering.take().is_some()
        };
        if was {
            log("steering: stopped");
            self.link_back_here(true);
        }
        was
    }

    /// A fresh state arrived from a place: if that is the device being steered, the
    /// player shows it.
    fn link_view(self: &Rc<Self>, from: Place) {
        let now = now_ms();
        let shown = {
            let mut link = self.link.borrow_mut();
            if link.steered_place() != Some(from) {
                return;
            }
            let fresh = link.session.as_ref().and_then(|session| session.peers.get(&from)).and_then(|seen| seen.peer.shown()).and_then(|shown| shown.tell.full.clone().map(|full| (full, shown.tell.playing, shown.tell.name.clone())));
            match (fresh, link.steering.as_mut()) {
                (Some((full, playing, name)), Some(steering)) => {
                    steering.view = Some(View { full, playing, heard_ms: now });
                    steering.name = name;
                    true
                }
                _ => false,
            }
        };
        if shown {
            self.show_joined();
            self.link_show_remote();
        }
    }

    /// While another device is being steered, shows it in the player and the queue panel in
    /// place of this PC's own queue, and returns true. Otherwise does nothing.
    pub fn link_show_remote(&self) -> bool {
        let now_ms = now_ms();
        let shown = {
            let Ok(mut link) = self.link.try_borrow_mut() else { return false };
            let Some(steering) = link.steering.as_mut() else { return false };
            let Steering { view, known, .. } = steering;
            view.as_ref().map(|view| {
                let mut row = |entry: &Entry| {
                    let song = known.entry(entry.id.clone()).or_insert_with(|| crate::search::current().and_then(|index| index.library.songs.iter().find(|song| song.id == entry.id).cloned())).clone();
                    let mut row = match &song {
                        Some(song) => self.track_row(song),
                        None => ui::Track { id: entry.id.as_str().into(), title: entry.title.as_str().into(), artist: entry.artist.as_str().into(), album: entry.album.as_str().into(), duration: clock(entry.duration).into(), progress: -1.0, ..Default::default() },
                    };
                    // Whether it can play offline is this PC's business, not the other's.
                    row.unavailable = false;
                    row
                };
                let now = view.full.now.as_ref().map(&mut row);
                let rows: Vec<ui::Track> = view.full.queue.iter().map(&mut row).collect();
                let ahead = view.full.queue.iter().take_while(|entry| entry.queued).count();
                (now, rows, ahead, view.playing, position_now(view, now_ms), view.full.now.as_ref().map_or(0.0, |song| f64::from(song.duration)), view.full.context.clone(), view.full.shuffle, view.full.repeat, view.full.volume)
            })
        };
        let player = self.ui.global::<ui::Player>();
        let (now, rows, ahead, playing, position, duration, context, shuffle, repeat, volume) = shown.unwrap_or_else(|| (None, Vec::new(), 0, false, 0.0, 0.0, String::new(), false, 0, f64::from(player.get_volume())));
        player.set_has_track(now.is_some());
        let now = now.unwrap_or_default();
        player.set_track_id(now.id.clone());
        player.set_track(now);
        player.set_duration(duration as f32);
        player.set_position(position as f32);
        player.set_context(context.into());
        player.set_playing(playing);
        player.set_shuffle(shuffle);
        player.set_repeat(i32::from(repeat));
        player.set_volume(volume as f32);
        player.set_muted(false);
        player.set_buffering(false);
        player.set_reconnecting(false);
        player.set_stalled(false);
        player.set_format("".into());
        self.show_queue(rows, ahead);
        true
    }

    /// While another device is being steered: the song a row of its queue stands for (-1 for
    /// the one playing), as this PC's library knows it. `None` when none is being steered.
    pub fn link_remote_song(&self, index: i32) -> Option<Option<Song>> {
        let link = self.link.try_borrow().ok()?;
        let steering = link.steering.as_ref()?;
        let entry = steering.view.as_ref().and_then(|view| if index < 0 { view.full.now.as_ref() } else { view.full.queue.get(index as usize) });
        Some(entry.and_then(|entry| steering.known.get(&entry.id).cloned().flatten()))
    }

    /// The device being steered: its place and name, and the player's view of it.
    fn link_steering<T>(&self, read: impl FnOnce(Place, &mut Steering) -> T) -> Option<T> {
        let mut link = self.link.try_borrow_mut().ok()?;
        let place = link.steered_place();
        let steering = link.steering.as_mut()?;
        // (Steering a device that has no place in the room just now: what is pressed has
        // nowhere to go, and is still not this PC's own.)
        Some(read(place.unwrap_or_default(), steering))
    }

    /// While another device is being steered, asks it to do `what` and returns true.
    /// Otherwise returns false, and the caller does it to this PC's own playback (or asks
    /// the jam it is in).
    pub fn link_remote(self: &Rc<Self>, what: &Remote) -> bool {
        let now = now_ms();
        let Some((place, name, command)) = self.link_steering(|place, steering| {
            let command = steering.view.as_ref().and_then(|view| {
                let entry = |at: usize| view.full.queue.get(at).map(|entry| entry.id.clone());
                Some(match what {
                    Remote::Toggle if view.playing => Command::Pause,
                    Remote::Toggle => Command::Play,
                    // (Asked for what it is already doing: nothing to send.)
                    Remote::Play(on) if *on == view.playing => return None,
                    Remote::Play(true) => Command::Play,
                    Remote::Play(false) => Command::Pause,
                    Remote::Next => Command::Next,
                    Remote::Previous => Command::Previous,
                    Remote::Seek(to) => Command::Seek { to: to.max(0.0) },
                    Remote::Remove(at) => Command::Remove { at: *at as u64, id: entry(*at)? },
                    Remote::Move(from, to, queued) => Command::Move { from: *from as u64, to: *to as u64, queued: *queued, id: entry(*from)? },
                    Remote::Clear(queued) => Command::Clear { queued: *queued },
                    Remote::Jump(at) => Command::Jump { at: *at as u64, id: entry(*at)? },
                    Remote::Shuffle => Command::Shuffle { on: !view.full.shuffle },
                    Remote::Repeat => Command::Repeat { mode: (view.full.repeat + 1) % 3 },
                    Remote::Volume(to) => Command::Volume { to: f64::from(to.clamp(0.0, 1.0)) },
                })
            });
            (place, steering.name.clone(), command)
        }) else {
            return false;
        };
        let Some(command) = command else { return true };
        match self.link_do(place, command.clone(), Out::Steer) {
            Press::Send(_) | Press::Waits => {
                // The player shows what was asked for straight away; the other device's
                // word on it follows a moment later.
                self.link_steering(|_, steering| {
                    if let Some(view) = steering.view.as_mut() {
                        let position = position_now(view, now);
                        shown_at_once(view, &command, now, position);
                    }
                });
                self.link_show_remote();
            }
            Press::No(wait) => self.link_not_sent(place, &name, wait, true),
        }
        true
    }

    /// While another device is being steered, queues songs there and returns true.
    pub fn link_remote_add(self: &Rc<Self>, songs: &[Song], play_next: bool) -> bool {
        let Some((place, name)) = self.link_steering(|place, steering| (place, steering.name.clone())) else { return false };
        let ids: Vec<String> = songs.iter().filter(|song| song.radio_url.is_empty() && id_ok(&song.id)).map(|song| song.id.clone()).take(MOST_ADDED).collect();
        if ids.is_empty() {
            self.toast("Radio stations can't be added from here");
            return true;
        }
        let count = ids.len();
        match self.link_do(place, Command::Add { ids, next: play_next }, Out::Add(count)) {
            Press::Send(_) | Press::Waits => {}
            Press::No(wait) => self.link_not_sent(place, &name, wait, true),
        }
        true
    }

    /// While another device is being steered, playing something here plays it there, as it
    /// would here, taking the queue's place; and this returns true.
    pub fn link_remote_play(self: &Rc<Self>, songs: &[Song], start: usize, context: &str) -> bool {
        let Some((place, name)) = self.link_steering(|place, steering| (place, steering.name.clone())) else { return false };
        let ids: Vec<String> = songs.iter().map(|song| if song.radio_url.is_empty() { song.id.clone() } else { String::new() }).collect();
        let Some((adopt, left)) = Adopt::fitted(&ids, start, 0, 0.0, context) else {
            self.toast(format!("That can't be played on {name} from here"));
            return true;
        };
        match self.link_do(place, Command::Adopt(adopt), Out::PlayThere(left)) {
            Press::Send(_) => {}
            // (A whole queue does not wait behind another press: it is asked for again.)
            Press::Waits => {}
            Press::No(wait) => self.link_not_sent(place, &name, wait, true),
        }
        true
    }

    /// A device was picked in the list. Another device: this PC starts steering it, and if
    /// music is playing here it carries on over there, this PC stopping once that one has
    /// started. This PC: what the other was playing carries on here.
    fn link_choose(self: &Rc<Self>, place: &str) {
        if place.is_empty() {
            self.link_play_here();
            return;
        }
        let Some(to) = text::read_place(place) else { return };
        let Some(name) = self.link_name(&to) else { return };
        if self.link.borrow().steered_place() == Some(to) {
            return;
        }
        if self.in_a_jam() {
            return self.toast("Leave the jam before moving to another device");
        }
        // (A device being steered the old way is let go of.)
        self.leave_jam();
        // Music playing here moves over there; otherwise that device is steered as it is.
        let handed = {
            let steering = self.link.borrow().steering.is_some();
            let playing = !steering && self.playback.borrow().playing;
            playing.then(|| self.link_handed()).filter(|handed| !handed.songs.is_empty())
        };
        let Some(handed) = handed else {
            self.link_let_go();
            return self.link_steer(to);
        };
        let Some((adopt, _)) = Adopt::fitted(&handed.songs, 0, handed.queued, handed.position, &handed.context) else { return };
        let giving = Giving::begin(self.link_rule.borrow().generation(), now_ms());
        if self.link_unanswered(&to) {
            return self.link_not_sent(to, &name, Wait::Unanswered, true);
        }
        let sent = adopt.songs.len();
        match self.link_do(to, Command::Adopt(adopt), Out::Give(giving, sent, handed.more as usize)) {
            Press::Send(_) => {
                self.link.borrow_mut().note = format!("Moving what's playing to {name}…");
                self.toast(format!("Moving what's playing to {name}…"));
            }
            Press::Waits => {}
            Press::No(wait) => self.link_not_sent(to, &name, wait, true),
        }
    }

    /// Brings what the device being steered is playing over to this PC, and stops steering
    /// it. Says whether a device was being steered.
    pub fn link_play_here(self: &Rc<Self>) -> bool {
        let Some((place, name)) = self.link_steering(|place, steering| (place, steering.name.clone())) else { return false };
        // Its queue is asked for; what is done with it is in `link_take`.
        match self.link_send(place, ASK, Ask::Look(Want::Queue).text()) {
            Some(nonce) => {
                let now = now_ms();
                let mut link = self.link.borrow_mut();
                if let Some(seen) = link.session.as_mut().and_then(|session| session.peers.get_mut(&place)) {
                    seen.peer.looked(nonce, now);
                }
                link.take = Some(Take::Asked { place, nonce, name: name.clone(), pressed_ms: now });
                log(format!("take: asked {name} for its queue"));
            }
            None => {
                self.link_let_go();
                self.link_say(format!("{name} can't be reached, so nothing was moved."));
            }
        }
        true
    }

    /// The queue that was asked for has arrived: it is taken and started here. The other
    /// device is paused once this one sounds (`link_moves`).
    fn link_take(self: &Rc<Self>, from: Place, handed: Option<Handed>, at: At, playing: bool) {
        let Some(Take::Asked { name, pressed_ms, .. }) = self.link.borrow_mut().take.take() else { return };
        // The player is this PC's own from here, whatever comes of it.
        self.link_let_go();
        let handed = handed.filter(|handed| playing && !handed.songs.is_empty());
        let Some(handed) = handed else {
            // Nothing was playing there to bring over: it is just let go of.
            return log(format!("take: {name} was not playing anything"));
        };
        let (songs, begin) = known_songs(&handed.songs, 0, handed.queued);
        if songs.is_empty() {
            return self.link_say(format!("This PC's library has none of those songs. {name} is still playing."));
        }
        self.load_queue_here(songs, begin, handed.position, &handed.context);
        let taking = Taking::begin(at, self.link_rule.borrow().generation(), pressed_ms);
        self.link.borrow_mut().take = Some(Take::Loaded { place: from, name: name.clone(), taking, heard_from: None });
        log(format!("take: loaded {name}'s queue here; it is paused when this PC sounds"));
    }

    /// Shows the other devices and what each is doing.
    fn link_show(&self) {
        let now = now_ms();
        let link = self.link.borrow();
        let steered = link.steering.as_ref().map(|steering| steering.device);
        let mut rows = Vec::new();
        if let Some(session) = &link.session {
            for place in &session.others {
                let Some(shown) = session.peers.get(place).and_then(|seen| seen.peer.shown()) else { continue };
                let tell = &shown.tell;
                let row = ui::LinkedDevice { place: text::place(place).into(), name: tell.name.as_str().into(), about: about(tell, now.saturating_sub(shown.sent_ms) / 1000).into(), playing: tell.playing, active: steered == Some(tell.device), ready: true };
                rows.push((tell.device, shown.sent_ms, row));
            }
        }
        let list = latest_of_each(rows);
        let global = self.ui.global::<ui::Linked>();
        global.set_on(true);
        global.set_entered(link.session.as_ref().is_some_and(|session| session.place.is_some()));
        global.set_note(link.note.as_str().into());
        global.set_steering(link.steering.as_ref().map(|steering| steering.name.as_str()).unwrap_or_default().into());
        // (Only when it has changed: the list is redrawn a few times a second otherwise.)
        let before: Vec<ui::LinkedDevice> = slint::Model::iter(&global.get_list()).collect();
        if before != list {
            log(format!("shown: {}", if list.is_empty() { "no other devices".to_string() } else { list.iter().map(|device| format!("{}: {}", device.name, device.about)).collect::<Vec<_>>().join("; ") }));
            global.set_list(ModelRc::new(VecModel::from(list)));
        }
    }

    pub fn wire_link(self: &Rc<Self>) {
        let linked = self.ui.global::<ui::Linked>();
        linked.on_pause(|place| {
            with(|ctl| ctl.link_pause(place.as_str()));
        });
        linked.on_choose(|place| {
            with(|ctl| ctl.link_choose(place.as_str()));
        });
        linked.on_reset(|| {
            with(|ctl| ctl.link_reset());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn look_with_no_reset(server: &mut impl Server, username: &str, used: Option<String>) -> Looked {
        look_with(server, username, used, false)
    }

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

    fn state(playing: bool) -> Tell {
        Tell { re: None, ok: None, why: None, device: [7; 16], name: "Study PC".into(), at: At { generation: 4, minute: 0 }, busy: false, by: None, playing, title: "Harbour Lights".into(), artist: "The Tide".into(), steering: String::new(), full: None, queue: None }
    }

    #[test]
    fn what_a_device_is_doing_is_said_in_a_few_words() {
        assert_eq!(about(&state(true), 3), "Playing Harbour Lights");
        assert_eq!(about(&state(false), 3), "Paused on Harbour Lights");
        assert_eq!(about(&Tell { title: String::new(), ..state(false) }, 3), "Nothing playing");
        assert_eq!(about(&Tell { busy: true, ..state(true) }, 3), "In the middle of something…");
        // One that is steering another says so, whatever its own player holds.
        assert_eq!(about(&Tell { steering: "Laptop".into(), busy: true, ..state(false) }, 3), "Steering Laptop");
        // What is shown says how old it is once it is older than a look and its answer.
        assert_eq!(about(&state(true), 49), "Playing Harbour Lights");
        assert_eq!(about(&state(true), 50), "Playing Harbour Lights (as of 50 seconds ago)");
    }

    fn view() -> View {
        let entry = |n: usize| Entry { id: format!("s{n}"), title: format!("Song {n}"), duration: 200, queued: n == 1, ..Default::default() };
        View { full: Full::fitted(40.0, Some(entry(0)), (1..5).map(entry), "Morning mix", false, 0, 0.5), playing: true, heard_ms: 10_000 }
    }

    #[test]
    fn the_player_shows_what_was_asked_of_the_other_device_straight_away() {
        // The progress bar moves on by itself while the other device plays, and not past the end.
        let mut shown = view();
        assert_eq!((position_now(&shown, 10_000), position_now(&shown, 15_000), position_now(&shown, 999_000)), (40.0, 45.0, 200.0));
        // Pause holds it where it had got to.
        shown_at_once(&mut shown, &Command::Pause, 15_000, 45.0);
        assert_eq!((shown.playing, position_now(&shown, 60_000)), (false, 45.0));
        shown_at_once(&mut shown, &Command::Seek { to: 120.0 }, 61_000, 45.0);
        assert_eq!((shown.playing, position_now(&shown, 70_000)), (false, 120.0));
        shown_at_once(&mut shown, &Command::Play, 71_000, 120.0);
        assert_eq!(position_now(&shown, 73_000), 122.0);
        shown_at_once(&mut shown, &Command::Volume { to: 0.2 }, 0, 0.0);
        shown_at_once(&mut shown, &Command::Shuffle { on: true }, 0, 0.0);
        shown_at_once(&mut shown, &Command::Repeat { mode: 2 }, 0, 0.0);
        assert_eq!((shown.full.volume, shown.full.shuffle, shown.full.repeat), (0.2, true, 2));
        // The queue's own edits: an entry taken out, and one moved into "Next in queue".
        shown_at_once(&mut shown, &Command::Remove { at: 1, id: "s2".into() }, 0, 0.0);
        assert_eq!(shown.full.queue.iter().map(|entry| entry.id.as_str()).collect::<Vec<_>>(), ["s1", "s3", "s4"]);
        shown_at_once(&mut shown, &Command::Move { from: 2, to: 0, queued: true, id: "s4".into() }, 0, 0.0);
        assert_eq!(shown.full.queue.iter().map(|entry| (entry.id.as_str(), entry.queued)).collect::<Vec<_>>(), [("s4", true), ("s1", true), ("s3", false)]);
        // One that names a place that isn't there changes nothing.
        shown_at_once(&mut shown, &Command::Remove { at: 9, id: "s9".into() }, 0, 0.0);
        assert_eq!(shown.full.queue.len(), 3);
    }

    #[test]
    fn the_person_is_told_which_device_is_playing_however_a_move_ends() {
        // Every end of a move says where the music is, by name.
        for gave in [Gave::Moved, Gave::StartedAndThisChanged, Gave::StartedLate, Gave::Stayed(Why::Busy), Gave::Stayed(Why::Changed), Gave::Stayed(Why::Late), Gave::Stayed(Why::Cannot), Gave::Stayed(Why::Invalid), Gave::NotReached, Gave::Unknown] {
            let words = gave_words(gave, "Study PC", 0, 12);
            assert!(words.contains("Study PC"), "{words}");
            let here = words.contains("here") || words.contains("This PC") || words.contains("Both");
            assert_eq!(here, gave != Gave::Moved, "{gave:?}: {words}");
        }
        assert_eq!(gave_words(Gave::Moved, "Study PC", 2_000, 3_000), "Playing on Study PC. The next 3000 of 5000 songs were moved.");
        // And anything else asked of a device that was not done says so, and why.
        for outcome in [Outcome::Refused(Why::Changed), Outcome::Refused(Why::Busy), Outcome::Refused(Why::Late), Outcome::Refused(Why::Cannot), Outcome::NotReached, Outcome::Unknown] {
            assert!(outcome_words(outcome, "Study PC").contains("Study PC"));
        }
        assert_eq!(outcome_words(Outcome::Done, "Study PC"), "");
    }

    /// A device at two places for a while (it was started again): what is asked of it goes
    /// to the place that answered last.
    #[test]
    fn a_device_is_found_at_the_place_that_last_answered_as_it() {
        let mut link = linked(0);
        let session = link.session.as_mut().unwrap();
        let (old, new, other) = ([2; 16], [3; 16], [4; 16]);
        session.others = vec![old, new, other];
        for (place, device, look, sent) in [(old, [7; 16], [1; 24], 100), (new, [7; 16], [2; 24], 900), (other, [8; 16], [3; 24], 950)] {
            let seen = session.peers.entry(place).or_default();
            seen.peer.looked(look, sent);
            seen.peer.told(Tell { re: Some(look), device, ..state(true) });
        }
        assert_eq!((session.place_of(&[7; 16]), session.place_of(&[8; 16]), session.place_of(&[9; 16])), (Some(new), Some(other), None));
        // The relay gives the old place up: it is the new one still.
        session.others.retain(|place| *place != old);
        assert_eq!(session.place_of(&[7; 16]), Some(new));
        link.steering = Some(Steering { device: [7; 16], name: "Study PC".into(), view: None, known: HashMap::new(), lost_ms: None });
        assert_eq!(link.steered_place(), Some(new));
        // Signed out, or linking switched off: nothing is being steered any more.
        link.stop();
        assert!(link.steering.is_none() && link.take.is_none());
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
            let looked = look_with_no_reset(&mut server, "tester", Some("k".into()));
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
        assert!(ends_session(&look_with_no_reset(&mut server, "tester", Some("gone".into()))) || server.asked.contains(&"make"));
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
        assert!(!ends_session(&look_with_no_reset(&mut server, "tester", Some("k".into()))));
        // A record found no good that cannot be written over is no good either.
        let mut server = Scripted::with(vec![playlist(RECORD, true)]);
        server.write_works = false;
        assert!(ends_session(&look_with_no_reset(&mut server, "tester", Some("k".into()))));
    }

    /// "Reset linking": a look that writes a new secret where it would have used the one it
    /// read, and uses only what it then reads back.
    #[test]
    fn a_look_that_resets_writes_a_new_secret_and_reads_it_back() {
        let fresh = Secret::new([9; 32]);
        let written = fresh.comment();
        let mut server = Scripted::with(vec![playlist(RECORD, false), playlist(&written, false)]);
        let looked = look_with(&mut server, "tester", Some("k".into()), true);
        assert_eq!(server.asked, ["list", "fetch", "write", "list", "fetch"]);
        assert!(matches!(&looked, Looked::Secret { secret, wrote: Some(secret::Why::Reset), .. } if *secret == fresh), "the reset did not come to the new secret");
        // The same look without the reset reads the record and writes nothing.
        let mut server = Scripted::with(vec![playlist(RECORD, false)]);
        assert!(matches!(look_with(&mut server, "tester", Some("k".into()), false), Looked::Secret { wrote: None, .. }));
        assert_eq!(server.asked, ["list", "fetch"]);
        // It writes once: reading the new one back is not a reason to write another.
        let mut server = Scripted::with(vec![playlist(RECORD, false), playlist(&written, false), playlist(&written, false)]);
        look_with(&mut server, "tester", Some("k".into()), true);
        assert_eq!(server.asked.iter().filter(|asked| **asked == "write").count(), 1);
        // Nothing a look would not use is written into: a playlist that is somebody's, one
        // with a later layout.
        let somebody = r#"<subsonic-response status="ok"><playlist id="k" name="Nexus Music link" comment="" songCount="3" public="false" owner="tester"></playlist></subsonic-response>"#;
        for stops in [Some(somebody.to_string()), playlist(r#"{"nexus_music_link":2}"#, false)] {
            let mut server = Scripted::with(vec![stops]);
            assert!(matches!(look_with(&mut server, "tester", Some("k".into()), true), Looked::Stopped(_)));
            assert!(!server.asked.contains(&"write"), "{:?}", server.asked);
        }
    }

    #[test]
    fn a_reset_that_cannot_be_written_leaves_the_secret_and_one_that_cannot_be_read_back_ends_it() {
        // The write gets no answer: nothing has changed, and nothing is concluded.
        let mut server = Scripted::with(vec![playlist(RECORD, false)]);
        server.write_works = false;
        let looked = look_with(&mut server, "tester", Some("k".into()), true);
        assert!(matches!(looked, Looked::Failed) && !ends_session(&looked));
        assert_eq!(server.asked, ["list", "fetch", "write"]);
        // Written, and the read-back gets no answer: the secret that was in use is the
        // record no longer, and is not used on the strength of a request that failed.
        let mut server = Scripted::with(vec![playlist(RECORD, false), None]);
        let looked = look_with(&mut server, "tester", Some("k".into()), true);
        assert!(ends_session(&looked), "the old secret would live on after a reset");
        // Signed out as it began: nothing is written.
        let mut server = Scripted::with(vec![playlist(RECORD, false)]);
        server.signed_out_after = Some(2);
        look_with(&mut server, "tester", Some("k".into()), true);
        assert_eq!(server.asked, ["list", "fetch"]);
    }

    /// The person signs out while a refresh is under way: the request in flight finishes,
    /// and the refresh makes no other.
    #[test]
    fn a_refresh_makes_no_request_after_the_sign_in_it_is_for_has_ended() {
        // A look that would take five requests: list, fetch (public), write, list, fetch.
        let whole = || Scripted::with(vec![playlist(RECORD, true), playlist(RECORD, false)]);
        let mut server = whole();
        look_with_no_reset(&mut server, "tester", Some("k".into()));
        assert_eq!(server.asked, ["list", "fetch", "write", "list", "fetch"]);
        // Signed out during each of them in turn: that one is the last.
        for during in 1..=5 {
            let mut server = whole();
            server.signed_out_after = Some(during);
            let looked = look_with_no_reset(&mut server, "tester", Some("k".into()));
            assert_eq!(server.asked.len(), during.min(5), "signed out during request {during}: {:?}", server.asked);
            if during < 5 {
                assert!(matches!(looked, Looked::Failed));
            }
        }
        // Signed out before it began, it asks nothing at all. And with a playlist to make.
        let mut server = whole();
        server.signed_out_after = Some(0);
        look_with_no_reset(&mut server, "tester", None);
        assert!(server.asked.is_empty());
        let mut server = Scripted::with(vec![]);
        server.listed.clear();
        server.signed_out_after = Some(2);
        look_with_no_reset(&mut server, "tester", None);
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

    /// Switching the setting off ends the session in the same step, so a frame that was
    /// already queued for this thread cannot act before the next tick notices.
    #[test]
    fn link_my_devices_off_takes_effect_at_once() {
        let mut link = linked(1_000);
        let (looks, over) = link.look_begins();
        let queued = link.session.as_ref().map(|session| session.stop.clone()).unwrap();
        // Switched on (as it was): nothing ends.
        assert!(!link.setting(true) && link.live(1, 1_000).is_some() && link.expects(looks));
        // Switched off: there is no session for the frame that arrives next to be opened
        // under, nothing queued is sent, and a look on its way starts nothing.
        assert!(link.setting(false));
        assert!(link.live(1, 1_000).is_none(), "a frame already on its way would still be opened");
        assert!(queued.load(Ordering::Relaxed) && over.load(Ordering::Relaxed) && !link.expects(looks));
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
