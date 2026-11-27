//! Experiment E6 (design 14.2): the backend's bounds, with no session, over real sockets
//! against the real listener, on loopback only. Each test starts its own backend.
//!
//! The proxy is played from 127.0.0.2 and a stranger from 127.0.0.3: both are this
//! machine's own loopback, and neither is 127.0.0.1, which may ask `hello` for the health
//! check. The bounds are the program's own (`bounds.rs`), not numbers made smaller for
//! testing; so some of these take as long as the bound they hold.

use std::io::{BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::header::{CACHE_CONTROL, SET_COOKIE};
use axum::routing::get;
use nexus_core::nexus::proto::{self, BackendId, Found};
use nexus_music_server::bounds::*;
use nexus_music_server::probe::{self, Head};
use nexus_music_server::serve::{self, Refusal, Shell};

const PROXY: &str = "127.0.0.2";
const STRANGER: &str = "127.0.0.3";
const HEALTH: &str = "127.0.0.1";
const ID: [u8; 16] = [0x5a; 16];
/// Time allowed beyond a bound for the machine to act on it.
const SLACK: Duration = Duration::from_secs(3);

struct Running {
    at: SocketAddr,
    shell: Arc<Shell>,
}

/// The real backend, with three routes more for the tests: one that faults, one that never
/// answers, one that tries to set a cookie and to be kept.
fn start(proxy: &str) -> Running {
    let hello = nexus_music_server::hello(BackendId::from_bytes(ID));
    let router = serve::router(&hello)
        .route("/nexus/api/test/fault", get(|| async { panic!("a fault in one request") }))
        .route(
            "/nexus/api/test/stall",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                "late"
            }),
        )
        .route("/nexus/api/test/cookie", get(|| async { ([(SET_COOKIE, "session=x"), (CACHE_CONTROL, "public, max-age=600")], "kept?") }));
    start_with(proxy, router)
}

fn start_with(proxy: &str, router: Router) -> Running {
    let proxy: IpAddr = proxy.parse().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let at = listener.local_addr().unwrap();
    let shell = Shell::new(proxy, router);
    let serving = shell.clone();
    thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            serve::serve(listener, serving, false).await;
        });
    });
    Running { at, shell }
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
    stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    stream.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
    stream
}

fn get_request(target: &str, mark: Option<&str>) -> String {
    let mark = mark.map(|value| format!("X-Nexus-Scheme: {value}\r\n")).unwrap_or_default();
    format!("GET {target} HTTP/1.1\r\nHost: music.example.org\r\n{mark}\r\n")
}

fn hello_request() -> String {
    get_request(proto::HELLO, Some("https"))
}

/// One answer read from `stream`, head and whole body; `None` if the connection ended (or was
/// reset) before a byte of an answer.
fn reply(stream: &TcpStream) -> Option<(Head, Vec<u8>)> {
    let mut from = BufReader::new(stream);
    let head = match probe::read_head(&mut from) {
        Ok(Some(head)) => head,
        Ok(None) => return None,
        Err(why) if why.kind() == std::io::ErrorKind::ConnectionReset => return None,
        Err(why) => panic!("an answer that could not be read: {why}"),
    };
    let length: usize = head.one("content-length").map_or(0, |length| length.parse().unwrap());
    let mut body = vec![0; length];
    from.read_exact(&mut body).unwrap();
    assert!(from.buffer().is_empty(), "more was sent than one answer");
    Some((head, body))
}

fn ask(from: &str, at: SocketAddr, request: &str) -> Option<(Head, Vec<u8>)> {
    let mut stream = connect(from, at);
    stream.write_all(request.as_bytes()).unwrap();
    reply(&stream)
}

/// Whether `stream` is closed with nothing on it: the end, or a reset, before any byte.
fn closed_with_nothing(stream: &mut TcpStream) -> bool {
    let mut byte = [0u8; 1];
    match stream.read(&mut byte) {
        Ok(0) => true,
        Ok(_) => false,
        Err(why) => why.kind() == std::io::ErrorKind::ConnectionReset,
    }
}

/// What every answer must carry, and must not.
fn assert_kept_by_no_one(head: &Head) {
    assert_eq!(head.one("cache-control"), Some("no-store"), "{head:?}");
    assert!(head.all("set-cookie").is_empty(), "{head:?}");
}

