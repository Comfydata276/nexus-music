//! A small client for the Subsonic / OpenSubsonic REST API that Navidrome exposes at `/rest`.
//!
//! Every call is blocking; callers run them on the worker pool, never on the UI thread.

use std::fmt;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;

const API_VERSION: &str = "1.16.1";
const CLIENT_NAME: &str = "NexusMusic";

#[derive(Debug, Clone)]
pub enum ApiError {
    /// The server could not be reached at all.
    Network(String),
    /// The server answered, but not with a Subsonic response (wrong URL, proxy page, …).
    NotSubsonic(String),
    /// The server understood the request and refused it.
    Server { code: i64, message: String },
}

impl ApiError {
    /// Whether the server turned the sign-in itself away (a wrong or unusable password or
    /// key), as against refusing one request or failing in some other way.
    pub fn sign_in_refused(&self) -> bool {
        matches!(self, ApiError::Server { code: 40..=44, .. })
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Network(m) => write!(f, "Can't reach the server. {m}"),
            ApiError::NotSubsonic(m) => {
                write!(f, "That address didn't answer like a Navidrome server. {m}")
            }
            ApiError::Server { code: 40, .. } => write!(f, "Wrong username or password."),
            ApiError::Server { code: 50, .. } => {
                write!(f, "This account isn't allowed to do that.")
            }
            ApiError::Server { code: 70, .. } => write!(f, "The server couldn't find that item."),
            ApiError::Server { message, .. } => write!(f, "The server reported: {message}"),
        }
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

// ---------------------------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReplayGain {
    pub track_gain: Option<f32>,
    pub album_gain: Option<f32>,
    pub track_peak: Option<f32>,
    pub album_peak: Option<f32>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Song {
    pub id: String,
    pub title: String,
    pub album: String,
    pub artist: String,
    pub album_id: String,
    pub artist_id: String,
    pub display_artist: String,
    /// Everyone credited on the song, when the server says (it may name only the first
    /// otherwise, in `artist`).
    pub artists: Vec<Credit>,
    pub track: u32,
    pub disc_number: u32,
    pub year: u32,
    pub cover_art: String,
    pub duration: u32,
    pub suffix: String,
    /// Size of the file on the server, in bytes.
    pub size: u64,
    pub bit_rate: u32,
    pub sampling_rate: u32,
    pub bit_depth: u32,
    pub genre: String,
    pub starred: Option<String>,
    /// When the song was added to the library.
    pub created: String,
    /// How many times it's been played, and when last.
    pub play_count: u32,
    pub played: String,
    /// The user's star rating, 1 to 5; 0 when unrated.
    pub user_rating: u8,
    pub replay_gain: Option<ReplayGain>,
    /// Set (by this app, not the server) on entries that stand for an internet radio station.
    pub radio_url: String,
    /// Set (by this app) on a queue entry that was put there by hand and hasn't played yet:
    /// it's in "Next in queue", ahead of the rest of whatever list is playing.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub queued: bool,
}

/// An artist named on a song.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Credit {
    pub id: String,
    pub name: String,
}

