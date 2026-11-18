//! Signing in to the login that can sit in front of Lidarr.
//!
//! Some people keep Lidarr behind Authentik, a sign-in service: a request only gets through
//! if it comes from someone signed in there. A password typed into this app would have to be
//! kept by it, so the sign-in happens in the web browser instead, with whatever second step
//! the service asks for, and the browser hands this app a token. That token is the only thing
//! kept. It's swapped for a new one each time it's used, and traded when needed for a
//! short-lived pass that the sign-in in front of Lidarr accepts.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;
use sha2::{Digest, Sha256};

/// The name this app goes by at the sign-in service. A client by this name has to be set up
/// there; the README says how.
pub const CLIENT: &str = "nexus-music";

/// Said when Lidarr can't be reached without signing in.
pub const NEEDED: &str = "Lidarr is behind a sign-in. In Settings, under Requests, sign in with your browser.";
const UNKNOWN: &str = "Lidarr is behind a sign-in this app doesn't know how to use.";

/// How long to wait for someone to finish signing in in the browser.
const WAIT: Duration = Duration::from_secs(5 * 60);

/// Where the sign-in service is, and the name it knows the sign-in in front of Lidarr by.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Found {
    /// E.g. "https://auth.example.org".
    pub service: String,
    pub client: String,
}

impl Found {
    /// The service's name without the "https://", for showing.
    pub fn host(&self) -> &str {
        self.service.split_once("://").map_or(self.service.as_str(), |(_, host)| host)
    }
}

// ---- small helpers for web addresses ------------------------------------------------------------

/// The "https://host" part of an address.
fn origin(url: &str) -> &str {
    let Some(scheme) = url.find("://") else { return url };
    match url[scheme + 3..].find(['/', '?', '#']) {
        Some(end) => &url[..scheme + 3 + end],
        None => url,
    }
}

/// Where a redirect leads, given the address that sent it.
fn absolute(from: &str, location: &str) -> String {
    let whole = location.split_once("://").is_some_and(|(scheme, _)| !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric()));
    if whole {
        location.to_string()
    } else {
        format!("{}/{}", origin(from), location.trim_start_matches('/'))
    }
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => out.push(b' '),
            b'%' if at + 2 < bytes.len() => {
                let hex = |byte: u8| (byte as char).to_digit(16);
                match (hex(bytes[at + 1]), hex(bytes[at + 2])) {
                    (Some(high), Some(low)) => {
                        out.push((high * 16 + low) as u8);
                        at += 2;
                    }
                    _ => out.push(b'%'),
                }
            }
            other => out.push(other),
        }
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One named value from the part of an address after the "?".
fn param(query: &str, name: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        (key == name).then(|| decode(value))
    })
}

/// Reads the sign-in service and client out of the address a login redirect points at.
fn parse_sign_in(url: &str) -> Option<Found> {
    let (path, query) = url.split_once('?')?;
    if !path.ends_with("/application/o/authorize/") {
        return None;
    }
    let client = param(query, "client_id").filter(|client| !client.is_empty())?;
    Some(Found { service: origin(url).to_string(), client })
}

/// Base64 in the form used in web addresses, without padding.
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16 | (*chunk.get(1).unwrap_or(&0) as u32) << 8 | *chunk.get(2).unwrap_or(&0) as u32;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        }
    }
    out
}

/// Letters nobody could guess, from the system's own source of randomness.
fn unguessable(bytes: usize) -> Result<String, String> {
    let mut buffer = vec![0u8; bytes];
    getrandom::fill(&mut buffer).map_err(|_| "This PC couldn't supply random numbers for the sign-in.".to_string())?;
    Ok(base64url(&buffer))
}

/// The check sent ahead of a secret, so only whoever holds the secret can finish the sign-in.
fn challenge(secret: &str) -> String {
    base64url(&Sha256::digest(secret.as_bytes()))
}

// ---- finding the sign-in --------------------------------------------------------------------------

/// Where `url` redirects to, if it does. `agent` must be one that doesn't follow redirects.
fn redirect_from(agent: &ureq::Agent, url: &str) -> Result<Option<String>, String> {
    let response = agent.get(url).call().map_err(|e| format!("Couldn't reach Lidarr. {e}"))?;
    if !response.status().is_redirection() {
        return Ok(None);
    }
    Ok(response.headers().get("location").and_then(|to| to.to_str().ok()).map(|to| absolute(url, to)))
}

