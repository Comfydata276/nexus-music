use super::*;

fn position() -> Position {
    Position {
        backend: state("alice@example.test").backend,
        account: acct(1),
        restore: 1,
        life: 2,
        change: 5,
    }
}

#[test]
fn only_a_genuine_stamp_ahead_for_this_account_and_restore_shows_a_rollback() {
    let p = position();
    let k = StampKey::new([3; SECRET_BYTES]);
    for (s, result) in [
        (
            Position {
                life: 3,
                change: 0,
                ..p.clone()
            },
            Some(Restore::EndSessions),
        ),
        (
            Position {
                change: 6,
                ..p.clone()
            },
            Some(Restore::EndSessions),
        ),
        (p.clone(), None),
        (
            Position {
                change: 4,
                ..p.clone()
            },
            None,
        ),
        (
            Position {
                life: 1,
                change: 99,
                ..p.clone()
            },
            None,
        ),
        (
            Position {
                account: acct(2),
                change: 6,
                ..p.clone()
            },
            None,
        ),
        (
            Position {
                backend: BackendId::from_bytes([2; 16]),
                change: 6,
                ..p.clone()
            },
            None,
        ),
        (
            Position {
                restore: 2,
                change: 6,
                ..p.clone()
            },
            None,
        ),
    ] {
        assert_eq!(k.presented(&p, &s, &k.stamp(&s)), result);
        assert_eq!(
            k.presented(&p, &s, &StampKey::new([4; SECRET_BYTES]).stamp(&s)),
            None
        );
    }
    for tampered in [
        Position {
            life: 3,
            ..p.clone()
        },
        Position {
            change: 6,
            ..p.clone()
        },
    ] {
        assert_eq!(k.presented(&p, &tampered, &k.stamp(&p)), None);
    }
}

#[test]
fn a_late_answer_only_applies_to_the_unchanged_component_backend_restore_account_and_life() {
    let p = position();
    assert!(may_apply(
        1,
        1,
        &p,
        &Position {
            change: 23,
            ..p.clone()
        }
    ));
    assert!(!may_apply(2, 1, &p, &p));
    for other in [
        Position {
            backend: BackendId::from_bytes([0; 16]),
            ..p.clone()
        },
        Position {
            restore: 2,
            ..p.clone()
        },
        Position {
            life: 3,
            ..p.clone()
        },
        Position {
            account: acct(2),
            ..p.clone()
        },
    ] {
        assert!(!may_apply(1, 1, &p, &other));
    }
}

pub(super) fn acct(n: u64) -> AccountId {
    AccountId::new(n).unwrap()
}

pub(super) fn dev(n: u64) -> DeviceId {
    DeviceId::new(n).unwrap()
}

pub(super) fn fresh(n: u8) -> FreshSession {
    FreshSession {
        id: SessionId::new(u64::from(n)).unwrap(),
        hash: hash(n),
        sign_out: hash(n ^ 128),
    }
}

pub(super) fn state(email: &str) -> AccountState {
    AccountState {
        backend: BackendId::from_bytes([4; 16]),
        restore: 1,
        id: acct(match email {
            "alice@example.test" => 1,
            "bob@example.test" => 2,
            "admin@example.test" => 3,
            _ => 4,
        }),
        email: Email::new(email).unwrap(),
        life: 1,
        status: Status::On,
        password_changes: 0,
        password_reset_required: false,
        code: None,
        admin: false,
        music_login: 1,
        music_managed: false,
        music_recovery: None,
        history_held: false,
        revision: 0,
    }
}

pub(super) fn hash(n: u8) -> Fingerprint {
    Fingerprint::of(&[n; SECRET_BYTES])
}

pub(super) fn clock(now: u64, high_water: u64) -> Clock {
    Clock::new(now, high_water)
}

pub(super) struct Stored {
    pub account: Account,
    pub apart: Apart,
    pub data: Rows,
    pub mark: u64,
}

impl Stored {
    pub fn new(account: AccountState) -> Self {
        let apart = Apart {
            account: account.clone(),
            devices: Default::default(),
        };
        let data = Rows {
            account,
            devices: Default::default(),
        };
        let account = Account::load(&apart, Some(&data));
        Self {
            account,
            apart,
            data,
            mark: 0,
        }
    }

