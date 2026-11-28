//! The music login, typed in by an admin (design 9; slice S9). **Every decision is the shared
//! library's**: `kept` gives and spends the admin's grant and installs the login under the
//! store's count; `music` builds the music server's two questions whole, reads their answers,
//! seals the password bound to this backend, the account, the address it was checked at, the
//! name and the count, and opens it only to hand out a fresh salt and token (`hand_out`). This
//! carries them out: the store, the thread that writes, the clock, random bytes, and the
//! music server's client ([`crate::music`]).
//!
//! - **Only an admin setting a login, with a grant, causes the music server to be asked**
//!   ([`Accounts::set_music_login`]): there is no other way to it, and it needs a session.
//! - **One address is ever asked** (the lead's ruling of 28 Nov 15:38): the container's
//!   `NEXUS_MUSIC_SERVER` (plain HTTP allowed: it is inside the container's network). A login
//!   is checked there and nowhere else. **The address people use** is a plain HTTPS address
//!   the admin sets with a grant ([`Accounts::set_music_server`]; its form is core's
//!   `MusicAddress`): the backend **never connects to it**; it is handed to devices as it is.
//!   A refusal is told apart from "could not reach it", and neither touches the login the
//!   account had.
//! - **A change of `NEXUS_MUSIC_SERVER`** between starts makes every login "cannot be used
//!   now" until it is checked again (typed again) at the new address ([`hand`]; core's seal is
//!   bound to it). A change of the address people use changes no login: devices are handed the
//!   new one.
//! - **Handed out** ([`hand`]): to a device, at sign-in and when it asks, a salt made now and
//!   its token, or why not ("none set yet", "cannot be used now"). Never to the admin's page.
//! - **Forcing a device out does not change this login** (9.5): nothing here is called by it.
//!
//! The typed password is in memory only while it is checked and sealed, in types that wipe
//! it, and is in no log line, error or `Debug`.

use nexus_core::nexus::account::{self, AccountId, MusicLoginResult, MusicSalt, PasswordDoor, RequestHead, SALT_BYTES};
use nexus_core::nexus::admin::{self, Applied, Denial, GetMusicServer, InternalAddress, MusicServer, NamedChange, Refusal, Reply, RequestGrant, SetMusicLogin, SetMusicServer};
use nexus_core::nexus::kept::{AccountState, AdminAction, AdminTarget, Bearer, Denied, Fingerprint, GrantUse, PasswordCheck, SessionKind};
use nexus_core::nexus::music::{self, NONCE_BYTES, Outcome, Ping, Provisioning, SealingKey};
use zeroize::Zeroizing;

use super::check::Checked as Password;
use super::rows;
use super::session::{Accounts, Answer, speaks_with};
use super::setup::random;
use crate::music::MusicClient;
use crate::store::keeper::Keeper;

/// How many logins may be being checked at the music server at once. Each holds a listener
/// slot while it waits; this keeps them well below the listener's (S9-3): more are told "busy"
/// at once, before their grant is spent.
pub const CHECKS_AT_ONCE: usize = 2;

/// The longest a login's check may take, both questions together: within the listener's own
/// deadline for an answer ([`crate::bounds::ANSWER_DEADLINE`]), so that the admin is told "not
/// dealt with" and the slot is let go before it (S9-3).
pub const CHECK_WITHIN: std::time::Duration = std::time::Duration::from_secs(8);

/// What the backend reaches the music server with: its client, and the address the container
/// was given (`NEXUS_MUSIC_SERVER`, never set from the page).
pub struct MusicSide {
    pub client: MusicClient,
    pub internal: String,
    checks: std::sync::Arc<tokio::sync::Semaphore>,
    within: std::time::Duration,
}

impl MusicSide {
    pub fn new(client: MusicClient, internal: String) -> MusicSide {
        MusicSide { client, internal, checks: std::sync::Arc::new(tokio::sync::Semaphore::new(CHECKS_AT_ONCE)), within: CHECK_WITHIN }
    }

    /// The same, with its own time for a check (the tests').
    pub fn within(mut self, within: std::time::Duration) -> MusicSide {
        self.within = within;
        self
    }
}

/// An admin call's answer, of `nexus::admin`'s shapes. A reply that cannot be encoded is "not
/// dealt with", which always can.
pub(super) fn answer<T: admin::ResultBody>(reply: Reply<T>) -> Answer {
    let status = reply.status();
    match admin::encode(&reply) {
        Ok(body) => Answer { status, body },
        Err(_) => refused(Refusal::NotDealtWith),
    }
}

