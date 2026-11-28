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

use nexus_core::nexus::account::{self, AccountId, Credential, DeviceId, DeviceSecret, Email, NotAllowed, NotSignedIn, OneTimeCode, Refusal, SessionId, SignIn, SignOutNote};
use nexus_core::nexus::kept::{Fingerprint, SessionKind};
use rusqlite::{Connection, OptionalExtension};

use super::desk::Admitted;
use super::session::{Answer, Clock, refused};
use crate::head::Callers;
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

/// Who is calling, by the session shown in a request's head, as the data has it now: found
/// before a work slot is taken and before a byte of the body is read. A route takes it as an
/// `axum::Extension<Caller>`, there only when the route needs a session and the check passed.
/// kept still judges the session on the thread that writes (its idle time, the restore number
/// the call names, a grant): this says only whose it is, and of which kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller {
    pub account: AccountId,
    pub device: DeviceId,
    pub session: SessionId,
    pub kind: SessionKind,
}

/// What a request's head shows, by the cheap read path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shown {
    /// No session, one not written as a session is, more than one, or one the data does not
    /// have (or has ended).
    NoOne,
    /// The data could not be read now: not "no one" (that would sign a device out), but "not
    /// dealt with, ask again".
    NotReadable,
    /// A session the data does not have, on data that has been put back from a copy (its
    /// restore number is not 0): it may be a device's from before, which must be told "the data
    /// was put back" (12.3, 13.3: it then sends its events again), not "session ended", and
    /// only the restore number in the body can say which. The route judges it
    /// (`Accounts::live`), with no `Caller`.
    Unproven,
    Caller(Caller),
}

