use super::*;

use std::io::{BufRead, BufReader, Read as _, Write};
use std::net::{SocketAddr, TcpListener, TcpStream as Socket};
use std::thread;

use nexus_core::http::Http;
use serde_json::{Value, json};

pub(super) const TEST_KEY: &str = "standinkeystandinkeystandinkey12";
const ARTIST: &str = "10000000-0000-0000-0000-000000000001";
const ALBUM: &str = "10000000-0000-0000-0000-000000000101";

struct Question {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn question(socket: &mut Socket) -> Option<Question> {
    socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut reader = BufReader::new(socket);
    let mut line = String::new();
    if reader.read_line(&mut line).ok()? == 0 {
        return None;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let mut headers = HashMap::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        headers.insert(name.to_ascii_lowercase(), value.trim().to_owned());
    }
    let length = headers.get("content-length").map(|len| len.parse().unwrap()).unwrap_or_default();
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    Some(Question { method, target, headers, body })
}

fn response(socket: &mut Socket, status: &str, headers: &str, body: &[u8]) -> std::io::Result<()> {
    write!(socket, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n", body.len())?;
    socket.write_all(body)
}

fn once(reply: impl FnOnce(&mut Socket, Question) + Send + 'static) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let running = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let ask = question(&mut socket).expect("the transport sent no request");
        assert!(ask.headers.get("x-api-key").is_some_and(|key| key == TEST_KEY));
        reply(&mut socket, ask);
    });
    (address, running)
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()
}

fn endpoint(address: SocketAddr) -> Endpoint {
    Endpoint::new(admin::InternalAddress::new(&format!("http://{address}")).unwrap()).unwrap()
}

#[test]
fn the_fixed_line_will_not_follow_a_redirect_or_dial_a_different_origin() {
    let trap = TcpListener::bind("127.0.0.1:0").unwrap();
    trap.set_nonblocking(true).unwrap();
    let trap_url = format!("http://{}/api/v1/trap", trap.local_addr().unwrap());
    let location = trap_url.clone();
    let (address, upstream) = once(move |socket, request| {
        assert_eq!((request.method.as_str(), request.target.as_str()), ("GET", "/api/v1/artist"));
        assert!(request.body.is_empty());
        response(socket, "302 Found", &format!("Location: {location}\r\n"), TEST_KEY.as_bytes()).unwrap();
    });
    let runtime = runtime();
    let endpoint = endpoint(address);
    let line = Line { endpoint: endpoint.clone(), runtime: runtime.handle().clone(), deadline: None,
        within: UPSTREAM_TIMEOUT, before_write: None };
    assert!(line.send(&http::Request::get(&trap_url)).is_err());
    let lidarr = endpoint.lidarr(runtime.handle().clone(), TEST_KEY, None);
    let error = lidarr.artist_numbers().unwrap_err();
    assert_eq!(wire::refusal_for(&error, None), Refusal::LidarrBehindSignIn);
    assert!(!error.contains(TEST_KEY));
    assert_eq!(trap.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
    upstream.join().unwrap();
}

#[test]
fn the_line_refuses_down_slow_garbage_and_one_byte_over_the_ceiling() {
    let runtime = runtime();
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let down = server.local_addr().unwrap();
    drop(server);
    let ask = |addr| endpoint(addr).lidarr(runtime.handle().clone(), TEST_KEY, None).artist_numbers();
    let reason = |result: Result<HashMap<String, i64>, String>| wire::refusal_for(&result.unwrap_err(), None);
    assert_eq!(reason(ask(down)), Refusal::LidarrUnavailable);

    let (address, upstream) = once(|socket, _| {
        thread::sleep(Duration::from_millis(180));
        let _ = response(socket, "200 OK", "", b"[]");
    });
    let arrived = Instant::now();
    let lidarr = endpoint(address).lidarr(runtime.handle().clone(), TEST_KEY, Some(arrived + Duration::from_millis(100)));
    assert_eq!(reason(lidarr.artist_numbers()), Refusal::LidarrUnavailable);
    assert!(arrived.elapsed() < Duration::from_secs(1));
    upstream.join().unwrap();

    let (address, upstream) = once(|socket, _| {
        response(socket, "200 OK", "", format!("not JSON {TEST_KEY}").as_bytes()).unwrap();
    });
    assert_eq!(reason(ask(address)), Refusal::LidarrUnrecognized);
    upstream.join().unwrap();

    let (address, upstream) = once(|socket, _| {
        let mut body = vec![b' '; http::BODY_LIMIT as usize + 1];
        body[..2].copy_from_slice(b"[]");
        let _ = response(socket, "200 OK", "", &body);
    });
    assert_eq!(reason(ask(address)), Refusal::LidarrUnavailable);
    upstream.join().unwrap();
}

fn album(monitored: bool) -> Value {
    json!({
        "id":9,"title":"Glass Orchard","foreignAlbumId":ALBUM,"albumType":"Album","secondaryTypes":[],
        "releaseDate":"2019-04-12T00:00:00Z","monitored":monitored,"artistId":5,
        "remoteCover":"https://covers.invalid/glass-orchard.jpg","ratings":{"votes":120,"value":8.1},
        "statistics":{"trackFileCount":0,"trackCount":10},"artist":{"id":5,"artistName":"Quillon Vane","foreignArtistId":ARTIST}
    })
}

#[test]
fn one_invalid_search_row_does_not_hide_its_neighbours() {
    let runtime = runtime();
    let (address, upstream) = once(|socket, request| {
        assert_eq!(request.target, "/api/v1/album/lookup?term=Glass%20Orchard");
        let first = album(false);
        let mut bad = first.clone();
        bad["releaseDate"] = json!("bad-date");
        let mut last = first.clone();
        last["title"] = json!("Paper Tides");
        last["foreignAlbumId"] = json!("10000000-0000-0000-0000-000000000102");
        response(socket, "200 OK", "", json!([first, bad, last]).to_string().as_bytes()).unwrap();
    });
    let lidarr = endpoint(address).lidarr(runtime.handle().clone(), TEST_KEY, None);
    let read = Read::FindAny(Search::new("Glass Orchard").unwrap());
    let Answer::FoundAny { albums, .. } = read.ask(&lidarr).unwrap() else { panic!("wrong read answer") };
    assert_eq!(albums.iter().map(|album| album.title.as_str()).collect::<Vec<_>>(), ["Glass Orchard", "Paper Tides"]);
    upstream.join().unwrap();
}

#[test]
fn snapshot_cursor_must_match_session_and_exact_read() {
    let mut books = Books::default();
    let session = SessionId::new(1).unwrap();
    let another = SessionId::new(2).unwrap();
    let read = Read::FindAny(Search::new("Glass Orchard").unwrap());
    let other = Read::FindAny(Search::new("Paper Tides").unwrap());
    let album = AlbumView::try_from(Offer {
        album: ALBUM.into(), artist_id: ARTIST.into(), title: "Glass Orchard".into(), ..Offer::default()
    }).unwrap();
    let found = Answer::FoundAny { albums: vec![album; wire::PAGE_MAX_ITEMS + 1], next: None };
    let Answer::FoundAny { albums, next: Some(next) } = books.first(AccountId::new(1).unwrap(), session, &read, found).unwrap() else {
        panic!("a continuation is required");
    };
    assert_eq!(albums.len(), wire::PAGE_MAX_ITEMS);
    assert_eq!(books.peek(another, &read, next).err(), Some(Refusal::SearchAgain));
    assert_eq!(books.peek(session, &other, next).err(), Some(Refusal::SearchAgain));
    let Answer::FoundAny { albums, next: None } = books.peek(session, &read, next).unwrap() else {
        panic!("expected the last page");
    };
    assert_eq!(albums.len(), 1);
}

#[test]
fn an_account_gets_only_one_upstream_place() {
    let rooms = Arc::<Mutex<Rooms>>::default();
    let first = Place::upstream(rooms.clone(), 1).unwrap();
    assert!(Place::upstream(rooms.clone(), 1).is_none());
    let mut others: Vec<_> = (2..=wire::RUNNING_MAX as u64)
        .map(|account| Place::upstream(rooms.clone(), account).unwrap()).collect();
    assert!(Place::upstream(rooms.clone(), 10).is_none());
    drop(first);
    others.push(Place::upstream(rooms, 10).unwrap());
}

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Condvar, atomic::AtomicU64};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::accounts::{
    admit::Admission,
    check::PasswordWorkers,
    desk::Desk,
    music::MusicSide,
    setup::{CodeKey, SetUpCode, dummy_salt_key, make_code},
};
use crate::probe;
use crate::serve::{self, Shell};
use crate::store::keeper::First;
use account::{
    Credential as SignInCredential, DeviceKind, DeviceSecret, GrantIssued, Name, OneTimeCode,
    Password, SignIn, SignedIn,
};
use nexus_core::nexus::proto::{SPEAKS, Version};

