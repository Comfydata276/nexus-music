//! Step 1 administration calls and the rules for password-backed grants.
//! Time and random bytes come from the caller. The backend authenticates the
//! session, checks the password and applies each mutation under its write lock.

use super::account::{
    self, AccountId, DeviceId, DeviceKind, Email, GrantRequest, GrantedChange, MusicAddress,
    Name, NotSignedIn, OneTimeCode, RequestHead, RequestId, SessionId, Wait,
};
pub use super::account::{GrantIssued, GrantSecret};
use super::proto::Version;
use super::wire::{self, Fields, object, plain_json, read_text, write_version};
pub use super::wire::{BadMessage, WireBody, encode, read};
use serde::{Serialize, Serializer};
use serde_json::value::RawValue;
use zeroize::Zeroizing;

pub const PAGE: &str = "/nexus/admin/";
pub const LIST_ACCOUNTS: &str = "/nexus/api/admin/accounts";
pub const CREATE_ACCOUNT: &str = "/nexus/api/admin/create-account";
pub const SET_ACCOUNT_ENABLED: &str = "/nexus/api/admin/set-account-enabled";
pub const SET_ADMIN: &str = "/nexus/api/admin/set-admin";
pub const DELETE_ACCOUNT: &str = "/nexus/api/admin/delete-account";
pub const RESET_PASSWORD: &str = "/nexus/api/admin/reset-password";
pub const GET_MUSIC_SERVER: &str = "/nexus/api/admin/music-server";
pub const SET_MUSIC_SERVER: &str = "/nexus/api/admin/set-music-server";
pub const SET_MUSIC_LOGIN: &str = "/nexus/api/admin/set-music-login";
pub const LIST_DEVICES: &str = "/nexus/api/admin/devices";
pub const FORCE_OUT: &str = "/nexus/api/admin/force-out";
pub const GET_DIAGNOSTICS_DEFAULT: &str = "/nexus/api/admin/diagnostics-default";
pub const SET_DIAGNOSTICS_DEFAULT: &str = "/nexus/api/admin/set-diagnostics-default";
pub const SET_REQUESTS_ENABLED: &str = "/nexus/api/admin/set-requests-enabled";
pub const REQUEST_GRANT: &str = "/nexus/api/admin/grant";

// the design does not say the POST spellings. The document alone takes GET.
pub const POST_PATHS: &[&str] = &[
    LIST_ACCOUNTS, CREATE_ACCOUNT, SET_ACCOUNT_ENABLED, SET_ADMIN, DELETE_ACCOUNT,
    RESET_PASSWORD, GET_MUSIC_SERVER, SET_MUSIC_SERVER, SET_MUSIC_LOGIN, LIST_DEVICES,
    FORCE_OUT, GET_DIAGNOSTICS_DEFAULT, SET_DIAGNOSTICS_DEFAULT, SET_REQUESTS_ENABLED,
    REQUEST_GRANT,
];

// §7.2 bounds password-bearing calls at 4 KiB. Use that ceiling for all admin
// requests. §7.1 gives list replies their larger ceiling.
pub const REQUEST_MAX_BYTES: usize = account::PASSWORD_REQUEST_MAX_BYTES;
pub const REPLY_MAX_BYTES: usize = account::SMALL_REPLY_MAX_BYTES;
pub const LIST_REPLY_MAX_BYTES: usize = account::LIST_REPLY_MAX_BYTES;
pub const ACCOUNTS_PER_PAGE: usize = 500;
pub const DEVICES_PER_PAGE: usize = 500;
pub const GRANT_LIFETIME_MS: u64 = account::PASSWORD_LEAVE_MS;
pub const REQUESTS_DEFAULT_ENABLED: bool = true;
pub const DIAGNOSTICS_DEFAULT_ENABLED: bool = true;
pub const PAGE_SESSION_IDLE_MS: u64 = 30 * 60 * 1000;

// the design does not say the typed-in music password's bounds. Permit existing
// short passwords and spaces. Bound the input to 512 UTF-8 bytes and reject controls.
pub const MUSIC_PASSWORD_MAX_BYTES: usize = 512;

pub struct MusicPassword(Zeroizing<String>);

impl MusicPassword {
    pub fn new(text: &str) -> Option<Self> {
        (!text.is_empty()
            && text.len() <= MUSIC_PASSWORD_MAX_BYTES
            && !text.chars().any(char::is_control))
        .then(|| Self(Zeroizing::new(text.to_owned())))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for MusicPassword {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MusicPassword(..)")
    }
}

impl PartialEq for MusicPassword {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_bytes() == other.0.as_bytes()
    }
}

