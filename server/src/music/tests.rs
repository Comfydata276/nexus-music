//! The music server's client against stand-ins on this machine's loopback: what it sends, and
//! that a redirect, an answer too large, one that never comes, a closed port and a certificate
//! not trusted each give no answer the library could take as a yes.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::{self, RootCertStore, ServerConfig};

use super::*;

const OK: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#;

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()
}

/// Reads one request (its head and its counted body) from `from`.
async fn request(from: &mut (impl tokio::io::AsyncRead + Unpin)) -> Vec<u8> {
    let mut read = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let Ok(got) = from.read(&mut chunk).await else { return read };
        if got == 0 {
            return read;
        }
        read.extend_from_slice(&chunk[..got]);
        if let Some(end) = read.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&read[..end]).to_ascii_lowercase();
            let length: usize = head.lines().find_map(|line| line.strip_prefix("content-length: ")).and_then(|n| n.trim().parse().ok()).unwrap_or(0);
            if read.len() >= end + 4 + length {
                return read;
            }
        }
    }
}

/// A plain stand-in that answers every connection with `answer` (or nothing, if `None`), and
/// hands each request it read to the test.
async fn stand_in(answer: Option<Vec<u8>>) -> (String, mpsc::UnboundedReceiver<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap();
    let (heard, hearing) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else { return };
            let (answer, heard) = (answer.clone(), heard.clone());
            tokio::spawn(async move {
                let _ = heard.send(request(&mut stream).await);
                match answer {
                    Some(answer) => {
                        let _ = stream.write_all(&answer).await;
                        let _ = stream.shutdown().await;
                    }
                    None => tokio::time::sleep(Duration::from_secs(30)).await,
                }
            });
        }
    });
    (format!("http://{at}"), hearing)
}

fn answer(status: &str, headers: &str, body: &str) -> Vec<u8> {
    format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/json\r\n{headers}\r\n{body}", body.len()).into_bytes()
}

fn client() -> MusicClient {
    MusicClient::trusting(RootCertStore::empty()).within(Duration::from_millis(800))
}

#[test]
fn a_question_is_one_post_of_the_form_alone_and_its_answer_is_taken_whole() {
    runtime().block_on(async {
        let (at, mut heard) = stand_in(Some(answer("200 OK", "", OK))).await;
        let form = "v=1.16.1&c=nexus-music&f=json&u=someone&s=salt&t=token";
        let got = client().ask_at(&format!("{at}/music/"), "/rest/ping", form).await;
        assert_eq!(got, Some((200, OK.as_bytes().to_vec())));
        let sent = String::from_utf8(heard.recv().await.unwrap()).unwrap();
        let (head, body) = sent.split_once("\r\n\r\n").unwrap();
        let mut lines = head.lines();
        assert_eq!(lines.next(), Some("POST /music/rest/ping HTTP/1.1"));
        let mut names: Vec<String> = lines.map(|line| line.split(':').next().unwrap().to_ascii_lowercase()).collect();
        names.sort();
        assert_eq!(names, ["connection", "content-length", "content-type", "host"], "{head}");
        assert!(head.to_ascii_lowercase().contains("content-type: application/x-www-form-urlencoded"));
        assert!(!head.contains("token"), "nothing of the form in the head or the address");
        assert_eq!(body, form);
    });
}

#[test]
fn a_redirect_is_an_answer_and_is_not_followed() {
    runtime().block_on(async {
        let (elsewhere, mut heard_elsewhere) = stand_in(Some(answer("200 OK", "", OK))).await;
        let location = format!("Location: {elsewhere}/rest/ping\r\n");
        let (at, mut heard) = stand_in(Some(answer("302 Found", &location, ""))).await;
        let got = client().ask_at(&at, "/rest/ping", "f=json").await;
        assert_eq!(got.map(|(status, _)| status), Some(302));
        assert!(heard.recv().await.is_some());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(heard_elsewhere.try_recv().is_err(), "the redirect was followed");
    });
}

#[test]
fn an_answer_too_large_one_that_never_comes_and_a_closed_port_are_no_answer() {
    runtime().block_on(async {
        let large = format!(r#"{{"subsonic-response":{{"status":"ok","version":"{}"}}}}"#, "9".repeat(music::ANSWER_MAX_BYTES));
        let (at, _heard) = stand_in(Some(answer("200 OK", "", &large))).await;
        assert_eq!(client().ask_at(&at, "/rest/ping", "f=json").await, None);
        let (silent, _heard) = stand_in(None).await;
        let asked = std::time::Instant::now();
        assert_eq!(client().ask_at(&silent, "/rest/ping", "f=json").await, None);
        assert!(asked.elapsed() < Duration::from_secs(3));
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        assert_eq!(client().ask_at(&format!("http://{closed}"), "/rest/ping", "f=json").await, None);
    });
}

/// A TLS stand-in for `localhost`, with a certificate of its own made now.
async fn tls_stand_in() -> (String, RootCertStore) {
    let made = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let mut roots = RootCertStore::empty();
    roots.add(made.cert.der().clone()).unwrap();
    let key = rustls::pki_types::PrivateKeyDer::try_from(made.key_pair.serialize_der()).unwrap();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![made.cert.der().clone()], key)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else { return };
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let Ok(mut tls) = acceptor.accept(stream).await else { return };
                request(&mut tls).await;
                let _ = tls.write_all(&answer("200 OK", "", OK)).await;
                let _ = tls.shutdown().await;
            });
        }
    });
    (format!("https://localhost:{port}"), roots)
}

#[test]
fn the_address_people_use_is_asked_by_https_with_a_certificate_this_machine_trusts_only() {
    runtime().block_on(async {
        let (at, roots) = tls_stand_in().await;
        let trusted = MusicClient::trusting(roots).within(Duration::from_secs(2));
        assert_eq!(trusted.ask_at(&at, "/rest/ping", "f=json").await.map(|(status, _)| status), Some(200));
        assert_eq!(client().ask_at(&at, "/rest/ping", "f=json").await, None, "a certificate not trusted was taken");
    });
}