    pub fn step<T>(
        &mut self,
        now: u64,
        act: impl FnOnce(Turn<'_>) -> Result<T, Denied>,
    ) -> Result<T, Denied> {
        self.account
            .request(self.apart.account.restore, clock(now, self.mark), act)
            .apply(|effects| {
                self.mark = effects.high_water;
                for update in &effects.updates {
                    self.apart = update.apart_first.clone();
                    assert_eq!(self.apart.account.id, update.data.account.id);
                    self.data = update.data.clone();
                    self.apart = update.apart_last.clone();
                }
                Ok(())
            })?
    }

    pub fn bind(&mut self, secret: u8, id: u64, at: u64) -> Result<PasswordReceipt, Denied> {
        let binding = self.step(at, |turn| {
            turn.check_sign_in(&DeviceSecret::from_bytes([secret; 32]), dev(id), hash(42))
        })?;
        binding.check(hash(42), clock(at, self.mark))
    }

    pub fn sign_in(&mut self, secret: u8, id: u64, n: u8, at: u64) -> Result<SessionGrant, Denied> {
        self.sign_in_as(SessionKind::App, secret, id, n, at)
    }

    pub fn sign_in_as(
        &mut self,
        kind: SessionKind,
        secret: u8,
        id: u64,
        n: u8,
        at: u64,
    ) -> Result<SessionGrant, Denied> {
        let receipt = self.bind(secret, id, at)?;
        let next = 1 + self
            .apart
            .devices
            .values()
            .map(|d| d.last_session)
            .max()
            .unwrap_or(0);
        let fresh = FreshSession {
            id: SessionId::new(next).unwrap(),
            hash: hash(n),
            sign_out: hash(n ^ 128),
        };
        self.step(at, |turn| turn.sign_in_as(kind, receipt, fresh, None))
    }

    pub fn leave_receipt(
        &mut self,
        id: u64,
        n: u8,
        purpose: LeaveFor,
        at: u64,
    ) -> Result<PasswordReceipt, Denied> {
        let binding = self.step(at, |turn| {
            turn.check_leave(bearer(id, &hash(n)), intent(purpose), hash(42))
        })?;
        binding.check(hash(42), clock(at, self.mark))
    }

    pub fn leave(&mut self, id: u64, n: u8, purpose: LeaveFor, at: u64) -> Result<(), Denied> {
        let receipt = self.leave_receipt(id, n, purpose, at)?;
        self.step(at, |turn| turn.grant_leave(receipt, grant()))
    }

    pub fn read(&mut self, id: u64, n: u8, at: u64) -> Result<Request, Denied> {
        self.step(at, |turn| turn.arrive(bearer(id, &hash(n)), Access::Read))
    }

    pub fn reload(&mut self) {
        self.account = Account::load(&self.apart, Some(&self.data));
    }
}

pub(super) fn req(number: u64) -> RequestId {
    RequestId::new(number).unwrap()
}

pub(super) fn intent(change: LeaveFor) -> GrantIntent {
    GrantIntent {
        request_id: req(1),
        change,
    }
}

pub(super) fn grant_use(secret: &GrantSecret) -> GrantUse<'_> {
    GrantUse {
        request_id: req(1),
        secret,
    }
}

pub(super) fn grant() -> &'static GrantSecret {
    static SECRET: std::sync::OnceLock<GrantSecret> = std::sync::OnceLock::new();
    SECRET.get_or_init(|| GrantSecret::from_bytes([14; SECRET_BYTES]))
}

pub(super) fn bearer(device: u64, hash: &Fingerprint) -> Bearer<'_> {
    Bearer {
        device: dev(device),
        hash,
        grant: Some(grant_use(grant())),
    }
}

