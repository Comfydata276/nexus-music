//! Real TLS and HTTP on loopback. The test-only CA signs a localhost/127.0.0.1
//! server identity. Production uses the platform trust store and fixed GitHub origins.
use super::*;
use native_tls::{Identity, TlsAcceptor};
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

const RELEASE: &str = r#"{"tag_name":"v99.0.0","html_url":"https://anything.example/"}"#;
const BYTES: &str = "example installer";

fn client(header_timeout: Duration) -> ureq::Agent {
    let ca = ureq::tls::Certificate::from_der(include_bytes!("../../tests/fixtures/update/ca.der"));
    agent_config(RootCerts::new_with_certs(&[ca])).proxy(None)
        .timeout_recv_response(Some(header_timeout)).build().into()
}

fn answer(status: &str, headers: &str, body: &str) -> String {
    format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}", body.len())
}

fn redirect(location: &str) -> String {
    answer("302 Found", &format!("Location: {location}\r\n"), "")
}

struct Server {
    address: SocketAddr,
    origin: String,
    requests: Arc<Mutex<Vec<String>>>,
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Server {
    fn new(reply: impl Fn(usize) -> String + Send + 'static) -> Self {
        Self::serve(move |index, stream| {
            // A deadline can close the client before the response.
            let _ = stream.write_all(reply(index).as_bytes());
        })
    }

    fn serve(reply: impl Fn(usize, &mut native_tls::TlsStream<TcpStream>) + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let identity = Identity::from_pkcs12(include_bytes!("../../tests/fixtures/update/loopback.p12"), "fixture").unwrap();
        let tls = TlsAcceptor::new(identity).unwrap();
        let requests: Arc<Mutex<Vec<String>>> = Arc::default();
        let connections = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = thread::spawn({
            let requests = requests.clone();
            let connections = connections.clone();
            let stop = stop.clone();
            move || {
                for stream in listener.incoming() {
                    let stream = stream.unwrap();
                    if stop.load(Ordering::Acquire) { break }
                    connections.fetch_add(1, Ordering::Relaxed);
                    stream.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
                    // Even a plaintext request or failed TLS handshake counts as contact.
                    let Ok(mut tls) = tls.accept(stream) else { continue };
                    let mut reader = BufReader::new(&mut tls);
                    let mut request = String::new();
                    while !request.ends_with("\r\n\r\n") {
                        if reader.read_line(&mut request).unwrap_or(0) == 0 { break }
                    }
                    let index = {
                        let mut log = requests.lock().unwrap();
                        log.push(request);
                        log.len() - 1
                    };
                    reply(index, &mut tls);
                }
            }
        });
        Self { address, origin: format!("https://{address}"), requests, connections, stop, worker: Some(worker) }
    }

    fn check(&self, budget: Duration) -> Result<Option<Release>, CheckError> {
        check_at(&client(Duration::from_secs(4)), Source::Metadata { origin: &self.origin },
            &format!("{}/latest", self.origin), budget)
    }

    fn installer(&self) -> Installer {
        Installer {
            url: format!("{}/{REPO}/releases/download/v99/setup.exe", self.origin),
            name: format!("example-setup-{}.exe", self.address.port()),
            size: BYTES.len() as u64,
            sha256: Some("a7983d4724b9fe87d417868083d1c6a770b8f76328dccbebc8270dce37f1beaa".into()),
        }
    }

    fn download(&self, assets: &Server, installer: &Installer, timeout: Duration) -> Result<PathBuf, String> {
        let source = Source::Installer { repository: &self.origin, assets: &[&assets.origin] };
        let agent = client(timeout);
        let mut calls = 0;
        let result = download_with(installer, |_| {}, || {
            calls += 1;
            get(&agent, source, &installer.url, Instant::now() + INSTALLER_BUDGET, Some(INSTALLER_READ_IDLE))
        });
        // The fetch function is the only downloader and consumes its single call.
        assert_eq!(calls, 1);
        result
    }

    fn count(&self) -> usize {
        self.connections.load(Ordering::Relaxed)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address); // Wake accept so the worker can finish.
        self.worker.take().unwrap().join().unwrap();
    }
}

