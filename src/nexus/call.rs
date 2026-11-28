//! One call to the Nexus server, made the way the component's look is made (see `mod.rs`): in
//! a process of its own, this same program started again from its own file, under one
//! deadline counted from before it is started, killed and reaped without blocking if it
//! overruns. A look says what it found by its exit status alone; a call has an answer to hand
//! back, so it is given its request on its standard input and writes the answer to its
//! standard output, and nothing else crosses.
//!
//! **What a call can be, checked twice** (here, before it is started, and again in the call's
//! own process before anything is sent):
//!
//! - **HTTPS only**, to the origin of the address the person typed (`proto::hello_url` decides
//!   whether an origin is one), and **only to one of the account calls' paths**
//!   (`nexus_core::nexus::account::PATHS`), which are all in plain form. So neither a
//!   password nor a session is ever sent anywhere else: not to a music server's paths, not to
//!   an address an answer named.
//! - **Nothing is followed.** The agent follows no redirect: a redirect is an answer, and it
//!   is not the backend's (only a 200 with a whole, valid body is: `account::read_reply`).
//! - **A session only in an `Authorization` header**, never in an address, and never with a
//!   sign-in (which carries the password instead).
//! - **One request on the wire**: a fresh process has no connection to reuse.
//! - **The body read whole or not at all**, at most `account::BODY_MAX_BYTES`.
//!
//! What it is handed is kept for as long as the call alone: the request (which may hold a
//! password or a session) is in memory that is wiped when it is let go of (`Zeroizing`), and
//! in the call's process, which ends with the call.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nexus_core::nexus::{account, proto};
use zeroize::Zeroizing;

use super::{end, note, own_program, test_var};

/// How the program is told it is a call.
pub const CALL_ARG: &str = "--nexus-call";
/// The exit status of a call that has written an answer.
const ANSWERED: i32 = 13;
const NO_ANSWER: i32 = 12;
/// The most of a request the call's process reads: the largest body, and its first two lines.
const REQUEST_MAX: usize = account::BODY_MAX_BYTES + 4096;
/// Inside the call's process, ureq's own limits, so that it normally ends by itself first.
const CONNECT: Duration = Duration::from_secs(8);

/// The calls running, by number: the supervisors', and `shutdown`'s at the app's end.
static RUNNING: Mutex<Vec<(u64, Child)>> = Mutex::new(Vec::new());
static NUMBER: AtomicU64 = AtomicU64::new(0);

/// What a call came back with: the backend's status and its whole body, or nothing.
pub type Answer = Option<(u16, Zeroizing<Vec<u8>>)>;

/// Where a call goes: the origin of the address typed (`https://host[:port]`), and one of the
/// account calls' paths. `None` for anything else.
pub fn target(origin: &str, path: &str) -> Option<String> {
    let url = format!("{origin}{path}");
    let hello = proto::hello_url(origin)?;
    (hello == format!("{origin}{}", proto::HELLO) && account::PATHS.contains(&path) && proto::plain(path).is_ok()).then_some(url)
}

/// The origin of an address a person typed, as calls are made to it: `https://host[:port]`,
/// whatever path came after it. `None` for an address that is not HTTPS, or not one.
pub fn origin(address: &str) -> Option<String> {
    let hello = proto::hello_url(address.trim())?;
    hello.strip_suffix(proto::HELLO).map(str::to_string)
}

