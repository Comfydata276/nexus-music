//! The music server, as the backend itself asks it (design 9.3; slice S9): the two questions
//! of a typed-in login's check (`ping`, then `getUser`), each a request **built whole by the
//! shared library** (`nexus_core::nexus::music::Post`) and only carried here.
//!
//! What this carries, and nothing more: one connection to the address the request names (the
//! container's `NEXUS_MUSIC_SERVER`, plain HTTP allowed there; or the address people use, by
//! HTTPS, checked against this machine's trusted certificates), one POST of the request's own
//! form with no header but the four any request needs (`Host`, `Content-Type`,
//! `Content-Length`, `Connection: close`), and the answer's status and body. **No redirect is
//! followed**: a redirect is an answer like any other, which the library does not take as a
//! yes. **The answer is held to a ceiling** ([`music::ANSWER_MAX_BYTES`], its head to 16 KiB)
//! and the whole exchange to [`ASK_WITHIN`]; past either, or on any failure to connect, there
//! is no answer, which the library reads as "could not ask", never as "wrong name or password".
//!
//! Nothing of a request or an answer is written to the log or put in an error. The form, which
//! carries a token made from the password, is wiped once sent where its buffer is this
//! request's alone.

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HOST};
use axum::http::{Method, Request};
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use nexus_core::nexus::music::{self, Post};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore, pki_types::ServerName};
use zeroize::Zeroize;

/// How long one question may take, from connecting to the last byte of the answer.
pub const ASK_WITHIN: Duration = Duration::from_secs(10);
/// The largest head of an answer read (hyper's least).
const ANSWER_HEAD_MAX_BYTES: usize = 16 * 1024;

/// What the backend asks the music server with: the certificates it trusts for HTTPS.
#[derive(Clone)]
pub struct MusicClient {
    tls: Arc<ClientConfig>,
    within: Duration,
}

/// The answer to one question: its status and its whole body, or nothing.
pub type Answered = Option<(u16, Vec<u8>)>;

impl MusicClient {
    /// Trusting this machine's certificates (and `SSL_CERT_FILE`, where it is set). Also says
    /// how many were found, for the start's log.
    pub fn from_this_machine() -> (MusicClient, usize) {
        let mut roots = RootCertStore::empty();
        let found = rustls_native_certs::load_native_certs();
        let (added, _) = roots.add_parsable_certificates(found.certs);
        (MusicClient::trusting(roots), added)
    }

    pub fn trusting(roots: RootCertStore) -> MusicClient {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("ring's default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        MusicClient { tls: Arc::new(config), within: ASK_WITHIN }
    }

    /// The same, with its own time for a question (the tests').
    pub fn within(mut self, within: Duration) -> MusicClient {
        self.within = within;
        self
    }

    /// Asks the one question `post` is, and gives its answer, or nothing.
    pub async fn ask(&self, post: &Post<'_>) -> Answered {
        self.ask_at(post.address(), post.path(), post.form()).await
    }

    /// The same, from its parts (the tests' way in: only the library makes a [`Post`]).
    pub(crate) async fn ask_at(&self, address: &str, path: &str, form: &str) -> Answered {
        let url = url::Url::parse(address).ok()?;
        let port = url.port_or_known_default()?;
        let (connect, name) = match url.host()? {
            url::Host::Domain(domain) => (domain.to_string(), ServerName::try_from(domain.to_string()).ok()?),
            url::Host::Ipv4(ip) => (ip.to_string(), ServerName::IpAddress(std::net::IpAddr::V4(ip).into())),
            url::Host::Ipv6(ip) => (ip.to_string(), ServerName::IpAddress(std::net::IpAddr::V6(ip).into())),
        };
        let authority = url[url::Position::BeforeHost..url::Position::AfterPort].to_string();
        let target = format!("{}{}", url.path().trim_end_matches('/'), path);
        let mut form = Bytes::from(form.as_bytes().to_vec());
        let answered = tokio::time::timeout(self.within, async {
            let tcp = TcpStream::connect((connect.as_str(), port)).await.ok()?;
            match url.scheme() {
                "http" => exchange(tcp, &authority, &target, form.clone()).await,
                "https" => {
                    let tls = TlsConnector::from(self.tls.clone()).connect(name, tcp).await.ok()?;
                    exchange(tls, &authority, &target, form.clone()).await
                }
                _ => None,
            }
        })
        .await
        .ok()
        .flatten();
        // Wiped, where nothing else holds it any more.
        if let Ok(mut owned) = std::mem::take(&mut form).try_into_mut() {
            owned.as_mut().zeroize();
        }
        answered
    }
}

/// One request on its own connection, and its answer.
async fn exchange(io: impl AsyncRead + AsyncWrite + Unpin + Send + 'static, authority: &str, target: &str, form: Bytes) -> Answered {
    let (mut sender, connection) = hyper::client::conn::http1::Builder::new().max_buf_size(ANSWER_HEAD_MAX_BYTES).handshake(TokioIo::new(io)).await.ok()?;
    let driving = tokio::spawn(async move {
        let _ = connection.await;
    });
    let request = Request::builder()
        .method(Method::POST)
        .uri(target)
        .header(HOST, authority)
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(CONTENT_LENGTH, form.len())
        .header(CONNECTION, "close")
        .body(Full::new(form))
        .ok()?;
    let answered = async {
        let response = sender.send_request(request).await.ok()?;
        let status = response.status().as_u16();
        let body = Limited::new(response.into_body(), music::ANSWER_MAX_BYTES).collect().await.ok()?.to_bytes();
        Some((status, body.to_vec()))
    }
    .await;
    drop(sender);
    driving.abort();
    let _ = driving.await;
    answered
}

#[cfg(test)]
mod tests;
