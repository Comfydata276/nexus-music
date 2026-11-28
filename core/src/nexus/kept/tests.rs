use super::*;

fn position() -> Position {
    Position {
        backend: state("alice@example.test").backend,
        account: name("alice@example.test"),
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
            Some(Restore::HoldThrough(3)),
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
                account: name("bob@example.test"),
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
            account: name("bob@example.test"),
            ..p.clone()
        },
    ] {
        assert!(!may_apply(1, 1, &p, &other));
    }
}

pub(super) fn name(email: &str) -> AccountName {
    AccountName::from_canonical(email.to_owned()).unwrap()
}

pub(super) fn state(email: &str) -> AccountState {
    AccountState {
        backend: BackendId::from_bytes([4; 16]),
        restore: 1,
        name: name(email),
        life: 1,
        status: Status::On,
        password_changes: 0,
        password_reset_required: false,
        code_available: false,
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
    Clock { now, high_water }
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
                    self.data = update.data.clone();
                    self.apart = update.apart_last.clone();
                }
                Ok(())
            })?
    }

    pub fn bind(&mut self, secret: u8, id: u64, at: u64) -> Result<Receipt, Denied> {
        let binding = self.step(at, |turn| {
            turn.check_sign_in(&DeviceSecret::new([secret; 32]), id, hash(42))
        })?;
        binding.check(hash(42), clock(at, self.mark))
    }

    pub fn sign_in(&mut self, secret: u8, id: u64, n: u8, at: u64) -> Result<SignedIn, Denied> {
        let receipt = self.bind(secret, id, at)?;
        self.step(at, |turn| turn.sign_in(receipt, hash(n), None))
    }

    pub fn leave_receipt(
        &mut self,
        id: u64,
        n: u8,
        purpose: LeaveFor,
        at: u64,
    ) -> Result<Receipt, Denied> {
        let binding = self.step(at, |turn| {
            turn.check_leave(bearer(id, &hash(n)), purpose, hash(42))
        })?;
        binding.check(hash(42), clock(at, self.mark))
    }

    pub fn leave(&mut self, id: u64, n: u8, purpose: LeaveFor, at: u64) -> Result<(), Denied> {
        let receipt = self.leave_receipt(id, n, purpose, at)?;
        self.step(at, |turn| turn.grant_leave(receipt))
    }

    pub fn read(&mut self, id: u64, n: u8, at: u64) -> Result<Request, Denied> {
        self.step(at, |turn| turn.arrive(bearer(id, &hash(n)), Access::Read))
    }

    pub fn reload(&mut self) {
        self.account = Account::load(&self.apart, Some(&self.data));
    }
}

pub(super) fn bearer(device: u64, hash: &Fingerprint) -> Bearer<'_> {
    Bearer { device, hash }
}