fn assert_is_the_backend(head: &Head, body: &[u8]) {
    assert_eq!(head.status, 200);
    assert_kept_by_no_one(head);
    match proto::classify(head.status, body) {
        Found::Backend(hello) => {
            assert_eq!(hello.id, BackendId::from_bytes(ID));
            assert_eq!(hello.speaks, proto::SPEAKS);
            assert!(hello.has().is_empty());
        }
        other => panic!("{other:?}"),
    }
}

fn wait_until(what: &str, deadline: Duration, mut done: impl FnMut() -> bool) -> Duration {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < deadline, "{what}: not within {deadline:?}");
        thread::sleep(Duration::from_millis(20));
    }
    start.elapsed()
}

// ---- who is answered -------------------------------------------------------------------

#[test]
fn the_proxy_marked_https_is_answered_hello() {
    let backend = start(PROXY);
    let (head, body) = ask(PROXY, backend.at, &hello_request()).unwrap();
    assert_is_the_backend(&head, &body);
    assert_eq!(head.one("content-type"), Some("application/json"));
    // And twice on one connection.
    let mut stream = connect(PROXY, backend.at);
    for _ in 0..2 {
        stream.write_all(hello_request().as_bytes()).unwrap();
        let (head, body) = reply(&stream).unwrap();
        assert_is_the_backend(&head, &body);
    }
}

#[test]
fn a_stranger_is_answered_nothing() {
    let backend = start(PROXY);
    let mut stream = connect(STRANGER, backend.at);
    // It may be closed before this is even sent.
    let _ = stream.write_all(hello_request().as_bytes());
    assert!(closed_with_nothing(&mut stream));
    wait_until("the refusal counted", SLACK, || backend.shell.refused(Refusal::Stranger) == 1);
}

#[test]
fn the_proxy_unmarked_is_answered_nothing() {
    let backend = start(PROXY);
    let marks: &[&[&str]] = &[&[], &["http"], &["HTTPS"], &["https", "https"], &["http", "https"], &["https, http"]];
    for marks in marks {
        let mut stream = connect(PROXY, backend.at);
        let headers: String = marks.iter().map(|mark| format!("X-Nexus-Scheme: {mark}\r\n")).collect();
        let request = format!("GET {} HTTP/1.1\r\nHost: music.example.org\r\n{headers}\r\n", proto::HELLO);
        stream.write_all(request.as_bytes()).unwrap();
        assert!(closed_with_nothing(&mut stream), "{marks:?}");
    }
    // A first request marked, a second not: the second is not answered.
    let mut stream = connect(PROXY, backend.at);
    stream.write_all(hello_request().as_bytes()).unwrap();
    assert_eq!(reply(&stream).unwrap().0.status, 200);
    stream.write_all(get_request(proto::HELLO, None).as_bytes()).unwrap();
    assert!(closed_with_nothing(&mut stream));
    assert_eq!(backend.shell.refused(Refusal::NotMarked), marks.len() as u64 + 1);
}

#[test]
fn this_machine_may_ask_hello_and_nothing_else() {
    // A proxy elsewhere (a made-up address of a documentation range).
    let backend = start("192.0.2.10");
    let (head, body) = ask(HEALTH, backend.at, &get_request(proto::HELLO, None)).unwrap();
    assert_is_the_backend(&head, &body);
    for request in [
        get_request("/nexus/api/test/cookie", None),
        get_request("/nexus/api/nothing", None),
        get_request("/nexus/api/hello/", None),
        format!("POST {} HTTP/1.1\r\nHost: x\r\nContent-Length: 0\r\n\r\n", proto::HELLO),
        format!("HEAD {} HTTP/1.1\r\nHost: x\r\n\r\n", proto::HELLO),
    ] {
        let mut stream = connect(HEALTH, backend.at);
        stream.write_all(request.as_bytes()).unwrap();
        assert!(closed_with_nothing(&mut stream), "{request}");
    }
    // Any other loopback address is a stranger there.
    let mut stream = connect(PROXY, backend.at);
    let _ = stream.write_all(hello_request().as_bytes());
    assert!(closed_with_nothing(&mut stream));
}

#[test]
fn a_proxy_on_loopback_needs_its_mark_there_too() {
    let backend = start(HEALTH);
    let mut stream = connect(HEALTH, backend.at);
    stream.write_all(get_request(proto::HELLO, None).as_bytes()).unwrap();
    assert!(closed_with_nothing(&mut stream));
    let (head, body) = ask(HEALTH, backend.at, &hello_request()).unwrap();
    assert_is_the_backend(&head, &body);
}

// ---- the address -------------------------------------------------------------------------

