//! The music-server login (§9). S9 supplies the configured internal address,
//! fresh random bytes and the transport. The public address goes to devices.

use super::account::{
    AccountId, MusicAddress, MusicLogin, MusicLoginResult, MusicSalt, MusicToken, Name, SALT_LENGTH,
};
use super::kept::{AccountState, MusicAuthorization, SessionKind, Status};
use super::proto::{self, BackendId};
use super::wire::{self, BadMessage, Fields, Value};
use serde_json::value::RawValue;
use std::fmt;
use zeroize::Zeroizing;

mod md5;
mod seal;
pub use seal::{CredentialBinding, NONCE_BYTES, SealedCredential, SealingKey};

/// The design does not say how large an upstream answer may be. Use the bounded
/// hello size for these small responses, including their unknown fields.
pub const ANSWER_MAX_BYTES: usize = proto::HELLO_MAX_BYTES;

/// §9.4. The only constructor requires C7's nonempty salt and the password bytes.
pub struct SaltedToken {
    salt: MusicSalt,
    token: MusicToken,
}

impl SaltedToken {
    pub fn from_password(password: &[u8], salt: MusicSalt) -> Self {
        let digest = md5::with_salt(password, &salt);
        Self {
            salt,
            token: MusicToken::from_bytes(*digest),
        }
    }

    pub fn salt(&self) -> &MusicSalt {
        &self.salt
    }

    pub fn token(&self) -> &MusicToken {
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
    Refused,
    NameDiffers,
    Unusable,
    CouldNotAsk,
}

/// A POST to the backend's configured internal endpoint. The transport must disable redirects,
/// cookies and caller-supplied headers and use application/x-www-form-urlencoded.
pub struct Post<'a> {
    address: &'a str,
    path: &'static str,
    form: Zeroizing<String>,
}

impl Post<'_> {
    pub fn address(&self) -> &str {
        self.address
    }

    pub fn path(&self) -> &'static str {
        self.path
    }

    pub fn form(&self) -> &str {
        &self.form
    }
}

impl fmt::Debug for Post<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Post")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct MusicInstallation {
    pub binding: CredentialBinding,
    pub sealed: SealedCredential,
}

/// Hand out a login using the current account authority from the apart store.
/// The authority supplies the count. Use the kind from sign-in or from
/// an authenticated session lookup, and never from the request body. Take
/// `internal` from NEXUS_MUSIC_SERVER. The public address is only for the device.
pub fn hand_out(
    state: &AccountState,
    kind: SessionKind,
    install: Option<&MusicInstallation>,
    key: &SealingKey,
    internal: &str,
    public: Option<&MusicAddress>,
    salt: MusicSalt,
) -> MusicLoginResult {
    if kind == SessionKind::AdminPage || state.music_login == 0 {
        return MusicLoginResult::NoneSetYet;
    }
    // The design does not say what to return when a login exists but the
    // device address is unset. Withhold it until the device has an address.
    let Some(public) = public else {
        return MusicLoginResult::UnusableNow;
    };
    let Some(login) = install.filter(|login| {
        let binding = &login.binding;
        state.status == Status::On
            && state.music_recovery.is_none()
            && binding.backend == state.backend
            && binding.account == state.id
            && binding.count == state.music_login
            && binding.internal == internal
    }) else {
        return MusicLoginResult::UnusableNow;
    };
    let Some(password) = key.open(&login.binding, &login.sealed) else {
        return MusicLoginResult::UnusableNow;
    };
    let SaltedToken { salt, token } = SaltedToken::from_password(&password, salt);
    MusicLoginResult::Login(MusicLogin {
        address: public.clone(),
        name: login.binding.name.clone(),
        salt,
        token,
        count: state.music_login,
    })
}

/// The only input kept accepts to install a typed-in login. It requires a
/// successful ping and an exact user reply from the configured internal address.
pub struct VerifiedLogin {
    authorization: MusicAuthorization,
    install: MusicInstallation,
}