#[test]
fn checked_password_receipts_are_single_purpose_scoped_and_short_lived() -> Result<(), Denied> {
    let mut s = Stored::new(state("alice@example.test"));
    let secret = DeviceSecret::from_bytes([8; 32]);
    let wrong = s.step(0, |turn| turn.check_sign_in(&secret, dev(1), hash(42)))?;
    assert_eq!(
        wrong.check(hash(41), clock(0, 0)).err(),
        Some(Denied::WrongPassword)
    );
    let receipt = s.bind(8, 1, 0)?;
    assert_eq!(
        s.step(0, |turn| turn.grant_leave(receipt, grant())),
        Err(Denied::NotSignedIn)
    );
    let receipt = s.bind(8, 1, 1)?;
    assert_eq!(
        s.step(5_001, |turn| turn.sign_in(receipt, fresh(1), None)),
        Err(Denied::Time)
    );
    let receipt = s.bind(8, 1, 5_002)?;
    let mut other = Stored::new(state("bob@example.test"));
    assert_eq!(
        other.step(5_002, |turn| turn.sign_in(receipt, fresh(1), None)),
        Err(Denied::NotSignedIn)
    );
    s.sign_in(8, 1, 1, 5_003)?;
    let receipt = s.leave_receipt(1, 1, LeaveFor::ChangePassword, 5_004)?;
    assert_eq!(
        s.step(5_004, |turn| turn.sign_in(receipt, fresh(2), None)),
        Err(Denied::WrongPurpose)
    );
    assert_eq!(
        s.step(5_005, |turn| turn.change_password(bearer(1, &hash(1)))),
        Err(Denied::LeaveRequired)
    );
    Ok(())
}

#[test]
fn sign_in_reuses_a_device_by_secret_and_retires_its_old_session() -> Result<(), Denied> {
    let mut s = Stored::new(state("alice@example.test"));
    assert_eq!(s.sign_in(8, 1, 1, 0)?.device, dev(1));
    let old = s.read(1, 1, 1)?;
    let old_receipt = s.bind(8, 91, 2)?;
    assert_eq!(s.sign_in(8, 91, 2, 2)?.device, dev(1));
    assert_eq!(s.read(1, 1, 3).err(), Some(Denied::NotSignedIn));
    assert_eq!(
        s.step(3, |turn| turn.sign_in(old_receipt, fresh(3), None)),
        Err(Denied::NotSignedIn)
    );
    assert_eq!(s.sign_in(9, 2, 1, 4)?.device, dev(2));
    assert_eq!(
        s.step(4, |turn| turn.complete(old)),
        Err(Denied::NotSignedIn)
    );
    s.read(1, 2, 4)?;
    s.read(2, 1, 4)?;
    assert_eq!(s.data.devices.len(), 2);
    Ok(())
}

#[test]
fn activity_renews_idle_time_and_expiry_is_a_durable_ending_on_denial() -> Result<(), Denied> {
    let mut s = Stored::new(state("alice@example.test"));
    s.sign_in(8, 1, 1, 0)?;
    for at in [
        7 * 24 * 60 * 60 * 1_000,
        SESSION_IDLE_MS - 1,
        SESSION_IDLE_MS + 2,
    ] {
        s.read(1, 1, at)?;
    }
    let prior = s.apart.devices.get(&dev(1)).unwrap().number;
    let old_copy = s.data.clone();
    assert_eq!(
        s.read(1, 2, 2 * SESSION_IDLE_MS + 2).err(),
        Some(Denied::NotSignedIn)
    );
    assert_eq!(s.apart.devices.get(&dev(1)).unwrap().number, prior + 1);
    s.data = old_copy;
    s.reload();
    assert_eq!(s.read(1, 1, 0).err(), Some(Denied::NotSignedIn));
    assert_eq!(s.mark, 2 * SESSION_IDLE_MS + 2);
    Ok(())
}

#[test]
fn write_completion_checks_session_identity_and_its_own_deadline() -> Result<(), Denied> {
    let mut s = Stored::new(state("alice@example.test"));
    s.sign_in(8, 1, 1, 0)?;
    let write = s.step(1, |turn| turn.arrive(bearer(1, &hash(1)), Access::Write))?;
    assert_eq!(
        s.step(WRITE_MS + 1, |turn| turn.complete(write)),
        Err(Denied::Time)
    );
    let write = s.step(WRITE_MS + 2, |turn| {
        turn.arrive(bearer(1, &hash(1)), Access::Write)
    })?;
    s.sign_in(8, 1, 2, WRITE_MS + 3)?;
    s.sign_in(8, 1, 1, WRITE_MS + 4)?;
    assert_eq!(
        s.step(WRITE_MS + 5, |turn| turn.complete(write)),
        Err(Denied::NotSignedIn)
    );
    Ok(())
}