impl Eq for MusicPassword {}

impl Serialize for MusicPassword {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.expose())
    }
}

impl wire::Value for MusicPassword {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        read_text(raw, Self::new)
    }
}

// the design does not say the internal address's ceiling. Accept HTTP for the
// container's route, with the same credential-free syntax as the public address.
pub const INTERNAL_ADDRESS_MAX_BYTES: usize = 1024;

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct InternalAddress(String);

impl InternalAddress {
    pub fn new(text: &str) -> Option<Self> {
        if text.len() > INTERNAL_ADDRESS_MAX_BYTES {
            return None;
        }
        match text.strip_prefix("http://") {
            Some(tail) => MusicAddress::new(&format!("https://{tail}"))?,
            None => MusicAddress::new(text)?,
        };
        Some(Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl wire::Value for InternalAddress {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        read_text(raw, Self::new)
    }
}

object! {
    Account {
        id: AccountId,
        email: Email,
        name: Name,
        admin: bool,
        enabled: bool,
        requests_enabled: bool,
    }
}

object! { ListAccounts { head: RequestHead, after: Option<AccountId> } }

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct DeviceCursor {
    pub account: AccountId,
    pub device: DeviceId,
}

impl wire::Value for DeviceCursor {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let fields = Fields::read(raw)?;
        Ok(Self { account: fields.get("account")?, device: fields.get("device")? })
    }
}

object! {
    AdminDevice {
        account: AccountId,
        device: DeviceId,
        name: Name,
        kind: DeviceKind,
        #[serde(serialize_with = "write_version")]
        app_version: Version,
        last_seen_ms: u64,
        session: Option<SessionId>,
    }
}

object! { ListDevices { head: RequestHead, after: Option<DeviceCursor> } }

/// the design does not say how rows advance. Request strictly after the final
/// key from the previous page. Empty pages end the listing.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Page<T> {
    rows: Vec<T>,
    more: bool,
}

pub trait Row {
    type Cursor: Ord;
    const LIMIT: usize;
    fn cursor(&self) -> Self::Cursor;
}

impl Row for Account {
    type Cursor = AccountId;
    const LIMIT: usize = ACCOUNTS_PER_PAGE;

    fn cursor(&self) -> AccountId {
        self.id
    }
}

impl Row for AdminDevice {
    type Cursor = DeviceCursor;
    const LIMIT: usize = DEVICES_PER_PAGE;

    fn cursor(&self) -> DeviceCursor {
        DeviceCursor { account: self.account, device: self.device }
    }
}

impl<T: Row> Page<T> {
    pub fn new(rows: Vec<T>, more: bool) -> Option<Self> {
        if rows.len() > T::LIMIT
            || (more && rows.is_empty())
            || !rows.iter().zip(rows.iter().skip(1)).all(|(a, b)| a.cursor() < b.cursor())
        {
            return None;
        }
        Some(Self { rows, more })
    }

    pub fn rows(&self) -> &[T] {
        &self.rows
    }

    pub fn next(&self) -> Option<T::Cursor> {
        self.rows.last().filter(|_| self.more).map(Row::cursor)
    }
}

impl<T: Row + wire::Value> wire::Value for Page<T> {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let fields = Fields::read(raw)?;
        // Borrow row slices and check the count before constructing any typed rows.
        let rows: Vec<T> = fields.get::<Rows<T>>("rows")?.0;
        Self::new(rows, fields.get("more")?).ok_or(BadMessage::Form)
    }
}

struct Rows<T>(Vec<T>);

impl<T: Row + wire::Value> wire::Value for Rows<T> {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let rows: Vec<&RawValue> = plain_json(raw.get())?;
        if rows.len() > T::LIMIT {
            return Err(BadMessage::Form);
        }
        rows.into_iter().map(T::read).collect::<Result<_, _>>().map(Self)
    }
}

pub type Accounts = Page<Account>;
pub type Devices = Page<AdminDevice>;

object! { GetMusicServer { head: RequestHead } }
object! { MusicServer { internal: InternalAddress, public: Option<MusicAddress> } }
object! { GetDiagnosticsDefault { head: RequestHead } }
object! { DiagnosticsDefault { enabled: bool } }

