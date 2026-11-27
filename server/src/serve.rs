//! The listener, and everything a request meets before it is answered (design 6.2, 13.5;
//! slice S1). In order:
//!
//! 1. **Who opened the connection.** Only the proxy (`NEXUS_PROXY`) is answered. One from
//!    anywhere else is closed as soon as it is accepted, without a byte read or written; but
//!    a connection from this machine's own loopback address may ask `hello`, and nothing
//!    else, for the container's health check.
//! 2. **How many are open.** Past [`CONNECTIONS_MAX`], one more is closed the same way.
//! 3. **Whether the proxy marked it HTTPS.** A request from the proxy that does not carry
//!    exactly one `X-Nexus-Scheme: https` is not answered: the connection is closed.
//! 4. **Its head:** it must all come within [`HEAD_DEADLINE`], in at most [`HEAD_MAX_BYTES`]
//!    and [`HEAD_MAX_HEADERS`] headers.
//! 5. **Its target**, exactly as it came: in plain form ([`proto::plain`]) and in origin form,
//!    or 400, and its body is not read.
//! 6. **Its body:** at most [`BODY_MAX_BYTES`], within [`BODY_DEADLINE`], or 413 / 408.
//! 7. **What answers it**, in a task of its own, within [`ANSWER_DEADLINE`] or 503; a fault
//!    there is that request's 500 and nothing else's.
//!
//! Every answer has `Cache-Control: no-store` and no cookie. Nothing about a request is
//! written to the log: not its address, a header, its target or its body. Only how many of
//! each kind were refused, now and then.

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::header::{CACHE_CONTROL, CONNECTION, CONTENT_TYPE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode, Uri};
use axum::routing::get;
use http_body_util::{BodyExt, LengthLimitError, Limited};
use hyper::body::{Body as _, Incoming};
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use nexus_core::nexus::proto::{self, Hello};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::Sleep;
use tower::ServiceExt;

use crate::bounds::*;

/// Who opened a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    /// The proxy: answered, if it marks each request as having come by HTTPS.
    Proxy,
    /// This machine's own loopback address, when that is not the proxy's: may ask `hello`
    /// alone, for the health check.
    Health,
    /// Anyone else: answered nothing.
    Stranger,
}

/// Who `from` is, for a proxy at `proxy`. Both are compared as plain addresses, so an IPv4
/// address written as IPv6 (`::ffff:a.b.c.d`) is the same address.
pub fn peer(from: IpAddr, proxy: IpAddr) -> Peer {
    let from = from.to_canonical();
    if from == proxy.to_canonical() {
        Peer::Proxy
    } else if from == IpAddr::V4(Ipv4Addr::LOCALHOST) || from == IpAddr::V6(Ipv6Addr::LOCALHOST) {
        Peer::Health
    } else {
        Peer::Stranger
    }
}

/// Whether the proxy marked a request as having come to it by HTTPS (design 6.2:
/// `proxy_set_header X-Nexus-Scheme $scheme;`): the header exactly once, exactly `https`.
pub fn marked_https(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all("x-nexus-scheme").iter();
    values.next().is_some_and(|value| value.as_bytes() == b"https") && values.next().is_none()
}

/// The target exactly as it came, if it is in origin form (`/…`): not the absolute form, and
/// not `*`.
fn origin_target(uri: &Uri) -> Option<&str> {
    if uri.scheme().is_some() || uri.authority().is_some() {
        return None;
    }
    uri.path_and_query().map(|target| target.as_str())
}

/// Each kind of refusal, counted for the log and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Stranger,
    Full,
    NotMarked,
    NotHealth,
    NotPlain,
    TooLarge,
    SlowBody,
    BadBody,
    Slow,
    Fault,
}

const REFUSALS: [(Refusal, &str); 10] = [
    (Refusal::Stranger, "from an address other than the proxy's"),
    (Refusal::Full, "with every connection taken"),
    (Refusal::NotMarked, "not marked by the proxy as HTTPS"),
    (Refusal::NotHealth, "from this machine, for anything but hello"),
    (Refusal::NotPlain, "for an address not in plain form"),
    (Refusal::TooLarge, "with a body too large"),
    (Refusal::SlowBody, "with a body too slow"),
    (Refusal::BadBody, "with a body that could not be read"),
    (Refusal::Slow, "answered too slowly"),
    (Refusal::Fault, "with a fault in what answered"),
];

/// The listener's own state: whom it answers, what answers, and its counts.
pub struct Shell {
    proxy: IpAddr,
    router: Router,
    open: AtomicUsize,
    refused: [AtomicU64; REFUSALS.len()],
}

impl Shell {
    pub fn new(proxy: IpAddr, router: Router) -> Arc<Self> {
        Arc::new(Shell { proxy: proxy.to_canonical(), router, open: AtomicUsize::new(0), refused: Default::default() })
    }

    /// How many connections are open now.
    pub fn open(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }

    /// How many of `kind` have been refused since the counts were last written.
    pub fn refused(&self, kind: Refusal) -> u64 {
        self.refused[kind as usize].load(Ordering::SeqCst)
    }

