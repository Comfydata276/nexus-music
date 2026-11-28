//! A small client for the Subsonic / OpenSubsonic REST API that Navidrome exposes at `/rest`.
//!
//! Every call is blocking; callers run them on the worker pool, never on the UI thread.

use std::fmt;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

#[cfg(test)]
use md5::{Digest, Md5};
use serde::Deserialize;
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

/// An answer, with the server's clock as the answer gave it (its `Date`, in seconds since
/// 1970; `None` if it gave none that can be read) and this PC's clock as it arrived.
#[derive(Debug, Clone)]
pub struct Timed<T> {
    pub value: T,
    pub date: Option<i64>,
    pub received: i64,
}

// ---------------------------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------------------------

// What the server's answers are read into is the library's (`nexus_core::model`), so that
// whatever is worked out from a song or a playlist is worked out the same by every app.
#[cfg(test)]
pub use nexus_core::model::ReplayGain;
pub use nexus_core::model::{Album, AlbumList, Artist, ArtistInfo, Credit, Found, Genre, LyricLine, Lyrics, PlayQueue, Playlist, ScanStatus, ServerInfo, Song};
use nexus_core::model::shifted;
pub use nexus_core::step::Stated;

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
    /// The music server has refused this sign-in as a wrong name or password (design 6.3,
    /// R10): nothing is sent with it again. Set once, by the first refusal; a new pair is a
    /// new `Client`.
    refused: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

// How a typed address is tidied, which addresses are tried for it and when plain HTTP
// matters are the library's.
pub use nexus_core::address::{addresses_to_try, https_only, normalize_server_url, unencrypted_beyond_home};

/// Longest an API call may take from start to finish. Audio downloads have no such limit.
const API_TIMEOUT: Duration = Duration::from_secs(45);

pub(crate) fn make_agent() -> ureq::Agent {
    agent_following(10, Duration::from_secs(300))
}

/// For the public services (see `corehttp::public`): one kept longer than `idle` isn't used
/// again; it follows no redirects (its caller does that by its own rules); and it never
/// connects to this machine or its network (`dns::PublicResolver`).
pub(crate) fn make_public_agent(idle: Duration) -> ureq::Agent {
    ureq::Agent::with_parts(config_following(0, idle), ureq::unversioned::transport::DefaultConnector::new(), dns::PublicResolver::default())
}

/// For callers that need to see where a redirect points instead of being taken there.
pub(crate) fn make_agent_without_redirects() -> ureq::Agent {
    agent_following(0, Duration::from_secs(300))
}

/// The public agent, through a proxy at `proxy` (as one from the environment would be).
#[cfg(test)]
pub(crate) fn make_public_agent_through(proxy: &str) -> ureq::Agent {
    let config = ureq::Agent::config_builder().proxy(Some(ureq::Proxy::new(proxy).unwrap())).http_status_as_error(false).max_redirects(0).build();
    ureq::Agent::with_parts(config, ureq::unversioned::transport::DefaultConnector::new(), dns::PublicResolver::default())
}

fn agent_following(redirects: u32, idle: Duration) -> ureq::Agent {
    use ureq::unversioned::transport::DefaultConnector;
    ureq::Agent::with_parts(config_following(redirects, idle), DefaultConnector::new(), dns::CachingResolver::default())
}

