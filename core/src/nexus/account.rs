//! Account calls, their JSON, and the decisions the server makes before checking a password.
//! Time is in milliseconds and comes from the caller. The caller owns the transport, the
//! slow password check, the store, and the lifetime of values returned by this module.

use super::proto::{BackendId, Speaks, Version};
use super::wire::{self, Fields, object, plain_json, read_text, write_backend, write_speaks, write_version};
pub use super::wire::{BODY_MAX_BYTES, BadMessage, WireBody, encode, read};
use serde::{Serialize, Serializer};
use serde_json::value::RawValue;
use sha2::digest::{CtOutput, Output, OutputSizeUser, array::ArraySize, consts::{U8, U12, U16, U32, U512}};
use std::collections::VecDeque;
use std::fmt;
use std::net::Ipv6Addr;
use zeroize::Zeroizing;

pub const SIGN_IN: &str = "/nexus/api/signin";
// the design does not say the spelling of the other paths. Each takes POST.
pub const SIGN_OUT: &str = "/nexus/api/signout";
pub const LIST_DEVICES: &str = "/nexus/api/devices";
pub const FORCE_OUT: &str = "/nexus/api/force-out";
pub const CHANGE_PASSWORD: &str = "/nexus/api/change-password";
pub const MUSIC_LOGIN: &str = "/nexus/api/music-login";
pub const PATHS: &[&str] = &[SIGN_IN, SIGN_OUT, LIST_DEVICES, FORCE_OUT, CHANGE_PASSWORD, MUSIC_LOGIN];

pub const EMAIL_MAX_BYTES: usize = 254;
pub const PASSWORD_MIN_CHARS: usize = 10;
pub const PASSWORD_MAX_CHARS: usize = 128;
const PASSWORD_MAX_BYTES: usize = 4 * PASSWORD_MAX_CHARS;
pub const SECRET_BYTES: usize = 32;
pub const CODE_LENGTH: usize = 12;
// the design does not say which characters look alike. Use upper case alone. Leave
// out the groups I/L/1, O/0, S/5 and Z/2, and keep only the digit in E/3, A/4, G/6,
// T/7, B/8 and Q/9.
pub const CODE_ALPHABET: &[u8] = b"CDFHJKMNPRUVWXY346789";
// the design does not say the field ceilings. Names are counted as UTF-8 bytes.
pub const NAME_MAX_BYTES: usize = 128;
pub const MUSIC_ADDRESS_MAX_BYTES: usize = 2048;
pub const SALT_LENGTH: usize = 16;
pub const TOKEN_BYTES: usize = 16;
pub const DEVICES_PER_PAGE: usize = 256;

pub const CODE_LIFETIME_MS: u64 = 7 * 24 * 60 * 60 * 1000;
pub const SESSION_IDLE_MS: u64 = 90 * 24 * 60 * 60 * 1000;
pub const PASSWORD_LEAVE_MS: u64 = 10 * 60 * 1000;
pub const RETRY_RECEIPT_MS: u64 = 30 * 24 * 60 * 60 * 1000;

