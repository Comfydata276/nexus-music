//! Signing in, end to end (slice S8; design 8.4 to 8.9): the program itself, as the container
//! runs it, on this machine's loopback, spoken to over HTTP as the proxy speaks to it (one
//! request to a connection, marked as having come by HTTPS). The account is made by the
//! owner's `set-up` command, with no browser; its person redeems the code with a password of
//! their own, signs in with it, is accepted, signs out, and is refused after. Made-up names.

use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nexus_core::nexus::account::{
    self, Credential, DeviceKind, DeviceSecret, Email, ForceOut, ListDevices, MusicLoginResult, Name, NotAllowed, NotSignedIn, OneTimeCode, Password,
    Refusal, Reply, RequestHead, RequestId, SessionId, SignIn, SignOut, SignedIn,
};
use nexus_core::nexus::proto::{SPEAKS, Version};

const PROGRAM: &str = env!("CARGO_BIN_EXE_nexus-music-server");

struct Folders {
    root: PathBuf,
}

impl Folders {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("nexus-server-signin-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        Folders { root }
    }

    fn settings(&self, port: u16) -> Vec<(&'static str, String)> {
        vec![
            ("NEXUS_MUSIC_SERVER", "http://navidrome:4533".into()),
            // The proxy is this machine's loopback, so every request carries the proxy's mark.
            ("NEXUS_PROXY", "127.0.0.1".into()),
            ("NEXUS_DATA", self.root.join("data").display().to_string()),
            ("NEXUS_STATE", self.root.join("state").display().to_string()),
            ("NEXUS_LISTEN", format!("127.0.0.1:{port}")),
        ]
    }
}

