//! Requests use the released Lidarr operations over a line owned by the server.
//! No request data can select the line's origin, credentials, or redirect target.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::http::header::{CONNECTION, CONTENT_TYPE, HOST};
use axum::http::{HeaderValue, Method, Request};
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use nexus_core::{bulk, http};
use nexus_core::lidarr::{self, Lidarr};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::runtime::Handle;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto, pki_types::ServerName};
use url::Url;

use std::collections::{HashMap, HashSet, VecDeque};
use std::num::NonZeroU64;
use std::sync::{Mutex, MutexGuard};

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, Response};
use axum::routing::post;
use nexus_core::nexus::account::{
    self, AccountId, DeviceId, NotSignedIn, Receipt, RequestHead, SessionId,
};
use nexus_core::nexus::admin;
use nexus_core::nexus::kept::{
    Access, Account, Bearer, Denied, Fingerprint, GrantUse, SessionKind,
};
use nexus_core::nexus::requests::{
    self as wire, AlbumView, Answer, ArtistView, AskedRelease, AttributionPage, AttributionRow,
    Availability, BatchAlbum, BatchLine,
    BatchLineProgress, BatchLineView, Call, Change, Completed, JobId, LidarrNumber,
    ListedView, MusicBrainzId, PageCursor, PendingView, Refusal, Search, SessionOffers,
    lidarr_items,
};
use nexus_core::offer::Offer;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

use crate::accounts::{check::Checked, rows, session::Accounts, setup::random};
use crate::events;
use crate::store::keeper::Keeper;

const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(8);
const UPSTREAM_HEAD_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub struct Endpoint {
    address: admin::InternalAddress,
    url: Url,
    api: String,
    tls: Option<TlsConnector>,
}

impl Endpoint {
    pub fn read(configured: Option<String>) -> Result<Option<Self>, &'static str> {
        let Some(address) = configured.filter(|address| !address.trim().is_empty()) else {
            return Ok(None);
        };
        Self::parse(address.trim()).map(Some).ok_or("NEXUS_LIDARR is wrong")
    }

    fn parse(address: &str) -> Option<Self> {
        if !(address.starts_with("http://") || address.starts_with("https://")) {
            return None;
        }
        let origin = lidarr::origin(address).ok()?;
        let url = Url::parse(address).ok()?;
        if url.query().is_some() || url.fragment().is_some() || origin.port != url.port_or_known_default()? {
            return None;
        }
        let written_path = address.split_once("://")?.1.split_once('/').map_or("", |(_, path)| path);
        if !written_path.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"/-_.~".contains(&byte))
            || written_path.split('/').any(|part| part == "." || part == "..")
            || written_path.contains("//")
        {
            return None;
        }
        let api = format!("{}/api/v1/", url.path().trim_end_matches('/'));
        let tls = (url.scheme() == "https").then(|| {
            let mut roots = RootCertStore::empty();
            roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
            let mut config = ClientConfig::builder_with_provider(Arc::new(crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .expect("ring supports the default TLS versions")
                .with_root_certificates(roots)
                .with_no_client_auth();
            config.alpn_protocols = vec![b"http/1.1".to_vec()];
            TlsConnector::from(Arc::new(config))
        });
        let address = admin::InternalAddress::new(url.as_str().trim_end_matches('/'))?;
        Some(Self { address, url, api, tls })
    }

    fn lidarr(&self, runtime: Handle, key: &str, deadline: Option<Instant>) -> Lidarr {
        Lidarr::new(self.url.as_str(), key, None, Arc::new(Line {
            endpoint: self.clone(), runtime, deadline, within: UPSTREAM_TIMEOUT, before_write: None,
        }))
    }

    fn request(&self, sent: &http::Request<'_>) -> Option<Request<Full<Bytes>>> {
        let url = Url::parse(sent.url).ok()?;
        if url.origin() != self.url.origin()
            || !url.username().is_empty() || url.password().is_some()
            || url.fragment().is_some() || !url.path().starts_with(&self.api)
        {
            return None;
        }
        let (method, data) = match sent.method {
            http::Method::Get => (Method::GET, ""),
            http::Method::Post(body) => (Method::POST, body),
            http::Method::Put(body) => (Method::PUT, body),
        };
        let authority = &self.url[url::Position::BeforeHost..url::Position::AfterPort];
        let target = &url[url::Position::BeforePath..url::Position::AfterQuery];
        let mut builder = Request::builder().method(method).uri(target)
            .header(HOST, authority).header(CONNECTION, "close");
        for &(name, value) in sent.headers {
            if name.eq_ignore_ascii_case("x-api-key") {
                let mut value = HeaderValue::from_str(value).ok()?;
                value.set_sensitive(true);
                builder = builder.header("x-api-key", value);
            } else if name.eq_ignore_ascii_case(CONTENT_TYPE.as_str()) {
                builder = builder.header(CONTENT_TYPE, value);
            }
        }
        builder.body(Full::new(Bytes::copy_from_slice(data.as_bytes()))).ok()
    }

    async fn ask(&self, request: Request<Full<Bytes>>) -> Option<(u16, String)> {
        let host = match self.url.host()? {
            url::Host::Domain(host) => host.to_owned(),
            url::Host::Ipv4(host) => host.to_string(),
            url::Host::Ipv6(host) => host.to_string(),
        };
        let key = request.headers().get("x-api-key")?.clone();
        let tcp = TcpStream::connect((host.as_str(), self.url.port_or_known_default()?)).await.ok()?;
        let (status, body) = match &self.tls {
            None => exchange(tcp, request).await,
            Some(tls) => {
                let name = ServerName::try_from(host).ok()?;
                exchange(tls.connect(name, tcp).await.ok()?, request).await
            }
        }?;
        // A service may reflect its own address or request headers. Refuse
        // the reflected result before the core sees any of that text.
        let key = key.to_str().ok()?;
        let address = self.url.as_str().trim_end_matches('/');
        let body = if contains_secret(&body, key, address) { "Invalid upstream JSON".into() } else { body };
        Some((status, body))
    }
}

fn contains_secret(body: &str, key: &str, address: &str) -> bool {
    let matches = |text: &str| text.contains(key) || text.contains(address);
    if matches(body) {
        return true;
    }
    if !body.contains('\\') {
        return false;
    }
    // JSON escaping can hide either value from comparison with the received
    // bytes. Inspect decoded string values only when escapes occur.
    fn includes(value: &serde_json::Value, matches: &impl Fn(&str) -> bool) -> bool {
        match value {
            serde_json::Value::String(text) => matches(text),
            serde_json::Value::Array(items) => items.iter().any(|item| includes(item, matches)),
            serde_json::Value::Object(items) => items.values().any(|item| includes(item, matches)),
            _ => false,
        }
    }
    serde_json::from_str::<serde_json::Value>(body).is_ok_and(|value| includes(&value, &matches))
}

struct Line {
    endpoint: Endpoint,
    runtime: Handle,
    deadline: Option<Instant>,
    within: Duration,
    before_write: Option<Arc<dyn Fn() -> bool + Send + Sync>>,
}

impl http::Http for Line {
    fn send(&self, question: &http::Request<'_>) -> Result<http::Response, http::Failure> {
        let unreachable = || http::Failure::Reach("No usable response.".into());
        let request = self.endpoint.request(question).ok_or_else(unreachable)?;
        if request.method() != Method::GET && self.before_write.as_ref().is_some_and(|keep| !keep()) {
            return Err(unreachable());
        }
        let within = question.patience.unwrap_or(self.within).min(self.within);
        let within = self.deadline.map_or(within, |deadline| within.min(deadline.saturating_duration_since(Instant::now())));
        if within.is_zero() {
            return Err(unreachable());
        }
        let (status, body) = self.runtime.block_on(async {
            tokio::time::timeout(within, self.endpoint.ask(request)).await.ok().flatten()
        }).ok_or_else(unreachable)?;
        Ok(http::Response::new(status, Vec::new(), move || Ok(body)))
    }
}

async fn exchange(io: impl AsyncRead + AsyncWrite + Unpin + Send + 'static, request: Request<Full<Bytes>>) -> Option<(u16, String)> {
    let (mut sender, connection) = hyper::client::conn::http1::Builder::new()
        .max_buf_size(UPSTREAM_HEAD_BYTES).handshake(TokioIo::new(io)).await.ok()?;
    let answer = async {
        let response = sender.send_request(request).await.ok()?;
        let status = response.status();
        if !status.is_success() {
            return Some((status.as_u16(), String::new()));
        }
        let body = Limited::new(response.into_body(), http::BODY_LIMIT as usize).collect().await.ok()?.to_bytes();
        Some((status.as_u16(), String::from_utf8(body.to_vec()).ok()?))
    };
    let _driver = ConnectionDriver(tokio::spawn(connection).abort_handle());
    answer.await
}

struct ConnectionDriver(tokio::task::AbortHandle);

impl Drop for ConnectionDriver {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests;

const MAX_BOOKS: usize = 64;
const MAX_SNAPSHOTS: usize = 16;
const ACCOUNT_SLOT_MAX: usize = 24;

pub(crate) struct Requests {
    accounts: Arc<Accounts>,
    origin: Option<Arc<str>>,
    endpoint: Option<Endpoint>,
    rooms: Arc<Mutex<Rooms>>,
    books: Mutex<Books>,
    wake: Arc<Notify>,
    mutation: Mutex<()>,
}

impl Requests {
    pub(crate) fn new(accounts: Arc<Accounts>, origin: Option<Arc<str>>, endpoint: Option<Endpoint>) -> Arc<Self> {
        Arc::new(Self {
            accounts,
            origin,
            endpoint,
            rooms: Arc::default(),
            books: Mutex::default(),
            wake: Arc::new(Notify::new()),
            mutation: Mutex::new(()),
        })
    }

    pub(crate) fn router(self: &Arc<Self>, base: Router) -> Router {
        let base = base.layer(axum::middleware::from_fn_with_state(self.clone(), admin_grant));
        base.merge(Router::new()
            .route(wire::REQUESTS, post(call))
            .route(admin::SET_REQUESTS_ENABLED, post(set_requests_enabled))
            .route(admin::GET_LIDARR_SETTING, post(get_lidarr))
            .route(admin::SET_LIDARR, post(set_lidarr))
            .route(wire::ADMIN_ATTRIBUTION, post(attribution))
            .with_state(self.clone()))
            .layer(axum::middleware::from_fn_with_state(self.clone(), ingress))
    }

    async fn account_place(self: &Arc<Self>, path: &str, headers: &HeaderMap) -> Result<Ingress, Response<Body>> {
        let outcome = async {
            if crate::admin::is_browser(headers)
                && !crate::admin::browser_origin_allowed(headers, self.origin.as_deref())
            {
                return Err(account::Refusal::Malformed);
            }
            let page = crate::admin::browser_origin_allowed(headers, self.origin.as_deref());
            let hash = Fingerprint::of(bearer(headers)?.expose_bytes());
            let copied = hash.clone();
            let clock = self.accounts.clock.clone();
            let who = events::answer(self.accounts.desk.ask(move |keeper| {
                let (number, device) = rows::session_by_hash(keeper.data().connection(), copied.as_bytes())
                    .map_err(events::unavailable)?.ok_or_else(ended)?;
                let mut account = keeper.load(number).map_err(events::unavailable)?;
                let kind = events::inspect_session(keeper, &mut account, device, &copied, clock())?;
                let session = session_id(&account, device)?;
                Ok((number, session, kind))
            }).await)?;
            if matches!(path, admin::GET_LIDARR_SETTING | admin::SET_LIDARR | admin::SET_REQUESTS_ENABLED | wire::ADMIN_ATTRIBUTION) && (!page || who.2 != SessionKind::AdminPage) {
                return Err(account::Refusal::NotAllowed(account::NotAllowed::Denied));
            }
            let place = Place::ingress(self.rooms.clone(), who.0).ok_or_else(events::busy)?;
            Ok(Ingress { hash, session: who.1, kind: who.2, page, _place: place })
        }.await;
        outcome.map_err(|why| events::respond(crate::accounts::session::refused(why)))
    }

    async fn shown<T: Send + 'static>(
        &self,
        hash: Fingerprint,
        head: RequestHead,
        expected: SessionId,
        step: impl FnOnce(&mut Keeper, Actor) -> Result<T, account::Refusal> + Send + 'static,
    ) -> Result<(T, Receipt), account::Refusal> {
        self.accounts.live(&head, &hash)?;
        let clock = self.accounts.clock.clone();
        events::answer(self.accounts.desk.ask(move |keeper| {
            let now = clock();
            let (_, device, account, _) = events::authenticated(keeper, &head, &hash, Access::Read, now)?;
            if session_id(&account, device)? != expected {
                return Err(ended());
            }
            let actor = Actor { account: account.state().id, session: expected, admin: account.state().admin,
                restore: head.restore, now: keeper.clock(now).effective() };
            let done = step(keeper, actor)?;
            Ok((done, events::receipt(keeper, account.state())?))
        }).await)
    }
}

