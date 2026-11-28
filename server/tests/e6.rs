//! Experiment E6 (design 14.2): the backend's bounds, with no session, over real sockets
//! against the real listener, on loopback only. Each test starts its own backend.
//!
//! The proxy is played from 127.0.0.2 and a stranger from 127.0.0.3: both are this
//! machine's own loopback, and neither is 127.0.0.1, which may ask `hello` for the health
//! check. The bounds are the program's own (`bounds.rs`), not numbers made smaller for
//! testing. Times are held only to what a loaded machine cannot get wrong: nothing happens
//! before its deadline, and what must not wait on a deadline is done before that deadline.

use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::http::header::{CACHE_CONTROL, SET_COOKIE};
use axum::routing::get;
use nexus_core::nexus::proto::{self, BackendId, Found};
use nexus_music_server::bounds::*;
use nexus_music_server::probe::{self, Head};
use nexus_music_server::serve::{self, GATE, Refusal, Shell};

const PROXY: &str = "127.0.0.2";
const STRANGER: &str = "127.0.0.3";
const HEALTH: &str = "127.0.0.1";
const ID: [u8; 16] = [0x5a; 16];
/// How long anything that has no deadline of its own may take, however loaded the machine.
const LONG: Duration = Duration::from_secs(30);
/// What the large route answers: far more than the kernel can hold for a caller not reading.
const LARGE: usize = 64 * 1024 * 1024;

struct Running {
    at: SocketAddr,
    shell: Arc<Shell>,
    /// How many times the stalling route has begun.
    stalling: Arc<AtomicUsize>,
}

/// The real backend, with routes more for the tests: one that faults, one that never
/// answers, one that tries to set a cookie and to be kept, and one with a large answer.
fn start(proxy: &str) -> Running {
    let hello = nexus_music_server::hello(BackendId::from_bytes(ID));
    let stalling = Arc::new(AtomicUsize::new(0));
    let begun = stalling.clone();
    let router = serve::router(&hello)
        .route("/nexus/api/test/fault", get(|| async { panic!("a fault in one request") }))
        .route(
            "/nexus/api/test/stall",
            get(move || {
                let begun = begun.clone();
                async move {
                    begun.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    "late"
                }
            }),
        )
        .route("/nexus/api/test/cookie", get(|| async { ([(SET_COOKIE, "session=x"), (CACHE_CONTROL, "public, max-age=600")], "kept?") }))
        .route("/nexus/api/test/large", get(|| async { vec![b'a'; LARGE] }));
    let mut routes: Vec<nexus_music_server::head::Route> = nexus_music_server::head::ROUTES.iter().copied().filter(|route| route.path == nexus_core::nexus::proto::HELLO).collect();
    for path in ["/nexus/api/test/fault", "/nexus/api/test/stall", "/nexus/api/test/cookie", "/nexus/api/test/large"] {
        routes.push((path, "GET").into());
    }
    let proxy: IpAddr = proxy.parse().unwrap();
    let shell = Shell::with_routes(proxy, router, routes);
    let serving = shell.clone();
    let (bound, at) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        runtime.block_on(async move {
            // The program's own listener, with its backlog.
            let listener = serve::listen("127.0.0.1:0".parse().unwrap()).unwrap();
            bound.send(listener.local_addr().unwrap()).unwrap();
            serve::serve(listener, serving, false).await;
        });
    });
    Running { at: at.recv().unwrap(), shell, stalling }
}

/// A connection to `to` from the loopback address `from`.
fn connect(from: &str, to: SocketAddr) -> TcpStream {
    connect_with(from, to, None)
}

fn connect_with(from: &str, to: SocketAddr, receive_buffer: Option<usize>) -> TcpStream {
    let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
    if let Some(size) = receive_buffer {
        socket.set_recv_buffer_size(size).unwrap();
    }
    socket.bind(&SocketAddr::new(from.parse().unwrap(), 0).into()).unwrap();
    socket.connect(&to.into()).unwrap();
    let stream: TcpStream = socket.into();
    stream.set_read_timeout(Some(LONG)).unwrap();
    stream.set_write_timeout(Some(LONG)).unwrap();
    stream
}

fn get_request(target: &str, mark: Option<&str>) -> String {
    let mark = mark.map(|value| format!("X-Nexus-Scheme: {value}\r\n")).unwrap_or_default();
    format!("GET {target} HTTP/1.1\r\nHost: music.example.org\r\n{mark}\r\n")
}

