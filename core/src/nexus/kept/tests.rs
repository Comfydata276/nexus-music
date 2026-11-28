use super::*;

fn hash(n: u8) -> Fingerprint {
    Fingerprint::of(&[n; SECRET_BYTES])
}
fn scope() -> Scope {
    Scope {
        backend: BackendId::from_bytes([1; 16]),
        restore: 1,
        account: "Alice".into(),
        life: 1,
        device: 7,
        number: 1,
    }
}
fn connect(d: &mut Device, s: &mut Scope, n: u8, at: u64) -> Result<(), Denied> {
    let t = d.begin(s, Purpose::Connect, at)?;
    d.finish(s, t, "Alice", Some(hash(n)), at)
}
fn connected() -> Result<(Device, Scope), Denied> {
    let mut s = scope();
    let mut d = Device::new(&s);
    connect(&mut d, &mut s, 1, 0)?;
    Ok((d, s))
}
fn again(d: &mut Device, s: &mut Scope, old: u8, new: u8, at: u64) -> Result<(), Denied> {
    let t = d.begin(s, Purpose::Again(hash(old)), at)?;
    d.finish(s, t, "Alice", Some(hash(new)), at)
}
fn leave(d: &mut Device, s: &mut Scope, n: u8, at: u64) -> Result<(), Denied> {
    let t = d.begin(s, Purpose::Delete(hash(n)), at)?;
    d.finish(s, t, "Alice", None, at)
}

#[test]
fn a_lost_connect_reply_finds_the_same_device_and_ends_its_previous_sessions() -> Result<(), Denied> {
    let (mut d, mut s) = connected()?;
    again(&mut d, &mut s, 1, 2, 1)?;
    let secret = hash(8);
    assert!(same_device("Alice", &secret, "Alice", &hash(8)));
    assert!(!same_device("Alice", &secret, "Alice", &hash(9)));
    assert!(!same_device("Alice", &secret, "Bob", &hash(8)));
    let candidate = Some((&s, &secret));
    let id = crate::nexus::prove::known_device(candidate, &hash(8))
        .ok_or(Denied::NotSignedIn)?
        .device;
    assert!(crate::nexus::prove::known_device(candidate, &hash(9)).is_none());
    let number = s.number;
    connect(&mut d, &mut s, 3, 2)?;
    assert_eq!(s.device, id);
    assert!(s.number > number);
    for n in [1, 2] {
        assert_eq!(
            d.arrive(&mut s, &hash(n), Access::Read, false, 3).err(),
            Some(Denied::NotSignedIn)
        );
    }
    d.arrive(&mut s, &hash(3), Access::Read, false, 3)?;
    Ok(())
}

#[test]
fn a_lost_renewal_is_replaced_while_the_old_session_keeps_its_own_times() -> Result<(), Denied> {
    let (mut d, mut s) = connected()?;
    again(&mut d, &mut s, 1, 2, READ_MS - 1)?;
    assert_eq!(
        d.arrive(&mut s, &hash(1), Access::Read, false, READ_MS).err(),
        Some(Denied::ProveAgain)
    );
    let old_write = d.arrive(&mut s, &hash(1), Access::Send, false, READ_MS)?;
    again(&mut d, &mut s, 1, 3, READ_MS)?;
    assert_eq!(
        d.arrive(&mut s, &hash(2), Access::Send, false, READ_MS).err(),
        Some(Denied::NotSignedIn)
    );
    d.arrive(&mut s, &hash(3), Access::Read, false, READ_MS)?;
    assert_eq!(
        d.complete(&mut s, old_write, false, READ_MS),
        Err(Denied::NotSignedIn)
    );
    Ok(())
}

#[test]
fn two_proofs_finishing_together_only_grant_the_latest_begin() -> Result<(), Denied> {
    for newest_first in [false, true] {
        let (mut d, mut s) = connected()?;
        let older = d.begin(&mut s, Purpose::Again(hash(1)), 1)?;
        let latest = d.begin(&mut s, Purpose::Again(hash(1)), 2)?;
        let finish_old = |d: &mut Device, s: &mut Scope| {
            assert_eq!(
                d.finish(s, older, "Alice", Some(hash(2)), 3),
                Err(Denied::NotSignedIn)
            );
        };
        if newest_first {
            d.finish(&mut s, latest, "Alice", Some(hash(3)), 3)?;
            finish_old(&mut d, &mut s);
        } else {
            finish_old(&mut d, &mut s);
            d.finish(&mut s, latest, "Alice", Some(hash(3)), 3)?;
        }
        d.arrive(&mut s, &hash(3), Access::Read, false, 4)?;
        assert_eq!(
            d.arrive(&mut s, &hash(2), Access::Read, false, 4).err(),
            Some(Denied::NotSignedIn)
        );
    }
    Ok(())
}