async fn ingress(
    State(service): State<Arc<Requests>>,
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response<Body> {
    if guards(request.uri().path()) {
        match service.account_place(request.uri().path(), request.headers()).await {
            Ok(ingress) => { request.extensions_mut().insert(Arc::new(ingress)); }
            Err(response) => return response,
        }
    }
    next.run(request).await
}

pub(crate) fn session_ended() -> Response<Body> {
    events::respond(crate::accounts::session::refused(ended()))
}

fn bearer(headers: &HeaderMap) -> Result<account::Session, account::Refusal> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = values.next().and_then(|value| value.to_str().ok()).ok_or_else(ended)?;
    if values.next().is_some() {
        return Err(ended());
    }
    account::Session::from_authorization(value).ok_or_else(ended)
}

fn ended() -> account::Refusal {
    account::Refusal::NotSignedIn(NotSignedIn::SessionEnded)
}

fn session_id(account: &Account, device: DeviceId) -> Result<SessionId, account::Refusal> {
    account.rows().devices.get(&device).and_then(|row| row.session.as_ref()).map(|session| session.id).ok_or_else(ended)
}

/// Held from header admission through body reading and dispatch. This is
/// separate from the upstream worker place so polls can run during a job.
pub(crate) struct Ingress {
    hash: Fingerprint,
    session: SessionId,
    kind: SessionKind,
    page: bool,
    _place: Place,
}

#[derive(Clone, Copy)]
struct Actor {
    account: AccountId,
    session: SessionId,
    admin: bool,
    restore: u64,
    now: u64,
}

fn access(keeper: &Keeper, account: AccountId) -> Result<wire::RequestsAccess, Refusal> {
    let on = keeper.requests_switch(account.get()).map_err(|_| Refusal::NotDealtWith)?.enabled;
    Ok(if on { wire::RequestsAccess::On } else { wire::RequestsAccess::Off })
}

fn lock<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[derive(Default)]
struct Rooms {
    ingress: HashSet<u64>,
    upstream: HashSet<u64>,
    jobs: HashSet<i64>,
}

struct Place {
    rooms: Arc<Mutex<Rooms>>,
    account: u64,
    upstream: bool,
    job: Option<i64>,
}

impl Place {
    fn ingress(rooms: Arc<Mutex<Rooms>>, account: u64) -> Option<Self> {
        let mut held = lock(&rooms);
        if held.ingress.len() == ACCOUNT_SLOT_MAX || !held.ingress.insert(account) {
            return None;
        }
        drop(held);
        Some(Self { rooms, account, upstream: false, job: None })
    }

    fn upstream(rooms: Arc<Mutex<Rooms>>, account: u64) -> Option<Self> {
        Self::take_upstream(rooms, account, None)
    }