fn hello_request() -> String {
    get_request(proto::HELLO, Some("https"))
}

/// Everything the backend writes on `stream` until it closes it (or resets it).
fn everything(stream: &mut TcpStream) -> Vec<u8> {
    let mut written = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => return written,
            Ok(got) => written.extend_from_slice(&chunk[..got]),
            Err(why) if why.kind() == std::io::ErrorKind::ConnectionReset => return written,
            Err(why) => panic!("reading the answer: {why}"),
        }
    }
}

/// Sends `request` (ignoring a connection closed under it) and gives back all that comes.
fn exchange(from: &str, at: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = connect(from, at);
    let _ = stream.write_all(request);
    everything(&mut stream)
}

/// The one answer in `written`, head and whole body.
fn one_answer(written: &[u8]) -> (Head, Vec<u8>) {
    let mut from = written;
    let head = probe::read_head(&mut from).unwrap().expect("an answer");
    let mut body = Vec::new();
    probe::body(&head, from).unwrap().read_to_end(&mut body).unwrap();
    (head, body)
}

/// Asserts that `written` is exactly the bare answer `status`: the same bytes every time.
fn assert_bare(written: &[u8], status: StatusCode) {
    assert_eq!(String::from_utf8_lossy(written), String::from_utf8_lossy(&serve::bare(status)));
}

fn assert_kept_by_no_one(head: &Head) {
    assert_eq!(head.one("cache-control"), Some("no-store"), "{head:?}");
    assert!(head.all("set-cookie").is_empty(), "{head:?}");
}

fn assert_is_the_backend(written: &[u8]) {
    let (head, body) = one_answer(written);
    assert_eq!(head.status, 200);
    assert_kept_by_no_one(&head);
    match proto::classify(head.status, &body[..]) {
        Found::Backend(hello) => assert_eq!(hello.id, BackendId::from_bytes(ID)),
        other => panic!("{other:?}"),
    }
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < LONG, "{what}: not within {LONG:?}");
        thread::sleep(Duration::from_millis(20));
    }
}

// ---- who is answered, and how ------------------------------------------------------------

#[test]
fn the_proxy_marked_https_is_answered_hello() {
    let backend = start(PROXY);
    let written = exchange(PROXY, backend.at, hello_request().as_bytes());
    assert_is_the_backend(&written);
    assert_eq!(one_answer(&written).0.one("content-type"), Some("application/json"));
}

#[test]
fn a_stranger_is_answered_nothing_whatever_it_sends() {
    let backend = start(PROXY);
    for request in [hello_request(), "GET /nexus/api/hello HTTP/1.1\r\nBad Header\r\n\r\n".into(), "\x16\x03\x01\x00".into(), String::new()] {
        assert!(exchange(STRANGER, backend.at, request.as_bytes()).is_empty(), "{request:?}");
    }
    assert_eq!(backend.shell.refused(Refusal::Stranger), 4);
}

/// Everything that fails before or at the gate, from the proxy. Each is given the gate's one
/// answer, the same bytes whatever the cause, and nothing else: no 400 or 431 of hyper's,
/// no `100 Continue`.
#[test]
fn the_proxy_failing_the_gate_is_given_one_bare_answer_whatever_the_cause() {
    let backend = start(PROXY);
    let headers = |count: usize| (0..count).map(|n| format!("X-N{n}: x\r\n")).collect::<String>();
    let requests: Vec<String> = vec![
        // No mark, a wrong one, two.
        get_request(proto::HELLO, None),
        get_request(proto::HELLO, Some("http")),
        get_request(proto::HELLO, Some("HTTPS")),
        get_request(proto::HELLO, Some("https, http")),
        format!("GET {} HTTP/1.1\r\nX-Nexus-Scheme: https\r\nX-Nexus-Scheme: https\r\n\r\n", proto::HELLO),
        // Unmarked, and wrong in a way hyper would have answered itself.
        format!("GET {} HTTP/1.1\r\n{}\r\n", proto::HELLO, headers(HEAD_MAX_HEADERS + 5)),
        format!("GET {} HTTP/1.1\r\nBad Header\r\n\r\n", proto::HELLO),
        format!("GET {} HTTP/1.1\r\nX-Large: {}\r\n\r\n", proto::HELLO, "a".repeat(HEAD_MAX_BYTES)),
        // Unmarked, asking hyper to ask for the body, or with a body hyper would choke on.
        format!("POST {} HTTP/1.1\r\nContent-Length: 5\r\nExpect: 100-continue\r\n\r\n", proto::HELLO),
        format!("POST {} HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\nnot a chunk\r\n", proto::HELLO),
        // Marked, but not a head at all, or too many headers, or too large: the mark in a
        // head that could not be read is not taken.
        format!("GET {} HTTP/1.1\r\nX-Nexus-Scheme: https\r\nBad Header\r\n\r\n", proto::HELLO),
        format!("GET {} HTTP/1.1\r\nX-Nexus-Scheme: https\r\n{}\r\n", proto::HELLO, headers(HEAD_MAX_HEADERS)),
        format!("GET {} HTTP/1.1\r\nX-Nexus-Scheme: https\r\nX-Large: {}\r\n\r\n", proto::HELLO, "a".repeat(HEAD_MAX_BYTES)),
        "\x16\x03\x01\x02\x00\x01\x00\x01\x03\x03".into(),
    ];
    for request in &requests {
        let written = exchange(PROXY, backend.at, request.as_bytes());
        assert_bare(&written, GATE);
        assert!(!String::from_utf8_lossy(&written).contains("100 Continue"), "{request:?}");
    }
    assert_eq!(backend.shell.refused(Refusal::NotMarked) + backend.shell.refused(Refusal::BadHead), requests.len() as u64);
}