pub fn refused(refusal: Refusal) -> Answer {
    let reply: Reply<Applied> = Reply::Refused(refusal);
    let status = reply.status();
    Answer { status, body: admin::encode(&reply).expect("a refusal always fits its 4 KiB") }
}

pub(super) fn not_dealt_with<E>(_: E) -> Refusal {
    Refusal::NotDealtWith
}

fn busy() -> Refusal {
    Refusal::Busy { retry_after_seconds: account::Wait::new(1) }
}

/// What a refusal of kept's is said as, to an admin's call.
pub(super) fn said(denied: Denied) -> Refusal {
    match denied {
        Denied::NotSignedIn => Refusal::NotSignedIn(account::NotSignedIn::SessionEnded),
        Denied::StartAgain => Refusal::NotSignedIn(account::NotSignedIn::DataPutBack),
        Denied::WrongPassword => Refusal::NotAllowed(Denial::WrongPassword),
        Denied::LeaveRequired => Refusal::NotAllowed(Denial::GrantRequired),
        // The account or the login changed since the grant was given (its revision, its
        // count, the address people use): the page asks again.
        Denied::WrongPurpose => Refusal::NotAllowed(Denial::Conflict),
        Denied::AdminRequired | Denied::AccountOff | Denied::HistoryHeld => Refusal::NotAllowed(Denial::Denied),
        Denied::Time => busy(),
        Denied::Exhausted | Denied::ReusedSession | Denied::DeviceInUse => Refusal::NotDealtWith,
    }
}

/// What came of asking the music server, as the page is told it: "wrong name or password"
/// (or a name that is not the one typed, letter for letter) is a refusal of the login; "could
/// not reach it" (or an answer that is no answer) is "not dealt with, later". Neither keeps
/// anything, and neither touches the login the account had.
fn asked(outcome: Outcome) -> Refusal {
    match outcome {
        Outcome::Refused | Outcome::NameDiffers => Refusal::NotAllowed(Denial::MusicLoginRefused),
        Outcome::CouldNotAsk | Outcome::Unusable => Refusal::NotDealtWith,
    }
}

pub(super) fn flatten(done: Result<Result<Answer, Refusal>, super::desk::Busy>) -> Answer {
    match done {
        Ok(Ok(answer)) => answer,
        Ok(Err(refusal)) => refused(refusal),
        Err(super::desk::Busy) => refused(busy()),
    }
}

/// The signed-in admin of a call: the account and device of the session `hash`, under the
/// restore number and speaking a version in common.
pub(super) fn caller(keeper: &mut Keeper, head: &RequestHead, hash: &Fingerprint) -> Result<(u64, account::DeviceId, nexus_core::nexus::kept::Account), Refusal> {
    if !speaks_with(&head.speaks) {
        return Err(Refusal::Malformed);
    }
    if head.restore != keeper.restore() {
        return Err(Refusal::NotSignedIn(account::NotSignedIn::DataPutBack));
    }
    let (number, device) = rows::session_by_hash(keeper.data().connection(), hash.as_bytes())
        .map_err(not_dealt_with)?
        .ok_or(Refusal::NotSignedIn(account::NotSignedIn::SessionEnded))?;
    let account = keeper.load(number).map_err(not_dealt_with)?;
    Ok((number, device, account))
}

/// S9-2 (the review of S9): the caller is a live admin, by kept's `admin_read`, **before**
/// anything of the target or of the backend's settings is looked at, so that a refusal says
/// nothing of which accounts there are or what is set.
pub(super) fn live_admin(keeper: &mut Keeper, account: &mut nexus_core::nexus::kept::Account, restore: u64, device: account::DeviceId, hash: &Fingerprint, now: u64) -> Result<(), Refusal> {
    account
        .request(restore, keeper.clock(now), |turn| turn.admin_read(Bearer { device, hash, grant: None }))
        .apply(|effects| keeper.persist(effects, &[], now))
        .map_err(not_dealt_with)?
        .map_err(said)
}

/// The account `id` names, loaded, if it is not the caller's own; `Ok(None)` for the caller's.
pub(super) fn target(keeper: &mut Keeper, caller: u64, id: AccountId) -> Result<Option<nexus_core::nexus::kept::Account>, Refusal> {
    if id.get() == caller {
        return Ok(None);
    }
    if !rows::account_exists(keeper.data().connection(), id.get()).map_err(not_dealt_with)? {
        return Err(Refusal::NotAllowed(Denial::NoSuchAccount));
    }
    keeper.load(id.get()).map(Some).map_err(not_dealt_with)
}