    fn take_upstream(rooms: Arc<Mutex<Rooms>>, account: u64, job: Option<i64>) -> Option<Self> {
        let mut held = lock(&rooms);
        if held.upstream.len() == wire::RUNNING_MAX || !held.upstream.insert(account) {
            return None;
        }
        if let Some(job) = job {
            held.jobs.insert(job);
        }
        drop(held);
        Some(Self { rooms, account, upstream: true, job })
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        let mut held = lock(&self.rooms);
        if self.upstream { &mut held.upstream } else { &mut held.ingress }.remove(&self.account);
        if let Some(job) = self.job {
            held.jobs.remove(&job);
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
enum Read {
    Status,
    FindReleases(Search),
    FindAny(Search),
    FindArtists(Search),
    Tracked,
    ArtistNumbers,
    KnownArtist(MusicBrainzId),
    Albums(LidarrNumber),
    Releases(LidarrNumber),
    Listed(LidarrNumber),
    Busy(Vec<LidarrNumber>),
    Pending,
    AskedHistory,
    Batch(JobId),
}

impl Read {
    fn from_call(call: Call) -> Option<(Self, Option<PageCursor>)> {
        Some(match call {
            Call::Status => (Self::Status, None),
            Call::FindReleases { search, after } => (Self::FindReleases(search), after),
            Call::FindAny { search, after } => (Self::FindAny(search), after),
            Call::FindArtists { search, after } => (Self::FindArtists(search), after),
            Call::Tracked { after } => (Self::Tracked, after),
            Call::ArtistNumbers { after } => (Self::ArtistNumbers, after),
            Call::KnownArtist { artist } => (Self::KnownArtist(artist), None),
            Call::AlbumsOf { artist, after } => (Self::Albums(artist), after),
            Call::ReleasesOf { artist, after } => (Self::Releases(artist), after),
            Call::Listed { artist, after } => (Self::Listed(artist), after),
            Call::Busy { artists } => (Self::Busy(artists), None),
            Call::Pending { after } => (Self::Pending, after),
            _ => return None,
        })
    }

    fn ask(&self, lidarr: &Lidarr) -> Result<Answer, String> {
        let next = None;
        Ok(match self {
            Self::Status => Answer::Status { lidarr: if lidarr.version().is_ok() { Availability::Ready } else { Availability::Unavailable } },
            Self::FindReleases(search) => {
                let (albums, singles) = lidarr.find_releases(search.as_str())?;
                Answer::FoundReleases { albums: lidarr_items(albums), singles: lidarr_items(singles), next }
            }
            Self::FindAny(search) => Answer::FoundAny { albums: lidarr_items(lidarr.find_any(search.as_str())?), next },
            Self::FindArtists(search) => Answer::FoundArtists { artists: lidarr_items(lidarr.find_artists(search.as_str())?), next },
            Self::Tracked => {
                let mut artists: Vec<_> = lidarr.tracked()?.into_iter().filter_map(|id| MusicBrainzId::try_from(id).ok()).collect();
                artists.sort_unstable();
                Answer::Tracked { artists, next }
            }
            Self::ArtistNumbers => {
                let mut artists: Vec<_> = lidarr.artist_numbers()?.into_iter().filter_map(|(id, number)| {
                    Some((MusicBrainzId::try_from(id).ok()?, LidarrNumber::new(number)?))
                }).collect();
                artists.sort_unstable();
                Answer::ArtistNumbers { artists, next }
            }
            Self::KnownArtist(artist) => {
                let artist = lidarr.known_artist(artist.as_str())?.and_then(|(number, name)| {
                    (name.len() <= wire::NAME_MAX_BYTES && !name.chars().any(char::is_control))
                        .then_some((LidarrNumber::new(number)?, name))
                });
                Answer::KnownArtist { artist }
            }
            Self::Albums(artist) | Self::Releases(artist) => {
                let number = i64::from(*artist);
                let mut releases = if matches!(self, Self::Albums(_)) {
                    lidarr.albums_of(number, "")?
                } else {
                    lidarr.releases_of(number, "")?
                };
                // Offer booking needs the trusted artist ID even when the album
                // endpoint returns no embedded artist.
                if releases.iter().any(|(album, _)| album.artist_id.is_empty()) {
                    if let Some((id, _)) = lidarr.artist_numbers()?.into_iter().find(|(_, known)| *known == number) {
                        for (album, _) in &mut releases {
                            if album.artist_id.is_empty() {
                                album.artist_id.clone_from(&id);
                            }
                        }
                    }
                }
                let releases = lidarr_items(releases);
                if matches!(self, Self::Albums(_)) {
                    Answer::AlbumsOf { albums: releases, next }
                } else {
                    Answer::ReleasesOf { releases, next }
                }
            }
            Self::Listed(artist) => Answer::Listed { releases: lidarr_items(lidarr.listed((*artist).into())?), next },
            Self::Busy(artists) => {
                let numbers = artists.iter().map(|number| i64::from(*number)).collect();
                let (busy, stamp) = lidarr.busy(&numbers)?;
                let positions = artists.iter().enumerate().filter(|(_, number)| busy.contains(&i64::from(**number)))
                    .map(|(index, _)| index as u16).collect();
                Answer::Busy { positions, stamp }
            }
            Self::Pending => Answer::Pending { albums: lidarr_items(lidarr.pending()?), next },
            Self::AskedHistory | Self::Batch(_) => unreachable!("this page is loaded from the data"),
        })
    }
}

#[derive(Default)]
struct Books {
    sessions: VecDeque<(SessionId, SessionBook)>,
    snapshots: VecDeque<Snapshot>,
    last_snapshot: u64,
}

struct Snapshot {
    id: NonZeroU64,
    session: SessionId,
    last_seen: Instant,
    content: SnapshotContent,
}

enum SnapshotContent {
    App { read: Read, found: Answer },
    Attribution(Vec<AttributionRow>),
}

impl Books {
    fn expire(&mut self) {
        self.snapshots.retain(|snapshot| snapshot.last_seen.elapsed() < Duration::from_secs(wire::SNAPSHOT_IDLE_SECONDS));
    }

    fn take(
        &mut self,
        session: SessionId,
        after: PageCursor,
        matches: impl Fn(&SnapshotContent) -> bool,
    ) -> Result<Snapshot, Refusal> {
        self.expire();
        let index = self.snapshots.iter().position(|known| {
            known.id == after.snapshot() && known.session == session && matches(&known.content)
        }).ok_or(Refusal::SearchAgain)?;
        let mut snapshot = self.snapshots.remove(index).ok_or(Refusal::SearchAgain)?;
        snapshot.last_seen = Instant::now();
        Ok(snapshot)
    }

    fn next_id(&mut self) -> Result<NonZeroU64, Refusal> {
        self.last_snapshot = self.last_snapshot.checked_add(1).ok_or(Refusal::NotDealtWith)?;
        NonZeroU64::new(self.last_snapshot).ok_or(Refusal::NotDealtWith)
    }

    fn keep_snapshot(&mut self, id: NonZeroU64, session: SessionId, content: SnapshotContent) {
        self.expire();
        let matching = self.snapshots.iter().enumerate().filter(|(_, known)| known.session == session);
        if let Some((index, _)) = matching.clone().next().filter(|_| matching.count() >= wire::SNAPSHOTS_PER_SESSION) {
            self.snapshots.remove(index);
        }
        if self.snapshots.len() == MAX_SNAPSHOTS {
            self.snapshots.pop_front();
        }
        self.snapshots.push_back(Snapshot { id, session, last_seen: Instant::now(), content });
    }

    fn peek(&mut self, session: SessionId, read: &Read, after: PageCursor) -> Result<Answer, Refusal> {
        let snapshot = self.take(session, after, |content| {
            matches!(content, SnapshotContent::App { read: known, .. } if known == read)
        })?;
        let SnapshotContent::App { found, .. } = &snapshot.content else { return Err(Refusal::SearchAgain) };
        let answer = page(found, snapshot.id, after.offset() as usize)?;
        self.snapshots.push_back(snapshot);
        Ok(answer)
    }

    fn first(&mut self, session: SessionId, read: &Read, found: Answer) -> Result<Answer, Refusal> {
        let Some((count, limit)) = page_size(&found) else { return Ok(found) };
        let id = self.next_id()?;
        let answer = page(&found, id, 0)?;
        if count > limit {
            self.keep_snapshot(id, session, SnapshotContent::App { read: read.clone(), found });
        }
        Ok(answer)
    }

    fn attribution_first(&mut self, session: SessionId, rows: Vec<AttributionRow>) -> Result<AttributionPage, Refusal> {
        let id = self.next_id()?;
        let answer = attribution_page(&rows, id, 0)?;
        if answer.next.is_some() {
            self.keep_snapshot(id, session, SnapshotContent::Attribution(rows));
        }
        Ok(answer)
    }

    fn attribution_next(&mut self, session: SessionId, after: PageCursor) -> Result<AttributionPage, Refusal> {
        let snapshot = self.take(session, after, |content| matches!(content, SnapshotContent::Attribution(_)))?;
        let SnapshotContent::Attribution(rows) = &snapshot.content else { return Err(Refusal::SearchAgain) };
        let answer = attribution_page(rows, snapshot.id, after.offset() as usize)?;
        self.snapshots.push_back(snapshot);
        Ok(answer)
    }

    fn remember(&mut self, session: SessionId, now: i64, read: &Read, page: &Answer) {
        let (mut book, artists) = match page {
            Answer::FoundAny { albums, .. } | Answer::FoundReleases { albums, .. } => (albums.clone(), &[][..]),
            Answer::FoundArtists { artists, .. } => (Vec::new(), artists.as_slice()),
            Answer::AlbumsOf { albums, .. } | Answer::ReleasesOf { releases: albums, .. } =>
                (albums.iter().map(|release| release.album.clone()).collect(), &[][..]),
            Answer::Listed { .. } => (Vec::new(), &[][..]),
            _ => return,
        };
        let existing = self.sessions.iter().position(|(known, _)| *known == session)
            .and_then(|index| self.sessions.remove(index)).map(|(_, offers)| offers);
        let mut offers = existing.unwrap_or_default();
        for album in book.drain(..) {
            let _ = offers.offers.remember_album(album, now);
        }
        for artist in artists {
            let _ = offers.offers.remember_artist(artist.clone(), now);
        }
        if let (Read::Listed(artist), Answer::Listed { releases, .. }) = (read, page) {
            offers.listed.retain(|(_, _, seen)| recent(*seen, now));
            for row in releases {
                offers.listed.retain(|(known_artist, known, _)| known_artist != artist || known.id != row.id);
                if offers.listed.len() == wire::KEPT_ALBUMS {
                    offers.listed.pop_front();
                }
                offers.listed.push_back((*artist, row.clone(), now));
            }
        }
        if self.sessions.len() == MAX_BOOKS {
            self.sessions.pop_front();
        }
        self.sessions.push_back((session, offers));
    }
}

fn page_size(found: &Answer) -> Option<(usize, usize)> {
    Some((match found {
        Answer::FoundReleases { albums, singles, .. } => albums.len() + singles.len(),
        Answer::FoundAny { albums, .. } => albums.len(),
        Answer::FoundArtists { artists, .. } => artists.len(),
        Answer::Tracked { artists, .. } => artists.len(),
        Answer::ArtistNumbers { artists, .. } => return Some((artists.len(), wire::NUMBER_PAGE_MAX_ITEMS)),
        Answer::AlbumsOf { albums, .. } => albums.len(),
        Answer::ReleasesOf { releases, .. } => releases.len(),
        Answer::Listed { releases, .. } => releases.len(),
        Answer::Pending { albums, .. } => albums.len(),
        Answer::AskedHistory { rows, .. } => rows.len(),
        Answer::BatchLines { lines, .. } => lines.len(),
        _ => return None,
    }, wire::PAGE_MAX_ITEMS))
}

fn page(found: &Answer, id: NonZeroU64, from: usize) -> Result<Answer, Refusal> {
    let (len, limit) = page_size(found).ok_or(Refusal::SearchAgain)?;
    let (to, next) = page_end(len, limit, id, from)?;
    Ok(match found {
        Answer::FoundReleases { albums, singles, .. } => {
            let last_album = albums.len();
            Answer::FoundReleases {
                albums: albums[from.min(last_album)..to.min(last_album)].to_vec(),
                singles: singles[from.saturating_sub(last_album)..to.saturating_sub(last_album)].to_vec(),
                next,
            }
        }
        Answer::FoundAny { albums, .. } => Answer::FoundAny { albums: albums[from..to].to_vec(), next },
        Answer::FoundArtists { artists, .. } => Answer::FoundArtists { artists: artists[from..to].to_vec(), next },
        Answer::Tracked { artists, .. } => Answer::Tracked { artists: artists[from..to].to_vec(), next },
        Answer::ArtistNumbers { artists, .. } => Answer::ArtistNumbers { artists: artists[from..to].to_vec(), next },
        Answer::AlbumsOf { albums, .. } => Answer::AlbumsOf { albums: albums[from..to].to_vec(), next },
        Answer::ReleasesOf { releases, .. } => Answer::ReleasesOf { releases: releases[from..to].to_vec(), next },
        Answer::Listed { releases, .. } => Answer::Listed { releases: releases[from..to].to_vec(), next },
        Answer::Pending { albums, .. } => Answer::Pending { albums: albums[from..to].to_vec(), next },
        Answer::AskedHistory { rows, .. } => Answer::AskedHistory { rows: rows[from..to].to_vec(), next },
        Answer::BatchLines { lines, .. } => Answer::BatchLines { lines: lines[from..to].to_vec(), next },
        _ => return Err(Refusal::SearchAgain),
    })
}

fn page_end(len: usize, limit: usize, id: NonZeroU64, from: usize) -> Result<(usize, Option<PageCursor>), Refusal> {
    if len > wire::LIST_MAX_ITEMS {
        return Err(Refusal::TooLarge);
    }
    if from >= len && from != 0 {
        return Err(Refusal::SearchAgain);
    }
    let to = (from + limit).min(len);
    let next = if to < len { PageCursor::new(id, to as u32) } else { None };
    Ok((to, next))
}

fn attribution_page(rows: &[AttributionRow], id: NonZeroU64, from: usize) -> Result<AttributionPage, Refusal> {
    let (to, next) = page_end(rows.len(), wire::PAGE_MAX_ITEMS, id, from)?;
    let rows = rows[from..to].iter().map(|row| AttributionRow {
        account: row.account,
        name: row.name.clone(),
        asked: row.asked.clone(),
    }).collect();
    Ok(AttributionPage { rows, next })
}

fn send(answer: Result<Answer, Refusal>, receipt: Receipt) -> Response<Body> {
    let result = answer.unwrap_or_else(|reason| Answer::Refused { reason });
    let reply = wire::Reply::Done { result, receipt };
    match events::result(reply, wire::write_reply) {
        Ok(answer) => events::respond(answer),
        Err(why) => events::respond(crate::accounts::session::refused(why)),
    }
}

#[derive(Default)]
struct SessionBook {
    offers: SessionOffers,
    listed: VecDeque<(LidarrNumber, ListedView, i64)>,
}

fn recent(seen: i64, now: i64) -> bool {
    (0..wire::OFFER_LIFETIME_SECONDS).contains(&now.saturating_sub(seen))
}

fn seconds(ms: u64) -> i64 {
    i64::try_from(ms / 1_000).unwrap_or(i64::MAX)
}

#[derive(Serialize, Deserialize)]
struct Plan {
    binding: [u8; 32],
    revision: u64,
    artist: Option<ArtistView>,
    offers: Vec<AlbumView>,
    listed: Vec<ListedView>,
    #[serde(default)]
    added_artist: Option<lidarr::AddedArtist>,
    #[serde(default)]
    prepared_batch: Option<Vec<u8>>,
}

impl Plan {
    fn resolve(
        change: &Change,
        books: &Books,
        actor: Actor,
        binding: [u8; 32],
        revision: u64,
    ) -> Result<Self, Refusal> {
        let book = || books.sessions.iter().find(|(id, _)| *id == actor.session)
            .map(|(_, book)| book).ok_or(Refusal::SearchAgain);
        let now = seconds(actor.now);
        let mut plan = Self {
            binding, revision, artist: None, offers: Vec::new(), listed: Vec::new(), added_artist: None,
            prepared_batch: None,
        };
        match change {
            Change::Request { offer } | Change::ListSingles { offer, .. } => {
                plan.offers.push(book()?.offers.album(offer, now)?.clone());
            }
            Change::AddArtist { artist } => {
                plan.artist = Some(book()?.offers.artist(artist, now)?.clone());
            }
            Change::AddWanting { artist, offers } => {
                let book = book()?;
                for offer in offers {
                    plan.offers.push(book.offers.album(offer, now)?.clone());
                }
                wire::check_add_wanting(artist, offers, &plan.offers)?;
            }
            Change::Want { artist, albums } => {
                let book = book()?;
                let shown: Vec<_> = book.listed.iter()
                    .filter(|(known, _, seen)| *known == artist.id && recent(*seen, now))
                    .map(|(_, row, _)| row.clone()).collect();
                for album in albums {
                    album.check_current(&shown)?;
                    let row = shown.iter().find(|row| row.id == album.id).ok_or(Refusal::SearchAgain)?;
                    plan.listed.push(row.clone());
                }
            }
            Change::Cancel { .. } | Change::Batch { .. } => {}
        }
        Ok(plan)
    }
}

const QUEUED: i64 = 0;
const STARTED: i64 = 1;
const MAY_HAVE_CHANGED: i64 = 2;
const FINISHED: i64 = 3;

#[derive(Serialize, Deserialize)]
enum JobResult {
    Done(Completed),
    Failed(Refusal),
}

impl JobResult {
    fn answer(self) -> Answer {
        match self {
            Self::Done(result) => Answer::Done { result },
            Self::Failed(reason) => Answer::Failed { reason },
        }
    }
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>, Refusal> {
    serde_json::to_vec(value).map_err(|_| Refusal::NotDealtWith)
}

fn decode<T: serde::de::DeserializeOwned>(value: &[u8]) -> Result<T, Refusal> {
    serde_json::from_slice(value).map_err(|_| Refusal::NotDealtWith)
}

fn sql_error(_: rusqlite::Error) -> Refusal {
    Refusal::NotDealtWith
}

fn job_id(row: i64) -> Result<JobId, Refusal> {
    u64::try_from(row).ok().and_then(NonZeroU64::new).map(JobId).ok_or(Refusal::NotDealtWith)
}

fn row_id(job: JobId) -> Result<i64, Refusal> {
    i64::try_from(job.0.get()).map_err(|_| Refusal::JobNotFound)
}

fn unsigned(number: i64) -> Result<u64, Refusal> {
    u64::try_from(number).map_err(|_| Refusal::NotDealtWith)
}

fn sql(number: u64) -> Result<i64, Refusal> {
    events::sql(number).map_err(sql_error)
}

struct Job {
    row: i64,
    account: AccountId,
    restore: u64,
    admin: bool,
    change: Change,
    plan: Plan,
    recovering: bool,
}

fn existing_job(db: &Connection, actor: Actor, request: wire::RequestId, change: &[u8]) -> Result<Option<JobId>, Refusal> {
    let existing: Option<(i64, Vec<u8>)> = db.query_row(
        "SELECT id, change FROM requests_jobs WHERE restore = ?1 AND account = ?2 AND session = ?3 AND request = ?4",
        params![sql(actor.restore)?, sql(actor.account.get())?, sql(actor.session.get())?, request.get().to_be_bytes().as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(sql_error)?;
    match existing {
        Some((row, saved)) if saved == change => job_id(row).map(Some),
        Some(_) => Err(Refusal::Malformed),
        None => Ok(None),
    }
}

fn enqueue(
    keeper: &mut Keeper,
    actor: Actor,
    request: wire::RequestId,
    change: &[u8],
    plan: &Plan,
    rooms: &Mutex<Rooms>,
) -> Result<JobId, Refusal> {
    let plan = encode(plan)?;
    let restore = sql(actor.restore)?;
    let account = sql(actor.account.get())?;
    let session = sql(actor.session.get())?;
    let occupied: bool = keeper.data().connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM requests_jobs WHERE restore = ?1 AND account = ?2 AND stage < ?3)",
        params![restore, account, FINISHED], |row| row.get(0),
    ).map_err(sql_error)?;
    if occupied {
        return Err(Refusal::Busy);
    }
    {
        let mut active = keeper.data().connection().prepare(
            "SELECT id FROM requests_jobs WHERE restore = ?1 AND stage < ?2",
        ).map_err(sql_error)?;
        let active = active.query_map(params![restore, FINISHED], |row| row.get::<_, i64>(0))
            .map_err(sql_error)?;
        let rooms = lock(rooms);
        let mut waiting = 0;
        for job in active {
            if !rooms.jobs.contains(&job.map_err(sql_error)?) {
                waiting += 1;
                if waiting == wire::WAITING_MAX {
                    return Err(Refusal::Busy);
                }
            }
        }
    }
    let row = keeper.write_data(actor.now, |tx| {
        tx.execute(
            "INSERT INTO requests_jobs (restore, account, session, request, change, work) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![restore, account, session, request.get().to_be_bytes().as_slice(), change, plan],
        )?;
        Ok((tx.last_insert_rowid(), true))
    }).map_err(|_| Refusal::NotDealtWith)?;
    job_id(row)
}

fn after_job(db: &Connection, actor: Actor, job: JobId) -> Result<Answer, Refusal> {
    let (stage, result): (i64, Option<String>) = db.query_row(
        "SELECT stage, result FROM requests_jobs WHERE id = ?1 AND restore = ?2 AND account = ?3 AND session = ?4",
        params![row_id(job)?, sql(actor.restore)?, sql(actor.account.get())?, sql(actor.session.get())?],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(sql_error)?.ok_or(Refusal::JobNotFound)?;
    if stage != FINISHED {
        return Ok(Answer::Working);
    }
    result.ok_or(Refusal::NotDealtWith).and_then(|text| decode::<JobResult>(text.as_bytes())).map(JobResult::answer)
}

#[derive(Serialize, Deserialize, Default)]
struct SavedLine {
    #[serde(default)]
    progress: Option<BatchLineProgress>,
    #[serde(default)]
    offer: Option<AlbumView>,
    #[serde(default)]
    resolution: Option<Resolution>,
}

#[derive(Serialize, Deserialize)]
struct Resolution {
    release: Option<MusicBrainzId>,
    #[serde(default)]
    lookup_started: bool,
}

fn owned_batch(db: &Connection, actor: Actor, job: JobId) -> Result<(Vec<BatchLine>, Plan, i64), Refusal> {
    let (change, work, stage): (Vec<u8>, Vec<u8>, i64) = db.query_row(
        "SELECT change, work, stage FROM requests_jobs WHERE id = ?1 AND restore = ?2 AND account = ?3 AND session = ?4",
        params![row_id(job)?, sql(actor.restore)?, sql(actor.account.get())?, sql(actor.session.get())?],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional().map_err(sql_error)?.ok_or(Refusal::JobNotFound)?;
    let Change::Batch { lines, .. } = decode(&change)? else { return Err(Refusal::JobNotFound) };
    Ok((lines, decode(&work)?, stage))
}

fn batch_lines(db: &Connection, actor: Actor, job: JobId) -> Result<Answer, Refusal> {
    let (input, plan, _) = owned_batch(db, actor, job)?;
    let mut stored = HashMap::new();
    let mut query = db.prepare("SELECT line, progress FROM requests_lines WHERE job = ?1").map_err(sql_error)?;
    let rows = query.query_map([row_id(job)?], |row| Ok((row.get::<_, u16>(0)?, row.get::<_, String>(1)?)))
        .map_err(sql_error)?;
    for row in rows {
        let (index, encoded) = row.map_err(sql_error)?;
        let line: SavedLine = decode(encoded.as_bytes())?;
        if let Some(progress) = line.progress {
            stored.insert(index, progress);
        }
    }
    let saved = if let Some(payload) = plan.prepared_batch {
        let batch = loaded_bulk(&payload, ReadOnlyBatch)?;
        let indexed = batch.indexed_lines().ok_or(Refusal::NotDealtWith)?;
        let mut rows = Vec::with_capacity(indexed.len());
        for line in &indexed {
            let mut projected = BatchLineView::from_indexed(line).map_err(|_| Refusal::NotDealtWith)?;
            if let Some(progress @ BatchLineProgress::Failed { .. }) = stored.remove(&projected.index) {
                projected.progress = progress;
            }
            rows.push(projected);
        }
        rows
    } else {
        (0..input.len() as u16).map(|index| BatchLineView {
            index, progress: stored.remove(&index).unwrap_or(BatchLineProgress::Waiting),
        }).collect()
    };
    Ok(Answer::BatchLines { lines: saved, next: None })
}

fn saved_line(db: &Connection, row: i64, index: u16) -> Result<SavedLine, Refusal> {
    let text: Option<String> = db.query_row(
        "SELECT progress FROM requests_lines WHERE job = ?1 AND line = ?2",
        params![row, index], |row| row.get(0),
    ).optional().map_err(sql_error)?;
    text.map(|text| decode(text.as_bytes())).transpose().map(Option::unwrap_or_default)
}

fn save_line(keeper: &mut Keeper, now: u64, row: i64, index: u16, line: &SavedLine) -> Result<(), Refusal> {
    let text = serde_json::to_string(line).map_err(|_| Refusal::NotDealtWith)?;
    keeper.write_data(now, |tx| {
        tx.execute(
            "INSERT INTO requests_lines (job, line, progress) VALUES (?1, ?2, ?3)
             ON CONFLICT(job, line) DO UPDATE SET progress = excluded.progress",
            params![row, index, text],
        ).map(|count| ((), count != 0))
    }).map_err(|_| Refusal::NotDealtWith)
}

fn resolve_batch_line(
    keeper: &mut Keeper, actor: Actor, job: JobId, index: u16, release: Option<MusicBrainzId>,
) -> Result<(), Refusal> {
    let db = keeper.data().connection();
    let (input, _, stage) = owned_batch(db, actor, job)?;
    if usize::from(index) >= input.len() {
        return Err(Refusal::NotAllowed);
    }
    let row = row_id(job)?;
    let mut line = saved_line(db, row, index)?;
    let state = match &line.resolution {
        Some(resolution) => wire::BatchResolutionState::Saved(resolution.release.as_ref()),
        None if stage != FINISHED && matches!(line.progress, Some(BatchLineProgress::NeedsResolution)) =>
            wire::BatchResolutionState::Awaiting,
        None => wire::BatchResolutionState::Closed,
    };
    if wire::check_batch_resolution(state, release.as_ref())? == wire::BatchResolutionWrite::Save {
        line.resolution = Some(Resolution { release, lookup_started: false });
        save_line(keeper, actor.now, row, index, &line)?;
    }
    Ok(())
}

struct ReadOnlyBatch;

impl bulk::Desk for ReadOnlyBatch {
    fn keep(&self, _: Option<Vec<u8>>, _: bool) {}
    fn keep_checked(&self, _: Vec<u8>) -> bool { false }
    fn now(&self) -> i64 { 0 }
    fn singles(&self) -> bool { false }
}

fn loaded_bulk(payload: &[u8], desk: impl bulk::Desk + 'static) -> Result<bulk::Job, Refusal> {
    let batch = bulk::Job::new(desk);
    if !batch.load(payload) {
        return Err(Refusal::NotDealtWith);
    }
    Ok(batch)
}

fn asked_history(db: &Connection, binding: &[u8; 32], account: AccountId) -> Result<Vec<AskedRelease>, Refusal> {
    let mut query = db.prepare(
        "SELECT metadata FROM requests_owners WHERE binding = ?1 AND account = ?2
         ORDER BY asked_ms DESC, release LIMIT ?3",
    ).map_err(sql_error)?;
    let rows = query.query_map(params![binding.as_slice(), sql(account.get())?, wire::LIST_MAX_ITEMS as i64 + 1],
        |row| row.get::<_, Vec<u8>>(0)).map_err(sql_error)?;
    rows.map(|row| decode(&row.map_err(sql_error)?)).collect()
}

fn attributed_requests(db: &Connection, binding: &[u8; 32]) -> Result<Vec<AttributionRow>, Refusal> {
    let mut query = db.prepare(
        "SELECT owners.account, accounts.shown, owners.metadata FROM requests_owners AS owners
         JOIN accounts ON accounts.number = owners.account
         WHERE owners.binding = ?1 ORDER BY owners.asked_ms DESC, owners.account, owners.release LIMIT ?2",
    ).map_err(sql_error)?;
    let rows = query.query_map(params![binding.as_slice(), wire::LIST_MAX_ITEMS as i64 + 1], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, Vec<u8>>(2)?))
    }).map_err(sql_error)?;
    let mut found = Vec::new();
    for row in rows {
        let (id, name, payload) = row.map_err(sql_error)?;
        let account = AccountId::new(unsigned(id)?).ok_or(Refusal::NotDealtWith)?;
        let name = account::Name::new(&name).ok_or(Refusal::NotDealtWith)?;
        found.push(AttributionRow { account, name, asked: decode(&payload)? });
    }
    Ok(lidarr_items(found))
}

fn owners(
    db: &Connection,
    binding: &[u8; 32],
    account: AccountId,
    releases: &[&str],
) -> Result<HashMap<String, Vec<AccountId>>, Refusal> {
    if releases.is_empty() {
        return Ok(HashMap::new());
    }
    let releases = serde_json::to_string(releases).map_err(|_| Refusal::NotDealtWith)?;
    let mut select = db.prepare(
        "SELECT DISTINCT value,
            (SELECT account FROM requests_owners WHERE binding = ?2 AND release = value AND account = ?3),
            (SELECT account FROM requests_owners WHERE binding = ?2 AND release = value AND account <> ?3 LIMIT 1)
         FROM json_each(?1)",
    ).map_err(sql_error)?;
    let rows = select.query_map(params![releases, binding.as_slice(), sql(account.get())?], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?, row.get::<_, Option<i64>>(2)?))
    }).map_err(sql_error)?;
    let mut found = HashMap::new();
    for row in rows {
        let (release, mine, other) = row.map_err(sql_error)?;
        let requesters: Vec<_> = [mine, other].into_iter().flatten()
            .filter_map(|number| u64::try_from(number).ok().and_then(AccountId::new)).collect();
        if !requesters.is_empty() {
            found.insert(release, requesters);
        }
    }
    Ok(found)
}

