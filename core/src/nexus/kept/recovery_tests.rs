use super::tests::{Stored, bearer, clock, dev, fresh, grant, hash, state};
use super::*;

impl Stored {
    fn admin() -> Result<Self, Denied> {
        let mut account = state("admin@example.test");
        account.admin = true;
        let mut admin = Self::new(account);
        admin.sign_in(7, 1, 7, 0)?;
        Ok(admin)
    }

    fn admin_change(
        &mut self,
        target: &mut Stored,
        change: AdminAction,
        at: u64,
    ) -> Result<(), Denied> {
        self.mark = self.mark.max(target.mark);
        let named = AdminTarget::Account(target.apart.account.id);
        self.leave(
            1,
            7,
            LeaveFor::Admin {
                action: change,
                target: named,
            },
            at,
        )?;
        self.account
            .request(self.apart.account.restore, clock(at, self.mark), |turn| {
                turn.administer(
                    bearer(1, &hash(7)),
                    change,
                    named,
                    Some(&mut target.account),
                )
            })
            .apply(|effects| {
                self.mark = effects.high_water;
                target.mark = effects.high_water;
                for update in &effects.updates {
                    let store = if update.data.account.id == target.data.account.id {
                        if change == AdminAction::SetAdmin(false) {
                            assert!(!update.apart_first.account.admin);
                        }
                        &mut *target
                    } else {
                        &mut *self
                    };
                    store.apart = update.apart_first.clone();
                    store.data = update.data.clone();
                    store.apart = update.apart_last.clone();
                }
                Ok(())
            })?
    }

    fn restore_copy(&mut self, copy: &Rows, mode: RestoreMode, at: u64) -> Result<(), Denied> {
        Account::restore(
            &self.apart,
            Some(copy),
            self.apart.account.restore + 1,
            mode,
            clock(at, self.mark),
        )
        .apply(|effects| {
            self.mark = effects.high_water;
            for update in &effects.updates {
                self.apart = update.apart_first.clone();
                assert_eq!(self.apart.account.restore, update.data.account.restore);
                self.data = update.data.clone();
                self.apart = update.apart_last.clone();
            }
            Ok(())
        })??;
        self.reload();
        Ok(())
    }
}

#[test]
fn account_off_and_delete_win_both_orders_with_sign_in_and_survive_an_older_copy()
-> Result<(), Denied> {
    for (action, denial) in [
        (AdminAction::SetOn(false), Denied::NotSignedIn),
        (AdminAction::DeleteAccount, Denied::NotSignedIn),
    ] {
        for sign_in_first in [false, true] {
            let mut admin = Stored::admin()?;
            let mut person = Stored::new(state("alice@example.test"));
            person.sign_in(8, 1, 1, 0)?;
            person.sign_in(9, 2, 2, 0)?;
            let older = person.data.clone();
            let receipt = person.bind(9, 2, 1)?;
            let inflight = person.read(2, 2, 1)?;
            if sign_in_first {
                person.step(2, |turn| turn.sign_in(receipt, fresh(3), None))?;
                admin.admin_change(&mut person, action, 2)?;
            } else {
                admin.admin_change(&mut person, action, 2)?;
                assert_eq!(
                    person.step(2, |turn| turn.sign_in(receipt, fresh(3), None)),
                    Err(Denied::NotSignedIn)
                );
            }
            assert_eq!(person.step(2, |turn| turn.complete(inflight)), Err(denial));
            assert_eq!(person.read(1, 1, 2).err(), Some(denial));
            assert_eq!(person.read(2, 3, 2).err(), Some(denial));
            assert!(
                person
                    .apart
                    .devices
                    .values()
                    .all(|device| device.number >= 1)
            );
            person.restore_copy(&older, RestoreMode::DataOnly, 3)?;
            assert_eq!(person.read(1, 1, 3).err(), Some(denial));
            if action == AdminAction::DeleteAccount {
                assert_eq!(person.apart.account.status, Status::Deleted);
                assert!(person.data.devices.is_empty());
            } else {
                assert_eq!(person.apart.account.status, Status::Off);
                assert_eq!(person.sign_in(8, 1, 4, 3), Err(Denied::AccountOff));
                admin.apart.account.restore = 2;
                admin.data.account.restore = 2;
                // Ordinary startup would restore the admin too. Use a newly signed-in
                // administrator under the new restore number here.
                let mut admin = Stored::new(admin.data.account.clone());
                admin.sign_in(7, 1, 7, 3)?;
                admin.admin_change(&mut person, AdminAction::SetOn(true), 4)?;
                assert_eq!(person.read(1, 1, 4).err(), Some(Denied::NotSignedIn));
                person.sign_in(8, 1, 4, 4)?;
            }
        }
    }
    Ok(())
}