#[test]
fn force_out_and_sign_in_in_both_orders_fence_old_sessions_and_receipts() -> Result<(), Denied> {
    for sign_in_first in [false, true] {
        let mut s = Stored::new(state("alice@example.test"));
        s.sign_in(8, 1, 1, 0)?;
        let signed = s.sign_in(9, 2, 2, 0)?;
        let target = SessionTarget {
            device: signed.device,
            session: signed.session,
        };
        let receipt = s.bind(9, 2, 1)?;
        let old = s.read(2, 2, 1)?;
        if sign_in_first {
            s.step(2, |turn| turn.sign_in(receipt, fresh(3), None))?;
            s.step(2, |turn| turn.force_out(bearer(1, &hash(1)), target))?;
        } else {
            s.step(2, |turn| turn.force_out(bearer(1, &hash(1)), target))?;
            assert_eq!(
                s.step(2, |turn| turn.sign_in(receipt, fresh(3), None)),
                Err(Denied::NotSignedIn)
            );
        }
        let number = s.apart.devices.get(&dev(2)).unwrap().number;
        s.step(2, |turn| turn.force_out(bearer(1, &hash(1)), target))?;
        assert_eq!(s.apart.devices.get(&dev(2)).unwrap().number, number);
        s.read(1, 1, 2)?;
        assert_eq!(s.read(2, 2, 2).err(), Some(Denied::NotSignedIn));
        if sign_in_first {
            s.read(2, 3, 2)?;
        } else {
            assert_eq!(s.read(2, 3, 2).err(), Some(Denied::NotSignedIn));
            s.sign_in(9, 2, 3, 2)?;
            s.step(2, |turn| turn.force_out(bearer(1, &hash(1)), target))?;
            s.read(2, 3, 2)?;
        }
        assert_eq!(
            s.step(2, |turn| turn.complete(old)),
            Err(Denied::NotSignedIn)
        );
        let leave = s.leave_receipt(1, 1, LeaveFor::DeleteHistory, 2)?;
        s.step(3, |turn| turn.sign_out(bearer(1, &hash(1))))?;
        assert_eq!(
            s.step(3, |turn| turn.grant_leave(leave, grant())),
            Err(Denied::NotSignedIn)
        );
    }
    Ok(())
}

#[test]
fn password_change_keeps_only_its_session_in_both_orders_with_other_sign_in() -> Result<(), Denied>
{
    for sign_in_first in [false, true] {
        let mut s = Stored::new(state("alice@example.test"));
        s.sign_in(8, 1, 1, 0)?;
        s.sign_in(9, 2, 2, 0)?;
        let receipt = s.bind(9, 2, 1)?;
        s.leave(1, 1, LeaveFor::ChangePassword, 1)?;
        let inflight = s.read(2, 2, 1)?;
        if sign_in_first {
            s.step(2, |turn| turn.sign_in(receipt, fresh(3), None))?;
            s.step(2, |turn| turn.change_password(bearer(1, &hash(1))))?;
        } else {
            s.step(2, |turn| turn.change_password(bearer(1, &hash(1))))?;
            assert_eq!(
                s.step(2, |turn| turn.sign_in(receipt, fresh(3), None)),
                Err(Denied::NotSignedIn)
            );
        }
        s.reload();
        assert_eq!(s.apart.account.password_changes, 1);
        assert_eq!(
            s.step(2, |turn| turn.complete(inflight)),
            Err(Denied::NotSignedIn)
        );
        assert_eq!(s.read(2, 3, 2).err(), Some(Denied::NotSignedIn));
        assert_eq!(s.read(2, 2, 2).err(), Some(Denied::NotSignedIn));
        s.read(1, 1, 2)?;
        assert_eq!(
            s.step(2, |turn| turn.change_password(bearer(1, &hash(1)))),
            Err(Denied::LeaveRequired)
        );
    }
    Ok(())
}

