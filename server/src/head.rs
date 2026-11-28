//! A request's head, read and judged by the backend itself before hyper sees a byte of it.
//!
//! hyper answers some heads on its own (a 400 for a head it cannot parse, a 431 for one too
//! large, a `100 Continue` when a body is asked for), and it does so before anything of
//! ours can look at who is asking. So the head is read here first, with the same parser
//! hyper uses (`httparse`), and judged ([`judge`]) on its exact bytes: who opened the
//! connection and whether the proxy marked it; the target exactly as it came, fragment and
//! all, before any parsing of it as a URI; the method; and how the body is framed and how
//! large it says it is. **Nothing of ours is written until the gate has decided**: a request
//! that fails it is given the gate's one fixed answer if it came from the proxy and nothing
//! otherwise ([`crate::serve`]); and nothing of the body is read until every check has passed.
//!
//! Only a head that passes every check is handed on to hyper, which then serves that one
//! request and closes the connection.

use axum::http::StatusCode;
use nexus_core::nexus::{account, admin, diag, events, proto};
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::bounds::{BODY_MAX_BYTES, HEAD_MAX_BYTES, HEAD_MAX_HEADERS};
use crate::serve::{Peer, Refusal};

/// A request's head as it came, and the bytes read with it. Its `Debug` says how large each
/// part is and which headers came, never a header's value or a byte read: a session, a note, a
/// password or a code can be in them (the review of S8, C2).
#[derive(Clone)]
pub struct Head {
    /// Every byte read from the connection so far: the head, and whatever of the body the
    /// last read happened to bring with it (never more than [`HEAD_MAX_BYTES`] in all), and a
    /// sign-in's whole body when it is read before a work slot ([`read_body_by`]).
    pub read: Vec<u8>,
    /// How many of `read` are the head.
    pub head_bytes: usize,
    pub method: String,
    /// The request target exactly as it came.
    pub target: String,
    /// Each header as it came, the name in lower case.
    pub headers: Vec<(String, Vec<u8>)>,
}

impl std::fmt::Debug for Head {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.headers.iter().map(|(name, _)| name.as_str()).collect();
        f.debug_struct("Head")
            .field("read_bytes", &self.read.len())
            .field("head_bytes", &self.head_bytes)
            .field("method", &self.method)
            .field("target_bytes", &self.target.len())
            .field("header_names", &names)
            .finish()
    }
}

impl Head {
    pub fn all(&self, name: &str) -> impl Iterator<Item = &[u8]> {
        self.headers.iter().filter(move |(have, _)| have == name).map(|(_, value)| value.as_slice())
    }

    /// What of the body has been read with the head.
    pub fn body(&self) -> &[u8] {
        &self.read[self.head_bytes.min(self.read.len())..]
    }
}

/// Why no head was had. Each fails the gate: the proxy is given the gate's one answer, anyone
/// else nothing (and a connection closed before a byte, nothing at all).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoHead {
    /// The connection ended before a byte.
    Closed,
    /// It ended, or failed, part way through a head.
    Cut,
    /// No end of the head within [`HEAD_MAX_BYTES`].
    TooLarge,
    /// More than [`HEAD_MAX_HEADERS`] headers.
    TooManyHeaders,
    /// Not a head that `httparse` reads.
    Wrong,
    /// Not whole by its deadline.
    Late,
}

/// What a complete head in `bytes` is, if `bytes` holds one.
fn parse(bytes: &[u8]) -> Result<Option<Head>, NoHead> {
    let mut headers = [httparse::EMPTY_HEADER; HEAD_MAX_HEADERS];
    let mut request = httparse::Request::new(&mut headers);
    match request.parse(bytes) {
        Ok(httparse::Status::Partial) => Ok(None),
        Ok(httparse::Status::Complete(head_bytes)) => Ok(Some(Head {
            read: Vec::new(),
            head_bytes,
            method: request.method.unwrap_or_default().to_string(),
            target: request.path.unwrap_or_default().to_string(),
            headers: request.headers.iter().map(|header| (header.name.to_ascii_lowercase(), header.value.to_vec())).collect(),
        })),
        Err(httparse::Error::TooManyHeaders) => Err(NoHead::TooManyHeaders),
        Err(_) => Err(NoHead::Wrong),
    }
}