object! { NewAccount { email: Email, name: Name } }
object! { AccountEnabled { account: AccountId, enabled: bool } }
object! { AdminRole { account: AccountId, admin: bool } }
object! { AccountTarget { account: AccountId } }
object! { MusicServerAddress { internal: InternalAddress, public: MusicAddress } }
object! { TypedMusicLogin { account: AccountId, name: Name, password: MusicPassword } }
object! { SessionTarget { account: AccountId, device: DeviceId, session: SessionId } }
object! { RequestsEnabled { account: AccountId, enabled: bool } }
object! { Applied {} }
object! { MusicServerSetting {} }

/// the design does not say how a code-bearing reply is retried without showing
/// the code twice. Persist only the result with the code set to AlreadyShown.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeDelivery {
    Issued(OneTimeCode),
    AlreadyShown,
}

impl CodeDelivery {
    pub fn take(&mut self) -> Option<OneTimeCode> {
        match std::mem::replace(self, Self::AlreadyShown) {
            Self::Issued(code) => Some(code),
            Self::AlreadyShown => None,
        }
    }
}

impl wire::Value for CodeDelivery {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        if raw.get().starts_with('"') {
            return read_text(raw, |s| (s == "already_shown").then_some(Self::AlreadyShown));
        }
        let (_, code) = Fields::read(raw)?.select(&["issued"])?;
        OneTimeCode::read(code).map(Self::Issued)
    }
}

object! { AccountCreated { account: AccountId, code: CodeDelivery } }
object! { PasswordReset { code: CodeDelivery } }

/// The operation and target the password check authorizes. Changing the
/// session, target, operation, or request number needs another grant.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    CreateAccount(Email),
    SetAccountEnabled(AccountEnabled),
    SetAdmin(AdminRole),
    DeleteAccount(AccountId),
    ResetPassword(AccountId),
    SetMusicServer(MusicServerSetting),
    SetMusicLogin(AccountId),
    ForceOut(SessionTarget),
    SetDiagnosticsDefault(DiagnosticsDefault),
    SetRequestsEnabled(RequestsEnabled),
}

impl wire::Value for Action {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        let (action, value) = Fields::read(raw)?.select(&[
            "create_account", "set_account_enabled", "set_admin", "delete_account",
            "reset_password", "set_music_server", "set_music_login", "force_out",
            "set_diagnostics_default", "set_requests_enabled",
        ])?;
        match action {
            "create_account" => Email::read(value).map(Self::CreateAccount),
            "set_account_enabled" => AccountEnabled::read(value).map(Self::SetAccountEnabled),
            "set_admin" => AdminRole::read(value).map(Self::SetAdmin),
            "delete_account" => AccountId::read(value).map(Self::DeleteAccount),
            "reset_password" => AccountId::read(value).map(Self::ResetPassword),
            "set_music_server" => MusicServerSetting::read(value).map(Self::SetMusicServer),
            "set_music_login" => AccountId::read(value).map(Self::SetMusicLogin),
            "force_out" => SessionTarget::read(value).map(Self::ForceOut),
            "set_diagnostics_default" => DiagnosticsDefault::read(value).map(Self::SetDiagnosticsDefault),
            "set_requests_enabled" => RequestsEnabled::read(value).map(Self::SetRequestsEnabled),
            _ => Err(BadMessage::Form),
        }
    }
}

// the design does not say how the grant and idempotency number are related.
// Name the number alongside the action in C7's common grant envelope.
object! { NamedChange { request_id: RequestId, action: Action } }

pub type RequestGrant = GrantRequest<NamedChange>;

/// All fields are from authenticated backend state, checked again at issuance
/// and at spend. The caller supplies time under the store's nondecreasing clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantContext {
    pub restore: u64,
    pub account: AccountId,
    pub revision: u64,
    pub session: SessionId,
    pub enabled: bool,
    pub admin: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantError {
    NotAllowed,
    Changed,
    Expired,
    WrongChange,
    Used,
}

#[derive(Debug)]
pub struct Grant {
    context: GrantContext,
    issued_ms: u64,
    request_id: RequestId,
    action: Action,
    used: bool,
}

fn fresh(at: u64, now: u64) -> bool {
    // the design does not say what a backwards clock or the exact end of the
    // window means. Refuse both. Subtract to avoid an overflowing deadline.
    now.checked_sub(at).is_some_and(|age| age < GRANT_LIFETIME_MS)
}