    fn count(&self, kind: Refusal) {
        self.refused[kind as usize].fetch_add(1, Ordering::SeqCst);
    }

    /// The counts since they were last taken, as a line for the log, and back to nothing.
    fn take_counts(&self) -> Option<String> {
        let parts: Vec<String> = REFUSALS
            .iter()
            .filter_map(|(kind, words)| {
                let count = self.refused[*kind as usize].swap(0, Ordering::SeqCst);
                (count > 0).then(|| format!("{count} {words}"))
            })
            .collect();
        (!parts.is_empty()).then(|| format!("Refused since the last count: {}.", parts.join("; ")))
    }
}

/// What answers, once a request is through every bound: `hello`, and "not found" for every
/// other path in plain form.
pub fn router(hello: &Hello) -> Router {
    let body = Bytes::from(hello.to_json());
    Router::new()
        .route(
            proto::HELLO,
            get(move || {
                let body = body.clone();
                async move { ([(CONTENT_TYPE, "application/json")], body) }
            }),
        )
        .fallback(|| async { StatusCode::NOT_FOUND })
}

/// Accepts and serves connections on `listener` for as long as the program runs. With `log`,
/// it writes the counts of refusals every [`COUNTS_EVERY`] that there were any.
pub async fn serve(listener: TcpListener, shell: Arc<Shell>, log: bool) {
    if log {
        let shell = shell.clone();
        tokio::spawn(async move {
            let mut every = tokio::time::interval(COUNTS_EVERY);
            every.tick().await;
            loop {
                every.tick().await;
                if let Some(line) = shell.take_counts() {
                    println!("{line}");
                }
            }
        });
    }
    let slots = Arc::new(Semaphore::new(CONNECTIONS_MAX));
    loop {
        let (stream, from) = match listener.accept().await {
            Ok(accepted) => accepted,
            // Out of file handles, or a connection gone before it was taken: wait a moment
            // rather than spin. Nothing of it is written out.
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let peer = peer(from.ip(), shell.proxy);
        if peer == Peer::Stranger {
            shell.count(Refusal::Stranger);
            drop(stream);
            continue;
        }
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            shell.count(Refusal::Full);
            drop(stream);
            continue;
        };
        let _ = stream.set_nodelay(true);
        let shell = shell.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let _open = Open::new(&shell);
            let service = {
                let shell = shell.clone();
                hyper::service::service_fn(move |request| answer(request, shell.clone(), peer))
            };
            let mut http = http1::Builder::new();
            http.timer(TokioTimer::new())
                .header_read_timeout(HEAD_DEADLINE)
                .max_buf_size(HEAD_MAX_BYTES)
                .max_headers(HEAD_MAX_HEADERS);
            // However it ends (closed, refused, a deadline, a write that made no progress),
            // nothing of it is written out.
            let _ = http.serve_connection(TokioIo::new(Guarded::new(stream)), service).await;
        });
    }
}

/// Counts a connection as open for as long as it lives.
struct Open(Arc<Shell>);

impl Open {
    fn new(shell: &Arc<Shell>) -> Self {
        shell.open.fetch_add(1, Ordering::SeqCst);
        Open(shell.clone())
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        self.0.open.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A request left unanswered: hyper closes the connection without writing a byte.
#[derive(Debug)]
struct Unanswered;

impl std::fmt::Display for Unanswered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("not answered")
    }
}

impl std::error::Error for Unanswered {}

/// An answer with no body that closes the connection.
fn refusal(status: StatusCode) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    response.headers_mut().insert(CONNECTION, HeaderValue::from_static("close"));
    finish(&mut response);
    response
}

/// What every answer carries, and what none does.
fn finish(response: &mut Response<Body>) {
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    headers.remove(SET_COOKIE);
}

