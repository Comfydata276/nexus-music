//! The music-server login (§9). The server supplies fresh random bytes and performs
//! the fixed POSTs. Music-server answers confer no Nexus account permissions.

use super::proto;
use serde_json::Value;
use std::fmt::{self, Write};
use zeroize::Zeroizing;

mod md5;

pub const SALT_BYTES: usize = 32;
const SALT_DIGITS: usize = 2 * SALT_BYTES;
pub const TOKEN_DIGITS: usize = 32;
/// The design does not say how large an upstream answer may be. Use the bounded
/// hello reader for these two small responses, including their unknown fields.
pub const ANSWER_MAX_BYTES: usize = proto::HELLO_MAX_BYTES;

fn append_hex(bytes: &[u8], text: &mut String) {
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
}

/// §9.4. The design does not say how to encode new salts. Keep the measured
/// 32 random bytes rendered as 64 lowercase hex digits, including leading zeroes.
pub struct Salt(Zeroizing<String>);

impl Salt {
    pub fn from_random(bytes: [u8; SALT_BYTES]) -> Self {
        let mut text = Zeroizing::new(String::with_capacity(SALT_DIGITS));
        append_hex(&bytes, &mut text);
        Self(text)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Salt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Salt(..)")
    }
}

/// §9.1 and §9.4. The only constructor needs a non-empty, typed salt. There is no
/// way to create a bare token. S9 passes both strings to C7's device login.
pub struct SaltedToken {
    salt: Salt,
    token: Zeroizing<String>,
}

impl SaltedToken {
    pub fn from_password(password: &[u8], salt: Salt) -> Self {
        let digest = md5::with_salt(password, &salt);
        let mut token = Zeroizing::new(String::with_capacity(TOKEN_DIGITS));
        append_hex(digest.as_slice(), &mut token);
        Self { salt, token }
    }

    pub fn salt(&self) -> &str {
        self.salt.as_str()
    }

    pub fn token(&self) -> &str {
        &self.token
    }
}

impl fmt::Debug for SaltedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SaltedToken(..)")
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Outcome {
    Verified,
    Refused,
    UntrustedCheck,
    CouldNotAsk,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Trust {
    RequiresCredentials,
    AcceptsHeader,
    Unknown,
}

/// A POST to the configured music server, with application/x-www-form-urlencoded.
/// Use the configured server and disable redirects. Never copy caller headers or
/// query parameters into this request.
pub struct Post {
    path: &'static str,
    form: Zeroizing<String>,
}

impl Post {
    pub fn path(&self) -> &'static str {
        self.path
    }

    pub fn form(&self) -> &str {
        &self.form
    }
}

impl fmt::Debug for Post {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Post")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

// The design does not say which client label or protocol version to put in forms.
// Use E3's measured protocol version and a fixed backend label.
const FORM: &str = "v=1.16.1&c=nexus-music&f=json";

/// §20. At startup and daily, send the configured identity header with this fresh
/// random value, no credentials, and no app-supplied headers. Cache the verdict
/// for the configured server only. Changing its configuration requires a new probe.
pub struct TrustProbe(Salt);

impl TrustProbe {
    pub fn from_random(bytes: [u8; SALT_BYTES]) -> Self {
        Self(Salt::from_random(bytes))
    }

    pub fn post(&self) -> Post {
        Post {
            path: "/rest/ping",
            form: Zeroizing::new(FORM.to_owned()),
        }
    }

    pub fn header_value(&self) -> &str {
        self.0.as_str()
    }

    pub fn answer(self, reply: Option<(u16, &[u8])>) -> Trust {
        match read_reply(reply) {
            Some(Envelope::Error(MISSING_PARAMETER, message)) if message == MISSING_USER => {
                Trust::RequiresCredentials
            }
            Some(Envelope::Error(BAD_SIGN_IN, _) | Envelope::Ok(_)) => Trust::AcceptsHeader,
            _ => Trust::Unknown,
        }
    }
}

impl fmt::Debug for TrustProbe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TrustProbe(..)")
    }
}

/// A login check always starts with ping. §20 keeps this check for a newly entered
/// or changed music login. Callers supply a complete HTTP response or None.
pub struct Ping<'a> {
    name: &'a str,
    pair: &'a SaltedToken,
}