/// Makes one call and waits for it, at most `deadline` from now. On a thread of the caller's,
/// never the window's.
pub fn call(origin: &str, path: &str, authorization: Option<&str>, body: &[u8], deadline: Duration) -> Answer {
    let began = Instant::now();
    let url = target(origin, path)?;
    if authorization.is_some_and(|value| !bearer(value)) || (authorization.is_some() && path == account::SIGN_IN) {
        return None;
    }
    let program = own_program()?;
    let mut request = Zeroizing::new(Vec::with_capacity(url.len() + 80 + body.len()));
    request.extend_from_slice(url.as_bytes());
    request.push(b'\n');
    request.extend_from_slice(authorization.unwrap_or_default().as_bytes());
    request.push(b'\n');
    request.extend_from_slice(body);
    let mut command = Command::new(program);
    command.arg(CALL_ARG).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window for it.
        command.creation_flags(0x0800_0000);
    }
    let Ok(mut child) = command.spawn() else { return None };
    let number = NUMBER.fetch_add(1, Ordering::SeqCst);
    note(format_args!("call {number} {path} spawned {}", child.id()));
    let (Some(mut input), Some(mut output)) = (child.stdin.take(), child.stdout.take()) else {
        end(&mut child);
        return None;
    };
    RUNNING.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push((number, child));
    // The request is written, and the answer read, by threads of their own, so that neither
    // can hold the supervisor past the deadline. Each ends when the process does.
    let writer = std::thread::Builder::new().name("nexus-call-in".into()).spawn(move || {
        let _ = input.write_all(&request);
        drop(input);
        drop(request);
    });
    let (sent, answer) = std::sync::mpsc::channel::<Option<Zeroizing<Vec<u8>>>>();
    let reader = std::thread::Builder::new().name("nexus-call-out".into()).spawn(move || {
        let mut bytes = Zeroizing::new(Vec::new());
        let read = (&mut output).take(REQUEST_MAX as u64 + 1).read_to_end(&mut bytes);
        let _ = sent.send((read.is_ok() && bytes.len() <= REQUEST_MAX).then_some(bytes));
    });
    if writer.is_err() || reader.is_err() {
        kill(number);
        return None;
    }
    let status = loop {
        let mut running = RUNNING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Ended by `shutdown`: the app is quitting.
        let Some(at) = running.iter().position(|(n, _)| *n == number) else { return None };
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
                note(format_args!("call {number} killed"));
                return None;
            }
        }
        drop(running);
        std::thread::sleep(Duration::from_millis(20));
    };
    note(format_args!("call {number} ended {:?}", status.code()));
    if status.code() != Some(ANSWERED) {
        return None;
    }
    // The process has ended, so its output has too: what is left is the reader's last read.
    let bytes = answer.recv_timeout(Duration::from_secs(2)).ok()??;
    read_answer(&bytes)
}

fn kill(number: u64) {
    let mut running = RUNNING.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(at) = running.iter().position(|(n, _)| *n == number) {
        let (_, mut child) = running.remove(at);
        drop(running);
        end(&mut child);
    }
}

/// At the app's end: every call still running is killed, and reaped without blocking.
pub fn shutdown() {
    let children: Vec<(u64, Child)> = std::mem::take(&mut *RUNNING.lock().unwrap_or_else(|poisoned| poisoned.into_inner()));
    for (number, mut child) in children {
        end(&mut child);
        note(format_args!("call {number} shutdown"));
    }
}

/// `Bearer ` and a session's 64 lower-case hex digits (`account::Session::authorization`).
fn bearer(value: &str) -> bool {
    account::Session::from_authorization(value).is_some()
}

/// What the call's process wrote: its status on the first line, then the whole body.
fn read_answer(bytes: &[u8]) -> Answer {
    let at = bytes.iter().position(|&byte| byte == b'\n')?;
    let status: u16 = std::str::from_utf8(&bytes[..at]).ok()?.parse().ok()?;
    let body = &bytes[at + 1..];
    (body.len() <= account::BODY_MAX_BYTES).then(|| (status, Zeroizing::new(body.to_vec())))
}

// ---- in the call's own process --------------------------------------------------------------