#[test]
fn this_machine_may_ask_hello_and_is_answered_nothing_else() {
    // A proxy elsewhere (a made-up address of a documentation range).
    let backend = start("192.0.2.10");
    assert_is_the_backend(&exchange(HEALTH, backend.at, get_request(proto::HELLO, None).as_bytes()));
    for request in [
        get_request("/nexus/api/test/cookie", None),
        get_request("/nexus/api/nothing", None),
        get_request("/nexus/api/hello/", None),
        // The fragment that a URI's parser would drop, leaving hello.
        get_request("/nexus/api/hello#x", None),
        format!("POST {} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n", proto::HELLO),
        format!("HEAD {} HTTP/1.1\r\nHost: x\r\n\r\n", proto::HELLO),
        format!("GET {} HTTP/1.1\r\nBad Header\r\n\r\n", proto::HELLO),
    ] {
        assert!(exchange(HEALTH, backend.at, request.as_bytes()).is_empty(), "{request}");
    }
    // Any other loopback address is a stranger there.
    assert!(exchange(PROXY, backend.at, hello_request().as_bytes()).is_empty());
}

#[test]
fn a_proxy_on_loopback_needs_its_mark_there_too() {
    let backend = start(HEALTH);
    assert_bare(&exchange(HEALTH, backend.at, get_request(proto::HELLO, None).as_bytes()), GATE);
    assert_is_the_backend(&exchange(HEALTH, backend.at, hello_request().as_bytes()));
}

// ---- one request, and its head -----------------------------------------------------------

#[test]
fn a_connection_carries_one_request() {
    let backend = start(PROXY);
    // A second request after the first, and one far over the head's bound: neither is read.
    let oversized = format!("GET {} HTTP/1.1\r\nX-Nexus-Scheme: https\r\nX-Large: {}\r\n\r\n", proto::HELLO, "a".repeat(2 * HEAD_MAX_BYTES));
    for second in [hello_request(), oversized] {
        let written = exchange(PROXY, backend.at, format!("{}{second}", hello_request()).as_bytes());
        assert_is_the_backend(&written);
        assert_eq!(String::from_utf8_lossy(&written).matches("HTTP/1.1 ").count(), 1);
    }
}

#[test]
fn a_head_at_its_bounds_is_served_and_one_past_them_is_not() {
    let backend = start(PROXY);
    // Exactly HEAD_MAX_BYTES, and one more.
    let sized = |size: usize| {
        let bare = format!("GET {} HTTP/1.1\r\nX-Nexus-Scheme: https\r\nX-Pad: \r\n\r\n", proto::HELLO);
        let head = format!("GET {} HTTP/1.1\r\nX-Nexus-Scheme: https\r\nX-Pad: {}\r\n\r\n", proto::HELLO, "a".repeat(size - bare.len()));
        assert_eq!(head.len(), size);
        head
    };
    assert_is_the_backend(&exchange(PROXY, backend.at, sized(HEAD_MAX_BYTES).as_bytes()));
    assert_bare(&exchange(PROXY, backend.at, sized(HEAD_MAX_BYTES + 1).as_bytes()), GATE);
    // Exactly HEAD_MAX_HEADERS headers, the mark among them, and one more.
    let counted = |count: usize| {
        let others: String = (0..count - 1).map(|n| format!("X-N{n}: x\r\n")).collect();
        format!("GET {} HTTP/1.1\r\nX-Nexus-Scheme: https\r\n{others}\r\n", proto::HELLO)
    };
    assert_is_the_backend(&exchange(PROXY, backend.at, counted(HEAD_MAX_HEADERS).as_bytes()));
    assert_bare(&exchange(PROXY, backend.at, counted(HEAD_MAX_HEADERS + 1).as_bytes()), GATE);
}

