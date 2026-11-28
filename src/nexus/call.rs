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
//! **Sent only if granted at the send itself** (the review of W7, MUST 4). The ask's process
//! connects (the name, the connection, TLS and the certificate check), and then, immediately
//! before the request's first byte goes, asks its parent for the send: one line, `?`. The
//! parent asks the machine (`grant`: `Client::waits_for`, at that moment) and answers `G` or
//! `N`; anything but `G` (the parent gone included) and nothing is sent. So a request granted
//! once is never sent after Offline, a sign-out, a newer press or the app's quitting has
//! made the machine stop waiting for it, however long its process took to get there.
//!
//! **One deadline, the ask's own**, given to its process too (MUST 8): every phase in it is
//! held to what is left of it (the connection to at most eight seconds of it), and the
//! parent kills the process at the deadline whatever it is doing.
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
/// Not granted its send: nothing was sent.
const UNSENT: i32 = 25;
/// The most of a request the ask's process reads: the largest body, and its first lines.
const REQUEST_MAX: usize = account::BODY_MAX_BYTES + 4096;

/// The asks running, by number: the supervisors', and `shutdown`'s at the app's end.
static RUNNING: Mutex<Vec<(u64, Child)>> = Mutex::new(Vec::new());
static NUMBER: AtomicU64 = AtomicU64::new(0);

/// Why an ask came back with no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsent {
    /// The machine no longer waited for it when it was about to go: nothing was sent.
    Refused,
    /// How far it got.
    Failed(How),
}

/// What an ask came back with: the status and whole body, or why not.
pub type Answer = Result<(u16, Zeroizing<Vec<u8>>), Unsent>;

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
    // The account calls, and what the sending half sends (design 13.3, 14.4).
    let sending = [nexus_core::nexus::events::SEND_PATH, nexus_core::nexus::diag::SEND_PATH];
    let known = if get { path == proto::HELLO } else { account::PATHS.contains(&path) || sending.contains(&path) };
    let session_ok = match authorization {
        None => true,
        Some(value) => !get && path != account::SIGN_IN && account::Session::from_authorization(value).is_some(),
    };
    known && proto::plain(path).is_ok() && session_ok
}

