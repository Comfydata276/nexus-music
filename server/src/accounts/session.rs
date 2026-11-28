//! Sessions (design 8.6 to 8.9, 11; slice S8c): signing in, with a password or a one-time
//! code and a new password; signing out, with the session or with the note that can only end
//! it; the list of a person's devices; forcing one out by the session it names; changing a
//! password. **Every decision is `kept`'s** (C3r): this finds what it decides with, hands it
//! in, keeps what it says on the thread that writes (store kept apart first), and makes the
//! answer of C7's shapes.
//!
//! The slow work is S8b's two workers (`check`), and is never done on the thread that writes:
//! a sign-in is three steps, (1) on the thread that writes, the account loaded and kept's
//! [`PasswordCheck`] made; (2) on a worker, the password checked; (3) on the thread that
//! writes again, the account loaded afresh and kept's sign-in made with the receipt, which
//! looks at the receipt's fences again inside the step that writes (8.6 step 4).
//!
//! Nothing worth protecting goes to a session that has ended: it is told which kind of ending
//! and nothing else (8.7). Nothing a person typed, no code, session, secret or salt is written
//! to the log or put in an error.

use std::sync::Arc;

use nexus_core::nexus::account::{
    self, AccountId, ChangePassword, Credential, DeviceId, DeviceSecret, DevicePage, DEVICES_PER_PAGE, Email, ForceOut, ForcedOut, ListDevices, MusicLoginResult,
    Name, NotAllowed, NotSignedIn, Notice, Password, PasswordChanged, PasswordDoor, Person, Receipt, Refusal, Reply, RequestHead, SignIn, SignOut, SignOutNote, SignOutSecret,
    SignedIn, SignedOut, Stamp,
};
use nexus_core::nexus::kept::{Access, Account, AccountState, Bearer, Denied, Effects, Fingerprint, FreshSession, LeaveFor, PasswordCheck, SessionTarget};
use nexus_core::nexus::proto::{BackendId, SPEAKS, Speaks};

use super::check::{Checked, PasswordWorkers};
use super::desk::{Busy, Desk};
use super::rows;
use super::setup::{CodeKey, SetUpCode, random};
use crate::store::data::{self, Change};
use crate::store::keeper::Keeper;

/// The clock every decision is made by: milliseconds on this machine's clock (kept takes the
/// later of it and the latest time the backend has acted at).
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The common passwords a new one may not be (8.2), one to a line.
pub const COMMON_PASSWORDS: &str = include_str!("common-passwords.txt");

/// What the backend says, in every sign-in's answer, it records (13.6).
/// CARRIED (the sentence is `nexus::words`', which does not exist yet).
pub const RECORDED: &str = "Your Nexus server keeps your devices, their names and when each was last used. It keeps no password, only a fingerprint of it.";

/// Everything signing in needs, shared by every request.
pub struct Accounts {
    pub desk: Desk,
    pub workers: PasswordWorkers,
    pub set_up_code: SetUpCode,
    pub clock: Clock,
    pub backend: BackendId,
}

/// An answer: its status, and its body (C7's shapes, encoded).
pub struct Answer {
    pub status: u16,
    pub body: account::WireBody,
}

/// Encodes a reply of one of C7's shapes. A reply that cannot be encoded (it would be over its
/// ceiling) is "not dealt with", which always can.
macro_rules! answer {
    ($reply:expr) => {{
        let reply = $reply;
        let status = reply.status();
        match account::encode(&reply) {
            Ok(body) => Answer { status, body },
            Err(_) => refused(Refusal::NotDealtWith),
        }
    }};
}


pub fn refused(refusal: Refusal) -> Answer {
    let reply: Reply<SignedOut> = Reply::Refused(refusal);
    let status = reply.status();
    let body = account::encode(&reply).expect("a refusal always fits its 4 KiB");
    Answer { status, body }
}

fn busy() -> Refusal {
    Refusal::Busy { retry_after_seconds: None }
}

impl From<Busy> for Refusal {
    fn from(_: Busy) -> Self {
        busy()
    }
}

fn ended() -> Refusal {
    Refusal::NotSignedIn(NotSignedIn::SessionEnded)
}

