//! The listener, and everything a request meets before it is answered (design 6.2, 13.5;
//! slice S1). Each connection carries **one request**, and then it is closed. In order:
//!
//! 1. **Who opened the connection.** Anyone but the proxy (`NEXUS_PROXY`) is closed as soon as
//!    it is accepted, without a byte read or written; except that this machine's own loopback
//!    address may ask `GET /nexus/api/hello`, and nothing else, for the container's health
//!    check.
//! 2. **How many are open:** past [`CONNECTIONS_MAX`], one more from the proxy is given the
//!    gate's answer; past [`TURNED_AWAY_MAX`] more, none is taken until there is room, and it
//!    waits in the kernel's backlog ([`LISTEN_BACKLOG`]).
//!
//!    Every connection's head deadline counts from when it is taken.
//! 3. **Its head**, read here and not by hyper ([`crate::head`]): all of it within
//!    [`HEAD_DEADLINE`], in at most [`HEAD_MAX_BYTES`] and [`HEAD_MAX_HEADERS`] headers.
//! 4. **The gate:** a request from the proxy must carry exactly one `X-Nexus-Scheme: https`.
//!
//!    Whatever fails before or at the gate is answered by **one fixed bare refusal when the
//!    connection is the proxy's** ([`GATE`]: the same bytes whatever the cause, so that the
//!    proxy hands the caller a plain answer and writes nothing to its error log; a connection
//!    closed under it would be written there with the caller's address and the request line,
//!    as measured), and **with nothing at all when it is not**.
//! 5. **The target exactly as it came**, the method, and the body's framing and declared size,
//!    each refused with a bare answer before a byte of the body is read.
//! 6. **The body**, read by hyper within [`BODY_DEADLINE`] into one buffer of the size it
//!    declared.
//! 7. **What answers it**, in a task of its own, within [`ANSWER_DEADLINE`] or 503; a fault
//!    there is that request's 500 and nothing else's.
//!
//! After a bare answer, whatever more the caller sends is discarded, for a moment and up to a
//! bound ([`LINGER_DEADLINE`], [`LINGER_MAX_BYTES`]), so that closing does not reset the
//! connection under the answer before the proxy has read it.
//!
//! Every answer has `Cache-Control: no-store` and no cookie. Nothing about a request is
//! written to the log: not its address, a header, its target or its body. Only how many of
//! each kind were refused, now and then.

use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, SET_COOKIE};
use axum::http::{HeaderValue, Request, Response, StatusCode};
use axum::routing::get;
use http_body_util::BodyExt;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper_util::rt::{TokioIo, TokioTimer};
use nexus_core::nexus::proto::{self, Hello};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::Sleep;
use tower::ServiceExt;

use crate::bounds::*;
use crate::head::{self, NoHead, Verdict};

/// Who opened a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Peer {
    /// The proxy: answered, if it marks the request as having come by HTTPS.
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

/// Each kind of refusal, counted for the log and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Stranger,
    Full,
    SlowHead,
    BadHead,
    NotMarked,
    NotHealth,
    NotPlain,
    NoSuchPath,
    NotAllowed,
    Framing,
    TooLarge,
    SlowBody,
    BadBody,
    Slow,
    Fault,
    Stalled,
}

const REFUSALS: [(Refusal, &str); 16] = [
    (Refusal::Stranger, "from an address other than the proxy's"),
    (Refusal::Full, "with every connection taken"),
    (Refusal::SlowHead, "with a head too slow"),
    (Refusal::BadHead, "with a head too large or not read"),
    (Refusal::NotMarked, "not marked by the proxy as HTTPS"),
    (Refusal::NotHealth, "from this machine, for anything but hello"),
    (Refusal::NotPlain, "for an address not in plain form"),
    (Refusal::NoSuchPath, "for a path there is not"),
    (Refusal::NotAllowed, "with a method not allowed"),
    (Refusal::Framing, "with a body framed otherwise than by its length"),
    (Refusal::TooLarge, "with a body too large"),
    (Refusal::SlowBody, "with a body too slow"),
    (Refusal::BadBody, "with a body that could not be read"),
    (Refusal::Slow, "answered too slowly"),
    (Refusal::Fault, "with a fault in what answered"),
    (Refusal::Stalled, "whose caller stopped taking the answer"),
];