impl Song {
    pub fn artist_label(&self) -> &str {
        if self.display_artist.is_empty() { &self.artist } else { &self.display_artist }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub artist: String,
    pub artist_id: String,
    pub display_artist: String,
    pub cover_art: String,
    pub song_count: u32,
    pub duration: u32,
    pub year: u32,
    pub genre: String,
    pub starred: Option<String>,
    /// When it was last played.
    pub played: String,
    pub release_types: Vec<String>,
    #[serde(rename = "song")]
    pub songs: Vec<Song>,
}

impl Album {
    pub fn artist_label(&self) -> &str {
        if self.display_artist.is_empty() { &self.artist } else { &self.display_artist }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Artist {
    pub id: String,
    pub name: String,
    pub cover_art: String,
    pub album_count: u32,
    pub starred: Option<String>,
    #[serde(rename = "album")]
    pub albums: Vec<Album>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ArtistInfo {
    pub biography: String,
    /// MusicBrainz's name for the artist, where the server knows it.
    pub music_brainz_id: String,
    #[serde(rename = "similarArtist")]
    pub similar: Vec<Artist>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub comment: String,
    pub owner: String,
    pub song_count: u32,
    pub duration: u32,
    pub cover_art: String,
    #[serde(rename = "entry")]
    pub songs: Vec<Song>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScanStatus {
    pub scanning: bool,
    pub count: u64,
    pub last_scan: String,
}

/// A play queue saved on the server.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PlayQueue {
    #[serde(rename = "entry")]
    pub songs: Vec<Song>,
    /// The id of the song that was playing. (Some servers send a number.)
    #[serde(deserialize_with = "text_or_number")]
    pub current: String,
    /// How far into that song, in milliseconds.
    pub position: u64,
    /// When it was saved, and by which app.
    pub changed: String,
    pub changed_by: String,
}

fn text_or_number<'de, D: serde::Deserializer<'de>>(from: D) -> Result<String, D::Error> {
    Ok(match serde_json::Value::deserialize(from)? {
        serde_json::Value::String(text) => text,
        serde_json::Value::Number(number) => number.to_string(),
        _ => String::new(),
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Genre {
    #[serde(rename = "value")]
    pub name: String,
    #[serde(rename = "albumCount")]
    pub album_count: u32,
    #[serde(rename = "songCount")]
    pub song_count: u32,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Found {
    #[serde(rename = "artist")]
    pub artists: Vec<Artist>,
    #[serde(rename = "album")]
    pub albums: Vec<Album>,
    #[serde(rename = "song")]
    pub songs: Vec<Song>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct LyricLine {
    /// Milliseconds from the start of the track; absent for unsynced lyrics.
    pub start: Option<i64>,
    pub value: String,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default)]
pub struct Lyrics {
    pub synced: bool,
    pub line: Vec<LyricLine>,
}

#[derive(Debug, Clone, Default)]
pub struct ServerInfo {
    pub version: String,
    /// Which server it is ("navidrome"), where it says.
    pub kind: String,
}

impl ServerInfo {
    /// Whether the song count `getScanStatus` gives is the number of songs in the library.
    /// Navidrome's is from 0.55 on, outside a scan: before then it was how many files the
    /// last scan went through. (It counts every library on the server.)
    pub fn counts_songs(&self) -> bool {
        let mut numbers = self.version.split(|c: char| !c.is_ascii_digit()).filter(|n| !n.is_empty()).map(|n| n.parse::<u32>().unwrap_or(0));
        let (major, minor) = (numbers.next().unwrap_or(0), numbers.next().unwrap_or(0));
        self.kind.eq_ignore_ascii_case("navidrome") && (major, minor) >= (0, 55)
    }
}

/// Which list `getAlbumList2` should return.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub enum AlbumList {
    Newest,
    Recent,
    Frequent,
    Random,
    Starred,
    AlphabeticalByName,
    AlphabeticalByArtist,
    ByYearDescending,
    ByGenre(String),
}

/// Lyric lines with the file's own offset taken off their times, so everything after this
/// can take the times as they are.
fn shifted(mut lines: Vec<LyricLine>, offset: i64) -> Vec<LyricLine> {
    if offset != 0 {
        for line in &mut lines {
            line.start = line.start.map(|at| (at - offset).max(0));
        }
    }
    lines
}

// ---------------------------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------------------------

struct Inner {
    base: String,
    username: String,
    salt: String,
    token: String,
    /// One agent for everything, so API calls, artwork and audio share warm connections.
    agent: ureq::Agent,
    /// Offline mode: every request fails at once instead of going out.
    offline: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

/// Tidies what the user typed into a base URL: adds a scheme (in small letters), drops
/// trailing slashes and any `/app/...` path copied from the browser's address bar, and the
/// `/rest` the API lives under.
pub fn normalize_server_url(input: &str) -> String {
    let mut url = input.trim().to_string();
    if url.is_empty() {
        return url;
    }
    url = match split_scheme(&url) {
        // "HTTP://" and "http://" are the same server, and the same account.
        Some((scheme, rest)) => format!("{scheme}://{rest}"),
        None => format!("http://{url}"),
    };
    if let Some(pos) = url.find("/app/") {
        url.truncate(pos);
    }
    // Trailing slashes first, so ".../rest/" loses its "/rest" as well, and the first
    // request doesn't go to ".../rest/rest/ping".
    let trim = |url: &mut String| {
        while url.ends_with('/') || url.ends_with('#') {
            url.pop();
        }
    };
    trim(&mut url);
    for suffix in ["/app", "/rest"] {
        if url.ends_with(suffix) {
            url.truncate(url.len() - suffix.len());
        }
    }
    trim(&mut url);
    url
}

/// The scheme an address starts with, in small letters, and the rest after its "://"; or
/// nothing if it doesn't start with one.
fn split_scheme(address: &str) -> Option<(String, &str)> {
    let (scheme, rest) = address.split_once("://")?;
    // As RFC 3986 has it: a letter, then letters, digits, "+", "-" or ".".
    let valid = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c));
    valid.then(|| (scheme.to_ascii_lowercase(), rest))
}

/// The scheme an address names ("http", "https" or another), in small letters whatever case
/// it was typed in.
pub fn scheme_of(address: &str) -> Option<String> {
    split_scheme(address.trim()).map(|(scheme, _)| scheme)
}

/// The host an address names, in small letters, without a port, a user name or brackets.
fn host_of(address: &str) -> String {
    let rest = split_scheme(address).map_or(address, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    // An IPv6 address is in brackets, with the port after them.
    match host.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    }
    .to_lowercase()
}

/// The addresses to try for what was typed as the server's. One that names its scheme
/// ("http://", "https://", in any case) is taken at its word. One that doesn't is tried over
/// HTTPS, and over plain HTTP as well only when it is on the home network: the sign-in never
/// goes unencrypted beyond it unless the person typed "http://" themselves.
pub fn addresses_to_try(typed: &str) -> Vec<String> {
    let typed = typed.trim();
    if typed.is_empty() {
        Vec::new()
    } else if split_scheme(typed).is_some() {
        vec![typed.to_string()]
    } else if unencrypted_beyond_home(&format!("http://{typed}")) {
        vec![format!("https://{typed}")]
    } else {
        vec![format!("https://{typed}"), format!("http://{typed}")]
    }
}

/// Said after an error when only HTTPS was tried, and only because no scheme was typed.
pub const TRY_PLAIN_HTTP: &str = "If your server really has no HTTPS, type http:// in front of the address.";

/// Whether an address is plain HTTP to somewhere that isn't on the home network (or this
/// PC): the case where what's sent can be read by others on the way.
pub fn unencrypted_beyond_home(address: &str) -> bool {
    let address = address.trim();
    if split_scheme(address).is_none_or(|(scheme, _)| scheme != "http") {
        return false;
    }
    let host = host_of(address);
    if let Ok(ip) = host.parse::<std::net::Ipv4Addr>() {
        let [a, b, ..] = ip.octets();
        // Private ranges, this PC, link-local, and the range Tailscale and similar use.
        return !(ip.is_private() || ip.is_loopback() || ip.is_link_local() || (a == 100 && (64..128).contains(&b)));
    }
    if let Ok(ip) = host.parse::<std::net::Ipv6Addr>() {
        return !(ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00 || (ip.segments()[0] & 0xffc0) == 0xfe80);
    }
    let local = ["localhost", ".local", ".lan", ".home", ".internal", ".home.arpa", ".localdomain"];
    !(host.is_empty() || !host.contains('.') || local.iter().any(|ending| host == ending.trim_start_matches('.') || host.ends_with(ending)))
}

/// Longest an API call may take from start to finish. Audio downloads have no such limit.
const API_TIMEOUT: Duration = Duration::from_secs(45);
/// Most redirects one request to the music server is taken through.
const MAX_REDIRECTS: u32 = 10;

/// Where a redirect's `Location` points, read against the address that gave it.
fn redirect_target(from: &str, location: &str) -> String {
    let location = location.trim();
    if split_scheme(location).is_some() {
        return location.to_string();
    }
    let (scheme, rest) = split_scheme(from).unwrap_or_else(|| ("http".to_string(), from));
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if let Some(elsewhere) = location.strip_prefix("//") {
        format!("{scheme}://{elsewhere}")
    } else if location.starts_with('/') {
        format!("{scheme}://{authority}{location}")
    } else {
        // Beside the address it came from: in the same folder.
        let path = rest[authority.len()..].split(['?', '#']).next().unwrap_or_default();
        let folder = path.rsplit_once('/').map_or("", |(folder, _)| folder);
        format!("{scheme}://{authority}{folder}/{location}")
    }
}

/// Why a redirect from `from` to `to` isn't followed by a request that carries the sign-in,
/// if it isn't: nothing goes from HTTPS down to plain HTTP, and a plain-HTTP request goes on
/// only to the same server (over HTTP or HTTPS), so the sign-in isn't handed to another.
pub fn redirect_refused(from: &str, to: &str) -> Option<&'static str> {
    let from_scheme = split_scheme(from).map(|(scheme, _)| scheme);
    let to_scheme = split_scheme(to).map(|(scheme, _)| scheme);
    match (from_scheme.as_deref(), to_scheme.as_deref()) {
        (Some("https"), Some("https")) => None,
        (Some("http"), Some("http" | "https")) if host_of(from) == host_of(to) => None,
        (Some("https"), _) => Some(
            "The server sent the app from its HTTPS address on to a plain-HTTP one, where your sign-in could be read on the way, so the app didn't go.",
        ),
        _ => Some("The server sent the app on to another address over plain HTTP, so the app didn't go, and your sign-in wasn't passed on."),
    }
}

pub(crate) fn make_agent() -> ureq::Agent {
    agent_following(10, Duration::from_secs(300))
}

/// For a server that closes idle connections sooner than most: one kept longer than `idle`
/// isn't used again.
pub(crate) fn make_agent_idle(idle: Duration) -> ureq::Agent {
    agent_following(10, idle)
}

/// For callers that need to see where a redirect points instead of being taken there. The
/// music server's client is one: it follows redirects itself, and only safe ones (see
/// `redirect_refused`), since its requests carry the sign-in.
pub(crate) fn make_agent_without_redirects() -> ureq::Agent {
    agent_following(0, Duration::from_secs(300))
}

fn agent_following(redirects: u32, idle: Duration) -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    use ureq::unversioned::transport::DefaultConnector;
    let tls = TlsConfig::builder()
        .provider(TlsProvider::NativeTls)
        .root_certs(RootCerts::PlatformVerifier)
        .build();
    let config = ureq::Agent::config_builder()
        .tls_config(tls)
        .http_status_as_error(false)
        .max_redirects(redirects)
        .max_redirects_will_error(false)
        .no_delay(true)
        .timeout_connect(Some(Duration::from_secs(8)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        // Setting up a connection is the expensive part of a request, so keep plenty of them
        // open, and for minutes, not seconds: enough for artwork to load many at a time.
        .max_idle_connections(24)
        .max_idle_connections_per_host(24)
        .max_idle_age(idle)
        .user_agent(concat!("NexusMusic/", env!("CARGO_PKG_VERSION")))
        .build();
    ureq::Agent::with_parts(config, DefaultConnector::new(), dns::CachingResolver::default())
}

/// Name lookups, remembered.
///
/// A lookup for a home-network name such as `tower` or `tower.local` can take a second or
/// more on Windows, and by default happens for every new connection. This resolver asks once,
/// works out which of the returned addresses actually answers fastest, and reuses that.
mod dns {
    use std::collections::HashMap;
    use std::net::{SocketAddr, TcpStream};
    use std::sync::Mutex;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use ureq::Error;
    use ureq::config::Config;
    use ureq::http::Uri;
    use ureq::unversioned::resolver::{DefaultResolver, ResolvedSocketAddrs, Resolver};
    use ureq::unversioned::transport::NextTimeout;

    const TTL: Duration = Duration::from_secs(600);

    static CACHE: Mutex<Option<HashMap<String, (Instant, Vec<SocketAddr>)>>> = Mutex::new(None);

    /// Forgets remembered addresses, e.g. after a request fails because the server moved.
    pub fn forget() {
        *CACHE.lock().unwrap() = None;
    }

    #[derive(Debug, Default)]
    pub struct CachingResolver {
        inner: DefaultResolver,
    }

    impl Resolver for CachingResolver {
        fn resolve(
            &self,
            uri: &Uri,
            config: &Config,
            timeout: NextTimeout,
        ) -> Result<ResolvedSocketAddrs, Error> {
            let key = match (uri.scheme(), uri.authority()) {
                (Some(scheme), Some(authority)) => DefaultResolver::host_and_port(scheme, authority),
                _ => None,
            };
            if let Some(key) = &key {
                let cache = CACHE.lock().unwrap();
                let hit = cache.as_ref().and_then(|map| map.get(key)).filter(|(at, _)| at.elapsed() < TTL);
                if let Some((_, addrs)) = hit {
                    let mut out = self.empty();
                    for addr in addrs {
                        out.push(*addr);
                    }
                    return Ok(out);
                }
            }
            let resolved = self.inner.resolve(uri, config, timeout)?;
            let ordered = fastest_first(resolved.iter().copied().collect());
            let mut out = self.empty();
            for addr in &ordered {
                out.push(*addr);
            }
            if let Some(key) = key {
                CACHE.lock().unwrap().get_or_insert_with(HashMap::new).insert(key, (Instant::now(), ordered));
            }
            Ok(out)
        }
    }

    /// Puts the address that accepts a connection soonest at the front. A name often resolves
    /// to an IPv6 address that goes nowhere ahead of the IPv4 one that works; trying them in
    /// order would wait out a timeout on every new connection.
    fn fastest_first(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
        if addrs.len() < 2 {
            return addrs;
        }
        let (tx, rx) = mpsc::channel();
        for addr in addrs.iter().copied() {
            let tx = tx.clone();
            std::thread::spawn(move || {
                if TcpStream::connect_timeout(&addr, Duration::from_secs(3)).is_ok() {
                    let _ = tx.send(addr);
                }
            });
        }
        drop(tx);
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(winner) => {
                let mut ordered = vec![winner];
                ordered.extend(addrs.into_iter().filter(|a| *a != winner));
                ordered
            }
            Err(_) => addrs,
        }
    }
}

fn random_salt() -> String {
    (0..16).map(|_| fastrand::alphanumeric()).collect()
}

pub fn encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 8);
    for b in input.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl Client {
    pub fn new(server: &str, username: &str, password: &str) -> Client {
        let salt = random_salt();
        let digest = Md5::digest(format!("{password}{salt}").as_bytes());
        let token: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        Client {
            inner: Arc::new(Inner {
                base: normalize_server_url(server),
                username: username.to_string(),
                salt,
                token,
                agent: make_agent_without_redirects(),
                offline: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    /// In offline mode nothing is sent: requests fail straight away.
    pub fn set_offline(&self, offline: bool) {
        self.inner.offline.store(offline, std::sync::atomic::Ordering::Relaxed);
    }

    fn check_online(&self) -> ApiResult<()> {
        if self.inner.offline.load(std::sync::atomic::Ordering::Relaxed) {
            Err(ApiError::Network("Offline mode is on.".into()))
        } else {
            Ok(())
        }
    }

    pub fn server(&self) -> &str {
        &self.inner.base
    }

    pub fn username(&self) -> &str {
        &self.inner.username
    }

    fn url(&self, endpoint: &str, params: &[(&str, &str)]) -> String {
        self.url_as(CLIENT_NAME, endpoint, params)
    }

    /// `app` is the name the request gives for this app.
    fn url_as(&self, app: &str, endpoint: &str, params: &[(&str, &str)]) -> String {
        let i = &self.inner;
        let mut url = format!(
            "{}/rest/{endpoint}?u={}&t={}&s={}&v={API_VERSION}&c={}&f=json",
            i.base,
            encode(&i.username),
            i.token,
            i.salt,
            encode(app)
        );
        for (k, v) in params {
            url.push('&');
            url.push_str(k);
            url.push('=');
            url.push_str(&encode(v));
        }
        url
    }

    /// URL the audio engine downloads a track from. `max_bitrate` of 0 asks for the original
    /// file; anything else has the server transcode to MP3 at that rate.
    pub fn stream_url(&self, id: &str, max_bitrate: u32, offset_secs: u32) -> String {
        let bitrate = max_bitrate.to_string();
        let offset = offset_secs.to_string();
        let mut params = vec![("id", id)];
        if max_bitrate > 0 {
            params.push(("format", "mp3"));
            params.push(("maxBitRate", &bitrate));
            if offset_secs > 0 {
                params.push(("timeOffset", &offset));
            }
        } else {
            params.push(("format", "raw"));
        }
        self.url("stream", &params)
    }

    pub fn cover_url(&self, id: &str, size: u32) -> String {
        self.url("getCoverArt", &[("id", id), ("size", &size.to_string())])
    }

    fn call<T: DeserializeOwned + Default>(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
        field: Option<&str>,
    ) -> ApiResult<T> {
        self.call_as(CLIENT_NAME, endpoint, params, field)
    }

    fn call_as<T: DeserializeOwned + Default>(
        &self,
        app: &str,
        endpoint: &str,
        params: &[(&str, &str)],
        field: Option<&str>,
    ) -> ApiResult<T> {
        let url = self.url_as(app, endpoint, params);
        // A long list of song ids can outgrow what servers and proxies accept in an address,
        // so those go in the body of a POST instead (the OpenSubsonic "formPost" extension).
        let response = match url.split_once('?') {
            Some((address, form)) if url.len() > 1800 => self.post_form(address, form)?,
            _ => self.get(&url, Some(API_TIMEOUT))?,
        };
        let status = response.status().as_u16();
        let reader = response.into_body().into_reader();
        let mut root: serde_json::Value = serde_json::from_reader(reader).map_err(|_| {
            ApiError::NotSubsonic(format!("Check the address (HTTP status {status})."))
        })?;
        let Some(body) = root.get_mut("subsonic-response") else {
            return Err(ApiError::NotSubsonic(format!("Check the address (HTTP status {status}).")));
        };
        if body.get("status").and_then(|s| s.as_str()) != Some("ok") {
            let err = body.get("error");
            return Err(ApiError::Server {
                code: err.and_then(|e| e.get("code")).and_then(|c| c.as_i64()).unwrap_or(0),
                message: err
                    .and_then(|e| e.get("message"))
                    .and_then(|m| m.as_str())
                    .unwrap_or("unknown error")
                    .to_string(),
            });
        }
        let value = match field {
            Some(field) => match body.get_mut(field) {
                Some(v) => v.take(),
                // An empty library section is simply omitted from the response.
                None => return Ok(T::default()),
            },
            None => body.take(),
        };
        serde_json::from_value(value)
            .map_err(|e| ApiError::NotSubsonic(format!("Unexpected response from {endpoint}: {e}")))
    }

    /// Sends a request with `send`, and again wherever the answer redirects it to. The agent
    /// follows no redirects by itself: one asked of the music server carries the sign-in, so
    /// only the redirects `redirect_refused` allows are followed. Anything else (a radio
    /// station's stream, say) is followed wherever it goes, as it carries nothing.
    fn following(
        &self,
        url: &str,
        mut send: impl FnMut(&str) -> ApiResult<ureq::http::Response<ureq::Body>>,
    ) -> ApiResult<ureq::http::Response<ureq::Body>> {
        let signed = url.starts_with(&self.inner.base);
        let mut url = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            let response = send(&url)?;
            let location = response.headers().get("location").and_then(|v| v.to_str().ok());
            let (true, Some(location)) = (response.status().is_redirection(), location) else {
                return Ok(response);
            };
            let next = redirect_target(&url, location);
            if let Some(reason) = signed.then(|| redirect_refused(&url, &next)).flatten() {
                return Err(ApiError::Network(reason.into()));
            }
            url = next;
        }
        Err(ApiError::Network("The server kept sending the app on from one address to another.".into()))
    }

    fn get(
        &self,
        url: &str,
        overall: Option<Duration>,
    ) -> ApiResult<ureq::http::Response<ureq::Body>> {
        self.check_online()?;
        self.following(url, |url| {
            self.inner.agent.get(url).config().timeout_global(overall).build().call().map_err(|e| {
                // The server may have moved; look its address up afresh next time.
                dns::forget();
                ApiError::Network(network_hint(&e))
            })
        })
    }

    /// A redirected form is sent on as it was: a server that moves its address means the
    /// request to go there, not to arrive without its contents.
    fn post_form(&self, address: &str, form: &str) -> ApiResult<ureq::http::Response<ureq::Body>> {
        self.check_online()?;
        self.following(address, |address| {
            self.inner
                .agent
                .post(address)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .config()
                .timeout_global(Some(API_TIMEOUT))
                .build()
                .send(form)
                .map_err(|e| ApiError::Network(network_hint(&e)))
        })
    }

    /// Opens connections ahead of need, so the first clicks don't pay for setting them up.
    pub fn warm_up(&self, connections: usize) {
        std::thread::scope(|scope| {
            for _ in 0..connections {
                scope.spawn(|| {
                    let _ = self.ping();
                });
            }
        });
    }

    /// Times a request on a brand-new connection and one on a reused connection, in
    /// milliseconds. The gap between them is what connection reuse saves.
    pub fn measure(&self) -> ApiResult<(u128, u128)> {
        let fresh = Client {
            inner: Arc::new(Inner {
                base: self.inner.base.clone(),
                username: self.inner.username.clone(),
                salt: self.inner.salt.clone(),
                token: self.inner.token.clone(),
                agent: make_agent_without_redirects(),
                offline: std::sync::atomic::AtomicBool::new(false),
            }),
        };
        dns::forget();
        let started = std::time::Instant::now();
        fresh.ping()?;
        let cold = started.elapsed().as_millis();
        let started = std::time::Instant::now();
        fresh.ping()?;
        Ok((cold, started.elapsed().as_millis()))
    }

    /// Whether anything at all answers at this address within `limit`. For trying an address
    /// that's only a guess without waiting long on it.
    pub fn answers_within(&self, limit: Duration) -> bool {
        self.get(&self.url_as(CLIENT_NAME, "ping", &[]), Some(limit)).is_ok()
    }

    pub fn ping(&self) -> ApiResult<ServerInfo> {
        let v: serde_json::Value = self.call("ping", &[], None)?;
        let text = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
        Ok(ServerInfo { version: text("serverVersion"), kind: text("type") })
    }

    pub fn album_list(&self, list: &AlbumList, size: u32, offset: u32) -> ApiResult<Vec<Album>> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Wrap {
            album: Vec<Album>,
        }
        let size = size.to_string();
        let offset = offset.to_string();
        let mut params = vec![("size", size.as_str()), ("offset", offset.as_str())];
        let kind = match list {
            AlbumList::Newest => "newest",
            AlbumList::Recent => "recent",
            AlbumList::Frequent => "frequent",
            AlbumList::Random => "random",
            AlbumList::Starred => "starred",
            AlbumList::AlphabeticalByName => "alphabeticalByName",
            AlbumList::AlphabeticalByArtist => "alphabeticalByArtist",
            AlbumList::ByYearDescending => {
                params.push(("fromYear", "9999"));
                params.push(("toYear", "0"));
                "byYear"
            }
            AlbumList::ByGenre(genre) => {
                params.push(("genre", genre.as_str()));
                "byGenre"
            }
        };
        params.push(("type", kind));
        Ok(self.call::<Wrap>("getAlbumList2", &params, Some("albumList2"))?.album)
    }

    pub fn album(&self, id: &str) -> ApiResult<Album> {
        self.call("getAlbum", &[("id", id)], Some("album"))
    }

    pub fn artists(&self) -> ApiResult<Vec<Artist>> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Index {
            artist: Vec<Artist>,
        }
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Wrap {
            index: Vec<Index>,
        }
        let wrap: Wrap = self.call("getArtists", &[], Some("artists"))?;
        Ok(wrap.index.into_iter().flat_map(|i| i.artist).collect())
    }

    pub fn artist(&self, id: &str) -> ApiResult<Artist> {
        self.call("getArtist", &[("id", id)], Some("artist"))
    }

    pub fn artist_info(&self, id: &str) -> ApiResult<ArtistInfo> {
        self.call("getArtistInfo2", &[("id", id), ("count", "12")], Some("artistInfo2"))
    }

    /// The names of artists like this one that the library doesn't have. The server asks
    /// whatever it's been set up with (Last.fm, Deezer), so this can take seconds, and
    /// comes back empty if it has nowhere to ask.
    pub fn similar_outside(&self, id: &str) -> ApiResult<Vec<String>> {
        let info: ArtistInfo = self.call("getArtistInfo2", &[("id", id), ("count", "20"), ("includeNotPresent", "true")], Some("artistInfo2"))?;
        // One that isn't in the library has no id of its own: none, or "-1".
        Ok(info.similar.into_iter().filter(|artist| artist.id.is_empty() || artist.id.starts_with('-')).map(|artist| artist.name).filter(|name| !name.is_empty()).collect())
    }

    pub fn top_songs(&self, artist_name: &str, count: u32) -> ApiResult<Vec<Song>> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Wrap {
            song: Vec<Song>,
        }
        let count = count.to_string();
        Ok(self
            .call::<Wrap>(
                "getTopSongs",
                &[("artist", artist_name), ("count", &count)],
                Some("topSongs"),
            )?
            .song)
    }

    pub fn similar_songs(&self, id: &str, count: u32) -> ApiResult<Vec<Song>> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Wrap {
            song: Vec<Song>,
        }
        let count = count.to_string();
        Ok(self
            .call::<Wrap>("getSimilarSongs2", &[("id", id), ("count", &count)], Some("similarSongs2"))?
            .song)
    }

    /// Random songs, optionally from one genre.
    pub fn random_songs(&self, count: u32, genre: Option<&str>) -> ApiResult<Vec<Song>> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Wrap {
            song: Vec<Song>,
        }
        let count = count.to_string();
        let mut params = vec![("size", count.as_str())];
        if let Some(genre) = genre.filter(|g| !g.is_empty()) {
            params.push(("genre", genre));
        }
        Ok(self.call::<Wrap>("getRandomSongs", &params, Some("randomSongs"))?.song)
    }

    pub fn genres(&self) -> ApiResult<Vec<Genre>> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Wrap {
            genre: Vec<Genre>,
        }
        Ok(self.call::<Wrap>("getGenres", &[], Some("genres"))?.genre)
    }

