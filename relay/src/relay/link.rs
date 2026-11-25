//! Where one person's devices pass each other sealed frames: a room, and in it a mailbox
//! for each device.
//!
//! None of this is a jam, and none of it is reached through one. There are no guests, no
//! joining, no peeking, no covers and no names here: a device that holds the host key
//! takes a place in a room, sends frames to other places, and collects its own. A frame
//! is text this relay cannot read. The room's name is all that says which devices belong
//! together, and it is worked out by the apps from a secret the relay never sees.
//!
//! **What it can hold.** Every limit is in [`Limits`], and they add up: rooms times what
//! a room may keep waiting is the most that is ever stored (40 MiB as shipped), and places
//! times rooms is the most polls ever held (320).
//!
//! **What it can be doing.** The route has threads and a sweeper of its own, so nothing
//! read or written here can hold up a jam. Requests wait for those threads in a queue of
//! bounded length, and answers that carry frames are counted against a budget from the
//! moment they are made until they have been written; a poll whose answer does not fit
//! waits, with its frames, for the next round. A body is read only after the host key and
//! the place's token have been checked, and only if it says how long it is.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Condvar;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tiny_http::Request;

use super::{Parked, Relay, Reply, error, header, json_reply, query_value, read_json, same};

/// What the route's version is called in `/api/hello`.
pub const VERSION: u64 = 4;

/// Every bound on the route. The numbers are the shipped ones; tests make some smaller.
pub struct Limits {
    pub rooms: usize,
    /// Rooms one caller's address may hold places in.
    pub rooms_per_address: usize,
    pub places: usize,
    /// The whole body of a `send`.
    pub send_body: usize,
    /// What may wait for one place...
    pub mailbox_frames: usize,
    pub mailbox_bytes: usize,
    /// ...and in one room.
    pub room_bytes: usize,
    /// A whole answer to a poll.
    pub poll_answer: usize,
    /// Answers carrying frames that have been made and not yet written, in bytes.
    pub writing: usize,
    /// Requests waiting for one of the route's threads.
    pub queue: usize,
    pub threads: usize,
    /// `enter` from one address: this many at once, refilled over a minute.
    pub enters: f64,
    /// `send` from one place: this many at once, then this many a second...
    pub sends: (f64, f64),
    /// ...and from one address, a second.
    pub sends_per_address: f64,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            rooms: 20,
            rooms_per_address: 8,
            places: 16,
            send_body: 288 * 1024,
            mailbox_frames: 32,
            mailbox_bytes: 512 * 1024,
            room_bytes: 2 * 1024 * 1024,
            poll_answer: 512 * 1024,
            writing: 8 * 1024 * 1024,
            queue: 64,
            threads: 6,
            enters: 30.0,
            sends: (20.0, 5.0),
            sends_per_address: 40.0,
        }
    }
}

struct Frame {
    seq: u64,
    from: String,
    frame: String,
}

/// One device's place in a room: its mailbox, and its wait for something to be put in it.
struct Place {
    place: String,
    /// What it was opened with. Nothing else collects from it, sends as it, or replaces it.
    token: String,
    address: String,
    /// When it last asked. A place that stops asking is given up.
    seen: Instant,
    poll: Option<Parked>,
    frames: VecDeque<Frame>,
    bytes: usize,
    /// The number the next frame gets, and the highest handed out in an answer.
    next: u64,
    handed: u64,
}

#[derive(Default)]
struct Room {
    places: Vec<Place>,
    /// Goes up whenever a place comes or goes.
    version: u64,
    bytes: usize,
}

impl Room {
    fn roster(&self) -> Value {
        json!(self.places.iter().map(|place| place.place.as_str()).collect::<Vec<_>>())
    }

    /// A place came or went: everyone waiting is told.
    fn changed(&mut self, link: &Link, limits: &Limits, out: &mut Vec<Answer>) {
        self.version += 1;
        let (version, roster) = (self.version, self.roster());
        for place in &mut self.places {
            if let Some(parked) = place.poll.take() {
                match place.answer(version, &roster, false, link, limits) {
                    Some((reply, held)) => out.push(Answer { request: parked.request, reply, held }),
                    // No room to write its frames just now: it is told of the change alone.
                    None => out.push(Answer { request: parked.request, reply: empty(version, &roster, true, false), held: 0 }),
                }
            }
        }
    }