#[test]
fn an_address_not_in_plain_form_is_refused_unread() {
    let backend = start(PROXY);
    // Every target of E2's table that reached the stand-in untidied and is not plain, and the
    // others the experiments tried, as a caller that reaches the backend may send them.
    let targets = [
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
        "*",
    ];
    for target in targets {
        // The head says a large body follows, and none is sent: the answer comes at once all
        // the same, so nothing of the body was waited for.
        let request = format!("POST {target} HTTP/1.1\r\nHost: music.example.org\r\nX-Nexus-Scheme: https\r\nContent-Length: 1000000\r\n\r\n");
        let started = Instant::now();
        let (head, body) = ask(PROXY, backend.at, &request).unwrap_or_else(|| panic!("{target}: no answer"));
        assert_eq!(head.status, 400, "{target}");
        assert!(body.is_empty(), "{target}");
        assert_kept_by_no_one(&head);
        assert_eq!(head.one("connection"), Some("close"), "{target}");
        assert!(started.elapsed() < SLACK, "{target}: {:?}", started.elapsed());
    }
    assert_eq!(backend.shell.refused(Refusal::NotPlain), targets.len() as u64);
    // A backslash is refused too, by hyper before the backend sees it or by the backend.
    let (head, _) = ask(PROXY, backend.at, "GET /nexus\\..\\rest\\ping HTTP/1.1\r\nHost: x\r\nX-Nexus-Scheme: https\r\n\r\n").unwrap();
    assert_eq!(head.status, 400);
}

#[test]
fn a_path_in_plain_form_that_is_not_the_backends_is_not_found() {
    let backend = start(PROXY);
    for target in ["/nexus/", "/nexus/api/", "/nexus/api/nothing", "/nexus/api/hello/"] {
        let (head, body) = ask(PROXY, backend.at, &get_request(target, Some("https"))).unwrap();
        assert_eq!(head.status, 404, "{target}");
        assert!(body.is_empty());
        assert_kept_by_no_one(&head);
    }
    let (head, _) = ask(PROXY, backend.at, &format!("DELETE {} HTTP/1.1\r\nHost: x\r\nX-Nexus-Scheme: https\r\n\r\n", proto::HELLO)).unwrap();
    assert_eq!(head.status, 405);
}

#[test]
fn no_answer_sets_a_cookie_or_may_be_kept() {
    let backend = start(PROXY);
    let (head, body) = ask(PROXY, backend.at, &get_request("/nexus/api/test/cookie", Some("https"))).unwrap();
    assert_eq!((head.status, &body[..]), (200, &b"kept?"[..]));
    assert_kept_by_no_one(&head);
    assert_eq!(head.all("cache-control").len(), 1);
}

// ---- the head ----------------------------------------------------------------------------

#[test]
fn a_slow_head_is_cut_off_at_its_deadline() {
    let backend = start(PROXY);
    let mut stream = connect(PROXY, backend.at);
    let started = Instant::now();
    stream.write_all(format!("GET {} HTTP/1.1\r\nHost: x\r\n", proto::HELLO).as_bytes()).unwrap();
    // A header every half second, never the end of the head.
    let mut dripping = stream.try_clone().unwrap();
    thread::spawn(move || {
        for n in 0..60 {
            thread::sleep(Duration::from_millis(500));
            if dripping.write_all(format!("X-Drip-{n}: x\r\n").as_bytes()).is_err() {
                break;
            }
        }
    });
    let answer = reply(&stream);
    let took = started.elapsed();
    assert!(answer.as_ref().is_none_or(|(head, _)| head.status == 408), "{answer:?}");
    assert!(took >= HEAD_DEADLINE - Duration::from_millis(500) && took < HEAD_DEADLINE + SLACK, "{took:?}");
    wait_until("the connection let go", SLACK, || backend.shell.open() == 0);
}

#[test]
fn an_idle_connection_is_closed_at_the_heads_deadline() {
    let backend = start(PROXY);
    let mut stream = connect(PROXY, backend.at);
    stream.write_all(hello_request().as_bytes()).unwrap();
    assert_eq!(reply(&stream).unwrap().0.status, 200);
    let started = Instant::now();
    assert!(closed_with_nothing(&mut stream));
    let took = started.elapsed();
    assert!(took >= HEAD_DEADLINE - Duration::from_millis(500) && took < HEAD_DEADLINE + SLACK, "{took:?}");
}