    pub fn playlists(&self) -> ApiResult<Vec<Playlist>> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Wrap {
            playlist: Vec<Playlist>,
        }
        Ok(self.call::<Wrap>("getPlaylists", &[], Some("playlists"))?.playlist)
    }

    pub fn playlist(&self, id: &str) -> ApiResult<Playlist> {
        self.call("getPlaylist", &[("id", id)], Some("playlist"))
    }

    pub fn create_playlist(&self, name: &str, song_ids: &[String]) -> ApiResult<Playlist> {
        let mut params = vec![("name", name)];
        params.extend(song_ids.iter().map(|id| ("songId", id.as_str())));
        self.call("createPlaylist", &params, Some("playlist"))
    }

    /// Replaces a playlist's comment, and keeps the playlist private.
    pub fn set_playlist_comment(&self, id: &str, comment: &str) -> ApiResult<()> {
        let params = [("playlistId", id), ("comment", comment), ("public", "false")];
        self.call::<serde_json::Value>("updatePlaylist", &params, None).map(drop)
    }

    pub fn song(&self, id: &str) -> ApiResult<Song> {
        self.call("getSong", &[("id", id)], Some("song"))
    }

    /// Saves the play queue on the server, where other devices can pick it up. `device`
    /// is recorded as who saved it. `position` is in milliseconds.
    pub fn save_play_queue(&self, device: &str, ids: &[String], current: &str, position: u64) -> ApiResult<()> {
        let position = position.to_string();
        let mut params: Vec<(&str, &str)> = ids.iter().map(|id| ("id", id.as_str())).collect();
        params.push(("current", current));
        params.push(("position", &position));
        self.call_as::<serde_json::Value>(device, "savePlayQueue", &params, None).map(drop)
    }