#[test]
fn checked_password_receipts_are_single_purpose_scoped_and_short_lived() -> Result<(), Denied> {
    let mut s = Stored::new(state("alice@example.test"));
    let secret = DeviceSecret::new([8; 32]);
    let wrong = s.step(0, |turn| turn.check_sign_in(&secret, 1, hash(42)))?;
    assert_eq!(
        wrong.check(hash(41), clock(0, 0)).err(),
        Some(Denied::WrongPassword)
    );
    let receipt = s.bind(8, 1, 0)?;
    assert_eq!(
        s.step(0, |turn| turn.grant_leave(receipt)),
        Err(Denied::NotSignedIn)
    );
    let receipt = s.bind(8, 1, 1)?;
    assert_eq!(
        s.step(5_001, |turn| turn.sign_in(receipt, hash(1), None)),
        Err(Denied::Time)
    );
    let receipt = s.bind(8, 1, 5_002)?;
    let mut other = Stored::new(state("bob@example.test"));
    assert_eq!(
        other.step(5_002, |turn| turn.sign_in(receipt, hash(1), None)),
        Err(Denied::NotSignedIn)
    );
    s.sign_in(8, 1, 1, 5_003)?;
    let receipt = s.leave_receipt(1, 1, LeaveFor::ChangePassword, 5_004)?;
    assert_eq!(
        s.step(5_004, |turn| turn.sign_in(receipt, hash(2), None)),
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
    assert_eq!(s.sign_in(8, 1, 1, 0)?.device, 1);
    let old = s.read(1, 1, 1)?;
    let old_receipt = s.bind(8, 91, 2)?;
    assert_eq!(s.sign_in(8, 91, 2, 2)?.device, 1);
    assert_eq!(s.read(1, 1, 3).err(), Some(Denied::NotSignedIn));
    assert_eq!(
        s.step(3, |turn| turn.sign_in(old_receipt, hash(3), None)),
        Err(Denied::NotSignedIn)
    );
    assert_eq!(s.sign_in(9, 2, 1, 4)?.device, 2);
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
    for at in [7 * 24 * 60 * 60 * 1_000, SESSION_MS - 1, SESSION_MS + 2] {
        s.read(1, 1, at)?;
    }
    let prior = s.apart.devices.get(&1).unwrap().number;
    let old_copy = s.data.clone();
    assert_eq!(
        s.read(1, 2, 2 * SESSION_MS + 2).err(),
        Some(Denied::NotSignedIn)
    );
    assert_eq!(s.apart.devices.get(&1).unwrap().number, prior + 1);
    s.data = old_copy;
    s.reload();
    assert_eq!(s.read(1, 1, 0).err(), Some(Denied::NotSignedIn));
    assert_eq!(s.mark, 2 * SESSION_MS + 2);
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
        s.sign_in(9, 2, 2, 0)?;
        let receipt = s.bind(9, 2, 1)?;
        let old = s.read(2, 2, 1)?;
        if sign_in_first {
            s.step(2, |turn| turn.sign_in(receipt, hash(3), None))?;
            s.step(2, |turn| turn.force_out(bearer(1, &hash(1)), 2))?;
        } else {
            s.step(2, |turn| turn.force_out(bearer(1, &hash(1)), 2))?;
            assert_eq!(
                s.step(2, |turn| turn.sign_in(receipt, hash(3), None)),
                Err(Denied::NotSignedIn)
            );
        }
        let number = s.apart.devices.get(&2).unwrap().number;
        s.step(2, |turn| turn.force_out(bearer(1, &hash(1)), 2))?;
        assert_eq!(s.apart.devices.get(&2).unwrap().number, number);
        s.read(1, 1, 2)?;
        for n in [2, 3] {
            assert_eq!(s.read(2, n, 2).err(), Some(Denied::NotSignedIn));
        }
        assert_eq!(
            s.step(2, |turn| turn.complete(old)),
            Err(Denied::NotSignedIn)
        );
        let leave = s.leave_receipt(1, 1, LeaveFor::DeleteHistory, 2)?;
        s.step(3, |turn| turn.sign_out(bearer(1, &hash(1))))?;
        assert_eq!(
            s.step(3, |turn| turn.grant_leave(leave)),
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
            s.step(2, |turn| turn.sign_in(receipt, hash(3), None))?;
            s.step(2, |turn| turn.change_password(bearer(1, &hash(1))))?;
        } else {
            s.step(2, |turn| turn.change_password(bearer(1, &hash(1))))?;
            assert_eq!(
                s.step(2, |turn| turn.sign_in(receipt, hash(3), None)),
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
fn a_code_session_only_chooses_a_password_and_the_code_cannot_sign_in_twice() -> Result<(), Denied>
{
    let mut account = state("alice@example.test");
    account.password_reset_required = true;
    account.code_available = true;
    let mut s = Stored::new(account);
    let binding = |s: &mut Stored, secret: u8, id| {
        s.step(0, |turn| {
            turn.check_code(&DeviceSecret::new([secret; 32]), id, hash(43))
        })?
        .check(hash(43), clock(0, 0))
    };
    let first = binding(&mut s, 8, 1)?;
    let second = binding(&mut s, 9, 2)?;
    assert_eq!(
        s.step(0, |turn| turn.sign_in(first, hash(1), None))?
            .music_login,
        None
    );
    assert_eq!(
        s.step(0, |turn| turn.sign_in(second, hash(2), None)),
        Err(Denied::NotSignedIn)
    );
    s.reload();
    assert!(s.read(1, 1, 1).is_err());
    s.step(2, |turn| turn.choose_password(bearer(1, &hash(1))))?;
    assert!(s.read(1, 1, 2).is_err());
    assert_eq!(
        s.step(2, |turn| turn.choose_password(bearer(1, &hash(1)))),
        Err(Denied::NotSignedIn)
    );
    assert!(!s.apart.account.password_reset_required);
    s.sign_in(8, 1, 2, 3)?;
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
        s.step(1 + LEAVE_MS, |turn| turn
            .delete_history(bearer(1, &hash(1)))),
        Err(Denied::LeaveRequired)
    );
    s.reload();
    assert_eq!(
        s.step(LEAVE_MS, |turn| turn.delete_history(bearer(1, &hash(1)))),
        Err(Denied::LeaveRequired)
    );
    assert_eq!(s.mark, 1 + LEAVE_MS);
    s.leave(1, 1, LeaveFor::DeleteHistory, 2 + LEAVE_MS)?;
    let old = s.step(2 + LEAVE_MS, |turn| {
        turn.arrive(bearer(2, &hash(2)), Access::WriteHistory)
    })?;
    let at = 2 * LEAVE_MS + 1;
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
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    assert_eq!(
        hex,
        "682f05a5143d474f7be8b1233d2aa7e540972d2f6057a98912c42d5e9be81fe8"
    );
    assert_eq!(
        format!("{tag:?} {key:?} {:?}", DeviceSecret::new([9; 32])),
        "Fingerprint(..) StampKey(..) DeviceSecret(..)"
    );
    assert_eq!(LEAVE_MS, 600_000);
    assert_eq!(SESSION_MS, 7_776_000_000);
}