#[test]
fn api_same_origin_redirect_returns_the_release() {
    let api = Server::new(|i| match i {
        0 => redirect("/published"),
        _ => answer("200 OK", "", RELEASE),
    });
    let release = api.check(Duration::from_secs(4)).unwrap().unwrap();
    assert_eq!(release.version, "99.0.0");
    // This is the destination used by all three browser actions, even with a foreign html_url.
    assert_eq!(release.page, format!("https://github.com/{REPO}/releases/tag/v99.0.0"));
    let log = api.requests.lock().unwrap();
    assert_eq!(log.len(), 2);
    for (request, path) in log.iter().zip(["/latest", "/published"]) {
        assert!(request.starts_with(&format!("GET {path} HTTP/1.1\r\n")));
        assert!(request.to_ascii_lowercase().contains("accept: application/vnd.github+json"));
    }
}

#[test]
fn api_refuses_downgrade_and_other_origins_before_connecting() {
    let other = Server::new(|_| answer("200 OK", "", RELEASE));
    for target in [
        other.origin.replace("https://", "http://"),
        other.origin.replace("127.0.0.1", "localhost"),
        other.origin.clone(), // Same host name, different port.
    ] {
        let api = Server::new(move |_| redirect(&target));
        assert_eq!(api.check(Duration::from_secs(4)), Err(CheckError::Refused));
        assert_eq!((api.count(), other.count()), (1, 0));
    }
}

#[test]
fn api_rejects_missing_and_ambiguous_redirect_destinations() {
    for headers in ["", "Location: /one\r\nLocation: /two\r\n"] {
        let api = Server::new(move |_| answer("302 Found", headers, RELEASE));
        assert_eq!(api.check(Duration::from_secs(4)), Err(CheckError::Refused));
        assert_eq!(api.count(), 1);
    }
}

#[test]
fn metadata_redirects_share_the_deadline_with_the_final_response() {
    // Both waits would fit separately. Together they exhaust one budget before a third GET.
    let api = Server::new(|i| {
        thread::sleep(Duration::from_millis(if i == 0 { 400 } else { 900 }));
        redirect("/next")
    });
    assert_eq!(api.check(Duration::from_millis(1100)), Err(CheckError::TimedOut));
    assert_eq!(api.count(), 2);
}

#[test]
fn final_metadata_body_only_has_the_remaining_time() {
    let api = Server::serve(|i, stream| {
        if i == 0 {
            thread::sleep(Duration::from_millis(400));
            let _ = stream.write_all(redirect("/published").as_bytes());
        } else {
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", RELEASE.len()).unwrap();
            stream.flush().unwrap();
            thread::sleep(Duration::from_millis(900));
            let _ = stream.write_all(RELEASE.as_bytes());
        }
    });
    assert_eq!(api.check(Duration::from_millis(1100)), Err(CheckError::TimedOut));
    assert_eq!(api.count(), 2);
}

#[test]
fn installer_follows_one_or_five_redirects_and_refuses_the_sixth() {
    for hops in [1, 5, 6] {
        let store = Server::new(move |i| {
            if i + 1 < hops { redirect(&format!("/asset-{}", i + 2)) }
            else { answer("200 OK", "", BYTES) }
        });
        let target = format!("{}/asset-1?sig=example%2Bvalue%2F%3D&name=setup.exe", store.origin);
        let repo = Server::new(move |_| redirect(&target));
        let result = repo.download(&store, &repo.installer(), Duration::from_secs(4));
        if hops == 6 {
            assert_eq!(result.unwrap_err(), CheckError::Refused.to_string());
        } else {
            let file = result.unwrap();
            assert_eq!(std::fs::read(&file).unwrap(), BYTES.as_bytes());
            std::fs::remove_file(file).unwrap();
        }
        assert_eq!(repo.count(), 1);
        let requests = store.requests.lock().unwrap();
        assert_eq!(requests.len(), hops.min(5));
        assert!(requests[0].starts_with("GET /asset-1?sig=example%2Bvalue%2F%3D&name=setup.exe HTTP/1.1\r\n"));
        for (i, request) in requests.iter().enumerate().skip(1) {
            assert!(request.starts_with(&format!("GET /asset-{} HTTP/1.1\r\n", i + 1)));
        }
    }
}