    /// The play queue last saved on the server; empty if there isn't one.
    pub fn play_queue(&self) -> ApiResult<PlayQueue> {
        self.call("getPlayQueue", &[], Some("playQueue"))
    }

    pub fn rename_playlist(&self, id: &str, name: &str) -> ApiResult<()> {
        self.call::<serde_json::Value>("updatePlaylist", &[("playlistId", id), ("name", name)], None)
            .map(drop)
    }

    pub fn add_to_playlist(&self, id: &str, song_ids: &[String]) -> ApiResult<()> {
        let mut params = vec![("playlistId", id)];
        params.extend(song_ids.iter().map(|s| ("songIdToAdd", s.as_str())));
        self.call::<serde_json::Value>("updatePlaylist", &params, None).map(drop)
    }

    /// Replaces a playlist's songs with `song_ids`, in that order.
    pub fn replace_playlist(&self, id: &str, song_ids: &[String]) -> ApiResult<()> {
        let mut params = vec![("playlistId", id)];
        params.extend(song_ids.iter().map(|s| ("songId", s.as_str())));
        self.call::<serde_json::Value>("createPlaylist", &params, None).map(drop)
    }

    /// Removes the songs at these positions from a playlist, in one request.
    pub fn remove_many_from_playlist(&self, id: &str, positions: &[usize]) -> ApiResult<()> {
        let positions: Vec<String> = positions.iter().map(|p| p.to_string()).collect();
        let mut params = vec![("playlistId", id)];
        params.extend(positions.iter().map(|p| ("songIndexToRemove", p.as_str())));
        self.call::<serde_json::Value>("updatePlaylist", &params, None).map(drop)
    }