fn wrong() -> Refusal {
    Refusal::NotAllowed(NotAllowed::WrongLogin)
}

fn not_dealt_with<E>(_: E) -> Refusal {
    Refusal::NotDealtWith
}

/// What a refusal of kept's is said as, to a caller who is signed in or signing in.
fn said(denied: Denied) -> Refusal {
    match denied {
        Denied::NotSignedIn => ended(),
        Denied::StartAgain => Refusal::NotSignedIn(NotSignedIn::DataPutBack),
        Denied::AccountOff => Refusal::NotAllowed(NotAllowed::AccountOff),
        Denied::WrongPassword => wrong(),
        Denied::HistoryHeld | Denied::WrongPurpose | Denied::LeaveRequired | Denied::AdminRequired => Refusal::NotAllowed(NotAllowed::Denied),
        Denied::Time => busy(),
        Denied::Exhausted | Denied::ReusedSession | Denied::DeviceInUse => Refusal::NotDealtWith,
    }
}

/// CARRIED (asked of C7: a `Speaks` that says whether two meet). The caller and this backend
/// meet when they have a number in common.
fn speaks_with(caller: &Speaks) -> bool {
    caller.low() <= SPEAKS.high() && SPEAKS.low() <= caller.high()
}

fn incompatible() -> Refusal {
    Refusal::NotAllowed(NotAllowed::Incompatible { speaks: SPEAKS })
}

/// The stamp and receipt for an answer to an account (12.3): how far the data's changes have
/// got stands for how far the account's have.
fn receipt(keeper: &Keeper, state: &AccountState) -> Result<Receipt, Refusal> {
    let changes = keeper.data().meta().map_err(not_dealt_with)?.changes;
    let position = state.position(changes);
    let tag = keeper.stamp_key().stamp(&position);
    Ok(Receipt {
        stamp: Stamp { backend: position.backend, restore: position.restore, account: position.account, life: position.life, change: position.change, tag },
        music_login_count: state.music_login,
    })
}

fn fresh_bytes() -> Result<[u8; 32], Refusal> {
    random::<32>().map(|bytes| *bytes).ok_or(Refusal::NotDealtWith)
}

/// What the thread that writes found for a sign-in, before the slow work.
struct Prepared {
    number: u64,
    email: Email,
    check: PasswordCheck,
    stored: Option<data::Password>,
    /// The device this sign-in is for: the one the secret is known by, or the id it would get.
    device: DeviceId,
    known: bool,
}

/// What a sign-in leaves to be made and kept, once the password (or code) has been checked.
struct Made {
    session: account::Session,
    hash: Fingerprint,
    note: SignOutSecret,
    revoke: [u8; 32],
}

fn made() -> Result<Made, Refusal> {
    let session = account::Session::from_bytes(fresh_bytes()?);
    let hash = Fingerprint::of(session.expose_bytes());
    let note = SignOutSecret::from_bytes(fresh_bytes()?);
    let revoke = *Fingerprint::of(note.expose_bytes()).as_bytes();
    Ok(Made { session, hash, note, revoke })
}

/// How a device describes itself, kept beside it in the data.
struct Described {
    name: Name,
    kind: &'static str,
    version: String,
}

impl Described {
    fn of(sign_in: &SignIn) -> Self {
        let kind = match sign_in.device_kind {
            account::DeviceKind::Pc => "pc",
            account::DeviceKind::Phone => "phone",
        };
        let version = format!("{}.{}.{}", sign_in.app_version.major, sign_in.app_version.minor, sign_in.app_version.patch);
        Described { name: sign_in.device_name.clone(), kind, version }
    }
}

impl Accounts {
    /// 8.6: one call, from the app's form. With a password, or with a one-time code and the new
    /// password chosen with it (8.5).
    pub async fn sign_in(&self, request: SignIn) -> Answer {
        match self.signing_in(request).await {
            Ok(answer) => answer,
            Err(refusal) => refused(refusal),
        }
    }

    async fn signing_in(&self, request: SignIn) -> Result<Answer, Refusal> {
        if !speaks_with(&request.speaks) {
            return Err(incompatible());
        }
        let described = Described::of(&request);
        let secret = request.device_secret.clone();
        match request.credential {
            Credential::Password { email, password } => self.with_password(email, password, secret, described).await,
            Credential::Code { code, new_password } => self.with_code(code, new_password, secret, described).await,
        }
    }

