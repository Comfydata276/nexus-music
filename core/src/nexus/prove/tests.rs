use super::e3::*;
use super::*;
use crate::nexus::kept::{Access, Clock, Denied, Device, Scope};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";
const SECRET: [u8; SECRET_BYTES] = [45; SECRET_BYTES];

fn id(n: u8) -> BackendId {
    BackendId::from_bytes([n; 16])
}
fn desc() -> Description {
    Description {
        name: "Kitchen".into(),
        kind: Kind::Pc,
        version: proto::Version {
            major: 1,
            minor: 2,
            patch: 3,
        },
    }
}
fn challenge(n: u64) -> Challenge {
    Challenge {
        number: n,
        salt: Salt::from_random([0xab; SALT_BYTES]),
    }
}

struct Rig {
    scope: Scope,
    device: Device,
    slot: Slot,
    mark: u64,
    switched: Option<kept::NewDevice>,
}
impl Rig {
    fn new(name: &str) -> Self {
        let scope = Scope {
            backend: id(1),
            restore: 1,
            life: 1,
            device: 77,
            number: 1,
            account: name.into(),
        };
        Self {
            device: Device::new(&scope),
            slot: Slot::new(&scope, Fingerprint::of(&SECRET)),
            scope,
            mark: 0,
            switched: None,
        }
    }

