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
    /// Nothing at the server's address took the connection: it was refused, or nothing
    /// answered in time. (As against a name that wasn't found, or a secure connection that
    /// was made and then failed.)
    Unconnected(String),
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

    /// Whether the server couldn't be reached, as against answering in some way.
    pub fn is_network(&self) -> bool {
        matches!(self, ApiError::Network(_) | ApiError::Unconnected(_))
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiError::Network(m) | ApiError::Unconnected(m) => write!(f, "Can't reach the server. {m}"),
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
    /// The number of songs, where the server says.
    pub count: Option<u64>,
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
/// HTTPS, and over plain HTTP as well only when it is on the home network, going by where its
/// name actually leads (see `resolves_home`): the sign-in never goes unencrypted beyond it
/// unless the person typed "http://" themselves. This looks the name up, so it's for a worker
/// thread.
pub fn addresses_to_try(typed: &str) -> Vec<String> {
    let typed = typed.trim();
    if typed.is_empty() {
        Vec::new()
    } else if split_scheme(typed).is_some() {
        vec![typed.to_string()]
    } else if resolves_home(&format!("http://{typed}")) {
        vec![format!("https://{typed}"), format!("http://{typed}")]
    } else {
        vec![format!("https://{typed}")]
    }
}

/// Said after an error when only HTTPS was tried, and only because no scheme was typed.
pub const TRY_PLAIN_HTTP: &str = "If your server really has no HTTPS, type http:// in front of the address.";

/// Whether to add `TRY_PLAIN_HTTP` to a failed sign-in to what was `typed`, having tried
/// `tried` addresses: only HTTPS was tried, because no scheme was typed, and nothing could be
/// connected to there. Not after a certificate problem (plain HTTP is no cure for that), and
/// not after the app turned down a redirect to plain HTTP (it's there, and has HTTPS).
pub fn suggests_plain_http(typed: &str, tried: usize, error: &ApiError) -> bool {
    tried == 1 && scheme_of(typed).is_none() && matches!(error, ApiError::Unconnected(_))
}

/// Whether to warn, at start-up, about signing in to `server` over plain HTTP beyond the home
/// network, when `warned` is the address last warned about. Looks the name up.
pub fn warn_of_plain_http(server: &str, warned: &str) -> bool {
    server != warned && plain_http_beyond_home(server)
}

/// Said when signed in over plain HTTP to a server beyond the home network.
pub const PLAIN_HTTP_WARNING: &str = "Signed in without encryption: over plain HTTP your sign-in and your music can be read on the way. If your server has an https:// address, sign in with that.";

/// Whether an address is on the home network or this PC, going by every address its name
/// leads to now (an address typed as numbers leads only to itself). One public address among
/// them is enough to say no, and so is a name that can't be looked up. This looks the name up,
/// so it's for a worker thread; the answer is remembered with the lookup, and the connection
/// made afterwards goes to the same addresses.
pub fn resolves_home(address: &str) -> bool {
    let found = dns::lookup(address);
    !found.is_empty() && found.iter().all(|ip| home_ip(*ip))
}

/// Whether an address is plain HTTP to somewhere beyond the home network, going by where its
/// name actually leads: the case where the sign-in can be read on the way. Looks the name up.
pub fn plain_http_beyond_home(address: &str) -> bool {
    scheme_of(address).as_deref() == Some("http") && !resolves_home(address.trim())
}

/// Whether an IP address is on the home network or this PC: private ranges, this PC,
/// link-local, and the range Tailscale and similar use.
fn home_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    let v4 = |ip: std::net::Ipv4Addr| {
        let [a, b, ..] = ip.octets();
        ip.is_private() || ip.is_loopback() || ip.is_link_local() || (a == 100 && (64..128).contains(&b))
    };
    match ip {
        IpAddr::V4(ip) => v4(ip),
        IpAddr::V6(ip) => match ip.to_ipv4_mapped() {
            Some(mapped) => v4(mapped),
            None => ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00 || (ip.segments()[0] & 0xffc0) == 0xfe80,
        },
    }
}

/// Whether an address is plain HTTP to somewhere that isn't on the home network (or this
/// PC), going by how it's written, without looking the name up: a bare name such as `nas` is
/// taken to be at home. For a jam relay's link, which is read as it's pasted. The music
/// server's address is judged by `plain_http_beyond_home`, which looks.
pub fn unencrypted_beyond_home(address: &str) -> bool {
    let address = address.trim();
    if split_scheme(address).is_none_or(|(scheme, _)| scheme != "http") {
        return false;
    }
    let host = host_of(address);
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return !home_ip(ip);
    }
    let local = ["localhost", ".local", ".lan", ".home", ".internal", ".home.arpa", ".localdomain"];
    !(host.is_empty() || !host.contains('.') || local.iter().any(|ending| host == ending.trim_start_matches('.') || host.ends_with(ending)))
}

