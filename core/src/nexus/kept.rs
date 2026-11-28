//! What the Nexus server decides in one serialized step. Times are milliseconds on the
//! backend's clock. The caller supplies the authoritative store numbers and commits the
//! resulting changes in the order below. Nothing here opens either store.

use super::proto::BackendId;
use hkdf::hmac::{Hmac, Mac};
use sha2::{Digest, Sha256, digest::CtOutput};
use std::fmt;
use zeroize::Zeroizing;

const HOUR: u64 = 60 * 60 * 1000;
/// §7.6, counted from this session's proof.
pub const SEND_MS: u64 = 7 * 24 * HOUR;
/// §7.6.
pub const READ_MS: u64 = 72 * HOUR;
/// §7.6–7.7.
pub const SESSION_MS: u64 = 90 * 24 * HOUR;
/// §7.6, counted from the separate proof to delete.
pub const DELETE_MS: u64 = 10 * 60 * 1000;
/// §7.6, counted from the arrival of the write.
pub const WRITE_MS: u64 = 30 * 1000;
/// §7.6, the app's occasion for proving again while in use.
pub const RENEW_MS: u64 = 12 * HOUR;
/// §7.5, the size of both a session bearer and the device's own random secret.
pub const SECRET_BYTES: usize = 32;

pub(super) fn within(start: u64, now: u64, limit: u64) -> bool {
    // §7.6: at the deadline the permission has ended.
    now.checked_sub(start).is_some_and(|age| age < limit)
}

pub fn should_prove(proven: u64, now: u64, in_use: bool, asked_again: bool) -> bool {
    asked_again || (in_use && now.checked_sub(proven).is_some_and(|age| age >= RENEW_MS))
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

    /// For storage or the SHA-256 field of prove/begin.
    pub fn as_bytes(&self) -> &[u8; SECRET_BYTES] {
        self.0.as_bytes().as_ref()
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Fingerprint(..)")
    }
}

/// §7.5. Look up by account and device secret even when the connect reply carrying its id
/// was lost. A claimed id alone never selects a device.
pub fn same_device(stored_name: &str, stored: &Fingerprint, name: &str, offered: &Fingerprint) -> bool {
    stored_name == name && stored == offered
}

/// §7.7 and §9.2. Taken from the store kept apart. An ending changes this identity even
/// for a connect that began before there was a working session.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Scope {
    pub backend: BackendId,
    pub restore: u64,
    pub account: String,
    pub life: u64,
    pub device: u64,
    pub number: u64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Purpose {
    Connect,
    Again(Fingerprint),
    Delete(Fingerprint),
}

#[derive(Debug)]
pub struct Try {
    scope: Scope,
    serial: u64,
    began: u64,
    purpose: Purpose,
}

