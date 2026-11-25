//! Where one person's devices pass each other sealed frames: a room, and in it a mailbox
//! for each device.
//!
//! None of this is a jam, and none of it is reached through one. There are no guests, no
//! joining, no peeking, no covers and no names here: a device that holds the host key
//! takes a place in a room, sends frames to other places, and collects its own. A frame
//! is text this relay cannot read. The room's name is all that says which devices belong
//! together, and it is worked out by the apps from a secret the relay never sees.
//!
//! **It can never cost a jam anything.** The route has a listener of its own, and reads
//! and writes its connections itself, on threads of its own: nothing on its path runs on
//! a thread a jam uses, refusals included. (It cannot share the jams' listener and keep
//! that promise. The HTTP library jams are served with gives no deadline on reading a
//! body, and reads what is left of a body, however long its sender takes, on whichever
//! thread lets go of the request.) The proxy in front sends `/api/link/` here.
//!
//! **What it can hold.** Every limit is in [`Limits`], and they add up: rooms times what
//! a room may keep waiting is the most that is ever stored (40 MiB as shipped), and places
//! times rooms is the most polls ever held (320).
//!
//! **What it can be doing.** Connections to the route are counted, and one too many is
//! closed as it arrives, unread. A connection waits for a thread in a queue of bounded
//! length; past that it is told the relay is busy and closed, unread. A request's head has
//! a deadline; a body is read only after the host key and the place's token have been
//! checked, only if it says how long it is and that is within the limit for its call, and
//! it too has a deadline. Every reply, head and body, is counted against a budget **before
//! it is built** and until it has been written; a poll whose answer does not fit waits,
//! with its frames, for the next round. A write that makes no progress is given up. Every
//! connection carries one request and is then closed.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::sync::Condvar;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Relay, same};

/// What the route's version is called in `/api/hello`.
pub const VERSION: u64 = 4;

/// The most a request's head may be: its first line and its headers.
const HEAD: usize = 8 * 1024;
/// The most the body of a call that is not `send` may be.
const SMALL_BODY: usize = 8 * 1024;

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
    /// The whole body of an answer to a poll.
    pub poll_answer: usize,
    /// Replies, heads and bodies, that have been admitted and not yet written, in bytes.
    pub writing: usize,
    /// Connections to the route at once, whatever each is doing.
    pub connections: usize,
    /// Connections waiting for one of the route's threads.
    pub queue: usize,
    pub threads: usize,
    /// How long a request's head may take to arrive, and its body.
    pub head_wait: Duration,
    pub body_wait: Duration,
    /// How long a reply may make no progress.
    pub write_wait: Duration,
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
            connections: 512,
            queue: 64,
            threads: 6,
            head_wait: Duration::from_secs(10),
            body_wait: Duration::from_secs(10),
            write_wait: Duration::from_secs(20),
            enters: 30.0,
            sends: (20.0, 5.0),
            sends_per_address: 40.0,
        }
    }
}

// ---- a connection, and what is said on it --------------------------------------------------

