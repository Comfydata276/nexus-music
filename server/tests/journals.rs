//! S12 and S7 over the program's real HTTP listener. SQLite reads inspect committed
//! contents when a wire acknowledgement alone cannot prove the state.

use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier, atomic::{AtomicU64, Ordering}};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nexus_core::nexus::account::{
    self, Credential, DeleteListeningData, DeleteListeningDataTarget, DeviceId, DeviceKind,
    DeviceSecret, Email, GrantChange, GrantIssued, GrantRequest, ListDevices, Name, OneTimeCode,
    Password, Refusal, Reply, RequestHead, RequestId, Session, SignIn, SignedIn, SignedOut, Stamp,
};
use nexus_core::nexus::diag::{self, About, Report, Upload};
use nexus_core::nexus::events::{
    self, Batch, Began, EventId, Kind, Outcome, Placed, PlayedFrom, Record, Sound, Stopped,
};
use nexus_core::nexus::proto::{SPEAKS, Version};
use nexus_music_server::accounts::{
    admit::Admission, check::PasswordWorkers, desk::Desk, music::MusicSide, session::Accounts,
    setup::{CodeKey, SetUpCode, dummy_salt_key, make_code},
};
use nexus_music_server::music::MusicClient;
use nexus_music_server::serve::{self, Shell};
use nexus_music_server::settings::DATA_BUDGET_DEFAULT;
use nexus_music_server::probe;
use nexus_music_server::store::keeper::{First, Keeper};
use rusqlite::{Connection, params, types::FromSql};

const PROGRAM: &str = env!("CARGO_BIN_EXE_nexus-music-server");
const EMAIL: &str = "journal@example.org";
const OTHER: &str = "other@example.org";
const PASSWORD: &str = "a good password for journal tests";
const TITLE: &str = "Private song title from a test";
const BROWSER: &str = "Origin: https://nexus.example.org\r\nX-Nexus-Admin: 1\r\n";

struct Fixture {
    root: PathBuf,
    port: u16,
    child: Option<Child>,
    clock_server: Option<(tokio::sync::oneshot::Sender<()>, JoinHandle<()>)>,
    budget: Option<u64>,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("nexus-journals-{}-{name}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("data")).unwrap();
        std::fs::create_dir(root.join("state")).unwrap();
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        Self { root, port, child: None, clock_server: None, budget: None }
    }

    fn program(&self) -> Command {
        let mut cmd = Command::new(PROGRAM);
        cmd.env_clear()
            .env("NEXUS_MUSIC_SERVER", "http://navidrome:4533")
            .env("NEXUS_PROXY", "127.0.0.1")
            .env("NEXUS_LISTEN", format!("127.0.0.1:{}", self.port))
            .env("NEXUS_DATA", self.root.join("data"))
            .env("NEXUS_STATE", self.root.join("state"))
            .env("NEXUS_PUBLIC_ORIGIN", "https://nexus.example.org");
        if let Some(budget) = self.budget {
            cmd.env("NEXUS_DATA_BUDGET", budget.to_string());
        }
        cmd
    }

    fn set_up(&self) -> OneTimeCode {
        let out = self.program().args(["set-up", EMAIL, "Journal owner"]).output().unwrap();
        assert!(out.status.success(), "set-up failed");
        let out = String::from_utf8(out.stdout).unwrap();
        let (_, code) = out.split_once("one-time code ").unwrap();
        OneTimeCode::typed(&code[..13]).unwrap()
    }

    fn other(&self) -> OneTimeCode {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let (mut keeper, _) = Keeper::open(&self.root.join("state"), &self.root.join("data"), now)
            .unwrap_or_else(|e| panic!("{e}"));
        let (code, print) = make_code(&CodeKey::of(&keeper)).unwrap();
        keeper.make_account(OTHER, "Other owner", false, First::Code(&print, now + 600_000), now).unwrap();
        keeper.close();
        code
    }

    fn start(&mut self) {
        self.child = Some(self.program().stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap());
        self.wait_until_ready();
    }

