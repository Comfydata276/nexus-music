//! The boundary with S3. The answer is released only after the caller applies every
//! effect, including for a refusal. S3 serializes load, decide, apply and publish.

use super::*;

/// Read this mark from the backend-wide apart store on every decision, including a
/// denial; write the new mark there before replying. A clock far in the future ends
/// sessions early. It is deliberately not corrected: that fails safe.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    pub now: u64,
    pub high_water: u64,
}

impl Clock {
    pub fn effective(self) -> u64 {
        self.now.max(self.high_water)
    }
}

/// Data rows. The session id distinguishes uses even if a source supplies a retired
/// bearer's bytes again. Its counter is reserved apart before a grant is written.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Rows {
    pub scope: Scope,
    pub working: Option<Session>,
    pub waiting: Option<Session>,
}

/// §9.2: the design does not say how to recognize session revocations and partial grants.
/// The apart store keeps at most two permitted session numbers and their delete leave.
/// It never needs the bearer or even its hash.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Permit {
    pub id: u64,
    pub delete_proven: Option<u64>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Apart {
    pub scope: Scope,
    pub last_session: u64,
    /// The original family deadline survives a crash while N is taking O's place.
    pub family_proven: Option<u64>,
    pub working: Option<Permit>,
    pub waiting: Option<Permit>,
}

impl Apart {
    fn permits(&self) -> impl Iterator<Item = &Permit> {
        self.working.iter().chain(&self.waiting)
    }

