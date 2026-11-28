//! The Nexus server's account and session decisions. S3 loads the rows under its
//! serialization lock and applies the returned effects before releasing the answer.

use super::proto::BackendId;
use hkdf::hmac::{Hmac, Mac};
use sha2::{Digest, Sha256, digest::CtOutput};
use std::fmt;
use zeroize::Zeroizing;

pub const SESSION_MS: u64 = 90 * 24 * 60 * 60 * 1000;
pub const LEAVE_MS: u64 = 10 * 60 * 1000;
pub const SECRET_BYTES: usize = 32;

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

/// A canonical account name supplied by the account layer, to be C7's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountName(String);

impl AccountName {
    pub fn from_canonical(name: String) -> Option<Self> {
        if name.len() > 254 {
            return None;
        }
        let mut parts = name.split('@');
        let local = parts.next()?;
        let domain = parts.next()?;
        // The design does not say whether to accept an empty side of the @.
        (!local.is_empty()
            && !domain.is_empty()
            && parts.next().is_none()
            && name.trim() == name
            && !name.bytes().any(|byte| byte.is_ascii_uppercase()))
        .then_some(Self(name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A device's raw secret, to be C7's. Decisions retain only its SHA-256.
pub struct DeviceSecret(Zeroizing<[u8; SECRET_BYTES]>);

impl DeviceSecret {
    pub fn new(bytes: [u8; SECRET_BYTES]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of(&self.0)
    }
}

impl fmt::Debug for DeviceSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeviceSecret(..)")
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Status {
    On,
    Off,
    Deleted,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct AccountState {
    pub backend: BackendId,
    pub restore: u64,
    pub name: AccountName,
    pub life: u64,
    pub status: Status,
    pub password_changes: u64,
    pub password_reset_required: bool,
    pub code_available: bool,
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
    account: AccountName,
    life: u64,
    revision: u64,
    device: u64,
    number: u64,
}

impl AccountState {
    fn scope(&self, device: &Device) -> Scope {
        Scope {
            backend: self.backend,
            restore: self.restore,
            account: self.name.clone(),
            life: self.life,
            revision: self.revision,
            device: device.id,
            number: device.number,
        }
    }

    fn same_account(&self, other: &Self) -> bool {
        self.backend == other.backend && self.name == other.name
    }

    pub fn position(&self, change: u64) -> Position {
        Position {
            backend: self.backend,
            restore: self.restore,
            account: self.name.clone(),
            life: self.life,
            change,
        }
    }

    fn allows_session(&self) -> Result<(), Denied> {
        match self.status {
            Status::Deleted => Err(Denied::NotSignedIn),
            Status::Off => Err(Denied::AccountOff),
            Status::On if self.password_reset_required => Err(Denied::PasswordResetRequired),
            Status::On => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AdminKind {
    CreateAccount,
    ChangeAccountStatus,
    DeleteAccount,
    ResetPassword,
    MakeAdmin,
    ForceOut,
    ChangeMusicLogin,
    SetMusicAddress,
    SetDiagnosticsDefault,
    SetRequests,
    SetLidarr,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum LeaveFor {
    ChangePassword,
    DeleteHistory,
    Admin(AdminKind),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Leave {
    purpose: LeaveFor,
    life: u64,
    granted: u64,
    serial: u64,
}

impl Leave {
    pub fn stored(purpose: LeaveFor, life: u64, granted: u64, serial: u64) -> Self {
        Self {
            purpose,
            life,
            granted,
            serial,
        }
    }

    pub fn fields(self) -> (LeaveFor, u64, u64, u64) {
        (self.purpose, self.life, self.granted, self.serial)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Session {
    pub id: u64,
    pub hash: Fingerprint,
    pub last_used: u64,
    pub restricted: bool,
    pub leave: Option<Leave>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Device {
    id: u64,
    number: u64,
    secret: Option<Fingerprint>,
    last_session: u64,
    last_leave: u64,
    session: Option<Session>,
}

impl Device {
    fn new(id: u64, secret: Fingerprint) -> Self {
        Self {
            id,
            number: 0,
            secret: Some(secret),
            last_session: 0,
            last_leave: 0,
            session: None,
        }
    }

    fn end(&mut self) -> Result<(), Denied> {
        if self.session.is_some() {
            self.number = next(self.number)?;
            self.session = None;
        }
        Ok(())
    }

    fn expire(&mut self, now: u64) -> Result<(), Denied> {
        if self
            .session
            .as_ref()
            .is_some_and(|s| !within(s.last_used, now, SESSION_MS))
        {
            self.end()?;
        }
        Ok(())
    }

    fn authenticate(&mut self, bearer: &Fingerprint, now: u64) -> Result<&mut Session, Denied> {
        self.expire(now)?;
        let session = self
            .session
            .as_mut()
            .filter(|s| s.hash == *bearer)
            .ok_or(Denied::NotSignedIn)?;
        if !session.restricted {
            session.last_used = now;
        }
        Ok(session)
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Denied {
    NotSignedIn,
    AccountOff,
    PasswordResetRequired,
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
    session_id: u64,
    access: Access,
    arrived: u64,
}

/// §8.8. A password receipt has one purpose; a leave receipt is also bound to the
/// session that requested it. §8.5 resolves devices by secret before password work.
mod receipt;
pub use receipt::{PasswordCheck, Receipt};

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
    pub account: AccountName,
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

/// §12.3 fixes the stamp encoding. Use HMAC-SHA-256 with a
/// 32-byte random key. After a fixed domain, encode id, restore, life and change at fixed
/// widths (big endian numbers), followed by all bytes of the exact UTF-8 account name.
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
        mac.update(b"Nexus Music stamp v1\0");
        mac.update(p.backend.as_bytes());
        mac.update(&p.restore.to_be_bytes());
        mac.update(&p.life.to_be_bytes());
        mac.update(&p.change.to_be_bytes());
        mac.update(p.account.as_str().as_bytes());
        mac
    }

    pub fn stamp(&self, p: &Position) -> Fingerprint {
        Fingerprint::from_bytes(self.mac(p).finalize().into_bytes().into())
    }

    /// Authenticate before inspecting the fields. Changes restart at deletion, so a lower
    /// life with a higher change never shows a rollback.
    pub fn presented(
        &self,
        current: &Position,
        shown: &Position,
        tag: &Fingerprint,
    ) -> Option<Restore> {
        if self.mac(shown).verify_slice(tag.as_bytes()).is_err()
            || current.backend != shown.backend
            || current.restore != shown.restore
            || current.account != shown.account
        {
            return None;
        }
        if shown.life > current.life {
            Some(Restore::HoldThrough(shown.life))
        } else if shown.life == current.life && shown.change > current.change {
            Some(Restore::EndSessions)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Restore {
    EndSessions,
    HoldThrough(u64),
}

mod durable;
pub use durable::*;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod recovery_tests;
