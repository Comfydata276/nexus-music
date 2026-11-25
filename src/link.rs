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
//! **The order of choices** is here too, and is on whether or not linking is: every way
//! into the player takes its place through [`Controller::choice`], [`Controller::lesser`]
//! or [`Controller::reserve`], so that a command from another device can be judged against
//! what has been chosen on this one.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use nexus_core::link::frame::{self, ASK, Keys, TELL};
use nexus_core::link::message::{Ask, At, Tell, Why, read_ask, read_tell};
use nexus_core::link::rule::Reservation;
use nexus_core::link::secret::{self, Fetched, Next, Secret, Stop, Verdict};
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

/// A choice is being carried out. While it is, what the player does on its way is part of
/// that choice and takes no place of its own.
pub struct Carrying<'a> {
    flag: &'a std::cell::Cell<bool>,
    outer: bool,
}

impl Drop for Carrying<'_> {
    fn drop(&mut self) {
        self.flag.set(self.outer);
    }
}

impl Controller {
    /// A choice that says what plays (play, pause, next, a new queue), made and carried out
    /// here and now: it takes its place in the order of choices. Anything still waiting to
    /// be carried out (a Play whose songs are being looked up) no longer has the say.
    pub fn choice(&self) -> Carrying<'_> {
        let outer = self.link_carrying.replace(true);
        if !outer {
            let mut rule = self.link_rule.borrow_mut();
            let reservation = rule.accept();
            rule.carry_out(reservation);
        }
        Carrying { flag: &self.link_carrying, outer }
    }

    /// A lesser choice (a seek, the volume, an edit to the queue), or something the player
    /// does by itself that changes what a command would act on (the next song starting).
    /// It does not displace a choice that is waiting to be carried out.
    pub fn lesser(&self) {
        if !self.link_carrying.get() {
            self.link_rule.borrow_mut().moved();
        }
    }

    /// As `lesser`, for something the player does by itself along a path a press also
    /// takes (passing over a song that won't play, the sleep timer's pause).
    pub fn by_itself(&self) -> Carrying<'_> {
        self.lesser();
        Carrying { flag: &self.link_carrying, outer: self.link_carrying.replace(true) }
    }

    /// A choice that says what plays is made now, at the press, and its work (looking the
    /// songs up) is still to do. Until `carried` or `given_up`, this PC is busy.
    pub fn reserve(&self) -> Reservation {
        self.link_rule.borrow_mut().accept()
    }

    /// The work for a reserved choice is done: carries it out with `then`, unless
    /// something has been chosen since, in which case nothing is done.
    pub fn carried(self: &Rc<Self>, held: Reservation, then: impl FnOnce(&Rc<Self>)) -> bool {
        if !self.link_rule.borrow_mut().carry_out(held) {
            return false;
        }
        let _carrying = Carrying { flag: &self.link_carrying, outer: self.link_carrying.replace(true) };
        then(self);
        true
    }

    /// The work for a reserved choice came to nothing.
    pub fn given_up(&self, held: Reservation) {
        self.link_rule.borrow_mut().give_up(held);
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
    NoRandom,
}