#[test]
fn a_slow_head_is_given_the_gates_answer_at_its_deadline_and_not_before() {
    let backend = start(PROXY);
    let mut stream = connect(PROXY, backend.at);
    let started = Instant::now();
    stream.write_all(format!("GET {} HTTP/1.1\r\nX-Nexus-Scheme: https\r\n", proto::HELLO).as_bytes()).unwrap();
    // A header every half second, never the end of the head.
    let mut dripping = stream.try_clone().unwrap();
    thread::spawn(move || {
        for n in 0.. {
            thread::sleep(Duration::from_millis(500));
            if dripping.write_all(format!("X-Drip-{n}: x\r\n").as_bytes()).is_err() {
                break;
            }
        }
    });
    let written = everything(&mut stream);
    assert!(started.elapsed() >= HEAD_DEADLINE, "{:?}", started.elapsed());
    assert_bare(&written, GATE);
    assert_eq!(backend.shell.refused(Refusal::SlowHead), 1);
}

// ---- the address and the method ------------------------------------------------------------

#[test]
fn an_address_not_in_plain_form_is_refused_unread() {
    let backend = start(PROXY);
    // Every target of E2's table that reached the stand-in untidied and is not plain, the
    // others the experiments tried, and a fragment, which a URI's parser would drop.
    let targets = [
        "/nexus/api/hello#x",
        "/nexus/api/hello#",
        "/nexus/x/../api/hello",
        "/./nexus/api/hello",
        "/nexus%2fapi/hello",
        "/%6eexus/api/hello",
        "/nexus%2F",
        "/nexus/;x=1",
        "/nexus/a%20b",
        "/nexus/api/hello?x=1&t=abc",
        "/nexus/api/hello?",
        "/rest/../nexus/api/hello",
        "/rest/%2e%2e/nexus/api/hello",
        "/app/../nexus/",
        "/nexus/app.js",
        "/nexus/x.png",
        "/nexus/?q=union+select+(1)",
        "/nexus/?u=http://x",
        "//nexus/api/hello",
        "http://music.test/nexus/api/hello",
        "/nexus/../rest/ping",
        "/nexus/%2e%2e/rest/ping",
        "/rest/ping",
        "/nexus",
        "/NEXUS/api/hello",
        "/nexusx",
        "/nexus.json",
        "/music/nexus/",
        "/nexus/%00",
        "/nexus\\..\\rest\\ping",
        "*",
    ];
    for target in targets {
        // The head says a body follows, and a malformed chunk follows it: it is answered
        // all the same, and the same, so nothing of the body was read or waited for.
        let request = format!("POST {target} HTTP/1.1\r\nHost: music.example.org\r\nX-Nexus-Scheme: https\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n");
        assert_bare(&exchange(PROXY, backend.at, request.as_bytes()), StatusCode::BAD_REQUEST);
    }
    assert_eq!(backend.shell.refused(Refusal::NotPlain), targets.len() as u64);
}

#[test]
fn hello_is_get_alone_and_every_other_method_is_one_bare_refusal() {
    let backend = start(PROXY);
    for method in ["HEAD", "POST", "PUT", "DELETE", "OPTIONS", "PATCH", "TRACE", "CONNECT"] {
        let request = format!("{method} {} HTTP/1.1\r\nX-Nexus-Scheme: https\r\n\r\n", proto::HELLO);
        assert_bare(&exchange(PROXY, backend.at, request.as_bytes()), StatusCode::METHOD_NOT_ALLOWED);
    }
    assert_eq!(backend.shell.refused(Refusal::NotAllowed), 8);
    // A path there is not: 404 for GET, and the same 405 for anything else.
    assert_bare(&exchange(PROXY, backend.at, get_request("/nexus/api/nothing", Some("https")).as_bytes()), StatusCode::NOT_FOUND);
    assert_bare(&exchange(PROXY, backend.at, get_request("/nexus/", Some("https")).as_bytes()), StatusCode::NOT_FOUND);
    let request = "HEAD /nexus/api/nothing HTTP/1.1\r\nX-Nexus-Scheme: https\r\n\r\n";
    assert_bare(&exchange(PROXY, backend.at, request.as_bytes()), StatusCode::METHOD_NOT_ALLOWED);
}

