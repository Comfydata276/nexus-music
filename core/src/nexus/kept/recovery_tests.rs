use super::tests::{Stored, bearer, clock, hash, state};
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
        kind: AdminKind,
        change: AdminAction,
        at: u64,
    ) -> Result<(), Denied> {
        self.mark = self.mark.max(target.mark);
        self.leave(1, 7, LeaveFor::Admin(kind), at)?;
        self.account
            .request(self.apart.account.restore, clock(at, self.mark), |turn| {
                turn.administer(bearer(1, &hash(7)), change, Some(&mut target.account))
            })
            .apply(|effects| {
                self.mark = effects.high_water;
                target.mark = effects.high_water;
                for update in &effects.updates {
                    let store = if update.data.account.name == target.data.account.name {
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
    for (kind, action, denial) in [
        (
            AdminKind::ChangeAccountStatus,
            AdminAction::SetOn(false),
            Denied::AccountOff,
        ),
        (
            AdminKind::DeleteAccount,
            AdminAction::DeleteAccount,
            Denied::NotSignedIn,
        ),
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
                person.step(2, |turn| turn.sign_in(receipt, hash(3), None))?;
                admin.admin_change(&mut person, kind, action, 2)?;
            } else {
                admin.admin_change(&mut person, kind, action, 2)?;
                assert_eq!(
                    person.step(2, |turn| turn.sign_in(receipt, hash(3), None)),
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
                admin.admin_change(&mut person, kind, AdminAction::SetOn(true), 4)?;
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
        Err(Denied::PasswordResetRequired)
    );
    assert_eq!(
        person.step(2, |turn| turn.sign_in(stale, hash(2), None)),
        Err(Denied::NotSignedIn)
    );
    assert_eq!(
        person.sign_in(8, 1, 2, 2),
        Err(Denied::PasswordResetRequired)
    );
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
fn admin_read_needs_role_and_every_admin_change_spends_its_matching_leave() -> Result<(), Denied> {
    let mut admin = Stored::admin()?;
    let mut user = Stored::new(state("alice@example.test"));
    user.sign_in(8, 1, 1, 0)?;
    assert_eq!(
        user.step(1, |turn| turn.admin_read(bearer(1, &hash(1)))),
        Err(Denied::AdminRequired)
    );
    assert_eq!(
        user.leave(1, 1, LeaveFor::Admin(AdminKind::MakeAdmin), 1),
        Err(Denied::AdminRequired)
    );
    admin.step(1, |turn| turn.admin_read(bearer(1, &hash(7))))?;
    for action in [
        AdminAction::SetOn(false),
        AdminAction::DeleteAccount,
        AdminAction::MakeAdmin,
        AdminAction::ResetPassword,
        AdminAction::ForceOut(1),
        AdminAction::MusicLoginChanged,
        AdminAction::CreateAccount,
        AdminAction::SetMusicAddress,
        AdminAction::SetDiagnosticsDefault,
        AdminAction::SetRequests,
        AdminAction::SetLidarr,
    ] {
        assert_eq!(
            admin.step(1, |turn| turn.administer(bearer(1, &hash(7)), action, None)),
            Err(Denied::LeaveRequired)
        );
    }
    admin.leave(1, 7, LeaveFor::Admin(AdminKind::SetRequests), 2)?;
    assert_eq!(
        admin.step(2, |turn| turn.administer(
            bearer(1, &hash(7)),
            AdminAction::SetLidarr,
            None
        )),
        Err(Denied::LeaveRequired)
    );
    admin.step(2, |turn| {
        turn.administer(bearer(1, &hash(7)), AdminAction::SetRequests, None)
    })?;
    assert_eq!(
        admin.step(2, |turn| turn.administer(
            bearer(1, &hash(7)),
            AdminAction::SetRequests,
            None
        )),
        Err(Denied::LeaveRequired)
    );
    admin.admin_change(&mut user, AdminKind::MakeAdmin, AdminAction::MakeAdmin, 3)?;
    user.step(3, |turn| turn.admin_read(bearer(1, &hash(1))))?;
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
    admin.admin_change(
        &mut user,
        AdminKind::ResetPassword,
        AdminAction::ResetPassword,
        2,
    )?;
    assert_eq!(
        user.step(2, |turn| turn.sign_in(receipt, hash(3), None)),
        Err(Denied::NotSignedIn)
    );
    assert_eq!(user.sign_in(8, 1, 3, 2), Err(Denied::PasswordResetRequired));
    assert!(user.data.devices.values().all(|row| row.session.is_none()));
    assert!(user.apart.account.code_available);
    user.restore_copy(&copy, RestoreMode::DataOnly, 3)?;
    let secret = DeviceSecret::new([8; 32]);
    assert_eq!(
        user.step(3, |turn| turn.check_code(&secret, 1, hash(43)))
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
            turn.sign_in(receipt, hash(2), None)
        })
        .apply(|e| {
            let update = e.updates.first().unwrap();
            assert_eq!(update.apart_first.devices.get(&1).unwrap().last_session, 2);
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
        .request(1, clock(1, user.mark), |turn| turn.grant_leave(receipt))
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
            assert!(update.apart_first.devices.get(&2).unwrap().permit.is_none());
            let mut recovered = Account::load(&update.apart_first, Some(&before));
            let check = recovered
                .request(1, clock(2, effects.high_water), |turn| {
                    turn.check_sign_in(&DeviceSecret::new([8; 32]), 1, hash(42))
                })
                .apply(|_| Ok::<_, Denied>(()))??;
            let receipt = check.check(hash(42), clock(2, 2))?;
            assert_eq!(
                recovered
                    .request(1, clock(2, effects.high_water), |turn| {
                        turn.sign_in(receipt, hash(3), None)
                    })
                    .apply(|_| Ok::<_, Denied>(()))?,
                Err(Denied::PasswordResetRequired)
            );
            Ok(())
        })??;
    Ok(())
}

#[test]
fn managed_rotation_is_persisted_with_force_out_and_advances_the_count_on_replacement()
-> Result<(), Denied> {
    let mut person = state("alice@example.test");
    person.music_managed = true;
    let mut s = Stored::new(person);
    assert_eq!(s.sign_in(8, 1, 1, 0)?.music_login, Some(1));
    s.sign_in(9, 2, 2, 0)?;
    s.step(1, |turn| turn.force_out(bearer(1, &hash(1)), 2))?;
    s.reload();
    assert_eq!(
        s.apart.account.music_recovery,
        Some(MusicRecovery::RotateManaged)
    );
    assert_eq!(s.step(2, |turn| turn.music_replaced(1)), Ok(2));
    assert_eq!(
        s.step(2, |turn| turn.music_replaced(1)),
        Err(Denied::WrongPurpose)
    );
    s.reload();
    assert_eq!(s.sign_in(9, 2, 3, 2)?.music_login, Some(2));
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
        new.step(1, |turn| turn.sign_in(bad, hash(2), Some(&mut old.account))),
        Err(Denied::NotSignedIn)
    );
    old.read(1, 1, 1)?;
    let checked = new.bind(8, 2, 1)?;
    new.account
        .request(1, clock(1, 1), |turn| {
            turn.sign_in(checked, hash(2), Some(&mut old.account))
        })
        .apply(|effects| {
            let ended = effects
                .updates
                .iter()
                .find(|u| u.data.account.name == old.data.account.name)
                .unwrap();
            let granted = effects
                .updates
                .iter()
                .find(|u| u.data.account.name == new.data.account.name)
                .unwrap();
            assert!(ended.apart_first.devices.get(&1).unwrap().permit.is_none());
            assert!(
                granted
                    .apart_first
                    .devices
                    .get(&2)
                    .unwrap()
                    .permit
                    .is_none()
            );
            assert!(granted.apart_last.devices.get(&2).unwrap().permit.is_some());
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
    user.apart.devices.get_mut(&1).unwrap().last_session = u64::MAX;
    user.reload();
    let receipt = user.bind(8, 1, u64::MAX - 1)?;
    assert_eq!(
        user.step(u64::MAX, |turn| turn.sign_in(receipt, hash(2), None)),
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
        admin.leave(1, 7, LeaveFor::Admin(AdminKind::SetLidarr), 2)?;
        let target = wrong_target.then_some(&mut wrong.account);
        let result = admin
            .account
            .request(1, clock(2, admin.mark), |turn| {
                turn.administer(bearer(1, &hash(7)), AdminAction::SetLidarr, target)
            })
            .apply(|effects| {
                let update = effects.updates.first().unwrap();
                assert_eq!(
                    update.change,
                    if wrong_target {
                        DataChange::Keep
                    } else {
                        DataChange::ApplyAdmin(AdminKind::SetLidarr)
                    }
                );
                assert!(
                    update
                        .apart_first
                        .devices
                        .get(&1)
                        .unwrap()
                        .permit
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
            user.step(1, |turn| turn.sign_in(receipt, hash(2), None))?;
            user.restore_copy(&copy, RestoreMode::DataOnly, 2)?;
        } else {
            user.restore_copy(&copy, RestoreMode::DataOnly, 2)?;
            assert_eq!(
                user.step(2, |turn| turn.sign_in(receipt, hash(2), None)),
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
fn adoption_renews_music_access_and_a_genuine_newer_life_holds_restored_history()
-> Result<(), Denied> {
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
        assert!(!user.apart.account.history_held);
        let key = StampKey::new([4; SECRET_BYTES]);
        let current = user.apart.account.position(10);
        let newer = Position {
            life: current.life + 1,
            change: 0,
            ..current.clone()
        };
        let evidence = key.presented(&current, &newer, &key.stamp(&newer));
        let copy = user.data.clone();
        user.restore_copy(&copy, RestoreMode::WholeMachine(evidence), 2)?;
        assert!(user.apart.account.history_held);
        assert_eq!(user.apart.account.life, newer.life);
    }
    Ok(())
}