#[test]
fn restore_fences_password_changes_old_lives_and_old_request_numbers() -> Result<(), Denied> {
    let mut person = Stored::new(state("alice@example.test"));
    person.sign_in(8, 1, 1, 0)?;
    let older = person.data.clone();
    let old = person.read(1, 1, 1)?;
    person.leave(1, 1, LeaveFor::DeleteHistory, 1)?;
    person.step(1, |turn| turn.delete_history(bearer(1, &hash(1))))?;
    person.leave(1, 1, LeaveFor::ChangePassword, 1)?;
    person.step(1, |turn| turn.change_password(bearer(1, &hash(1))))?;
    let stale = person.bind(8, 1, 1)?;
    person.restore_copy(&older, RestoreMode::DataOnly, 2)?;
    assert_eq!(person.apart.account.life, 2);
    assert!(person.apart.account.history_held);
    assert_eq!(person.apart.account.password_changes, 1);
    assert!(person.apart.account.password_reset_required);
    assert_eq!(
        person.step(2, |turn| turn.complete(old)),
        Err(Denied::NotSignedIn)
    );
    assert_eq!(
        person.step(2, |turn| turn.sign_in(stale, fresh(2), None)),
        Err(Denied::NotSignedIn)
    );
    assert_eq!(person.sign_in(8, 1, 2, 2), Err(Denied::WrongPassword));
    assert_eq!(
        person
            .account
            .request(1, clock(2, person.mark), |_| -> Result<(), Denied> {
                panic!("an old restore number reached the operation");
            })
            .apply(|_| Ok::<_, Denied>(())),
        Ok(Err(Denied::StartAgain))
    );
    Ok(())
}

#[test]
fn restore_allows_new_sign_in_but_hold_blocks_history_until_the_person_chooses()
-> Result<(), Denied> {
    for erase in [false, true] {
        let mut person = Stored::new(state("alice@example.test"));
        person.sign_in(8, 1, 1, 0)?;
        let older = person.data.clone();
        person.leave(1, 1, LeaveFor::DeleteHistory, 1)?;
        person.step(1, |turn| turn.delete_history(bearer(1, &hash(1))))?;
        person.restore_copy(&older, RestoreMode::DataOnly, 2)?;
        person.sign_in(8, 1, 2, 2)?;
        for access in [Access::ReadHistory, Access::WriteHistory] {
            assert_eq!(
                person
                    .step(2, |turn| turn.arrive(bearer(1, &hash(2)), access))
                    .err(),
                Some(Denied::HistoryHeld)
            );
        }
        if erase {
            person.leave(1, 2, LeaveFor::DeleteHistory, 2)?;
            person.step(2, |turn| turn.delete_history(bearer(1, &hash(2))))?;
        } else {
            person.step(2, |turn| turn.keep_history(bearer(1, &hash(2))))?;
        }
        let request = person.step(2, |turn| {
            turn.arrive(bearer(1, &hash(2)), Access::WriteHistory)
        })?;
        person.step(2, |turn| turn.complete(request))?;
    }
    Ok(())
}