    async fn with_password(&self, email: Email, password: Password, secret: DeviceSecret, described: Described) -> Result<Answer, Refusal> {
        let clock = self.clock.clone();
        let (asked, secret_for_check) = (email.clone(), secret.clone());
        // (1) On the thread that writes: kept's check, bound to the account and device as they are.
        let prepared = self
            .desk
            .ask(move |keeper| -> Result<Option<Prepared>, Refusal> {
                let now = clock();
                let Some(number) = keeper.account_named(asked.as_str()).map_err(not_dealt_with)? else {
                    // 8.3: an email with no account does the same work here as one with an
                    // account: kept's decision keeps the clock (the store kept apart written as
                    // for any decision), so this keeps it too. Measured without it: 19 ms
                    // against 42 ms, which told the two apart (A1).
                    let effects = Effects { high_water: keeper.clock(now).effective(), updates: Vec::new() };
                    let _ = keeper.data().password(0);
                    keeper.persist(&effects, &[], now).map_err(not_dealt_with)?;
                    return Ok(None);
                };
                let mut account = keeper.load(number).map_err(not_dealt_with)?;
                let stored = keeper.data().password(number).map_err(not_dealt_with)?;
                // With no password kept, nothing can match: the same slow work is done (S8b).
                let expected = stored.as_ref().map_or_else(|| Fingerprint::from_bytes(fresh_bytes().unwrap_or([0; 32])), |kept| Fingerprint::from_bytes(kept.hash));
                let vacant = DeviceId::new(keeper.vacant_device()).ok_or(Refusal::NotDealtWith)?;
                let print = Fingerprint::of(secret_for_check.expose_bytes());
                let known = account.rows().devices.iter().find(|(_, row)| row.secret == print).map(|(id, _)| *id);
                let check = account
                    .sign_in_turn(keeper.clock(now), |turn| turn.check_sign_in(&secret_for_check, vacant, expected))
                    .apply(|effects| keeper.persist(effects, &[], now))
                    .map_err(not_dealt_with)?
                    .map_err(said)?;
                let device = known.unwrap_or(vacant);
                Ok(Some(Prepared { number, email: asked, known: check.known_device(), check, stored, device }))
            })
            .await??;
        // (2) On a worker: the slow check, through the door kept's check says (8.3). An email
        // with no account is checked against nothing, at the other door, the same work.
        let Some(prepared) = prepared else {
            let _ = self.workers.check(&email, PasswordDoor::Other, None, password, None).await?;
            return Err(wrong());
        };
        let door = PasswordDoor::for_device(prepared.known);
        let device = (door == PasswordDoor::KnownDevice).then_some(prepared.device);
        let checked = self.workers.check(&prepared.email, door, device, password, prepared.stored).await?;
        let Checked::Match { derived, replacement } = checked else { return Err(wrong()) };
        // (3) On the thread that writes: the sign-in, made in the one step that looks again.
        let clock = self.clock.clone();
        self.desk
            .ask(move |keeper| {
                let now = clock();
                let receipt = prepared.check.check(derived, keeper.clock(now)).map_err(said)?;
                let mut extra_passwords = Vec::new();
                if let Some(replacement) = replacement {
                    extra_passwords.push(replacement);
                }
                finish_sign_in(keeper, prepared.number, prepared.device, &secret, &described, &extra_passwords, now, |turn, fresh, previous| turn.sign_in(receipt, fresh, previous))
            })
            .await?
    }