/// The listener's own state: whom it answers, what answers, and its counts.
pub struct Shell {
    proxy: IpAddr,
    router: Router,
    routes: Vec<(&'static str, &'static str)>,
    open: AtomicUsize,
    refused: [AtomicU64; REFUSALS.len()],
}

impl Shell {
    /// The backend's own: [`router`] and [`head::ROUTES`].
    pub fn new(proxy: IpAddr, hello: &Hello) -> Arc<Self> {
        Self::with_routes(proxy, router(hello), head::ROUTES.to_vec())
    }

    /// With other routes (the tests add some): `routes` names each path `router` answers, and
    /// the one method it answers there.
    pub fn with_routes(proxy: IpAddr, router: Router, routes: Vec<(&'static str, &'static str)>) -> Arc<Self> {
        Arc::new(Shell { proxy: proxy.to_canonical(), router, routes, open: AtomicUsize::new(0), refused: Default::default() })
    }

    /// How many connections are open now, turned-away ones included.
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

/// What answers, once a request is through every bound. Every other path and
/// method was refused before this ([`head::ROUTES`]).
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
        .route(crate::admin::PAGE, get(crate::admin::document))
        .route(crate::admin::SIGN_IN, axum::routing::post(crate::admin::signin))
        .fallback(|| async { StatusCode::NOT_FOUND })
}

/// A bare answer: a status, nothing else said, no body, and the connection closed.
pub fn bare(status: StatusCode) -> Vec<u8> {
    format!(
        "HTTP/1.1 {} {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        status.as_str(),
        status.canonical_reason().unwrap_or("")
    )
    .into_bytes()
}

/// What the proxy is answered for everything that fails before or at the gate: a bad, large
/// or slow head, no mark, every connection taken. One status for every cause, so that it says
/// nothing of which. A 400, because the proxy hands any status of the backend's to the caller
/// as it is (it intercepts none), and a 400, unlike a reset or a timeout, is never a reason
/// for it to try elsewhere or to note the backend as failing.
pub const GATE: StatusCode = StatusCode::BAD_REQUEST;