/// Longest an API call may take from start to finish. Audio downloads have no such limit.
const API_TIMEOUT: Duration = Duration::from_secs(45);
/// Most redirects one request is taken through.
const MAX_REDIRECTS: usize = 10;
/// How long a stream has, from the first request to the answer at the end of any redirects.
/// (Its audio may then take as long as it takes.)
const STREAM_ANSWER: Duration = Duration::from_secs(30);
/// The answers that send a request on to another address. (300, 304 and 305 don't.)
const REDIRECTS: [u16; 5] = [301, 302, 303, 307, 308];

/// What a request carries, which decides which redirects it follows.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Carries {
    /// It was built by this client from the server's address, and has the sign-in in its
    /// address or its form: it goes on only to the same server.
    SignIn,
    /// Somewhere else's address, with nothing of the account's in it: a radio station's
    /// stream, or a cover from another site. It goes wherever it's sent.
    Nothing,
}

const REDIRECT_DOWNGRADE: &str = "It sent the app from its HTTPS address on to a plain-HTTP one, where your sign-in could be read on the way, so the app didn't go.";
const REDIRECT_ELSEWHERE: &str = "It sent the app on to a different server, so the app didn't go, and your sign-in wasn't passed on.";
const REDIRECT_NAMED: &str = "It sent the app on to an address with a name or password in it, so the app didn't go.";
const REDIRECT_UNREADABLE: &str = "It sent the app on to an address the app couldn't make sense of.";
const REDIRECT_LOOP: &str = "It kept sending the app on from one address to another.";
const OUT_OF_TIME: &str = "The connection timed out. Check the address and your network.";

/// Where a redirect from `from` to `location` goes, if it's one to follow. `location` is read
/// as a URI reference against `from` (RFC 3986, by the `url` crate): a path, `../`, a query
/// or fragment alone, or `//` and another host. A request with the sign-in goes on only to the
/// same server; why not, if not.
fn redirect_to(carries: Carries, from: &url::Url, location: &str) -> Result<url::Url, &'static str> {
    let mut to = from.join(location.trim()).map_err(|_| REDIRECT_UNREADABLE)?;
    // A fragment is for a browser; it isn't sent.
    to.set_fragment(None);
    if !matches!(to.scheme(), "http" | "https") {
        return Err(REDIRECT_UNREADABLE);
    }
    if carries == Carries::SignIn {
        same_server(from, &to)?;
    }
    Ok(to)
}

/// Whether `to` is the same server as `from`, for a request that carries the sign-in: the same
/// host, and either the same scheme and port or plain HTTP on its usual port going up to HTTPS
/// on its usual port. Never down from HTTPS to plain HTTP, and never to an address with a
/// name or password in it.
fn same_server(from: &url::Url, to: &url::Url) -> Result<(), &'static str> {
    if from.scheme() == "https" && to.scheme() != "https" {
        return Err(REDIRECT_DOWNGRADE);
    }
    if !to.username().is_empty() || to.password().is_some() {
        return Err(REDIRECT_NAMED);
    }
    // The url crate has already put a name in small letters and an IPv6 address in one form;
    // a name with a dot on the end is the same name.
    let host = |url: &url::Url| match url.host() {
        Some(url::Host::Domain(name)) => Some(name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase()),
        Some(other) => Some(other.to_string()),
        None => None,
    };
    if host(from).is_none() || host(from) != host(to) {
        return Err(REDIRECT_ELSEWHERE);
    }
    let ports = (from.port_or_known_default(), to.port_or_known_default());
    let same = match (from.scheme(), to.scheme()) {
        (a, b) if a == b => ports.0 == ports.1,
        ("http", "https") => ports == (Some(80), Some(443)),
        _ => false,
    };
    if same { Ok(()) } else { Err(REDIRECT_ELSEWHERE) }
}