#[test]
fn admin_grants_name_the_action_and_target_and_die_with_the_role() -> Result<(), Denied> {
    let mut admin = Stored::admin()?;
    let mut alice = Stored::new(state("alice@example.test"));
    let mut bob = Stored::new(state("bob@example.test"));
    alice.sign_in(8, 1, 1, 0)?;
    bob.sign_in(9, 1, 2, 0)?;
    let a = AdminTarget::Account(alice.apart.account.id);
    let b = AdminTarget::Account(bob.apart.account.id);
    let reset = AdminAction::ResetPassword;
    let grant = LeaveFor::Admin {
        action: reset,
        target: a,
    };
    assert_eq!(
        alice.step(1, |turn| turn.admin_read(bearer(1, &hash(1)))),
        Err(Denied::AdminRequired)
    );
    assert_eq!(alice.leave(1, 1, grant, 1), Err(Denied::AdminRequired));
    admin.step(1, |turn| turn.admin_read(bearer(1, &hash(7))))?;
    assert_eq!(
        admin.step(1, |turn| turn.administer(
            bearer(1, &hash(7)),
            reset,
            a,
            Some(&mut alice.account)
        )),
        Err(Denied::LeaveRequired)
    );
    admin.leave(1, 7, grant, 1)?;
    assert_eq!(
        admin.step(1, |turn| turn.administer(
            bearer(1, &hash(7)),
            reset,
            b,
            Some(&mut bob.account)
        )),
        Err(Denied::LeaveRequired)
    );
    assert_eq!(
        admin.step(1, |turn| turn.administer(
            bearer(1, &hash(7)),
            reset,
            a,
            Some(&mut bob.account)
        )),
        Err(Denied::WrongPurpose)
    );
    bob.read(1, 2, 1)?;
    admin.admin_change(&mut alice, AdminAction::SetAdmin(true), 2)?;
    alice.step(2, |turn| turn.admin_read(bearer(1, &hash(1))))?;
    alice.leave(
        1,
        1,
        LeaveFor::Admin {
            action: reset,
            target: b,
        },
        2,
    )?;
    let old = alice.data.clone();
    admin.admin_change(&mut alice, AdminAction::SetAdmin(false), 3)?;
    let mut reverted = Account::load(&alice.apart, Some(&old));
    assert_eq!(
        reverted
            .request(1, clock(3, 3), |turn| turn.admin_read(bearer(1, &hash(1))))
            .apply(|_| Ok::<_, Denied>(()))?,
        Err(Denied::AdminRequired)
    );
    admin.admin_change(&mut alice, AdminAction::SetAdmin(true), 3)?;
    assert_eq!(
        alice.step(3, |turn| turn.administer(
            bearer(1, &hash(1)),
            reset,
            b,
            Some(&mut bob.account)
        )),
        Err(Denied::LeaveRequired)
    );
    Ok(())
}

#[test]
fn admin_reset_ends_all_sessions_and_retires_the_previous_password_and_code() -> Result<(), Denied>
{
    let mut admin = Stored::admin()?;
    let mut user = Stored::new(state("alice@example.test"));
    user.sign_in(8, 1, 1, 0)?;
    user.sign_in(9, 2, 2, 0)?;
    let receipt = user.bind(8, 1, 1)?;
    let copy = user.data.clone();
    admin.admin_change(&mut user, AdminAction::ResetPassword, 2)?;
    assert_eq!(
        user.step(2, |turn| turn.sign_in(receipt, fresh(3), None)),
        Err(Denied::NotSignedIn)
    );
    assert_eq!(user.sign_in(8, 1, 3, 2), Err(Denied::WrongPassword));
    assert!(user.data.devices.values().all(|row| row.session.is_none()));
    assert!(!user.apart.account.code.unwrap().used);
    user.restore_copy(&copy, RestoreMode::DataOnly, 3)?;
    let secret = DeviceSecret::from_bytes([8; 32]);
    assert_eq!(
        user.step(3, |turn| turn.check_code(&secret, dev(1), 1, hash(43)))
            .err(),
        Some(Denied::WrongPassword)
    );
    Ok(())
}

