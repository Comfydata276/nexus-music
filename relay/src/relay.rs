//! Jams, the people in them, and the requests that wait on them.
//!
//! Three kinds of request are held open until there's something to say: the host app's
//! wait for something to do, a guest's wait for the jam to change, and a guest's question
//! waiting for the host app's answer. None of them holds a thread; each is put away with a
//! time to give up by, and a sweeper answers the ones that have run out.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Cursor, Read};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tiny_http::{Header, Method, Request, Response, Server};

const PAGE: &str = include_str!("page.html");
const SCRIPT: &str = include_str!("app.js");
const STYLE: &str = include_str!("app.css");
const ICON: &[u8] = include_bytes!("../../assets/app-icon.png");

/// The most a guest may send in one request, in bytes.
const GUEST_BODY: usize = 8 * 1024;
/// The most the host app may send as the jam's state.
const STATE_BODY: usize = 512 * 1024;
/// The most one cover may weigh.
const COVER_BODY: usize = 1536 * 1024;
/// What a guest may ask the host app to do.
const ACTIONS: [&str; 6] = ["search", "add", "remove", "skip", "pause", "play"];
/// Things waiting for the host app, per jam, before more are turned away.
const MOST_WAITING: usize = 120;
/// What the host app hasn't collected yet, per jam, before the oldest is dropped.
const MOST_EVENTS: usize = 400;
/// Covers remembered per jam, counting the ones remembered as missing.
const MOST_COVERS: usize = 4000;

pub struct Config {
    /// What a copy of the app must show to start a jam here.
    pub host_key: String,
    pub max_jams: usize,
    pub max_guests: usize,
    /// How long a wait for news is held before it's answered with "nothing new".
    pub hold: Duration,
    /// How long a guest's question waits for the host app.
    pub ask_wait: Duration,
    pub cover_wait: Duration,
    /// How long the host app can go without asking before guests are told it's away...
    pub away_after: Duration,
    /// ...and before the jam is ended.
    pub end_after: Duration,
    /// How long a guest can go without asking before they're no longer listed.
    pub idle_after: Duration,
    /// Covers held per jam, in bytes.
    pub cover_budget: usize,
}

impl Config {
    pub fn new(host_key: String) -> Config {
        Config {
            host_key,
            max_jams: 5,
            max_guests: 50,
            hold: Duration::from_secs(20),
            ask_wait: Duration::from_secs(12),
            cover_wait: Duration::from_secs(12),
            away_after: Duration::from_secs(15),
            end_after: Duration::from_secs(30 * 60),
            idle_after: Duration::from_secs(45),
            cover_budget: 48 * 1024 * 1024,
        }
    }
}

type Reply = Response<Cursor<Vec<u8>>>;

/// Answers to send once the jams are no longer locked: writing to a slow connection
/// shouldn't hold everyone else up.
#[derive(Default)]
struct Outbox(Vec<(Request, Reply)>);

impl Outbox {
    fn push(&mut self, request: Request, reply: Reply) {
        self.0.push((request, reply));
    }

    fn send(self) {
        for (request, reply) in self.0 {
            let _ = request.respond(reply);
        }
    }
}

/// A request put away until there's an answer for it, or until `until`.
struct Parked {
    request: Request,
    until: Instant,
}

struct Guest {
    id: String,
    token: String,
    name: String,
    seen: Instant,
    /// Listed as being in the jam. A phone that has gone to sleep stops asking; its owner is
    /// unlisted until it asks again, and keeps their place meanwhile.
    here: bool,
    /// What they may still ask for before being told to slow down, and when that was worked out.
    allowance: f64,
    refilled: Instant,
}

/// Covers the host app has sent, so each is asked of it once.
#[derive(Default)]
struct Covers {
    /// `None` for a cover the host app said it doesn't have.
    held: HashMap<String, Option<(Arc<Vec<u8>>, &'static str)>>,
    order: VecDeque<String>,
    bytes: usize,
}

impl Covers {
    fn keep(&mut self, key: String, cover: Option<(Vec<u8>, &'static str)>, budget: usize) {
        let size = cover.as_ref().map_or(0, |(bytes, _)| bytes.len());
        if let Some(Some((old, _))) = self.held.remove(&key) {
            self.bytes -= old.len();
            self.order.retain(|other| *other != key);
        }
        while self.bytes + size > budget || self.order.len() >= MOST_COVERS {
            let Some(oldest) = self.order.pop_front() else { break };
            if let Some(Some((gone, _))) = self.held.remove(&oldest) {
                self.bytes -= gone.len();
            }
        }
        self.bytes += size;
        self.order.push_back(key.clone());
        self.held.insert(key, cover.map(|(bytes, kind)| (Arc::new(bytes), kind)));
    }
}

struct Jam {
    /// What the host app shows on every request after the first.
    secret: String,
    host: String,
    /// What's playing and what's queued, as the host app last described it, and when.
    state: Value,
    state_at: Instant,
    /// Goes up whenever a guest would see something different.
    version: u64,
    guests: Vec<Guest>,
    /// Tokens of guests the host removed.
    removed: HashSet<String>,
    /// What the host app has yet to be told, oldest first, each numbered by `seq`.
    events: VecDeque<Value>,
    seq: u64,
    /// The number of the newest event the host app has been sent.
    delivered: u64,
    asks: u64,
    host_seen: Instant,
    away: bool,
    /// The host app, waiting for something to do.
    poll: Option<Parked>,
    /// Guests waiting for the jam to change, by token.
    watchers: Vec<(String, Parked)>,
    /// Guests' questions waiting for the host app's answer, by the question's number.
    pending: HashMap<u64, Parked>,
    covers: Covers,
    cover_waiters: HashMap<String, Vec<Parked>>,
    started: Instant,
}

impl Jam {
    fn new(secret: String, host: String) -> Jam {
        let now = Instant::now();
        Jam {
            secret,
            host,
            state: Value::Null,
            state_at: now,
            version: 1,
            guests: Vec::new(),
            removed: HashSet::new(),
            events: VecDeque::new(),
            seq: 0,
            delivered: 0,
            asks: 0,
            host_seen: now,
            away: false,
            poll: None,
            watchers: Vec::new(),
            pending: HashMap::new(),
            covers: Covers::default(),
            cover_waiters: HashMap::new(),
            started: now,
        }
    }

    fn guest_list(&self) -> Value {
        Value::Array(self.guests.iter().filter(|guest| guest.here).map(|guest| json!({"id": guest.id, "name": guest.name})).collect())
    }

    /// The jam as one guest sees it.
    fn snapshot(&self, token: &str) -> Value {
        let you = self.guests.iter().find(|guest| guest.token == token).map(|guest| json!({"id": guest.id, "name": guest.name}));
        json!({
            "v": self.version,
            "host": self.host,
            "away": self.away,
            "age": self.state_at.elapsed().as_secs_f64(),
            "state": self.state,
            "guests": self.guest_list(),
            "you": you,
        })
    }