/// A connection to the route. It is counted from the moment it is taken until it is let
/// go of, whatever happens to it in between.
struct Conn {
    stream: TcpStream,
    peer: Option<IpAddr>,
    open: Arc<AtomicUsize>,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A request's head, as much of it as the route has any use for.
struct Asked {
    post: bool,
    room: String,
    verb: String,
    query: String,
    /// The host key shown, and the place's token.
    key: String,
    token: String,
    /// How long the body says it is.
    length: Option<usize>,
    /// The last address in `X-Forwarded-For`, if there is one and it is an address.
    forwarded: Option<IpAddr>,
    /// What had arrived of the body by the time the head was read.
    rest: Vec<u8>,
}

/// Reads until `deadline`, a piece at a time. `None` when the time is up or the
/// connection is gone.
fn read_some(stream: &mut TcpStream, into: &mut [u8], deadline: Instant) -> Option<usize> {
    let left = deadline.checked_duration_since(Instant::now()).filter(|left| !left.is_zero())?;
    stream.set_read_timeout(Some(left)).ok()?;
    match stream.read(into) {
        Ok(0) | Err(_) => None,
        Ok(read) => Some(read),
    }
}

/// Reads a request's head. `Err` is what to say, if anything is to be said at all: a
/// connection that sends no head in time, or not one, is simply closed.
fn read_head(conn: &mut Conn, wait: Duration) -> Result<Asked, Option<(u16, &'static str)>> {
    let deadline = Instant::now() + wait;
    let mut bytes = Vec::with_capacity(1024);
    let end = loop {
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
        if bytes.len() >= HEAD {
            return Err(Some((431, "That request's headers are too long.")));
        }
        let mut piece = [0u8; 2048];
        let room = (HEAD - bytes.len()).min(piece.len());
        let read = read_some(&mut conn.stream, &mut piece[..room], deadline).ok_or(None)?;
        bytes.extend_from_slice(&piece[..read]);
    };
    let rest = bytes.split_off(end + 4);
    let head = std::str::from_utf8(&bytes[..end]).map_err(|_| None)?;
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, target) = (first.next().unwrap_or(""), first.next().unwrap_or(""));
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let (room, verb) = match parts.as_slice() {
        ["api", "link", room, verb] => (*room, *verb),
        _ => return Err(Some((404, "There's nothing at that address."))),
    };
    let post = match (method, verb) {
        ("POST", "enter" | "send" | "leave") => true,
        ("GET", "poll") => false,
        _ => return Err(Some((404, "There's nothing at that address."))),
    };
    let mut asked = Asked { post, room: room.to_string(), verb: verb.to_string(), query: query.to_string(), key: String::new(), token: String::new(), length: None, forwarded: None, rest };
    for line in lines {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "authorization" => asked.key = value.strip_prefix("Bearer ").unwrap_or("").trim().to_string(),
            "x-nexus-link" => asked.token = value.to_string(),
            "content-length" => asked.length = Some(value.parse().map_err(|_| Some((400, "That wasn't understood.")))?),
            "x-forwarded-for" => asked.forwarded = value.rsplit(',').next().and_then(|text| text.trim().parse().ok()),
            // A body that comes in pieces doesn't say how long it is: it is not read.
            "transfer-encoding" => return Err(Some((411, "Say how much is being sent."))),
            _ => {}
        }
    }
    Ok(asked)
}

/// Reads the body the head promised, within its deadline, as JSON. `Err` is what to say.
fn read_body(conn: &mut Conn, asked: &mut Asked, wait: Duration) -> Result<Value, Option<(u16, &'static str)>> {
    let length = asked.length.unwrap_or(0);
    let mut body = std::mem::take(&mut asked.rest);
    body.truncate(length);
    let deadline = Instant::now() + wait;
    while body.len() < length {
        let mut piece = [0u8; 16 * 1024];
        let room = (length - body.len()).min(piece.len());
        // Not all of it came in time: there is nobody left to tell.
        let read = read_some(&mut conn.stream, &mut piece[..room], deadline).ok_or(None)?;
        body.extend_from_slice(&piece[..read]);
    }
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&body).map_err(|_| Some((400, "That wasn't understood.")))
}

