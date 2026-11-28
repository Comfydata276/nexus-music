//! S12 and S7 over the program's real HTTP listener. SQLite reads inspect committed
//! contents when a wire acknowledgement alone cannot prove the state.

use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
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
use nexus_music_server::accounts::setup::{CodeKey, make_code};
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
    budget: Option<u64>,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("nexus-journals-{}-{name}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(root.join("data")).unwrap();
        std::fs::create_dir(root.join("state")).unwrap();
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        Self { root, port, child: None, budget: None }
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
        OneTimeCode::new(&code[..13].replace('-', "")).unwrap()
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
        let request = account::encode(&SignIn {
            credential,
            device_secret: DeviceSecret::from_bytes([device; 32]),
            device_name: Name::new("Local device name").unwrap(),
            device_kind: DeviceKind::Pc,
            app_version: Version::parse("0.41.0").unwrap(),
            speaks: SPEAKS,
            restore: None,
        }).unwrap();
        let (status, body) = self.post(account::SIGN_IN, None, request.bytes());
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
    let (status, body) = fixture.post(events::DELETE_PATH, Some(&who), delete.bytes());
    let Reply::Done { receipt, .. } = account::read_reply::<SignedOut>(status, &body).unwrap()
        else { panic!("deletion refused") };
    assert_eq!(receipt.stamp.life, stamp.life + 1);
    assert_eq!(count(&fixture, "listening_events"), 0);
    assert_eq!(fixture.post(account::DELETE_LISTENING_DATA, Some(&who), delete.bytes()).0, 403);
    let (status, body) = send(&fixture, &who, &old);
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

fn report(count: u64, populated: bool) -> Report {
    let mut record = diag::Record::default();
    if populated {
        for index in 0..diag::MAX_ENTRIES as u64 {
            let note = if index % 2 == 0 { diag::Note::AppMessage } else { diag::Note::DownloadsFolderChanged };
            record.note(1_767_225_600_000 + index * 1_000, diag::Line::Note(note));
        }
    }
    Report {
        about: About {
            version: Version::parse("0.41.0").unwrap(),
            os: Version::parse("10.0.0").unwrap(),
            device: DeviceKind::Pc,
            signed_in: true,
            https: true,
            offline: false,
            local_permission: None,
            android_auto: None,
            loss: events::LossCounts { refused: count, ..Default::default() },
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
fn diagnostics_refuse_private_text_and_keep_the_newest_per_device() {
    let mut fixture = Fixture::new("diag");
    let code = fixture.set_up();
    let other_code = fixture.other();
    fixture.start();
    let (who, stamp) = fixture.sign_in(with_code(code), 1);
    let (other, other_stamp) = fixture.sign_in(with_code(other_code), 2);
    let written = Upload::new(head(&stamp), RequestId::new(1).unwrap(), &report(1, false)).unwrap().write().unwrap();
    let dirty = std::str::from_utf8(written.bytes()).unwrap().replace("0.41.0 on PC", "private-title on PC");
    assert_ne!(dirty.as_bytes(), written.bytes());
    let (status, body) = fixture.post(diag::SEND_PATH, Some(&who), dirty.as_bytes());
    assert_eq!(diag::read_reply(status, &body).unwrap(), Reply::Refused(Refusal::Malformed));
    assert!(!String::from_utf8(body).unwrap().contains("private-title"));
    let first = upload(&fixture, &who, &stamp, 1, &report(1, false));
    assert_eq!(upload(&fixture, &who, &stamp, 1, &report(1, false)), first);
    assert_eq!(count(&fixture, "diagnostic_reports"), 1);
    for n in 2..=diag::STORED_REPORTS as u64 + 1 {
        upload(&fixture, &who, &stamp, n, &report(n, false));
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
    let (status, body) = fixture.post_with(path, Some(&who), BROWSER, page(&stamp, None).bytes());
    assert_eq!(status, 200);
    assert_eq!(json::<String>(&body, "$.done.result.reports[0].text"), report(50, false).text().unwrap());
    assert_eq!(json::<i64>(&body, "$.done.result.reports[0].account"), other.person.id.get() as i64);
    assert!(!fixture.stop().contains("private-title"));
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
    let (status, body) = fixture.post_with(nexus_music_server::diag::ADMIN_PATH, Some(&who), BROWSER, page(&stamp, None).bytes());
    assert_eq!(status, 200);
    assert_eq!(json::<String>(&body, "$.done.result.reports[0].text"), report(1, false).text().unwrap());
    assert_eq!(count(&fixture, "diagnostic_reports"), 1);
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
    fixture.budget = Some(1);
    fixture.start();
    let mixed = batch(&stamp, &[start(1, 100, 0), start(2, 200, 0)]);
    assert_outcomes(send(&fixture, &who, &mixed), &mixed, &[Outcome::Taken, Outcome::NotDealtWith]);
    assert_eq!(count(&fixture, "listening_events"), 1);
}

#[test]
#[ignore = "measure SQLite pages and HTTP timing on demand"]
fn measure_storage_for_events_and_reports() {
    let mut fixture = Fixture::new("measure");
    let code = fixture.set_up();
    fixture.start();
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
        let report = report(n, true);
        report_bytes += report.text().unwrap().len();
        upload(&fixture, &who, &stamp, n, &report);
    }
    let report_use = bytes() - after_first;
    milliseconds.sort_by(f64::total_cmp);
    eprintln!("5000 events: {wire_bytes} bytes JSON, {event_use} allocated bytes. 500-event HTTP median {:.1} ms and max {:.1} ms (includes reply verification). 30 reports: {report_bytes} text bytes, {report_use} allocated bytes.",
        milliseconds[5], milliseconds[9]);
}