    fn remove(&mut self, at: usize, out: &mut Vec<Answer>) {
        let place = self.places.remove(at);
        self.bytes -= place.bytes;
        if let Some(parked) = place.poll {
            out.push(Answer { request: parked.request, reply: error(404, "Enter the room first."), held: 0 });
        }
    }
}

/// An answer with no frames in it.
fn empty(version: u64, roster: &Value, more: bool, replaced: bool) -> Reply {
    let mut body = json!({"v": version, "places": roster, "frames": [], "more": more});
    if replaced {
        body["replaced"] = json!(true);
    }
    json_reply(200, &body)
}

impl Place {
    /// What is waiting for this place, as an answer to its poll: as many frames, oldest
    /// first, as an answer may weigh. `None` if they cannot be written just now.
    fn answer(&mut self, version: u64, roster: &Value, replaced: bool, link: &Link, limits: &Limits) -> Option<(Reply, usize)> {
        // What wraps each frame, and the list of places, come out of the same allowance.
        let room_for_frames = limits.poll_answer.saturating_sub(4096);
        let (mut held, mut weight, mut frames, mut more, mut last) = (0, 0, Vec::new(), false, self.handed);
        for frame in &self.frames {
            let adds = frame.frame.len() + 96;
            if !frames.is_empty() && weight + adds > room_for_frames {
                more = true;
                break;
            }
            weight += adds;
            held += frame.frame.len();
            last = last.max(frame.seq);
            frames.push(json!({"seq": frame.seq, "from": frame.from, "frame": frame.frame}));
        }
        if held > 0 && !link.take(held, limits.writing) {
            return None;
        }
        self.handed = last;
        let mut body = json!({"v": version, "places": roster, "frames": frames, "more": more});
        if replaced {
            body["replaced"] = json!(true);
        }
        Some((json_reply(200, &body), held))
    }

    /// Forgets the frames its device says it has dealt with. A number higher than any
    /// handed out acknowledges nothing: it was not this place's to give.
    fn dealt_with(&mut self, after: u64) -> usize {
        if after > self.handed {
            return 0;
        }
        let mut freed = 0;
        while self.frames.front().is_some_and(|frame| frame.seq <= after) {
            freed += self.frames.pop_front().map_or(0, |frame| frame.frame.len());
        }
        self.bytes -= freed;
        freed
    }
}

/// An answer to write, and how much of the writing budget it holds until it is written.
struct Answer {
    request: Request,
    reply: Reply,
    held: usize,
}

#[derive(Clone, Copy)]
pub enum Call {
    Enter,
    Poll,
    Send,
    Leave,
}

enum Job {
    Call(Request, Call, String, String),
    Write(Answer),
}

/// How much the route holds at this moment: for the tests, which count it.
#[cfg(test)]
#[derive(Debug, Default, PartialEq)]
pub struct Totals {
    pub rooms: usize,
    pub places: usize,
    pub frames: usize,
    /// Bytes of frames waiting to be collected.
    pub bytes: usize,
    /// Polls held open.
    pub held: usize,
    /// Bytes of answers made and not yet written.
    pub writing: usize,
    /// Requests waiting for a thread.
    pub waiting: usize,
}

#[derive(Default)]
pub struct Link {
    rooms: Mutex<HashMap<String, Room>>,
    jobs: Mutex<VecDeque<Job>>,
    wake: Condvar,
    waiting: AtomicUsize,
    writing: AtomicUsize,
}

impl Link {
    /// Sets `bytes` aside from what may be being written at once, if there is that much.
    fn take(&self, bytes: usize, limit: usize) -> bool {
        self.writing.try_update(Ordering::SeqCst, Ordering::SeqCst, |now| (now + bytes <= limit).then_some(now + bytes)).is_ok()
    }

