//! The handoff from the server's bounded Argon2id work to the serialized decision.

use super::*;

// The design does not say how long a checked sign-in may remain in flight.
// Retain the five-second receipt lifetime, measured at the backend's effective time.
pub const RECEIPT_MS: u64 = 5_000;

pub(super) enum Purpose {
    SignIn {
        secret: Fingerprint,
        is_new: bool,
    },
    Leave {
        session: SessionId,
        intent: GrantIntent,
    },
    RedeemCode {
        secret: Fingerprint,
        is_new: bool,
        code: u64,
    },
}

/// Bound to the authoritative account/device and verifier before S8 starts the
/// slow password check. S8 supplies the stored Argon2id output, then the derived
/// output for the submitted password. An unknown account takes the dummy path in S8.
pub struct PasswordCheck {
    scope: Scope,
    purpose: Purpose,
    expected: Fingerprint,
    started: u64,
    known: bool,
}

pub struct PasswordReceipt {
    scope: Scope,
    purpose: Purpose,
    checked: u64,
}

impl PasswordCheck {
    pub(super) fn new(
        scope: Scope,
        purpose: Purpose,
        expected: Fingerprint,
        started: u64,
        known: bool,
    ) -> Self {
        Self {
            scope,
            purpose,
            expected,
            started,
            known,
        }
    }

    /// Select S8's known-device worker before doing password work.
    pub fn known_device(&self) -> bool {
        self.known
    }

    /// The ID for the known-device wait is resolved from the secret, never
    /// from an ID claimed in the request.
    pub fn known_device_id(&self) -> Option<DeviceId> {
        self.known.then_some(self.scope.device)
    }

    /// S8 passes its effective backend time and the Argon2id output, never the password.
    pub fn check(self, derived: Fingerprint, clock: Clock) -> Result<PasswordReceipt, Denied> {
        if self.expected != derived {
            return Err(Denied::WrongPassword);
        }
        let checked = clock.effective();
        if checked < self.started {
            return Err(Denied::Time);
        }
        Ok(PasswordReceipt {
            scope: self.scope,
            purpose: self.purpose,
            checked,
        })
    }
}

impl PasswordReceipt {
    pub(super) fn checked(&self) -> u64 {
        self.checked
    }

    pub(super) fn scope(&self) -> &Scope {
        &self.scope
    }

    pub(super) fn consume(self, current: &Scope, now: u64) -> Result<Purpose, Denied> {
        if &self.scope != current {
            return Err(Denied::NotSignedIn);
        }
        if !within(self.checked, now, RECEIPT_MS) {
            return Err(Denied::Time);
        }
        Ok(self.purpose)
    }
}

impl fmt::Debug for PasswordCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PasswordCheck(..)")
    }
}

impl fmt::Debug for PasswordReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PasswordReceipt(..)")
    }
}
