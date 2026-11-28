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
use nexus_core::bulk::PreparedBatch;
use nexus_core::lidarr::{self, AlbumPlacement, Lidarr, PlacementRecovery};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::runtime::Handle;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto, pki_types::ServerName};
use url::Url;

use std::collections::{HashMap, HashSet, VecDeque};
use std::num::NonZeroU64;
use std::sync::{Mutex, MutexGuard};

use axum::{Extension, Router};
use axum::body::Body;
use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, Response, Uri};
use axum::routing::post;
use nexus_core::nexus::account::{
    self, AccountId, DeviceId, NotSignedIn, Receipt, RequestHead, SessionId,
};
use nexus_core::nexus::admin;
use nexus_core::nexus::events::clean_display_text;
use nexus_core::nexus::kept::{
    Access, Account, Bearer, Denied, Fingerprint, GrantUse, SessionKind, Status,
};
use nexus_core::nexus::requests::{
    self as wire, AlbumView, Answer, ArtistView, AskedRelease, AttributionPage, AttributionRow,
    Availability, BatchAlbum, BatchLine,
    BatchLineProgress, BatchLineView, Call, Change, Completed, JobId, LidarrNumber,
    ListedView, MusicBrainzId, PageCursor, Refusal, Search, SessionOffers,
    lidarr_items,
};
use nexus_core::offer::Offer;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

use crate::accounts::{admit::Caller, rows, session::Accounts, setup::random};
use crate::events;
use crate::store::keeper::Keeper;

const UPSTREAM_TIMEOUT: Duration = Duration::from_secs(8);
// The released Windows Lidarr line allows 30 seconds for response headers.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const ANSWER_TIMEOUT: Duration = Duration::from_secs(33);
const UPSTREAM_HEAD_BYTES: usize = 16 * 1024;

#[derive(Clone)]
pub struct Endpoint {
    address: admin::InternalAddress,
    url: Url,
    api: String,
    tls: Option<TlsConnector>,
}

impl Endpoint {
    fn new(address: admin::InternalAddress) -> Option<Self> {
        let url = Url::parse(address.as_str()).ok()?;
        let origin = lidarr::origin(address.as_str()).ok()?;
        if !matches!(url.scheme(), "http" | "https") || url.port_or_known_default()? != origin.port
            || !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some()
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
        Some(Self { address, url, api, tls })
    }