/// Looks at what answers at Lidarr's address. `None` if it's Lidarr itself; otherwise the
/// sign-in in front of it.
pub fn find(agent: &ureq::Agent, lidarr: &str) -> Result<Option<Found>, String> {
    let Some(first) = redirect_from(agent, &format!("{lidarr}/api/v1/system/status"))? else {
        return Ok(None);
    };
    // The door in front of Lidarr sends the caller to itself first, then on to the service.
    let sign_in = match parse_sign_in(&first) {
        Some(found) => Some(found),
        None => redirect_from(agent, &first)?.and_then(|second| parse_sign_in(&second)),
    };
    sign_in.map(Some).ok_or_else(|| UNKNOWN.to_string())
}

// ---- talking to the sign-in service ---------------------------------------------------------------

#[derive(Deserialize, Default)]
#[serde(default)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    expires_in: serde_json::Value,
    error: String,
}

enum Trouble {
    /// The service understood and said no.
    Refused,
    Unreachable(String),
}

fn ask_for_tokens(agent: &ureq::Agent, service: &str, fields: &[(&str, &str)]) -> Result<Tokens, Trouble> {
    let response = agent
        .post(&format!("{service}/application/o/token/"))
        .send_form(fields.iter().copied())
        .map_err(|e| Trouble::Unreachable(format!("Couldn't reach the sign-in service. {e}")))?;
    let status = response.status().as_u16();
    let text = response.into_body().read_to_string().unwrap_or_default();
    let tokens: Tokens = serde_json::from_str(&text).unwrap_or_default();
    match status {
        200 if !tokens.access_token.is_empty() => Ok(tokens),
        400 | 401 | 403 if !tokens.error.is_empty() => Err(Trouble::Refused),
        _ => Err(Trouble::Unreachable(format!("The sign-in service answered with an error ({status})."))),
    }
}

/// Trades this app's own token for a pass the sign-in in front of Lidarr accepts.
fn trade(agent: &ureq::Agent, found: &Found, access: &str) -> Result<Tokens, String> {
    let fields = [
        ("grant_type", "client_credentials"),
        ("client_assertion_type", "urn:ietf:params:oauth:client-assertion-type:jwt-bearer"),
        ("client_assertion", access),
        ("client_id", found.client.as_str()),
        ("scope", "openid profile email ak_proxy"),
    ];
    ask_for_tokens(agent, &found.service, &fields).map_err(|trouble| match trouble {
        Trouble::Refused => {
            "You're signed in, but the sign-in service won't let this app through to Lidarr. Either your account isn't allowed to use Lidarr, or Lidarr's sign-in hasn't been told to trust Nexus Music yet."
                .to_string()
        }
        Trouble::Unreachable(why) => why,
    })
}

/// The name of whoever a token belongs to, for showing; empty if it can't be had.
fn name_of(agent: &ureq::Agent, service: &str, access: &str) -> String {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Person {
        preferred_username: String,
        name: String,
    }
    let Ok(response) = agent.get(&format!("{service}/application/o/userinfo/")).header("Authorization", &format!("Bearer {access}")).call()
    else {
        return String::new();
    };
    let person: Person = response.into_body().read_json().unwrap_or_default();
    if person.preferred_username.is_empty() { person.name } else { person.preferred_username }
}

