//! A request's head, read and judged by the backend itself before hyper sees a byte of it.
//!
//! hyper answers some heads on its own (a 400 for a head it cannot parse, a 431 for one too
//! large, a `100 Continue` when a body is asked for), and it does so before anything of
//! ours can look at who is asking. So the head is read here first, with the same parser
//! hyper uses (`httparse`), and judged ([`judge`]) on its exact bytes: who opened the
//! connection and whether the proxy marked it; the target exactly as it came, fragment and
//! all, before any parsing of it as a URI; the method; and how the body is framed and how
//! large it says it is. **Nothing is written to the connection until the gate has passed**,
//! and nothing of the body is read until all of it has.
//!
//! Only a head that passes every check is handed on to hyper, which then serves that one
//! request and closes the connection.

use axum::http::StatusCode;
use nexus_core::nexus::proto;
use tokio::io::{AsyncRead, AsyncReadExt};

use crate::bounds::{BODY_MAX_BYTES, HEAD_MAX_BYTES, HEAD_MAX_HEADERS};
use crate::serve::{Peer, Refusal};

/// A request's head as it came, and the bytes read with it.
#[derive(Debug, Clone)]
pub struct Head {
    /// Every byte read from the connection so far: the head, and whatever of the body the
    /// last read happened to bring with it (never more than [`HEAD_MAX_BYTES`] in all).
    pub read: Vec<u8>,
    pub method: String,
    /// The request target exactly as it came.
    pub target: String,
    /// Each header as it came, the name in lower case.
    pub headers: Vec<(String, Vec<u8>)>,
}

impl Head {
    fn all(&self, name: &str) -> impl Iterator<Item = &[u8]> {
        self.headers.iter().filter(move |(have, _)| have == name).map(|(_, value)| value.as_slice())
    }
}

/// Why no head was had. Each is answered with nothing.
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
}

/// What a complete head in `bytes` is, if `bytes` holds one.
fn parse(bytes: &[u8]) -> Result<Option<Head>, NoHead> {
    let mut headers = [httparse::EMPTY_HEADER; HEAD_MAX_HEADERS];
    let mut request = httparse::Request::new(&mut headers);
    match request.parse(bytes) {
        Ok(httparse::Status::Partial) => Ok(None),
        Ok(httparse::Status::Complete(_)) => Ok(Some(Head {
            read: Vec::new(),
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

/// What is done with a request, decided from its head alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing is written: the connection is closed.
    Unanswered(Refusal),
    /// One fixed answer with no body, and the connection is closed.
    Refused(StatusCode, Refusal),
    /// Handed on, with a body of this many bytes (0 for none).
    Serve(usize),
}

/// Every path the backend answers, and the one method each answers.
pub const ROUTES: &[(&str, &str)] = &[(proto::HELLO, "GET")];

/// Whether the proxy marked the request as having come to it by HTTPS (design 6.2:
/// `proxy_set_header X-Nexus-Scheme $scheme;`): the header exactly once, exactly `https`.
pub fn marked_https(head: &Head) -> bool {
    let mut values = head.all("x-nexus-scheme");
    values.next() == Some(b"https") && values.next().is_none()
}

/// What is done with the request whose head is `head`, from a connection opened by `peer`. In
/// order: the gate (who, and the proxy's mark); then the target exactly as it came; the
/// method; and the body's framing and size.
pub fn judge(peer: Peer, head: &Head, routes: &[(&str, &str)]) -> Verdict {
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
    let Some(&(_, method)) = routes.iter().find(|(path, _)| *path == head.target) else {
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
                Some(length) if length <= BODY_MAX_BYTES as u64 => length as usize,
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
    fn a_head_is_whole_within_its_bounds_or_not_at_all() {
        assert_eq!(parse(b"GET /nexus/api/hello HTTP/1.1\r\nHost: x\r\n").unwrap().map(|_| ()), None);
        assert_eq!(parse(b"GET /nexus/api/hello HTTP/1.1\r\nBad Header\r\n\r\n").unwrap_err(), NoHead::Wrong);
        assert_eq!(parse(b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03").unwrap_err(), NoHead::Wrong);
        let headers = |count: usize| (0..count).map(|n| format!("X-{n}: x\r\n")).collect::<String>();
        assert!(parse(format!("GET / HTTP/1.1\r\n{}\r\n", headers(HEAD_MAX_HEADERS)).as_bytes()).unwrap().is_some());
        assert_eq!(parse(format!("GET / HTTP/1.1\r\n{}\r\n", headers(HEAD_MAX_HEADERS + 1)).as_bytes()).unwrap_err(), NoHead::TooManyHeaders);
    }
}