#[test]
fn both_cuts_of_replacement_leave_neither_session_and_reserve_the_new_identity()
-> Result<(), Denied> {
    let mut user = Stored::new(state("alice@example.test"));
    user.sign_in(8, 1, 1, 0)?;
    let before = user.data.clone();
    let receipt = user.bind(8, 1, 1)?;
    user.account
        .request(1, clock(1, user.mark), |turn| {
            turn.sign_in(receipt, fresh(2), None)
        })
        .apply(|e| {
            let update = e.updates.first().unwrap();
            assert_eq!(
                update
                    .apart_first
                    .devices
                    .get(&dev(1))
                    .unwrap()
                    .last_session,
                2
            );
            for rows in [&before, &update.data] {
                let mut recovered = Account::load(&update.apart_first, Some(rows));
                for n in [1, 2] {
                    assert_eq!(
                        recovered
                            .request(1, clock(1, e.high_water), |turn| {
                                turn.arrive(bearer(1, &hash(n)), Access::Read)
                            })
                            .apply(|_| Ok::<_, Denied>(()))?
                            .err(),
                        Some(Denied::NotSignedIn)
                    );
                }
            }
            let mut recovered = Account::load(&update.apart_last, Some(&update.data));
            recovered
                .request(1, clock(1, e.high_water), |turn| {
                    turn.arrive(bearer(1, &hash(2)), Access::Read)
                })
                .apply(|_| Ok::<_, Denied>(()))??;
            Ok(())
        })??;
    Ok(())
}

#[test]
fn a_crash_cannot_restore_consumed_leave_or_recognize_new_leave_early() -> Result<(), Denied> {
    let mut user = Stored::new(state("alice@example.test"));
    user.sign_in(8, 1, 1, 0)?;
    user.leave(1, 1, LeaveFor::ChangePassword, 1)?;
    let before = user.data.clone();
    let receipt = user.leave_receipt(1, 1, LeaveFor::DeleteHistory, 1)?;
    user.account
        .request(1, clock(1, user.mark), |turn| {
            turn.grant_leave(receipt, grant())
        })
        .apply(|e| {
            let update = e.updates.first().unwrap();
            for rows in [&before, &update.data] {
                let mut recovered = Account::load(&update.apart_first, Some(rows));
                for deletion in [false, true] {
                    assert_eq!(
                        recovered
                            .request(1, clock(1, e.high_water), |turn| {
                                if deletion {
                                    turn.delete_history(bearer(1, &hash(1)))
                                } else {
                                    turn.change_password(bearer(1, &hash(1)))
                                }
                            })
                            .apply(|_| Ok::<_, Denied>(()))?,
                        Err(Denied::LeaveRequired)
                    );
                }
            }
            Ok(())
        })??;
    Ok(())
}

#[test]
fn password_fence_lands_first_and_blocks_an_interrupted_password_write() -> Result<(), Denied> {
    let mut user = Stored::new(state("alice@example.test"));
    user.sign_in(8, 1, 1, 0)?;
    user.sign_in(9, 2, 2, 0)?;
    user.leave(1, 1, LeaveFor::ChangePassword, 1)?;
    let before = user.data.clone();
    user.account
        .request(1, clock(2, user.mark), |turn| {
            turn.change_password(bearer(1, &hash(1)))
        })
        .apply(|effects| {
            let update = effects.updates.first().unwrap();
            assert_eq!(update.change, DataChange::ChangePassword);
            assert!(update.apart_first.account.password_reset_required);
            assert_eq!(update.apart_first.account.password_changes, 1);
            assert!(
                update
                    .apart_first
                    .devices
                    .get(&dev(2))
                    .unwrap()
                    .permit
                    .is_none()
            );
            let mut recovered = Account::load(&update.apart_first, Some(&before));
            let check = recovered
                .request(1, clock(2, effects.high_water), |turn| {
                    turn.check_sign_in(&DeviceSecret::from_bytes([8; 32]), dev(1), hash(42))
                })
                .apply(|_| Ok::<_, Denied>(()))??;
            let receipt = check.check(hash(42), clock(2, 2))?;
            assert_eq!(
                recovered
                    .request(1, clock(2, effects.high_water), |turn| {
                        turn.sign_in(receipt, fresh(3), None)
                    })
                    .apply(|_| Ok::<_, Denied>(()))?,
                Err(Denied::WrongPassword)
            );
            Ok(())
        })??;
    Ok(())
}

