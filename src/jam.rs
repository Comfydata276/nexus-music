//! Jams: one queue that several people add to.
//!
//! The music plays on the host's PC. Guests see what's playing and what's queued, search
//! the host's library, add songs, and (if the host allows) pause and skip. A guest can be
//! this app on another PC or a page in a browser; both talk to a small relay service, and
//! so does the host, because the host's PC can't be reached from outside its own network.
//!
//! Hosting: the app starts a jam at the relay with its host key, then keeps one request
//! open that the relay answers whenever a guest wants something. Searches and covers are
//! answered from worker threads; anything that changes what plays is done on the UI thread.
//! The relay is told what's playing whenever that changes.
//!
//! Joining: the app joins with the jam's link, keeps one request open that the relay
//! answers whenever the jam changes, and sends what the guest asks for.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::config;
use crate::controller::{Controller, Route, clock, post, with};
use crate::subsonic::{Client, Song};
use crate::ui;

/// How much of the queue guests are shown.
const SHOWN: usize = 100;
/// How many songs a guest's search brings back.
const FOUND: usize = 40;
/// The most songs one request may add to a jam's queue.
const MOST_ADDED: usize = 50;
/// The most songs handed from one device to another with a queue.
const MOST_HANDED: usize = 3000;
/// How long to leave it before trying again to open this device's session at the relay.
const TRY_AGAIN: Duration = Duration::from_secs(30);
/// The relay is told what's playing at least this often, changed or not.
const RETELL: Duration = Duration::from_secs(30);

// ---- the relay ---------------------------------------------------------------------------

/// A connection to a relay.
pub struct Line {
    base: String,
    agent: ureq::Agent,
}

impl Line {
    fn new(base: &str) -> Line {
        Line { base: base.trim_end_matches('/').to_string(), agent: crate::subsonic::make_agent() }
    }

    fn read(response: ureq::http::Response<ureq::Body>) -> (u16, Value) {
        let status = response.status().as_u16();
        // An answer that isn't JSON (a proxy's error page, say) is read as nothing.
        (status, serde_json::from_reader(response.into_body().into_reader()).unwrap_or(Value::Null))
    }

    /// `wait` is how long the relay may take: it holds some requests open on purpose.
    fn get(&self, path: &str, pass: &str, wait: Duration) -> Result<(u16, Value), String> {
        let mut request = self.agent.get(format!("{}/{path}", self.base));
        if !pass.is_empty() {
            request = request.header("Authorization", &format!("Bearer {pass}"));
        }
        request.config().timeout_global(Some(wait)).timeout_recv_response(Some(wait)).build().call().map(Line::read).map_err(|_| unreachable())
    }

    fn post(&self, path: &str, pass: &str, body: &Value) -> Result<(u16, Value), String> {
        self.send(path, pass, "application/json", body.to_string().as_bytes(), Duration::from_secs(20))
    }

    fn send(&self, path: &str, pass: &str, kind: &str, body: &[u8], wait: Duration) -> Result<(u16, Value), String> {
        let mut request = self.agent.post(format!("{}/{path}", self.base)).header("Content-Type", kind);
        if !pass.is_empty() {
            request = request.header("Authorization", &format!("Bearer {pass}"));
        }
        request.config().timeout_global(Some(wait)).build().send(body).map(Line::read).map_err(|_| unreachable())
    }
}

fn unreachable() -> String {
    "Can't reach the jam relay. Check the link and your connection.".to_string()
}

/// What the relay said went wrong, or `otherwise` if it didn't say.
fn reason(body: &Value, otherwise: &str) -> String {
    body["error"].as_str().filter(|text| !text.is_empty()).unwrap_or(otherwise).to_string()
}

/// A relay's address from something pasted: `https://` is assumed, and plain HTTP is only
/// taken for somewhere on the home network, since a key travels with every request.
fn address(text: &str) -> Result<String, String> {
    let text = text.trim().trim_end_matches('/');
    if text.is_empty() || text.contains(char::is_whitespace) {
        return Err("That doesn't look like a link.".into());
    }
    let whole = if text.starts_with("https://") || text.starts_with("http://") { text.to_string() } else { format!("https://{text}") };
    if crate::subsonic::unencrypted_beyond_home(&whole) {
        return Err("That link starts with http://, which others on the way could read. Use the https:// one.".into());
    }
    let host = whole.split_once("://").map_or("", |(_, rest)| rest).split('/').next().unwrap_or("");
    if host.is_empty() {
        return Err("That doesn't look like a link.".into());
    }
    Ok(whole)
}

/// A host link is the relay's address with the host key after a `#`:
/// `https://jam.example.org/#key`. Returns the address and the key.
pub fn parse_host_link(text: &str) -> Result<(String, String), String> {
    // The relay's whole log line may have been pasted: the link is the part with the # in it.
    let text = text.split_whitespace().find(|word| word.contains('#')).unwrap_or(text.trim());
    let Some((before, key)) = text.split_once('#') else {
        return Err("A host link ends with # and a key. Copy the whole link from your relay's log.".into());
    };
    let key = key.trim();
    if key.len() < 16 || key.contains(char::is_whitespace) {
        return Err("The key at the end of that link is too short. Copy the whole link from your relay's log.".into());
    }
    Ok((address(before)?, key.to_string()))
}

/// A jam's link is the relay's address, then `/j/` and the jam: `https://jam.example.org/j/abc`.
/// Returns the address and the jam.
pub fn parse_jam_link(text: &str) -> Result<(String, String), String> {
    let text = text.trim();
    let text = text.split(['?', '#']).next().unwrap_or(text).trim_end_matches('/');
    let Some((before, id)) = text.rsplit_once("/j/") else {
        return Err("That isn't a jam link. One looks like https://jam.example.org/j/abc123.".into());
    };
    if id.is_empty() || id.len() > 64 || !id.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err("That isn't a jam link. One looks like https://jam.example.org/j/abc123.".into());
    }
    Ok((address(before)?, id.to_string()))
}

/// The part of an address people recognise: `jam.example.org`.
fn host_of(address: &str) -> &str {
    address.split_once("://").map_or(address, |(_, rest)| rest).split('/').next().unwrap_or(address)
}

// ---- what's passed about -----------------------------------------------------------------

fn song_json(song: &Song) -> Value {
    json!({
        "id": song.id,
        "title": song.title,
        "artist": song.artist_label(),
        "album": song.album,
        "cover": song.cover_art,
        "duration": song.duration,
    })
}

/// Songs in the library matching what a guest typed, best first.
fn find_songs(query: &str, client: Option<&Client>) -> Vec<Value> {
    let query: String = query.trim().chars().take(100).collect();
    if query.is_empty() {
        return Vec::new();
    }
    let songs = match crate::search::current() {
        Some(index) => index.search(&query, &crate::search::is_liked, 0, 0, FOUND).songs,
        // The copy of the library isn't ready yet: ask the server.
        None => client.and_then(|client| client.search(&query, 0, 0, FOUND as u32).ok()).map(|found| found.songs).unwrap_or_default(),
    };
    songs.iter().filter(|song| song.radio_url.is_empty()).map(song_json).collect()
}

/// A song by its id: from the copy of the library on this PC, or else from the server.
fn find_song(id: &str, client: Option<&Client>) -> Option<Song> {
    if id.is_empty() || id.len() > 128 {
        return None;
    }
    let kept = crate::search::current().and_then(|index| index.library.songs.iter().find(|song| song.id == id).cloned());
    kept.or_else(|| client.and_then(|client| client.song(id).ok())).filter(|song| song.id == id)
}

/// A cover made ready to send to guests: small ones are cut down to what a row needs, and
/// large ones are passed on as they are unless they're heavy.
fn cover_for_guests(bytes: Vec<u8>, large: bool) -> Option<(Vec<u8>, &'static str)> {
    let kind = match image::guess_format(&bytes).ok()? {
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::WebP => "image/webp",
        _ => "",
    };
    if large && !kind.is_empty() && bytes.len() <= 300 * 1024 {
        return Some((bytes, kind));
    }
    let edge = if large { 640 } else { 128 };
    let picture = image::load_from_memory(&bytes).ok()?;
    let picture = if picture.width().max(picture.height()) > edge { picture.thumbnail(edge, edge) } else { picture };
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, if large { 86 } else { 80 }).encode_image(&picture.to_rgb8()).ok()?;
    Some((out, "image/jpeg"))
}

/// A link as a QR code, dark on white with a margin, for a phone's camera.
fn qr_image(text: &str) -> slint::Image {
    let Ok(code) = qrcode::QrCode::new(text.as_bytes()) else { return slint::Image::default() };
    let (cells, colours) = (code.width(), code.to_colors());
    let (scale, margin) = (6usize, 3usize);
    let edge = (cells + 2 * margin) * scale;
    let mut pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(edge as u32, edge as u32);
    let bytes = pixels.make_mut_bytes();
    bytes.fill(0xff);
    for (index, colour) in colours.iter().enumerate() {
        if *colour != qrcode::Color::Dark {
            continue;
        }
        let (column, line) = (index % cells + margin, index / cells + margin);
        for y in line * scale..(line + 1) * scale {
            for x in column * scale..(column + 1) * scale {
                let at = (y * edge + x) * 4;
                bytes[at..at + 3].fill(0x12);
            }
        }
    }
    slint::Image::from_rgba8(pixels)
}

// ---- state -------------------------------------------------------------------------------

/// Something done to playback. When this PC has joined a jam it isn't done here: it's
/// asked of the jam's host, whose queue and player this PC is then showing.
pub enum Remote {
    Toggle,
    Play(bool),
    Next,
    Previous,
    Seek(f64),
    /// Take out the entry this many places after the current track.
    Remove(usize),
    /// Move an entry from one place after the current track to another, into "Next in
    /// queue" or out of it.
    Move(usize, usize, bool),
    /// Empty "Next in queue", or else the rest of what's to come.
    Clear(bool),
    /// Play the entry this many places after the current track.
    Jump(usize),
    Shuffle,
    Repeat,
    /// Only between a person's own devices: a guest in a jam turns their own volume.
    Volume(f32),
}

/// Something for the thread that talks to the relay on the host's behalf.
enum Out {
    /// What's playing and queued. Only the newest waiting to go is sent.
    State(Value),
    /// The answer to something a guest asked for.
    Reply(Value),
    /// Take this guest out of the jam.
    Remove(String),
    /// End the jam, and say on this channel when the relay has been told.
    End(mpsc::Sender<()>),
}

/// A jam this PC is hosting.
pub struct Hosting {
    serial: u64,
    /// The session's name at the relay.
    id: String,
    /// It's a jam: friends have been given its link. Otherwise it's only there for this
    /// person's other devices to steer this one through, and nothing on screen says "jam".
    public: bool,
    /// Guests that proved to be this person's own devices, by their guest id.
    owners: HashSet<String>,
    /// What friends open to join.
    link: String,
    /// Who's here: each guest's id and name.
    guests: Vec<(String, String)>,
    /// Who added which of the songs still to come: song id to the guest's id and name.
    added: HashMap<String, (String, String)>,
    out: mpsc::Sender<Out>,
    stop: Arc<AtomicBool>,
    /// What the relay was last told, to tell a change from more of the same: a fingerprint
    /// of it, when it was sent, and where the song was and whether it was playing.
    told: u64,
    told_at: Instant,
    told_position: f64,
    told_playing: bool,
    /// Why the relay can't be reached just now; empty when it can.
    trouble: String,
}