impl VerifiedLogin {
    pub(in crate::nexus) fn into_parts(
        self,
        internal: &str,
    ) -> Option<(MusicAuthorization, MusicInstallation)> {
        (self.install.binding.internal == internal).then_some((self.authorization, self.install))
    }
}

pub struct Provisioning<'a> {
    /// The trusted address from backend configuration. Private HTTP is allowed.
    pub internal: &'a str,
    pub name: Name,
    pub password: Zeroizing<Vec<u8>>,
    pub salt: MusicSalt,
    pub nonce: [u8; NONCE_BYTES],
}

struct Check {
    authorization: MusicAuthorization,
    install: MusicInstallation,
    pair: SaltedToken,
}

/// The check starts only after a grant has been consumed for this account.
pub struct Ping(Check);

impl Ping {
    pub fn provision(
        authorization: MusicAuthorization,
        input: Provisioning<'_>,
        key: &SealingKey,
    ) -> Result<Self, Outcome> {
        let internal = internal_address(input.internal).ok_or(Outcome::Unusable)?;
        let binding = CredentialBinding {
            backend: authorization.backend(),
            account: authorization.account(),
            internal,
            name: input.name,
            count: authorization.next_count(),
        };
        let sealed = key
            .seal(&binding, &input.password, input.nonce)
            .ok_or(Outcome::Unusable)?;
        let pair = SaltedToken::from_password(&input.password, input.salt);
        Ok(Self(Check {
            authorization,
            install: MusicInstallation { binding, sealed },
            pair,
        }))
    }

    pub fn post(&self) -> Post<'_> {
        login_post(
            &self.0.install.binding.internal,
            self.0.install.binding.name.as_str(),
            &self.0.pair,
            false,
        )
    }

    pub fn answer(self, reply: Option<(u16, &[u8])>) -> Result<GetUser, Outcome> {
        ping_answer(reply)?;
        Ok(GetUser(self.0))
    }
}

pub struct GetUser(Check);

impl GetUser {
    pub fn post(&self) -> Post<'_> {
        login_post(
            &self.0.install.binding.internal,
            self.0.install.binding.name.as_str(),
            &self.0.pair,
            true,
        )
    }

    pub fn answer(self, reply: Option<(u16, &[u8])>) -> Result<VerifiedLogin, Outcome> {
        let check = self.0;
        user_answer(reply, check.install.binding.name.as_str())?;
        Ok(VerifiedLogin {
            authorization: check.authorization,
            install: check.install,
        })
    }
}

fn internal_address(address: &str) -> Option<String> {
    // The design does not say what is allowed in the backend-only address.
    // Accept only an absolute HTTP(S) base without credentials, query or fragment.
    if address.len() > ANSWER_MAX_BYTES
        || address.contains(['\\', '@'])
        || address.chars().any(char::is_whitespace)
    {
        return None;
    }
    let url = url::Url::parse(address).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !address.starts_with(&format!("{}://", url.scheme()))
        || url.host().is_none()
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    Some(address.to_owned())
}

// The design does not say which client label or protocol version to put in forms.
// Use E3's measured protocol version and a fixed backend label.
const FORM: &str = "v=1.16.1&c=nexus-music&f=json";
const BAD_SIGN_IN: u64 = 40;
const NOT_ALLOWED: u64 = 50;

fn login_post<'a>(address: &'a str, name: &str, pair: &SaltedToken, user: bool) -> Post<'a> {
    let name = crate::text::encode(name);
    let salt = pair.salt().as_str();
    let token = pair.token().expose_hex();
    let mut form = Zeroizing::new(String::with_capacity(
        FORM.len() + name.len() * 2 + salt.len() + token.len() + 21,
    ));
    form.push_str(FORM);
    if user {
        form.push_str("&username=");
        form.push_str(&name);
    }
    form.push_str("&u=");
    form.push_str(&name);
    form.push_str("&s=");
    form.push_str(salt);
    form.push_str("&t=");
    form.push_str(&token);
    Post {
        address,
        path: if user { "/rest/getUser" } else { "/rest/ping" },
        form,
    }
}