    /// Something a guest would see has changed: tell everyone who is waiting to hear.
    fn changed(&mut self, out: &mut Outbox) {
        self.version += 1;
        let now = Instant::now();
        for (token, parked) in std::mem::take(&mut self.watchers) {
            if let Some(guest) = self.guests.iter_mut().find(|guest| guest.token == token) {
                guest.seen = now;
            }
            out.push(parked.request, json_reply(200, &self.snapshot(&token)));
        }
    }

    /// Everything the host app hasn't collected, marked as sent.
    fn batch(&mut self) -> Value {
        self.delivered = self.seq;
        json!({"events": self.events, "guests": self.guest_list()})
    }

    /// Queues something for the host app, and hands it over at once if it's waiting.
    fn tell_host(&mut self, mut event: Value, out: &mut Outbox) {
        self.seq += 1;
        event["seq"] = json!(self.seq);
        self.events.push_back(event);
        while self.events.len() > MOST_EVENTS {
            self.events.pop_front();
        }
        if let Some(parked) = self.poll.take() {
            self.host_seen = Instant::now();
            let batch = self.batch();
            out.push(parked.request, json_reply(200, &batch));
        }
    }

    /// The host app has been heard from.
    fn host_here(&mut self, out: &mut Outbox) {
        self.host_seen = Instant::now();
        if self.away {
            self.away = false;
            self.changed(out);
        }
    }

    /// Answers whatever has waited long enough. Returns whether the jam is over.
    fn sweep(&mut self, config: &Config, now: Instant, out: &mut Outbox) -> bool {
        if let Some(parked) = self.poll.take_if(|parked| parked.until <= now) {
            self.host_seen = now;
            let batch = self.batch();
            out.push(parked.request, json_reply(200, &batch));
        }
        let quiet = now.saturating_duration_since(self.host_seen);
        if self.poll.is_none() && quiet > config.end_after || now.saturating_duration_since(self.started) > Duration::from_secs(24 * 3600) {
            return true;
        }
        let mut changed = false;
        let away = self.poll.is_none() && quiet > config.away_after;
        if away != self.away {
            self.away = away;
            changed = true;
        }
        // A wait that has run its course is answered with the jam as it stands.
        let mut waiting = Vec::new();
        for (token, parked) in std::mem::take(&mut self.watchers) {
            if parked.until > now {
                waiting.push((token, parked));
                continue;
            }
            if let Some(guest) = self.guests.iter_mut().find(|guest| guest.token == token) {
                guest.seen = now;
            }
            out.push(parked.request, json_reply(200, &self.snapshot(&token)));
        }
        self.watchers = waiting;
        let mut unlisted = false;
        for guest in &mut self.guests {
            let waiting = self.watchers.iter().any(|(token, _)| *token == guest.token);
            if guest.here && !waiting && now.saturating_duration_since(guest.seen) > config.idle_after {
                guest.here = false;
                unlisted = true;
            }
        }
        // A question the host app never answered. If it hasn't even been collected, it's
        // taken back, so that it isn't acted on after the guest was told it failed.
        let late: Vec<u64> = self.pending.iter().filter(|(_, parked)| parked.until <= now).map(|(id, _)| *id).collect();
        for id in late {
            if let Some(parked) = self.pending.remove(&id) {
                out.push(parked.request, error(504, "The host's app didn't answer. Try again."));
            }
            let delivered = self.delivered;
            self.events.retain(|event| event["id"].as_u64() != Some(id) || event["seq"].as_u64().unwrap_or(0) <= delivered);
        }
        for waiters in self.cover_waiters.values_mut() {
            let mut kept = Vec::new();
            for parked in std::mem::take(waiters) {
                if parked.until > now {
                    kept.push(parked);
                } else {
                    out.push(parked.request, error(404, "No cover."));
                }
            }
            *waiters = kept;
        }
        self.cover_waiters.retain(|_, waiters| !waiters.is_empty());
        if unlisted {
            self.tell_host(json!({"kind": "guests"}), out);
        }
        if changed || unlisted {
            self.changed(out);
        }
        false
    }

    /// The jam is over: everyone waiting is told so.
    fn close(mut self, out: &mut Outbox) {
        let ended = || error(404, "This jam has ended.");
        if let Some(parked) = self.poll.take() {
            out.push(parked.request, ended());
        }
        for (_, parked) in self.watchers.drain(..) {
            out.push(parked.request, ended());
        }
        for (_, parked) in self.pending.drain() {
            out.push(parked.request, ended());
        }
        for (_, waiters) in self.cover_waiters.drain() {
            for parked in waiters {
                out.push(parked.request, ended());
            }
        }
    }
}

/// Who may do how much, how often: each key has an allowance that refills over time.
#[derive(Default)]
struct Limits {
    buckets: HashMap<String, (f64, Instant)>,
}

impl Limits {
    fn allow(&mut self, key: &str, capacity: f64, per_second: f64) -> bool {
        let now = Instant::now();
        let (left, at) = self.buckets.get(key).copied().unwrap_or((capacity, now));
        let left = (left + now.saturating_duration_since(at).as_secs_f64() * per_second).min(capacity);
        let allowed = left >= 1.0;
        self.buckets.insert(key.to_string(), (if allowed { left - 1.0 } else { left }, now));
        allowed
    }

    fn forget_idle(&mut self) {
        let now = Instant::now();
        self.buckets.retain(|_, (_, at)| now.saturating_duration_since(*at) < Duration::from_secs(600));
    }
}

pub struct Relay {
    config: Config,
    jams: Mutex<HashMap<String, Jam>>,
    limits: Mutex<Limits>,
}

impl Relay {
    pub fn new(config: Config) -> Relay {
        Relay { config, jams: Mutex::new(HashMap::new()), limits: Mutex::new(Limits::default()) }
    }

    fn allow(&self, key: &str, capacity: f64, per_second: f64) -> bool {
        self.limits.lock().unwrap().allow(key, capacity, per_second)
    }