/// Tells the service a token is no longer wanted. Best effort.
pub fn give_up_token(agent: &ureq::Agent, service: &str, token: &str) {
    let _ = agent.post(&format!("{service}/application/o/revoke/")).send_form([("token", token), ("client_id", CLIENT)]);
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

/// What a finished sign-in leaves behind.
pub struct SignedIn {
    /// The token to keep.
    pub token: String,
    /// Who signed in, for showing; may be empty.
    pub name: String,
}

/// Gets ready for a sign-in: opens a door on this PC for the browser to come back through.
pub fn begin(found: &Found) -> Result<Attempt, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("Couldn't get ready for the browser to come back. {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let back = format!("http://127.0.0.1:{port}/callback");
    let secret = unguessable(48)?;
    let state = unguessable(18)?;
    let link = format!(
        "{}/application/o/authorize/?response_type=code&client_id={CLIENT}&redirect_uri={}&scope=openid+profile+offline_access&state={state}&code_challenge={}&code_challenge_method=S256",
        found.service,
        crate::subsonic::encode(&back),
        challenge(&secret),
    );
    Ok(Attempt { listener, found: found.clone(), secret, state, back, link })
}

fn page(heading: &str, body: &str) -> String {
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Nexus Music</title>\
         <body style=\"margin:0;height:100vh;display:grid;place-items:center;background:#0d1117;color:#e6edf3;font:16px system-ui,sans-serif\">\
         <div style=\"text-align:center;max-width:28em;padding:24px\"><h1 style=\"font-size:22px\">{heading}</h1><p style=\"color:#9da7b3\">{body}</p></div>"
    )
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
        let fields = [
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", self.back.as_str()),
            ("client_id", CLIENT),
            ("code_verifier", self.secret.as_str()),
        ];
        let tokens = ask_for_tokens(agent, &self.found.service, &fields).map_err(|trouble| match trouble {
            Trouble::Refused => "The sign-in service wouldn't finish the sign-in. Try again.".to_string(),
            Trouble::Unreachable(why) => why,
        })?;
        if tokens.refresh_token.is_empty() {
            return Err("The sign-in worked but gave nothing lasting to keep. Its Nexus Music client needs the offline_access scope.".into());
        }
        // Find out now, not at the first request, whether this gets through to Lidarr. If it
        // doesn't, the token is no use: hand it straight back.
        if let Err(why) = trade(agent, &self.found, &tokens.access_token) {
            give_up_token(agent, &self.found.service, &tokens.refresh_token);
            return Err(why);
        }
        Ok(SignedIn { name: name_of(agent, &self.found.service, &tokens.access_token), token: tokens.refresh_token })
    }

    fn wait(&self, given_up: &impl Fn() -> bool) -> Result<String, String> {
        self.listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + WAIT;
        loop {
            if given_up() {
                return Err(String::new());
            }
            if Instant::now() > deadline {
                return Err("Nobody signed in within five minutes. Try again when you're ready.".into());
            }
            let mut stream = match self.listener.accept() {
                Ok((stream, _)) => stream,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(e) => return Err(format!("Couldn't hear back from the browser. {e}")),
            };
            let line = first_line(&mut stream);
            let target = line.split_whitespace().nth(1).unwrap_or_default();
            let (path, query) = target.split_once('?').unwrap_or((target, ""));
            if path != "/callback" || param(query, "state").as_deref() != Some(self.state.as_str()) {
                reply(&mut stream, "404 Not Found", &page("Nothing here", "This page only finishes a sign-in to Nexus Music."));
                continue;
            }
            if let Some(error) = param(query, "error") {
                let why = param(query, "error_description").filter(|why| !why.is_empty()).unwrap_or(error);
                reply(&mut stream, "200 OK", &page("The sign-in didn't go through", "Go back to Nexus Music to try again."));
                return Err(format!("The sign-in service turned the sign-in down: {why}"));
            }
            if let Some(code) = param(query, "code").filter(|code| !code.is_empty()) {
                reply(&mut stream, "200 OK", &page("You're signed in", "You can close this tab and go back to Nexus Music."));
                return Ok(code);
            }
            reply(&mut stream, "400 Bad Request", &page("Nothing here", "This page only finishes a sign-in to Nexus Music."));
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

// ---- staying signed in ----------------------------------------------------------------------------

struct Held {
    /// The token that's kept; empty once the service has stopped honouring it.
    token: String,
    /// The pass for Lidarr's sign-in, and when it runs out.
    pass: String,
    until: Option<Instant>,
}

/// A sign-in that's been done: hands out passes for Lidarr and keeps the token fresh.
pub struct Session {
    found: Found,
    agent: ureq::Agent,
    held: Mutex<Held>,
    /// Called with the token whenever it changes, to put it away; with "" when it's dead.
    keep: Box<dyn Fn(&str) + Send + Sync>,
}

impl Session {
    pub fn new(found: Found, token: String, keep: impl Fn(&str) + Send + Sync + 'static) -> Session {
        Session {
            found,
            agent: crate::subsonic::make_agent_without_redirects(),
            held: Mutex::new(Held { token, pass: String::new(), until: None }),
            keep: Box::new(keep),
        }
    }

    pub fn found(&self) -> &Found {
        &self.found
    }

    /// A pass to send along with a request to Lidarr, fetched anew if the last has run out.
    /// Only one caller renews at a time: each renewal replaces the token.
    pub fn pass(&self) -> Result<String, String> {
        let mut held = self.held.lock().map_err(|_| NEEDED.to_string())?;
        if held.until.is_some_and(|until| Instant::now() + Duration::from_secs(60) < until) {
            return Ok(held.pass.clone());
        }
        if held.token.is_empty() {
            return Err(NEEDED.into());
        }
        let fields = [("grant_type", "refresh_token"), ("refresh_token", held.token.as_str()), ("client_id", CLIENT)];
        let fresh = match ask_for_tokens(&self.agent, &self.found.service, &fields) {
            Ok(fresh) => fresh,
            Err(Trouble::Refused) => {
                // Signed out at the service, or left unused too long.
                held.token.clear();
                held.until = None;
                (self.keep)("");
                return Err("Your sign-in to Lidarr has ended. In Settings, under Requests, sign in with your browser again.".into());
            }
            Err(Trouble::Unreachable(why)) => return Err(why),
        };
        if !fresh.refresh_token.is_empty() && fresh.refresh_token != held.token {
            held.token = fresh.refresh_token;
            (self.keep)(&held.token);
        }
        let pass = trade(&self.agent, &self.found, &fresh.access_token)?;
        let lasts = pass.expires_in.as_u64().or_else(|| pass.expires_in.as_str().and_then(|text| text.parse().ok())).unwrap_or(300);
        held.pass = pass.access_token;
        held.until = Some(Instant::now() + Duration::from_secs(lasts));
        Ok(held.pass.clone())
    }

    /// Ends the sign-in here and returns the token, for handing back to the service.
    pub fn close(&self) -> String {
        match self.held.lock() {
            Ok(mut held) => {
                held.until = None;
                std::mem::take(&mut held.token)
            }
            Err(_) => String::new(),
        }
    }

    /// The pass was turned away: don't hand it out again.
    pub fn forget_pass(&self) {
        if let Ok(mut held) = self.held.lock() {
            held.until = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_sign_in_from_a_redirect() {
        let found = parse_sign_in(
            "https://auth.example.org/application/o/authorize/?client_id=abc%20123&redirect_uri=https%3A%2F%2Flidarr.example.org%2Foutpost.goauthentik.io%2Fcallback&response_type=code",
        )
        .unwrap();
        assert_eq!(found, Found { service: "https://auth.example.org".into(), client: "abc 123".into() });
        assert_eq!(found.host(), "auth.example.org");
        assert_eq!(parse_sign_in("https://lidarr.example.org/login?returnUrl=/"), None);
        assert_eq!(parse_sign_in("https://auth.example.org/application/o/authorize/?redirect_uri=x"), None);
    }

    #[test]
    fn follows_redirects_within_a_site() {
        assert_eq!(origin("https://lidarr.example.org:8443/api/v1/x?y=1"), "https://lidarr.example.org:8443");
        assert_eq!(origin("http://127.0.0.1:48080"), "http://127.0.0.1:48080");
        assert_eq!(
            absolute("https://lidarr.example.org/api/v1/system/status", "/outpost.goauthentik.io/start?rd=x"),
            "https://lidarr.example.org/outpost.goauthentik.io/start?rd=x"
        );
        assert_eq!(absolute("https://lidarr.example.org/a", "https://auth.example.org/b"), "https://auth.example.org/b");
        // An address inside the redirect doesn't make it a whole one.
        assert_eq!(
            absolute("http://127.0.0.1:48080/api/v1/system/status", "/outpost.goauthentik.io/start?rd=http://127.0.0.1:48080/api"),
            "http://127.0.0.1:48080/outpost.goauthentik.io/start?rd=http://127.0.0.1:48080/api"
        );
    }

    #[test]
    fn reads_what_the_browser_brings_back() {
        let query = "code=ab%2Fcd&state=x-y_z&error_description=not+allowed%21&odd=%zz&end=%";
        assert_eq!(param(query, "code").as_deref(), Some("ab/cd"));
        assert_eq!(param(query, "state").as_deref(), Some("x-y_z"));
        assert_eq!(param(query, "error_description").as_deref(), Some("not allowed!"));
        assert_eq!(param(query, "odd").as_deref(), Some("%zz"));
        assert_eq!(param(query, "end").as_deref(), Some("%"));
        assert_eq!(param(query, "missing"), None);
    }

    #[test]
    fn makes_the_check_the_standard_way() {
        // The worked example from the standard (RFC 7636, appendix B).
        assert_eq!(challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"), "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        let (one, two) = (unguessable(48).unwrap(), unguessable(48).unwrap());
        assert_eq!(one.len(), 64);
        assert_ne!(one, two);
    }
}