fn ping_answer(reply: Option<(u16, &[u8])>) -> Result<(), Outcome> {
    match read_reply(reply) {
        Some(Envelope::Ok(None)) => Ok(()),
        Some(Envelope::Error(BAD_SIGN_IN)) => Err(Outcome::Refused),
        _ => Err(Outcome::CouldNotAsk),
    }
}

fn user_answer(reply: Option<(u16, &[u8])>, name: &str) -> Result<(), Outcome> {
    match read_reply(reply) {
        Some(Envelope::Ok(Some(found))) if found.as_str() == name => Ok(()),
        Some(Envelope::Ok(Some(_))) => Err(Outcome::NameDiffers),
        // E3.4. Wrong credentials and the upstream limiter have the same envelope.
        Some(Envelope::Error(BAD_SIGN_IN)) => Err(Outcome::Refused),
        _ => Err(Outcome::CouldNotAsk),
    }
}

/// §6.3. Only a complete, bounded Navidrome envelope can mark a login refused.
/// A caller can inspect the prefix of a stream without buffering its media.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ReplyStart {
    Refused,
    NotAllowed,
    Busy,
    RangeUnavailable,
    Other,
}

pub fn classify_start(status: u16, prefix: &[u8], complete: bool) -> ReplyStart {
    // The design does not say which HTTP statuses separate these failures.
    // Use only the explicit HTTP meanings and measured Subsonic codes.
    match status {
        403 => ReplyStart::NotAllowed,
        416 => ReplyStart::RangeUnavailable,
        429 | 503 => ReplyStart::Busy,
        200 if complete => match read_reply(Some((status, prefix))) {
            Some(Envelope::Error(BAD_SIGN_IN)) => ReplyStart::Refused,
            Some(Envelope::Error(NOT_ALLOWED)) => ReplyStart::NotAllowed,
            _ => ReplyStart::Other,
        },
        _ => ReplyStart::Other,
    }
}

enum Envelope {
    Ok(Option<Zeroizing<String>>),
    Error(u64),
}

impl Value for Envelope {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let root = Fields::read(raw)?;
        if root.0.len() != 1 {
            return Err(BadMessage::Form);
        }
        let response = root.0.get("subsonic-response").ok_or(BadMessage::Form)?;
        let body = Fields::read(response)?;
        // The design does not say which extra envelope payloads are safe.
        // Permit observed metadata and only the expected success/error members.
        for (key, raw) in &body.0 {
            match key.as_str() {
                "version" | "type" | "serverVersion" => {
                    wire::json_string(raw)?;
                }
                "openSubsonic" => {
                    bool::read(raw)?;
                }
                "status" | "user" | "error" => {}
                _ => return Err(BadMessage::Form),
            }
        }
        let status = wire::json_string(body.0.get("status").ok_or(BadMessage::Form)?)?;
        match status.as_str() {
            "ok" if !body.0.contains_key("error") => {
                let name = if let Some(raw) = body.0.get("user") {
                    let user = Fields::read(raw)?;
                    let name = wire::json_string(user.0.get("username").ok_or(BadMessage::Form)?)?;
                    if name.is_empty() {
                        return Err(BadMessage::Form);
                    }
                    Some(name)
                } else {
                    None
                };
                Ok(Self::Ok(name))
            }
            "failed" if !body.0.contains_key("user") => {
                let error = Fields::read(body.0.get("error").ok_or(BadMessage::Form)?)?;
                wire::json_string(error.0.get("message").ok_or(BadMessage::Form)?)?;
                Ok(Self::Error(error.get("code")?))
            }
            _ => Err(BadMessage::Form),
        }
    }
}

fn read_reply(reply: Option<(u16, &[u8])>) -> Option<Envelope> {
    let (status, bytes) = reply?;
    // The design does not say what a convincing body at an unmeasured status
    // means. Treat only a complete measured HTTP 200 envelope as authoritative.
    if status != 200 {
        return None;
    }
    wire::read_value(bytes, ANSWER_MAX_BYTES).ok()
}

#[cfg(test)]
mod e3;
#[cfg(test)]
mod tests;