    // The production router and loopback listener with a clock the test can
    // advance. Report acceptance is exercised over HTTP through the real Desk.
    fn start_with_clock(&mut self) -> Arc<AtomicU64> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        // Start past the store's restart allowance so exact gap assertions
        // measure the injected clock from the first receipt.
        let now = now + 2 * nexus_core::nexus::kept::CLOCK_MARK_INTERVAL_MS;
        let clock = Arc::new(AtomicU64::new(now));
        let clock_for_server = clock.clone();
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let addr = ([127, 0, 0, 1], self.port).into();
        let root = self.root.clone();
        let thread = std::thread::spawn(move || {
            let (mut keeper, _) = Keeper::open(&root.join("state"), &root.join("data"), now)
                .unwrap_or_else(|why| panic!("{why}"));
            let backend = keeper.backend();
            let clock: nexus_music_server::accounts::session::Clock =
                Arc::new(move || clock_for_server.load(Ordering::SeqCst));
            let accounts = Arc::new(Accounts {
                workers: PasswordWorkers::new(dummy_salt_key(&keeper)).unwrap(),
                set_up_code: SetUpCode::at_start(keeper.set_up(), now).0,
                admission: Arc::new(Admission::open(&mut keeper, clock.clone()).unwrap()),
                desk: Desk::open(keeper, clock.clone()).unwrap(),
                clock,
                backend,
                music: MusicSide::new(MusicClient::from_this_machine().0, "http://navidrome:4533".into()),
                lidarr: None,
            });
            tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap().block_on(async {
                let listener = serve::listen(addr).unwrap();
                let shell = Shell::backend(
                    addr.ip(), &nexus_music_server::hello(backend), accounts,
                    Some("https://nexus.example.org"), DATA_BUDGET_DEFAULT,
                );
                let serving = tokio::spawn(serve::serve(listener, shell, false));
                let _ = stopped.await;
                serving.abort();
                let _ = serving.await;
            });
        });
        self.clock_server = Some((stop, thread));
        self.wait_until_ready();
        clock
    }

    fn wait_until_ready(&self) {
        let start = Instant::now();
        loop {
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "the server never listened");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn stop(&mut self) -> String {
        if let Some((stop, thread)) = self.clock_server.take() {
            let _ = stop.send(());
            thread.join().unwrap();
        }
        let Some(mut child) = self.child.take() else { return String::new() };
        let _ = child.kill();
        let output = child.wait_with_output().unwrap();
        [output.stdout, output.stderr].into_iter().map(|bytes| String::from_utf8(bytes).unwrap()).collect()
    }

    fn db(&self) -> Connection {
        Connection::open(self.root.join("data/nexus.db")).unwrap()
    }

    fn request(&self, path: &str, session: Option<&Session>, headers: &str, length: usize) -> TcpStream {
        let mut socket = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        write!(socket, "POST {path} HTTP/1.1\r\nHost: nexus\r\nX-Nexus-Scheme: https\r\nContent-Type: application/json\r\nContent-Length: {length}\r\n").unwrap();
        if let Some(session) = session {
            write!(socket, "Authorization: {}\r\n", session.authorization().as_str()).unwrap();
        }
        write!(socket, "{headers}Connection: close\r\n\r\n").unwrap();
        socket
    }

    fn post(&self, path: &str, who: Option<&SignedIn>, body: &[u8]) -> (u16, Vec<u8>) {
        self.post_with(path, who, "", body)
    }

    fn post_with(&self, path: &str, who: Option<&SignedIn>, headers: &str, body: &[u8]) -> (u16, Vec<u8>) {
        let mut socket = self.request(path, who.map(|who| &who.session), headers, body.len());
        socket.write_all(body).unwrap();
        response(socket)
    }

    fn sign_in(&self, credential: Credential, device: u8) -> (SignedIn, Stamp) {
        self.sign_in_with(credential, device, "")
    }

    fn sign_in_with(&self, credential: Credential, device: u8, headers: &str) -> (SignedIn, Stamp) {
        let request = account::encode(&SignIn {
            credential,
            device_secret: DeviceSecret::from_bytes([device; 32]),
            device_name: Name::new("Local device name").unwrap(),
            device_kind: DeviceKind::Pc,
            app_version: Version::parse("0.41.0").unwrap(),
            speaks: SPEAKS,
            restore: None,
        }).unwrap();
        let (status, body) = self.post_with(account::SIGN_IN, None, headers, request.bytes());
        match account::read_reply::<SignedIn>(status, &body).unwrap() {
            Reply::Done { result, receipt } => (result, receipt.stamp),
            refusal => panic!("sign-in refused: {refusal:?}"),
        }
    }

    fn stats(&self, who: &SignedIn, stamp: &Stamp, after: Option<DeviceId>) -> Vec<u8> {
        let (status, body) = self.post(nexus_music_server::events::STATS_PATH, Some(who), page(stamp, after).bytes());
        assert_eq!(status, 200);
        body
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn response(socket: TcpStream) -> (u16, Vec<u8>) {
    let mut from = BufReader::new(socket);
    let head = probe::read_head(&mut from).unwrap().expect("an HTTP answer");
    let mut bytes = Vec::new();
    probe::body(&head, from).unwrap().read_to_end(&mut bytes).unwrap();
    (head.status, bytes)
}

fn with_code(code: OneTimeCode) -> Credential {
    Credential::Code { code, new_password: Password::new(PASSWORD).unwrap() }
}

fn with_password() -> Credential {
    Credential::Password { email: Email::new(EMAIL).unwrap(), password: Password::presented(PASSWORD).unwrap() }
}

fn head(stamp: &Stamp) -> RequestHead {
    RequestHead { speaks: SPEAKS, restore: stamp.restore }
}

fn page(stamp: &Stamp, after: Option<DeviceId>) -> account::WireBody {
    account::encode(&ListDevices { head: head(stamp), after }).unwrap()
}

fn json<T: FromSql>(body: &[u8], path: &str) -> T {
    let body = std::str::from_utf8(body).unwrap();
    Connection::open_in_memory().unwrap().query_row(
        "SELECT json_extract(?1, ?2)", params![body, path], |row| row.get(0)
    ).unwrap()
}

fn id(n: u64) -> EventId {
    let mut bytes = [0; 16];
    bytes[8..].copy_from_slice(&n.to_be_bytes());
    EventId::from_bytes(bytes)
}

fn start(n: u64, at: u64, pos: u32) -> Placed {
    Record::new(id(n), at, Kind::Start {
        song_id: format!("test-{n}"),
        title: TITLE.into(),
        artist: "A private artist".into(),
        album: "A private album".into(),
        from: PlayedFrom::Album { id: "a1".into() },
        began: Began::PressedHere,
        sound: Sound::Streamed,
        position_ms: pos,
    }).unwrap().place(n).unwrap()
}

fn batch(stamp: &Stamp, rows: &[Placed]) -> Batch {
    Batch::prefix(head(stamp), stamp.life, rows, events::BATCH_BYTES).unwrap()
}

fn send(fixture: &Fixture, who: &SignedIn, batch: &Batch) -> (u16, Vec<u8>) {
    fixture.post(events::SEND_PATH, Some(who), batch.write().unwrap().bytes())
}

fn assert_outcomes(reply: (u16, Vec<u8>), batch: &Batch, expected: &[Outcome]) {
    let (status, body) = reply;
    assert_eq!(status, 200);
    let Reply::Done { result, receipt: _ } = events::read_reply(status, &body, batch).unwrap() else {
        panic!("event batch not done");
    };
    assert_eq!(result.answers().iter().map(|item| item.outcome).collect::<Vec<_>>(), expected);
}

fn count(fixture: &Fixture, table: &str) -> i64 {
    fixture.db().query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row.get(0)).unwrap()
}

#[test]
fn retries_keep_one_copy_and_a_bad_event_does_not_stop_good_events() {
    let mut fixture = Fixture::new("retry");
    let code = fixture.set_up();
    fixture.start();
    let (who, stamp) = fixture.sign_in(with_code(code), 1);
    let batch = batch(&stamp, &[start(1, 100, 0), start(2, 200, 1), start(3, 300, 0)]);
    let written = batch.write().unwrap();
    let mixed = std::str::from_utf8(written.bytes()).unwrap().replace("\"position_ms\":1", "\"position_ms\":-1");
    assert_ne!(mixed.as_bytes(), written.bytes());
    let first = fixture.post(events::SEND_PATH, Some(&who), mixed.as_bytes());
    let retry = fixture.post(events::SEND_PATH, Some(&who), mixed.as_bytes());
    assert_eq!(first, retry);
    assert_outcomes(first, &batch, &[Outcome::Taken, Outcome::Refused, Outcome::Taken]);
    assert_eq!(count(&fixture, "listening_events"), 2);
    let (other_device, _) = fixture.sign_in(with_password(), 2);
    assert_outcomes(send(&fixture, &other_device, &batch), &batch, &[Outcome::Refused, Outcome::Taken, Outcome::Refused]);
    assert_eq!(count(&fixture, "listening_events"), 3);
    let log = fixture.stop();
    assert!(!log.contains(TITLE));
    assert!(!log.contains("A private artist"));
    assert!(!log.contains("A private album"));
}

#[test]
fn a_password_grant_erases_only_its_owners_history_and_fences_old_life() {
    let mut fixture = Fixture::new("life");
    let code = fixture.set_up();
    let other_code = fixture.other();
    fixture.start();
    let (who, stamp) = fixture.sign_in(with_code(code), 1);
    let (other, other_stamp) = fixture.sign_in(with_code(other_code), 2);
    let old = batch(&stamp, &[start(1, 100, 0)]);
    assert_outcomes(send(&fixture, &who, &old), &old, &[Outcome::Taken]);
    assert_eq!(json::<i64>(&fixture.stats(&other, &other_stamp, None), "$.done.result.starts"), 0);
    let id = RequestId::new(19).unwrap();
    let grant = account::encode(&GrantRequest {
        head: head(&stamp),
        password: Password::presented(PASSWORD).unwrap(),
        change: GrantChange::DeleteListeningData {
            account: who.person.id,
            target_revision: who.person.revision,
            request_id: id,
        },
    }).unwrap();
    assert_eq!(fixture.post(account::GRANT, Some(&other), grant.bytes()).0, 403);
    let (status, body) = fixture.post(account::GRANT, Some(&who), grant.bytes());
    let Reply::Done { result: GrantIssued { grant }, .. } = account::read_reply::<GrantIssued>(status, &body).unwrap() else {
        panic!("grant refused");
    };
    let delete = account::encode(&DeleteListeningData {
        head: head(&stamp),
        request_id: id,
        change: DeleteListeningDataTarget { account: who.person.id, target_revision: who.person.revision },
        grant,
    }).unwrap();
    assert_eq!(fixture.post(account::DELETE_LISTENING_DATA, Some(&other), delete.bytes()).0, 403);
    assert_eq!(count(&fixture, "listening_events"), 1);
    let racing = batch(&stamp, &[start(2, 200, 0)]);
    let barrier = Barrier::new(2);
    let (status, body) = std::thread::scope(|scope| {
        let racing = scope.spawn(|| {
            barrier.wait();
            let (status, body) = send(&fixture, &who, &racing);
            match events::read_reply(status, &body, &racing) {
                Ok(Reply::Done { result, .. }) => assert_eq!(result.outcome(racing.events()[0].record().id()), Outcome::Taken),
                Err(events::Wrong::LifeMoved { life }) => assert_eq!(life, stamp.life + 1),
                _ => panic!("racing batch did not finish"),
            }
        });
        barrier.wait();
        let reply = fixture.post(events::DELETE_PATH, Some(&who), delete.bytes());
        racing.join().unwrap();
        reply
    });
    let Reply::Done { receipt, .. } = account::read_reply::<SignedOut>(status, &body).unwrap()
        else { panic!("deletion refused") };
    assert_eq!(receipt.stamp.life, stamp.life + 1);
    assert_eq!(count(&fixture, "listening_events"), 0);
    assert_eq!(fixture.post(account::DELETE_LISTENING_DATA, Some(&who), delete.bytes()).0, 403);
    let (status, body) = send(&fixture, &who, &old);
    let Reply::Done { result, .. } = account::read_reply::<events::BatchAnswer>(status, &body).unwrap()
        else { panic!("stale batch did not carry the new receipt") };
    assert!(result.answers().is_empty());
    assert_eq!(events::read_reply(status, &body, &old), Err(events::Wrong::LifeMoved { life: receipt.stamp.life }));
    assert_eq!(count(&fixture, "listening_events"), 0);
    let new = batch(&receipt.stamp, &[start(1, 300, 0)]);
    assert_outcomes(send(&fixture, &who, &new), &new, &[Outcome::Taken]);
    assert_eq!(count(&fixture, "listening_events"), 1);
}

#[test]
fn length_is_refused_from_the_head_and_a_slow_body_does_not_hold_the_writer() {
    let mut fixture = Fixture::new("bounds");
    let code = fixture.set_up();
    fixture.start();
    let (who, stamp) = fixture.sign_in(with_code(code), 1);
    for (path, limit) in [(events::SEND_PATH, events::BATCH_BYTES), (diag::SEND_PATH, diag::CALL_BYTES)] {
        let socket = fixture.request(path, Some(&who.session), "", limit + 1);
        // Deliberately send none of the declared body.
        assert_eq!(response(socket).0, 413);
    }
    let slow = fixture.request(events::SEND_PATH, Some(&who.session), "", events::BATCH_BYTES);
    let _ = fixture.sign_in(with_password(), 2);
    drop(slow);

    let rows = (1..=events::BATCH_EVENTS as u64).map(|n| start(n, n, 0)).collect::<Vec<_>>();
    let full = batch(&stamp, &rows);
    let bytes = full.write().unwrap();
    let prefix = std::str::from_utf8(bytes.bytes()).unwrap().strip_suffix("]}").unwrap();
    let extra = String::from_utf8(start(501, 501, 0).line().unwrap()).unwrap();
    let oversized = format!("{prefix},{}]}}", extra.trim_end());
    assert!(oversized.len() < events::BATCH_BYTES);
    let (status, body) = fixture.post(events::SEND_PATH, Some(&who), oversized.as_bytes());
    assert_eq!(account::read_reply::<SignedOut>(status, &body).unwrap(), Reply::Refused(Refusal::Malformed));
    assert_outcomes(send(&fixture, &who, &full), &full, &vec![Outcome::Taken; events::BATCH_EVENTS]);
    let first = fixture.stats(&who, &stamp, None);
    assert_eq!(json::<i64>(&first, "$.done.result.starts"), 500);
    assert_eq!(json::<i64>(&first, "$.done.result.recent[0].event.sequence"), 500);
    let next = DeviceId::new(json::<i64>(&first, "$.done.result.next") as u64);
    let second = fixture.stats(&who, &stamp, next);
    assert_eq!(json::<i64>(&second, "$.done.result.recent[0].event.sequence"), 450);
}

#[test]
fn batch_places_belong_to_live_accounts() {
    let mut fixture = Fixture::new("batch-admission");
    let code = fixture.set_up();
    let other_code = fixture.other();
    fixture.start();
    let (who, stamp) = fixture.sign_in(with_code(code), 1);
    let (other, other_stamp) = fixture.sign_in(with_code(other_code), 2);
    let fake = Session::from_bytes([99; 32]);
    let forged = b"{";
    let mut socket = fixture.request(events::SEND_PATH, Some(&fake), "", forged.len());
    socket.write_all(forged).unwrap();
    // Session admission precedes JSON parsing and does not claim a batch place.
    assert_eq!(response(socket).0, 401);

    let rows = (1..=500).map(|n| start(n, n, 0)).collect::<Vec<_>>();
    let first = batch(&stamp, &rows);
    let second = batch(&other_stamp, &rows);
    let written = first.write().unwrap();
    let body = written.bytes();
    let stale = Batch::prefix(
        RequestHead { restore: stamp.restore + 1, ..head(&stamp) },
        stamp.life, &rows, events::BATCH_BYTES,
    ).unwrap().write().unwrap();
    // A stale restore number is told "data put back" (a real session: on data never put back,
    // a session the data does not have is "no one", refused before its body by the common
    // check; on data put back, it is judged from its body: `admit::Shown::Unproven`).
    let mut socket = fixture.request(events::SEND_PATH, Some(&who.session), "", stale.bytes().len());
    socket.write_all(stale.bytes()).unwrap();
    let (status, stale_reply) = response(socket);
    assert_eq!(account::read_reply::<SignedOut>(status, &stale_reply).unwrap(),
        Reply::Refused(Refusal::NotSignedIn(account::NotSignedIn::DataPutBack)));

    let barrier = Barrier::new(10);
    std::thread::scope(|scope| {
        let first_send = scope.spawn(|| {
            barrier.wait();
            assert_outcomes(send(&fixture, &who, &first), &first, &vec![Outcome::Taken; 500]);
        });
        let second_send = scope.spawn(|| {
            barrier.wait();
            assert_outcomes(send(&fixture, &other, &second), &second, &vec![Outcome::Taken; 500]);
        });
        for _ in 0..8 {
            scope.spawn(|| {
                barrier.wait();
                let mut socket = fixture.request(events::SEND_PATH, Some(&fake), "", body.len());
                socket.write_all(body).unwrap();
                assert_eq!(response(socket).0, 401);
            });
        }
        first_send.join().unwrap();
        second_send.join().unwrap();
    });
    assert_eq!(count(&fixture, "listening_events"), 1_000);
}

fn report(count: u64, populated: bool) -> Report {
    let mut record = diag::Record::default();
    if populated {
        for index in 0..diag::MAX_ENTRIES as u64 {
            let line = diag::Line::DownloadJobStopped(index as u8);
            record.note(1_767_225_600_000 + index * 1_000, line);
        }
        assert_eq!(record.entries().len(), diag::MAX_ENTRIES);
    }
    Report {
        about: About {
            version: Version::parse("0.41.0").unwrap(),
            os: Version::parse("10.0.0").unwrap(),
            device: if populated { DeviceKind::Phone } else { DeviceKind::Pc },
            signed_in: true,
            https: true,
            offline: false,
            local_permission: None,
            android_auto: None,
            loss: events::LossCounts { refused: count, ..Default::default() },
            pc: None,
        },
        record,
    }
}

fn upload(fixture: &Fixture, who: &SignedIn, stamp: &Stamp, n: u64, report: &Report) -> (u16, Vec<u8>) {
    let body = Upload::new(head(stamp), RequestId::new(n).unwrap(), report).unwrap().write().unwrap();
    let (status, response) = fixture.post(diag::SEND_PATH, Some(who), body.bytes());
    assert!(matches!(diag::read_reply(status, &response).unwrap(), Reply::Done { .. }));
    (status, response)
}

#[test]
fn diagnostics_cooldown_spares_the_writer_and_allows_known_device_sign_in() {
    let mut fixture = Fixture::new("report-admission");
    let code = fixture.set_up();
    let clock = fixture.start_with_clock();
    let (who, stamp) = fixture.sign_in(with_code(code), 1);
    let (other_device, other_stamp) = fixture.sign_in(with_password(), 2);
    upload(&fixture, &who, &stamp, 1, &report(1, true));
    // The cooldown is per device, including for devices on the same account.
    upload(&fixture, &other_device, &other_stamp, 1, &report(1, true));
    let next = Upload::new(head(&stamp), RequestId::new(2).unwrap(), &report(2, true)).unwrap().write().unwrap();
    let changes = || fixture.db().query_row("SELECT changes FROM meta", [], |row| row.get::<_, i64>(0)).unwrap();
    let before = changes();
    let (status, body) = fixture.post(diag::SEND_PATH, Some(&who), next.bytes());
    assert!(matches!(diag::read_reply(status, &body).unwrap(), Reply::Refused(Refusal::Busy { .. })));
    let head_end = next.bytes().iter().position(|&byte| byte == b'}').unwrap();
    let (status, _) = fixture.post(diag::SEND_PATH, Some(&who), &next.bytes()[..=head_end]);
    assert_eq!(status, 429, "a cooling device is refused before report parsing");
    assert_eq!(changes(), before);

    let barrier = Barrier::new(13);
    std::thread::scope(|scope| {
        let sign_in = scope.spawn(|| {
            barrier.wait();
            fixture.sign_in(with_password(), 2)
        });
        for _ in 0..12 {
            scope.spawn(|| {
                barrier.wait();
                let (status, body) = fixture.post(diag::SEND_PATH, Some(&who), next.bytes());
                assert!(matches!(diag::read_reply(status, &body).unwrap(), Reply::Refused(Refusal::Busy { .. })));
            });
        }
        sign_in.join().unwrap();
    });
    assert_eq!(count(&fixture, "diagnostic_reports"), 2);

    clock.fetch_add(diag::MIN_GAP_MS - 1, Ordering::SeqCst);
    let (status, body) = fixture.post(diag::SEND_PATH, Some(&who), next.bytes());
    assert_eq!(diag::read_reply(status, &body).unwrap(), Reply::Refused(Refusal::Busy {
        retry_after_seconds: account::Wait::new(1),
    }));
    clock.fetch_add(1, Ordering::SeqCst);
    upload(&fixture, &who, &stamp, 2, &report(2, true));
    assert_eq!(count(&fixture, "diagnostic_reports"), 3);
}

#[test]
fn diagnostics_refuse_private_text_without_echo() {
    let mut fixture = Fixture::new("diag-private");
    let code = fixture.set_up();
    fixture.start();
    let (who, stamp) = fixture.sign_in(with_code(code), 1);
    let written = Upload::new(head(&stamp), RequestId::new(1).unwrap(), &report(1, false)).unwrap().write().unwrap();
    let dirty = std::str::from_utf8(written.bytes()).unwrap().replace("0.41.0 on PC", "private-title on PC");
    assert_ne!(dirty.as_bytes(), written.bytes());
    let (status, body) = fixture.post(diag::SEND_PATH, Some(&who), dirty.as_bytes());
    assert_eq!(diag::read_reply(status, &body).unwrap(), Reply::Refused(Refusal::Malformed));
    assert!(!String::from_utf8(body).unwrap().contains("private-title"));
    assert!(!fixture.stop().contains("private-title"));
}

#[test]
fn diagnostics_keep_the_newest_per_device_and_require_a_page_session_for_text() {
    let mut fixture = Fixture::new("diag");
    let code = fixture.set_up();
    let other_code = fixture.other();
    let clock = fixture.start_with_clock();
    let (who, stamp) = fixture.sign_in(with_code(code), 1);
    let (other, other_stamp) = fixture.sign_in(with_code(other_code), 2);
    upload(&fixture, &who, &stamp, 1, &report(1, false));
    clock.fetch_add(diag::MIN_GAP_MS, Ordering::SeqCst);
    upload(&fixture, &who, &stamp, 1, &report(1, false));
    assert_eq!(count(&fixture, "diagnostic_reports"), 1);
    for n in 2..=diag::STORED_REPORTS as u64 + 1 {
        upload(&fixture, &who, &stamp, n, &report(n, false));
        clock.fetch_add(diag::MIN_GAP_MS, Ordering::SeqCst);
    }
    assert_eq!(count(&fixture, "diagnostic_reports"), diag::STORED_REPORTS as i64);
    let db = fixture.db();
    let oldest: String = db.query_row("SELECT text FROM diagnostic_reports ORDER BY row_id LIMIT 1", [], |row| row.get(0)).unwrap();
    assert_eq!(oldest, report(2, false).text().unwrap());
    upload(&fixture, &other, &other_stamp, 1, &report(50, false));
    let (status, body) = fixture.post(nexus_music_server::diag::LIST_PATH, Some(&who), page(&stamp, None).bytes());
    assert_eq!(status, 200);
    assert_eq!(json::<i64>(&body, "$.done.result.reports[30].id"), 2);
    assert_eq!(json::<Option<i64>>(&body, "$.done.result.reports[31].id"), None);
    assert_eq!(json::<Option<String>>(&body, "$.done.result.reports[0].text"), None);

    let path = nexus_music_server::diag::ADMIN_PATH;
    assert_eq!(fixture.post(path, Some(&who), page(&stamp, None).bytes()).0, 403);
    assert_eq!(fixture.post_with(path, Some(&other), BROWSER, page(&other_stamp, None).bytes()).0, 403);
    assert_eq!(fixture.post_with(path, Some(&who), BROWSER, page(&stamp, None).bytes()).0, 403);
    let (admin, admin_stamp) = fixture.sign_in_with(with_password(), 3, BROWSER);
    let (status, body) = fixture.post_with(path, Some(&admin), BROWSER, page(&admin_stamp, None).bytes());
    assert_eq!(status, 200);
    assert_eq!(json::<String>(&body, "$.done.result.reports[0].text"), report(50, false).text().unwrap());
    assert_eq!(json::<i64>(&body, "$.done.result.reports[0].account"), other.person.id.get() as i64);
    let event = batch(&other_stamp, &[start(1, 100, 0)]);
    assert_outcomes(send(&fixture, &other, &event), &event, &[Outcome::Taken]);
    let stats_path = nexus_music_server::events::STATS_PATH;
    assert_eq!(fixture.post_with(
        stats_path, Some(&admin), BROWSER, page(&admin_stamp, DeviceId::new(1)).bytes(),
    ).0, 403);
    let stats = fixture.stats(&who, &stamp, DeviceId::new(1));
    assert_eq!(json::<i64>(&stats, "$.done.result.starts"), 0);
    assert_eq!(json::<Option<String>>(&stats, "$.done.result.recent[0].event"), None);
}

#[test]
fn restart_keeps_acknowledged_events_and_reports() {
    let mut fixture = Fixture::new("restart");
    let code = fixture.set_up();
    fixture.start();
    let (who, stamp) = fixture.sign_in(with_code(code), 1);
    let kept = batch(&stamp, &[start(1, 100, 0)]);
    assert_outcomes(send(&fixture, &who, &kept), &kept, &[Outcome::Taken]);
    upload(&fixture, &who, &stamp, 1, &report(1, false));
    fixture.stop();
    fixture.start();
    assert_eq!(json::<i64>(&fixture.stats(&who, &stamp, None), "$.done.result.starts"), 1);
    assert_outcomes(send(&fixture, &who, &kept), &kept, &[Outcome::Taken]);
    let (status, body) = fixture.post(nexus_music_server::diag::LIST_PATH, Some(&who), page(&stamp, None).bytes());
    assert_eq!(status, 200);
    assert_eq!(json::<i64>(&body, "$.done.result.reports[0].id"), 1);
    assert_eq!(count(&fixture, "diagnostic_reports"), 1);
}

#[test]
fn the_writer_expires_old_reports_without_another_upload_from_the_device() {
    let mut fixture = Fixture::new("expiry");
    let code = fixture.set_up();
    let clock = fixture.start_with_clock();
    let (who, stamp) = fixture.sign_in(with_code(code), 1);
    upload(&fixture, &who, &stamp, 1, &report(1, false));
    clock.fetch_add(diag::MIN_GAP_MS, Ordering::SeqCst);
    upload(&fixture, &who, &stamp, 2, &report(2, false));

    // Leave the data mark due for the next idle turn, then advance past the
    // first report's lifetime. The device never uploads another report.
    let sent = batch(&stamp, &[start(1, 100, 0)]);
    assert_outcomes(send(&fixture, &who, &sent), &sent, &[Outcome::Taken]);
    clock.fetch_add(diag::MAX_AGE_MS - diag::MIN_GAP_MS + 1, Ordering::SeqCst);
    let (status, body) = fixture.post(nexus_music_server::diag::LIST_PATH, Some(&who), page(&stamp, None).bytes());
    assert_eq!(status, 200);
    assert_eq!(json::<i64>(&body, "$.done.result.reports[0].id"), 2);
    assert_eq!(json::<Option<i64>>(&body, "$.done.result.reports[1].id"), None);
    assert_eq!(count(&fixture, "diagnostic_reports"), 1);
}

fn fill_test_database(fixture: &Fixture) {
    fixture.db().execute_batch(
        "CREATE TABLE budget_fixture(reserved BLOB);
         INSERT INTO budget_fixture VALUES (zeroblob(1000000));",
    ).unwrap();
}

#[test]
fn invalid_budget_refuses_to_start() {
    let fixture = Fixture::new("invalid-budget");
    let output = fixture.program().env("NEXUS_DATA_BUDGET", "four").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr).unwrap().contains("NEXUS_DATA_BUDGET is wrong"));
}