fn config_following(redirects: u32, idle: Duration) -> ureq::config::Config {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    let tls = TlsConfig::builder()
        .provider(TlsProvider::NativeTls)
        .root_certs(RootCerts::PlatformVerifier)
        .build();
    ureq::Agent::config_builder()
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
        .build()
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
            self.resolve_keeping(uri, config, timeout, "", |_| true)
        }
    }

    impl CachingResolver {
        /// Looks `uri`'s name up, keeping only the addresses `keep` lets through: left out
        /// before any is probed, and remembered under `kind` (so that one cache never hands
        /// another what it left out).
        fn resolve_keeping(&self, uri: &Uri, config: &Config, timeout: NextTimeout, kind: &str, keep: impl Fn(&SocketAddr) -> bool) -> Result<ResolvedSocketAddrs, Error> {
            let key = match (uri.scheme(), uri.authority()) {
                (Some(scheme), Some(authority)) => DefaultResolver::host_and_port(scheme, authority).map(|key| format!("{kind}{key}")),
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
            let kept: Vec<SocketAddr> = resolved.iter().copied().filter(|addr| keep(addr)).collect();
            if kept.is_empty() {
                return Err(Error::HostNotFound);
            }
            let ordered = fastest_first(kept);
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

    /// The public services' resolver: the same look-ups, with every address on this machine or
    /// its network left out (`far_away`), for every connection the agent makes, redirected or
    /// not. What's judged is what's connected to: there is no second look-up to differ. A name
    /// that leads nowhere else is not found.
    ///
    /// With a proxy (ureq takes one from the environment: `ALL_PROXY`, `HTTPS_PROXY` and the
    /// like), the proxy looks the service's name up, not this; only the proxy's own address
    /// comes here, and it's the person's choice, so it's let through as it is. Then only the
    /// written-address rule (`corehttp::Public`) applies to where a redirect leads.
    #[derive(Debug, Default)]
    pub struct PublicResolver {
        inner: CachingResolver,
    }

    impl Resolver for PublicResolver {
        fn resolve(&self, uri: &Uri, config: &Config, timeout: NextTimeout) -> Result<ResolvedSocketAddrs, Error> {
            let the_proxy = config.proxy().is_some_and(|proxy| uri.host() == Some(proxy.host()) && uri.port_u16().unwrap_or(0) == proxy.port());
            if the_proxy {
                return self.inner.resolve(uri, config, timeout);
            }
            // Left out before anything is probed (`fastest_first` connects to each to see which
            // answers first), and remembered apart from the other agents' look-ups.
            self.inner.resolve_keeping(uri, config, timeout, "public:", |addr| far_away(addr.ip()))
        }
    }

    /// An address on the open internet: not this machine, not its network, not one that isn't
    /// anywhere.
    pub fn far_away(ip: std::net::IpAddr) -> bool {
        use std::net::IpAddr;
        match ip {
            IpAddr::V4(v4) => {
                let [a, b, ..] = v4.octets();
                !(v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast() || (a == 100 && (64..128).contains(&b)))
            }
            IpAddr::V6(v6) => {
                if let Some(v4) = v6.to_ipv4_mapped() {
                    return far_away(IpAddr::V4(v4));
                }
                let first = v6.segments()[0];
                !(v6.is_loopback() || v6.is_unspecified() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80)
            }
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

/// The Subsonic error for a sign-in the music server refuses: "wrong username or password".
const WRONG_SIGN_IN: i64 = 40;

/// What a request with a refused sign-in fails with, unsent.
pub const REFUSED: &str = "Your music sign-in needs renewing.";

/// Whether a stream's JSON answer is the music server refusing the sign-in.
fn stream_refused_sign_in(text: &str) -> bool {
    let Ok(root) = serde_json::from_str::<serde_json::Value>(text) else { return false };
    root.get("subsonic-response").and_then(|body| body.get("error")).and_then(|error| error.get("code")).and_then(|code| code.as_i64()) == Some(WRONG_SIGN_IN)
}

#[cfg(test)]
fn random_salt() -> String {
    (0..16).map(|_| fastrand::alphanumeric()).collect()
}

pub use nexus_core::text::encode;
pub(crate) use dns::far_away;

impl Client {
    /// The music server's client, from the sign-in a device is handed (design 9.4): the
    /// server's address, the name, and a salt and token made for this device. No password
    /// is known to this app, so none is turned into a token here.
    pub fn from_pair(server: &str, username: &str, salt: &str, token: &str) -> Client {
        Client {
            inner: Arc::new(Inner {
                base: normalize_server_url(server),
                username: username.to_string(),
                salt: salt.to_string(),
                token: token.to_string(),
                agent: make_agent(),
                offline: std::sync::atomic::AtomicBool::new(false),
                refused: std::sync::atomic::AtomicBool::new(false),
            }),
        }
    }

    /// For the tests alone: a client whose token is made from a password, as a music server's
    /// own sign-in makes one. The app itself never holds a music-server password.
    #[cfg(test)]
    pub fn new(server: &str, username: &str, password: &str) -> Client {
        let salt = random_salt();
        let digest = Md5::digest(format!("{password}{salt}").as_bytes());
        let token: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        Client::from_pair(server, username, &salt, &token)
    }

    /// Whether this is the same sign-in as `other`: the same address, name, salt and token.
    pub fn same_pair(&self, other: &Client) -> bool {
        let (a, b) = (&self.inner, &other.inner);
        a.base == b.base && a.username == b.username && a.salt == b.salt && a.token == b.token
    }

    /// Whether this is the same client (the same pair, made once) as `other`.
    pub fn is(&self, other: &Client) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Whether the music server has refused this sign-in: then nothing more is sent with it.
    pub fn refused(&self) -> bool {
        self.inner.refused.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Sends nothing more with this sign-in (the shared library's rule set it aside).
    pub fn set_aside(&self) {
        self.inner.refused.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// The music server said "wrong name or password" to this sign-in. The first time, the
    /// controller is told (once, on the window's thread), so that the current sign-in can be
    /// asked for; every request after it is refused here, unsent (design 6.3).
    fn was_refused(&self) {
        if !self.inner.refused.swap(true, std::sync::atomic::Ordering::SeqCst) {
            let refused = self.clone();
            crate::controller::post(move |ctl| ctl.music_refused(&refused));
        }
    }

    /// In offline mode nothing is sent: requests fail straight away.
    pub fn set_offline(&self, offline: bool) {
        self.inner.offline.store(offline, std::sync::atomic::Ordering::Relaxed);
    }

    fn check_online(&self) -> ApiResult<()> {
        if self.inner.offline.load(std::sync::atomic::Ordering::Relaxed) {
            Err(ApiError::Network("Offline mode is on.".into()))
        } else if self.refused() {
            // Never sent again as it was (design 6.3): treated as the server out of reach,
            // so that what is on this PC plays.
            Err(ApiError::Network(REFUSED.into()))
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
        self.call_as(CLIENT_NAME, endpoint, params, field, API_TIMEOUT)
    }

    /// `limit` is how long to wait for the whole answer.
    fn call_as<T: DeserializeOwned + Default>(
        &self,
        app: &str,
        endpoint: &str,
        params: &[(&str, &str)],
        field: Option<&str>,
        limit: Duration,
    ) -> ApiResult<T> {
        self.call_timed(app, endpoint, params, field, limit).map(|answer| answer.value)
    }

    /// As `call_as`, with the server's clock as the answer gave it.
    fn call_timed<T: DeserializeOwned + Default>(
        &self,
        app: &str,
        endpoint: &str,
        params: &[(&str, &str)],
        field: Option<&str>,
        limit: Duration,
    ) -> ApiResult<Timed<T>> {
        let url = self.url_as(app, endpoint, params);
        // A long list of song ids can outgrow what servers and proxies accept in an address,
        // so those go in the body of a POST instead (the OpenSubsonic "formPost" extension).
        let response = match url.split_once('?') {
            Some((address, form)) if url.len() > 1800 => self.post_form(address, form)?,
            _ => self.get(&url, Some(limit))?,
        };
        let status = response.status().as_u16();
        // The server's clock, and this PC's as the answer's headers arrived.
        let date = response.headers().get("date").and_then(|date| date.to_str().ok()).and_then(nexus_core::time::http_date);
        let received = chrono::Utc::now().timestamp();
        let timed = |value: T| Timed { value, date, received };
        let reader = response.into_body().into_reader();
        let mut root: serde_json::Value = serde_json::from_reader(reader).map_err(|_| {
            ApiError::NotSubsonic(format!("Check the address (HTTP status {status})."))
        })?;
        let Some(body) = root.get_mut("subsonic-response") else {
            return Err(ApiError::NotSubsonic(format!("Check the address (HTTP status {status}).")));
        };
        if body.get("status").and_then(|s| s.as_str()) != Some("ok") {
            let err = body.get("error");
            let code = err.and_then(|e| e.get("code")).and_then(|c| c.as_i64()).unwrap_or(0);
            if code == WRONG_SIGN_IN {
                self.was_refused();
            }
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
                None => return Ok(timed(T::default())),
            },
            None => body.take(),
        };
        serde_json::from_value(value)
            .map(timed)
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

    /// Whether the person has put the app in offline mode: nothing is asked of the server.
    pub fn is_offline(&self) -> bool {
        self.check_online().is_err()
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
                refused: std::sync::atomic::AtomicBool::new(false),
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

    /// The playlists as the server states them, for keeping devices in step (see `sync`):
    /// an owner or a comment the answer leaves out is `None`, not empty. With the server's
    /// clock.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn playlists_stated(&self) -> ApiResult<Timed<Vec<Stated>>> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Wrap {
            playlist: Vec<Stated>,
        }
        let answer = self.call_timed::<Wrap>(CLIENT_NAME, "getPlaylists", &[], Some("playlists"), API_TIMEOUT)?;
        Ok(Timed { value: answer.value.playlist, date: answer.date, received: answer.received })
    }

    /// The list of playlists, **asked for once and read both ways**: as the server states
    /// them ([`Client::playlists_stated`]) and as the app lists them
    /// ([`Client::playlists`]). A look at what other devices have changed wants both, and
    /// they are the same request. The second reading takes nothing from the first: if the
    /// answer can't be read as the app lists playlists, that half is `None` and the stated
    /// half stands, exactly as it would have asked by itself.
    pub fn playlists_both(&self) -> ApiResult<(Option<Vec<Playlist>>, Timed<Vec<Stated>>)> {
        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct Wrap {
            playlist: Vec<serde_json::Value>,
        }
        let answer = self.call_timed::<Wrap>(CLIENT_NAME, "getPlaylists", &[], Some("playlists"), API_TIMEOUT)?;
        let all = serde_json::Value::Array(answer.value.playlist);
        let stated: Vec<Stated> =
            serde_json::from_value(all.clone()).map_err(|e| ApiError::NotSubsonic(format!("Unexpected response from getPlaylists: {e}")))?;
        Ok((serde_json::from_value(all).ok(), Timed { value: stated, date: answer.date, received: answer.received }))
    }

    /// One playlist as the server states it, without its songs.
    pub fn playlist_stated(&self, id: &str) -> ApiResult<Timed<Stated>> {
        self.call_timed(CLIENT_NAME, "getPlaylist", &[("id", id)], Some("playlist"), API_TIMEOUT)
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
        self.call_as::<serde_json::Value>(device, "savePlayQueue", &params, None, API_TIMEOUT).map(drop)
    }

    /// The play queue last saved on the server (empty if there isn't one), with the server's
    /// clock as this answer gives it: the time a saved queue changed is the server's, and is compared with this PC's.
    pub fn play_queue_timed(&self) -> ApiResult<Timed<PlayQueue>> {
        self.call_timed(CLIENT_NAME, "getPlayQueue", &[], Some("playQueue"), API_TIMEOUT)
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

    /// The same, waiting no longer than `limit` for the answer.
    pub fn scan_status_within(&self, limit: Duration) -> ApiResult<ScanStatus> {
        self.call_as(CLIENT_NAME, "getScanStatus", &[], Some("scanStatus"), limit)
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

    /// Fetches a small binary resource such as cover art. A refused sign-in, which comes as
    /// a success with a small JSON body, is told apart before anything is used as a picture
    /// (design 6.3).
    pub fn fetch_bytes(&self, url: &str, limit: u64) -> ApiResult<Vec<u8>> {
        let mut response = self.get(url, Some(API_TIMEOUT))?;
        if !response.status().is_success() {
            return Err(ApiError::Network(format!("HTTP status {}.", response.status().as_u16())));
        }
        let is_json = response.headers().get("content-type").and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("json"));
        if is_json && url.starts_with(&self.inner.base) {
            let text = response.body_mut().with_config().limit(16 * 1024).read_to_string().unwrap_or_default();
            if stream_refused_sign_in(&text) {
                self.was_refused();
                return Err(ApiError::Server { code: WRONG_SIGN_IN, message: "wrong name or password".into() });
            }
            return Err(ApiError::Server { code: 70, message: "not a picture".into() });
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
        self.open_stream_ranges(url, range).map(|(body, length, _)| (body, length))
    }

    /// The same, and whether the server says it takes byte ranges at this address
    /// ("Accept-Ranges: bytes").
    pub fn open_stream_ranges(
        &self,
        url: &str,
        range: Option<(u64, Option<u64>)>,
    ) -> ApiResult<(Box<dyn Read + Send>, Option<u64>, bool)> {
        self.open_stream_following(url, range, 3, None)
    }

    /// Opens an internet radio stream. Stations that announce what they're playing have
    /// those announcements taken out of the audio and the latest written to `title`.
    pub fn open_live(
        &self,
        url: &str,
        title: &std::sync::Arc<std::sync::Mutex<String>>,
    ) -> ApiResult<(Box<dyn Read + Send>, Option<u64>)> {
        self.open_stream_following(url, None, 3, Some(title)).map(|(body, length, _)| (body, length))
    }

    /// `hops` is how many playlist files may be followed before giving up: radio stations
    /// often publish a small playlist that points at the real stream.
    fn open_stream_following(
        &self,
        url: &str,
        range: Option<(u64, Option<u64>)>,
        hops: u8,
        live: Option<&std::sync::Arc<std::sync::Mutex<String>>>,
    ) -> ApiResult<(Box<dyn Read + Send>, Option<u64>, bool)> {
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
        // A server that takes the connection and then says nothing is given up on sooner for
        // audio than for other calls: something is waiting to be heard.
        let response = request
            .config()
            .timeout_recv_response(Some(Duration::from_secs(15)))
            .build()
            .call()
            .map_err(|e| ApiError::Network(network_hint(&e)))?;
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
            // Subsonic reports stream errors as a JSON body with HTTP 200: among them a
            // refused sign-in, which is never sent again (design 6.3).
            let mut response = response;
            let text = response.body_mut().with_config().limit(16 * 1024).read_to_string().unwrap_or_default();
            if stream_refused_sign_in(&text) {
                self.was_refused();
                return Err(ApiError::Server { code: WRONG_SIGN_IN, message: "wrong name or password".into() });
            }
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
        let ranges = response
            .headers()
            .get("accept-ranges")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("bytes"));
        let interval = response
            .headers()
            .get("icy-metaint")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n > 0);
        let body = response.into_body().into_reader();
        Ok(match (live, interval) {
            (Some(title), Some(interval)) => {
                (Box::new(IcyReader { inner: body, interval, until_note: interval, title: title.clone() }), None, false)
            }
            _ => (Box::new(body), length, ranges),
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

    /// Starts a web server for one test and gives its address. `answer` is given each request:
    /// its path and query (as in "/rest/ping?u=..."), where a byte range asked for starts,
    /// and the connection to answer on.
    pub fn server(answer: impl Fn(&str, Option<u64>, &mut TcpStream) + Send + Sync + 'static) -> String {
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
                    // A connection that sends nothing isn't a request.
                    if lines.read_line(&mut first).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut from = None;
                    loop {
                        let mut line = String::new();
                        if lines.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(value) = line.to_lowercase().strip_prefix("range: bytes=") {
                            from = value.split('-').next().and_then(|n| n.trim().parse().ok());
                        }
                    }
                    let target = first.split_whitespace().nth(1).unwrap_or_default().to_string();
                    answer(&target, from, &mut stream);
                });
            }
        });
        address
    }

    /// A request as a stand-in server was sent it: the request line and headers exactly as
    /// they came, with the server's own address written as "standin", and the body.
    #[derive(Debug, Clone, PartialEq)]
    pub struct Sent {
        pub head: String,
        pub body: String,
    }

    impl Sent {
        /// The request line, e.g. "GET /api/v1/system/status HTTP/1.1".
        pub fn line(&self) -> &str {
            self.head.lines().next().unwrap_or_default()
        }
    }

    /// A server that writes down every request whole before it's answered. `answer` is given
    /// the request's place in the order they came (from 0), the request, and the connection.
    /// Gives the server's address and what it has been sent so far.
    pub fn recorder(answer: impl Fn(usize, &Sent, &mut TcpStream) + Send + Sync + 'static) -> (String, Arc<std::sync::Mutex<Vec<Sent>>>) {
        use std::io::Read;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let host = listener.local_addr().unwrap().to_string();
        let address = format!("http://{host}");
        let sent: Arc<std::sync::Mutex<Vec<Sent>>> = Arc::default();
        let (answer, record) = (Arc::new(answer), sent.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (answer, record, host) = (answer.clone(), record.clone(), host.clone());
                std::thread::spawn(move || {
                    let mut stream = stream;
                    let mut lines = BufReader::new(stream.try_clone().unwrap());
                    let mut head = String::new();
                    let mut length = 0;
                    loop {
                        let mut line = String::new();
                        if lines.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                        if let Some(value) = line.to_lowercase().strip_prefix("content-length:") {
                            length = value.trim().parse().unwrap_or(0);
                        }
                        head.push_str(&line);
                    }
                    let mut body = vec![0; length];
                    if lines.read_exact(&mut body).is_err() {
                        return;
                    }
                    let request = Sent { head: head.replace(&host, "standin"), body: String::from_utf8_lossy(&body).into_owned() };
                    let place = {
                        let mut record = record.lock().unwrap();
                        record.push(request.clone());
                        record.len() - 1
                    };
                    answer(place, &request, &mut stream);
                });
            }
        });
        (address, sent)
    }

    /// Answers with a status and some JSON, and closes the connection.
    pub fn json(stream: &mut TcpStream, status: &str, body: &str) {
        let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        let _ = stream.flush();
    }

    /// Answers with a Subsonic response holding `fields` (a JSON object's insides).
    pub fn ok(stream: &mut TcpStream, fields: &str) {
        let comma = if fields.is_empty() { "" } else { "," };
        let body = format!(r#"{{"subsonic-response":{{"status":"ok","version":"1.16.1"{comma}{fields}}}}}"#);
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against a Navidrome of its own, never one whose playlists matter: makes a song go
    /// missing from the library while a playlist holds it, and shows that the app's way of
    /// changing a playlist sends no removal, move or Undo then, but still adds. Run by hand:
    ///
    /// ```sh
    /// NEXUS_TEST_SERVER=http://127.0.0.1:<port> NEXUS_TEST_USER=<name> NEXUS_TEST_PASSWORD=<password> \
    /// NEXUS_TEST_MISSING_FILE=<one song's file in that server's music folder> \
    ///   cargo test missing_song -- --ignored --nocapture
    /// ```
    ///
    /// The file is renamed for a while (and the library scanned) to make its song missing, and
    /// put back afterwards, even if the test fails.
    #[test]
    #[ignore = "needs a Navidrome of its own; see the comment"]
    fn a_playlist_holding_a_missing_song_is_refused_on_navidrome() {
        use crate::playlists::{Edit, Outcome, carry_out, moving, restoring, spot};
        let env = |name: &str| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
        let client = Client::new(&env("NEXUS_TEST_SERVER"), &env("NEXUS_TEST_USER"), &env("NEXUS_TEST_PASSWORD"));
        let file = std::path::PathBuf::from(env("NEXUS_TEST_MISSING_FILE"));
        let aside = file.with_extension("away");

        let scan = || {
            client.call::<serde_json::Value>("startScan", &[("fullScan", "true")], None).unwrap();
            std::thread::sleep(Duration::from_secs(1));
            while client.scan_status().unwrap().scanning {
                std::thread::sleep(Duration::from_millis(300));
            }
            std::thread::sleep(Duration::from_secs(1));
        };
        let library = || -> std::collections::HashSet<String> { client.library_page(0, 0, 5000, 0).unwrap().songs.into_iter().map(|song| song.id).collect() };
        struct PutBack<'a>(&'a std::path::Path, &'a std::path::Path);
        impl Drop for PutBack<'_> {
            fn drop(&mut self) {
                if self.1.exists() {
                    let _ = std::fs::rename(self.1, self.0);
                }
            }
        }
        let _put_back = PutBack(&file, &aside);
        let away = || std::fs::rename(&file, &aside).unwrap();
        let back = || std::fs::rename(&aside, &file).unwrap();

        // Which song the file is: the one that goes when it does.
        let all = library();
        away();
        scan();
        let gone: Vec<String> = all.difference(&library()).cloned().collect();
        assert_eq!(gone.len(), 1, "one song goes with the file: {gone:?}");
        let x = gone[0].clone();
        back();
        scan();

        // The playlist [X, A, B, C], and D to add: songs of the same album.
        let album = client.album(&client.song(&x).unwrap().album_id).unwrap();
        let others: Vec<String> = album.songs.iter().map(|song| song.id.clone()).filter(|id| *id != x).collect();
        assert!(others.len() >= 4, "an album of at least five songs");
        let (a, b, c, d) = (others[0].clone(), others[1].clone(), others[2].clone(), others[3].clone());
        let id = client.create_playlist("Missing song test", &[x.clone(), a.clone(), b.clone(), c.clone()]).unwrap().id;
        let counted = || client.playlists().unwrap().into_iter().find(|p| p.id == id).map(|p| p.song_count);
        let shown = || -> Vec<String> { client.playlist(&id).unwrap().songs.into_iter().map(|song| song.id).collect() };

        away();
        scan();
        let page = shown();
        println!("X missing: the playlist shows {} songs, the list of playlists counts {:?}", page.len(), counted());
        assert_eq!(page, [a.clone(), b.clone(), c.clone()]);
        assert_eq!(counted(), Some(4));
        // Remove B as shown, move C to the top, and the Undo of a removal of D.
        let edits = [
            ("remove", Edit::Remove(vec![spot(&page, 1).unwrap()])),
            ("move", moving(&page, 2, 0)),
            ("undo", restoring(&[a.clone(), b.clone(), c.clone(), d.clone()], &[3])),
        ];
        for (what, edit) in &edits {
            let outcome = carry_out(&client, &id, &page, edit).unwrap();
            println!("{what}: {outcome:?}; shown after {}, counted {:?}", shown().len(), counted());
            assert_eq!(outcome, Outcome::HidesSongs, "{what}");
            assert_eq!(shown(), page, "{what}");
            assert_eq!(counted(), Some(4), "{what}");
        }
        client.add_to_playlist(&id, std::slice::from_ref(&d)).unwrap();
        println!("add: shown after {}, counted {:?}", shown().len(), counted());
        assert_eq!(shown(), [a.clone(), b.clone(), c.clone(), d.clone()]);
        assert_eq!(counted(), Some(5));

        // X back: everything kept, and the same removal now takes out B.
        back();
        scan();
        let page = shown();
        println!("X back: the playlist shows {} songs, the list of playlists counts {:?}", page.len(), counted());
        assert_eq!(page, [x.clone(), a.clone(), b.clone(), c.clone(), d.clone()]);
        assert!(matches!(carry_out(&client, &id, &page, &Edit::Remove(vec![spot(&page, 2).unwrap()])).unwrap(), Outcome::Done { elsewhere: false, .. }));
        assert_eq!(shown(), [x, a, c, d]);
        client.delete_playlist(&id).unwrap();
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
}
