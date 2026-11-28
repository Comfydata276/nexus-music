//! S3 serializes load, decide, apply and publish, including across accounts for
//! an admin change and across the backend for restore.

use super::receipt::Purpose;
use super::*;
use crate::nexus::music::{CurrentEndpoints, MusicInstallation, VerifiedLogin};
use std::collections::BTreeMap;

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

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct DeviceRow {
    pub number: u64,
    pub secret: Fingerprint,
    pub session: Option<Session>,
}

/// The design does not say how to recognize partial grants. The apart record
/// remembers which identity and leave the data may grant, without either hash.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Permit {
    pub id: SessionId,
    pub leave: Option<Leave>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ApartDevice {
    pub number: u64,
    pub last_session: u64,
    pub last_leave: u64,
    pub permit: Option<Permit>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Rows {
    pub account: AccountState,
    pub devices: BTreeMap<DeviceId, DeviceRow>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Apart {
    pub account: AccountState,
    pub devices: BTreeMap<DeviceId, ApartDevice>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum DataChange {
    Keep,
    ChangePassword,
    ResetPassword,
    RedeemCode {
        code: u64,
        verifier: Fingerprint,
    },
    EraseHistory,
    DeleteAccount,
    ApplyAdmin {
        action: AdminAction,
        target: AdminTarget,
    },
    InstallMusicLogin(MusicInstallation),
}

/// When credentials must be renewed, S3/S9 apply this alongside the data changes.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum MusicRecovery {
    RotateManaged,
    EnterAgain,
}

/// First persist the backend clock and every apart_first together, then commit data
/// and the action (such as replacing the verifier), then recognize apart_last.
/// If any store fails, publish no answer and reload before serving.
#[derive(Debug)]
pub struct Update {
    pub apart_first: Apart,
    pub data: Rows,
    pub change: DataChange,
    pub music: Option<MusicRecovery>,
    pub apart_last: Apart,
}

#[derive(Debug)]
pub struct Effects {
    pub high_water: u64,
    pub updates: Vec<Update>,
}

#[must_use = "apply the durable effects even when the request was refused"]
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

#[derive(Clone)]
pub struct Account {
    state: AccountState,
    devices: BTreeMap<DeviceId, Device>,
}

impl Account {
    /// S3 uses the whole affected account for changes that can touch every device.
    /// A tombstone needs no data row. Before serving an older copy call restore.
    pub fn load(apart: &Apart, data: Option<&Rows>) -> Self {
        let mut state = apart.account.clone();
        let mut devices = BTreeMap::new();
        let data = data.filter(|rows| state.same_account(&rows.account));
        if state.status != Status::Deleted {
            // The design does not say how to treat a password write interrupted
            // after its apart fence. The old verifier can never sign in again.
            let verifier_missing =
                data.is_none_or(|rows| state.password_changes != rows.account.password_changes);
            state.password_reset_required |= verifier_missing;
            if verifier_missing {
                state.code = state.code.map(|code| LiveCode { used: true, ..code });
            }
            if let Some(rows) = data {
                state.history_held |= state.life > rows.account.life;
                if rows.account.music_login != state.music_login {
                    state.music_recovery = Some(state.music_recovery());
                }
            }
        }
        for (&id, record) in &apart.devices {
            let row = data
                .filter(|_| state.status != Status::Deleted)
                .and_then(|rows| rows.devices.get(&id));
            let session = row.and_then(|row| {
                let stored = row.session.as_ref()?;
                let permit = record.permit.as_ref()?;
                let same_restore = data.is_some_and(|rows| rows.account.restore == state.restore);
                if !same_restore
                    || state.status != Status::On
                    || state.password_reset_required
                    || row.number != record.number
                    || permit.id != stored.id
                    || permit.id.get() > record.last_session
                {
                    return None;
                }
                let mut session = stored.clone();
                session.leave = session.leave.filter(|leave| {
                    Some(leave) == permit.leave.as_ref()
                        && leave.serial <= record.last_leave
                        && leave.revision == state.revision
                        && leave.admin == state.admin
                        && leave.life == state.life
                });
                Some(session)
            });
            devices.insert(
                id,
                Device {
                    id,
                    number: record.number,
                    secret: row.map(|row| row.secret.clone()),
                    last_session: record.last_session,
                    last_leave: record.last_leave,
                    // The design does not say how to recover sub-hour activity.
                    // Only a reload allows the maximum unrecorded interval.
                    last_accepted: session
                        .as_ref()
                        .map(|s| s.last_used.saturating_add(USE_WRITE_MS - 1)),
                    session,
                },
            );
        }
        Self { state, devices }
    }

    pub fn state(&self) -> &AccountState {
        &self.state
    }

    pub fn rows(&self) -> Rows {
        let devices = self
            .devices
            .iter()
            .filter_map(|(&id, device)| {
                Some((
                    id,
                    DeviceRow {
                        number: device.number,
                        secret: device.secret.clone()?,
                        session: device.session.clone(),
                    },
                ))
            })
            .collect();
        Rows {
            account: self.state.clone(),
            devices,
        }
    }

    pub fn apart(&self) -> Apart {
        let devices = self
            .devices
            .iter()
            .map(|(&id, device)| {
                (
                    id,
                    ApartDevice {
                        number: device.number,
                        last_session: device.last_session,
                        last_leave: device.last_leave,
                        permit: device.session.as_ref().map(|session| Permit {
                            id: session.id,
                            leave: session.leave.clone(),
                        }),
                    },
                )
            })
            .collect();
        Apart {
            account: self.state.clone(),
            devices,
        }
    }

    /// A new id (from S3) is used only for a secret not found on this account.
    fn identify(&self, secret: &Fingerprint, vacant: DeviceId) -> Result<(Device, bool), Denied> {
        if let Some(device) = self
            .devices
            .values()
            .find(|device| device.secret.as_ref() == Some(secret))
        {
            return Ok((device.clone(), false));
        }
        if self.devices.contains_key(&vacant) {
            return Err(Denied::DeviceInUse);
        }
        Ok((Device::new(vacant, secret.clone()), true))
    }

    /// S8 uses this current-restore turn for sign-in and code-redemption checks.
    /// Receipts still bind the restore number captured when the check began.
    pub fn sign_in_turn<T>(
        &mut self,
        clock: Clock,
        operation: impl FnOnce(Turn<'_>) -> Result<T, Denied>,
    ) -> Decision<Result<T, Denied>> {
        self.request(self.state.restore, clock, operation)
    }

    pub fn request<T>(
        &mut self,
        restore: u64,
        clock: Clock,
        operation: impl FnOnce(Turn<'_>) -> Result<T, Denied>,
    ) -> Decision<Result<T, Denied>> {
        let now = clock.effective();
        let mut effects = Effects {
            high_water: now,
            updates: Vec::new(),
        };
        let answer = if restore != self.state.restore {
            Err(Denied::StartAgain)
        } else {
            let before = self.rows();
            let apart = self.apart();
            let mut change = DataChange::Keep;
            let mut music = None;
            let answer = operation(Turn {
                account: self,
                now,
                change: &mut change,
                music: &mut music,
                others: &mut effects.updates,
            });
            if let Some(update) = self.update(&before, &apart, change, music) {
                effects.updates.push(update);
            }
            answer
        };
        Decision { effects, answer }
    }

    fn update(
        &self,
        before: &Rows,
        apart: &Apart,
        change: DataChange,
        music: Option<MusicRecovery>,
    ) -> Option<Update> {
        let data = self.rows();
        let apart_last = self.apart();
        if *before == data && *apart == apart_last && change == DataChange::Keep && music.is_none()
        {
            return None;
        }
        let mut apart_first = apart_last.clone();
        let first = &mut apart_first.account;
        let old = &apart.account;
        if old.status != Status::On && first.status == Status::On {
            first.status = old.status;
        }
        first.admin &= old.admin;
        first.music_recovery = first.music_recovery.or(old.music_recovery);
        first.history_held |= old.history_held;
        first.password_reset_required |=
            old.password_reset_required || old.password_changes != first.password_changes;
        // Changing the live identity retires the previous code at the first cut.
        // A newly issued code still requires its own matching data verifier.
        if change != DataChange::ResetPassword {
            first.code = first.code.map(|code| LiveCode {
                used: code.used || old.code != Some(code),
                ..code
            });
        }
        for (&id, after) in &mut apart_first.devices {
            after.permit = after.permit.take().and_then(|new| {
                let old = apart.devices.get(&id)?.permit.as_ref()?;
                (old.id == new.id).then_some(Permit {
                    id: old.id,
                    leave: old
                        .leave
                        .as_ref()
                        .filter(|leave| Some(*leave) == new.leave.as_ref())
                        .cloned(),
                })
            });
        }
        Some(Update {
            apart_first,
            data,
            change,
            music,
            apart_last,
        })
    }

    fn session(
        &mut self,
        id: DeviceId,
        hash: &Fingerprint,
        now: u64,
    ) -> Result<&mut Session, Denied> {
        self.state
            .allows_session()
            .map_err(|_| Denied::NotSignedIn)?;
        self.devices
            .get_mut(&id)
            .ok_or(Denied::NotSignedIn)?
            .authenticate(hash, now)
    }

    fn scope(&self, id: DeviceId) -> Result<Scope, Denied> {
        let device = self.devices.get(&id).ok_or(Denied::NotSignedIn)?;
        Ok(self.state.scope(device))
    }

    fn end_others(&mut self, keep: Option<DeviceId>) -> Result<(), Denied> {
        // Check the whole ending before touching the first row.
        if self
            .devices
            .values()
            .any(|d| Some(d.id) != keep && d.session.is_some() && d.number == u64::MAX)
        {
            return Err(Denied::Exhausted);
        }
        for device in self.devices.values_mut().filter(|d| Some(d.id) != keep) {
            device.end()?;
        }
        Ok(())
    }

    fn change_password(&mut self, keep: Option<DeviceId>, reset: bool) -> Result<(), Denied> {
        let changes = next(self.state.password_changes)?;
        let revision = next(self.state.revision)?;
        self.end_others(keep)?;
        self.state.password_changes = changes;
        self.state.revision = revision;
        self.state.password_reset_required = reset;
        self.state.code = if reset {
            Some(LiveCode {
                number: changes,
                used: false,
            })
        } else {
            self.state.code.map(|code| LiveCode { used: true, ..code })
        };
        Ok(())
    }

    /// S3 calls once per account before serving any request, all under one new
    /// restore number. Persist the new backend restore number apart with the effects.
    pub fn restore(
        apart: &Apart,
        data: Option<&Rows>,
        new_restore: u64,
        mode: RestoreMode,
        clock: Clock,
    ) -> Decision<Result<(), Denied>> {
        let high_water = clock.effective();
        let mut effects = Effects {
            high_water,
            updates: Vec::new(),
        };
        let answer = Self::restore_update(apart, data, new_restore, mode)
            .map(|update| effects.updates.extend(update));
        Decision { effects, answer }
    }

    fn restore_update(
        apart: &Apart,
        data: Option<&Rows>,
        new_restore: u64,
        mode: RestoreMode,
    ) -> Result<Option<Update>, Denied> {
        let mut account = Self::load(apart, data);
        if new_restore < account.state.restore
            || data.is_some_and(|data| new_restore < data.account.restore)
        {
            return Err(Denied::StartAgain);
        }
        let advancing = new_restore > account.state.restore;
        if advancing {
            for device in account.devices.values_mut() {
                device.number = next(device.number)?;
                device.session = None;
                device.last_accepted = None;
            }
            account.state.restore = new_restore;
            if mode == RestoreMode::Adopt && account.state.status != Status::Deleted {
                account.state.password_reset_required = true;
                account.state.code = None;
                account.state.revision = next(account.state.revision)?;
            }
        }
        let change = if account.state.status == Status::Deleted {
            DataChange::DeleteAccount
        } else {
            DataChange::Keep
        };
        let music = if account.state.status == Status::Deleted || mode == RestoreMode::WholeMachine
        {
            None
        } else if account.state.music_managed {
            Some(MusicRecovery::RotateManaged)
        } else if mode == RestoreMode::Adopt
            || data.is_some_and(|rows| rows.account.music_login != account.state.music_login)
        {
            Some(MusicRecovery::EnterAgain)
        } else {
            None
        };
        account.state.music_recovery = music.or(account.state.music_recovery);
        let before = data.cloned().unwrap_or_else(|| Rows {
            account: apart.account.clone(),
            devices: BTreeMap::new(),
        });
        Ok(account.update(&before, apart, change, music))
    }
}

/// Adoption uses an apart baseline rebuilt by S3 from data, including the new keys.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RestoreMode {
    DataOnly,
    Adopt,
    WholeMachine,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum StoreComparison {
    Matches,
    EndingRecorded,
    GrantOnlyInData,
    OlderCopy,
    StoreLost,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Startup {
    Ready,
    FinishEnding,
    DiscardGrant,
    RestoreData,
    Stop,
    AdoptData,
}

pub fn at_start(comparison: StoreComparison, owner_adopts: bool) -> Startup {
    match comparison {
        StoreComparison::Matches => Startup::Ready,
        StoreComparison::EndingRecorded => Startup::FinishEnding,
        StoreComparison::GrantOnlyInData => Startup::DiscardGrant,
        StoreComparison::OlderCopy => Startup::RestoreData,
        StoreComparison::StoreLost if owner_adopts => Startup::AdoptData,
        StoreComparison::StoreLost => Startup::Stop,
    }
}

pub struct Turn<'a> {
    account: &'a mut Account,
    now: u64,
    change: &'a mut DataChange,
    music: &'a mut Option<MusicRecovery>,
    others: &'a mut Vec<Update>,
}

pub struct Bearer<'a> {
    pub device: DeviceId,
    pub hash: &'a Fingerprint,
    /// Supplied by a grant-bearing request. Ordinary authenticated calls omit it.
    pub grant: Option<&'a GrantSecret>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct SessionGrant {
    pub account: AccountId,
    pub device: DeviceId,
    pub session: SessionId,
    pub music_login: u64,
}

/// Allocated by S3, monotonically across the backend and never reused, including
/// across restore. The device record also fences identifiers already used on it.
pub struct FreshSession {
    pub id: SessionId,
    pub hash: Fingerprint,
}

/// One consumed admin grant for provisioning a named account's login. Only
/// kept issues it, and the checked result is fenced again before installation.
pub struct MusicAuthorization {
    actor: Scope,
    session: SessionId,
    granted: u64,
    target: AccountId,
    target_revision: u64,
    next_count: u64,
}

impl MusicAuthorization {
    pub(in crate::nexus) fn backend(&self) -> BackendId {
        self.actor.backend
    }

    pub(in crate::nexus) fn account(&self) -> AccountId {
        self.target
    }

    pub(in crate::nexus) fn next_count(&self) -> u64 {
        self.next_count
    }
}

impl Turn<'_> {
    pub fn check_sign_in(
        self,
        device: &DeviceSecret,
        vacant: DeviceId,
        expected: Fingerprint,
    ) -> Result<PasswordCheck, Denied> {
        self.check_sign_in_for(device, vacant, expected, None)
    }

    /// S8 supplies the fingerprint and deadline of the authoritative live code.
    /// S3 rechecks the live verifier and deadline under its lock before writing
    /// any part of a RedeemCode effect, including its apart-first spent fence.
    pub fn check_code(
        self,
        device: &DeviceSecret,
        vacant: DeviceId,
        code: u64,
        expected: Fingerprint,
    ) -> Result<PasswordCheck, Denied> {
        if self.account.state.code
            != Some(LiveCode {
                number: code,
                used: false,
            })
        {
            return Err(Denied::WrongPassword);
        }
        self.check_sign_in_for(device, vacant, expected, Some(code))
    }

    fn check_sign_in_for(
        self,
        secret: &DeviceSecret,
        vacant: DeviceId,
        expected: Fingerprint,
        code: Option<u64>,
    ) -> Result<PasswordCheck, Denied> {
        let secret = Fingerprint::of(secret.expose_bytes());
        let (device, is_new) = self.account.identify(&secret, vacant)?;
        if let Some(row) = self.account.devices.get_mut(&device.id) {
            row.expire(self.now)?;
        }
        let device = self.account.devices.get(&device.id).unwrap_or(&device);
        let scope = self.account.state.scope(device);
        let purpose = if let Some(code) = code {
            Purpose::RedeemCode {
                secret,
                is_new,
                code,
            }
        } else {
            Purpose::SignIn { secret, is_new }
        };
        Ok(PasswordCheck::new(scope, purpose, expected, self.now))
    }

    /// On an account switch S3 passes the other account, found by device-secret
    /// fingerprint under the same lock. A claimed id never selects that account.
    pub fn sign_in(
        self,
        receipt: PasswordReceipt,
        fresh: FreshSession,
        previous: Option<&mut Account>,
    ) -> Result<SessionGrant, Denied> {
        self.sign_in_checked(receipt, fresh, previous, None)
    }

    /// Spend the code apart first, then write the new verifier and the session in
    /// the same data transaction, and recognize the new session last.
    pub fn redeem_code(
        self,
        receipt: PasswordReceipt,
        verifier: Fingerprint,
        fresh: FreshSession,
        previous: Option<&mut Account>,
    ) -> Result<SessionGrant, Denied> {
        self.sign_in_checked(receipt, fresh, previous, Some(verifier))
    }

    fn sign_in_checked(
        self,
        receipt: PasswordReceipt,
        fresh: FreshSession,
        previous: Option<&mut Account>,
        verifier: Option<Fingerprint>,
    ) -> Result<SessionGrant, Denied> {
        let id = receipt.scope().device;
        if let Some(device) = self.account.devices.get_mut(&id) {
            device.expire(self.now)?;
        }
        let scope = match self.account.scope(id) {
            Ok(scope) => scope,
            Err(_) => self
                .account
                .state
                .scope(&Device::new(id, fresh.hash.clone())),
        };
        let (secret, is_new, code) = match (receipt.consume(&scope, self.now)?, verifier) {
            (Purpose::SignIn { secret, is_new }, None) => {
                self.account.state.allows_session()?;
                (secret, is_new, None)
            }
            (
                Purpose::RedeemCode {
                    secret,
                    is_new,
                    code,
                },
                Some(verifier),
            ) => {
                match self.account.state.status {
                    Status::On => {}
                    Status::Off => return Err(Denied::AccountOff),
                    Status::Deleted => return Err(Denied::WrongPassword),
                }
                if self.account.state.code
                    != Some(LiveCode {
                        number: code,
                        used: false,
                    })
                {
                    return Err(Denied::WrongPassword);
                }
                (secret, is_new, Some((code, verifier)))
            }
            _ => return Err(Denied::WrongPurpose),
        };
        let (mut device, still_new) = self.account.identify(&secret, id)?;
        if is_new != still_new || device.id != id {
            return Err(Denied::NotSignedIn);
        }
        if fresh.id.get() <= device.last_session
            || self.account.devices.values().any(|d| {
                d.session
                    .as_ref()
                    .is_some_and(|s| s.hash == fresh.hash || s.id == fresh.id)
            })
        {
            return Err(Denied::ReusedSession);
        }
        device.last_session = fresh.id.get();
        device.end()?;
        device.session = Some(Session {
            id: fresh.id,
            hash: fresh.hash,
            last_used: self.now,
            leave: None,
        });
        device.last_accepted = Some(self.now);

        let redeemed = if let Some((code, verifier)) = code {
            let mut account = self.account.clone();
            account.change_password(Some(id), false)?;
            Some((account, DataChange::RedeemCode { code, verifier }))
        } else {
            None
        };
        // The design does not say how a device moves between accounts. Keep the
        // rows account-scoped, and end the old account's session before granting.
        if let Some(previous) = previous {
            if previous.state.backend != self.account.state.backend
                || previous.state.restore != self.account.state.restore
                || previous.state.same_account(&self.account.state)
            {
                return Err(Denied::WrongPurpose);
            }
            let before = previous.rows();
            let apart = previous.apart();
            let old_device = previous
                .devices
                .values_mut()
                .find(|d| d.secret.as_ref() == Some(&secret))
                .ok_or(Denied::NotSignedIn)?;
            old_device.end()?;
            if let Some(update) = previous.update(&before, &apart, DataChange::Keep, None) {
                self.others.push(update);
            }
        }
        if let Some((account, change)) = redeemed {
            *self.account = account;
            *self.change = change;
        }
        self.account.devices.insert(id, device);
        Ok(SessionGrant {
            account: self.account.state.id,
            device: id,
            session: fresh.id,
            music_login: self.account.state.music_login,
        })
    }

    pub fn check_leave(
        self,
        bearer: Bearer<'_>,
        for_what: LeaveFor,
        expected: Fingerprint,
    ) -> Result<PasswordCheck, Denied> {
        let session = self
            .account
            .session(bearer.device, bearer.hash, self.now)?
            .id;
        if matches!(for_what, LeaveFor::Admin { .. }) && !self.account.state.admin {
            return Err(Denied::AdminRequired);
        }
        Ok(PasswordCheck::new(
            self.account.scope(bearer.device)?,
            Purpose::Leave { session, for_what },
            expected,
            self.now,
        ))
    }

    /// S8 generates a fresh grant secret. Only the fingerprint persists.
    pub fn grant_leave(self, receipt: PasswordReceipt, grant: &GrantSecret) -> Result<(), Denied> {
        let id = receipt.scope().device;
        self.account
            .state
            .allows_session()
            .map_err(|_| Denied::NotSignedIn)?;
        let device = self
            .account
            .devices
            .get_mut(&id)
            .ok_or(Denied::NotSignedIn)?;
        device.expire(self.now)?;
        let scope = self.account.state.scope(device);
        let checked = receipt.checked();
        let Purpose::Leave { session, for_what } = receipt.consume(&scope, self.now)? else {
            return Err(Denied::WrongPurpose);
        };
        let row = device
            .session
            .as_mut()
            .filter(|s| s.id == session)
            .ok_or(Denied::NotSignedIn)?;
        if matches!(for_what, LeaveFor::Admin { .. }) && !self.account.state.admin {
            return Err(Denied::AdminRequired);
        }
        device.last_leave = next(device.last_leave)?;
        // The design does not say how simultaneous leave for different operations
        // interacts. Retain just one, and spend it on a single matching change.
        row.leave = Some(Leave::stored(
            for_what,
            scope.life,
            scope.revision,
            self.account.state.admin,
            checked,
            device.last_leave,
            Fingerprint::of(grant.expose_bytes()),
        ));
        device.touch(self.now)?;
        Ok(())
    }

    pub fn arrive(self, bearer: Bearer<'_>, access: Access) -> Result<Request, Denied> {
        let session_id = self
            .account
            .session(bearer.device, bearer.hash, self.now)?
            .id;
        access.check(&self.account.state)?;
        Ok(Request {
            scope: self.account.scope(bearer.device)?,
            session_id,
            access,
            arrived: self.now,
        })
    }

    pub fn complete(self, request: Request) -> Result<(), Denied> {
        let id = request.scope.device;
        self.account
            .state
            .allows_session()
            .map_err(|_| Denied::NotSignedIn)?;
        let device = self
            .account
            .devices
            .get_mut(&id)
            .ok_or(Denied::NotSignedIn)?;
        device.expire(self.now)?;
        if self.account.state.scope(device) != request.scope
            || !device
                .session
                .as_ref()
                .is_some_and(|s| s.id == request.session_id)
        {
            return Err(Denied::NotSignedIn);
        }
        if self.now < request.arrived
            || (request.access.is_write() && !within(request.arrived, self.now, WRITE_MS))
        {
            return Err(Denied::Time);
        }
        request.access.check(&self.account.state)
    }

    pub fn sign_out(self, bearer: Bearer<'_>) -> Result<(), Denied> {
        let device = self
            .account
            .devices
            .get_mut(&bearer.device)
            .ok_or(Denied::NotSignedIn)?;
        device.expire(self.now)?;
        if device
            .session
            .as_ref()
            .is_some_and(|s| s.hash == *bearer.hash)
        {
            device.end()?;
            return Ok(());
        }
        Err(Denied::NotSignedIn)
    }

    pub fn force_out(self, bearer: Bearer<'_>, other: SessionTarget) -> Result<(), Denied> {
        self.account.session(bearer.device, bearer.hash, self.now)?;
        if other.device == bearer.device {
            return Err(Denied::WrongPurpose);
        }
        force_out(self.account, other, self.music)
    }

    fn consume_leave(&mut self, bearer: Bearer<'_>, purpose: LeaveFor) -> Result<Leave, Denied> {
        let life = self.account.state.life;
        let revision = self.account.state.revision;
        let admin = self.account.state.admin;
        let session = self.account.session(bearer.device, bearer.hash, self.now)?;
        let hash = bearer
            .grant
            .map(|grant| Fingerprint::of(grant.expose_bytes()))
            .ok_or(Denied::LeaveRequired)?;
        if !session.leave.as_ref().is_some_and(|leave| {
            leave.hash == hash
                && leave.life == life
                && leave.revision == revision
                && leave.admin == admin
                && leave.purpose == purpose
                && within(leave.granted, self.now, PASSWORD_LEAVE_MS)
        }) {
            return Err(Denied::LeaveRequired);
        }
        session.leave.take().ok_or(Denied::LeaveRequired)
    }

    pub fn change_password(mut self, bearer: Bearer<'_>) -> Result<(), Denied> {
        let keep = bearer.device;
        self.consume_leave(bearer, LeaveFor::ChangePassword)?;
        self.account.change_password(Some(keep), false)?;
        *self.change = DataChange::ChangePassword;
        Ok(())
    }

    pub fn delete_history(mut self, bearer: Bearer<'_>) -> Result<(), Denied> {
        self.consume_leave(bearer, LeaveFor::DeleteHistory)?;
        self.account.state.life = next(self.account.state.life)?;
        self.account.state.history_held = false;
        // §12.5 fences work from the earlier life while sessions remain usable.
        for session in self
            .account
            .devices
            .values_mut()
            .filter_map(|d| d.session.as_mut())
        {
            session.leave = None;
        }
        *self.change = DataChange::EraseHistory;
        Ok(())
    }

    pub fn keep_history(self, bearer: Bearer<'_>) -> Result<(), Denied> {
        self.account.session(bearer.device, bearer.hash, self.now)?;
        self.account.state.history_held = false;
        Ok(())
    }

    /// Consume a single grant before S9 sends either credential-bearing POST.
    /// None selects the admin's own account.
    pub fn authorize_music(
        mut self,
        bearer: Bearer<'_>,
        target: Option<&Account>,
    ) -> Result<MusicAuthorization, Denied> {
        if !self.account.state.admin {
            return Err(Denied::AdminRequired);
        }
        let target_state = &target.unwrap_or(self.account).state;
        if target_state.backend != self.account.state.backend
            || target_state.restore != self.account.state.restore
            || target_state.status == Status::Deleted
        {
            return Err(Denied::WrongPurpose);
        }
        let target = target_state.id;
        let target_revision = target_state.revision;
        let next_count = next(target_state.music_login)?;
        let device = bearer.device;
        let hash = bearer.hash;
        let grant = self.consume_leave(
            bearer,
            LeaveFor::Admin {
                action: AdminAction::MusicLoginChanged,
                target: AdminTarget::Account(target),
            },
        )?;
        let session = self.account.session(device, hash, self.now)?.id;
        Ok(MusicAuthorization {
            actor: self.account.scope(device)?,
            session,
            granted: grant.granted,
            target,
            target_revision,
            next_count,
        })
    }

    /// Install the verified login only while the same authorization and endpoints
    /// are current. S3 commits the count and sealed bytes under the same lock.
    pub fn install_music(
        self,
        bearer: Bearer<'_>,
        verified: VerifiedLogin,
        endpoints: CurrentEndpoints<'_>,
        target: Option<&mut Account>,
    ) -> Result<u64, Denied> {
        let (auth, login) = verified.into_parts(endpoints).ok_or(Denied::WrongPurpose)?;
        let session = self
            .account
            .session(bearer.device, bearer.hash, self.now)?
            .id;
        if !self.account.state.admin
            || self.account.scope(bearer.device)? != auth.actor
            || session != auth.session
            || !within(auth.granted, self.now, PASSWORD_LEAVE_MS)
        {
            return Err(Denied::LeaveRequired);
        }
        if let Some(target) = target {
            if target.state.same_account(&self.account.state) {
                return Err(Denied::WrongPurpose);
            }
            let before = target.rows();
            let apart = target.apart();
            install_music(target, &auth)?;
            if let Some(update) =
                target.update(&before, &apart, DataChange::InstallMusicLogin(login), None)
            {
                self.others.push(update);
            }
        } else {
            install_music(self.account, &auth)?;
            *self.change = DataChange::InstallMusicLogin(login);
        }
        Ok(auth.next_count)
    }

    pub fn admin_read(self, bearer: Bearer<'_>) -> Result<(), Denied> {
        self.account.session(bearer.device, bearer.hash, self.now)?;
        if self.account.state.admin {
            Ok(())
        } else {
            Err(Denied::AdminRequired)
        }
    }

    /// Other account, or None for a change to the actor's own account or a global
    /// setting. ApplyAdmin/InstallMusicLogin tell S3 to include the action's
    /// payload in the data write. A denied change may still consume leave.
    pub fn administer(
        mut self,
        bearer: Bearer<'_>,
        action: AdminAction,
        named: AdminTarget,
        target: Option<&mut Account>,
    ) -> Result<(), Denied> {
        if !self.account.state.admin {
            return Err(Denied::AdminRequired);
        }
        if action == AdminAction::MusicLoginChanged {
            return Err(Denied::WrongPurpose);
        }
        self.consume_leave(
            bearer,
            LeaveFor::Admin {
                action,
                target: named,
            },
        )?;
        let selected = target.as_deref().unwrap_or(self.account).state.id;
        if !action.matches_target(named, selected, target.is_some()) {
            return Err(Denied::WrongPurpose);
        }
        if let Some(target) = target {
            if target.state.backend != self.account.state.backend
                || target.state.restore != self.account.state.restore
                || target.state.same_account(&self.account.state)
            {
                return Err(Denied::WrongPurpose);
            }
            let before = target.rows();
            let apart = target.apart();
            let mut change = DataChange::Keep;
            let mut music = None;
            let answer = admin_change(target, action, named, &mut change, &mut music);
            if let Some(update) = target.update(&before, &apart, change, music) {
                self.others.push(update);
            }
            answer
        } else {
            admin_change(self.account, action, named, self.change, self.music)
        }
    }
}

fn install_music(account: &mut Account, auth: &MusicAuthorization) -> Result<(), Denied> {
    let state = &mut account.state;
    if state.backend != auth.actor.backend
        || state.restore != auth.actor.restore
        || state.id != auth.target
        || state.revision != auth.target_revision
        || state.status == Status::Deleted
        || next(state.music_login)? != auth.next_count
    {
        return Err(Denied::WrongPurpose);
    }
    state.music_login = auth.next_count;
    state.music_managed = false;
    state.music_recovery = None;
    Ok(())
}

fn force_out(
    account: &mut Account,
    target: SessionTarget,
    music: &mut Option<MusicRecovery>,
) -> Result<(), Denied> {
    if let Some(device) = account.devices.get_mut(&target.device)
        && device
            .session
            .as_ref()
            .is_some_and(|session| session.id == target.session)
    {
        device.end()?;
        if account.state.music_managed {
            require_music_rotation(account, music);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum AdminAction {
    SetOn(bool),
    DeleteAccount,
    ResetPassword,
    SetAdmin(bool),
    ForceOut(SessionTarget),
    MusicLoginChanged,
    CreateAccount,
    SetMusicAddress,
    SetDiagnosticsDefault,
    SetRequests,
    SetLidarr,
}

impl AdminAction {
    fn matches_target(self, target: AdminTarget, selected: AccountId, other: bool) -> bool {
        match self {
            Self::CreateAccount => matches!(target, AdminTarget::NewAccount(_)) && !other,
            Self::SetMusicAddress | Self::SetDiagnosticsDefault | Self::SetLidarr => {
                target == AdminTarget::Backend && !other
            }
            _ => target == AdminTarget::Account(selected),
        }
    }
}

fn admin_change(
    account: &mut Account,
    action: AdminAction,
    target: AdminTarget,
    change: &mut DataChange,
    music: &mut Option<MusicRecovery>,
) -> Result<(), Denied> {
    if account.state.status == Status::Deleted {
        return Err(Denied::NotSignedIn);
    }
    match action {
        AdminAction::SetOn(on) => {
            let status = if on { Status::On } else { Status::Off };
            if account.state.status != status {
                let revision = next(account.state.revision)?;
                if !on {
                    account.end_others(None)?;
                    account.state.code = account
                        .state
                        .code
                        .map(|code| LiveCode { used: true, ..code });
                    if account.state.music_managed {
                        require_music_rotation(account, music);
                    }
                }
                account.state.status = status;
                account.state.revision = revision;
            }
        }
        AdminAction::DeleteAccount => {
            let revision = next(account.state.revision)?;
            let life = next(account.state.life)?;
            account.end_others(None)?;
            account.state.status = Status::Deleted;
            account.state.admin = false;
            account.state.code = account
                .state
                .code
                .map(|code| LiveCode { used: true, ..code });
            account.state.revision = revision;
            account.state.life = life;
            for device in account.devices.values_mut() {
                device.secret = None;
            }
            *change = DataChange::DeleteAccount;
        }
        AdminAction::ResetPassword => {
            account.change_password(None, true)?;
            *change = DataChange::ResetPassword;
            if account.state.music_managed {
                require_music_rotation(account, music);
            }
        }
        AdminAction::SetAdmin(admin) => {
            if account.state.admin != admin {
                account.state.revision = next(account.state.revision)?;
                account.state.admin = admin;
                for session in account
                    .devices
                    .values_mut()
                    .filter_map(|d| d.session.as_mut())
                {
                    session.leave = None;
                }
            }
        }
        AdminAction::ForceOut(device) => force_out(account, device, music)?,
        AdminAction::MusicLoginChanged => return Err(Denied::WrongPurpose),
        AdminAction::CreateAccount
        | AdminAction::SetMusicAddress
        | AdminAction::SetDiagnosticsDefault
        | AdminAction::SetRequests
        | AdminAction::SetLidarr => *change = DataChange::ApplyAdmin { action, target },
    }
    Ok(())
}

impl AccountState {
    fn music_recovery(&self) -> MusicRecovery {
        if self.music_managed {
            MusicRecovery::RotateManaged
        } else {
            MusicRecovery::EnterAgain
        }
    }
}

fn require_music_rotation(account: &mut Account, music: &mut Option<MusicRecovery>) {
    let task = MusicRecovery::RotateManaged;
    account.state.music_recovery = Some(task);
    *music = Some(task);
}