fn owner_view(
    db: &Connection,
    binding: &[u8; 32],
    actor: Actor,
    found: &mut Answer,
) -> Result<(), Refusal> {
    let mine = |known: &HashMap<String, Vec<AccountId>>, release: &MusicBrainzId| {
        known.get(release.as_str()).is_some_and(|requesters| requesters.contains(&actor.account))
    };
    match found {
        Answer::Pending { albums, .. } => {
            let known = owners(db, binding, actor.account, &albums.iter().map(|album| album.album.as_str()).collect::<Vec<_>>())?;
            albums.retain_mut(|album| {
                let requesters = known.get(album.album.as_str()).map(Vec::as_slice).unwrap_or_default();
                album.removable = wire::may_cancel(wire::RequestsAccess::On, actor.account, actor.admin, requesters).is_ok();
                actor.admin || requesters.contains(&actor.account)
            });
        }
        Answer::FoundReleases { albums, .. } | Answer::FoundAny { albums, .. } if !actor.admin => {
            let known = owners(db, binding, actor.account, &albums.iter().map(|album| album.album.0.as_str()).collect::<Vec<_>>())?;
            for album in albums {
                album.requested &= mine(&known, &album.album.0);
            }
        }
        Answer::AlbumsOf { albums, .. } | Answer::ReleasesOf { releases: albums, .. } if !actor.admin => {
            let known = owners(db, binding, actor.account, &albums.iter().map(|release| release.album.album.0.as_str()).collect::<Vec<_>>())?;
            for release in albums {
                release.album.requested &= mine(&known, &release.album.album.0);
            }
        }
        Answer::Listed { releases, .. } if !actor.admin => {
            let known = owners(db, binding, actor.account, &releases.iter().map(|release| release.album.as_str()).collect::<Vec<_>>())?;
            for release in releases {
                release.wanted &= mine(&known, &release.album);
            }
        }
        _ => {}
    }
    Ok(())
}

fn binding(endpoint: &Endpoint) -> [u8; 32] {
    endpoint_binding(endpoint.url.as_str())
}

struct Credential {
    revision: u64,
    key: admin::LidarrKey,
}

enum Dispatch {
    Done(Answer),
    Read { ask: Read, actor: Actor, credential: Credential },
}

async fn call(
    State(service): State<Arc<Requests>>,
    axum::Extension(ingress): axum::Extension<Arc<Ingress>>,
    body: Bytes,
) -> Response<Body> {
    let message = match wire::read_call(&body) {
        Ok(message) => message,
        Err(_) => return events::respond(crate::accounts::session::refused(account::Refusal::Malformed)),
    };
    let hash = ingress.hash.clone();
    let head = message.head;
    let for_finish = RequestHead { speaks: head.speaks, restore: head.restore };
    let session = ingress.session;
    let state = service.clone();
    let result = service.shown(hash.clone(), head, session, move |keeper, actor| {
        Ok(state.dispatch(keeper, actor, message.call))
    }).await;
    match result {
        Ok((Ok(Dispatch::Done(answer)), receipt)) => send(Ok(answer), receipt),
        Ok((Err(reason), receipt)) => send(Err(reason), receipt),
        Ok((Ok(Dispatch::Read { ask, actor, credential }), receipt)) => {
            let Some(place) = Place::upstream(service.rooms.clone(), actor.account.get()) else {
                return send(Err(Refusal::Busy), receipt);
            };
            let endpoint = service.endpoint.clone().expect("a credential has an endpoint");
            let for_read = ask.clone();
            let runtime = Handle::current();
            let binding = binding(&endpoint);
            let found = tokio::task::spawn_blocking(move || {
                let _place = place;
                let lidarr = endpoint.lidarr(runtime, credential.key.expose(), Some(Instant::now() + UPSTREAM_TIMEOUT));
                for_read.ask(&lidarr).map_err(|error| wire::refusal_for(&error, None))
            }).await.unwrap_or(Err(Refusal::NotDealtWith));
            let state = service.clone();
            match service.shown(hash, for_finish, session, move |keeper, actor| {
                Ok((|| {
                    access(keeper, actor.account)?.check()?;
                    let db = keeper.data().connection();
                    let mut found = found?;
                    owner_view(db, &binding, actor, &mut found)?;
                    let mut books = lock(&state.books);
                    let answer = books.first(actor.session, &ask, found)?;
                    books.remember(actor.session, seconds(actor.now), &ask, &answer);
                    Ok(answer)
                })())
            }).await {
                Ok((answer, receipt)) => send(answer, receipt),
                Err(why) => events::respond(crate::accounts::session::refused(why)),
            }
        }
        Err(why) => events::respond(crate::accounts::session::refused(why)),
    }
}