    pub fn delete_playlist(&self, id: &str) -> ApiResult<()> {
        self.call::<serde_json::Value>("deletePlaylist", &[("id", id)], None).map(drop)
    }

    pub fn starred(&self) -> ApiResult<Found> {
        self.call("getStarred2", &[], Some("starred2"))
    }

    /// Stars or unstars a song, album or artist. `kind` is the parameter name the API expects:
    /// `id` for songs, `albumId` or `artistId` otherwise.
    pub fn set_starred(&self, kind: &str, id: &str, starred: bool) -> ApiResult<()> {
        let endpoint = if starred { "star" } else { "unstar" };
        self.call::<serde_json::Value>(endpoint, &[(kind, id)], None).map(drop)
    }

    /// Records that a song was played at an earlier time (milliseconds since 1970).
    pub fn scrobble_at(&self, id: &str, at: i64) -> ApiResult<()> {
        let at = at.to_string();
        self.call::<serde_json::Value>("scrobble", &[("id", id), ("submission", "true"), ("time", &at)], None)
            .map(drop)
    }

    /// Rates a song from 1 to 5 stars; 0 removes the rating.
    pub fn set_rating(&self, id: &str, rating: u8) -> ApiResult<()> {
        let rating = rating.min(5).to_string();
        self.call::<serde_json::Value>("setRating", &[("id", id), ("rating", &rating)], None).map(drop)
    }

