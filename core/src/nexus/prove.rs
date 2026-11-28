//! A single attempt to prove the music sign-in (§7.2). S2 owns the random source, the
//! per-device lock, the clock and the HTTP client. These types decide which request it
//! may make next, which parts of the answer count and whether the result still applies.

use super::kept::{self, Fingerprint, Purpose, SECRET_BYTES, Scope, Try, Turn};
use super::proto::{self, BackendId, Hello};
use serde_json::Value;
use std::fmt::{self, Write};
use zeroize::Zeroizing;

/// §7.2 step 3, in milliseconds on the server clock.
pub const TRY_MS: u64 = 60_000;
/// §7.2 step 9, including a refused or malformed finish.
pub const MIN_REPLY_MS: u64 = 250;
/// §7.2 step 3, fresh random bytes supplied by the backend for each begin.
pub const SALT_BYTES: usize = 32;
/// §7.2 step 4.
pub const TOKEN_DIGITS: usize = 32;
/// §7.2 step 8: the design does not say how large an upstream answer may be. These
/// two small answers must fit the same bounded reader as hello, including unknown fields.
pub const ANSWER_MAX_BYTES: usize = proto::HELLO_MAX_BYTES;

/// §7.1 and §7.2 step 1, on each device before asking to begin.
pub fn may_begin(agreed: Option<(BackendId, &str)>, hello: &Hello, canonical_name: &str) -> bool {
    !canonical_name.is_empty() && agreed == Some((hello.id, canonical_name))
}