#[test]
fn managed_force_out_records_rotation_without_advancing_the_count() -> Result<(), Denied> {
    let mut person = state("alice@example.test");
    person.music_managed = true;
    let mut s = Stored::new(person);
    assert_eq!(s.sign_in(8, 1, 1, 0)?.music_login, 1);
    let signed = s.sign_in(9, 2, 2, 0)?;
    let target = SessionTarget {
        device: signed.device,
        session: signed.session,
    };
    s.step(1, |turn| turn.force_out(bearer(1, &hash(1)), target))?;
    s.reload();
    assert_eq!(
        s.apart.account.music_recovery,
        Some(MusicRecovery::RotateManaged)
    );
    assert_eq!(s.apart.account.music_login, 1);
    assert_eq!(s.sign_in(9, 2, 3, 2)?.music_login, 1);
    Ok(())
}

#[test]
fn switching_accounts_keeps_the_rows_separate_and_ends_the_old_session_first() -> Result<(), Denied>
{
    let mut old = Stored::new(state("alice@example.test"));
    old.sign_in(8, 1, 1, 0)?;
    let mut new = Stored::new(state("bob@example.test"));
    let bad = new.bind(9, 2, 1)?;
    assert_eq!(
        new.step(1, |turn| turn.sign_in(
            bad,
            fresh(2),
            Some(&mut old.account)
        )),
        Err(Denied::NotSignedIn)
    );
    old.read(1, 1, 1)?;
    let checked = new.bind(8, 2, 1)?;
    new.account
        .request(1, clock(1, 1), |turn| {
            turn.sign_in(checked, fresh(2), Some(&mut old.account))
        })
        .apply(|effects| {
            let ended = effects
                .updates
                .iter()
                .find(|u| u.data.account.id == old.data.account.id)
                .unwrap();
            let granted = effects
                .updates
                .iter()
                .find(|u| u.data.account.id == new.data.account.id)
                .unwrap();
            assert!(
                ended
                    .apart_first
                    .devices
                    .get(&dev(1))
                    .unwrap()
                    .permit
                    .is_none()
            );
            assert!(
                granted
                    .apart_first
                    .devices
                    .get(&dev(2))
                    .unwrap()
                    .permit
                    .is_none()
            );
            assert!(
                granted
                    .apart_last
                    .devices
                    .get(&dev(2))
                    .unwrap()
                    .permit
                    .is_some()
            );
            Ok::<_, Denied>(())
        })??;
    assert_eq!(old.read(1, 1, 1).err(), Some(Denied::NotSignedIn));
    Ok(())
}

#[test]
fn startup_plans_apply_before_serving_and_adoption_is_explicit() {
    for (comparison, action) in [
        (StoreComparison::Matches, Startup::Ready),
        (StoreComparison::EndingRecorded, Startup::FinishEnding),
        (StoreComparison::GrantOnlyInData, Startup::DiscardGrant),
        (StoreComparison::OlderCopy, Startup::RestoreData),
        (StoreComparison::StoreLost, Startup::Stop),
    ] {
        assert_eq!(at_start(comparison, false), action);
    }
    assert_eq!(
        at_start(StoreComparison::StoreLost, true),
        Startup::AdoptData
    );
}

