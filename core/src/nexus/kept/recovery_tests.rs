use super::*;

fn hash(n: u8) -> Fingerprint {
    Fingerprint::of(&[n; SECRET_BYTES])
}

/// The two persisted records, the backend-wide high-water mark, and one in-memory row.
/// decide/apply is also how S2 obtains a denial: effects are applied before it gets one.
struct Stored {
    mark: u64,
    apart: Apart,
    data: Rows,
    device: Device,
    last_family: Option<Family>,
}

impl Stored {
    fn new() -> Self {
        let scope = Scope {
            backend: BackendId::from_bytes([4; 16]),
            account: "Alice".into(),
            restore: 1,
            device: 3,
            number: 1,
            life: 1,
        };
        let device = Device::new(&scope);
        let data = device.rows();
        let apart = device.apart();
        Self {
            mark: 0,
            apart,
            data,
            device,
            last_family: None,
        }
    }

    fn step<T>(&mut self, now: u64, f: impl FnOnce(Turn<'_>) -> Result<T, Denied>) -> Result<T, Denied> {
        self.device
            .decide(
                &self.apart.scope,
                Clock {
                    now,
                    high_water: self.mark,
                },
                f,
            )
            .apply(|effects| {
                self.mark = effects.high_water;
                self.last_family = None;
                if let Some(update) = &effects.update {
                    self.last_family = update.family;
                    self.apart = update.apart_first.clone();
                    self.data = update.data.clone();
                    self.apart = update.apart_last.clone();
                }
                Ok(())
            })?
    }

    fn reload(&mut self) {
        self.device = Device::load(&self.apart, &self.data);
    }

    fn proof(&mut self, purpose: Purpose, fresh: Option<u8>, t: u64) -> Result<(), Denied> {
        let ticket = self.step(t, |turn| turn.begin(purpose))?;
        self.step(t, |turn| turn.finish(ticket, "Alice", fresh.map(hash)))
    }

    fn send(&mut self, bearer: u8, t: u64) -> Result<Request, Denied> {
        self.step(t, |turn| turn.arrive(&hash(bearer), Access::Send, false))
    }
}

#[test]
fn an_ending_is_persisted_even_when_the_call_that_discovered_it_is_refused() -> Result<(), Denied> {
    for wrong_account in [false, true] {
        let mut s = Stored::new();
        s.proof(Purpose::Connect, Some(1), 0)?;
        s.proof(Purpose::Again(hash(1)), Some(2), 1)?;
        let old_data = s.data.clone();
        let number = s.apart.scope.number;
        let refusal = if wrong_account {
            let t = s.step(2, |turn| turn.begin(Purpose::Again(hash(1))))?;
            s.step(3, |turn| turn.finish(t, "Bob", Some(hash(3))))
        } else {
            s.send(1, SESSION_MS).map(|_| ())
        };
        assert_eq!(refusal, Err(Denied::NotSignedIn));
        assert_eq!(s.last_family, Some(Family::Device));
        assert_eq!(s.apart.scope.number, number + 1);
        s.data = old_data;
        s.reload();
        for n in [1, 2] {
            assert_eq!(s.send(n, 3).err(), Some(Denied::NotSignedIn));
        }
    }
    Ok(())
}

#[test]
fn a_copy_from_before_promotion_or_replacement_cannot_revive_the_revoked_bearer() -> Result<(), Denied> {
    for promote in [true, false] {
        let mut s = Stored::new();
        s.proof(Purpose::Connect, Some(1), 0)?;
        s.proof(Purpose::Again(hash(1)), Some(2), 1)?;
        let copy = s.data.clone();
        if promote {
            s.send(2, 2)?;
        } else {
            s.proof(Purpose::Again(hash(1)), Some(3), 2)?;
        }
        s.data = copy;
        s.reload();
        let dead = if promote { 1 } else { 2 };
        assert_eq!(s.send(dead, 3).err(), Some(Denied::NotSignedIn));
        s.send(if promote { 2 } else { 1 }, 3)?;
    }
    Ok(())
}

#[test]
fn a_crash_at_either_cut_of_a_mixed_replacement_keeps_neither_the_revoked_nor_unrecognized_grant()
-> Result<(), Denied> {
    let mut s = Stored::new();
    s.proof(Purpose::Connect, Some(1), 0)?;
    s.proof(Purpose::Again(hash(1)), Some(2), 1)?;
    let old_data = s.data.clone();
    let t = s.step(2, |turn| turn.begin(Purpose::Again(hash(1))))?;
    let decision = s.device.decide(
        &s.apart.scope,
        Clock {
            now: 3,
            high_water: s.mark,
        },
        |turn| turn.finish(t, "Alice", Some(hash(3))),
    );
    decision.apply(|effects| {
        let u = effects.update.as_ref().ok_or(Denied::NotSignedIn)?;
        for rows in [&old_data, &u.data] {
            let mut recovered = Device::load(&u.apart_first, rows);
            let mut scope = u.apart_first.scope.clone();
            for n in [2, 3] {
                assert_eq!(
                    recovered
                        .arrive(&mut scope, &hash(n), Access::Send, false, 3)
                        .err(),
                    Some(Denied::NotSignedIn)
                );
            }
            recovered.arrive(&mut scope, &hash(1), Access::Send, false, 3)?;
        }
        Ok(())
    })?
}

#[test]
fn deletion_leave_runs_its_full_ten_minutes_across_the_send_deadline_but_never_the_hard_end()
-> Result<(), Denied> {
    for (offset, result) in [
        (0, Ok(())),
        (DELETE_MS - 2, Ok(())),
        (DELETE_MS - 1, Err(Denied::ProveAgain)),
    ] {
        let mut s = Stored::new();
        s.proof(Purpose::Connect, Some(1), 0)?;
        s.proof(Purpose::Delete(hash(1)), None, SEND_MS - 1)?;
        s.reload();
        assert_eq!(s.step(SEND_MS + offset, |turn| turn.delete(&hash(1))), result);
    }
    let mut s = Stored::new();
    s.proof(Purpose::Connect, Some(1), 0)?;
    s.proof(Purpose::Delete(hash(1)), None, SEND_MS - 1)?;
    assert_eq!(
        s.step(SESSION_MS, |turn| turn.delete(&hash(1))),
        Err(Denied::NotSignedIn)
    );
    assert_eq!(s.last_family, Some(Family::Device));
    Ok(())
}

#[test]
fn a_refused_send_and_expired_delete_leave_stay_refused_after_clock_rollback_and_restart()
-> Result<(), Denied> {
    for restart in [false, true] {
        let mut s = Stored::new();
        s.proof(Purpose::Connect, Some(1), 0)?;
        s.proof(Purpose::Delete(hash(1)), None, 1)?;
        assert_eq!(
            s.step(1 + DELETE_MS, |turn| turn.delete(&hash(1))),
            Err(Denied::ProveAgain)
        );
        if restart {
            s.reload();
        }
        assert_eq!(
            s.step(DELETE_MS, |turn| turn.delete(&hash(1))),
            Err(Denied::ProveAgain)
        );
        assert_eq!(s.send(1, SEND_MS).err(), Some(Denied::ProveAgain));
        if restart {
            s.reload();
        }
        assert_eq!(s.send(1, SEND_MS - 1).err(), Some(Denied::ProveAgain));
        assert_eq!(s.mark, SEND_MS);
    }
    Ok(())
}

#[test]
fn reusing_retired_bearer_bytes_cannot_complete_a_request_from_the_first_session() -> Result<(), Denied> {
    let mut s = Stored::new();
    s.proof(Purpose::Connect, Some(1), 0)?;
    let request = s.send(1, 1)?;
    s.proof(Purpose::Again(hash(1)), Some(2), 2)?;
    s.send(2, 3)?;
    s.proof(Purpose::Again(hash(2)), Some(1), 4)?;
    s.send(1, 5)?;
    assert_eq!(
        s.step(6, |turn| turn.complete(request, false)),
        Err(Denied::NotSignedIn)
    );
    Ok(())
}

#[test]
fn delete_proof_and_ending_in_either_order_cannot_leave_power_to_delete() -> Result<(), Denied> {
    for finish_first in [true, false] {
        let mut s = Stored::new();
        s.proof(Purpose::Connect, Some(1), 0)?;
        let t = s.step(1, |turn| turn.begin(Purpose::Delete(hash(1))))?;
        if finish_first {
            s.step(2, |turn| turn.finish(t, "Alice", None))?;
            s.step(2, |turn| turn.end())?;
        } else {
            s.step(2, |turn| turn.end())?;
            assert_eq!(
                s.step(2, |turn| turn.finish(t, "Alice", None)),
                Err(Denied::NotSignedIn)
            );
        }
        s.reload();
        assert_eq!(s.step(3, |turn| turn.delete(&hash(1))), Err(Denied::NotSignedIn));
    }
    Ok(())
}

#[test]
fn deleting_on_one_device_also_advances_the_sleeping_devices_number() -> Result<(), Denied> {
    let mut s = Stored::new();
    s.proof(Purpose::Connect, Some(1), 0)?;
    s.proof(Purpose::Delete(hash(1)), None, 1)?;
    let other = Scope {
        device: 9,
        ..s.apart.scope.clone()
    };
    let decision = s.device.decide(
        &s.apart.scope,
        Clock {
            now: 2,
            high_water: s.mark,
        },
        |t| t.delete(&hash(1)),
    );
    decision.apply(|effects| {
        let update = effects.update.as_ref().ok_or(Denied::NotSignedIn)?;
        assert_eq!(update.family, Some(Family::Account));
        let next = Family::Account
            .end_other(update, &other)?
            .ok_or(Denied::NotSignedIn)?;
        assert_eq!((next.life, next.number), (other.life + 1, other.number + 1));
        assert!(!may_refill(Some(other.life), next.life));
        Ok(())
    })?
}
