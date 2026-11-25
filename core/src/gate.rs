//! Signing in to the login that can sit in front of Lidarr.
//!
//! Some people keep Lidarr behind Authentik, a sign-in service: a request only gets through
//! if it comes from someone signed in there. A password typed into this app would have to be
//! kept by it, so the sign-in happens in the web browser instead, with whatever second step
//! the service asks for, and the browser hands this app a token. That token is the only thing
//! kept. It's swapped for a new one each time it's used, and traded when needed for a
//! short-lived pass that the sign-in in front of Lidarr accepts.
//!
//! What is asked of the service and what is made of its answers are here. The browser's part
//! is the app's: somewhere for it to come back to, opening it, and keeping the token.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::http::{Failure, Http, Method, Request, STOPPED};
use crate::lidarr::NEEDED;

/// The name this app goes by at the sign-in service. A client by this name has to be set up
/// there; the README says how.
pub const CLIENT: &str = "nexus-music";

const UNKNOWN: &str = "Lidarr is behind a sign-in this app doesn't know how to use.";

/// Where the sign-in service is, and the name it knows the sign-in in front of Lidarr by.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
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

    /// Whether this sign-in service may be used for a Lidarr at `lidarr` (the address as typed):
    /// it's on HTTPS, or it has exactly the scheme and host typed for Lidarr (a home network on
    /// plain HTTP is the person's own choice; a step down they didn't type is not). It must be
    /// a place and nothing more: a scheme, a host and a port. Checked when it's found, and
    /// before a kept one is used.
    pub fn trusted(&self, lidarr: &str) -> Result<(), String> {
        let refused = || Err(not_trusted(self.host()));
        let Ok(service) = crate::lidarr::origin(&self.service) else { return refused() };
        if origin(&self.service) != self.service {
            return refused();
        }
        if service.scheme == "https" {
            return Ok(());
        }
        match crate::lidarr::origin(lidarr) {
            Ok(lidarr) if lidarr.scheme == service.scheme && lidarr.host == service.host => Ok(()),
            _ => refused(),
        }
    }
}

/// Where at the sign-in service a request goes: `path` under the service, which must be a place
/// and nothing more (see `Found::trusted`), so that every request goes where the service was
/// accepted, and nowhere else.
fn endpoint(service: &str, path: &str) -> Option<String> {
    (crate::lidarr::origin(service).is_ok() && origin(service) == service).then(|| format!("{service}{path}"))
}

