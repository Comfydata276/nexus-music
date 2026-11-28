//! One ask of the Nexus server's (`nexus_core::nexus::client::Ask`), sent the way the
//! component's look is sent (see `mod.rs`): in a process of its own, this same program started
//! again from its own file, under one deadline counted from before it is started, killed and
//! reaped without blocking if it overruns. A look says what it found by its exit status
//! alone; an ask has an answer to hand back, so it is given its request on its standard input
//! and writes the answer to its standard output, and nothing else crosses.
//!
//! **What may be sent, checked twice** (here, before the process is started, and again in it
//! before anything goes out), whatever the core asks:
//!
//! - **HTTPS only**, to an address read by the one reading of an address
//!   (`nexus_core::origin::Address::parse`), with no query, **and only to the backend's own
//!   paths**: `GET` for `hello`, `POST` for one of the account calls
//!   (`nexus_core::nexus::account::PATHS`). So neither a password nor a session is ever sent
//!   anywhere else: not to a music server's paths, and not to an address an answer named.
//! - **Nothing is followed.** The agent follows no redirect: a redirect is handed back as the
//!   status it is, with no body, and is never the backend's answer.
//! - **A session only in an `Authorization` header**, never in an address, never with
//!   `hello` or a sign-in.
//! - **One request on the wire**: a fresh process has no connection to reuse.
//! - **A body read whole or not at all**, within the ask's own bound; for `hello`, a status
//!   other than 200, and for anything a redirect, is handed back with no body read.
//!
//! What it is handed is kept for as long as the ask alone: the request (which may hold a
//! password or a session) is in memory wiped when it is let go of (`Zeroizing`), and in the
//! ask's process, which ends with it.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nexus_core::nexus::client::How;
use nexus_core::nexus::{account, proto};
use nexus_core::origin::Address;
use zeroize::Zeroizing;

use super::{end, note, own_program, test_var};

/// How the program is told it is an ask.
pub const CALL_ARG: &str = "--nexus-call";
/// The exit status of an ask that has written an answer; and of one that could not be made,
/// by how far it got.
const ANSWERED: i32 = 13;
const BAD: i32 = 2;
const FAILED_NAME: i32 = 20;
const FAILED_CONNECT: i32 = 21;
const FAILED_SECURE: i32 = 22;
const FAILED_TIMEOUT: i32 = 23;
const FAILED_CUT: i32 = 24;
/// The most of a request the ask's process reads: the largest body, and its first lines.
const REQUEST_MAX: usize = account::BODY_MAX_BYTES + 4096;

/// The asks running, by number: the supervisors', and `shutdown`'s at the app's end.
static RUNNING: Mutex<Vec<(u64, Child)>> = Mutex::new(Vec::new());
static NUMBER: AtomicU64 = AtomicU64::new(0);

/// What an ask came back with: the status and whole body, or how far it got.
pub type Answer = Result<(u16, Zeroizing<Vec<u8>>), How>;

/// One request, as the core asked it.
pub struct Request<'a> {
    pub get: bool,
    pub url: &'a str,
    pub authorization: Option<&'a str>,
    pub body: Option<&'a str>,
    pub max_body: usize,
}

/// Whether `url` (with `get` or not, a session or not) is a request this component may make.
pub fn allowed(get: bool, url: &str, authorization: Option<&str>) -> bool {
    let Ok(address) = Address::parse(url) else { return false };
    if address.query().is_some() || address.origin().scheme() != "https" || address.origin().to_string() + address.path() != url {
        return false;
    }
    let path = address.path();
    let known = if get { path == proto::HELLO } else { account::PATHS.contains(&path) };
    let session_ok = match authorization {
        None => true,
        Some(value) => !get && path != account::SIGN_IN && account::Session::from_authorization(value).is_some(),
    };
    known && proto::plain(path).is_ok() && session_ok
}

