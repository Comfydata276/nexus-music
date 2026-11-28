//! The Nexus server's account and session decisions. S3 loads the rows under its
//! serialization lock and applies the returned effects before releasing the answer.

use super::account::{
    AccountId, CODE_LIFETIME_MS, DeviceId, DeviceSecret, Email, GrantSecret, MusicAddress,
    PASSWORD_LEAVE_MS, RequestId, SECRET_BYTES, SESSION_IDLE_MS, SessionId, StampTag,
};
use super::proto::BackendId;
use hkdf::hmac::{Hmac, Mac};
use sha2::{Digest, Sha256, digest::CtOutput};
use std::fmt;
use zeroize::Zeroizing;

const USE_WRITE_MS: u64 = 60 * 60 * 1000;

// The design does not say whether the old in-flight write bound survives §8.6.
// Keep the 30-second commit bound. It belongs to a request, which cannot renew it.
pub const WRITE_MS: u64 = 30_000;

// The design does not say whether permission includes its last millisecond.
// A deadline is exclusive, and a future start confers no permission.
fn within(start: u64, now: u64, limit: u64) -> bool {
    now.checked_sub(start).is_some_and(|age| age < limit)
}

/// A stored SHA-256 of a bearer or device secret. RustCrypto's CtOutput compares all bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct Fingerprint(CtOutput<Sha256>);

impl Fingerprint {
    pub fn of(secret: &[u8; SECRET_BYTES]) -> Self {
        Self(CtOutput::new(Sha256::digest(secret)))
    }

    pub fn from_bytes(hash: [u8; SECRET_BYTES]) -> Self {
        Self(CtOutput::new(hash.into()))
    }