    /// Whether a request carries the host key. Wrong guesses are counted, across everyone,
    /// so the key can't be found by trying.
    fn host_key_ok(&self, request: &Request) -> Result<(), Reply> {
        let shown = bearer(request).unwrap_or_default();
        if same(shown.as_bytes(), self.config.host_key.as_bytes()) {
            return Ok(());
        }
        if !self.allow("wrong-host-key", 10.0, 10.0 / 60.0) {
            return Err(error(429, "Too many wrong host keys. Wait a minute."));
        }
        Err(error(401, "That host link isn't right for this relay."))
    }
}

/// Starts answering requests and comes back; the work goes on in other threads.
pub fn start(relay: Arc<Relay>, server: Arc<Server>, workers: usize) {
    for index in 0..workers.max(1) {
        let (relay, server) = (relay.clone(), server.clone());
        let _ = std::thread::Builder::new().name(format!("worker-{index}")).spawn(move || {
            loop {
                match server.recv() {
                    Ok(request) => handle(&relay, request),
                    Err(_) => std::thread::sleep(Duration::from_millis(50)),
                }
            }
        });
    }
    let _ = std::thread::Builder::new().name("sweeper".into()).spawn(move || {
        let mut rounds = 0u32;
        loop {
            std::thread::sleep(Duration::from_millis(250));
            let mut out = Outbox::default();
            let now = Instant::now();
            {
                let mut jams = relay.jams.lock().unwrap();
                let over: Vec<String> = jams.iter_mut().filter_map(|(id, jam)| jam.sweep(&relay.config, now, &mut out).then(|| id.clone())).collect();
                for id in over {
                    if let Some(jam) = jams.remove(&id) {
                        jam.close(&mut out);
                        println!("A jam ended: its host went quiet. {} running.", jams.len());
                    }
                }
            }
            out.send();
            rounds += 1;
            if rounds % 240 == 0 {
                relay.limits.lock().unwrap().forget_idle();
            }
        }
    });
}

/// Answers requests for as long as the process runs.
pub fn serve(relay: Arc<Relay>, server: Arc<Server>, workers: usize) -> ! {
    start(relay, server, workers);
    loop {
        std::thread::park();
    }
}

// ---- answering ---------------------------------------------------------------------------

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("a header this file wrote")
}

fn reply(status: u16, kind: &str, body: Vec<u8>) -> Reply {
    Response::from_data(body)
        .with_status_code(status)
        .with_header(header("Content-Type", kind))
        .with_header(header("X-Content-Type-Options", "nosniff"))
        .with_header(header("Referrer-Policy", "no-referrer"))
}

fn json_reply(status: u16, value: &Value) -> Reply {
    reply(status, "application/json", value.to_string().into_bytes()).with_header(header("Cache-Control", "no-store"))
}

fn error(status: u16, message: &str) -> Reply {
    json_reply(status, &json!({"error": message}))
}

/// Something that's part of the page. `root` is how the page gets from where it is to the
/// relay's top level, which may itself be under a path on a shared address.
fn asset(kind: &str, body: Vec<u8>) -> Reply {
    reply(200, kind, body).with_header(header("Cache-Control", "no-cache"))
}

fn page(root: &str) -> Reply {
    asset("text/html; charset=utf-8", PAGE.replace("{root}", root).into_bytes())
        .with_header(header(
            "Content-Security-Policy",
            "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
        ))
        .with_header(header("X-Frame-Options", "DENY"))
}

fn cover_reply(bytes: &[u8], kind: &str) -> Reply {
    reply(200, kind, bytes.to_vec()).with_header(header("Cache-Control", "private, max-age=604800, immutable"))
}

fn bearer(request: &Request) -> Option<String> {
    let header = request.headers().iter().find(|header| header.field.equiv("Authorization"))?;
    header.value.as_str().strip_prefix("Bearer ").map(|token| token.trim().to_string())
}

/// Compares without stopping at the first difference, so timing says nothing about a key.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |sum, (x, y)| sum | (x ^ y)) == 0
}

/// Who is asking, for counting purposes: the address a proxy in front says it's passing
/// on for, or else the connection's own.
fn caller(request: &Request) -> String {
    let forwarded = request.headers().iter().find(|header| header.field.equiv("X-Forwarded-For")).and_then(|header| header.value.as_str().rsplit(',').next().map(|address| address.trim().to_string()));
    forwarded.filter(|address| !address.is_empty()).or_else(|| request.remote_addr().map(|address| address.ip().to_string())).unwrap_or_default()
}

fn read_body(request: &mut Request, limit: usize) -> Result<Vec<u8>, Reply> {
    if request.body_length().is_some_and(|length| length > limit) {
        return Err(error(413, "That's too much to send."));
    }
    let mut body = Vec::new();
    match request.as_reader().take(limit as u64 + 1).read_to_end(&mut body) {
        Ok(_) if body.len() <= limit => Ok(body),
        Ok(_) => Err(error(413, "That's too much to send.")),
        Err(_) => Err(error(400, "The request was cut short.")),
    }
}

fn read_json(request: &mut Request, limit: usize) -> Result<Value, Reply> {
    let body = read_body(request, limit)?;
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&body).map_err(|_| error(400, "That wasn't understood."))
}

fn query_value<'a>(query: &'a str, name: &str) -> Option<&'a str> {
    query.split('&').filter_map(|pair| pair.split_once('=')).find(|(key, _)| *key == name).map(|(_, value)| value)
}

/// Letters and digits that can't be mistaken for each other, 5 bits apiece.
pub fn random_text(length: usize) -> String {
    const LETTERS: &[u8; 32] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut bytes = vec![0u8; length];
    getrandom::fill(&mut bytes).expect("the system's source of random numbers");
    bytes.iter().map(|byte| LETTERS[(byte & 31) as usize] as char).collect()
}

/// A name fit to show to other people: one line, no invisible characters, not too long.
fn clean_name(text: &str, otherwise: &str) -> String {
    let hidden = |c: char| c.is_control() || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}');
    let visible: String = text.chars().filter(|c| !hidden(*c)).collect();
    let name: String = visible.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(24).collect();
    let name = name.trim();
    if name.is_empty() { otherwise.to_string() } else { name.to_string() }
}

/// Cover ids as music servers write them: letters, digits and a little punctuation.
fn plain_id(text: &str) -> bool {
    !text.is_empty() && text.len() <= 128 && text.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~'))
}

// ---- requests ----------------------------------------------------------------------------

fn handle(relay: &Relay, request: Request) {
    let mut out = Outbox::default();
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let method = request.method().clone();
    match (&method, parts.as_slice()) {
        (Method::Get | Method::Head, []) => out.push(request, page("./")),
        (Method::Get | Method::Head, ["j", _]) => out.push(request, page("../")),
        (Method::Get, ["app.js"]) => out.push(request, asset("text/javascript; charset=utf-8", SCRIPT.as_bytes().to_vec())),
        (Method::Get, ["app.css"]) => out.push(request, asset("text/css; charset=utf-8", STYLE.as_bytes().to_vec())),
        (Method::Get, ["icon.png"]) => out.push(request, reply(200, "image/png", ICON.to_vec()).with_header(header("Cache-Control", "public, max-age=86400"))),
        (Method::Get | Method::Head, ["healthz"]) => out.push(request, reply(200, "text/plain", b"ok".to_vec())),
        (Method::Get, ["api", "hello"]) => hello(relay, request, &mut out),
        (Method::Post, ["api", "host", "start"]) => host_start(relay, request, &mut out),
        (Method::Get, ["api", "host", jam, "poll"]) => host_poll(relay, request, jam, query, &mut out),
        (Method::Post, ["api", "host", jam, "state"]) => host_state(relay, request, jam, &mut out),
        (Method::Post, ["api", "host", jam, "reply"]) => host_reply(relay, request, jam, &mut out),
        (Method::Post, ["api", "host", jam, "cover"]) => host_cover(relay, request, jam, query, &mut out),
        (Method::Post, ["api", "host", jam, "remove"]) => host_remove(relay, request, jam, &mut out),
        (Method::Post, ["api", "host", jam, "end"]) => host_end(relay, request, jam, &mut out),
        (Method::Get, ["api", "jam", jam]) => guest_peek(relay, request, jam, &mut out),
        (Method::Post, ["api", "jam", jam, "join"]) => guest_join(relay, request, jam, &mut out),
        (Method::Get, ["api", "jam", jam, "state"]) => guest_state(relay, request, jam, query, &mut out),
        (Method::Post, ["api", "jam", jam, "ask"]) => guest_ask(relay, request, jam, &mut out),
        (Method::Get, ["api", "jam", jam, "cover", size, cover]) => guest_cover(relay, request, jam, size, cover, &mut out),
        _ => out.push(request, error(404, "There's nothing at that address.")),
    }
    out.send();
}