const TEST_ORIGIN: &str = "https://nexus.example.org";
const TEST_PAGE: &str = "Origin: https://nexus.example.org\r\nX-Nexus-Admin: 1\r\n";
const PASSWORD: &str = "a long password for the Requests walk";

#[derive(Default)]
struct LidarrState {
    monitored: bool,
    arrived: bool,
    ambiguous_pending_number: bool,
    new_releases: Option<String>,
    calls: Vec<String>,
}

type HeldResponse = (Mutex<Option<(&'static str, &'static str)>>, Condvar);

struct MockLidarr {
    address: SocketAddr,
    state: Arc<Mutex<LidarrState>>,
    stop: Arc<AtomicBool>,
    hold: Arc<HeldResponse>,
    thread: Option<thread::JoinHandle<()>>,
}

impl MockLidarr {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let state = Arc::<Mutex<LidarrState>>::default();
        let available = state.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let hold: Arc<HeldResponse> = Arc::default();
        let blocked = hold.clone();
        let thread = thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        let Some(incoming) = question(&mut socket) else { continue };
                        assert!(incoming.headers.get("x-api-key").is_some_and(|key| key == TEST_KEY));
                        let body = mock(&incoming, &mut lock(&available));
                        let stop = lock(&blocked.0);
                        drop(blocked.1.wait_while(stop, |held| {
                            *held == Some((incoming.method.as_str(), incoming.target.as_str()))
                        }).unwrap());
                        let _ = response(&mut socket, "200 OK", "Content-Type: application/json\r\n", body.to_string().as_bytes());
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(2)),
                    Err(err) => panic!("stand-in accept failed: {err}"),
                }
            }
        });
        Self { address, state, stop, hold, thread: Some(thread) }
    }

    fn hold_response(&self, exchange: Option<(&'static str, &'static str)>) {
        *lock(&self.hold.0) = exchange;
        self.hold.1.notify_all();
    }

    fn wait_for_call(&self, expected: &str) {
        let until = Instant::now() + Duration::from_secs(8);
        while !lock(&self.state).calls.iter().any(|call| call == expected) {
            assert!(Instant::now() < until, "Lidarr did not receive {expected}");
            thread::sleep(Duration::from_millis(15));
        }
    }

    fn take_calls(&self) -> Vec<String> {
        std::mem::take(&mut lock(&self.state).calls)
    }
}

impl Drop for MockLidarr {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.hold_response(None);
        let finished = self.thread.take().unwrap().join();
        if !thread::panicking() {
            finished.unwrap();
        }
    }
}

fn mock(request: &Question, state: &mut LidarrState) -> Value {
    let target = request.target.strip_prefix("/api/v1/").unwrap();
    state.calls.push(format!("{} {target}", request.method));
    let artist = json!({"id":5,"monitored":true,"artistName":"Quillon Vane","foreignArtistId":ARTIST,
        "monitorNewItems":state.new_releases.as_deref().unwrap_or("all")});
    match (request.method.as_str(), target) {
        ("GET", "system/status") => json!({"version":"2.0.0"}),
        ("GET", "album/lookup?term=Glass%20Orchard" | "album/lookup?term=Glass%20Orchard%20Quillon%20Vane") =>
            json!([album(state.monitored)]),
        ("GET", lookup) if lookup == format!("album/lookup?term=lidarr%3A{ALBUM}") =>
            json!([album(state.monitored)]),
        ("GET", "artist") => json!([artist]),
        ("GET", "artist/5") => artist,
        ("GET", "command") => json!([]),
        ("GET", "album?artistId=5") => json!([album(state.monitored)]),
        ("GET", "album/9") => album(state.monitored),
        ("GET", "wanted/missing?includeArtist=true&page=1&pageSize=500") => {
            let mut found: Vec<_> = (state.monitored && !state.arrived).then(|| album(true)).into_iter().collect();
            if state.ambiguous_pending_number {
                let mut competing = album(true);
                competing["foreignAlbumId"] = json!("10000000-0000-0000-0000-000000000999");
                found.push(competing);
            }
            json!({"records":found,"totalRecords":found.len()})
        }
        ("GET", "queue?page=1&pageSize=500") => json!({"records":[{"albumId":9,"size":100,"sizeleft":27}],"totalRecords":1}),
        ("PUT", "artist/5") => {
            let input: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(input["id"], 5);
            state.new_releases = Some(input["monitorNewItems"].as_str().unwrap().to_owned());
            json!({})
        }
        ("PUT", "album/monitor") => {
            let input: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(input["albumIds"], json!([9]));
            state.monitored = input["monitored"].as_bool().unwrap();
            json!({})
        }
        ("POST", "command") => {
            let input: Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(input, json!({"name":"AlbumSearch","albumIds":[9]}));
            json!({"id":100})
        }
        _ => panic!("unexpected stand-in call {} {target}", request.method),
    }
}

// The production socket listener, the one writer, and real signed-in sessions.
struct TestServer {
    root: PathBuf,
    address: SocketAddr,
    accounts: Option<Arc<Accounts>>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
    child: Option<Child>,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
}

impl TestServer {
    fn start(name: &str, endpoint: Option<Endpoint>) -> (Self, [OneTimeCode; 3]) {
        Self::start_with_clock(name, endpoint, Arc::new(now))
    }

    fn start_with_clock(
        name: &str, endpoint: Option<Endpoint>, clock: crate::accounts::session::Clock,
    ) -> (Self, [OneTimeCode; 3]) {
        let (root, keeper, codes) = fixture(name);
        let accounts = fixture_accounts(keeper, clock, endpoint.map(|endpoint| endpoint.address));
        let id = accounts.backend;
        let server = accounts.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let (ready, address) = std::sync::mpsc::channel();
        let thread = thread::spawn(move || runtime().block_on(async move {
            let listener = serve::listen("127.0.0.1:0".parse().unwrap()).unwrap();
            let address = listener.local_addr().unwrap();
            let shell = Shell::backend(address.ip(), &crate::hello(id), server,
                Some(TEST_ORIGIN), crate::settings::DATA_BUDGET_DEFAULT);
            ready.send(address).unwrap();
            let serving = tokio::spawn(serve::serve(listener, shell, false));
            let _ = stopped.await;
            serving.abort();
            let _ = serving.await;
        }));
        let address = address.recv_timeout(Duration::from_secs(3)).unwrap();
        (Self { root, address, accounts: Some(accounts), stop: Some(stop), thread: Some(thread), child: None }, codes)
    }

    fn start_child(name: &str, endpoint: SocketAddr) -> (Self, [OneTimeCode; 3]) {
        let (root, keeper, codes) = fixture(name);
        keeper.close();
        (Self::launch_saved(root, endpoint), codes)
    }

    fn launch_saved(root: PathBuf, endpoint: SocketAddr) -> Self {
        let address = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        let mut server = Self { root, address, accounts: None, stop: None, thread: None, child: None };
        server.launch(endpoint);
        server
    }

    fn launch(&mut self, lidarr: SocketAddr) {
        self.child = Some(Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "requests::tests::serve_in_a_child_for_the_restart_case"])
            .env("NEXUS_REQUESTS_TEST_DATA", &self.root)
            .env("NEXUS_REQUESTS_TEST_LISTEN", self.address.to_string())
            .env("NEXUS_REQUESTS_TEST_LIDARR", lidarr.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn().unwrap());
        let until = Instant::now() + Duration::from_secs(5);
        while Socket::connect(self.address).is_err() {
            assert!(Instant::now() < until, "the loopback fixture did not start");
            assert!(self.child.as_mut().unwrap().try_wait().unwrap().is_none(), "the loopback fixture exited");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn kill_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }

    fn socket(&self, path: &str, who: Option<&SignedIn>, headers: &str, length: usize) -> Socket {
        let mut socket = Socket::connect(self.address).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(9))).unwrap();
        write!(socket, "POST {path} HTTP/1.1\r\nHost: nexus\r\nX-Nexus-Scheme: https\r\nContent-Type: application/json\r\nContent-Length: {length}\r\n").unwrap();
        if let Some(who) = who {
            write!(socket, "Authorization: {}\r\n", who.session.authorization().as_str()).unwrap();
        }
        write!(socket, "{headers}Connection: close\r\n\r\n").unwrap();
        socket
    }

    fn post(&self, path: &str, who: Option<&SignedIn>, headers: &str, body: &[u8]) -> (u16, Vec<u8>) {
        let mut socket = self.socket(path, who, headers, body.len());
        socket.write_all(body).unwrap();
        let answer = read_http(socket);
        assert!(!answer.1.windows(TEST_KEY.len()).any(|window| window == TEST_KEY.as_bytes()));
        assert!(!answer.1.windows(self.address.to_string().len()).any(|window| window == self.address.to_string().as_bytes()));
        answer
    }

    fn redeem(&self, code: OneTimeCode, device: u8, headers: &str) -> Person {
        let credential = SignInCredential::Code { code, new_password: Password::new(PASSWORD).unwrap() };
        self.sign_in(credential, device, headers)
    }

    fn sign_in(&self, credential: SignInCredential, device: u8, headers: &str) -> Person {
        let body = account::encode(&SignIn {
            credential,
            device_secret: DeviceSecret::from_bytes([device; 32]),
            device_name: Name::new("Requests test").unwrap(),
            device_kind: DeviceKind::Pc,
            app_version: Version::parse("0.41.0").unwrap(),
            speaks: SPEAKS,
            restore: None,
        }).unwrap();
        let (status, body) = self.post(account::SIGN_IN, None, headers, body.bytes());
        match account::read_reply::<SignedIn>(status, &body).unwrap() {
            account::Reply::Done { result, receipt } => Person { signed_in: result, restore: receipt.stamp.restore, headers: headers.to_owned() },
            refusal => panic!("sign-in failed: {refusal:?}"),
        }
    }

    fn admin_app(&self, page: &Person) -> Person {
        self.sign_in(SignInCredential::Password {
            email: page.signed_in.person.email.clone(),
            password: Password::presented(PASSWORD).unwrap(),
        }, 5, "")
    }

    fn grant(&self, who: &Person, headers: &str, action: admin::Action, number: u64) -> admin::Reply<GrantIssued> {
        let request = admin::encode(&admin::RequestGrant {
            head: RequestHead { speaks: SPEAKS, restore: who.restore },
            change: admin::NamedChange { request_id: wire::RequestId::new(number).unwrap(), action },
            password: Password::presented(PASSWORD).unwrap(),
        }).unwrap();
        let (status, body) = self.post(admin::REQUEST_GRANT, Some(&who.signed_in), headers, request.bytes());
        admin::read_reply(status, &body).unwrap()
    }

    fn call(&self, who: &Person, call: Call) -> Answer {
        let body = body(who.restore, call);
        let (status, reply) = self.post(wire::REQUESTS, Some(&who.signed_in), &who.headers, &body);
        match wire::read_reply(status, &reply).unwrap() {
            account::Reply::Done { result, .. } => result,
            refusal => panic!("requests not answered: {refusal:?}"),
        }
    }

    fn offer(&self, who: &Person) -> wire::OfferName {
        let answer = self.call(who, Call::FindAny { search: Search::new("Glass Orchard").unwrap(), after: None });
        let Answer::FoundAny { albums, next: None } = answer else { panic!("offer search failed") };
        assert_eq!(albums.len(), 1);
        albums[0].album.clone()
    }

    fn done(&self, who: &Person, job: JobId) -> Completed {
        self.outcome(who, job).expect("job failed")
    }

    fn outcome(&self, who: &Person, job: JobId) -> Result<Completed, Refusal> {
        let start = Instant::now();
        loop {
            match self.call(who, Call::After { job }) {
                Answer::Done { result } => return Ok(result),
                Answer::Working => {}
                Answer::Failed { reason } => return Err(reason),
                _ => panic!("unexpected job answer"),
            }
            if start.elapsed() >= Duration::from_secs(8) {
                let db = Connection::open(self.root.join("data/nexus.db")).unwrap();
                let saved = db.query_row("SELECT stage, result FROM requests_jobs WHERE id = ?1",
                    [row_id(job).unwrap()], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))).unwrap();
                panic!("job did not finish: {saved:?}");
            }
            thread::sleep(Duration::from_millis(40));
        }
    }

    fn apply<T: Serialize>(
        &self, who: &Person, headers: &str, path: &str, change: T, number: u64,
        grant: admin::GrantSecret,
    ) -> admin::Reply<admin::Applied> {
        let request = serde_json::to_vec(&admin::Granted {
            head: RequestHead { speaks: SPEAKS, restore: who.restore },
            request_id: wire::RequestId::new(number).unwrap(),
            grant,
            change,
        }).unwrap();
        let (status, body) = self.post(path, Some(&who.signed_in), headers, &request);
        admin::read_reply(status, &body).unwrap()
    }

    fn configure(&self, page: &Person) {
        let admin::Reply::Done(issued) =
            self.grant(page, TEST_PAGE, admin::Action::SetLidarr(key_change(TEST_KEY)), 100)
        else {
            panic!("the admin page could not get a Lidarr grant");
        };
        assert!(matches!(self.apply(page, TEST_PAGE, admin::SET_LIDARR,
            key_change(TEST_KEY), 100, issued.grant), admin::Reply::Done(_)));
    }

    fn set_switch(&self, page: &Person, person: &Person, enabled: bool) {
        let change = || admin::RequestsEnabled { account: person.signed_in.person.id, enabled };
        let admin::Reply::Done(issued) =
            self.grant(page, TEST_PAGE, admin::Action::SetRequestsEnabled(change()), 101)
        else {
            panic!("the admin page could not get a request-switch grant");
        };
        assert!(matches!(self.apply(page, TEST_PAGE, admin::SET_REQUESTS_ENABLED,
            change(), 101, issued.grant), admin::Reply::Done(_)));
    }

}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
        self.kill_child();
        drop(self.accounts.take());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn key_change(key: &str) -> admin::LidarrKeyChange {
    admin::LidarrKeyChange { key: admin::LidarrKey::new(key).unwrap() }
}

struct Person {
    signed_in: SignedIn,
    restore: u64,
    headers: String,
}

fn read_http(socket: Socket) -> (u16, Vec<u8>) {
    let mut stream = BufReader::new(socket);
    let reply = probe::read_head(&mut stream).unwrap().unwrap();
    let mut body = Vec::new();
    probe::body(&reply, stream).unwrap().read_to_end(&mut body).unwrap();
    (reply.status, body)
}

fn body(restore: u64, call: Call) -> Vec<u8> {
    wire::write_call(&wire::Message { head: RequestHead { speaks: SPEAKS, restore }, call }).unwrap().bytes().to_vec()
}

fn begin(number: u64, change: Change) -> Call {
    Call::Begin { request_id: wire::RequestId::new(number).unwrap(), change }
}

fn begun(answer: Answer) -> JobId {
    match answer {
        Answer::Begun { job } => job,
        Answer::Refused { reason } => panic!("begin refused: {reason:?}"),
        _ => panic!("unexpected begin answer"),
    }
}

fn cancelled() -> Change {
    Change::Cancel { album: LidarrNumber::new(9).unwrap(), release: MusicBrainzId::new(ALBUM).unwrap() }
}