/// An email is a name, not a delivery address. the design does not say what to do with
/// empty halves or internal spaces and controls. Refuse them; leave other non-ASCII
/// letters alone. The byte ceiling applies to the tidied name.
pub fn tidy_email(text: &str) -> Option<String> {
    let text = text.trim();
    if text.len() > EMAIL_MAX_BYTES || text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let (local, host) = text.split_once('@')?;
    if local.is_empty() || host.is_empty() || host.contains('@') {
        return None;
    }
    Some(text.to_ascii_lowercase())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Email(String);

impl Email {
    pub fn new(text: &str) -> Option<Self> {
        tidy_email(text).map(Self)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Name(String);

impl Name {
    // the design does not say whether blank names or controls are allowed.
    pub fn new(text: &str) -> Option<Self> {
        if text.len() > NAME_MAX_BYTES || text.trim().is_empty() || text.chars().any(char::is_control) {
            return None;
        }
        Some(Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A password for one check. All Unicode scalar values count as characters, including
/// spaces. The caller drops this value after the check.
pub struct Password {
    bytes: Zeroizing<[u8; PASSWORD_MAX_BYTES]>,
    len: usize,
}

fn padded(text: &str) -> Zeroizing<[u8; PASSWORD_MAX_BYTES]> {
    let mut bytes = Zeroizing::new([0u8; PASSWORD_MAX_BYTES]);
    for (out, byte) in bytes.iter_mut().zip(text.bytes()) {
        *out = byte;
    }
    bytes
}

// CtOutput compares every byte of a fixed-size RustCrypto array. Here it only
// supplies equality; no digest or MAC is computed.
struct CompareSize<N>(std::marker::PhantomData<N>);

impl<N: ArraySize> OutputSizeUser for CompareSize<N> {
    type OutputSize = N;
}

fn same_bytes<N: ArraySize>(left: &[u8], right: &[u8]) -> bool {
    let (Ok(left), Ok(right)) = (Output::<CompareSize<N>>::try_from(left), Output::<CompareSize<N>>::try_from(right)) else {
        return false;
    };
    CtOutput::<CompareSize<N>>::new(left) == CtOutput::<CompareSize<N>>::new(right)
}

impl Password {
    pub fn new(text: &str) -> Option<Self> {
        if text.len() > PASSWORD_MAX_BYTES || !(PASSWORD_MIN_CHARS..=PASSWORD_MAX_CHARS).contains(&text.chars().count()) {
            return None;
        }
        Some(Self { bytes: padded(text), len: text.len() })
    }

    pub fn expose(&self) -> &str {
        self.bytes.get(..self.len).and_then(|bytes| std::str::from_utf8(bytes).ok()).unwrap_or_default()
    }

    /// Check before accepting a new password, against the email from the account.
    pub fn allowed_for(&self, email: &Email) -> bool {
        // the design does not say if "not the email" includes a spelling that tidies to it.
        // Reject that spelling too, without making a second ordinary String of the password.
        let spelling = self.expose().trim();
        let mut folded = Zeroizing::new([0u8; PASSWORD_MAX_BYTES]);
        for (out, byte) in folded.iter_mut().zip(spelling.bytes()) {
            *out = byte.to_ascii_lowercase();
        }
        let canonical = padded(email.as_str());
        !(same_bytes::<U512>(folded.as_ref(), canonical.as_ref())
            & same_bytes::<U8>(&(spelling.len() as u64).to_be_bytes(), &(email.as_str().len() as u64).to_be_bytes()))
    }
}

impl PartialEq for Password {
    fn eq(&self, other: &Self) -> bool {
        same_bytes::<U512>(self.bytes.as_ref(), other.bytes.as_ref())
            & same_bytes::<U8>(&(self.len as u64).to_be_bytes(), &(other.len as u64).to_be_bytes())
    }
}

impl Eq for Password {}

macro_rules! secret_debug {
    ($name:ident) => {
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($name), "(..)"))
            }
        }
    };
}

secret_debug!(Password);

impl Serialize for Password {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.expose())
    }
}

fn lower_hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn decode_hex<const N: usize>(text: &str) -> Option<Zeroizing<[u8; N]>> {
    if text.len() != N * 2 {
        return None;
    }
    let mut decoded = Zeroizing::new([0u8; N]);
    for (out, pair) in decoded.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        let [hi, lo] = pair else { return None };
        *out = (lower_hex(*hi)? << 4) | lower_hex(*lo)?;
    }
    Some(decoded)
}

fn hex_text(bytes: &[u8]) -> Zeroizing<String> {
    let digit = |n: u8| char::from(if n < 10 { b'0' + n } else { b'a' + n - 10 });
    let mut text = Zeroizing::new(String::with_capacity(bytes.len() * 2));
    for &byte in bytes {
        text.push(digit(byte >> 4));
        text.push(digit(byte & 15));
    }
    text
}

// the design does not say how binary fields are written. Use the same canonical
// lower-case hex as hello's id, including in the Authorization: Bearer value.
macro_rules! hex_secret {
    ($name:ident, $len:ident, $width:ty) => {
        #[derive(Clone)]
        pub struct $name(Zeroizing<[u8; $len]>);

        impl $name {
            pub fn from_bytes(bytes: [u8; $len]) -> Self {
                Self(Zeroizing::new(bytes))
            }

            pub fn from_hex(text: &str) -> Option<Self> {
                decode_hex(text).map(Self)
            }

            pub fn expose_bytes(&self) -> &[u8; $len] {
                &self.0
            }

            pub fn expose_hex(&self) -> Zeroizing<String> {
                hex_text(self.expose_bytes())
            }
        }

        secret_debug!($name);

        impl PartialEq for $name {
            fn eq(&self, other: &Self) -> bool {
                same_bytes::<$width>(self.expose_bytes(), other.expose_bytes())
            }
        }

        impl Eq for $name {}

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.expose_hex())
            }
        }

        impl wire::Value for $name {
            fn read(raw: &RawValue) -> Result<Self, BadMessage> {
                read_text(raw, Self::from_hex)
            }
        }
    };
}