/// A listener on `address` whose backlog, the connections the kernel holds before they are
/// taken, is [`LISTEN_BACKLOG`].
pub fn listen(address: std::net::SocketAddr) -> io::Result<TcpListener> {
    let socket = if address.is_ipv4() { tokio::net::TcpSocket::new_v4()? } else { tokio::net::TcpSocket::new_v6()? };
    socket.set_reuseaddr(true)?;
    socket.bind(address)?;
    socket.listen(LISTEN_BACKLOG)
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
    // Room for every connection taken: those served, and those only given the gate's answer.
    // A connection is taken only when there is room at least to answer it, so that one from
    // the proxy is never taken and then closed with nothing (which the proxy would log, and
    // answer with a 502). Past that, it waits in the kernel's backlog ([`LISTEN_BACKLOG`])
    // until there is room.
    let room = Arc::new(Semaphore::new(CONNECTIONS_MAX + TURNED_AWAY_MAX));
    let slots = Arc::new(Semaphore::new(CONNECTIONS_MAX));
    loop {
        let Ok(roomed) = room.clone().acquire_owned().await else { return };
        let (stream, from) = match listener.accept().await {
            Ok(accepted) => accepted,
            // Out of file handles, or a connection gone before it was taken: wait a moment
            // rather than spin. Nothing of it is written out.
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        // The head's deadline counts from here, not from when its task first runs.
        let deadline = tokio::time::Instant::now() + HEAD_DEADLINE;
        let peer = peer(from.ip(), shell.proxy);
        if peer == Peer::Stranger {
            shell.count(Refusal::Stranger);
            drop(stream);
            continue;
        }
        let _ = stream.set_nodelay(true);
        let shell = shell.clone();
        match slots.clone().try_acquire_owned() {
            Ok(slot) => {
                tokio::spawn(async move {
                    let _room = roomed;
                    let _slot = slot;
                    let _open = Open::new(&shell);
                    connection(Guarded::new(stream, shell.clone()), peer, deadline, shell).await;
                });
            }
            Err(_) => {
                shell.count(Refusal::Full);
                // The proxy is told so, by the gate's one answer, for which there is always room
                // here; no one else is told anything.
                if peer != Peer::Proxy {
                    continue;
                }
                tokio::spawn(async move {
                    let _room = roomed;
                    let _open = Open::new(&shell);
                    answer_bare(Guarded::new(stream, shell.clone()), GATE).await;
                });
            }
        }
    }
}

/// One connection: its head read and judged, then refused or served.
async fn connection(mut stream: Guarded, peer: Peer, deadline: tokio::time::Instant, shell: Arc<Shell>) {
    let refused = |kind| {
        shell.count(kind);
        peer == Peer::Proxy
    };
    let head = match head::read_by(deadline, &mut stream).await {
        Ok(head) => head,
        // Opened and closed with nothing said: there is no one to answer.
        Err(NoHead::Closed) => return,
        Err(why) => {
            if refused(if why == NoHead::Late { Refusal::SlowHead } else { Refusal::BadHead }) {
                answer_bare(stream, GATE).await;
            }
            return;
        }
    };
    match head::judge(peer, &head, &shell.routes) {
        Verdict::Unanswered(kind) => {
            if refused(kind) {
                answer_bare(stream, GATE).await;
            }
        }
        Verdict::Refused(status, kind) => {
            shell.count(kind);
            answer_bare(stream, status).await;
        }
        Verdict::Serve(length) => {
            let service = {
                let shell = shell.clone();
                hyper::service::service_fn(move |request| answer(request, length, shell.clone()))
            };
            let mut http = http1::Builder::new();
            http.timer(TokioTimer::new())
                .keep_alive(false)
                .header_read_timeout(HEAD_DEADLINE)
                .max_buf_size(HEAD_MAX_BYTES)
                .max_header_size(HEAD_MAX_BYTES)
                .max_headers(HEAD_MAX_HEADERS);
            // hyper reads the same head again, from what was read already: a head that passed
            // every check above, so that hyper has nothing of its own to answer. However the
            // connection ends, nothing of it is written out.
            let replay = Replay { ahead: head.read, at: 0, stream };
            let _ = http.serve_connection(TokioIo::new(replay), service).await;
        }
    }
}

/// Writes a bare answer, and then discards whatever more comes, for a moment and up to a
/// bound, so that closing does not reset the connection under the answer.
async fn answer_bare(mut stream: Guarded, status: StatusCode) {
    if stream.write_all(&bare(status)).await.is_err() || stream.shutdown().await.is_err() {
        return;
    }
    let _ = tokio::time::timeout(LINGER_DEADLINE, async {
        let mut left = LINGER_MAX_BYTES;
        let mut sink = [0u8; 8192];
        while left > 0 {
            match stream.read(&mut sink[..left.min(8192)]).await {
                Ok(0) | Err(_) => break,
                Ok(got) => left -= got,
            }
        }
    })
    .await;
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

/// What every answer carries, and what none does.
fn finish(response: &mut Response<Body>) {
    let headers = response.headers_mut();
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    headers.remove(SET_COOKIE);
}

fn refusal(status: StatusCode) -> Response<Body> {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    finish(&mut response);
    response
}

/// Reads a body that declared `length` bytes into one buffer of that size, each piece as it
/// comes and then let go, so that what it costs is its length however it is cut up.
pub async fn take_body<B>(mut body: B, length: usize) -> Result<Vec<u8>, Refusal>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
{
    let mut bytes = Vec::with_capacity(length);
    while let Some(frame) = body.frame().await {
        let frame = frame.map_err(|_| Refusal::BadBody)?;
        if let Ok(data) = frame.into_data() {
            if data.len() > length - bytes.len() {
                return Err(Refusal::TooLarge);
            }
            bytes.extend_from_slice(&data);
        }
    }
    if bytes.len() != length {
        return Err(Refusal::BadBody);
    }
    Ok(bytes)
}

async fn answer(request: Request<Incoming>, length: usize, shell: Arc<Shell>) -> Result<Response<Body>, Infallible> {
    let (parts, body) = request.into_parts();
    let bytes = match tokio::time::timeout(BODY_DEADLINE, take_body(body, length)).await {
        Err(_) => {
            shell.count(Refusal::SlowBody);
            return Ok(refusal(StatusCode::REQUEST_TIMEOUT));
        }
        Ok(Err(kind)) => {
            shell.count(kind);
            return Ok(refusal(if kind == Refusal::TooLarge { StatusCode::PAYLOAD_TOO_LARGE } else { StatusCode::BAD_REQUEST }));
        }
        Ok(Ok(bytes)) => Bytes::from(bytes),
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

/// The bytes read with the head, given again ahead of the rest of the connection.
struct Replay {
    ahead: Vec<u8>,
    at: usize,
    stream: Guarded,
}

impl AsyncRead for Replay {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, into: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.at < this.ahead.len() {
            let given = into.remaining().min(this.ahead.len() - this.at);
            into.put_slice(&this.ahead[this.at..this.at + given]);
            this.at += given;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.stream).poll_read(cx, into)
    }
}

impl AsyncWrite for Replay {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[u8]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write(cx, bytes)
    }

    fn poll_write_vectored(self: Pin<&mut Self>, cx: &mut Context<'_>, bytes: &[io::IoSlice<'_>]) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().stream).poll_write_vectored(cx, bytes)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().stream).poll_shutdown(cx)
    }
}

