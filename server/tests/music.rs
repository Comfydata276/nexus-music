//! The music login, typed in by an admin, end to end (slice S9; design 9.3 to 9.5): the
//! program itself on this machine's loopback, spoken to as the proxy speaks to it, against the
//! TEST Navidrome on this machine (never a real one; no request leaves the machine). Its
//! address is `NEXUS_TEST_NAVIDROME` (e.g. `http://127.0.0.1:4690`, the test kit's
//! `scripts/server --instance s9 start`), with its made-up user `tester` / `tester-pass-1`.
//!
//! Run by hand, one at a time (Navidrome refuses a name after five wrong tries in twenty
//! seconds):
//! `NEXUS_TEST_NAVIDROME=http://127.0.0.1:4690 cargo test --manifest-path server/Cargo.toml --test music -- --ignored --test-threads 1`
//!
//! Between the backend and Navidrome, in this process: a **forwarder** standing for the
//! container's own address (`NEXUS_MUSIC_SERVER`, plain HTTP), which the test can switch to
//! "down" (every connection closed at once) or to "redirect" (a 302 to a trap that counts
//! anyone arriving); and a **TLS front** standing for the address people use
//! (`https://localhost:<port>`), with a certificate made for the run and given to the backend
//! by `SSL_CERT_FILE`. A "device" plays through the TLS front with what it was handed.

use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nexus_core::nexus::account::{
    self, AccountId, Credential, DeviceKind, DeviceSecret, Email, GetMusicLogin, GrantIssued, GrantRequest, GrantedChange, MusicAddress, MusicLogin,
    MusicLoginResult, Name, OneTimeCode, Password, Reply, RequestHead, RequestId, Session, SignIn, SignedIn,
};
use nexus_core::nexus::admin::{self, Action, Applied, Denial, MusicPassword, MusicServerAddress, NamedChange, TypedMusicLogin};
use nexus_core::nexus::proto::{SPEAKS, Version};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::rustls::{self, RootCertStore};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const PROGRAM: &str = env!("CARGO_BIN_EXE_nexus-music-server");
const MUSIC_USER: &str = "tester";
const MUSIC_PASSWORD: &str = "tester-pass-1";
const ADMIN: &str = "admin@example.org";
const PERSON: &str = "person@example.org";
const PASSWORD: &str = "a made-up password of the admin's";
const PERSONS_PASSWORD: &str = "a made-up password of the person's";

fn navidrome() -> (String, u16) {
    let address = std::env::var("NEXUS_TEST_NAVIDROME").expect("NEXUS_TEST_NAVIDROME: the test Navidrome's address, e.g. http://127.0.0.1:4690");
    let url = url::Url::parse(&address).unwrap();
    assert!(matches!(url.host_str(), Some("127.0.0.1" | "localhost")), "the test Navidrome is on this machine only");
    (url.host_str().unwrap().to_string(), url.port().unwrap())
}

// ---------------------------------------------------------------------------------------------
// Between the backend and Navidrome
// ---------------------------------------------------------------------------------------------

const FORWARD: u8 = 0;
const DOWN: u8 = 1;
const REDIRECT: u8 = 2;

struct Between {
    runtime: tokio::runtime::Runtime,
    /// The forwarder: the container's own address, as the backend reaches it.
    internal: SocketAddr,
    mode: Arc<AtomicU8>,
    /// Connections the forwarder took, and the trap a redirect points at.
    taken: Arc<AtomicUsize>,
    trapped: Arc<AtomicUsize>,
    /// The TLS front: the address people use.
    public_port: u16,
    roots: RootCertStore,
    cert_file: PathBuf,
}

async fn pipe(mut from: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin, to: (String, u16)) {
    if let Ok(mut navidrome) = tokio::net::TcpStream::connect((to.0.as_str(), to.1)).await {
        let _ = tokio::io::copy_bidirectional(&mut from, &mut navidrome).await;
    }
}