impl Try {
    pub fn scope(&self) -> &Scope {
        &self.scope
    }
    pub fn purpose(&self) -> &Purpose {
        &self.purpose
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id: u64,
    pub hash: Fingerprint,
    pub proven: u64,
    pub delete_proven: Option<u64>,
}

impl Session {
    fn new(id: u64, hash: Fingerprint, proven: u64) -> Self {
        Self {
            id,
            hash,
            proven,
            delete_proven: None,
        }
    }
}

/// One row of device state, used under the server's transaction. A new row has no sessions.
/// A try's serial is in memory only. On restart, discard every in-memory try and ticket.
#[derive(Debug)]
pub struct Device {
    recorded: Scope,
    last_session: u64,
    interrupted_promotion: Option<u64>,
    working: Option<Session>,
    waiting: Option<Session>,
    serial: u64,
    pending: Option<u64>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Denied {
    NotSignedIn,
    ProveAgain,
    HistoryHeld,
    Time,
    Exhausted,
    WrongPurpose,
    ReusedSession,
    StartAgain,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Access {
    Send,
    Read,
}

#[derive(Debug)]
pub struct Request {
    scope: Scope,
    session: Fingerprint,
    session_id: u64,
    action: Access,
    arrived: u64,
}

impl Device {
    pub fn new(scope: &Scope) -> Self {
        Self {
            recorded: scope.clone(),
            last_session: 0,
            interrupted_promotion: None,
            working: None,
            waiting: None,
            serial: 0,
            pending: None,
        }
    }

    /// §7.7. Persist the changed number before applying the cleared rows. An exhausted
    /// number stops the transaction and serving that device; never wrap it.
    fn end(&mut self, scope: &mut Scope) -> Result<(), Denied> {
        if &self.recorded != scope {
            return Err(Denied::NotSignedIn);
        }
        let number = scope.number.checked_add(1).ok_or(Denied::Exhausted)?;
        scope.number = number;
        self.recorded.number = number;
        self.interrupted_promotion = None;
        self.working = None;
        self.waiting = None;
        self.pending = None;
        Ok(())
    }

    /// §7.7. Expiry of the working session ends the whole device, even if a newer one waits.
    /// §7.5 keeps the older deadline until promotion.
    fn expire(&mut self, scope: &mut Scope, now: u64) -> Result<(), Denied> {
        if &self.recorded != scope {
            return Err(Denied::NotSignedIn);
        }
        if self
            .working
            .as_ref()
            .map(|s| s.proven)
            .or(self.interrupted_promotion)
            .is_some_and(|proof| now.checked_sub(proof).is_some_and(|age| age >= SESSION_MS))
        {
            self.end(scope)?;
        }
        Ok(())
    }

    fn use_session(&mut self, hash: &Fingerprint, now: u64) -> Result<&mut Session, Denied> {
        if self
            .waiting
            .as_ref()
            .is_some_and(|s| &s.hash == hash && within(s.proven, now, SESSION_MS))
        {
            self.working = self.waiting.take();
            self.interrupted_promotion = None;
            // A proof under the old session cannot give anything after promotion.
            self.pending = None;
        }
        self.working
            .as_mut()
            .filter(|s| &s.hash == hash && within(s.proven, now, SESSION_MS))
            .ok_or(Denied::NotSignedIn)
    }

    /// §7.2 and §7.7. Start (and supersede) under the same lock used by finish. Keep the
    /// ticket with the challenge; retrieve and spend the challenge before the first upstream call.
    fn begin(&mut self, scope: &mut Scope, purpose: Purpose, now: u64) -> Result<Try, Denied> {
        self.expire(scope, now)?;
        if let Purpose::Again(hash) | Purpose::Delete(hash) = &purpose {
            self.use_session(hash, now)?;
        }
        self.serial = self.serial.checked_add(1).ok_or(Denied::Exhausted)?;
        self.pending = Some(self.serial);
        Ok(Try {
            scope: scope.clone(),
            serial: self.serial,
            began: now,
            purpose,
        })
    }

    /// Is this spent challenge still current before an upstream request or a grant?
    fn current_try(&self, scope: &Scope, ticket: &Try) -> bool {
        self.recorded == *scope && ticket.scope == *scope && self.pending == Some(ticket.serial)
    }

    /// Called after a bound proof, in the step granting power. A newer begin also invalidates
    /// a spend already waiting on Navidrome. Supply a fresh random session's hash only for
    /// Connect/Again. A different authenticated name ends this device's sessions (§7.7).
    #[cfg(test)]
    fn finish(
        &mut self,
        scope: &mut Scope,
        ticket: Try,
        account: &str,
        fresh: Option<Fingerprint>,
        now: u64,
    ) -> Result<(), Denied> {
        self.finish_at(scope, ticket, account, fresh, now, now)
    }

    fn finish_at(
        &mut self,
        scope: &mut Scope,
        ticket: Try,
        authenticated: &str,
        fresh: Option<Fingerprint>,
        proven: u64,
        now: u64,
    ) -> Result<(), Denied> {
        self.expire(scope, now)?;
        if *scope != ticket.scope || self.pending != Some(ticket.serial) {
            return Err(Denied::NotSignedIn);
        }
        self.pending = None;
        if now < ticket.began {
            return Err(Denied::Time);
        }
        if let Purpose::Again(hash) | Purpose::Delete(hash) = &ticket.purpose {
            self.use_session(hash, now)?;
        }
        if authenticated != scope.account {
            self.end(scope)?;
            return Err(Denied::NotSignedIn);
        }
        if let Some(hash) = &fresh {
            if self.working.iter().chain(&self.waiting).any(|s| s.hash == *hash) {
                return Err(Denied::ReusedSession);
            }
        }
        let new_id = if fresh.is_some() {
            self.last_session.checked_add(1).ok_or(Denied::Exhausted)?
        } else {
            self.last_session
        };
        match (ticket.purpose, fresh) {
            (Purpose::Connect, Some(hash)) => {
                self.end(scope)?;
                self.working = Some(Session::new(new_id, hash, proven));
            }
            (Purpose::Again(_), Some(hash)) => self.waiting = Some(Session::new(new_id, hash, proven)),
            (Purpose::Delete(hash), None) => self.use_session(&hash, now)?.delete_proven = Some(proven),
            _ => return Err(Denied::WrongPurpose),
        }
        self.last_session = new_id;
        Ok(())
    }

    fn arrive(
        &mut self,
        scope: &mut Scope,
        session: &Fingerprint,
        action: Access,
        held: bool,
        now: u64,
    ) -> Result<Request, Denied> {
        self.expire(scope, now)?;
        let s = self.use_session(session, now)?;
        allowed(s, action, held, now)?;
        Ok(Request {
            scope: scope.clone(),
            session: session.clone(),
            session_id: s.id,
            action,
            arrived: now,
        })
    }

    /// Check in the step keeping a write or immediately before writing a read's answer.
    fn complete(
        &mut self,
        scope: &mut Scope,
        request: Request,
        held: bool,
        now: u64,
    ) -> Result<(), Denied> {
        self.expire(scope, now)?;
        if request.scope != *scope {
            return Err(Denied::NotSignedIn);
        }
        let s = self.use_session(&request.session, now)?;
        if s.id != request.session_id {
            return Err(Denied::NotSignedIn);
        }
        if now < request.arrived
            || (request.action == Access::Send && !within(request.arrived, now, WRITE_MS))
        {
            return Err(Denied::Time);
        }
        allowed(s, request.action, held, now)
    }

    /// §11.1. First persist the next life, then erase the account's data in one transaction.
    /// All devices and their tries belong to the old life. This device ends too.
    fn delete(&mut self, scope: &mut Scope, hash: &Fingerprint, now: u64) -> Result<(), Denied> {
        self.expire(scope, now)?;
        let session = self.use_session(hash, now)?;
        if !session.delete_proven.is_some_and(|at| within(at, now, DELETE_MS)) {
            return Err(Denied::ProveAgain);
        }
        let life = scope.life.checked_add(1).ok_or(Denied::Exhausted)?;
        self.end(scope)?;
        scope.life = life;
        self.recorded.life = life;
        Ok(())
    }
}

fn allowed(s: &Session, action: Access, held: bool, now: u64) -> Result<(), Denied> {
    if held {
        return Err(Denied::HistoryHeld);
    }
    let until = match action {
        Access::Send => SEND_MS,
        Access::Read => READ_MS,
    };
    if within(s.proven, now, until) {
        Ok(())
    } else {
        Err(Denied::ProveAgain)
    }
}

/// §11.1. Retain the last consented life through disconnect. Only a new choice enables a
/// new install, or enables sending again after deletion.
pub fn may_refill(consented_life: Option<u64>, current_life: u64) -> bool {
    consented_life == Some(current_life)
}

/// §7.7. The client captures both its component number and the destination at dispatch.
pub fn may_apply(component: u64, sent_under: u64, expected: &Position, answer: &Position) -> bool {
    component == sent_under && expected.same_life(answer)
}

/// §9.2. The fields covered by one fingerprint.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Position {
    pub backend: BackendId,
    pub restore: u64,
    pub account: String,
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

/// §9.2 fixes the stamp encoding. Use HMAC-SHA-256 with a
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
        mac.update(p.account.as_bytes());
        mac
    }