    fn give(&self, bytes: usize) {
        self.writing.fetch_sub(bytes, Ordering::SeqCst);
    }

    #[cfg(test)]
    pub fn totals(&self) -> Totals {
        let rooms = self.rooms.lock().unwrap();
        let places = || rooms.values().flat_map(|room| room.places.iter());
        Totals {
            rooms: rooms.len(),
            places: places().count(),
            frames: places().map(|place| place.frames.len()).sum(),
            bytes: rooms.values().map(|room| room.bytes).sum(),
            held: places().filter(|place| place.poll.is_some()).count(),
            writing: self.writing.load(Ordering::SeqCst),
            waiting: self.waiting.load(Ordering::SeqCst),
        }
    }

    fn queue(&self, job: Job) {
        self.jobs.lock().unwrap().push_back(job);
        self.wake.notify_one();
    }

    fn write(&self, answer: Answer) {
        let _ = answer.request.respond(answer.reply);
        self.give(answer.held);
    }
}

/// A request for the route, from one of the threads jams share: it is put in the route's
/// own queue and that thread goes back to its work. With the queue full it is turned away.
pub fn take(relay: &Relay, request: Request, call: Call, room: &str, query: &str) {
    let link = &relay.link;
    if link.waiting.try_update(Ordering::SeqCst, Ordering::SeqCst, |now| (now < relay.config.link.queue).then_some(now + 1)).is_err() {
        let _ = request.respond(busy("This relay is busy. Try again in a moment."));
        return;
    }
    link.queue(Job::Call(request, call, room.to_string(), query.to_string()));
}

/// The route's own threads, and its sweeper.
pub fn start(relay: Arc<Relay>) {
    for index in 0..relay.config.link.threads.max(1) {
        let relay = relay.clone();
        let _ = std::thread::Builder::new().name(format!("link-{index}")).spawn(move || {
            loop {
                let job = {
                    let mut jobs = relay.link.jobs.lock().unwrap();
                    loop {
                        match jobs.pop_front() {
                            Some(job) => break job,
                            None => jobs = relay.link.wake.wait(jobs).unwrap(),
                        }
                    }
                };
                match job {
                    Job::Write(answer) => relay.link.write(answer),
                    Job::Call(request, call, room, query) => {
                        relay.link.waiting.fetch_sub(1, Ordering::SeqCst);
                        let mut out = Vec::new();
                        match call {
                            Call::Enter => enter(&relay, request, &room, &mut out),
                            Call::Poll => poll(&relay, request, &room, &query, &mut out),
                            Call::Send => send(&relay, request, &room, &mut out),
                            Call::Leave => leave(&relay, request, &room, &mut out),
                        }
                        for answer in out {
                            relay.link.write(answer);
                        }
                    }
                }
            }
        });
    }
    let _ = std::thread::Builder::new().name("link-sweeper".into()).spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(250));
            // It writes nothing itself: what it finds to answer goes to the route's threads.
            for answer in sweep(&relay, Instant::now()) {
                relay.link.queue(Job::Write(answer));
            }
        }
    });
}

/// Answers polls that have waited long enough, or whose frames can now be written, and
/// gives up the places that have stopped asking.
fn sweep(relay: &Relay, now: Instant) -> Vec<Answer> {
    let (link, limits) = (&relay.link, &relay.config.link);
    let mut out = Vec::new();
    let mut rooms = link.rooms.lock().unwrap();
    for room in rooms.values_mut() {
        let (version, roster) = (room.version, room.roster());
        for place in &mut room.places {
            let due = place.poll.as_ref().is_some_and(|parked| parked.until <= now);
            if place.poll.is_none() || (!due && place.frames.is_empty()) {
                continue;
            }
            let answer = match place.answer(version, &roster, false, link, limits) {
                Some(answer) => answer,
                // Its wait is over and its frames still can't be written: it is told to ask again.
                None if due => (empty(version, &roster, true, false), 0),
                None => continue,
            };
            if let Some(parked) = place.poll.take() {
                place.seen = now;
                out.push(Answer { request: parked.request, reply: answer.0, held: answer.1 });
            }
        }
        let gone: Vec<usize> = (0..room.places.len()).filter(|&at| room.places[at].poll.is_none() && now.saturating_duration_since(room.places[at].seen) > relay.config.idle_after).collect();
        for at in gone.iter().rev() {
            room.remove(*at, &mut out);
        }
        if !gone.is_empty() {
            room.changed(link, limits, &mut out);
        }
    }
    rooms.retain(|_, room| !room.places.is_empty());
    out
}