#[test]
fn a_full_budget_defers_new_events_without_losing_existing_ones() {
    let mut fixture = Fixture::new("budget");
    let code = fixture.set_up();
    fixture.start();
    let (who, stamp) = fixture.sign_in(with_code(code), 1);
    let kept = batch(&stamp, &[start(1, 100, 0)]);
    assert_outcomes(send(&fixture, &who, &kept), &kept, &[Outcome::Taken]);
    fixture.stop();
    fill_test_database(&fixture);
    fixture.budget = Some(1_000_000);
    fixture.start();
    let mixed = batch(&stamp, &[start(1, 100, 0), start(2, 200, 0)]);
    assert_outcomes(send(&fixture, &who, &mixed), &mixed, &[Outcome::Taken, Outcome::NotDealtWith]);
    assert_eq!(count(&fixture, "listening_events"), 1);
}

#[test]
fn crossing_the_budget_rolls_back_only_the_rows_that_do_not_fit() {
    let mut fixture = Fixture::new("budget-crossing");
    let code = fixture.set_up();
    fixture.start();
    let (who, stamp) = fixture.sign_in(with_code(code), 1);
    fixture.stats(&who, &stamp, None);
    fixture.stop();
    fill_test_database(&fixture);
    let cap = nexus_music_server::events::data_bytes(&fixture.db()).unwrap() + 1;
    fixture.budget = Some(cap);
    fixture.start();
    let rows = (1..=500).map(|n| start(n, n, 0)).collect::<Vec<_>>();
    let sent = batch(&stamp, &rows);
    let (status, body) = send(&fixture, &who, &sent);
    let Reply::Done { result, .. } = events::read_reply(status, &body, &sent).unwrap()
        else { panic!("batch not done") };
    let taken = result.answers().iter().filter(|row| row.outcome == Outcome::Taken).count();
    assert!((1..500).contains(&taken));
    assert!(result.answers()[taken..].iter().all(|row| row.outcome == Outcome::NotDealtWith));
    assert_eq!(count(&fixture, "listening_events"), taken as i64);
    assert_eq!(json::<i64>(&fixture.stats(&who, &stamp, None), "$.done.result.starts"), taken as i64);
    assert!(nexus_music_server::events::data_bytes(&fixture.db()).unwrap() <= cap);
}

