//! The admin document and its temporary sign-in answer.

use axum::http::header::{CONTENT_TYPE, ORIGIN};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
pub(crate) use nexus_core::nexus::{
    account::SIGN_IN,
    admin::PAGE,
};

const DOCUMENT: &str = include_str!("../admin/index.html");
const CSP: &str = concat!(
    "default-src 'none'; ",
    "script-src 'sha256-5MvwnS3vzzYkHCreW9oisQWAbUtoQToUBiNTRWcQyiU='; ",
    "style-src 'sha256-fw+aSH8Ue12IJIFbU4RLsE3Czhnbj5vTaLaAmz+BosA='; ",
    "connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; ",
    "require-trusted-types-for 'script'; trusted-types 'none'",
);
const ADMIN_HEADER: &str = "x-nexus-admin";

pub(crate) const PAGE_HEADERS: [(&str, &str); 6] = [
    ("content-security-policy", CSP),
    ("x-frame-options", "DENY"),
    ("referrer-policy", "no-referrer"),
    ("cross-origin-resource-policy", "same-origin"),
    ("cross-origin-opener-policy", "same-origin"),
    ("strict-transport-security", "max-age=31536000"),
];

pub(crate) async fn document() -> impl IntoResponse {
    ([(CONTENT_TYPE, "text/html; charset=utf-8")], DOCUMENT)
}

pub(crate) fn is_browser(headers: &HeaderMap) -> bool {
    headers.contains_key(ADMIN_HEADER) || headers.contains_key(ORIGIN)
}

pub(crate) fn browser_origin_allowed(headers: &HeaderMap, configured: Option<&str>) -> bool {
    one(headers, CONTENT_TYPE.as_str()) == Some("application/json")
        && one(headers, ADMIN_HEADER) == Some("1")
        && configured.is_some_and(|origin| one(headers, ORIGIN.as_str()) == Some(origin))
}

fn one<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?;
    values.next().is_none().then_some(value)
}