// ---- answering ---------------------------------------------------------------------------

fn busy(message: &str) -> Reply {
    error(503, message)
}

fn slow_down(message: &str) -> Reply {
    error(429, message).with_header(header("Retry-After", "1"))
}

fn refuse(request: Request, reply: Reply, out: &mut Vec<Answer>) {
    out.push(Answer { request, reply, held: 0 });
}

/// The place's token, from its own header.
fn token(request: &Request) -> String {
    request.headers().iter().find(|header| header.field.equiv("X-Nexus-Link")).map(|header| header.value.as_str().trim().to_string()).unwrap_or_default()
}

/// Who is asking, for counting: the connection's own address, unless the connection comes
/// from the proxy this relay was told to trust, in which case the last address in
/// `X-Forwarded-For`, which that proxy wrote. Nothing a client sends is believed otherwise.
fn caller(relay: &Relay, request: &Request) -> String {
    let own = request.remote_addr().map(|address| address.ip());
    let forwarded = || request.headers().iter().find(|header| header.field.equiv("X-Forwarded-For")).and_then(|header| header.value.as_str().rsplit(',').next().and_then(|text| text.trim().parse::<IpAddr>().ok()));
    let address = match (own, relay.config.trusted_proxy) {
        (Some(own), Some(proxy)) if plain(own) == plain(proxy) => forwarded().unwrap_or(own),
        (own, _) => match own {
            Some(own) => own,
            None => return String::new(),
        },
    };
    counted(address)
}

/// An address as itself, whichever way it was written.
fn plain(address: IpAddr) -> IpAddr {
    match address {
        IpAddr::V6(six) => six.to_ipv4_mapped().map_or(address, IpAddr::V4),
        four => four,
    }
}

/// What an address is counted as. One household has one IPv4 address and a whole /64 of
/// IPv6 ones, so the /64 is what is counted.
pub(super) fn counted(address: IpAddr) -> String {
    match plain(address) {
        IpAddr::V4(four) => four.to_string(),
        IpAddr::V6(six) => {
            let parts = six.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", parts[0], parts[1], parts[2], parts[3])
        }
    }
}

const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Whether `text` is `bytes` bytes in the given alphabet with no padding, spelled the one
/// way it can be: every character of the alphabet, and no bits left over set.
fn spelled(text: &str, alphabet: &[u8], bytes: usize) -> bool {
    let bits = alphabet.len().trailing_zeros() as usize;
    let length = (bytes * 8).div_ceil(bits);
    let spare = length * bits - bytes * 8;
    let value = |byte: u8| alphabet.iter().position(|letter| *letter == byte);
    text.len() == length && text.bytes().all(|byte| value(byte).is_some()) && text.bytes().last().and_then(value).is_some_and(|last| last & ((1 << spare) - 1) == 0)
}

pub(super) fn is_room(text: &str) -> bool {
    spelled(text, BASE32, 32)
}

fn is_place(text: &str) -> bool {
    spelled(text, BASE32, 16)
}

fn is_frame(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| BASE64.contains(&byte))
}

/// A token for a place: 32 bytes from the system, as 43 characters. `None` if the system
/// has none to give, in which case nobody is let in.
fn new_token() -> Option<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).ok()?;
    let mut text = String::with_capacity(43);
    for chunk in bytes.chunks(3) {
        let all = chunk.iter().fold(0u32, |sum, byte| sum << 8 | *byte as u32) << (8 * (3 - chunk.len()));
        for at in 0..=chunk.len() {
            text.push(BASE64[(all >> (18 - 6 * at) & 63) as usize] as char);
        }
    }
    Some(text)
}