#[test]
fn counter_exhaustion_never_wraps_or_opens_an_ended_session() -> Result<(), Denied> {
    let mut user = Stored::new(state("alice@example.test"));
    user.sign_in(8, 1, 1, u64::MAX - 2)?;
    user.apart.devices.get_mut(&dev(1)).unwrap().number = u64::MAX;
    user.data.devices.get_mut(&dev(1)).unwrap().number = u64::MAX;
    user.reload();
    let receipt = user.bind(8, 1, u64::MAX - 1)?;
    assert_eq!(
        user.step(u64::MAX, |turn| turn.sign_in(receipt, fresh(2), None)),
        Err(Denied::Exhausted)
    );
    assert_eq!(user.mark, u64::MAX);
    assert_eq!(user.read(1, 2, 1).err(), Some(Denied::NotSignedIn));
    user.read(1, 1, 1)?;
    Ok(())
}

#[test]
fn admin_payload_is_authorized_in_the_effects_only_when_the_change_is_allowed() -> Result<(), Denied>
{
    let mut admin = Stored::admin()?;
    let mut wrong = Stored::new(state("other@example.test"));
    wrong.account = {
        wrong.apart.account.restore = 2;
        wrong.data.account.restore = 2;
        Account::load(&wrong.apart, Some(&wrong.data))
    };
    for wrong_target in [true, false] {
        admin.leave(
            1,
            7,
            LeaveFor::Admin {
                action: AdminAction::SetLidarr,
                target: AdminTarget::Backend,
            },
            2,
        )?;
        let target = wrong_target.then_some(&mut wrong.account);
        let result = admin
            .account
            .request(1, clock(2, admin.mark), |turn| {
                turn.administer(
                    bearer(1, &hash(7)),
                    AdminAction::SetLidarr,
                    AdminTarget::Backend,
                    target,
                )
            })
            .apply(|effects| {
                let update = effects.updates.first().unwrap();
                assert_eq!(
                    update.change,
                    if wrong_target {
                        DataChange::Keep
                    } else {
                        DataChange::ApplyAdmin {
                            action: AdminAction::SetLidarr,
                            target: AdminTarget::Backend,
                        }
                    }
                );
                assert!(
                    update
                        .apart_first
                        .devices
                        .get(&dev(1))
                        .unwrap()
                        .permit
                        .as_ref()
                        .unwrap()
                        .leave
                        .is_none()
                );
                Ok::<_, Denied>(())
            })?;
        assert_eq!(
            result,
            if wrong_target {
                Err(Denied::WrongPurpose)
            } else {
                Ok(())
            }
        );
    }
    Ok(())
}

#[test]
fn restore_before_or_after_sign_in_retires_old_authorization_and_cursor() -> Result<(), Denied> {
    for sign_in_first in [false, true] {
        let mut user = Stored::new(state("alice@example.test"));
        user.sign_in(8, 1, 1, 0)?;
        let copy = user.data.clone();
        let receipt = user.bind(8, 1, 1)?;
        if sign_in_first {
            user.step(1, |turn| turn.sign_in(receipt, fresh(2), None))?;
            user.restore_copy(&copy, RestoreMode::DataOnly, 2)?;
        } else {
            user.restore_copy(&copy, RestoreMode::DataOnly, 2)?;
            assert_eq!(
                user.step(2, |turn| turn.sign_in(receipt, fresh(2), None)),
                Err(Denied::NotSignedIn)
            );
        }
        for n in [1, 2] {
            assert_eq!(user.read(1, n, 2).err(), Some(Denied::NotSignedIn));
        }
        user.sign_in(8, 1, 3, 2)?;
        let result = user
            .account
            .request(1, clock(3, user.mark), |turn| {
                turn.arrive(bearer(1, &hash(3)), Access::ReadHistory)
            })
            .apply(|_| Ok::<_, Denied>(()))?;
        assert_eq!(result.err(), Some(Denied::StartAgain));
        user.step(3, |turn| {
            turn.arrive(bearer(1, &hash(3)), Access::ReadHistory)
        })?;
    }
    Ok(())
}