pub(crate) fn signin(headers: HeaderMap, public_origin: Option<&str>) -> StatusCode {
    if one(&headers, CONTENT_TYPE.as_str()) != Some("application/json")
        || (is_browser(&headers) && !browser_origin_allowed(&headers, public_origin))
    {
        return StatusCode::BAD_REQUEST;
    }
    StatusCode::NOT_IMPLEMENTED
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{bounds::BODY_MAX_BYTES, probe, serve};
    use nexus_core::nexus::{account, proto::{BackendId, SPEAKS, Version}};
    use std::io::{ErrorKind, Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;

    const PROXY: &str = "127.0.0.2";
    const PUBLIC_ORIGIN: &str = "https://nexus.example.test";
    const ORIGIN_HEADERS: &str = "Origin: https://nexus.example.test\r\nX-Nexus-Admin: 1\r\n";
    const HTTPS: &str = "X-Nexus-Scheme: https\r\n";

    struct Server {
        _runtime: tokio::runtime::Runtime,
        at: SocketAddr,
    }

    impl Server {
        fn start(public_origin: Option<&str>) -> Self {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2).enable_all().build().unwrap();
            let listener = runtime.block_on(async {
                serve::listen("127.0.0.1:0".parse().unwrap()).unwrap()
            });
            let at = listener.local_addr().unwrap();
            let hello = crate::hello(BackendId::from_bytes([0x51; 16]));
            runtime.spawn(serve::serve(listener, serve::Shell::new_with_public_origin(PROXY.parse().unwrap(), &hello, public_origin), false));
            Self { _runtime: runtime, at }
        }

        fn request(&self, from: &str, method: &str, target: &str, headers: &str, body: &str) -> Vec<u8> {
            let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None).unwrap();
            socket.bind(&SocketAddr::new(from.parse().unwrap(), 0).into()).unwrap();
            socket.connect(&self.at.into()).unwrap();
            let mut stream: TcpStream = socket.into();
            stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let _ = write!(stream, "{method} {target} HTTP/1.1\r\nHost: bexp-target:8080\r\n{headers}\r\n{body}");
            let mut answer = Vec::new();
            match stream.read_to_end(&mut answer) {
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::ConnectionReset => {}
                Err(error) => panic!("reading the answer: {error}"),
            }
            answer
        }
    }

    fn split(answer: &[u8]) -> (probe::Head, Vec<u8>) {
        let mut bytes = answer;
        let head = probe::read_head(&mut bytes).unwrap().unwrap();
        let mut body = Vec::new();
        probe::body(&head, bytes).unwrap().read_to_end(&mut body).unwrap();
        (head, body)
    }

    #[test]
    fn the_admin_document_comes_only_through_the_existing_gate() {
        let server = Server::start(Some(PUBLIC_ORIGIN));
        for from in ["127.0.0.3", "127.0.0.1"] {
            assert!(server.request(from, "GET", PAGE, HTTPS, "").is_empty());
        }
        for mark in ["", "X-Nexus-Scheme: http\r\n", "X-Nexus-Scheme: https\r\nX-Nexus-Scheme: https\r\n"] {
            assert_eq!(server.request(PROXY, "GET", PAGE, mark, ""), serve::bare(serve::GATE));
        }
        assert_eq!(server.request(PROXY, "GET", "/nexus/admin/?x", HTTPS, ""),
            serve::bare_response(StatusCode::BAD_REQUEST, true));
        assert_eq!(server.request(PROXY, "HEAD", PAGE, HTTPS, ""),
            serve::bare_response(StatusCode::METHOD_NOT_ALLOWED, true));
        assert_eq!(server.request(PROXY, "GET", PAGE,
            &format!("{HTTPS}Content-Length: {}\r\n", BODY_MAX_BYTES + 1), ""),
            serve::bare_response(StatusCode::PAYLOAD_TOO_LARGE, true));
        assert_eq!(split(&server.request(PROXY, "GET", PAGE, HTTPS, "")).0.status, 200);
    }

    #[test]
    fn only_the_exact_document_path_is_served() {
        let server = Server::start(Some(PUBLIC_ORIGIN));
        assert_eq!(server.request(PROXY, "GET", "/nexus/admin", HTTPS, ""), serve::bare(StatusCode::NOT_FOUND));
        for path in ["/nexus/admin/accounts", "/nexus/admin/index", "/nexus/admin/accounts/"] {
            assert_protected_refusal(&server, path, 404);
        }
        for path in ["/nexus/admin/index.html", "/nexus/admin//", "/nexus/admin/%61", "/nexus/admin/../"] {
            assert_protected_refusal(&server, path, 400);
        }
    }

    fn assert_protected_headers(head: &probe::Head) {
        for (name, value) in PAGE_HEADERS {
            assert_eq!(head.one(name), Some(value), "{name}");
        }
        assert_eq!(head.one("cache-control"), Some("no-store"));
        assert_eq!(head.one("x-content-type-options"), Some("nosniff"));
        assert!(head.all("set-cookie").is_empty());
        assert!(head.all("access-control-allow-origin").is_empty());
    }

    fn assert_protected_refusal(server: &Server, path: &str, status: u16) {
        let (head, body) = split(&server.request(PROXY, "GET", path, HTTPS, ""));
        assert_eq!(head.status, status, "{path}");
        assert!(body.is_empty(), "{path}");
        assert_protected_headers(&head);
    }

    #[test]
    fn csp_hashes_match_the_embedded_blocks() {
        for (tag, directive) in [("script", "script-src"), ("style", "style-src")] {
            let (_, rest) = DOCUMENT.split_once(&format!("<{tag}>")).unwrap();
            let (block, _) = rest.split_once(&format!("</{tag}>")).unwrap();
            let hash = nexus_core::gate::challenge(block).replace('-', "+").replace('_', "/");
            assert!(CSP.contains(&format!("{directive} 'sha256-{hash}='")));
        }
    }

    #[test]
    fn the_protected_document_is_fixed_and_refers_to_no_other_origin() {
        let server = Server::start(Some(PUBLIC_ORIGIN));
        let plain = server.request(PROXY, "GET", PAGE, HTTPS, "");
        let untrusted = server.request(PROXY, "GET", PAGE,
            &format!("{HTTPS}X-Request-Data: <script>sample()</script>\r\nCookie: sample=1\r\n"), "");
        let (head, body) = split(&plain);
        assert_eq!(body, split(&untrusted).1);
        assert_eq!(body, DOCUMENT.as_bytes());
        assert_eq!(head.one("content-type"), Some("text/html; charset=utf-8"));
        assert_eq!(head.one("content-security-policy"), Some(CSP));
        assert!(CSP.contains("frame-ancestors 'none'") && CSP.contains("connect-src 'self'"));
        assert!(!CSP.contains("unsafe-inline") && !CSP.contains("unsafe-eval"));
        assert_protected_headers(&head);
        let html = std::str::from_utf8(&body).unwrap();
        for remote in ["http://", "https://", "//"] {
            assert!(!html.contains(remote), "{remote}");
        }
        assert!(html.contains(account::SIGN_IN) && html.contains(account::SIGN_OUT));
    }

    #[test]
    fn the_sign_in_stub_is_empty_bounded_and_checks_browser_origin() {
        let server = Server::start(Some(PUBLIC_ORIGIN));
        let headers = format!("{HTTPS}Content-Type: application/json\r\nContent-Length: 2\r\n");
        let login = account::SignIn {
            credential: account::Credential::Password {
                email: account::Email::new("sample@example.test").unwrap(),
                password: account::Password::presented("example administrator password").unwrap(),
            },
            device_secret: account::DeviceSecret::from_bytes([0x42; account::SECRET_BYTES]),
            device_name: account::Name::new("Nexus admin browser").unwrap(),
            device_kind: account::DeviceKind::Pc,
            app_version: Version { major: 0, minor: 1, patch: 0 },
            speaks: SPEAKS,
            restore: None,
        };
        let valid = account::encode(&login).unwrap();
        let valid_headers = format!("{HTTPS}{ORIGIN_HEADERS}Content-Type: application/json\r\nContent-Length: {}\r\n", valid.bytes().len());
        let valid_body = std::str::from_utf8(valid.bytes()).unwrap();
        let valid = server.request(PROXY, "POST", account::SIGN_IN, &valid_headers, valid_body);
        let (head, body) = split(&valid);
        assert_eq!(head.status, 501);
        assert!(body.is_empty());
        assert_eq!(head.one("cache-control"), Some("no-store"));
        assert!(head.all("set-cookie").is_empty());
        assert!(head.all("access-control-allow-origin").is_empty());
        assert_eq!(split(&server.request(PROXY, "POST", account::SIGN_IN, &headers, "{}")).0.status, 501);
        for origin in [
            "X-Nexus-Admin: 1\r\n",
            "Origin: https://other.example.test\r\nX-Nexus-Admin: 1\r\n",
            "Origin: https://nexus.example.test:8443\r\nX-Nexus-Admin: 1\r\n",
            "Origin: HTTPS://nexus.example.test\r\nX-Nexus-Admin: 1\r\n",
            "Origin: https://NEXUS.example.test\r\nX-Nexus-Admin: 1\r\n",
            "Origin: https://nexus.example.test.\r\nX-Nexus-Admin: 1\r\n",
            "Origin: http://nexus.example.test\r\nX-Nexus-Admin: 1\r\n",
            "Origin: https://nexus.example.test\r\nX-Nexus-Admin: true\r\n",
            "Origin: https://nexus.example.test\r\nX-Nexus-Admin: 1\r\nX-Nexus-Admin: 1\r\n",
            "Origin: null\r\nX-Nexus-Admin: 1\r\n",
            "Origin: https://nexus.example.test\r\n",
            "Origin: https://nexus.example.test\r\nOrigin: https://nexus.example.test\r\nX-Nexus-Admin: 1\r\n",
        ] {
            let (head, body) = split(&server.request(PROXY, "POST", account::SIGN_IN, &format!("{headers}{origin}"), "{}"));
            assert_eq!(head.status, 400);
            assert!(body.is_empty() && head.all("access-control-allow-origin").is_empty());
        }
        assert_eq!(server.request(PROXY, "OPTIONS", account::SIGN_IN, &format!("{HTTPS}{ORIGIN_HEADERS}"), ""),
            serve::bare(StatusCode::METHOD_NOT_ALLOWED));
        let wrong_type = format!("{HTTPS}{ORIGIN_HEADERS}Content-Type: text/plain\r\nContent-Length: 2\r\n");
        assert_eq!(split(&server.request(PROXY, "POST", account::SIGN_IN, &wrong_type, "{}")).0.status, 400);
        assert_eq!(server.request(PROXY, "POST", account::SIGN_IN,
            &format!("{HTTPS}Content-Length: {}\r\n", account::PASSWORD_REQUEST_MAX_BYTES + 1), ""),
            serve::bare(StatusCode::PAYLOAD_TOO_LARGE));
    }

    #[test]
    fn browser_sign_in_fails_closed_without_a_configured_origin() {
        let server = Server::start(None);
        let headers = format!("{HTTPS}{ORIGIN_HEADERS}Content-Type: application/json\r\nContent-Length: 2\r\n");
        let (head, body) = split(&server.request(PROXY, "POST", SIGN_IN, &headers, "{}"));
        assert_eq!(head.status, 400);
        assert!(body.is_empty());
    }
}