#[test]
fn after_send_and_read_expire_reproof_restores_them_and_delete_is_separate() -> Result<(), Denied> {
    let (mut d, mut s) = connected()?;
    for (access, limit) in [(Access::Read, READ_MS), (Access::Send, SEND_MS)] {
        let r = d.arrive(&mut s, &hash(1), access, false, limit - 1)?;
        assert_eq!(d.complete(&mut s, r, false, limit), Err(Denied::ProveAgain));
        assert_eq!(
            d.arrive(&mut s, &hash(1), access, false, limit).err(),
            Some(Denied::ProveAgain)
        );
    }
    leave(&mut d, &mut s, 1, SEND_MS)?;
    again(&mut d, &mut s, 1, 2, SEND_MS)?;
    assert_eq!(
        d.arrive(&mut s, &hash(1), Access::Send, false, SEND_MS).err(),
        Some(Denied::ProveAgain)
    );
    d.arrive(&mut s, &hash(2), Access::Read, false, SEND_MS)?;
    Ok(())
}

#[test]
fn a_write_has_thirty_seconds_and_a_read_rechecks_just_before_answering() -> Result<(), Denied> {
    let (mut d, mut s) = connected()?;
    for (finish, expected) in [(WRITE_MS - 1, Ok(())), (WRITE_MS, Err(Denied::Time))] {
        let r = d.arrive(&mut s, &hash(1), Access::Send, false, 0)?;
        assert_eq!(d.complete(&mut s, r, false, finish), expected);
    }
    let r = d.arrive(&mut s, &hash(1), Access::Read, false, 0)?;
    d.complete(&mut s, r, false, WRITE_MS)?;
    let r = d.arrive(&mut s, &hash(1), Access::Read, false, 1)?;
    assert_eq!(d.complete(&mut s, r, true, 1), Err(Denied::HistoryHeld));
    Ok(())
}

#[test]
fn a_delete_proof_is_once_for_ten_minutes_and_does_not_renew() -> Result<(), Denied> {
    let (mut d, mut s) = connected()?;
    leave(&mut d, &mut s, 1, READ_MS - 1)?;
    assert_eq!(
        d.arrive(&mut s, &hash(1), Access::Read, false, READ_MS).err(),
        Some(Denied::ProveAgain)
    );
    assert_eq!(
        d.delete(&mut s, &hash(1), READ_MS - 1 + DELETE_MS),
        Err(Denied::ProveAgain)
    );
    leave(&mut d, &mut s, 1, READ_MS + DELETE_MS)?;
    let old = s.life;
    d.delete(&mut s, &hash(1), READ_MS + 2 * DELETE_MS - 1)?;
    assert_eq!(s.life, old + 1);
    assert_eq!(
        d.delete(&mut s, &hash(1), READ_MS + 2 * DELETE_MS - 1),
        Err(Denied::NotSignedIn)
    );
    Ok(())
}

#[test]
fn delete_leave_does_not_follow_a_session_when_its_replacement_is_used() -> Result<(), Denied> {
    let (mut d, mut s) = connected()?;
    leave(&mut d, &mut s, 1, 1)?;
    again(&mut d, &mut s, 1, 2, 2)?;
    assert_eq!(d.delete(&mut s, &hash(2), 2), Err(Denied::ProveAgain));
    assert_eq!(d.delete(&mut s, &hash(1), 2), Err(Denied::NotSignedIn));
    Ok(())
}

#[test]
fn ending_during_any_proof_beats_the_proof_and_an_inflight_write() -> Result<(), Denied> {
    for purpose in [
        Purpose::Connect,
        Purpose::Again(hash(1)),
        Purpose::Delete(hash(1)),
    ] {
        let (mut d, mut s) = connected()?;
        again(&mut d, &mut s, 1, 2, 1)?;
        leave(&mut d, &mut s, 1, 2)?;
        let old = d.arrive(&mut s, &hash(1), Access::Send, false, 3)?;
        let t = d.begin(&mut s, purpose, 3)?;
        d.end(&mut s)?;
        assert_eq!(
            d.finish(&mut s, t, "Alice", Some(hash(3)), 4),
            Err(Denied::NotSignedIn)
        );
        assert_eq!(d.complete(&mut s, old, false, 4), Err(Denied::NotSignedIn));
        for n in [1, 2] {
            assert_eq!(d.delete(&mut s, &hash(n), 4), Err(Denied::NotSignedIn));
        }
    }
    Ok(())
}