async fn answer(request: Request<Incoming>, shell: Arc<Shell>, peer: Peer) -> Result<Response<Body>, Unanswered> {
    match peer {
        Peer::Proxy if !marked_https(request.headers()) => {
            shell.count(Refusal::NotMarked);
            return Err(Unanswered);
        }
        Peer::Health if !(request.method() == Method::GET && origin_target(request.uri()) == Some(proto::HELLO)) => {
            shell.count(Refusal::NotHealth);
            return Err(Unanswered);
        }
        Peer::Stranger => return Err(Unanswered),
        _ => {}
    }
    if !origin_target(request.uri()).is_some_and(|target| proto::plain(target).is_ok()) {
        shell.count(Refusal::NotPlain);
        return Ok(refusal(StatusCode::BAD_REQUEST));
    }
    let (parts, body) = request.into_parts();
    // A body that says it is too large is refused before a byte of it is read.
    if body.size_hint().lower() > BODY_MAX_BYTES as u64 {
        shell.count(Refusal::TooLarge);
        return Ok(refusal(StatusCode::PAYLOAD_TOO_LARGE));
    }
    let bytes = match tokio::time::timeout(BODY_DEADLINE, Limited::new(body, BODY_MAX_BYTES).collect()).await {
        Err(_) => {
            shell.count(Refusal::SlowBody);
            return Ok(refusal(StatusCode::REQUEST_TIMEOUT));
        }
        Ok(Err(why)) if why.downcast_ref::<LengthLimitError>().is_some() => {
            shell.count(Refusal::TooLarge);
            return Ok(refusal(StatusCode::PAYLOAD_TOO_LARGE));
        }
        Ok(Err(_)) => {
            shell.count(Refusal::BadBody);
            return Ok(refusal(StatusCode::BAD_REQUEST));
        }
        Ok(Ok(collected)) => collected.to_bytes(),
    };
    let request = Request::from_parts(parts, Body::from(bytes));
    let mut task = tokio::spawn(shell.router.clone().oneshot(request));
    let mut response = match tokio::time::timeout(ANSWER_DEADLINE, &mut task).await {
        Err(_) => {
            task.abort();
            shell.count(Refusal::Slow);
            refusal(StatusCode::SERVICE_UNAVAILABLE)
        }
        Ok(Err(_)) => {
            shell.count(Refusal::Fault);
            refusal(StatusCode::INTERNAL_SERVER_ERROR)
        }
        Ok(Ok(Ok(response))) => response,
        Ok(Ok(Err(never))) => match never {},
    };
    finish(&mut response);
    Ok(response)
}

/// A connection whose writes may make no progress for no longer than [`WRITE_DEADLINE`]:
/// past it, the write fails and the connection is closed.
struct Guarded {
    stream: TcpStream,
    stalled: Option<Pin<Box<Sleep>>>,
}

impl Guarded {
    fn new(stream: TcpStream) -> Self {
        Guarded { stream, stalled: None }
    }

    fn watch<T>(&mut self, done: Poll<io::Result<T>>, cx: &mut Context<'_>) -> Poll<io::Result<T>> {
        match done {
            Poll::Ready(done) => {
                self.stalled = None;
                Poll::Ready(done)
            }
            Poll::Pending => {
                let stalled = self.stalled.get_or_insert_with(|| Box::pin(tokio::time::sleep(WRITE_DEADLINE)));
                match stalled.as_mut().poll(cx) {
                    Poll::Ready(()) => Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, "a write made no progress"))),
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    }
}

impl AsyncRead for Guarded {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, into: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_read(cx, into)
    }
}

impl AsyncWrite for Guarded {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let done = Pin::new(&mut this.stream).poll_write(cx, bytes);
        this.watch(done, cx)
    }

    fn poll_write_vectored(self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[io::IoSlice<'_>]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let done = Pin::new(&mut this.stream).poll_write_vectored(cx, bytes);
        this.watch(done, cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let done = Pin::new(&mut this.stream).poll_flush(cx);
        this.watch(done, cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn who_is_answered() {
        let proxy: IpAddr = "172.18.0.2".parse().unwrap();
        assert_eq!(peer("172.18.0.2".parse().unwrap(), proxy), Peer::Proxy);
        assert_eq!(peer("::ffff:172.18.0.2".parse().unwrap(), proxy), Peer::Proxy);
        assert_eq!(peer("127.0.0.1".parse().unwrap(), proxy), Peer::Health);
        assert_eq!(peer("::1".parse().unwrap(), proxy), Peer::Health);
        assert_eq!(peer("::ffff:127.0.0.1".parse().unwrap(), proxy), Peer::Health);
        for stranger in ["172.18.0.3", "127.0.0.2", "10.0.0.1", "::ffff:10.0.0.1", "fd00::2", "0.0.0.0"] {
            assert_eq!(peer(stranger.parse().unwrap(), proxy), Peer::Stranger, "{stranger}");
        }
        // A proxy on this machine's loopback is the proxy there, and needs its mark.
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        assert_eq!(peer(local, local), Peer::Proxy);
    }

    #[test]
    fn marked_once_and_exactly() {
        let mark = |values: &[&str]| {
            let mut headers = HeaderMap::new();
            for value in values {
                headers.append("x-nexus-scheme", HeaderValue::from_str(value).unwrap());
            }
            marked_https(&headers)
        };
        assert!(mark(&["https"]));
        for wrong in [&[][..], &["http"], &["HTTPS"], &["https "], &["https", "https"], &["http", "https"], &["https,http"]] {
            assert!(!mark(wrong), "{wrong:?}");
        }
    }

    #[test]
    fn the_counts_are_taken_once() {
        let shell = Shell::new("172.18.0.2".parse().unwrap(), Router::new());
        assert_eq!(shell.take_counts(), None);
        shell.count(Refusal::Stranger);
        shell.count(Refusal::Stranger);
        shell.count(Refusal::NotPlain);
        assert_eq!(
            shell.take_counts().as_deref(),
            Some("Refused since the last count: 2 from an address other than the proxy's; 1 for an address not in plain form.")
        );
        assert_eq!(shell.take_counts(), None);
        for (index, (kind, _)) in REFUSALS.iter().enumerate() {
            assert_eq!(*kind as usize, index);
        }
    }
}