#[test]
fn installer_policy_refusal_never_contacts_the_destination_or_retries() {
    let other = Server::new(|_| answer("200 OK", "", BYTES));
    for target in [other.origin.replace("https://", "http://"), other.origin.replace("127.0.0.1", "localhost")] {
        let assets = Server::new(move |_| redirect(&target));
        let store_url = assets.origin.clone();
        let repo = Server::new(move |_| redirect(&store_url));
        assert_eq!(repo.download(&assets, &repo.installer(), Duration::from_secs(4)).unwrap_err(),
            CheckError::Refused.to_string());
        assert_eq!((repo.count(), assets.count(), other.count()), (1, 1, 0));
    }
}

#[test]
fn installer_timeout_ends_after_the_single_fetch() {
    let repo = Server::new(|_| {
        thread::sleep(Duration::from_millis(800));
        answer("200 OK", "", BYTES)
    });
    let store = Server::new(|_| answer("200 OK", "", BYTES));
    assert_eq!(repo.download(&store, &repo.installer(), Duration::from_millis(350)).unwrap_err(),
        CheckError::TimedOut.to_string());
    assert_eq!((repo.count(), store.count()), (1, 0));
}

#[test]
fn stalled_installer_body_is_cancelled_by_either_limit() {
    // In each case only one deadline can expire before the server finishes its safety wait.
    let short = Duration::from_millis(800);
    let long = Duration::from_secs(30);
    for (budget, idle) in [(long, short), (short, long)] {
        let (resume, wait) = std::sync::mpsc::channel::<()>();
        let repo = Server::serve(move |_, stream| {
            let (first, rest) = BYTES.split_at(BYTES.len() / 2);
            let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{first}", BYTES.len());
            if stream.write_all(header.as_bytes()).and_then(|()| stream.flush()).is_err() { return }
            let _ = wait.recv_timeout(Duration::from_secs(10));
            let _ = stream.write_all(rest.as_bytes());
        });
        let source = Source::Installer { repository: &repo.origin, assets: &[] };
        let installer = repo.installer();
        let agent = client(long);
        let mut progress = 0.0;
        let result = download_with(&installer, |fraction| progress = fraction, || {
            get(&agent, source, &installer.url, Instant::now() + budget, Some(idle))
        });
        let _ = resume.send(());
        assert!(progress > 0.0 && progress < 1.0, "the installer must have stalled mid-body");
        assert_eq!(result.unwrap_err(), "The download was interrupted.");
        assert_eq!(repo.count(), 1);
        assert!(!std::env::temp_dir().read_dir().unwrap().flatten()
            .any(|entry| entry.file_name().to_string_lossy().ends_with(&installer.name)));
    }
}

#[test]
fn wrong_size_or_digest_leaves_no_installer_to_launch() {
    let repo = Server::new(|_| answer("200 OK", "", BYTES));
    let store = Server::new(|_| answer("200 OK", "", BYTES));
    for installer in [
        Installer { size: BYTES.len() as u64 + 1, ..repo.installer() },
        Installer { size: BYTES.len() as u64 - 1, ..repo.installer() },
        Installer { sha256: Some("0".repeat(64)), ..repo.installer() },
    ] {
        assert!(repo.download(&store, &installer, Duration::from_secs(4)).is_err());
        assert!(!std::env::temp_dir().read_dir().unwrap().flatten()
            .any(|entry| entry.file_name().to_string_lossy().ends_with(&installer.name)));
    }
    assert_eq!((repo.count(), store.count()), (3, 0));
}