impl Between {
    fn start(root: &std::path::Path) -> Between {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let to = navidrome();
        let mode = Arc::new(AtomicU8::new(FORWARD));
        let (taken, trapped) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (internal, public_port, roots, cert_file) = runtime.block_on(async {
            let trap = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let trap_at = trap.local_addr().unwrap();
            let caught = trapped.clone();
            tokio::spawn(async move {
                while let Ok((_stream, _)) = trap.accept().await {
                    caught.fetch_add(1, Ordering::SeqCst);
                }
            });
            let forwarder = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let internal = forwarder.local_addr().unwrap();
            let (mode, taken, to) = (mode.clone(), taken.clone(), to.clone());
            tokio::spawn(async move {
                while let Ok((mut stream, _)) = forwarder.accept().await {
                    taken.fetch_add(1, Ordering::SeqCst);
                    let (mode, to) = (mode.load(Ordering::SeqCst), to.clone());
                    tokio::spawn(async move {
                        match mode {
                            DOWN => drop(stream),
                            REDIRECT => {
                                let mut head = [0u8; 4096];
                                let _ = stream.read(&mut head).await;
                                let answer = format!("HTTP/1.1 302 Found\r\nLocation: http://{trap_at}/rest/ping\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                                let _ = stream.write_all(answer.as_bytes()).await;
                                let _ = stream.shutdown().await;
                            }
                            _ => pipe(stream, to).await,
                        }
                    });
                }
            });
            // The address people use: TLS made here, then on to Navidrome.
            let made = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
            let cert_file = root.join("front.pem");
            std::fs::write(&cert_file, made.cert.pem()).unwrap();
            let mut roots = RootCertStore::empty();
            roots.add(made.cert.der().clone()).unwrap();
            let key = rustls::pki_types::PrivateKeyDer::try_from(made.key_pair.serialize_der()).unwrap();
            let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(vec![made.cert.der().clone()], key)
                .unwrap();
            let acceptor = TlsAcceptor::from(Arc::new(config));
            let front = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let public_port = front.local_addr().unwrap().port();
            tokio::spawn(async move {
                while let Ok((stream, _)) = front.accept().await {
                    let (acceptor, to) = (acceptor.clone(), navidrome());
                    tokio::spawn(async move {
                        if let Ok(tls) = acceptor.accept(stream).await {
                            pipe(tls, to).await;
                        }
                    });
                }
            });
            (internal, public_port, roots, cert_file)
        });
        Between { runtime, internal, mode, taken, trapped, public_port, roots, cert_file }
    }

    fn internal(&self) -> String {
        format!("http://{}", self.internal)
    }

    fn public(&self) -> String {
        format!("https://localhost:{}", self.public_port)
    }

    fn set(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
    }

    /// A device's request to the music server with what it was handed, through the address
    /// people use: the status and up to `keep` bytes of the body, and the content type.
    fn device_get(&self, login: &MusicLogin, method: &str, more: &str, keep: usize) -> (u16, String, Vec<u8>) {
        let target = format!(
            "/rest/{method}?u={}&t={}&s={}&v=1.16.1&c=nexus-s9-test&f=json{more}",
            login.name.as_str(),
            login.token.expose_hex().as_str(),
            login.salt.as_str()
        );
        let connector = TlsConnector::from(Arc::new(
            rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_root_certificates(self.roots.clone())
                .with_no_client_auth(),
        ));
        let port = self.public_port;
        self.runtime.block_on(async move {
            let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
            let mut tls = connector.connect(rustls::pki_types::ServerName::try_from("localhost").unwrap(), tcp).await.unwrap();
            let request = format!("GET {target} HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n");
            tls.write_all(request.as_bytes()).await.unwrap();
            let mut read = Vec::new();
            let mut chunk = [0u8; 8192];
            let end = loop {
                let got = tls.read(&mut chunk).await.unwrap_or(0);
                if got == 0 {
                    break None;
                }
                read.extend_from_slice(&chunk[..got]);
                if let Some(end) = read.windows(4).position(|w| w == b"\r\n\r\n") {
                    if read.len() >= end + 4 + keep {
                        break Some(end);
                    }
                }
            };
            let end = end.or_else(|| read.windows(4).position(|w| w == b"\r\n\r\n")).expect("an answer's head");
            let head = String::from_utf8_lossy(&read[..end]).to_string();
            let status: u16 = head.split(' ').nth(1).unwrap().parse().unwrap();
            let kind = head.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("content-type: ").map(str::to_string)).unwrap_or_default();
            let body = read[end + 4..].iter().copied().take(keep).collect();
            (status, kind, body)
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------------------------

struct Folders {
    root: PathBuf,
}

impl Folders {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("nexus-server-music-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        Folders { root }
    }

    fn settings(&self, port: u16, between: &Between) -> Vec<(&'static str, String)> {
        vec![
            ("NEXUS_MUSIC_SERVER", between.internal()),
            ("NEXUS_PROXY", "127.0.0.1".into()),
            ("NEXUS_DATA", self.root.join("data").display().to_string()),
            ("NEXUS_STATE", self.root.join("state").display().to_string()),
            ("NEXUS_LISTEN", format!("127.0.0.1:{port}")),
            ("SSL_CERT_FILE", between.cert_file.display().to_string()),
        ]
    }

    fn data(&self) -> PathBuf {
        self.root.join("data")
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

/// A one-time code from the owner's `set-up` or `recover`, said on its own terminal.
fn code_from(settings: &[(&'static str, String)], arguments: &[&str]) -> OneTimeCode {
    let output = program(settings, arguments).output().unwrap();
    let said = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(output.status.success(), "{said}{}", String::from_utf8_lossy(&output.stderr));
    let at = said.find("one-time code ").expect("a code") + "one-time code ".len();
    OneTimeCode::new(&said[at..at + 13].chars().filter(|c| *c != '-').collect::<String>()).unwrap()
}

struct Serving {
    child: Option<Child>,
    at: SocketAddr,
    written: String,
}

impl Serving {
    fn start(settings: &[(&'static str, String)], port: u16) -> Serving {
        let child = program(settings, &[]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let at: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let start = Instant::now();
        while TcpStream::connect(at).is_err() {
            assert!(start.elapsed() < Duration::from_secs(20), "it never listened");
            std::thread::sleep(Duration::from_millis(50));
        }
        Serving { child: Some(child), at, written: String::new() }
    }

    fn stop(&mut self) -> String {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
            let mut written = String::new();
            let _ = child.stdout.take().unwrap().read_to_string(&mut written);
            let _ = child.stderr.take().unwrap().read_to_string(&mut written);
            self.written = written;
        }
        self.written.clone()
    }

    fn post(&self, path: &str, session: Option<&Session>, body: &[u8]) -> (u16, Vec<u8>) {
        let mut stream = TcpStream::connect(self.at).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(40))).unwrap();
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

fn head(restore: u64) -> RequestHead {
    RequestHead { speaks: SPEAKS, restore }
}

fn sign_in(backend: &Serving, credential: Credential, device: u8) -> SignedIn {
    let request = SignIn {
        credential,
        device_secret: DeviceSecret::from_bytes([device; 32]),
        device_name: Name::new("A made-up device").unwrap(),
        device_kind: DeviceKind::Phone,
        app_version: Version::parse("0.41.0").unwrap(),
        speaks: SPEAKS,
        restore: None,
    };
    let (status, body) = backend.post(account::SIGN_IN, None, account::encode(&request).unwrap().bytes());
    match account::read_reply::<SignedIn>(status, &body).unwrap() {
        Reply::Done { result, .. } => result,
        other => panic!("not signed in: {status} {other:?}"),
    }
}

fn with_code(code: &OneTimeCode, password: &str) -> Credential {
    Credential::Code { code: OneTimeCode::new(code.expose()).unwrap(), new_password: Password::new(password).unwrap() }
}

fn with_password(email: &str, password: &str) -> Credential {
    Credential::Password { email: Email::new(email).unwrap(), password: Password::presented(password).unwrap() }
}

/// The admin's signed-in session, with its restore number, and a counter of request numbers.
struct Admin<'a> {
    backend: &'a Serving,
    signed: SignedIn,
    next: u64,
}

impl Admin<'_> {
    fn grant(&mut self, action: Action) -> (RequestId, account::GrantSecret) {
        self.next += 1;
        let request_id = RequestId::new(self.next).unwrap();
        let request = GrantRequest { head: head(self.signed.restore), change: NamedChange { request_id, action }, password: Password::presented(PASSWORD).unwrap() };
        let (status, body) = self.backend.post(admin::REQUEST_GRANT, Some(&self.signed.session), admin::encode(&request).unwrap().bytes());
        match admin::read_reply::<GrantIssued>(status, &body).unwrap() {
            admin::Reply::Done(GrantIssued { grant }) => (request_id, grant),
            other => panic!("no grant: {status} {other:?}"),
        }
    }

    fn set_music_server(&mut self, public: &str) -> admin::Reply<Applied> {
        let address = MusicAddress::new(public).unwrap();
        let (request_id, grant) = self.grant(Action::SetMusicServer(address.clone()));
        let request = GrantedChange { head: head(self.signed.restore), request_id, change: MusicServerAddress { public: address }, grant };
        let (status, body) = self.backend.post(admin::SET_MUSIC_SERVER, Some(&self.signed.session), admin::encode(&request).unwrap().bytes());
        admin::read_reply::<Applied>(status, &body).unwrap()
    }

    fn set_music_login(&mut self, account: AccountId, name: &str, password: &str) -> (u16, admin::Reply<Applied>) {
        let (request_id, grant) = self.grant(Action::SetMusicLogin(account));
        let change = TypedMusicLogin { account, name: Name::new(name).unwrap(), password: MusicPassword::new(password).unwrap() };
        let request = GrantedChange { head: head(self.signed.restore), request_id, change, grant };
        let (status, body) = self.backend.post(admin::SET_MUSIC_LOGIN, Some(&self.signed.session), admin::encode(&request).unwrap().bytes());
        (status, admin::read_reply::<Applied>(status, &body).unwrap())
    }
}

fn music_login(backend: &Serving, signed: &SignedIn) -> MusicLoginResult {
    let request = GetMusicLogin { head: head(signed.restore) };
    let (status, body) = backend.post(account::MUSIC_LOGIN, Some(&signed.session), account::encode(&request).unwrap().bytes());
    match account::read_reply::<MusicLoginResult>(status, &body).unwrap() {
        Reply::Done { result, .. } => result,
        other => panic!("no answer: {status} {other:?}"),
    }
}

fn login(result: MusicLoginResult) -> MusicLogin {
    match result {
        MusicLoginResult::Login(login) => login,
        other => panic!("no login handed: {other:?}"),
    }
}

/// The device plays with what it was handed: `ping`, then a song's first bytes.
fn plays(between: &Between, handed: &MusicLogin) {
    let (status, _, body) = between.device_get(handed, "ping", "", 4_096);
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&body).contains(r#""status":"ok""#), "ping: {}", String::from_utf8_lossy(&body));
    let (status, _, songs) = between.device_get(handed, "getRandomSongs", "&size=1", 2_000);
    assert_eq!(status, 200);
    let text = String::from_utf8_lossy(&songs).to_string();
    let at = text.find(r#""id":""#).expect("a song") + r#""id":""#.len();
    let id: String = text[at..].chars().take_while(|c| *c != '"').collect();
    let (status, kind, first) = between.device_get(handed, "stream", &format!("&id={id}"), 4_096);
    assert_eq!(status, 200, "stream");
    assert!(kind.starts_with("audio/"), "stream's type: {kind}");
    assert_eq!(first.len(), 4_096, "the stream's first bytes");
}

fn refused<T: std::fmt::Debug>(reply: admin::Reply<T>) -> admin::Refusal {
    match reply {
        admin::Reply::Refused(refusal) => refusal,
        other => panic!("not refused: {other:?}"),
    }
}

/// The milestone, then the music server's refusals and failures: an admin made by the owner's
/// command signs in, takes a grant and sets the address people use and a person's music login
/// (a real user of the test Navidrome); the person signs in from a second device and plays with
/// the salt and token in the answer. A wrong password, the music server down, and the music
/// server answering a redirect each keep nothing and leave the working login as it was.
#[test]
#[ignore = "needs the test Navidrome: NEXUS_TEST_NAVIDROME=http://127.0.0.1:4690"]
fn an_admin_sets_a_music_login_and_the_person_plays_and_no_failure_touches_it() {
    let folders = Folders::new("milestone");
    let between = Between::start(&folders.root);
    let port = free_port();
    let settings = folders.settings(port, &between);
    let admin_code = code_from(&settings, &["set-up", ADMIN, "Admin"]);
    let person_code = code_from(&settings, &["recover", PERSON]);
    let mut backend = Serving::start(&settings, port);
    let signed = sign_in(&backend, with_code(&admin_code, PASSWORD), 1);
    assert_eq!(signed.music, MusicLoginResult::NoneSetYet);
    let mut admin = Admin { backend: &backend, signed, next: 0 };
    // The person signs in first: none set yet.
    let person = sign_in(&backend, with_code(&person_code, PERSONS_PASSWORD), 2);
    assert_eq!(person.music, MusicLoginResult::NoneSetYet);

    // No address people use yet: a login cannot be set (nothing to check it at).
    let (status, reply) = admin.set_music_login(person.person.id, MUSIC_USER, MUSIC_PASSWORD);
    assert_eq!((status, refused(reply)), (403, admin::Refusal::NotAllowed(Denial::Conflict)));
    assert!(matches!(admin.set_music_server(&between.public()), admin::Reply::Done(_)));

    // The login set: both addresses asked, two questions each.
    let asked_before = between.taken.load(Ordering::SeqCst);
    let (status, reply) = admin.set_music_login(person.person.id, MUSIC_USER, MUSIC_PASSWORD);
    assert_eq!(status, 200, "{reply:?}");
    assert_eq!(between.taken.load(Ordering::SeqCst) - asked_before, 2, "ping and getUser at the container's address");

    // The person signs in from a second device: a salt and token in the answer, and it plays.
    let mut phone = sign_in(&backend, with_password(PERSON, PERSONS_PASSWORD), 3);
    let handed = login(std::mem::replace(&mut phone.music, MusicLoginResult::NoneSetYet));
    assert_eq!((handed.address.as_str(), handed.name.as_str(), handed.count), (between.public().as_str(), MUSIC_USER, 1));
    plays(&between, &handed);
    // Asked again: the same login, with a salt of its own.
    let again = login(music_login(&backend, &phone));
    assert_eq!(again.count, 1);
    assert_ne!(again.salt.as_str(), handed.salt.as_str());
    plays(&between, &again);

    // A wrong music password: not kept; the answer says so; the login works on.
    let (status, reply) = admin.set_music_login(person.person.id, MUSIC_USER, "not the music password");
    assert_eq!((status, refused(reply)), (403, admin::Refusal::NotAllowed(Denial::MusicLoginRefused)));
    let still = login(music_login(&backend, &phone));
    assert_eq!(still.count, 1);
    plays(&between, &still);

    // The music server down: not kept; "could not reach it"; the login works on.
    between.set(DOWN);
    let (status, reply) = admin.set_music_login(person.person.id, MUSIC_USER, MUSIC_PASSWORD);
    assert_eq!((status, refused(reply)), (503, admin::Refusal::NotDealtWith));
    between.set(FORWARD);
    assert_eq!(login(music_login(&backend, &phone)).count, 1);

    // The music server answering a redirect: not followed (the trap sees nobody), not kept.
    between.set(REDIRECT);
    let (status, reply) = admin.set_music_login(person.person.id, MUSIC_USER, MUSIC_PASSWORD);
    assert_eq!((status, refused(reply)), (503, admin::Refusal::NotDealtWith));
    between.set(FORWARD);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(between.trapped.load(Ordering::SeqCst), 0, "the redirect was followed");
    assert_eq!(login(music_login(&backend, &phone)).count, 1);

    // A grant is for one change: the login set with the grant of another account's is refused.
    let (request_id, grant) = admin.grant(Action::SetMusicLogin(admin.signed.person.id));
    let change = TypedMusicLogin { account: person.person.id, name: Name::new(MUSIC_USER).unwrap(), password: MusicPassword::new(MUSIC_PASSWORD).unwrap() };
    let request = GrantedChange { head: head(admin.signed.restore), request_id, change, grant };
    let (status, _) = backend.post(admin::SET_MUSIC_LOGIN, Some(&admin.signed.session), admin::encode(&request).unwrap().bytes());
    assert_eq!(status, 403);
    // And nobody signed in can cause the music server to be asked at all.
    let asked_before = between.taken.load(Ordering::SeqCst);
    let (status, _) = backend.post(admin::SET_MUSIC_LOGIN, None, admin::encode(&request).unwrap().bytes());
    assert_eq!(status, 401);
    // Nor can a call that is not one.
    let (status, _) = backend.post(admin::REQUEST_GRANT, Some(&phone.session), b"{}");
    assert_eq!(status, 400);
    assert_eq!(between.taken.load(Ordering::SeqCst), asked_before);

    // Forcing the phone out does not change its login (9.5): the person's other device is
    // handed the same count.
    let force = account::ForceOut { head: head(person.restore), device: phone.device, session: phone.session_id };
    let (status, body) = backend.post(account::FORCE_OUT, Some(&person.session), account::encode(&force).unwrap().bytes());
    assert_eq!(status, 200);
    assert!(matches!(account::read_reply::<account::ForcedOut>(status, &body).unwrap(), Reply::Done { result: account::ForcedOut { music_cut_off: false }, .. }));
    assert_eq!(login(music_login(&backend, &person)).count, 1);

    // The log holds no music password, token or salt.
    let written = backend.stop();
    for secret in [MUSIC_PASSWORD, "not the music password", handed.token.expose_hex().as_str(), handed.salt.as_str()] {
        assert!(!written.contains(secret), "the log has {secret:?}");
    }
}

/// The sealed login moved or changed in the data does not open; an older copy of the data put
/// back does not hand out the login it held; the admin typing it again makes it good again.
#[test]
#[ignore = "needs the test Navidrome: NEXUS_TEST_NAVIDROME=http://127.0.0.1:4690"]
fn a_sealed_login_tampered_moved_or_put_back_from_an_older_copy_is_not_handed_out() {
    let folders = Folders::new("sealed");
    let between = Between::start(&folders.root);
    let port = free_port();
    let settings = folders.settings(port, &between);
    let admin_code = code_from(&settings, &["set-up", ADMIN, "Admin"]);
    let person_code = code_from(&settings, &["recover", PERSON]);
    let mut backend = Serving::start(&settings, port);
    let signed = sign_in(&backend, with_code(&admin_code, PASSWORD), 1);
    let person = sign_in(&backend, with_code(&person_code, PERSONS_PASSWORD), 2);
    let mut admin = Admin { backend: &backend, signed, next: 0 };
    assert!(matches!(admin.set_music_server(&between.public()), admin::Reply::Done(_)));
    let admin_id = admin.signed.person.id;
    // Both accounts have the same music user's login (the test Navidrome's one user), each
    // sealed for its own account.
    assert_eq!(admin.set_music_login(person.person.id, MUSIC_USER, MUSIC_PASSWORD).0, 200);
    assert_eq!(admin.set_music_login(admin_id, MUSIC_USER, MUSIC_PASSWORD).0, 200);
    drop(admin);
    backend.stop();
    let data = folders.data().join("nexus.db");

    // Moved: the person's sealed bytes put in the admin's row. The admin's does not open; the
    // person's still does.
    {
        let db = rusqlite::Connection::open(&data).unwrap();
        db.execute("UPDATE music_logins SET sealed = (SELECT sealed FROM music_logins WHERE account = ?1) WHERE account = ?2", [person.person.id.get() as i64, admin_id.get() as i64]).unwrap();
    }
    let backend = Serving::start(&settings, port);
    let admin_again = sign_in(&backend, with_password(ADMIN, PASSWORD), 1);
    assert_eq!(admin_again.music, MusicLoginResult::UnusableNow, "a login moved to another account opened");
    plays(&between, &login(sign_in(&backend, with_password(PERSON, PERSONS_PASSWORD), 3).music));
    drop(backend);

    // Changed: one byte of the person's sealed bytes. It does not open.
    {
        let db = rusqlite::Connection::open(&data).unwrap();
        let mut sealed: Vec<u8> = db.query_row("SELECT sealed FROM music_logins WHERE account = ?1", [person.person.id.get() as i64], |row| row.get(0)).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        db.execute("UPDATE music_logins SET sealed = ?1 WHERE account = ?2", rusqlite::params![sealed, person.person.id.get() as i64]).unwrap();
    }
    let backend = Serving::start(&settings, port);
    assert_eq!(sign_in(&backend, with_password(PERSON, PERSONS_PASSWORD), 3).music, MusicLoginResult::UnusableNow, "a changed seal opened");
    // The admin types both again: good again, under the next counts.
    let mut admin = Admin { backend: &backend, signed: sign_in(&backend, with_password(ADMIN, PASSWORD), 1), next: 100 };
    assert_eq!(admin.set_music_login(person.person.id, MUSIC_USER, MUSIC_PASSWORD).0, 200);
    let handed = login(sign_in(&backend, with_password(PERSON, PERSONS_PASSWORD), 3).music);
    assert_eq!(handed.count, 2);
    plays(&between, &handed);
    drop(admin);
    drop(backend);

    // An older copy of the data, then the login set again (count 3), then the copy put back:
    // the data's login is the older one (count 2), the store's count is 3: not handed out.
    let copy = nexus_music_server::store::copies::nightly(&folders.data(), now_ms()).unwrap();
    let backend = Serving::start(&settings, port);
    let mut admin = Admin { backend: &backend, signed: sign_in(&backend, with_password(ADMIN, PASSWORD), 1), next: 200 };
    assert_eq!(admin.set_music_login(person.person.id, MUSIC_USER, MUSIC_PASSWORD).0, 200);
    assert_eq!(login(sign_in(&backend, with_password(PERSON, PERSONS_PASSWORD), 3).music).count, 3);
    drop(admin);
    drop(backend);
    let output = program(&settings, &["put-back", &copy.path.display().to_string()]).output().unwrap();
    assert!(output.status.success(), "{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    let backend = Serving::start(&settings, port);
    let after = sign_in(&backend, with_password(PERSON, PERSONS_PASSWORD), 3);
    assert_eq!(after.music, MusicLoginResult::UnusableNow, "the older copy's login was handed out");
    // Typed again: handed out again, at a count past every earlier one.
    let mut admin = Admin { backend: &backend, signed: sign_in(&backend, with_password(ADMIN, PASSWORD), 1), next: 300 };
    assert_eq!(admin.set_music_login(person.person.id, MUSIC_USER, MUSIC_PASSWORD).0, 200);
    let handed = login(music_login(&backend, &after));
    assert_eq!(handed.count, 4);
    plays(&between, &handed);
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}