/// 9.4: the music login for a device of the account `state` is, or why there is none: a salt
/// made now and its token, by `music::hand_out`, under the store's count, with the address
/// people use (none set: "cannot be used now" for a login there is). A login checked at another
/// `NEXUS_MUSIC_SERVER` than `internal` is not handed out (core's binding). Called on the thread
/// that writes, with the account loaded from the store.
pub fn hand(keeper: &Keeper, number: u64, state: &AccountState, kind: SessionKind, internal: &str) -> MusicLoginResult {
    let connection = keeper.data().connection();
    let Ok(public) = rows::music_public(connection) else { return MusicLoginResult::UnusableNow };
    let Ok(install) = rows::music_login(connection, keeper.backend(), number) else { return MusicLoginResult::UnusableNow };
    let Some(salt) = random::<SALT_BYTES>() else { return MusicLoginResult::UnusableNow };
    let key = SealingKey::new(keeper.seal_key());
    music::hand_out(state, kind, install.as_ref(), &key, internal, public.as_ref(), MusicSalt::from_random(*salt))
}

pub use nexus_core::nexus::admin::MusicState;

/// What the admin's page is told of an account's music login (S10b's `admin::Account.music`,
/// shape posted at `t3/nexus-s10` 059fbc8: `none_set | set | type_again`), by the very rule a
/// device is handed one by ([`hand`], as for an app's session): none set yet; one that would
/// be handed out; or one that cannot be used now and is to be typed again (9.4). Nothing of the
/// login itself is kept: the salt and token made to decide it are dropped at once.
pub fn music_state(keeper: &Keeper, number: u64, state: &AccountState, internal: &str) -> MusicState {
    match hand(keeper, number, state, SessionKind::App, internal) {
        MusicLoginResult::NoneSetYet => MusicState::NoneSet,
        MusicLoginResult::Login(_) => MusicState::Set,
        MusicLoginResult::UnusableNow => MusicState::TypeAgain,
    }
}

impl Accounts {
    /// C1 (the review of S8; the lead's ruling of 15:38): an admin's call has its session looked
    /// up on the cheap read path first; only one found there goes on to the thread that writes.
    pub(super) fn admin_live(&self, speaks: &nexus_core::nexus::proto::Speaks, restore: u64, hash: &Fingerprint) -> Result<(), Refusal> {
        if !speaks_with(speaks) {
            return Err(Refusal::Malformed);
        }
        match self.admission.live(restore, hash) {
            super::admit::Live::Yes => Ok(()),
            super::admit::Live::PutBack => Err(Refusal::NotSignedIn(account::NotSignedIn::DataPutBack)),
            super::admit::Live::Ended => Err(Refusal::NotSignedIn(account::NotSignedIn::SessionEnded)),
        }
    }

    /// 8.8: a grant for one named admin change, for the password typed again: checked at this
    /// device's own door, under its waits, as a password change's is. `kept` gives it, bound to
    /// who asked, the change, the exact thing it is done to, that thing's revision and the
    /// request's number.
    pub async fn admin_grant(&self, bearer: account::Session, request: RequestGrant) -> Answer {
        match self.granting(bearer, request).await {
            Ok(answer) => answer,
            Err(refusal) => refused(refusal),
        }
    }

    async fn granting(&self, bearer: account::Session, request: RequestGrant) -> Result<Answer, Refusal> {
        let account::GrantRequest { head, change, password } = request;
        let hash = Fingerprint::of(bearer.expose_bytes());
        self.admin_live(&head.speaks, head.restore, &hash)?;
        let clock = self.clock.clone();
        let (number, device, email, check, stored) = {
            let hash = hash.clone();
            let head = RequestHead { speaks: head.speaks, restore: head.restore };
            self.desk
                .ask(move |keeper| -> Result<(u64, account::DeviceId, account::Email, PasswordCheck, _), Refusal> {
                    let now = clock();
                    let (number, device, mut account) = caller(keeper, &head, &hash)?;
                    live_admin(keeper, &mut account, head.restore, device, &hash, now)?;
                    let selected = match change.action.target_account() {
                        Some(id) => match target(keeper, number, id)? {
                            Some(other) => Some(other.state().clone()),
                            None => Some(account.state().clone()),
                        },
                        None => None,
                    };
                    let intent = NamedChange::grant_intent(&change, selected.as_ref()).ok_or(Refusal::Malformed)?;
                    let stored = keeper.data().password(number).map_err(not_dealt_with)?;
                    let expected = stored.as_ref().map_or_else(|| Fingerprint::from_bytes([0; 32]), |kept| Fingerprint::from_bytes(kept.hash));
                    let email = account.state().email.clone();
                    let check = account
                        .request(head.restore, keeper.clock(now), |turn| turn.check_leave(Bearer { device, hash: &hash, grant: None }, intent, expected))
                        .apply(|effects| keeper.persist(effects, &[], now))
                        .map_err(not_dealt_with)?
                        .map_err(said)?;
                    Ok((number, device, email, check, stored))
                })
                .await
                .map_err(|_| busy())??
        };
        let checked = self.workers.check(&email, PasswordDoor::KnownDevice, Some(device), password, stored).await.map_err(|_| busy())?;
        let Password::Match { derived, .. } = checked else { return Err(Refusal::NotAllowed(Denial::WrongPassword)) };
        let clock = self.clock.clone();
        flatten_into(
            self.desk
                .ask(move |keeper| -> Result<Answer, Refusal> {
                    let now = clock();
                    let receipt = check.check(derived, keeper.clock(now)).map_err(said)?;
                    let mut account = keeper.load(number).map_err(not_dealt_with)?;
                    let grant = account::GrantSecret::from_bytes(*random::<32>().ok_or(Refusal::NotDealtWith)?);
                    account
                        .request(head.restore, keeper.clock(now), |turn| turn.grant_leave(receipt, &grant))
                        .apply(|effects| keeper.persist(effects, &[], now))
                        .map_err(not_dealt_with)?
                        .map_err(said)?;
                    Ok(answer(Reply::Done(account::GrantIssued { grant })))
                })
                .await,
        )
    }