#[test]
fn code_redemption_spends_the_code_sets_password_and_signs_in_once() -> Result<(), Denied> {
    let mut account = state("alice@example.test");
    account.password_reset_required = true;
    account.code = Some(LiveCode::new(1, 0)?);
    let mut s = Stored::new(account);
    let bind = |s: &mut Stored, secret: u8, id| {
        s.step(0, |turn| {
            turn.check_code(
                &DeviceSecret::from_bytes([secret; 32]),
                dev(id),
                1,
                hash(43),
            )
        })?
        .check(hash(43), clock(0, 0))
    };
    let wrong_purpose = bind(&mut s, 8, 1)?;
    assert_eq!(
        s.step(0, |turn| turn.sign_in(wrong_purpose, fresh(1), None)),
        Err(Denied::WrongPurpose)
    );
    let first = bind(&mut s, 8, 1)?;
    let second = bind(&mut s, 9, 2)?;
    let old_data = s.data.clone();
    s.account
        .sign_in_turn(clock(1, 0), |turn| {
            turn.redeem_code(first, hash(44), fresh(1), None)
        })
        .apply(|effects| {
            let u = effects.updates.first().unwrap();
            assert_eq!(
                u.change,
                DataChange::RedeemCode {
                    code: 1,
                    verifier: hash(44)
                }
            );
            assert!(u.apart_first.account.code.unwrap().used);
            assert!(u.apart_first.devices.get(&dev(1)).unwrap().permit.is_none());
            let mut crash = Account::load(&u.apart_first, Some(&old_data));
            assert_eq!(
                crash
                    .sign_in_turn(clock(1, 1), |turn| {
                        turn.check_code(&DeviceSecret::from_bytes([8; 32]), dev(2), 1, hash(43))
                    })
                    .apply(|_| Ok::<_, Denied>(()))?
                    .err(),
                Some(Denied::WrongPassword)
            );
            sign_in_after_password_commit(&u.apart_first, &u.data, 1)?;
            Account::restore(
                &u.apart_last,
                Some(&old_data),
                2,
                RestoreMode::DataOnly,
                clock(2, 1),
            )
            .apply(|effects| {
                let restored = effects.updates.first().unwrap();
                assert!(restored.apart_last.account.code.unwrap().used);
                assert!(restored.apart_last.account.password_reset_required);
                Ok::<_, Denied>(())
            })??;
            s.apart = u.apart_last.clone();
            s.data = u.data.clone();
            Ok::<_, Denied>(())
        })??;
    assert_eq!(
        s.step(1, |turn| turn.redeem_code(second, hash(44), fresh(2), None)),
        Err(Denied::NotSignedIn)
    );
    s.reload();
    s.read(1, 1, 1)?;
    assert!(!s.apart.account.password_reset_required);
    assert_eq!(bind(&mut s, 8, 1).err(), Some(Denied::WrongPassword));
    Ok(())
}

#[test]
fn leave_is_one_kind_one_use_and_cannot_reopen_after_ten_minutes() -> Result<(), Denied> {
    let mut s = Stored::new(state("alice@example.test"));
    s.sign_in(8, 1, 1, 0)?;
    s.sign_in(9, 2, 2, 0)?;
    s.leave(1, 1, LeaveFor::DeleteHistory, 1)?;
    assert_eq!(
        s.step(2, |turn| turn.change_password(bearer(1, &hash(1)))),
        Err(Denied::LeaveRequired)
    );
    assert_eq!(
        s.step(2, |turn| turn.delete_history(bearer(2, &hash(2)))),
        Err(Denied::LeaveRequired)
    );
    assert_eq!(
        s.step(1 + PASSWORD_LEAVE_MS, |turn| turn
            .delete_history(bearer(1, &hash(1)))),
        Err(Denied::LeaveRequired)
    );
    s.reload();
    assert_eq!(
        s.step(PASSWORD_LEAVE_MS, |turn| turn
            .delete_history(bearer(1, &hash(1)))),
        Err(Denied::LeaveRequired)
    );
    assert_eq!(s.mark, 1 + PASSWORD_LEAVE_MS);
    s.leave(1, 1, LeaveFor::DeleteHistory, 2 + PASSWORD_LEAVE_MS)?;
    let old = s.step(2 + PASSWORD_LEAVE_MS, |turn| {
        turn.arrive(bearer(2, &hash(2)), Access::WriteHistory)
    })?;
    let at = 2 * PASSWORD_LEAVE_MS + 1;
    s.step(at, |turn| turn.delete_history(bearer(1, &hash(1))))?;
    assert_eq!(
        s.step(at, |turn| turn.delete_history(bearer(1, &hash(1)))),
        Err(Denied::LeaveRequired)
    );
    assert_eq!(
        s.step(at, |turn| turn.complete(old)),
        Err(Denied::NotSignedIn)
    );
    s.read(2, 2, at)?;
    assert!(!may_refill(Some(1), s.apart.account.life));
    assert!(may_refill(Some(2), s.apart.account.life));
    Ok(())
}