/// A jam this PC has joined.
pub struct Joined {
    serial: u64,
    /// Set when what's joined is one of this person's own devices, not a friend's jam: the
    /// word that proves this copy is theirs, which gives it the run of the other device.
    owner: Option<String>,
    /// That device's id and name.
    device: (String, String),
    line: Arc<Line>,
    id: String,
    token: String,
    /// This guest, by the jam's id for them.
    me: String,
    host: String,
    stop: Arc<AtomicBool>,
    /// The jam as last heard, and when: what's playing, who's here.
    heard: Value,
    heard_at: Instant,
    /// Songs found for what's typed in the jam's search box; `None` when nothing is.
    results: Option<Vec<Value>>,
    /// Counts searches, so a late answer isn't shown.
    searches: u64,
    /// Songs being added right now.
    adding: HashSet<String>,
    /// The jam's songs as this PC's own library knows them, by id; `None` for one it
    /// doesn't have. Kept so each is looked up once.
    known: HashMap<String, Option<Song>>,
    trouble: String,
}

#[derive(Default)]
pub struct Jam {
    /// Counts jams started or joined, to tell a late answer for an old one from news of this one.
    serial: u64,
    /// A jam is being started or joined.
    busy: bool,
    hosting: Option<Hosting>,
    joined: Option<Joined>,
    /// Counts checks of the relay in Settings, likewise.
    checks: u64,
    /// This device's session is being opened at the relay, and when that was last tried.
    opening: bool,
    tried: Option<Instant>,
    /// "Start a jam" was pressed while it was opening: make it a jam when it's open.
    go_public: bool,
    /// This device's place among the person's other devices, once it has one.
    presence: Option<Presence>,
    /// The other devices in the room: id, name, and what each says of itself.
    devices: Vec<(String, String, Value)>,
    /// What this device last said of itself (a fingerprint of it), and when.
    said: u64,
    said_at: Option<Instant>,
    /// There's no saved password to work the room's name out from: don't keep trying.
    no_words: bool,
}

/// This device's membership of the room where the person's devices find each other.
struct Presence {
    serial: u64,
    stop: Arc<AtomicBool>,
    line: Arc<Line>,
    key: String,
    /// The room's name, and the word that proves a device is one of this person's. Both
    /// are worked out from the account, so only copies signed in to it can know them.
    room: String,
    word: String,
}

/// What comes with joining one of this person's own devices instead of a friend's jam.
struct Own {
    word: String,
    device: (String, String),
    /// The queue this device was playing, to carry on with over there.
    handover: Option<Value>,
}

impl Jam {
    /// Who added each song still to come in the jam being hosted, for the queue panel.
    pub fn added_by(&self) -> HashMap<String, String> {
        self.hosting.as_ref().map(|hosting| hosting.added.iter().map(|(song, (_, name))| (song.clone(), name.clone())).collect()).unwrap_or_default()
    }
}

// ---- hosting: the threads ----------------------------------------------------------------

/// Sends what the UI thread hands over, in order.
fn send_loop(line: Arc<Line>, id: String, secret: String, waiting: mpsc::Receiver<Out>) {
    while let Ok(first) = waiting.recv() {
        let mut batch = vec![first];
        while let Ok(more) = waiting.try_recv() {
            batch.push(more);
        }
        let newest = batch.iter().rposition(|out| matches!(out, Out::State(_)));
        for (index, out) in batch.into_iter().enumerate() {
            match out {
                Out::State(state) => {
                    if Some(index) == newest {
                        let _ = line.post(&format!("api/host/{id}/state"), &secret, &state);
                    }
                }
                Out::Reply(reply) => {
                    let _ = line.post(&format!("api/host/{id}/reply"), &secret, &reply);
                }
                Out::Remove(guest) => {
                    let _ = line.post(&format!("api/host/{id}/remove"), &secret, &json!({"guest": guest}));
                }
                Out::End(done) => {
                    let _ = line.send(&format!("api/host/{id}/end"), &secret, "application/json", b"{}", Duration::from_secs(3));
                    let _ = done.send(());
                    return;
                }
            }
        }
    }
}

/// Waits on the relay for things to do, for as long as the jam runs.
fn host_loop(line: Arc<Line>, id: String, secret: String, serial: u64, stop: Arc<AtomicBool>, pool: Arc<crate::pool::Pool>, client: Option<Client>, covers: std::path::PathBuf) {
    let (mut after, mut failures) = (0u64, 0u32);
    while !stop.load(Ordering::Relaxed) {
        let answer = line.get(&format!("api/host/{id}/poll?after={after}"), &secret, Duration::from_secs(40));
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let body = match answer {
            Ok((200, body)) => body,
            Ok((401 | 404, _)) => {
                return post(move |ctl| ctl.jam_lost(serial, "The jam has ended: the relay no longer has it."));
            }
            _ => {
                failures += 1;
                if failures == 2 {
                    post(move |ctl| ctl.jam_trouble(serial, "Reconnecting to the relay…"));
                }
                std::thread::sleep(Duration::from_secs(failures.min(5) as u64));
                continue;
            }
        };
        if failures >= 2 {
            post(move |ctl| ctl.jam_trouble(serial, ""));
        }
        failures = 0;
        let mut arrivals = Vec::new();
        for event in body["events"].as_array().cloned().unwrap_or_default() {
            after = after.max(event["seq"].as_u64().unwrap_or(0));
            match event["kind"].as_str().unwrap_or("") {
                "joined" => arrivals.push(event["guest"]["name"].as_str().unwrap_or("Someone").to_string()),
                "ask" => {
                    let (line, id, secret, client) = (line.clone(), id.clone(), secret.clone(), client.clone());
                    pool.spawn(move || answer_guest(&line, &id, &secret, serial, client.as_ref(), event));
                }
                "cover" => {
                    let (line, id, secret, client, covers) = (line.clone(), id.clone(), secret.clone(), client.clone(), covers.clone());
                    pool.spawn(move || send_cover(&line, &id, &secret, client.as_ref(), &covers, &event));
                }
                _ => {}
            }
        }
        let guests: Vec<(String, String)> = body["guests"]
            .as_array()
            .map(|list| list.iter().map(|guest| (guest["id"].as_str().unwrap_or("").to_string(), guest["name"].as_str().unwrap_or("").to_string())).collect())
            .unwrap_or_default();
        post(move |ctl| ctl.jam_news(serial, guests, arrivals));
    }
}

/// Deals with one thing a guest asked for. Looking things up happens here, on a worker
/// thread; changing what plays is passed to the UI thread.
fn answer_guest(line: &Line, id: &str, secret: &str, serial: u64, client: Option<&Client>, event: Value) {
    let ask = event["id"].as_u64().unwrap_or(0);
    let guest = (event["guest"]["id"].as_str().unwrap_or("").to_string(), event["guest"]["name"].as_str().unwrap_or("Someone").to_string());
    let action = event["action"].as_str().unwrap_or("").to_string();
    let data = event["data"].clone();
    if action == "search" {
        let songs = find_songs(data["q"].as_str().unwrap_or(""), client);
        let _ = line.post(&format!("api/host/{id}/reply"), secret, &json!({"id": ask, "ok": true, "data": {"songs": songs}}));
        return;
    }
    // Songs to add are named by id: one, or a list of them.
    let wanted: Vec<String> = match data["ids"].as_array() {
        Some(ids) => ids.iter().filter_map(Value::as_str).take(MOST_ADDED).map(str::to_string).collect(),
        None => data["id"].as_str().map(str::to_string).into_iter().collect(),
    };
    let mut data = data;
    let songs: Vec<Song> = match action.as_str() {
        "add" => wanted.iter().filter_map(|id| find_song(id, client)).collect(),
        "load" => {
            let (songs, index) = handed_songs(&data);
            data["index"] = json!(index);
            songs
        }
        _ => Vec::new(),
    };
    post(move |ctl| ctl.jam_asked(serial, ask, guest, &action, &data, songs));
}

/// The songs of a queue handed over from another device, as this library knows them, and
/// where in them the one to start with is. Songs this library doesn't have are left out.
fn handed_songs(data: &Value) -> (Vec<Song>, usize) {
    let Some(index) = crate::search::current() else { return (Vec::new(), 0) };
    let known: HashMap<&str, &Song> = index.library.songs.iter().map(|song| (song.id.as_str(), song)).collect();
    let start = data["index"].as_u64().unwrap_or(0) as usize;
    let (mut songs, mut begin) = (Vec::new(), 0);
    for (place, entry) in data["songs"].as_array().into_iter().flatten().take(MOST_HANDED).enumerate() {
        if place == start {
            begin = songs.len();
        }
        if let Some(song) = entry["id"].as_str().and_then(|id| known.get(id)) {
            songs.push(Song { queued: entry["queued"].as_bool() == Some(true), ..(*song).clone() });
        }
    }
    let begin = begin.min(songs.len().saturating_sub(1));
    (songs, begin)
}

/// Waits on the relay for news of this person's other devices.
fn room_loop(line: Arc<Line>, key: String, room: String, device: String, serial: u64, stop: Arc<AtomicBool>) {
    let mut version = 0u64;
    while !stop.load(Ordering::Relaxed) {
        let answer = line.get(&format!("api/room/{room}/poll?device={device}&v={version}"), &key, Duration::from_secs(40));
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match answer {
            Ok((200, body)) => {
                version = body["v"].as_u64().unwrap_or(0);
                let seen: Vec<(String, String, Value)> = body["devices"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|device| (device["id"].as_str().unwrap_or("").to_string(), device["name"].as_str().unwrap_or("").to_string(), device["status"].clone()))
                    .collect();
                post(move |ctl| ctl.devices_seen(serial, seen));
            }
            // The relay has forgotten this device (it restarted, say): say hello again.
            Ok((404, _)) => {
                post(move |ctl| ctl.devices_forgot(serial));
                std::thread::sleep(Duration::from_secs(3));
            }
            Ok((401 | 429, _)) => std::thread::sleep(Duration::from_secs(30)),
            _ => std::thread::sleep(Duration::from_secs(5)),
        }
    }
}