    /// 9.3: the admin sets a person's music login, with a grant for exactly that account at its
    /// revision: the music server is asked at the container's address (`NEXUS_MUSIC_SERVER`)
    /// and nowhere else, and only if it says yes, with the same name, is the login sealed and
    /// kept, under the next count.
    pub async fn set_music_login(&self, bearer: account::Session, request: SetMusicLogin) -> Answer {
        match self.setting_music_login(bearer, request).await {
            Ok(answer) => answer,
            Err(refusal) => refused(refusal),
        }
    }

    async fn setting_music_login(&self, bearer: account::Session, request: SetMusicLogin) -> Result<Answer, Refusal> {
        let account::GrantedChange { head, request_id, change, grant } = request;
        let hash = Fingerprint::of(bearer.expose_bytes());
        self.admin_live(&head.speaks, head.restore, &hash)?;
        // S9-3: at most CHECKS_AT_ONCE under way; one more is "busy" at once, its grant unspent.
        let _checking = self.music.checks.clone().try_acquire_owned().map_err(|_| busy())?;
        let again = RequestHead { speaks: head.speaks, restore: head.restore };
        let clock = self.clock.clone();
        let target_id = change.account;
        // (1) On the thread that writes: the caller a live admin, the grant spent (kept's
        // `authorize_music`), and the sealing key.
        let (authorized, key) = {
            let hash = hash.clone();
            let grant = account::GrantSecret::from_bytes(*grant.expose_bytes());
            self.desk
                .ask(move |keeper| -> Result<_, Refusal> {
                    let now = clock();
                    let (number, device, mut account) = caller(keeper, &head, &hash)?;
                    live_admin(keeper, &mut account, head.restore, device, &hash, now)?;
                    let other = target(keeper, number, target_id)?;
                    let bearer = Bearer { device, hash: &hash, grant: Some(GrantUse { request_id, secret: &grant }) };
                    let authorized = account
                        .request(head.restore, keeper.clock(now), |turn| turn.authorize_music(bearer, other.as_ref()))
                        .apply(|effects| keeper.persist(effects, &[], now))
                        .map_err(not_dealt_with)?
                        .map_err(said)?;
                    Ok((authorized, Zeroizing::new(keeper.seal_key())))
                })
                .await
                .map_err(|_| busy())??
        };
        // (2) Here, off the thread that writes: the two questions, at the container's address
        // alone.
        let salt = random::<SALT_BYTES>().ok_or(Refusal::NotDealtWith)?;
        let nonce = random::<NONCE_BYTES>().ok_or(Refusal::NotDealtWith)?;
        let typed = Provisioning {
            internal: &self.music.internal,
            name: change.name,
            password: Zeroizing::new(change.password.expose().as_bytes().to_vec()),
            salt: MusicSalt::from_random(*salt),
            nonce: *nonce,
        };
        drop(change.password);
        let sealing = SealingKey::new(*key);
        let ping = Ping::provision(authorized, typed, &sealing).map_err(asked)?;
        // Both questions within CHECK_WITHIN (S9-3); past it, "could not reach it".
        let client = &self.music.client;
        let verified = tokio::time::timeout(self.music.within, async move {
            let pinged = {
                let post = ping.post();
                client.ask(&post).await
            };
            let user = ping.answer(reply(&pinged)).map_err(asked)?;
            let named = {
                let post = user.post();
                client.ask(&post).await
            };
            user.answer(reply(&named)).map_err(asked)
        })
        .await
        .map_err(|_| asked(Outcome::CouldNotAsk))??;
        // (3) On the thread that writes: installed only while the same grant's authority, the
        // same account at the same revision and count, and the same container's address are
        // current.
        let clock = self.clock.clone();
        let internal = self.music.internal.clone();
        let grant = account::GrantSecret::from_bytes(*grant.expose_bytes());
        flatten_into(
            self.desk
                .ask(move |keeper| -> Result<Answer, Refusal> {
                    let now = clock();
                    let (number, device, mut account) = caller(keeper, &again, &hash)?;
                    live_admin(keeper, &mut account, again.restore, device, &hash, now)?;
                    let mut other = target(keeper, number, target_id)?;
                    let bearer = Bearer { device, hash: &hash, grant: Some(GrantUse { request_id, secret: &grant }) };
                    account
                        .request(again.restore, keeper.clock(now), |turn| turn.install_music(bearer, verified, &internal, other.as_mut()))
                        .apply(|effects| keeper.persist(effects, &[], now))
                        .map_err(not_dealt_with)?
                        .map_err(said)?;
                    Ok(answer(Reply::Done(Applied {})))
                })
                .await,
        )
    }