#[test]
#[ignore = "measure SQLite pages and HTTP timing on demand"]
fn measure_storage_for_events_and_reports() {
    let mut fixture = Fixture::new("measure");
    let code = fixture.set_up();
    let clock = fixture.start_with_clock();
    let (who, stamp) = fixture.sign_in(with_code(code), 1);
    let bytes = || nexus_music_server::events::data_bytes(&fixture.db()).unwrap();
    fixture.stats(&who, &stamp, None);
    let baseline = bytes();
    let mut wire_bytes = 0;
    let mut milliseconds = Vec::new();
    for range in 0..10 {
        let records = (range * 500 + 1..=range * 500 + 500).map(|n| {
            if n % 2 == 1 {
                start(n, 1_767_225_600_000 + n * 2_000, 0)
            } else {
                Record::new(id(n), 1_767_225_600_000 + n * 2_000, Kind::Stop {
                    start: id(n - 1), position_ms: Some(180_000), duration_ms: Some(180_000),
                    heard_ms: Some(180_000), why: Stopped::Finished,
                }).unwrap().place(n).unwrap()
            }
        }).collect::<Vec<_>>();
        wire_bytes += records.iter().map(|event| event.line().unwrap().len()).sum::<usize>();
        let batch = batch(&stamp, &records);
        let at = Instant::now();
        assert_outcomes(send(&fixture, &who, &batch), &batch, &vec![Outcome::Taken; 500]);
        milliseconds.push(at.elapsed().as_secs_f64() * 1000.0);
    }
    let event_use = bytes() - baseline;
    upload(&fixture, &who, &stamp, 1, &report(1, true));
    let after_first = bytes();
    let mut report_bytes = 0;
    for n in 2..=31 {
        clock.fetch_add(diag::MIN_GAP_MS, Ordering::SeqCst);
        let report = report(n, true);
        report_bytes += report.text().unwrap().len();
        upload(&fixture, &who, &stamp, n, &report);
    }
    let report_use = bytes() - after_first;
    milliseconds.sort_by(f64::total_cmp);
    eprintln!("5000 events: {wire_bytes} bytes JSON, {event_use} allocated bytes. 500-event HTTP median {:.1} ms and max {:.1} ms (includes reply verification). 30 reports: {report_bytes} text bytes, {report_use} allocated bytes.",
        milliseconds[5], milliseconds[9]);
}

