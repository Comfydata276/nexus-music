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

/// Tidies what the user typed into a base URL: adds a scheme, drops trailing slashes and any
/// `/app/...` path copied from the browser's address bar.
pub fn normalize_server_url(input: &str) -> String {
    let mut url = input.trim().to_string();
    if url.is_empty() {
        return url;
    }
    if !url.contains("://") {
        url = format!("http://{url}");
    }
    if let Some(pos) = url.find("/app/") {
        url.truncate(pos);
    }
    for suffix in ["/app", "/rest"] {
        if url.ends_with(suffix) {
            url.truncate(url.len() - suffix.len());
        }
    }
    while url.ends_with('/') || url.ends_with('#') {
        url.pop();
    }
    url
}

/// Longest an API call may take from start to finish. Audio downloads have no such limit.
const API_TIMEOUT: Duration = Duration::from_secs(45);

pub(crate) fn make_agent() -> ureq::Agent {
    agent_following(10, Duration::from_secs(300))
}

/// For a server that closes idle connections sooner than most: one kept longer than `idle`
/// isn't used again.
pub(crate) fn make_agent_idle(idle: Duration) -> ureq::Agent {
    agent_following(10, idle)
}

/// For callers that need to see where a redirect points instead of being taken there.
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
                agent: make_agent(),
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

    fn get(
        &self,
        url: &str,
        overall: Option<Duration>,
    ) -> ApiResult<ureq::http::Response<ureq::Body>> {
        self.check_online()?;
        self.inner.agent.get(url).config().timeout_global(overall).build().call().map_err(|e| {
            // The server may have moved; look its address up afresh next time.
            dns::forget();
            ApiError::Network(network_hint(&e))
        })
    }

    fn post_form(&self, address: &str, form: &str) -> ApiResult<ureq::http::Response<ureq::Body>> {
        self.check_online()?;
        self.inner
            .agent
            .post(address)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .config()
            .timeout_global(Some(API_TIMEOUT))
            .build()
            .send(form)
            .map_err(|e| ApiError::Network(network_hint(&e)))
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
                agent: make_agent(),
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

    pub fn ping(&self) -> ApiResult<ServerInfo> {
        let v: serde_json::Value = self.call("ping", &[], None)?;
        let text = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
        Ok(ServerInfo { version: text("serverVersion") })
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
            structured_lyrics: Vec<Lyrics>,
        }
        let wrap: Wrap = self.call("getLyricsBySongId", &[("id", song_id)], Some("lyricsList"))?;
        let mut all = wrap.structured_lyrics;
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
        let response = request.call().map_err(|e| ApiError::Network(network_hint(&e)))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_typed_addresses() {
        assert_eq!(normalize_server_url("192.168.1.10:4533"), "http://192.168.1.10:4533");
        assert_eq!(normalize_server_url(" https://music.example.com/ "), "https://music.example.com");
        assert_eq!(
            normalize_server_url("http://tower:4533/app/#/album"),
            "http://tower:4533"
        );
        assert_eq!(normalize_server_url("http://tower/music/"), "http://tower/music");
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