#[test]
fn another_authenticated_account_ends_the_device_but_an_obsolete_proof_cannot() -> Result<(), Denied> {
    let (mut d, mut s) = connected()?;
    let old = d.begin(&mut s, Purpose::Again(hash(1)), 1)?;
    let t = d.begin(&mut s, Purpose::Again(hash(1)), 1)?;
    let before = s.number;
    assert_eq!(
        d.finish(&mut s, old, "Bob", Some(hash(2)), 2),
        Err(Denied::NotSignedIn)
    );
    assert_eq!(s.number, before);
    assert_eq!(
        d.finish(&mut s, t, "Bob", Some(hash(2)), 2),
        Err(Denied::NotSignedIn)
    );
    assert!(s.number > before);
    assert_eq!(
        d.arrive(&mut s, &hash(1), Access::Read, false, 3).err(),
        Some(Denied::NotSignedIn)
    );
    Ok(())
}

#[test]
fn ninety_days_ends_the_waiting_session_and_any_unfinished_proof() -> Result<(), Denied> {
    let (mut d, mut s) = connected()?;
    again(&mut d, &mut s, 1, 2, SESSION_MS - 2)?;
    let t = d.begin(&mut s, Purpose::Again(hash(1)), SESSION_MS - 1)?;
    let before = s.number;
    assert_eq!(
        d.finish(&mut s, t, "Alice", Some(hash(3)), SESSION_MS),
        Err(Denied::NotSignedIn)
    );
    assert!(s.number > before);
    assert_eq!(
        d.arrive(&mut s, &hash(2), Access::Read, false, SESSION_MS).err(),
        Some(Denied::NotSignedIn)
    );
    Ok(())
}

#[test]
fn sleeping_reconnected_new_and_halfway_through_devices_cannot_refill_a_deleted_life() -> Result<(), Denied>
{
    let (mut d, mut s) = connected()?;
    let remembered = s.life;
    let r = d.arrive(&mut s, &hash(1), Access::Send, false, 0)?;
    let mut other = s.clone();
    other.device = 44;
    let mut sleeping = Device::new(&other);
    connect(&mut sleeping, &mut other, 4, 0)?;
    let t = sleeping.begin(&mut other, Purpose::Connect, 1)?;
    leave(&mut d, &mut s, 1, 1)?;
    d.delete(&mut s, &hash(1), 2)?;
    other.life = s.life;
    assert_eq!(
        sleeping.finish(&mut other, t, "Alice", Some(hash(2)), 3),
        Err(Denied::NotSignedIn)
    );
    assert_eq!(
        sleeping
            .arrive(&mut other, &hash(4), Access::Read, false, 3)
            .err(),
        Some(Denied::NotSignedIn)
    );
    assert_eq!(d.complete(&mut s, r, false, 3), Err(Denied::NotSignedIn));
    assert!(!may_refill(Some(remembered), s.life));
    assert!(!may_refill(None, s.life));
    assert!(may_refill(Some(s.life), s.life));
    let mut reconnected = Device::new(&other);
    connect(&mut reconnected, &mut other, 7, 4)?;
    let mut send = |consent| {
        if !may_refill(consent, other.life) {
            return Err(Denied::ProveAgain);
        }
        reconnected
            .arrive(&mut other, &hash(7), Access::Send, false, 5)
            .map(|_| ())
    };
    assert_eq!(send(Some(remembered)), Err(Denied::ProveAgain));
    assert_eq!(send(None), Err(Denied::ProveAgain));
    send(Some(s.life))?;
    Ok(())
}

#[test]
fn a_restore_between_two_calls_ends_both_sessions_and_invalidates_the_first_call() -> Result<(), Denied> {
    let (mut d, mut s) = connected()?;
    again(&mut d, &mut s, 1, 2, 1)?;
    let r = d.arrive(&mut s, &hash(1), Access::Read, false, 2)?;
    let t = d.begin(&mut s, Purpose::Connect, 2)?;
    restore_device(&mut d, &mut s, 2)?;
    assert_eq!(d.complete(&mut s, r, false, 3), Err(Denied::NotSignedIn));
    assert_eq!(
        d.arrive(&mut s, &hash(1), Access::Read, false, 3).err(),
        Some(Denied::NotSignedIn)
    );
    assert_eq!(
        d.finish(&mut s, t, "Alice", Some(hash(3)), 3),
        Err(Denied::NotSignedIn)
    );
    assert_eq!(
        d.arrive(&mut s, &hash(2), Access::Read, false, 3).err(),
        Some(Denied::NotSignedIn)
    );
    Ok(())
}