/// THE check before a body is read, for every route (`serve::answer` calls it with the route's
/// [`Callers`] and what the head shows): `Ok(Some(caller))` for a route that needs a session
/// and was shown one of its kind; `Ok(None)` for one that needs none; otherwise the refusal to
/// send at once, nothing read or reserved for it. The bytes are the same on every route that
/// refuses for the same reason, whatever its body would have named. The one exception is
/// [`Shown::Unproven`] (after a put-back): `Ok(None)`, and the route judges it from its body.
pub fn before_body(callers: Callers, shown: Shown) -> Result<Option<Caller>, Answer> {
    let wanted = match callers {
        Callers::Anyone => return Ok(None),
        Callers::EitherOrNone => return Ok(match shown { Shown::Caller(caller) => Some(caller), _ => None }),
        Callers::App => SessionKind::App,
        Callers::Page => SessionKind::AdminPage,
    };
    match shown {
        Shown::NoOne => Err(refused(Refusal::NotSignedIn(NotSignedIn::SessionEnded))),
        Shown::NotReadable => Err(refused(Refusal::NotDealtWith)),
        Shown::Unproven => Ok(None),
        Shown::Caller(caller) if caller.kind == wanted => Ok(Some(caller)),
        Shown::Caller(_) => Err(refused(Refusal::NotAllowed(NotAllowed::Denied))),
    }
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

    /// Whose the session `hash` is, and its kind, whatever restore number it is shown under.
    fn caller(&self, hash: &Fingerprint) -> Shown {
        let found = self.read(|data| {
            data.query_row(
                "SELECT devices.account, sessions.device, sessions.id, sessions.kind FROM sessions JOIN devices ON devices.id = sessions.device WHERE sessions.hash = ?1 AND sessions.kind IS NOT NULL",
                [hash.as_bytes().as_slice()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?, row.get::<_, String>(3)?)),
            )
            .optional()
        });
        let number = |value: i64| u64::try_from(value).ok();
        match found {
            None => Shown::NotReadable,
            Some(None) => match self.read(|data| data.query_row("SELECT restore FROM meta WHERE one = 1", [], |row| row.get::<_, i64>(0))) {
                None => Shown::NotReadable,
                Some(0) => Shown::NoOne,
                Some(_) => Shown::Unproven,
            },
            Some(Some((account, device, session, kind))) => {
                let caller = (|| {
                    Some(Caller {
                        account: AccountId::new(number(account)?)?,
                        device: DeviceId::new(number(device)?)?,
                        session: SessionId::new(number(session)?)?,
                        kind: match kind.as_str() {
                            "app" => SessionKind::App,
                            "page" => SessionKind::AdminPage,
                            _ => return None,
                        },
                    })
                })();
                // A row not as the data writes one: not readable, never someone.
                caller.map_or(Shown::NotReadable, Shown::Caller)
            }
        }
    }

    /// What the `Authorization` header(s) of a request's head show.
    pub fn shown(&self, authorization: &[&[u8]]) -> Shown {
        let shown = match authorization {
            [one] => std::str::from_utf8(one).ok().and_then(account::Session::from_authorization),
            _ => None,
        };
        match shown {
            Some(session) => self.caller(&Fingerprint::of(session.expose_bytes())),
            None => Shown::NoOne,
        }
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
    /// With it, who the head shows ([`Shown`]), for [`before_body`].
    pub fn listener(&self, target: &str, authorization: &[&[u8]], body: &[u8]) -> (Admitted, Shown) {
        if !authorization.is_empty() {
            let shown = self.shown(authorization);
            let admitted = if matches!(shown, Shown::Caller(_)) { Admitted::Reserved } else { Admitted::Unresolved };
            return (admitted, shown);
        }
        (self.unshown(target, body), Shown::NoOne)
    }

    fn unshown(&self, target: &str, body: &[u8]) -> Admitted {
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

    /// The check's refusals are the same bytes whichever family of shapes the route answers in
    /// (an account's call, an admin's, events', Diagnostics'), so one encoding serves them all.
    #[test]
    fn the_check_refuses_in_bytes_every_route_shares() {
        use nexus_core::nexus::admin;
        for (ours, theirs) in [
            (Refusal::NotSignedIn(NotSignedIn::SessionEnded), admin::Refusal::NotSignedIn(NotSignedIn::SessionEnded)),
            (Refusal::NotAllowed(NotAllowed::Denied), admin::Refusal::NotAllowed(admin::Denial::Denied)),
            (Refusal::NotDealtWith, admin::Refusal::NotDealtWith),
        ] {
            let ours = refused(ours);
            let theirs = super::super::music::refused(theirs);
            assert_eq!((ours.status, ours.body.bytes()), (theirs.status, theirs.body.bytes()));
        }
    }

    #[test]
    fn the_check_lets_a_route_s_own_kind_through_and_no_one_else() {
        let caller = |kind| Caller { account: AccountId::new(1).unwrap(), device: DeviceId::new(2).unwrap(), session: SessionId::new(3).unwrap(), kind };
        let (app, page) = (Shown::Caller(caller(SessionKind::App)), Shown::Caller(caller(SessionKind::AdminPage)));
        let status = |result: Result<Option<Caller>, Answer>| result.map_err(|refused| refused.status);
        assert_eq!(status(before_body(Callers::App, app)), Ok(Some(caller(SessionKind::App))));
        assert_eq!(status(before_body(Callers::Page, page)), Ok(Some(caller(SessionKind::AdminPage))));
        assert_eq!(status(before_body(Callers::App, page)), Err(403));
        assert_eq!(status(before_body(Callers::Page, app)), Err(403));
        for callers in [Callers::App, Callers::Page] {
            assert_eq!(status(before_body(callers, Shown::NoOne)), Err(401));
            assert_eq!(status(before_body(callers, Shown::NotReadable)), Err(503));
        }
        assert_eq!(status(before_body(Callers::Anyone, app)), Ok(None));
        assert_eq!(status(before_body(Callers::Anyone, Shown::NoOne)), Ok(None));
        assert_eq!(status(before_body(Callers::EitherOrNone, page)), Ok(Some(caller(SessionKind::AdminPage))));
        assert_eq!(status(before_body(Callers::EitherOrNone, Shown::NoOne)), Ok(None));
    }

    /// A session the data does not have is "no one" (refused before its body) on data never put
    /// back; on data put back it is judged from its body (it may be a device's from before,
    /// which must be told "the data was put back", and then sends its events again).
    #[test]
    fn a_session_not_found_is_no_one_unless_the_data_was_put_back() {
        let root = std::env::temp_dir().join(format!("nexus-admit-unproven-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        let (mut keeper, _) = Keeper::open(&root.join("state"), &root.join("data"), 1_800_000_000_000).unwrap();
        let clock: Clock = std::sync::Arc::new(|| 1_800_000_000_000);
        let admission = Admission::open(&mut keeper, clock.clone()).unwrap();
        let shown = account::Session::from_bytes([42; 32]).authorization();
        let shown = [shown.as_str().as_bytes()];
        assert_eq!(admission.shown(&shown), Shown::NoOne);
        assert_eq!(admission.listener(account::LIST_DEVICES, &shown, b"").0, Admitted::Unresolved);
        assert_eq!(admission.shown(&[]), Shown::NoOne);
        assert_eq!(admission.shown(&[b"Bearer nonsense"]), Shown::NoOne);
        assert_eq!(admission.shown(&[shown[0], shown[0]]), Shown::NoOne, "two are none");
        keeper.data().connection().execute("UPDATE meta SET restore = 1 WHERE one = 1", []).unwrap();
        assert_eq!(admission.shown(&shown), Shown::Unproven);
        assert_eq!(before_body(Callers::App, Shown::Unproven).map_err(|refused| refused.status), Ok(None));
        // None shown, or one not written as a session is, is no one even then.
        assert_eq!(admission.shown(&[]), Shown::NoOne);
        assert_eq!(admission.shown(&[b"Bearer nonsense"]), Shown::NoOne);
        drop(admission);
        drop(keeper);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn every_route_is_the_page_s_or_an_app_s_by_its_path() {
        use crate::head::{ROUTES, Route};
        use nexus_core::nexus::{admin, proto};
        let callers = |path: &str| ROUTES.iter().find(|route| route.path == path).map(|route| route.callers);
        for path in [proto::HELLO, crate::admin::PAGE, account::SIGN_IN, account::SET_UP] {
            assert_eq!(callers(path), Some(Callers::Anyone), "{path}");
        }
        assert_eq!(callers(account::SIGN_OUT), Some(Callers::EitherOrNone));
        for path in [admin::REQUEST_GRANT, admin::SET_MUSIC_LOGIN, admin::SET_MUSIC_SERVER, admin::GET_MUSIC_SERVER, crate::diag::ADMIN_PATH] {
            assert_eq!(callers(path), Some(Callers::Page), "{path}");
        }
        for path in [
            account::LIST_DEVICES,
            account::FORCE_OUT,
            account::CHANGE_PASSWORD,
            account::MUSIC_LOGIN,
            account::GRANT,
            account::DELETE_LISTENING_DATA,
            nexus_core::nexus::events::SEND_PATH,
            nexus_core::nexus::events::DELETE_PATH,
            crate::events::STATS_PATH,
            crate::diag::SEND_PATH,
            crate::diag::LIST_PATH,
        ] {
            assert_eq!(callers(path), Some(Callers::App), "{path}");
        }
        // Every route of the backend's is one of these; a route added later under the admin's
        // calls is the page's without saying so.
        assert_eq!(Route::new(admin::SET_LIDARR, "POST", 1).callers, Callers::Page);
        assert_eq!(Route::new("/nexus/api/requests/ask", "POST", 1).callers, Callers::App);
        assert_eq!(Route::new("/nexus/api/administer", "POST", 1).callers, Callers::App);
    }

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