/// Sends one request and waits for it, at most `deadline` from now; `grant` is asked
/// immediately before its first byte goes (on this thread). On a thread of the caller's,
/// never the window's.
pub fn send(request: &Request<'_>, deadline: Duration, grant: &dyn Fn() -> bool) -> Answer {
    let began = Instant::now();
    let failed = |how| Err(Unsent::Failed(how));
    if !allowed(request.get, request.url, request.authorization) || request.max_body > account::BODY_MAX_BYTES {
        return failed(How::Connect);
    }
    let Some(program) = own_program() else { return failed(How::Connect) };
    let body = request.body.unwrap_or_default();
    let mut input = Zeroizing::new(Vec::with_capacity(request.url.len() + 160 + body.len()));
    input.extend_from_slice(if request.get { b"GET\n" } else { b"POST\n" });
    input.extend_from_slice(request.url.as_bytes());
    input.push(b'\n');
    input.extend_from_slice(request.authorization.unwrap_or_default().as_bytes());
    input.push(b'\n');
    input.extend_from_slice(request.max_body.to_string().as_bytes());
    input.push(b'\n');
    input.extend_from_slice(deadline.as_millis().to_string().as_bytes());
    input.push(b'\n');
    input.extend_from_slice(body.len().to_string().as_bytes());
    input.push(b'\n');
    input.extend_from_slice(body.as_bytes());
    let mut command = Command::new(program);
    command.arg(CALL_ARG).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window for it.
        command.creation_flags(0x0800_0000);
    }
    let Ok(mut child) = command.spawn() else { return failed(How::Connect) };
    let number = NUMBER.fetch_add(1, Ordering::SeqCst);
    let path = request.url.split_once("/nexus/").map(|(_, path)| path).unwrap_or_default();
    note(format_args!("ask {number} {path} spawned {}", child.id()));
    let (Some(mut stdin), Some(mut stdout)) = (child.stdin.take(), child.stdout.take()) else {
        end(&mut child);
        return failed(How::Connect);
    };
    RUNNING.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push((number, child));
    // The request is written, the grant answered, and the answer read, by threads of their
    // own, so that none can hold the supervisor past the deadline. Each ends when the
    // process does.
    let (decided, decision) = std::sync::mpsc::channel::<bool>();
    let writer = std::thread::Builder::new().name("nexus-ask-in".into()).spawn(move || {
        if stdin.write_all(&input).is_err() || stdin.flush().is_err() {
            return;
        }
        drop(input);
        // The process asks for its send: the supervisor's answer, once.
        let go = decision.recv().unwrap_or(false);
        let _ = stdin.write_all(if go { b"G" } else { b"N" });
        drop(stdin);
    });
    let (asked, asking) = std::sync::mpsc::channel::<()>();
    let (sent, answer) = std::sync::mpsc::channel::<Option<Zeroizing<Vec<u8>>>>();
    let reader = std::thread::Builder::new().name("nexus-ask-out".into()).spawn(move || {
        let mut ask = [0u8; 2];
        if stdout.read_exact(&mut ask).is_err() || &ask != b"?\n" {
            let _ = sent.send(None);
            return;
        }
        let _ = asked.send(());
        let mut bytes = Zeroizing::new(Vec::new());
        let read = (&mut stdout).take(REQUEST_MAX as u64 + 1).read_to_end(&mut bytes);
        let _ = sent.send((read.is_ok() && bytes.len() <= REQUEST_MAX).then_some(bytes));
    });
    if writer.is_err() || reader.is_err() {
        kill(number);
        return failed(How::Connect);
    }
    let mut decided = Some(decided);
    let mut granted = None;
    let status = loop {
        if asking.try_recv().is_ok()
            && let Some(decided) = decided.take()
        {
            // Immediately before its first byte: the machine, now.
            let go = grant();
            granted = Some(go);
            let _ = decided.send(go);
            if !go {
                note(format_args!("ask {number} not granted"));
            }
        }
        let mut running = RUNNING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Ended by `shutdown`: the app is quitting.
        let Some(at) = running.iter().position(|(n, _)| *n == number) else { return Err(Unsent::Failed(How::Timeout)) };
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
                return Err(Unsent::Failed(How::Timeout));
            }
        }
        drop(running);
        std::thread::sleep(Duration::from_millis(10));
    };
    note(format_args!("ask {number} ended {:?}", status.code()));
    if granted == Some(false) {
        return Err(Unsent::Refused);
    }
    match status.code() {
        Some(ANSWERED) => {}
        Some(UNSENT) => return Err(Unsent::Refused),
        Some(FAILED_NAME) => return failed(How::Name),
        Some(FAILED_SECURE) => return failed(How::Secure),
        Some(FAILED_TIMEOUT) => return failed(How::Timeout),
        Some(FAILED_CUT) => return failed(How::Cut),
        _ => return failed(How::Connect),
    }
    // The process has ended, so its output has too: what is left is the reader's last read.
    let bytes = match answer.recv_timeout(Duration::from_secs(2)) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            note(format_args!("ask {number} answer unreadable"));
            return failed(How::Cut);
        }
        Err(_) => {
            note(format_args!("ask {number} answer not read in time"));
            return failed(How::Cut);
        }
    };
    let read = read_answer(&bytes, request.max_body);
    if read.is_none() {
        note(format_args!("ask {number} answer of {} bytes not an answer", bytes.len()));
    }
    read.ok_or(Unsent::Failed(How::Cut))
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