/// Sends one request and waits for it, at most `deadline` from now. On a thread of the
/// caller's, never the window's.
pub fn send(request: &Request<'_>, deadline: Duration) -> Answer {
    let began = Instant::now();
    if !allowed(request.get, request.url, request.authorization) || request.max_body > account::BODY_MAX_BYTES {
        return Err(How::Connect);
    }
    let program = own_program().ok_or(How::Connect)?;
    let mut input = Zeroizing::new(Vec::with_capacity(request.url.len() + 128 + request.body.map_or(0, str::len)));
    input.extend_from_slice(if request.get { b"GET\n" } else { b"POST\n" });
    input.extend_from_slice(request.url.as_bytes());
    input.push(b'\n');
    input.extend_from_slice(request.authorization.unwrap_or_default().as_bytes());
    input.push(b'\n');
    input.extend_from_slice(request.max_body.to_string().as_bytes());
    input.push(b'\n');
    input.extend_from_slice(request.body.unwrap_or_default().as_bytes());
    let mut command = Command::new(program);
    command.arg(CALL_ARG).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window for it.
        command.creation_flags(0x0800_0000);
    }
    let Ok(mut child) = command.spawn() else { return Err(How::Connect) };
    let number = NUMBER.fetch_add(1, Ordering::SeqCst);
    let path = request.url.split_once("/nexus/").map(|(_, path)| path).unwrap_or_default();
    note(format_args!("ask {number} {path} spawned {}", child.id()));
    let (Some(mut stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
        end(&mut child);
        return Err(How::Connect);
    };
    RUNNING.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push((number, child));
    // The request is written, and the answer read, by threads of their own, so that neither
    // can hold the supervisor past the deadline. Each ends when the process does.
    let writer = std::thread::Builder::new().name("nexus-ask-in".into()).spawn(move || {
        let _ = stdin.write_all(&input);
        drop(stdin);
        drop(input);
    });
    let (sent, answer) = std::sync::mpsc::channel::<Option<Zeroizing<Vec<u8>>>>();
    let reader = std::thread::Builder::new().name("nexus-ask-out".into()).spawn(move || {
        let mut bytes = Zeroizing::new(Vec::new());
        let read = (&mut stdout).take(REQUEST_MAX as u64 + 1).read_to_end(&mut bytes);
        let _ = sent.send((read.is_ok() && bytes.len() <= REQUEST_MAX).then_some(bytes));
    });
    if writer.is_err() || reader.is_err() {
        kill(number);
        return Err(How::Connect);
    }
    let status = loop {
        let mut running = RUNNING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Ended by `shutdown`: the app is quitting.
        let Some(at) = running.iter().position(|(n, _)| *n == number) else { return Err(How::Timeout) };
        match running[at].1.try_wait() {
            Ok(Some(status)) if began.elapsed() <= deadline => {
                running.remove(at);
                break status;
            }
            Ok(None) if began.elapsed() <= deadline => {}
            _ => {
                let (_, mut child) = running.remove(at);
                drop(running);
                end(&mut child);
                note(format_args!("ask {number} killed"));
                return Err(How::Timeout);
            }
        }
        drop(running);
        std::thread::sleep(Duration::from_millis(20));
    };
    note(format_args!("ask {number} ended {:?}", status.code()));
    match status.code() {
        Some(ANSWERED) => {}
        Some(FAILED_NAME) => return Err(How::Name),
        Some(FAILED_SECURE) => return Err(How::Secure),
        Some(FAILED_TIMEOUT) => return Err(How::Timeout),
        Some(FAILED_CUT) => return Err(How::Cut),
        _ => return Err(How::Connect),
    }
    // The process has ended, so its output has too: what is left is the reader's last read.
    let bytes = answer.recv_timeout(Duration::from_secs(2)).ok().flatten().ok_or(How::Cut)?;
    read_answer(&bytes, request.max_body).ok_or(How::Cut)
}