impl Requests {
    fn dispatch(&self, keeper: &mut Keeper, actor: Actor, call: Call) -> Result<Dispatch, Refusal> {
        access(keeper, actor.account)?.check()?;
        let endpoint = self.endpoint.as_ref().ok_or(Refusal::LidarrNotSetUp)?;
        let credential = credential(keeper, endpoint)?.ok_or(Refusal::LidarrNotSetUp)?;
        let binding = binding(endpoint);
        let db = keeper.data().connection();
        let answer = match call {
            Call::Begin { request_id, change } => {
                let change_bytes = encode(&change)?;
                if let Some(job) = existing_job(db, actor, request_id, &change_bytes)? {
                    return Ok(Dispatch::Done(Answer::Begun { job }));
                }
                if let Change::Cancel { release, .. } = &change {
                    let owners = owners(db, &binding, actor.account, &[release.as_str()])?;
                    wire::may_cancel(wire::RequestsAccess::On, actor.account, actor.admin,
                        owners.get(release.as_str()).map(Vec::as_slice).unwrap_or_default())?;
                }
                let work = Plan::resolve(&change, &lock(&self.books), actor, binding, credential.revision)?;
                let job = enqueue(keeper, actor, request_id, &change_bytes, &work, &self.rooms)?;
                self.wake.notify_one();
                Answer::Begun { job }
            }
            Call::After { job } => after_job(db, actor, job)?,
            Call::ResolveBatchLine { job, index, release } => {
                resolve_batch_line(keeper, actor, job, index, release)?;
                self.wake.notify_one();
                Answer::BatchResolutionAccepted
            }
            Call::AskedHistory { after } => {
                if let Some(after) = after {
                    lock(&self.books).peek(actor.session, &Read::AskedHistory, after)?
                } else {
                    let found = Answer::AskedHistory { rows: asked_history(db, &binding, actor.account)?, next: None };
                    lock(&self.books).first(actor.session, &Read::AskedHistory, found)?
                }
            }
            Call::BatchLines { job, after } => {
                let key = Read::Batch(job);
                if let Some(after) = after {
                    lock(&self.books).peek(actor.session, &key, after)?
                } else {
                    let found = batch_lines(db, actor, job)?;
                    lock(&self.books).first(actor.session, &key, found)?
                }
            }
            call => {
                let (ask, cursor) = Read::from_call(call).ok_or(Refusal::Malformed)?;
                if let Some(cursor) = cursor {
                    let mut books = lock(&self.books);
                    let answer = books.peek(actor.session, &ask, cursor)?;
                    books.remember(actor.session, seconds(actor.now), &ask, &answer);
                    return Ok(Dispatch::Done(answer));
                }
                return Ok(Dispatch::Read { ask, actor, credential });
            }
        };
        Ok(Dispatch::Done(answer))
    }

    pub(crate) async fn start(self: &Arc<Self>) {
        let address = self.endpoint.as_ref().map(|endpoint| endpoint.address.clone());
        let observed = self.accounts.desk.ask(move |keeper| keeper.lidarr(address.as_ref()).map(drop)).await;
        if !matches!(observed, Ok(Ok(()))) || self.endpoint.is_none() {
            return;
        }
        let service = Arc::downgrade(self);
        let wake = self.wake.clone();
        tokio::spawn(async move {
            loop {
                let Some(state) = service.upgrade() else { return };
                state.restore_at_start().await;
                loop {
                    let dispatching = state.clone();
                    let clock = state.accounts.clock.clone();
                    let selected = state.accounts.desk.ask(move |keeper| {
                        dispatching.claim(keeper, clock())
                    }).await;
                    let Ok(Ok(Some((job, place, credential)))) = selected else { break };
                    let state = state.clone();
                    let runtime = Handle::current();
                    tokio::task::spawn_blocking(move || {
                        let _place = place;
                        state.run(job, credential, runtime);
                        state.wake.notify_one();
                    });
                }
                drop(state);
                let _ = tokio::time::timeout(Duration::from_secs(2), wake.notified()).await;
            }
        });
    }

    async fn restore_at_start(self: &Arc<Self>) {
        let Some(endpoint) = self.endpoint.clone() else { return };
        let identity = binding(&endpoint);
        let lookup = endpoint.clone();
        let found = self.accounts.desk.ask(move |keeper| {
            let pending = keeper.data().connection().query_row(
                "SELECT EXISTS(SELECT 1 FROM requests_way_back WHERE binding = ?1)",
                [identity.as_slice()], |row| row.get::<_, bool>(0),
            ).map_err(sql_error)?;
            if !pending {
                return Ok(None);
            }
            Ok(credential(keeper, &lookup)?.map(|credential| (keeper.restore(), credential)))
        }).await;
        let Ok(Ok::<_, Refusal>(Some((restore, key)))) = found else { return };
        let Some(place) = Place::upstream(self.rooms.clone(), 0) else { return };
        let runtime = Handle::current();
        let journal = Journal {
            service: self.clone(), runtime: runtime.clone(), binding: identity, restore, row: 0, artist: None,
        };
        let _ = tokio::task::spawn_blocking(move || {
            let _place = place;
            let _turn = lock(&journal.service.mutation);
            let lidarr = endpoint.lidarr(runtime, key.key.expose(), None);
            journal.restore_pending(&lidarr)
        }).await;
    }

    fn claim(&self, keeper: &mut Keeper, now: u64) -> Result<Option<(Job, Place, Credential)>, Refusal> {
        let Some(endpoint) = &self.endpoint else { return Ok(None) };
        let db = keeper.data().connection();
        let (
            row, account, session, change, plan, stage, place,
        ) = {
            let mut query = db.prepare(
                "SELECT id, account, session, change, work, stage FROM requests_jobs
                 WHERE restore = ?1 AND stage < ?2 AND NOT EXISTS (
                    SELECT 1 FROM requests_lines
                    WHERE job = requests_jobs.id
                     AND json_extract(progress, '$.progress.state') = 'needs_resolution'
                     AND json_type(progress, '$.resolution') = 'null'
                 ) ORDER BY id LIMIT ?3",
            ).map_err(sql_error)?;
            let mut rows = query.query(params![sql(keeper.restore())?, FINISHED, (wire::WAITING_MAX + wire::RUNNING_MAX) as i64])
                .map_err(sql_error)?;
            loop {
                let Some(row) = rows.next().map_err(sql_error)? else { return Ok(None) };
                let account = unsigned(row.get::<_, i64>(1).map_err(sql_error)?)?;
                let id = row.get::<_, i64>(0).map_err(sql_error)?;
                let Some(place) = Place::take_upstream(self.rooms.clone(), account, Some(id)) else { continue };
                break (id, account, row.get::<_, i64>(2).map_err(sql_error)?,
                    row.get::<_, Vec<u8>>(3).map_err(sql_error)?, row.get::<_, Vec<u8>>(4).map_err(sql_error)?,
                    row.get::<_, i64>(5).map_err(sql_error)?, place);
            }
        };
        let plan: Plan = decode(&plan)?;
        let mut actor = keeper.load(account).map_err(|_| Refusal::NotDealtWith)?;
        let account_id = actor.state().id;
        let restore = keeper.restore();
        let live = SessionId::new(unsigned(session)?).ok_or(Refusal::NotDealtWith)?;
        let db = keeper.data().connection();
        let live = rows::session_by_id(db, live).map_err(sql_error)?;
        let access = access(keeper, account_id)?.check();
        let key = credential(keeper, endpoint)?;
        let allowed = (|| {
            if access.is_err() {
                return Err(Refusal::SwitchedOff);
            }
            let (number, device, hash, _) = live.ok_or(Refusal::NotSignedIn)?;
            if number != account {
                return Err(Refusal::NotSignedIn);
            }
            events::inspect_session(keeper, &mut actor, device, &Fingerprint::from_bytes(hash), now)
                .map_err(|_| Refusal::NotSignedIn)?;
            let key = key.ok_or(Refusal::LidarrNotSetUp)?;
            if plan.binding != binding(endpoint) || plan.revision != key.revision {
                return Err(Refusal::LidarrNotSetUp);
            }
            Ok(key)
        })();
        let key = match allowed {
            Ok(key) => key,
            Err(reason) => {
                finish_job(keeper, now, row, &JobResult::Failed(reason), &[], None)?;
                self.wake.notify_one();
                return Ok(None);
            }
        };
        if stage == QUEUED {
            keeper.write_data(now, |tx| {
                tx.execute("UPDATE requests_jobs SET stage = ?1 WHERE id = ?2 AND stage = ?3", params![STARTED, row, QUEUED])
                    .map(|count| ((), count != 0))
            }).map_err(|_| Refusal::NotDealtWith)?;
        }
        Ok(Some((Job {
            row,
            account: account_id,
            restore,
            admin: actor.state().admin,
            change: decode(&change)?,
            plan,
            recovering: stage == MAY_HAVE_CHANGED,
        }, place, key)))
    }

    fn run(self: &Arc<Self>, job: Job, credential: Credential, runtime: Handle) {
        let Some(endpoint) = &self.endpoint else { return };
        let journal = Journal {
            service: self.clone(), runtime: runtime.clone(), binding: job.plan.binding,
            restore: job.restore, row: job.row,
            artist: job.plan.offers.first().and_then(|offer| offer.artist_id.clone()),
        };
        let ready = Arc::new(AtomicBool::new(job.recovering));
        let pre_write = {
            let journal = journal.clone();
            Arc::new(move || {
                if ready.load(Ordering::Relaxed) {
                    return true;
                }
                let was_kept = journal.ask(|keeper, now, row| {
                    keeper.write_data(now, |tx| tx.execute(
                        "UPDATE requests_jobs SET stage = ?1 WHERE id = ?2 AND stage = ?3",
                        params![MAY_HAVE_CHANGED, row, STARTED],
                    ).map(|count| (count != 0, count != 0))).map_err(|_| Refusal::NotDealtWith)
                }).unwrap_or(false);
                ready.store(was_kept, Ordering::Relaxed);
                was_kept
            })
        };
        let lidarr = Lidarr::new(endpoint.url.as_str(), credential.key.expose(), None, Arc::new(Line {
            endpoint: endpoint.clone(), runtime: runtime.clone(), within: UPSTREAM_TIMEOUT,
            deadline: None, before_write: Some(pre_write),
        }));
        let _mutation = (!matches!(job.change, Change::Batch { .. })).then(|| lock(&self.mutation));
        let result = if let Change::Batch { singles, lines } = &job.change {
            match self.run_batch(&job, &journal, &lidarr, *singles, lines) {
                Ok(Some(asked)) => Ok((Completed::Batch, asked, None)),
                Ok(None) => return,
                Err(reason) => Err(reason),
            }
        } else {
            let cleaned = journal.restore_pending(&endpoint.lidarr(runtime, credential.key.expose(), None));
            cleaned.and_then(|()| {
                if job.recovering { job.recover(&lidarr) } else { job.run(&journal, &lidarr) }
            })
        };
        let account = job.account;
        let binding = job.plan.binding;
        let (result, adds, removed) = match result {
            Ok((done, releases, removed)) => (JobResult::Done(done), releases, removed),
            Err(reason) => (JobResult::Failed(reason), Vec::new(), None),
        };
        let _ = journal.ask(move |keeper, now, row| {
            finish_job(keeper, now, row, &result, &adds, Some((binding, account, removed)))
        });
    }
}

fn finish_job(
    keeper: &mut Keeper,
    now: u64,
    row: i64,
    result: &JobResult,
    adds: &[AskedRelease],
    ownership: Option<([u8; 32], AccountId, Option<MusicBrainzId>)>,
) -> Result<(), Refusal> {
    let text = serde_json::to_string(result).map_err(|_| Refusal::NotDealtWith)?;
    let at = keeper.clock(now).effective();
    keeper.write_data(now, |tx| {
        let changed = tx.execute(
            "UPDATE requests_jobs SET stage = ?1, result = ?2 WHERE id = ?3 AND stage < ?1",
            params![FINISHED, text, row],
        )? != 0;
        if let Some((binding, account, removed)) = ownership.filter(|_| changed) {
            for release in adds {
                keep_owner(tx, &binding, account, release, at)?;
            }
            if let Some(release) = removed {
                tx.execute("DELETE FROM requests_owners WHERE binding = ?1 AND release = ?2",
                    params![binding.as_slice(), release.as_str()])?;
            }
        }
        Ok(((), changed))
    }).map_err(|_| Refusal::NotDealtWith)
}

fn keep_owner(db: &Connection, binding: &[u8; 32], account: AccountId, asked: &AskedRelease, now: u64) -> rusqlite::Result<()> {
    let mut row = asked.clone();
    row.asked_ms = now;
    let metadata = serde_json::to_vec(&row).map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    db.execute(
        "INSERT OR IGNORE INTO requests_owners (binding, account, release, asked_ms, metadata)
         SELECT ?1, number, ?2, ?3, ?4 FROM accounts WHERE number = ?5",
        params![binding.as_slice(), row.release.as_str(), events::sql(now)?, metadata, events::sql(account.get())?],
    ).map(drop)
}

fn asked_release(view: &AlbumView) -> Result<AskedRelease, Refusal> {
    Ok(AskedRelease {
        release: view.album.0.clone(),
        artist_id: view.artist_id.clone().ok_or(Refusal::LidarrUnrecognized)?,
        title: view.title.clone(),
        artist: view.artist.clone(),
        cover: view.cover.clone(),
        asked_ms: 0,
    })
}

#[derive(Clone)]
struct Journal {
    service: Arc<Requests>,
    runtime: Handle,
    binding: [u8; 32],
    restore: u64,
    row: i64,
    artist: Option<MusicBrainzId>,
}