/// Lets go of a connection that has been answered. A request refused with some of its body
/// unread would, if simply closed, have its reply thrown away at the other end along with
/// the connection. So the sending side is closed first, and the other end is given a
/// moment to finish and close: a quarter of a second and 64 KiB at the very most, which
/// is not the body being read (that could be anything) and is on one of the route's own
/// threads.
fn let_go(stream: &mut TcpStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let deadline = Instant::now() + Duration::from_millis(250);
    let (mut piece, mut seen) = ([0u8; 8192], 0);
    while seen < 64 * 1024 {
        match read_some(stream, &mut piece, deadline) {
            Some(read) => seen += read,
            None => break,
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        _ => "Service Unavailable",
    }
}

/// The head of a reply whose body is `length` bytes.
fn reply_head(status: u16, length: usize) -> String {
    let retry = if status == 429 { "Retry-After: 1\r\n" } else { "" };
    format!("HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n{retry}Connection: close\r\n\r\n", reason(status))
}

/// A reply to write: every byte of it, all of which has been counted against the budget.
struct Answer {
    conn: Conn,
    bytes: Vec<u8>,
}

/// Somewhere a poll's answer is written: for real, or only to be measured.
trait Sink {
    fn put(&mut self, text: &str);
}

impl Sink for Vec<u8> {
    fn put(&mut self, text: &str) {
        self.extend_from_slice(text.as_bytes());
    }
}

/// How long it would be.
struct Measure(usize);

impl Sink for Measure {
    fn put(&mut self, text: &str) {
        self.0 += text.len();
    }
}

/// The body of an answer to a poll. Written the same way to be measured and to be sent, so
/// what is charged is what is written. (Places and frames are spelled in letters that need
/// no escaping.)
fn poll_body(out: &mut impl Sink, version: u64, places: &[&str], frames: &[&Frame], more: bool, replaced: bool) {
    out.put("{\"v\":");
    out.put(&version.to_string());
    out.put(",\"places\":[");
    for (at, place) in places.iter().enumerate() {
        out.put(if at == 0 { "\"" } else { ",\"" });
        out.put(place);
        out.put("\"");
    }
    out.put("],\"frames\":[");
    for (at, frame) in frames.iter().enumerate() {
        out.put(if at == 0 { "{\"seq\":" } else { ",{\"seq\":" });
        out.put(&frame.seq.to_string());
        out.put(",\"from\":\"");
        out.put(&frame.from);
        out.put("\",\"frame\":\"");
        out.put(&frame.frame);
        out.put("\"}");
    }
    out.put(if more { "],\"more\":true" } else { "],\"more\":false" });
    out.put(if replaced { ",\"replaced\":true}" } else { "}" });
}

// ---- rooms -------------------------------------------------------------------------------

struct Frame {
    seq: u64,
    from: String,
    frame: String,
}

/// A poll put away until there is something to answer it with, or until `until`.
struct Parked {
    conn: Conn,
    until: Instant,
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
    fn roster(&self) -> Vec<String> {
        self.places.iter().map(|place| place.place.clone()).collect()
    }

    /// A place came or went: everyone waiting is told.
    fn changed(&mut self, link: &Link, limits: &Limits, out: &mut Vec<Answer>) {
        self.version += 1;
        let (version, roster) = (self.version, self.roster());
        for place in &mut self.places {
            let Some(parked) = place.poll.take() else { continue };
            // With no room to write its frames just now, it is told of the change alone.
            let bytes = place.answer(version, &roster, false, link, limits).or_else(|| link.admit_poll(version, &roster, &[], true, false, limits));
            match bytes {
                Some(bytes) => out.push(Answer { conn: parked.conn, bytes }),
                None => place.poll = Some(parked),
            }
        }
    }

    fn remove(&mut self, at: usize, link: &Link, limits: &Limits, out: &mut Vec<Answer>) {
        let place = self.places.remove(at);
        self.bytes -= place.bytes;
        if let Some(parked) = place.poll {
            link.say(parked.conn, 404, "Enter the room first.", limits, out);
        }
    }
}

impl Place {
    /// What is waiting for this place, as the whole reply to its poll: as many frames,
    /// oldest first, as an answer may weigh. `None` if it cannot be written just now. No
    /// frame is copied until the whole reply has been measured and admitted.
    fn answer(&mut self, version: u64, roster: &[String], replaced: bool, link: &Link, limits: &Limits) -> Option<Vec<u8>> {
        let places: Vec<&str> = roster.iter().map(String::as_str).collect();
        // (What wraps a frame in the answer is under a hundred bytes; the places, a few
        // hundred in all.)
        let room_for_frames = limits.poll_answer.saturating_sub(1024);
        let (mut weight, mut chosen) = (0, Vec::new());
        for frame in &self.frames {
            let adds = frame.frame.len() + 96;
            if !chosen.is_empty() && weight + adds > room_for_frames {
                break;
            }
            weight += adds;
            chosen.push(frame);
        }
        let more = chosen.len() < self.frames.len();
        let last = chosen.last().map_or(self.handed, |frame| frame.seq);
        let bytes = link.admit_poll(version, &places, &chosen, more, replaced, limits)?;
        self.handed = self.handed.max(last);
        Some(bytes)
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
    /// Bytes of replies admitted and not yet written.
    pub writing: usize,
    /// Connections waiting for a thread.
    pub waiting: usize,
    /// Connections to the route, whatever each is doing.
    pub open: usize,
}

enum Job {
    Call(Conn),
    Write(Answer),
}

#[derive(Default)]
pub struct Link {
    rooms: Mutex<HashMap<String, Room>>,
    jobs: Mutex<VecDeque<Job>>,
    wake: Condvar,
    waiting: AtomicUsize,
    writing: AtomicUsize,
    open: Arc<AtomicUsize>,
    /// Every byte of every reply the route has written, for the tests to set beside what
    /// the callers received.
    #[cfg(test)]
    written: AtomicUsize,
    /// Where the route listens, once it does. Until then `/api/hello` does not offer it.
    at: Mutex<Option<SocketAddr>>,
}

impl Link {
    /// Whether the route is being served: what `/api/hello` goes by.
    pub fn listening(&self) -> bool {
        self.at.lock().unwrap().is_some()
    }

    /// Sets `bytes` aside from what may be being written at once, if there is that much.
    fn take(&self, bytes: usize, limit: usize) -> bool {
        self.writing.try_update(Ordering::SeqCst, Ordering::SeqCst, |now| (now + bytes <= limit).then_some(now + bytes)).is_ok()
    }

    /// The whole reply to a poll, if it may be written now: measured, admitted against the
    /// budget, and only then built.
    fn admit_poll(&self, version: u64, places: &[impl AsRef<str>], frames: &[&Frame], more: bool, replaced: bool, limits: &Limits) -> Option<Vec<u8>> {
        let places: Vec<&str> = places.iter().map(AsRef::as_ref).collect();
        let mut measure = Measure(0);
        poll_body(&mut measure, version, &places, frames, more, replaced);
        let head = reply_head(200, measure.0);
        let whole = head.len() + measure.0;
        if !self.take(whole, limits.writing) {
            return None;
        }
        let mut bytes = Vec::with_capacity(whole);
        bytes.extend_from_slice(head.as_bytes());
        poll_body(&mut bytes, version, &places, frames, more, replaced);
        debug_assert_eq!(bytes.len(), whole);
        Some(bytes)
    }

    /// A short reply: a refusal, or the answer to anything but a poll. With no room in the
    /// budget even for that, the connection is closed with nothing said.
    fn tell(&self, conn: Conn, status: u16, body: &Value, limits: &Limits, out: &mut Vec<Answer>) {
        let body = body.to_string();
        let head = reply_head(status, body.len());
        if self.take(head.len() + body.len(), limits.writing) {
            let mut bytes = head.into_bytes();
            bytes.extend_from_slice(body.as_bytes());
            out.push(Answer { conn, bytes });
        }
    }

    fn say(&self, conn: Conn, status: u16, message: &str, limits: &Limits, out: &mut Vec<Answer>) {
        self.tell(conn, status, &json!({"error": message}), limits, out);
    }

    /// Writes a reply and closes the connection. A write that makes no progress is given
    /// up; either way what the reply held of the budget is given back.
    fn write(&self, mut answer: Answer, limits: &Limits) {
        let stream = &mut answer.conn.stream;
        let _ = stream.set_write_timeout(Some(limits.write_wait));
        let sent = stream.write_all(&answer.bytes).and_then(|()| stream.flush());
        #[cfg(test)]
        if sent.is_ok() {
            self.written.fetch_add(answer.bytes.len(), Ordering::SeqCst);
        }
        self.writing.fetch_sub(answer.bytes.len(), Ordering::SeqCst);
        if sent.is_ok() {
            let_go(stream);
        }
    }

    fn queue(&self, job: Job) {
        self.jobs.lock().unwrap().push_back(job);
        self.wake.notify_one();
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
            open: self.open.load(Ordering::SeqCst),
        }
    }
}

/// Listens for the route at `address`, on a listener and threads of its own, and comes
/// back with where that is. From then on `/api/hello` offers the route.
pub fn listen(relay: Arc<Relay>, address: &str) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind(address)?;
    let at = listener.local_addr()?;
    *relay.link.at.lock().unwrap() = Some(at);
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
                let limits = &relay.config.link;
                match job {
                    Job::Write(answer) => relay.link.write(answer, limits),
                    Job::Call(conn) => {
                        relay.link.waiting.fetch_sub(1, Ordering::SeqCst);
                        let mut out = Vec::new();
                        call(&relay, conn, &mut out);
                        for answer in out {
                            relay.link.write(answer, limits);
                        }
                    }
                }
            }
        });
    }
    {
        let relay = relay.clone();
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
    let _ = std::thread::Builder::new().name("link-listener".into()).spawn(move || {
        let (link, limits) = (&relay.link, &relay.config.link);
        for stream in listener.incoming() {
            let Ok(stream) = stream else {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            // One connection too many is closed as it arrives. Nothing of it is read, and
            // nothing is written to it.
            if link.open.try_update(Ordering::SeqCst, Ordering::SeqCst, |open| (open < limits.connections).then_some(open + 1)).is_err() {
                continue;
            }
            let conn = Conn { peer: stream.peer_addr().ok().map(|peer| peer.ip()), stream, open: link.open.clone() };
            // With every thread busy and the queue full it is told so, in a few bytes that a
            // new connection always has room for, and closed. Nothing of it is read.
            if link.waiting.try_update(Ordering::SeqCst, Ordering::SeqCst, |waiting| (waiting < limits.queue).then_some(waiting + 1)).is_err() {
                let body = json!({"error": "This relay is busy. Try again in a moment."}).to_string();
                let mut conn = conn;
                if conn.stream.set_nonblocking(true).is_ok() {
                    let _ = conn.stream.write_all(format!("{}{body}", reply_head(503, body.len())).as_bytes());
                }
                continue;
            }
            link.queue(Job::Call(conn));
        }
    });
    Ok(at)
}

/// One request, from its head to whatever is to be written for it.
fn call(relay: &Relay, mut conn: Conn, out: &mut Vec<Answer>) {
    let (link, limits) = (&relay.link, &relay.config.link);
    let asked = match read_head(&mut conn, limits.head_wait) {
        Ok(asked) => asked,
        Err(Some((status, message))) => return link.say(conn, status, message, limits, out),
        Err(None) => return,
    };
    if let Err((status, message)) = host_key_ok(relay, &asked.key) {
        return link.say(conn, status, message, limits, out);
    }
    // A body is taken only if it says how long it is, and that is within what its call
    // allows. Otherwise the request is refused here, with none of its body read.
    if asked.post {
        let most = if asked.verb == "send" { limits.send_body } else { SMALL_BODY };
        match asked.length {
            None => return link.say(conn, 411, "Say how much is being sent.", limits, out),
            Some(length) if length > most => return link.say(conn, 413, "That's too much to send.", limits, out),
            Some(_) => {}
        }
    }
    match asked.verb.as_str() {
        "enter" => enter(relay, conn, asked, out),
        "poll" => poll(relay, conn, asked, out),
        "send" => send(relay, conn, asked, out),
        _ => leave(relay, conn, asked, out),
    }
}

/// Whether the host key shown is the right one. Wrong guesses are counted with the jams'.
fn host_key_ok(relay: &Relay, shown: &str) -> Result<(), (u16, &'static str)> {
    if same(shown.as_bytes(), relay.config.host_key.as_bytes()) {
        return Ok(());
    }
    if !relay.allow("wrong-host-key", 10.0, 10.0 / 60.0) {
        return Err((429, "Too many wrong host keys. Wait a minute."));
    }
    Err((401, "That host link isn't right for this relay."))
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
            let bytes = match place.answer(version, &roster, false, link, limits) {
                Some(bytes) => Some(bytes),
                // Its wait is over and its frames still can't be written: it is told to ask again.
                None if due => link.admit_poll(version, &roster, &[], true, false, limits),
                None => None,
            };
            match (bytes, place.poll.take()) {
                (Some(bytes), Some(parked)) => {
                    place.seen = now;
                    out.push(Answer { conn: parked.conn, bytes });
                }
                // Not even that can be written just now, and its wait is over: it is let go.
                (None, Some(parked)) if due => {
                    place.seen = now;
                    drop(parked);
                }
                (None, parked) => place.poll = parked,
                (Some(_), None) => {}
            }
        }
        let gone: Vec<usize> = (0..room.places.len()).filter(|&at| room.places[at].poll.is_none() && now.saturating_duration_since(room.places[at].seen) > relay.config.idle_after).collect();
        for at in gone.iter().rev() {
            room.remove(*at, link, limits, &mut out);
        }
        if !gone.is_empty() {
            room.changed(link, limits, &mut out);
        }
    }
    rooms.retain(|_, room| !room.places.is_empty());
    out
}