    /// The internet radio stations set up on the server, as playable entries.
    pub fn radio_stations(&self) -> ApiResult<Vec<Song>> {
        #[derive(Deserialize, Default)]
        #[serde(rename_all = "camelCase", default)]
        struct Station {
            id: String,
            name: String,
            stream_url: String,
            home_page_url: String,
        }
        #[derive(Deserialize, Default)]
        #[serde(rename_all = "camelCase", default)]
        struct Stations {
            internet_radio_station: Vec<Station>,
        }
        let stations: Stations = self.call("getInternetRadioStations", &[], Some("internetRadioStations"))?;
        Ok(stations
            .internet_radio_station
            .into_iter()
            .filter(|station| !station.stream_url.is_empty())
            .map(|station| {
                // Shown where a song's artist would be: the station's site, without the clutter.
                let site = station
                    .home_page_url
                    .trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .trim_start_matches("www.")
                    .trim_end_matches('/')
                    .to_string();
                Song {
                    id: format!("radio:{}", station.id),
                    title: station.name,
                    artist: if site.is_empty() { "Internet radio".into() } else { site },
                    album: "Internet radio".into(),
                    radio_url: station.stream_url,
                    suffix: "mp3".into(),
                    ..Default::default()
                }
            })
            .collect())
    }

    pub fn search(&self, query: &str, artists: u32, albums: u32, songs: u32) -> ApiResult<Found> {
        let (a, b, c) = (artists.to_string(), albums.to_string(), songs.to_string());
        self.call(
            "search3",
            &[("query", query), ("artistCount", &a), ("albumCount", &b), ("songCount", &c)],
            Some("searchResult3"),
        )
    }

    /// One page of everything in the library: up to this many artists, albums and songs,
    /// starting `offset` in to each.
    pub fn library_page(&self, artists: u32, albums: u32, songs: u32, offset: u32) -> ApiResult<Found> {
        let (a, b, c, o) = (artists.to_string(), albums.to_string(), songs.to_string(), offset.to_string());
        self.call(
            "search3",
            &[
                ("query", ""),
                ("artistCount", &a),
                ("albumCount", &b),
                ("songCount", &c),
                ("artistOffset", &o),
                ("albumOffset", &o),
                ("songOffset", &o),
            ],
            Some("searchResult3"),
        )
    }

    /// How many songs the server has, and when it last looked through its folders.
    pub fn scan_status(&self) -> ApiResult<ScanStatus> {
        self.call("getScanStatus", &[], Some("scanStatus"))
    }

    pub fn scrobble(&self, id: &str, submission: bool) -> ApiResult<()> {
        let submission = if submission { "true" } else { "false" };
        self.call::<serde_json::Value>("scrobble", &[("id", id), ("submission", submission)], None)
            .map(drop)
    }

    pub fn lyrics(&self, song_id: &str) -> ApiResult<Option<Lyrics>> {
        #[derive(Deserialize, Default)]
        #[serde(default, rename_all = "camelCase")]
        struct Wrap {
            structured_lyrics: Vec<Offset>,
        }
        /// A set of lyrics as the server sends it, with the shift a lyrics file can ask for.
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Offset {
            synced: bool,
            line: Vec<LyricLine>,
            /// In milliseconds: positive shows every line that much sooner.
            offset: i64,
        }
        let wrap: Wrap = self.call("getLyricsBySongId", &[("id", song_id)], Some("lyricsList"))?;
        let mut all: Vec<Lyrics> = wrap.structured_lyrics.into_iter().map(|set| Lyrics { synced: set.synced, line: shifted(set.line, set.offset) }).collect();
        // Prefer a synced set when the file carries both.
        all.sort_by_key(|l| !l.synced);
        Ok(all.into_iter().find(|l| !l.line.is_empty()))
    }

    /// Fetches a small binary resource such as cover art.
    pub fn fetch_bytes(&self, url: &str, limit: u64) -> ApiResult<Vec<u8>> {
        let mut response = self.get(url, Some(API_TIMEOUT))?;
        if !response.status().is_success() {
            return Err(ApiError::Network(format!("HTTP status {}.", response.status().as_u16())));
        }
        response
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|e| ApiError::Network(e.to_string()))
    }

    /// Opens an audio stream, optionally starting at a byte offset (and ending at an inclusive
    /// end offset). Returns the body reader and the number of bytes the response will carry,
    /// when the server knows it.
    pub fn open_stream(
        &self,
        url: &str,
        range: Option<(u64, Option<u64>)>,
    ) -> ApiResult<(Box<dyn Read + Send>, Option<u64>)> {
        self.open_stream_following(url, range, 3, None)
    }

    /// Opens an internet radio stream. Stations that announce what they're playing have
    /// those announcements taken out of the audio and the latest written to `title`.
    pub fn open_live(
        &self,
        url: &str,
        title: &std::sync::Arc<std::sync::Mutex<String>>,
    ) -> ApiResult<(Box<dyn Read + Send>, Option<u64>)> {
        self.open_stream_following(url, None, 3, Some(title))
    }

    /// `hops` is how many playlist files may be followed before giving up: radio stations
    /// often publish a small playlist that points at the real stream.
    fn open_stream_following(
        &self,
        url: &str,
        range: Option<(u64, Option<u64>)>,
        hops: u8,
        live: Option<&std::sync::Arc<std::sync::Mutex<String>>>,
    ) -> ApiResult<(Box<dyn Read + Send>, Option<u64>)> {
        self.check_online()?;
        let response = self.following(url, |url| {
            let mut request = self.inner.agent.get(url);
            if live.is_some() {
                // Asks the station to include "now playing" announcements in the stream.
                request = request.header("Icy-MetaData", "1");
            }
            if let Some((start, end)) = range {
                let value = match end {
                    Some(end) => format!("bytes={start}-{end}"),
                    None => format!("bytes={start}-"),
                };
                request = request.header("Range", value);
            }
            // A server that takes the connection and then says nothing is given up on sooner
            // for audio than for other calls: something is waiting to be heard.
            request
                .config()
                .timeout_recv_response(Some(Duration::from_secs(15)))
                .build()
                .call()
                .map_err(|e| ApiError::Network(network_hint(&e)))
        })?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(ApiError::Network(format!("HTTP status {status}.")));
        }
        if range.is_some_and(|(start, _)| start > 0) && status != 206 {
            return Err(ApiError::Network("The server ignored a range request.".into()));
        }
        let is_json = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("json"));
        if is_json {
            // Subsonic reports stream errors as a JSON body with HTTP 200.
            return Err(ApiError::Server { code: 70, message: "stream unavailable".into() });
        }
        let kind = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_lowercase();
        if range.is_none() && (kind.contains("mpegurl") || kind.contains("scpls") || kind.contains("pls+xml")) {
            let mut response = response;
            let text = response
                .body_mut()
                .with_config()
                .limit(256 * 1024)
                .read_to_string()
                .map_err(|e| ApiError::Network(e.to_string()))?;
            return match (stream_in_playlist(&text), hops) {
                (Some(target), 1..) => self.open_stream_following(&target, None, hops - 1, live),
                _ => Err(ApiError::Network("That station's playlist has no stream this app can play.".into())),
            };
        }
        let length = response.body().content_length();
        let interval = response
            .headers()
            .get("icy-metaint")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n > 0);
        let body = response.into_body().into_reader();
        Ok(match (live, interval) {
            (Some(title), Some(interval)) => {
                (Box::new(IcyReader { inner: body, interval, until_note: interval, title: title.clone() }), None)
            }
            _ => (Box::new(body), length),
        })
    }
}