    async fn with_code(&self, code: account::OneTimeCode, new_password: Password, secret: DeviceSecret, described: Described) -> Result<Answer, Refusal> {
        // The code's own door: ten a second in all (8.3), taken before it is looked up.
        self.workers.admit_code()?;
        let clock = self.clock.clone();
        // Whose code it is, and that it is live and in time.
        let found = self
            .desk
            .ask(move |keeper| -> Result<Option<(u64, u64, Email, Fingerprint)>, Refusal> {
                let now = keeper.clock(clock()).effective();
                let print = CodeKey::of(keeper).print(&code);
                let Some((number, code_number, until)) = rows::code(keeper.data().connection(), &print).map_err(not_dealt_with)? else { return Ok(None) };
                if now >= until {
                    return Ok(None);
                }
                let state = keeper.load(number).map_err(not_dealt_with)?.state().clone();
                Ok(Some((number, code_number, state.email, Fingerprint::from_bytes(print))))
            })
            .await??;
        let Some((number, code_number, email, print)) = found else { return Err(wrong()) };
        if !new_password.allowed_for(&email, COMMON_PASSWORDS) {
            return Err(Refusal::NotAllowed(NotAllowed::PasswordNotAllowed));
        }
        // The new password's fingerprint, before the code is spent: the code is a live one, so
        // this is the known-device worker's, one at a time for an account, with no wait (the
        // lead's ruling of 14:06; a stranger at the other door cannot stop it).
        let stored = self.workers.fingerprint(&email, PasswordDoor::RedeemedCode, None, new_password).await?;
        let clock = self.clock.clone();
        self.desk
            .ask(move |keeper| {
                let now = clock();
                let mut account = keeper.load(number).map_err(not_dealt_with)?;
                let vacant = DeviceId::new(keeper.vacant_device()).ok_or(Refusal::NotDealtWith)?;
                let mine = Fingerprint::of(secret.expose_bytes());
                let device = account.rows().devices.iter().find(|(_, row)| row.secret == mine).map_or(vacant, |(id, _)| *id);
                let check = account
                    .sign_in_turn(keeper.clock(now), |turn| turn.check_code(&secret, vacant, code_number, print.clone()))
                    .apply(|effects| keeper.persist(effects, &[], now))
                    .map_err(not_dealt_with)?
                    .map_err(said)?;
                let receipt = check.check(print, keeper.clock(now)).map_err(said)?;
                let verifier = Fingerprint::from_bytes(stored.hash);
                finish_sign_in(keeper, number, device, &secret, &described, std::slice::from_ref(&stored), now, |turn, fresh, previous| {
                    turn.redeem_code(receipt, verifier, fresh, previous)
                })
            })
            .await?
    }

    /// Section 11, 8.7: signing out on this session, or with the note that can end it and do
    /// nothing else (and needs no session).
    pub async fn sign_out(&self, bearer: Option<account::Session>, request: SignOut) -> Answer {
        let clock = self.clock.clone();
        let done = self
            .desk
            .ask(move |keeper| -> Result<Answer, Refusal> {
                let now = clock();
                match (request.revoke, bearer) {
                    (Some(note), _) => by_note(keeper, &request.head, note, now),
                    (None, Some(session)) => {
                        let hash = Fingerprint::of(session.expose_bytes());
                        let (number, device, mut account) = signed_in(keeper, &request.head, &hash)?;
                        account
                            .request(request.head.restore, keeper.clock(now), |turn| turn.sign_out(Bearer { device, hash: &hash, grant: None }))
                            .apply(|effects| keeper.persist(effects, &[], now))
                            .map_err(not_dealt_with)?
                            .map_err(said)?;
                        let _ = number;
                        let receipt = receipt(keeper, account.state())?;
                        Ok(answer!(Reply::Done { result: SignedOut {}, receipt }))
                    }
                    (None, None) => Err(ended()),
                }
            })
            .await;
        flatten(done)
    }

    /// 8.7: the list of a person's devices, each with the id of its session.
    pub async fn devices(&self, bearer: account::Session, request: ListDevices) -> Answer {
        let clock = self.clock.clone();
        let done = self
            .desk
            .ask(move |keeper| -> Result<Answer, Refusal> {
                let now = clock();
                let hash = Fingerprint::of(bearer.expose_bytes());
                let (number, device, mut account) = arrived(keeper, &request.head, &hash, Access::Read, now)?;
                let (listed, more) = rows::devices(keeper.data().connection(), number, request.after, DEVICES_PER_PAGE).map_err(not_dealt_with)?;
                let devices = listed
                    .into_iter()
                    .map(|row| account::Device {
                        id: row.id,
                        name: row.name,
                        kind: row.kind,
                        app_version: row.version,
                        last_seen_ms: row.last_seen,
                        this_device: row.id == device,
                        session: row.session,
                    })
                    .collect();
                let page = DevicePage::new(devices, more).ok_or(Refusal::NotDealtWith)?;
                let receipt = receipt(keeper, account.state())?;
                let _ = &mut account;
                Ok(answer!(Reply::Done { result: page, receipt }))
            })
            .await;
        flatten(done)
    }