impl Journal {
    fn ask<T: Send + 'static>(
        &self,
        step: impl FnOnce(&mut Keeper, u64, i64) -> Result<T, Refusal> + Send + 'static,
    ) -> Result<T, Refusal> {
        let clock = self.service.accounts.clock.clone();
        let row = self.row;
        let restore = self.restore;
        self.runtime.block_on(self.service.accounts.desk.ask(move |keeper| {
            if keeper.restore() != restore {
                return Err(Refusal::StartAgain);
            }
            step(keeper, clock(), row)
        })).map_err(|_| Refusal::Busy)?
    }

    fn restore_pending(&self, lidarr: &Lidarr) -> Result<(), Refusal> {
        let binding = self.binding;
        let pending = self.ask(move |keeper, _, _| {
            let mut query = keeper.data().connection().prepare(
                "SELECT artist, artist_key, setting FROM requests_way_back WHERE binding = ?1 ORDER BY id LIMIT 20",
            ).map_err(sql_error)?;
            let rows = query.query_map([binding.as_slice()], |row| Ok((
                row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
            ))).map_err(sql_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
        })?;
        for (artist, artist_key, setting) in pending {
            let known = lidarr_result(lidarr.known_artist(&artist_key), None)?;
            if known.is_some_and(|(number, _)| number == artist) {
                lidarr.set_new_releases(artist, &setting).map_err(|_| Refusal::LidarrCouldNotRestore)?;
                self.cross_off(artist, setting)?;
            } else {
                return Err(Refusal::LidarrCouldNotRestore);
            }
        }
        Ok(())
    }

    fn cross_off(&self, artist: i64, setting: String) -> Result<(), Refusal> {
        let binding = self.binding;
        self.ask(move |keeper, now, _| {
            keeper.write_data(now, |tx| tx.execute(
                "DELETE FROM requests_way_back WHERE binding = ?1 AND artist = ?2 AND setting = ?3",
                params![binding.as_slice(), artist, setting],
            ).map(|count| ((), count != 0))).map_err(|_| Refusal::NotDealtWith)
        })
    }
}

// A job's outer mutation turn serializes the check, effect and ownership
// commit with every other job. Core's nested WayBack turn uses the same scope.
impl lidarr::WayBack for Journal {
    fn turn(&self) -> Box<dyn lidarr::Turn + '_> {
        Box::new(())
    }

    fn keep(&self, address: &str, artist: i64, before: &str) -> Result<bool, String> {
        let fail = || "Couldn't keep the way back.".to_owned();
        let binding = self.binding;
        if binding != endpoint_binding(address) || !matches!(before, "all" | "new" | "none") {
            return Err(fail());
        }
        let before = before.to_owned();
        let artist_key = self.artist.clone().ok_or_else(fail)?;
        let kept = self.ask(move |keeper, now, _| {
            keeper.write_data(now, |tx| tx.execute(
                "INSERT OR IGNORE INTO requests_way_back (binding, artist, artist_key, setting) VALUES (?1, ?2, ?3, ?4)",
                params![binding.as_slice(), artist, artist_key.as_str(), before],
            ).map(|count| (count != 0, count != 0))).map_err(|_| Refusal::NotDealtWith)
        }).map_err(|_| fail())?;
        if kept { Ok(true) } else { Err(fail()) }
    }

    fn before_artist_albums(&self, added: &lidarr::AddedArtist) -> Result<(), String> {
        let added = added.clone();
        self.ask(move |keeper, now, row| {
            update_work(keeper, now, row, |plan| plan.added_artist = Some(added))
        }).map_err(|_| "Couldn't keep the artist's selected releases.".to_owned())
    }

    fn put_back(&self, address: &str, artist: i64, before: &str) {
        if self.binding == endpoint_binding(address) {
            let _ = self.cross_off(artist, before.to_owned());
        }
    }
}

fn endpoint_binding(address: &str) -> [u8; 32] {
    Sha256::digest(address.trim_end_matches('/')).into()
}

type Changed = (Completed, Vec<AskedRelease>, Option<MusicBrainzId>);

fn lidarr_result<T>(value: Result<T, String>, wanted: Option<&Offer>) -> Result<T, Refusal> {
    value.map_err(|why| wire::refusal_for(&why, wanted))
}

impl Job {
    fn run(&self, journal: &Journal, lidarr: &Lidarr) -> Result<Changed, Refusal> {
        let done = |result| Ok((result, Vec::new(), None));
        match &self.change {
            Change::Request { offer } => {
                let view = self.plan.offers.first().ok_or(Refusal::SearchAgain)?;
                let asked = asked_release(view)?;
                self.request_album(journal, lidarr, &offer.0, &view.clone().into())?;
                Ok((Completed::Requested, vec![asked], None))
            }
            Change::AddArtist { artist } => {
                let name = &self.plan.artist.as_ref().ok_or(Refusal::SearchAgain)?.name;
                let added = lidarr_result(lidarr.add_artist(artist.as_str(), name, journal), None)?;
                added_artist(&added)
            }
            Change::AddWanting { artist, offers: selected } => {
                wire::check_add_wanting(artist, selected, &self.plan.offers)?;
                let offers: Vec<Offer> = self.plan.offers.iter().cloned().map(Offer::from).collect();
                let asked = self.plan.offers.iter().map(asked_release).collect::<Result<_, _>>()?;
                let mut setup = lidarr_result(lidarr.setup(), None)?;
                let artist = lidarr_result(lidarr.add_wanting(&mut setup, &offers), None)?;
                let artist = LidarrNumber::new(artist).ok_or(Refusal::LidarrUnrecognized)?;
                Ok((Completed::AddedWanting { artist }, asked, None))
            }
            Change::Want { artist, albums } => {
                let current = artist_listed(lidarr, artist)?;
                for album in albums {
                    album.check_current(&self.plan.listed)?;
                    album.check_current(&current)?;
                }
                let asked = listed_metadata(lidarr, artist, albums)?;
                let numbers: Vec<_> = albums.iter().map(|album| i64::from(album.id)).collect();
                lidarr_result(lidarr.want(artist.id.into(), &numbers), None)?;
                Ok((Completed::Wanted, asked, None))
            }
            Change::ListSingles { artist, offer } => {
                let view = self.plan.offers.first().ok_or(Refusal::SearchAgain)?;
                let wanted: Offer = view.clone().into();
                let current = lidarr_result(lidarr.known_artist(artist.artist.as_str()), Some(&wanted))?;
                wire::check_list_singles(artist, offer, view, current.map(|(number, _)| number))?;
                let started = lidarr_result(lidarr.start_listing_singles_kept(artist.id.into(), &wanted, journal), Some(&wanted))?;
                if let Some((before, wrote)) = started {
                    let artist = i64::from(artist.id);
                    let started = Instant::now();
                    let watched = HashSet::from([artist]);
                    let wait = (|| {
                        lidarr_result(lidarr.pause(Duration::from_secs(3)), Some(&wanted))?;
                        while lidarr_result(lidarr.busy_with(&watched), Some(&wanted))?.contains(&artist) {
                            if started.elapsed() >= Duration::from_secs(90) {
                                return Err(Refusal::LidarrUnavailable);
                            }
                            lidarr_result(lidarr.pause(Duration::from_secs(2)), Some(&wanted))?;
                        }
                        Ok(())
                    })();
                    let restored = lidarr.set_new_releases(artist, &before).map_err(|_| Refusal::LidarrCouldNotRestore);
                    if restored.is_ok() && wrote {
                        journal.cross_off(artist, before)?;
                    }
                    wait.and(restored)?;
                }
                done(Completed::ListedSingles)
            }
            Change::Cancel { album, release } => {
                let pending = pending_target(lidarr, *album, release)?;
                pending.check_cancel_target(*album, release)?;
                let binding = self.plan.binding;
                let account = self.account;
                let admin = self.admin;
                let target = release.clone();
                journal.ask(move |keeper, _, _| {
                    let current = keeper.load(account.get()).map_err(|_| Refusal::NotAllowed)?;
                    let db = keeper.data().connection();
                    let access = access(keeper, account)?;
                    let owners = owners(db, &binding, account, &[target.as_str()])?;
                    wire::may_cancel(access, account, admin && current.state().admin, owners.get(target.as_str()).map(Vec::as_slice).unwrap_or_default())
                })?;
                lidarr_result(lidarr.cancel((*album).into()), None)?;
                Ok((Completed::Cancelled, Vec::new(), Some(release.clone())))
            }
            Change::Batch { .. } => Err(Refusal::NotDealtWith),
        }
    }

    fn request_album(&self, journal: &Journal, lidarr: &Lidarr, release: &MusicBrainzId, wanted: &Offer) -> Result<(), Refusal> {
        let binding = self.plan.binding;
        let release = release.clone();
        let owned = journal.ask(move |keeper, _, _| {
            keeper.data().connection().query_row(
                "SELECT EXISTS(SELECT 1 FROM requests_owners WHERE binding = ?1 AND release = ?2)",
                params![binding.as_slice(), release.as_str()], |row| row.get::<_, bool>(0),
            ).map_err(sql_error)
        })?;
        // Someone has already had this exact group placed. Associate the new
        // request with it without issuing another AlbumSearch.
        if owned {
            return Ok(());
        }
        lidarr_result(lidarr.request(wanted, journal), Some(wanted))
    }

    /// CARRIED while core supplies interrupted-operation decisions. A restart
    /// can run a job which made no writes, or verify a remote effect which may
    /// already have happened. It must never replay an uncertain mutation.
    fn recover(&self, lidarr: &Lidarr) -> Result<Changed, Refusal> {
        match &self.change {
            Change::Request { .. } => {
                let view = self.plan.offers.first().ok_or(Refusal::SearchAgain)?;
                if album_is_wanted(lidarr, &view.clone().into())? {
                    Ok((Completed::Requested, vec![asked_release(view)?], None))
                } else {
                    Err(Refusal::NotDealtWith)
                }
            }
            Change::Want { artist, albums } => {
                let current = artist_listed(lidarr, artist)?;
                for album in albums {
                    album.check_current(&self.plan.listed)?;
                    already_marked(album, &current)?;
                }
                Ok((Completed::Wanted, listed_metadata(lidarr, artist, albums)?, None))
            }
            Change::AddArtist { artist } => {
                let added = self.plan.added_artist.as_ref().ok_or(Refusal::NotDealtWith)?;
                let id = LidarrNumber::new(added.number).ok_or(Refusal::NotDealtWith)?;
                let current = artist_listed(lidarr, &wire::ArtistTarget { id, artist: artist.clone() })?;
                for marked in &added.newly_marked {
                    let id = LidarrNumber::new(marked.number).ok_or(Refusal::NotDealtWith)?;
                    let release = MusicBrainzId::new(&marked.offer.album).ok_or(Refusal::NotDealtWith)?;
                    already_marked(&wire::AlbumTarget { id, release }, &current)?;
                }
                added_artist(added)
            }
            Change::AddWanting { artist, offers: selected } => {
                wire::check_add_wanting(artist, selected, &self.plan.offers)?;
                let wanted: Offer = self.plan.offers.first().ok_or(Refusal::SearchAgain)?.clone().into();
                let Some((number, _)) = lidarr_result(lidarr.known_artist(&wanted.artist_id), Some(&wanted))? else {
                    return Err(Refusal::NotDealtWith);
                };
                let current = lidarr_result(lidarr.listed(number), Some(&wanted))?;
                if !self.plan.offers.iter().all(|offer| current.iter().any(|row|
                    row.album == offer.album.0.as_str() && (row.wanted || row.have)))
                {
                    return Err(Refusal::NotDealtWith);
                }
                let artist = LidarrNumber::new(number).ok_or(Refusal::LidarrUnrecognized)?;
                let asked = self.plan.offers.iter().map(asked_release).collect::<Result<_, _>>()?;
                Ok((Completed::AddedWanting { artist }, asked, None))
            }
            _ => Err(Refusal::NotDealtWith),
        }
    }
}

fn artist_listed(lidarr: &Lidarr, artist: &wire::ArtistTarget) -> Result<Vec<lidarr::Listed>, Refusal> {
    let current = lidarr_result(lidarr.known_artist(artist.artist.as_str()), None)?;
    artist.check_current(current.map(|(number, _)| number))?;
    lidarr_result(lidarr.listed(artist.id.into()), None)
}

fn listed_metadata(lidarr: &Lidarr, artist: &wire::ArtistTarget, albums: &[wire::AlbumTarget]) -> Result<Vec<AskedRelease>, Refusal> {
    let current = lidarr_result(lidarr.known_artist(artist.artist.as_str()), None)?;
    artist.check_current(current.as_ref().map(|(number, _)| *number))?;
    let (_, name) = current.ok_or(Refusal::SearchAgain)?;
    let releases = lidarr_result(lidarr.releases_of(artist.id.into(), &name), None)?;
    let views: Vec<AlbumView> = lidarr_items(releases.into_iter().map(|(mut offer, _)| {
        if offer.artist_id.is_empty() {
            offer.artist_id = artist.artist.as_str().to_owned();
        }
        offer
    }));
    albums.iter().map(|album| {
        let view = views.iter().find(|view| view.album.0 == album.release
            && view.artist_id.as_ref() == Some(&artist.artist)).ok_or(Refusal::NoLongerListed)?;
        asked_release(view)
    }).collect()
}

