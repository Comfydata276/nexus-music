//! S3 serializes load, decide, apply and publish, including across accounts for
//! an admin change and across the backend for restore.

use super::receipt::Purpose;
use super::*;
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
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct Permit {
    pub id: u64,
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
    pub devices: BTreeMap<u64, DeviceRow>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Apart {
    pub account: AccountState,
    pub devices: BTreeMap<u64, ApartDevice>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DataChange {
    Keep,
    ChangePassword,
    ResetPassword,
    SpendCode,
    EraseHistory,
    DeleteAccount,
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
    devices: BTreeMap<u64, Device>,
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
            state.code_available &= !verifier_missing;
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
                let permit = record.permit?;
                let same_restore = data.is_some_and(|rows| rows.account.restore == state.restore);
                if !same_restore
                    || state.status != Status::On
                    || (state.password_reset_required && !stored.restricted)
                    || row.number != record.number
                    || permit.id != stored.id
                    || permit.id == 0
                    || permit.id > record.last_session
                {
                    return None;
                }
                let mut session = stored.clone();
                session.leave = session.leave.filter(|leave| {
                    Some(*leave) == permit.leave && leave.serial <= record.last_leave
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
                            leave: session.leave,
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
    fn identify(&self, secret: &Fingerprint, vacant: u64) -> Result<(Device, bool), Denied> {
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
        first.code_available &=
            old.code_available && old.password_changes == first.password_changes;
        for (&id, after) in &mut apart_first.devices {
            after.permit = after.permit.and_then(|new| {
                let old = apart.devices.get(&id)?.permit?;
                (old.id == new.id).then_some(Permit {
                    id: old.id,
                    leave: old.leave.filter(|leave| Some(*leave) == new.leave),
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

    fn session(&mut self, id: u64, hash: &Fingerprint, now: u64) -> Result<&mut Session, Denied> {
        self.state.allows_session()?;
        let session = self
            .devices
            .get_mut(&id)
            .ok_or(Denied::NotSignedIn)?
            .authenticate(hash, now)?;
        if session.restricted {
            return Err(Denied::NotSignedIn);
        }
        Ok(session)
    }

    fn scope(&self, id: u64) -> Result<Scope, Denied> {
        let device = self.devices.get(&id).ok_or(Denied::NotSignedIn)?;
        Ok(self.state.scope(device))
    }

    fn end_others(&mut self, keep: Option<u64>) -> Result<(), Denied> {
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

    fn change_password(&mut self, keep: Option<u64>, reset: bool) -> Result<(), Denied> {
        let changes = next(self.state.password_changes)?;
        let revision = next(self.state.revision)?;
        self.end_others(keep)?;
        self.state.password_changes = changes;
        self.state.revision = revision;
        self.state.password_reset_required = reset;
        self.state.code_available = reset;
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
            .map(|update| effects.updates.push(update));
        Decision { effects, answer }
    }

    fn restore_update(
        apart: &Apart,
        data: Option<&Rows>,
        new_restore: u64,
        mode: RestoreMode,
    ) -> Result<Update, Denied> {
        let mut account = Self::load(apart, data);
        if new_restore <= account.state.restore
            || data.is_some_and(|data| new_restore <= data.account.restore)
        {
            return Err(Denied::StartAgain);
        }
        // End even a session that load had to suppress. Device numbers and the new
        // restore number are recorded apart before the copied rows are cleared.
        for (&id, device) in &mut account.devices {
            if apart.devices.get(&id).is_some_and(|d| d.permit.is_some())
                || data
                    .and_then(|rows| rows.devices.get(&id))
                    .is_some_and(|row| row.session.is_some())
            {
                device.number = next(device.number)?;
            }
            device.session = None;
        }
        account.state.restore = new_restore;
        if let RestoreMode::WholeMachine(Some(Restore::HoldThrough(life))) = mode {
            // The design does not say how to handle history when a genuine stamp
            // proves deletion after the whole machine went back. Hold that history.
            account.state.life = account.state.life.max(life);
            account.state.history_held = true;
        }

        let change = if account.state.status == Status::Deleted {
            DataChange::DeleteAccount
        } else {
            DataChange::Keep
        };
        let music = if account.state.status == Status::Deleted
            || matches!(mode, RestoreMode::WholeMachine(_))
        {
            None
        } else if account.state.music_managed {
            Some(MusicRecovery::RotateManaged)
        } else if mode == RestoreMode::Adopt
            || data.is_some_and(|rows| rows.account.music_login != account.state.music_login)
        {
            // The design does not say whether to hand out a brought credential
            // when its count proves the restored credential is stale. Require re-entry.
            Some(MusicRecovery::EnterAgain)
        } else {
            None
        };
        account.state.music_recovery = music.or(account.state.music_recovery);
        let before = data.cloned().unwrap_or_else(|| Rows {
            account: apart.account.clone(),
            devices: BTreeMap::new(),
        });
        account
            .update(&before, apart, change, music)
            .ok_or(Denied::StartAgain)
    }
}

/// Adoption uses an apart baseline rebuilt by S3 from data, including the new keys.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum RestoreMode {
    DataOnly,
    Adopt,
    WholeMachine(Option<Restore>),
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
    pub device: u64,
    pub hash: &'a Fingerprint,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct SignedIn {
    pub device: u64,
    pub music_login: Option<u64>,
}

impl Turn<'_> {
    pub fn check_sign_in(
        self,
        device: &DeviceSecret,
        vacant: u64,
        expected: Fingerprint,
    ) -> Result<PasswordCheck, Denied> {
        self.check_sign_in_for(device, vacant, expected, false)
    }

    /// S8 supplies the stored verifier only for a live one-time code, and checks
    /// its expiry and storage preconditions when applying SpendCode.
    pub fn check_code(
        self,
        device: &DeviceSecret,
        vacant: u64,
        expected: Fingerprint,
    ) -> Result<PasswordCheck, Denied> {
        if !self.account.state.code_available {
            return Err(Denied::WrongPassword);
        }
        self.check_sign_in_for(device, vacant, expected, true)
    }

    fn check_sign_in_for(
        self,
        secret: &DeviceSecret,
        vacant: u64,
        expected: Fingerprint,
        code: bool,
    ) -> Result<PasswordCheck, Denied> {
        let secret = secret.fingerprint();
        let (device, is_new) = self.account.identify(&secret, vacant)?;
        if let Some(row) = self.account.devices.get_mut(&device.id) {
            row.expire(self.now)?;
        }
        let device = self.account.devices.get(&device.id).unwrap_or(&device);
        let scope = self.account.state.scope(device);
        let purpose = if code {
            Purpose::ChoosePassword { secret, is_new }
        } else {
            Purpose::SignIn { secret, is_new }
        };
        Ok(PasswordCheck::new(scope, purpose, expected, self.now))
    }

    /// On an account switch S3 passes the other account, found by device-secret
    /// fingerprint under the same lock. A claimed id never selects that account.
    pub fn sign_in(
        self,
        receipt: Receipt,
        fresh: Fingerprint,
        previous: Option<&mut Account>,
    ) -> Result<SignedIn, Denied> {
        let id = receipt.scope().device;
        if let Some(device) = self.account.devices.get_mut(&id) {
            device.expire(self.now)?;
        }
        let scope = match self.account.scope(id) {
            Ok(scope) => scope,
            Err(_) => self.account.state.scope(&Device::new(id, fresh.clone())),
        };
        let (secret, is_new, restricted) = match receipt.consume(&scope, self.now)? {
            Purpose::SignIn { secret, is_new } => {
                self.account.state.allows_session()?;
                (secret, is_new, false)
            }
            Purpose::ChoosePassword { secret, is_new }
                if self.account.state.status == Status::On && self.account.state.code_available =>
            {
                (secret, is_new, true)
            }
            Purpose::ChoosePassword { .. } if self.account.state.status == Status::Off => {
                return Err(Denied::AccountOff);
            }
            Purpose::ChoosePassword { .. } => return Err(Denied::NotSignedIn),
            Purpose::Leave { .. } => return Err(Denied::WrongPurpose),
        };
        let (mut device, still_new) = self.account.identify(&secret, id)?;
        if is_new != still_new || device.id != id {
            return Err(Denied::NotSignedIn);
        }
        if self
            .account
            .devices
            .values()
            .filter_map(|d| d.session.as_ref())
            .any(|s| s.hash == fresh)
        {
            return Err(Denied::ReusedSession);
        }
        device.last_session = next(device.last_session)?;
        device.end()?;
        device.session = Some(Session {
            id: device.last_session,
            hash: fresh,
            last_used: self.now,
            restricted,
            leave: None,
        });
        let code_revision = if restricted {
            Some(next(self.account.state.revision)?)
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
        if let Some(revision) = code_revision {
            self.account.state.revision = revision;
            self.account.state.code_available = false;
            *self.change = DataChange::SpendCode;
        }
        self.account.devices.insert(id, device);
        Ok(SignedIn {
            device: id,
            music_login: (!restricted).then_some(self.account.state.music_login),
        })
    }

    /// The code confers only this operation, for one session. S8 stores the new
    /// verifier with ChangePassword, then the device signs in with that password.
    pub fn choose_password(self, bearer: Bearer<'_>) -> Result<(), Denied> {
        if self.account.state.status != Status::On {
            return Err(Denied::NotSignedIn);
        }
        let device = self
            .account
            .devices
            .get_mut(&bearer.device)
            .ok_or(Denied::NotSignedIn)?;
        device.expire(self.now)?;
        let session = device
            .session
            .as_ref()
            .filter(|s| s.hash == *bearer.hash && s.restricted)
            .ok_or(Denied::NotSignedIn)?;
        // The design does not say how long the restricted code session may choose
        // a password. Use the same ten-minute bound as password leave.
        if !within(session.last_used, self.now, LEAVE_MS) {
            return Err(Denied::LeaveRequired);
        }
        self.account.change_password(None, false)?;
        *self.change = DataChange::ChangePassword;
        Ok(())
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
        if matches!(for_what, LeaveFor::Admin(_)) && !self.account.state.admin {
            return Err(Denied::AdminRequired);
        }
        Ok(PasswordCheck::new(
            self.account.scope(bearer.device)?,
            Purpose::Leave { session, for_what },
            expected,
            self.now,
        ))
    }

    pub fn grant_leave(self, receipt: Receipt) -> Result<(), Denied> {
        let id = receipt.scope().device;
        self.account.state.allows_session()?;
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
            .filter(|s| s.id == session && !s.restricted)
            .ok_or(Denied::NotSignedIn)?;
        if matches!(for_what, LeaveFor::Admin(_)) && !self.account.state.admin {
            return Err(Denied::AdminRequired);
        }
        device.last_leave = next(device.last_leave)?;
        // The design does not say how simultaneous leave for different operations
        // interacts. Retain just one, and spend it on a single matching change.
        row.leave = Some(Leave::stored(
            for_what,
            scope.life,
            checked,
            device.last_leave,
        ));
        row.last_used = self.now;
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
        self.account.state.allows_session()?;
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
                .is_some_and(|s| s.id == request.session_id && !s.restricted)
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

    pub fn force_out(self, bearer: Bearer<'_>, other: u64) -> Result<(), Denied> {
        self.account.session(bearer.device, bearer.hash, self.now)?;
        if other == bearer.device {
            return Err(Denied::WrongPurpose);
        }
        force_out(self.account, other, self.music)
    }

    fn consume_leave(&mut self, bearer: Bearer<'_>, purpose: LeaveFor) -> Result<(), Denied> {
        let life = self.account.state.life;
        let session = self.account.session(bearer.device, bearer.hash, self.now)?;
        if !session.leave.is_some_and(|leave| {
            leave.life == life
                && leave.purpose == purpose
                && within(leave.granted, self.now, LEAVE_MS)
        }) {
            return Err(Denied::LeaveRequired);
        }
        session.leave = None;
        Ok(())
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

    /// S9 applies a successfully replaced pending login, using the count under
    /// which the replacement started. The storage step installs the checked pair's
    /// source credentials alongside the count.
    pub fn music_replaced(self, previous_count: u64) -> Result<u64, Denied> {
        let state = &mut self.account.state;
        if state.status == Status::Deleted
            || state.music_recovery.is_none()
            || state.music_login != previous_count
        {
            return Err(Denied::WrongPurpose);
        }
        state.music_login = next(state.music_login)?;
        state.music_recovery = None;
        Ok(state.music_login)
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
    /// setting. S3 writes the authorized action in the data step and only on success.
    pub fn administer(
        mut self,
        bearer: Bearer<'_>,
        action: AdminAction,
        target: Option<&mut Account>,
    ) -> Result<(), Denied> {
        if !self.account.state.admin {
            return Err(Denied::AdminRequired);
        }
        self.consume_leave(bearer, LeaveFor::Admin(action.kind()))?;
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
            let answer = admin_change(target, action, &mut change, &mut music);
            if let Some(update) = target.update(&before, &apart, change, music) {
                self.others.push(update);
            }
            answer
        } else {
            admin_change(self.account, action, self.change, self.music)
        }
    }
}

fn force_out(
    account: &mut Account,
    id: u64,
    music: &mut Option<MusicRecovery>,
) -> Result<(), Denied> {
    let device = account.devices.get_mut(&id).ok_or(Denied::NotSignedIn)?;
    if device.session.is_some() {
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
    MakeAdmin,
    ForceOut(u64),
    MusicLoginChanged,
    CreateAccount,
    SetMusicAddress,
    SetDiagnosticsDefault,
    SetRequests,
    SetLidarr,
}

impl AdminAction {
    fn kind(self) -> AdminKind {
        match self {
            Self::SetOn(_) => AdminKind::ChangeAccountStatus,
            Self::DeleteAccount => AdminKind::DeleteAccount,
            Self::ResetPassword => AdminKind::ResetPassword,
            Self::MakeAdmin => AdminKind::MakeAdmin,
            Self::ForceOut(_) => AdminKind::ForceOut,
            Self::MusicLoginChanged => AdminKind::ChangeMusicLogin,
            Self::CreateAccount => AdminKind::CreateAccount,
            Self::SetMusicAddress => AdminKind::SetMusicAddress,
            Self::SetDiagnosticsDefault => AdminKind::SetDiagnosticsDefault,
            Self::SetRequests => AdminKind::SetRequests,
            Self::SetLidarr => AdminKind::SetLidarr,
        }
    }
}

fn admin_change(
    account: &mut Account,
    action: AdminAction,
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
            account.state.code_available = false;
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
        AdminAction::MakeAdmin => {
            if !account.state.admin {
                account.state.revision = next(account.state.revision)?;
                account.state.admin = true;
            }
        }
        AdminAction::ForceOut(device) => force_out(account, device, music)?,
        AdminAction::MusicLoginChanged => {
            // S9 supplies this action when the replacement is ready for its data write.
            account.state.music_login = next(account.state.music_login)?;
            account.state.music_recovery = None;
        }
        AdminAction::CreateAccount
        | AdminAction::SetMusicAddress
        | AdminAction::SetDiagnosticsDefault
        | AdminAction::SetRequests
        | AdminAction::SetLidarr => {}
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