#[test]
fn stored_secrets_and_stamp_bytes_match_the_protocol() {
    let p = position();
    let key = StampKey::new([3; SECRET_BYTES]);
    let tag = key.stamp(&p);
    let hex = tag
        .expose_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    assert_eq!(
        hex,
        "e09fda3fd24759238a1a7042f576513cd480634f314a44e3bfc680582f18c00e"
    );
    assert_eq!(
        format!("{tag:?} {key:?} {:?}", DeviceSecret::from_bytes([9; 32])),
        "StampTag(..) StampKey(..) DeviceSecret(..)"
    );
    assert_eq!(PASSWORD_LEAVE_MS, 600_000);
    assert_eq!(SESSION_IDLE_MS, 7_776_000_000);
}

#[test]
fn renaming_keeps_identity_and_recreating_an_email_cannot_use_its_old_stamp() -> Result<(), Denied>
{
    let mut s = Stored::new(state("alice@example.test"));
    let key = StampKey::new([3; SECRET_BYTES]);
    let before = s.apart.account.position(10);
    let receipt = s.bind(8, 1, 0)?;
    let renamed = Email::new("renamed@example.test").unwrap();
    s.apart.account.email = renamed.clone();
    s.data.account.email = renamed;
    s.reload();
    s.step(1, |turn| turn.sign_in(receipt, fresh(1), None))?;
    assert_eq!(s.apart.account.position(10), before);
    let mut recreated = state("alice@example.test");
    recreated.id = acct(42);
    let current = recreated.position(0);
    assert_eq!(key.presented(&current, &before, &key.stamp(&before)), None);
    Ok(())
}

#[test]
fn only_hourly_activity_is_persisted_and_recovery_never_makes_it_older() -> Result<(), Denied> {
    let mut s = Stored::new(state("alice@example.test"));
    s.sign_in(8, 1, 1, 0)?;
    let secret = DeviceSecret::from_bytes([8; 32]);
    assert!(
        s.step(1, |turn| turn.check_sign_in(&secret, dev(9), hash(42)))?
            .known_device()
    );
    assert!(
        !s.step(1, |turn| turn.check_sign_in(
            &DeviceSecret::from_bytes([9; 32]),
            dev(9),
            hash(42)
        ))?
        .known_device()
    );
    let last = |s: &Stored| {
        s.data
            .devices
            .get(&dev(1))
            .unwrap()
            .session
            .as_ref()
            .unwrap()
            .last_used
    };
    s.read(1, 1, USE_WRITE_MS - 1)?;
    assert_eq!(last(&s), 0);
    s.read(1, 1, USE_WRITE_MS)?;
    s.read(1, 1, 2 * USE_WRITE_MS - 1)?;
    assert_eq!(last(&s), USE_WRITE_MS);
    s.reload();
    assert_eq!(
        s.read(1, 2, SESSION_IDLE_MS + USE_WRITE_MS).err(),
        Some(Denied::NotSignedIn)
    );
    assert!(s.data.devices.get(&dev(1)).unwrap().session.is_some());
    assert_eq!(
        s.read(1, 1, SESSION_IDLE_MS + 2 * USE_WRITE_MS - 1).err(),
        Some(Denied::NotSignedIn)
    );
    assert!(s.data.devices.get(&dev(1)).unwrap().session.is_none());
    Ok(())
}

#[test]
fn a_grant_secret_cannot_be_omitted_or_spend_its_replacement() -> Result<(), Denied> {
    let mut s = Stored::new(state("alice@example.test"));
    s.sign_in(8, 1, 1, 0)?;
    s.leave(1, 1, LeaveFor::DeleteHistory, 1)?;
    let replacement = GrantSecret::from_bytes([15; SECRET_BYTES]);
    let receipt = s.leave_receipt(1, 1, LeaveFor::DeleteHistory, 1)?;
    s.step(1, |turn| turn.grant_leave(receipt, &replacement))?;
    for grant in [None, Some(grant_use(grant()))] {
        assert_eq!(
            s.step(1, |turn| turn.delete_history(Bearer {
                device: dev(1),
                hash: &hash(1),
                grant
            })),
            Err(Denied::LeaveRequired)
        );
    }
    s.reload();
    s.step(1, |turn| {
        turn.delete_history(Bearer {
            device: dev(1),
            hash: &hash(1),
            grant: Some(grant_use(&replacement)),
        })
    })?;
    assert_eq!(s.apart.account.life, 2);
    Ok(())
}