/// Reads one request's head from `from`, reading no more than [`HEAD_MAX_BYTES`] in all. The
/// caller bounds the time it may take.
pub async fn read(from: &mut (impl AsyncRead + Unpin)) -> Result<Head, NoHead> {
    let mut read = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        let room = HEAD_MAX_BYTES - read.len();
        if room == 0 {
            return Err(NoHead::TooLarge);
        }
        let got = match from.read(&mut chunk[..room.min(4096)]).await {
            Ok(0) if read.is_empty() => return Err(NoHead::Closed),
            Ok(0) | Err(_) => return Err(NoHead::Cut),
            Ok(got) => got,
        };
        read.extend_from_slice(&chunk[..got]);
        if let Some(mut head) = parse(&read)? {
            head.read = read;
            return Ok(head);
        }
    }
}

/// Reads one request's head from `from`, whole by `deadline` (counted from when the connection
/// was taken) or not at all: a head that is read only as the deadline passes is late too.
pub async fn read_by(deadline: tokio::time::Instant, from: &mut (impl AsyncRead + Unpin)) -> Result<Head, NoHead> {
    match tokio::time::timeout_at(deadline, read(from)).await {
        // The timer and the read may be ready together, and the read is looked at first: the
        // deadline wins.
        Ok(Ok(_)) if tokio::time::Instant::now() >= deadline => Err(NoHead::Late),
        Ok(done) => done,
        Err(_) => Err(NoHead::Late),
    }
}

/// Reads the rest of `head`'s body, `length` bytes in all, into `head.read`, whole by
/// `deadline` (the head's own) or not at all: for a sign-in, whose body says whether it may take
/// a reserved slot (the review of S8, C1). Nothing past the body is read.
pub async fn read_body_by(deadline: tokio::time::Instant, from: &mut (impl AsyncRead + Unpin), head: &mut Head, length: usize) -> Result<(), NoHead> {
    let want = head.head_bytes + length;
    let reading = async {
        let mut chunk = [0u8; 4096];
        while head.read.len() < want {
            let room = (want - head.read.len()).min(chunk.len());
            match from.read(&mut chunk[..room]).await {
                Ok(0) | Err(_) => return Err(NoHead::Cut),
                Ok(got) => head.read.extend_from_slice(&chunk[..got]),
            }
        }
        Ok(())
    };
    match tokio::time::timeout_at(deadline, reading).await {
        Ok(done) => done,
        Err(_) => Err(NoHead::Late),
    }
}

/// What is done with a request, decided from its head alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// It failed the gate: the proxy is given the gate's one fixed answer, anyone else nothing,
    /// and the connection is closed.
    Unanswered(Refusal),
    /// One fixed answer with no body, and the connection is closed.
    Refused(StatusCode, Refusal),
    /// Handed on, with a body of this many bytes (0 for none).
    Serve(usize),
}

/// A path the backend answers: the one method it answers there, and the largest body that
/// call takes (design 7.2, 12.1), checked from the head before a byte of the body is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub path: &'static str,
    pub method: &'static str,
    pub body_max: usize,
}

impl Route {
    pub const fn new(path: &'static str, method: &'static str, body_max: usize) -> Self {
        Route { path, method, body_max }
    }
}

/// A path and its method, with a body of up to the backend's largest.
impl From<(&'static str, &'static str)> for Route {
    fn from((path, method): (&'static str, &'static str)) -> Self {
        Route::new(path, method, BODY_MAX_BYTES)
    }
}