/// Said when Lidarr sends the app to a sign-in it won't go to: one on plain HTTP away from the
/// address typed for Lidarr, where the sign-in would cross the network unprotected.
pub fn not_trusted(service: &str) -> String {
    format!("The sign-in at {service} wasn't trusted: it isn't on HTTPS, and isn't where Lidarr's own address is, so signing in there would send your sign-in across the network unprotected.")
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
pub fn param(query: &str, name: &str) -> Option<String> {
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
pub fn base64url(bytes: &[u8]) -> String {
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

/// The check sent ahead of a secret, so only whoever holds the secret can finish the sign-in.
pub fn challenge(secret: &str) -> String {
    base64url(&Sha256::digest(secret.as_bytes()))
}

/// The page the browser is sent to, to sign in. `back` is where the service sends it back to
/// with the code, `state` what it brings back with it, and `secret` what only this app knows.
pub fn sign_in_link(found: &Found, back: &str, state: &str, secret: &str) -> String {
    format!(
        "{}/application/o/authorize/?response_type=code&client_id={CLIENT}&redirect_uri={}&scope=openid+profile+offline_access&state={state}&code_challenge={}&code_challenge_method=S256",
        found.service,
        crate::text::encode(back),
        challenge(secret),
    )
}

/// A form's fields as the app has always sent them (as ureq's `send_form` writes them): each
/// name and value percent-encoded, with a space as "+", joined by "&".
fn form(fields: &[(&str, &str)]) -> String {
    let encode = |text: &str| {
        let mut out = String::with_capacity(text.len());
        for byte in text.bytes() {
            match byte {
                b' ' => out.push('+'),
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'!' | b'(' | b')' | b'*' | b'-' | b'.' | b'_' | b'~' => out.push(byte as char),
                _ => out.push_str(&format!("%{byte:02X}")),
            }
        }
        out
    };
    fields.iter().map(|(name, value)| format!("{}={}", encode(name), encode(value))).collect::<Vec<_>>().join("&")
}

/// Posts a form. An answer is an answer whatever its status.
fn post_form(http: &dyn Http, url: &str, fields: &[(&str, &str)]) -> Result<crate::http::Response, Failure> {
    let body = form(fields);
    http.send(&Request { method: Method::Post(&body), url, headers: &[("Content-Type", "application/x-www-form-urlencoded")], patience: None })
}

// ---- finding the sign-in --------------------------------------------------------------------------

/// Where `url` redirects to, if it does. `http` must be one that doesn't follow redirects.
fn redirect_from(http: &dyn Http, url: &str) -> Result<Option<String>, String> {
    let response = http.send(&Request::get(url)).map_err(|failure| match failure {
        Failure::Reach(e) => format!("Couldn't reach Lidarr. {e}"),
        Failure::Stopped => STOPPED.to_string(),
    })?;
    if !(300..400).contains(&response.status) {
        return Ok(None);
    }
    Ok(response.header("location").map(|to| absolute(url, to)))
}

/// Looks at what answers at Lidarr's address. `None` if it's Lidarr itself; otherwise the
/// sign-in in front of it.
pub fn find(http: &dyn Http, lidarr: &str) -> Result<Option<Found>, String> {
    let Some(first) = redirect_from(http, &format!("{lidarr}/api/v1/system/status"))? else {
        return Ok(None);
    };
    // The door in front of Lidarr sends the caller to itself first, then on to the service.
    let sign_in = match parse_sign_in(&first) {
        Some(found) => Some(found),
        None => redirect_from(http, &first)?.and_then(|second| parse_sign_in(&second)),
    };
    let found = sign_in.ok_or_else(|| UNKNOWN.to_string())?;
    // Believed only on HTTPS, or where Lidarr itself was typed to be.
    found.trusted(lidarr)?;
    Ok(Some(found))
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

fn ask_for_tokens(http: &dyn Http, service: &str, fields: &[(&str, &str)]) -> Result<Tokens, Trouble> {
    let Some(url) = endpoint(service, "/application/o/token/") else { return Err(Trouble::Unreachable(not_trusted(service))) };
    let response = post_form(http, &url, fields).map_err(|failure| match failure {
        Failure::Reach(e) => Trouble::Unreachable(format!("Couldn't reach the sign-in service. {e}")),
        Failure::Stopped => Trouble::Unreachable(STOPPED.to_string()),
    })?;
    let status = response.status;
    let text = response.body().unwrap_or_default();
    let tokens: Tokens = serde_json::from_str(&text).unwrap_or_default();
    match status {
        200 if !tokens.access_token.is_empty() => Ok(tokens),
        400 | 401 | 403 if !tokens.error.is_empty() => Err(Trouble::Refused),
        _ => Err(Trouble::Unreachable(format!("The sign-in service answered with an error ({status})."))),
    }
}

/// Trades this app's own token for a pass the sign-in in front of Lidarr accepts.
fn trade(http: &dyn Http, found: &Found, access: &str) -> Result<Tokens, String> {
    let fields = [
        ("grant_type", "client_credentials"),
        ("client_assertion_type", "urn:ietf:params:oauth:client-assertion-type:jwt-bearer"),
        ("client_assertion", access),
        ("client_id", found.client.as_str()),
        ("scope", "openid profile email ak_proxy"),
    ];
    ask_for_tokens(http, &found.service, &fields).map_err(|trouble| match trouble {
        Trouble::Refused => {
            "You're signed in, but the sign-in service won't let this app through to Lidarr. Either your account isn't allowed to use Lidarr, or Lidarr's sign-in hasn't been told to trust Nexus Music yet."
                .to_string()
        }
        Trouble::Unreachable(why) => why,
    })
}

/// The name of whoever a token belongs to, for showing; empty if it can't be had.
fn name_of(http: &dyn Http, service: &str, access: &str) -> String {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct Person {
        preferred_username: String,
        name: String,
    }
    let Some(url) = endpoint(service, "/application/o/userinfo/") else { return String::new() };
    let bearer = format!("Bearer {access}");
    let Ok(response) = http.send(&Request { headers: &[("Authorization", &bearer)], ..Request::get(&url) }) else {
        return String::new();
    };
    let person: Person = response.json().unwrap_or_default();
    if person.preferred_username.is_empty() { person.name } else { person.preferred_username }
}

/// Tells the service a token is no longer wanted. Best effort.
pub fn give_up_token(http: &dyn Http, service: &str, token: &str) {
    let Some(url) = endpoint(service, "/application/o/revoke/") else { return };
    let _ = post_form(http, &url, &[("token", token), ("client_id", CLIENT)]);
}

// ---- finishing a sign-in --------------------------------------------------------------------------

/// What a finished sign-in leaves behind.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SignedIn {
    /// The token to keep.
    pub token: String,
    /// Who signed in, for showing; may be empty.
    pub name: String,
}

/// Collects the token once the browser has come back with `code`. `back` and `secret` are
/// what the sign-in was started with (see `sign_in_link`).
///
/// This doesn't check where the code came from: the caller must, before calling it. The
/// browser comes back with the `state` the sign-in was started with, and a code that comes
/// with any other state (or none) is someone else's and must not be traded, as the Windows
/// app's `gate::Attempt` refuses it. The caller must also have kept `secret` to itself.
pub fn finish(http: &dyn Http, found: &Found, code: &str, back: &str, secret: &str) -> Result<SignedIn, String> {
    let fields = [("grant_type", "authorization_code"), ("code", code), ("redirect_uri", back), ("client_id", CLIENT), ("code_verifier", secret)];
    let tokens = ask_for_tokens(http, &found.service, &fields).map_err(|trouble| match trouble {
        Trouble::Refused => "The sign-in service wouldn't finish the sign-in. Try again.".to_string(),
        Trouble::Unreachable(why) => why,
    })?;
    if tokens.refresh_token.is_empty() {
        return Err("The sign-in worked but gave nothing lasting to keep. Its Nexus Music client needs the offline_access scope.".into());
    }
    // Find out now, not at the first request, whether this gets through to Lidarr. If it
    // doesn't, the token is no use: hand it straight back.
    if let Err(why) = trade(http, found, &tokens.access_token) {
        give_up_token(http, &found.service, &tokens.refresh_token);
        return Err(why);
    }
    Ok(SignedIn { name: name_of(http, &found.service, &tokens.access_token), token: tokens.refresh_token })
}

// ---- the browser's part ---------------------------------------------------------------------------

/// How long to wait for someone to finish signing in in the browser.
pub const WAIT: Duration = Duration::from_secs(5 * 60);

/// Said when nobody did.
pub const NOBODY_SIGNED_IN: &str = "Nobody signed in within five minutes. Try again when you're ready.";

/// The pages the browser is shown when it comes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Page {
    /// "You're signed in": it came back with a code.
    SignedIn,
    /// "The sign-in didn't go through": it came back with an error.
    NotThrough,
    /// "Nothing here": something else came to the door.
    NothingHere,
}

/// A page to show the browser, as HTML.
pub fn page(which: Page) -> String {
    let (heading, body) = match which {
        Page::SignedIn => ("You're signed in", "You can close this tab and go back to Nexus Music."),
        Page::NotThrough => ("The sign-in didn't go through", "Go back to Nexus Music to try again."),
        Page::NothingHere => ("Nothing here", "This page only finishes a sign-in to Nexus Music."),
    };
    format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Nexus Music</title>\
         <body style=\"margin:0;height:100vh;display:grid;place-items:center;background:#0d1117;color:#e6edf3;font:16px system-ui,sans-serif\">\
         <div style=\"text-align:center;max-width:28em;padding:24px\"><h1 style=\"font-size:22px\">{heading}</h1><p style=\"color:#9da7b3\">{body}</p></div>"
    )
}

/// Said when the service turned the sign-in down, with its reason.
pub fn turned_down(why: &str) -> String {
    format!("The sign-in service turned the sign-in down: {why}")
}

/// Said when there was nowhere for the browser to come back to.
pub fn not_ready(why: &dyn std::fmt::Display) -> String {
    format!("Couldn't get ready for the browser to come back. {why}")
}

/// Said when listening for the browser failed.
pub fn not_heard(why: &dyn std::fmt::Display) -> String {
    format!("Couldn't hear back from the browser. {why}")
}

/// Said when the device had no random numbers to give; `device` is what it's called ("PC").
pub fn no_random_numbers(device: &str) -> String {
    format!("This {device} couldn't supply random numbers for the sign-in.")
}

/// "Lidarr is behind the sign-in at auth.example.org."
pub fn behind_sign_in(host: &str) -> String {
    format!("Lidarr is behind the sign-in at {host}.")
}

/// Said while the browser is open at the sign-in.
pub fn finish_in_browser(host: &str) -> String {
    format!("Finish signing in at {host} in your browser.")
}

/// Said once signed out: "Signed out." and where the sign-in is (`behind_sign_in`).
pub fn signed_out(behind: &str) -> String {
    format!("Signed out. {behind}")
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
    http: Arc<dyn Http>,
    held: Mutex<Held>,
    /// Called with the token whenever it changes, to put it away; with "" when it's dead.
    keep: Box<dyn Fn(&str) + Send + Sync>,
}

impl Session {
    /// `http` is what the service is asked over. It must be one that nothing stops: no screen
    /// left, no search typed afresh, no operation given up. Renewing the pass replaces the
    /// token kept for this sign-in, at the service, the moment the service answers; a renewal
    /// cut off after that, before the new token is kept, leaves only the old one, which the
    /// service no longer honours, and the person is signed out. (A request to Lidarr that is
    /// stopped meanwhile is the Lidarr's `Http`'s affair, not this one's.)
    pub fn new(found: Found, token: String, keep: impl Fn(&str) + Send + Sync + 'static, http: Arc<dyn Http>) -> Session {
        Session {
            found,
            http,
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
        let fresh = match ask_for_tokens(self.http.as_ref(), &self.found.service, &fields) {
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
        let pass = trade(self.http.as_ref(), &self.found, &fresh.access_token)?;
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

impl crate::lidarr::SignIn for Session {
    fn pass(&self) -> Result<String, String> {
        Session::pass(self)
    }

    fn forget_pass(&self) {
        Session::forget_pass(self)
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
    }

    #[test]
    fn writes_a_form_as_it_always_has() {
        // Every printable character, a space, a line break and two that aren't ASCII.
        let all: String = (0x20u8..0x7f).map(char::from).collect::<String>() + "\né✓";
        assert_eq!(
            form(&[("a key", &all), ("", "")]),
            "a+key=+!%22%23%24%25%26%27()*%2B%2C-.%2F0123456789%3A%3B%3C%3D%3E%3F%40ABCDEFGHIJKLMNOPQRSTUVWXYZ%5B%5C%5D%5E_%60abcdefghijklmnopqrstuvwxyz%7B%7C%7D~%0A%C3%A9%E2%9C%93&="
        );
    }

    #[test]
    fn links_to_the_sign_in() {
        let found = Found { service: "https://auth.example.org".into(), client: "proxy".into() };
        assert_eq!(
            sign_in_link(&found, "http://127.0.0.1:5000/callback", "st4te", "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "https://auth.example.org/application/o/authorize/?response_type=code&client_id=nexus-music&redirect_uri=http%3A%2F%2F127.0.0.1%3A5000%2Fcallback&scope=openid+profile+offline_access&state=st4te&code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM&code_challenge_method=S256"
        );
    }

    /// A Lidarr whose door sends every caller to a sign-in at `service`.
    struct Door {
        service: &'static str,
    }

    impl Http for Door {
        fn send(&self, request: &Request) -> Result<crate::http::Response, Failure> {
            let to = format!("{}/application/o/authorize/?client_id=proxy&response_type=code", self.service);
            assert!(request.url.contains("/api/v1/system/status"), "{}", request.url);
            Ok(crate::http::Response::new(302, vec![("Location".into(), Some(to))], || Ok(String::new())))
        }
    }

    #[test]
    fn a_sign_in_is_believed_only_on_https_or_where_lidarr_was_typed() {
        let found = |lidarr: &str, service: &'static str| find(&Door { service }, lidarr).map(|found| found.map(|found| found.service));
        // On HTTPS, wherever it is.
        assert_eq!(found("https://lidarr.example.org", "https://auth.example.org"), Ok(Some("https://auth.example.org".into())));
        assert_eq!(found("http://192.168.1.5:8686", "https://auth.example.org"), Ok(Some("https://auth.example.org".into())));
        // Plain HTTP only at the scheme and host typed for Lidarr (any port): a home network.
        assert_eq!(found("http://192.168.1.5:8686", "http://192.168.1.5:9000"), Ok(Some("http://192.168.1.5:9000".into())));
        // Not a step down from what was typed, and not elsewhere.
        assert_eq!(found("https://lidarr.example.org", "http://auth.example.org"), Err(not_trusted("auth.example.org")));
        assert_eq!(found("https://lidarr.example.org", "http://lidarr.example.org"), Err(not_trusted("lidarr.example.org")));
        assert_eq!(found("http://192.168.1.5:8686", "http://192.168.1.9:9000"), Err(not_trusted("192.168.1.9:9000")));
    }
}