fn kill(number: u64) {
    let mut running = RUNNING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(at) = running.iter().position(|(n, _)| *n == number) {
        let (_, mut child) = running.remove(at);
        drop(running);
        end(&mut child);
    }
}

/// At the app's end: every ask still running is killed, and reaped without blocking.
pub fn shutdown() {
    let children: Vec<(u64, Child)> = std::mem::take(&mut *RUNNING.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
    for (number, mut child) in children {
        end(&mut child);
        note(format_args!("ask {number} shutdown"));
    }
}

/// What the ask's process wrote: its status on the first line, then the whole body.
fn read_answer(bytes: &[u8], max_body: usize) -> Option<(u16, Zeroizing<Vec<u8>>)> {
    let at = bytes.iter().position(|&byte| byte == b'\n')?;
    let status: u16 = std::str::from_utf8(&bytes[..at]).ok()?.parse().ok()?;
    let body = &bytes[at + 1..];
    (body.len() <= max_body).then(|| (status, Zeroizing::new(body.to_vec())))
}

// ---- in the ask's own process --------------------------------------------------------------

/// The ask itself, in its own process: reads the request, checks it again, sends it, and
/// writes the answer. Returns the code to exit with.
pub fn in_process(mut args: impl Iterator<Item = std::ffi::OsString>) -> i32 {
    if args.next().is_some() {
        return BAD;
    }
    let mut request = Zeroizing::new(Vec::new());
    if std::io::stdin().take(REQUEST_MAX as u64 + 1).read_to_end(&mut request).is_err() || request.len() > REQUEST_MAX {
        return BAD;
    }
    match test_var("NEXUS_TEST_PROBE_CHILD").as_deref() {
        Some("abort") => std::process::abort(),
        Some("stall") => loop {
            std::thread::park();
        },
        _ => {}
    }
    let mut lines = request.splitn(5, |&byte| byte == b'\n');
    let (Some(method), Some(url), Some(authorization), Some(max_body), Some(body)) = (lines.next(), lines.next(), lines.next(), lines.next(), lines.next()) else { return BAD };
    let (Ok(url), Ok(authorization), Some(max_body)) =
        (std::str::from_utf8(url), std::str::from_utf8(authorization), std::str::from_utf8(max_body).ok().and_then(|text| text.parse::<usize>().ok()))
    else {
        return BAD;
    };
    let get = match method {
        b"GET" => true,
        b"POST" => false,
        _ => return BAD,
    };
    let authorization = (!authorization.is_empty()).then_some(authorization);
    // Only ever a request this program makes: checked again.
    if !allowed(get, url, authorization) || max_body > account::BODY_MAX_BYTES || (get && !body.is_empty()) {
        return BAD;
    }
    match fetch(get, url, authorization, body, max_body) {
        Ok((status, answer)) => {
            let mut out = std::io::stdout().lock();
            if write!(out, "{status}\n").is_err() || out.write_all(&answer).is_err() || out.flush().is_err() {
                return FAILED_CUT;
            }
            ANSWERED
        }
        Err(code) => code,
    }
}

/// One request, with no redirect followed and the body read whole within its limit.
fn fetch(get: bool, url: &str, authorization: Option<&str>, body: &[u8], max_body: usize) -> Result<(u16, Zeroizing<Vec<u8>>), i32> {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build();
    let whole = test_var("NEXUS_TEST_PROBE_WHOLE_MS").and_then(|ms| ms.parse().ok()).map(Duration::from_millis);
    let agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .https_only(true)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .timeout_connect(Some(whole.unwrap_or(Duration::from_secs(8))))
        .timeout_global(Some(whole.unwrap_or(Duration::from_secs(28))))
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .user_agent(concat!("NexusMusic/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();
    let sent = if get {
        agent.get(url).call()
    } else {
        let mut request = agent.post(url).header("Content-Type", "application/json");
        if let Some(value) = authorization {
            request = request.header("Authorization", value);
        }
        request.send(body)
    };
    let mut response = sent.map_err(|error| failed(&error))?;
    let status = response.status().as_u16();
    // `hello`'s answer is the backend's only with 200: any other's body is never read. A
    // call's refusals come with statuses of their own and a small body (design 7.1), read
    // whole as any answer is. A redirect's body is never read: it is not followed, and it
    // is not the backend's.
    if (get && status != 200) || (300..400).contains(&status) {
        return Ok((status, Zeroizing::new(Vec::new())));
    }
    let bytes = response.body_mut().with_config().limit(max_body as u64).read_to_vec().map_err(|error| match failed(&error) {
        FAILED_TIMEOUT => FAILED_TIMEOUT,
        _ => FAILED_CUT,
    })?;
    Ok((status, Zeroizing::new(bytes)))
}

/// How far a request got, from ureq's error.
fn failed(error: &ureq::Error) -> i32 {
    use ureq::Timeout;
    match error {
        ureq::Error::HostNotFound => FAILED_NAME,
        ureq::Error::Timeout(Timeout::Resolve) => FAILED_NAME,
        ureq::Error::Timeout(Timeout::Connect) | ureq::Error::ConnectionFailed => FAILED_CONNECT,
        ureq::Error::Timeout(_) => FAILED_TIMEOUT,
        ureq::Error::Tls(_) | ureq::Error::NativeTls(_) | ureq::Error::TlsRequired => FAILED_SECURE,
        ureq::Error::BodyExceedsLimit(_) => FAILED_CUT,
        ureq::Error::Io(io) if matches!(io.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::AddrNotAvailable | std::io::ErrorKind::NetworkUnreachable | std::io::ErrorKind::HostUnreachable) => FAILED_CONNECT,
        _ => FAILED_TIMEOUT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the backend's own paths, over HTTPS: GET for `hello` alone, POST for the calls.
    #[test]
    fn an_ask_goes_only_to_the_backends_paths_over_https() {
        let at = |path: &str| format!("https://nexus.example.org{path}");
        assert!(allowed(true, &at(proto::HELLO), None));
        assert!(allowed(false, &at(account::SIGN_IN), None));
        assert!(!allowed(false, &at(proto::HELLO), None));
        assert!(!allowed(true, &at(account::SIGN_IN), None));
        assert!(!allowed(true, &at("/rest/ping"), None));
        assert!(!allowed(false, &at("/rest/ping"), None));
        assert!(!allowed(false, "http://nexus.example.org/nexus/api/signin", None));
        assert!(!allowed(false, &format!("{}?x=1", at(account::SIGN_IN)), None));
        assert!(!allowed(false, &at("/nexus/api/signin/"), None));
        assert!(!allowed(false, "https://user:pw@nexus.example.org/nexus/api/signin", None));
    }

    /// A session only in its own form, and never with `hello` or a sign-in.
    #[test]
    fn a_session_goes_only_as_a_bearer_and_never_with_a_password() {
        let session = account::Session::from_bytes([7; account::SECRET_BYTES]).authorization();
        let at = |path: &str| format!("https://nexus.example.org{path}");
        assert!(allowed(false, &at(account::LIST_DEVICES), Some(&session)));
        assert!(!allowed(false, &at(account::SIGN_IN), Some(&session)));
        assert!(!allowed(true, &at(proto::HELLO), Some(&session)));
        assert!(!allowed(false, &at(account::LIST_DEVICES), Some("Basic abc")));
    }

    #[test]
    fn an_answer_is_its_status_and_its_whole_body() {
        let (status, body) = read_answer(b"200\n{\"done\":1}", 100).unwrap();
        assert_eq!((status, body.as_slice()), (200, &b"{\"done\":1}"[..]));
        assert!(read_answer(b"200\n{\"done\":1}", 4).is_none());
        assert!(read_answer(b"no status", 100).is_none());
    }
}
