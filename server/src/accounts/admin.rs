//! C9's account and device views and changes. The desk serializes the check,
//! the grant spend and the writes. Kept owns the authorization and its effects.

use nexus_core::nexus::account::{self, AccountId, DeviceId, DeviceKind, Email, Name, RequestHead, SessionId};
use nexus_core::nexus::admin::{
    self, AccountCreated, Accounts as AccountPage, AdminDevice, Applied, CodeDelivery, Denial,
    Devices, DiagnosticsDefault, ListAccounts, ListDevices, MusicState, NamedChange, PasswordReset,
    Refusal, Reply,
};
use nexus_core::nexus::kept::{Bearer, DataChange, Fingerprint, GrantUse, LeaveFor, SessionKind, Status};
use nexus_core::nexus::proto::Version;
use rusqlite::params;

use super::music::{answer, caller, flatten, hand, not_dealt_with, said, target};
use super::session::{Accounts, Answer};
use super::{rows, setup};
use crate::store::{data::Change, keeper::{First, Keeper}};

fn read_authority(keeper: &mut Keeper, head: &RequestHead, hash: &Fingerprint, now: u64) -> Result<(), Refusal> {
    let (_, device, mut actor) = caller(keeper, head, hash)?;
    actor
        .request(head.restore, keeper.clock(now), |turn| turn.admin_read(Bearer { device, hash, grant: None }))
        .apply(|effects| keeper.persist(effects, &[], now))
        .map_err(not_dealt_with)?
        .map_err(said)
}

fn cursor(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn unsigned(value: i64) -> Result<u64, Refusal> {
    value.try_into().map_err(not_dealt_with)
}

fn list_accounts(keeper: &mut Keeper, after: Option<AccountId>, internal: &str) -> Result<AccountPage, Refusal> {
    let mut rows = {
        let mut query = keeper.data().connection().prepare(
            "SELECT number, email, shown FROM accounts WHERE number > ?1 ORDER BY number LIMIT ?2",
        ).map_err(not_dealt_with)?;
        let rows = query.query_map(params![cursor(after.map_or(0, AccountId::get)), (admin::ACCOUNTS_PER_PAGE + 1) as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        }).map_err(not_dealt_with)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(not_dealt_with)?
    };
    let more = rows.len() > admin::ACCOUNTS_PER_PAGE;
    rows.truncate(admin::ACCOUNTS_PER_PAGE);
    let rows = rows.into_iter().map(|(number, email, shown)| {
        let number = unsigned(number)?;
        let decoded = keeper.load(number).map_err(not_dealt_with)?;
        let state = decoded.state();
        let music = match hand(keeper, number, state, SessionKind::App, internal) {
            account::MusicLoginResult::NoneSetYet => MusicState::NoneSet,
            account::MusicLoginResult::Login(_) => MusicState::Set,
            account::MusicLoginResult::UnusableNow => MusicState::TypeAgain,
        };
        Ok(admin::Account {
            id: state.id,
            email: Email::new(&email).ok_or(Refusal::NotDealtWith)?,
            name: Name::new(&shown).ok_or(Refusal::NotDealtWith)?,
            admin: state.admin,
            enabled: state.status == Status::On,
            requests_enabled: admin::REQUESTS_DEFAULT_ENABLED,
            music,
        })
    }).collect::<Result<_, _>>()?;
    AccountPage::new(rows, more).ok_or(Refusal::NotDealtWith)
}

fn list_devices(keeper: &Keeper, after: Option<admin::DeviceCursor>) -> Result<Devices, Refusal> {
    let mut query = keeper.data().connection().prepare(
        "SELECT devices.account, devices.id, devices.name, devices.kind, devices.version,
                max(devices.first_signed_in, coalesce(devices.last_heard, 0)), sessions.id
         FROM devices LEFT JOIN sessions ON sessions.device = devices.id
         WHERE (devices.account, devices.id) > (?1, ?2)
           AND (sessions.kind IS NULL OR sessions.kind = 'app')
         ORDER BY devices.account, devices.id LIMIT ?3",
    ).map_err(not_dealt_with)?;
    let (account, device) = after.map_or((0, 0), |after| (after.account.get(), after.device.get()));
    let rows = query.query_map(params![cursor(account), cursor(device), (admin::DEVICES_PER_PAGE + 1) as i64], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?,
            row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, i64>(5)?,
            row.get::<_, Option<i64>>(6)?))
    }).map_err(not_dealt_with)?;
    let mut rows = rows.map(|row| {
        let (account, device, name, kind, version, heard, session) = row.map_err(not_dealt_with)?;
        Ok(AdminDevice {
            account: AccountId::new(unsigned(account)?).ok_or(Refusal::NotDealtWith)?,
            device: DeviceId::new(unsigned(device)?).ok_or(Refusal::NotDealtWith)?,
            name: Name::new(&name).ok_or(Refusal::NotDealtWith)?,
            kind: if kind == "phone" { DeviceKind::Phone } else { DeviceKind::Pc },
            app_version: Version::parse(&version).ok_or(Refusal::NotDealtWith)?,
            last_seen_ms: unsigned(heard)?,
            session: session.map(unsigned).transpose()?.and_then(SessionId::new),
        })
    }).collect::<Result<Vec<_>, _>>()?;
    let more = rows.len() > admin::DEVICES_PER_PAGE;
    rows.truncate(admin::DEVICES_PER_PAGE);
    Devices::new(rows, more).ok_or(Refusal::NotDealtWith)
}

