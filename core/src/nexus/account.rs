//! Account calls, their JSON, and the decisions the server makes before checking a password.
//! Time is in milliseconds and comes from the caller. The caller owns the transport, the
//! slow password check, the store, and the lifetime of values returned by this module.

use super::proto::{BackendId, Speaks, Version};
use super::wire::{self, Fields, object, plain_json, read_text, write_backend, write_speaks, write_version};
pub use super::wire::{BODY_MAX_BYTES, BadMessage, WireBody, encode, read};
use serde::{Serialize, Serializer};
use serde_json::value::RawValue;
use hkdf::hmac::{Hmac, Mac};
use sha2::Sha256;
use sha2::digest::{CtOutput, Output, OutputSizeUser, array::ArraySize, consts::{U8, U12, U16, U32, U64, U512}};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use crate::origin::{Address, AddressError, HandoverSource, MusicBase, Origin};
use zeroize::Zeroizing;

pub const SIGN_IN: &str = "/nexus/api/signin";
// the design does not say the spelling of the other paths. Each takes POST.
pub const SIGN_OUT: &str = "/nexus/api/signout";
pub const LIST_DEVICES: &str = "/nexus/api/devices";
pub const FORCE_OUT: &str = "/nexus/api/force-out";
pub const CHANGE_PASSWORD: &str = "/nexus/api/change-password";
pub const MUSIC_LOGIN: &str = "/nexus/api/music-login";
pub const GRANT: &str = "/nexus/api/grant";
pub const PATHS: &[&str] = &[SIGN_IN, SIGN_OUT, LIST_DEVICES, FORCE_OUT, CHANGE_PASSWORD, MUSIC_LOGIN, GRANT];

pub const EMAIL_MAX_BYTES: usize = 254;
pub const PASSWORD_MIN_CHARS: usize = 15;
pub const PASSWORD_MAX_CHARS: usize = 128;
const PASSWORD_MAX_BYTES: usize = 4 * PASSWORD_MAX_CHARS;
pub const SECRET_BYTES: usize = 32;
pub const CODE_LENGTH: usize = 12;
// the design does not say which 32-symbol alphabet. Use Crockford base32.
pub const CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
// the design does not say the field ceilings. Names are counted as UTF-8 bytes.
pub const NAME_MAX_BYTES: usize = 128;
pub const MUSIC_ADDRESS_MAX_BYTES: usize = 2048;
pub const SALT_BYTES: usize = 32;
pub const SALT_LENGTH: usize = 2 * SALT_BYTES;
pub const TOKEN_BYTES: usize = 16;
pub const DEVICES_PER_PAGE: usize = 500;
pub const SMALL_REPLY_MAX_BYTES: usize = 4 * 1024;
pub const SIGN_IN_REPLY_MAX_BYTES: usize = 8 * 1024;
pub const LIST_REPLY_MAX_BYTES: usize = BODY_MAX_BYTES;
pub const PASSWORD_REQUEST_MAX_BYTES: usize = 4 * 1024;
// the design does not say the bound of a generic grant use. Allow the same 4 KiB
// here because an admin can send both music addresses or login text in the change.
pub const GRANTED_CHANGE_MAX_BYTES: usize = PASSWORD_REQUEST_MAX_BYTES;
// the design does not say the size of calls without passwords; their fixed fields fit in 1 KiB.
pub const SMALL_REQUEST_MAX_BYTES: usize = 1024;

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

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
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

    /// The backend hands in its common-password list, one UTF-8 password per line.
    /// Check with the account's own email before accepting a new password.
    pub fn allowed_for(&self, email: &Email, common_passwords: &str) -> bool {
        // the design does not say how variations are treated. Reject case and outer
        // whitespace variations of the email and list entries. Keep the actual password exact.
        fn folded(text: &str) -> Zeroizing<[u8; PASSWORD_MAX_BYTES]> {
            let mut bytes = padded(text);
            bytes.make_ascii_lowercase();
            bytes
        }

        let spelling = self.expose().trim();
        let candidate = folded(spelling);
        let matches = |text: &str| {
            let text = text.trim();
            same_bytes::<U512>(candidate.as_ref(), folded(text).as_ref())
                & same_bytes::<U8>(&(spelling.len() as u64).to_be_bytes(), &(text.len() as u64).to_be_bytes())
        };
        let blocked = common_passwords.lines().fold(matches(email.as_str()), |blocked, line| blocked | matches(line));
        !blocked
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
hex_secret!(SignOutSecret, SECRET_BYTES, U32);
hex_secret!(GrantSecret, SECRET_BYTES, U32);
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

/// §9.4. the design does not say the salt's form or its module. Keep the music
/// prover's spelling of 32 random bytes as 64 lower-case hex digits.
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

impl OneTimeCode {
    /// Read a typed code and normalize Crockford's case and ambiguous spellings.
    pub fn new(text: &str) -> Option<Self> {
        if text.len() != CODE_LENGTH {
            return None;
        }
        let mut canonical = Zeroizing::new([0; CODE_LENGTH]);
        for (out, byte) in canonical.iter_mut().zip(text.bytes()) {
            let byte = match byte.to_ascii_uppercase() {
                b'I' | b'L' => b'1',
                b'O' => b'0',
                byte => byte,
            };
            if !CODE_ALPHABET.contains(&byte) {
                return None;
            }
            *out = byte;
        }
        Some(Self(canonical))
    }

    /// Encode 60 caller-supplied random bits, most significant group first.
    pub fn from_random(random: u64) -> Self {
        let mut remaining = random;
        let mut code = Zeroizing::new([b'0'; CODE_LENGTH]);
        for out in code.iter_mut().rev() {
            *out = CODE_ALPHABET.get((remaining & 31) as usize).copied().unwrap_or(b'0');
            remaining >>= 5;
        }
        Self(code)
    }

    pub fn expose_bytes(&self) -> &[u8; CODE_LENGTH] {
        &self.0
    }

    pub fn expose(&self) -> &str {
        std::str::from_utf8(self.expose_bytes()).unwrap_or_default()
    }
}

secret_debug!(OneTimeCode);

impl PartialEq for OneTimeCode {
    fn eq(&self, other: &Self) -> bool {
        same_bytes::<U12>(self.expose_bytes(), other.expose_bytes())
    }
}

impl Eq for OneTimeCode {}

impl Serialize for OneTimeCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.expose())
    }
}
text_secret!(MusicSalt, SALT_LENGTH, U64, |byte| lower_hex(*byte).is_some());