pub(super) fn sign_in_after_password_commit(
    apart: &Apart,
    data: &Rows,
    at: u64,
) -> Result<(), Denied> {
    let mut recovered = Account::load(apart, Some(data));
    assert!(!recovered.state().password_reset_required);
    let check = recovered
        .sign_in_turn(clock(at, at), |turn| {
            turn.check_sign_in(&DeviceSecret::from_bytes([8; 32]), dev(1), hash(44))
        })
        .apply(|_| Ok::<_, Denied>(()))??;
    let receipt = check.check(hash(44), clock(at, at))?;
    recovered
        .sign_in_turn(clock(at, at), |turn| turn.sign_in(receipt, fresh(3), None))
        .apply(|_| Ok::<_, Denied>(()))??;
    Ok(())
}

#[test]
fn a_note_only_ends_its_session_and_ordinary_sign_out_preserves_the_known_door()
-> Result<(), Denied> {
    let mut s = Stored::new(state("alice@example.test"));
    let signed = s.sign_in(8, 1, 1, 0)?;
    let target = SessionTarget {
        device: signed.device,
        session: signed.session,
    };
    let note = hash(1 ^ 128);
    for access in [Access::Read, Access::Write] {
        assert_eq!(
            s.step(1, |t| t.arrive(bearer(1, &note), access)).err(),
            Some(Denied::NotSignedIn)
        );
    }
    assert_eq!(
        s.step(1, |t| t.check_leave(
            bearer(1, &note),
            intent(LeaveFor::DeleteHistory),
            hash(42)
        ))
        .err(),
        Some(Denied::NotSignedIn)
    );
    assert_eq!(
        s.step(1, |t| t.sign_out_by_note(target, &hash(1))),
        Err(Denied::NotSignedIn)
    );
    s.account
        .request(1, clock(1, 1), |t| t.sign_out_by_note(target, &note))
        .apply(|effects| {
            let u = effects.updates.first().unwrap();
            let apart = u.apart_first.devices.get(&dev(1)).unwrap();
            assert!(apart.permit.is_none());
            assert!(apart.known);
            s.apart = u.apart_last.clone();
            s.data = u.data.clone();
            Ok::<_, Denied>(())
        })??;
    s.reload();
    assert!(
        s.step(1, |t| t.check_sign_in(
            &DeviceSecret::from_bytes([8; 32]),
            dev(9),
            hash(42)
        ))?
        .known_device()
    );
    let replacement = s.sign_in(8, 1, 2, 2)?;
    s.step(2, |t| t.sign_out_by_note(target, &note))?;
    assert_eq!(
        s.step(2, |t| t.sign_out_by_note(
            SessionTarget {
                device: replacement.device,
                session: replacement.session
            },
            &note
        )),
        Err(Denied::NotSignedIn)
    );
    s.read(1, 2, 2)?;
    Ok(())
}

#[test]
fn a_forced_out_row_is_only_known_again_after_a_successful_sign_in() -> Result<(), Denied> {
    let mut s = Stored::new(state("alice@example.test"));
    s.sign_in(8, 1, 1, 0)?;
    let old = s.sign_in(9, 2, 2, 0)?;
    let before = s.data.clone();
    let target = SessionTarget {
        device: old.device,
        session: old.session,
    };
    s.step(1, |t| t.force_out(bearer(1, &hash(1)), target))?;
    s.data = before;
    s.reload();
    let secret = DeviceSecret::from_bytes([9; 32]);
    let check = s.step(1, |t| t.check_sign_in(&secret, dev(99), hash(42)))?;
    assert_eq!(check.known_device_id(), None);
    assert_eq!(
        check.check(hash(41), clock(1, 1)).err(),
        Some(Denied::WrongPassword)
    );
    assert!(!s.apart.devices.get(&dev(2)).unwrap().known);
    assert_eq!(s.sign_in(9, 99, 3, 2)?.device, dev(2));
    assert_eq!(
        s.step(2, |t| t.check_sign_in(&secret, dev(99), hash(42)))?
            .known_device_id(),
        Some(dev(2))
    );
    Ok(())
}