/// Every path the backend answers, the one method each answers, and the largest body each
/// takes. A call that carries a password takes 4 KiB at most (7.2, the slice notes' S8).
pub const ROUTES: &[Route] = &[
    Route::new(proto::HELLO, "GET", BODY_MAX_BYTES),
    Route::new(crate::admin::PAGE, "GET", BODY_MAX_BYTES),
    Route::new(account::SIGN_IN, "POST", account::PASSWORD_REQUEST_MAX_BYTES),
    Route::new(account::SET_UP, "POST", account::SET_UP_MAX_BYTES),
    Route::new(account::CHANGE_PASSWORD, "POST", account::PASSWORD_REQUEST_MAX_BYTES),
    Route::new(account::SIGN_OUT, "POST", account::SMALL_REQUEST_MAX_BYTES),
    Route::new(account::LIST_DEVICES, "POST", account::SMALL_REQUEST_MAX_BYTES),
    Route::new(account::FORCE_OUT, "POST", account::SMALL_REQUEST_MAX_BYTES),
    Route::new(account::MUSIC_LOGIN, "POST", account::SMALL_REQUEST_MAX_BYTES),
    Route::new(events::SEND_PATH, "POST", events::BATCH_BYTES),
    Route::new(events::DELETE_PATH, "POST", account::GRANTED_CHANGE_MAX_BYTES),
    Route::new(account::DELETE_LISTENING_DATA, "POST", account::GRANTED_CHANGE_MAX_BYTES),
    Route::new(account::GRANT, "POST", account::PASSWORD_REQUEST_MAX_BYTES),
    Route::new(crate::events::STATS_PATH, "POST", account::SMALL_REQUEST_MAX_BYTES),
    Route::new(nexus_core::nexus::requests::REQUESTS, "POST", nexus_core::nexus::requests::CALL_MAX_BYTES),
    Route::new(diag::SEND_PATH, "POST", diag::CALL_BYTES),
    Route::new(crate::diag::LIST_PATH, "POST", account::SMALL_REQUEST_MAX_BYTES),
    Route::new(crate::diag::ADMIN_PATH, "POST", account::SMALL_REQUEST_MAX_BYTES),
    // The admin's calls of S9: the two that carry a password at 4 KiB, the address at the grant
    // envelope's own ceiling, the read small.
    Route::new(admin::REQUEST_GRANT, "POST", account::PASSWORD_REQUEST_MAX_BYTES),
    Route::new(admin::SET_MUSIC_LOGIN, "POST", account::PASSWORD_REQUEST_MAX_BYTES),
    Route::new(nexus_core::nexus::requests::ADMIN_ATTRIBUTION, "POST", account::SMALL_REQUEST_MAX_BYTES),
    Route::new(crate::requests::GET_LIDARR, "POST", account::SMALL_REQUEST_MAX_BYTES),
    Route::new(admin::SET_REQUESTS_ENABLED, "POST", account::GRANTED_CHANGE_MAX_BYTES),
    Route::new(admin::SET_MUSIC_SERVER, "POST", account::GRANTED_CHANGE_MAX_BYTES),
    Route::new(admin::GET_MUSIC_SERVER, "POST", account::SMALL_REQUEST_MAX_BYTES),
];

/// Whether the proxy marked the request as having come to it by HTTPS (design 6.2:
/// `proxy_set_header X-Nexus-Scheme $scheme;`): the header exactly once, exactly `https`.
pub fn marked_https(head: &Head) -> bool {
    let mut values = head.all("x-nexus-scheme");
    values.next() == Some(b"https") && values.next().is_none()
}