#[test]
fn authenticated_loopback_requests_keep_order_and_ownership_and_survive_the_client_leaving() {
    let lidarr = MockLidarr::start();
    let (server, [admin_code, first_code, other_code]) = TestServer::start("walk", Some(endpoint(lidarr.address)));
    let admin = server.redeem(admin_code, 1, TEST_PAGE);
    server.configure(&admin);
    let first = server.redeem(first_code, 2, "");
    let other = server.redeem(other_code, 3, "");

    let unknown = account::Session::from_bytes([17; 32]);
    let forged = format!("Authorization: {}\r\n", unknown.authorization().as_str());
    for headers in ["", forged.as_str()] {
        let unauthenticated = server.socket(wire::REQUESTS, None, headers, 4096);
        let (status, body) = read_http(unauthenticated);
        assert_eq!(status, 401, "the listener waited for a stranger's body");
        assert!(matches!(wire::read_reply(status, &body).unwrap(),
            account::Reply::Refused(account::Refusal::NotSignedIn(NotSignedIn::SessionEnded))));
    }
    assert!(lidarr.take_calls().is_empty());

    let offer = server.offer(&first);
    let change = Change::Request { offer };
    let job = begun(server.call(&first, begin(1, change.clone())));
    assert_eq!(begun(server.call(&first, begin(1, change.clone()))), job);
    assert!(matches!(server.call(&first, begin(1, cancelled())), Answer::Refused { reason: Refusal::Malformed }));
    assert!(matches!(server.call(&other, Call::After { job }), Answer::Refused { reason: Refusal::JobNotFound }));

    // The connection that received Begun is closed. The work and its poll
    // receipt belong to the server and finish without that connection.
    assert_eq!(server.done(&first, job), Completed::Requested { placed: true });
    assert_eq!(begun(server.call(&first, begin(1, change.clone()))), job);
    assert_eq!(lidarr.take_calls(), [
        "GET album/lookup?term=Glass%20Orchard",
        "GET artist",
        "GET album?artistId=5",
        "GET command",
        "GET artist/5",
        "PUT album/monitor",
        "GET album/9",
        "GET artist/5",
        "POST command",
    ]);

    assert!(matches!(server.call(&first, Call::Pending { after: None }),
        Answer::Pending { albums, .. } if albums.len() == 1 && albums[0].removable && albums[0].progress == Some(73)));
    assert!(matches!(server.call(&other, Call::Pending { after: None }), Answer::Pending { albums, .. } if albums.is_empty()));

    lock(&lidarr.state).arrived = true;
    assert!(matches!(server.call(&first, Call::Pending { after: None }), Answer::Pending { albums, .. } if albums.is_empty()));
    let same_account = server.sign_in(SignInCredential::Password {
        email: first.signed_in.person.email.clone(),
        password: Password::presented(PASSWORD).unwrap(),
    }, 4, "");
    assert!(matches!(server.call(&same_account, Call::AskedHistory { after: None }),
        Answer::AskedHistory { rows, next: None } if rows.len() == 1 && rows[0].release.as_str() == ALBUM));
    assert!(matches!(server.call(&other, Call::AskedHistory { after: None }),
        Answer::AskedHistory { rows, next: None } if rows.is_empty()));
    lock(&lidarr.state).arrived = false;

    assert!(matches!(server.call(&other, begin(1, cancelled())), Answer::Refused { reason: Refusal::AskedByOthers }));
    let stale = Change::Cancel { album: LidarrNumber::new(10).unwrap(), release: MusicBrainzId::new(ALBUM).unwrap() };
    let stale = begun(server.call(&first, begin(4, stale)));
    assert_eq!(server.outcome(&first, stale), Err(Refusal::NoLongerListed));

    lock(&lidarr.state).ambiguous_pending_number = true;
    let ambiguous = begun(server.call(&first, begin(5, cancelled())));
    assert_eq!(server.outcome(&first, ambiguous), Err(Refusal::NoLongerListed));
    lock(&lidarr.state).ambiguous_pending_number = false;
    assert!(lock(&lidarr.state).monitored);
    assert!(!lidarr.take_calls().iter().any(|call| call.starts_with("PUT ")));

    server.offer(&other);
    let coowned = begun(server.call(&other, begin(2, change)));
    assert_eq!(server.done(&other, coowned), Completed::Requested { placed: false });
    let read = wire::write_attribution(&wire::AttributionRead {
        head: RequestHead { speaks: SPEAKS, restore: admin.restore }, after: None,
    }).unwrap();
    let (code, reply) = server.post(wire::ADMIN_ATTRIBUTION, Some(&admin.signed_in), TEST_PAGE, read.bytes());
    let admin::Reply::Done(page) = wire::read_attribution_reply(code, &reply).unwrap() else {
        panic!("the admin page could not read attribution");
    };
    let accounts: HashSet<_> = page.rows.into_iter().map(|row| (row.account, row.asked.release)).collect();
    let release = MusicBrainzId::new(ALBUM).unwrap();
    assert_eq!(accounts, HashSet::from([
        (first.signed_in.person.id, release.clone()), (other.signed_in.person.id, release),
    ]));
    assert_eq!(server.post(wire::ADMIN_ATTRIBUTION, Some(&first.signed_in), TEST_PAGE, read.bytes()).0, 403);
    assert!(matches!(server.call(&first, begin(2, cancelled())), Answer::Refused { reason: Refusal::AskedByOthers }));
    let admin_app = server.admin_app(&admin);
    let cancel = begun(server.call(&admin_app, begin(1, cancelled())));
    assert_eq!(server.done(&admin_app, cancel), Completed::Cancelled);
    assert!(matches!(server.call(&first, Call::Pending { after: None }), Answer::Pending { albums, .. } if albums.is_empty()));
    assert!(!lidarr.take_calls().iter().any(|line| line == "POST command"));

    server.set_switch(&admin, &first, false);
    assert!(matches!(server.call(&first, Call::Pending { after: None }), Answer::Refused { reason: Refusal::SwitchedOff }));
    assert!(matches!(server.call(&first, begin(3, cancelled())), Answer::Refused { reason: Refusal::SwitchedOff }));
    assert!(matches!(server.call(&first, Call::After { job }), Answer::Refused { reason: Refusal::SwitchedOff }));
}