/// §7.2 step 9. Use for every finish return path, including local failures. No absolute
/// deadline addition can wrap. A backwards clock has made no progress.
/// Supply monotonic elapsed transport times, independent of the durable wall-clock mark.
pub fn wait_before_reply(arrived: u64, now: u64) -> u64 {
    MIN_REPLY_MS.saturating_sub(now.saturating_sub(arrived))
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Kind {
    Pc,
    Phone,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Description {
    pub name: String,
    pub kind: Kind,
    pub version: proto::Version,
}

/// §7.2. A verified receipt is used within five seconds of getUser's answer.
pub const RECEIPT_MS: u64 = 5_000;

/// Only this module's successful end of a spent try can construct a receipt. S2 can
/// inspect it and give it once to kept::Turn::finish. It holds no bearer or token.
pub struct Receipt {
    ticket: Try,
    target: Scope,
    device_hash: Fingerprint,
    description: Description,
    spent: u64,
    made: u64,
}

impl Receipt {
    pub(in crate::nexus) fn ticket_ref(&self) -> &Try {
        &self.ticket
    }
    pub fn source(&self) -> &Scope {
        self.ticket.scope()
    }
    pub fn target(&self) -> &Scope {
        &self.target
    }
    pub fn account(&self) -> &str {
        &self.target.account
    }
    pub fn purpose(&self) -> &Purpose {
        self.ticket.purpose()
    }
    pub fn description(&self) -> &Description {
        &self.description
    }
    pub fn spent(&self) -> u64 {
        self.spent
    }
    pub fn made(&self) -> u64 {
        self.made
    }
    pub(in crate::nexus) fn into_parts(self) -> (Try, Scope, Fingerprint, Description) {
        (self.ticket, self.target, self.device_hash, self.description)
    }
}

pub struct Token(Zeroizing<String>);

impl Token {
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
/// both in the begin reply and in the two POST bodies.
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

/// Wire data for begin. The service allocates a fresh try number and salt. The number
/// alone gives no power and the answer has the same shape for known and unknown names.
pub struct Challenge {
    pub number: u64,
    pub salt: Salt,
}

struct Open {
    challenge: Challenge,
    ticket: Try,
    description: Description,
    offered: Fingerprint,
    target: Scope,
    began: u64,
}

/// Exactly one slot per device, in memory. On connect select by the hash index and
/// known_device, allocating a fresh device on a miss. For a session use
/// the device that owns it. Supply its stored hash, or the first begin's hash on a miss.
pub struct Slot {
    owner: Scope,
    registered: Fingerprint,
    open: Option<Open>,
    last_number: Option<u64>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Failure {
    NoTry,
    DeviceSecret,
    TokenForm,
    Stale,
    InvalidName,
    Number,
    Session(kept::Denied),
}

/// The lookup at connect. S3 supplies the candidate found in its device-secret index.
/// A miss needs a newly allocated device. A claimed device id is never an input. This
/// only chooses the tentative slot: spend must still verify the actual secret.
pub fn known_device<'a>(
    candidate: Option<(&'a Scope, &Fingerprint)>,
    offered: &Fingerprint,
) -> Option<&'a Scope> {
    let (scope, stored) = candidate?;
    (stored == offered).then_some(scope)
}

impl Slot {
    pub fn new(owner: &Scope, registered: Fingerprint) -> Self {
        Self {
            owner: owner.clone(),
            registered,
            open: None,
            last_number: None,
        }
    }

    /// Keep purpose, exact canonical name, backend, restore, device, life and device
    /// number (in the ticket), plus the presented device hash, metadata, salt and time.
    pub fn begin(
        &mut self,
        turn: Turn<'_>,
        purpose: Purpose,
        target: Scope,
        description: Description,
        offered: Fingerprint,
        challenge: Challenge,
    ) -> Result<&Challenge, Failure> {
        if offered != self.registered {
            return Err(Failure::DeviceSecret);
        }
        let source = turn.scope();
        if source.backend != self.owner.backend
            || source.account != self.owner.account
            || source.device != self.owner.device
            || source.life != self.owner.life
            || source.restore != self.owner.restore
        {
            return Err(Failure::Stale);
        }
        // the design does not say how a switch allocates B’s row. Bind its vacant scope
        // and life at begin, then require that exact scope again at commit.
        // Same account: use its own current row. Switch: S3 supplies the other
        // account's current life and its newly reserved device identity, never from wire.
        if target.account.is_empty()
            || target.backend != source.backend
            || target.restore != source.restore
            || (target.account == source.account && &target != source)
            || (target.account != source.account && target.device == source.device)
        {
            return Err(Failure::InvalidName);
        }
        // the design does not say how to allocate try numbers. S2 supplies fresh numbers
        // across restarts, increasing per device; a delayed finish cannot spend its successor.
        if self.last_number.is_some_and(|last| challenge.number <= last) {
            return Err(Failure::Number);
        }
        let began = turn.now();
        let ticket = turn.begin(purpose).map_err(Failure::Session)?;
        self.last_number = Some(challenge.number);
        Ok(&self
            .open
            .insert(Open {
                challenge,
                ticket,
                description,
                offered,
                target,
                began,
            })
            .challenge)
    }

    /// §7.2 steps 4–5. Take the matching try first, even on bad input. Nothing can ask
    /// Navidrome until the raw device secret matches both begin and the device record.
    pub fn spend(
        &mut self,
        turn: Turn<'_>,
        number: u64,
        secret: &[u8; SECRET_BYTES],
        text: &str,
    ) -> Result<Check, Failure> {
        if self.open.as_ref().map(|o| o.challenge.number) != Some(number) {
            return Err(Failure::NoTry);
        }
        let open = self.open.take().ok_or(Failure::NoTry)?;
        let token = Token::read(text).ok_or(Failure::TokenForm)?;
        let actual = Fingerprint::of(secret);
        if !((actual == open.offered) & (actual == self.registered)) {
            return Err(Failure::DeviceSecret);
        }
        if !kept::within(open.began, turn.now(), TRY_MS) {
            return Err(Failure::Stale);
        }
        let spent = turn.now();
        turn.current(&open.ticket).map_err(Failure::Session)?;
        Ok(Check {
            open,
            token,
            spent,
            stage: Stage::Trust,
        })
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Stage {
    Trust,
    Ping,
    GetUser,
}

/// The effects permitted by the answer. Navidrome's hidden limit also returns code 40:
/// it is *always* Refused. TooManyTries requires separate evidence from S2's §7.8 budget.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Outcome {
    Proven,
    Refused,
    TooManyTries,
    UntrustedCheck,
    CouldNotAsk,
}

/// Supplies the admission decision made under S2's shared §7.8 counters.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Admission {
    Allowed,
    LimitReached,
}

/// The only credential-bearing request builder. POST the form to the configured server
/// plus path(), with application/x-www-form-urlencoded, and disable redirects. The trust
/// probe alone adds the configured identity header with a fresh random word as its value.
/// There is no input for a caller's address, cookie, headers, query, credentials or URL.
pub struct Call {
    check: Check,
    body: Zeroizing<String>,
}

impl Call {
    pub fn path(&self) -> &'static str {
        match self.check.stage {
            Stage::Trust | Stage::Ping => "/rest/ping",
            Stage::GetUser => "/rest/getUser",
        }
    }
    pub fn form(&self) -> &str {
        &self.body
    }
    pub fn needs_trust_probe_header(&self) -> bool {
        self.check.stage == Stage::Trust
    }

    pub fn answer(
        self,
        turn: Turn<'_>,
        reply: Option<(u16, &[u8])>,
        admission: Admission,
    ) -> Result<Step, Failure> {
        self.check.answer(turn, reply, admission)
    }
}

impl fmt::Debug for Call {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Call(..)")
    }
}

pub struct Check {
    open: Open,
    token: Token,
    spent: u64,
    stage: Stage,
}

/// A closed sequence. Only Next can make another call. Only after both successes can
/// the caller commit a grant, and only while the ticket still belongs to that device.
#[derive(Debug)]
pub enum Step {
    Next(Check),
    Done(Outcome),
    Verified(Receipt),
}

impl Check {
    fn current(&self, turn: Turn<'_>) -> Result<(), Failure> {
        // §7.2. The receipt must be made while the try is still in time.
        if !kept::within(self.open.began, turn.now(), TRY_MS) {
            return Err(Failure::Stale);
        }
        turn.current(&self.open.ticket).map_err(Failure::Session)
    }

    /// Build only the call allowed at this step. The service runs its admission check
    /// before dispatch. The only sign-in fields are u/t/s; v/c/f and getUser's username
    /// are fixed here. the design does not say which client label or protocol version;
    /// use the measured v1.16.1 and a fixed backend name.
    pub fn call(self, turn: Turn<'_>) -> Result<Call, Failure> {
        self.current(turn)?;
        let mut body = Zeroizing::new(String::from("v=1.16.1&c=nexus-music&f=json"));
        if self.stage != Stage::Trust {
            let account = crate::text::encode(&self.open.target.account);
            if self.stage == Stage::GetUser {
                body.push_str("&username=");
                body.push_str(&account);
            }
            body.push_str("&u=");
            body.push_str(&account);
            body.push_str("&s=");
            body.push_str(self.open.challenge.salt.as_str());
            body.push_str("&t=");
            body.push_str(&self.token.0);
        }
        Ok(Call { check: self, body })
    }

    /// S2 supplies a complete response read to EOF under ANSWER_MAX_BYTES; None means the
    /// transport failed or a redirect was encountered. A 429 alone proves nothing.
    fn answer(
        mut self,
        turn: Turn<'_>,
        reply: Option<(u16, &[u8])>,
        admission: Admission,
    ) -> Result<Step, Failure> {
        let made = turn.now();
        self.current(turn)?;
        let Some(envelope) = reply.and_then(|(status, bytes)| read(status, bytes)) else {
            return Ok(Step::Done(Outcome::CouldNotAsk));
        };
        let done = match (self.stage, envelope) {
            (Stage::Trust, Envelope::Error(MISSING_PARAMETER, message)) if message == MISSING_USER => {
                if admission == Admission::LimitReached {
                    Some(Outcome::TooManyTries)
                } else {
                    self.stage = Stage::Ping;
                    None
                }
            }
            (Stage::Trust, Envelope::Error(BAD_SIGN_IN, _) | Envelope::Ok(_)) => {
                Some(Outcome::UntrustedCheck)
            }
            (Stage::Trust, _) => Some(Outcome::CouldNotAsk),
            (_, Envelope::Error(BAD_SIGN_IN, _)) => Some(Outcome::Refused),
            (Stage::Ping, Envelope::Ok(None)) => {
                self.stage = Stage::GetUser;
                None
            }
            (Stage::GetUser, Envelope::Ok(Some(name))) if name == self.open.target.account => {
                let target = Scope {
                    account: name,
                    ..self.open.target
                };
                return Ok(Step::Verified(Receipt {
                    ticket: self.open.ticket,
                    target,
                    device_hash: self.open.offered,
                    made,
                    spent: self.spent,
                    description: self.open.description,
                }));
            }
            // A mismatch with the requested spelling is an invalid answer. Only a
            // successfully verified change of account can end the old device (§7.7).
            _ => Some(Outcome::CouldNotAsk),
        };
        Ok(match done {
            Some(outcome) => Step::Done(outcome),
            None => Step::Next(self),
        })
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

#[cfg(test)]
mod tests;

#[cfg(test)]
mod e3;

#[cfg(test)]
mod receipt_tests;

impl fmt::Debug for Receipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Receipt(..)")
    }
}

impl fmt::Debug for Challenge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Challenge(..)")
    }
}

impl fmt::Debug for Open {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Open(..)")
    }
}

impl fmt::Debug for Slot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Slot(..)")
    }
}

impl fmt::Debug for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Check(..)")
    }
}