fn already_marked(album: &wire::AlbumTarget, current: &[lidarr::Listed]) -> Result<(), Refusal> {
    album.check_current(current)?;
    if current.iter().any(|row| row.id == i64::from(album.id) && (row.wanted || row.have)) {
        Ok(())
    } else {
        Err(Refusal::NotDealtWith)
    }
}

fn added_artist(added: &lidarr::AddedArtist) -> Result<Changed, Refusal> {
    let count = u32::try_from(added.newly_marked.len()).map_err(|_| Refusal::TooLarge)?;
    let views: Vec<AlbumView> = lidarr_items(added.newly_marked.iter().map(|marked| marked.offer.clone()));
    let asked = views.iter().map(asked_release).collect::<Result<Vec<_>, _>>()?;
    Ok((Completed::ArtistAdded { count }, asked, None))
}

fn album_is_wanted(lidarr: &Lidarr, wanted: &Offer) -> Result<bool, Refusal> {
    let Some((artist, _)) = lidarr_result(lidarr.known_artist(&wanted.artist_id), Some(wanted))? else {
        return Ok(false);
    };
    let listed = lidarr_result(lidarr.listed(artist), Some(wanted))?;
    Ok(listed.iter().any(|known| known.album == wanted.album && (known.wanted || known.have)))
}

fn pending_target(lidarr: &Lidarr, album: LidarrNumber, release: &MusicBrainzId) -> Result<PendingView, Refusal> {
    let rows: Vec<PendingView> = lidarr_items(lidarr_result(lidarr.pending(), None)?);
    rows.into_iter().find(|row| row.check_cancel_target(album, release).is_ok()).ok_or(Refusal::NoLongerListed)
}

impl Requests {
    fn run_batch(
        &self, job: &Job, journal: &Journal, lidarr: &Lidarr, singles: bool, lines: &[BatchLine],
    ) -> Result<Option<Vec<AskedRelease>>, Refusal> {
        let desk = BatchDesk { journal: journal.clone(), singles };
        let batch = if let Some(payload) = &job.plan.prepared_batch {
            loaded_bulk(payload, desk)?
        } else {
            let mut wants = Vec::with_capacity(lines.len());
            let mut waiting = false;
            for (index, line) in lines.iter().enumerate() {
                match prepare_line(journal, lidarr, index as u16, line, singles)? {
                    Some(want) => wants.push(want),
                    None => waiting = true,
                }
            }
            if waiting {
                return Ok(None);
            }
            let batch = bulk::Job::new(desk);
            lidarr_result(batch.start_indexed(wants), None)?;
            batch
        };

        let _turn = lock(&self.mutation);
        journal.restore_pending(lidarr)?;
        if job.plan.prepared_batch.is_some() {
            let indexed = batch.indexed_lines().ok_or(Refusal::NotDealtWith)?;
            let mut reconciled = HashSet::new();
            for line in indexed.iter().filter(|line| {
                line.before_effect && matches!(line.state, bulk::State::Found | bulk::State::Asked)
            }) {
                if reconciled.insert(line.slot) {
                    let confirmed = line.offer.as_ref().map(|offer| confirmed_batch_offer(lidarr, offer))
                        .transpose()?.unwrap_or(false);
                    lidarr_result(batch.reconcile_indexed(line.slot, confirmed), line.offer.as_ref())?;
                }
            }
        }

        let worked = lidarr_result(batch.work_prepared(lidarr, &|_| {}), None);
        let rows = batch.indexed_lines().ok_or(Refusal::NotDealtWith)?;
        let views: Vec<AlbumView> = lidarr_items(rows.into_iter().filter_map(|line| {
            matches!(line.state, bulk::State::Asked | bulk::State::Done).then_some(line.offer).flatten()
        }));
        let asked = views.iter().map(asked_release).collect::<Result<Vec<_>, _>>()?;
        if let Err(reason) = worked {
            let account = job.account;
            let binding = job.plan.binding;
            journal.ask(move |keeper, now, row| {
                finish_job(keeper, now, row, &JobResult::Failed(reason), &asked, Some((binding, account, None)))
            })?;
            return Err(reason);
        }
        Ok(Some(asked))
    }
}

fn confirmed_batch_offer(lidarr: &Lidarr, offer: &Offer) -> Result<bool, Refusal> {
    let artist = MusicBrainzId::new(&offer.artist_id).ok_or(Refusal::LidarrUnrecognized)?;
    let Some((number, _)) = lidarr_result(lidarr.known_artist(artist.as_str()), Some(offer))? else {
        return Ok(false);
    };
    let Some(id) = LidarrNumber::new(number) else { return Ok(false) };
    let listed = lidarr_result(lidarr.listed(number), Some(offer))?;
    Ok(wire::batch_effect_confirmed(offer, &wire::ArtistTarget { id, artist }, Some(number), &listed))
}

fn prepare_line(
    journal: &Journal, lidarr: &Lidarr, index: u16, line: &BatchLine, singles: bool,
) -> Result<Option<bulk::Want>, Refusal> {
    let mut saved = journal.ask(move |keeper, _, row| saved_line(keeper.data().connection(), row, index))?;
    let mut updated = false;
    if saved.progress.is_none() {
        updated = true;
        saved.progress = Some(match find_line(lidarr, line, singles) {
            Ok(Some(view)) => {
                let album = batch_album(&view);
                saved.offer = Some(view);
                BatchLineProgress::Found { album }
            }
            Ok(None) => BatchLineProgress::NeedsResolution,
            Err(reason) => BatchLineProgress::Failed { reason, album: None },
        });
    }
    if matches!(saved.progress, Some(BatchLineProgress::NeedsResolution)) {
        if let Some(resolution) = &mut saved.resolution {
            let selected = if resolution.lookup_started {
                Err(Refusal::NotDealtWith)
            } else if let Some(release) = resolution.release.clone() {
                resolution.lookup_started = true;
                let started = encode(&saved)?;
                journal.ask(move |keeper, now, row| save_line(keeper, now, row, index, &decode(&started)?))?;
                let search = format!("lidarr:{}", release.as_str());
                lidarr_result(lidarr.find_any(&search), None)
                    .map(|found| wire::batch_fallback_offer(&release, singles, &found).cloned())
            } else {
                Ok(None)
            };
            saved.progress = Some(match selected {
                Ok(found) => {
                    saved.offer = lidarr_items::<AlbumView, _>(found).pop().filter(|view| view.artist_id.is_some());
                    match &saved.offer {
                        Some(view) => BatchLineProgress::Found { album: batch_album(view) },
                        None => BatchLineProgress::NotFound,
                    }
                }
                Err(reason) => BatchLineProgress::Failed { reason, album: None },
            });
            updated = true;
        }
    }
    let want = match &saved.progress {
        Some(BatchLineProgress::Found { .. }) => Some(bulk::Want {
            artist: line.artist.clone(), album: line.album.clone(), songs: vec![line.song.clone()],
            state: bulk::State::Found, offer: saved.offer.clone().map(Offer::from), ..Default::default()
        }),
        Some(BatchLineProgress::NotFound) => Some(bulk::Want { state: bulk::State::Unknown, ..Default::default() }),
        Some(BatchLineProgress::Failed { reason, .. }) => Some(bulk::Want {
            state: bulk::State::Failed, reason: reason.sentence(None).to_string(), ..Default::default()
        }),
        _ => None,
    };
    if updated {
        journal.ask(move |keeper, now, row| save_line(keeper, now, row, index, &saved))?;
    }
    Ok(want)
}

fn batch_album(view: &AlbumView) -> BatchAlbum {
    BatchAlbum { offer: view.album.clone(), title: view.title.clone(), artist: view.artist.clone() }
}

// Name-based lookup ends at Lidarr. An unresolved line asks the original
// device for its MusicBrainz lookup and stores that answer before using it.
fn find_line(lidarr: &Lidarr, line: &BatchLine, singles: bool) -> Result<Option<AlbumView>, Refusal> {
    if line.album.is_empty() {
        return Ok(None);
    }
    let find = |name: &str| {
        lidarr_result(lidarr.find_any(&format!("{name} {}", line.artist)), None)
            .map(|found| bulk::choose(&found, &line.artist, &line.album, singles))
    };
    let mut found = find(&line.album)?;
    let bare = nexus_core::text::undecorated(&line.album);
    if found.is_none() && bare != line.album.trim() {
        found = find(&bare)?;
    }
    Ok(lidarr_items::<AlbumView, _>(found).pop().filter(|view| view.artist_id.is_some()))
}

struct BatchDesk {
    journal: Journal,
    singles: bool,
}

impl bulk::Desk for BatchDesk {
    fn keep(&self, batch: Option<Vec<u8>>, _: bool) {
        if let Some(batch) = batch {
            let _ = self.keep_checked(batch);
        }
    }

    fn keep_checked(&self, batch: Vec<u8>) -> bool {
        self.journal.ask(move |keeper, now, row| {
            update_work(keeper, now, row, |plan| plan.prepared_batch = Some(batch))
        }).is_ok()
    }

    fn now(&self) -> i64 {
        seconds((self.journal.service.accounts.clock)())
    }

    fn singles(&self) -> bool {
        self.singles
    }
}

fn update_work(
    keeper: &mut Keeper, now: u64, row: i64, change: impl FnOnce(&mut Plan),
) -> Result<(), Refusal> {
    let work: Vec<u8> = keeper.data().connection().query_row(
        "SELECT work FROM requests_jobs WHERE id = ?1",
        [row], |record| record.get(0),
    ).map_err(sql_error)?;
    let mut plan: Plan = decode(&work)?;
    change(&mut plan);
    let work = encode(&plan)?;
    keeper.write_data(now, |tx| {
        tx.execute("UPDATE requests_jobs SET work = ?1 WHERE id = ?2", params![work, row])
            .map(|count| ((), count != 0))
    }).map_err(|_| Refusal::NotDealtWith)
}

fn credential(keeper: &mut Keeper, endpoint: &Endpoint) -> Result<Option<Credential>, Refusal> {
    let saved = keeper.lidarr(Some(&endpoint.address)).map_err(|_| Refusal::NotDealtWith)?;
    let (Some(binding), Some(sealed)) = (saved.binding, saved.sealed) else { return Ok(None) };
    let sealing = admin::LidarrSealingKey::new(keeper.seal_key());
    Ok(sealing.open(&binding, &sealed).map(|key| Credential { revision: binding.revision, key }))
}

pub(crate) fn guards(path: &str) -> bool {
    matches!(path, wire::REQUESTS | admin::REQUEST_GRANT | admin::SET_REQUESTS_ENABLED | admin::GET_LIDARR_SETTING | admin::SET_LIDARR | wire::ADMIN_ATTRIBUTION)
}

fn admin_reply<T: admin::ResultBody>(answer: Result<T, admin::Refusal>) -> Response<Body> {
    let reply = match answer {
        Ok(result) => admin::Reply::Done(result),
        Err(why) => admin::Reply::Refused(why),
    };
    match admin::encode(&reply) {
        Ok(body) => events::respond(crate::accounts::session::Answer { status: reply.status(), body }),
        Err(_) => events::respond(crate::accounts::music::refused(admin::Refusal::NotDealtWith)),
    }
}

fn admin_denied() -> admin::Refusal {
    admin::Refusal::NotAllowed(admin::Denial::Denied)
}

fn send_admin_denied() -> Response<Body> {
    admin_reply::<admin::Applied>(Err(admin_denied()))
}

fn read_admin<T>(body: Bytes, read: impl FnOnce(&[u8]) -> Result<T, account::BadMessage>) -> Result<T, admin::Refusal> {
    use zeroize::Zeroize;
    let result = read(&body).map_err(|_| admin::Refusal::Malformed);
    if let Ok(mut unique) = body.try_into_mut() {
        unique.as_mut().zeroize();
    }
    result
}