/// A connection whose writes may make no progress for no longer than [`WRITE_DEADLINE`]:
/// past it, the write fails, the connection is closed, and it is counted.
struct Guarded {
    stream: TcpStream,
    stalled: Option<Pin<Box<Sleep>>>,
    shell: Arc<Shell>,
}

impl Guarded {
    fn new(stream: TcpStream, shell: Arc<Shell>) -> Self {
        Guarded { stream, stalled: None, shell }
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
                    Poll::Ready(()) => {
                        self.shell.count(Refusal::Stalled);
                        Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, "a write made no progress")))
                    }
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
    fn the_gates_answer_is_one_and_bare() {
        let gate = String::from_utf8(bare(GATE)).unwrap();
        assert_eq!(
            gate,
            "HTTP/1.1 400 Bad Request\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn the_counts_are_taken_once() {
        let shell = Shell::with_routes("172.18.0.2".parse().unwrap(), Router::new(), Vec::new());
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

    /// A body in as many one-byte pieces as it has bytes.
    struct Crumbs(usize);

    impl hyper::body::Body for Crumbs {
        type Data = Bytes;
        type Error = Infallible;

        fn poll_frame(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Result<hyper::body::Frame<Bytes>, Infallible>>> {
            let this = self.get_mut();
            if this.0 == 0 {
                return Poll::Ready(None);
            }
            this.0 -= 1;
            Poll::Ready(Some(Ok(hyper::body::Frame::data(Bytes::from_static(b"a")))))
        }
    }

    #[test]
    fn a_body_in_crumbs_costs_its_length_and_no_more() {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        runtime.block_on(async {
            let bytes = take_body(Crumbs(BODY_MAX_BYTES), BODY_MAX_BYTES).await.unwrap();
            assert_eq!(bytes.len(), BODY_MAX_BYTES);
            // One buffer, of exactly the length declared: the pieces were each let go.
            assert_eq!(bytes.capacity(), BODY_MAX_BYTES);
            assert_eq!(take_body(Crumbs(11), 10).await, Err(Refusal::TooLarge));
            assert_eq!(take_body(Crumbs(9), 10).await, Err(Refusal::BadBody));
        });
    }
}