/// Says this is a jam relay and, if a host key came with the question, whether it's right.
fn hello(relay: &Relay, request: Request, out: &mut Outbox) {
    let mut answer = json!({"nexus_jam": 1, "version": env!("CARGO_PKG_VERSION")});
    if bearer(&request).is_some() {
        if let Err(refusal) = relay.host_key_ok(&request) {
            return out.push(request, refusal);
        }
        answer["host"] = json!(true);
    }
    out.push(request, json_reply(200, &answer));
}

fn host_start(relay: &Relay, mut request: Request, out: &mut Outbox) {
    if let Err(refusal) = relay.host_key_ok(&request) {
        return out.push(request, refusal);
    }
    let body = match read_json(&mut request, GUEST_BODY) {
        Ok(body) => body,
        Err(refusal) => return out.push(request, refusal),
    };
    let host = clean_name(body["name"].as_str().unwrap_or(""), "Someone");
    let mut jams = relay.jams.lock().unwrap();
    if jams.len() >= relay.config.max_jams {
        return out.push(request, error(503, "This relay is already running as many jams as it allows."));
    }
    let (id, secret) = (random_text(22), random_text(40));
    jams.insert(id.clone(), Jam::new(secret.clone(), host));
    println!("A jam started. {} running.", jams.len());
    out.push(request, json_reply(200, &json!({"jam": id, "secret": secret})));
}

/// Finds the jam a host app's request is for and checks the request is from its host.
fn host_jam<'a>(jams: &'a mut HashMap<String, Jam>, id: &str, request: &Request) -> Result<&'a mut Jam, Reply> {
    let Some(jam) = jams.get_mut(id) else { return Err(error(404, "This jam has ended.")) };
    if !same(bearer(request).unwrap_or_default().as_bytes(), jam.secret.as_bytes()) {
        return Err(error(401, "That isn't this jam's host."));
    }
    Ok(jam)
}

fn host_poll(relay: &Relay, request: Request, id: &str, query: &str, out: &mut Outbox) {
    let after = query_value(query, "after").and_then(|text| text.parse::<u64>().ok()).unwrap_or(0);
    let mut jams = relay.jams.lock().unwrap();
    let jam = match host_jam(&mut jams, id, &request) {
        Ok(jam) => jam,
        Err(refusal) => return out.push(request, refusal),
    };
    jam.host_here(out);
    jam.events.retain(|event| event["seq"].as_u64().unwrap_or(0) > after);
    if let Some(earlier) = jam.poll.take() {
        out.push(earlier.request, json_reply(200, &json!({"events": [], "guests": jam.guest_list()})));
    }
    if jam.events.is_empty() {
        jam.poll = Some(Parked { request, until: Instant::now() + relay.config.hold });
    } else {
        let batch = jam.batch();
        out.push(request, json_reply(200, &batch));
    }
}

fn host_state(relay: &Relay, mut request: Request, id: &str, out: &mut Outbox) {
    let body = match read_json(&mut request, STATE_BODY) {
        Ok(body) => body,
        Err(refusal) => return out.push(request, refusal),
    };
    let mut jams = relay.jams.lock().unwrap();
    let jam = match host_jam(&mut jams, id, &request) {
        Ok(jam) => jam,
        Err(refusal) => return out.push(request, refusal),
    };
    jam.state = body;
    jam.state_at = Instant::now();
    jam.host_here(out);
    jam.changed(out);
    out.push(request, json_reply(200, &json!({"ok": true})));
}

fn host_reply(relay: &Relay, mut request: Request, id: &str, out: &mut Outbox) {
    let body = match read_json(&mut request, STATE_BODY) {
        Ok(body) => body,
        Err(refusal) => return out.push(request, refusal),
    };
    let mut jams = relay.jams.lock().unwrap();
    let jam = match host_jam(&mut jams, id, &request) {
        Ok(jam) => jam,
        Err(refusal) => return out.push(request, refusal),
    };
    let waiting = body["id"].as_u64().and_then(|ask| jam.pending.remove(&ask));
    let late = waiting.is_none();
    if let Some(parked) = waiting {
        let answer = if body["ok"].as_bool() == Some(true) {
            json_reply(200, &json!({"ok": true, "data": body["data"]}))
        } else {
            json_reply(200, &json!({"ok": false, "error": body["error"].as_str().unwrap_or("That couldn't be done.")}))
        };
        out.push(parked.request, answer);
    }
    out.push(request, json_reply(200, &json!({"ok": true, "late": late})));
}

fn host_cover(relay: &Relay, mut request: Request, id: &str, query: &str, out: &mut Outbox) {
    let (cover, size) = (query_value(query, "cover").unwrap_or(""), query_value(query, "size").unwrap_or(""));
    if !plain_id(cover) || !matches!(size, "s" | "l") {
        return out.push(request, error(400, "Which cover?"));
    }
    let missing = query_value(query, "missing").is_some();
    let kind = match request.headers().iter().find(|header| header.field.equiv("Content-Type")).map(|header| header.value.as_str().to_ascii_lowercase()) {
        Some(kind) if kind.starts_with("image/png") => "image/png",
        Some(kind) if kind.starts_with("image/webp") => "image/webp",
        _ => "image/jpeg",
    };
    let bytes = match read_body(&mut request, COVER_BODY) {
        Ok(bytes) => bytes,
        Err(refusal) => return out.push(request, refusal),
    };
    let mut jams = relay.jams.lock().unwrap();
    let jam = match host_jam(&mut jams, id, &request) {
        Ok(jam) => jam,
        Err(refusal) => return out.push(request, refusal),
    };
    let key = format!("{size}/{cover}");
    let found = (!missing && !bytes.is_empty()).then_some((bytes, kind));
    for parked in jam.cover_waiters.remove(&key).unwrap_or_default() {
        let answer = match &found {
            Some((bytes, kind)) => cover_reply(bytes, kind),
            None => error(404, "No cover."),
        };
        out.push(parked.request, answer);
    }
    jam.covers.keep(key, found, relay.config.cover_budget);
    out.push(request, json_reply(200, &json!({"ok": true})));
}

