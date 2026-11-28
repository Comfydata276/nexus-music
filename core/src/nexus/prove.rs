//! The music-server login. Its answers are independent of Nexus account sign-in.

use super::proto;
use serde_json::Value;
use std::fmt::{self, Write};
use zeroize::Zeroizing;

pub const SALT_BYTES: usize = 32;
pub const TOKEN_DIGITS: usize = 32;
/// The design does not say how large an upstream answer may be. Use the bounded
/// hello reader for these two small responses, including their unknown fields.
pub const ANSWER_MAX_BYTES: usize = proto::HELLO_MAX_BYTES;

pub struct Token(Zeroizing<String>);

impl Token {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn read(text: &str) -> Option<Self> {
        (text.len() == TOKEN_DIGITS
            && text
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
        .then(|| Self(Zeroizing::new(text.to_owned())))
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// Random salt rendered once, always 64 lower-case digits. The transport needs the text
/// both in the device login and in the upstream POST bodies.
pub struct Salt(String);

impl Salt {
    pub fn from_random(bytes: [u8; SALT_BYTES]) -> Self {
        let mut text = String::with_capacity(SALT_BYTES * 2);
        for byte in bytes {
            let _ = write!(text, "{byte:02x}");
        }
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

/// A music-server verdict is never a Nexus account receipt.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Outcome {
    Verified,
    Refused,
    CouldNotAsk,
}

pub fn read_login(ping: Option<(u16, &[u8])>, user: Option<(u16, &[u8])>, name: &str) -> Outcome {
    let answer =
        |reply: Option<(u16, &[u8])>| reply.and_then(|(status, bytes)| read(status, bytes));
    match answer(ping) {
        Some(Envelope::Error(BAD_SIGN_IN, _)) => Outcome::Refused,
        Some(Envelope::Ok(None)) => match answer(user) {
            Some(Envelope::Error(BAD_SIGN_IN, _)) => Outcome::Refused,
            Some(Envelope::Ok(Some(found))) if found == name => Outcome::Verified,
            _ => Outcome::CouldNotAsk,
        },
        _ => Outcome::CouldNotAsk,
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Trust {
    RequiresCredentials,
    AcceptsHeader,
    Unknown,
}

pub fn read_trust(reply: Option<(u16, &[u8])>) -> Trust {
    match reply.and_then(|(status, bytes)| read(status, bytes)) {
        Some(Envelope::Error(MISSING_PARAMETER, message)) if message == MISSING_USER => {
            Trust::RequiresCredentials
        }
        Some(Envelope::Error(BAD_SIGN_IN, _) | Envelope::Ok(_)) => Trust::AcceptsHeader,
        _ => Trust::Unknown,
    }
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