// ---- who is asking, and how things are spelled ---------------------------------------------

/// Who is asking, for counting: the connection's own address, unless the connection comes
/// from the proxy this relay was told to trust, in which case the last address in
/// `X-Forwarded-For`, which that proxy wrote. Nothing a client sends is believed otherwise.
fn caller(relay: &Relay, conn: &Conn, asked: &Asked) -> String {
    let address = match (conn.peer, relay.config.trusted_proxy) {
        (Some(own), Some(proxy)) if plain(own) == plain(proxy) => asked.forwarded.unwrap_or(own),
        (Some(own), _) => own,
        (None, _) => return String::new(),
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

fn number(query: &str, field: &str) -> u64 {
    query.split('&').filter_map(|pair| pair.split_once('=')).find(|(key, _)| *key == field).and_then(|(_, value)| value.parse().ok()).unwrap_or(0)
}

// ---- the four calls ------------------------------------------------------------------------

/// A device takes a place in a room. The place is its own choice of name, new for each run
/// of the app; one that is taken stays taken.
fn enter(relay: &Relay, mut conn: Conn, mut asked: Asked, out: &mut Vec<Answer>) {
    let (link, limits) = (&relay.link, &relay.config.link);
    let address = caller(relay, &conn, &asked);
    if !relay.allow(&format!("link-enter:{address}"), limits.enters, limits.enters / 60.0) {
        return link.say(conn, 429, "Too many arrivals from this address. Wait a moment.", limits, out);
    }
    let body = match read_body(&mut conn, &mut asked, limits.body_wait) {
        Ok(body) => body,
        Err(Some((status, message))) => return link.say(conn, status, message, limits, out),
        Err(None) => return,
    };
    let (name, place) = (asked.room.as_str(), body["place"].as_str().unwrap_or(""));
    if !is_room(name) || !is_place(place) {
        return link.say(conn, 400, "That isn't a room or a place in one.", limits, out);
    }
    let Some(token) = new_token() else { return link.say(conn, 503, "This relay can't make a token just now.", limits, out) };
    let mut rooms = link.rooms.lock().unwrap();
    if !rooms.contains_key(name) && rooms.len() >= limits.rooms {
        return link.say(conn, 503, "This relay has as many rooms as it allows.", limits, out);
    }
    let elsewhere = rooms.iter().filter(|(other, room)| other.as_str() != name && room.places.iter().any(|place| place.address == address)).count();
    if elsewhere >= limits.rooms_per_address && !rooms.get(name).is_some_and(|room| room.places.iter().any(|place| place.address == address)) {
        return link.say(conn, 503, "That's as many rooms as one address may be in.", limits, out);
    }
    match rooms.get(name).map(|room| (room.places.iter().any(|other| other.place == place), room.places.len())) {
        Some((true, _)) => return link.say(conn, 409, "That place is taken.", limits, out),
        Some((_, places)) if places >= limits.places => return link.say(conn, 503, "That's as many devices as a room holds.", limits, out),
        _ => {}
    }
    let room = rooms.entry(name.to_string()).or_default();
    room.places.push(Place { place: place.to_string(), token: token.clone(), address, seen: Instant::now(), poll: None, frames: VecDeque::new(), bytes: 0, next: 1, handed: 0 });
    room.changed(link, limits, out);
    link.tell(conn, 200, &json!({"token": token, "v": room.version, "places": room.roster()}), limits, out);
}

/// A device waits for a frame, or for a place to come or go.
fn poll(relay: &Relay, conn: Conn, asked: Asked, out: &mut Vec<Answer>) {
    let (link, limits) = (&relay.link, &relay.config.link);
    let (seen, after) = (number(&asked.query, "v"), number(&asked.query, "after"));
    let mut rooms = link.rooms.lock().unwrap();
    let Some(room) = rooms.get_mut(&asked.room) else { return link.say(conn, 404, "Enter the room first.", limits, out) };
    let Some(at) = holder(room, &asked.token) else { return link.say(conn, 404, "Enter the room first.", limits, out) };
    let (version, roster) = (room.version, room.roster());
    let freed = room.places[at].dealt_with(after);
    room.bytes -= freed;
    let place = &mut room.places[at];
    place.seen = Instant::now();
    if let Some(earlier) = place.poll.take()
        && let Some(bytes) = link.admit_poll(version, &roster, &[], !place.frames.is_empty(), true, limits)
    {
        out.push(Answer { conn: earlier.conn, bytes });
    }
    if (!place.frames.is_empty() || seen != version)
        && let Some(bytes) = place.answer(version, &roster, false, link, limits)
    {
        return out.push(Answer { conn, bytes });
    }
    place.poll = Some(Parked { conn, until: Instant::now() + relay.config.hold });
}

/// A device puts a frame in another's mailbox. The frame is marked with the sender's
/// place as this relay knows it from the token, not as the sender says.
fn send(relay: &Relay, mut conn: Conn, mut asked: Asked, out: &mut Vec<Answer>) {
    let (link, limits) = (&relay.link, &relay.config.link);
    let name = asked.room.clone();
    let from = {
        let rooms = link.rooms.lock().unwrap();
        rooms.get(&name).and_then(|room| holder(room, &asked.token).map(|at| room.places[at].place.clone()))
    };
    let Some(from) = from else { return link.say(conn, 404, "Enter the room first.", limits, out) };
    let address = caller(relay, &conn, &asked);
    if !relay.allow(&format!("link-send:{name}:{from}"), limits.sends.0, limits.sends.1) || !relay.allow(&format!("link-sends:{address}"), limits.sends_per_address, limits.sends_per_address) {
        return link.say(conn, 429, "Too much is being sent. Wait a moment.", limits, out);
    }
    // The body is read only now: its sender holds the host key and a place.
    let mut body = match read_body(&mut conn, &mut asked, limits.body_wait) {
        Ok(body) => body,
        Err(Some((status, message))) => return link.say(conn, status, message, limits, out),
        Err(None) => return,
    };
    let to = body["to"].as_str().unwrap_or("").to_string();
    let frame = match body["frame"].take() {
        Value::String(frame) if is_place(&to) && is_frame(&frame) => frame,
        _ => return link.say(conn, 400, "That isn't a frame for a place.", limits, out),
    };
    let mut rooms = link.rooms.lock().unwrap();
    let Some(room) = rooms.get_mut(&name) else { return link.say(conn, 404, "Enter the room first.", limits, out) };
    // The sender may have gone, or been given up on, while its body was read.
    if holder(room, &asked.token).is_none() {
        return link.say(conn, 404, "Enter the room first.", limits, out);
    }
    let Some(at) = room.places.iter().position(|place| place.place == to) else { return link.say(conn, 404, "There's no such place in the room.", limits, out) };
    let place = &room.places[at];
    if place.frames.len() >= limits.mailbox_frames || place.bytes + frame.len() > limits.mailbox_bytes || room.bytes + frame.len() > limits.room_bytes {
        return link.say(conn, 429, "That device has as much waiting as it may. Try again in a moment.", limits, out);
    }
    let (version, roster) = (room.version, room.roster());
    room.bytes += frame.len();
    let place = &mut room.places[at];
    place.bytes += frame.len();
    place.frames.push_back(Frame { seq: place.next, from, frame });
    place.next += 1;
    if place.poll.is_some()
        && let Some(bytes) = place.answer(version, &roster, false, link, limits)
        && let Some(parked) = place.poll.take()
    {
        place.seen = Instant::now();
        out.push(Answer { conn: parked.conn, bytes });
    }
    link.tell(conn, 200, &json!({"ok": true}), limits, out);
}

/// A device gives up its place: what was waiting for it is dropped.
fn leave(relay: &Relay, mut conn: Conn, mut asked: Asked, out: &mut Vec<Answer>) {
    let (link, limits) = (&relay.link, &relay.config.link);
    // (Whatever it sends with that is of no interest, and is not waited for long.)
    if read_body(&mut conn, &mut asked, limits.body_wait).is_err_and(|said| said.is_none()) {
        return;
    }
    let mut rooms = link.rooms.lock().unwrap();
    let left = rooms.get_mut(&asked.room).and_then(|room| {
        let at = holder(room, &asked.token)?;
        room.remove(at, link, limits, out);
        room.changed(link, limits, out);
        Some(room.places.is_empty())
    });
    match left {
        Some(emptied) => {
            if emptied {
                rooms.remove(&asked.room);
            }
            link.tell(conn, 200, &json!({"ok": true}), limits, out);
        }
        None => link.say(conn, 404, "Enter the room first.", limits, out),
    }
}

#[cfg(test)]
mod tests;