    /// 8.7: forcing one of one's own other devices out, by the session it names. Sent again
    /// after that device has signed in afresh, it ends nothing. Needs no grant (8.8).
    pub async fn force_out(&self, bearer: account::Session, request: ForceOut) -> Answer {
        let clock = self.clock.clone();
        let done = self
            .desk
            .ask(move |keeper| -> Result<Answer, Refusal> {
                let now = clock();
                let hash = Fingerprint::of(bearer.expose_bytes());
                let (_, device, mut account) = signed_in(keeper, &request.head, &hash)?;
                let target = SessionTarget { device: request.device, session: request.session };
                account
                    .request(request.head.restore, keeper.clock(now), |turn| turn.force_out(Bearer { device, hash: &hash, grant: None }, target))
                    .apply(|effects| keeper.persist(effects, &[], now))
                    .map_err(not_dealt_with)?
                    .map_err(said)?;
                heard(keeper, device, now);
                let receipt = receipt(keeper, account.state())?;
                // Forcing a device out does not change its music login (9.5).
                Ok(answer!(Reply::Done { result: ForcedOut { music_cut_off: false }, receipt }))
            })
            .await;
        flatten(done)
    }

    /// 9.4: the music login for this device, or why there is none. Until S9, there is none.
    pub async fn music_login(&self, bearer: account::Session, head: RequestHead) -> Answer {
        let clock = self.clock.clone();
        let done = self
            .desk
            .ask(move |keeper| -> Result<Answer, Refusal> {
                let now = clock();
                let hash = Fingerprint::of(bearer.expose_bytes());
                let (_, _, account) = arrived(keeper, &head, &hash, Access::Read, now)?;
                let receipt = receipt(keeper, account.state())?;
                Ok(answer!(Reply::Done { result: MusicLoginResult::NoneSetYet, receipt }))
            })
            .await;
        flatten(done)
    }

    /// 8.9: the present password and a new one, in one call. Proving the present one is the
    /// grant for this change (kept gives it and spends it within the call); every other session
    /// of the person ends.
    pub async fn change_password(&self, bearer: account::Session, request: ChangePassword) -> Answer {
        match self.changing_password(bearer, request).await {
            Ok(answer) => answer,
            Err(refusal) => refused(refusal),
        }
    }

