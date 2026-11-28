// The verifier's sole production construction site is exercised by the exchange tests;
// these fixtures isolate what the durable decision accepts from it.
use super::*;
use crate::nexus::kept::{Clock, Denied, Device, Family, SESSION_MS, Scope};
use crate::nexus::proto::BackendId;

fn scope() -> Scope {
    Scope {
        backend: BackendId::from_bytes([1; 16]),
        device: 1,
        number: 1,
        account: "Alice".into(),
        life: 1,
        restore: 1,
    }
}
fn evidence(d: &mut Device, s: &Scope, target: Scope, made: u64) -> Result<Receipt, Denied> {
    let t = d
        .decide(
            s,
            Clock {
                now: 0,
                high_water: 0,
            },
            |t| t.begin(Purpose::Connect),
        )
        .apply(|_| Ok(()))??;
    Ok(Receipt {
        ticket: t,
        target,
        spent: 1,
        made,
        device_hash: Fingerprint::of(&[2; 32]),
        description: Description {
            name: "Kitchen".into(),
            kind: Kind::Pc,
            version: proto::Version {
                major: 1,
                minor: 0,
                patch: 0,
            },
        },
    })
}

#[test]
fn a_receipt_gets_five_seconds_of_its_own_and_the_session_dates_from_the_proof() -> Result<(), Denied> {
    for (at, expected) in [
        (64_998, Ok(())),
        (64_999, Err(Denied::NotSignedIn)),
        (SESSION_MS, Err(Denied::NotSignedIn)),
    ] {
        let s = scope();
        let mut d = Device::new(&s);
        let receipt = evidence(&mut d, &s, s.clone(), 59_999)?;
        let decision = d.decide(
            &s,
            Clock {
                now: at,
                high_water: 59_999,
            },
            |t| t.finish(receipt, &s, Some(Fingerprint::of(&[3; 32]))),
        );
        assert_eq!(
            decision.apply(|effects| {
                if expected.is_ok() {
                    let row = effects
                        .update
                        .as_ref()
                        .and_then(|u| u.data.working.as_ref())
                        .ok_or(Denied::NotSignedIn)?;
                    assert_eq!(row.proven, 59_999);
                } else {
                    assert!(effects.update.is_none());
                }
                Ok(())
            })?,
            expected
        );
    }
    Ok(())
}

#[test]
fn a_switch_receipt_starts_a_separate_empty_row_only_under_the_same_destination_life() -> Result<(), Denied>
{
    for deleted in [false, true] {
        let s = scope();
        let mut d = Device::new(&s);
        let target = Scope {
            account: "Bob".into(),
            device: 2,
            ..s.clone()
        };
        let receipt = evidence(&mut d, &s, target.clone(), 2)?;
        let current = Scope {
            life: if deleted { 2 } else { 1 },
            ..target.clone()
        };
        let decision = d.decide(
            &s,
            Clock {
                now: 3,
                high_water: 2,
            },
            |t| t.finish(receipt, &current, None),
        );
        assert_eq!(
            decision.apply(|effects| {
                if deleted {
                    assert!(effects.new_device.is_none() && effects.update.is_none());
                } else {
                    let end = effects.update.as_ref().ok_or(Denied::NotSignedIn)?;
                    assert_eq!(end.family, Some(Family::Device));
                    assert!(end.apart_first.working.is_none() && end.apart_first.waiting.is_none());
                    let new = effects.new_device.as_ref().ok_or(Denied::NotSignedIn)?;
                    assert_eq!(new.scope, target);
                    assert_eq!(new.device_hash, Fingerprint::of(&[2; 32]));
                }
                Ok(())
            })?,
            Err(Denied::NotSignedIn)
        );
    }
    Ok(())
}