    fn step<T>(
        &mut self,
        at: u64,
        f: impl FnOnce(&mut Slot, Turn<'_>) -> Result<T, Failure>,
    ) -> Result<T, Failure> {
        self.device
            .decide(
                &self.scope,
                Clock {
                    now: at,
                    high_water: self.mark,
                },
                |turn| f(&mut self.slot, turn),
            )
            .apply(|effects| {
                self.mark = effects.high_water;
                if let Some(new) = &effects.new_device {
                    self.switched = Some(kept::NewDevice {
                        scope: new.scope.clone(),
                        device_hash: new.device_hash.clone(),
                        description: new.description.clone(),
                    });
                }
                if let Some(update) = &effects.update {
                    self.scope = update.apart_last.scope.clone();
                }
                Ok(())
            })?
    }
    fn target(&self, name: &str) -> Scope {
        if name == self.scope.account {
            self.scope.clone()
        } else {
            Scope {
                account: name.into(),
                device: 78,
                number: 0,
                life: 3,
                ..self.scope.clone()
            }
        }
    }
    fn commit(
        &mut self,
        receipt: Receipt,
        fresh: Option<Fingerprint>,
        now: u64,
    ) -> Result<Outcome, Failure> {
        let target = self.target(receipt.account());
        self.step(now, |_, t| {
            t.finish(receipt, &target, fresh)
                .map(|_| Outcome::Proven)
                .map_err(Failure::Session)
        })
    }
    fn begin(&mut self, purpose: Purpose, name: &str, n: u64, at: u64) -> Result<(), Failure> {
        let target = self.target(name);
        self.step(at, |s, t| {
            s.begin(t, purpose, target, desc(), Fingerprint::of(&SECRET), challenge(n))
                .map(|_| ())
        })
    }
    fn spend(&mut self, n: u64, at: u64) -> Result<Check, Failure> {
        self.step(at, |s, t| s.spend(t, n, &SECRET, TOKEN))
    }
    fn call(&mut self, check: Check) -> Result<Call, Failure> {
        self.step(self.mark, |_, t| check.call(t))
    }
    fn answer(&mut self, call: Call, bytes: &[u8]) -> Result<Step, Failure> {
        self.step(self.mark, |_, t| {
            call.answer(t, Some((200, bytes)), Admission::Allowed)
        })
    }
    fn next(&mut self, call: Call, bytes: &[u8]) -> Result<Call, Failure> {
        let Step::Next(check) = self.answer(call, bytes)? else {
            return Err(Failure::Stale);
        };
        self.call(check)
    }
    fn exchange(&mut self, name: &str, purpose: Purpose, n: u64) -> Result<Call, Failure> {
        self.begin(purpose, name, n, self.mark)?;
        let check = self.spend(n, self.mark)?;
        self.call(check)
    }
    fn user(&mut self, name: &str, purpose: Purpose, n: u64) -> Result<Call, Failure> {
        let probe = self.exchange(name, purpose, n)?;
        let ping = self.next(probe, MISSING_U)?;
        self.next(ping, OK)
    }
    fn proven(&mut self, user: Call, bytes: &[u8]) -> Result<Receipt, Failure> {
        let Step::Verified(v) = self.answer(user, bytes)? else {
            return Err(Failure::Stale);
        };
        Ok(v)
    }
}

fn outcome(step: Step) -> Option<Outcome> {
    match step {
        Step::Done(o) => Some(o),
        _ => None,
    }
}

#[test]
fn only_the_consented_backend_and_exact_account_may_begin() -> Result<(), Failure> {
    let h = Hello::new(desc().version, id(1), proto::SPEAKS, &[]).ok_or(Failure::InvalidName)?;
    assert!(may_begin(Some((id(1), "Alice")), &h, "Alice"));
    assert!(!may_begin(None, &h, "Alice"));
    assert!(!may_begin(Some((id(2), "Alice")), &h, "Alice"));
    assert!(!may_begin(Some((id(1), "Alice")), &h, "alice"));
    assert!(!may_begin(Some((id(1), "")), &h, ""));
    Ok(())
}

#[test]
fn a_begin_answers_known_and_unknown_names_alike_and_binds_the_device_facts() -> Result<(), Failure> {
    for name in ["Alice", "ghostmarker"] {
        let mut rig = Rig::new(name);
        let target = rig.target(name);
        let (n, salt) = rig.step(1, |s, t| {
            s.begin(
                t,
                Purpose::Connect,
                target,
                desc(),
                Fingerprint::of(&SECRET),
                challenge(9),
            )
            .map(|c| (c.number, c.salt.as_str().to_owned()))
        })?;
        assert_eq!(n, 9);
        assert!(salt == "ab".repeat(32));
        let check = rig.spend(n, 1)?;
        assert_eq!(check.open.description, desc());
        assert_eq!(check.open.target.account, name);
        assert_eq!(check.open.ticket.scope(), &rig.scope);
        assert_eq!(check.open.ticket.purpose(), &Purpose::Connect);
    }
    Ok(())
}

#[test]
fn a_token_is_exactly_thirty_two_lower_case_hex_digits_and_salt_cannot_be_empty() {
    assert!(Token::read(TOKEN).is_some());
    for bad in [
        "",
        "0",
        &"0".repeat(31),
        &"0".repeat(33),
        &TOKEN.to_uppercase(),
        &format!("{TOKEN}\n"),
        "g123456789abcdef0123456789abcdef",
        "é123456789abcdef0123456789abcde",
    ] {
        assert!(Token::read(bad).is_none());
    }
    assert_eq!(Salt::from_random([0; SALT_BYTES]).as_str().len(), 64);
}

#[test]
fn spending_twice_with_a_wrong_secret_or_with_a_bad_token_never_reaches_navidrome() -> Result<(), Failure> {
    for (secret, token, why) in [
        ([7; SECRET_BYTES], TOKEN, Failure::DeviceSecret),
        (SECRET, "", Failure::TokenForm),
    ] {
        let mut r = Rig::new("Alice");
        r.begin(Purpose::Connect, "Alice", 3, 0)?;
        assert_eq!(r.step(1, |s, t| s.spend(t, 3, &secret, token)).err(), Some(why));
        assert_eq!(r.spend(3, 2).err(), Some(Failure::NoTry));
    }
    let mut r = Rig::new("Alice");
    r.begin(Purpose::Connect, "Alice", 3, 0)?;
    r.spend(3, 0)?;
    assert_eq!(r.spend(3, 0).err(), Some(Failure::NoTry));
    Ok(())
}

#[test]
fn claiming_a_device_with_a_different_secret_gets_a_fresh_row_and_leaves_the_claimed_try_working()
-> Result<(), Failure> {
    let mut original = Rig::new("Alice");
    let stored = Fingerprint::of(&SECRET);
    let stranger = [7; SECRET_BYTES];
    let other = Fingerprint::of(&stranger);
    original.begin(Purpose::Connect, "Alice", 1, 0)?;
    let candidate = Some((&original.scope, &stored));
    assert_eq!(known_device(candidate, &stored), Some(&original.scope));
    assert!(known_device(candidate, &other).is_none());
    let original_target = original.scope.clone();
    assert_eq!(
        original.step(0, |s, t| s
            .begin(
                t,
                Purpose::Connect,
                original_target,
                desc(),
                other.clone(),
                challenge(2)
            )
            .map(|_| ())),
        Err(Failure::DeviceSecret)
    );
    let new_scope = Scope {
        device: 99,
        ..original.scope.clone()
    };
    let mut new = Rig {
        device: Device::new(&new_scope),
        slot: Slot::new(&new_scope, other.clone()),
        scope: new_scope.clone(),
        mark: 0,
        switched: None,
    };
    new.step(0, |s, t| {
        s.begin(
            t,
            Purpose::Connect,
            new_scope.clone(),
            desc(),
            other,
            challenge(1),
        )
        .map(|_| ())
    })?;
    let check = new.step(1, |s, t| s.spend(t, 1, &stranger, TOKEN))?;
    let probe = new.call(check)?;
    let ping = new.next(probe, MISSING_U)?;
    let user = new.next(ping, OK)?;
    let receipt = new.proven(user, ALICE)?;
    assert_eq!(receipt.source().device, 99);
    new.commit(receipt, Some(Fingerprint::of(&[6; SECRET_BYTES])), 1)?;
    let check = original.spend(1, 2)?;
    let probe = original.call(check)?;
    let ping = original.next(probe, MISSING_U)?;
    let user = original.next(ping, OK)?;
    let receipt = original.proven(user, ALICE)?;
    assert_eq!(receipt.source().device, 77);
    original.commit(receipt, Some(Fingerprint::of(&[8; SECRET_BYTES])), 2)?;
    assert_eq!(
        original
            .step(3, |_, t| t
                .arrive(&Fingerprint::of(&[6; SECRET_BYTES]), Access::Read, false)
                .map_err(Failure::Session))
            .err(),
        Some(Failure::Session(Denied::NotSignedIn))
    );
    Ok(())
}

#[test]
fn a_new_begin_invalidates_both_an_open_try_and_one_already_at_the_music_server() -> Result<(), Failure> {
    let mut r = Rig::new("Alice");
    r.begin(Purpose::Connect, "Alice", 3, 0)?;
    r.begin(Purpose::Connect, "Alice", 4, 1)?;
    assert_eq!(r.spend(3, 2).err(), Some(Failure::NoTry));
    let old = r.spend(4, 2)?;
    let issued = r.call(old)?;
    r.begin(Purpose::Connect, "Alice", 5, 3)?;
    assert_eq!(
        r.answer(issued, MISSING_U).err(),
        Some(Failure::Session(Denied::NotSignedIn))
    );
    assert_eq!(r.begin(Purpose::Connect, "Alice", 4, 3), Err(Failure::Number));
    r.spend(5, 3)?;
    Ok(())
}

#[test]
fn spending_and_receipt_creation_end_at_sixty_seconds_and_an_old_receipt_cannot_grant()
-> Result<(), Failure> {
    for (at, succeeds) in [(59_999, true), (60_000, false), (u64::MAX, false)] {
        let mut r = Rig::new("Alice");
        r.begin(Purpose::Connect, "Alice", 1, 0)?;
        assert_eq!(r.spend(1, at).is_ok(), succeeds);
    }
    let mut r = Rig::new("Alice");
    let late = r.user("Alice", Purpose::Connect, 1)?;
    assert_eq!(
        r.step(TRY_MS, |_, t| late.answer(
            t,
            Some((200, ALICE)),
            Admission::Allowed
        ))
        .err(),
        Some(Failure::Stale)
    );
    let mut r = Rig::new("Alice");
    let u = r.user("Alice", Purpose::Connect, 1)?;
    let v = r.proven(u, ALICE)?;
    assert_eq!(
        r.commit(v, Some(Fingerprint::of(&[9; SECRET_BYTES])), TRY_MS),
        Err(Failure::Session(Denied::NotSignedIn))
    );
    Ok(())
}

#[test]
fn the_probe_carries_no_sign_in_and_only_exact_missing_u_allows_the_ping() -> Result<(), Failure> {
    for (answer, expected) in [
        (REFUSED, Outcome::UntrustedCheck),
        (OK, Outcome::UntrustedCheck),
        (MISSING_V, Outcome::CouldNotAsk),
        (MISSING_C, Outcome::CouldNotAsk),
        (FORBIDDEN, Outcome::CouldNotAsk),
        (XML_MISSING_U, Outcome::CouldNotAsk),
    ] {
        let mut r = Rig::new("Alice");
        let probe = r.exchange("Alice", Purpose::Connect, 1)?;
        assert!(probe.needs_trust_probe_header());
        assert!(probe.form() == "v=1.16.1&c=nexus-music&f=json");
        assert_eq!(probe.path(), "/rest/ping");
        assert_eq!(outcome(r.answer(probe, answer)?), Some(expected));
    }
    Ok(())
}

#[test]
fn only_a_local_limit_is_too_many_tries_the_measured_upstream_limit_is_code_forty() -> Result<(), Failure> {
    let mut r = Rig::new("Alice");
    let probe = r.exchange("Alice", Purpose::Connect, 1)?;
    let stop = r.step(1, |_, t| {
        probe.answer(t, Some((200, MISSING_U)), Admission::LimitReached)
    })?;
    assert_eq!(outcome(stop), Some(Outcome::TooManyTries));
    for refused in [REFUSED, REFUSED_WITH_VERSION] {
        let probe = r.exchange("Alice", Purpose::Connect, 2 + r.mark)?;
        let ping = r.next(probe, MISSING_U)?;
        assert_eq!(outcome(r.answer(ping, refused)?), Some(Outcome::Refused));
        r.mark += 1;
    }
    Ok(())
}

#[test]
fn both_posts_are_built_here_using_only_the_three_credentials_and_fixed_protocol_fields()
-> Result<(), Failure> {
    let name = "A &p=bad+jwt";
    let mut r = Rig::new(name);
    let probe = r.exchange(name, Purpose::Connect, 1)?;
    let ping = r.next(probe, MISSING_U)?;
    assert!(!ping.needs_trust_probe_header());
    assert_eq!(ping.path(), "/rest/ping");
    let encoded = "A%20%26p%3Dbad%2Bjwt";
    let auth = format!("&u={encoded}&s={}&t={TOKEN}", "ab".repeat(32));
    assert!(ping.form() == format!("v=1.16.1&c=nexus-music&f=json{auth}"));
    let user = r.next(ping, OK_WITH_VERSION)?;
    assert!(!user.needs_trust_probe_header());
    assert_eq!(user.path(), "/rest/getUser");
    assert!(user.form() == format!("v=1.16.1&c=nexus-music&f=json&username={encoded}{auth}"));
    Ok(())
}

#[test]
fn each_recorded_json_refusal_and_unexpected_xml_is_read_in_its_request_context() -> Result<(), Failure> {
    for bytes in [
        MISSING_U,
        MISSING_V,
        MISSING_C,
        FORBIDDEN,
        SONG_MISSING,
        SONG,
        XML_OK,
        XML_REFUSED,
        XML_MISSING_U,
        XML_MISSING_C,
        XML_MISSING_V,
        XML_LARGE,
    ] {
        let mut r = Rig::new("Alice");
        let probe = r.exchange("Alice", Purpose::Connect, 1)?;
        let ping = r.next(probe, MISSING_U)?;
        assert_eq!(outcome(r.answer(ping, bytes)?), Some(Outcome::CouldNotAsk));
        let user = r.user("Alice", Purpose::Connect, 2)?;
        assert_eq!(outcome(r.answer(user, bytes)?), Some(Outcome::CouldNotAsk));
    }
    for name in ["Alice", "ghostmarker"] {
        let mut r = Rig::new(name);
        let user = r.user(name, Purpose::Connect, 1)?;
        assert_eq!(outcome(r.answer(user, REFUSED)?), Some(Outcome::Refused));
    }
    Ok(())
}

#[test]
fn an_ok_ping_and_an_exact_stored_getuser_name_are_both_required() -> Result<(), Failure> {
    for (name, wire) in [("Alice", ALICE), ("bob smith", BOB), ("Exp-Admin", ADMIN)] {
        let mut r = Rig::new(name);
        let u = r.user(name, Purpose::Connect, 1)?;
        let v = r.proven(u, wire)?;
        assert_eq!(v.description(), &desc());
        assert_eq!(v.account(), name);
        assert_eq!(v.source(), &r.scope);
        assert_eq!((v.spent(), v.made()), (0, 0));
        assert_eq!(v.purpose(), &Purpose::Connect);
        assert_eq!(
            r.commit(v, Some(Fingerprint::of(&[7; SECRET_BYTES])), 1)?,
            Outcome::Proven
        );
    }
    for name in ["alice", "ALICE", " Alice", "Alice "] {
        let mut r = Rig::new(name);
        let u = r.user(name, Purpose::Connect, 1)?;
        assert_eq!(outcome(r.answer(u, ALICE)?), Some(Outcome::CouldNotAsk));
    }
    let mut r = Rig::new("Alice");
    let u = r.user("Alice", Purpose::Connect, 1)?;
    assert_eq!(outcome(r.answer(u, OK)?), Some(Outcome::CouldNotAsk));
    Ok(())
}

#[test]
fn an_authenticated_account_change_ends_the_old_device_but_a_mismatched_reply_does_not()
-> Result<(), Failure> {
    let mut r = Rig::new("Alice");
    let session = Fingerprint::of(&[9; SECRET_BYTES]);
    let u = r.user("Alice", Purpose::Connect, 1)?;
    let v = r.proven(u, ALICE)?;
    r.commit(v, Some(session.clone()), 1)?;
    let old = r.scope.number;
    let u = r.user("Alice", Purpose::Again(session.clone()), 2)?;
    assert_eq!(outcome(r.answer(u, BOB)?), Some(Outcome::CouldNotAsk));
    assert_eq!(r.scope.number, old);
    let u = r.user("Alice", Purpose::Again(session.clone()), 3)?;
    let former = r.proven(u, ALICE)?;
    let u = r.user("bob smith", Purpose::Again(session.clone()), 4)?;
    let v = r.proven(u, BOB)?;
    assert_eq!(
        r.commit(v, Some(Fingerprint::of(&[8; SECRET_BYTES])), 2),
        Err(Failure::Session(Denied::NotSignedIn))
    );
    assert!(r.scope.number > old);
    assert_eq!(
        r.commit(former, Some(Fingerprint::of(&[22; SECRET_BYTES])), 3),
        Err(Failure::Session(Denied::NotSignedIn))
    );
    let new = r.switched.as_ref().ok_or(Failure::Stale)?;
    assert_eq!(new.scope.account, "bob smith");
    assert_ne!(new.scope.device, r.scope.device);
    let mut b = Device::new(&new.scope);
    let no_session = b.decide(
        &new.scope,
        Clock {
            now: 2,
            high_water: 2,
        },
        |t| t.arrive(&session, Access::Read, false),
    );
    assert_eq!(no_session.apply(|_| Ok(()))?.err(), Some(Denied::NotSignedIn));
    assert_eq!(
        r.step(3, |_, t| t
            .arrive(&session, Access::Read, false)
            .map_err(Failure::Session))
            .err(),
        Some(Failure::Session(Denied::NotSignedIn))
    );
    Ok(())
}

#[test]
fn ending_restoring_or_deleting_the_life_during_a_connect_gives_no_grant() -> Result<(), Failure> {
    for change in ["end", "restore", "life"] {
        let mut r = Rig::new("Alice");
        let u = r.user("Alice", Purpose::Connect, 1)?;
        let v = r.proven(u, ALICE)?;
        match change {
            "end" => r.step(1, |_, t| t.end().map_err(Failure::Session))?,
            "restore" => r.step(1, |_, t| t.restore_device(2).map_err(Failure::Session))?,
            _ => r.scope.life += 1,
        }
        assert_eq!(
            r.commit(v, Some(Fingerprint::of(&[7; SECRET_BYTES])), 2),
            Err(Failure::Session(Denied::NotSignedIn))
        );
    }
    Ok(())
}

#[test]
fn malformed_ambiguous_oversized_or_incomplete_envelopes_never_prove_anything() {
    let large = vec![b' '; ANSWER_MAX_BYTES + 1];
    for body in [b"".as_slice(), b"{}", b"not JSON", b"{}", b"\xff", br#"{"subsonic-response":{"status":"ok"}"#,
        br#"{"subsonic-response":{"status":"ok","status":"failed"}}"#, br#"{"subsonic-response":{"status":"ok","error":null}}"#,
        br#"{"subsonic-response":{"status":"ok","user":{"username":"Alice","extra":{"x":1,"x":2}}}}"#,
        br#"{"subsonic-response":{"status":"ok","user":{"username":1}}}"#,
        br#"{"subsonic-response":{"status":"failed","error":{"code":"40","message":"Wrong username or password"}}}"#,
        br#"{"subsonic-response":{"status":"failed","error":{"code":40}}}"#, br#"{"subsonic-response":{"status":"ok"}}{}"#,
        &large] { assert!(read(200,body).is_none()); }
    for (depth, accepted) in [(13, true), (14, false)] {
        // Root, response and user contribute three containers before these arrays.
        let nested = format!(
            r#"{{"subsonic-response":{{"status":"ok","user":{{"username":"Alice","extra":{}{}}}}}}}"#,
            "[".repeat(depth),
            "]".repeat(depth)
        );
        assert_eq!(read(200, nested.as_bytes()).is_some(), accepted);
    }
    for status in [302, 401, 429, 500] {
        assert!(read(status, OK).is_none());
    }
}

#[test]
fn a_finish_reply_waits_a_quarter_second_for_every_outcome_even_near_clock_limits() {
    assert_eq!(wait_before_reply(10, 10), 250);
    assert_eq!(wait_before_reply(10, 259), 1);
    assert_eq!(wait_before_reply(10, 260), 0);
    assert_eq!(wait_before_reply(10, 9), 250);
    assert_eq!(wait_before_reply(u64::MAX - 1, u64::MAX), 249);
}

#[test]
fn tokens_salts_and_credential_forms_never_print_their_contents() -> Result<(), Failure> {
    let token = Token::read(TOKEN).ok_or(Failure::TokenForm)?;
    assert_eq!(format!("{token:?}"), "Token(..)");
    assert_eq!(format!("{:?}", challenge(1).salt), "Salt(..)");
    let mut r = Rig::new("Alice");
    let probe = r.exchange("Alice", Purpose::Connect, 1)?;
    let ping = r.next(probe, MISSING_U)?;
    assert_eq!(format!("{ping:?}"), "Call(..)");
    assert_eq!(format!("{:?}", r.slot), "Slot(..)");
    assert_eq!(format!("{:?}", challenge(1)), "Challenge(..)");
    let user = r.next(ping, OK)?;
    let receipt = r.proven(user, ALICE)?;
    assert_eq!(format!("{receipt:?}"), "Receipt(..)");
    Ok(())
}

#[test]
fn a_delete_receipt_grants_only_the_bound_session_one_deletion() -> Result<(), Failure> {
    let mut r = Rig::new("Alice");
    let session = Fingerprint::of(&[17; SECRET_BYTES]);
    let user = r.user("Alice", Purpose::Connect, 1)?;
    let receipt = r.proven(user, ALICE)?;
    r.commit(receipt, Some(session.clone()), 1)?;
    let purpose = Purpose::Delete(session.clone());
    let user = r.user("Alice", purpose.clone(), 2)?;
    let receipt = r.proven(user, ALICE)?;
    assert_eq!(receipt.purpose(), &purpose);
    assert_eq!(
        r.commit(receipt, Some(Fingerprint::of(&[18; SECRET_BYTES])), 2),
        Err(Failure::Session(Denied::WrongPurpose))
    );
    assert_eq!(
        r.step(2, |_, t| t.delete(&session).map_err(Failure::Session)),
        Err(Failure::Session(Denied::ProveAgain))
    );
    let user = r.user("Alice", purpose, 3)?;
    let receipt = r.proven(user, ALICE)?;
    let remembered = r.scope.life;
    r.commit(receipt, None, 3)?;
    r.step(4, |_, t| t.delete(&session).map_err(Failure::Session))?;
    assert_eq!(r.scope.life, remembered + 1);
    assert_eq!(
        r.step(4, |_, t| t.delete(&session).map_err(Failure::Session)),
        Err(Failure::Session(Denied::NotSignedIn))
    );
    Ok(())
}

#[test]
fn a_receipt_made_in_the_last_millisecond_can_be_used_after_the_try_deadline() -> Result<(), Failure> {
    let mut r = Rig::new("Alice");
    let user = r.user("Alice", Purpose::Connect, 1)?;
    let Step::Verified(receipt) = r.step(TRY_MS - 1, |_, t| {
        user.answer(t, Some((200, ALICE)), Admission::Allowed)
    })?
    else {
        return Err(Failure::Stale);
    };
    assert_eq!((receipt.spent(), receipt.made()), (0, TRY_MS - 1));
    r.commit(
        receipt,
        Some(Fingerprint::of(&[8; SECRET_BYTES])),
        TRY_MS + RECEIPT_MS - 2,
    )?;
    Ok(())
}

#[test]
fn the_literal_status_and_error_fragments_printed_in_e3_are_read_inside_the_envelope() -> Result<(), Failure>
{
    // E3.1–2 printed these fragments verbatim, not full raw JSON responses.
    let ok = r#"{"status":"ok"}"#;
    for (fragment, expected) in [
        (
            r#"{"code":40,"message":"Wrong username or password"}"#,
            Outcome::Refused,
        ),
        (
            r#"{"code":50,"message":"User is not authorized for the given operation"}"#,
            Outcome::CouldNotAsk,
        ),
        (r#"{"code":70,"message":"Song not found"}"#, Outcome::CouldNotAsk),
    ] {
        let mut r = Rig::new("Alice");
        let probe = r.exchange("Alice", Purpose::Connect, 1)?;
        let ping = r.next(probe, MISSING_U)?;
        let user = r.next(ping, format!(r#"{{"subsonic-response":{ok}}}"#).as_bytes())?;
        let body = format!(r#"{{"subsonic-response":{{"status":"failed","error":{fragment}}}}}"#);
        assert_eq!(outcome(r.answer(user, body.as_bytes())?), Some(expected));
    }
    Ok(())
}