/// One look (see `nexus_core::link::secret`). Every call is a form POST, so neither the
/// sign-in nor the secret is in an address, and no redirect is followed.
fn look(client: &Client, mut used: Option<String>) -> Looked {
    let agent = crate::subsonic::make_agent_without_redirects();
    let call = |endpoint: &str, params: &[(&str, &str)]| client.link_call(&agent, endpoint, params).ok();
    // A new secret into a playlist, which is kept private. The secret used afterwards is the
    // one the next round reads back, not this one.
    let write = |id: &str| match random::<32>().map(Secret::new) {
        None => Some(Looked::NoRandom),
        Some(fresh) => call("updatePlaylist", &[("playlistId", id), ("comment", &fresh.comment()), ("public", "false")]).is_none().then_some(Looked::Failed),
    };
    let mut wrote = None;
    for _ in 0..secret::ROUNDS {
        let Some(listed) = call("getPlaylists", &[]) else { return Looked::Failed };
        let listed: Vec<Stated> = serde_json::from_value(listed["playlists"]["playlist"].clone()).unwrap_or_default();
        let id = match secret::choose(&listed, client.username(), used.as_deref()) {
            Next::Stop(stop) => return Looked::Stopped(stop),
            Next::Make => {
                let Some(made) = call("createPlaylist", &[("name", secret::PLAYLIST_NAME)]) else { return Looked::Failed };
                let Some(id) = made["playlist"]["id"].as_str().filter(|id| !id.is_empty()) else { return Looked::Failed };
                if let Some(failed) = write(id) {
                    return failed;
                }
                log(format!("look: made the playlist {id} and wrote a secret into it"));
                used = Some(id.to_string());
                continue;
            }
            Next::Fetch { id, replace } => (id, replace),
        };
        let (id, replace) = id;
        let Some(answer) = call("getPlaylist", &[("id", &id)]) else { return Looked::Failed };
        let playlist = &answer["playlist"];
        let said = |field: &str| playlist[field].as_str().map(str::to_string);
        let fetched = Fetched { id: said("id").unwrap_or_default(), name: said("name").unwrap_or_default(), owner: said("owner"), public: playlist["public"].as_bool(), comment: said("comment") };
        match secret::judge(&id, replace, &fetched, client.username()) {
            Verdict::Use(secret) => return Looked::Secret { secret, id, wrote },
            Verdict::Stop(stop) => return Looked::Stopped(stop),
            Verdict::Write(why) => {
                if let Some(failed) = write(&id) {
                    return failed;
                }
                log(format!("look: wrote a new secret into {id} ({why:?})"));
                wrote = Some(why);
                used = Some(id);
            }
        }
    }
    Looked::Failed
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
    since_ms: u64,
    wire: Arc<Wire>,
    stop: Arc<AtomicBool>,
    /// This device's place in the room and the token for it, once it has entered.
    place: Option<(Place, String)>,
    /// The other places in the room, in the relay's order, and what is known of each.
    others: Vec<Place>,
    peers: HashMap<Place, Seen>,
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
            self.link_end("");
            let mut link = self.link.borrow_mut();
            link.account = account;
            link.used = None;
            link.looked_ms = None;
            link.looking = false;
            link.looks += 1;
        }
        let wanted = self.client().is_some() && self.can_host_jam() && !self.link.borrow().off;
        if !wanted {
            self.link_end("");
            return self.link_show();
        }
        if self.link.borrow().session.as_ref().is_some_and(|session| !secret_holds(session.since_ms, now)) {
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

    /// Ends the session, if there is one: at sign-out, when the secret changes or runs out.
    pub fn link_end(&self, note: &str) {
        let mut link = self.link.borrow_mut();
        if let Some(session) = link.session.take() {
            session.stop.store(true, Ordering::Relaxed);
            link.serial += 1;
            log("session: ended");
        }
        if !note.is_empty() {
            link.note = note.to_string();
        }
    }

    fn link_look(self: &Rc<Self>, now: u64) {
        let Some(client) = self.client() else { return };
        let (used, looks, pool) = {
            let mut link = self.link.borrow_mut();
            link.looking = true;
            (link.used.clone(), link.looks, link.pool())
        };
        pool.spawn(move || {
            let looked = look(&client, used);
            post(move |ctl| ctl.link_looked(looks, now, looked));
        });
    }

    /// A look has finished. `since` is when it began.
    fn link_looked(self: &Rc<Self>, looks: u64, since: u64, looked: Looked) {
        let begin = {
            let mut link = self.link.borrow_mut();
            if link.looks != looks {
                return;
            }
            link.looking = false;
            link.looked_ms = Some(now_ms());
            match looked {
                Looked::Secret { secret, id, wrote } => {
                    link.used = Some(id);
                    if wrote == Some(secret::Why::Public) {
                        link.note = "Your devices' link could be seen by others on the music server. It has been made private and reset.".into();
                    }
                    match link.session.as_mut().filter(|session| session.secret == secret) {
                        Some(session) => {
                            session.since_ms = since;
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
                    }
                    .into();
                    log(format!("look: stopped ({stop:?})"));
                    None
                }
                Looked::Failed => {
                    log("look: the music server could not be asked");
                    None
                }
                Looked::NoRandom => {
                    link.off = true;
                    link.note = "This PC couldn't make random numbers, so linking is off.".into();
                    None
                }
            }
        };
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
            link.session = Some(Session { serial: link.serial, secret, keys, since_ms: since, wire: wire.clone(), stop: stop.clone(), place: None, others: Vec::new(), peers: HashMap::new() });
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
        let session = link.session.as_ref()?;
        let (place, token) = session.place.clone()?;
        let sealed = frame::seal(&session.keys, kind, &nonce, &place, &to, message.as_bytes())?;
        let (wire, room) = (session.wire.clone(), session.keys.room().to_string());
        pool.spawn(move || {
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
            busy: rule.busy(),
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
            let Some(session) = link.session.as_mut().filter(|session| session.serial == serial) else { return };
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
                let Some(session) = link.session.as_ref().filter(|session| session.serial == serial) else { return };
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
                let admitted = self.link_rule.borrow_mut().admit(at, minute(now));
                let done = admitted.and_then(|reservation| {
                    // ...and again at the moment the player is changed, under the session
                    // it arrived in.
                    let same_session = self.link.borrow().session.as_ref().is_some_and(|session| session.serial == serial);
                    self.link_rule.borrow_mut().finish(reservation, at, minute(now_ms()), same_session, nonce)?;
                    let _carrying = Carrying { flag: &self.link_carrying, outer: self.link_carrying.replace(true) };
                    self.pause_here();
                    Ok(())
                });
                match done {
                    Ok(()) => {
                        log(format!("pause from {who} against generation {}: carried out", at.generation));
                        self.link_state(Some(nonce), Some(true), None)
                    }
                    Err(why) => {
                        log(format!("pause from {who} against generation {}: refused, {why:?} (this device is at {}{})", at.generation, self.link_rule.borrow().generation(), if self.link_rule.borrow().busy() { ", busy" } else { "" }));
                        self.link_state(Some(nonce), Some(false), Some(why))
                    }
                }
            }
        };
        self.link_send(from, TELL, answer.text());
    }

    /// Another device says something of itself.
    fn link_told(self: &Rc<Self>, serial: u64, from: Place, message: &[u8]) {
        let Some(tell) = read_tell(message) else {
            return log(format!("tell from {}: breaks its shape, dropped", text::place(&from)));
        };
        let (told, name) = {
            let mut link = self.link.borrow_mut();
            let Some(session) = link.session.as_mut().filter(|session| session.serial == serial) else { return };
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
        let mut list = Vec::new();
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
                list.push(ui::LinkedDevice { place: text::place(place).into(), name: tell.name.as_str().into(), about: about.into(), playing: tell.playing });
            }
        }
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