#[test]
fn a_finished_job_releases_its_worker_before_reporting_completion() {
    let (root, mut keeper, _) = fixture("completed-worker");
    let rooms = Arc::new(Mutex::new(Rooms::default()));
    let actor = Actor {
        account: AccountId::new(1).unwrap(),
        session: SessionId::new(1).unwrap(),
        admin: false,
        restore: keeper.restore(),
        now: now(),
    };
    let change = cancelled();
    let plan = Plan::resolve(&change, &Books::default(), actor, [0; 32], 0).unwrap();
    let job = enqueue(
        &mut keeper, actor, wire::RequestId::new(1).unwrap(), &encode(&change).unwrap(), &plan, false, &rooms,
    ).unwrap();
    let row = row_id(job).unwrap();
    let reservation = JobReservation::take(rooms.clone(), row, actor.account.get()).unwrap();
    let place = Place::take_upstream(rooms.clone(), actor.account.get(), Some(row)).unwrap();
    finish_job(&mut keeper, actor.now, row, &JobResult::Done(Completed::Cancelled), &[], None).unwrap();
    assert!(matches!(after_job(keeper.data().connection(), actor, job, &rooms), Ok(Answer::Working)));
    drop(place);
    drop(reservation);
    assert!(matches!(after_job(keeper.data().connection(), actor, job, &rooms),
        Ok(Answer::Done { result: Completed::Cancelled })));
    keeper.close();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_member_can_complete_the_full_walk_using_the_page_configured_key() {
    let lidarr = MockLidarr::start();
    let (server, [admin, member, other]) = TestServer::start("member-walk", Some(endpoint(lidarr.address)));
    server.configure(&server.redeem(admin, 1, TEST_PAGE));
    let member = server.redeem(member, 2, "");
    let other = server.redeem(other, 3, "");
    let offer = server.offer(&member);
    let job = begun(server.call(&member, begin(1, Change::Request { offer })));
    assert_eq!(server.done(&member, job), Completed::Requested { placed: true });
    let Answer::Pending { albums, .. } = server.call(&member, Call::Pending { after: None }) else {
        panic!("the member could not read pending requests");
    };
    assert_eq!(albums.len(), 1);
    assert_eq!(albums[0].progress, Some(73));
    assert!(albums[0].removable);
    assert!(matches!(server.call(&other, Call::Pending { after: None }),
        Answer::Pending { albums, .. } if albums.is_empty()));
    assert!(matches!(server.call(&member, Call::AskedHistory { after: None }),
        Answer::AskedHistory { rows, next: None } if rows.len() == 1 && rows[0].release.as_str() == ALBUM));
    assert!(matches!(server.call(&other, Call::AskedHistory { after: None }),
        Answer::AskedHistory { rows, next: None } if rows.is_empty()));
    assert!(matches!(server.call(&other, begin(1, cancelled())),
        Answer::Refused { reason: Refusal::AskedByOthers }));
    let target = &albums[0];
    let removal = Change::Cancel { album: target.id, release: target.album.clone() };
    let job = begun(server.call(&member, begin(2, removal)));
    assert_eq!(server.done(&member, job), Completed::Cancelled);
    assert!(matches!(server.call(&member, Call::Pending { after: None }),
        Answer::Pending { albums, .. } if albums.is_empty()));
}

#[test]
fn a_request_works_after_upgrading_the_deployed_form() {
    let lidarr = MockLidarr::start();
    let (root, mut keeper, [admin, member, _]) = fixture("deployed-form");
    let binding = binding(&endpoint(lidarr.address));
    keeper.write_data(now(), |tx| {
        tx.execute(
            "INSERT INTO requests_owners (binding, account, release, asked_ms)
             SELECT ?1, number, ?2, 1 FROM accounts WHERE email = 'person1@example.org'",
            params![binding.as_slice(), ALBUM],
        ).map(|count| ((), count != 0))
    }).unwrap();
    keeper.close();

    let db = Connection::open(root.join("data/nexus.db")).unwrap();
    db.execute_batch("DROP TRIGGER requests_owners_placed_stays;
        ALTER TABLE requests_owners DROP COLUMN placed;
        ALTER TABLE requests_owners DROP COLUMN metadata;
        PRAGMA user_version = 1; PRAGMA journal_mode = DELETE;").unwrap();
    let deployed = Connection::open_in_memory().unwrap();
    deployed.execute_batch(include_str!("../../tests/fixtures/form-1-4097756.sql")).unwrap();
    let old_rows = |db: &Connection| {
        db.prepare("SELECT name, type FROM pragma_table_info('requests_owners') ORDER BY cid").unwrap()
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))).unwrap()
            .map(Result::unwrap).collect::<Vec<_>>()
    };
    assert_eq!(old_rows(&db), old_rows(&deployed));
    drop(db);

    let server = TestServer::launch_saved(root, lidarr.address);
    let page = server.redeem(admin, 1, TEST_PAGE);
    server.configure(&page);
    let member = server.redeem(member, 2, "");
    let db = Connection::open(server.root.join("data/nexus.db")).unwrap();
    let stored: (Vec<u8>, bool) = db.query_row("SELECT metadata, placed FROM requests_owners",
        [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    assert_eq!(stored, (Vec::new(), false));
    assert!(matches!(server.call(&member, Call::AskedHistory { after: None }),
        Answer::AskedHistory { rows, .. } if rows.is_empty()));

    let offer = server.offer(&member);
    let job = begun(server.call(&member, begin(1, Change::Request { offer })));
    assert_eq!(server.done(&member, job), Completed::Requested { placed: true });
    assert!(matches!(server.call(&member, Call::AskedHistory { after: None }),
        Answer::AskedHistory { rows, .. } if rows.len() == 1 && rows[0].release.as_str() == ALBUM));
    let job = begun(server.call(&member, begin(2, cancelled())));
    assert_eq!(server.done(&member, job), Completed::Cancelled);
    let form: i64 = db.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
    assert_eq!(form, crate::store::data::FORM);
}

#[test]
fn asking_for_an_already_monitored_album_does_not_grant_removal() {
    let lidarr = MockLidarr::start();
    lock(&lidarr.state).monitored = true;
    let (server, [admin, member, _]) = TestServer::start("existing-album", Some(endpoint(lidarr.address)));
    let admin = server.redeem(admin, 1, TEST_PAGE);
    server.configure(&admin);
    let member = server.redeem(member, 2, "");
    let offer = server.offer(&member);
    let job = begun(server.call(&member, begin(1, Change::Request { offer })));
    assert_eq!(server.done(&member, job), Completed::Requested { placed: false });
    assert!(matches!(server.call(&member, Call::AskedHistory { after: None }),
        Answer::AskedHistory { rows, .. } if rows.len() == 1 && rows[0].release.as_str() == ALBUM));
    assert!(matches!(server.call(&member, Call::Pending { after: None }),
        Answer::Pending { albums, .. } if albums.len() == 1 && !albums[0].removable));
    lidarr.take_calls();
    assert!(matches!(server.call(&member, begin(2, cancelled())),
        Answer::Refused { reason: Refusal::NotAllowed }));
    assert!(lock(&lidarr.state).monitored);
    assert!(!lidarr.take_calls().iter().any(|call| call.starts_with("PUT ") || call.starts_with("POST ")));
    let admin_app = server.admin_app(&admin);
    let job = begun(server.call(&admin_app, begin(1, cancelled())));
    assert_eq!(server.done(&admin_app, job), Completed::Cancelled);
}

#[test]
fn the_page_can_switch_off_every_requests_call_for_a_signed_in_member() {
    let lidarr = MockLidarr::start();
    let (server, [admin, member, _]) = TestServer::start("switch", Some(endpoint(lidarr.address)));
    let admin = server.redeem(admin, 1, TEST_PAGE);
    server.configure(&admin);
    let member = server.redeem(member, 2, "");
    server.set_switch(&admin, &member, false);

    let job = job_id(1).unwrap();
    for call in [
        Call::Status,
        find_artist_call(),
        Call::Pending { after: None },
        Call::AskedHistory { after: None },
        Call::After { job },
        Call::BatchLines { job, after: None },
        Call::ResolveBatchLine { job, index: 0, release: None },
        begin(1, cancelled()),
    ] {
        assert!(matches!(server.call(&member, call), Answer::Refused { reason: Refusal::SwitchedOff }));
    }
    assert!(lidarr.take_calls().is_empty());
}

#[test]
fn without_a_container_address_neither_the_page_nor_the_app_can_enable_requests() {
    let (server, [admin, member, _]) = TestServer::start("no-lidarr", None);
    let page = server.redeem(admin, 1, TEST_PAGE);
    let app = server.redeem(member, 2, "");
    for call in [find_artist_call(), Call::AskedHistory { after: None }, begin(1, cancelled())] {
        assert!(matches!(server.call(&app, call), Answer::Refused { reason: Refusal::LidarrNotSetUp }));
    }
    let body = admin::encode(&admin::GetLidarrSetting {
        head: RequestHead { speaks: SPEAKS, restore: page.restore },
    }).unwrap();
    let (status, reply) = server.post(admin::GET_LIDARR_SETTING, Some(&page.signed_in), TEST_PAGE, body.bytes());
    assert!(matches!(admin::read_reply::<admin::LidarrSetting>(status, &reply).unwrap(),
        admin::Reply::Done(admin::LidarrSetting { address: None, configured: false })));
    assert!(matches!(server.grant(&page, TEST_PAGE, admin::Action::SetLidarr(key_change(TEST_KEY)), 1),
        admin::Reply::Refused(_)));
}

#[test]
fn only_the_admin_page_can_configure_a_key_for_the_read_only_address() {
    let lidarr = MockLidarr::start();
    let (server, [admin_code, member_code, _]) = TestServer::start("page", Some(endpoint(lidarr.address)));
    let page = server.redeem(admin_code, 1, TEST_PAGE);
    let app = server.redeem(member_code, 2, "");
    let app_admin = server.sign_in(SignInCredential::Password {
        email: page.signed_in.person.email.clone(),
        password: Password::presented(PASSWORD).unwrap(),
    }, 4, "");
    let head = RequestHead { speaks: SPEAKS, restore: page.restore };
    let body = admin::encode(&admin::GetLidarrSetting { head }).unwrap();
    let setting = || {
        let (status, reply) = server.post(admin::GET_LIDARR_SETTING, Some(&page.signed_in), TEST_PAGE, body.bytes());
        let admin::Reply::Done(setting) = admin::read_reply::<admin::LidarrSetting>(status, &reply).unwrap()
        else {
            panic!("the admin page could not read the configured address");
        };
        assert_eq!(setting.address, Some(endpoint(lidarr.address).address));
        setting.configured
    };
    assert!(!setting());
    assert!(matches!(server.call(&app, Call::Pending { after: None }),
        Answer::Refused { reason: Refusal::LidarrNotSetUp }));

    for (who, headers) in [(&page.signed_in, ""), (&app.signed_in, TEST_PAGE), (&app_admin.signed_in, TEST_PAGE)] {
        assert_eq!(server.post(admin::GET_LIDARR_SETTING, Some(who), headers, body.bytes()).0, 403);
    }
    let unrelated = "a-replacement-key";
    assert!(matches!(server.grant(&app_admin, TEST_PAGE, admin::Action::SetLidarr(key_change(TEST_KEY)), 20),
        admin::Reply::Refused(admin::Refusal::NotAllowed(admin::Denial::Denied))));
    let admin::Reply::Done(issued) =
        server.grant(&page, TEST_PAGE, admin::Action::SetLidarr(key_change(TEST_KEY)), 20)
    else {
        panic!("the page could not request a matching grant");
    };
    let grant = || admin::GrantSecret::from_bytes(*issued.grant.expose_bytes());
    assert!(matches!(server.apply(&app_admin, TEST_PAGE, admin::SET_LIDARR,
        key_change(TEST_KEY), 20, grant()), admin::Reply::Refused(_)));
    assert!(matches!(server.apply(&page, TEST_PAGE, admin::SET_LIDARR,
        key_change(unrelated), 20, grant()), admin::Reply::Refused(_)));
    assert!(matches!(server.apply(&page, TEST_PAGE, admin::SET_LIDARR,
        key_change(TEST_KEY), 21, grant()), admin::Reply::Refused(_)));
    assert!(!setting());
    assert!(matches!(server.apply(&page, TEST_PAGE, admin::SET_LIDARR,
        key_change(TEST_KEY), 20, grant()), admin::Reply::Done(_)));
    assert!(setting());
    assert!(matches!(server.apply(&page, TEST_PAGE, admin::SET_LIDARR,
        key_change(TEST_KEY), 20, grant()), admin::Reply::Refused(_)));

    let toggle = || admin::Action::SetRequestsEnabled(admin::RequestsEnabled {
        account: app.signed_in.person.id,
        enabled: false,
    });
    assert!(matches!(server.grant(&app_admin, TEST_PAGE, toggle(), 21),
        admin::Reply::Refused(admin::Refusal::NotAllowed(admin::Denial::Denied))));
    assert!(matches!(server.grant(&page, TEST_PAGE, toggle(), 21), admin::Reply::Done(_)));
    assert!(lidarr.take_calls().is_empty());
}

fn fixture(name: &str) -> (PathBuf, Keeper, [OneTimeCode; 3]) {
        let root = std::env::temp_dir().join(format!("nexus-requests-{name}-{}", std::process::id()));
        std::fs::create_dir_all(root.join("state")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        let now = now();
        let (mut keeper, _) = Keeper::open(&root.join("state"), &root.join("data"), now).unwrap();
        let key = CodeKey::of(&keeper);
        let codes = std::array::from_fn(|index| {
            let (code, print) = make_code(&key).unwrap();
            keeper.make_account(&format!("person{index}@example.org"), "Requests walk", index == 0,
                First::Code(&print, now + 600_000), now).unwrap();
            code
        });
        (root, keeper, codes)
}

fn fixture_accounts(
    mut keeper: Keeper, clock: crate::accounts::session::Clock, lidarr: Option<admin::InternalAddress>,
) -> Arc<Accounts> {
        let now = clock();
        let id = keeper.backend();
        let admission = Arc::new(Admission::open(&mut keeper, clock.clone()).unwrap());
        Arc::new(Accounts {
            workers: PasswordWorkers::new(dummy_salt_key(&keeper)).unwrap(),
            set_up_code: SetUpCode::at_start(keeper.set_up(), now).0,
            admission,
            lidarr,
            desk: Desk::open(keeper, clock.clone()).unwrap(),
            clock,
            backend: id,
            music: MusicSide::new(
                crate::music::MusicClient::trusting(RootCertStore::empty()),
                "http://127.0.0.1:1".into(),
            ),
        })
}

fn reopen(root: &Path) -> Keeper {
    Keeper::open(&root.join("state"), &root.join("data"), now()).unwrap().0
}

#[test]
#[ignore = "child process entry point, invoked by the restart case"]
fn serve_in_a_child_for_the_restart_case() {
    let Some(root) = std::env::var_os("NEXUS_REQUESTS_TEST_DATA").map(PathBuf::from) else { return };
    let address: SocketAddr = std::env::var("NEXUS_REQUESTS_TEST_LISTEN").unwrap().parse().unwrap();
    let lidarr: SocketAddr = std::env::var("NEXUS_REQUESTS_TEST_LIDARR").unwrap().parse().unwrap();
    assert!(address.ip().is_loopback() && lidarr.ip().is_loopback());
    let accounts = fixture_accounts(reopen(&root), Arc::new(now), Some(endpoint(lidarr).address));
    let id = accounts.backend;
    runtime().block_on(async {
        let listener = serve::listen(address).unwrap();
        let shell = Shell::backend(address.ip(), &crate::hello(id), accounts,
            Some(TEST_ORIGIN), crate::settings::DATA_BUDGET_DEFAULT);
        serve::serve(listener, shell, false).await;
    });
}

#[test]
fn restart_sees_the_begun_job_through_without_accepting_it_twice() {
    let lidarr = MockLidarr::start();
    let (mut server, [admin_code, member, _]) = TestServer::start_child("restart", lidarr.address);
    server.configure(&server.redeem(admin_code, 1, TEST_PAGE));
    let member = server.redeem(member, 2, "");
    let change = Change::Request { offer: server.offer(&member) };
    lidarr.hold_response(Some(("GET", "/api/v1/artist")));
    let job = begun(server.call(&member, begin(1, change.clone())));
    lidarr.wait_for_call("GET artist");
    server.kill_child();
    lidarr.hold_response(None);
    server.launch(lidarr.address);
    assert_eq!(server.done(&member, job), Completed::Requested { placed: true });
    assert_eq!(begun(server.call(&member, begin(1, change))), job);
    let calls = lidarr.take_calls();
    assert_eq!(calls.iter().filter(|&line| line == "PUT album/monitor").count(), 1);
    assert_eq!(calls.iter().filter(|&line| line == "POST command").count(), 1);
}

#[test]
fn restart_after_lidarr_took_the_search_does_not_send_the_command_again() {
    let lidarr = MockLidarr::start();
    let (mut server, [admin_code, member, _]) = TestServer::start_child("lost-lidarr-reply", lidarr.address);
    server.configure(&server.redeem(admin_code, 1, TEST_PAGE));
    let member = server.redeem(member, 2, "");
    let change = Change::Request { offer: server.offer(&member) };
    lidarr.hold_response(Some(("POST", "/api/v1/command")));
    let job = begun(server.call(&member, begin(1, change.clone())));
    lidarr.wait_for_call("POST command");
    assert!(matches!(server.call(&member, Call::After { job }), Answer::Working));

    server.kill_child();
    lidarr.hold_response(None);
    server.launch(lidarr.address);
    assert_eq!(server.done(&member, job), Completed::Requested { placed: true });
    assert_eq!(begun(server.call(&member, begin(1, change))), job);
    let calls = lidarr.take_calls();
    assert_eq!(calls.iter().filter(|&line| line == "PUT album/monitor").count(), 1);
    assert_eq!(calls.iter().filter(|&line| line == "POST command").count(), 1);
}

#[test]
fn a_late_lidarr_answer_does_not_lose_the_placement_or_the_cancellation() {
    let lidarr = MockLidarr::start();
    let (server, [admin, member, _]) = TestServer::start("late-mutations", Some(endpoint(lidarr.address)));
    server.configure(&server.redeem(admin, 1, TEST_PAGE));
    let member = server.redeem(member, 2, "");
    let change = Change::Request { offer: server.offer(&member) };

    lidarr.hold_response(Some(("POST", "/api/v1/command")));
    let job = begun(server.call(&member, begin(1, change)));
    lidarr.wait_for_call("POST command");
    thread::sleep(UPSTREAM_TIMEOUT + Duration::from_millis(150));
    lidarr.hold_response(None);
    assert_eq!(server.done(&member, job), Completed::Requested { placed: true });
    assert!(matches!(server.call(&member, Call::Pending { after: None }),
        Answer::Pending { albums, .. } if albums.len() == 1 && albums[0].removable));
    let search_calls = lidarr.take_calls();
    assert_eq!(search_calls.iter().filter(|&call| call == "POST command").count(), 1);

    lidarr.hold_response(Some(("PUT", "/api/v1/album/monitor")));
    let job = begun(server.call(&member, begin(2, cancelled())));
    lidarr.wait_for_call("PUT album/monitor");
    thread::sleep(UPSTREAM_TIMEOUT + Duration::from_millis(150));
    lidarr.hold_response(None);
    assert_eq!(server.done(&member, job), Completed::Cancelled);
    assert!(matches!(server.call(&member, Call::AskedHistory { after: None }),
        Answer::AskedHistory { rows, .. } if rows.is_empty()));
    let cancel_calls = lidarr.take_calls();
    assert_eq!(cancel_calls.iter().filter(|&call| call == "PUT album/monitor").count(), 1);
}

#[test]
fn startup_puts_back_a_recorded_artist_setting_for_the_same_lidarr() {
    let lidarr = MockLidarr::start();
    lock(&lidarr.state).new_releases = Some("none".into());
    let binding = binding(&endpoint(lidarr.address));
    let (mut server, [admin_code, _, _]) = TestServer::start_child("way-back", lidarr.address);
    server.configure(&server.redeem(admin_code, 1, TEST_PAGE));
    server.kill_child();
    let mut keeper = reopen(&server.root);
    keeper.write_data(now(), |tx| {
        tx.execute("INSERT INTO requests_way_back (binding, artist, artist_key, setting) VALUES (?1, 17, ?2, 'all')",
            params![binding.as_slice(), ARTIST])?;
        tx.execute("INSERT INTO requests_way_back (binding,artist,artist_key,setting) VALUES (?1,5,?2,'all')",
            params![binding.as_slice(), ARTIST])?;
        Ok(((), true))
    }).unwrap();
    keeper.close();
    server.launch(lidarr.address);
    let db = Connection::open(server.root.join("data/nexus.db")).unwrap();
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        let current = db.query_row("SELECT count(*) FROM requests_way_back", [], |row| row.get::<_, i64>(0)).unwrap();
        if current == 0 {
            break;
        }
        assert!(Instant::now() < until, "the way back was still kept");
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(lock(&lidarr.state).new_releases.as_deref(), Some("all"));
    assert_eq!(lidarr.take_calls(), ["GET artist", "GET artist", "GET artist/5", "PUT artist/5"]);
}

fn find_artist_call() -> Call {
    Call::FindArtists { search: Search::new("Quillon Vane").unwrap(), after: None }
}

#[test]
fn the_backend_answers_fixed_refusals_for_hostile_lidarr_responses() {
    let dead = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = dead.local_addr().unwrap();
    drop(dead);
    let assert_refusal = |name, address: SocketAddr, expected| {
        let (server, [admin_code, member, _]) = TestServer::start(name, Some(endpoint(address)));
        server.configure(&server.redeem(admin_code, 1, TEST_PAGE));
        let member = server.redeem(member, 2, "");
        let sent = body(member.restore, find_artist_call());
        let (status, reply) = server.post(wire::REQUESTS, Some(&member.signed_in), "", &sent);
        assert!(!String::from_utf8_lossy(&reply).contains(&address.to_string()));
        assert!(matches!(wire::read_reply(status, &reply).unwrap(),
            account::Reply::Done { result: Answer::Refused { reason }, .. } if reason == expected));
    };
    assert_refusal("down", address, Refusal::LidarrUnavailable);

    let trap = TcpListener::bind("127.0.0.1:0").unwrap();
    trap.set_nonblocking(true).unwrap();
    let trap_address = trap.local_addr().unwrap();
    let (address, reply) = once(move |socket, _| {
        response(socket, "302 Found", &format!("Location: http://{trap_address}/\r\n"), TEST_KEY.as_bytes()).unwrap();
    });
    assert_refusal("redirect", address, Refusal::LidarrBehindSignIn);
    reply.join().unwrap();
    assert_eq!(trap.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);

    let (address, reply) = once(|socket, _| {
        let mut too_large = vec![b' '; http::BODY_LIMIT as usize + 1];
        too_large[..2].copy_from_slice(b"[]");
        let _ = response(socket, "200 OK", "", &too_large);
    });
    assert_refusal("large", address, Refusal::LidarrUnavailable);
    reply.join().unwrap();

    let (address, reply) = once(|socket, _| {
        response(socket, "200 OK", "", format!("<garbage>{TEST_KEY}</garbage>").as_bytes()).unwrap();
    });
    assert_refusal("garbage", address, Refusal::LidarrUnrecognized);
    reply.join().unwrap();
}

#[test]
fn a_slow_lidarr_read_leaves_sign_in_and_the_writer_available() {
    let (arrived, read) = std::sync::mpsc::channel();
    let (address, reply) = once(move |socket, _| {
        arrived.send(()).unwrap();
        thread::sleep(UPSTREAM_TIMEOUT + Duration::from_millis(150));
        let _ = response(socket, "200 OK", "", b"[]");
    });
    let (server, [admin_code, member, other]) = TestServer::start("slow", Some(endpoint(address)));
    server.configure(&server.redeem(admin_code, 1, TEST_PAGE));
    let member = server.redeem(member, 2, "");
    thread::scope(|scope| {
        let search = scope.spawn(|| server.call(&member, find_artist_call()));
        read.recv_timeout(Duration::from_secs(2)).unwrap();
        let start = Instant::now();
        let _ = server.redeem(other, 3, "");
        assert!(start.elapsed() < Duration::from_secs(3));
        assert!(matches!(search.join().unwrap(), Answer::FoundArtists { artists, .. } if artists.is_empty()));
    });
    reply.join().unwrap();
}

#[test]
fn switching_off_a_running_batch_blocks_its_next_lidarr_write() {
    let lidarr = MockLidarr::start();
    let (server, [admin, member, _]) = TestServer::start("stop-batch", Some(endpoint(lidarr.address)));
    let page = server.redeem(admin, 1, TEST_PAGE);
    server.configure(&page);
    let member = server.redeem(member, 2, "");
    lidarr.hold_response(Some(("PUT", "/api/v1/album/monitor")));
    let line = BatchLine { artist: "Quillon Vane".into(), album: "Glass Orchard".into(), song: "First Light".into() };
    let job = begun(server.call(&member, begin(1, Change::Batch { singles: false, lines: vec![line] })));
    lidarr.wait_for_call("PUT album/monitor");
    server.set_switch(&page, &member, false);
    lidarr.hold_response(None);

    let db = Connection::open(server.root.join("data/nexus.db")).unwrap();
    let until = Instant::now() + Duration::from_secs(6);
    loop {
        let (stage, result): (i64, Option<String>) = db.query_row(
            "SELECT stage, result FROM requests_jobs WHERE id = ?1", [row_id(job).unwrap()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        if stage == FINISHED {
            assert!(matches!(read_job_result(&result.unwrap()).unwrap(),
                JobResult::Failed(Refusal::SwitchedOff)));
            break;
        }
        assert!(Instant::now() < until, "the stopped job did not finish");
        thread::sleep(Duration::from_millis(20));
    }
    let line = saved_line(&db, row_id(job).unwrap(), 0).unwrap();
    assert!(matches!(line.progress, Some(BatchLineProgress::Failed { reason: Refusal::SwitchedOff, .. })));
    let calls = lidarr.take_calls();
    assert_eq!(calls.iter().filter(|&call| call == "PUT album/monitor").count(), 1);
    assert!(!calls.iter().any(|call| call == "POST command"));
}

#[test]
fn a_batch_waiting_for_its_device_releases_the_account_and_expires() {
    let lidarr = MockLidarr::start();
    let offset = Arc::new(AtomicU64::new(0));
    let clock = {
        let offset = offset.clone();
        Arc::new(move || now() + offset.load(Ordering::Relaxed))
    };
    let (server, [admin, _, _]) =
        TestServer::start_with_clock("fallback-expiry", Some(endpoint(lidarr.address)), clock);
    let page = server.redeem(admin, 1, TEST_PAGE);
    server.configure(&page);
    let admin = server.admin_app(&page);
    let job = begun(server.call(&admin, begin(1, Change::Batch {
        singles: false,
        lines: vec![BatchLine { artist: "Quillon Vane".into(), album: String::new(), song: "First Light".into() }],
    })));
    let until = Instant::now() + Duration::from_secs(6);
    loop {
        if matches!(server.call(&admin, Call::BatchLines { job, after: None }), Answer::BatchLines { lines, .. }
            if lines[0].progress == BatchLineProgress::NeedsResolution)
        {
            break;
        }
        assert!(Instant::now() < until, "the batch did not wait for the submitting device");
        thread::sleep(Duration::from_millis(30));
    }
    let second = loop {
        match server.call(&admin, begin(2, cancelled())) {
            Answer::Begun { job } => break job,
            Answer::Refused { reason: Refusal::Busy } if Instant::now() < until => {
                thread::sleep(Duration::from_millis(30));
            }
            _ => panic!("the parked batch held the account's place"),
        }
    };
    assert_eq!(server.outcome(&admin, second), Err(Refusal::NoLongerListed));
    // Include the keeper's one-minute clock allowance after opening the store.
    offset.store((wire::RESOLUTION_WAIT_SECONDS + 61) * 1000, Ordering::Relaxed);
    assert!(matches!(server.call(&admin, Call::ResolveBatchLine { job, index: 0, release: MusicBrainzId::new(ALBUM) }),
        Answer::Refused { reason: Refusal::NotAllowed }));
    assert_eq!(server.done(&admin, job), Completed::Batch);
    assert!(matches!(server.call(&admin, Call::BatchLines { job, after: None }), Answer::BatchLines { lines, .. }
        if lines.len() == 1 && lines[0].progress == BatchLineProgress::NotFound));
    assert!(!lidarr.take_calls().iter().any(|call| call.starts_with("PUT ") || call.starts_with("POST ")));
}

#[test]
fn sign_out_closes_an_awaiting_line_and_finishes_the_rest_of_the_batch() {
    let lidarr = MockLidarr::start();
    let (server, [admin, member, _]) = TestServer::start("signout-fallback", Some(endpoint(lidarr.address)));
    server.configure(&server.redeem(admin, 1, TEST_PAGE));
    let member = server.redeem(member, 2, "");
    let another_device = server.sign_in(SignInCredential::Password {
        email: member.signed_in.person.email.clone(),
        password: Password::presented(PASSWORD).unwrap(),
    }, 3, "");
    let album = BatchLine { artist: "Quillon Vane".into(), album: "Glass Orchard".into(), song: "First Light".into() };
    let song = BatchLine { album: String::new(), ..album.clone() };
    let job = begun(server.call(&member, begin(1, Change::Batch { singles: false, lines: vec![album, song] })));
    let until = Instant::now() + Duration::from_secs(6);
    loop {
        if matches!(server.call(&member, Call::BatchLines { job, after: None }),
            Answer::BatchLines { lines, .. } if lines[1].progress == BatchLineProgress::NeedsResolution)
        {
            break;
        }
        assert!(Instant::now() < until, "the batch did not wait for the submitting device");
        thread::sleep(Duration::from_millis(30));
    }

    let sign_out = account::encode(&account::SignOut {
        head: RequestHead { speaks: SPEAKS, restore: member.restore },
        request_id: account::RequestId::new(1).unwrap(),
        revoke: None,
    }).unwrap();
    assert_eq!(server.post(account::SIGN_OUT, Some(&member.signed_in), "", sign_out.bytes()).0, 200);
    assert!(matches!(server.call(&another_device, Call::BatchLines { job, after: None }),
        Answer::Refused { reason: Refusal::JobNotFound }));

    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if matches!(server.call(&another_device, Call::AskedHistory { after: None }),
            Answer::AskedHistory { rows, .. } if rows.len() == 1 && rows[0].release.as_str() == ALBUM)
        {
            break;
        }
        assert!(Instant::now() < until, "the accepted line did not finish after sign-out");
        thread::sleep(Duration::from_millis(30));
    }
    let db = Connection::open(server.root.join("data/nexus.db")).unwrap();
    let text: String = db.query_row("SELECT progress FROM requests_lines WHERE job = ?1 AND line = 1",
        [row_id(job).unwrap()], |row| row.get(0)).unwrap();
    assert_eq!(decode::<SavedLine>(text.as_bytes()).unwrap().progress, Some(BatchLineProgress::NotFound));
    assert_eq!(lidarr.take_calls().iter().filter(|&call| call == "POST command").count(), 1);
}

#[test]
fn batch_keeps_its_original_lines_and_exact_device_resolution_across_restart() {
    let lidarr = MockLidarr::start();
    let (mut server, [admin_code, member, other]) = TestServer::start_child("batch", lidarr.address);
    server.configure(&server.redeem(admin_code, 1, TEST_PAGE));
    let member = server.redeem(member, 2, "");
    let other = server.redeem(other, 3, "");
    let source = BatchLine { artist: "Quillon Vane".into(), album: "Glass Orchard".into(), song: "First Light".into() };
    let lines = vec![source.clone(), BatchLine { album: String::new(), ..source }];
    let job = begun(server.call(&member, begin(1, Change::Batch { singles: false, lines })));
    assert!(matches!(server.call(&other, Call::BatchLines { job, after: None }),
        Answer::Refused { reason: Refusal::JobNotFound }));
    let until = Instant::now() + Duration::from_secs(7);
    loop {
        if matches!(server.call(&member, Call::BatchLines { job, after: None }), Answer::BatchLines { lines, .. }
            if lines.iter().any(|line| line.index == 1 && line.progress == BatchLineProgress::NeedsResolution))
        {
            break;
        }
        assert!(Instant::now() < until, "the batch did not ask its device for resolution");
        thread::sleep(Duration::from_millis(30));
    }
    let resolution = || Call::ResolveBatchLine { job, index: 1, release: MusicBrainzId::new(ALBUM) };
    assert!(matches!(server.call(&other, resolution()), Answer::Refused { reason: Refusal::JobNotFound }));
    lidarr.hold_response(Some(("POST", "/api/v1/command")));
    assert!(matches!(server.call(&member, resolution()), Answer::BatchResolutionAccepted));
    lidarr.wait_for_call("POST command");
    server.kill_child();
    lidarr.hold_response(None);
    server.launch(lidarr.address);
    assert_eq!(server.done(&member, job), Completed::Batch);
    assert!(matches!(server.call(&member, resolution()), Answer::BatchResolutionAccepted));
    assert!(matches!(server.call(&member, Call::ResolveBatchLine { job, index: 1, release: None }),
        Answer::Refused { reason: Refusal::NotAllowed }));

    let Answer::BatchLines { lines, next: None } = server.call(&member, Call::BatchLines { job, after: None }) else {
        panic!("the batch did not return its progress");
    };
    assert_eq!(lines.iter().map(|line| line.index).collect::<Vec<_>>(), [0, 1]);
    assert!(lines.iter().all(|line| matches!(&line.progress,
        BatchLineProgress::Done { album } if album.offer.0.as_str() == ALBUM)));
    let calls = lidarr.take_calls();
    assert_eq!(calls.iter().filter(|&call| call == "POST command").count(), 1);
    assert_eq!(calls.iter().filter(|&call| call == &format!("GET album/lookup?term=lidarr%3A{ALBUM}")).count(), 1);
}