#[test]
fn a_head_too_large_or_with_too_many_headers_is_refused() {
    let backend = start(PROXY);
    let large = format!("GET {} HTTP/1.1\r\nHost: x\r\nX-Nexus-Scheme: https\r\nX-Large: {}\r\n\r\n", proto::HELLO, "a".repeat(HEAD_MAX_BYTES));
    let many: String = (0..HEAD_MAX_HEADERS + 1).map(|n| format!("X-N{n}: x\r\n")).collect();
    let many = format!("GET {} HTTP/1.1\r\nHost: x\r\nX-Nexus-Scheme: https\r\n{many}\r\n", proto::HELLO);
    for request in [large, many] {
        let mut stream = connect(PROXY, backend.at);
        let _ = stream.write_all(request.as_bytes());
        match reply(&stream) {
            None => {}
            Some((head, _)) => assert!(head.status == 431 || head.status == 400, "{head:?}"),
        }
    }
    // Just inside both, it is answered.
    let fits: String = (0..HEAD_MAX_HEADERS - 3).map(|n| format!("X-N{n}: x\r\n")).collect();
    let fits = format!("GET {} HTTP/1.1\r\nHost: x\r\nX-Nexus-Scheme: https\r\n{fits}\r\n", proto::HELLO);
    assert_eq!(ask(PROXY, backend.at, &fits).unwrap().0.status, 200);
}

// ---- the body ----------------------------------------------------------------------------

fn post_head(target: &str, length: usize) -> String {
    format!("POST {target} HTTP/1.1\r\nHost: x\r\nX-Nexus-Scheme: https\r\nContent-Length: {length}\r\n\r\n")
}

#[test]
fn a_slow_body_is_cut_off_at_its_deadline() {
    let backend = start(PROXY);
    let mut stream = connect(PROXY, backend.at);
    stream.write_all(post_head(proto::HELLO, 1000).as_bytes()).unwrap();
    let started = Instant::now();
    let mut dripping = stream.try_clone().unwrap();
    thread::spawn(move || {
        for _ in 0..30 {
            thread::sleep(Duration::from_millis(500));
            if dripping.write_all(b"0123456789").is_err() {
                break;
            }
        }
    });
    let (head, _) = reply(&stream).unwrap();
    let took = started.elapsed();
    assert_eq!(head.status, 408);
    assert_kept_by_no_one(&head);
    assert!(took >= BODY_DEADLINE - Duration::from_millis(500) && took < BODY_DEADLINE + SLACK, "{took:?}");
    assert!(closed_with_nothing(&mut stream));
    assert_eq!(backend.shell.refused(Refusal::SlowBody), 1);
}

#[test]
fn a_body_over_its_bound_is_refused() {
    let backend = start(PROXY);
    // Said to be one byte over: refused at once, before a byte of it is sent.
    let started = Instant::now();
    let (head, _) = ask(PROXY, backend.at, &post_head(proto::HELLO, BODY_MAX_BYTES + 1)).unwrap();
    assert_eq!(head.status, 413);
    assert_kept_by_no_one(&head);
    assert!(started.elapsed() < SLACK);
    // Sent in chunks, saying nothing of its size: refused at the first byte past the bound.
    let mut stream = connect(PROXY, backend.at);
    stream
        .write_all(format!("POST {} HTTP/1.1\r\nHost: x\r\nX-Nexus-Scheme: https\r\nTransfer-Encoding: chunked\r\n\r\n", proto::HELLO).as_bytes())
        .unwrap();
    let mut sending = stream.try_clone().unwrap();
    let sender = thread::spawn(move || {
        let chunk = vec![b'a'; 64 * 1024];
        let mut sent = 0;
        // Far more than the kernel's buffers on both sides can hold, so that the backend taking
        // it all would show.
        while sent < 64 * BODY_MAX_BYTES {
            let framed = [format!("{:x}\r\n", chunk.len()).into_bytes(), chunk.clone(), b"\r\n".to_vec()].concat();
            if sending.write_all(&framed).is_err() {
                break;
            }
            sent += chunk.len();
        }
        sent
    });
    let (head, _) = reply(&stream).unwrap();
    assert_eq!(head.status, 413);
    assert!(closed_with_nothing(&mut stream.try_clone().unwrap()));
    let sent = sender.join().unwrap();
    assert!(sent < 64 * BODY_MAX_BYTES, "the whole body was taken: {sent}");
    drop(stream);
    assert_eq!(backend.shell.refused(Refusal::TooLarge), 2);
    // At the bound exactly, the body is taken whole, and `hello` says it answers GET alone.
    let mut stream = connect(PROXY, backend.at);
    stream.write_all(post_head(proto::HELLO, BODY_MAX_BYTES).as_bytes()).unwrap();
    stream.write_all(&vec![b'a'; BODY_MAX_BYTES]).unwrap();
    assert_eq!(reply(&stream).unwrap().0.status, 405);
}

