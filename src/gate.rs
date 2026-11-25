//! Signing in to the login that can sit in front of Lidarr.
//!
//! Some people keep Lidarr behind Authentik, a sign-in service: a request only gets through
//! if it comes from someone signed in there. A password typed into this app would have to be
//! kept by it, so the sign-in happens in the web browser instead, with whatever second step
//! the service asks for, and the browser hands this app a token. That token is the only thing
//! kept. It's swapped for a new one each time it's used, and traded when needed for a
//! short-lived pass that the sign-in in front of Lidarr accepts.
//!
//! What is asked of the service and what is made of its answers are in the library
//! (`nexus_core::gate`), for the Android app to use too. This is the Windows app's end: the
//! connection it's asked over, and the browser's part of signing in.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nexus_core::gate::{base64url, param};
pub use nexus_core::gate::{Found, Session, SignedIn};
pub use nexus_core::lidarr::NEEDED;

use nexus_core::gate::{Page, WAIT};

/// The connection the sign-in service is asked over: one of the app's agents, which must be
/// one that doesn't follow redirects.
fn over(agent: &ureq::Agent) -> crate::corehttp::Agent {
    crate::corehttp::Agent(agent.clone())
}

/// Looks at what answers at Lidarr's address. `None` if it's Lidarr itself; otherwise the
/// sign-in in front of it.
pub fn find(agent: &ureq::Agent, lidarr: &str) -> Result<Option<Found>, String> {
    nexus_core::gate::find(&over(agent), lidarr)
}

/// Tells the service a token is no longer wanted. Best effort.
pub fn give_up_token(agent: &ureq::Agent, service: &str, token: &str) {
    nexus_core::gate::give_up_token(&over(agent), service, token)
}

/// A sign-in that's been done, asked over an agent of the app's own: hands out passes for
/// Lidarr and keeps the token fresh. `keep` is called with the token whenever it changes,
/// to put it away; with "" when it's dead.
pub fn session(found: Found, token: String, keep: impl Fn(&str) + Send + Sync + 'static) -> Session {
    Session::new(found, token, keep, Arc::new(crate::corehttp::Agent(crate::subsonic::make_agent_without_redirects())))
}

/// Letters nobody could guess, from the system's own source of randomness.
fn unguessable(bytes: usize) -> Result<String, String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).map_err(|_| nexus_core::gate::no_random_numbers("PC"))?;
    Ok(base64url(&buffer))
}

// ---- signing in with the browser ------------------------------------------------------------------

/// Only the newest attempt to sign in counts; starting another (or cancelling) retires the rest.
static ROUND: AtomicU64 = AtomicU64::new(0);

/// Retires any attempt in progress and returns the number of the next one.
pub fn new_round() -> u64 {
    ROUND.fetch_add(1, Ordering::SeqCst) + 1
}

pub fn is_current(round: u64) -> bool {
    ROUND.load(Ordering::SeqCst) == round
}

/// A sign-in that's under way: the browser has somewhere to come back to.
pub struct Attempt {
    listener: TcpListener,
    found: Found,
    secret: String,
    state: String,
    back: String,
    /// The page to open in the browser.
    pub link: String,
}

/// Gets ready for a sign-in: opens a door on this PC for the browser to come back through.
pub fn begin(found: &Found) -> Result<Attempt, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| nexus_core::gate::not_ready(&e))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let back = format!("http://127.0.0.1:{port}/callback");
    let secret = unguessable(48)?;
    let state = unguessable(18)?;
    let link = nexus_core::gate::sign_in_link(found, &back, &state, &secret);
    Ok(Attempt { listener, found: found.clone(), secret, state, back, link })
}

fn reply(stream: &mut TcpStream, status: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

/// The first line of what a browser sent, e.g. "GET /callback?code=… HTTP/1.1".
fn first_line(stream: &mut TcpStream) -> String {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut seen = Vec::new();
    let mut buffer = [0u8; 2048];
    while !seen.windows(2).any(|pair| pair == b"\r\n") && seen.len() < 16 * 1024 {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => seen.extend_from_slice(&buffer[..count]),
        }
    }
    String::from_utf8_lossy(&seen).lines().next().unwrap_or_default().to_string()
}