impl Grant {
    /// Call after a successful password check. Pass the context captured before
    /// the check, and the current context inside the write transaction.
    pub fn issue(
        checked: GrantContext,
        current: GrantContext,
        checked_ms: u64,
        now_ms: u64,
        request_id: RequestId,
        action: Action,
    ) -> Result<Self, GrantError> {
        if !current.enabled || !current.admin {
            return Err(GrantError::NotAllowed);
        }
        if checked != current {
            return Err(GrantError::Changed);
        }
        if !fresh(checked_ms, now_ms) {
            return Err(GrantError::Expired);
        }
        // the design does not say whether slow issuance extends the freshness of
        // the proof. Keep the earlier deadline, ten minutes from the check.
        Ok(Self { context: current, issued_ms: checked_ms, request_id, action, used: false })
    }

    /// Do this in the same serialized transaction as the mutation, after
    /// checking for a matching idempotency record. The backend uses the supplied
    /// token to find this record and commits the spend with the mutation.
    pub fn spend(&mut self, current: GrantContext, now_ms: u64, request_id: RequestId, action: &Action) -> Result<(), GrantError> {
        if self.used {
            return Err(GrantError::Used);
        }
        if !current.enabled || !current.admin {
            return Err(GrantError::NotAllowed);
        }
        if current != self.context {
            return Err(GrantError::Changed);
        }
        if !fresh(self.issued_ms, now_ms) {
            return Err(GrantError::Expired);
        }
        if request_id != self.request_id || *action != self.action {
            return Err(GrantError::WrongChange);
        }
        self.used = true;
        Ok(())
    }
}

pub trait Change {
    fn action(&self) -> Action;
}

macro_rules! change {
    ($kind:ty, $value:ident => $action:expr) => {
        impl Change for $kind {
            fn action(&self) -> Action {
                let $value = self;
                $action
            }
        }
    };
}

change!(NewAccount, v => Action::CreateAccount(v.email.clone()));
change!(AccountEnabled, v => Action::SetAccountEnabled(AccountEnabled { account: v.account, enabled: v.enabled }));
change!(AdminRole, v => Action::SetAdmin(AdminRole { account: v.account, admin: v.admin }));
impl Change for MusicServerAddress {
    fn action(&self) -> Action {
        Action::SetMusicServer(MusicServerSetting {})
    }
}
change!(TypedMusicLogin, v => Action::SetMusicLogin(v.account));
change!(SessionTarget, v => Action::ForceOut(SessionTarget { account: v.account, device: v.device, session: v.session }));
change!(DiagnosticsDefault, v => Action::SetDiagnosticsDefault(DiagnosticsDefault { enabled: v.enabled }));
change!(RequestsEnabled, v => Action::SetRequestsEnabled(RequestsEnabled { account: v.account, enabled: v.enabled }));

object! { DeleteTarget { account: AccountId } }
change!(DeleteTarget, v => Action::DeleteAccount(v.account));
change!(AccountTarget, v => Action::ResetPassword(v.account));

/// Mutations use C7's grant envelope with a typed admin payload.
pub type Granted<T> = GrantedChange<T>;

pub type CreateAccount = Granted<NewAccount>;
pub type SetAccountEnabled = Granted<AccountEnabled>;
pub type SetAdmin = Granted<AdminRole>;
pub type DeleteAccount = Granted<DeleteTarget>;
pub type ResetPassword = Granted<AccountTarget>;
pub type SetMusicServer = Granted<MusicServerAddress>;
pub type SetMusicLogin = Granted<TypedMusicLogin>;
pub type ForceOut = Granted<SessionTarget>;
pub type SetDiagnosticsDefault = Granted<DiagnosticsDefault>;
pub type SetRequestsEnabled = Granted<RequestsEnabled>;

/// the design does not say each admin denial's tag. Use fixed reasons and
/// C7's response status classes.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Denial {
    Denied,
    WrongPassword,
    GrantRequired,
    NoSuchAccount,
    NoSuchSession,
    MusicLoginRefused,
    Conflict,
}

impl wire::Value for Denial {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        read_text(raw, |tag| match tag {
            "denied" => Some(Self::Denied),
            "wrong_password" => Some(Self::WrongPassword),
            "grant_required" => Some(Self::GrantRequired),
            "no_such_account" => Some(Self::NoSuchAccount),
            "no_such_session" => Some(Self::NoSuchSession),
            "music_login_refused" => Some(Self::MusicLoginRefused),
            "conflict" => Some(Self::Conflict),
            _ => None,
        })
    }
}

#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    Malformed,
    NotSignedIn(NotSignedIn),
    NotAllowed(Denial),
    Busy { retry_after_seconds: Option<Wait> },
    NotDealtWith,
}