#[test]
fn a_new_accounts_code_expires_before_both_check_and_redemption() -> Result<(), Denied> {
    let code = LiveCode::new(9, 10)?;
    let account = AccountState::new(
        BackendId::from_bytes([4; 16]),
        1,
        acct(1),
        Email::new("alice@example.test").unwrap(),
        Some(code),
    );
    assert!(account.password_reset_required);
    assert_eq!(account.status, Status::On);
    assert_eq!(account.music_login, 0);
    let mut s = Stored::new(account);
    let device = DeviceSecret::from_bytes([8; 32]);
    let at = code.expires - 1;
    let checked = s
        .step(at, |t| t.check_code(&device, dev(1), 9, hash(42)))?
        .check(hash(42), clock(at, at))?;
    assert_eq!(
        s.step(code.expires, |t| t.redeem_code(
            checked,
            hash(43),
            fresh(1),
            None
        )),
        Err(Denied::WrongPassword)
    );
    assert_eq!(
        s.step(at, |t| t.check_code(&device, dev(1), 9, hash(42)))
            .err(),
        Some(Denied::WrongPassword)
    );
    Ok(())
}

#[test]
fn admin_page_idle_is_thirty_minutes_from_last_use_even_after_reload() -> Result<(), Denied> {
    let mut authority = state("alice@example.test");
    authority.admin = true;
    let mut s = Stored::new(authority);
    let receipt = s.bind(8, 1, 0)?;
    let browser = s.step(0, |turn| {
        turn.sign_in_as(SessionKind::AdminPage, receipt, fresh(1), None)
    })?;
    assert_eq!(browser.kind, SessionKind::AdminPage);
    s.sign_in(9, 2, 2, 0)?;
    let last = ADMIN_PAGE_IDLE_MS - 1;
    assert_eq!(s.read(1, 1, last)?.kind(), SessionKind::AdminPage);
    assert_eq!(
        s.data
            .devices
            .get(&dev(1))
            .unwrap()
            .session
            .as_ref()
            .unwrap()
            .last_used,
        last
    );
    let mut before_deadline = Account::load(&s.apart, Some(&s.data));
    let before = last + ADMIN_PAGE_IDLE_MS - 1;
    let request = before_deadline
        .request(1, clock(before, before), |turn| {
            turn.arrive(bearer(1, &hash(1)), Access::Read)
        })
        .apply(|_| Ok::<_, Denied>(()))??;
    assert_eq!(request.kind(), SessionKind::AdminPage);
    s.reload();
    assert_eq!(
        s.read(1, 1, last + ADMIN_PAGE_IDLE_MS).err(),
        Some(Denied::NotSignedIn)
    );
    assert!(s.apart.devices.get(&dev(1)).unwrap().permit.is_none());
    assert_eq!(
        s.step(last + ADMIN_PAGE_IDLE_MS, |turn| turn
            .session_kind(bearer(2, &hash(2))))?,
        SessionKind::App
    );
    Ok(())
}

#[test]
fn a_correct_non_admin_page_sign_in_has_the_same_refusal_as_a_wrong_password() -> Result<(), Denied>
{
    let mut s = Stored::new(state("alice@example.test"));
    let secret = DeviceSecret::from_bytes([8; 32]);
    let wrong = s
        .step(0, |turn| turn.check_sign_in(&secret, dev(1), hash(42)))?
        .check(hash(41), clock(0, 0))
        .err();
    let checked = s.bind(8, 1, 0)?;
    s.account
        .sign_in_turn(clock(0, 0), |turn| {
            turn.sign_in_as(SessionKind::AdminPage, checked, fresh(1), None)
        })
        .apply(|effects| {
            assert!(effects.updates.is_empty());
            Ok::<_, Denied>(())
        })
        .map(|answer| assert_eq!(answer.err(), wrong))?;
    assert_eq!(wrong, Some(Denied::WrongPassword));
    assert!(wrong.is_some_and(Denied::counts_as_wrong_password));
    s.sign_in(8, 1, 1, 0)?;
    Ok(())
}