#[test]
fn admin_accounts_require_authority_and_a_live_page_session() {
    use nexus_core::nexus::admin;

    let mut fixture = Fixture::new("admin-accounts-idle");
    let admin_code = fixture.set_up();
    let other_code = fixture.other();
    let clock = fixture.start_with_clock();
    fixture.sign_in_with(with_code(admin_code), 1, BROWSER);
    let (other, other_stamp) = fixture.sign_in(with_code(other_code), 2);
    let list = |stamp: &Stamp| account::encode(&admin::ListAccounts {
        head: head(stamp),
        after: None,
    }).unwrap();

    assert_eq!(fixture.post(admin::LIST_ACCOUNTS, Some(&other), list(&other_stamp).bytes()).0, 403);
    let change = |number| account::encode(&admin::SetAccountEnabled {
        head: head(&other_stamp),
        request_id: RequestId::new(1).unwrap(),
        grant: account::GrantSecret::from_bytes([0; 32]),
        change: admin::AccountEnabled { account: account::AccountId::new(number).unwrap(), enabled: false },
    }).unwrap();
    let post = |number| fixture.post(admin::SET_ACCOUNT_ENABLED, Some(&other), change(number).bytes());
    let denied = post(other.person.id.get());
    assert_eq!(denied.0, 403);
    assert_eq!(denied, post(9001));

    let (admin, stamp) = fixture.sign_in_with(with_password(), 3, BROWSER);
    let body = list(&stamp);
    let (status, rows) = fixture.post_with(admin::LIST_ACCOUNTS, Some(&admin), BROWSER, body.bytes());
    assert_eq!(status, 200);
    assert_eq!(json::<i64>(&rows, "$.done.rows[0].id"), admin.person.id.get() as i64);
    assert_eq!(json::<String>(&rows, "$.done.rows[0].music"), "none_set");
    assert_eq!(json::<Option<String>>(&rows, "$.done.rows[0].password"), None);

    clock.fetch_add(admin::PAGE_SESSION_IDLE_MS + 1, Ordering::SeqCst);
    let (status, refusal) = fixture.post_with(admin::LIST_ACCOUNTS, Some(&admin), BROWSER, body.bytes());
    assert_eq!(status, 401);
    assert_eq!(json::<String>(&refusal, "$.refused.not_signed_in"), "session_ended");
}