#[test]
fn store_order_and_startup_plans_distinguish_an_interrupted_ending_from_an_unrecognized_grant() {
    use Where::*;
    assert_eq!(Change::EndDevice.order(), [Apart, Data]);
    assert_eq!(Change::DeleteAccount.order(), [Apart, Data]);
    assert_eq!(Change::GrantSession.order(), [Data, Apart]);
    assert_eq!(at_start(StoreComparison::Matches, false), Startup::Ready);
    assert_eq!(
        at_start(StoreComparison::EndingRecorded, false),
        Startup::FinishEnding
    );
    assert_eq!(
        at_start(StoreComparison::GrantOnlyInData, false),
        Startup::EndUnknownSession
    );
}

#[test]
fn a_restored_copy_holds_deleted_accounts_and_a_missing_store_requires_the_owner() {
    assert_eq!(at_start(StoreComparison::OlderCopy, false), Startup::RestoreData);
    assert_eq!(restored_account(2, 1, false), Restore::HoldThrough(2));
    assert_eq!(restored_account(2, 2, false), Restore::Reprove);
    assert_eq!(restored_account(2, 2, true), Restore::HoldThrough(2));
    assert_eq!(at_start(StoreComparison::StoreLost, false), Startup::Stop);
    assert_eq!(at_start(StoreComparison::StoreLost, true), Startup::HoldAll);
}

fn position() -> Position {
    Position {
        backend: scope().backend,
        account: "Alice".into(),
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
            Some(Restore::Reprove),
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
                account: "Bob".into(),
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
        Position { life: 3, ..p.clone() },
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
        Position { life: 3, ..p.clone() },
        Position {
            account: "Bob".into(),
            ..p.clone()
        },
    ] {
        assert!(!may_apply(1, 1, &p, &other));
    }
}

#[test]
fn the_app_proves_at_twelve_hours_in_use_or_on_a_request_to_prove_again() {
    assert!(!should_prove(0, RENEW_MS - 1, true, false));
    assert!(should_prove(0, RENEW_MS, true, false));
    assert!(!should_prove(0, RENEW_MS, false, false));
    assert!(should_prove(0, 1, false, true));
}

#[test]
fn clocks_and_numbers_near_their_limits_cannot_extend_leave_or_wrap() -> Result<(), Denied> {
    assert!(within(u64::MAX - 1, u64::MAX, READ_MS));
    assert!(!within(5, 4, READ_MS));
    let mut s = Scope {
        number: u64::MAX,
        ..scope()
    };
    let mut d = Device::new(&s);
    assert_eq!(d.end(&mut s), Err(Denied::Exhausted));
    let (mut d, mut s) = connected()?;
    assert_eq!(again(&mut d, &mut s, 1, 1, 1), Err(Denied::ReusedSession));
    let mut near = Scope {
        life: u64::MAX,
        number: u64::MAX - 1,
        ..scope()
    };
    let mut live = Device::new(&near);
    connect(&mut live, &mut near, 7, u64::MAX - 2)?;
    leave(&mut live, &mut near, 7, u64::MAX - 1)?;
    assert_eq!(live.delete(&mut near, &hash(7), u64::MAX), Err(Denied::Exhausted));
    assert_eq!(near.life, u64::MAX);
    assert_eq!(near.number, u64::MAX);
    Ok(())
}

#[test]
fn stored_secrets_and_the_stamp_key_print_nothing_of_their_contents() {
    assert_eq!(format!("{:?}", hash(7)), "Fingerprint(..)");
    assert_eq!(format!("{:?}", StampKey::new([9; SECRET_BYTES])), "StampKey(..)");
}

#[test]
fn the_stamps_bytes_and_session_deadlines_match_the_protocol() {
    let key = StampKey::new([3; SECRET_BYTES]);
    let expected = Fingerprint::from_bytes([
        0xcd, 0xb2, 0xed, 0xbd, 0x7d, 0x27, 0xf1, 0x94, 0xdd, 0xe9, 0xf7, 0x75, 0xd9, 0xcf, 0xd0, 0x4c,
        0xcb, 0x9c, 0x8b, 0x2d, 0x94, 0x63, 0x40, 0xf0, 0x25, 0xc2, 0xb1, 0xef, 0x24, 0x40, 0x5b, 0xd1,
    ]);
    assert_eq!(key.stamp(&position()), expected);
    assert_eq!(
        (READ_MS, SEND_MS, SESSION_MS),
        (259_200_000, 604_800_000, 7_776_000_000)
    );
    assert_eq!((WRITE_MS, DELETE_MS, RENEW_MS), (30_000, 600_000, 43_200_000));
}