/// Reads a radio stream that has "now playing" announcements mixed in every `interval`
/// bytes, passing on only the audio and keeping the latest announcement.
struct IcyReader<R> {
    inner: R,
    interval: usize,
    /// Audio bytes left before the next announcement.
    until_note: usize,
    title: std::sync::Arc<std::sync::Mutex<String>>,
}

impl<R: Read> Read for IcyReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.until_note == 0 {
            // One byte gives the announcement's length in units of 16 bytes; usually zero.
            let mut length = [0u8; 1];
            self.inner.read_exact(&mut length)?;
            let mut note = vec![0u8; length[0] as usize * 16];
            self.inner.read_exact(&mut note)?;
            if let Some(title) = stream_title(&String::from_utf8_lossy(&note)) {
                *self.title.lock().unwrap() = title;
            }
            self.until_note = self.interval;
        }
        let want = buf.len().min(self.until_note);
        let n = self.inner.read(&mut buf[..want])?;
        self.until_note -= n;
        Ok(n)
    }
}

/// Picks the song title out of an announcement like `StreamTitle='Artist - Song';StreamUrl='';`.
fn stream_title(note: &str) -> Option<String> {
    let rest = note.split_once("StreamTitle='")?.1;
    let title = rest.split_once("';").map(|(title, _)| title).unwrap_or(rest.trim_end_matches(['\0', '\'', ';']));
    Some(title.trim().to_string())
}

/// The first stream address in an M3U or PLS playlist. (HLS playlists, which list short
/// segments instead of one continuous stream, aren't supported.)
fn stream_in_playlist(text: &str) -> Option<String> {
    if text.contains("#EXT-X-TARGETDURATION") {
        return None;
    }
    text.lines()
        .map(str::trim)
        .filter_map(|line| match line.split_once('=') {
            // PLS: "File1=http://..."
            Some((key, value)) if key.to_lowercase().starts_with("file") => Some(value.trim()),
            _ => Some(line),
        })
        .find(|line| line.starts_with("http://") || line.starts_with("https://"))
        .map(str::to_string)
}

fn network_hint(err: &ureq::Error) -> String {
    let text = err.to_string();
    let lower = text.to_lowercase();
    if lower.contains("dns") || lower.contains("resolve") || lower.contains("lookup") {
        "The server name couldn't be found. Check the address.".into()
    } else if lower.contains("refused") {
        "The connection was refused. Check the port and that Navidrome is running.".into()
    } else if lower.contains("timeout") || lower.contains("timed out") {
        "The connection timed out. Check the address and your network.".into()
    } else if lower.contains("certificate") || lower.contains("tls") {
        format!("The secure connection failed ({text}).")
    } else {
        format!("({text})")
    }
}