#[test]
fn the_diagnostics_default_needs_a_matching_grant_and_reaches_hello() {
    use nexus_core::nexus::admin;

    let mut fixture = Fixture::new("admin-diagnostics");
    let code = fixture.set_up();
    fixture.start_with_clock();
    fixture.sign_in_with(with_code(code), 1, BROWSER);
    let (admin, stamp) = fixture.sign_in_with(with_password(), 2, BROWSER);
    let request_id = RequestId::new(41).unwrap();
    let request = account::encode(&admin::RequestGrant {
        head: head(&stamp),
        change: admin::NamedChange {
            request_id,
            action: admin::Action::SetDiagnosticsDefault(admin::DiagnosticsDefault { enabled: false }),
        },
        password: Password::presented(PASSWORD).unwrap(),
    }).unwrap();
    let (status, body) = fixture.post_with(admin::REQUEST_GRANT, Some(&admin), BROWSER, request.bytes());
    let grant = match admin::read_reply::<GrantIssued>(status, &body).unwrap() {
        admin::Reply::Done(issued) => issued.grant,
        _ => panic!("no grant issued"),
    };
    let change = |enabled| account::encode(&admin::SetDiagnosticsDefault {
        head: head(&stamp),
        request_id,
        grant: account::GrantSecret::from_bytes(*grant.expose_bytes()),
        change: admin::DiagnosticsDefault { enabled },
    }).unwrap();
    let wrong = change(true);
    let (status, body) = fixture.post_with(admin::SET_DIAGNOSTICS_DEFAULT, Some(&admin), BROWSER, wrong.bytes());
    assert_eq!(status, 403);
    assert_eq!(json::<String>(&body, "$.refused.not_allowed"), "grant_required");
    let change = change(false);
    let post = || fixture.post_with(admin::SET_DIAGNOSTICS_DEFAULT, Some(&admin), BROWSER, change.bytes());
    assert_eq!(post().0, 200);
    assert_eq!(post().0, 403);

    let mut socket = TcpStream::connect(("127.0.0.1", fixture.port)).unwrap();
    socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    socket.write_all(b"GET /nexus/api/hello HTTP/1.1\r\nHost: nexus\r\nX-Nexus-Scheme: https\r\nConnection: close\r\n\r\n").unwrap();
    let (status, hello) = response(socket);
    assert_eq!(status, 200);
    assert_eq!(json::<bool>(&hello, "$.diagnostics"), false);
}