/// The place a token opens, if it opens one in this room.
fn holder(room: &Room, token: &str) -> Option<usize> {
    room.places.iter().position(|place| same(place.token.as_bytes(), token.as_bytes()))
}

// ---- the four calls ------------------------------------------------------------------------

/// A device takes a place in a room. The place is its own choice of name, new for each run
/// of the app; one that is taken stays taken.
fn enter(relay: &Relay, mut request: Request, name: &str, out: &mut Vec<Answer>) {
    let (link, limits) = (&relay.link, &relay.config.link);
    if let Err(refusal) = relay.host_key_ok(&request) {
        return refuse(request, refusal, out);
    }
    let address = caller(relay, &request);
    if !relay.allow(&format!("link-enter:{address}"), limits.enters, limits.enters / 60.0) {
        return refuse(request, slow_down("Too many arrivals from this address. Wait a moment."), out);
    }
    if request.body_length().is_none() {
        return refuse(request, error(411, "Say how much is being sent."), out);
    }
    let body = match read_json(&mut request, super::GUEST_BODY) {
        Ok(body) => body,
        Err(refusal) => return refuse(request, refusal, out),
    };
    let place = body["place"].as_str().unwrap_or("");
    if !is_room(name) || !is_place(place) {
        return refuse(request, error(400, "That isn't a room or a place in one."), out);
    }
    let Some(token) = new_token() else { return refuse(request, busy("This relay can't make a token just now."), out) };
    let mut rooms = link.rooms.lock().unwrap();
    if !rooms.contains_key(name) && rooms.len() >= limits.rooms {
        return refuse(request, busy("This relay has as many rooms as it allows."), out);
    }
    let elsewhere = rooms.iter().filter(|(other, room)| other.as_str() != name && room.places.iter().any(|place| place.address == address)).count();
    if elsewhere >= limits.rooms_per_address && !rooms.get(name).is_some_and(|room| room.places.iter().any(|place| place.address == address)) {
        return refuse(request, busy("That's as many rooms as one address may be in."), out);
    }
    let taken = rooms.get(name).map(|room| (room.places.iter().any(|other| other.place == place), room.places.len()));
    match taken {
        Some((true, _)) => return refuse(request, error(409, "That place is taken."), out),
        Some((_, places)) if places >= limits.places => return refuse(request, busy("That's as many devices as a room holds."), out),
        _ => {}
    }
    let room = rooms.entry(name.to_string()).or_default();
    room.places.push(Place { place: place.to_string(), token: token.clone(), address, seen: Instant::now(), poll: None, frames: VecDeque::new(), bytes: 0, next: 1, handed: 0 });
    room.changed(link, limits, out);
    refuse(request, json_reply(200, &json!({"token": token, "v": room.version, "places": room.roster()})), out);
}

/// A device waits for a frame, or for a place to come or go.
fn poll(relay: &Relay, request: Request, name: &str, query: &str, out: &mut Vec<Answer>) {
    let (link, limits) = (&relay.link, &relay.config.link);
    if let Err(refusal) = relay.host_key_ok(&request) {
        return refuse(request, refusal, out);
    }
    let number = |field: &str| query_value(query, field).and_then(|text| text.parse::<u64>().ok()).unwrap_or(0);
    let (seen, after) = (number("v"), number("after"));
    let mut rooms = link.rooms.lock().unwrap();
    let Some(room) = rooms.get_mut(name) else { return refuse(request, error(404, "Enter the room first."), out) };
    let Some(at) = holder(room, &token(&request)) else { return refuse(request, error(404, "Enter the room first."), out) };
    let (version, roster) = (room.version, room.roster());
    let freed = room.places[at].dealt_with(after);
    room.bytes -= freed;
    let place = &mut room.places[at];
    place.seen = Instant::now();
    if let Some(earlier) = place.poll.take() {
        out.push(Answer { request: earlier.request, reply: empty(version, &roster, !place.frames.is_empty(), true), held: 0 });
    }
    if (!place.frames.is_empty() || seen != version)
        && let Some((reply, held)) = place.answer(version, &roster, false, link, limits)
    {
        return out.push(Answer { request, reply, held });
    }
    place.poll = Some(Parked { request, until: Instant::now() + relay.config.hold });
}