impl MusicSalt {
    pub fn from_random(bytes: [u8; SALT_BYTES]) -> Self {
        let bytes = Zeroizing::new(bytes);
        let hex = hex_text(bytes.as_ref());
        let mut text = Zeroizing::new([0; SALT_LENGTH]);
        for (out, byte) in text.iter_mut().zip(hex.bytes()) {
            *out = byte;
        }
        Self(text)
    }

    pub fn as_str(&self) -> &str {
        self.expose()
    }
}

/// The origin the person typed for Nexus (§7.1), independent of their music server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NexusOrigin(Origin);

impl NexusOrigin {
    pub fn typed(text: &str) -> Result<Self, AddressError> {
        let address = Address::typed(text)?;
        if address.path() != "/" {
            return Err(AddressError::Path);
        }
        if address.query().is_some() {
            return Err(AddressError::BaseQuery);
        }
        if text.contains('#') {
            return Err(AddressError::Fragment);
        }
        Ok(Self(address.origin().clone()))
    }

    pub fn origin(&self) -> &Origin {
        &self.0
    }

    /// Supply a Nexus call's path constant.
    pub fn call(&self, path: &'static str) -> Result<Address, AddressError> {
        super::proto::plain(path).map_err(|_| AddressError::Path)?;
        Address::parse(&format!("{}{path}", self.0))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct MusicAddress(String);

impl MusicAddress {
    pub fn new(text: &str) -> Option<Self> {
        // the design does not say the base URL's size. Bound it before the common
        // origin parser and keep queries, fragments and credentials out of it.
        if text.len() > MUSIC_ADDRESS_MAX_BYTES || text.contains('#') {
            return None;
        }
        let address = Address::parse(text).ok()?;
        if address.query().is_some() {
            return None;
        }
        Some(Self(text.to_owned()))
    }

    /// Check which Nexus origin and request actually handed over the music address.
    pub fn handed_over(&self, nexus: &NexusOrigin, source: HandoverSource<'_>) -> Result<MusicBase, AddressError> {
        MusicBase::handed_over(nexus.origin(), source, self.as_str())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// the design does not say the number representation. Use kept's nonzero u64
// identifiers. Zero remains available to kept for an unassigned row.
macro_rules! number_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(u64);

        impl $name {
            pub fn new(number: u64) -> Option<Self> {
                (number != 0).then_some(Self(number))
            }

            pub fn get(self) -> u64 {
                self.0
            }
        }

        impl wire::Value for $name {
            fn read(raw: &RawValue) -> Result<Self, BadMessage> {
                Self::new(u64::read(raw)?).ok_or(BadMessage::Form)
            }
        }
    };
}

number_id! {
    /// The account's immutable number, never reassigned on this backend.
    AccountId
}
number_id! {
    /// The device's number.
    DeviceId
}
number_id! {
    /// The identity issued to one session. The bearer is a separate secret.
    SessionId
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    Pc,
    Phone,
}

/// A code replaces the email and current password. Redeem it while setting the
/// new password, then issue the normal sign-in answer (§8.5).
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Credential {
    Password { email: Email, password: Password },
    Code { code: OneTimeCode, new_password: Password },
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

object! { Person { id: AccountId, revision: u64, email: Email, name: Name, admin: bool } }
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

/// §9.4. An account stays signed in in all three cases.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MusicLoginResult {
    Login(MusicLogin),
    NoneSetYet,
    UnusableNow,
}

object! {
    /// Store this note on sign-out if the backend was unreachable. The server binds
    /// its separate secret to just this session and checks it only at sign-out.
    SignOutNote { session: SessionId, secret: SignOutSecret }
}

object! {
    SignedIn {
        session: Session,
        session_id: SessionId,
        sign_out: SignOutNote,
        person: Person,
        device: DeviceId,
        restore: u64,
        music: MusicLoginResult,
        notice: Notice,
    }
}
pub type SignInAnswer = SignedIn;

object! {
    Stamp {
        #[serde(serialize_with = "write_backend")]
        backend: BackendId,
        restore: u64,
        account: AccountId,
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

object! {
    SignOut {
        head: RequestHead,
        request_id: RequestId,
        // the design does not say how a note is offered. Null requires normal bearer
        // authentication. A note is a separate credential and is valid on this path only.
        revoke: Option<SignOutNote>,
    }
}
object! { ListDevices { head: RequestHead, after: Option<DeviceId> } }
object! { ForceOut { head: RequestHead, device: DeviceId, session: SessionId } }
object! { GetMusicLogin { head: RequestHead } }

object! {
    ChangePassword {
        head: RequestHead,
        request_id: RequestId,
        // §8.9 keeps this one call. The server proves the current password and
        // consumes its exact change grant when accepting the new one.
        current_password: Password,
        new_password: Password,
    }
}

object! {
    Device {
        id: DeviceId, name: Name, kind: DeviceKind,
        #[serde(serialize_with = "write_version")]
        app_version: Version,
        last_seen_ms: u64,
        session: Option<SessionId>,
        this_device: bool,
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
        let mut own = devices.iter().filter(|device| device.this_device);
        if own.next().is_some_and(|device| device.session.is_none()) || own.next().is_some() {
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

/// §8.8. C7 names the self-service changes. C9 can put its admin-specific
/// targets in the same generic request and grant-use shapes.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantChange {
    ChangePassword { account: AccountId },
    DeleteListeningData { account: AccountId },
}

// the design does not say how grants travel. Return a separate opaque secret,
// then send it with the exact named change. Its session and revision are checked by kept.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct GrantRequest<T = GrantChange> {
    pub head: RequestHead,
    pub change: T,
    pub password: Password,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct GrantedChange<T = GrantChange> {
    pub head: RequestHead,
    pub request_id: RequestId,
    pub change: T,
    pub grant: GrantSecret,
}

object! { GrantIssued { grant: GrantSecret } }

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotSignedIn {
    SessionEnded,
    DataPutBack,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NotAllowed {
    Denied,
    WrongLogin,
    AccountOff,
    NoSuchDevice,
    PasswordNotAllowed,
    Incompatible {
        #[serde(serialize_with = "write_speaks")]
        speaks: Speaks,
    },
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    Malformed,
    NotSignedIn(NotSignedIn),
    NotAllowed(NotAllowed),
    Busy { retry_after_seconds: Option<Wait> },
    NotDealtWith,
}

impl Refusal {
    /// the design does not say the status numbers. Use one status per answer kind.
    pub fn status(&self) -> u16 {
        match self {
            Self::Malformed => 400,
            Self::NotSignedIn(_) => 401,
            Self::NotAllowed(_) => 403,
            Self::Busy { .. } => 429,
            Self::NotDealtWith => 503,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Wait(u64);

impl Wait {
    pub fn new(seconds: u64) -> Option<Self> {
        (1..=60).contains(&seconds).then_some(Self(seconds))
    }

    pub fn seconds(self) -> u64 {
        self.0
    }
}

pub const DONE_STATUS: u16 = 200;

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply<T> {
    Done { result: T, receipt: Receipt },
    Refused(Refusal),
    // the design does not say how to stamp a reply without a trusted account.
    // Only refusals to an authenticated device may carry its receipt.
    RefusedWithReceipt(RefusedWithReceipt),
}

impl<T> Reply<T> {
    pub fn status(&self) -> u16 {
        match self {
            Self::Done { .. } => DONE_STATUS,
            Self::Refused(reason) => reason.status(),
            Self::RefusedWithReceipt(context) => context.reason.status(),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct RefusedWithReceipt {
    reason: Refusal,
    receipt: Receipt,
}

impl RefusedWithReceipt {
    pub fn new(reason: Refusal, receipt: Receipt) -> Option<Self> {
        if matches!(reason, Refusal::NotAllowed(NotAllowed::WrongLogin) | Refusal::NotSignedIn(_)) {
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
pub type MusicLoginReply = Reply<MusicLoginResult>;
pub type GrantReply = Reply<GrantIssued>;

pub fn read_reply<T>(status: u16, body: &[u8]) -> Result<Reply<T>, BadMessage>
where
    Reply<T>: wire::Message,
{
    let reply: Reply<T> = match status {
        DONE_STATUS => read(body)?,
        400 | 401 | 403 | 429 | 503 => wire::read_bounded(body, SMALL_REPLY_MAX_BYTES)?,
        _ => return Err(BadMessage::Status(status)),
    };
    if reply.status() != status {
        return Err(BadMessage::Status(status));
    }
    Ok(reply)
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
text_value!(OneTimeCode, OneTimeCode::new);
text_value!(MusicAddress, MusicAddress::new);
text_value!(Notice, Notice::new);
text_value!(DeviceKind, |text| match text {
    "pc" => Some(DeviceKind::Pc),
    "phone" => Some(DeviceKind::Phone),
    _ => None,
});

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
            ("code", raw) => {
                let fields = Fields::read(raw)?;
                Ok(Self::Code { code: fields.get("code")?, new_password: fields.get("new_password")? })
            }
            _ => Err(BadMessage::Form),
        }
    }
}

impl wire::Value for MusicLoginResult {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        if raw.get().starts_with('"') {
            return read_text(raw, |text| match text {
                "none_set_yet" => Some(Self::NoneSetYet),
                "unusable_now" => Some(Self::UnusableNow),
                _ => None,
            });
        }
        let (_, raw) = Fields::read(raw)?.select(&["login"])?;
        Ok(Self::Login(MusicLogin::read(raw)?))
    }
}

impl wire::Value for GrantChange {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        match Fields::read(raw)?.select(&["change_password", "delete_listening_data"])? {
            ("change_password", raw) => Ok(Self::ChangePassword { account: Fields::read(raw)?.get("account")? }),
            ("delete_listening_data", raw) => Ok(Self::DeleteListeningData { account: Fields::read(raw)?.get("account")? }),
            _ => Err(BadMessage::Form),
        }
    }
}

impl<T: wire::Value> wire::Value for GrantRequest<T> {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let fields = Fields::read(raw)?;
        Ok(Self { head: fields.get("head")?, change: fields.get("change")?, password: fields.get("password")? })
    }
}

impl<T: wire::Value> wire::Value for GrantedChange<T> {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let fields = Fields::read(raw)?;
        Ok(Self {
            head: fields.get("head")?, request_id: fields.get("request_id")?,
            change: fields.get("change")?, grant: fields.get("grant")?,
        })
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

impl wire::Value for NotSignedIn {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        read_text(raw, |text| match text {
            "session_ended" => Some(Self::SessionEnded),
            "data_put_back" => Some(Self::DataPutBack),
            _ => None,
        })
    }
}

impl wire::Value for NotAllowed {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        if raw.get().starts_with('"') {
            return read_text(raw, |text| match text {
                "denied" => Some(Self::Denied),
                "wrong_login" => Some(Self::WrongLogin),
                "account_off" => Some(Self::AccountOff),
                "no_such_device" => Some(Self::NoSuchDevice),
                "password_not_allowed" => Some(Self::PasswordNotAllowed),
                _ => None,
            });
        }
        let (_, raw) = Fields::read(raw)?.select(&["incompatible"])?;
        Ok(Self::Incompatible { speaks: Fields::read(raw)?.get("speaks")? })
    }
}

impl wire::Value for Refusal {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        if raw.get().starts_with('"') {
            return read_text(raw, |text| match text {
                "malformed" => Some(Self::Malformed),
                "not_dealt_with" => Some(Self::NotDealtWith),
                _ => None,
            });
        }
        match Fields::read(raw)?.select(&["not_signed_in", "not_allowed", "busy"])? {
            ("not_signed_in", raw) => Ok(Self::NotSignedIn(NotSignedIn::read(raw)?)),
            ("not_allowed", raw) => Ok(Self::NotAllowed(NotAllowed::read(raw)?)),
            ("busy", raw) => Ok(Self::Busy { retry_after_seconds: Fields::read(raw)?.get("retry_after_seconds")? }),
            _ => Err(BadMessage::Form),
        }
    }
}

pub(crate) trait ReplyResult: wire::Value + Serialize {
    const MAX_DONE_BYTES: usize = SMALL_REPLY_MAX_BYTES;

    fn agrees(&self, _: &Receipt) -> bool {
        true
    }
}

impl ReplyResult for SignedOut {}
impl ReplyResult for ForcedOut {}
impl ReplyResult for PasswordChanged {}
impl ReplyResult for GrantIssued {}

impl ReplyResult for DevicePage {
    const MAX_DONE_BYTES: usize = LIST_REPLY_MAX_BYTES;
}

impl ReplyResult for MusicLoginResult {
    fn agrees(&self, receipt: &Receipt) -> bool {
        match self {
            Self::Login(login) => login.count == receipt.music_login_count,
            Self::NoneSetYet | Self::UnusableNow => true,
        }
    }
}

impl ReplyResult for SignedIn {
    const MAX_DONE_BYTES: usize = SIGN_IN_REPLY_MAX_BYTES;

    fn agrees(&self, receipt: &Receipt) -> bool {
        self.person.id == receipt.stamp.account
            && self.restore == receipt.stamp.restore
            && self.session_id == self.sign_out.session
            && !same_bytes::<U32>(self.session.expose_bytes(), self.sign_out.secret.expose_bytes())
            && self.music.agrees(receipt)
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

macro_rules! request_bound {
    ($bytes:ident: $($kind:ty),+ $(,)?) => {
        $(impl wire::Message for $kind {
            const MAX_BYTES: usize = $bytes;
        })+
    };
}

request_bound!(PASSWORD_REQUEST_MAX_BYTES: SignIn, ChangePassword);
request_bound!(SMALL_REQUEST_MAX_BYTES: SignOut, ListDevices, ForceOut, GetMusicLogin);
request_bound!(SMALL_REPLY_MAX_BYTES: MusicLogin);

impl<T: wire::Value + Serialize> wire::Message for GrantRequest<T> {
    const MAX_BYTES: usize = PASSWORD_REQUEST_MAX_BYTES;
}

impl<T: wire::Value + Serialize> wire::Message for GrantedChange<T> {
    const MAX_BYTES: usize = GRANTED_CHANGE_MAX_BYTES;
}

impl<T: ReplyResult> wire::Message for Reply<T> {
    const MAX_BYTES: usize = T::MAX_DONE_BYTES;

    fn max_bytes(&self) -> usize {
        match self {
            Self::Done { .. } => T::MAX_DONE_BYTES,
            _ => SMALL_REPLY_MAX_BYTES,
        }
    }
}


// Password work uses one reserved worker per door. The server owns the worker,
// queue, and account reservations. No rate state contains a password or a code.
pub const OTHER_PASSWORDS_WAITING: usize = 8;
pub const PASSWORD_QUEUE_MS: u64 = 10_000;
pub const FIRST_PASSWORD_WAIT_MS: u64 = 1_000;
pub const MAX_PASSWORD_WAIT_MS: u64 = 60_000;
pub const PASSWORD_WAIT_MEMORY_MS: u64 = 60 * 60 * 1000;
pub const PASSWORD_WAIT_NAMES: usize = 10_000;
pub const CODES_PER_SECOND: usize = 10;
const SECOND_MS: u64 = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordDoor {
    KnownDevice,
    Other,
}

impl PasswordDoor {
    /// Select by the account's verified device secret, after checking its history.
    /// For a re-typed password (a grant or password change), use the authenticated
    /// session's device. A claimed ID or a device known to another account confers nothing.
    pub fn for_device(known_to_this_account: bool) -> Self {
        if known_to_this_account { Self::KnownDevice } else { Self::Other }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasswordLoad {
    /// The worker reserved for this door, including work whose client disconnected.
    pub worker_busy: bool,
    /// In-progress work for this name at this door.
    pub account_busy: bool,
    /// The number already waiting at this door.
    pub waiting: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordAdmission {
    Check,
    WaitForWorker,
    ComeBack(Wait),
    Busy,
}

impl PasswordAdmission {
    pub fn refusal(self) -> Option<Refusal> {
        match self {
            Self::Check | Self::WaitForWorker => None,
            Self::ComeBack(wait) => Some(Refusal::Busy { retry_after_seconds: Some(wait) }),
            Self::Busy => Some(Refusal::Busy { retry_after_seconds: None }),
        }
    }
}

/// Decide under this door's admission lock. Retain a worker's reservation until
/// the work finishes. The other door's queue and checks cannot reserve this worker.
pub fn admit_password_check(
    door: PasswordDoor,
    load: PasswordLoad,
    waits: &PasswordWaits,
    email: &Email,
    device: Option<DeviceId>,
    now_ms: u64,
) -> PasswordAdmission {
    if door == PasswordDoor::KnownDevice && device.is_none() {
        return PasswordAdmission::Busy;
    }
    // Backoff is a reply, never time spent in a worker queue.
    if let Some(wait) = waits.retry_after(door, email, device, now_ms) {
        return PasswordAdmission::ComeBack(wait);
    }
    match door {
        PasswordDoor::KnownDevice => {
            // the design does not say how much the known door holds while occupied.
            // Keep no queue there. Busy is a retry, and the other door has no effect.
            if load.worker_busy || load.account_busy {
                PasswordAdmission::Busy
            } else {
                PasswordAdmission::Check
            }
        }
        PasswordDoor::Other => {
            if !load.worker_busy && !load.account_busy && load.waiting == 0 {
                PasswordAdmission::Check
            } else if load.waiting < OTHER_PASSWORDS_WAITING {
                PasswordAdmission::WaitForWorker
            } else {
                PasswordAdmission::Busy
            }
        }
    }
}

/// Ask when the head of the other door's queue can start. Age is counted from
/// its original admission, including time before a worker becomes free.
pub fn resume_password_check(
    queued_at_ms: u64,
    worker_busy: bool,
    account_busy: bool,
    waits: &PasswordWaits,
    email: &Email,
    now_ms: u64,
) -> PasswordAdmission {
    if now_ms.saturating_sub(queued_at_ms) >= PASSWORD_QUEUE_MS {
        return PasswordAdmission::Busy;
    }
    admit_password_check(
        PasswordDoor::Other,
        PasswordLoad { worker_busy, account_busy, waiting: 0 },
        waits,
        email,
        None,
        now_ms,
    )
}

#[derive(Clone, Copy)]
struct AccountWait {
    last_wrong_ms: u64,
    delay_ms: u64,
}

/// The other door shares a wait by canonical email, including unknown names.
/// The known door has a separate wait for each account and authenticated device.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PasswordWaitKey {
    email: Email,
    device: Option<DeviceId>,
}

impl PasswordWaitKey {
    fn new(door: PasswordDoor, email: &Email, device: Option<DeviceId>) -> Option<Self> {
        // the design does not say how the known door waits. The lead ruled that
        // a wait there belongs to the individual device, including for re-typed passwords.
        let device = match door {
            PasswordDoor::KnownDevice => Some(device?),
            PasswordDoor::Other => None,
        };
        Some(Self { email: email.clone(), device })
    }
}

/// One in-memory table shared by the two password doors.
#[derive(Default)]
pub struct PasswordWaits {
    by_key: BTreeMap<PasswordWaitKey, AccountWait>,
    oldest: BTreeSet<(u64, PasswordWaitKey)>,
    latest_ms: u64,
}

impl PasswordWaits {
    pub fn retry_after(&self, door: PasswordDoor, email: &Email, device: Option<DeviceId>, now_ms: u64) -> Option<Wait> {
        let wait = self.by_key.get(&PasswordWaitKey::new(door, email, device)?)?;
        let elapsed = now_ms.max(self.latest_ms).saturating_sub(wait.last_wrong_ms);
        let remaining = wait.delay_ms.saturating_sub(elapsed);
        Wait::new(remaining.div_ceil(SECOND_MS))
    }

    pub fn expire(&mut self, now_ms: u64) {
        self.latest_ms = self.latest_ms.max(now_ms);
        while self.oldest.first().is_some_and(|(last, _)| self.latest_ms.saturating_sub(*last) >= PASSWORD_WAIT_MEMORY_MS) {
            if let Some((_, key)) = self.oldest.pop_first() {
                self.by_key.remove(&key);
            }
        }
    }

    /// Call after an actual completed wrong check, including a re-typed password
    /// or an unknown-email dummy check. Pass the same device as at admission.
    pub fn record_wrong(&mut self, door: PasswordDoor, email: &Email, device: Option<DeviceId>, now_ms: u64) {
        let Some(key) = PasswordWaitKey::new(door, email, device) else {
            return;
        };
        self.expire(now_ms);
        // the design does not say when a successful check resets the sequence.
        // Remember the last wrong check for the stated hour, or until eviction.
        let delay_ms = if let Some(prior) = self.by_key.get(&key) {
            self.oldest.remove(&(prior.last_wrong_ms, key.clone()));
            prior.delay_ms.saturating_mul(2).min(MAX_PASSWORD_WAIT_MS)
        } else {
            // the design does not say which tied key is oldest. Break ties by key.
            if self.by_key.len() >= PASSWORD_WAIT_NAMES {
                if let Some((_, oldest_key)) = self.oldest.pop_first() {
                    self.by_key.remove(&oldest_key);
                }
            }
            FIRST_PASSWORD_WAIT_MS
        };
        self.by_key.insert(key.clone(), AccountWait { last_wrong_ms: self.latest_ms, delay_ms });
        self.oldest.insert((self.latest_ms, key));
    }
}

/// The fast code check has its own rolling window. Issuing the new password
/// verifier after a valid code still uses the server's fixed password workers.
#[derive(Default)]
pub struct CodeLimit {
    seen: VecDeque<u64>,
    latest_ms: u64,
}

impl CodeLimit {
    pub fn admit(&mut self, now_ms: u64) -> Result<(), Wait> {
        self.latest_ms = self.latest_ms.max(now_ms);
        // the design does not say whether the second is fixed or rolling.
        while self.seen.front().is_some_and(|first| self.latest_ms.saturating_sub(*first) >= SECOND_MS) {
            self.seen.pop_front();
        }
        if self.seen.len() >= CODES_PER_SECOND {
            return Err(Wait(1));
        }
        self.seen.push_back(self.latest_ms);
        Ok(())
    }
}

pub const PASSWORD_SALT_BYTES: usize = 16;

/// A key from the backend used only to derive the stable dummy salt for an
/// unknown canonical email. The server still does the same slow Argon2id check.
pub struct DummySaltKey(Zeroizing<[u8; SECRET_BYTES]>);

secret_debug!(DummySaltKey);

impl DummySaltKey {
    pub fn from_bytes(key: [u8; SECRET_BYTES]) -> Self {
        Self(Zeroizing::new(key))
    }

    pub fn salt_for(&self, email: &Email) -> Zeroizing<[u8; PASSWORD_SALT_BYTES]> {
        // the design does not say the keyed function. Use a domain-separated
        // HMAC-SHA-256, taking the first 16 bytes for the Argon2id salt.
        let mut mac: Hmac<Sha256> = hkdf::HmacImpl::new_from_slice(self.0.as_slice());
        mac.update(b"Nexus Music dummy password salt v1\0");
        mac.update(email.as_str().as_bytes());
        let digest = mac.finalize();
        let mut salt = Zeroizing::new([0; PASSWORD_SALT_BYTES]);
        for (out, byte) in salt.iter_mut().zip(digest.as_bytes()) {
            *out = *byte;
        }
        salt
    }
}

#[cfg(test)]
mod tests;
