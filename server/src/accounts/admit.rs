//! Who may take what before any slow work, looked up cheaply (the review of S8, C1; the lead's
//! ruling of 28 Nov 15:38). **Nothing here decides anything kept decides**: it only chooses
//! where a request may wait, so that a flood of strangers cannot take every place from people
//! already signed in. kept still says, on the thread that writes, whether a session is live,
//! which door a sign-in goes through and whether a code is good.
//!
//! - **A cheap read path**: a connection of its own that only reads the data (WAL: it never
//!   waits for the thread that writes, nor it for this), for a session by its fingerprint, the
//!   data's restore number, an account by its email, a code by its keyed fingerprint and a
//!   sign-out note's session. Each is one indexed lookup.
//! - **A bounded record of known devices** ([`Seen`]): (account, device secret's fingerprint)
//!   for each device its account knows now (the store kept apart's `known`), read at the start,
//!   added to at each sign-in and taken from at each force-out; at most [`SEEN_MAX`].
//! - **The listener's question** ([`Admission::listener`]): from a request's head (and, for a
//!   sign-in, its body), whether it may take a slot of the reserve: a session that resolves, a
//!   known device signing in to its own account, or a live one-time code.
//!
//! Nothing a person typed, no code, session or secret is kept here beyond the one lookup, or
//! written anywhere.

use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;

use nexus_core::nexus::account::{self, Credential, DeviceSecret, Email, OneTimeCode, SignIn, SignOutNote};
use nexus_core::nexus::kept::Fingerprint;
use rusqlite::{Connection, OptionalExtension};

use super::desk::Admitted;
use super::session::Clock;
use super::setup::CodeKey;
use crate::store::keeper::Keeper;

/// The most (account, device) pairs [`Seen`] holds. Far more than a household's devices; past
/// it the longest-held is let go (it then waits in the strangers' lane until it signs in again).
pub const SEEN_MAX: usize = 4096;

type Pair = (u64, [u8; 32]);

/// The devices each account knows now, by (account, device secret's fingerprint), at most
/// [`SEEN_MAX`]. Which lane a sign-in waits in, and whether it may take a reserved slot, is all
/// it chooses.
#[derive(Default)]
pub struct Seen(Mutex<(HashSet<Pair>, VecDeque<Pair>)>);

impl Seen {
    pub fn new(known: impl IntoIterator<Item = Pair>) -> Self {
        let seen = Seen::default();
        for (account, print) in known {
            seen.add(account, print);
        }
        seen
    }

    pub fn has(&self, account: u64, print: &[u8; 32]) -> bool {
        self.0.lock().is_ok_and(|seen| seen.0.contains(&(account, *print)))
    }

    pub fn add(&self, account: u64, print: [u8; 32]) {
        let Ok(mut seen) = self.0.lock() else { return };
        let (set, order) = &mut *seen;
        if !set.insert((account, print)) {
            return;
        }
        order.push_back((account, print));
        while order.len() > SEEN_MAX {
            if let Some(oldest) = order.pop_front() {
                set.remove(&oldest);
            }
        }
    }

    pub fn remove(&self, account: u64, print: &[u8; 32]) {
        let Ok(mut seen) = self.0.lock() else { return };
        let (set, order) = &mut *seen;
        if set.remove(&(account, *print)) {
            order.retain(|pair| *pair != (account, *print));
        }
    }