async fn admin_grant(
    State(service): State<Arc<Requests>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response<Body> {
    if request.uri().path() != admin::REQUEST_GRANT {
        return next.run(request).await;
    }
    let (parts, body) = request.into_parts();
    let Some(ingress) = parts.extensions.get::<Arc<Ingress>>() else { return send_admin_denied() };
    let Ok(session) = bearer(&parts.headers) else { return send_admin_denied() };
    let Ok(body) = axum::body::to_bytes(body, account::PASSWORD_REQUEST_MAX_BYTES).await else {
        return admin_reply::<admin::Applied>(Err(admin::Refusal::Malformed));
    };
    let grant = match read_admin(body, admin::read::<admin::RequestGrant>) {
        Ok(grant) => grant,
        Err(reason) => return admin_reply::<admin::Applied>(Err(reason)),
    };
    if !matches!(&grant.change.action, admin::Action::SetRequestsEnabled(_) | admin::Action::SetLidarr(_)) {
        return events::respond(service.accounts.admin_grant(session, grant).await);
    }
    if !ingress.page || ingress.kind != SessionKind::AdminPage {
        return send_admin_denied();
    }
    admin_reply(requests_grant(&service, ingress.clone(), grant).await)
}

async fn requests_grant(
    service: &Requests,
    ingress: Arc<Ingress>,
    request: admin::RequestGrant,
) -> Result<admin::GrantIssued, admin::Refusal> {
    use admin::Refusal as Why;

    let account::GrantRequest { head, change, password } = request;
    let restore = head.restore;
    service.accounts.live(&head, &ingress.hash).map_err(admin_from_account)?;
    let address = service.endpoint.as_ref().map(|endpoint| endpoint.address.clone());
    let clock = service.accounts.clock.clone();
    let busy = || Why::Busy { retry_after_seconds: account::Wait::new(1) };
    let (number, device, email, check, stored) = service.accounts.desk.ask(move |keeper| {
        let now = clock();
        let (device, mut caller) = admin_actor(keeper, &head, &ingress, now)?;
        let intent = match &change.action {
            admin::Action::SetRequestsEnabled(request) => {
                let target = admin_target(keeper, &caller, request.account)?;
                let current = keeper.requests_switch(request.account.get()).map_err(|_| Why::NotDealtWith)?;
                change.requests_grant_intent(target.as_ref().unwrap_or(&caller).state(), current.revision)
            }
            admin::Action::SetLidarr(_) => {
                let binding = current_lidarr(keeper, address.as_ref())?;
                change.lidarr_grant_intent(&binding)
            }
            _ => None,
        }.ok_or(Why::Malformed)?;
        let number = caller.state().id.get();
        let email = caller.state().email.clone();
        let stored = keeper.data().password(number).map_err(|_| Why::NotDealtWith)?;
        let expected = Fingerprint::from_bytes(stored.as_ref().map_or([0; 32], |kept| kept.hash));
        let bearer = Bearer { device, hash: &ingress.hash, grant: None };
        let check = caller.request(head.restore, keeper.clock(now), |turn| {
            turn.check_leave(bearer, intent, expected)
        }).apply(|effects| keeper.persist(effects, &[], now))
            .map_err(|_| Why::NotDealtWith)?
            .map_err(admin_said)?;
        Ok((number, device, email, check, stored))
    }).await.map_err(|_| busy())??;
    let checked = service.accounts.workers.check(
        &email, account::PasswordDoor::KnownDevice, Some(device), password, stored,
    ).await.map_err(|_| busy())?;
    let Checked::Match { derived, .. } = checked else {
        return Err(Why::NotAllowed(admin::Denial::WrongPassword));
    };
    let clock = service.accounts.clock.clone();
    service.accounts.desk.ask(move |keeper| {
        let now = clock();
        let receipt = check.check(derived, keeper.clock(now)).map_err(admin_said)?;
        let mut account = keeper.load(number).map_err(|_| Why::NotDealtWith)?;
        let grant = admin::GrantSecret::from_bytes(*random::<32>().ok_or(Why::NotDealtWith)?);
        account.request(restore, keeper.clock(now), |turn| turn.grant_leave(receipt, &grant))
            .apply(|effects| keeper.persist(effects, &[], now))
            .map_err(|_| Why::NotDealtWith)?
            .map_err(admin_said)?;
        Ok(admin::GrantIssued { grant })
    }).await.map_err(|_| busy())?
}

fn admin_target(keeper: &mut Keeper, actor: &Account, target: AccountId) -> Result<Option<Account>, admin::Refusal> {
    if actor.state().id == target {
        return Ok(None);
    }
    if !rows::account_exists(keeper.data().connection(), target.get()).map_err(|_| admin::Refusal::NotDealtWith)? {
        return Err(admin::Refusal::NotAllowed(admin::Denial::NoSuchAccount));
    }
    keeper.load(target.get()).map(Some).map_err(|_| admin::Refusal::NotDealtWith)
}

fn current_lidarr(
    keeper: &mut Keeper, address: Option<&admin::InternalAddress>,
) -> Result<admin::LidarrBinding, admin::Refusal> {
    keeper.lidarr(address).map_err(|_| admin::Refusal::NotDealtWith)?.binding
        .ok_or(admin::Refusal::NotAllowed(admin::Denial::Conflict))
}

async fn set_requests_enabled(
    State(service): State<Arc<Requests>>,
    axum::Extension(ingress): axum::Extension<Arc<Ingress>>,
    body: Bytes,
) -> Response<Body> {
    if !ingress.page || ingress.kind != SessionKind::AdminPage {
        return send_admin_denied();
    }
    let request = match read_admin(body, admin::read::<admin::SetRequestsEnabled>) {
        Ok(request) => request,
        Err(why) => return admin_reply::<admin::Applied>(Err(why)),
    };
    let clock = service.accounts.clock.clone();
    let finished = service.accounts.desk.ask(move |keeper| {
        let now = clock();
        let (device, mut actor) = admin_actor(keeper, &request.head, &ingress, now)?;
        let account = request.change.account;
        let mut target = admin_target(keeper, &actor, account)?;
        let current = keeper.requests_switch(account.get()).map_err(|_| admin::Refusal::NotDealtWith)?;
        let grant = GrantUse { request_id: request.request_id, secret: &request.grant };
        let bearer = Bearer { device, hash: &ingress.hash, grant: Some(grant) };
        actor.request(request.head.restore, keeper.clock(now), |turn| {
            turn.set_requests_enabled(bearer, account, request.change.enabled, current.revision, target.as_mut())
        }).apply(|effects| keeper.persist(effects, &[], now))
            .map_err(|_| admin::Refusal::NotDealtWith)?
            .map_err(admin_said)?;
        Ok(admin::Applied {})
    }).await.unwrap_or(Err(admin::Refusal::Busy { retry_after_seconds: account::Wait::new(1) }));
    admin_reply(finished)
}

fn admin_actor(keeper: &mut Keeper, head: &RequestHead, ingress: &Ingress, now: u64) -> Result<(DeviceId, Account), admin::Refusal> {
    let (_, device, mut actor, kind) = events::authenticated(keeper, head, &ingress.hash, Access::Read, now)
        .map_err(admin_from_account)?;
    if session_id(&actor, device).map_err(admin_from_account)? != ingress.session || kind != SessionKind::AdminPage {
        return Err(admin_denied());
    }
    actor.request(head.restore, keeper.clock(now), |turn| {
        turn.admin_read(Bearer { device, hash: &ingress.hash, grant: None })
    }).apply(|effects| keeper.persist(effects, &[], now))
        .map_err(|_| admin::Refusal::NotDealtWith)?
        .map_err(admin_said)?;
    Ok((device, actor))
}

fn admin_from_account(why: account::Refusal) -> admin::Refusal {
    match why {
        account::Refusal::Malformed => admin::Refusal::Malformed,
        account::Refusal::NotSignedIn(reason) => admin::Refusal::NotSignedIn(reason),
        account::Refusal::NotAllowed(_) => admin_denied(),
        account::Refusal::Busy { retry_after_seconds } => admin::Refusal::Busy { retry_after_seconds },
        account::Refusal::NotDealtWith => admin::Refusal::NotDealtWith,
    }
}

fn admin_said(why: Denied) -> admin::Refusal {
    match why {
        Denied::NotSignedIn => admin::Refusal::NotSignedIn(NotSignedIn::SessionEnded),
        Denied::StartAgain => admin::Refusal::NotSignedIn(NotSignedIn::DataPutBack),
        Denied::WrongPurpose => admin::Refusal::NotAllowed(admin::Denial::Conflict),
        Denied::LeaveRequired => admin::Refusal::NotAllowed(admin::Denial::GrantRequired),
        Denied::WrongPassword => admin::Refusal::NotAllowed(admin::Denial::WrongPassword),
        Denied::Time => admin::Refusal::Busy { retry_after_seconds: account::Wait::new(1) },
        Denied::Exhausted | Denied::ReusedSession | Denied::DeviceInUse => admin::Refusal::NotDealtWith,
        Denied::AdminRequired | Denied::AccountOff | Denied::HistoryHeld => admin_denied(),
    }
}

async fn get_lidarr(
    State(service): State<Arc<Requests>>,
    axum::Extension(ingress): axum::Extension<Arc<Ingress>>,
    body: Bytes,
) -> Response<Body> {
    if !ingress.page || ingress.kind != SessionKind::AdminPage {
        return send_admin_denied();
    }
    let request = match read_admin(body, admin::read::<admin::GetLidarrSetting>) {
        Ok(request) => request,
        Err(why) => return admin_reply::<admin::LidarrSetting>(Err(why)),
    };
    let clock = service.accounts.clock.clone();
    let endpoint = service.endpoint.clone();
    let configured = service.accounts.desk.ask(move |keeper| {
        admin_actor(keeper, &request.head, &ingress, clock())?;
        let address = endpoint.as_ref().map(|endpoint| endpoint.address.clone());
        let configured = match endpoint {
            None => false,
            Some(endpoint) => credential(keeper, &endpoint).map(|key| key.is_some())
                .map_err(|_| admin::Refusal::NotDealtWith)?,
        };
        Ok(admin::LidarrSetting { address, configured })
    }).await.unwrap_or(Err(admin::Refusal::Busy { retry_after_seconds: account::Wait::new(1) }));
    admin_reply(configured)
}

async fn set_lidarr(
    State(service): State<Arc<Requests>>,
    axum::Extension(ingress): axum::Extension<Arc<Ingress>>,
    body: Bytes,
) -> Response<Body> {
    if !ingress.page || ingress.kind != SessionKind::AdminPage {
        return send_admin_denied();
    }
    let request = match read_admin(body, admin::read::<admin::SetLidarr>) {
        Ok(request) => request,
        Err(why) => return admin_reply::<admin::Applied>(Err(why)),
    };
    let address = service.endpoint.as_ref().map(|endpoint| endpoint.address.clone());
    let clock = service.accounts.clock.clone();
    let finished = service.accounts.desk.ask(move |keeper| {
        let now = clock();
        let (device, mut actor) = admin_actor(keeper, &request.head, &ingress, now)?;
        let binding = current_lidarr(keeper, address.as_ref())?;
        let sealing = admin::LidarrSealingKey::new(keeper.seal_key());
        let nonce = random::<{ admin::LIDARR_NONCE_BYTES }>()
            .ok_or(admin::Refusal::NotDealtWith)?;
        let grant = GrantUse { request_id: request.request_id, secret: &request.grant };
        let bearer = Bearer { device, hash: &ingress.hash, grant: Some(grant) };
        actor.request(request.head.restore, keeper.clock(now), |turn| {
            turn.set_lidarr(bearer, &binding, &request.change.key, &sealing, *nonce)
        }).apply(|effects| keeper.persist(effects, &[], now))
            .map_err(|_| admin::Refusal::NotDealtWith)?
            .map_err(admin_said)?;
        Ok(admin::Applied {})
    }).await.unwrap_or(Err(admin::Refusal::Busy { retry_after_seconds: account::Wait::new(1) }));
    if finished.is_ok() {
        service.wake.notify_one();
    }
    admin_reply(finished)
}

async fn attribution(
    State(service): State<Arc<Requests>>,
    axum::Extension(ingress): axum::Extension<Arc<Ingress>>,
    body: Bytes,
) -> Response<Body> {
    if !ingress.page || ingress.kind != SessionKind::AdminPage {
        return send_admin_denied();
    }
    let request = match wire::read_attribution(&body) {
        Ok(request) => request,
        Err(_) => return admin_reply::<AttributionPage>(Err(admin::Refusal::Malformed)),
    };
    let clock = service.accounts.clock.clone();
    let state = service.clone();
    let finished = service.accounts.desk.ask(move |keeper| {
        admin_actor(keeper, &request.head, &ingress, clock())?;
        let answer = if let Some(after) = request.after {
            lock(&state.books).attribution_next(ingress.session, after)
        } else {
            let rows = state.endpoint.as_ref().map(|endpoint| {
                attributed_requests(keeper.data().connection(), &binding(endpoint))
            }).transpose().map_err(|_| admin::Refusal::NotDealtWith)?.unwrap_or_default();
            lock(&state.books).attribution_first(ingress.session, rows)
        };
        answer.map_err(|why| match why {
            Refusal::SearchAgain => admin::Refusal::NotAllowed(admin::Denial::Conflict),
            _ => admin::Refusal::NotDealtWith,
        })
    }).await.unwrap_or(Err(admin::Refusal::Busy { retry_after_seconds: account::Wait::new(1) }));
    let reply = match finished {
        Ok(page) => admin::Reply::Done(page),
        Err(why) => admin::Reply::Refused(why),
    };
    match wire::write_attribution_reply(&reply) {
        Ok(body) => events::respond(crate::accounts::session::Answer { status: reply.status(), body }),
        Err(_) => admin_reply::<AttributionPage>(Err(admin::Refusal::NotDealtWith)),
    }
}