hex_secret!(DeviceSecret, SECRET_BYTES, U32);
hex_secret!(Session, SECRET_BYTES, U32);
hex_secret!(MusicToken, TOKEN_BYTES, U16);
hex_secret!(StampTag, SECRET_BYTES, U32);

impl Session {
    /// The Authorization field value. the design does not say how the scheme is written.
    /// Accept the HTTP scheme case-insensitively, with one ASCII space and canonical hex.
    pub fn from_authorization(value: &str) -> Option<Self> {
        if value.len() != 7 + 2 * SECRET_BYTES {
            return None;
        }
        let (scheme, token) = value.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("Bearer") {
            return None;
        }
        Self::from_hex(token)
    }

    pub fn authorization(&self) -> Zeroizing<String> {
        let mut value = Zeroizing::new(String::with_capacity(7 + 2 * SECRET_BYTES));
        value.push_str("Bearer ");
        value.push_str(&self.expose_hex());
        value
    }
}

/// A one-time code. A caller supplying randomness chooses bytes uniformly from
/// CODE_ALPHABET. This constructor does not make random choices.
pub struct OneTimeCode(Zeroizing<[u8; CODE_LENGTH]>);

/// §9.4. the design does not say the salt's form. Use the 16 ASCII letters or digits that
/// the existing app uses when it makes a salt.
#[derive(Clone)]
pub struct MusicSalt(Zeroizing<[u8; SALT_LENGTH]>);

macro_rules! text_secret {
    ($name:ident, $len:ident, $width:ty, $allowed:expr) => {
        impl $name {
            pub fn new(text: &str) -> Option<Self> {
                let bytes: &[u8; $len] = text.as_bytes().try_into().ok()?;
                if !bytes.iter().all($allowed) {
                    return None;
                }
                Some(Self(Zeroizing::new(*bytes)))
            }

            pub fn expose_bytes(&self) -> &[u8; $len] {
                &self.0
            }

            pub fn expose(&self) -> &str {
                // The constructor only admits ASCII bytes.
                std::str::from_utf8(self.expose_bytes()).unwrap_or_default()
            }
        }

        secret_debug!($name);

        impl PartialEq for $name {
            fn eq(&self, other: &Self) -> bool {
                same_bytes::<$width>(self.expose_bytes(), other.expose_bytes())
            }
        }

        impl Eq for $name {}

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(self.expose())
            }
        }

        impl wire::Value for $name {
            fn read(raw: &RawValue) -> Result<Self, BadMessage> {
                read_text(raw, Self::new)
            }
        }
    };
}

text_secret!(OneTimeCode, CODE_LENGTH, U12, |byte| CODE_ALPHABET.contains(byte));
text_secret!(MusicSalt, SALT_LENGTH, U16, u8::is_ascii_alphanumeric);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct MusicAddress(String);