// ---- connections -------------------------------------------------------------------------

#[test]
fn one_connection_more_than_the_cap_is_closed_and_the_rest_are_served() {
    let backend = start(PROXY);
    let held: Vec<TcpStream> = (0..CONNECTIONS_MAX).map(|_| connect(PROXY, backend.at)).collect();
    wait_until("every connection taken", SLACK, || backend.shell.open() == CONNECTIONS_MAX);
    let mut more = connect(PROXY, backend.at);
    let _ = more.write_all(hello_request().as_bytes());
    assert!(closed_with_nothing(&mut more));
    assert_eq!(backend.shell.refused(Refusal::Full), 1);
    // Those already open are served.
    let mut first = held[0].try_clone().unwrap();
    first.write_all(hello_request().as_bytes()).unwrap();
    assert_eq!(reply(&first).unwrap().0.status, 200);
    // One let go, and the next is taken.
    drop(first);
    let mut held = held;
    drop(held.remove(0));
    wait_until("one let go", SLACK, || backend.shell.open() == CONNECTIONS_MAX - 1);
    let (head, body) = ask(PROXY, backend.at, &hello_request()).unwrap();
    assert_is_the_backend(&head, &body);
}

#[test]
fn a_write_that_makes_no_progress_is_cut_off_at_its_deadline() {
    let backend = start(PROXY);
    // A caller that asks and asks, and never reads an answer.
    let stream = connect_with(PROXY, backend.at, Some(4096));
    let mut asking = stream.try_clone().unwrap();
    asking.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
    let many = hello_request().repeat(1000);
    let blocked = thread::spawn(move || {
        while asking.write_all(many.as_bytes()).is_ok() {}
        Instant::now()
    })
    .join()
    .unwrap();
    // The caller's writes stopped because the backend's did: it is still open, waiting.
    assert_eq!(backend.shell.open(), 1);
    let took = wait_until("the stalled connection let go", WRITE_DEADLINE + SLACK, || backend.shell.open() == 0);
    // Its own deadline, and not the head's.
    assert!(blocked.elapsed() < HEAD_DEADLINE, "{took:?}");
    drop(stream);
}

// ---- what answers ------------------------------------------------------------------------

#[test]
fn a_fault_in_one_request_leaves_the_others_alone() {
    let backend = start(PROXY);
    let mut kept = connect(PROXY, backend.at);
    kept.write_all(hello_request().as_bytes()).unwrap();
    assert_eq!(reply(&kept).unwrap().0.status, 200);
    let (head, body) = ask(PROXY, backend.at, &get_request("/nexus/api/test/fault", Some("https"))).unwrap();
    assert_eq!(head.status, 500);
    assert!(body.is_empty());
    assert_kept_by_no_one(&head);
    // The connection open beside it, a new one, and many at once after it.
    kept.write_all(hello_request().as_bytes()).unwrap();
    assert_eq!(reply(&kept).unwrap().0.status, 200);
    let at = backend.at;
    let askers: Vec<_> = (0..16).map(|_| thread::spawn(move || ask(PROXY, at, &hello_request()).unwrap().0.status)).collect();
    for asker in askers {
        assert_eq!(asker.join().unwrap(), 200);
    }
    assert_eq!(backend.shell.refused(Refusal::Fault), 1);
}

#[test]
fn an_answer_too_slow_is_given_up_and_the_others_are_served() {
    let backend = start(PROXY);
    let at = backend.at;
    let slow = thread::spawn(move || {
        let started = Instant::now();
        let (head, _) = ask(PROXY, at, &get_request("/nexus/api/test/stall", Some("https"))).unwrap();
        (head.status, started.elapsed())
    });
    thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    let (head, body) = ask(PROXY, at, &hello_request()).unwrap();
    assert_is_the_backend(&head, &body);
    assert!(started.elapsed() < SLACK);
    let (status, took) = slow.join().unwrap();
    assert_eq!(status, 503);
    assert!(took >= ANSWER_DEADLINE - Duration::from_millis(500) && took < ANSWER_DEADLINE + SLACK, "{took:?}");
}