fn spend<T: admin::Change>(
    keeper: &mut Keeper,
    request: &admin::Granted<T>,
    hash: &Fingerprint,
    now: u64,
    reset_print: Option<&[u8; 32]>,
) -> Result<(), Refusal> {
    let (number, device, mut actor) = caller(keeper, &request.head, hash)?;
    let change = NamedChange::for_request(request);
    let mut other = change.action.target_account()
        .map(|id| target(keeper, number, id)).transpose()?.flatten();
    let state = change.action.target_account().map(|_| other.as_ref().unwrap_or(&actor).state());
    let intent = change.grant_intent(state).ok_or(Refusal::Malformed)?;
    let LeaveFor::Admin { action, target: named } = intent.change else { return Err(Refusal::Malformed) };
    let bearer = Bearer { device, hash, grant: Some(GrantUse { request_id: intent.request_id, secret: &request.grant }) };
    actor.request(request.head.restore, keeper.clock(now), |turn| turn.administer(bearer, action, named, other.as_mut()))
        .apply(|effects| {
            let extra = reset_print.and_then(|print| {
                let reset = effects.updates.iter().find(|update| update.change == DataChange::ResetPassword)?;
                let code = reset.data.account.code?;
                Some(Change::Code {
                    account: reset.data.account.id.get(), number: code.number, code: print,
                    made_at: effects.high_water, until: code.expires,
                })
            });
            keeper.persist(effects, extra.as_slice(), now)
        })
        .map_err(not_dealt_with)?
        .map_err(said)
}

impl Accounts {
    async fn read_admin<T: admin::ResultBody + Send + 'static>(
        &self, session: account::Session, head: RequestHead,
        read: impl FnOnce(&mut Keeper) -> Result<T, Refusal> + Send + 'static,
    ) -> Answer {
        let clock = self.clock.clone();
        flatten(self.desk.ask(move |keeper| {
            read_authority(keeper, &head, &Fingerprint::of(session.expose_bytes()), clock())?;
            read(keeper).map(|page| answer(Reply::Done(page)))
        }).await)
    }

    pub async fn admin_accounts(&self, session: account::Session, request: ListAccounts) -> Answer {
        let internal = self.music.internal.clone();
        self.read_admin(session, request.head, move |keeper| list_accounts(keeper, request.after, &internal)).await
    }

    pub async fn admin_devices(&self, session: account::Session, request: ListDevices) -> Answer {
        self.read_admin(session, request.head, move |keeper| list_devices(keeper, request.after)).await
    }

    pub async fn get_diagnostics_default(&self, session: account::Session, request: admin::GetDiagnosticsDefault) -> Answer {
        self.read_admin(session, request.head, |keeper| {
            let enabled = rows::diagnostics_default(keeper.data().connection()).map_err(not_dealt_with)?
                .unwrap_or(admin::DIAGNOSTICS_DEFAULT_ENABLED);
            Ok(DiagnosticsDefault { enabled })
        }).await
    }

    pub async fn admin_apply<T: admin::Change + Send + 'static>(&self, session: account::Session, request: admin::Granted<T>) -> Answer {
        let clock = self.clock.clone();
        flatten(self.desk.ask(move |keeper| {
            spend(keeper, &request, &Fingerprint::of(session.expose_bytes()), clock(), None)?;
            Ok(answer(Reply::Done(Applied {})))
        }).await)
    }

    pub async fn set_diagnostics_default(
        &self, session: account::Session, request: admin::SetDiagnosticsDefault,
    ) -> Answer {
        let clock = self.clock.clone();
        let diagnostics = self.desk.diagnostics.clone();
        flatten(self.desk.ask(move |keeper| {
            spend(keeper, &request, &Fingerprint::of(session.expose_bytes()), clock(), None)?;
            diagnostics.set(request.change.enabled);
            Ok(answer(Reply::Done(Applied {})))
        }).await)
    }

    pub async fn create_account(&self, session: account::Session, request: admin::CreateAccount) -> Answer {
        let clock = self.clock.clone();
        flatten(self.desk.ask(move |keeper| {
            let now = clock();
            // Persist the grant spend first. A crash between writes only loses a grant.
            spend(keeper, &request, &Fingerprint::of(session.expose_bytes()), now, None)?;
            let email = request.change.email.as_str();
            if keeper.account_named(email).map_err(not_dealt_with)?.is_some() {
                return Err(Refusal::NotAllowed(Denial::Conflict));
            }
            let (code, print) = setup::make_code(&setup::CodeKey::of(keeper)).ok_or(Refusal::NotDealtWith)?;
            let at = keeper.clock(now).effective();
            let until = at.saturating_add(account::CODE_LIFETIME_MS);
            let number = keeper.make_account(email, request.change.name.as_str(), false, First::Code(&print, until), at)
                .map_err(not_dealt_with)?;
            let account = AccountId::new(number).ok_or(Refusal::NotDealtWith)?;
            Ok(answer(Reply::Done(AccountCreated { account, code: CodeDelivery::Issued(code) })))
        }).await)
    }

    pub async fn reset_admin_password(&self, session: account::Session, request: admin::ResetPassword) -> Answer {
        let clock = self.clock.clone();
        flatten(self.desk.ask(move |keeper| {
            let (code, print) = setup::make_code(&setup::CodeKey::of(keeper)).ok_or(Refusal::NotDealtWith)?;
            spend(keeper, &request, &Fingerprint::of(session.expose_bytes()), clock(), Some(&print))?;
            Ok(answer(Reply::Done(PasswordReset { code: CodeDelivery::Issued(code) })))
        }).await)
    }
}