impl Refusal {
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

impl wire::Value for Refusal {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        if raw.get().starts_with('"') {
            return read_text(raw, |tag| match tag {
                "malformed" => Some(Self::Malformed),
                "not_dealt_with" => Some(Self::NotDealtWith),
                _ => None,
            });
        }
        match Fields::read(raw)?.select(&["not_signed_in", "not_allowed", "busy"])? {
            ("not_signed_in", raw) => NotSignedIn::read(raw).map(Self::NotSignedIn),
            ("not_allowed", raw) => Denial::read(raw).map(Self::NotAllowed),
            ("busy", raw) => Ok(Self::Busy { retry_after_seconds: Fields::read(raw)?.get("retry_after_seconds")? }),
            _ => Err(BadMessage::Form),
        }
    }
}

/// Admin answers carry no device receipt, which is for synchronizing a device's
/// own kept data. The only credentials returned are a newly issued code or grant.
#[derive(Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reply<T> {
    Done(T),
    Refused(Refusal),
}

impl<T> Reply<T> {
    pub fn status(&self) -> u16 {
        match self {
            Self::Done(_) => 200,
            Self::Refused(reason) => reason.status(),
        }
    }
}

impl<T: wire::Value> wire::Value for Reply<T> {
    fn read(raw: &RawValue) -> Result<Self, BadMessage> {
        match Fields::read(raw)?.select(&["done", "refused"])? {
            ("done", raw) => T::read(raw).map(Self::Done),
            ("refused", raw) => Refusal::read(raw).map(Self::Refused),
            _ => Err(BadMessage::Form),
        }
    }
}

macro_rules! read_requests {
    ($($kind:ty),* $(,)?) => {
        $(impl wire::Message for $kind {
            const MAX_BYTES: usize = account::SMALL_REQUEST_MAX_BYTES;
        })*
    };
}

read_requests!(ListAccounts, ListDevices, GetMusicServer, GetDiagnosticsDefault);

pub trait ResultBody: wire::Value + Serialize {
    const MAX_BYTES: usize = REPLY_MAX_BYTES;
}
impl ResultBody for Applied {}
impl ResultBody for AccountCreated {}
impl ResultBody for PasswordReset {}
impl ResultBody for Accounts {
    const MAX_BYTES: usize = LIST_REPLY_MAX_BYTES;
}
impl ResultBody for Devices {
    const MAX_BYTES: usize = LIST_REPLY_MAX_BYTES;
}
impl ResultBody for MusicServer {}
impl ResultBody for DiagnosticsDefault {}
impl ResultBody for GrantIssued {}

impl<T: ResultBody> wire::Message for Reply<T> {
    const MAX_BYTES: usize = T::MAX_BYTES;

    fn max_bytes(&self) -> usize {
        match self {
            Self::Done(_) => T::MAX_BYTES,
            Self::Refused(_) => REPLY_MAX_BYTES,
        }
    }
}