fn send_cover(line: &Line, id: &str, secret: &str, client: Option<&Client>, covers: &std::path::Path, event: &Value) {
    let (cover, size) = (event["cover"].as_str().unwrap_or(""), event["size"].as_str().unwrap_or("s"));
    // Only the library's own covers: an id that is an address would have this PC fetch it.
    let plain = !cover.is_empty() && cover.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~'));
    let large = size == "l";
    let found = if plain { client.and_then(|client| crate::covers::file_bytes(client, covers, cover, large)).and_then(|bytes| cover_for_guests(bytes, large)) } else { None };
    let path = format!("api/host/{id}/cover?cover={cover}&size={size}");
    let _ = match found {
        Some((bytes, kind)) => line.send(&path, secret, kind, &bytes, Duration::from_secs(30)),
        None => line.send(&format!("{path}&missing=1"), secret, "application/octet-stream", b"", Duration::from_secs(20)),
    };
}

// ---- joining: the thread -----------------------------------------------------------------

/// Waits on the relay for the jam to change, for as long as this PC is in it.
fn guest_loop(line: Arc<Line>, id: String, token: String, serial: u64, stop: Arc<AtomicBool>) {
    let (mut version, mut failures) = (0u64, 0u32);
    while !stop.load(Ordering::Relaxed) {
        let answer = line.get(&format!("api/jam/{id}/state?v={version}"), &token, Duration::from_secs(40));
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match answer {
            Ok((200, body)) => {
                failures = 0;
                version = body["v"].as_u64().unwrap_or(0);
                post(move |ctl| ctl.jam_heard(serial, body));
            }
            Ok((404, _)) => return post(move |ctl| ctl.jam_gone(serial, "The jam has ended.")),
            Ok((403, _)) => return post(move |ctl| ctl.jam_gone(serial, "The host removed you from the jam.")),
            Ok((401, _)) => return post(move |ctl| ctl.jam_gone(serial, "The jam was restarted. Join it again from its link.")),
            _ => {
                failures += 1;
                if failures == 2 {
                    post(move |ctl| ctl.jam_guest_trouble(serial, "Reconnecting to the jam…"));
                }
                std::thread::sleep(Duration::from_secs(failures.min(5) as u64));
            }
        }
    }
}

// ---- the UI thread -----------------------------------------------------------------------

impl Controller {
    /// The name this person goes by in jams: the one they chose, or else their account's.
    fn jam_name(&self) -> String {
        let settings = self.settings.borrow();
        let chosen = settings.jam_name.trim();
        if chosen.is_empty() { settings.username.clone() } else { chosen.to_string() }
    }

    /// Keeps the name typed in the jam window, if it's a new one.
    fn set_jam_name(&self, typed: &str) {
        let typed: String = typed.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(24).collect();
        if !typed.is_empty() && typed != self.jam_name() {
            self.settings.borrow_mut().jam_name = typed;
            self.save_settings();
            self.push_jam();
        }
    }

    /// Whether a jam could be hosted from here: hosting is on and a host link is on file.
    fn can_host_jam(&self) -> bool {
        let settings = self.settings.borrow();
        settings.jam_host && !settings.jam_relay.is_empty() && config::load_secret(&self.paths, "jam-key").is_some()
    }

    /// Shows the settings that jams depend on.
    pub fn push_jam(&self) {
        let page = self.ui.global::<ui::Settings>();
        {
            let settings = self.settings.borrow();
            page.set_jam_host(settings.jam_host);
            page.set_jam_relay(host_of(&settings.jam_relay).into());
            page.set_link_devices(settings.link_devices);
        }
        page.set_device_name(self.this_device().1.into());
        let jam = self.ui.global::<ui::Jam>();
        jam.set_can_host(self.can_host_jam());
        jam.set_name(self.jam_name().into());
        jam.set_guests_control(self.settings.borrow().jam_guests_control);
    }

    fn jam_setting_state(&self, state: &str, text: &str) {
        let page = self.ui.global::<ui::Settings>();
        page.set_jam_state(state.into());
        page.set_jam_status(text.into());
    }

    /// Keeps a pasted host link (the address in the settings file, the key with the
    /// passwords) and checks that the relay accepts it.
    fn save_jam_link(&self, link: &str) {
        match parse_host_link(link) {
            Ok((address, key)) => {
                self.settings.borrow_mut().jam_relay = address;
                config::save_secret(&self.paths, "jam-key", &key);
                self.save_settings();
                self.push_jam();
                self.check_jam();
            }
            Err(why) => self.jam_setting_state("error", &why),
        }
    }

    /// Asks the relay on file whether it's there and takes this PC's host key.
    pub fn check_jam(&self) {
        let (on, address) = {
            let settings = self.settings.borrow();
            (settings.jam_host, settings.jam_relay.clone())
        };
        if !on {
            return;
        }
        let Some(key) = config::load_secret(&self.paths, "jam-key").filter(|_| !address.is_empty()) else {
            return self.jam_setting_state("none", "Paste the host link from your relay to finish.");
        };
        let check = {
            let mut jam = self.jam.borrow_mut();
            jam.checks += 1;
            jam.checks
        };
        self.jam_setting_state("checking", "Checking…");
        self.jam_pool.spawn(move || {
            let (state, text) = match Line::new(&address).get("api/hello", &key, Duration::from_secs(15)) {
                Ok((200, body)) if body["nexus_jam"].as_u64() == Some(1) && body["host"].as_bool() == Some(true) => ("connected", format!("Connected to the relay at {}.", host_of(&address))),
                Ok((401, _)) => ("error", "The relay didn't accept that host link. Copy it again from the relay's log.".to_string()),
                Ok((429, body)) => ("error", reason(&body, "The relay is refusing checks for the moment. Try again in a minute.")),
                Ok(_) => ("error", format!("{} didn't answer like a jam relay.", host_of(&address))),
                Err(why) => ("error", why),
            };
            post(move |ctl| {
                if ctl.jam.borrow().checks == check {
                    ctl.jam_setting_state(state, &text);
                }
            });
        });
    }

    /// Brings the jam's part of the window up to date with a jam being hosted (or none).
    fn show_hosting(&self) {
        let ui = self.ui.global::<ui::Jam>();
        let jam = self.jam.borrow();
        ui.set_busy(jam.busy);
        match jam.hosting.as_ref().filter(|hosting| hosting.public) {
            Some(hosting) => {
                ui.set_hosting(true);
                ui.set_link(hosting.link.as_str().into());
                ui.set_status(hosting.trouble.as_str().into());
                // This person's own devices aren't guests.
                let guests: Vec<ui::JamGuest> = hosting.guests.iter().filter(|(id, _)| !hosting.owners.contains(id)).map(|(id, name)| ui::JamGuest { id: id.into(), name: name.into() }).collect();
                ui.set_guests(ModelRc::new(VecModel::from(guests)));
            }
            None => {
                ui.set_hosting(false);
                ui.set_link("".into());
                ui.set_status("".into());
                ui.set_qr(slint::Image::default());
                ui.set_guests(ModelRc::default());
            }
        }
    }

    fn jam_problem(&self, text: &str) {
        self.ui.global::<ui::Jam>().set_problem(text.into());
    }

    /// Starts a jam. If this device already has a session open at the relay for the
    /// person's other devices, that session becomes the jam; otherwise one is opened.
    pub fn start_jam(self: &Rc<Self>) {
        self.jam_problem("");
        if self.jam.borrow().busy {
            return;
        }
        if self.jam.borrow().joined.is_some() {
            return self.jam_problem("Leave the jam you're in before starting one.");
        }
        if !self.can_host_jam() {
            return self.jam_problem("Hosting isn't set up yet. Turn it on in Settings, under Jam.");
        }
        let link = {
            let mut jam = self.jam.borrow_mut();
            match jam.hosting.as_mut() {
                Some(hosting) => {
                    hosting.public = true;
                    Some(hosting.link.clone())
                }
                None => {
                    jam.busy = true;
                    jam.go_public = true;
                    None
                }
            }
        };
        match link {
            Some(link) => {
                self.ui.global::<ui::Jam>().set_qr(qr_image(&link));
                self.show_hosting();
                self.sync_queue();
            }
            None => {
                self.show_hosting();
                self.open_session();
            }
        }
    }

    /// Opens this device's session at the relay, through which it can be steered: by
    /// guests if it's made a jam, and by the person's other devices either way.
    fn open_session(self: &Rc<Self>) {
        if self.jam.borrow().opening || self.jam.borrow().hosting.is_some() {
            return;
        }
        let (address, key) = (self.settings.borrow().jam_relay.clone(), config::load_secret(&self.paths, "jam-key"));
        let Some(key) = key.filter(|_| self.can_host_jam()) else { return };
        let name = self.jam_name();
        let serial = {
            let mut jam = self.jam.borrow_mut();
            jam.opening = true;
            jam.tried = Some(Instant::now());
            jam.serial += 1;
            jam.serial
        };
        self.jam_pool.spawn(move || {
            let line = Line::new(&address);
            let made = match line.post("api/host/start", &key, &json!({"name": name})) {
                Ok((200, body)) => match (body["jam"].as_str(), body["secret"].as_str()) {
                    (Some(id), Some(secret)) => Ok((id.to_string(), secret.to_string())),
                    _ => Err("The relay's answer wasn't understood. It may be a different version.".to_string()),
                },
                Ok((401, _)) => Err("The relay didn't accept the host link. Set it up again in Settings, under Jam.".to_string()),
                Ok((_, body)) => Err(reason(&body, "The relay couldn't start a jam.")),
                Err(why) => Err(why),
            };
            post(move |ctl| ctl.jam_started(serial, line, made));
        });
    }