    /// For storage.
    pub fn as_bytes(&self) -> &[u8; SECRET_BYTES] {
        self.0.as_bytes().as_ref()
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Fingerprint(..)")
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Status {
    On,
    Off,
    Deleted,
}

#[derive(Clone, Eq, PartialEq)]
pub struct AccountState {
    pub backend: BackendId,
    pub restore: u64,
    pub id: AccountId,
    pub email: Email,
    pub life: u64,
    pub status: Status,
    pub password_changes: u64,
    pub password_reset_required: bool,
    pub code: Option<LiveCode>,
    pub admin: bool,
    pub music_login: u64,
    pub music_managed: bool,
    pub music_recovery: Option<MusicRecovery>,
    pub history_held: bool,
    /// Fences a checked sign-in across off/on and other account-wide changes,
    /// even when its device did not have a session to end.
    pub revision: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct Scope {
    backend: BackendId,
    restore: u64,
    account: AccountId,
    life: u64,
    revision: u64,
    device: DeviceId,
    number: u64,
}

impl fmt::Debug for AccountState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AccountState")
            .field("id", &self.id)
            .field("status", &self.status)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

impl AccountState {
    /// The initial data and apart state. The caller supplies the reserved ID and
    /// stores the verifier for any supplied live code, but no password verifier.
    pub fn new(
        backend: BackendId,
        restore: u64,
        id: AccountId,
        email: Email,
        code: Option<LiveCode>,
    ) -> Self {
        Self {
            backend,
            restore,
            id,
            email,
            code,
            life: 0,
            status: Status::On,
            password_changes: 0,
            password_reset_required: true,
            admin: false,
            music_login: 0,
            music_managed: false,
            music_recovery: None,
            history_held: false,
            revision: 0,
        }
    }

    fn scope(&self, device: &Device) -> Scope {
        Scope {
            backend: self.backend,
            restore: self.restore,
            account: self.id,
            life: self.life,
            revision: self.revision,
            device: device.id,
            number: device.number,
        }
    }

    fn same_account(&self, other: &Self) -> bool {
        self.backend == other.backend && self.id == other.id
    }

    pub fn admin_target(&self) -> AdminTarget {
        AdminTarget::Account {
            id: self.id,
            revision: self.revision,
        }
    }

    pub fn position(&self, change: u64) -> Position {
        Position {
            backend: self.backend,
            restore: self.restore,
            account: self.id,
            life: self.life,
            change,
        }
    }

    fn allows_session(&self) -> Result<(), Denied> {
        match self.status {
            Status::Deleted => Err(Denied::NotSignedIn),
            _ if self.password_reset_required => Err(Denied::WrongPassword),
            Status::Off => Err(Denied::AccountOff),
            Status::On => Ok(()),
        }
    }
}

/// The authoritative code identity and deadline are recorded apart. S8 stores
/// and checks the fingerprint belonging to that number.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct LiveCode {
    pub number: u64,
    pub used: bool,
    pub expires: u64,
}

impl LiveCode {
    pub fn new(number: u64, now: u64) -> Result<Self, Denied> {
        let expires = now.checked_add(CODE_LIFETIME_MS).ok_or(Denied::Exhausted)?;
        Ok(Self {
            number,
            used: false,
            expires,
        })
    }

    fn allows(self, number: u64, now: u64) -> bool {
        self.number == number && !self.used && now < self.expires
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct SessionTarget {
    pub device: DeviceId,
    pub session: SessionId,
}

#[derive(Clone, Eq, PartialEq)]
pub enum AdminTarget {
    Account { id: AccountId, revision: u64 },
    NewAccount(Email),
    Backend,
}

impl fmt::Debug for AdminTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Account { id, revision } => f
                .debug_struct("Account")
                .field("id", id)
                .field("revision", revision)
                .finish(),
            Self::NewAccount(_) => f.write_str("NewAccount(..)"),
            Self::Backend => f.write_str("Backend"),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum LeaveFor {
    ChangePassword,
    DeleteHistory,
    Admin {
        action: AdminAction,
        target: AdminTarget,
    },
}

/// The mutation and request number the password check authorizes.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct GrantIntent {
    pub request_id: RequestId,
    pub change: LeaveFor,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Leave {
    intent: GrantIntent,
    life: u64,
    revision: u64,
    admin: bool,
    granted: u64,
    serial: u64,
    hash: Fingerprint,
}

impl Leave {
    pub fn stored(
        intent: GrantIntent,
        life: u64,
        revision: u64,
        admin: bool,
        granted: u64,
        serial: u64,
        hash: Fingerprint,
    ) -> Self {
        Self {
            intent,
            life,
            revision,
            admin,
            granted,
            serial,
            hash,
        }
    }

    pub fn fields(&self) -> (&GrantIntent, u64, u64, bool, u64, u64, &Fingerprint) {
        (
            &self.intent,
            self.life,
            self.revision,
            self.admin,
            self.granted,
            self.serial,
            &self.hash,
        )
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Session {
    pub id: SessionId,
    pub hash: Fingerprint,
    pub sign_out: Fingerprint,
    pub last_used: u64,
    pub leave: Option<Leave>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Device {
    id: DeviceId,
    number: u64,
    secret: Option<Fingerprint>,
    known: bool,
    last_session: u64,
    last_leave: u64,
    session: Option<Session>,
    last_accepted: Option<u64>,
}

impl Device {
    fn new(id: DeviceId, secret: Fingerprint) -> Self {
        Self {
            id,
            number: 0,
            secret: Some(secret),
            known: false,
            last_session: 0,
            last_leave: 0,
            session: None,
            last_accepted: None,
        }
    }

    fn end(&mut self) -> Result<(), Denied> {
        if self.session.is_some() {
            self.number = next(self.number)?;
            self.session = None;
            self.last_accepted = None;
        }
        Ok(())
    }

    fn expire(&mut self, now: u64) -> Result<(), Denied> {
        if self
            .last_accepted
            .is_some_and(|last| now.saturating_sub(last) >= SESSION_IDLE_MS)
        {
            self.end()?;
        }
        Ok(())
    }

    fn touch(&mut self, now: u64) -> Result<&mut Session, Denied> {
        let session = self.session.as_mut().ok_or(Denied::NotSignedIn)?;
        self.last_accepted = Some(self.last_accepted.unwrap_or(now).max(now));
        if now.saturating_sub(session.last_used) >= USE_WRITE_MS {
            session.last_used = now;
        }
        Ok(session)
    }

    fn authenticate(&mut self, bearer: &Fingerprint, now: u64) -> Result<&mut Session, Denied> {
        self.expire(now)?;
        if !self
            .session
            .as_ref()
            .is_some_and(|session| session.hash == *bearer)
        {
            return Err(Denied::NotSignedIn);
        }
        self.touch(now)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Denied {
    NotSignedIn,
    AccountOff,
    WrongPassword,
    HistoryHeld,
    Time,
    Exhausted,
    WrongPurpose,
    LeaveRequired,
    AdminRequired,
    ReusedSession,
    DeviceInUse,
    StartAgain,
}

// The design does not say how counters exhaust. Exhaustion refuses the operation
// and the store must fail closed if a required ending cannot be recorded.
fn next(value: u64) -> Result<u64, Denied> {
    value.checked_add(1).ok_or(Denied::Exhausted)
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Access {
    Read,
    Write,
    ReadHistory,
    WriteHistory,
}

impl Access {
    fn is_write(self) -> bool {
        matches!(self, Self::Write | Self::WriteHistory)
    }

    fn check(self, account: &AccountState) -> Result<(), Denied> {
        if account.history_held && matches!(self, Self::ReadHistory | Self::WriteHistory) {
            return Err(Denied::HistoryHeld);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct Request {
    scope: Scope,
    session_id: SessionId,
    access: Access,
    arrived: u64,
}

/// §8.8. A password receipt has one purpose; a leave receipt is also bound to the
/// session that requested it. §8.5 resolves devices by secret before password work.
mod receipt;
pub use receipt::{PasswordCheck, PasswordReceipt};

/// §12.5. Retain the last consented life through disconnect. Only a new choice enables a
/// new install, or enables sending again after deletion.
pub fn may_refill(consented_life: Option<u64>, current_life: u64) -> bool {
    consented_life == Some(current_life)
}

/// §8.7. The client captures both its component number and the destination at dispatch.
pub fn may_apply(component: u64, sent_under: u64, expected: &Position, answer: &Position) -> bool {
    component == sent_under && expected.same_life(answer)
}

/// §12.3. The fields covered by one fingerprint.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Position {
    pub backend: BackendId,
    pub restore: u64,
    pub account: AccountId,
    pub life: u64,
    pub change: u64,
}

impl Position {
    fn same_life(&self, other: &Self) -> bool {
        self.backend == other.backend
            && self.restore == other.restore
            && self.account == other.account
            && self.life == other.life
    }
}

/// §12.3. The design does not say how to distinguish the numbered encoding
/// from the old email encoding. Use a new HMAC domain, then the backend id,
/// restore, life, change and account number. Numbers are big-endian u64s.
pub struct StampKey(Zeroizing<[u8; SECRET_BYTES]>);

impl fmt::Debug for StampKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StampKey(..)")
    }
}

impl StampKey {
    pub fn new(bytes: [u8; SECRET_BYTES]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    fn mac(&self, p: &Position) -> Hmac<Sha256> {
        let mut mac: Hmac<Sha256> = hkdf::HmacImpl::new_from_slice(self.0.as_slice());
        mac.update(b"Nexus Music stamp v2\0");
        mac.update(p.backend.as_bytes());
        mac.update(&p.restore.to_be_bytes());
        mac.update(&p.life.to_be_bytes());
        mac.update(&p.change.to_be_bytes());
        mac.update(&p.account.get().to_be_bytes());
        mac
    }

    pub fn stamp(&self, p: &Position) -> StampTag {
        StampTag::from_bytes(self.mac(p).finalize().into_bytes().into())
    }

    /// Authenticate before inspecting the fields. Changes restart at deletion, so a lower
    /// life with a higher change never shows a rollback.
    pub fn presented(
        &self,
        current: &Position,
        shown: &Position,
        tag: &StampTag,
    ) -> Option<Restore> {
        if self.mac(shown).verify_slice(tag.expose_bytes()).is_err()
            || current.backend != shown.backend
            || current.restore != shown.restore
            || current.account != shown.account
        {
            return None;
        }
        if (shown.life, shown.change) > (current.life, current.change) {
            Some(Restore::EndSessions)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Restore {
    EndSessions,
}

mod durable;
pub use durable::*;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod recovery_tests;