impl Attempt {
    /// Waits for the browser to come back, then collects the token. `given_up` is asked now
    /// and then whether to stop waiting.
    pub fn finish(self, agent: &ureq::Agent, given_up: impl Fn() -> bool) -> Result<SignedIn, String> {
        let code = self.wait(&given_up)?;
        nexus_core::gate::finish(&over(agent), &self.found, &code, &self.back, &self.secret)
    }

    fn wait(&self, given_up: &impl Fn() -> bool) -> Result<String, String> {
        self.listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + WAIT;
        loop {
            if given_up() {
                return Err(String::new());
            }
            if Instant::now() > deadline {
                return Err(nexus_core::gate::NOBODY_SIGNED_IN.into());
            }
            let mut stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(e) => return Err(nexus_core::gate::not_heard(&e)),
            };
            let line = first_line(&mut stream);
            let target = line.split_whitespace().nth(1).unwrap_or_default();
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            if path != "/callback" || param(query, "state").as_deref() != Some(self.state.as_str()) {
                reply(&mut stream, "404 Not Found", &nexus_core::gate::page(Page::NothingHere));
                continue;
            }
            if let Some(error) = param(query, "error") {
                let why = param(query, "error_description").filter(|why| !why.is_empty()).unwrap_or(error);
                reply(&mut stream, "200 OK", &nexus_core::gate::page(Page::NotThrough));
                return Err(nexus_core::gate::turned_down(&why));
            }
            if let Some(code) = param(query, "code").filter(|code| !code.is_empty()) {
                reply(&mut stream, "200 OK", &nexus_core::gate::page(Page::SignedIn));
                return Ok(code);
            }
            reply(&mut stream, "400 Bad Request", &nexus_core::gate::page(Page::NothingHere));
        }
    }
}