#[test]
fn admin_devices_show_live_page_sessions_and_omit_forced_out_or_idle_pages() {
    use nexus_core::nexus::admin;

    let mut fixture = Fixture::new("admin-page-devices");
    let code = fixture.set_up();
    let clock = fixture.start_with_clock();
    let (admin, stamp) = fixture.sign_in_with(with_code(code), 1, BROWSER);
    let (other, _) = fixture.sign_in_with(with_password(), 2, BROWSER);
    // This App session exercises only the native device list.
    let (app, app_stamp) = fixture.sign_in(with_password(), 3);

    let devices = |admin, stamp: &Stamp| {
        let request = admin::encode(&admin::ListDevices { head: head(stamp), after: None }).unwrap();
        let (status, body) = fixture.post_with(admin::LIST_DEVICES, Some(admin), BROWSER, request.bytes());
        match admin::read_reply::<admin::Devices>(status, &body).unwrap() {
            admin::Reply::Done(devices) => devices,
            _ => panic!("admin devices were refused"),
        }
    };
    let live = devices(&admin, &stamp);
    let is_other = |row: &admin::AdminDevice| row.device == other.device;
    assert_eq!(live.rows().len(), 3);
    let browser_row = live.rows().iter().find(|row| is_other(row)).unwrap();
    assert!(browser_row.page);
    assert_eq!(browser_row.session, Some(other.session_id));
    assert_eq!(browser_row.name.as_str(), "Admin page");
    assert!(browser_row.last_seen_ms > 0);
    let (status, native) = fixture.post(account::LIST_DEVICES, Some(&app), page(&app_stamp, None).bytes());
    assert_eq!(status, 200);
    assert_eq!(json::<i64>(&native, "$.done.result.devices[0].id"), app.device.get() as i64);
    assert_eq!(json::<Option<i64>>(&native, "$.done.result.devices[1].id"), None);

    let request_id = RequestId::new(41).unwrap();
    let target = || admin::SessionTarget {
        account: other.person.id, device: other.device, session: other.session_id,
    };
    let request = admin::encode(&admin::RequestGrant {
        head: head(&stamp),
        change: admin::NamedChange { request_id, action: admin::Action::ForceOut(target()) },
        password: Password::presented(PASSWORD).unwrap(),
    }).unwrap();
    let (status, body) = fixture.post_with(admin::REQUEST_GRANT, Some(&admin), BROWSER, request.bytes());
    let grant = match admin::read_reply::<GrantIssued>(status, &body).unwrap() {
        admin::Reply::Done(issued) => issued.grant,
        _ => panic!("no grant was issued"),
    };
    let force = admin::encode(&admin::ForceOut { head: head(&stamp), request_id, grant, change: target() }).unwrap();
    assert_eq!(fixture.post_with(admin::FORCE_OUT, Some(&admin), BROWSER, force.bytes()).0, 200);
    assert_eq!(devices(&admin, &stamp).rows().len(), 2);
    let list = admin::encode(&admin::ListDevices { head: head(&stamp), after: None }).unwrap();
    assert_eq!(fixture.post_with(admin::LIST_DEVICES, Some(&other), BROWSER, list.bytes()).0, 401);

    clock.fetch_add(admin::PAGE_SESSION_IDLE_MS, Ordering::SeqCst);
    let (fresh, fresh_stamp) = fixture.sign_in_with(with_password(), 4, BROWSER);
    let remaining = devices(&fresh, &fresh_stamp);
    assert_eq!(remaining.rows().len(), 2);
    assert_eq!(remaining.rows().iter().find(|row| row.page).unwrap().session, Some(fresh.session_id));
    assert!(remaining.rows().iter().any(|row| row.device == app.device && !row.page));
}