/// The ask itself, in its own process: reads the request, checks it again, connects, asks
/// for its send, sends it if granted, and writes the answer. Returns the code to exit with.
pub fn in_process(mut args: impl Iterator<Item = std::ffi::OsString>) -> i32 {
    if args.next().is_some() {
        return BAD;
    }
    // The request, read under the standard input's lock, which is let go of before the
    // send is asked for (`granted` reads the same input).
    let read = {
        let mut stdin = std::io::stdin().lock();
        let mut line = |most: usize| -> Option<Zeroizing<Vec<u8>>> {
            let mut bytes = Zeroizing::new(Vec::new());
            let mut byte = [0u8; 1];
            loop {
                stdin.read_exact(&mut byte).ok()?;
                if byte[0] == b'\n' {
                    return Some(bytes);
                }
                if bytes.len() >= most {
                    return None;
                }
                bytes.push(byte[0]);
            }
        };
        let (Some(method), Some(url), Some(authorization), Some(max_body), Some(within), Some(length)) =
            (line(8), line(4096), line(4096), line(32), line(32), line(32))
        else {
            return BAD;
        };
        let number = |bytes: &[u8]| std::str::from_utf8(bytes).ok().and_then(|text| text.parse::<usize>().ok());
        let (Ok(url), Ok(authorization), Some(max_body), Some(within), Some(length)) =
            (std::str::from_utf8(&url), std::str::from_utf8(&authorization), number(&max_body), number(&within), number(&length))
        else {
            return BAD;
        };
        if length > REQUEST_MAX {
            return BAD;
        }
        let mut body = Zeroizing::new(vec![0u8; length]);
        if stdin.read_exact(&mut body).is_err() {
            return BAD;
        }
        (method, url.to_string(), Zeroizing::new(authorization.to_string()), max_body, within, body)
    };
    let (method, url, authorization, max_body, within, body) = read;
    let (url, authorization) = (url.as_str(), authorization.as_str());
    match test_var("NEXUS_TEST_PROBE_CHILD").as_deref() {
        Some("abort") => std::process::abort(),
        Some("stall") => loop {
            std::thread::park();
        },
        _ => {}
    }
    let get = match method.as_slice() {
        b"GET" => true,
        b"POST" => false,
        _ => return BAD,
    };
    let authorization = (!authorization.is_empty()).then_some(authorization);
    // Only ever a request this program makes: checked again.
    if !allowed(get, url, authorization) || max_body > account::BODY_MAX_BYTES || (get && !body.is_empty()) {
        return BAD;
    }
    let within = Duration::from_millis(within as u64);
    match fetch(get, url, authorization, &body, max_body, within) {
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

/// Asks the parent for the send, once: `?` on the standard output, `G` back on the standard
/// input. Anything else, and nothing is sent.
fn granted() -> bool {
    use std::sync::atomic::AtomicU8;
    // 0 not asked yet, 1 granted, 2 refused.
    static ASKED: AtomicU8 = AtomicU8::new(0);
    match ASKED.load(Ordering::SeqCst) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    let mut out = std::io::stdout().lock();
    let asked = out.write_all(b"?\n").is_ok() && out.flush().is_ok();
    drop(out);
    let mut answer = [0u8; 1];
    let go = asked && std::io::stdin().read_exact(&mut answer).is_ok() && answer[0] == b'G';
    ASKED.store(if go { 1 } else { 2 }, Ordering::SeqCst);
    go
}

/// The connection's first byte of request waits for `granted` (after the name, the connection
/// and TLS): see the module's notes.
mod grant {
    use ureq::unversioned::transport::{Buffers, ConnectionDetails, Connector, NextTimeout, Transport};

    #[derive(Debug)]
    pub struct Asking;

    impl Connector<Box<dyn Transport>> for Asking {
        type Out = Granted;

        fn connect(&self, _: &ConnectionDetails, chained: Option<Box<dyn Transport>>) -> Result<Option<Granted>, ureq::Error> {
            Ok(chained.map(|inner| Granted { inner }))
        }
    }

    pub struct Granted {
        inner: Box<dyn Transport>,
    }

    impl std::fmt::Debug for Granted {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Granted")
        }
    }

    impl Transport for Granted {
        fn buffers(&mut self) -> &mut dyn Buffers {
            self.inner.buffers()
        }

        fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
            if !super::granted() {
                return Err(ureq::Error::Io(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "not granted")));
            }
            self.inner.transmit_output(amount, timeout)
        }

        fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
            self.inner.await_input(timeout)
        }

        fn is_open(&mut self) -> bool {
            self.inner.is_open()
        }

        fn is_tls(&self) -> bool {
            self.inner.is_tls()
        }
    }
}

/// One request, with no redirect followed and the body read whole within its limit, all
/// within `within` (the connection within at most eight seconds of it).
fn fetch(get: bool, url: &str, authorization: Option<&str>, body: &[u8], max_body: usize, within: Duration) -> Result<(u16, Zeroizing<Vec<u8>>), i32> {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    use ureq::unversioned::transport::{Connector, DefaultConnector};
    let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build();
    let whole = test_var("NEXUS_TEST_PROBE_WHOLE_MS").and_then(|ms| ms.parse().ok()).map(Duration::from_millis).unwrap_or(within);
    // A little short of the parent's own kill, so that a time-out is said as one.
    let whole = whole.saturating_sub(Duration::from_millis(250)).max(Duration::from_millis(100));
    let config = ureq::Agent::config_builder()
        .tls_config(tls)
        .https_only(true)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .timeout_connect(Some(whole.min(Duration::from_secs(8))))
        .timeout_global(Some(whole))
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .user_agent(concat!("NexusMusic/", env!("CARGO_PKG_VERSION")))
        .build();
    let agent = ureq::Agent::with_parts(config, DefaultConnector::new().chain(grant::Asking), ureq::unversioned::resolver::DefaultResolver::default());
    let sent = if get {
        agent.get(url).call()
    } else {
        let mut request = agent.post(url).header("Content-Type", "application/json");
        if let Some(value) = authorization {
            request = request.header("Authorization", value);
        }
        request.send(body)
    };
    let mut response = match sent {
        Ok(response) => response,
        Err(ureq::Error::Io(io)) if io.kind() == std::io::ErrorKind::PermissionDenied => return Err(UNSENT),
        Err(error) => return Err(failed(&error)),
    };
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