/// Opens a web page in the browser.
pub fn open_link(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    // For testing without a browser: the address is written to the named file instead.
    if let Some(file) = std::env::var_os("NEXUS_OPEN_LINKS") {
        let _ = std::fs::write(file, url);
        return;
    }
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        use windows::core::{HSTRING, PCWSTR, w};
        ShellExecuteW(None, w!("open"), &HSTRING::from(url), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The page as 0.40.8 made it, kept to hold the library's to.
    fn page_as_it_was(heading: &str, body: &str) -> String {
        format!(
            "<!doctype html><meta charset=\"utf-8\"><title>Nexus Music</title>\
             <body style=\"margin:0;height:100vh;display:grid;place-items:center;background:#0d1117;color:#e6edf3;font:16px system-ui,sans-serif\">\
             <div style=\"text-align:center;max-width:28em;padding:24px\"><h1 style=\"font-size:22px\">{heading}</h1><p style=\"color:#9da7b3\">{body}</p></div>"
        )
    }

    #[test]
    fn shows_the_browser_the_pages_and_says_the_words_it_always_has() {
        assert_eq!(nexus_core::gate::page(Page::SignedIn), page_as_it_was("You're signed in", "You can close this tab and go back to Nexus Music."));
        assert_eq!(nexus_core::gate::page(Page::NotThrough), page_as_it_was("The sign-in didn't go through", "Go back to Nexus Music to try again."));
        assert_eq!(nexus_core::gate::page(Page::NothingHere), page_as_it_was("Nothing here", "This page only finishes a sign-in to Nexus Music."));
        assert_eq!(WAIT, Duration::from_secs(5 * 60));
        assert_eq!(nexus_core::gate::NOBODY_SIGNED_IN, "Nobody signed in within five minutes. Try again when you're ready.");
        assert_eq!(nexus_core::gate::turned_down("not allowed"), "The sign-in service turned the sign-in down: not allowed");
        assert_eq!(nexus_core::gate::not_ready(&"in use"), "Couldn't get ready for the browser to come back. in use");
        assert_eq!(nexus_core::gate::not_heard(&"broken"), "Couldn't hear back from the browser. broken");
        assert_eq!(nexus_core::gate::no_random_numbers("PC"), "This PC couldn't supply random numbers for the sign-in.");
        assert_eq!(nexus_core::gate::behind_sign_in("auth.example.org"), "Lidarr is behind the sign-in at auth.example.org.");
        assert_eq!(nexus_core::gate::finish_in_browser("auth.example.org"), "Finish signing in at auth.example.org in your browser.");
        assert_eq!(nexus_core::gate::signed_out("Lidarr is behind the sign-in at x."), "Signed out. Lidarr is behind the sign-in at x.");
        assert_eq!(nexus_core::lidarr::connected("2.12.4"), "Connected to Lidarr 2.12.4.");
        assert_eq!(nexus_core::lidarr::NOT_CONNECTED, "Not connected yet.");
        assert_eq!(nexus_core::lidarr::CHECKING, "Checking…");
        assert_eq!(nexus_core::lidarr::KEY_FORGOTTEN, "That's a different address, so the saved key was forgotten. Enter the key for it.");
    }

    #[test]
    fn makes_secrets_nobody_could_guess() {
        let (one, two) = (unguessable(48).unwrap(), unguessable(48).unwrap());
        assert_eq!(one.len(), 64);
        assert_ne!(one, two);
    }
}

/// What goes to the sign-in service and back, against a stand-in server: written against the
/// app as it was before the sign-in's rules moved to the library, so that the move can be
/// seen to change nothing on the wire.
#[cfg(test)]
mod wire {
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::subsonic::fake::{Sent, json, recorder};
    use nexus_core::gate::CLIENT;

    fn agent() -> ureq::Agent {
        crate::subsonic::make_agent_without_redirects()
    }

    fn session(service: &str, token: &str, kept: Arc<Mutex<Vec<String>>>) -> Session {
        let found = Found { service: service.into(), client: "lidarr-proxy".into() };
        super::session(found, token.into(), move |token| kept.lock().unwrap().push(token.to_string()))
    }

    /// A request's head as the app has always sent it: what every request carries, in the
    /// order it goes, then the caller's own headers.
    fn head(line: &str, length: Option<usize>, own: &str) -> String {
        let length = length.map(|length| format!("content-length: {length}\r\n")).unwrap_or_default();
        format!("{line} HTTP/1.1\r\naccept-encoding: gzip\r\n{length}user-agent: NexusMusic/{}\r\naccept: */*\r\nhost: standin\r\n{own}", env!("CARGO_PKG_VERSION"))
    }

    fn form(path: &str, length: usize) -> String {
        head(&format!("POST /application/o/{path}/"), Some(length), "content-type: application/x-www-form-urlencoded\r\n")
    }

    fn redirect(stream: &mut std::net::TcpStream, to: &[u8]) {
        let _ = stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: ");
        let _ = stream.write_all(to);
        let _ = stream.write_all(b"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    }

    #[test]
    fn finds_the_sign_in_in_front_of_lidarr() {
        let (address, sent) = recorder(|_, request, stream| {
            if request.line().starts_with("GET /api/v1/system/status ") {
                redirect(stream, b"/outpost.goauthentik.io/start?rd=http%3A%2F%2Fx");
            } else {
                redirect(stream, b"https://auth.example.org/application/o/authorize/?client_id=abc%20123&response_type=code");
            }
        });
        assert_eq!(find(&agent(), &address), Ok(Some(Found { service: "https://auth.example.org".into(), client: "abc 123".into() })));
        let heads: Vec<String> = sent.lock().unwrap().iter().map(|sent| sent.head.clone()).collect();
        assert_eq!(
            heads,
            [
                head("GET /api/v1/system/status", None, ""),
                head("GET /outpost.goauthentik.io/start?rd=http%3A%2F%2Fx", None, ""),
            ]
        );
        // Lidarr itself, and something else.
        let (address, _) = recorder(|_, _, stream| json(stream, "200 OK", "{}"));
        assert_eq!(find(&agent(), &address), Ok(None));
        let (address, _) = recorder(|_, _, stream| redirect(stream, b"/login"));
        assert_eq!(find(&agent(), &address), Err("Lidarr is behind a sign-in this app doesn't know how to use.".to_string()));
    }

    #[test]
    fn the_first_place_a_redirect_names_is_the_one_that_counts() {
        // Two Location headers: the first, readable or not, is the one gone by.
        let (address, _) = recorder(|_, _, stream| {
            let _ = stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: \xff\xfe\r\nLocation: https://auth.example.org/application/o/authorize/?client_id=x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        assert_eq!(find(&agent(), &address), Ok(None));
        let (address, _) = recorder(|_, _, stream| {
            let _ = stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: https://auth.example.org/application/o/authorize/?client_id=first\r\nLocation: https://auth.example.org/application/o/authorize/?client_id=second\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        });
        assert_eq!(find(&agent(), &address).map(|found| found.map(|found| found.client)), Ok(Some("first".to_string())));
    }

    #[test]
    fn signs_in_through_the_browser() {
        let (address, sent) = recorder(|_, request, stream| match request.line() {
            line if line.starts_with("POST /application/o/token/ ") && request.body.contains("authorization_code") => {
                json(stream, "200 OK", r#"{"access_token":"first-access","refresh_token":"to keep/1","expires_in":300}"#)
            }
            line if line.starts_with("POST /application/o/token/ ") => json(stream, "200 OK", r#"{"access_token":"a-pass","expires_in":600}"#),
            _ => json(stream, "200 OK", r#"{"preferred_username":"someone","name":"Some One"}"#),
        });
        let found = Found { service: address.clone(), client: "lidarr-proxy".into() };
        let attempt = begin(&found).unwrap();
        let link = attempt.link.clone();
        let back = param(link.split_once('?').unwrap().1, "redirect_uri").unwrap();
        let state = param(link.split_once('?').unwrap().1, "state").unwrap();
        assert!(link.starts_with(&format!("{address}/application/o/authorize/?response_type=code&client_id=nexus-music&redirect_uri=http%3A%2F%2F127.0.0.1%3A")));
        assert!(link.contains("&scope=openid+profile+offline_access&state=") && link.ends_with("&code_challenge_method=S256"));
        // The browser, coming back.
        let browser = std::thread::spawn(move || {
            let port = back.trim_start_matches("http://127.0.0.1:").trim_end_matches("/callback").to_string();
            let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
            write!(stream, "GET /callback?code=c0de%2B1&state={state} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").unwrap();
            let mut said = String::new();
            let _ = stream.read_to_string(&mut said);
            said.lines().next().unwrap_or_default().to_string()
        });
        let done = attempt.finish(&agent(), || false).unwrap();
        assert_eq!(browser.join().unwrap(), "HTTP/1.1 200 OK");
        assert_eq!((done.token.as_str(), done.name.as_str()), ("to keep/1", "someone"));
        let sent: Vec<Sent> = sent.lock().unwrap().clone();
        let back = param(link.split_once('?').unwrap().1, "redirect_uri").unwrap();
        let code = format!(
            "grant_type=authorization_code&code=c0de%2B1&redirect_uri={}&client_id=nexus-music&code_verifier=",
            back.replace(':', "%3A").replace('/', "%2F")
        );
        let trade = "grant_type=client_credentials&client_assertion_type=urn%3Aietf%3Aparams%3Aoauth%3Aclient-assertion-type%3Ajwt-bearer&client_assertion=first-access&client_id=lidarr-proxy&scope=openid+profile+email+ak_proxy";
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0].head, form("token", sent[0].body.len()));
        assert!(sent[0].body.starts_with(&code) && sent[0].body.len() == code.len() + 64, "{}", sent[0].body);
        assert_eq!((sent[1].head.clone(), sent[1].body.as_str()), (form("token", trade.len()), trade));
        assert_eq!(sent[2].head, head("GET /application/o/userinfo/", None, "authorization: Bearer first-access\r\n"));
    }

    #[test]
    fn a_sign_in_that_does_not_get_through_hands_its_token_back() {
        let (address, sent) = recorder(|_, request, stream| match request.line() {
            line if line.starts_with("POST /application/o/token/ ") && request.body.contains("authorization_code") => {
                json(stream, "200 OK", r#"{"access_token":"first-access","refresh_token":"to keep/1"}"#)
            }
            line if line.starts_with("POST /application/o/token/ ") => json(stream, "403 Forbidden", r#"{"error":"invalid_grant"}"#),
            _ => json(stream, "200 OK", "{}"),
        });
        let attempt = begin(&Found { service: address, client: "lidarr-proxy".into() }).unwrap();
        let query = attempt.link.split_once('?').unwrap().1.to_string();
        let (back, state) = (param(&query, "redirect_uri").unwrap(), param(&query, "state").unwrap());
        std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(back.trim_start_matches("http://").trim_end_matches("/callback")).unwrap();
            write!(stream, "GET /callback?code=c&state={state} HTTP/1.1\r\n\r\n").unwrap();
            let _ = stream.read_to_string(&mut String::new());
        });
        assert_eq!(
            attempt.finish(&agent(), || false).err().as_deref(),
            Some("You're signed in, but the sign-in service won't let this app through to Lidarr. Either your account isn't allowed to use Lidarr, or Lidarr's sign-in hasn't been told to trust Nexus Music yet.")
        );
        let sent = sent.lock().unwrap().clone();
        let revoke = "token=to+keep%2F1&client_id=nexus-music";
        assert_eq!(sent.len(), 3);
        assert_eq!((sent[2].head.clone(), sent[2].body.as_str()), (form("revoke", revoke.len()), revoke));
    }

    #[test]
    fn hands_a_token_back() {
        let (address, sent) = recorder(|_, _, stream| json(stream, "200 OK", ""));
        give_up_token(&agent(), &address, "a token+/=é");
        let sent = sent.lock().unwrap().clone();
        let body = "token=a+token%2B%2F%3D%C3%A9&client_id=nexus-music";
        assert_eq!(sent.len(), 1);
        assert_eq!((sent[0].head.clone(), sent[0].body.as_str()), (form("revoke", body.len()), body));
    }

    #[test]
    fn a_form_is_written_as_ureq_writes_one() {
        // Every printable character, a space, a line break and two that aren't ASCII, sent
        // by ureq's own `send_form` and by the library: the same bytes.
        let all: String = (0x20u8..0x7f).map(char::from).collect::<String>() + "\né✓";
        let (address, sent) = recorder(|_, _, stream| json(stream, "200 OK", ""));
        let _ = agent().post(&format!("{address}/application/o/revoke/")).send_form([("token", all.as_str()), ("client_id", CLIENT)]);
        give_up_token(&agent(), &address, &all);
        let sent = sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0], sent[1]);
    }

    #[test]
    fn a_pass_lasts_until_a_minute_before_it_runs_out() {
        let (address, sent) = recorder(|_, request, stream| {
            if request.body.contains("client_credentials") {
                json(stream, "200 OK", r#"{"access_token":"a-pass","expires_in":600}"#);
            } else {
                json(stream, "200 OK", r#"{"access_token":"access","refresh_token":"kept"}"#);
            }
        });
        let kept = Arc::new(Mutex::new(Vec::new()));
        let held = session(&address, "kept", kept.clone());
        assert_eq!(held.pass().as_deref(), Ok("a-pass"));
        assert_eq!(held.pass().as_deref(), Ok("a-pass"));
        // The same token came back: nothing new to put away.
        assert_eq!((sent.lock().unwrap().len(), kept.lock().unwrap().len()), (2, 0));
        held.forget_pass();
        assert_eq!(held.pass().as_deref(), Ok("a-pass"));
        assert_eq!(sent.lock().unwrap().len(), 4);
        // One that's about to run out is renewed.
        let (address, sent) = recorder(|_, request, stream| {
            if request.body.contains("client_credentials") {
                json(stream, "200 OK", r#"{"access_token":"short","expires_in":"59"}"#);
            } else {
                json(stream, "200 OK", r#"{"access_token":"access","refresh_token":"new"}"#);
            }
        });
        let held = session(&address, "old", kept.clone());
        assert_eq!(held.pass().as_deref(), Ok("short"));
        assert_eq!(held.pass().as_deref(), Ok("short"));
        assert_eq!(sent.lock().unwrap().len(), 4);
        assert_eq!(*kept.lock().unwrap(), ["new"]);
        assert_eq!(held.close(), "new");
    }

    #[test]
    fn a_sign_in_the_service_has_ended_is_said_so_and_forgotten() {
        let (address, sent) = recorder(|_, _, stream| json(stream, "400 Bad Request", r#"{"error":"invalid_grant"}"#));
        let kept = Arc::new(Mutex::new(Vec::new()));
        let held = session(&address, "kept", kept.clone());
        assert_eq!(
            held.pass().err().as_deref(),
            Some("Your sign-in to Lidarr has ended. In Settings, under Requests, sign in with your browser again.")
        );
        assert_eq!(*kept.lock().unwrap(), [""]);
        assert_eq!(held.pass().err().as_deref(), Some(NEEDED));
        assert_eq!(sent.lock().unwrap().len(), 1);
        // An error that isn't a refusal is only passed on.
        let (address, _) = recorder(|_, _, stream| json(stream, "400 Bad Request", "{}"));
        let held = session(&address, "kept", kept.clone());
        assert_eq!(held.pass().err().as_deref(), Some("The sign-in service answered with an error (400)."));
        assert_eq!(held.close(), "kept");
    }

    #[test]
    fn an_error_from_the_sign_in_service_is_judged_once_its_body_has_been_waited_for() {
        const STALL: Duration = Duration::from_secs(3);
        let (address, _) = recorder(|_, _, stream| {
            let _ = write!(stream, "HTTP/1.1 502 Bad Gateway\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n{{");
            let _ = stream.flush();
            std::thread::sleep(STALL);
        });
        let started = Instant::now();
        let held = session(&address, "kept", Arc::default());
        assert_eq!(held.pass().err().as_deref(), Some("The sign-in service answered with an error (502)."));
        assert!(started.elapsed() >= STALL - Duration::from_millis(500), "took {:?}", started.elapsed());
        let found = session("http://127.0.0.1:9", "kept", Arc::default()).pass().unwrap_err();
        assert!(found.starts_with("Couldn't reach the sign-in service. "), "{found}");
    }

    /// Signs in against a stand-in service whose UserInfo is answered by `userinfo`; the rest
    /// as Authentik answers. Gives what came of it and how long finishing took.
    fn sign_in_with_userinfo(userinfo: impl Fn(&mut std::net::TcpStream) + Send + Sync + 'static) -> (Result<SignedIn, String>, Duration) {
        let (address, _) = recorder(move |_, request, stream| match request.line() {
            line if line.starts_with("POST /application/o/token/ ") && request.body.contains("authorization_code") => {
                json(stream, "200 OK", r#"{"access_token":"first-access","refresh_token":"to keep"}"#)
            }
            line if line.starts_with("POST /application/o/token/ ") => json(stream, "200 OK", r#"{"access_token":"a-pass","expires_in":600}"#),
            _ => userinfo(stream),
        });
        let attempt = begin(&Found { service: address, client: "lidarr-proxy".into() }).unwrap();
        let query = attempt.link.split_once('?').unwrap().1.to_string();
        let (back, state) = (param(&query, "redirect_uri").unwrap(), param(&query, "state").unwrap());
        std::thread::spawn(move || {
            let mut stream = std::net::TcpStream::connect(back.trim_start_matches("http://").trim_end_matches("/callback")).unwrap();
            write!(stream, "GET /callback?code=c&state={state} HTTP/1.1\r\n\r\n").unwrap();
            let _ = stream.read_to_string(&mut String::new());
        });
        let started = Instant::now();
        let done = attempt.finish(&agent(), || false);
        (done, started.elapsed())
    }

    fn answer(stream: &mut std::net::TcpStream, kind: &str, body: &[u8]) {
        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        let _ = stream.write_all(body);
    }

    #[test]
    fn a_name_page_that_is_not_json_and_never_ends_is_given_up_at_once() {
        // A proxy's page in place of the answer, sent as it's made and never finished: what
        // it begins with is enough to tell it isn't the answer.
        let (done, took) = sign_in_with_userinfo(|stream| {
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nTransfer-Encoding: chunked\r\n\r\n6\r\n<html>\r\n");
            let _ = stream.flush();
            std::thread::sleep(Duration::from_secs(20));
        });
        let done = done.unwrap();
        assert_eq!((done.token.as_str(), done.name.as_str()), ("to keep", ""));
        assert!(took < Duration::from_secs(5), "took {took:?}");
    }

    #[test]
    fn a_name_page_that_says_how_long_it_is_is_read_whole_first() {
        // One that says how long it is is read whole before it's looked at, and so waited for.
        const STALL: Duration = Duration::from_secs(3);
        let (done, took) = sign_in_with_userinfo(|stream| {
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n<html>");
            let _ = stream.flush();
            std::thread::sleep(STALL);
        });
        assert_eq!(done.unwrap().name, "");
        assert!(took >= STALL - Duration::from_millis(500), "took {took:?}");
    }

    #[test]
    fn bytes_that_are_not_text_in_a_claim_nobody_reads_do_not_lose_the_name() {
        for kind in ["application/json", "text/plain"] {
            let (done, _) = sign_in_with_userinfo(move |stream| answer(stream, kind, b"{\"sub\":\"\xff\xfe\",\"preferred_username\":\"someone\",\"name\":\"Some One\"}"));
            assert_eq!(done.unwrap().name, "someone", "as {kind}");
        }
    }

    #[test]
    fn bytes_that_are_not_text_in_the_name_lose_the_name() {
        for kind in ["application/json", "text/plain"] {
            let (done, _) = sign_in_with_userinfo(move |stream| answer(stream, kind, b"{\"preferred_username\":\"some\xffone\",\"name\":\"Some One\"}"));
            assert_eq!(done.unwrap().name, "", "as {kind}");
        }
    }
}