impl<'a> Ping<'a> {
    pub fn new(name: &'a str, pair: &'a SaltedToken, trust: Trust) -> Result<Self, Outcome> {
        match trust {
            Trust::RequiresCredentials => {}
            Trust::AcceptsHeader => return Err(Outcome::UntrustedCheck),
            Trust::Unknown => return Err(Outcome::CouldNotAsk),
        }
        // The design does not say how large / unrestricted a music name may be.
        // Require it to fit a possible getUser answer and reject control characters.
        if name.is_empty() || name.len() > ANSWER_MAX_BYTES || name.chars().any(char::is_control) {
            return Err(Outcome::CouldNotAsk);
        }
        Ok(Self { name, pair })
    }

    pub fn post(&self) -> Post {
        login_post(self.name, self.pair, "/rest/ping", false)
    }

    pub fn answer(self, reply: Option<(u16, &[u8])>) -> Result<GetUser<'a>, Outcome> {
        match read_reply(reply) {
            Some(Envelope::Ok(None)) => Ok(GetUser {
                name: self.name,
                pair: self.pair,
            }),
            Some(Envelope::Error(BAD_SIGN_IN, _)) => Err(Outcome::Refused),
            _ => Err(Outcome::CouldNotAsk),
        }
    }
}

/// Only a well-formed ping success allows S9 to ask getUser with the same login.
pub struct GetUser<'a> {
    name: &'a str,
    pair: &'a SaltedToken,
}

impl GetUser<'_> {
    pub fn post(&self) -> Post {
        login_post(self.name, self.pair, "/rest/getUser", true)
    }

    pub fn answer(self, reply: Option<(u16, &[u8])>) -> Outcome {
        match read_reply(reply) {
            Some(Envelope::Ok(Some(name))) if name == self.name => Outcome::Verified,
            // E3.4. A wrong name/password and the upstream limiter have the same answer.
            Some(Envelope::Error(BAD_SIGN_IN, _)) => Outcome::Refused,
            _ => Outcome::CouldNotAsk,
        }
    }
}

fn login_post(name: &str, pair: &SaltedToken, path: &'static str, get_user: bool) -> Post {
    let name = crate::text::encode(name);
    let mut form = Zeroizing::new(FORM.to_owned());
    if get_user {
        form.push_str("&username=");
        form.push_str(&name);
    }
    form.push_str("&u=");
    form.push_str(&name);
    form.push_str("&s=");
    form.push_str(pair.salt());
    form.push_str("&t=");
    form.push_str(pair.token());
    Post { path, form }
}

fn read_reply(reply: Option<(u16, &[u8])>) -> Option<Envelope> {
    let (status, bytes) = reply?;
    read(status, bytes)
}

// §7.2–7.3 / E3.2 and E3.6. Code 10 alone also means missing v or c.
const MISSING_PARAMETER: u64 = 10;
const MISSING_USER: &str = "missing parameter: 'u'";
const BAD_SIGN_IN: u64 = 40;

enum Envelope {
    Ok(Option<String>),
    Error(u64, String),
}

fn read(status: u16, bytes: &[u8]) -> Option<Envelope> {
    // the design does not say how to treat a valid body at an unmeasured status.
    // A transport failure is never authentication; only a measured 200 is eligible.
    if status != 200 || bytes.len() > ANSWER_MAX_BYTES || !proto::complete_json(bytes) {
        return None;
    }
    let root: Value = serde_json::from_slice(bytes).ok()?;
    let root = root.as_object()?;
    if root.len() != 1 {
        return None;
    }
    let body = root.get("subsonic-response")?.as_object()?;
    // the design does not say which extra envelope payloads are safe. A successful
    // getSong or extension listing is not a successful ping. Accept Navidrome's measured
    // metadata and the two expected payloads; fields inside a real user may vary.
    if !body.keys().all(|k| {
        matches!(
            k.as_str(),
            "status" | "version" | "type" | "serverVersion" | "openSubsonic" | "user" | "error"
        )
    }) {
        return None;
    }
    for key in ["version", "type", "serverVersion"] {
        if body.get(key).is_some_and(|v| !v.is_string()) {
            return None;
        }
    }
    if body.get("openSubsonic").is_some_and(|v| !v.is_boolean()) {
        return None;
    }
    match body.get("status")?.as_str()? {
        "ok" if !body.contains_key("error") => {
            let name = match body.get("user") {
                None => None,
                Some(user) => Some(user.as_object()?.get("username")?.as_str()?.to_owned()),
            };
            if name.as_deref() == Some("") {
                return None;
            }
            Some(Envelope::Ok(name))
        }
        "failed" if !body.contains_key("user") => {
            let error = body.get("error")?.as_object()?;
            Some(Envelope::Error(
                error.get("code")?.as_u64()?,
                error.get("message")?.as_str()?.to_owned(),
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod e3;
#[cfg(test)]
mod tests;