    pub fn stamp(&self, p: &Position) -> Fingerprint {
        Fingerprint::from_bytes(self.mac(p).finalize().into_bytes().into())
    }

    /// Authenticate before inspecting the fields. Changes restart at deletion, so a lower
    /// life with a higher change never shows a rollback.
    pub fn presented(&self, current: &Position, shown: &Position, tag: &Fingerprint) -> Option<Restore> {
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
            Some(Restore::Reprove)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Restore {
    Reprove,
    HoldThrough(u64),
}

/// §9.2. S3 persists each half in this order. Reconnect first ends, then grants.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Change {
    EndDevice,
    DeleteAccount,
    GrantSession,
}
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Where {
    Apart,
    Data,
}
impl Change {
    pub fn order(self) -> [Where; 2] {
        match self {
            Self::EndDevice | Self::DeleteAccount => [Where::Apart, Where::Data],
            Self::GrantSession => [Where::Data, Where::Apart],
        }
    }
}

/// §9.2 and §11.2. S3 supplies the durable comparison (including whether a write was
/// interrupted). Before answering, follow this plan across all affected rows.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Startup {
    Ready,
    /// Finish an ending in the data that was already recorded in the store.
    FinishEnding,
    /// Discard a grant recorded only in the data.
    EndUnknownSession,
    /// Advance the restore number and end every device. Use restored_account for each account.
    RestoreData,
    /// Data present with no readable store: stop until the owner's explicit adoption.
    Stop,
    /// After explicit adoption: advance restore, end all devices, hold every account.
    HoldAll,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum StoreComparison {
    Matches,
    EndingRecorded,
    GrantOnlyInData,
    OlderCopy,
    StoreLost,
}

pub fn at_start(comparison: StoreComparison, owner_adopts: bool) -> Startup {
    match comparison {
        StoreComparison::Matches => Startup::Ready,
        StoreComparison::EndingRecorded => Startup::FinishEnding,
        StoreComparison::GrantOnlyInData => Startup::EndUnknownSession,
        StoreComparison::OlderCopy => Startup::RestoreData,
        StoreComparison::StoreLost if owner_adopts => Startup::HoldAll,
        StoreComparison::StoreLost => Startup::Stop,
    }
}

pub fn restored_account(store_life: u64, data_life: u64, already_held: bool) -> Restore {
    if already_held || store_life > data_life {
        Restore::HoldThrough(store_life)
    } else {
        Restore::Reprove
    }
}

/// §11.2. Restore number and device numbers advance before answering again.
fn restore_device(device: &mut Device, scope: &mut Scope, new_restore: u64) -> Result<(), Denied> {
    if new_restore <= scope.restore {
        return Err(Denied::Exhausted);
    }
    device.end(scope)?;
    scope.restore = new_restore;
    device.recorded.restore = new_restore;
    Ok(())
}

mod durable;
pub use durable::*;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod recovery_tests;