fn host_remove(relay: &Relay, mut request: Request, id: &str, out: &mut Outbox) {
    let body = match read_json(&mut request, GUEST_BODY) {
        Ok(body) => body,
        Err(refusal) => return out.push(request, refusal),
    };
    let mut jams = relay.jams.lock().unwrap();
    let jam = match host_jam(&mut jams, id, &request) {
        Ok(jam) => jam,
        Err(refusal) => return out.push(request, refusal),
    };
    let who = body["guest"].as_str().unwrap_or("");
    if let Some(at) = jam.guests.iter().position(|guest| guest.id == who) {
        let guest = jam.guests.remove(at);
        let mut waiting = Vec::new();
        for (token, parked) in std::mem::take(&mut jam.watchers) {
            if token == guest.token {
                out.push(parked.request, error(403, "The host removed you from this jam."));
            } else {
                waiting.push((token, parked));
            }
        }
        jam.watchers = waiting;
        jam.removed.insert(guest.token);
        jam.changed(out);
    }
    out.push(request, json_reply(200, &json!({"ok": true, "guests": jam.guest_list()})));
}

fn host_end(relay: &Relay, request: Request, id: &str, out: &mut Outbox) {
    let mut jams = relay.jams.lock().unwrap();
    if let Err(refusal) = host_jam(&mut jams, id, &request) {
        return out.push(request, refusal);
    }
    if let Some(jam) = jams.remove(id) {
        jam.close(out);
    }
    println!("A jam ended. {} running.", jams.len());
    out.push(request, json_reply(200, &json!({"ok": true})));
}

/// Whose jam this is, for the page to say before anyone has joined it.
fn guest_peek(relay: &Relay, request: Request, id: &str, out: &mut Outbox) {
    if !relay.allow(&format!("peek:{}", caller(&request)), 120.0, 2.0) {
        return out.push(request, error(429, "Slow down a little."));
    }
    let jams = relay.jams.lock().unwrap();
    match jams.get(id) {
        Some(jam) => out.push(request, json_reply(200, &json!({"host": jam.host, "guests": jam.guests.iter().filter(|guest| guest.here).count()}))),
        None => out.push(request, error(404, "This jam has ended.")),
    }
}

fn guest_join(relay: &Relay, mut request: Request, id: &str, out: &mut Outbox) {
    if !relay.allow(&format!("join:{}", caller(&request)), 20.0, 20.0 / 60.0) {
        return out.push(request, error(429, "Too many people joining from here at once. Try again in a minute."));
    }
    let body = match read_json(&mut request, GUEST_BODY) {
        Ok(body) => body,
        Err(refusal) => return out.push(request, refusal),
    };
    let mut jams = relay.jams.lock().unwrap();
    let Some(jam) = jams.get_mut(id) else { return out.push(request, error(404, "This jam has ended.")) };
    if jam.guests.len() >= relay.config.max_guests {
        // A place kept for someone who has gone quiet goes to someone who is here.
        let quietest = jam.guests.iter().enumerate().filter(|(_, guest)| !guest.here).min_by_key(|(_, guest)| guest.seen).map(|(at, _)| at);
        let Some(at) = quietest else { return out.push(request, error(503, "This jam is full.")) };
        jam.guests.remove(at);
    }
    let wanted = clean_name(body["name"].as_str().unwrap_or(""), "Guest");
    let taken = |name: &str| jam.guests.iter().any(|guest| guest.name.eq_ignore_ascii_case(name)) || jam.host.eq_ignore_ascii_case(name);
    let name = if taken(&wanted) { (2..).map(|n| format!("{wanted} {n}")).find(|name| !taken(name)).unwrap_or(wanted) } else { wanted };
    let now = Instant::now();
    let guest = Guest { id: random_text(8), token: random_text(32), name, seen: now, here: true, allowance: 40.0, refilled: now };
    let about = json!({"id": guest.id, "name": guest.name});
    let answer = json!({"token": guest.token, "guest": about, "host": jam.host});
    jam.guests.push(guest);
    jam.tell_host(json!({"kind": "joined", "guest": about}), out);
    jam.changed(out);
    out.push(request, json_reply(200, &answer));
}

/// Finds the guest a request is from, or says why it can't be answered.
fn guest_of<'a>(jam: &'a mut Jam, request: &Request) -> Result<&'a mut Guest, Reply> {
    let token = bearer(request).unwrap_or_default();
    if jam.removed.contains(&token) {
        return Err(error(403, "The host removed you from this jam."));
    }
    jam.guests.iter_mut().find(|guest| same(guest.token.as_bytes(), token.as_bytes())).ok_or_else(|| error(401, "Join the jam first."))
}

fn guest_state(relay: &Relay, request: Request, id: &str, query: &str, out: &mut Outbox) {
    let seen = query_value(query, "v").and_then(|text| text.parse::<u64>().ok()).unwrap_or(0);
    let mut jams = relay.jams.lock().unwrap();
    let Some(jam) = jams.get_mut(id) else { return out.push(request, error(404, "This jam has ended.")) };
    let (token, back) = match guest_of(jam, &request) {
        Ok(guest) => {
            guest.seen = Instant::now();
            let back = !guest.here;
            guest.here = true;
            (guest.token.clone(), back)
        }
        Err(refusal) => return out.push(request, refusal),
    };
    if back {
        jam.tell_host(json!({"kind": "guests"}), out);
        jam.changed(out);
    }
    // One wait per guest: a second one (another tab, a reload) replaces the first.
    if let Some(at) = jam.watchers.iter().position(|(other, _)| *other == token) {
        let (_, earlier) = jam.watchers.remove(at);
        out.push(earlier.request, json_reply(200, &jam.snapshot(&token)));
    }
    if seen != jam.version {
        return out.push(request, json_reply(200, &jam.snapshot(&token)));
    }
    jam.watchers.push((token, Parked { request, until: Instant::now() + relay.config.hold }));
}