impl Drop for Folders {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn program(settings: &[(&'static str, String)], arguments: &[&str]) -> Command {
    let mut command = Command::new(PROGRAM);
    command.env_clear().args(arguments);
    for (name, value) in settings {
        command.env(name, value);
    }
    command
}

/// The backend serving, stopped (and what it wrote kept) when dropped.
struct Serving {
    child: Option<Child>,
    at: SocketAddr,
    written: String,
}

impl Serving {
    fn start(settings: &[(&'static str, String)], port: u16) -> Serving {
        let child = program(settings, &[]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let at: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let serving = Serving { child: Some(child), at, written: String::new() };
        let start = Instant::now();
        while TcpStream::connect(at).is_err() {
            assert!(start.elapsed() < Duration::from_secs(20), "it never listened");
            std::thread::sleep(Duration::from_millis(50));
        }
        serving
    }

    fn stop(&mut self) -> &str {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            let mut written = String::new();
            let _ = child.stdout.take().unwrap().read_to_string(&mut written);
            let _ = child.stderr.take().unwrap().read_to_string(&mut written);
            self.written = written;
        }
        &self.written
    }

    /// One POST, as the proxy sends it, and its status and body.
    fn post(&self, path: &str, session: Option<&account::Session>, body: &[u8]) -> (u16, Vec<u8>) {
        let mut stream = TcpStream::connect(self.at).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let authorization = session.map_or_else(String::new, |session| format!("Authorization: {}\r\n", session.authorization().as_str()));
        let head = format!(
            "POST {path} HTTP/1.1\r\nHost: nexus\r\nX-Nexus-Scheme: https\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{authorization}Connection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        let mut from = BufReader::new(stream);
        let head = nexus_music_server::probe::read_head(&mut from).unwrap().expect("an answer");
        let mut bytes = Vec::new();
        nexus_music_server::probe::body(&head, from).unwrap().read_to_end(&mut bytes).unwrap();
        (head.status, bytes)
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.stop();
    }
}

fn speaks(restore: u64) -> RequestHead {
    RequestHead { speaks: SPEAKS, restore }
}

fn sign_in(credential: Credential, device: u8, name: &str, kind: DeviceKind) -> Vec<u8> {
    let request = SignIn {
        credential,
        device_secret: DeviceSecret::from_bytes([device; 32]),
        device_name: Name::new(name).unwrap(),
        device_kind: kind,
        app_version: Version::parse("0.41.0").unwrap(),
        speaks: SPEAKS,
        restore: None,
    };
    account::encode(&request).unwrap().bytes().to_vec()
}

fn with_password(email: &str, password: &str) -> Credential {
    Credential::Password { email: Email::new(email).unwrap(), password: Password::presented(password).unwrap() }
}

fn signed_in(status: u16, body: &[u8]) -> SignedIn {
    match account::read_reply::<SignedIn>(status, body).unwrap() {
        Reply::Done { result, receipt } => {
            assert_eq!(receipt.stamp.account, result.person.id);
            result
        }
        other => panic!("not signed in: {other:?}"),
    }
}

fn refusal<T>(reply: Reply<T>) -> Refusal {
    match reply {
        Reply::Refused(refusal) => refusal,
        _ => panic!("not refused"),
    }
}

/// The owner's `set-up`: the first admin, and the one-time code said on its own terminal.
fn set_up(settings: &[(&'static str, String)], email: &str) -> OneTimeCode {
    let output = program(settings, &["set-up", email, "Robin"]).output().unwrap();
    let said = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(output.status.success(), "{said}{}", String::from_utf8_lossy(&output.stderr));
    let at = said.find("one-time code ").expect("a code") + "one-time code ".len();
    let code: String = said[at..at + 13].chars().filter(|c| *c != '-').collect();
    OneTimeCode::new(&code).unwrap()
}

const EMAIL: &str = "robin@example.org";
const PASSWORD: &str = "a made-up password of robin's";

#[test]
fn a_person_signs_in_end_to_end_and_is_refused_once_signed_out() {
    let folders = Folders::new("milestone");
    let port = free_port();
    let settings = folders.settings(port);
    let code = set_up(&settings, EMAIL);
    let typed = code.expose().to_string();
    let mut backend = Serving::start(&settings, port);

    // The first sign-in, on the PC, with the code and the password chosen with it (8.5).
    let (status, body) = backend.post(account::SIGN_IN, None, &sign_in(Credential::Code { code, new_password: Password::new(PASSWORD).unwrap() }, 1, "Robin's PC", DeviceKind::Pc));
    let first = signed_in(status, &body);
    assert!(first.person.admin);
    assert_eq!(first.person.email.as_str(), EMAIL);
    assert_eq!(first.music, MusicLoginResult::NoneSetYet);

    // The phone, with the email and password (8.6).
    let (status, body) = backend.post(account::SIGN_IN, None, &sign_in(with_password(EMAIL, PASSWORD), 2, "Robin's phone", DeviceKind::Phone));
    let phone = signed_in(status, &body);
    assert_ne!(phone.device, first.device);
    let restore = phone.restore;

    // An authenticated call is accepted: the list of devices, each with its session (8.7).
    let list = account::encode(&ListDevices { head: speaks(restore), after: None }).unwrap();
    let (status, body) = backend.post(account::LIST_DEVICES, Some(&phone.session), list.bytes());
    let Reply::Done { result: page, .. } = account::read_reply::<account::DevicePage>(status, &body).unwrap() else { panic!("{status}") };
    assert_eq!(page.devices().len(), 2);
    let own = page.devices().iter().find(|device| device.this_device).unwrap();
    assert_eq!((own.id, own.session), (phone.device, Some(phone.session_id)));
    assert_eq!(page.devices().iter().find(|device| !device.this_device).unwrap().session, Some(first.session_id));

    // Signing out on the phone, then the phone's session is refused: "this session has ended".
    let out = account::encode(&SignOut { head: speaks(restore), request_id: RequestId::new(1).unwrap(), revoke: None }).unwrap();
    let (status, body) = backend.post(account::SIGN_OUT, Some(&phone.session), out.bytes());
    assert!(matches!(account::read_reply::<account::SignedOut>(status, &body).unwrap(), Reply::Done { .. }), "{status}");
    let (status, body) = backend.post(account::LIST_DEVICES, Some(&phone.session), list.bytes());
    assert_eq!(status, 401);
    assert_eq!(refusal(account::read_reply::<account::DevicePage>(status, &body).unwrap()), Refusal::NotSignedIn(NotSignedIn::SessionEnded));
    // The PC's is untouched.
    let (status, _) = backend.post(account::LIST_DEVICES, Some(&first.session), list.bytes());
    assert_eq!(status, 200);

    // Nothing worth protecting was written to the log: no code, no session, no password, no email.
    let written = backend.stop().to_string();
    assert!(written.contains("Nexus server 0.1.0 started"), "{written}");
    for secret in [typed.clone(), format!("{}-{}", &typed[..6], &typed[6..]), PASSWORD.to_string(), EMAIL.to_string(), first.session.expose_hex().to_string(), phone.session.expose_hex().to_string()] {
        assert!(!written.contains(&secret), "the log holds {secret:?}");
    }

    // Set-up happens once: asked again, the command refuses (8.4).
    let output = program(&settings, &["set-up", "someone-else@example.org"]).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("set up already"));
}

#[test]
fn a_wrong_password_an_unknown_email_and_a_note_all_answer_as_the_design_says() {
    let folders = Folders::new("refusals");
    let port = free_port();
    let settings = folders.settings(port);
    let code = set_up(&settings, EMAIL);
    let typed = code.expose().to_string();
    let backend = Serving::start(&settings, port);
    let (status, body) = backend.post(account::SIGN_IN, None, &sign_in(Credential::Code { code, new_password: Password::new(PASSWORD).unwrap() }, 1, "Robin's PC", DeviceKind::Pc));
    let pc = signed_in(status, &body);

    // The code is spent: used again, it is the one answer for a wrong login.
    let (status, wrong_code) = backend.post(account::SIGN_IN, None, &sign_in(Credential::Code { code: OneTimeCode::new(&typed).unwrap(), new_password: Password::new(PASSWORD).unwrap() }, 3, "Another", DeviceKind::Pc));
    assert_eq!(status, 403);
    // A wrong password and an email with no account: the same status, the same bytes (8.3).
    let (wrong_status, wrong) = backend.post(account::SIGN_IN, None, &sign_in(with_password(EMAIL, "not robin's password at all"), 4, "A stranger", DeviceKind::Pc));
    let (nobody_status, nobody) = backend.post(account::SIGN_IN, None, &sign_in(with_password("nobody@example.org", "not robin's password at all"), 5, "A stranger", DeviceKind::Pc));
    assert_eq!((wrong_status, nobody_status), (403, 403));
    assert_eq!(wrong, nobody);
    assert_eq!(wrong, wrong_code);
    assert_eq!(refusal(account::read_reply::<SignedIn>(wrong_status, &wrong).unwrap()), Refusal::NotAllowed(NotAllowed::WrongLogin));

    // The phone signs in; the PC forces it out by the session it names (8.7).
    // (The other door now makes a guesser at Robin's email wait: the phone is a new device.)
    std::thread::sleep(Duration::from_millis(1100));
    let (status, body) = backend.post(account::SIGN_IN, None, &sign_in(with_password(EMAIL, PASSWORD), 2, "Robin's phone", DeviceKind::Phone));
    let phone = signed_in(status, &body);
    let force = |session: SessionId| account::encode(&ForceOut { head: speaks(pc.restore), device: phone.device, session }).unwrap();
    let (status, _) = backend.post(account::FORCE_OUT, Some(&pc.session), force(phone.session_id).bytes());
    assert_eq!(status, 200);
    let list = account::encode(&ListDevices { head: speaks(pc.restore), after: None }).unwrap();
    let (status, _) = backend.post(account::LIST_DEVICES, Some(&phone.session), list.bytes());
    assert_eq!(status, 401);

    // The phone signs in again; the same force-out sent again ends nothing (8.7).
    let (status, body) = backend.post(account::SIGN_IN, None, &sign_in(with_password(EMAIL, PASSWORD), 2, "Robin's phone", DeviceKind::Phone));
    let again = signed_in(status, &body);
    assert_eq!(again.device, phone.device);
    let (status, _) = backend.post(account::FORCE_OUT, Some(&pc.session), force(phone.session_id).bytes());
    assert_eq!(status, 200);
    let (status, _) = backend.post(account::LIST_DEVICES, Some(&again.session), list.bytes());
    assert_eq!(status, 200);

    // Signing out with the note alone, no session shown (section 11): it ends that session only.
    let out = account::encode(&SignOut { head: speaks(pc.restore), request_id: RequestId::new(2).unwrap(), revoke: Some(again.sign_out) }).unwrap();
    let (status, body) = backend.post(account::SIGN_OUT, None, out.bytes());
    assert!(matches!(account::read_reply::<account::SignedOut>(status, &body).unwrap(), Reply::Done { .. }), "{status}");
    let (status, _) = backend.post(account::LIST_DEVICES, Some(&again.session), list.bytes());
    assert_eq!(status, 401);
    let (status, _) = backend.post(account::LIST_DEVICES, Some(&pc.session), list.bytes());
    assert_eq!(status, 200);

    // A sign-in body over 4 KiB is refused from its head, before a byte of it is read.
    let (status, _) = backend.post(account::SIGN_IN, None, &vec![b' '; account::PASSWORD_REQUEST_MAX_BYTES + 1]);
    assert_eq!(status, 413);
}