    fn jam_started(self: &Rc<Self>, serial: u64, line: Line, made: Result<(String, String), String>) {
        if self.jam.borrow().serial != serial {
            return;
        }
        let public = {
            let mut jam = self.jam.borrow_mut();
            jam.opening = false;
            jam.busy = false;
            std::mem::take(&mut jam.go_public)
        };
        let (id, secret) = match made {
            Ok(made) => made,
            Err(why) => {
                self.show_hosting();
                // Only someone who pressed "Start a jam" is waiting to hear why not.
                if public {
                    self.jam_problem(&why);
                }
                return;
            }
        };
        let line = Arc::new(line);
        let link = format!("{}/j/{id}", line.base);
        let (out, waiting) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (line, id, secret) = (line.clone(), id.clone(), secret.clone());
            let _ = std::thread::Builder::new().name("jam-send".into()).spawn(move || send_loop(line, id, secret, waiting));
        }
        {
            let (id, stop, pool, client, covers) = (id.clone(), stop.clone(), self.jam_pool.clone(), self.client(), self.covers.dir().to_path_buf());
            let _ = std::thread::Builder::new().name("jam-host".into()).spawn(move || host_loop(line, id, secret, serial, stop, pool, client, covers));
        }
        if public {
            self.ui.global::<ui::Jam>().set_qr(qr_image(&link));
        }
        self.jam.borrow_mut().hosting = Some(Hosting {
            serial,
            id,
            public,
            owners: HashSet::new(),
            link,
            guests: Vec::new(),
            added: HashMap::new(),
            out,
            stop,
            told: 0,
            told_at: Instant::now(),
            told_position: 0.0,
            told_playing: false,
            trouble: String::new(),
        });
        self.show_hosting();
        self.jam_sync(true);
    }

    /// Ends the jam being hosted. With `wait`, gives the relay a moment to hear of it:
    /// for when the app is closing and the thread that tells it would otherwise be cut off.
    pub fn end_jam(self: &Rc<Self>, wait: bool) {
        let hosting = {
            let mut jam = self.jam.borrow_mut();
            jam.busy = false;
            jam.opening = false;
            jam.go_public = false;
            // Anything still on its way for this jam is for nobody now.
            jam.serial += 1;
            jam.hosting.take()
        };
        let Some(hosting) = hosting else { return };
        hosting.stop.store(true, Ordering::Relaxed);
        let (done, told) = mpsc::channel();
        let _ = hosting.out.send(Out::End(done));
        if wait {
            let _ = told.recv_timeout(Duration::from_millis(1500));
        }
        self.show_hosting();
        // The queue panel no longer says who added what.
        self.sync_queue();
    }

    /// Guests have come or gone.
    fn jam_news(self: &Rc<Self>, serial: u64, guests: Vec<(String, String)>, arrivals: Vec<String>) {
        let public = {
            let mut jam = self.jam.borrow_mut();
            let Some(hosting) = jam.hosting.as_mut().filter(|hosting| hosting.serial == serial) else { return };
            if hosting.guests == guests && arrivals.is_empty() {
                return;
            }
            hosting.guests = guests;
            hosting.public
        };
        self.show_hosting();
        // Outside a jam, the only ones who come and go are this person's own devices.
        if public {
            match arrivals.as_slice() {
                [] => {}
                [one] => self.toast(format!("{one} joined the jam")),
                several => self.toast(format!("{} people joined the jam", several.len())),
            }
        }
    }

    fn jam_trouble(&self, serial: u64, text: &str) {
        if let Some(hosting) = self.jam.borrow_mut().hosting.as_mut().filter(|hosting| hosting.serial == serial) {
            hosting.trouble = text.to_string();
        }
        self.show_hosting();
    }

    /// The relay no longer knows the jam (it restarted, or the jam ran out).
    fn jam_lost(self: &Rc<Self>, serial: u64, why: &str) {
        let public = match self.jam.borrow().hosting.as_ref() {
            Some(hosting) if hosting.serial == serial => hosting.public,
            _ => return,
        };
        self.end_jam(false);
        // A session that was only there for the person's own devices is quietly opened again.
        if public {
            self.toast(why);
        }
    }

    fn remove_jam_guest(&self, guest: &str) {
        if let Some(hosting) = self.jam.borrow_mut().hosting.as_mut() {
            let _ = hosting.out.send(Out::Remove(guest.to_string()));
            hosting.guests.retain(|(id, _)| id != guest);
        }
        self.show_hosting();
    }

    /// A guest asked for something that changes what plays. `songs` are the ones to add,
    /// when that's what was asked, as far as the library has them.
    fn jam_asked(self: &Rc<Self>, serial: u64, ask: u64, guest: (String, String), action: &str, data: &Value, songs: Vec<Song>) {
        if self.jam.borrow().hosting.as_ref().is_none_or(|hosting| hosting.serial != serial) {
            return;
        }
        // One of this person's own devices says so with a word only copies signed in to
        // their account can work out. It has the run of this one.
        let own = {
            let mut jam = self.jam.borrow_mut();
            let word = jam.presence.as_ref().map(|presence| presence.word.clone());
            let own = word.is_some_and(|word| data["owner"].as_str() == Some(word.as_str()));
            let fresh = own && jam.hosting.as_mut().is_some_and(|hosting| hosting.owners.insert(guest.0.clone()));
            drop(jam);
            if fresh {
                self.show_hosting();
            }
            own
        };
        // Adding songs, and taking your own back out, is what a jam is for. The rest is
        // the host's to allow.
        let control = own || self.settings.borrow().jam_guests_control;
        let done = match action {
            "add" => self.jam_add_for(&guest, songs, control && data["next"].as_bool() == Some(true), own),
            "remove" => self.jam_remove_for(&guest, data, control),
            "hello" | "queue" | "load" | "volume" if !own => Err("That's for your own devices.".to_string()),
            "hello" => Ok(json!({})),
            "queue" => Ok(self.queue_to_hand_over()),
            "load" => {
                if songs.is_empty() {
                    Err("This device's library has none of those songs.".to_string())
                } else {
                    let index = data["index"].as_u64().unwrap_or(0) as usize;
                    self.load_queue(songs, index, data["position"].as_f64().unwrap_or(0.0), data["playing"].as_bool() != Some(false), data["context"].as_str().unwrap_or(""));
                    Ok(json!({}))
                }
            }
            "volume" => {
                self.set_volume(data["to"].as_f64().unwrap_or(1.0) as f32);
                Ok(json!({}))
            }
            "skip" | "previous" | "pause" | "play" | "seek" | "jump" | "move" | "clear" | "shuffle" | "repeat" if !control => {
                Err("The host is choosing what plays. You can still add songs.".to_string())
            }
            "skip" | "previous" | "pause" | "play" | "seek" | "jump" | "move" | "clear" | "shuffle" | "repeat" => self.jam_control(action, data).map(|()| json!({})),
            _ => Err("That isn't something a guest can do.".to_string()),
        };
        let reply = match done {
            Ok(data) => json!({"id": ask, "ok": true, "data": data}),
            Err(why) => json!({"id": ask, "ok": false, "error": why}),
        };
        if let Some(hosting) = self.jam.borrow().hosting.as_ref() {
            let _ = hosting.out.send(Out::Reply(reply));
        }
        self.jam_sync(false);
    }

    /// What this device is playing and has queued, for another device to carry on with:
    /// the current song first, then the rest in the order they'd play.
    fn queue_to_hand_over(&self) -> Value {
        let pb = self.playback.borrow();
        let songs: Vec<Value> = pb.order.iter().skip(pb.cursor).filter_map(|&index| pb.queue.get(index)).filter(|song| song.radio_url.is_empty()).take(MOST_HANDED).map(|song| json!({"id": song.id, "queued": song.queued})).collect();
        json!({"songs": songs, "index": 0, "position": pb.position, "playing": pb.playing && pb.current().is_some(), "context": pb.context})
    }

    /// Which entry after the current track a guest means: the one at `at`, if it's still
    /// the song they saw there, or else the nearest copy of that song. The queue may have
    /// moved on between their seeing it and their asking.
    fn jam_entry(&self, at: &Value, id: &Value) -> Result<usize, String> {
        let (at, id) = (at.as_u64().unwrap_or(u64::MAX) as usize, id.as_str().unwrap_or(""));
        let pb = self.playback.borrow();
        let ahead = || pb.order.iter().skip(pb.cursor + 1).filter_map(|&index| pb.queue.get(index));
        if ahead().nth(at).is_some_and(|song| song.id == id) {
            return Ok(at);
        }
        ahead().enumerate().filter(|(_, song)| song.id == id).map(|(place, _)| place).min_by_key(|place| place.abs_diff(at)).ok_or_else(|| "That song is no longer in the queue.".to_string())
    }

    /// Does to playback what a guest who's allowed to asked for.
    fn jam_control(self: &Rc<Self>, action: &str, data: &Value) -> Result<(), String> {
        if self.playback.borrow().current().is_none() {
            return Err("Nothing is playing.".to_string());
        }
        match action {
            "skip" => self.next_track(),
            "previous" => self.previous_track(),
            "pause" => self.set_playing(false),
            "play" => self.set_playing(true),
            "seek" => self.seek(data["to"].as_f64().unwrap_or(0.0).max(0.0)),
            "jump" => self.jump_to_upcoming(self.jam_entry(&data["at"], &data["id"])?),
            "move" => {
                let from = self.jam_entry(&data["from"], &data["id"])?;
                let last = {
                    let pb = self.playback.borrow();
                    pb.order.len().saturating_sub(pb.cursor + 2)
                };
                self.move_upcoming(from, (data["to"].as_u64().unwrap_or(0) as usize).min(last), data["queued"].as_bool() == Some(true));
            }
            "clear" => self.clear_upcoming(data["queued"].as_bool() == Some(true)),
            "shuffle" => self.set_shuffle(data["on"].as_bool() == Some(true)),
            _ => self.set_repeat(data["mode"].as_u64().unwrap_or(0).min(2) as u8),
        }
        Ok(())
    }

    /// Queues a guest's songs: in "Next in queue", at the bottom of it (or the top, with
    /// `next`), so what guests add plays in the order they added it and ahead of the rest
    /// of whatever list the host was playing.
    fn jam_add_for(self: &Rc<Self>, guest: &(String, String), songs: Vec<Song>, next: bool, own: bool) -> Result<Value, String> {
        let asked = songs.len();
        let (empty, mut waiting) = {
            let pb = self.playback.borrow();
            let waiting: HashSet<String> = pb.current().into_iter().chain(pb.order.iter().skip(pb.cursor + 1).take(pb.queued_ahead()).filter_map(|&index| pb.queue.get(index))).map(|song| song.id.clone()).collect();
            (pb.order.is_empty(), waiting)
        };
        // A song that's already waiting in the queue, or playing, isn't queued a second time.
        let fresh: Vec<Song> = songs.into_iter().filter(|song| song.radio_url.is_empty() && waiting.insert(song.id.clone())).collect();
        if fresh.is_empty() {
            return Err(if asked == 0 { "That song isn't in the host's library." } else { "That's already in the queue." }.to_string());
        }
        // What the person queued from another of their devices is theirs, not a guest's.
        if let Some(hosting) = self.jam.borrow_mut().hosting.as_mut().filter(|_| !own) {
            for song in &fresh {
                hosting.added.insert(song.id.clone(), guest.clone());
            }
        }
        let said = match fresh.as_slice() {
            [one] => format!("{} added {}", guest.1, one.title),
            many => format!("{} added {} songs", guest.1, many.len()),
        };
        let count = fresh.len();
        self.enqueue(fresh, next);
        if empty && !self.playback.borrow().playing {
            // Nothing was queued at all: the first song anyone adds starts the music.
            self.toggle_play();
        }
        if !own {
            self.toast(said);
        }
        Ok(json!({"added": count}))
    }

    /// Takes a song out of the queue for a guest: any song if the host lets guests steer,
    /// otherwise only one they queued themselves.
    fn jam_remove_for(self: &Rc<Self>, guest: &(String, String), data: &Value, control: bool) -> Result<Value, String> {
        let place = self.jam_entry(&data["at"], &data["id"])?;
        if !control {
            let queued = place < self.playback.borrow().queued_ahead();
            let theirs = self.jam.borrow().hosting.as_ref().is_some_and(|hosting| hosting.added.get(data["id"].as_str().unwrap_or("")).is_some_and(|(id, _)| *id == guest.0));
            if !(queued && theirs) {
                return Err("You can take out the songs you added.".to_string());
            }
        }
        self.remove_upcoming(place);
        Ok(json!({}))
    }

    /// Tells the relay what's playing and queued, if that's changed since it was last told
    /// (or regardless, with `force`). Called a few times a second while a jam is hosted.
    pub fn jam_sync(self: &Rc<Self>, force: bool) {
        use std::hash::{Hash, Hasher};
        let control = self.settings.borrow().jam_guests_control;
        let mut jam = self.jam.borrow_mut();
        let Some(hosting) = jam.hosting.as_mut() else { return };
        let pb = self.playback.borrow();
        let now = pb.current();
        let ahead: Vec<&Song> = pb.order.iter().skip(pb.cursor + 1).filter_map(|&index| pb.queue.get(index)).collect();
        // Who added a song matters only until it has played or been taken out.
        if !hosting.added.is_empty() {
            let around: HashSet<&str> = now.iter().copied().chain(ahead.iter().copied()).map(|song| song.id.as_str()).collect();
            hosting.added.retain(|song, _| around.contains(song.as_str()));
        }
        let playing = pb.playing && now.is_some();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (now.map(|song| song.id.as_str()), playing, control, ahead.len(), pb.shuffle, pb.repeat, pb.context.as_str(), (pb.volume * 100.0) as i32).hash(&mut hasher);
        for song in ahead.iter().take(SHOWN) {
            (song.id.as_str(), song.queued, hosting.added.get(&song.id).map(|(id, _)| id.as_str())).hash(&mut hasher);
        }
        let print = hasher.finish();
        // Guests work out where the song has got to from where it was when they were told;
        // a seek, or a stall, makes that wrong.
        let expected = hosting.told_position + if hosting.told_playing { hosting.told_at.elapsed().as_secs_f64() } else { 0.0 };
        let moved = now.is_some() && (pb.position - expected).abs() > 2.0;
        if !(force || print != hosting.told || moved || hosting.told_at.elapsed() >= RETELL) {
            return;
        }
        let queue: Vec<Value> = ahead
            .iter()
            .take(SHOWN)
            .map(|song| {
                let mut entry = song_json(song);
                if song.queued {
                    // Queued by hand: part of "Next in queue". If a guest queued it, by whom.
                    entry["queued"] = json!(true);
                    if let Some((id, name)) = hosting.added.get(&song.id) {
                        entry["by"] = json!(name);
                        entry["by_id"] = json!(id);
                    }
                }
                entry
            })
            .collect();
        let state = json!({
            "playing": playing,
            "position": pb.position,
            "control": control,
            "now": now.map(song_json),
            "queue": queue,
            "more": ahead.len().saturating_sub(SHOWN),
            "context": pb.context,
            "shuffle": pb.shuffle,
            "repeat": pb.repeat,
            "volume": pb.volume,
        });
        hosting.told = print;
        hosting.told_at = Instant::now();
        hosting.told_position = pb.position;
        hosting.told_playing = playing;
        let _ = hosting.out.send(Out::State(state));
    }

    // ---- joining -----------------------------------------------------------------------------

    /// Joins the jam a link leads to, under `name`.
    pub fn join_jam(self: &Rc<Self>, link: &str, name: &str) {
        self.jam_problem("");
        if self.jam.borrow().busy {
            return;
        }
        if self.jam.borrow().hosting.as_ref().is_some_and(|hosting| hosting.public) {
            return self.jam_problem("End the jam you're hosting before joining another.");
        }
        let (address, id) = match parse_jam_link(link) {
            Ok(parts) => parts,
            Err(why) => return self.jam_problem(&why),
        };
        self.set_jam_name(name);
        let name = self.jam_name();
        self.join_session(address, id, name, None);
    }

    /// Joins a session at a relay: a friend's jam, or (with `own`) one of this person's
    /// other devices, to steer it.
    fn join_session(self: &Rc<Self>, address: String, id: String, name: String, own: Option<Own>) {
        self.leave_jam();
        let serial = {
            let mut jam = self.jam.borrow_mut();
            jam.busy = own.is_none();
            jam.serial += 1;
            jam.serial
        };
        self.show_hosting();
        self.jam_pool.spawn(move || {
            let line = Line::new(&address);
            let joined = match line.post(&format!("api/jam/{id}/join"), "", &json!({"name": name})) {
                Ok((200, body)) => match (body["token"].as_str(), body["guest"]["id"].as_str()) {
                    (Some(token), Some(me)) => Ok((token.to_string(), me.to_string(), body["host"].as_str().unwrap_or("Someone").to_string())),
                    _ => Err("That link didn't lead to a jam.".to_string()),
                },
                Ok((404, body)) if body["error"].is_string() => Err("That jam has ended. Ask its host for a new link.".to_string()),
                Ok((429 | 503, body)) => Err(reason(&body, "The jam can't take anyone else just now.")),
                Ok(_) => Err("That link didn't lead to a jam.".to_string()),
                Err(why) => Err(why),
            };
            post(move |ctl| ctl.jam_joined(serial, line, id, joined, own));
        });
    }

    fn jam_joined(self: &Rc<Self>, serial: u64, line: Line, id: String, joined: Result<(String, String, String), String>, own: Option<Own>) {
        if self.jam.borrow().serial != serial {
            return;
        }
        self.jam.borrow_mut().busy = false;
        self.show_hosting();
        let (token, me, host) = match joined {
            Ok(joined) => joined,
            Err(why) => {
                return match own {
                    Some(own) => self.toast(format!("Couldn't reach {}. {why}", own.device.1)),
                    None => self.jam_problem(&why),
                };
            }
        };
        let line = Arc::new(line);
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (line, id, token, stop) = (line.clone(), id.clone(), token.clone(), stop.clone());
            let _ = std::thread::Builder::new().name("jam-guest".into()).spawn(move || guest_loop(line, id, token, serial, stop));
        }
        // From here the player and the queue panel are the other device's. What was playing
        // on this PC stops, and is still there, where it was, afterwards.
        self.set_playing(false);
        let (owner, device, handover) = match own {
            Some(own) => (Some(own.word), own.device, own.handover),
            None => (None, Default::default(), None),
        };
        let friend = owner.is_none();
        self.jam.borrow_mut().joined = Some(Joined {
            serial,
            owner,
            device,
            line,
            id,
            token,
            me,
            host,
            stop,
            heard: Value::Null,
            heard_at: Instant::now(),
            results: None,
            searches: 0,
            adding: HashSet::new(),
            known: HashMap::new(),
            trouble: String::new(),
        });
        let ui = self.ui.global::<ui::Jam>();
        ui.set_query("".into());
        ui.set_open(false);
        self.show_joined();
        self.show_remote();
        self.show_devices();
        if friend {
            self.navigate(Route::Jam);
        } else {
            // Say who's asking, so the other device gives this one the run of it; and if
            // this one was playing, hand it what was playing here to carry on with.
            match handover {
                Some(queue) => self.jam_simply("load", queue),
                None => self.jam_simply("hello", json!({})),
            }
        }
    }

    /// Leaves the jam this PC joined, if any. The player goes back to this PC's own queue.
    pub fn leave_jam(self: &Rc<Self>) {
        self.leave_jam_saying(None);
    }

    /// Leaves, first asking one last thing of the other side (`last` is an action and what
    /// goes with it). The two go one after the other, so the goodbye can't overtake it.
    fn leave_jam_saying(self: &Rc<Self>, last: Option<(&str, Value)>) {
        let joined = {
            let mut jam = self.jam.borrow_mut();
            let joined = jam.joined.take();
            if joined.is_some() {
                jam.serial += 1;
            }
            joined
        };
        let Some(joined) = joined else { return };
        joined.stop.store(true, Ordering::Relaxed);
        // The host's list of who's there is put right at once, not when this PC has gone quiet.
        let (line, id, token) = (joined.line.clone(), joined.id.clone(), joined.token.clone());
        let last = last.map(|(action, mut data)| {
            if let Some(word) = &joined.owner {
                data["owner"] = json!(word);
            }
            json!({"action": action, "data": data})
        });
        self.jam_pool.spawn(move || {
            if let Some(last) = last {
                let _ = line.post(&format!("api/jam/{id}/ask"), &token, &last);
            }
            let _ = line.send(&format!("api/jam/{id}/leave"), &token, "application/json", b"{}", Duration::from_secs(5));
        });
        self.jam_timer.stop();
        self.show_joined();
        self.track_changed();
        self.show_devices();
        if self.current_route() == Some(Route::Jam) {
            self.navigate(Route::Home);
        }
    }

    /// The jam is over, or this guest is out of it.
    fn jam_gone(self: &Rc<Self>, serial: u64, why: &str) {
        let device = match self.jam.borrow().joined.as_ref() {
            Some(joined) if joined.serial == serial => joined.owner.as_ref().map(|_| joined.device.1.clone()),
            _ => return,
        };
        self.leave_jam();
        match device {
            Some(name) => self.toast(format!("{name} can't be reached any more, so the player is back on this PC.")),
            None => self.toast(why),
        }
    }

    fn jam_guest_trouble(&self, serial: u64, text: &str) {
        if let Some(joined) = self.jam.borrow_mut().joined.as_mut().filter(|joined| joined.serial == serial) {
            joined.trouble = text.to_string();
        }
        self.show_joined();
    }

    /// News of the jam this PC is in.
    fn jam_heard(&self, serial: u64, body: Value) {
        {
            let mut jam = self.jam.borrow_mut();
            let Some(joined) = jam.joined.as_mut().filter(|joined| joined.serial == serial) else { return };
            if let Some(host) = body["host"].as_str() {
                joined.host = host.to_string();
            }
            joined.heard = body;
            joined.heard_at = Instant::now();
            joined.trouble.clear();
        }
        self.show_joined();
        self.show_remote();
    }

    /// Asks the relay for the jam as it stands, after something this PC assumed turned out
    /// not to be so.
    fn jam_refresh(&self) {
        let jam = self.jam.borrow();
        let Some(joined) = &jam.joined else { return };
        let (line, id, token, serial) = (joined.line.clone(), joined.id.clone(), joined.token.clone(), joined.serial);
        self.jam_pool.spawn(move || {
            if let Ok((200, body)) = line.get(&format!("api/jam/{id}/state?v=0"), &token, Duration::from_secs(20)) {
                post(move |ctl| ctl.jam_heard(serial, body));
            }
        });
    }

    /// Where the host's song has got to: where it was when last heard, plus the time since.
    fn jam_position(joined: &Joined) -> f64 {
        let state = &joined.heard["state"];
        let duration = state["now"]["duration"].as_f64().unwrap_or(0.0);
        let moving = state["playing"].as_bool() == Some(true) && joined.heard["away"].as_bool() != Some(true);
        let since = if moving { joined.heard["age"].as_f64().unwrap_or(0.0) + joined.heard_at.elapsed().as_secs_f64() } else { 0.0 };
        (state["position"].as_f64().unwrap_or(0.0) + since).clamp(0.0, duration.max(0.0))
    }

    /// Brings the jam page up to date with the jam this PC is in (or none): who's there,
    /// and what the host's library has for what's typed.
    fn show_joined(&self) {
        let ui = self.ui.global::<ui::Jam>();
        let jam = self.jam.borrow();
        let Some(joined) = &jam.joined else {
            ui.set_joined(false);
            ui.set_linked(false);
            ui.set_host("".into());
            ui.set_results(ModelRc::default());
            return;
        };
        let state = &joined.heard["state"];
        let text = |value: &Value| SharedString::from(value.as_str().unwrap_or(""));
        // A friend's jam, or one of this person's own devices being steered from here.
        ui.set_joined(joined.owner.is_none());
        ui.set_linked(joined.owner.is_some());
        ui.set_device(joined.device.1.as_str().into());
        ui.set_host(joined.host.as_str().into());
        let others: Vec<&str> = joined.heard["guests"].as_array().map(|list| list.iter().filter(|guest| guest["id"].as_str() != Some(joined.me.as_str())).filter_map(|guest| guest["name"].as_str()).collect()).unwrap_or_default();
        ui.set_with(match others.as_slice() {
            [] => "Just you and the host so far.".into(),
            [one] => format!("{one} is here too.").into(),
            [first, second] => format!("{first} and {second} are here too.").into(),
            [first, second, rest @ ..] => format!("{first}, {second} and {} more are here too.", rest.len()).into(),
        });
        let away = joined.heard["away"].as_bool() == Some(true);
        ui.set_notice(if !joined.trouble.is_empty() {
            joined.trouble.as_str().into()
        } else if away {
            format!("{}'s app isn't connected right now. It should be back shortly.", joined.host).into()
        } else {
            SharedString::default()
        });
        ui.set_control(state["control"].as_bool() == Some(true));
        let coming: HashSet<&str> = state["queue"].as_array().into_iter().flatten().filter(|song| song["queued"].as_bool() == Some(true)).chain(state["now"].is_object().then_some(&state["now"])).filter_map(|song| song["id"].as_str()).collect();
        ui.set_searched(joined.results.is_some());
        let found: Vec<ui::JamSong> = joined
            .results
            .iter()
            .flatten()
            .map(|song| {
                let id = song["id"].as_str().unwrap_or("");
                let (artist, album) = (song["artist"].as_str().unwrap_or(""), song["album"].as_str().unwrap_or(""));
                ui::JamSong {
                    id: text(&song["id"]),
                    title: text(&song["title"]),
                    about: if album.is_empty() { artist.to_string() } else { format!("{artist} • {album}") }.into(),
                    cover: joined.cover(song, "s"),
                    duration: clock(song["duration"].as_u64().unwrap_or(0) as u32).into(),
                    // 1 it can be added, 2 it's being added, 3 it's already waiting in the queue.
                    action: if joined.adding.contains(id) {
                        2
                    } else if coming.contains(id) {
                        3
                    } else {
                        1
                    },
                }
            })
            .collect();
        ui.set_results(ModelRc::new(VecModel::from(found)));
    }

    /// When this PC is in someone's jam, shows the jam in the player and the queue panel,
    /// in place of this PC's own queue, and returns true. Otherwise does nothing.
    pub fn show_remote(&self) -> bool {
        let (now, rows, ahead, playing, position, duration, context, shuffle, repeat, volume) = {
            let mut jam = self.jam.borrow_mut();
            let Some(joined) = jam.joined.as_mut() else { return false };
            let state = joined.heard["state"].clone();
            let now = state["now"].is_object().then(|| self.jam_row(joined, &state["now"]));
            let queue = state["queue"].as_array().map(Vec::as_slice).unwrap_or_default();
            let rows: Vec<ui::Track> = queue.iter().map(|song| self.jam_row(joined, song)).collect();
            let ahead = queue.iter().take_while(|song| song["queued"].as_bool() == Some(true)).count();
            (
                now,
                rows,
                ahead,
                state["playing"].as_bool() == Some(true),
                Controller::jam_position(joined),
                state["now"]["duration"].as_f64().unwrap_or(0.0),
                state["context"].as_str().unwrap_or("").to_string(),
                state["shuffle"].as_bool() == Some(true),
                state["repeat"].as_i64().unwrap_or(0) as i32,
                // Between a person's own devices the volume slider is the other device's.
                joined.owner.as_ref().and_then(|_| state["volume"].as_f64()),
            )
        };
        let player = self.ui.global::<ui::Player>();
        player.set_has_track(now.is_some());
        let now = now.unwrap_or_default();
        player.set_track_id(now.id.clone());
        player.set_track(now);
        player.set_duration(duration as f32);
        player.set_position(position as f32);
        player.set_context(context.into());
        player.set_playing(playing);
        player.set_shuffle(shuffle);
        player.set_repeat(repeat);
        if let Some(volume) = volume {
            player.set_volume(volume as f32);
            player.set_muted(false);
        }
        player.set_buffering(false);
        player.set_reconnecting(false);
        player.set_stalled(false);
        player.set_format("".into());
        self.show_queue(rows, ahead);
        true
    }

    /// A song of the jam as a row for the player or the queue panel. A song this PC's own
    /// library has is shown as it is anywhere else in the app (its artist can be opened,
    /// it can be liked); one it doesn't have is shown from what the host said of it.
    fn jam_row(&self, joined: &mut Joined, song: &Value) -> ui::Track {
        let id = song["id"].as_str().unwrap_or("");
        let known = joined.known.entry(id.to_string()).or_insert_with(|| crate::search::current().and_then(|index| index.library.songs.iter().find(|known| known.id == id).cloned())).clone();
        let mut row = match &known {
            Some(known) => self.track_row(known),
            None => ui::Track {
                id: id.into(),
                title: song["title"].as_str().unwrap_or("").into(),
                artist: song["artist"].as_str().unwrap_or("").into(),
                album: song["album"].as_str().unwrap_or("").into(),
                cover: joined.cover(song, "l"),
                duration: clock(song["duration"].as_u64().unwrap_or(0) as u32).into(),
                progress: -1.0,
                ..Default::default()
            },
        };
        // Whether it can play offline is this PC's business, not the jam's.
        row.unavailable = false;
        row.by = match song["by"].as_str() {
            Some(_) if song["by_id"].as_str() == Some(joined.me.as_str()) => "you".into(),
            Some(name) => name.into(),
            None => SharedString::default(),
        };
        row
    }

    /// In a jam: the song a row of its queue stands for (-1 for the one playing), as this
    /// PC's library knows it. `None` when this PC isn't in a jam.
    pub fn remote_song(&self, index: i32) -> Option<Option<Song>> {
        let jam = self.jam.borrow();
        let joined = jam.joined.as_ref()?;
        let state = &joined.heard["state"];
        let song = if index < 0 { &state["now"] } else { &state["queue"][index as usize] };
        Some(song["id"].as_str().and_then(|id| joined.known.get(id).cloned().flatten()))
    }

    /// In a jam, asks the host to do `what` and returns true. Otherwise returns false, and
    /// the caller does it to this PC's own playback.
    pub fn remote(self: &Rc<Self>, what: Remote) -> bool {
        let (action, data) = {
            let mut jam = self.jam.borrow_mut();
            let Some(joined) = jam.joined.as_mut() else { return false };
            let me = joined.me.clone();
            let position = Controller::jam_position(joined);
            let state = &mut joined.heard["state"];
            let entry = |state: &Value, at: usize| state["queue"][at]["id"].as_str().unwrap_or("").to_string();
            // Guests may always take out what they queued themselves. Anything else that
            // steers the jam is the host's to allow, and there's no point asking if not.
            let mine = matches!(what, Remote::Remove(at) if state["queue"][at]["by_id"].as_str() == Some(me.as_str()));
            let own = joined.owner.is_some();
            if let (Remote::Volume(_), false) = (&what, own) {
                // In a friend's jam the volume slider is still this PC's own.
                return false;
            }
            if state["control"].as_bool() != Some(true) && !mine && !own {
                let host = joined.host.clone();
                drop(jam);
                self.toast(format!("{host} is choosing what plays. You can still add songs."));
                return true;
            }
            // The player shows what was asked for straight away; the host's word on it
            // follows a moment later.
            let settle = |state: &mut Value, playing: bool, at: f64| {
                state["playing"] = json!(playing);
                state["position"] = json!(at);
            };
            let sent = match what {
                Remote::Toggle | Remote::Play(_) => {
                    let playing = state["playing"].as_bool() == Some(true);
                    let wanted = match what {
                        Remote::Play(on) => on,
                        _ => !playing,
                    };
                    if wanted == playing {
                        return true;
                    }
                    settle(state, wanted, position);
                    (if wanted { "play" } else { "pause" }, json!({}))
                }
                Remote::Next => ("skip", json!({})),
                Remote::Previous => ("previous", json!({})),
                Remote::Seek(to) => {
                    let playing = state["playing"].as_bool() == Some(true);
                    settle(state, playing, to);
                    ("seek", json!({"to": to}))
                }
                Remote::Remove(at) => {
                    let id = entry(state, at);
                    if let Some(queue) = state["queue"].as_array_mut().filter(|queue| at < queue.len()) {
                        queue.remove(at);
                    }
                    ("remove", json!({"at": at, "id": id}))
                }
                Remote::Move(from, to, queued) => {
                    let id = entry(state, from);
                    if let Some(queue) = state["queue"].as_array_mut().filter(|queue| from < queue.len() && to < queue.len()) {
                        let mut moved = queue.remove(from);
                        moved["queued"] = json!(queued);
                        queue.insert(to, moved);
                    }
                    ("move", json!({"from": from, "to": to, "queued": queued, "id": id}))
                }
                Remote::Clear(queued) => ("clear", json!({"queued": queued})),
                Remote::Jump(at) => ("jump", json!({"at": at, "id": entry(state, at)})),
                Remote::Shuffle => ("shuffle", json!({"on": state["shuffle"].as_bool() != Some(true)})),
                Remote::Repeat => ("repeat", json!({"mode": (state["repeat"].as_u64().unwrap_or(0) + 1) % 3})),
                Remote::Volume(to) => {
                    state["volume"] = json!(to);
                    ("volume", json!({"to": to}))
                }
            };
            joined.heard["age"] = json!(0.0);
            joined.heard_at = Instant::now();
            sent
        };
        self.show_remote();
        self.jam_simply(action, data);
        true
    }

    /// Steering playback elsewhere, queues songs there and returns true. Otherwise returns false.
    pub fn remote_add(&self, songs: &[Song], play_next: bool) -> bool {
        let Some(place) = self.jam.borrow().joined.as_ref().map(|joined| if joined.owner.is_some() { format!("the queue on {}", joined.device.1) } else { "the jam's queue".to_string() }) else { return false };
        let ids: Vec<&str> = songs.iter().filter(|song| song.radio_url.is_empty()).map(|song| song.id.as_str()).take(MOST_ADDED).collect();
        if ids.is_empty() {
            self.toast("Radio stations can't be added from here");
            return true;
        }
        let cut = songs.len() > ids.len();
        self.jam_request("add", json!({"ids": ids, "next": play_next}), move |ctl, answer| match answer {
            Ok(done) => match done["added"].as_u64().unwrap_or(1) {
                1 => ctl.toast(format!("Added to {place}")),
                count if cut => ctl.toast(format!("Added the first {count} songs to {place}")),
                count => ctl.toast(format!("Added {count} songs to {place}")),
            },
            Err(why) => ctl.toast(why),
        });
        true
    }

    /// Steering playback elsewhere, playing something here plays it there, and this
    /// returns true. On one of the person's own devices it plays as it would here, taking
    /// the queue's place. In a friend's jam it's queued instead: the song that was picked,
    /// or the whole list when the list itself was played. Returns false otherwise.
    pub fn remote_play(&self, songs: &[Song], start: usize, context: &str, whole: bool) -> bool {
        let Some(own) = self.jam.borrow().joined.as_ref().map(|joined| joined.owner.is_some()) else { return false };
        if !own {
            let picked = if whole { songs } else { songs.get(start..start + 1).unwrap_or_default() };
            return self.remote_add(picked, false);
        }
        // A very long list is handed over from the song that was picked onwards.
        let from = if songs.len() > MOST_HANDED { start } else { 0 };
        let handed: Vec<Value> = songs.iter().skip(from).take(MOST_HANDED).map(|song| json!({"id": song.id})).collect();
        self.jam_simply("load", json!({"songs": handed, "index": start - from, "position": 0.0, "playing": true, "context": context}));
        true
    }

    /// Asks the host's app for something and hands back its answer on the UI thread.
    fn jam_request(&self, action: &str, data: Value, done: impl FnOnce(&Rc<Controller>, Result<Value, String>) + Send + 'static) {
        let jam = self.jam.borrow();
        let Some(joined) = &jam.joined else { return };
        let (line, id, token, serial) = (joined.line.clone(), joined.id.clone(), joined.token.clone(), joined.serial);
        let mut data = data;
        if let Some(word) = &joined.owner {
            // What shows the other device that this is one of its owner's own.
            data["owner"] = json!(word);
        }
        let body = json!({"action": action, "data": data});
        self.jam_pool.spawn(move || {
            let answer = match line.post(&format!("api/jam/{id}/ask"), &token, &body) {
                Ok((200, body)) if body["ok"].as_bool() == Some(true) => Ok(body["data"].clone()),
                Ok((_, body)) => Err(reason(&body, "That didn't work. Try again.")),
                Err(_) => Err("No connection to the jam. Try again.".to_string()),
            };
            post(move |ctl| {
                if ctl.jam.borrow().joined.as_ref().is_some_and(|joined| joined.serial == serial) {
                    done(ctl, answer);
                }
            });
        });
    }

    /// Something was typed in the jam's search box: look it up in the host's library once
    /// the typing pauses.
    fn jam_search(self: &Rc<Self>, text: &str) {
        let text = text.trim().to_string();
        let count = {
            let mut jam = self.jam.borrow_mut();
            let Some(joined) = jam.joined.as_mut() else { return };
            joined.searches += 1;
            if text.is_empty() {
                joined.results = None;
            }
            joined.searches
        };
        self.jam_timer.stop();
        if text.is_empty() {
            return self.show_joined();
        }
        self.jam_timer.start(slint::TimerMode::SingleShot, Duration::from_millis(250), move || {
            let text = text.clone();
            with(|ctl| {
                ctl.jam_request("search", json!({"q": text}), move |ctl, answer| {
                    {
                        let mut jam = ctl.jam.borrow_mut();
                        let Some(joined) = jam.joined.as_mut().filter(|joined| joined.searches == count) else { return };
                        match answer {
                            Ok(found) => joined.results = Some(found["songs"].as_array().cloned().unwrap_or_default()),
                            Err(why) => {
                                drop(jam);
                                return ctl.toast(why);
                            }
                        }
                    }
                    ctl.show_joined();
                });
            });
        });
    }

    /// The + beside a song found in the host's library.
    fn jam_add(self: &Rc<Self>, song: &str) {
        let song = song.to_string();
        let fresh = self.jam.borrow_mut().joined.as_mut().is_some_and(|joined| joined.adding.insert(song.clone()));
        if !fresh {
            return;
        }
        self.show_joined();
        self.jam_request("add", json!({"id": song}), move |ctl, answer| {
            if let Some(joined) = ctl.jam.borrow_mut().joined.as_mut() {
                joined.adding.remove(&song);
            }
            ctl.show_joined();
            match answer {
                Ok(_) => ctl.toast("Added to the jam's queue"),
                Err(why) => ctl.toast(why),
            }
        });
    }

    /// Asks for something with nothing to show for it but a complaint if it fails, and a
    /// fresh look at the jam, since the player showed it as done.
    fn jam_simply(&self, action: &str, data: Value) {
        self.jam_request(action, data, |ctl, answer| {
            if let Err(why) = answer {
                ctl.toast(why);
                ctl.jam_refresh();
            }
        });
    }

    /// Called a few times a second: keeps guests and other devices up to date with what's
    /// playing here, and moves the progress bar while steering playback elsewhere.
    pub fn jam_tick(self: &Rc<Self>) {
        let (hosting, position) = {
            let jam = self.jam.borrow();
            (jam.hosting.is_some(), jam.joined.as_ref().map(Controller::jam_position))
        };
        if hosting {
            self.jam_sync(false);
        }
        if let Some(position) = position {
            self.ui.global::<ui::Player>().set_position(position as f32);
        }
        self.link_devices();
    }

    // ---- this person's other devices ---------------------------------------------------------

    /// This device, to the others: an id that stays the same, and a name to show.
    fn this_device(&self) -> (String, String) {
        if self.settings.borrow().device_id.is_empty() {
            self.settings.borrow_mut().device_id = format!("{:08x}", fastrand::u32(..));
            self.save_settings();
        }
        let settings = self.settings.borrow();
        let chosen = settings.device_name.trim();
        let name = if chosen.is_empty() { std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).ok().filter(|name| !name.trim().is_empty()).unwrap_or_else(|| "This PC".to_string()) } else { chosen.to_string() };
        (settings.device_id.clone(), name.chars().take(24).collect())
    }

    /// What only copies of the app signed in to this account can work out: the name of the
    /// room they meet in, and the word that shows one of them is asking. Both come from the
    /// account's name and password, and neither gives the password away.
    fn account_words(&self) -> Option<(String, String)> {
        use sha2::{Digest, Sha256};
        let (server, username) = {
            let settings = self.settings.borrow();
            (settings.server.clone(), settings.username.clone())
        };
        let password = config::load_password(&self.paths, &server, &username)?;
        let word = |purpose: &str| Sha256::digest(format!("nexus-music {purpose}\n{username}\n{password}").as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect::<String>();
        Some((word("room"), word("owner")[..32].to_string()))
    }

    /// Whether this device should be finding the person's other devices: signed in, a relay
    /// set up, and linking not switched off.
    fn linking_wanted(&self) -> bool {
        self.client().is_some() && self.can_host_jam() && self.settings.borrow().link_devices
    }

    /// Keeps this device's place among the person's others: in the room, with a session
    /// open that they can steer it through, and saying what it's playing. Called a few
    /// times a second; does something only when something needs doing.
    fn link_devices(self: &Rc<Self>) {
        if !self.linking_wanted() {
            if self.jam.borrow().presence.is_some() {
                self.stop_linking();
            }
            return;
        }
        if self.jam.borrow().presence.is_none() {
            if self.jam.borrow().no_words {
                return;
            }
            let Some((room, word)) = self.account_words() else {
                self.jam.borrow_mut().no_words = true;
                return;
            };
            let (address, Some(key)) = (self.settings.borrow().jam_relay.clone(), config::load_secret(&self.paths, "jam-key")) else { return };
            let (line, stop) = (Arc::new(Line::new(&address)), Arc::new(AtomicBool::new(false)));
            let serial = {
                let mut jam = self.jam.borrow_mut();
                jam.serial += 1;
                jam.said = 0;
                jam.said_at = None;
                jam.presence = Some(Presence { serial: jam.serial, stop: stop.clone(), line: line.clone(), key: key.clone(), room: room.clone(), word });
                jam.serial
            };
            self.say_status(true);
            let device = self.this_device().0;
            let _ = std::thread::Builder::new().name("jam-room".into()).spawn(move || {
                // Give the first hello a moment to land before asking who else is there.
                std::thread::sleep(Duration::from_millis(700));
                room_loop(line, key, room, device, serial, stop)
            });
            self.show_devices();
        }
        // A session for the others to steer this device through, opened again if it's lost.
        let open = {
            let jam = self.jam.borrow();
            jam.hosting.is_none() && !jam.opening && jam.tried.is_none_or(|at| at.elapsed() >= TRY_AGAIN)
        };
        if open {
            self.open_session();
        }
        self.say_status(false);
    }

    /// Tells the room what this device is doing, if that's changed (or with `again`).
    fn say_status(&self, again: bool) {
        use std::hash::{Hash, Hasher};
        let (id, name) = self.this_device();
        let mut jam = self.jam.borrow_mut();
        let status = {
            let pb = self.playback.borrow();
            let now = pb.current();
            // While this device is steering another, or in a friend's jam, it isn't itself
            // there to be steered.
            let steering = jam.joined.as_ref().map(|joined| if joined.owner.is_some() { joined.device.1.clone() } else { format!("{}'s jam", joined.host) });
            json!({
                "session": jam.hosting.as_ref().filter(|_| steering.is_none()).map(|hosting| hosting.id.clone()),
                "title": now.map(|song| song.title.clone()),
                "artist": now.map(|song| song.artist_label().to_string()),
                "playing": pb.playing && now.is_some(),
                "steering": steering,
            })
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        (status.to_string(), &name).hash(&mut hasher);
        let print = hasher.finish();
        let stale = jam.said_at.is_none_or(|at| at.elapsed() >= RETELL);
        let Some(presence) = jam.presence.as_ref() else { return };
        if !(again || stale || print != jam.said) {
            return;
        }
        let (line, key, room) = (presence.line.clone(), presence.key.clone(), presence.room.clone());
        jam.said = print;
        jam.said_at = Some(Instant::now());
        self.jam_pool.spawn(move || {
            let _ = line.post(&format!("api/room/{room}/enter"), &key, &json!({"id": id, "name": name, "status": status}));
        });
    }

    /// Leaves the room: at sign-out, when linking is switched off, and when the app closes.
    pub fn stop_linking(&self) {
        let presence = {
            let mut jam = self.jam.borrow_mut();
            jam.devices.clear();
            jam.no_words = false;
            jam.tried = None;
            jam.presence.take()
        };
        if let Some(presence) = presence {
            presence.stop.store(true, Ordering::Relaxed);
            let id = self.this_device().0;
            self.jam_pool.spawn(move || {
                let _ = presence.line.send(&format!("api/room/{}/leave", presence.room), &presence.key, "application/json", json!({"id": id}).to_string().as_bytes(), Duration::from_secs(5));
            });
        }
        self.show_devices();
    }

    /// News of who's in the room.
    fn devices_seen(&self, serial: u64, seen: Vec<(String, String, Value)>) {
        let mine = self.this_device().0;
        {
            let mut jam = self.jam.borrow_mut();
            if jam.presence.as_ref().is_none_or(|presence| presence.serial != serial) {
                return;
            }
            jam.devices = seen.into_iter().filter(|(id, _, _)| *id != mine).collect();
        }
        self.show_devices();
    }

    /// The relay no longer has this device in the room: say hello again.
    fn devices_forgot(&self, serial: u64) {
        if self.jam.borrow().presence.as_ref().is_some_and(|presence| presence.serial == serial) {
            self.say_status(true);
        }
    }

    /// Brings the list of devices up to date.
    fn show_devices(&self) {
        let ui = self.ui.global::<ui::Devices>();
        let name = self.this_device().1;
        let jam = self.jam.borrow();
        ui.set_linked(jam.presence.is_some());
        let steering = jam.joined.as_ref().filter(|joined| joined.owner.is_some()).map(|joined| joined.device.0.clone());
        let mut list = vec![ui::Device { id: "".into(), name: name.into(), about: "This PC".into(), active: steering.is_none(), ready: true }];
        for (id, name, status) in &jam.devices {
            let title = status["title"].as_str().unwrap_or("");
            let about = match (status["steering"].as_str(), status["session"].is_string()) {
                (Some(other), _) => format!("Steering {other}"),
                (None, false) => "Getting ready…".to_string(),
                (None, true) if title.is_empty() => "Nothing playing".to_string(),
                (None, true) if status["playing"].as_bool() == Some(true) => format!("Playing {title}"),
                (None, true) => format!("Paused on {title}"),
            };
            list.push(ui::Device { id: id.into(), name: name.into(), about: about.into(), active: steering.as_deref() == Some(id.as_str()), ready: status["session"].is_string() });
        }
        ui.set_list(ModelRc::new(VecModel::from(list)));
    }

    /// A device was picked from the list. Another device: this one starts steering it, and
    /// if music is playing here it carries on over there. This PC: what the other device
    /// was playing carries on here.
    fn pick_device(self: &Rc<Self>, id: &str) {
        if id.is_empty() {
            return self.play_here();
        }
        let (target, address, word, already, in_jam) = {
            let jam = self.jam.borrow();
            let target = jam.devices.iter().find(|(other, _, _)| other == id).map(|(_, name, status)| (name.clone(), status["session"].as_str().map(str::to_string)));
            (
                target,
                jam.presence.as_ref().map(|presence| presence.line.base.clone()),
                jam.presence.as_ref().map(|presence| presence.word.clone()),
                jam.joined.as_ref().is_some_and(|joined| joined.owner.is_some() && joined.device.0 == id),
                jam.joined.as_ref().is_some_and(|joined| joined.owner.is_none()) || jam.hosting.as_ref().is_some_and(|hosting| hosting.public),
            )
        };
        if already {
            return;
        }
        if in_jam {
            return self.toast("Leave the jam before moving to another device");
        }
        let (Some((name, Some(session))), Some(address), Some(word)) = (target, address, word) else {
            return self.toast("That device isn't ready to be steered yet. Try again in a moment.");
        };
        // Music playing here moves over there; otherwise that device is steered as it is.
        let handover = {
            let playing = self.jam.borrow().joined.is_none() && self.playback.borrow().playing;
            playing.then(|| self.queue_to_hand_over())
        };
        let me = self.this_device().1;
        self.join_session(address, session, me, Some(Own { word, device: (id.to_string(), name), handover }));
    }

    /// Brings what another device is playing over to this one, and stops steering it.
    fn play_here(self: &Rc<Self>) {
        if self.jam.borrow().joined.as_ref().is_none_or(|joined| joined.owner.is_none()) {
            return;
        }
        self.jam_request("queue", json!({}), |ctl, answer| {
            let Ok(queue) = answer else { return ctl.leave_jam() };
            if queue["playing"].as_bool() != Some(true) {
                // Nothing was playing there to bring over: just let go of it.
                return ctl.leave_jam();
            }
            let (songs, index) = handed_songs(&queue);
            // The other device stops as this one starts.
            ctl.leave_jam_saying(Some(("pause", json!({}))));
            ctl.load_queue(songs, index, queue["position"].as_f64().unwrap_or(0.0), true, queue["context"].as_str().unwrap_or(""));
        });
    }

    pub fn wire_jam(self: &Rc<Self>) {
        let jam = self.ui.global::<ui::Jam>();
        jam.on_start(|name| {
            with(|ctl| {
                ctl.set_jam_name(&name);
                ctl.start_jam();
            });
        });
        jam.on_end(|| {
            with(|ctl| ctl.end_jam(false));
        });
        jam.on_remove_guest(|guest| {
            with(|ctl| ctl.remove_jam_guest(&guest));
        });
        jam.on_set_guests_control(|on| {
            with(|ctl| {
                ctl.settings.borrow_mut().jam_guests_control = on;
                ctl.save_settings();
                ctl.push_jam();
                ctl.jam_sync(true);
            });
        });
        jam.on_join(|link, name| {
            with(|ctl| ctl.join_jam(&link, &name));
        });
        jam.on_leave(|| {
            with(|ctl| ctl.leave_jam());
        });
        jam.on_search(|text| {
            with(|ctl| ctl.jam_search(&text));
        });
        jam.on_add(|song| {
            with(|ctl| ctl.jam_add(&song));
        });
        jam.on_opened(|| {
            with(|ctl| ctl.jam_problem(""));
        });
        self.ui.global::<ui::Devices>().on_choose(|id| {
            with(|ctl| ctl.pick_device(&id));
        });
        let settings = self.ui.global::<ui::Settings>();
        settings.on_set_jam_host(|on| {
            with(|ctl| {
                ctl.settings.borrow_mut().jam_host = on;
                ctl.save_settings();
                ctl.push_jam();
                if on {
                    ctl.check_jam();
                } else {
                    ctl.end_jam(false);
                }
            });
        });
        settings.on_set_link_devices(|on| {
            with(|ctl| {
                ctl.settings.borrow_mut().link_devices = on;
                ctl.save_settings();
                ctl.push_jam();
            });
        });
        settings.on_set_device_name(|name| {
            with(|ctl| {
                ctl.settings.borrow_mut().device_name = name.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(24).collect();
                ctl.save_settings();
                ctl.push_jam();
                ctl.show_devices();
            });
        });
        settings.on_save_jam_link(|link| {
            with(|ctl| ctl.save_jam_link(&link));
        });
        settings.on_check_jam(|| {
            with(|ctl| ctl.check_jam());
        });
    }
}

impl Joined {
    /// Where one of the jam's covers can be had: from the relay, which asks the host's app.
    fn cover(&self, song: &Value, size: &str) -> SharedString {
        match song["cover"].as_str().filter(|cover| !cover.is_empty()) {
            Some(cover) => format!("{}/api/jam/{}/cover/{size}/{}", self.line.base, self.id, crate::subsonic::encode(cover)).into(),
            None => SharedString::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_host_links() {
        assert_eq!(parse_host_link(" https://jam.example.org/#abcdefghijklmnop "), Ok(("https://jam.example.org".into(), "abcdefghijklmnop".into())));
        // The whole line from the relay's log, pasted as it stands.
        assert_eq!(parse_host_link("Host link, for Nexus Music's settings (keep it private): https://jam.example.org/#abcdefghijklmnop\n"), Ok(("https://jam.example.org".into(), "abcdefghijklmnop".into())));
        // Under a path on a shared address, and with no scheme typed.
        assert_eq!(parse_host_link("example.org/jam/#abcdefghijklmnop"), Ok(("https://example.org/jam".into(), "abcdefghijklmnop".into())));
        assert_eq!(parse_host_link("http://192.168.1.5:8080/#abcdefghijklmnop"), Ok(("http://192.168.1.5:8080".into(), "abcdefghijklmnop".into())));
        // No key, a key that's too short, and a key that would cross the internet in the clear.
        assert!(parse_host_link("https://jam.example.org/").is_err());
        assert!(parse_host_link("https://jam.example.org/#short").is_err());
        assert!(parse_host_link("http://jam.example.org/#abcdefghijklmnop").is_err());
        assert!(parse_host_link("#abcdefghijklmnop").is_err());
    }

    #[test]
    fn reads_jam_links() {
        assert_eq!(parse_jam_link("https://jam.example.org/j/abc234"), Ok(("https://jam.example.org".into(), "abc234".into())));
        assert_eq!(parse_jam_link(" jam.example.org/j/abc234/ "), Ok(("https://jam.example.org".into(), "abc234".into())));
        assert_eq!(parse_jam_link("https://example.org/jam/j/abc234?from=chat#top"), Ok(("https://example.org/jam".into(), "abc234".into())));
        assert_eq!(parse_jam_link("http://localhost:8080/j/abc234"), Ok(("http://localhost:8080".into(), "abc234".into())));
        // A host link isn't a jam link, and nor is anything else.
        assert!(parse_jam_link("https://jam.example.org/#abcdefghijklmnop").is_err());
        assert!(parse_jam_link("https://jam.example.org/j/").is_err());
        assert!(parse_jam_link("https://jam.example.org/j/../x").is_err());
        assert!(parse_jam_link("http://jam.example.org/j/abc234").is_err());
        assert!(parse_jam_link("abc234").is_err());
        assert_eq!(host_of("https://jam.example.org/sub"), "jam.example.org");
    }

    #[test]
    fn covers_are_cut_down_for_guests() {
        let picture = image::RgbImage::from_fn(900, 900, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(picture).write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let (small, kind) = cover_for_guests(png.clone(), false).unwrap();
        assert_eq!(kind, "image/jpeg");
        let cut = image::load_from_memory(&small).unwrap();
        assert_eq!((cut.width(), cut.height()), (128, 128));
        // A large one that's light enough goes as it is; a heavy one is cut to size.
        let (large, kind) = cover_for_guests(png.clone(), true).unwrap();
        if png.len() <= 300 * 1024 {
            assert_eq!((large.len(), kind), (png.len(), "image/png"));
        } else {
            assert_eq!(image::load_from_memory(&large).unwrap().width(), 640);
        }
        assert!(cover_for_guests(b"not a picture".to_vec(), false).is_none());
    }

    #[test]
    fn a_link_becomes_a_qr_code() {
        let code = qr_image("https://jam.example.org/j/abcdefghijkmnpqrstuvwx");
        let size = code.size();
        assert!(size.width >= 150 && size.width == size.height, "{size:?}");
    }

    #[test]
    fn describes_a_song_for_guests() {
        let song = Song { id: "s1".into(), title: "Blue".into(), artist: "A".into(), display_artist: "A & B".into(), album: "Sky".into(), cover_art: "al-1".into(), duration: 185, ..Default::default() };
        assert_eq!(song_json(&song), json!({"id": "s1", "title": "Blue", "artist": "A & B", "album": "Sky", "cover": "al-1", "duration": 185}));
    }
}