    /// Before writing the data, retain only powers present both before and after. An
    /// old grant to delete cannot survive a copy made before it was replaced.
    fn without_revoked(&self, after: &Self) -> Self {
        let retain = |old: Permit| -> Option<Permit> {
            if self.scope != after.scope {
                return None;
            }
            let new = after.permits().find(|p| p.id == old.id)?;
            Some(Permit {
                id: old.id,
                delete_proven: old
                    .delete_proven
                    .filter(|_| old.delete_proven == new.delete_proven),
            })
        };
        Self {
            scope: after.scope.clone(),
            last_session: after.last_session,
            family_proven: self.family_proven.filter(|_| self.scope == after.scope),
            working: self.working.and_then(retain),
            waiting: self.waiting.and_then(retain),
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Family {
    /// End this device's working, waiting, try and leave.
    Device,
    /// In the apart transaction, advance the account life, ending every device. In
    /// the data transaction erase the account's history and its devices (§11.1).
    Account,
    /// In the apart transaction advance restore and end every device on this backend.
    /// Apply restore_device to each row under that one new restore number (§11.2).
    Backend,
}

#[derive(Debug)]
pub struct Update {
    pub previous: Scope,
    pub family: Option<Family>,
    pub apart_first: Apart,
    pub data: Rows,
    pub apart_last: Apart,
}

/// A separate, empty device row for a verified switch. Insert it under the new
/// account in the data transaction, with no sessions, leave or fetch cursor. S3
/// allocates the vacant destination by secret, under its serialization lock.
#[derive(Debug)]
pub struct NewDevice {
    pub scope: Scope,
    pub device_hash: Fingerprint,
    pub description: crate::nexus::prove::Description,
}

/// Order: atomically persist high_water, family fences and apart_first in the apart
/// store; install data in a data transaction; finally recognize new grants apart.
/// Recovery at either cut uses Device::load. Publish no response on a storage error.
#[must_use]
#[derive(Debug)]
pub struct Effects {
    pub high_water: u64,
    pub update: Option<Update>,
    pub new_device: Option<NewDevice>,
}

#[must_use = "apply the durable effects even when the device's request was refused"]
pub struct Decision<T> {
    effects: Effects,
    answer: T,
}

impl<T> Decision<T> {
    pub fn apply<E>(self, persist: impl FnOnce(&Effects) -> Result<(), E>) -> Result<T, E> {
        persist(&self.effects)?;
        Ok(self.answer)
    }
}

impl Device {
    /// Start/restart using only stored rows. A grant known on only one side is dead.
    /// An old copy cannot resurrect a bearer the apart store has stopped recognizing.
    /// On restart no try or pending upstream proof is loaded.
    pub fn load(apart: &Apart, data: &Rows) -> Self {
        let recover = |permit: Permit| -> Option<Session> {
            if apart.scope != data.scope || permit.id == 0 || permit.id > apart.last_session {
                return None;
            }
            let mut row = data
                .working
                .iter()
                .chain(&data.waiting)
                .find(|s| s.id == permit.id)?
                .clone();
            row.delete_proven = row
                .delete_proven
                .filter(|_| row.delete_proven == permit.delete_proven);
            Some(row)
        };
        let mut device = Self::new(&apart.scope);
        device.last_session = apart.last_session;
        device.working = apart.working.and_then(recover);
        if apart.family_proven.is_some() && apart.waiting.map(|p| p.id) != apart.working.map(|p| p.id) {
            device.waiting = apart.waiting.and_then(recover);
            if device.working.is_none() && device.waiting.is_some() {
                device.interrupted_promotion = apart.family_proven;
            }
        }
        device
    }

    pub(super) fn rows(&self) -> Rows {
        Rows {
            scope: self.recorded.clone(),
            working: self.working.clone(),
            waiting: self.waiting.clone(),
        }
    }

    pub(super) fn apart(&self) -> Apart {
        let permit = |s: &Session| Permit {
            id: s.id,
            delete_proven: s.delete_proven,
        };
        Apart {
            scope: self.recorded.clone(),
            last_session: self.last_session,
            family_proven: self
                .working
                .as_ref()
                .map(|s| s.proven)
                .or(self.interrupted_promotion),
            working: self.working.as_ref().map(permit),
            waiting: self.waiting.as_ref().map(permit),
        }
    }

    /// §9.2. The entry for every device request. Check the restore it actually named
    /// before applying any part of the request, including a fetch under a new bearer.
    pub fn request<T>(
        &mut self,
        authority: &Scope,
        received_restore: u64,
        clock: Clock,
        action: impl FnOnce(Turn<'_>) -> Result<T, Denied>,
    ) -> Decision<Result<T, Denied>> {
        self.decide(authority, clock, |turn| {
            if received_restore != turn.scope().restore {
                Err(Denied::StartAgain)
            } else {
                action(turn)
            }
        })
    }

    /// The one entry to a serialized decision. A Turn is consumed by its operation. If
    /// persisting fails, discard this in-memory device and recover before serving again.
    pub fn decide<T>(
        &mut self,
        authority: &Scope,
        clock: Clock,
        action: impl FnOnce(Turn<'_>) -> T,
    ) -> Decision<T> {
        let before = self.apart();
        let mut scope = authority.clone();
        let now = clock.effective();
        let mut new_device = None;
        let answer = action(Turn {
            new_device: &mut new_device,
            device: self,
            scope: &mut scope,
            now,
        });
        let after = self.apart();
        let update = (before != after).then(|| {
            let family = if before.scope.restore != after.scope.restore {
                Some(Family::Backend)
            } else if before.scope.life != after.scope.life {
                Some(Family::Account)
            } else if before.scope.number != after.scope.number {
                Some(Family::Device)
            } else {
                None
            };
            Update {
                previous: before.scope.clone(),
                family,
                apart_first: before.without_revoked(&after),
                data: self.rows(),
                apart_last: after,
            }
        });
        Decision {
            effects: Effects {
                high_water: now,
                new_device,
                update,
            },
            answer,
        }
    }
}

/// The one step under S3's lock. Finish accepts only the result of a consumed, verified
/// C4 proof. The authenticated name must be the name the device asked to prove, including
/// for an account change. An unexpected getUser answer alone never ends a device.
pub struct Turn<'a> {
    new_device: &'a mut Option<NewDevice>,
    device: &'a mut Device,
    scope: &'a mut Scope,
    now: u64,
}

impl Turn<'_> {
    pub fn now(&self) -> u64 {
        self.now
    }
    pub fn scope(&self) -> &Scope {
        self.scope
    }
    pub fn end(self) -> Result<(), Denied> {
        self.device.end(self.scope)
    }
    pub fn expire(self) -> Result<(), Denied> {
        self.device.expire(self.scope, self.now)
    }
    pub fn begin(self, purpose: Purpose) -> Result<Try, Denied> {
        self.device.begin(self.scope, purpose, self.now)
    }
    pub fn current(self, ticket: &Try) -> Result<(), Denied> {
        self.device.expire(self.scope, self.now)?;
        if self.device.current_try(self.scope, ticket) {
            Ok(())
        } else {
            Err(Denied::NotSignedIn)
        }
    }
    /// Only a receipt from the completed exchange can grant power. Supply the current
    /// destination scope from the apart store; for a changed account it must still be
    /// the vacant row allocated at begin. A change ends A and creates B empty even
    /// though this request under A is answered NotSignedIn (§7.5, §7.7).
    pub fn finish(
        self,
        receipt: crate::nexus::prove::Receipt,
        current_target: &Scope,
        fresh: Option<Fingerprint>,
    ) -> Result<(), Denied> {
        self.device.expire(self.scope, self.now)?;
        if !within(receipt.made(), self.now, crate::nexus::prove::RECEIPT_MS)
            || !self.device.current_try(self.scope, receipt.ticket_ref())
            || receipt.target() != current_target
        {
            return Err(Denied::NotSignedIn);
        }
        let proven = receipt.made();
        let (ticket, target, device_hash, description) = receipt.into_parts();
        if target.account != self.scope.account {
            self.device.end(self.scope)?;
            *self.new_device = Some(NewDevice {
                scope: target,
                device_hash,
                description,
            });
            return Err(Denied::NotSignedIn);
        }
        self.device
            .finish_at(self.scope, ticket, &target.account, fresh, proven, self.now)
    }

    #[cfg(test)]
    pub(super) fn finish_fixture(
        self,
        ticket: Try,
        name: &str,
        fresh: Option<Fingerprint>,
    ) -> Result<(), Denied> {
        self.device.finish(self.scope, ticket, name, fresh, self.now)
    }
    pub fn arrive(self, session: &Fingerprint, action: Access, held: bool) -> Result<Request, Denied> {
        self.device.arrive(self.scope, session, action, held, self.now)
    }
    pub fn complete(self, request: Request, held: bool) -> Result<(), Denied> {
        self.device.complete(self.scope, request, held, self.now)
    }
    pub fn delete(self, hash: &Fingerprint) -> Result<(), Denied> {
        self.device.delete(self.scope, hash, self.now)
    }
    pub fn restore_device(self, restore: u64) -> Result<(), Denied> {
        restore_device(self.device, self.scope, restore)
    }
}

impl Family {
    /// The additional devices in the affected account/backend. S3 applies all new
    /// numbers in one apart transaction, or none on exhaustion. The initiating device
    /// already has its new scope in apart_first.
    pub fn end_other(self, change: &Update, other: &Scope) -> Result<Option<Scope>, Denied> {
        let source = &change.previous;
        let next = &change.apart_first.scope;
        if change.family != Some(self)
            || source.backend != other.backend
            || source.restore != other.restore
            || (source.device == other.device && source.account == other.account)
        {
            return Ok(None);
        }
        let mut scope = other.clone();
        match self {
            Family::Backend => {
                scope.restore = next.restore;
                scope.number = scope.number.checked_add(1).ok_or(Denied::Exhausted)?;
            }
            Family::Account if source.account == other.account && source.life == other.life => {
                scope.life = next.life
            }
            _ => return Ok(None),
        }
        Ok(Some(scope))
    }
}