pub fn read_reply<T: ResultBody>(status: u16, body: &[u8]) -> Result<Reply<T>, BadMessage> {
    let reply: Reply<T> = match status {
        account::DONE_STATUS => read(body)?,
        400 | 401 | 403 | 429 | 503 => wire::read_bounded(body, REPLY_MAX_BYTES)?,
        _ => return Err(BadMessage::Status(status)),
    };
    if status != reply.status() {
        return Err(BadMessage::Status(status));
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::account::Password;
    use super::super::proto::{self, SPEAKS};

    fn round_trip<T: wire::Message + std::fmt::Debug + PartialEq>(message: T) {
        assert_eq!(read::<T>(encode(&message).unwrap().bytes()).unwrap(), message);
    }

    fn head() -> RequestHead {
        RequestHead { speaks: SPEAKS, restore: 1 }
    }

    fn granted<T>(change: T) -> Granted<T> {
        GrantedChange {
            head: head(),
            request_id: RequestId::new(41).unwrap(),
            grant: GrantSecret::from_bytes([3; account::SECRET_BYTES]),
            change,
        }
    }

    fn account_id() -> AccountId {
        AccountId::new(7).unwrap()
    }

    fn email() -> Email {
        Email::new(" SAMPLE@example.test ").unwrap()
    }

    fn internal() -> InternalAddress {
        InternalAddress::new("http://music.test:4533/").unwrap()
    }

    fn public() -> MusicAddress {
        MusicAddress::new("https://music.example.test/").unwrap()
    }

    fn session_target() -> SessionTarget {
        SessionTarget {
            account: account_id(),
            device: DeviceId::new(26).unwrap(),
            session: SessionId::new(28).unwrap(),
        }
    }

    #[test]
    fn routes_are_unique_and_plain() {
        assert_eq!(proto::plain(PAGE), Ok(()));
        let mut seen = std::collections::BTreeSet::new();
        for path in POST_PATHS {
            assert_eq!(proto::plain(path), Ok(()));
            assert!(path.starts_with(proto::API));
            assert!(seen.insert(*path));
        }
    }

    #[test]
    fn every_request_and_grant_action_round_trips() {
        round_trip(ListAccounts { head: head(), after: Some(account_id()) });
        round_trip(ListDevices {
            head: head(),
            after: Some(DeviceCursor { account: account_id(), device: session_target().device }),
        });
        round_trip(GetMusicServer { head: head() });
        round_trip(GetDiagnosticsDefault { head: head() });

        fn write_change<T>(change: T)
        where
            T: Change + wire::Value + Serialize + std::fmt::Debug + PartialEq,
        {
            let request = granted(change);
            round_trip(RequestGrant {
                head: head(),
                change: NamedChange { request_id: request.request_id, action: request.change.action() },
                password: Password::new("example admin password").unwrap(),
            });
            round_trip(request);
        }

        write_change(NewAccount { email: email(), name: Name::new("Example").unwrap() });
        write_change(AccountEnabled { account: account_id(), enabled: false });
        write_change(AdminRole { account: account_id(), admin: true });
        write_change(DeleteTarget { account: account_id() });
        write_change(AccountTarget { account: account_id() });
        write_change(MusicServerAddress { internal: internal(), public: public() });
        write_change(TypedMusicLogin {
            account: account_id(),
            name: Name::new("sample music user").unwrap(),
            password: MusicPassword::new("A💜\\\"").unwrap(),
        });
        write_change(session_target());
        write_change(DiagnosticsDefault { enabled: true });
        write_change(RequestsEnabled { account: account_id(), enabled: false });
    }

    #[test]
    fn every_reply_shape_round_trips_and_status_must_agree() {
        round_trip(Reply::Done(Applied {}));
        round_trip(Reply::Done(GrantIssued { grant: GrantSecret::from_bytes([4; account::SECRET_BYTES]) }));
        round_trip(Reply::Done(MusicServer { internal: internal(), public: Some(public()) }));
        round_trip(Reply::Done(DiagnosticsDefault { enabled: true }));
        let mut code = CodeDelivery::Issued(OneTimeCode::new("CDFHJKMNPRVW").unwrap());
        assert!(code.take().is_some());
        assert!(code.take().is_none());
        round_trip(Reply::Done(AccountCreated { account: account_id(), code }));
        round_trip(Reply::Done(PasswordReset {
            code: CodeDelivery::Issued(OneTimeCode::new("CDFHJKMNPRVW").unwrap()),
        }));

        for reason in [
            Refusal::Malformed,
            Refusal::NotSignedIn(NotSignedIn::SessionEnded),
            Refusal::NotSignedIn(NotSignedIn::DataPutBack),
            Refusal::NotAllowed(Denial::Denied),
            Refusal::NotAllowed(Denial::WrongPassword),
            Refusal::NotAllowed(Denial::GrantRequired),
            Refusal::NotAllowed(Denial::NoSuchAccount),
            Refusal::NotAllowed(Denial::NoSuchSession),
            Refusal::NotAllowed(Denial::MusicLoginRefused),
            Refusal::NotAllowed(Denial::Conflict),
            Refusal::Busy { retry_after_seconds: Wait::new(1) },
            Refusal::Busy { retry_after_seconds: None },
            Refusal::NotDealtWith,
        ] {
            let reply = Reply::<Applied>::Refused(reason);
            let bytes = encode(&reply).unwrap();
            assert_eq!(read_reply(reply.status(), bytes.bytes()), Ok(reply));
            assert_eq!(read_reply::<Applied>(200, bytes.bytes()), Err(BadMessage::Status(200)));
        }
    }

    fn account_row(id: usize) -> Account {
        let email = format!("{}@x", "\"".repeat(account::EMAIL_MAX_BYTES - 2));
        Account {
            id: AccountId::new(u64::MAX - id as u64).unwrap(),
            email: Email::new(&email).unwrap(),
            name: Name::new(&"\"".repeat(account::NAME_MAX_BYTES)).unwrap(),
            admin: true, enabled: false, requests_enabled: false,
        }
    }

    fn device_row(id: usize, signed_in: bool) -> AdminDevice {
        AdminDevice {
            account: AccountId::new(u64::MAX).unwrap(),
            device: DeviceId::new(u64::MAX - id as u64).unwrap(),
            name: Name::new(&"\"".repeat(account::NAME_MAX_BYTES)).unwrap(),
            kind: DeviceKind::Phone,
            app_version: Version { major: u32::MAX, minor: u32::MAX, patch: u32::MAX },
            last_seen_ms: u64::MAX,
            session: signed_in.then(|| SessionId::new(u64::MAX).unwrap()),
        }
    }

    #[test]
    fn paged_replies_bound_rows_and_serialized_size_and_always_advance() {
        assert!(Accounts::new(Vec::new(), true).is_none());
        assert!(Accounts::new(vec![account_row(0), account_row(1)], false).is_none());
        assert!(Accounts::new(vec![account_row(0), account_row(0)], false).is_none());
        assert!(Accounts::new((0..=ACCOUNTS_PER_PAGE).rev().map(account_row).collect(), false).is_none());
        assert!(Devices::new((0..=DEVICES_PER_PAGE).rev().map(|id| device_row(id, true)).collect(), false).is_none());
        round_trip(Reply::Done(Devices::new(vec![device_row(0, false)], false).unwrap()));

        let accounts = Accounts::new((0..ACCOUNTS_PER_PAGE).rev().map(account_row).collect(), true).unwrap();
        assert_eq!(accounts.next(), AccountId::new(u64::MAX));
        round_trip(Reply::Done(accounts));

        let devices = Devices::new((0..DEVICES_PER_PAGE).rev().map(|id| device_row(id, true)).collect(), false).unwrap();
        assert_eq!(devices.next(), None);
        round_trip(Reply::Done(devices));
    }

    #[test]
    fn wire_rejects_bad_whole_json_and_enforces_field_bounds() {
        let valid = r#"{"head":{"speaks":[1,1],"restore":0},"after":null}"#;
        assert!(read::<ListAccounts>(valid.as_bytes()).is_ok());
        assert!(read::<ListAccounts>(valid[..valid.len() - 1].as_bytes()).is_err());
        let unknown = r#"{"head":{"speaks":[1,1],"restore":0},"after":null,"extra":{"value":[]}}"#;
        assert!(read::<ListAccounts>(unknown.as_bytes()).is_ok());
        let duplicated = r#"{"head":{"speaks":[1,1],"restore":0},"after":null,"extra":{"value":0,"\u0076alue":1}}"#;
        assert!(read::<ListAccounts>(duplicated.as_bytes()).is_err());
        let nested = format!(r#"{{"head":{{"speaks":[1,1],"restore":0}},"after":null,"extra":{}}}"#,
            format!("{}0{}", "[".repeat(proto::HELLO_MAX_DEPTH), "]".repeat(proto::HELLO_MAX_DEPTH)));
        assert!(read::<ListAccounts>(nested.as_bytes()).is_err());
        assert!(read::<ListAccounts>(&[0xff]).is_err());
        assert_eq!(read::<ListAccounts>(&vec![b' '; REQUEST_MAX_BYTES + 1]), Err(BadMessage::TooLarge));
        assert_eq!(read::<Reply<Applied>>(&vec![b' '; REPLY_MAX_BYTES + 1]), Err(BadMessage::TooLarge));
        assert!(read::<ListAccounts>(valid.replace("null", "0").as_bytes()).is_err());
        assert!(GrantSecret::from_hex(&"AA".repeat(account::SECRET_BYTES)).is_none());
        assert!(GrantSecret::from_hex("abcd").is_none());
        assert!(MusicPassword::new("").is_none());
        assert!(MusicPassword::new("line\nbreak").is_none());
        assert!(MusicPassword::new(&"x".repeat(MUSIC_PASSWORD_MAX_BYTES + 1)).is_none());
        assert!(InternalAddress::new(&format!("http://music.test/{}", "a".repeat(INTERNAL_ADDRESS_MAX_BYTES))).is_none());
        assert!(InternalAddress::new("http://admin:password@music.test").is_none());
        assert!(MusicAddress::new("http://music.example.test").is_none());
    }

    #[test]
    fn a_list_can_exceed_four_kib_but_its_refusals_cannot() {
        let list = Accounts::new((0..ACCOUNTS_PER_PAGE).rev().map(account_row).collect(), true)
            .unwrap();
        let list = Reply::Done(list);
        let body = encode(&list).unwrap();
        assert!(body.bytes().len() > REPLY_MAX_BYTES);
        assert_eq!(read_reply::<Accounts>(200, body.bytes()), Ok(list));

        let refusal = format!(r#"{{"refused":{{"busy":{{"retry_after_seconds":null}}}},"extra":"{}"}}"#, "x".repeat(REPLY_MAX_BYTES));
        assert_eq!(read::<Reply<Accounts>>(refusal.as_bytes()), Err(BadMessage::TooLarge));
        assert_eq!(read_reply::<Accounts>(429, refusal.as_bytes()), Err(BadMessage::TooLarge));
    }

    #[test]
    fn the_largest_grant_request_and_music_setter_fit_their_bounds() {
        let email = format!("{}@x", "\"".repeat(account::EMAIL_MAX_BYTES - 2));
        let grant = RequestGrant {
            head: head(),
            change: NamedChange {
                request_id: RequestId::new(u64::MAX).unwrap(),
                action: Action::CreateAccount(Email::new(&email).unwrap()),
            },
            password: Password::new(&"\0".repeat(account::PASSWORD_MAX_CHARS)).unwrap(),
        };
        round_trip(grant);

        let internal_prefix = "http://music.test/";
        let public_prefix = "https://music.example.test/";
        let internal = format!("{internal_prefix}{}", "a".repeat(INTERNAL_ADDRESS_MAX_BYTES - internal_prefix.len()));
        let public = format!("{public_prefix}{}", "a".repeat(account::MUSIC_ADDRESS_MAX_BYTES - public_prefix.len()));
        let request = granted(MusicServerAddress {
            internal: InternalAddress::new(&internal).unwrap(),
            public: MusicAddress::new(&public).unwrap(),
        });
        assert!(encode(&request).unwrap().bytes().len() > account::SMALL_REQUEST_MAX_BYTES);
        round_trip(request);
    }

    #[test]
    fn grants_require_recent_password_and_unchanged_active_admin_context() {
        let context = GrantContext {
            restore: 2, account: account_id(), revision: 10, session: session_target().session,
            enabled: true, admin: true,
        };
        let request = granted(session_target());
        let issue = |checked, current, at, now| {
            Grant::issue(checked, current, at, now, request.request_id, request.change.action())
        };
        let at = 1000;
        assert!(matches!(issue(context, context, at, at - 1), Err(GrantError::Expired)));
        assert!(matches!(issue(context, context, at, at + GRANT_LIFETIME_MS), Err(GrantError::Expired)));
        let mut grant = issue(context, context, at, at + 20).unwrap();
        let spend = |grant: &mut Grant, current, now| {
            grant.spend(current, now, request.request_id, &request.change.action())
        };

        for wrong in [
            GrantContext { restore: 3, ..context },
            GrantContext { account: AccountId::new(1).unwrap(), ..context },
            GrantContext { revision: 11, ..context },
            GrantContext { session: SessionId::new(1).unwrap(), ..context },
        ] {
            assert!(matches!(issue(context, wrong, at, at), Err(GrantError::Changed)));
            assert_eq!(spend(&mut grant, wrong, at), Err(GrantError::Changed));
        }
        for inactive in [
            GrantContext { enabled: false, ..context },
            GrantContext { admin: false, ..context },
        ] {
            assert!(matches!(issue(inactive, inactive, at, at), Err(GrantError::NotAllowed)));
            assert_eq!(spend(&mut grant, inactive, at), Err(GrantError::NotAllowed));
        }
        assert_eq!(spend(&mut grant, context, at + GRANT_LIFETIME_MS), Err(GrantError::Expired));
        assert_eq!(spend(&mut grant, context, at - 1), Err(GrantError::Expired));
        let mut new_session = session_target();
        new_session.session = SessionId::new(123).unwrap();
        assert_eq!(grant.spend(context, at, request.request_id, &new_session.action()), Err(GrantError::WrongChange));
        assert_eq!(grant.spend(context, at, request.request_id, &Action::DeleteAccount(account_id())), Err(GrantError::WrongChange));
        assert_eq!(grant.spend(context, at, RequestId::new(1).unwrap(), &request.change.action()), Err(GrantError::WrongChange));
        let last = at + GRANT_LIFETIME_MS - 1;
        assert_eq!(spend(&mut grant, context, last), Ok(()));
        assert_eq!(spend(&mut grant, context, last), Err(GrantError::Used));

        let mut near_limit = issue(context, context, u64::MAX, u64::MAX).unwrap();
        assert_eq!(spend(&mut near_limit, context, u64::MAX), Ok(()));
    }
}