/// What is done with the request whose head is `head`, from a connection opened by `peer`. In
/// order: the gate (who, and the proxy's mark); then the target exactly as it came; the
/// method; and the body's framing and size.
pub fn judge(peer: Peer, head: &Head, routes: &[Route]) -> Verdict {
    // The gate. Until it has passed, nothing is written.
    match peer {
        Peer::Stranger => return Verdict::Unanswered(Refusal::Stranger),
        Peer::Proxy if !marked_https(head) => return Verdict::Unanswered(Refusal::NotMarked),
        Peer::Health if !(head.method == "GET" && head.target == proto::HELLO) => return Verdict::Unanswered(Refusal::NotHealth),
        _ => {}
    }
    // The target, every byte of it, before anything is made of it.
    if proto::plain(&head.target).is_err() {
        return Verdict::Refused(StatusCode::BAD_REQUEST, Refusal::NotPlain);
    }
    let Some(&Route { method, body_max, .. }) = routes.iter().find(|route| route.path == head.target) else {
        return Verdict::Refused(if head.method == "GET" { StatusCode::NOT_FOUND } else { StatusCode::METHOD_NOT_ALLOWED }, Refusal::NoSuchPath);
    };
    if head.method != method {
        return Verdict::Refused(StatusCode::METHOD_NOT_ALLOWED, Refusal::NotAllowed);
    }
    // The body: counted, and no larger than its bound. The proxy sends every body with a
    // Content-Length (experiment E2.4: it takes a chunked body whole and sends it counted),
    // so no other framing is taken; nor `Expect`, which would have hyper ask for the body.
    if head.all("transfer-encoding").next().is_some() || head.all("expect").next().is_some() {
        return Verdict::Refused(StatusCode::BAD_REQUEST, Refusal::Framing);
    }
    let mut lengths = head.all("content-length");
    let length = match (lengths.next(), lengths.next()) {
        (None, _) => 0,
        // Digits alone; a number too large to read is too large.
        (Some(length), None) if !length.is_empty() && length.iter().all(u8::is_ascii_digit) => {
            match std::str::from_utf8(length).ok().and_then(|length| length.parse::<u64>().ok()) {
                Some(length) if length <= body_max.min(BODY_MAX_BYTES) as u64 => length as usize,
                _ => return Verdict::Refused(StatusCode::PAYLOAD_TOO_LARGE, Refusal::TooLarge),
            }
        }
        _ => return Verdict::Refused(StatusCode::BAD_REQUEST, Refusal::Framing),
    };
    Verdict::Serve(length)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(text: &str) -> Head {
        let mut head = parse(text.as_bytes()).unwrap().expect("a whole head");
        head.read = text.as_bytes().to_vec();
        head
    }

    fn judge_s1(peer: Peer, head: &Head) -> Verdict {
        judge(peer, head, ROUTES)
    }

    fn get(target: &str, extra: &str) -> Head {
        head(&format!("GET {target} HTTP/1.1\r\nHost: x\r\nX-Nexus-Scheme: https\r\n{extra}\r\n"))
    }

    /// C2 (the review of S8): a head read with its body's first bytes shows neither the
    /// session in its `Authorization` nor anything of the body.
    #[test]
    fn a_head_s_debug_shows_no_header_value_and_no_byte_read() {
        let session = "Bearer sess-0123456789abcdef0123456789abcdef";
        let body = r#"{"credential":{"password":{"email":"someone@example.test","password":"hunter2-hunter2-hunter2"}},"code":"ABCDEF-GHJKMN","revoke":{"secret":"note-secret-77"}}"#;
        let request = format!(
            "POST {} HTTP/1.1\r\nHost: x\r\nX-Nexus-Scheme: https\r\nAuthorization: {session}\r\nContent-Length: {}\r\n\r\n{body}",
            account::SIGN_IN,
            body.len()
        );
        let mut from = request.as_bytes();
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let read = runtime.block_on(read(&mut from)).unwrap();
        assert!(read.read.len() > body.len(), "the body came with the head");
        let shown = format!("{read:?} {read:#?}");
        for secret in ["sess-0123", "hunter2", "someone@", "ABCDEF", "note-secret", "Bearer", "credential"] {
            assert!(!shown.contains(secret), "Debug showed {secret:?}: {shown}");
        }
        assert!(shown.contains("authorization"), "the header's name is shown: {shown}");
    }

    #[test]
    fn the_gate_comes_first() {
        let unmarked = head("GET /nexus/api/hello HTTP/1.1\r\nHost: x\r\n\r\n");
        let marked = get(proto::HELLO, "");
        assert_eq!(judge_s1(Peer::Stranger, &marked), Verdict::Unanswered(Refusal::Stranger));
        assert_eq!(judge_s1(Peer::Proxy, &unmarked), Verdict::Unanswered(Refusal::NotMarked));
        assert_eq!(judge_s1(Peer::Proxy, &marked), Verdict::Serve(0));
        assert_eq!(judge_s1(Peer::Health, &unmarked), Verdict::Serve(0));
        // Unmarked, whatever else is wrong with it, is answered nothing.
        for wrong in ["GET /nexus/x/../api HTTP/1.1\r\n\r\n", "HEAD /nexus/api/hello HTTP/1.1\r\n\r\n", "POST /nexus/api/hello HTTP/1.1\r\nTransfer-Encoding: chunked\r\nExpect: 100-continue\r\n\r\n"] {
            assert!(matches!(judge_s1(Peer::Proxy, &head(wrong)), Verdict::Unanswered(_)), "{wrong}");
        }
        // This machine may ask hello, by GET, at exactly its path, and nothing else.
        for wrong in ["HEAD /nexus/api/hello HTTP/1.1\r\n\r\n", "GET /nexus/api/hello#x HTTP/1.1\r\n\r\n", "GET /nexus/api/hello/ HTTP/1.1\r\n\r\n"] {
            assert_eq!(judge_s1(Peer::Health, &head(wrong)), Verdict::Unanswered(Refusal::NotHealth), "{wrong}");
        }
    }

    #[test]
    fn the_target_is_judged_as_it_came() {
        for target in ["/nexus/api/hello#x", "/nexus/api/hello?", "/nexus/api/hello?x=1", "/nexus/api/../api/hello", "http://music.test/nexus/api/hello", "*"] {
            assert_eq!(judge_s1(Peer::Proxy, &get(target, "")), Verdict::Refused(StatusCode::BAD_REQUEST, Refusal::NotPlain), "{target}");
        }
        assert_eq!(judge_s1(Peer::Proxy, &get("/nexus/api/nothing", "")), Verdict::Refused(StatusCode::NOT_FOUND, Refusal::NoSuchPath));
    }

    #[test]
    fn hello_is_get_alone() {
        for method in ["HEAD", "POST", "PUT", "DELETE", "OPTIONS", "PATCH", "get", "TRACE"] {
            let head = head(&format!("{method} {} HTTP/1.1\r\nX-Nexus-Scheme: https\r\n\r\n", proto::HELLO));
            assert_eq!(judge_s1(Peer::Proxy, &head), Verdict::Refused(StatusCode::METHOD_NOT_ALLOWED, Refusal::NotAllowed), "{method}");
        }
        let head = head("POST /nexus/api/nothing HTTP/1.1\r\nX-Nexus-Scheme: https\r\n\r\n");
        assert_eq!(judge_s1(Peer::Proxy, &head), Verdict::Refused(StatusCode::METHOD_NOT_ALLOWED, Refusal::NoSuchPath));
    }

    #[test]
    fn only_a_counted_body_within_its_bound_is_taken() {
        let ok = |extra: &str| judge_s1(Peer::Proxy, &get(proto::HELLO, extra));
        assert_eq!(ok("Content-Length: 0\r\n"), Verdict::Serve(0));
        assert_eq!(ok(&format!("Content-Length: {BODY_MAX_BYTES}\r\n")), Verdict::Serve(BODY_MAX_BYTES));
        assert_eq!(ok(&format!("Content-Length: {}\r\n", BODY_MAX_BYTES + 1)), Verdict::Refused(StatusCode::PAYLOAD_TOO_LARGE, Refusal::TooLarge));
        assert_eq!(ok("Content-Length: 99999999999999999999\r\n"), Verdict::Refused(StatusCode::PAYLOAD_TOO_LARGE, Refusal::TooLarge));
        for framing in [
            "Transfer-Encoding: chunked\r\n",
            "Transfer-Encoding: identity\r\n",
            "Expect: 100-continue\r\n",
            "Content-Length: 1\r\nContent-Length: 1\r\n",
            "Content-Length: +1\r\n",
            "Content-Length: 1, 1\r\n",
            "Content-Length: \r\n",
            "Content-Length: 5\r\nTransfer-Encoding: chunked\r\n",
        ] {
            assert_eq!(ok(framing), Verdict::Refused(StatusCode::BAD_REQUEST, Refusal::Framing), "{framing}");
        }
    }

    #[test]
    fn a_head_ready_only_as_its_deadline_passes_is_late() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        runtime.block_on(async {
            let whole = b"GET /nexus/api/hello HTTP/1.1\r\nX-Nexus-Scheme: https\r\n\r\n";
            // Its deadline already passed when the head is first looked at, though every byte
            // of it is there to be read.
            let passed = tokio::time::Instant::now() - std::time::Duration::from_millis(1);
            assert_eq!(read_by(passed, &mut &whole[..]).await.map(|_| ()), Err(NoHead::Late));
            let ahead = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
            assert!(read_by(ahead, &mut &whole[..]).await.is_ok());
        });
    }

    #[test]
    fn a_head_is_whole_within_its_bounds_or_not_at_all() {
        assert_eq!(parse(b"GET /nexus/api/hello HTTP/1.1\r\nHost: x\r\n").unwrap().map(|_| ()), None);
        assert_eq!(parse(b"GET /nexus/api/hello HTTP/1.1\r\nBad Header\r\n\r\n").unwrap_err(), NoHead::Wrong);
        assert_eq!(parse(b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03").unwrap_err(), NoHead::Wrong);
        let headers = |count: usize| (0..count).map(|n| format!("X-{n}: x\r\n")).collect::<String>();
        assert!(parse(format!("GET / HTTP/1.1\r\n{}\r\n", headers(HEAD_MAX_HEADERS)).as_bytes()).unwrap().is_some());
        assert_eq!(parse(format!("GET / HTTP/1.1\r\n{}\r\n", headers(HEAD_MAX_HEADERS + 1)).as_bytes()).unwrap_err(), NoHead::TooManyHeaders);
    }
}