/// Sends a request with `send`, and again wherever the answer redirects it to, by the rules of
/// `redirect_to`. `send` is given the address and the time left: everything, every redirect
/// included, is over by `deadline`. A redirect that isn't followed is an error: something
/// answered in the server's place. A request that had a form sends the same form again, since
/// it's to the same server's API; one that had none still has none.
fn follow(
    carries: Carries,
    url: &str,
    deadline: std::time::Instant,
    mut send: impl FnMut(&str, Duration) -> ApiResult<ureq::http::Response<ureq::Body>>,
) -> ApiResult<ureq::http::Response<ureq::Body>> {
    let mut at = url.to_string();
    let mut been: Vec<String> = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(ApiError::Network(OUT_OF_TIME.into()));
        }
        let response = send(&at, left)?;
        let location = response.headers().get("location").and_then(|v| v.to_str().ok());
        let (true, Some(location)) = (REDIRECTS.contains(&response.status().as_u16()), location) else {
            return Ok(response);
        };
        let refused = |why: &str| ApiError::NotSubsonic(why.to_string());
        let from = url::Url::parse(&at).map_err(|_| refused(REDIRECT_UNREADABLE))?;
        let next = redirect_to(carries, &from, location).map_err(refused)?.to_string();
        been.push(from.to_string());
        if been.contains(&next) || been.len() > MAX_REDIRECTS {
            return Err(refused(REDIRECT_LOOP));
        }
        at = next;
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
/// `follow`), since its requests carry the sign-in.
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

    /// The IP addresses the host in `address` (a whole URL) leads to, looked up as a request
    /// to it would look them up, and remembered for that request. Empty if it can't be.
    pub fn lookup(address: &str) -> Vec<std::net::IpAddr> {
        let Ok(uri) = address.trim().parse::<Uri>() else { return Vec::new() };
        CachingResolver::default()
            .resolve(&uri, &Config::default(), NextTimeout::default())
            .map(|found| found.iter().map(|addr| addr.ip()).collect())
            .unwrap_or_default()
    }

    /// Names that lead where a test says, as `host:port`, whatever the PC's own lookups say.
    #[cfg(test)]
    pub static FIXED: Mutex<Option<HashMap<String, Vec<SocketAddr>>>> = Mutex::new(None);

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
            #[cfg(test)]
            if let Some(fixed) = key.as_ref().and_then(|key| FIXED.lock().unwrap().as_ref()?.get(key).cloned()) {
                let mut out = self.empty();
                fixed.into_iter().for_each(|addr| out.push(addr));
                return Ok(out);
            }
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
            _ => self.get(&url, Carries::SignIn, API_TIMEOUT)?,
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

    /// A GET of `url` that's all over within `overall`, redirects and the body included.
    fn get(&self, url: &str, carries: Carries, overall: Duration) -> ApiResult<ureq::http::Response<ureq::Body>> {
        self.check_online()?;
        follow(carries, url, std::time::Instant::now() + overall, |url, left| {
            self.inner.agent.get(url).config().timeout_global(Some(left)).build().call().map_err(|e| {
                // The server may have moved; look its address up afresh next time.
                dns::forget();
                network_error(&e)
            })
        })
    }

    /// A form sent to the server's API. Redirected (within the same server), it goes again as
    /// the same form: without its contents the request would mean nothing.
    fn post_form(&self, address: &str, form: &str) -> ApiResult<ureq::http::Response<ureq::Body>> {
        self.check_online()?;
        follow(Carries::SignIn, address, std::time::Instant::now() + API_TIMEOUT, |address, left| {
            self.inner
                .agent
                .post(address)
                .header("Content-Type", "application/x-www-form-urlencoded")
                .config()
                .timeout_global(Some(left))
                .build()
                .send(form)
                .map_err(|e| network_error(&e))
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
        self.get(&self.url_as(CLIENT_NAME, "ping", &[]), Carries::SignIn, limit).is_ok()
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

    /// Fetches a small binary resource such as cover art, from the music server: `url` is one
    /// this client made.
    pub fn fetch_bytes(&self, url: &str, limit: u64) -> ApiResult<Vec<u8>> {
        self.fetch_bytes_carrying(url, Carries::SignIn, limit)
    }

    /// Fetches a small binary resource from another site, such as a cover from a catalogue
    /// of them. Nothing of the account's goes with it, so it follows redirects anywhere.
    pub fn fetch_elsewhere(&self, url: &str, limit: u64) -> ApiResult<Vec<u8>> {
        self.fetch_bytes_carrying(url, Carries::Nothing, limit)
    }

    fn fetch_bytes_carrying(&self, url: &str, carries: Carries, limit: u64) -> ApiResult<Vec<u8>> {
        let mut response = self.get(url, carries, API_TIMEOUT)?;
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

    /// Opens a song's audio from the music server (`url` is one this client made), optionally
    /// starting at a byte offset (and ending at an inclusive end offset). Returns the body
    /// reader and the number of bytes the response will carry, when the server knows it.
    pub fn open_stream(
        &self,
        url: &str,
        range: Option<(u64, Option<u64>)>,
    ) -> ApiResult<(Box<dyn Read + Send>, Option<u64>)> {
        self.open_stream_following(url, range, 0, None)
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

    /// `live` is given for a radio station, and only then: its address is the station's, not
    /// the music server's, and carries nothing of the account's. `hops` is how many playlist
    /// files may be followed before giving up, as stations often publish a small playlist
    /// that points at the real stream; a song never does that.
    fn open_stream_following(
        &self,
        url: &str,
        range: Option<(u64, Option<u64>)>,
        hops: u8,
        live: Option<&std::sync::Arc<std::sync::Mutex<String>>>,
    ) -> ApiResult<(Box<dyn Read + Send>, Option<u64>)> {
        self.check_online()?;
        let carries = if live.is_some() { Carries::Nothing } else { Carries::SignIn };
        let response = follow(carries, url, std::time::Instant::now() + STREAM_ANSWER, |url, left| {
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
            // for audio than for other calls: something is waiting to be heard. Neither wait
            // goes past the time left for the answer.
            request
                .config()
                .timeout_connect(Some(left.min(Duration::from_secs(8))))
                .timeout_recv_response(Some(left.min(Duration::from_secs(15))))
                .build()
                .call()
                .map_err(|e| network_error(&e))
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
        if live.is_some() && range.is_none() && (kind.contains("mpegurl") || kind.contains("scpls") || kind.contains("pls+xml")) {
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

/// What a request that failed to get an answer comes to.
fn network_error(err: &ureq::Error) -> ApiError {
    use std::io::ErrorKind;
    let unconnected = match err {
        ureq::Error::ConnectionFailed | ureq::Error::Timeout(ureq::Timeout::Connect) => true,
        ureq::Error::Io(e) => matches!(
            e.kind(),
            ErrorKind::ConnectionRefused | ErrorKind::HostUnreachable | ErrorKind::NetworkUnreachable | ErrorKind::NetworkDown | ErrorKind::AddrNotAvailable
        ),
        _ => false,
    };
    if unconnected { ApiError::Unconnected(network_hint(err)) } else { ApiError::Network(network_hint(err)) }
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

    /// A request as it reached a test server.
    #[derive(Debug, Clone, Default)]
    pub struct Arrived {
        pub method: String,
        /// The path and query, as in "/rest/ping?u=...".
        pub target: String,
        pub body: String,
    }

    /// Starts a web server for one test and gives its address. `answer` is given each request
    /// in turn: its path and query (as in "/rest/ping?u=..."), and the connection to answer on.
    pub fn server(answer: impl Fn(&str, &mut TcpStream) + Send + Sync + 'static) -> String {
        recording(move |arrived, stream| answer(&arrived.target, stream))
    }

    /// The same, with the whole request: its method and body as well.
    pub fn recording(answer: impl Fn(&Arrived, &mut TcpStream) + Send + Sync + 'static) -> String {
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
                    // A connection that sends nothing is the app's resolver seeing which of a
                    // name's addresses answers first, not a request.
                    if first.trim().is_empty() {
                        return;
                    }
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        if lines.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some((name, value)) = line.split_once(':') {
                            if name.eq_ignore_ascii_case("content-length") {
                                length = value.trim().parse().unwrap_or(0);
                            }
                        }
                    }
                    let mut body = vec![0u8; length];
                    let _ = std::io::Read::read_exact(&mut lines, &mut body);
                    let mut words = first.split_whitespace();
                    let arrived = Arrived {
                        method: words.next().unwrap_or_default().to_string(),
                        target: words.next().unwrap_or_default().to_string(),
                        body: String::from_utf8_lossy(&body).into_owned(),
                    };
                    answer(&arrived, &mut stream);
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
        redirect(stream, 302, location);
    }

    /// Answers with `status`, pointing at `location`.
    pub fn redirect(stream: &mut TcpStream, status: u16, location: &str) {
        let _ = write!(stream, "HTTP/1.1 {status} Moved\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Has `name` lead to `ips` for the rest of the test run, on any port the tests use.
    fn pretend_name(name: &str, ips: &[&str]) {
        let mut fixed = dns::FIXED.lock().unwrap();
        let fixed = fixed.get_or_insert_with(Default::default);
        for port in [80, 443, 4533] {
            let addrs = ips.iter().map(|ip| std::net::SocketAddr::new(ip.parse().unwrap(), port)).collect();
            fixed.insert(format!("{name}:{port}"), addrs);
        }
    }

    #[test]
    fn plain_http_is_tried_only_where_the_name_really_leads_home() {
        // A bare name isn't taken to be at home: it's looked up.
        pretend_name("faraway-nas", &["203.0.113.7"]);
        pretend_name("nearby-nas", &["192.168.1.5"]);
        pretend_name("split-nas", &["192.168.1.6", "2001:db8::6"]);
        pretend_name("mapped-nas", &["::ffff:10.0.0.9"]);
        assert_eq!(addresses_to_try("faraway-nas:4533"), ["https://faraway-nas:4533"]);
        assert_eq!(addresses_to_try("nearby-nas:4533"), ["https://nearby-nas:4533", "http://nearby-nas:4533"]);
        assert_eq!(addresses_to_try("mapped-nas"), ["https://mapped-nas", "http://mapped-nas"]);
        // One public address among them is enough to say no.
        assert_eq!(addresses_to_try("split-nas"), ["https://split-nas"]);
        // A name under a home-sounding ending is looked up too.
        pretend_name("music.lan", &["198.51.100.20"]);
        assert_eq!(addresses_to_try("music.lan"), ["https://music.lan"]);
        // A name that can't be looked up gets no fallback either.
        assert_eq!(addresses_to_try("no-such-host.invalid"), ["https://no-such-host.invalid"]);
        assert_eq!(addresses_to_try("localhost:4533"), ["https://localhost:4533", "http://localhost:4533"]);
        // The warning goes by the same judgement.
        assert!(plain_http_beyond_home("http://faraway-nas:4533"));
        assert!(plain_http_beyond_home("HTTP://split-nas"));
        assert!(!plain_http_beyond_home("http://nearby-nas:4533"));
        assert!(!plain_http_beyond_home("https://faraway-nas"));
        assert!(!plain_http_beyond_home("http://127.0.0.1:4533"));
        assert!(plain_http_beyond_home("http://203.0.113.7"));
        // At start-up, once for each address.
        assert!(warn_of_plain_http("http://faraway-nas:4533", ""));
        assert!(!warn_of_plain_http("http://faraway-nas:4533", "http://faraway-nas:4533"));
        assert!(warn_of_plain_http("http://faraway-nas:4533", "http://203.0.113.7"));
        assert!(!warn_of_plain_http("http://nearby-nas:4533", ""));
        assert!(!warn_of_plain_http("https://faraway-nas", ""));
    }

    #[test]
    fn suggests_plain_http_only_when_nothing_could_be_connected_to() {
        let refused = ApiError::Unconnected("The connection was refused.".into());
        assert!(suggests_plain_http("music.example.org", 1, &refused));
        // Typed with a scheme, or plain HTTP already tried: nothing to suggest.
        assert!(!suggests_plain_http("https://music.example.org", 1, &refused));
        assert!(!suggests_plain_http("nas", 2, &refused));
        // A certificate problem, a name not found, a redirect turned down, a server's "no".
        for other in [
            ApiError::Network("The secure connection failed (certificate).".into()),
            ApiError::NotSubsonic(REDIRECT_DOWNGRADE.into()),
            ApiError::Server { code: 40, message: "wrong".into() },
        ] {
            assert!(!suggests_plain_http("music.example.org", 1, &other), "{other:?}");
        }
    }

    #[test]
    fn tells_a_refused_connection_from_other_failures() {
        // Nothing listening: refused, so nothing was connected to.
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}", closed.local_addr().unwrap());
        drop(closed);
        let error = Client::new(&address, "someone", "secret").ping().unwrap_err();
        assert!(matches!(error, ApiError::Unconnected(_)), "{error:?}");
        assert!(error.is_network());
        // Something there that doesn't speak HTTPS: connected, then the secure part failed.
        let plain = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("https://{}", plain.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut stream in plain.incoming().flatten() {
                let _ = std::io::Write::write_all(&mut stream, b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n");
            }
        });
        let error = Client::new(&address, "someone", "secret").ping().unwrap_err();
        assert!(matches!(error, ApiError::Network(_)), "{error:?}");
        // Turned away from a redirect down to plain HTTP: something answered.
        let error = follow(Carries::SignIn, "https://music.example.org/rest/ping", far_off(), |url, _| {
            Ok(answer(if url.starts_with("https") { 301 } else { 200 }, "http://music.example.org/rest/ping"))
        })
        .unwrap_err();
        assert!(matches!(error, ApiError::NotSubsonic(_)), "{error:?}");
    }

    #[test]
    fn tries_https_first_and_knows_when_plain_http_matters() {
        // With no scheme typed, plain HTTP is tried as well only on the home network.
        pretend_name("music.example.org", &["203.0.113.8"]);
        assert_eq!(addresses_to_try(" music.example.org "), ["https://music.example.org"]);
        assert_eq!(addresses_to_try("192.168.1.5:4533"), ["https://192.168.1.5:4533", "http://192.168.1.5:4533"]);
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

    fn far_off() -> std::time::Instant {
        std::time::Instant::now() + Duration::from_secs(30)
    }

    /// An answer with `status`, and `location` if it's a redirect.
    fn answer(status: u16, location: &str) -> ureq::http::Response<ureq::Body> {
        let mut response = ureq::http::Response::builder().status(status);
        if (300..400).contains(&status) {
            response = response.header("location", location);
        }
        response.body(ureq::Body::builder().data("")).unwrap()
    }

    /// Where a redirect from `from` to `location` is followed to, for a request with the
    /// sign-in, or why not.
    fn signed_to(from: &str, location: &str) -> Result<String, &'static str> {
        redirect_to(Carries::SignIn, &url::Url::parse(from).unwrap(), location).map(|to| to.to_string())
    }

    #[test]
    fn reads_where_a_redirect_points_as_a_uri_reference() {
        let from = "http://tower:4533/music/rest/ping?u=a";
        assert_eq!(signed_to(from, "/rest/ping?u=a"), Ok("http://tower:4533/rest/ping?u=a".into()));
        assert_eq!(signed_to(from, "stream?id=1"), Ok("http://tower:4533/music/rest/stream?id=1".into()));
        assert_eq!(signed_to(from, "./stream"), Ok("http://tower:4533/music/rest/stream".into()));
        assert_eq!(signed_to(from, "../rest/ping"), Ok("http://tower:4533/music/rest/ping".into()));
        assert_eq!(signed_to(from, "../../../../rest/ping"), Ok("http://tower:4533/rest/ping".into()));
        assert_eq!(signed_to(from, "?u=b"), Ok("http://tower:4533/music/rest/ping?u=b".into()));
        // A fragment alone is the same address; it isn't sent.
        assert_eq!(signed_to(from, "#top"), Ok("http://tower:4533/music/rest/ping?u=a".into()));
        assert_eq!(signed_to(from, "ping/"), Ok("http://tower:4533/music/rest/ping/".into()));
        // Protocol-relative: the same scheme, and whatever host it names.
        assert_eq!(signed_to(from, "//tower:4533/rest/ping"), Ok("http://tower:4533/rest/ping".into()));
        assert_eq!(signed_to(from, "//elsewhere/rest/ping"), Err(REDIRECT_ELSEWHERE));
        assert_eq!(signed_to(from, " HTTP://TOWER:4533/x "), Ok("http://tower:4533/x".into()));
        assert_eq!(signed_to(from, "ftp://tower/x"), Err(REDIRECT_UNREADABLE));
        assert_eq!(signed_to(from, "http://[::1"), Err(REDIRECT_UNREADABLE));
    }

    #[test]
    fn follows_a_redirect_with_the_sign_in_only_to_the_same_server() {
        // The same host however it's written: case, a dot on the end, IPv6 in another form.
        assert!(signed_to("http://tower:4533/x", "http://TOWER:4533/y").is_ok());
        assert!(signed_to("https://music.example.org/x", "https://music.example.org./y").is_ok());
        assert!(signed_to("http://[::1]:4533/x", "http://[0:0:0:0:0:0:0:1]:4533/y").is_ok());
        assert!(signed_to("https://music.example.org/x", "https://music.example.org:443/y").is_ok());
        // Up from plain HTTP to HTTPS on the same host, each on its usual port.
        assert!(signed_to("http://music.example.org/x", "https://music.example.org/x").is_ok());
        assert!(signed_to("http://music.example.org:80/x", "https://music.example.org:443/x").is_ok());
        assert_eq!(signed_to("http://tower:4533/x", "https://tower:4533/x"), Err(REDIRECT_ELSEWHERE));
        assert_eq!(signed_to("http://tower:4533/x", "https://tower/x"), Err(REDIRECT_ELSEWHERE));
        // Another port on the same host is another server.
        assert_eq!(signed_to("http://tower:4533/x", "http://tower:8080/x"), Err(REDIRECT_ELSEWHERE));
        assert_eq!(signed_to("https://music.example.org/x", "https://music.example.org:8443/x"), Err(REDIRECT_ELSEWHERE));
        // Another host, including one under the same name, or the same machine by number.
        assert_eq!(signed_to("https://music.example.org/x", "https://auth.example.org/x"), Err(REDIRECT_ELSEWHERE));
        assert_eq!(signed_to("https://music.example.org/x", "https://music.example.org.evil.test/x"), Err(REDIRECT_ELSEWHERE));
        assert_eq!(signed_to("http://localhost:4533/x", "http://127.0.0.1:4533/x"), Err(REDIRECT_ELSEWHERE));
        // Never down to plain HTTP, even on the same host.
        assert_eq!(signed_to("https://music.example.org/x", "http://music.example.org/x"), Err(REDIRECT_DOWNGRADE));
        assert_eq!(signed_to("https://music.example.org/x", "HTTP://music.example.org/x"), Err(REDIRECT_DOWNGRADE));
        // Never to an address with a name or password in it.
        assert_eq!(signed_to("http://tower:4533/x", "http://someone@tower:4533/x"), Err(REDIRECT_NAMED));
        assert_eq!(signed_to("http://tower:4533/x", "http://:secret@tower:4533/x"), Err(REDIRECT_NAMED));
        // What carries nothing goes anywhere.
        let station = url::Url::parse("https://radio.example.org/live").unwrap();
        assert!(redirect_to(Carries::Nothing, &station, "http://cdn.example.net/live.mp3").is_ok());
    }

    #[test]
    fn follows_http_up_to_https_on_the_usual_ports() {
        // (A test can't listen on the usual ports, so this one answers in place of a network.)
        let mut asked = Vec::new();
        let response = follow(Carries::SignIn, "http://music.example.org/rest/ping?u=a", far_off(), |url, _| {
            asked.push(url.to_string());
            Ok(if url.starts_with("http:") { answer(301, "https://music.example.org/rest/ping?u=a") } else { answer(200, "") })
        })
        .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(asked, ["http://music.example.org/rest/ping?u=a", "https://music.example.org/rest/ping?u=a"]);
        // Only the five redirects are followed: 300 and 304 come back as they are.
        for status in [300, 304] {
            let mut asked = 0;
            let response = follow(Carries::SignIn, "http://tower/x", far_off(), |_, _| {
                asked += 1;
                Ok(answer(status, "/y"))
            })
            .unwrap();
            assert_eq!((response.status().as_u16(), asked), (status, 1));
        }
    }

    /// A long playlist: its request goes as a form in the body of a POST.
    fn long_list() -> Vec<String> {
        (0..300).map(|n| format!("song-{n:05}")).collect()
    }

    #[test]
    fn a_long_form_is_sent_again_within_the_server_and_never_to_another() {
        for status in [302, 303, 307, 308] {
            // Somewhere else, which counts what reaches it: by another name, and on another port.
            let reached = Arc::new(std::sync::Mutex::new(Vec::<fake::Arrived>::new()));
            let log = reached.clone();
            let elsewhere = fake::recording(move |arrived, stream| {
                log.lock().unwrap().push(arrived.clone());
                fake::ok(stream, "");
            });
            let by_name = elsewhere.replace("127.0.0.1", "localhost");
            let arrived = Arc::new(std::sync::Mutex::new(Vec::<fake::Arrived>::new()));
            let log = arrived.clone();
            let server = fake::recording(move |request, stream| {
                log.lock().unwrap().push(request.clone());
                let path = request.target.split('?').next().unwrap();
                match path {
                    "/rest/createPlaylist" => fake::redirect(stream, status, "/moved/rest/createPlaylist"),
                    "/moved/rest/createPlaylist" => fake::ok(stream, ""),
                    "/rest/getPlaylist" => fake::redirect(stream, status, &format!("{by_name}/rest/getPlaylist")),
                    // A removal goes to the same machine by another name; an addition to the
                    // same name on another port.
                    _ if request.body.contains("songIndexToRemove") => fake::redirect(stream, status, &format!("{by_name}{path}")),
                    _ => fake::redirect(stream, status, &format!("{elsewhere}{path}")),
                }
            });
            let client = Client::new(&server, "someone", "secret");
            let songs = long_list();
            client.replace_playlist("p1", &songs).unwrap_or_else(|e| panic!("{status}: {e}"));
            let seen = arrived.lock().unwrap().clone();
            assert_eq!(seen.len(), 2, "{status}");
            // The same POST, with the same form, sign-in and all.
            assert!(seen.iter().all(|request| request.method == "POST"), "{status}: {seen:?}");
            assert_eq!(seen[0].body, seen[1].body, "{status}");
            assert!(seen[1].body.contains("u=someone") && seen[1].body.contains("t=") && seen[1].body.contains("song-00299"), "{status}");
            // Sent on to another server, it doesn't go, and it's no success.
            let positions: Vec<usize> = (0..400).collect();
            for refused in [client.add_to_playlist("p1", &songs), client.remove_many_from_playlist("p1", &positions), client.playlist("x").map(drop)] {
                let error = refused.unwrap_err();
                assert!(matches!(error, ApiError::NotSubsonic(_)), "{status}: {error:?}");
                assert!(error.to_string().contains("didn't go"), "{error}");
            }
            assert!(reached.lock().unwrap().is_empty(), "{status}: {:?}", reached.lock().unwrap());
        }
    }

    #[test]
    fn a_redirect_to_another_server_or_port_is_not_followed() {
        let reached = Arc::new(AtomicUsize::new(0));
        let counter = reached.clone();
        let elsewhere = fake::server(move |_, stream| {
            counter.fetch_add(1, Ordering::Relaxed);
            fake::ok(stream, "");
        });
        // The same machine by another name, and the same name on another port.
        let by_name = elsewhere.replace("127.0.0.1", "localhost");
        let secure = elsewhere.replace("http://", "https://");
        let named = elsewhere.replace("http://", "http://someone@");
        let server = fake::server(move |target, stream| {
            let path = target.split('?').next().unwrap();
            match path {
                "/rest/ping" => fake::moved(stream, &format!("{by_name}{target}")),
                "/rest/getScanStatus" => fake::moved(stream, &format!("{elsewhere}{target}")),
                "/rest/getGenres" => fake::moved(stream, &format!("{secure}{target}")),
                "/rest/getStarred2" => fake::moved(stream, &format!("{named}{target}")),
                "/rest/getPlaylists" => fake::redirect(stream, 300, &format!("{elsewhere}{target}")),
                _ => fake::ok(stream, ""),
            }
        });
        let client = Client::new(&server, "someone", "secret");
        for refused in [client.ping().map(drop), client.scan_status().map(drop), client.genres().map(drop), client.starred().map(drop), client.playlists().map(drop)] {
            let error = refused.unwrap_err();
            assert!(matches!(error, ApiError::NotSubsonic(_)), "{error:?}");
        }
        assert_eq!(reached.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_usual_proxy_redirects_still_sign_in() {
        // A slash added on the end, a server moved to a path of its own, and `..` in the way.
        let server = fake::server(move |target, stream| {
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            match path {
                "/rest/ping" => fake::redirect(stream, 301, &format!("/rest/ping/?{query}")),
                "/rest/ping/" => fake::redirect(stream, 308, &format!("/music/rest/ping?{query}")),
                "/music/rest/ping" => fake::ok(stream, ""),
                "/rest/getScanStatus" => fake::redirect(stream, 307, &format!("../music/./rest/getScanStatus?{query}")),
                "/music/rest/getScanStatus" => fake::ok(stream, r#""scanStatus":{"scanning":false,"count":7}"#),
                _ => fake::ok(stream, ""),
            }
        });
        let client = Client::new(&server, "someone", "secret");
        client.ping().unwrap();
        assert_eq!(client.scan_status().unwrap().count, Some(7));
    }

    #[test]
    fn one_deadline_covers_every_redirect() {
        // Each answer takes a while and sends the app on to another address on the same server.
        let server = fake::server(move |target, stream| {
            std::thread::sleep(Duration::from_millis(400));
            let hop: u32 = target.trim_start_matches("/hop").split(['/', '?']).next().unwrap().parse().unwrap_or(0);
            fake::moved(stream, &format!("/hop{}/rest/ping", hop + 1));
        });
        let client = Client::new(&server, "someone", "secret");
        let started = std::time::Instant::now();
        assert!(!client.answers_within(Duration::from_secs(1)));
        assert!(started.elapsed() < Duration::from_millis(1800), "{:?}", started.elapsed());
    }

    #[test]
    fn stops_when_redirects_go_round_in_a_circle() {
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = asked.clone();
        let server = fake::server(move |target, stream| {
            counter.fetch_add(1, Ordering::Relaxed);
            let other = if target.starts_with("/a") { "/b" } else { "/a" };
            fake::moved(stream, other);
        });
        let error = Client::new(&server, "someone", "secret").ping().unwrap_err();
        assert_eq!(error.to_string(), format!("That address didn't answer like a Navidrome server. {REDIRECT_LOOP}"));
        assert_eq!(asked.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn radio_is_told_apart_by_how_it_was_asked_for_not_by_its_address() {
        use std::io::Write;
        // A station, and the stream its playlist and its redirect lead to, on other servers.
        let reached = Arc::new(AtomicUsize::new(0));
        let counter = reached.clone();
        let stream_host = fake::server(move |_, stream| {
            counter.fetch_add(1, Ordering::Relaxed);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd");
        });
        let target = stream_host.replace("127.0.0.1", "localhost");
        let playlist = format!("#EXTM3U\n{target}/live.mp3\n");
        let station = fake::server(move |path, stream| {
            if path.starts_with("/moved") {
                fake::moved(stream, &format!("{target}/live.mp3"));
            } else {
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: audio/x-mpegurl\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{playlist}", playlist.len());
            }
        });
        // The music server is elsewhere again: the station's address isn't under it.
        let client = Client::new("http://127.0.0.1:9", "someone", "secret");
        let title = Arc::new(std::sync::Mutex::new(String::new()));
        // As a radio station: its playlist and its redirect are followed wherever they lead.
        assert!(client.open_live(&format!("{station}/listen.m3u"), &title).is_ok());
        assert!(client.open_live(&format!("{station}/moved"), &title).is_ok());
        assert_eq!(reached.load(Ordering::Relaxed), 2);
        // As a song, the same addresses lead nowhere else.
        assert!(client.open_stream(&format!("{station}/moved"), None).is_err());
        let _ = client.open_stream(&format!("{station}/listen.m3u"), None);
        assert_eq!(reached.load(Ordering::Relaxed), 2);
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