fn guest_ask(relay: &Relay, mut request: Request, id: &str, out: &mut Outbox) {
    let body = match read_json(&mut request, GUEST_BODY) {
        Ok(body) => body,
        Err(refusal) => return out.push(request, refusal),
    };
    let action = body["action"].as_str().unwrap_or("").to_string();
    if !ACTIONS.contains(&action.as_str()) {
        return out.push(request, error(400, "That isn't something a guest can do."));
    }
    let mut jams = relay.jams.lock().unwrap();
    let Some(jam) = jams.get_mut(id) else { return out.push(request, error(404, "This jam has ended.")) };
    let about = match guest_of(jam, &request) {
        Ok(guest) => {
            let now = Instant::now();
            guest.seen = now;
            // Searching as you type sends many small questions; changing what plays costs more.
            let cost = if action == "search" { 1.0 } else { 4.0 };
            guest.allowance = (guest.allowance + now.saturating_duration_since(guest.refilled).as_secs_f64() * 4.0).min(40.0);
            guest.refilled = now;
            if guest.allowance < cost {
                return out.push(request, error(429, "Slow down a little."));
            }
            guest.allowance -= cost;
            json!({"id": guest.id, "name": guest.name})
        }
        Err(refusal) => return out.push(request, refusal),
    };
    if jam.away {
        return out.push(request, error(503, "The host's app isn't connected right now."));
    }
    if jam.pending.len() >= MOST_WAITING {
        return out.push(request, error(503, "The jam is busy. Try again in a moment."));
    }
    jam.asks += 1;
    let ask = jam.asks;
    jam.pending.insert(ask, Parked { request, until: Instant::now() + relay.config.ask_wait });
    jam.tell_host(json!({"kind": "ask", "id": ask, "guest": about, "action": action, "data": body["data"]}), out);
}