/// A pretend music server for tests.
#[cfg(test)]
pub(crate) mod fake {
    use std::io::{BufRead, BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;

    /// Starts a web server for one test and gives its address. `answer` is given each request
    /// in turn: its path and query (as in "/rest/ping?u=..."), and the connection to answer on.
    pub fn server(answer: impl Fn(&str, &mut TcpStream) + Send + Sync + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let answer = Arc::new(answer);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let answer = answer.clone();
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let mut lines = BufReader::new(stream.try_clone().unwrap());
                    let mut first = String::new();
                    let _ = lines.read_line(&mut first);
                    loop {
                        let mut line = String::new();
                        if lines.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                    }
                    let target = first.split_whitespace().nth(1).unwrap_or_default().to_string();
                    answer(&target, &mut stream);
                });
            }
        });
        address
    }

    /// Answers with a Subsonic response holding `fields` (a JSON object's insides).
    pub fn ok(stream: &mut TcpStream, fields: &str) {
        let comma = if fields.is_empty() { "" } else { "," };
        let body = format!(
            r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1","type":"navidrome","serverVersion":"0.58.0 (test)"{comma}{fields}}}}}"#
        );
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }

    /// Answers that what was asked for is at `location` instead.
    pub fn moved(stream: &mut TcpStream, location: &str) {
        let _ = write!(stream, "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn tries_https_first_and_knows_when_plain_http_matters() {
        // With no scheme typed, plain HTTP is tried as well only on the home network.
        assert_eq!(addresses_to_try(" music.example.org "), ["https://music.example.org"]);
        assert_eq!(addresses_to_try("192.168.1.5:4533"), ["https://192.168.1.5:4533", "http://192.168.1.5:4533"]);
        assert_eq!(addresses_to_try("nas:4533"), ["https://nas:4533", "http://nas:4533"]);
        assert_eq!(addresses_to_try("http://192.168.1.5:4533"), ["http://192.168.1.5:4533"]);
        // Typed with "http://" it's taken at its word, in whatever case.
        assert_eq!(addresses_to_try("http://music.example.org"), ["http://music.example.org"]);
        assert_eq!(addresses_to_try("HTTP://music.example.org"), ["HTTP://music.example.org"]);
        assert_eq!(addresses_to_try("https://music.example.org/"), ["https://music.example.org/"]);
        assert!(addresses_to_try("  ").is_empty());
        // The scheme is read, not compared as text.
        assert!(unencrypted_beyond_home("HTTP://music.example.org"));
        assert!(unencrypted_beyond_home(" Http://music.example.org"));
        assert!(!unencrypted_beyond_home("HTTPS://music.example.org"));
        assert!(!unencrypted_beyond_home("HTTP://192.168.1.5"));
        assert!(!unencrypted_beyond_home("music.example.org"));
        assert_eq!(scheme_of(" HTTPS://music.example.org").as_deref(), Some("https"));
        assert_eq!(scheme_of("music.example.org:4533"), None);
        assert_eq!(scheme_of("://x"), None);
        // Plain HTTP only matters where someone else could be listening.
        for home in ["http://192.168.1.5:4533", "http://10.0.0.2", "http://localhost:4533", "http://nas:4533", "http://nas.local", "http://music.lan/", "http://100.96.1.2:4533", "http://127.0.0.1:4533", "http://[::1]:4533", "http://[fd00::5]:4533"] {
            assert!(!unencrypted_beyond_home(home), "{home}");
        }
        for away in ["http://music.example.org", "http://music.example.org:4533/x", "http://8.8.8.8:4533", "http://100.200.1.2", "http://[2001:db8::1]:4533"] {
            assert!(unencrypted_beyond_home(away), "{away}");
        }
        assert!(!unencrypted_beyond_home("https://music.example.org"));
    }

    #[test]
    fn takes_a_lyrics_file_s_offset_off_its_times() {
        let line = |start: Option<i64>| LyricLine { start, value: "la".into() };
        // 500 ms sooner; a line can't start before the song does.
        let sooner = shifted(vec![line(Some(200)), line(Some(1500)), line(None)], 500);
        assert_eq!(sooner.iter().map(|line| line.start).collect::<Vec<_>>(), [Some(0), Some(1000), None]);
        let later = shifted(vec![line(Some(200))], -300);
        assert_eq!(later[0].start, Some(500));
    }

    #[test]
    fn normalizes_typed_addresses() {
        assert_eq!(normalize_server_url("192.168.1.10:4533"), "http://192.168.1.10:4533");
        assert_eq!(normalize_server_url(" https://music.example.com/ "), "https://music.example.com");
        assert_eq!(
            normalize_server_url("http://tower:4533/app/#/album"),
            "http://tower:4533"
        );
        assert_eq!(normalize_server_url("http://tower/music/"), "http://tower/music");
        // A trailing slash doesn't keep "/rest" or "/app" in.
        assert_eq!(normalize_server_url("https://music.example.org/rest/"), "https://music.example.org");
        assert_eq!(normalize_server_url("https://music.example.org/rest"), "https://music.example.org");
        assert_eq!(normalize_server_url("https://music.example.org/navidrome/rest//"), "https://music.example.org/navidrome");
        assert_eq!(normalize_server_url("http://tower:4533/app/"), "http://tower:4533");
        assert_eq!(normalize_server_url("http://tower:4533/app#/"), "http://tower:4533");
        // The scheme in small letters, so it's the same account however it was typed.
        assert_eq!(normalize_server_url("HTTP://tower:4533"), "http://tower:4533");
    }

    #[test]
    fn follows_only_redirects_that_keep_the_sign_in_safe() {
        // Never from HTTPS down to plain HTTP, wherever to.
        assert!(redirect_refused("https://music.example.org/rest/ping", "http://music.example.org/rest/ping").is_some());
        assert!(redirect_refused("https://music.example.org/rest/ping", "HTTP://music.example.org/rest/ping").is_some());
        assert!(redirect_refused("https://music.example.org/x", "https://other.example.org/x").is_none());
        // From plain HTTP, to the same server only, over either.
        assert!(redirect_refused("http://music.example.org/x", "https://music.example.org/x").is_none());
        assert!(redirect_refused("http://tower:4533/x", "http://TOWER:8080/y").is_none());
        assert!(redirect_refused("http://tower:4533/x", "http://elsewhere:4533/x").is_some());
        assert!(redirect_refused("http://tower:4533/x", "https://elsewhere/x").is_some());
        assert!(redirect_refused("http://tower:4533/x", "http://someone@elsewhere/x").is_some());
        // Where a redirect points, read against the address it came from.
        assert_eq!(redirect_target("http://tower:4533/rest/ping?u=a", "/music/rest/ping?u=a"), "http://tower:4533/music/rest/ping?u=a");
        assert_eq!(redirect_target("http://tower:4533/rest/ping?u=a", "stream?id=1"), "http://tower:4533/rest/stream?id=1");
        assert_eq!(redirect_target("http://tower:4533/rest/ping", "//elsewhere/rest/ping"), "http://elsewhere/rest/ping");
        assert_eq!(redirect_target("http://tower:4533/rest/ping", "HTTPS://tower/rest/ping"), "HTTPS://tower/rest/ping");
    }

    #[test]
    fn a_redirect_to_another_server_over_plain_http_is_not_followed() {
        // The server the redirect points at counts what reaches it.
        let reached = Arc::new(AtomicUsize::new(0));
        let counter = reached.clone();
        let elsewhere = fake::server(move |_, stream| {
            counter.fetch_add(1, Ordering::Relaxed);
            fake::ok(stream, "");
        });
        // The same machine by another name is another server, as far as the address goes.
        let elsewhere = elsewhere.replace("127.0.0.1", "localhost");
        let server = fake::server(move |target, stream| match target {
            t if t.starts_with("/rest/ping") => fake::moved(stream, &format!("{elsewhere}{t}")),
            t if t.starts_with("/moved/rest/getScanStatus") => fake::ok(stream, r#""scanStatus":{"scanning":false,"count":7}"#),
            t => fake::moved(stream, &format!("/moved{t}")),
        });
        let client = Client::new(&server, "someone", "secret");
        let refused = client.ping().err().expect("the redirect isn't followed");
        assert!(refused.to_string().contains("didn't go"), "{refused}");
        assert_eq!(reached.load(Ordering::Relaxed), 0);
        // On the same server it is.
        assert_eq!(client.scan_status().unwrap().count, 7);
    }

    #[test]
    fn knows_when_the_scan_count_is_the_song_count() {
        let info = |kind: &str, version: &str| ServerInfo { kind: kind.into(), version: version.into() };
        assert!(info("navidrome", "0.55.0 (1a2b3c)").counts_songs());
        assert!(info("navidrome", "0.58.5").counts_songs());
        assert!(info("Navidrome", "v1.0.0").counts_songs());
        assert!(!info("navidrome", "0.54.5 (1a2b3c)").counts_songs());
        assert!(!info("", "1.16.1").counts_songs());
        assert!(!info("gonic", "0.60.0").counts_songs());
    }

    #[test]
    fn finds_the_stream_in_station_playlists() {
        let m3u = "#EXTM3U\n#EXTINF:-1,Some Station\nhttp://example.org/live.mp3\n";
        assert_eq!(stream_in_playlist(m3u).as_deref(), Some("http://example.org/live.mp3"));
        let pls = "[playlist]\nNumberOfEntries=1\nFile1=https://example.org/stream\nTitle1=Some Station\n";
        assert_eq!(stream_in_playlist(pls).as_deref(), Some("https://example.org/stream"));
        assert_eq!(stream_in_playlist("#EXTM3U\n#EXT-X-TARGETDURATION:6\nhttp://x/seg1.ts\n"), None);
        assert_eq!(stream_in_playlist("nothing here"), None);
    }

    #[test]
    fn strips_announcements_from_a_radio_stream() {
        // Four bytes of audio, then a 16-byte announcement, then four more bytes of audio.
        let mut stream = b"AAAA".to_vec();
        stream.push(2);
        let mut note = b"StreamTitle='Band - Song';".to_vec();
        note.resize(32, 0);
        stream.extend_from_slice(&note);
        stream.extend_from_slice(b"BBBB");
        stream.push(0);
        stream.extend_from_slice(b"CC");
        let title = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let mut reader = IcyReader { inner: &stream[..], interval: 4, until_note: 4, title: title.clone() };
        let mut audio = Vec::new();
        reader.read_to_end(&mut audio).unwrap();
        assert_eq!(audio, b"AAAABBBBCC");
        assert_eq!(*title.lock().unwrap(), "Band - Song");
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(encode("Marrow & Pine"), "Marrow%20%26%20Pine");
        assert_eq!(encode("Études"), "%C3%89tudes");
    }
}