    pub fn len(&self) -> usize {
        self.0.lock().map_or(0, |seen| seen.0.len())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What a session shown is, by the cheap read path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Live {
    /// The data has it, and the restore number is this backend's: the thread that writes judges it.
    Yes,
    /// The restore number is not this backend's: the data was put back (12.3).
    PutBack,
    /// The data has no such session (or one ended, `kind` NULL).
    Ended,
}

/// The cheap read path, the known devices, and the key codes are fingerprinted with.
pub struct Admission {
    reader: Mutex<Connection>,
    pub seen: Seen,
    codes: CodeKey,
    clock: Clock,
}

impl Admission {
    /// At the start, from the keeper, before the thread that writes takes it: a connection that
    /// only reads the data, and the devices every account knows now.
    pub fn open(keeper: &mut Keeper, clock: Clock) -> Result<Admission, String> {
        let reader = crate::store::data::open_to_read(keeper.data().path(), "The data")?;
        let numbers = super::rows::account_numbers(keeper.data().connection()).map_err(|_| "The accounts couldn't be read from the data.".to_string())?;
        let mut known = Vec::new();
        for number in numbers {
            let account = keeper.load(number).map_err(|_| "An account couldn't be read.".to_string())?;
            let rows = account.rows();
            for (id, device) in account.apart().devices {
                if let (true, Some(row)) = (device.known, rows.devices.get(&id)) {
                    known.push((number, *row.secret.as_bytes()));
                }
            }
        }
        Ok(Admission { reader: Mutex::new(reader), seen: Seen::new(known), codes: CodeKey::of(keeper), clock })
    }

    fn read<T>(&self, query: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Option<T> {
        let reader = self.reader.lock().ok()?;
        query(&reader).ok()
    }

    /// The session `hash`, shown under the restore number `restore`.
    pub fn live(&self, restore: u64, hash: &Fingerprint) -> Live {
        let found = self.read(|data| {
            let current: i64 = data.query_row("SELECT restore FROM meta WHERE one = 1", [], |row| row.get(0))?;
            let session = data.query_row("SELECT 1 FROM sessions WHERE hash = ?1 AND kind IS NOT NULL", [hash.as_bytes().as_slice()], |_| Ok(())).optional()?;
            Ok((current, session.is_some()))
        });
        match found {
            Some((current, _)) if u64::try_from(current).ok() != Some(restore) => Live::PutBack,
            Some((_, true)) => Live::Yes,
            // Not readable now: the thread that writes judges it, as before.
            None => Live::Yes,
            Some((_, false)) => Live::Ended,
        }
    }

    /// Whether the session `hash` is in the data, whatever restore number it is shown under.
    fn resolves(&self, hash: &Fingerprint) -> bool {
        self.read(|data| data.query_row("SELECT 1 FROM sessions WHERE hash = ?1 AND kind IS NOT NULL", [hash.as_bytes().as_slice()], |_| Ok(())).optional())
            .is_some_and(|found| found.is_some())
    }

    /// Whether the device with `secret` is one the account named `email` knows now.
    pub fn known_device(&self, email: &Email, secret: &DeviceSecret) -> bool {
        let Some(Some(number)) = self.read(|data| crate::store::data::account_named_of(data, email.as_str())) else { return false };
        self.seen.has(number, Fingerprint::of(secret.expose_bytes()).as_bytes())
    }

    /// Whether `code` is a one-time code the data has, and not past its time.
    fn code_live(&self, code: &OneTimeCode) -> bool {
        let print = self.codes.print(code);
        let now = (self.clock)();
        self.read(|data| data.query_row("SELECT until FROM codes WHERE code = ?1", [print.as_slice()], |row| row.get::<_, i64>(0)).optional())
            .flatten()
            .is_some_and(|until| u64::try_from(until).is_ok_and(|until| now < until))
    }

    /// Whether `note`'s secret is the one that can end the session it names. Only such a note
    /// is handed to the thread that writes; every other is "that session has ended" at once,
    /// the same bytes as a note the thread found wrong (the review of S8, B1).
    pub fn note_matches(&self, note: &SignOutNote) -> bool {
        let given = Fingerprint::of(note.secret.expose_bytes());
        let Ok(id) = i64::try_from(note.session.get()) else { return false };
        match self.read(|data| data.query_row("SELECT sign_out FROM sessions WHERE id = ?1", [id], |row| row.get::<_, Vec<u8>>(0)).optional()) {
            Some(Some(kept)) => kept.as_slice() == given.as_bytes().as_slice(),
            Some(None) => false,
            // Not readable now: the thread that writes judges it.
            None => true,
        }
    }

    /// The listener's question (C1), before a work slot is taken: from the request's `target`,
    /// its `Authorization` header(s) and, for a sign-in, its `body`.
    pub fn listener(&self, target: &str, authorization: &[&[u8]], body: &[u8]) -> Admitted {
        if !authorization.is_empty() {
            let shown = match authorization {
                [one] => std::str::from_utf8(one).ok().and_then(account::Session::from_authorization),
                _ => None,
            };
            return match shown {
                Some(session) if self.resolves(&Fingerprint::of(session.expose_bytes())) => Admitted::Reserved,
                _ => Admitted::Unresolved,
            };
        }
        if target == account::SIGN_IN
            && let Ok(sign_in) = account::read::<SignIn>(body)
        {
            let privileged = match &sign_in.credential {
                Credential::Password { email, .. } => self.known_device(email, &sign_in.device_secret),
                Credential::Code { code, .. } => self.code_live(code),
            };
            if privileged {
                return Admitted::Reserved;
            }
        }
        Admitted::Plain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_record_of_known_devices_is_bounded_and_belongs_to_one_account() {
        let seen = Seen::default();
        seen.add(1, [1; 32]);
        assert!(seen.has(1, &[1; 32]));
        // The same device's secret, at another account: not known there.
        assert!(!seen.has(2, &[1; 32]));
        seen.remove(1, &[1; 32]);
        assert!(!seen.has(1, &[1; 32]));
        for n in 0..(SEEN_MAX as u64 + 10) {
            seen.add(n, [7; 32]);
        }
        assert_eq!(seen.len(), SEEN_MAX);
        assert!(!seen.has(0, &[7; 32]), "the longest-held was let go");
        assert!(seen.has(SEEN_MAX as u64 + 9, &[7; 32]));
    }
}