/// A cover, from what the host app has already sent or by asking it. Anyone with the
/// jam's link may ask: the link is what lets people in.
fn guest_cover(relay: &Relay, request: Request, id: &str, size: &str, cover: &str, out: &mut Outbox) {
    if !plain_id(cover) || !matches!(size, "s" | "l") {
        return out.push(request, error(404, "No cover."));
    }
    let mut jams = relay.jams.lock().unwrap();
    let Some(jam) = jams.get_mut(id) else { return out.push(request, error(404, "This jam has ended.")) };
    let key = format!("{size}/{cover}");
    match jam.covers.held.get(&key) {
        Some(Some((bytes, kind))) => return out.push(request, cover_reply(bytes, kind)),
        Some(None) => return out.push(request, error(404, "No cover.")),
        None => {}
    }
    let parked = Parked { request, until: Instant::now() + relay.config.cover_wait };
    if let Some(waiters) = jam.cover_waiters.get_mut(&key) {
        return waiters.push(parked);
    }
    // Each cover the relay doesn't hold yet is work for the host's app: one caller can't
    // ask for an endless run of them.
    let asked_too_much = !relay.allow(&format!("cover:{}", caller(&parked.request)), 600.0, 10.0);
    if jam.away || jam.cover_waiters.len() >= MOST_WAITING || asked_too_much {
        return out.push(parked.request, error(404, "No cover."));
    }
    jam.cover_waiters.insert(key, vec![parked]);
    jam.tell_host(json!({"kind": "cover", "cover": cover, "size": size}), out);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{SocketAddr, TcpStream};

    const KEY: &str = "a-host-key-for-the-tests";

    /// A relay on a port of its own, with short waits.
    fn relay(change: impl FnOnce(&mut Config)) -> SocketAddr {
        let mut config = Config::new(KEY.to_string());
        config.hold = Duration::from_millis(400);
        config.ask_wait = Duration::from_millis(600);
        config.cover_wait = Duration::from_millis(600);
        change(&mut config);
        let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
        let address = server.server_addr().to_ip().unwrap();
        start(Arc::new(Relay::new(config)), server, 4);
        address
    }

    /// One request, as HTTP/1.0 so the answer ends when the connection does.
    fn call(to: SocketAddr, method: &str, path: &str, token: &str, body: &[u8]) -> (u16, Vec<u8>) {
        let mut stream = TcpStream::connect(to).unwrap();
        let mut head = format!("{method} {path} HTTP/1.0\r\nHost: relay\r\nContent-Length: {}\r\n", body.len());
        if !token.is_empty() {
            head.push_str(&format!("Authorization: Bearer {token}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        let mut answer = Vec::new();
        stream.read_to_end(&mut answer).unwrap();
        let split = answer.windows(4).position(|window| window == b"\r\n\r\n").unwrap();
        let status = std::str::from_utf8(&answer[9..12]).unwrap().parse().unwrap();
        (status, answer[split + 4..].to_vec())
    }

    fn ask(to: SocketAddr, method: &str, path: &str, token: &str, body: Value) -> (u16, Value) {
        let sent = if body.is_null() { Vec::new() } else { body.to_string().into_bytes() };
        let (status, bytes) = call(to, method, path, token, &sent);
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// Starts a jam: its id and its host's secret.
    fn jam(to: SocketAddr) -> (String, String) {
        let (status, made) = ask(to, "POST", "/api/host/start", KEY, json!({"name": "Jez"}));
        assert_eq!(status, 200);
        (made["jam"].as_str().unwrap().to_string(), made["secret"].as_str().unwrap().to_string())
    }

    fn join(to: SocketAddr, jam: &str, name: &str) -> (String, String) {
        let (status, joined) = ask(to, "POST", &format!("/api/jam/{jam}/join"), "", json!({"name": name}));
        assert_eq!(status, 200, "{joined}");
        (joined["token"].as_str().unwrap().to_string(), joined["guest"]["id"].as_str().unwrap().to_string())
    }

    #[test]
    fn only_the_host_key_starts_a_jam() {
        let to = relay(|_| {});
        assert_eq!(ask(to, "GET", "/api/hello", "", Value::Null).1["nexus_jam"], 1);
        assert_eq!(ask(to, "GET", "/api/hello", KEY, Value::Null).1["host"], true);
        assert_eq!(ask(to, "GET", "/api/hello", "not-the-key", Value::Null).0, 401);
        assert_eq!(ask(to, "POST", "/api/host/start", "not-the-key", json!({})).0, 401);
        let (id, secret) = jam(to);
        assert_eq!(id.len(), 22);
        // The jam's own secret, not the host key, is what runs it from then on.
        assert_eq!(ask(to, "GET", &format!("/api/host/{id}/poll"), KEY, Value::Null).0, 401);
        assert_eq!(ask(to, "POST", &format!("/api/host/{id}/end"), &secret, json!({})).0, 200);
        assert_eq!(ask(to, "GET", &format!("/api/jam/{id}"), "", Value::Null).0, 404);
    }

    #[test]
    fn wrong_host_keys_are_counted() {
        let to = relay(|_| {});
        let refused: Vec<u16> = (0..14).map(|_| ask(to, "GET", "/api/hello", "guess", Value::Null).0).collect();
        assert!(refused[..10].iter().all(|status| *status == 401), "{refused:?}");
        assert!(refused[11..].iter().all(|status| *status == 429), "{refused:?}");
    }

    #[test]
    fn a_guest_joins_and_sees_what_is_playing() {
        let to = relay(|_| {});
        let (id, secret) = jam(to);
        assert_eq!(ask(to, "GET", &format!("/api/jam/{id}"), "", Value::Null).1["host"], "Jez");
        let (token, guest) = join(to, &id, "  Sam\u{202E}  the \n Second and a name that goes on and on ");
        // The host app hears who joined, by a name that's been tidied.
        let (_, told) = ask(to, "GET", &format!("/api/host/{id}/poll?after=0"), &secret, Value::Null);
        assert_eq!(told["events"][0]["kind"], "joined");
        assert_eq!(told["events"][0]["guest"]["id"], guest.as_str());
        assert_eq!(told["events"][0]["guest"]["name"], "Sam the Second and a nam");
        assert_eq!(told["guests"].as_array().unwrap().len(), 1);
        // The state the host app sends is what the guest gets, with how old it is.
        let state = json!({"playing": true, "position": 12.5, "now": {"id": "s1", "title": "Song"}, "queue": []});
        assert_eq!(ask(to, "POST", &format!("/api/host/{id}/state"), &secret, state.clone()).0, 200);
        let (status, seen) = ask(to, "GET", &format!("/api/jam/{id}/state?v=0"), &token, Value::Null);
        assert_eq!(status, 200);
        assert_eq!(seen["state"], state);
        assert_eq!(seen["you"]["id"], guest.as_str());
        assert!(seen["age"].as_f64().unwrap() < 5.0);
        // Asking again with the same version waits, and is woken by a change.
        let version = seen["v"].as_u64().unwrap();
        let (again, path) = (token.clone(), format!("/api/jam/{id}/state?v={version}"));
        let waiting = std::thread::spawn(move || ask(to, "GET", &path, &again, Value::Null));
        std::thread::sleep(Duration::from_millis(100));
        ask(to, "POST", &format!("/api/host/{id}/state"), &secret, json!({"playing": false}));
        let (_, woken) = waiting.join().unwrap();
        assert_eq!(woken["state"]["playing"], false);
        assert!(woken["v"].as_u64().unwrap() > version);
        // With nothing new, the wait ends by itself at the same version.
        let version = woken["v"].as_u64().unwrap();
        let (_, same) = ask(to, "GET", &format!("/api/jam/{id}/state?v={version}"), &token, Value::Null);
        assert_eq!(same["v"].as_u64().unwrap(), version);
        // Without a token there's nothing to see.
        assert_eq!(ask(to, "GET", &format!("/api/jam/{id}/state?v=0"), "", Value::Null).0, 401);
    }

    #[test]
    fn a_guest_s_question_reaches_the_host_and_its_answer_comes_back() {
        let to = relay(|_| {});
        let (id, secret) = jam(to);
        let (token, guest) = join(to, &id, "Sam");
        let (path, asker) = (format!("/api/jam/{id}/ask"), token.clone());
        let asking = std::thread::spawn(move || ask(to, "POST", &path, &asker, json!({"action": "search", "data": {"q": "blue"}})));
        let mut after = 0;
        let question = loop {
            let (_, told) = ask(to, "GET", &format!("/api/host/{id}/poll?after={after}"), &secret, Value::Null);
            let events = told["events"].as_array().unwrap().clone();
            if let Some(event) = events.iter().find(|event| event["kind"] == "ask") {
                break event.clone();
            }
            after = events.last().map_or(after, |event| event["seq"].as_u64().unwrap());
        };
        assert_eq!(question["action"], "search");
        assert_eq!(question["data"]["q"], "blue");
        assert_eq!(question["guest"]["id"], guest.as_str());
        let answer = json!({"id": question["id"], "ok": true, "data": {"songs": [{"id": "s9"}]}});
        assert_eq!(ask(to, "POST", &format!("/api/host/{id}/reply"), &secret, answer).1["late"], false);
        let (status, got) = asking.join().unwrap();
        assert_eq!(status, 200);
        assert_eq!(got["data"]["songs"][0]["id"], "s9");
        // A refusal from the host app comes back as one.
        let (path, asker) = (format!("/api/jam/{id}/ask"), token.clone());
        let asking = std::thread::spawn(move || ask(to, "POST", &path, &asker, json!({"action": "skip"})));
        let (_, told) = ask(to, "GET", &format!("/api/host/{id}/poll?after={}", question["seq"]), &secret, Value::Null);
        let refusal = json!({"id": told["events"][0]["id"], "ok": false, "error": "The host is choosing what plays."});
        ask(to, "POST", &format!("/api/host/{id}/reply"), &secret, refusal);
        let (_, got) = asking.join().unwrap();
        assert_eq!(got["ok"], false);
        assert_eq!(got["error"], "The host is choosing what plays.");
        // Only what a guest may do is passed on at all.
        assert_eq!(ask(to, "POST", &format!("/api/jam/{id}/ask"), &token, json!({"action": "end"})).0, 400);
    }

    #[test]
    fn a_question_nobody_answers_is_given_up_on_and_taken_back() {
        let to = relay(|_| {});
        let (id, secret) = jam(to);
        let (token, _) = join(to, &id, "Sam");
        ask(to, "GET", &format!("/api/host/{id}/poll?after=0"), &secret, Value::Null);
        let (status, got) = ask(to, "POST", &format!("/api/jam/{id}/ask"), &token, json!({"action": "add", "data": {"id": "s1"}}));
        assert_eq!(status, 504, "{got}");
        // The host app wasn't there to collect it, so it never hears of it.
        let (_, told) = ask(to, "GET", &format!("/api/host/{id}/poll?after=1"), &secret, Value::Null);
        assert!(told["events"].as_array().unwrap().iter().all(|event| event["kind"] != "ask"), "{told}");
    }

    #[test]
    fn a_cover_is_asked_of_the_host_once() {
        let to = relay(|_| {});
        let (id, secret) = jam(to);
        let path = format!("/api/jam/{id}/cover/s/al-1_abc");
        let wanting = { let path = path.clone(); std::thread::spawn(move || call(to, "GET", &path, "", b"")) };
        let (_, told) = ask(to, "GET", &format!("/api/host/{id}/poll?after=0"), &secret, Value::Null);
        assert_eq!(told["events"][0]["kind"], "cover");
        assert_eq!(told["events"][0]["cover"], "al-1_abc");
        assert_eq!(told["events"][0]["size"], "s");
        assert_eq!(call(to, "POST", &format!("/api/host/{id}/cover?cover=al-1_abc&size=s"), &secret, b"picture").0, 200);
        assert_eq!(wanting.join().unwrap(), (200, b"picture".to_vec()));
        // The second time it comes from here, and the host app isn't asked.
        assert_eq!(call(to, "GET", &path, "", b""), (200, b"picture".to_vec()));
        let (_, told) = ask(to, "GET", &format!("/api/host/{id}/poll?after=1"), &secret, Value::Null);
        assert!(told["events"].as_array().unwrap().is_empty(), "{told}");
        // One the host app doesn't have is remembered as missing.
        assert_eq!(call(to, "POST", &format!("/api/host/{id}/cover?cover=al-2&size=l&missing=1"), &secret, b"").0, 200);
        assert_eq!(call(to, "GET", &format!("/api/jam/{id}/cover/l/al-2"), "", b"").0, 404);
        // Nothing that isn't a plain id gets as far as the host app.
        assert_eq!(call(to, "GET", &format!("/api/jam/{id}/cover/s/..%2Fetc"), "", b"").0, 404);
        assert_eq!(call(to, "GET", &format!("/api/jam/{id}/cover/huge/al-1_abc"), "", b"").0, 404);
    }

    #[test]
    fn a_removed_guest_is_out_and_an_ended_jam_is_gone() {
        let to = relay(|_| {});
        let (id, secret) = jam(to);
        let (sam, sam_id) = join(to, &id, "Sam");
        let (alex, _) = join(to, &id, "sam");
        // Two guests don't get the same name.
        let (_, seen) = ask(to, "GET", &format!("/api/jam/{id}/state?v=0"), &alex, Value::Null);
        assert_eq!(seen["you"]["name"], "sam 2");
        assert_eq!(ask(to, "POST", &format!("/api/host/{id}/remove"), &secret, json!({"guest": sam_id})).0, 200);
        assert_eq!(ask(to, "GET", &format!("/api/jam/{id}/state?v=0"), &sam, Value::Null).0, 403);
        assert_eq!(ask(to, "POST", &format!("/api/jam/{id}/ask"), &sam, json!({"action": "skip"})).0, 403);
        let (_, seen) = ask(to, "GET", &format!("/api/jam/{id}/state?v=0"), &alex, Value::Null);
        assert_eq!(seen["guests"].as_array().unwrap().len(), 1);
        // Ending the jam wakes whoever is waiting, with the news.
        let version = seen["v"].as_u64().unwrap();
        let (path, waiter) = (format!("/api/jam/{id}/state?v={version}"), alex.clone());
        let waiting = std::thread::spawn(move || ask(to, "GET", &path, &waiter, Value::Null));
        std::thread::sleep(Duration::from_millis(100));
        ask(to, "POST", &format!("/api/host/{id}/end"), &secret, json!({}));
        assert_eq!(waiting.join().unwrap().0, 404);
        assert_eq!(ask(to, "POST", &format!("/api/jam/{id}/join"), "", json!({"name": "Late"})).0, 404);
    }

    #[test]
    fn guests_are_told_when_the_host_goes_quiet_and_the_jam_ends_if_it_stays_away() {
        let to = relay(|config| {
            config.away_after = Duration::from_millis(300);
            config.end_after = Duration::from_millis(1500);
        });
        let (id, secret) = jam(to);
        let (token, _) = join(to, &id, "Sam");
        ask(to, "GET", &format!("/api/host/{id}/poll?after=9"), &secret, Value::Null);
        std::thread::sleep(Duration::from_millis(700));
        let (_, seen) = ask(to, "GET", &format!("/api/jam/{id}/state?v=0"), &token, Value::Null);
        assert_eq!(seen["away"], true);
        assert_eq!(ask(to, "POST", &format!("/api/jam/{id}/ask"), &token, json!({"action": "skip"})).0, 503);
        // Back again: guests see that too.
        ask(to, "POST", &format!("/api/host/{id}/state"), &secret, json!({"playing": true}));
        let (_, seen) = ask(to, "GET", &format!("/api/jam/{id}/state?v=0"), &token, Value::Null);
        assert_eq!(seen["away"], false);
        std::thread::sleep(Duration::from_millis(2200));
        assert_eq!(ask(to, "GET", &format!("/api/jam/{id}"), "", Value::Null).0, 404);
    }

    #[test]
    fn a_guest_who_stops_asking_is_unlisted_but_keeps_their_place() {
        let to = relay(|config| config.idle_after = Duration::from_millis(300));
        let (id, secret) = jam(to);
        let (token, _) = join(to, &id, "Sam");
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(ask(to, "GET", &format!("/api/jam/{id}"), "", Value::Null).1["guests"], 0);
        let (_, told) = ask(to, "GET", &format!("/api/host/{id}/poll?after=0"), &secret, Value::Null);
        assert!(told["events"].as_array().unwrap().iter().any(|event| event["kind"] == "guests"), "{told}");
        assert!(told["guests"].as_array().unwrap().is_empty());
        // Their phone wakes up: the same token still works, and they're listed again.
        let (status, seen) = ask(to, "GET", &format!("/api/jam/{id}/state?v=0"), &token, Value::Null);
        assert_eq!(status, 200);
        assert_eq!(seen["guests"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_full_jam_gives_a_quiet_guest_s_place_to_a_new_one() {
        let to = relay(|config| {
            config.max_guests = 2;
            config.idle_after = Duration::from_millis(300);
        });
        let (id, _) = jam(to);
        let (first, _) = join(to, &id, "First");
        let (second, _) = join(to, &id, "Second");
        // Both here: there's no room.
        assert_eq!(ask(to, "POST", &format!("/api/jam/{id}/join"), "", json!({"name": "Third"})).0, 503);
        // The first keeps asking; the second goes quiet and loses their place.
        for _ in 0..4 {
            std::thread::sleep(Duration::from_millis(200));
            ask(to, "GET", &format!("/api/jam/{id}/state?v=0"), &first, Value::Null);
        }
        join(to, &id, "Third");
        assert_eq!(ask(to, "GET", &format!("/api/jam/{id}/state?v=0"), &first, Value::Null).0, 200);
        assert_eq!(ask(to, "GET", &format!("/api/jam/{id}/state?v=0"), &second, Value::Null).0, 401);
    }

    #[test]
    fn the_page_and_its_parts_are_served() {
        let to = relay(|_| {});
        let (status, body) = call(to, "GET", "/j/anything", "", b"");
        assert_eq!(status, 200);
        let body = String::from_utf8(body).unwrap();
        assert!(body.contains("../app.js") && !body.contains("{root}"), "{body}");
        assert!(String::from_utf8(call(to, "GET", "/", "", b"").1).unwrap().contains("./app.js"));
        assert_eq!(call(to, "GET", "/app.js", "", b"").0, 200);
        assert_eq!(call(to, "GET", "/app.css", "", b"").0, 200);
        assert_eq!(call(to, "GET", "/icon.png", "", b"").0, 200);
        assert_eq!(call(to, "GET", "/healthz", "", b""), (200, b"ok".to_vec()));
        assert_eq!(call(to, "GET", "/api/nothing", "", b"").0, 404);
    }

    #[test]
    fn tidies_names_and_ids() {
        assert_eq!(clean_name("  A  B ", "Guest"), "A B");
        assert_eq!(clean_name("\u{200B}\n", "Guest"), "Guest");
        assert!(plain_id("al-2Dh_6731ab2c"));
        assert!(!plain_id("../x") && !plain_id("") && !plain_id("a b"));
        assert_eq!(random_text(22).len(), 22);
        assert_ne!(random_text(22), random_text(22));
    }
}