#[test]
fn no_answer_sets_a_cookie_or_may_be_kept() {
    let backend = start(PROXY);
    let (head, body) = one_answer(&exchange(PROXY, backend.at, get_request("/nexus/api/test/cookie", Some("https")).as_bytes()));
    assert_eq!((head.status, &body[..]), (200, &b"kept?"[..]));
    assert_kept_by_no_one(&head);
    assert_eq!(head.all("cache-control").len(), 1);
}

// ---- the body ----------------------------------------------------------------------------

fn get_with_length(length: usize) -> String {
    format!("GET {} HTTP/1.1\r\nX-Nexus-Scheme: https\r\nContent-Length: {length}\r\n\r\n", proto::HELLO)
}

#[test]
fn a_body_is_counted_and_no_other_framing_is_taken() {
    let backend = start(PROXY);
    // Said to be one byte over: refused before a byte of it is sent, so before its deadline.
    let started = Instant::now();
    assert_bare(&exchange(PROXY, backend.at, get_with_length(BODY_MAX_BYTES + 1).as_bytes()), StatusCode::PAYLOAD_TOO_LARGE);
    assert!(started.elapsed() < BODY_DEADLINE);
    // Chunks, of any size, and `Expect`: refused, and nothing is asked for.
    let chunked = format!("GET {} HTTP/1.1\r\nX-Nexus-Scheme: https\r\nTransfer-Encoding: chunked\r\n\r\n{}0\r\n\r\n", proto::HELLO, "1\r\na\r\n".repeat(1000));
    assert_bare(&exchange(PROXY, backend.at, chunked.as_bytes()), StatusCode::BAD_REQUEST);
    let expect = format!("GET {} HTTP/1.1\r\nX-Nexus-Scheme: https\r\nContent-Length: 5\r\nExpect: 100-continue\r\n\r\n", proto::HELLO);
    let written = exchange(PROXY, backend.at, expect.as_bytes());
    assert_bare(&written, StatusCode::BAD_REQUEST);
    assert_eq!(backend.shell.refused(Refusal::TooLarge), 1);
    assert_eq!(backend.shell.refused(Refusal::Framing), 2);
    // At the bound exactly, the body is taken whole and hello answered.
    let mut request = get_with_length(BODY_MAX_BYTES).into_bytes();
    request.extend(vec![b'a'; BODY_MAX_BYTES]);
    assert_is_the_backend(&exchange(PROXY, backend.at, &request));
}

#[test]
fn a_slow_body_is_cut_off_at_its_deadline_and_not_before() {
    let backend = start(PROXY);
    let mut stream = connect(PROXY, backend.at);
    stream.write_all(get_with_length(1000).as_bytes()).unwrap();
    let started = Instant::now();
    let mut dripping = stream.try_clone().unwrap();
    thread::spawn(move || {
        for _ in 0..90 {
            thread::sleep(Duration::from_millis(500));
            if dripping.write_all(b"0123456789").is_err() {
                break;
            }
        }
    });
    let (head, _) = one_answer(&everything(&mut stream));
    assert!(started.elapsed() >= BODY_DEADLINE, "{:?}", started.elapsed());
    assert_eq!(head.status, 408);
    assert_kept_by_no_one(&head);
    assert_eq!(backend.shell.refused(Refusal::SlowBody), 1);
}

// ---- connections -------------------------------------------------------------------------

/// A request whose head is in and whose one-byte body is withheld: it holds a work slot
/// (its body is read within the slot) for as long as it waits.
fn holding(at: SocketAddr) -> TcpStream {
    let mut stream = connect(PROXY, at);
    stream.write_all(format!("GET {} HTTP/1.1\r\nHost: music.example.org\r\nX-Nexus-Scheme: https\r\nContent-Length: 1\r\n\r\n", proto::HELLO).as_bytes()).unwrap();
    stream
}