impl MusicAddress {
    pub fn new(text: &str) -> Option<Self> {
        // the design does not say if the music address (as distinct from the Nexus
        // address) may be cleartext. Require HTTPS and an unambiguous base URL.
        if text.len() > MUSIC_ADDRESS_MAX_BYTES || !text.is_ascii() || text.contains(['?', '#', '\\']) {
            return None;
        }
        let tail = text.strip_prefix("https://")?;
        let (authority, path) = tail.split_once('/').unwrap_or((tail, ""));
        if authority.trim() != authority {
            return None;
        }
        super::proto::hello_url(&format!("https://{authority}"))?;
        if let Some(literal) = authority.strip_prefix('[') {
            literal.split_once(']')?.0.parse::<Ipv6Addr>().ok()?;
        }
        // Use an ordinary URI path. Validate percent-escapes before the app can join to it.
        let mut bytes = path.bytes();
        while let Some(byte) = bytes.next() {
            if byte == b'%' {
                if !bytes.next()?.is_ascii_hexdigit() || !bytes.next()?.is_ascii_hexdigit() {
                    return None;
                }
            } else if !(byte.is_ascii_alphanumeric() || b"/-._~!$&'()*+,;=:@".contains(&byte)) {
                return None;
            }
        }
        Some(Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A device number is never zero. the design does not say its wire type; use the kept
/// state's u64 device numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct DeviceId(u64);

impl DeviceId {
    pub fn new(number: u64) -> Option<Self> {
        (number > 0).then_some(Self(number))
    }
    pub fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Pc,
    Phone,
}

/// Signing in by password checks the named account. §8.4's code takes the place of
/// both the email and password. A code is only a request for password setup.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Credential {
    Password { email: Email, password: Password },
    Code(OneTimeCode),
}

object! {
    SignIn {
        credential: Credential,
        device_secret: DeviceSecret,
        device_name: Name,
        device_kind: DeviceKind,
        #[serde(serialize_with = "write_version")]
        app_version: Version,
        #[serde(serialize_with = "write_speaks")]
        speaks: Speaks,
        // the design does not say how the first sign-in supplies a restore number.
        // Null means the app has none. A number is checked by the server.
        restore: Option<u64>,
    }
}

object! { Person { email: Email, name: Name, admin: bool } }
object! { MusicLogin { address: MusicAddress, name: Name, salt: MusicSalt, token: MusicToken, count: u64 } }

// §13.6. the design does not say the notice's wire type. Use bounded text, leaving
// exactly when to show it with the device.
pub const NOTICE_MAX_BYTES: usize = 2048;
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Notice(String);

impl Notice {
    pub fn new(text: &str) -> Option<Self> {
        (!text.trim().is_empty() && text.len() <= NOTICE_MAX_BYTES && !text.chars().any(|c| c.is_control() && c != '\n'))
            .then(|| Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

object! {
    SignedIn { session: Session, person: Person, device: DeviceId, music: MusicLogin, notice: Notice }
}

object! {
    /// The server grants this session leave only for choosing a password (§8.4).
    PasswordSetup { session: Session, person: Person, device: DeviceId }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignInAnswer {
    SignedIn(SignedIn),
    PasswordSetup(PasswordSetup),
}

object! {
    Stamp {
        #[serde(serialize_with = "write_backend")]
        backend: BackendId,
        restore: u64,
        account: Email,
        life: u64,
        change: u64,
        tag: StampTag,
    }
}
object! { Receipt { stamp: Stamp, music_login_count: u64 } }

object! {
    /// A call authenticated by the Authorization header (outside its JSON body).
    RequestHead {
        #[serde(serialize_with = "write_speaks")]
        speaks: Speaks,
        restore: u64,
    }
}

/// §12.2. Make this once when the person acts and reuse it for a retry. The server
/// keeps the outcome for [`RETRY_RECEIPT_MS`].
/// the design does not say the type or range; use a nonzero u64, like a device id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RequestId(u64);

impl RequestId {
    pub fn new(number: u64) -> Option<Self> {
        (number > 0).then_some(Self(number))
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

object! { SignOut { head: RequestHead, request_id: RequestId } }
object! { ListDevices { head: RequestHead, after: Option<DeviceId> } }
object! { ForceOut { head: RequestHead, device: DeviceId } }
object! { GetMusicLogin { head: RequestHead } }

object! {
    ChangePassword {
        head: RequestHead,
        request_id: RequestId,
        // Null only under the password-setup session. For an ordinary session the
        // server requires and checks the current password before taking the new one.
        current_password: Option<Password>,
        new_password: Password,
    }
}

object! {
    Device {
        id: DeviceId, name: Name, kind: DeviceKind,
        #[serde(serialize_with = "write_version")]
        app_version: Version,
        last_seen_ms: u64,
        signed_in: bool,
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct DevicePage {
    devices: Vec<Device>,
    more: bool,
}

impl DevicePage {
    // the design does not say how to bound the listing. Page in device-id order,
    // excluding the cursor itself. A page with more to read must advance its cursor.
    pub fn new(devices: Vec<Device>, more: bool) -> Option<Self> {
        if devices.len() > DEVICES_PER_PAGE || (more && devices.is_empty()) {
            return None;
        }
        if !devices.iter().zip(devices.iter().skip(1)).all(|(left, right)| left.id < right.id) {
            return None;
        }
        Some(Self { devices, more })
    }

    pub fn devices(&self) -> &[Device] {
        &self.devices
    }

    pub fn next(&self) -> Option<DeviceId> {
        self.devices.last().filter(|_| self.more).map(|device| device.id)
    }
}

object! { SignedOut {} }
object! { ForcedOut { music_cut_off: bool } }
object! { PasswordChanged {} }

/// All refusals use fixed tags. A wrong password, absent account, or invalid, used or
/// expired one-time code all use WrongLogin, without an account or a receipt.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    WrongLogin,
    TooManyTries { wait: Wait },
    AccountOff,
    Busy,
    StartAgain,
    NotSignedIn,
    NoSuchDevice,
    PasswordNotAllowed,
    PasswordSetupOnly,
    BadRequest,
    NoMusicLogin,
    Incompatible {
        #[serde(serialize_with = "write_speaks")]
        speaks: Speaks,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Wait(u64);

impl Wait {
    pub fn new(ms: u64) -> Option<Self> {
        (1..=WRONG_PASSWORD_WINDOW_MS).contains(&ms).then_some(Self(ms))
    }

    pub fn ms(self) -> u64 {
        self.0
    }
}

/// the design does not say which status carries a typed refusal. Like hello, only 200
/// with a whole and valid body counts. A network failure supplies no body to parse.
pub const REPLY_STATUS: u16 = 200;

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply<T> {
    Done { result: T, receipt: Receipt },
    Refused(Refusal),
    // the design does not say how to stamp a reply without a trusted account.
    // Only refusals to an authenticated device may carry its receipt.
    RefusedWithReceipt(RefusedWithReceipt),
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct RefusedWithReceipt {
    reason: Refusal,
    receipt: Receipt,
}

impl RefusedWithReceipt {
    pub fn new(reason: Refusal, receipt: Receipt) -> Option<Self> {
        if matches!(reason, Refusal::WrongLogin | Refusal::NotSignedIn) {
            return None;
        }
        Some(Self { reason, receipt })
    }

    pub fn reason(&self) -> &Refusal {
        &self.reason
    }

    pub fn receipt(&self) -> &Receipt {
        &self.receipt
    }
}

pub type SignInReply = Reply<SignInAnswer>;
pub type SignOutReply = Reply<SignedOut>;
pub type ListDevicesReply = Reply<DevicePage>;
pub type ForceOutReply = Reply<ForcedOut>;
pub type ChangePasswordReply = Reply<PasswordChanged>;
pub type MusicLoginReply = Reply<MusicLogin>;

pub fn read_reply<T>(status: u16, body: &[u8]) -> Result<Reply<T>, BadMessage>
where
    Reply<T>: wire::Message,
{
    if status != REPLY_STATUS {
        return Err(BadMessage::Status(status));
    }
    read(body)
}

macro_rules! text_value {
    ($kind:ty, $constructor:expr) => {
        impl wire::Value for $kind {
            fn read(raw: &RawValue) -> Result<Self, BadMessage> {
                read_text(raw, $constructor)
            }
        }
    };
}

text_value!(Email, Email::new);
text_value!(Name, Name::new);
text_value!(Password, Password::new);
text_value!(MusicAddress, MusicAddress::new);
text_value!(Notice, Notice::new);
text_value!(DeviceKind, |text| match text {
    "pc" => Some(DeviceKind::Pc),
    "phone" => Some(DeviceKind::Phone),
    _ => None,
});

impl wire::Value for DeviceId {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        Self::new(u64::read(raw)?).ok_or(BadMessage::Form)
    }
}

impl wire::Value for RequestId {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        Self::new(u64::read(raw)?).ok_or(BadMessage::Form)
    }
}

impl wire::Value for Wait {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        Self::new(u64::read(raw)?).ok_or(BadMessage::Form)
    }
}

impl wire::Value for Credential {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let fields = Fields::read(raw)?;
        match fields.select(&["password", "code"])? {
            ("password", raw) => {
                let fields = Fields::read(raw)?;
                Ok(Self::Password { email: fields.get("email")?, password: fields.get("password")? })
            }
            ("code", raw) => Ok(Self::Code(OneTimeCode::read(raw)?)),
            _ => Err(BadMessage::Form),
        }
    }
}

impl wire::Value for SignInAnswer {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        match Fields::read(raw)?.select(&["signed_in", "password_setup"])? {
            ("signed_in", raw) => Ok(Self::SignedIn(SignedIn::read(raw)?)),
            ("password_setup", raw) => Ok(Self::PasswordSetup(PasswordSetup::read(raw)?)),
            _ => Err(BadMessage::Form),
        }
    }
}

impl wire::Value for DevicePage {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let fields = Fields::read(raw)?;
        let rows = fields.0.get("devices").ok_or(BadMessage::Form)?;
        let rows: Vec<&RawValue> = plain_json(rows.get())?;
        if rows.len() > DEVICES_PER_PAGE {
            return Err(BadMessage::Form);
        }
        let rows = rows.into_iter().map(Device::read).collect::<Result<_, _>>()?;
        Self::new(rows, fields.get("more")?).ok_or(BadMessage::Form)
    }
}

impl wire::Value for Refusal {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        if raw.get().starts_with('"') {
            return read_text(raw, |text| match text {
                "wrong_login" => Some(Self::WrongLogin),
                "account_off" => Some(Self::AccountOff),
                "busy" => Some(Self::Busy),
                "start_again" => Some(Self::StartAgain),
                "not_signed_in" => Some(Self::NotSignedIn),
                "no_such_device" => Some(Self::NoSuchDevice),
                "password_not_allowed" => Some(Self::PasswordNotAllowed),
                "password_setup_only" => Some(Self::PasswordSetupOnly),
                "bad_request" => Some(Self::BadRequest),
                "no_music_login" => Some(Self::NoMusicLogin),
                _ => None,
            });
        }
        let fields = Fields::read(raw)?;
        match fields.select(&["too_many_tries", "incompatible"])? {
            ("too_many_tries", raw) => Ok(Self::TooManyTries { wait: Fields::read(raw)?.get("wait")? }),
            ("incompatible", raw) => Ok(Self::Incompatible { speaks: Fields::read(raw)?.get("speaks")? }),
            _ => Err(BadMessage::Form),
        }
    }
}

trait ReplyResult: wire::Value + Serialize {
    fn agrees(&self, _: &Receipt) -> bool {
        true
    }
}

impl ReplyResult for SignedOut {}
impl ReplyResult for DevicePage {}
impl ReplyResult for ForcedOut {}
impl ReplyResult for PasswordChanged {}

impl ReplyResult for MusicLogin {
    fn agrees(&self, receipt: &Receipt) -> bool {
        self.count == receipt.music_login_count
    }
}

impl ReplyResult for SignInAnswer {
    fn agrees(&self, receipt: &Receipt) -> bool {
        match self {
            Self::SignedIn(reply) => reply.person.email == receipt.stamp.account && reply.music.agrees(receipt),
            Self::PasswordSetup(reply) => reply.person.email == receipt.stamp.account,
        }
    }
}

impl<T: ReplyResult> wire::Value for Reply<T> {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        match Fields::read(raw)?.select(&["done", "refused", "refused_with_receipt"])? {
            ("done", raw) => {
                let fields = Fields::read(raw)?;
                Ok(Self::Done { result: fields.get("result")?, receipt: fields.get("receipt")? })
            }
            ("refused", raw) => Ok(Self::Refused(Refusal::read(raw)?)),
            ("refused_with_receipt", raw) => {
                let fields = Fields::read(raw)?;
                RefusedWithReceipt::new(fields.get("reason")?, fields.get("receipt")?)
                    .map(Self::RefusedWithReceipt).ok_or(BadMessage::Form)
            }
            _ => Err(BadMessage::Form),
        }
    }

    fn check(&self) -> Result<(), BadMessage> {
        if let Self::Done { result, receipt } = self {
            if !result.agrees(receipt) {
                return Err(BadMessage::Form);
            }
        }
        Ok(())
    }
}

impl wire::Message for SignIn {}
impl wire::Message for SignOut {}
impl wire::Message for ListDevices {}
impl wire::Message for ForceOut {}
impl wire::Message for GetMusicLogin {}
impl wire::Message for ChangePassword {}
impl wire::Message for MusicLogin {}
impl<T: ReplyResult> wire::Message for Reply<T> {}

// ---------------------------------------------------------------------------------------
// Limits. These states contain only counts and times.
// ---------------------------------------------------------------------------------------

pub const WRONG_PASSWORDS: u8 = 5;
pub const WRONG_PASSWORD_WINDOW_MS: u64 = 15 * 60 * 1000;
pub const SIGN_INS_PER_MINUTE: usize = 30;
pub const SIGN_IN_WINDOW_MS: u64 = 60 * 1000;
pub const STARTUP_QUIET_MS: u64 = 20 * 1000;
pub const PASSWORD_CHECKS_AT_ONCE: usize = 2;
pub const PASSWORD_CHECKS_WAITING: usize = 8;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AccountFailures {
    window: Option<(u64, u8)>,
}

impl AccountFailures {
    /// Discard an expired entry when keeping the per-email counts in memory.
    pub fn is_expired(&self, now: u64) -> bool {
        self.window.is_none_or(|(first, _)| now.saturating_sub(first) >= WRONG_PASSWORD_WINDOW_MS)
    }

    /// Check before any proof for this account. Keep the decision and its outcome
    /// serialized per email, so two parallel checks cannot both use the fifth
    /// place. A success leaves this state as it is.
    pub fn retry_after(&self, now: u64) -> Option<Wait> {
        let (first, count) = self.window?;
        let remaining = WRONG_PASSWORD_WINDOW_MS.saturating_sub(now.saturating_sub(first));
        if count < WRONG_PASSWORDS {
            return None;
        }
        Wait::new(remaining)
    }

    /// Apply only a wrong password that was actually checked. The first miss opens
    /// the window; the fifth can finish its check, and the next is refused unchecked.
    pub fn record_wrong(&mut self, now: u64) {
        self.window = match self.window {
            // the design does not say how to handle an earlier timestamp in this
            // function. Treat age as zero, so a backward clock never clears a count.
            Some((first, count)) if now.saturating_sub(first) < WRONG_PASSWORD_WINDOW_MS => {
                Some((first, count.saturating_add(1).min(WRONG_PASSWORDS)))
            }
            _ => Some((now, 1)),
        };
    }
}

/// One instance for the whole server, recreated on start. Serialize each admission
/// with other admissions. Call before looking at the account or checking a password.
#[derive(Debug)]
pub struct SignInLimit {
    started: u64,
    seen_at: u64,
    admitted: VecDeque<u64>,
}

impl SignInLimit {
    pub fn new(started: u64) -> Self {
        Self { started, seen_at: started, admitted: VecDeque::with_capacity(SIGN_INS_PER_MINUTE) }
    }

    pub fn admit(&mut self, now: u64) -> Result<(), Refusal> {
        let now = self.seen_at.max(now);
        self.seen_at = now;
        let startup_left = STARTUP_QUIET_MS.saturating_sub(now.saturating_sub(self.started));
        if let Some(wait) = Wait::new(startup_left) {
            return Err(Refusal::TooManyTries { wait });
        }

        // the design does not say whether the minute is a fixed or rolling bucket.
        // Enforce every rolling minute. An admission exactly 60 seconds ago is out.
        while self.admitted.front().is_some_and(|first| now.saturating_sub(*first) >= SIGN_IN_WINDOW_MS) {
            self.admitted.pop_front();
        }
        if self.admitted.len() >= SIGN_INS_PER_MINUTE {
            if let Some(wait) = self.admitted.front().and_then(|first| Wait::new(SIGN_IN_WINDOW_MS.saturating_sub(now.saturating_sub(*first)))) {
                return Err(Refusal::TooManyTries { wait });
            }
            return Err(Refusal::Busy);
        }
        self.admitted.push_back(now);
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordAdmission {
    Check,
    Wait,
    Busy,
}

/// The caller owns the queue and applies the decision with its own admission lock.
pub fn admit_password_check(checking: usize, waiting: usize) -> PasswordAdmission {
    // the design does not say whether a newcomer can jump the queue. Give already
    // waiting checks first use of any newly free place.
    if checking < PASSWORD_CHECKS_AT_ONCE && waiting == 0 {
        PasswordAdmission::Check
    } else if waiting < PASSWORD_CHECKS_WAITING {
        PasswordAdmission::Wait
    } else {
        PasswordAdmission::Busy
    }
}

#[cfg(test)]
mod tests;