#[test]
fn adoption_requires_reset_and_a_newer_stamp_cannot_change_the_account_life() -> Result<(), Denied>
{
    for managed in [false, true] {
        let mut state = state("alice@example.test");
        state.music_managed = managed;
        let mut user = Stored::new(state);
        user.sign_in(8, 1, 1, 0)?;
        let copy = user.data.clone();
        user.restore_copy(&copy, RestoreMode::Adopt, 1)?;
        assert_eq!(
            user.apart.account.music_recovery,
            Some(if managed {
                MusicRecovery::RotateManaged
            } else {
                MusicRecovery::EnterAgain
            })
        );
        assert_eq!(user.read(1, 1, 1).err(), Some(Denied::NotSignedIn));
        assert!(user.apart.account.password_reset_required);
        assert!(!user.apart.account.history_held);
        let key = StampKey::new([4; SECRET_BYTES]);
        let current = user.apart.account.position(10);
        let newer = Position {
            life: current.life + 1,
            change: 0,
            ..current.clone()
        };
        assert_eq!(
            key.presented(&current, &newer, &key.stamp(&newer)),
            Some(Restore::EndSessions)
        );
        let copy = user.data.clone();
        user.restore_copy(&copy, RestoreMode::WholeMachine, 2)?;
        assert!(!user.apart.account.history_held);
        assert_eq!(user.apart.account.life, current.life);
    }
    Ok(())
}