    /// 9.3, 19.2: the admin sets the address people reach the music server at (HTTPS; its form
    /// checked by core's `MusicAddress`), with a grant bound to exactly that address. **Nothing
    /// connects to it**: it is handed to devices as it is, and no login changes with it. The
    /// backend's own address is the container's and is never set here.
    pub async fn set_music_server(&self, bearer: account::Session, request: SetMusicServer) -> Answer {
        let account::GrantedChange { head, request_id, change, grant } = request;
        let hash = Fingerprint::of(bearer.expose_bytes());
        if let Err(refusal) = self.admin_live(&head.speaks, head.restore, &hash) {
            return refused(refusal);
        }
        let clock = self.clock.clone();
        let done = self
            .desk
            .ask(move |keeper| -> Result<Answer, Refusal> {
                let now = clock();
                let (_, device, mut account) = caller(keeper, &head, &hash)?;
                let bearer = Bearer { device, hash: &hash, grant: Some(GrantUse { request_id, secret: &grant }) };
                account
                    .request(head.restore, keeper.clock(now), |turn| turn.administer(bearer, AdminAction::SetMusicAddress(change.public), AdminTarget::Backend, None))
                    .apply(|effects| keeper.persist(effects, &[], now))
                    .map_err(not_dealt_with)?
                    .map_err(said)?;
                Ok(answer(Reply::Done(Applied {})))
            })
            .await;
        flatten(done)
    }

    /// The music server's two addresses, for the page to show and the admin to confirm (9.3):
    /// the container's (`NEXUS_MUSIC_SERVER`) and the one people use, if set. An admin's read.
    pub async fn get_music_server(&self, bearer: account::Session, request: GetMusicServer) -> Answer {
        let hash = Fingerprint::of(bearer.expose_bytes());
        if let Err(refusal) = self.admin_live(&request.head.speaks, request.head.restore, &hash) {
            return refused(refusal);
        }
        let clock = self.clock.clone();
        let internal = self.music.internal.clone();
        let done = self
            .desk
            .ask(move |keeper| -> Result<Answer, Refusal> {
                let now = clock();
                let (_, device, mut account) = caller(keeper, &request.head, &hash)?;
                account
                    .request(request.head.restore, keeper.clock(now), |turn| turn.admin_read(Bearer { device, hash: &hash, grant: None }))
                    .apply(|effects| keeper.persist(effects, &[], now))
                    .map_err(not_dealt_with)?
                    .map_err(said)?;
                let public = rows::music_public(keeper.data().connection()).map_err(not_dealt_with)?;
                let internal = InternalAddress::new(&internal).ok_or(Refusal::NotDealtWith)?;
                Ok(answer(Reply::Done(MusicServer { internal, public })))
            })
            .await;
        flatten(done)
    }
}

/// An answer as core reads it.
fn reply(answered: &crate::music::Answered) -> Option<(u16, &[u8])> {
    answered.as_ref().map(|(status, body)| (*status, body.as_slice()))
}

fn flatten_into(done: Result<Result<Answer, Refusal>, super::desk::Busy>) -> Result<Answer, Refusal> {
    match done {
        Ok(done) => done,
        Err(super::desk::Busy) => Err(busy()),
    }
}