/// A device puts a frame in another's mailbox. The frame is marked with the sender's
/// place as this relay knows it from the token, not as the sender says.
fn send(relay: &Relay, mut request: Request, name: &str, out: &mut Vec<Answer>) {
    let (link, limits) = (&relay.link, &relay.config.link);
    if let Err(refusal) = relay.host_key_ok(&request) {
        return refuse(request, refusal, out);
    }
    let shown = token(&request);
    let from = {
        let rooms = link.rooms.lock().unwrap();
        rooms.get(name).and_then(|room| holder(room, &shown).map(|at| room.places[at].place.clone()))
    };
    let Some(from) = from else { return refuse(request, error(404, "Enter the room first."), out) };
    let address = caller(relay, &request);
    if !relay.allow(&format!("link-send:{name}:{from}"), limits.sends.0, limits.sends.1) || !relay.allow(&format!("link-sends:{address}"), limits.sends_per_address, limits.sends_per_address) {
        return refuse(request, slow_down("Too much is being sent. Wait a moment."), out);
    }
    // The body is read only now, and only if it says how long it is.
    if request.body_length().is_none() {
        return refuse(request, error(411, "Say how much is being sent."), out);
    }
    let mut body = match read_json(&mut request, limits.send_body) {
        Ok(body) => body,
        Err(refusal) => return refuse(request, refusal, out),
    };
    let to = body["to"].as_str().unwrap_or("").to_string();
    let frame = match body["frame"].take() {
        Value::String(frame) if is_place(&to) && is_frame(&frame) => frame,
        _ => return refuse(request, error(400, "That isn't a frame for a place."), out),
    };
    let mut rooms = link.rooms.lock().unwrap();
    let Some(room) = rooms.get_mut(name) else { return refuse(request, error(404, "Enter the room first."), out) };
    // The sender may have gone, or been given up on, while its body was read.
    if holder(room, &shown).is_none() {
        return refuse(request, error(404, "Enter the room first."), out);
    }
    let Some(at) = room.places.iter().position(|place| place.place == to) else { return refuse(request, error(404, "There's no such place in the room."), out) };
    let place = &room.places[at];
    if place.frames.len() >= limits.mailbox_frames || place.bytes + frame.len() > limits.mailbox_bytes || room.bytes + frame.len() > limits.room_bytes {
        return refuse(request, slow_down("That device has as much waiting as it may. Try again in a moment."), out);
    }
    let (version, roster) = (room.version, room.roster());
    room.bytes += frame.len();
    let place = &mut room.places[at];
    place.bytes += frame.len();
    place.frames.push_back(Frame { seq: place.next, from, frame });
    place.next += 1;
    if place.poll.is_some()
        && let Some((reply, held)) = place.answer(version, &roster, false, link, limits)
        && let Some(parked) = place.poll.take()
    {
        place.seen = Instant::now();
        out.push(Answer { request: parked.request, reply, held });
    }
    refuse(request, json_reply(200, &json!({"ok": true})), out);
}

/// A device gives up its place: what was waiting for it is dropped.
fn leave(relay: &Relay, request: Request, name: &str, out: &mut Vec<Answer>) {
    let (link, limits) = (&relay.link, &relay.config.link);
    if let Err(refusal) = relay.host_key_ok(&request) {
        return refuse(request, refusal, out);
    }
    let mut rooms = link.rooms.lock().unwrap();
    let left = rooms.get_mut(name).and_then(|room| {
        let at = holder(room, &token(&request))?;
        room.remove(at, out);
        room.changed(link, limits, out);
        Some(room.places.is_empty())
    });
    match left {
        Some(emptied) => {
            if emptied {
                rooms.remove(name);
            }
            refuse(request, json_reply(200, &json!({"ok": true})), out);
        }
        None => refuse(request, error(404, "Enter the room first."), out),
    }
}

#[cfg(test)]
mod tests;