    async fn changing_password(&self, bearer: account::Session, request: ChangePassword) -> Result<Answer, Refusal> {
        let ChangePassword { head, current_password, new_password, .. } = request;
        let clock = self.clock.clone();
        let hash = Fingerprint::of(bearer.expose_bytes());
        let (number, device, email, check, stored) = {
            let hash = hash.clone();
            let head = RequestHead { speaks: head.speaks, restore: head.restore };
            self.desk
                .ask(move |keeper| -> Result<_, Refusal> {
                    let now = clock();
                    let (number, device, mut account) = signed_in(keeper, &head, &hash)?;
                    let stored = keeper.data().password(number).map_err(not_dealt_with)?;
                    let expected = stored.as_ref().map_or_else(|| Fingerprint::from_bytes([0; 32]), |kept| Fingerprint::from_bytes(kept.hash));
                    let email = account.state().email.clone();
                    let check = account
                        .request(head.restore, keeper.clock(now), |turn| turn.check_leave(Bearer { device, hash: &hash, grant: None }, LeaveFor::ChangePassword, expected))
                        .apply(|effects| keeper.persist(effects, &[], now))
                        .map_err(not_dealt_with)?
                        .map_err(said)?;
                    Ok((number, device, email, check, stored))
                })
                .await??
        };
        if !new_password.allowed_for(&email, COMMON_PASSWORDS) {
            return Err(Refusal::NotAllowed(NotAllowed::PasswordNotAllowed));
        }
        // A password typed again on a signed-in device is checked at that device's door, under
        // the same waits (the lead's ruling of 13:46).
        let door = PasswordDoor::KnownDevice;
        let Checked::Match { derived, .. } = self.workers.check(&email, door, Some(device), current_password, stored).await? else { return Err(wrong()) };
        let chosen = self.workers.fingerprint(&email, door, Some(device), new_password).await?;
        let clock = self.clock.clone();
        self.desk
            .ask(move |keeper| -> Result<Answer, Refusal> {
                let now = clock();
                let receipt_of_check = check.check(derived, keeper.clock(now)).map_err(said)?;
                let mut account = keeper.load(number).map_err(not_dealt_with)?;
                let grant = account::GrantSecret::from_bytes(fresh_bytes()?);
                let bearer = |grant| Bearer { device, hash: &hash, grant };
                account
                    .request(head.restore, keeper.clock(now), |turn| turn.grant_leave(receipt_of_check, &grant))
                    .apply(|effects| keeper.persist(effects, &[], now))
                    .map_err(not_dealt_with)?
                    .map_err(said)?;
                account
                    .request(head.restore, keeper.clock(now), |turn| turn.change_password(bearer(Some(&grant))))
                    .apply(|effects| keeper.persist(effects, &[Change::Password { account: number, password: Some(&chosen) }], now))
                    .map_err(not_dealt_with)?
                    .map_err(said)?;
                let receipt = receipt(keeper, account.state())?;
                Ok(answer!(Reply::Done { result: PasswordChanged {}, receipt }))
            })
            .await?
    }
}

fn flatten(done: Result<Result<Answer, Refusal>, Busy>) -> Answer {
    match done {
        Ok(Ok(answer)) => answer,
        Ok(Err(refusal)) => refused(refusal),
        Err(Busy) => refused(busy()),
    }
}

/// When a device was last heard from: the data alone keeps it (12.3). A failure here takes
/// nothing away and gives nothing, so it does not change the answer.
fn heard(keeper: &mut Keeper, device: DeviceId, now: u64) {
    let _ = keeper.heard(device.get(), now);
}

/// The account and device of the session `hash`, under the restore number the caller names.
/// A session the data no longer has has ended; a restore number other than this backend's
/// means the data was put back (12.3).
fn signed_in(keeper: &mut Keeper, head: &RequestHead, hash: &Fingerprint) -> Result<(u64, DeviceId, Account), Refusal> {
    if !speaks_with(&head.speaks) {
        return Err(incompatible());
    }
    if head.restore != keeper.restore() {
        return Err(Refusal::NotSignedIn(NotSignedIn::DataPutBack));
    }
    let (number, device) = rows::session_by_hash(keeper.data().connection(), hash.as_bytes()).map_err(not_dealt_with)?.ok_or_else(ended)?;
    let account = keeper.load(number).map_err(not_dealt_with)?;
    Ok((number, device, account))
}

/// A call accepted from a session (kept's `arrive`), and what that keeps.
fn arrived(keeper: &mut Keeper, head: &RequestHead, hash: &Fingerprint, access: Access, now: u64) -> Result<(u64, DeviceId, Account), Refusal> {
    let (number, device, mut account) = signed_in(keeper, head, hash)?;
    account
        .request(head.restore, keeper.clock(now), |turn| turn.arrive(Bearer { device, hash, grant: None }, access).map(|_| ()))
        .apply(|effects| keeper.persist(effects, &[], now))
        .map_err(not_dealt_with)?
        .map_err(said)?;
    heard(keeper, device, now);
    Ok((number, device, account))
}