/// Every general work slot held (the review of S8, C1: a shell without accounts has nothing
/// that may take the reserve): one more from the proxy is given the gate's answer, one from
/// anyone else nothing, and those holding slots are served.
#[test]
fn one_request_more_than_the_slots_is_turned_away_and_the_rest_are_served() {
    let backend = start(PROXY);
    let general = CONNECTIONS_MAX - RESERVED_SLOTS;
    let mut held: Vec<TcpStream> = (0..general).map(|_| holding(backend.at)).collect();
    wait_until("every general slot taken", || backend.shell.serving() == general);
    // The proxy is given the gate's answer; anyone else, nothing.
    assert_bare(&exchange(PROXY, backend.at, hello_request().as_bytes()), GATE);
    assert!(exchange(HEALTH, backend.at, get_request(proto::HELLO, None).as_bytes()).is_empty());
    assert_eq!(backend.shell.refused(Refusal::Full), 2);
    assert_eq!(backend.shell.serving(), general, "the reserve was taken by a request that is no one's");
    // Those holding slots are served.
    held[0].write_all(b"x").unwrap();
    assert_is_the_backend(&everything(&mut held[0]));
    // That one done, the next is taken.
    drop(held.remove(0));
    wait_until("one let go", || backend.shell.serving() == general - 1);
    assert_is_the_backend(&exchange(PROXY, backend.at, hello_request().as_bytes()));
}

/// With every connection taken by callers that have said nothing (a head not yet read holds no
/// work slot), one more from the proxy is not taken and closed with nothing (which the proxy
/// would log, and answer with a 502): it waits in the backlog, and is served once the silent
/// ones have reached their head's deadline and been let go.
#[test]
fn with_every_room_taken_one_more_from_the_proxy_waits_and_is_answered() {
    let all = CONNECTIONS_MAX + TURNED_AWAY_MAX;
    let backend = start(PROXY);
    let _held: Vec<TcpStream> = (0..all).map(|_| connect(PROXY, backend.at)).collect();
    wait_until("every room taken", || backend.shell.open() == all);
    assert_eq!(backend.shell.serving(), 0, "a head not read took a work slot");
    let mut more = connect(PROXY, backend.at);
    more.write_all(hello_request().as_bytes()).unwrap();
    let written = everything(&mut more);
    assert!(!written.is_empty(), "taken and closed with nothing");
    assert_is_the_backend(&written);
}

#[test]
fn a_write_that_makes_no_progress_is_cut_off_at_its_deadline() {
    let backend = start(PROXY);
    // A caller that asks for a large answer and never reads it.
    let mut stream = connect_with(PROXY, backend.at, Some(4096));
    let asked = Instant::now();
    stream.write_all(get_request("/nexus/api/test/large", Some("https")).as_bytes()).unwrap();
    // The write guard itself is what lets it go: counted as such.
    wait_until("the stalled write cut off", || backend.shell.refused(Refusal::Stalled) == 1);
    assert!(asked.elapsed() >= WRITE_DEADLINE);
    wait_until("the connection let go", || backend.shell.open() == 0);
    // Far less than the answer reached the caller.
    assert!(everything(&mut stream).len() < LARGE);
}

// ---- what answers ------------------------------------------------------------------------

#[test]
fn a_fault_in_one_request_leaves_the_others_alone() {
    let backend = start(PROXY);
    let at = backend.at;
    let (head, body) = one_answer(&exchange(PROXY, at, get_request("/nexus/api/test/fault", Some("https")).as_bytes()));
    assert_eq!(head.status, 500);
    assert!(body.is_empty());
    assert_kept_by_no_one(&head);
    let askers: Vec<_> = (0..16).map(|_| thread::spawn(move || exchange(PROXY, at, hello_request().as_bytes()))).collect();
    for asker in askers {
        assert_is_the_backend(&asker.join().unwrap());
    }
    assert_eq!(backend.shell.refused(Refusal::Fault), 1);
}

#[test]
fn an_answer_too_slow_is_given_up_and_the_others_are_served_meanwhile() {
    let backend = start(PROXY);
    let at = backend.at;
    let slow = thread::spawn(move || {
        let started = Instant::now();
        let (head, _) = one_answer(&exchange(PROXY, at, get_request("/nexus/api/test/stall", Some("https")).as_bytes()));
        (head, started.elapsed())
    });
    wait_until("the slow answer begun", || backend.stalling.load(Ordering::SeqCst) == 1);
    let started = Instant::now();
    assert_is_the_backend(&exchange(PROXY, at, hello_request().as_bytes()));
    assert!(started.elapsed() < ANSWER_DEADLINE);
    let (head, took) = slow.join().unwrap();
    assert_eq!(head.status, 503);
    assert_kept_by_no_one(&head);
    assert!(took >= ANSWER_DEADLINE, "{took:?}");
}