#[test]
fn a_page_cannot_spend_a_grant_that_locks_out_its_own_administrator() {
    let mut fixture = Fixture::new("protect-own-admin");
    let code = fixture.set_up();
    fixture.start_with_clock();
    let (admin, stamp) = fixture.sign_in_with(with_code(code), 1, BROWSER);
    let account = admin.person.id.get();
    let restore = stamp.restore;
    let off = format!(r#"{{"account":{account},"enabled":false}}"#);
    let demote = format!(r#"{{"account":{account},"admin":false}}"#);
    let remove = format!(r#"{{"account":{account}}}"#);
    let remove_target = account.to_string();
    for (id, (action, target, change)) in [
        ("set_account_enabled", off.as_str(), off.as_str()),
        ("delete_account", remove_target.as_str(), remove.as_str()),
        ("set_admin", demote.as_str(), demote.as_str()),
    ].into_iter().enumerate() {
        let request_id = id + 1;
        let request = format!(
            r#"{{"head":{{"speaks":[1,1],"restore":{restore}}},"change":{{"request_id":{request_id},"action":{{"{action}":{target}}}}},"password":"{PASSWORD}"}}"#,
        );
        let (status, grant) = fixture.post_with(nexus_core::nexus::admin::REQUEST_GRANT, Some(&admin), BROWSER, request.as_bytes());
        assert_eq!(status, 200, "grant issued for the spend test");
        let grant = json::<String>(&grant, "$.done.grant");
        let path = format!("/nexus/api/admin/{}", action.replace('_', "-"));
        let request = format!(
            r#"{{"head":{{"speaks":[1,1],"restore":{restore}}},"request_id":{request_id},"grant":"{grant}","change":{change}}}"#,
        );
        let (status, refusal) = fixture.post_with(&path, Some(&admin), BROWSER, request.as_bytes());
        assert_eq!(status, 403);
        assert_eq!(json::<String>(&refusal, "$.refused.not_allowed"), "denied");
    }
    let (again, _) = fixture.sign_in_with(with_password(), 2, BROWSER);
    assert_eq!(again.person.id, admin.person.id);
    assert!(again.person.admin);
}