/// Section 11: the note ends its one session and nothing else. A note for a session that has
/// gone is already done.
/// CARRIED (C3r's `Turn::sign_out_by_note` is on `t3/nexus-c3rb` `066116b`, not yet in the
/// base): the note's secret is checked against the fingerprint kept beside its session, and
/// then that session is ended as kept ends one signed out on.
fn by_note(keeper: &mut Keeper, head: &RequestHead, note: SignOutNote, now: u64) -> Result<Answer, Refusal> {
    if !speaks_with(&head.speaks) {
        return Err(incompatible());
    }
    if head.restore != keeper.restore() {
        // Every session was ended when the data was put back: so was this one.
        return Err(Refusal::NotSignedIn(NotSignedIn::DataPutBack));
    }
    let Some((number, device, hash, revoke)) = rows::session_by_id(keeper.data().connection(), note.session).map_err(not_dealt_with)? else {
        return Err(ended());
    };
    let given = Fingerprint::of(note.secret.expose_bytes());
    if revoke.map(Fingerprint::from_bytes) != Some(given) {
        return Err(ended());
    }
    let hash = Fingerprint::from_bytes(hash);
    let mut account = keeper.load(number).map_err(not_dealt_with)?;
    account
        .request(head.restore, keeper.clock(now), |turn| turn.sign_out(Bearer { device, hash: &hash, grant: None }))
        .apply(|effects| keeper.persist(effects, &[], now))
        .map_err(not_dealt_with)?
        .map_err(said)?;
    let receipt = receipt(keeper, account.state())?;
    Ok(answer!(Reply::Done { result: SignedOut {}, receipt }))
}

/// The step that writes a sign-in (8.6 step 4, 8.5): the account loaded afresh, a session made
/// with an id never given before, kept's sign-in with the receipt (which looks at its fences
/// again), and what it says kept, store first; then the answer.
#[allow(clippy::too_many_arguments)]
fn finish_sign_in(
    keeper: &mut Keeper,
    number: u64,
    device: DeviceId,
    secret: &DeviceSecret,
    described: &Described,
    passwords: &[data::Password],
    now: u64,
    step: impl FnOnce(nexus_core::nexus::kept::Turn<'_>, FreshSession, Option<&mut Account>) -> Result<nexus_core::nexus::kept::SessionGrant, Denied>,
) -> Result<Answer, Refusal> {
    let mut account = keeper.load(number).map_err(not_dealt_with)?;
    let made = made()?;
    let id = keeper.fresh_session().ok_or(Refusal::NotDealtWith)?;
    let fresh = FreshSession { id, hash: made.hash.clone() };
    // A device signed in to another account now is signed out of it by this sign-in (8.6).
    let print = Fingerprint::of(secret.expose_bytes());
    let other = rows::other_accounts_signed_in(keeper.data().connection(), print.as_bytes(), number).map_err(not_dealt_with)?.into_iter().next();
    let mut previous = match other {
        Some(other) => Some(keeper.load(other).map_err(not_dealt_with)?),
        None => None,
    };
    let mut extra = vec![
        Change::Describe { device: device.get(), name: described.name.as_str(), kind: described.kind, version: &described.version },
        Change::Revoke { device: device.get(), session: id.get(), revoke: &made.revoke },
    ];
    extra.extend(passwords.iter().map(|password| Change::Password { account: number, password: Some(password) }));
    let granted = account
        .sign_in_turn(keeper.clock(now), |turn| step(turn, fresh, previous.as_mut()))
        .apply(|effects| keeper.persist(effects, &extra, now))
        .map_err(not_dealt_with)?
        .map_err(said)?;
    if granted.device != device {
        // kept and this disagree about which device signed in: nothing more is said.
        return Err(Refusal::NotDealtWith);
    }
    heard(keeper, device, now);
    let state = account.state().clone();
    let shown = rows::shown(keeper.data().connection(), number).map_err(not_dealt_with)?.and_then(|shown| Name::new(&shown));
    let name = shown.unwrap_or_else(|| Name::new("You").expect("a fixed name is a name"));
    let answer = SignedIn {
        session: made.session,
        session_id: granted.session,
        sign_out: SignOutNote { session: granted.session, secret: made.note },
        person: Person { id: AccountId::new(number).ok_or(Refusal::NotDealtWith)?, revision: state.revision, email: state.email.clone(), name, admin: state.admin },
        device,
        restore: keeper.restore(),
        // Until S9 there is never a music login to hand out (the slice notes, S8).
        music: MusicLoginResult::NoneSetYet,
        notice: Notice::new(RECORDED).ok_or(Refusal::NotDealtWith)?,
    };
    let receipt = receipt(keeper, &state)?;
    Ok(answer!(Reply::Done { result: answer, receipt }))
}