#[test]
fn provisioning_requires_the_grant_both_exact_logins_and_current_authority() -> Result<(), Denied> {
    use crate::nexus::{
        account::{MusicAddress, MusicSalt, Name, SALT_BYTES},
        music::{
            Checked, CurrentEndpoints, Outcome, Ping, Provisioning, SealingKey, VerifiedLogin,
        },
    };
    const PRIVATE: &str = "http://navidrome.example.test:4533";
    let public = MusicAddress::new("https://music.example.test").unwrap();
    let key = SealingKey::new([8; 32]);
    let mut admin = Stored::admin()?;
    let mut alice = Stored::new(state("alice@example.test"));
    let mut bob = Stored::new(state("bob@example.test"));
    let actor = hash(7);
    let target = AdminTarget::Account(alice.apart.account.id);
    let action = AdminAction::MusicLoginChanged;
    assert_eq!(
        admin
            .step(1, |t| t
                .authorize_music(bearer(1, &actor), Some(&alice.account)))
            .err(),
        Some(Denied::LeaveRequired)
    );
    admin.leave(1, 7, LeaveFor::Admin { action, target }, 2)?;
    assert_eq!(
        admin
            .step(2, |t| t
                .authorize_music(bearer(1, &actor), Some(&bob.account)))
            .err(),
        Some(Denied::LeaveRequired)
    );

    let checked = |admin: &mut Stored,
                   alice: &Stored,
                   n: u8,
                   public_name: &[u8]|
     -> Result<VerifiedLogin, Outcome> {
        admin
            .leave(1, 7, LeaveFor::Admin { action, target }, u64::from(n))
            .unwrap();
        let auth = admin
            .step(u64::from(n), |t| {
                t.authorize_music(bearer(1, &actor), Some(&alice.account))
            })
            .unwrap();
        let input = Provisioning {
            internal: PRIVATE,
            public: public.clone(),
            name: Name::new("Alice").unwrap(),
            password: b"Example password",
            salt: MusicSalt::from_random([n; SALT_BYTES]),
            nonce: [n; 24],
        };
        let ping = Ping::provision(auth, input, &key)?;
        assert_eq!(ping.post().address(), PRIVATE);
        let first_form = ping.post().form().to_owned();
        let ok = br#"{"subsonic-response":{"status":"ok"}}"#;
        let person = br#"{"subsonic-response":{"status":"ok","user":{"username":"Alice"}}}"#;
        let user = ping.answer(Some((200, ok)))?;
        assert_eq!(user.post().path(), "/rest/getUser");
        let Checked::Public(ping) = user.answer(Some((200, person)))? else {
            panic!("public login wasn't checked")
        };
        assert_eq!(ping.post().address(), public.as_str());
        assert_eq!(ping.post().form(), first_form);
        let Checked::Verified(result) = ping
            .answer(Some((200, ok)))?
            .answer(Some((200, public_name)))?
        else {
            panic!("public login wasn't verified")
        };
        Ok(result)
    };

    let other = br#"{"subsonic-response":{"status":"ok","user":{"username":"Bob"}}}"#;
    let exact = br#"{"subsonic-response":{"status":"ok","user":{"username":"Alice"}}}"#;
    assert_eq!(
        checked(&mut admin, &alice, 3, other).err(),
        Some(Outcome::CouldNotAsk)
    );
    assert_eq!(alice.apart.account.music_login, 1);

    let replaced = checked(&mut admin, &alice, 4, exact).ok().unwrap();
    assert_eq!(
        admin.step(4, |t| t.install_music(
            bearer(1, &actor),
            replaced,
            CurrentEndpoints {
                internal: PRIVATE,
                public: &public
            },
            Some(&mut bob.account)
        )),
        Err(Denied::WrongPurpose)
    );

    let wrong_address = MusicAddress::new("https://changed.example.test").unwrap();
    let replaced = checked(&mut admin, &alice, 5, exact).ok().unwrap();
    assert_eq!(
        admin.step(5, |t| t.install_music(
            bearer(1, &actor),
            replaced,
            CurrentEndpoints {
                internal: PRIVATE,
                public: &wrong_address
            },
            Some(&mut alice.account)
        )),
        Err(Denied::WrongPurpose)
    );

    let replaced = checked(&mut admin, &alice, 6, exact).ok().unwrap();
    let stale_count = checked(&mut admin, &alice, 7, exact).ok().unwrap();
    admin
        .account
        .request(1, clock(7, 7), |t| {
            t.install_music(
                bearer(1, &actor),
                replaced,
                CurrentEndpoints {
                    internal: PRIVATE,
                    public: &public,
                },
                Some(&mut alice.account),
            )
        })
        .apply(|effects| {
            let update = effects
                .updates
                .iter()
                .find(|u| u.data.account.id == alice.apart.account.id)
                .unwrap();
            let DataChange::InstallMusicLogin(login) = &update.change else {
                panic!("no sealed login effect")
            };
            assert_eq!(login.binding.count, update.data.account.music_login);
            assert_eq!(
                &*key.open(&login.binding, &login.sealed).unwrap(),
                b"Example password"
            );
            assert_eq!(update.data.account.music_login, 2);
            Ok::<_, Denied>(())
        })??;
    assert_eq!(
        admin.step(7, |t| t.install_music(
            bearer(1, &actor),
            stale_count,
            CurrentEndpoints {
                internal: PRIVATE,
                public: &public
            },
            Some(&mut alice.account)
        )),
        Err(Denied::WrongPurpose)
    );

    let replaced = checked(&mut admin, &alice, 8, exact).ok().unwrap();
    admin.step(8, |t| t.sign_out(bearer(1, &actor)))?;
    assert_eq!(
        admin.step(8, |t| t.install_music(
            bearer(1, &actor),
            replaced,
            CurrentEndpoints {
                internal: PRIVATE,
                public: &public
            },
            Some(&mut alice.account)
        )),
        Err(Denied::NotSignedIn)
    );
    Ok(())
}