/// The call itself, in its own process: reads the request, checks it again, sends it, and
/// writes the answer. Returns the code to exit with.
pub fn in_process(mut args: impl Iterator<Item = std::ffi::OsString>) -> i32 {
    if args.next().is_some() {
        return 2;
    }
    let mut request = Zeroizing::new(Vec::new());
    if std::io::stdin().take(REQUEST_MAX as u64 + 1).read_to_end(&mut request).is_err() || request.len() > REQUEST_MAX {
        return 2;
    }
    match test_var("NEXUS_TEST_PROBE_CHILD").as_deref() {
        Some("abort") => std::process::abort(),
        Some("stall") => loop {
            std::thread::park();
        },
        _ => {}
    }
    let mut lines = request.splitn(3, |&byte| byte == b'\n');
    let (Some(url), Some(authorization), Some(body)) = (lines.next(), lines.next(), lines.next()) else { return 2 };
    let (Ok(url), Ok(authorization)) = (std::str::from_utf8(url), std::str::from_utf8(authorization)) else { return 2 };
    // Only ever a call this program makes: the origin and the path checked again.
    let Some(path) = account::PATHS.iter().find(|path| url.ends_with(**path)) else { return 2 };
    let origin = &url[..url.len() - path.len()];
    if target(origin, path).as_deref() != Some(url) || (!authorization.is_empty() && (!bearer(authorization) || *path == account::SIGN_IN)) {
        return 2;
    }
    match send(url, authorization, body) {
        Some((status, answer)) => {
            let mut out = std::io::stdout().lock();
            if write!(out, "{status}\n").is_err() || out.write_all(&answer).is_err() || out.flush().is_err() {
                return NO_ANSWER;
            }
            ANSWERED
        }
        None => NO_ANSWER,
    }
}

/// One POST, with no redirect followed and the body read whole within its limit.
fn send(url: &str, authorization: &str, body: &[u8]) -> Option<(u16, Zeroizing<Vec<u8>>)> {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build();
    let whole = test_var("NEXUS_TEST_PROBE_WHOLE_MS").and_then(|ms| ms.parse().ok()).map(Duration::from_millis);
    let agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .https_only(true)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .timeout_connect(Some(whole.unwrap_or(CONNECT)))
        .timeout_global(Some(whole.unwrap_or(super::CALL_DEADLINE - Duration::from_secs(2))))
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .user_agent(concat!("NexusMusic/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();
    let mut request = agent.post(url).header("Content-Type", "application/json");
    if !authorization.is_empty() {
        request = request.header("Authorization", authorization);
    }
    let mut response = request.send(body).ok()?;
    let status = response.status().as_u16();
    // Any status but 200 is no answer of the backend's: its body is never read.
    if status != account::REPLY_STATUS {
        return Some((status, Zeroizing::new(Vec::new())));
    }
    let bytes = response.body_mut().with_config().limit(account::BODY_MAX_BYTES as u64).read_to_vec().ok()?;
    Some((status, Zeroizing::new(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A call goes only to HTTPS, only to an account call's path, and only to the origin.
    #[test]
    fn a_call_goes_only_to_an_account_path_over_https() {
        let origin = origin("https://nexus.example.org/somewhere/else").unwrap();
        assert_eq!(origin, "https://nexus.example.org");
        assert_eq!(target(&origin, account::SIGN_IN).as_deref(), Some("https://nexus.example.org/nexus/api/signin"));
        assert_eq!(target(&origin, "/rest/ping"), None);
        assert_eq!(target(&origin, "/nexus/api/hello"), None);
        assert_eq!(target("http://nexus.example.org", account::SIGN_IN), None);
        assert_eq!(target("https://nexus.example.org/x", account::SIGN_IN), None);
        assert_eq!(super::origin("http://nexus.example.org"), None);
        assert_eq!(super::origin("nexus.example.org"), None);
        assert_eq!(super::origin("https://nexus.example.org:8443").as_deref(), Some("https://nexus.example.org:8443"));
    }

    /// A session only in its own form, and never with a sign-in.
    #[test]
    fn a_session_goes_only_as_a_bearer_and_never_with_a_password() {
        let session = account::Session::from_bytes([7; account::SECRET_BYTES]).authorization();
        assert!(bearer(&session));
        assert!(!bearer("Basic abc"));
        assert!(call("https://nexus.example.org", account::SIGN_IN, Some(&session), b"{}", Duration::from_secs(1)).is_none());
        assert!(call("https://nexus.example.org", account::LIST_DEVICES, Some("Bearer nope"), b"{}", Duration::from_secs(1)).is_none());
    }

    #[test]
    fn an_answer_is_its_status_and_its_whole_body() {
        let (status, body) = read_answer(b"200\n{\"done\":1}").unwrap();
        assert_eq!((status, body.as_slice()), (200, &b"{\"done\":1}"[..]));
        assert!(read_answer(b"no status").is_none());
        assert!(read_answer(b"abc\n{}").is_none());
    }
}