    fn lidarr(&self, runtime: Handle, key: &str, deadline: Option<Instant>) -> Lidarr {
        let within = deadline.map_or(UPSTREAM_TIMEOUT, |deadline| deadline.saturating_duration_since(Instant::now()));
        Lidarr::new(self.url.as_str(), key, None, Arc::new(Line {
            endpoint: self.clone(), runtime, deadline, within, before_write: None,
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
const BOOKS_PER_ACCOUNT: usize = 4;
const MAX_SNAPSHOTS: usize = 16;
const ACCOUNT_SLOT_MAX: usize = 24;

pub(crate) struct Requests {
    accounts: Arc<Accounts>,
    origin: Option<Arc<str>>,
    endpoint: Option<Endpoint>,
    rooms: Arc<Mutex<Rooms>>,
    books: Mutex<Books>,
    wake: Arc<Notify>,
    mutation: Arc<tokio::sync::Mutex<()>>,
}

impl Requests {
    pub(crate) fn new(accounts: Arc<Accounts>, origin: Option<Arc<str>>) -> Arc<Self> {
        let endpoint = accounts.lidarr.clone().and_then(Endpoint::new);
        Arc::new(Self {
            accounts,
            origin,
            endpoint,
            rooms: Arc::default(),
            books: Mutex::default(),
            wake: Arc::new(Notify::new()),
            mutation: Arc::default(),
        })
    }

    pub(crate) fn router(self: &Arc<Self>, base: Router) -> Router {
        base.merge(Router::new()
            .route(wire::REQUESTS, post(call))
            .route(wire::ADMIN_REQUESTS, post(call))
            .route(admin::SET_REQUESTS_ENABLED, post(set_requests_enabled))
            .route(admin::GET_LIDARR_SETTING, post(get_lidarr))
            .route(admin::SET_LIDARR, post(set_lidarr))
            .route(wire::ADMIN_ATTRIBUTION, post(attribution))
            .with_state(self.clone()))
    }

    fn admitted(
        &self, headers: &HeaderMap, caller: Option<Extension<Caller>>, head: &RequestHead,
    ) -> Result<Ingress, account::Refusal> {
        let hash = Fingerprint::of(bearer(headers)?.expose_bytes());
        // The shared listener can withhold Caller for an unproven bearer after a
        // put-back. Use the body's restore number to issue the expected refusal.
        self.accounts.live(head, &hash)?;
        let Extension(caller) = caller.ok_or_else(ended)?;
        Ok(Ingress { hash, caller })
    }

    async fn shown<T: Send + 'static>(
        &self,
        ingress: Ingress,
        head: RequestHead,
        step: impl FnOnce(&mut Keeper, Actor) -> Result<T, account::Refusal> + Send + 'static,
    ) -> Result<(T, Receipt), account::Refusal> {
        self.accounts.live(&head, &ingress.hash)?;
        let clock = self.accounts.clock.clone();
        events::answer(self.accounts.desk.ask(move |keeper| {
            let now = clock();
            let (_, device, account, kind) = events::authenticated(keeper, &head, &ingress.hash, Access::Read, now)?;
            ingress.check(&account, device, kind)?;
            let actor = Actor { account: account.state().id, session: ingress.caller.session, admin_page: kind == SessionKind::AdminPage && account.state().admin,
                restore: head.restore, now: keeper.clock(now).effective() };
            let done = step(keeper, actor)?;
            Ok((done, events::receipt(keeper, account.state())?))
        }).await)
    }
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

#[derive(Clone)]
struct Ingress {
    hash: Fingerprint,
    caller: Caller,
}

impl Ingress {
    fn check(&self, account: &Account, device: DeviceId, kind: SessionKind) -> Result<(), account::Refusal> {
        if account.state().id != self.caller.account || device != self.caller.device
            || session_id(account, device)? != self.caller.session || kind != self.caller.kind
        {
            return Err(ended());
        }
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct Actor {
    account: AccountId,
    session: SessionId,
    admin_page: bool,
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
    jobs: HashMap<i64, ClaimedJob>,
    working_jobs: HashSet<i64>,
    recovery_jobs: HashMap<([u8; 32], i64), i64>,
    available: Arc<Notify>,
}

// A claimed durable job stays unique while it waits or yields. The separate
// Place is acquired only for actual upstream work.
struct JobReservation {
    rooms: Arc<Mutex<Rooms>>,
    row: i64,
}

struct ClaimedJob {
    account: u64,
    batch: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum ClaimBlocked {
    Waiting,
    BatchesFull,
}

impl JobReservation {
    fn take(rooms: Arc<Mutex<Rooms>>, row: i64, account: u64, batch: bool) -> Result<Self, ClaimBlocked> {
        let mut held = lock(&rooms);
        if held.jobs.values().any(|job| job.account == account) {
            return Err(ClaimBlocked::Waiting);
        }
        // Batches keep their reservation even when a group gives back its
        // mutation turn. Leave a worker for a single change during those gaps.
        if batch && held.jobs.values().filter(|job| job.batch).count() >= wire::RUNNING_MAX - 1 {
            return Err(ClaimBlocked::BatchesFull);
        }
        if held.jobs.len() >= wire::RUNNING_MAX {
            return Err(ClaimBlocked::Waiting);
        }
        held.jobs.insert(row, ClaimedJob { account, batch });
        drop(held);
        Ok(Self { rooms, row })
    }
}

impl Drop for JobReservation {
    fn drop(&mut self) {
        let mut rooms = lock(&self.rooms);
        rooms.jobs.remove(&self.row);
        rooms.recovery_jobs.retain(|_, job| *job != self.row);
    }
}

struct MutationTurn {
    _upstream: Place,
    _turn: tokio::sync::OwnedMutexGuard<()>,
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
            held.working_jobs.insert(job);
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
            held.working_jobs.remove(&job);
        }
        if self.upstream {
            held.available.notify_waiters();
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

    fn timeout(&self) -> Duration {
        match self {
            Self::FindReleases(_) | Self::FindAny(_) | Self::FindArtists(_) => SEARCH_TIMEOUT,
            _ => UPSTREAM_TIMEOUT,
        }
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
                    let name = clean_display_text(&name);
                    (name.len() <= wire::NAME_MAX_BYTES).then_some((LidarrNumber::new(number)?, name))
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
    sessions: VecDeque<(AccountId, SessionId, SessionBook)>,
    snapshots: VecDeque<Snapshot>,
    last_snapshot: u64,
}

struct Snapshot {
    id: NonZeroU64,
    account: AccountId,
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

    fn keep_snapshot(&mut self, id: NonZeroU64, account: AccountId, session: SessionId, content: SnapshotContent) {
        self.expire();
        let matching = self.snapshots.iter().enumerate().filter(|(_, known)| known.account == account);
        if let Some((index, _)) = matching.clone().next().filter(|_| matching.count() >= wire::SNAPSHOTS_PER_SESSION) {
            self.snapshots.remove(index);
        }
        if self.snapshots.len() == MAX_SNAPSHOTS {
            self.snapshots.pop_front();
        }
        self.snapshots.push_back(Snapshot { id, account, session, last_seen: Instant::now(), content });
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

    fn first(&mut self, account: AccountId, session: SessionId, read: &Read, found: Answer) -> Result<Answer, Refusal> {
        let Some((count, limit)) = page_size(&found) else { return Ok(found) };
        let id = self.next_id()?;
        let answer = page(&found, id, 0)?;
        if count > limit {
            self.keep_snapshot(id, account, session, SnapshotContent::App { read: read.clone(), found });
        }
        Ok(answer)
    }

    fn attribution_first(&mut self, account: AccountId, session: SessionId, rows: Vec<AttributionRow>) -> Result<AttributionPage, Refusal> {
        let id = self.next_id()?;
        let answer = attribution_page(&rows, id, 0)?;
        if answer.next.is_some() {
            self.keep_snapshot(id, account, session, SnapshotContent::Attribution(rows));
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

    fn remember(&mut self, account: AccountId, session: SessionId, now: i64, read: &Read, page: &Answer) {
        let (mut book, artists) = match page {
            Answer::FoundAny { albums, .. } | Answer::FoundReleases { albums, .. } => (albums.clone(), &[][..]),
            Answer::FoundArtists { artists, .. } => (Vec::new(), artists.as_slice()),
            Answer::AlbumsOf { albums, .. } | Answer::ReleasesOf { releases: albums, .. } =>
                (albums.iter().map(|release| release.album.clone()).collect(), &[][..]),
            Answer::Listed { .. } => (Vec::new(), &[][..]),
            _ => return,
        };
        let existing = self.sessions.iter().position(|(owner, known, _)| *owner == account && *known == session)
            .and_then(|index| self.sessions.remove(index)).map(|(_, _, offers)| offers);
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
        let own = self.sessions.iter().enumerate().filter(|(_, (owner, _, _))| *owner == account);
        if let Some((index, _)) = own.clone().next().filter(|_| own.count() >= BOOKS_PER_ACCOUNT) {
            self.sessions.remove(index);
        }
        if self.sessions.len() == MAX_BOOKS {
            self.sessions.pop_front();
        }
        self.sessions.push_back((account, session, offers));
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
        arrived: row.arrived,
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
    placements: Vec<AlbumPlacement>,
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
        let book = || books.sessions.iter().find(|(owner, id, _)| *owner == actor.account && *id == actor.session)
            .map(|(_, _, book)| book).ok_or(Refusal::SearchAgain);
        let now = seconds(actor.now);
        let mut plan = Self {
            binding, revision, artist: None, offers: Vec::new(), listed: Vec::new(), added_artist: None,
            placements: Vec::new(), prepared_batch: None,
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

const PARKED: i64 = -1;
const QUEUED: i64 = 0;
const STARTED: i64 = 1;
const MAY_HAVE_CHANGED: i64 = 2;
const FINISHED: i64 = 3;

#[derive(Clone, Serialize, Deserialize)]
enum JobResult {
    Done(Completed),
    Failed(Refusal),
}

#[derive(Serialize, Deserialize)]
struct FinishedJob {
    finished_ms: u64,
    result: JobResult,
}

fn read_job_result(text: &str) -> Result<JobResult, Refusal> {
    match serde_json::from_str::<FinishedJob>(text) {
        Ok(finished) => Ok(finished.result),
        Err(_) => decode(text.as_bytes()),
    }
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
    admin_page: bool,
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
    batch: bool,
    rooms: &Mutex<Rooms>,
) -> Result<JobId, Refusal> {
    let plan = encode(plan)?;
    let restore = sql(actor.restore)?;
    let account = sql(actor.account.get())?;
    let session = sql(actor.session.get())?;
    let occupied: bool = keeper.data().connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM requests_jobs WHERE restore = ?1 AND account = ?2 AND stage < ?3
                       AND (stage <> ?4 OR ?5))",
        params![restore, account, FINISHED, PARKED, batch], |row| row.get(0),
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
            if !rooms.working_jobs.contains(&job.map_err(sql_error)?) {
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

fn after_job(db: &Connection, actor: Actor, job: JobId, rooms: &Mutex<Rooms>) -> Result<Answer, Refusal> {
    let row = row_id(job)?;
    let (stage, result): (i64, Option<String>) = db.query_row(
        "SELECT stage, result FROM requests_jobs WHERE id = ?1 AND restore = ?2 AND account = ?3 AND session = ?4",
        params![row, sql(actor.restore)?, sql(actor.account.get())?, sql(actor.session.get())?],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional().map_err(sql_error)?.ok_or(Refusal::JobNotFound)?;
    // Once the app sees completion, its next upstream read can use this place.
    if stage != FINISHED || lock(rooms).jobs.contains_key(&row) {
        return Ok(Answer::Working);
    }
    result.as_deref().ok_or(Refusal::NotDealtWith).and_then(read_job_result).map(JobResult::answer)
}

#[derive(Serialize, Deserialize, Default)]
struct SavedLine {
    #[serde(default)]
    progress: Option<BatchLineProgress>,
    #[serde(default)]
    offer: Option<AlbumView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    confirmed_placement: Option<bool>,
    #[serde(default)]
    resolution: Option<Resolution>,
    #[serde(default)]
    awaiting_ms: Option<u64>,
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
            stored.insert(index, (progress, line.confirmed_placement));
        }
    }
    let saved = if let Some(payload) = plan.prepared_batch {
        let batch = loaded_bulk(&payload, ReadOnlyBatch)?;
        let indexed = batch.indexed_lines().ok_or(Refusal::NotDealtWith)?;
        let mut rows = Vec::with_capacity(indexed.len());
        for line in &indexed {
            let mut projected = BatchLineView::from_indexed(line).map_err(|_| Refusal::NotDealtWith)?;
            if let Some((progress, placed)) = stored.remove(&projected.index) {
                if placed.is_some() || matches!(progress, BatchLineProgress::Failed { .. }) {
                    projected.progress = progress;
                    projected.placed = placed.unwrap_or_default();
                }
            }
            rows.push(projected);
        }
        rows
    } else {
        (0..input.len() as u16).map(|index| BatchLineView {
            index,
            placed: false,
            progress: stored.remove(&index).map(|(progress, _)| progress).unwrap_or(BatchLineProgress::Waiting),
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
    keeper.write_data(now, |tx| keep_line(tx, row, index, &text).map(|count| ((), count != 0)))
        .map_err(|_| Refusal::NotDealtWith)
}

fn keep_line(db: &Connection, row: i64, index: u16, text: &str) -> rusqlite::Result<usize> {
    db.execute(
        "INSERT INTO requests_lines (job, line, progress) VALUES (?1, ?2, ?3)
         ON CONFLICT(job, line) DO UPDATE SET progress = excluded.progress",
        params![row, index, text],
    )
}

fn submitting_session_kind(
    keeper: &Keeper, actor: &mut Account, session: i64, now: u64,
) -> Result<Option<SessionKind>, Refusal> {
    let session = SessionId::new(unsigned(session)?).ok_or(Refusal::NotDealtWith)?;
    let live = rows::session_by_id(keeper.data().connection(), session).map_err(sql_error)?;
    Ok(live.filter(|(number, ..)| *number == actor.state().id.get())
        .and_then(|(_, device, hash, _)| {
            events::inspect_session(keeper, actor, device, &Fingerprint::from_bytes(hash), now).ok()
        }))
}

fn close_parked_lines(keeper: &mut Keeper, now: u64) -> Result<(), Refusal> {
    let restore = sql(keeper.restore())?;
    let effective = keeper.clock(now).effective();
    let cutoff = sql(effective.saturating_sub(wire::RESOLUTION_WAIT_SECONDS * 1000))?;
    let parked: Vec<(i64, i64, i64)> = {
        let mut read = keeper.data().connection().prepare(
            "SELECT id, account, session FROM requests_jobs WHERE restore = ?1 AND stage = ?2",
        ).map_err(sql_error)?;
        let rows = read.query_map(params![restore, PARKED], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .map_err(sql_error)?;
        rows.collect::<Result<_, _>>().map_err(sql_error)?
    };
    let mut expired = Vec::new();
    for (row, account, session) in parked {
        let account = unsigned(account)?;
        let session_live = if rows::account_exists(keeper.data().connection(), account).map_err(sql_error)? {
            let mut actor = keeper.load(account).map_err(|_| Refusal::NotDealtWith)?;
            submitting_session_kind(keeper, &mut actor, session, now)?.is_some()
        } else {
            false
        };
        let mut read = keeper.data().connection().prepare(
            "SELECT line, progress FROM requests_lines
             WHERE job = ?1 AND json_extract(progress, '$.progress.state') = 'needs_resolution'
               AND json_type(progress, '$.resolution') = 'null'
               AND (NOT ?2 OR COALESCE(json_extract(progress, '$.awaiting_ms'), 0) <= ?3)",
        ).map_err(sql_error)?;
        let lines = read.query_map(params![row, session_live, cutoff], |record| {
            Ok((record.get::<_, u16>(0)?, record.get::<_, String>(1)?))
        }).map_err(sql_error)?;
        for line in lines {
            let (index, text) = line.map_err(sql_error)?;
            let mut saved: SavedLine = decode(text.as_bytes())?;
            let live = session_live && saved.awaiting_ms.is_some();
            let progress = wire::batch_resolution_progress(saved.awaiting_ms.unwrap_or(0), effective, live);
            if progress == BatchLineProgress::NotFound {
                saved.progress = Some(progress);
                let contents = serde_json::to_string(&saved).map_err(|_| Refusal::NotDealtWith)?;
                expired.push((row, index, contents));
            }
        }
    }
    if !expired.is_empty() {
        keeper.write_data(now, |tx| {
            for (row, index, text) in &expired {
                keep_line(tx, *row, *index, text)?;
            }
            Ok(((), true))
        }).map_err(|_| Refusal::NotDealtWith)?;
    }
    Ok(())
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
            line.awaiting_ms.map_or(wire::BatchResolutionState::Closed, |since_ms| {
                wire::BatchResolutionState::Awaiting { since_ms }
            }),
        None => wire::BatchResolutionState::Closed,
    };
    if wire::check_batch_resolution(state, release.as_ref(), actor.now, true)? == wire::BatchResolutionWrite::Save {
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
        "SELECT metadata FROM requests_owners WHERE binding = ?1 AND account = ?2 AND length(metadata) > 0
         ORDER BY asked_ms DESC, release LIMIT ?3",
    ).map_err(sql_error)?;
    let rows = query.query_map(params![binding.as_slice(), sql(account.get())?, wire::LIST_MAX_ITEMS as i64],
        |row| row.get::<_, Vec<u8>>(0)).map_err(sql_error)?;
    rows.map(|row| decode(&row.map_err(sql_error)?).map(clean_asked)).collect()
}

fn attributed_requests(db: &Connection, binding: &[u8; 32]) -> Result<Vec<AttributionRow>, Refusal> {
    let mut query = db.prepare(
        "SELECT owners.account, accounts.shown, owners.metadata FROM requests_owners AS owners
         JOIN accounts ON accounts.number = owners.account
         WHERE owners.binding = ?1 AND length(owners.metadata) > 0
         ORDER BY owners.asked_ms DESC, owners.account, owners.release LIMIT ?2",
    ).map_err(sql_error)?;
    let rows = query.query_map(params![binding.as_slice(), wire::LIST_MAX_ITEMS as i64], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, Vec<u8>>(2)?))
    }).map_err(sql_error)?;
    let mut found = Vec::new();
    for row in rows {
        let (id, name, payload) = row.map_err(sql_error)?;
        let account = AccountId::new(unsigned(id)?).ok_or(Refusal::NotDealtWith)?;
        let name = account::Name::new(&name).ok_or(Refusal::NotDealtWith)?;
        found.push(AttributionRow { account, name, asked: clean_asked(decode(&payload)?), arrived: None });
    }
    Ok(lidarr_items(found))
}

fn owners(
    db: &Connection,
    binding: &[u8; 32],
    account: AccountId,
    releases: &[&str],
) -> Result<HashMap<String, Vec<(AccountId, bool)>>, Refusal> {
    if releases.is_empty() {
        return Ok(HashMap::new());
    }
    let releases = serde_json::to_string(releases).map_err(|_| Refusal::NotDealtWith)?;
    let mut select = db.prepare(
        "SELECT DISTINCT value,
            (SELECT placed FROM requests_owners WHERE binding = ?2 AND release = value AND account = ?3),
            (SELECT account FROM requests_owners WHERE binding = ?2 AND release = value AND account <> ?3 LIMIT 1)
         FROM json_each(?1)",
    ).map_err(sql_error)?;
    let rows = select.query_map(params![releases, binding.as_slice(), sql(account.get())?], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<bool>>(1)?, row.get::<_, Option<i64>>(2)?))
    }).map_err(sql_error)?;
    let mut found = HashMap::new();
    for row in rows {
        let (release, mine, other) = row.map_err(sql_error)?;
        // The shared rule needs our placement and whether any other account asked.
        let other = other.and_then(|number| u64::try_from(number).ok().and_then(AccountId::new));
        let requesters: Vec<_> = mine.map(|placed| (account, placed)).into_iter()
            .chain(other.map(|id| (id, false))).collect();
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
    let mine = |known: &HashMap<String, Vec<(AccountId, bool)>>, release: &MusicBrainzId| {
        known.get(release.as_str()).is_some_and(|requesters| requesters.iter().any(|&(id, _)| id == actor.account))
    };
    match found {
        Answer::Pending { albums, .. } => {
            let known = owners(db, binding, actor.account, &albums.iter().map(|album| album.album.as_str()).collect::<Vec<_>>())?;
            albums.retain_mut(|album| {
                let requesters = known.get(album.album.as_str()).map(Vec::as_slice).unwrap_or_default();
                album.removable = wire::may_cancel(wire::RequestsAccess::On, actor.account, actor.admin_page, requesters).is_ok();
                actor.admin_page || requesters.iter().any(|&(id, _)| id == actor.account)
            });
        }
        Answer::FoundReleases { albums, .. } | Answer::FoundAny { albums, .. } if !actor.admin_page => {
            let known = owners(db, binding, actor.account, &albums.iter().map(|album| album.album.0.as_str()).collect::<Vec<_>>())?;
            for album in albums {
                album.requested &= mine(&known, &album.album.0);
            }
        }
        Answer::AlbumsOf { albums, .. } | Answer::ReleasesOf { releases: albums, .. } if !actor.admin_page => {
            let known = owners(db, binding, actor.account, &albums.iter().map(|release| release.album.album.0.as_str()).collect::<Vec<_>>())?;
            for release in albums {
                release.album.requested &= mine(&known, &release.album.album.0);
            }
        }
        Answer::Listed { releases, .. } if !actor.admin_page => {
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
    check_stamp: [u8; 32],
}

enum Dispatch {
    Done(Answer),
    Read { ask: Read, actor: Actor, credential: Credential },
}

async fn call(
    State(service): State<Arc<Requests>>,
    uri: Uri,
    caller: Option<Extension<Caller>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let page = uri.path() == wire::ADMIN_REQUESTS;
    if (page || crate::admin::is_browser(&headers))
        && !crate::admin::browser_origin_allowed(&headers, service.origin.as_deref())
    {
        return events::respond(crate::accounts::session::refused(account::Refusal::Malformed));
    }
    let message = match wire::read_call(&body) {
        Ok(message) => message,
        Err(_) => return events::respond(crate::accounts::session::refused(account::Refusal::Malformed)),
    };
    let head = message.head;
    let ingress = match service.admitted(&headers, caller, &head) {
        Ok(ingress) => ingress,
        Err(why) => return events::respond(crate::accounts::session::refused(why)),
    };
    let Some(_place) = Place::ingress(service.rooms.clone(), ingress.caller.account.get()) else {
        return events::respond(crate::accounts::session::refused(events::busy()));
    };
    let for_finish = RequestHead { speaks: head.speaks, restore: head.restore };
    let state = service.clone();
    let result = service.shown(ingress.clone(), head, move |keeper, actor| {
        if page && (!actor.admin_page || !matches!(message.call,
            Call::Pending { .. } | Call::After { .. } | Call::Begin { change: Change::Cancel { .. }, .. }))
        {
            return Err(events::denied());
        }
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
                let lidarr = endpoint.lidarr(runtime, credential.key.expose(), Some(Instant::now() + for_read.timeout()));
                for_read.ask(&lidarr).map_err(|error| wire::refusal_for(&error, None))
            }).await.unwrap_or(Err(Refusal::NotDealtWith));
            let state = service.clone();
            match service.shown(ingress, for_finish, move |keeper, actor| {
                Ok((|| {
                    access(keeper, actor.account)?.check()?;
                    let db = keeper.data().connection();
                    let mut found = found?;
                    owner_view(db, &binding, actor, &mut found)?;
                    let mut books = lock(&state.books);
                    let answer = books.first(actor.account, actor.session, &ask, found)?;
                    books.remember(actor.account, actor.session, seconds(actor.now), &ask, &answer);
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
                    let known = if actor.admin_page {
                        HashMap::new()
                    } else {
                        owners(db, &binding, actor.account, &[release.as_str()])?
                    };
                    wire::may_cancel(wire::RequestsAccess::On, actor.account, actor.admin_page,
                        known.get(release.as_str()).map(Vec::as_slice).unwrap_or_default())?;
                }
                let work = Plan::resolve(&change, &lock(&self.books), actor, binding, credential.revision)?;
                let job = enqueue(keeper, actor, request_id, &change_bytes, &work, matches!(&change, Change::Batch { .. }), &self.rooms)?;
                self.wake.notify_one();
                Answer::Begun { job }
            }
            Call::After { job } => after_job(db, actor, job, &self.rooms)?,
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
                    lock(&self.books).first(actor.account, actor.session, &Read::AskedHistory, found)?
                }
            }
            Call::BatchLines { job, after } => {
                let key = Read::Batch(job);
                if let Some(after) = after {
                    lock(&self.books).peek(actor.session, &key, after)?
                } else {
                    let found = batch_lines(db, actor, job)?;
                    lock(&self.books).first(actor.account, actor.session, &key, found)?
                }
            }
            call => {
                let (ask, cursor) = Read::from_call(call).ok_or(Refusal::Malformed)?;
                if let Some(cursor) = cursor {
                    let mut books = lock(&self.books);
                    let answer = books.peek(actor.session, &ask, cursor)?;
                    books.remember(actor.account, actor.session, seconds(actor.now), &ask, &answer);
                    return Ok(Dispatch::Done(answer));
                }
                return Ok(Dispatch::Read { ask, actor, credential });
            }
        };
        Ok(Dispatch::Done(answer))
    }

    pub(crate) async fn start(self: &Arc<Self>) {
        let address = self.accounts.lidarr.clone();
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
                    let Ok(Ok(Some((job, reservation, credential)))) = selected else { break };
                    let state = state.clone();
                    let runtime = Handle::current();
                    tokio::task::spawn_blocking(move || {
                        state.run(job, credential, runtime);
                        drop(reservation);
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
        let Ok(turn) = self.mutation.clone().try_lock_owned() else { return };
        let Some(place) = Place::upstream(self.rooms.clone(), 0) else { return };
        let runtime = Handle::current();
        let journal = Journal {
            service: self.clone(), runtime: runtime.clone(), binding: identity, restore, row: 0, artist: None,
        };
        let _ = tokio::task::spawn_blocking(move || {
            let _place = place;
            let _turn = turn;
            let lidarr = endpoint.lidarr(runtime, key.key.expose(), None);
            journal.restore_pending(&lidarr)
        }).await;
    }

    fn claim(&self, keeper: &mut Keeper, now: u64) -> Result<Option<(Job, JobReservation, Credential)>, Refusal> {
        let Some(endpoint) = &self.endpoint else { return Ok(None) };
        close_parked_lines(keeper, now)?;
        let db = keeper.data().connection();
        let selected = {
            let mut query = db.prepare(
                "SELECT id, account, session, change, work, stage FROM requests_jobs
                 WHERE restore = ?1 AND stage < ?2 AND (stage <> ?3 OR NOT EXISTS (
                    SELECT 1 FROM requests_lines
                    WHERE job = requests_jobs.id
                     AND json_extract(progress, '$.progress.state') = 'needs_resolution'
                     AND json_type(progress, '$.resolution') = 'null'
                 )) ORDER BY id LIMIT ?4",
            ).map_err(sql_error)?;
            let mut rows = query.query(params![sql(keeper.restore())?, FINISHED, PARKED, (wire::WAITING_MAX + wire::RUNNING_MAX) as i64])
                .map_err(sql_error)?;
            loop {
                let Some(row) = rows.next().map_err(sql_error)? else { return Ok(None) };
                let account = unsigned(row.get::<_, i64>(1).map_err(sql_error)?)?;
                let id = row.get::<_, i64>(0).map_err(sql_error)?;
                let change: Change = decode(&row.get::<_, Vec<u8>>(3).map_err(sql_error)?)?;
                let stage: i64 = row.get(5).map_err(sql_error)?;
                let reservation = match JobReservation::take(self.rooms.clone(), id, account, matches!(&change, Change::Batch { .. })) {
                    Ok(reservation) => reservation,
                    Err(ClaimBlocked::BatchesFull) if stage == QUEUED => break Err(id),
                    Err(_) => continue,
                };
                break Ok((id, account, row.get::<_, i64>(2).map_err(sql_error)?, change,
                    row.get::<_, Vec<u8>>(4).map_err(sql_error)?, stage, reservation));
            }
        };
        let (row, account, session, change, plan, stage, reservation) = match selected {
            Ok(job) => job,
            Err(row) => {
                finish_job(keeper, now, row, &JobResult::Failed(Refusal::Busy), &[], None)?;
                self.wake.notify_one();
                return Ok(None);
            }
        };
        let plan: Plan = decode(&plan)?;
        let restore = keeper.restore();
        if !rows::account_exists(keeper.data().connection(), account).map_err(sql_error)? {
            finish_job(keeper, now, row, &JobResult::Failed(Refusal::NotSignedIn), &[], None)?;
            self.wake.notify_one();
            return Ok(None);
        }
        let mut actor = keeper.load(account).map_err(|_| Refusal::NotDealtWith)?;
        let account_id = actor.state().id;
        let allowed = (|| {
            access(keeper, account_id)?.check()?;
            if actor.state().status != Status::On {
                return Err(Refusal::NotSignedIn);
            }
            let kind = if matches!(&change, Change::Batch { .. }) {
                None
            } else {
                Some(submitting_session_kind(keeper, &mut actor, session, now)?.ok_or(Refusal::NotSignedIn)?)
            };
            let key = credential(keeper, endpoint)?.ok_or(Refusal::LidarrNotSetUp)?;
            if plan.binding != binding(endpoint) || plan.revision != key.revision {
                return Err(Refusal::LidarrNotSetUp);
            }
            Ok((key, kind == Some(SessionKind::AdminPage) && actor.state().admin))
        })();
        let (key, admin_page) = match allowed {
            Ok(allowed) => allowed,
            Err(reason) => {
                finish_job(keeper, now, row, &JobResult::Failed(reason), &[], None)?;
                self.wake.notify_one();
                return Ok(None);
            }
        };
        if matches!(stage, QUEUED | PARKED) {
            keeper.write_data(now, |tx| {
                tx.execute("UPDATE requests_jobs SET stage = ?1 WHERE id = ?2 AND stage = ?3", params![STARTED, row, stage])
                    .map(|count| ((), count != 0))
            }).map_err(|_| Refusal::NotDealtWith)?;
        }
        Ok(Some((Job {
            row,
            account: account_id,
            restore,
            admin_page,
            change,
            plan,
            recovering: stage == MAY_HAVE_CHANGED,
        }, reservation, key)))
    }

    async fn upstream_for_job(&self, job: &Job) -> Place {
        let available = lock(&self.rooms).available.clone();
        loop {
            let waiting = available.notified();
            tokio::pin!(waiting);
            waiting.as_mut().enable();
            if let Some(place) = Place::take_upstream(self.rooms.clone(), job.account.get(), Some(job.row)) {
                return place;
            }
            waiting.await;
        }
    }

    fn mutation_turn(&self, job: &Job, runtime: &Handle) -> MutationTurn {
        runtime.block_on(async {
            let waiting = self.mutation.clone().lock_owned();
            #[cfg(test)]
            let waiting = tests::observe_mutation_wait(waiting, self.accounts.backend, job.row);
            let turn = waiting.await;
            let upstream = self.upstream_for_job(job).await;
            MutationTurn { _upstream: upstream, _turn: turn }
        })
    }

    fn run(self: &Arc<Self>, mut job: Job, credential: Credential, runtime: Handle) {
        let Some(endpoint) = &self.endpoint else { return };
        let journal = Journal {
            service: self.clone(), runtime: runtime.clone(), binding: job.plan.binding,
            restore: job.restore, row: job.row,
            artist: job.plan.offers.first().and_then(|offer| offer.artist_id.clone()),
        };
        let ready = Arc::new(AtomicBool::new(job.recovering));
        let denied = Arc::new(Mutex::new(None));
        let batch = matches!(&job.change, Change::Batch { .. });
        let account = job.account;
        let pre_write = {
            let journal = journal.clone();
            let ready = ready.clone();
            let denied = denied.clone();
            Arc::new(move || {
                if batch && lock(&denied).is_some() {
                    return false;
                }
                let marked = ready.load(Ordering::Relaxed);
                if marked && !batch {
                    return true;
                }
                let accepted = journal.ask(move |keeper, now, row| {
                    if batch {
                        access(keeper, account)?.check()?;
                    }
                    if marked {
                        return Ok(true);
                    }
                    keeper.write_data(now, |tx| tx.execute(
                        "UPDATE requests_jobs SET stage = ?1 WHERE id = ?2 AND stage = ?3",
                        params![MAY_HAVE_CHANGED, row, STARTED],
                    ).map(|count| (count != 0, count != 0))).map_err(|_| Refusal::NotDealtWith)
                });
                match accepted {
                    Ok(true) => {
                        ready.store(true, Ordering::Relaxed);
                        true
                    }
                    Ok(false) => false,
                    Err(reason) => {
                        if batch {
                            *lock(&denied) = Some(reason);
                        }
                        false
                    }
                }
            })
        };
        let lidarr = Lidarr::new(endpoint.url.as_str(), credential.key.expose(), None, Arc::new(Line {
            endpoint: endpoint.clone(), runtime: runtime.clone(), within: UPSTREAM_TIMEOUT,
            deadline: None, before_write: Some(pre_write),
        }));
        if let Change::Batch { singles, lines } = &job.change {
            if let Err(reason) = self.run_batch(&job, &journal, &lidarr, credential.key.expose(), *singles, lines, &denied) {
                let _ = journal.ask(move |keeper, now, row| {
                    finish_job(keeper, now, row, &JobResult::Failed(reason), &[], None)
                });
            }
            return;
        }
        let _turn = self.mutation_turn(&job, &runtime);
        let cleaned = journal.restore_pending(&endpoint.lidarr(runtime, credential.key.expose(), None));
        let result = cleaned.and_then(|()| {
            if job.recovering {
                return job.recover(&lidarr);
            }
            let result = job.run(&journal, &lidarr);
            if result.is_err() && ready.load(Ordering::Relaxed) {
                // The write may have reached Lidarr before its response was lost.
                // Recover from the album-level evidence just kept by the writer.
                if let Ok(plan) = journal.read_plan() {
                    job.plan = plan;
                    if let Ok(confirmed) = job.recover(&lidarr) {
                        return Ok(confirmed);
                    }
                }
            }
            result
        });
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
    adds: &[RecordedAsk],
    ownership: Option<([u8; 32], AccountId, Option<MusicBrainzId>)>,
) -> Result<(), Refusal> {
    let at = keeper.clock(now).effective();
    let text = serde_json::to_string(&FinishedJob { finished_ms: at, result: result.clone() })
        .map_err(|_| Refusal::NotDealtWith)?;
    let restore = sql(keeper.restore())?;
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
        // Drop previous terminal job receipts and their lines. Ownership
        // lives independently. Bound each cleanup to one small transaction.
        let expired = tx.execute(
            "DELETE FROM requests_jobs WHERE id IN (
                SELECT id FROM requests_jobs WHERE restore <= ?1 AND stage = ?2
                  AND json_extract(result, '$.finished_ms') < ?3
                ORDER BY id LIMIT 32
            )",
            params![restore, FINISHED, events::sql(at.saturating_sub(24 * 60 * 60 * 1000))?],
        )?;
        Ok(((), changed || expired != 0))
    }).map_err(|_| Refusal::NotDealtWith)
}

#[derive(Clone)]
struct RecordedAsk {
    asked: AskedRelease,
    placed: bool,
}

fn clean_asked(mut asked: AskedRelease) -> AskedRelease {
    asked.title = clean_display_text(&asked.title);
    asked.artist = clean_display_text(&asked.artist);
    asked
}

fn keep_owner(db: &Connection, binding: &[u8; 32], account: AccountId, ask: &RecordedAsk, now: u64) -> rusqlite::Result<()> {
    let mut asked = clean_asked(ask.asked.clone());
    asked.asked_ms = now;
    let metadata = serde_json::to_vec(&asked)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    db.execute(
        "INSERT INTO requests_owners (binding, account, release, asked_ms, metadata, placed)
         SELECT ?1, number, ?2, ?3, ?4, ?6 FROM accounts WHERE number = ?5
         ON CONFLICT(binding, account, release) DO UPDATE SET
            placed = MAX(requests_owners.placed, excluded.placed),
            metadata = CASE WHEN length(requests_owners.metadata) = 0 THEN excluded.metadata ELSE requests_owners.metadata END,
            asked_ms = CASE WHEN length(requests_owners.metadata) = 0 THEN excluded.asked_ms ELSE requests_owners.asked_ms END
         WHERE length(requests_owners.metadata) = 0 OR excluded.placed > requests_owners.placed",
        params![binding.as_slice(), asked.release.as_str(), events::sql(now)?, metadata,
            events::sql(account.get())?, ask.placed],
    ).map(drop)
}

fn record_asks(asked: Vec<AskedRelease>, placements: &[AlbumPlacement]) -> Vec<RecordedAsk> {
    asked.into_iter().map(|asked| {
        let placed = placements.iter().any(|known| {
            known.release == asked.release.as_str() && known.artist == asked.artist_id.as_str() && known.newly_placed()
        });
        RecordedAsk { asked, placed }
    }).collect()
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

    fn read_plan(&self) -> Result<Plan, Refusal> {
        self.ask(|keeper, _, row| {
            read_work(keeper.data().connection(), row)
        })
    }

    fn keep_placements(&self, observed: &[AlbumPlacement]) -> Result<(), String> {
        let observed = observed.to_vec();
        self.ask(move |keeper, now, row| {
            update_work(keeper, now, row, |plan| {
                if plan.placements.is_empty() {
                    plan.placements = observed;
                }
            })
        }).map_err(|_| "Couldn't keep the selected releases before placing them.".to_owned())
    }

    fn restore_pending(&self, lidarr: &Lidarr) -> Result<(), Refusal> {
        let binding = self.binding;
        let active: Vec<_> = lock(&self.service.rooms).recovery_jobs.keys()
            .filter_map(|&(endpoint, artist)| (endpoint == binding).then_some(artist)).collect();
        let active = serde_json::to_string(&active).map_err(|_| Refusal::NotDealtWith)?;
        let pending = self.ask(move |keeper, _, _| {
            let mut query = keeper.data().connection().prepare(
                "SELECT artist, artist_key, setting FROM requests_way_back WHERE binding = ?1
                 AND artist NOT IN (SELECT value FROM json_each(?2)) ORDER BY id LIMIT 20",
            ).map_err(sql_error)?;
            let rows = query.query_map(params![binding.as_slice(), active], |row| Ok((
                row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
            ))).map_err(sql_error)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(sql_error)
        })?;
        for (artist, artist_key, setting) in pending {
            self.restore_artist(lidarr, artist, &artist_key, setting)?;
        }
        Ok(())
    }

    fn restore_artist(&self, lidarr: &Lidarr, artist: i64, artist_key: &str, setting: String) -> Result<(), Refusal> {
        let known = lidarr_result(lidarr.known_artist(artist_key), None)?;
        if known.is_some_and(|(number, _)| number == artist) {
            lidarr.set_new_releases(artist, &setting).map_err(|_| Refusal::LidarrCouldNotRestore)?;
        } else {
            eprintln!("An obsolete Lidarr setting recovery entry was discarded.");
        }
        self.cross_off(artist, setting)
    }

    fn restore_saved_artist(&self, lidarr: &Lidarr, artist: i64, setting: &str) -> Result<(), Refusal> {
        let binding = self.binding;
        if lock(&self.service.rooms).recovery_jobs.get(&(binding, artist)).is_some_and(|job| *job != self.row) {
            return Err(Refusal::Busy);
        }
        let setting = setting.to_owned();
        let lookup = setting.clone();
        let artist_key: Option<String> = self.ask(move |keeper, _, _| {
            keeper.data().connection().query_row(
                "SELECT artist_key FROM requests_way_back WHERE binding = ?1 AND artist = ?2 AND setting = ?3",
                params![binding.as_slice(), artist, lookup], |record| record.get(0),
            ).optional().map_err(sql_error)
        })?;
        if let Some(artist_key) = artist_key {
            self.restore_artist(lidarr, artist, &artist_key, setting)?;
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
        })?;
        let mut rooms = lock(&self.service.rooms);
        if rooms.recovery_jobs.get(&(binding, artist)) == Some(&self.row) {
            rooms.recovery_jobs.remove(&(binding, artist));
        }
        Ok(())
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

    fn before_request_album(&self, placement: &AlbumPlacement) -> Result<(), String> {
        self.keep_placements(std::slice::from_ref(placement))
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

type Changed = (Completed, Vec<RecordedAsk>, Option<MusicBrainzId>);

fn lidarr_result<T>(value: Result<T, String>, wanted: Option<&Offer>) -> Result<T, Refusal> {
    value.map_err(|why| wire::refusal_for(&why, wanted))
}

impl Job {
    fn run(&self, journal: &Journal, lidarr: &Lidarr) -> Result<Changed, Refusal> {
        let done = |result| Ok((result, Vec::new(), None));
        match &self.change {
            Change::Request { .. } => {
                let view = self.plan.offers.first().ok_or(Refusal::SearchAgain)?;
                let asked = asked_release(view)?;
                let wanted: Offer = view.clone().into();
                // A different request number may name an album already asked for.
                // Reuse that observation only when core confirms the same live release.
                let binding = self.plan.binding;
                let release = wanted.album.clone();
                let known = journal.ask(move |keeper, _, _| {
                    keeper.data().connection().query_row(
                        "SELECT EXISTS(SELECT 1 FROM requests_owners WHERE binding = ?1 AND release = ?2)",
                        params![binding.as_slice(), release], |row| row.get::<_, bool>(0),
                    ).map_err(sql_error)
                })?;
                let placed = if known && matches!(confirmed_placement(lidarr, &wanted, None)?, PlacementRecovery::Listed { .. }) {
                    false
                } else {
                    lidarr_result(lidarr.request_placement(&wanted, journal), Some(&wanted))?
                };
                Ok((Completed::Requested { placed }, vec![RecordedAsk { asked, placed }], None))
            }
            Change::AddArtist { artist } => {
                let name = &self.plan.artist.as_ref().ok_or(Refusal::SearchAgain)?.name;
                let added = lidarr_result(lidarr.add_artist_exact(artist.as_str(), name, journal), None)?;
                added_artist(&added)
            }
            Change::AddWanting { artist, offers: selected } => {
                wire::check_add_wanting(artist, selected, &self.plan.offers)?;
                let offers: Vec<Offer> = self.plan.offers.iter().cloned().map(Offer::from).collect();
                let asked = self.plan.offers.iter().map(asked_release).collect::<Result<_, _>>()?;
                if lidarr_result(lidarr.known_artist(artist.as_str()), None)?.is_some() {
                    return Err(Refusal::SearchAgain);
                }
                let placements: Vec<_> = offers.iter().map(|offer| AlbumPlacement {
                    artist: offer.artist_id.clone(), release: offer.album.clone(),
                    artist_number: None, album_number: None, previously_wanted: false, previously_held: false,
                }).collect();
                let mut setup = lidarr_result(lidarr.setup(), None)?;
                let artist = lidarr_result(lidarr.add_wanting_before(&mut setup, &offers, || journal.keep_placements(&placements)), None)?;
                let artist = LidarrNumber::new(artist).ok_or(Refusal::LidarrUnrecognized)?;
                Ok((Completed::AddedWanting { artist }, record_asks(asked, &placements), None))
            }
            Change::Want { artist, albums } => {
                let current = artist_listed(lidarr, artist)?;
                for album in albums {
                    album.check_current(&self.plan.listed)?;
                }
                let placements = wire::placements_for_want(artist, albums, Some(artist.id.into()), &current)?;
                let asked = listed_metadata(lidarr, artist, albums)?;
                let numbers: Vec<_> = placements.iter().filter_map(|placement| placement.album_number).collect();
                lidarr_result(lidarr.want_before(artist.id.into(), &numbers, || journal.keep_placements(&placements)), None)?;
                Ok((Completed::Wanted, record_asks(asked, &placements), None))
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
                check_pending_target(lidarr, *album, release)?;
                let binding = self.plan.binding;
                let account = self.account;
                let admin_page = self.admin_page;
                let target = release.clone();
                journal.ask(move |keeper, _, _| {
                    let current = keeper.load(account.get()).map_err(|_| Refusal::NotAllowed)?;
                    let db = keeper.data().connection();
                    let access = access(keeper, account)?;
                    let owners = owners(db, &binding, account, &[target.as_str()])?;
                    wire::may_cancel(access, account, admin_page && current.state().admin, owners.get(target.as_str()).map(Vec::as_slice).unwrap_or_default())
                })?;
                lidarr_result(lidarr.cancel((*album).into()), None)?;
                Ok((Completed::Cancelled, Vec::new(), Some(release.clone())))
            }
            Change::Batch { .. } => Err(Refusal::NotDealtWith),
        }
    }

    /// A restart can run a job which made no writes, or verify a remote effect
    /// that may already have happened. An uncertain mutation must never replay.
    fn recover(&self, lidarr: &Lidarr) -> Result<Changed, Refusal> {
        match &self.change {
            Change::Request { .. } => {
                let view = self.plan.offers.first().ok_or(Refusal::SearchAgain)?;
                let wanted = view.clone().into();
                match confirmed_placement(lidarr, &wanted, self.plan.placements.first())? {
                    PlacementRecovery::Listed { placed } => {
                        let asked = asked_release(view)?;
                        Ok((Completed::Requested { placed }, vec![RecordedAsk { asked, placed }], None))
                    }
                    PlacementRecovery::Unconfirmed => Err(Refusal::NotDealtWith),
                }
            }
            Change::Want { artist, albums } => {
                let current = artist_listed(lidarr, artist)?;
                for album in albums {
                    album.check_current(&self.plan.listed)?;
                    already_marked(album, &current)?;
                }
                let asked = listed_metadata(lidarr, artist, albums)?;
                let records = recovered_asks(asked, artist, &current, &self.plan.placements)?;
                Ok((Completed::Wanted, records, None))
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
                let id = MusicBrainzId::new(&wanted.artist_id).ok_or(Refusal::LidarrUnrecognized)?;
                let records = recovered_asks(asked, &wire::ArtistTarget { id: artist, artist: id }, &current, &self.plan.placements)?;
                Ok((Completed::AddedWanting { artist }, records, None))
            }
            Change::Cancel { album, release } => {
                let pending = lidarr_result(lidarr.pending(), None)?;
                if pending.iter().any(|row| row.id == album.get() && row.album == release.as_str()) {
                    Err(Refusal::NotDealtWith)
                } else {
                    Ok((Completed::Cancelled, Vec::new(), Some(release.clone())))
                }
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
    let asked = views.iter().map(|view| Ok(RecordedAsk { asked: asked_release(view)?, placed: true }))
        .collect::<Result<_, _>>()?;
    Ok((Completed::ArtistAdded { count }, asked, None))
}

fn recovered_asks(
    asks: Vec<AskedRelease>, artist: &wire::ArtistTarget, current: &[lidarr::Listed], placements: &[AlbumPlacement],
) -> Result<Vec<RecordedAsk>, Refusal> {
    asks.into_iter().map(|asked| {
        let offer = Offer { album: asked.release.as_str().into(), artist_id: asked.artist_id.as_str().into(), ..Default::default() };
        let placement = placements.iter().find(|known| known.release == offer.album && known.artist == offer.artist_id);
        match wire::batch_effect_confirmed(&offer, artist, Some(artist.id.into()), current, placement) {
            PlacementRecovery::Listed { placed } => Ok(RecordedAsk { asked, placed }),
            PlacementRecovery::Unconfirmed => Err(Refusal::NotDealtWith),
        }
    }).collect()
}

fn check_pending_target(lidarr: &Lidarr, album: LidarrNumber, release: &MusicBrainzId) -> Result<(), Refusal> {
    let rows: Vec<_> = lidarr_result(lidarr.pending(), None)?.into_iter()
        .filter(|row| row.id == album.get())
        .map(|row| lidarr::Listed { id: row.id, album: row.album, wanted: true, have: false })
        .collect();
    wire::AlbumTarget { id: album, release: release.clone() }.check_current(&rows)
}

impl Requests {
    fn run_batch(
        &self, job: &Job, journal: &Journal, lidarr: &Lidarr, lidarr_key: &str, singles: bool, lines: &[BatchLine], denied: &Mutex<Option<Refusal>>,
    ) -> Result<(), Refusal> {
        let desk = BatchDesk { journal: journal.clone(), singles };
        let batch = if let Some(payload) = &job.plan.prepared_batch {
            loaded_bulk(payload, desk)?
        } else {
            let mut wants = Vec::with_capacity(lines.len());
            let mut waiting = Vec::new();
            for (index, line) in lines.iter().enumerate() {
                let index = index as u16;
                let _place = journal.runtime.block_on(self.upstream_for_job(job));
                let deadline = Instant::now() + SEARCH_TIMEOUT;
                let endpoint = self.endpoint.as_ref().ok_or(Refusal::LidarrNotSetUp)?;
                let lookup = endpoint.lidarr(journal.runtime.clone(), lidarr_key, Some(deadline));
                match prepare_line(journal, &lookup, index, line, singles)? {
                    Some(want) => wants.push(want),
                    None => waiting.push(BatchLineView { index, placed: false, progress: BatchLineProgress::NeedsResolution }),
                }
            }
            if wire::batch_parked_on_app(&waiting) {
                return journal.ask(move |keeper, now, row| {
                    keeper.write_data(now, |tx| {
                        tx.execute("UPDATE requests_jobs SET stage = ?1 WHERE id = ?2 AND stage = ?3",
                            params![PARKED, row, STARTED]).map(|count| ((), count != 0))
                    }).map_err(|_| Refusal::NotDealtWith)
                });
            }
            let batch = bulk::Job::new(desk);
            lidarr_result(batch.start_indexed(wants), None)?;
            batch
        };

        let control = BatchControl { job, journal, lidarr, denied, checked: Default::default() };
        {
            let _turn = control.turn()?;
            let endpoint = self.endpoint.as_ref().ok_or(Refusal::LidarrNotSetUp)?;
            let recovery = endpoint.lidarr(journal.runtime.clone(), lidarr_key, None);
            journal.restore_pending(&recovery)?;
            if job.plan.prepared_batch.is_some() {
                // Startup and other workers can restore a cut job's durable settings.
                // Discard its local copy only after checking the shared journal.
                if let Ok(saved) = batch.saved_way_backs() {
                    for entry in saved {
                        if endpoint_binding(&entry.address) != journal.binding {
                            return Err(Refusal::LidarrCouldNotRestore);
                        }
                        journal.restore_saved_artist(&recovery, entry.artist, &entry.before)?;
                        batch.clear_saved_way_back(&entry)?;
                    }
                }
                let indexed = batch.indexed_lines().ok_or(Refusal::NotDealtWith)?;
                let mut reconciled = HashSet::new();
                for line in indexed.iter().filter(|line| {
                    line.before_effect && matches!(line.state, bulk::State::Found | bulk::State::Asked)
                }) {
                    if reconciled.insert(line.slot) {
                        let confirmed = control.confirm(line)?.unwrap_or(PlacementRecovery::Unconfirmed);
                        lidarr_result(batch.reconcile_indexed(line.slot, confirmed), line.offer.as_ref())?;
                    }
                }
                let indexed = batch.indexed_lines().ok_or(Refusal::NotDealtWith)?;
                control.publish(None, indexed, None)?;
            }
        }

        let upstream = BatchUpstream { lidarr, journal, batch: &batch, denied };
        let worked = batch.work_prepared_in_turns(&upstream, &|_| {}, &control);
        let refusal = (*lock(denied)).or_else(|| match worked {
            Ok(()) => None,
            Err(bulk::PreparedError::Refused(reason)) => Some(reason),
            Err(bulk::PreparedError::Lidarr(reason)) => Some(wire::refusal_for(&reason, None)),
        });
        let result = refusal.map_or(JobResult::Done(Completed::Batch), JobResult::Failed);
        // Only an unresolved effect may need a final observation. Earlier group
        // ownership was already published and may have been cancelled meanwhile.
        let _turn = control.turn()?;
        let indexed = batch.indexed_lines().ok_or(Refusal::NotDealtWith)?;
        control.publish(None, indexed, refusal)?;
        journal.ask(move |keeper, now, row| finish_job(keeper, now, row, &result, &[], None))
    }
}


struct BatchControl<'a> {
    job: &'a Job,
    journal: &'a Journal,
    lidarr: &'a Lidarr,
    denied: &'a Mutex<Option<Refusal>>,
    checked: std::cell::RefCell<HashMap<usize, Option<PlacementRecovery>>>,
}

impl BatchControl<'_> {
    fn confirm(&self, line: &bulk::IndexedLine) -> Result<Option<PlacementRecovery>, Refusal> {
        let Some(offer) = &line.offer else { return Ok(None) };
        let observed = confirmed_placement(self.lidarr, offer, line.placement.as_ref())?;
        let protected = matches!(observed, PlacementRecovery::Listed { placed: true })
            && self.protected_releases(&offer.artist_id)?.contains(&offer.album);
        Ok(Some(if protected { PlacementRecovery::Listed { placed: false } } else { observed }))
    }

    fn publish(&self, saved: Option<Vec<u8>>, lines: Vec<bulk::IndexedLine>, close: Option<Refusal>) -> Result<(), Refusal> {
        let account = self.job.account;
        let binding = self.journal.binding;
        let mut changes = Vec::new();
        for line in lines {
            let projected = BatchLineView::from_indexed(&line).map_err(|_| Refusal::NotDealtWith)?;
            let observed = match line.state {
                bulk::State::Done => Some(line.placed),
                bulk::State::Found | bulk::State::Asked | bulk::State::Failed
                    if line.placement.is_some() && (close.is_some() || line.state == bulk::State::Failed
                        || (line.state == bulk::State::Found && !line.reason.is_empty())) =>
                {
                    let mut checked = self.checked.borrow_mut();
                    match checked.entry(line.slot).or_insert_with(|| self.confirm(&line).ok().flatten()) {
                        Some(PlacementRecovery::Listed { placed }) => Some(*placed),
                        _ => None,
                    }
                }
                _ => None,
            };
            let ending = if let Some(placed) = observed {
                let Some(view) = lidarr_items::<AlbumView, _>(line.offer).pop() else { continue };
                LineEnding::Confirmed { album: batch_album(&view), ask: RecordedAsk { asked: asked_release(&view)?, placed } }
            } else if let Some(reason) = close.filter(|_| matches!(line.state, bulk::State::Found | bulk::State::Asked | bulk::State::Failed)) {
                LineEnding::Failed(reason)
            } else if matches!(projected.progress, BatchLineProgress::Asked { .. } | BatchLineProgress::Failed { .. }) {
                LineEnding::Progress(projected.progress)
            } else {
                continue;
            };
            changes.push((projected.index, ending));
        }
        self.journal.ask(move |keeper, now, row| {
            checkpoint_batch(keeper, now, row, binding, account, saved, changes)
        })
    }
}

impl PreparedBatch for BatchControl<'_> {
    fn turn(&self) -> Result<Box<dyn lidarr::Turn + '_>, Refusal> {
        Ok(Box::new(self.journal.service.mutation_turn(self.job, &self.journal.runtime)))
    }

    fn check_access(&self) -> Result<(), Refusal> {
        if let Some(reason) = *lock(self.denied) {
            return Err(reason);
        }
        let account = self.job.account;
        self.journal.ask(move |keeper, _, _| access(keeper, account)?.check())
    }

    fn checkpoint(&self, checkpoint: bulk::PreparedCheckpoint) -> Result<(), Refusal> {
        let close = *lock(self.denied);
        self.publish(Some(checkpoint.saved), checkpoint.lines, close)
    }

    fn protected_releases(&self, artist: &str) -> Result<HashSet<String>, Refusal> {
        let binding = self.journal.binding;
        let artist = artist.to_owned();
        self.journal.ask(move |keeper, _, row| {
            let db = keeper.data().connection();
            let saved_binding = serde_json::to_string(&binding).map_err(|_| Refusal::NotDealtWith)?;
            let restore = sql(keeper.restore())?;
            let mut read = db.prepare(
                "SELECT release FROM requests_owners
                 WHERE binding = ?1 AND CASE WHEN length(metadata) > 0
                   THEN json_extract(CAST(metadata AS TEXT), '$.artist_id') = ?2 ELSE 0 END
                 UNION
                 SELECT json_extract(lines.progress, '$.progress.album.offer')
                 FROM requests_jobs jobs JOIN requests_lines lines ON jobs.id = lines.job
                 WHERE jobs.restore = ?3 AND jobs.stage < ?4 AND jobs.id <> ?5
                   AND json_extract(CAST(jobs.work AS TEXT), '$.binding') = ?6
                   AND json_extract(lines.progress, '$.progress.state') = 'asked'
                   AND json_extract(lines.progress, '$.offer.artist_id') = ?2",
            ).map_err(sql_error)?;
            let releases = read.query_map(params![binding.as_slice(), artist, restore, FINISHED, row, saved_binding],
                |record| record.get::<_, String>(0)).map_err(sql_error)?;
            releases.collect::<Result<_, _>>().map_err(sql_error)
        })
    }
}

enum LineEnding {
    Confirmed { album: BatchAlbum, ask: RecordedAsk },
    Progress(BatchLineProgress),
    Failed(Refusal),
}

fn checkpoint_batch(
    keeper: &mut Keeper, now: u64, row: i64, binding: [u8; 32], account: AccountId,
    saved: Option<Vec<u8>>, changes: Vec<(u16, LineEnding)>,
) -> Result<(), Refusal> {
    let work = saved.map(|saved| {
        let mut plan = read_work(keeper.data().connection(), row)?;
        plan.prepared_batch = Some(saved);
        encode(&plan)
    }).transpose()?;
    let mut updates = Vec::new();
    let mut asked = HashMap::new();
    for (index, ending) in changes {
        let mut line = saved_line(keeper.data().connection(), row, index)?;
        // A confirmed line and its ownership were committed together. Never
        // recreate that ask after another job has removed the release.
        if line.confirmed_placement.is_some() {
            continue;
        }
        match ending {
            LineEnding::Confirmed { album, ask } => {
                line.progress = Some(BatchLineProgress::Done { album });
                line.confirmed_placement = Some(ask.placed);
                asked.entry(ask.asked.release.clone()).or_insert(ask);
            }
            LineEnding::Progress(_) if matches!(line.progress, Some(BatchLineProgress::Failed { .. })) => continue,
            LineEnding::Progress(progress) => line.progress = Some(progress),
            LineEnding::Failed(_) if matches!(line.progress, Some(BatchLineProgress::NotFound | BatchLineProgress::Failed { .. })) => continue,
            LineEnding::Failed(reason) => {
                line.progress = Some(BatchLineProgress::Failed { reason, album: line.offer.as_ref().map(batch_album) });
            }
        }
        updates.push((index, serde_json::to_string(&line).map_err(|_| Refusal::NotDealtWith)?));
    }
    if work.is_none() && updates.is_empty() {
        return Ok(());
    }
    let at = keeper.clock(now).effective();
    keeper.write_data(now, |tx| {
        if let Some(work) = &work {
            tx.execute("UPDATE requests_jobs SET work = ?1 WHERE id = ?2", params![work, row])?;
        }
        for (index, text) in &updates {
            keep_line(tx, row, *index, text)?;
        }
        for ask in asked.values() {
            keep_owner(tx, &binding, account, ask, at)?;
        }
        Ok(((), true))
    }).map_err(|_| Refusal::NotDealtWith)
}

fn confirmed_placement(
    lidarr: &Lidarr, offer: &Offer, placement: Option<&AlbumPlacement>,
) -> Result<PlacementRecovery, Refusal> {
    let artist = MusicBrainzId::new(&offer.artist_id).ok_or(Refusal::LidarrUnrecognized)?;
    let Some((number, _)) = lidarr_result(lidarr.known_artist(artist.as_str()), Some(offer))? else {
        return Ok(PlacementRecovery::Unconfirmed);
    };
    let Some(id) = LidarrNumber::new(number) else { return Ok(PlacementRecovery::Unconfirmed) };
    let listed = lidarr_result(lidarr.listed(number), Some(offer))?;
    Ok(wire::batch_effect_confirmed(offer, &wire::ArtistTarget { id, artist }, Some(number), &listed, placement))
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
        journal.ask(move |keeper, now, row| {
            if matches!(saved.progress, Some(BatchLineProgress::NeedsResolution)) && saved.awaiting_ms.is_none() {
                saved.awaiting_ms = Some(keeper.clock(now).effective());
            }
            save_line(keeper, now, row, index, &saved)
        })?;
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

// Preserve the core batch's own work checkpoints and mirror every temporary
// artist setting to the same recovery journal used by single jobs.
struct BatchUpstream<'a> {
    lidarr: &'a Lidarr,
    journal: &'a Journal,
    batch: &'a bulk::Job,
    denied: &'a Mutex<Option<Refusal>>,
}

impl BatchUpstream<'_> {
    fn after_write<T>(&self, result: T) -> T {
        if lock(self.denied).is_some() {
            self.batch.stop();
        }
        result
    }

    fn recovery<'a>(&self, offer: &Offer, batch: &'a dyn lidarr::WayBack) -> BatchWayBack<'a> {
        BatchWayBack {
            batch,
            journal: Journal {
                artist: MusicBrainzId::new(&offer.artist_id),
                ..self.journal.clone()
            },
        }
    }
}

struct BatchWayBack<'a> {
    batch: &'a dyn lidarr::WayBack,
    journal: Journal,
}

impl lidarr::WayBack for BatchWayBack<'_> {
    fn turn(&self) -> Box<dyn lidarr::Turn + '_> {
        self.batch.turn()
    }

    fn keep(&self, address: &str, artist: i64, before: &str) -> Result<bool, String> {
        let artist_id = self.journal.artist.as_ref().ok_or("Couldn't keep the way back.")?;
        self.keep_for_artist(address, artist, before, artist_id.as_str())
    }

    fn keep_for_artist(&self, address: &str, artist: i64, before: &str, artist_id: &str) -> Result<bool, String> {
        if self.journal.artist.as_ref().is_none_or(|known| known.as_str() != artist_id) {
            return Err("Couldn't keep the way back.".into());
        }
        let kept = self.batch.keep_for_artist(address, artist, before, artist_id)?;
        if !kept {
            return Ok(false);
        }
        if let Err(reason) = self.journal.keep(address, artist, before) {
            self.batch.put_back(address, artist, before);
            return Err(reason);
        }
        let journal = &self.journal;
        lock(&journal.service.rooms).recovery_jobs.insert((journal.binding, artist), journal.row);
        Ok(true)
    }

    fn before_request_album(&self, placement: &AlbumPlacement) -> Result<(), String> {
        self.batch.before_request_album(placement)
    }

    fn put_back(&self, address: &str, artist: i64, before: &str) {
        self.journal.put_back(address, artist, before);
        self.batch.put_back(address, artist, before);
    }
}

impl lidarr::RequestLidarr for BatchUpstream<'_> {
    fn find_releases(&self, term: &str) -> Result<(Vec<Offer>, Vec<lidarr::Single>), String> {
        self.lidarr.find_releases(term)
    }

    fn find_any(&self, term: &str) -> Result<Vec<Offer>, String> {
        self.lidarr.find_any(term)
    }

    fn find_artists(&self, term: &str) -> Result<Vec<lidarr::ArtistOffer>, String> {
        self.lidarr.find_artists(term)
    }

    fn tracked(&self) -> Result<HashSet<String>, String> {
        self.lidarr.tracked()
    }

    fn known_artist(&self, artist: &str) -> Result<Option<(i64, String)>, String> {
        self.lidarr.known_artist(artist)
    }

    fn albums_of(&self, artist: i64, name: &str) -> Result<Vec<(Offer, bool)>, String> {
        self.lidarr.albums_of(artist, name)
    }

    fn releases_of(&self, artist: i64, name: &str) -> Result<Vec<(Offer, bool)>, String> {
        self.lidarr.releases_of(artist, name)
    }

    fn add_artist(&self, artist: &str, name: &str, way_back: &dyn lidarr::WayBack) -> Result<usize, String> {
        self.after_write(lidarr::RequestLidarr::add_artist(self.lidarr, artist, name, way_back))
    }

    fn request(&self, wanted: &Offer, way_back: &dyn lidarr::WayBack) -> Result<(), String> {
        self.request_placement(wanted, way_back).map(drop)
    }

    fn request_placement(&self, wanted: &Offer, way_back: &dyn lidarr::WayBack) -> Result<bool, String> {
        self.after_write(self.lidarr.request_placement(wanted, &self.recovery(wanted, way_back)))
    }

    fn pending(&self) -> Result<Vec<lidarr::Pending>, String> {
        self.lidarr.pending()
    }

    fn cancel(&self, id: i64) -> Result<(), String> {
        self.after_write(self.lidarr.cancel(id))
    }
}

impl lidarr::BulkLidarr for BatchUpstream<'_> {
    fn pause(&self, wait: Duration) -> Result<(), String> {
        if let Some(reason) = *lock(self.denied) {
            return Err(reason.sentence(None).to_string());
        }
        self.lidarr.pause(wait)
    }

    fn address(&self) -> &str {
        self.lidarr.address()
    }

    fn way_back_address(&self) -> String {
        self.lidarr.way_back_address()
    }

    fn artist_numbers(&self) -> Result<HashMap<String, i64>, String> {
        self.lidarr.artist_numbers()
    }

    fn listed(&self, artist: i64) -> Result<Vec<lidarr::Listed>, String> {
        self.lidarr.listed(artist)
    }

    fn busy(&self, artists: &HashSet<i64>) -> Result<(HashSet<i64>, u64), String> {
        self.lidarr.busy(artists)
    }

    fn busy_with(&self, artists: &HashSet<i64>) -> Result<HashSet<i64>, String> {
        self.lidarr.busy_with(artists)
    }

    fn start_listing_singles_kept(
        &self, artist: i64, wanted: &Offer, way_back: &dyn lidarr::WayBack,
    ) -> Result<Option<(String, bool)>, String> {
        self.after_write(self.lidarr.start_listing_singles_kept(artist, wanted, &self.recovery(wanted, way_back)))
    }

    fn want(&self, artist: i64, albums: &[i64]) -> Result<(), String> {
        self.after_write(self.lidarr.want(artist, albums))
    }

    fn want_before(&self, artist: i64, albums: &[i64], before: &dyn Fn() -> Result<(), String>) -> Result<(), String> {
        self.after_write(self.lidarr.want_before(artist, albums, before))
    }

    fn setup(&self) -> Result<lidarr::Setup, String> {
        self.lidarr.setup()
    }

    fn add_wanting(&self, setup: &mut lidarr::Setup, wanted: &[Offer]) -> Result<i64, String> {
        self.after_write(self.lidarr.add_wanting(setup, wanted))
    }

    fn add_wanting_before(
        &self, setup: &mut lidarr::Setup, wanted: &[Offer], before: &dyn Fn() -> Result<(), String>,
    ) -> Result<i64, String> {
        self.after_write(self.lidarr.add_wanting_before(setup, wanted, before))
    }

    fn unwant(&self, albums: &[i64]) -> Result<(), String> {
        self.after_write(self.lidarr.unwant(albums))
    }

    fn set_new_releases(&self, artist: i64, setting: &str) -> Result<(), String> {
        self.after_write(self.journal.restore_saved_artist(self.lidarr, artist, setting))
            .map_err(|reason| reason.sentence(None).to_string())
    }
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

fn read_work(db: &Connection, row: i64) -> Result<Plan, Refusal> {
    let saved: Vec<u8> = db.query_row("SELECT work FROM requests_jobs WHERE id = ?1", [row], |record| record.get(0))
        .map_err(sql_error)?;
    decode(&saved)
}

fn update_work(
    keeper: &mut Keeper, now: u64, row: i64, change: impl FnOnce(&mut Plan),
) -> Result<(), Refusal> {
    let mut plan = read_work(keeper.data().connection(), row)?;
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
    Ok(sealing.open(&binding, &sealed).map(|key| Credential {
        revision: binding.revision, key, check_stamp: sealed.check_stamp(),
    }))
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

fn admin_input<T, E>(
    service: &Requests, caller: Option<Extension<Caller>>, headers: &HeaderMap, body: Bytes,
    read: impl FnOnce(&[u8]) -> Result<T, E>, head: impl FnOnce(&T) -> &RequestHead,
) -> Result<(T, Ingress), admin::Refusal> {
    use zeroize::Zeroize;
    let request = crate::admin::browser_origin_allowed(headers, service.origin.as_deref())
        .then(|| read(&body));
    if let Ok(mut unique) = body.try_into_mut() {
        unique.as_mut().zeroize();
    }
    let request = request.ok_or_else(admin_denied)?.map_err(|_| admin::Refusal::Malformed)?;
    let ingress = service.admitted(headers, caller, head(&request)).map_err(admin_from_account)?;
    Ok((request, ingress))
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
    caller: Option<Extension<Caller>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let (request, ingress) = match admin_input(&service, caller, &headers, body, admin::read::<admin::SetRequestsEnabled>, |ask| &ask.head) {
        Ok(input) => input,
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
    ingress.check(&actor, device, kind).map_err(admin_from_account)?;
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
    caller: Option<Extension<Caller>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let (request, ingress) = match admin_input(&service, caller, &headers, body, admin::read::<admin::GetLidarrSetting>, |ask| &ask.head) {
        Ok(input) => input,
        Err(why) => return admin_reply::<admin::LidarrSetting>(Err(why)),
    };
    let clock = service.accounts.clock.clone();
    let endpoint = service.endpoint.clone();
    let address = service.accounts.lidarr.clone();
    let configured = service.accounts.desk.ask(move |keeper| {
        admin_actor(keeper, &request.head, &ingress, clock())?;
        let key = match endpoint {
            None => None,
            Some(endpoint) => credential(keeper, &endpoint).map_err(|_| admin::Refusal::NotDealtWith)?,
        };
        let checked = match &key {
            Some(key) => keeper.data().lidarr_checked(&key.check_stamp).map_err(|_| admin::Refusal::NotDealtWith)?,
            None => false,
        };
        Ok(admin::LidarrSetting { address, configured: key.is_some(), checked })
    }).await.unwrap_or(Err(admin::Refusal::Busy { retry_after_seconds: account::Wait::new(1) }));
    admin_reply(configured)
}

async fn set_lidarr(
    State(service): State<Arc<Requests>>,
    caller: Option<Extension<Caller>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let finished = async {
        let (request, ingress) = admin_input(&service, caller, &headers, body, admin::read::<admin::SetLidarr>, |ask| &ask.head)?;
        save_lidarr_key(&service, request, ingress).await
    }.await;
    if finished.is_ok() {
        service.wake.notify_one();
    }
    admin_reply(finished)
}

async fn save_lidarr_key(
    service: &Requests, request: admin::SetLidarr, ingress: Ingress,
) -> Result<admin::LidarrKeySaved, admin::Refusal> {
    let endpoint = service.endpoint.as_ref().ok_or(admin::Refusal::NotAllowed(admin::Denial::Conflict))?;
    let place = Place::upstream(service.rooms.clone(), ingress.caller.account.get()).ok_or_else(admin_busy)?;
    let address = endpoint.address.clone();
    let clock = service.accounts.clock.clone();
    let (authorized, request, ingress) = service.accounts.desk.ask(move |keeper| {
        let now = clock();
        let (device, mut actor) = admin_actor(keeper, &request.head, &ingress, now)?;
        let binding = current_lidarr(keeper, Some(&address))?;
        let grant = GrantUse { request_id: request.request_id, secret: &request.grant };
        let bearer = Bearer { device, hash: &ingress.hash, grant: Some(grant) };
        let authorized = actor.request(request.head.restore, keeper.clock(now), |mut turn| {
            turn.authorize_lidarr(bearer, &binding, &request.change.key)
        }).apply(|effects| keeper.persist(effects, &[], now))
            .map_err(|_| admin::Refusal::NotDealtWith)?
            .map_err(admin_said)?;
        Ok((authorized, request, ingress))
    }).await.map_err(|_| admin_busy())??;

    let lidarr = endpoint.lidarr(Handle::current(), request.change.key.expose(), None);
    let checked = tokio::task::spawn_blocking(move || {
        let _place = place;
        lidarr_result(lidarr.version(), None)
    }).await.map_err(|_| admin::Refusal::NotDealtWith)?;
    let check = match checked {
        Ok(_) => admin::LidarrKeyCheck::Works,
        Err(Refusal::LidarrKeyRefused) => return Err(admin::Refusal::NotAllowed(admin::Denial::LidarrLoginRefused)),
        Err(_) => admin::LidarrKeyCheck::Unreachable,
    };

    let clock = service.accounts.clock.clone();
    let address = endpoint.address.clone();
    service.accounts.desk.ask(move |keeper| {
        let now = clock();
        let (device, mut actor) = admin_actor(keeper, &request.head, &ingress, now)?;
        let binding = current_lidarr(keeper, Some(&address))?;
        let sealing = admin::LidarrSealingKey::new(keeper.seal_key());
        let nonce = random::<{ admin::LIDARR_NONCE_BYTES }>().ok_or(admin::Refusal::NotDealtWith)?;
        let grant = GrantUse { request_id: request.request_id, secret: &request.grant };
        let bearer = Bearer { device, hash: &ingress.hash, grant: Some(grant) };
        actor.request(request.head.restore, keeper.clock(now), |turn| {
            turn.install_lidarr(bearer, authorized, &binding, &request.change.key, check, &sealing, *nonce)
        }).apply(|effects| keeper.persist(effects, &[], now))
            .map_err(|_| admin::Refusal::NotDealtWith)?
            .map_err(admin_said)?;
        Ok(admin::LidarrKeySaved { check })
    }).await.map_err(|_| admin_busy())?
}

fn admin_busy() -> admin::Refusal {
    admin::Refusal::Busy { retry_after_seconds: account::Wait::new(1) }
}

async fn attribution(
    State(service): State<Arc<Requests>>,
    caller: Option<Extension<Caller>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response<Body> {
    let finished = async {
        let (request, ingress) = admin_input(&service, caller, &headers, body, wire::read_attribution, |ask| &ask.head)?;
        read_attributed(&service, request, ingress).await
    }.await;
    let reply = match finished {
        Ok(page) => admin::Reply::Done(page),
        Err(why) => admin::Reply::Refused(why),
    };
    match wire::write_attribution_reply(&reply) {
        Ok(body) => events::respond(crate::accounts::session::Answer { status: reply.status(), body }),
        Err(_) => admin_reply::<AttributionPage>(Err(admin::Refusal::NotDealtWith)),
    }
}

async fn read_attributed(
    service: &Arc<Requests>, request: wire::AttributionRead, ingress: Ingress,
) -> Result<AttributionPage, admin::Refusal> {
    let account = ingress.caller.account;
    let session = ingress.caller.session;
    let clock = service.accounts.clock.clone();
    let state = service.clone();
    if let Some(after) = request.after {
        return service.accounts.desk.ask(move |keeper| {
            admin_actor(keeper, &request.head, &ingress, clock())?;
            lock(&state.books).attribution_next(session, after).map_err(|why| match why {
                Refusal::SearchAgain => admin::Refusal::NotAllowed(admin::Denial::Conflict),
                _ => admin::Refusal::NotDealtWith,
            })
        }).await.map_err(|_| admin_busy())?;
    }

    let (mut rows, key, request, ingress) = service.accounts.desk.ask(move |keeper| {
        admin_actor(keeper, &request.head, &ingress, clock())?;
        let (rows, key) = if let Some(endpoint) = &state.endpoint {
            let rows = attributed_requests(keeper.data().connection(), &binding(endpoint))
                .map_err(|_| admin::Refusal::NotDealtWith)?;
            let key = credential(keeper, endpoint).map_err(|_| admin::Refusal::NotDealtWith)?;
            (rows, key)
        } else {
            (Vec::new(), None)
        };
        Ok((rows, key, request, ingress))
    }).await.map_err(|_| admin_busy())??;

    if !rows.is_empty()
        && let (Some(endpoint), Some(key)) = (&service.endpoint, key)
        && let Some(place) = Place::upstream(service.rooms.clone(), account.get())
    {
        let lidarr = endpoint.lidarr(Handle::current(), key.key.expose(), Some(Instant::now() + UPSTREAM_TIMEOUT));
        rows = tokio::task::spawn_blocking(move || {
            let _place = place;
            mark_arrived(&lidarr, &mut rows);
            rows
        }).await.map_err(|_| admin::Refusal::NotDealtWith)?;
    }

    let clock = service.accounts.clock.clone();
    let state = service.clone();
    service.accounts.desk.ask(move |keeper| {
        admin_actor(keeper, &request.head, &ingress, clock())?;
        lock(&state.books).attribution_first(account, session, rows).map_err(|_| admin::Refusal::NotDealtWith)
    }).await.map_err(|_| admin_busy())?
}

fn mark_arrived(lidarr: &Lidarr, rows: &mut [AttributionRow]) {
    let Ok(arrived) = lidarr.arrived_releases() else { return };
    for row in rows {
        if arrived.get(row.asked.artist_id.as_str()).is_some_and(|releases| releases.contains(row.asked.release.as_str())) {
            row.arrived = Some(true);
        }
    }
}
