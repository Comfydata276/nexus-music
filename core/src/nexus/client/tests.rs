//! The specification: every sequence of section 6 (and the slice notes' C10), fed in as
//! events, with the things to do checked. Made-up names and addresses only.

use super::*;
use crate::nexus::account::{Device, Notice, Person, SignedIn, Stamp, StampTag, Wait};
use crate::nexus::proto::Hello;

const NEXUS: &str = "https://nexus.example.org";
const MUSIC: &str = "https://music.example.org/navidrome";
const EMAIL: &str = "ada@example.org";
const PASSWORD: &str = "a long enough password";
const SESSION: [u8; 32] = [0x5e; 32];
const START_MS: u64 = 1_800_000_000_000;
const DAY: u64 = 24 * 60 * 60 * 1000;

fn backend() -> BackendId {
    BackendId::from_bytes([7; 16])
}

fn app(kind: DeviceKind) -> App {
    App { version: Version { major: 0, minor: 41, patch: 0 }, kind, device_name: Name::new("Ada's phone").unwrap() }
}

fn token(count: u64) -> [u8; 16] {
    [0xa0 | (count as u8 & 0x0f); 16]
}

fn login(count: u64) -> MusicLogin {
    MusicLogin {
        address: account::MusicAddress::new(MUSIC).unwrap(),
        name: Name::new("ada").unwrap(),
        salt: account::MusicSalt::new("s4ltS4LTs4ltS4LT").unwrap(),
        token: account::MusicToken::from_bytes(token(count)),
        count,
    }
}

fn receipt_from(backend: BackendId, restore: u64, count: u64) -> Receipt {
    Receipt {
        stamp: Stamp { backend, restore, account: Email::new(EMAIL).unwrap(), life: 1, change: 1, tag: StampTag::from_bytes([1; 32]) },
        music_login_count: count,
    }
}

fn receipt(count: u64) -> Receipt {
    receipt_from(backend(), 1, count)
}

fn person() -> Person {
    Person { email: Email::new(EMAIL).unwrap(), name: Name::new("Ada").unwrap(), admin: false }
}

fn hello_body(has: &[&str]) -> Vec<u8> {
    Hello::new(Version { major: 0, minor: 2, patch: 0 }, backend(), SPEAKS, has).unwrap().to_json().into_bytes()
}

fn encoded<T>(reply: T) -> Vec<u8>
where
    T: Serialize,
{
    // account's encoder needs its own trait; JSON written by serde is the same bytes.
    serde_json::to_vec(&reply).unwrap()
}

fn signed_in_body(count: u64) -> Vec<u8> {
    encoded(Reply::Done {
        result: SignInAnswer::SignedIn(SignedIn {
            session: Session::from_bytes(SESSION),
            person: person(),
            device: DeviceId::new(3).unwrap(),
            music: login(count),
            notice: Notice::new("What you play is recorded.").unwrap(),
        }),
        receipt: receipt(count),
    })
}

fn refused(refusal: Refusal) -> Vec<u8> {
    encoded(Reply::<SignedOut>::Refused(refusal))
}

fn music_body(count: u64) -> Vec<u8> {
    encoded(Reply::Done { result: login(count), receipt: receipt(count) })
}

fn devices_body(count: u64, devices: Vec<Device>) -> Vec<u8> {
    encoded(Reply::Done { result: DevicePage::new(devices, false).unwrap(), receipt: receipt(count) })
}

fn device(id: u64, signed_in: bool) -> Device {
    Device {
        id: DeviceId::new(id).unwrap(),
        name: Name::new(&format!("Device {id}")).unwrap(),
        kind: DeviceKind::Pc,
        app_version: Version { major: 0, minor: 41, patch: 0 },
        last_seen_ms: START_MS - DAY,
        signed_in,
    }
}

fn saved(count: u64) -> Saved {
    Saved {
        nexus: NEXUS.to_owned(),
        backend_id: backend().to_hex(),
        account: EMAIL.to_owned(),
        email: EMAIL.to_owned(),
        name: "Ada".to_owned(),
        device: 3,
        restore: 1,
        life: 1,
        change: 1,
        notice: None,
        session: Some(Session::from_bytes(SESSION)),
        music: Some(login(count)),
    }
}

fn kept_signed_in(count: u64) -> Kept {
    Kept { device_secret: Some(DeviceSecret::from_bytes([0xd5; 32])), saved: Some(saved(count)), note: None, offline: false }
}

/// A device under test: the machine, the clock, and every thing it was told to do.
struct Rig {
    client: Client,
    kind: DeviceKind,
    now: u64,
    /// Everything ever given back, as JSON, in order.
    log: Vec<String>,
    shown: Option<Shown>,
}

impl Rig {
    fn new(kind: DeviceKind) -> Rig {
        Rig { client: Client::default(), kind, now: START_MS, log: Vec::new(), shown: None }
    }

    fn given(&self) -> Given {
        Given { now_ms: self.now, local_offset_minutes: 600, random: Some(Random::new([0x42; 32])) }
    }

    fn take(&mut self, out: Vec<Do>) -> Vec<Do> {
        for thing in &out {
            self.log.push(serde_json::to_string(thing).unwrap());
            if let Do::Show(shown) = thing {
                self.shown = Some(shown.clone());
            }
        }
        out
    }

    fn start(&mut self, kept: Kept) -> Vec<Do> {
        let given = self.given();
        let out = self.client.start(app(self.kind), kept, &given);
        self.take(out)
    }

    fn happen(&mut self, event: Happened) -> Vec<Do> {
        let given = self.given();
        let out = self.client.happened(event, &given);
        self.take(out)
    }

    fn press(&mut self, press: Press) -> Vec<Do> {
        self.happen(Happened::Pressed(press))
    }

    fn answer(&mut self, ask: u64, status: u16, body: Vec<u8>) -> Vec<Do> {
        self.happen(Happened::Answered { ask, status, body: AnswerBody::new(body) })
    }

    fn fail(&mut self, ask: u64, how: How) -> Vec<Do> {
        self.happen(Happened::Failed { ask, failure: Failure { how, inside: None, permission: None } })
    }

    fn sign_in_press(&mut self) -> Vec<Do> {
        self.press(Press::SignIn { address: "nexus.example.org".to_owned(), email: EMAIL.to_owned(), password: Typed::new(PASSWORD) })
    }

    /// A whole sign-in, answered: gives back the last answer's things to do.
    fn sign_in(&mut self, count: u64) -> Vec<Do> {
        let out = self.sign_in_press();
        let hello = only_ask(&out).id;
        let out = self.answer(hello, 200, hello_body(&["accounts"]));
        let sign_in = only_ask(&out).id;
        self.answer(sign_in, 200, signed_in_body(count))
    }

    fn shown(&self) -> &Shown {
        self.shown.as_ref().unwrap()
    }
}

fn asks(out: &[Do]) -> Vec<&Ask> {
    out.iter()
        .filter_map(|thing| match thing {
            Do::Ask(ask) => Some(ask),
            _ => None,
        })
        .collect()
}

fn only_ask(out: &[Do]) -> &Ask {
    let found = asks(out);
    assert_eq!(found.len(), 1, "one ask, in {out:?}");
    found[0]
}

fn ask_to<'a>(out: &'a [Do], path: &str) -> &'a Ask {
    asks(out).into_iter().find(|ask| ask.url.ends_with(path)).unwrap_or_else(|| panic!("an ask to {path} in {out:?}"))
}

fn handed(out: &[Do]) -> Vec<&Hand> {
    out.iter()
        .filter_map(|thing| match thing {
            Do::Hand(hand) => Some(hand),
            _ => None,
        })
        .collect()
}

fn forgets(out: &[Do]) -> Vec<Forget> {
    out.iter()
        .filter_map(|thing| match thing {
            Do::Forget(forget) => Some(*forget),
            _ => None,
        })
        .collect()
}

fn kept_saved(out: &[Do]) -> Vec<&Saved> {
    out.iter()
        .filter_map(|thing| match thing {
            Do::Keep(Keep::Saved(saved)) => Some(saved),
            _ => None,
        })
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// ---------------------------------------------------------------------------------------
// Starting: playing never waits (R1, 6.1)
// ---------------------------------------------------------------------------------------

#[test]
fn cold_start_plays_from_what_sign_in_left() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(kept_signed_in(4));
    // The very first thing: the player is handed the kept login, before anything is asked.
    assert!(matches!(&out[0], Do::Hand(Hand::Login(login)) if login.count == 4), "{out:?}");
    let ask = only_ask(&out);
    assert!(ask.url.ends_with(account::LIST_DEVICES));
    assert_eq!(rig.shown().screen, Screen::SignedIn);
    assert!(rig.shown().plays);
}

#[test]
fn a_cold_start_with_the_backend_down_keeps_everything_and_says_since_when() {
    let mut rig = Rig::new(DeviceKind::Pc);
    let out = rig.start(kept_signed_in(4));
    let check = only_ask(&out).id;
    for how in [How::Connect, How::Timeout] {
        let _ = how;
    }
    let out = rig.fail(check, How::Timeout);
    assert!(forgets(&out).is_empty());
    assert!(handed(&out).is_empty());
    // 1_800_000_000_000 ms is 08:00 UTC; ten hours east is 18:00.
    assert_eq!(rig.shown().row, "Nexus server: not reachable since 18:00. Your music plays as usual.");
    assert_eq!(rig.shown().screen, Screen::SignedIn);
    // Ticks ask again only at the check's own pace, never in a loop.
    rig.now += TICK_MS;
    assert!(asks(&rig.happen(Happened::Tick)).is_empty());
    rig.now += CHECK_EVERY_MS;
    assert_eq!(asks(&rig.happen(Happened::Tick)).len(), 1);
}

#[test]
fn a_cold_start_with_offline_on_hands_the_login_and_asks_nothing() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let mut kept = kept_signed_in(4);
    kept.offline = true;
    let out = rig.start(kept);
    assert!(matches!(&out[0], Do::Hand(Hand::Login(_))));
    assert!(asks(&out).is_empty());
    assert!(asks(&rig.happen(Happened::Tick)).is_empty());
    assert!(asks(&rig.happen(Happened::Network { up: true })).is_empty());
}

#[test]
fn a_device_secret_is_made_once_from_the_random_bytes_handed_in() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(Kept::default());
    assert!(out.iter().any(|thing| matches!(thing, Do::Keep(Keep::DeviceSecret(secret)) if secret.expose_bytes() == &[0x42; 32])));
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(kept_signed_in(1));
    assert!(!out.iter().any(|thing| matches!(thing, Do::Keep(Keep::DeviceSecret(_)))));
}

#[test]
fn with_no_random_bytes_signing_in_waits_and_says_so() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let given = Given { now_ms: START_MS, local_offset_minutes: 0, random: None };
    let out = rig.client.start(app(DeviceKind::Phone), Kept::default(), &given);
    rig.take(out);
    let out = rig.client.happened(
        Happened::Pressed(Press::SignIn { address: NEXUS.to_owned(), email: EMAIL.to_owned(), password: Typed::new(PASSWORD) }),
        &given,
    );
    let out = rig.take(out);
    assert!(asks(&out).is_empty());
    assert_eq!(rig.shown().sign_in.state, SignInState::NoRandom);
}

// ---------------------------------------------------------------------------------------
// Signing in
// ---------------------------------------------------------------------------------------

#[test]
fn signing_in_asks_hello_at_the_typed_origin_then_signs_in_and_keeps_one_thing() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let out = rig.sign_in_press();
    let hello = only_ask(&out);
    assert_eq!(hello.url, "https://nexus.example.org/nexus/api/hello");
    assert_eq!(hello.method, Method::Get);
    assert!(hello.authorization.is_none() && hello.body.is_none());
    assert_eq!(hello.max_body, proto::HELLO_MAX_BYTES);
    assert_eq!(rig.shown().sign_in.state, SignInState::Working);
    let hello = hello.id;

    let out = rig.answer(hello, 200, hello_body(&["accounts"]));
    let sign_in = only_ask(&out);
    assert_eq!(sign_in.url, "https://nexus.example.org/nexus/api/signin");
    assert_eq!(sign_in.method, Method::Post);
    assert!(sign_in.authorization.is_none());
    assert_eq!(sign_in.within_ms, SIGN_IN_WITHIN_MS);
    let body: serde_json::Value = serde_json::from_str(sign_in.body.as_ref().unwrap().expose()).unwrap();
    assert_eq!(body["credential"]["password"]["email"], EMAIL);
    assert_eq!(body["device_secret"], hex(&[0x42; 32]));
    assert_eq!(body["restore"], serde_json::Value::Null);
    let sign_in = sign_in.id;

    let out = rig.answer(sign_in, 200, signed_in_body(2));
    let kept = kept_saved(&out);
    assert_eq!(kept.len(), 1, "kept as one thing: {out:?}");
    assert_eq!(kept[0].nexus, NEXUS);
    assert_eq!(kept[0].backend_id, backend().to_hex());
    assert_eq!(kept[0].session.as_ref().map(|session| *session.expose_bytes()), Some(SESSION));
    assert_eq!(kept[0].music.as_ref().map(|login| login.count), Some(2));
    // Kept first, whole; then the player.
    let keep_at = out.iter().position(|thing| matches!(thing, Do::Keep(Keep::Saved(_)))).unwrap();
    let hand_at = out.iter().position(|thing| matches!(thing, Do::Hand(Hand::Login(_)))).unwrap();
    assert!(keep_at < hand_at);
    let shown = rig.shown();
    assert_eq!(shown.screen, Screen::SignedIn);
    assert_eq!(shown.who.as_deref(), Some("Ada (ada@example.org)"));
    assert_eq!(shown.notice.as_deref(), Some("What you play is recorded."));
    assert_eq!(shown.row, "Nexus server: connected");

    let out = rig.press(Press::NoticeSeen);
    assert!(kept_saved(&out)[0].notice.is_none());
    assert!(rig.shown().notice.is_none());
}

#[test]
fn what_may_be_typed_as_the_address() {
    for (typed, state) in [
        ("http://nexus.example.org", SignInState::NeedsHttps),
        ("HTTP://nexus.example.org", SignInState::NeedsHttps),
        ("", SignInState::BadAddress),
        ("ftp://nexus.example.org", SignInState::BadAddress),
        ("https://nexus.example.org/music", SignInState::BadAddress),
        ("https://nexus.example.org?x=1", SignInState::BadAddress),
        ("https://ada:pw@nexus.example.org", SignInState::BadAddress),
        ("https://nexus example.org", SignInState::BadAddress),
    ] {
        let mut rig = Rig::new(DeviceKind::Pc);
        rig.start(Kept::default());
        let out = rig.press(Press::SignIn { address: typed.to_owned(), email: EMAIL.to_owned(), password: Typed::new(PASSWORD) });
        assert!(asks(&out).is_empty(), "{typed}");
        assert_eq!(rig.shown().sign_in.state, state, "{typed}");
    }
    for typed in ["nexus.example.org", "https://nexus.example.org/", " https://Nexus.Example.org ", "nexus.example.org:8443"] {
        let mut rig = Rig::new(DeviceKind::Pc);
        rig.start(Kept::default());
        let out = rig.press(Press::SignIn { address: typed.to_owned(), email: EMAIL.to_owned(), password: Typed::new(PASSWORD) });
        assert!(only_ask(&out).url.starts_with("https://nexus.example.org"), "{typed}");
        assert!(only_ask(&out).url.ends_with("/nexus/api/hello"), "{typed}");
    }
}

#[test]
fn an_email_and_a_password_are_held_to_account_rules_before_anything_is_sent() {
    let mut rig = Rig::new(DeviceKind::Pc);
    rig.start(Kept::default());
    let out = rig.press(Press::SignIn { address: NEXUS.to_owned(), email: "not an email".to_owned(), password: Typed::new(PASSWORD) });
    assert!(asks(&out).is_empty());
    assert_eq!(rig.shown().sign_in.state, SignInState::BadEmail);
    let out = rig.press(Press::SignIn { address: NEXUS.to_owned(), email: EMAIL.to_owned(), password: Typed::new("") });
    assert!(asks(&out).is_empty());
    assert_eq!(rig.shown().sign_in.state, SignInState::NoPassword);
    // Shorter than any password can be: nobody's, so refused here as a wrong one, unsent.
    let out = rig.press(Press::SignIn { address: NEXUS.to_owned(), email: EMAIL.to_owned(), password: Typed::new("short") });
    assert!(asks(&out).is_empty());
    assert_eq!(rig.shown().sign_in.state, SignInState::WrongLogin);
}

#[test]
fn what_is_at_the_address_if_not_a_backend_that_has_accounts() {
    let cases: Vec<(u16, Vec<u8>, SignInState)> = vec![
        (302, Vec::new(), SignInState::NoNexusServer),
        (404, b"not found".to_vec(), SignInState::NoNexusServer),
        (200, b"<html>Navidrome</html>".to_vec(), SignInState::NoNexusServer),
        (200, hello_body(&["plays"]), SignInState::ServerTooOld),
    ];
    for (status, body, state) in cases {
        let mut rig = Rig::new(DeviceKind::Pc);
        rig.start(Kept::default());
        let hello = only_ask(&rig.sign_in_press()).id;
        let out = rig.answer(hello, status, body);
        assert!(asks(&out).is_empty(), "{status}");
        assert_eq!(rig.shown().sign_in.state, state, "{status}");
    }
    let mut rig = Rig::new(DeviceKind::Pc);
    rig.start(Kept::default());
    let hello = only_ask(&rig.sign_in_press()).id;
    let too_new = Hello::new(Version { major: 9, minor: 0, patch: 0 }, backend(), Speaks::new(5, 6).unwrap(), &["accounts"]).unwrap();
    rig.answer(hello, 200, too_new.to_json().into_bytes());
    assert_eq!(rig.shown().sign_in.state, SignInState::AppTooOld);
    assert!(rig.shown().sign_in.words.contains("too old for your Nexus server"));
}

#[test]
fn each_refusal_of_a_sign_in_is_its_own_sentence() {
    let cases = [
        (Refusal::WrongLogin, SignInState::WrongLogin, "Wrong email or password."),
        (Refusal::TooManyTries { wait: Wait::new(90_000).unwrap() }, SignInState::TooManyTries, "Wait 2 minutes"),
        (Refusal::AccountOff, SignInState::AccountOff, "switched off"),
        (Refusal::Busy, SignInState::Busy, "busy"),
    ];
    for (refusal, state, words) in cases {
        let mut rig = Rig::new(DeviceKind::Pc);
        rig.start(Kept::default());
        let hello = only_ask(&rig.sign_in_press()).id;
        let sign_in = only_ask(&rig.answer(hello, 200, hello_body(&["accounts"]))).id;
        let out = rig.answer(sign_in, 200, refused(refusal));
        assert!(asks(&out).is_empty());
        assert!(kept_saved(&out).is_empty());
        assert_eq!(rig.shown().sign_in.state, state);
        assert!(rig.shown().sign_in.words.contains(words), "{}", rig.shown().sign_in.words);
        assert_eq!(rig.shown().screen, Screen::SignIn);
    }
}

#[test]
fn a_sign_in_that_waits_its_turn_says_so() {
    let mut rig = Rig::new(DeviceKind::Pc);
    rig.start(Kept::default());
    rig.sign_in_press();
    assert!(rig.shown().sign_in.words.contains("up to a minute"));
}

#[test]
fn two_sign_ins_pressed_quickly_are_one_try() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let first = rig.sign_in_press();
    assert_eq!(asks(&first).len(), 1);
    let second = rig.sign_in_press();
    assert!(asks(&second).is_empty());
    // And while the sign-in itself (after hello) is under way.
    let hello = only_ask(&first).id;
    let out = rig.answer(hello, 200, hello_body(&["accounts"]));
    assert_eq!(asks(&out).len(), 1);
    assert!(asks(&rig.sign_in_press()).is_empty());
}

#[test]
fn an_answer_to_an_older_try_is_dropped() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let hello = only_ask(&rig.sign_in_press()).id;
    let first = only_ask(&rig.answer(hello, 200, hello_body(&["accounts"]))).id;
    // The first try fails (its answer was lost); the person tries again.
    rig.fail(first, How::Timeout);
    let hello = only_ask(&rig.sign_in_press()).id;
    let second = only_ask(&rig.answer(hello, 200, hello_body(&["accounts"]))).id;
    assert_ne!(first, second);
    // The first try's answer turns up late: dropped.
    let out = rig.answer(first, 200, signed_in_body(1));
    assert!(out.is_empty(), "{out:?}");
    let out = rig.answer(second, 200, signed_in_body(1));
    assert_eq!(kept_saved(&out).len(), 1);
}

#[test]
fn the_local_network_permission_is_asked_for_only_when_no_connection_was_opened() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let hello = only_ask(&rig.sign_in_press()).id;
    rig.happen(Happened::Failed { ask: hello, failure: Failure { how: How::Connect, inside: Some(true), permission: Some(false) } });
    assert_eq!(rig.shown().sign_in.state, SignInState::NeedsPermission);
    assert!(rig.shown().sign_in.ask_permission);

    // A public address, or the permission held: just "can't be reached".
    for failure in [
        Failure { how: How::Connect, inside: Some(false), permission: Some(false) },
        Failure { how: How::Connect, inside: Some(true), permission: Some(true) },
        Failure { how: How::Timeout, inside: Some(true), permission: Some(false) },
    ] {
        let mut rig = Rig::new(DeviceKind::Phone);
        rig.start(Kept::default());
        let hello = only_ask(&rig.sign_in_press()).id;
        rig.happen(Happened::Failed { ask: hello, failure });
        assert_eq!(rig.shown().sign_in.state, SignInState::CantReach, "{failure:?}");
        assert!(!rig.shown().sign_in.ask_permission);
    }
    // A certificate that is wrong never looks like a permission that is missing.
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let hello = only_ask(&rig.sign_in_press()).id;
    rig.happen(Happened::Failed { ask: hello, failure: Failure { how: How::Secure, inside: Some(true), permission: Some(false) } });
    assert_eq!(rig.shown().sign_in.state, SignInState::NotSecure);
    assert!(!rig.shown().sign_in.ask_permission);
    // A PC never shows the phone's permission screen.
    let mut rig = Rig::new(DeviceKind::Pc);
    rig.start(Kept::default());
    let hello = only_ask(&rig.sign_in_press()).id;
    rig.happen(Happened::Failed { ask: hello, failure: Failure { how: How::Connect, inside: Some(true), permission: Some(false) } });
    assert!(!rig.shown().sign_in.ask_permission);
}

#[test]
fn a_code_is_redeemed_with_its_new_password_and_nothing_is_kept_between() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let differ = rig.press(Press::SignInWithCode {
        address: NEXUS.to_owned(),
        code: Typed::new("CDFH-JKMN-3479"),
        password: Typed::new("my new long password"),
        again: Typed::new("my new long passw0rd"),
    });
    assert!(asks(&differ).is_empty());
    assert_eq!(rig.shown().sign_in.state, SignInState::PasswordsDiffer);
    let out = rig.press(Press::SignInWithCode {
        address: NEXUS.to_owned(),
        code: Typed::new("cdfh jkmn 3479"),
        password: Typed::new("my new long password"),
        again: Typed::new("my new long password"),
    });
    let hello = only_ask(&out).id;
    let out = rig.answer(hello, 200, hello_body(&["accounts"]));
    let sign_in = only_ask(&out);
    let body: serde_json::Value = serde_json::from_str(sign_in.body.as_ref().unwrap().expose()).unwrap();
    assert_eq!(body["credential"]["code"], "CDFHJKMN3479");
    let sign_in = sign_in.id;
    let setup = Session::from_bytes([0x11; 32]);
    let out = rig.answer(
        sign_in,
        200,
        encoded(Reply::Done {
            result: SignInAnswer::PasswordSetup(account::PasswordSetup { session: setup.clone(), person: person(), device: DeviceId::new(3).unwrap() }),
            receipt: receipt(0),
        }),
    );
    assert!(kept_saved(&out).is_empty(), "nothing kept between a code and a password");
    let choose = only_ask(&out);
    assert!(choose.url.ends_with(account::CHANGE_PASSWORD));
    assert_eq!(choose.authorization.as_ref().unwrap().expose(), &*setup.authorization());
    let choose = choose.id;
    let out = rig.answer(choose, 200, encoded(Reply::Done { result: PasswordChanged {}, receipt: receipt(0) }));
    let again = only_ask(&out);
    let body: serde_json::Value = serde_json::from_str(again.body.as_ref().unwrap().expose()).unwrap();
    assert_eq!(body["credential"]["password"]["password"], "my new long password");
    let again = again.id;
    let out = rig.answer(again, 200, signed_in_body(1));
    assert_eq!(kept_saved(&out).len(), 1);
    assert_eq!(rig.shown().screen, Screen::SignedIn);
}

// ---------------------------------------------------------------------------------------
// The fence: answers no longer waited for
// ---------------------------------------------------------------------------------------

#[test]
fn an_answer_arriving_after_sign_out_is_thrown_away() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(kept_signed_in(1));
    let out = rig.press(Press::OpenDevices);
    let _ = out;
    let out = rig.happen(Happened::MusicRefused { count: 1 });
    let music = ask_to(&out, account::MUSIC_LOGIN).id;
    let out = rig.press(Press::SignOut);
    assert_eq!(forgets(&out), vec![Forget::Saved]);
    // The renewal's answer arrives: nothing is kept and nothing handed.
    let out = rig.answer(music, 200, music_body(2));
    assert!(kept_saved(&out).is_empty() && handed(&out).is_empty(), "{out:?}");
    assert!(!rig.client.waits_for(music));
}

#[test]
fn the_account_switched_while_an_ask_is_in_flight() {
    let mut rig = Rig::new(DeviceKind::Pc);
    let out = rig.start(kept_signed_in(1));
    let check = only_ask(&out).id;
    let generation = rig.client.generation();
    rig.client.stop();
    assert!(rig.client.generation() > generation);
    // After stop, nothing at all.
    assert!(rig.answer(check, 200, refused(Refusal::NotSignedIn)).is_empty());
    // The next account starts; the first's answer turns up: it touches nothing.
    let mut other = kept_signed_in(7);
    if let Some(saved) = other.saved.as_mut() {
        saved.email = "grace@example.org".to_owned();
        saved.account = "grace@example.org".to_owned();
    }
    rig.start(other);
    let out = rig.answer(check, 200, refused(Refusal::NotSignedIn));
    assert!(out.is_empty(), "{out:?}");
    assert_eq!(rig.shown().screen, Screen::SignedIn);
}

#[test]
fn offline_switched_on_with_an_ask_in_flight() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let hello = only_ask(&rig.sign_in_press()).id;
    let out = rig.happen(Happened::Offline(true));
    assert!(asks(&out).is_empty());
    assert_eq!(rig.shown().sign_in.state, SignInState::Offline);
    // Its answer is not waited for.
    let out = rig.answer(hello, 200, hello_body(&["accounts"]));
    assert!(asks(&out).is_empty());
    // And nothing is asked while Offline is on.
    assert!(asks(&rig.sign_in_press()).is_empty());
    assert!(asks(&rig.happen(Happened::Tick)).is_empty());
}

#[test]
fn an_answer_is_taken_once() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(kept_signed_in(1));
    let check = only_ask(&out).id;
    let first = rig.answer(check, 200, devices_body(1, vec![device(3, true)]));
    assert!(!first.is_empty());
    assert!(rig.answer(check, 200, refused(Refusal::NotSignedIn)).is_empty());
    assert_eq!(rig.shown().screen, Screen::SignedIn);
}

// ---------------------------------------------------------------------------------------
// Endings: only the backend, to this session, signs a device out
// ---------------------------------------------------------------------------------------

#[test]
fn a_forced_out_device_stops_only_when_told() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(kept_signed_in(1));
    let check = only_ask(&out).id;
    // Things that look alike change nothing: a failure, a proxy's error, a malformed body.
    let out = rig.fail(check, How::Connect);
    assert!(forgets(&out).is_empty());
    let out = rig.happen(Happened::Network { up: true });
    let check = ask_to(&out, account::LIST_DEVICES).id;
    let out = rig.answer(check, 502, b"Bad Gateway".to_vec());
    assert!(forgets(&out).is_empty());
    let out = rig.happen(Happened::Network { up: true });
    let check = ask_to(&out, account::LIST_DEVICES).id;
    let out = rig.answer(check, 200, b"{\"refused\":\"not_signed_in\",\"refused\":1}".to_vec());
    assert!(forgets(&out).is_empty());
    assert_eq!(rig.shown().screen, Screen::SignedIn);
    // The backend says this session has ended.
    let out = rig.happen(Happened::Network { up: true });
    let check = ask_to(&out, account::LIST_DEVICES).id;
    let out = rig.answer(check, 200, refused(Refusal::NotSignedIn));
    assert_eq!(forgets(&out), vec![Forget::Saved]);
    assert_eq!(rig.shown().screen, Screen::SignIn);
    assert_eq!(rig.shown().sign_in.state, SignInState::ForcedOut);
    assert!(!rig.shown().plays);
    assert!(rig.shown().sign_in.words.contains("downloads are still here"));
}

#[test]
fn the_session_ended_while_the_device_was_offline_for_a_week() {
    let mut rig = Rig::new(DeviceKind::Pc);
    let mut kept = kept_signed_in(1);
    kept.offline = true;
    let out = rig.start(kept);
    assert!(asks(&out).is_empty());
    for _ in 0..7 {
        rig.now += DAY;
        assert!(asks(&rig.happen(Happened::Tick)).is_empty());
    }
    assert_eq!(rig.shown().screen, Screen::SignedIn);
    let out = rig.happen(Happened::Offline(false));
    let check = ask_to(&out, account::LIST_DEVICES).id;
    let out = rig.answer(check, 200, refused(Refusal::NotSignedIn));
    assert_eq!(forgets(&out), vec![Forget::Saved]);
    assert_eq!(rig.shown().sign_in.state, SignInState::ForcedOut);
}

#[test]
fn the_backend_put_back_from_a_copy_means_signing_in_again_with_the_music_kept() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(kept_signed_in(3));
    let check = only_ask(&out).id;
    let out = rig.answer(check, 200, refused(Refusal::StartAgain));
    assert!(forgets(&out).is_empty(), "the music login is not forgotten");
    let kept = kept_saved(&out);
    assert_eq!(kept.len(), 1);
    assert!(kept[0].session.is_none());
    assert_eq!(kept[0].music.as_ref().map(|login| login.count), Some(3));
    assert!(handed(&out).is_empty());
    assert_eq!(rig.shown().screen, Screen::SignIn);
    assert_eq!(rig.shown().sign_in.state, SignInState::StartAgain);
    assert!(rig.shown().plays);

    // Started again from that: the music plays, the form is up, and nothing is asked.
    let mut rig = Rig::new(DeviceKind::Phone);
    let mut kept = kept_signed_in(3);
    if let Some(saved) = kept.saved.as_mut() {
        saved.session = None;
    }
    let out = rig.start(kept);
    assert!(matches!(&out[0], Do::Hand(Hand::Login(_))));
    assert!(asks(&out).is_empty());
    assert_eq!(rig.shown().sign_in.state, SignInState::StartAgain);
    // Signing in again names the restore number the device has from this backend.
    let hello = only_ask(&rig.sign_in_press()).id;
    let out = rig.answer(hello, 200, hello_body(&["accounts"]));
    let sign_in = only_ask(&out);
    let body: serde_json::Value = serde_json::from_str(sign_in.body.as_ref().unwrap().expose()).unwrap();
    assert_eq!(body["restore"], 1);
}

#[test]
fn another_backend_answering_at_the_origin_is_not_ours() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(kept_signed_in(1));
    let check = only_ask(&out).id;
    let stranger = BackendId::from_bytes([9; 16]);
    let body = encoded(Reply::Done { result: DevicePage::new(vec![], false).unwrap(), receipt: receipt_from(stranger, 1, 1) });
    let out = rig.answer(check, 200, body);
    assert!(forgets(&out).is_empty() && kept_saved(&out).is_empty() && asks(&out).is_empty());
    assert!(rig.shown().row.contains("a different server answers"));
    // Nothing more is sent to it.
    rig.now += CHECK_EVERY_MS;
    assert!(asks(&rig.happen(Happened::Tick)).is_empty());
    assert!(asks(&rig.happen(Happened::MusicRefused { count: 1 })).is_empty());
}

// ---------------------------------------------------------------------------------------
// Signing out
// ---------------------------------------------------------------------------------------

#[test]
fn signing_out_is_lasting_at_once_and_the_backend_is_told() {
    let mut rig = Rig::new(DeviceKind::Pc);
    rig.start(kept_signed_in(1));
    let out = rig.press(Press::SignOut);
    assert_eq!(forgets(&out), vec![Forget::Saved]);
    let note = out.iter().find_map(|thing| match thing {
        Do::Keep(Keep::Note(note)) => Some(note.clone()),
        _ => None,
    });
    let note = note.unwrap();
    // The note can end that one session and nothing else: the session's SHA-256, never it.
    assert_eq!(note.session_sha256, hex(super::super::kept::Fingerprint::of(&SESSION).as_bytes()));
    assert!(!serde_json::to_string(&note).unwrap().contains(&hex(&SESSION)));
    let tell = ask_to(&out, account::SIGN_OUT);
    assert_eq!(tell.authorization.as_ref().unwrap().expose(), format!("Bearer {}", hex(&SESSION)));
    let tell = tell.id;
    assert_eq!(rig.shown().screen, Screen::SignIn);
    assert_eq!(rig.shown().sign_in.state, SignInState::SignedOut);
    let out = rig.answer(tell, 200, encoded(Reply::Done { result: SignedOut {}, receipt: receipt(1) }));
    assert_eq!(forgets(&out), vec![Forget::Note]);
}

#[test]
fn signing_out_offline_leaves_a_note_told_when_it_can_be() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let mut kept = kept_signed_in(1);
    kept.offline = true;
    rig.start(kept);
    let out = rig.press(Press::SignOut);
    assert_eq!(forgets(&out), vec![Forget::Saved]);
    assert!(asks(&out).is_empty());
    let note = out
        .iter()
        .find_map(|thing| match thing {
            Do::Keep(Keep::Note(note)) => Some(note.clone()),
            _ => None,
        })
        .unwrap();
    // A restart with Offline still on asks nothing; with it off, the note is told, without
    // the session.
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(Kept { note: Some(note.clone()), offline: true, ..Kept::default() });
    assert!(asks(&out).is_empty());
    let out = rig.happen(Happened::Offline(false));
    let tell = only_ask(&out);
    assert!(tell.url.ends_with(SIGN_OUT_NOTE));
    assert!(tell.authorization.is_none());
    assert!(tell.body.as_ref().unwrap().expose().contains(&note.session_sha256));
    let tell = tell.id;
    let out = rig.fail(tell, How::Connect);
    assert!(forgets(&out).is_empty());
    // Tried again at its own pace, not in a loop.
    assert!(asks(&rig.happen(Happened::Tick)).is_empty());
    rig.now += TELL_EVERY_MS;
    let tell = only_ask(&rig.happen(Happened::Tick)).id;
    let out = rig.answer(tell, 200, encoded(Reply::Done { result: SignedOut {}, receipt: receipt(1) }));
    assert_eq!(forgets(&out), vec![Forget::Note]);
    // A note 90 days old is given up.
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.now = note.made_ms + NOTE_LIFE_MS + 1;
    let out = rig.start(Kept { note: Some(note), ..Kept::default() });
    assert!(asks(&out).is_empty());
}

#[test]
fn a_late_sign_out_for_the_first_account_cannot_touch_the_second() {
    let mut rig = Rig::new(DeviceKind::Pc);
    rig.start(kept_signed_in(1));
    let out = rig.press(Press::SignOut);
    let tell = ask_to(&out, account::SIGN_OUT).id;
    // A sign-in to another backend while the telling is under way.
    let out = rig.press(Press::SignIn { address: "other.example.net".to_owned(), email: "grace@example.org".to_owned(), password: Typed::new(PASSWORD) });
    let hello = only_ask(&out).id;
    let sign_in = only_ask(&rig.answer(hello, 200, hello_body(&["accounts"]))).id;
    rig.answer(sign_in, 200, signed_in_body(1));
    assert_eq!(rig.shown().screen, Screen::SignedIn);
    // The first account's "not signed in" turns up: it settles the note, nothing else.
    let out = rig.answer(tell, 200, refused(Refusal::NotSignedIn));
    assert_eq!(forgets(&out), vec![Forget::Note]);
    assert_eq!(rig.shown().screen, Screen::SignedIn);
}

// ---------------------------------------------------------------------------------------
// A refused music login (6.3): the one rule
// ---------------------------------------------------------------------------------------

#[test]
fn a_refused_music_login_is_asked_for_once() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(kept_signed_in(4));
    let check = only_ask(&out).id;
    rig.answer(check, 200, devices_body(4, vec![device(3, true)]));
    // Ten callers holding the same login report it: the first sets it aside, once.
    let out = rig.happen(Happened::MusicRefused { count: 4 });
    assert!(matches!(handed(&out)[..], [Hand::Refused { count: 4 }]));
    let renew = only_ask(&out);
    assert!(renew.url.ends_with(account::MUSIC_LOGIN));
    let renew = renew.id;
    for _ in 0..9 {
        let out = rig.happen(Happened::MusicRefused { count: 4 });
        assert!(asks(&out).is_empty() && handed(&out).is_empty(), "{out:?}");
    }
    assert!(rig.shown().car.is_some());
    assert_eq!(rig.shown().screen, Screen::SignedIn, "a refusal never signs anyone out");
    // A newer one comes: in place in one step, and handed over.
    let out = rig.answer(renew, 200, music_body(5));
    assert_eq!(kept_saved(&out)[0].music.as_ref().map(|login| login.count), Some(5));
    assert!(matches!(handed(&out)[..], [Hand::Login(login)] if login.count == 5));
    assert!(rig.shown().car.is_none() && rig.shown().music.is_none());
    // A caller still holding the old one reports it: it is handed the new one, nothing asked.
    let out = rig.happen(Happened::MusicRefused { count: 4 });
    assert!(asks(&out).is_empty());
    assert!(matches!(handed(&out)[..], [Hand::Login(login)] if login.count == 5));
}

#[test]
fn a_refused_login_with_nothing_newer_stays_aside_until_the_network_comes_back() {
    let mut rig = Rig::new(DeviceKind::Pc);
    let out = rig.start(kept_signed_in(4));
    let check = only_ask(&out).id;
    rig.answer(check, 200, devices_body(4, vec![]));
    let renew = only_ask(&rig.happen(Happened::MusicRefused { count: 4 })).id;
    let out = rig.answer(renew, 200, music_body(4));
    assert!(handed(&out).is_empty(), "the same number is not handed again: {out:?}");
    assert!(rig.shown().music.as_deref().unwrap().contains("no newer one"));
    // Nothing renews it but the network coming back or the number moving.
    for _ in 0..5 {
        rig.now += CHECK_EVERY_MS;
        let out = rig.happen(Happened::Tick);
        assert!(!asks(&out).iter().any(|ask| ask.url.ends_with(account::MUSIC_LOGIN)));
        if let Some(check) = asks(&out).first().map(|ask| ask.id) {
            rig.answer(check, 200, devices_body(4, vec![]));
        }
    }
    let out = rig.happen(Happened::Network { up: true });
    let renew = ask_to(&out, account::MUSIC_LOGIN).id;
    let check = ask_to(&out, account::LIST_DEVICES).id;
    let out = rig.answer(renew, 200, music_body(4));
    assert!(handed(&out).is_empty());
    // The number moves (a receipt says so): one renewal, which brings it.
    let out = rig.answer(check, 200, devices_body(6, vec![]));
    let renew = ask_to(&out, account::MUSIC_LOGIN).id;
    let out = rig.answer(renew, 200, music_body(6));
    assert!(matches!(handed(&out)[..], [Hand::Login(login)] if login.count == 6));
}

#[test]
fn a_refused_login_with_the_backend_down_plays_what_is_here_and_retries_nothing() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(kept_signed_in(4));
    let check = only_ask(&out).id;
    rig.fail(check, How::Connect);
    let renew = only_ask(&rig.happen(Happened::MusicRefused { count: 4 })).id;
    let out = rig.fail(renew, How::Connect);
    assert!(asks(&out).is_empty());
    assert!(forgets(&out).is_empty());
    assert!(rig.shown().music.as_deref().unwrap().contains("when your Nexus server can be reached"));
    assert!(rig.shown().plays);
    // No loop: further reports and ticks ask nothing.
    for _ in 0..20 {
        assert!(asks(&rig.happen(Happened::MusicRefused { count: 4 })).is_empty());
        rig.now += TICK_MS;
        let out = rig.happen(Happened::Tick);
        assert!(!asks(&out).iter().any(|ask| ask.url.ends_with(account::MUSIC_LOGIN)));
    }
    // The backend may be reachable again: the owed renewal, once.
    let out = rig.happen(Happened::Network { up: true });
    let renewals: Vec<_> = asks(&out).into_iter().filter(|ask| ask.url.ends_with(account::MUSIC_LOGIN)).collect();
    assert_eq!(renewals.len(), 1);
}

#[test]
fn the_music_login_refused_mid_song_with_offline_on_is_renewed_when_it_goes_off() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let mut kept = kept_signed_in(4);
    kept.offline = true;
    rig.start(kept);
    let out = rig.happen(Happened::MusicRefused { count: 4 });
    assert!(matches!(handed(&out)[..], [Hand::Refused { count: 4 }]));
    assert!(asks(&out).is_empty());
    let out = rig.happen(Happened::Offline(false));
    assert!(asks(&out).iter().any(|ask| ask.url.ends_with(account::MUSIC_LOGIN)));
}

#[test]
fn a_moved_number_in_any_answer_is_one_renewal() {
    let mut rig = Rig::new(DeviceKind::Pc);
    let out = rig.start(kept_signed_in(2));
    let check = only_ask(&out).id;
    let out = rig.answer(check, 200, devices_body(3, vec![]));
    let renew = only_ask(&out);
    assert!(renew.url.ends_with(account::MUSIC_LOGIN));
    let renew = renew.id;
    let out = rig.answer(renew, 200, music_body(3));
    assert!(matches!(handed(&out)[..], [Hand::Login(login)] if login.count == 3));
}

#[test]
fn a_refusal_without_anyone_signed_in_asks_nothing_and_says_how_to_renew() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let mut kept = kept_signed_in(4);
    if let Some(saved) = kept.saved.as_mut() {
        saved.session = None;
    }
    rig.start(kept);
    let out = rig.happen(Happened::MusicRefused { count: 4 });
    assert!(asks(&out).is_empty());
    assert!(matches!(handed(&out)[..], [Hand::Refused { count: 4 }]));
    assert!(rig.shown().music.as_deref().unwrap().contains("Sign in"));
}

// ---------------------------------------------------------------------------------------
// The rules that hold across every sequence
// ---------------------------------------------------------------------------------------

/// Runs most of the sequences above on one rig and gives back everything it was told.
fn a_long_run() -> Rig {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(kept_signed_in(4));
    let check = only_ask(&out).id;
    rig.fail(check, How::Timeout);
    let out = rig.happen(Happened::Network { up: true });
    let check = ask_to(&out, account::LIST_DEVICES).id;
    rig.answer(check, 200, devices_body(4, vec![device(3, true), device(9, true)]));
    rig.press(Press::OpenDevices);
    let renew = only_ask(&rig.happen(Happened::MusicRefused { count: 4 })).id;
    rig.answer(renew, 302, Vec::new());
    let out = rig.happen(Happened::Network { up: true });
    for ask in asks(&out).iter().map(|ask| ask.id).collect::<Vec<_>>() {
        rig.answer(ask, 200, music_body(5));
    }
    rig.happen(Happened::Offline(true));
    rig.happen(Happened::Offline(false));
    let out = rig.press(Press::ForceOut { device: 9 });
    if let Some(id) = asks(&out).first().map(|ask| ask.id) {
        rig.answer(id, 200, encoded(Reply::Done { result: ForcedOut { music_cut_off: false }, receipt: receipt(5) }));
    }
    rig.press(Press::ChangePassword { current: Typed::new(PASSWORD), new: Typed::new("another long password"), again: Typed::new("another long password") });
    rig.press(Press::SignOut);
    rig.sign_in(6);
    rig.happen(Happened::Tick);
    rig
}

#[test]
fn the_machine_never_holds_the_player_back() {
    let rig = a_long_run();
    // The closed set has no such thing; what touches the player is a login handed over, a
    // number set aside after a report, and forgetting at a sign-out or an ending.
    for line in &rig.log {
        let thing: serde_json::Value = serde_json::from_str(line).unwrap();
        let kind = thing.as_object().and_then(|object| object.keys().next().cloned()).unwrap();
        assert!(["ask", "keep", "forget", "show", "hand"].contains(&kind.as_str()), "{line}");
        if let Some(hand) = thing.get("hand") {
            assert!(hand.get("login").is_some() || hand.get("refused").is_some(), "{line}");
        }
    }
    let refused = rig.log.iter().filter(|line| line.contains("\"refused\":{\"count\"")).count();
    assert_eq!(refused, 1, "set aside once, on the one report");
}

#[test]
fn the_session_goes_to_the_backend_alone_and_the_music_login_to_the_player_alone() {
    let rig = a_long_run();
    let token4 = hex(&token(4));
    let token5 = hex(&token(5));
    for line in &rig.log {
        let thing: serde_json::Value = serde_json::from_str(line).unwrap();
        if let Some(ask) = thing.get("ask") {
            let url = ask["url"].as_str().unwrap();
            assert!(url.starts_with("https://nexus.example.org/nexus/api/"), "{url}");
            assert!(!url.contains("music.example.org"));
            for secret in [&token4, &token5, &"s4ltS4LTs4ltS4LT".to_owned()] {
                assert!(!line.contains(secret.as_str()), "the music login is in an ask: {line}");
            }
            if let Some(authorization) = ask["authorization"].as_str() {
                assert!(authorization.starts_with("Bearer "));
            }
        }
        if let Some(hand) = thing.get("hand") {
            assert!(!hand.to_string().contains(&hex(&SESSION)), "the session is handed to the player: {line}");
        }
    }
}

#[test]
fn every_ask_is_one_request_with_its_own_number() {
    let rig = a_long_run();
    let mut ids = Vec::new();
    for line in &rig.log {
        let thing: serde_json::Value = serde_json::from_str(line).unwrap();
        if let Some(ask) = thing.get("ask") {
            ids.push(ask["id"].as_u64().unwrap());
        }
    }
    let mut unique = ids.clone();
    unique.dedup();
    assert_eq!(ids, unique);
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn no_secret_is_shown_by_debug() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let start = rig.start(kept_signed_in(4));
    let mut debug = format!("{start:?}{:?}", kept_signed_in(4));
    let press = Press::SignIn { address: NEXUS.to_owned(), email: EMAIL.to_owned(), password: Typed::new(PASSWORD) };
    debug.push_str(&format!("{press:?}"));
    rig.press(Press::SignOut);
    let out = rig.sign_in(4);
    debug.push_str(&format!("{out:?}{:?}{:?}", rig.client, Random::new([0x42; 32])));
    let body = AnswerBody::new(signed_in_body(4));
    debug.push_str(&format!("{body:?}"));
    for secret in [hex(&SESSION), hex(&token(4)), "s4ltS4LTs4ltS4LT".to_owned(), PASSWORD.to_owned(), hex(&[0xd5; 32]), hex(&[0x42; 32])] {
        assert!(!debug.contains(&secret), "{secret} in {debug}");
    }
    assert!(!debug.contains("94, 94, 94"), "session bytes in {debug}");
}

#[test]
fn whatever_an_answer_holds_nothing_panics() {
    let bodies: Vec<Vec<u8>> = vec![
        Vec::new(),
        b"{".to_vec(),
        vec![0xff, 0xfe],
        b"{\"done\":{}}".to_vec(),
        b"{\"done\":{\"result\":{},\"receipt\":{}}}".to_vec(),
        "[".repeat(10_000).into_bytes(),
        b"null".to_vec(),
        refused(Refusal::BadRequest),
        refused(Refusal::Incompatible { speaks: Speaks::new(7, 9).unwrap() }),
    ];
    for status in [200, 204, 301, 401, 500] {
        for body in &bodies {
            let mut rig = Rig::new(DeviceKind::Phone);
            let out = rig.start(kept_signed_in(4));
            let check = only_ask(&out).id;
            rig.answer(check, status, body.clone());
            let out = rig.happen(Happened::MusicRefused { count: 4 });
            for ask in asks(&out).iter().map(|ask| ask.id).collect::<Vec<_>>() {
                rig.answer(ask, status, body.clone());
            }
            rig.press(Press::SignOut);
            let hello = asks(&rig.sign_in_press()).first().map(|ask| ask.id);
            if let Some(hello) = hello {
                rig.answer(hello, status, body.clone());
            }
        }
    }
}

#[test]
fn a_backend_that_speaks_no_number_of_ours_is_asked_nothing_more() {
    let mut rig = Rig::new(DeviceKind::Pc);
    let out = rig.start(kept_signed_in(4));
    let check = only_ask(&out).id;
    rig.answer(check, 200, refused(Refusal::Incompatible { speaks: Speaks::new(7, 9).unwrap() }));
    assert!(rig.shown().row.contains("too old"));
    rig.now += CHECK_EVERY_MS;
    assert!(asks(&rig.happen(Happened::Tick)).is_empty());
    assert!(asks(&rig.happen(Happened::Network { up: true })).is_empty());
    assert_eq!(rig.shown().screen, Screen::SignedIn);
}

// ---------------------------------------------------------------------------------------
// Devices and the password
// ---------------------------------------------------------------------------------------

#[test]
fn the_devices_list_and_forcing_one_out() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(kept_signed_in(4));
    let check = only_ask(&out).id;
    // Opened while the check is under way: the check's answer fills it, no second ask.
    assert!(asks(&rig.press(Press::OpenDevices)).is_empty());
    assert_eq!(rig.shown().devices.state, ListState::Working);
    rig.answer(check, 200, devices_body(4, vec![device(3, true), device(9, true), device(12, false)]));
    let rows = &rig.shown().devices.rows;
    assert_eq!(rows.len(), 3);
    assert!(rows[0].this_one && !rows[0].can_force_out && rows[0].seen == "This device");
    assert!(rows[1].can_force_out && rows[1].seen == "Used yesterday");
    assert!(!rows[2].can_force_out && rows[2].seen == "Signed out");
    // One that is signed out, or unknown, is not forced out.
    assert!(asks(&rig.press(Press::ForceOut { device: 12 })).is_empty());
    assert!(asks(&rig.press(Press::ForceOut { device: 77 })).is_empty());
    let out = rig.press(Press::ForceOut { device: 9 });
    let force = only_ask(&out);
    assert!(force.url.ends_with(account::FORCE_OUT));
    let force = force.id;
    assert!(asks(&rig.press(Press::ForceOut { device: 9 })).is_empty(), "pressed twice: one request");
    rig.answer(force, 200, encoded(Reply::Done { result: ForcedOut { music_cut_off: false }, receipt: receipt(4) }));
    let shown = rig.shown();
    assert!(!shown.devices.rows[1].can_force_out);
    assert!(shown.devices.words.contains("change that too"));
    // Forcing out this device is signing out here.
    let out = rig.press(Press::ForceOut { device: 3 });
    assert_eq!(forgets(&out), vec![Forget::Saved]);
}

#[test]
fn changing_the_password() {
    let mut rig = Rig::new(DeviceKind::Pc);
    let out = rig.start(kept_signed_in(4));
    let check = only_ask(&out).id;
    rig.answer(check, 200, devices_body(4, vec![]));
    let change = |current: &str, new: &str, again: &str| Press::ChangePassword { current: Typed::new(current), new: Typed::new(new), again: Typed::new(again) };
    assert!(asks(&rig.press(change(PASSWORD, "another long password", "another long passwor"))).is_empty());
    assert_eq!(rig.shown().password.state, PasswordState::Differ);
    assert!(asks(&rig.press(change(PASSWORD, "short", "short"))).is_empty());
    assert_eq!(rig.shown().password.state, PasswordState::NotAllowed);
    assert!(asks(&rig.press(change(PASSWORD, EMAIL, EMAIL))).is_empty(), "not the email");
    let out = rig.press(change(PASSWORD, "another long password", "another long password"));
    let ask = only_ask(&out);
    assert!(ask.url.ends_with(account::CHANGE_PASSWORD));
    assert!(ask.body.as_ref().unwrap().expose().contains("another long password"));
    let ask = ask.id;
    rig.answer(ask, 200, refused(Refusal::WrongLogin));
    assert_eq!(rig.shown().password.state, PasswordState::WrongCurrent);
    let ask = only_ask(&rig.press(change(PASSWORD, "another long password", "another long password"))).id;
    rig.answer(ask, 200, encoded(Reply::Done { result: PasswordChanged {}, receipt: receipt(4) }));
    assert_eq!(rig.shown().password.state, PasswordState::Changed);
    assert!(rig.shown().password.words.contains("other devices are signed out"));
    assert_eq!(rig.shown().screen, Screen::SignedIn);
}

// ---------------------------------------------------------------------------------------
// The phone's entry point
// ---------------------------------------------------------------------------------------

#[test]
fn the_json_entry_point_makes_the_same_calls() {
    let mut client = Client::default();
    let mut saved = serde_json::to_value(saved(4)).unwrap();
    saved["session"] = serde_json::Value::String(hex(&SESSION));
    let start = serde_json::json!({"start": {
        "app": {"version": "0.41.0", "kind": "phone", "device_name": "Ada's phone"},
        "kept": {"device_secret": hex(&[0xd5; 32]), "saved": saved, "note": null, "offline": false},
        "given": {"now_ms": START_MS, "local_offset_minutes": 600, "random": hex(&[0x42; 32])}
    }});
    let out: serde_json::Value = serde_json::from_slice(&client.call_json(start.to_string().as_bytes())).unwrap();
    let things = out["do"].as_array().unwrap();
    assert_eq!(things[0]["hand"]["login"]["count"], 4);
    let ask = things.iter().find_map(|thing| thing.get("ask")).unwrap();
    assert_eq!(ask["method"], "POST");
    assert_eq!(ask["generation"], out["generation"]);
    let id = ask["id"].as_u64().unwrap();
    let answer = serde_json::json!({"happened": {
        "event": {"answered": {"ask": id, "status": 200, "body": String::from_utf8(refused(Refusal::NotSignedIn)).unwrap()}},
        "given": {"now_ms": START_MS + 1000}
    }});
    let out: serde_json::Value = serde_json::from_slice(&client.call_json(answer.to_string().as_bytes())).unwrap();
    assert!(out["do"].as_array().unwrap().iter().any(|thing| thing["forget"] == "saved"));
    let press = serde_json::json!({"happened": {"event": {"pressed": {"sign_in": {"address": NEXUS, "email": EMAIL, "password": PASSWORD}}}, "given": {"now_ms": START_MS}}});
    let out: serde_json::Value = serde_json::from_slice(&client.call_json(press.to_string().as_bytes())).unwrap();
    assert!(out["do"].as_array().unwrap().iter().any(|thing| thing.get("ask").is_some()));
    let out: serde_json::Value = serde_json::from_slice(&client.call_json(br#"{"stop":{}}"#)).unwrap();
    assert_eq!(out["generation"], client.generation());
    assert_eq!(out["do"], serde_json::json!([]));
    // After stop, an answer is nothing.
    let out: serde_json::Value = serde_json::from_slice(&client.call_json(answer.to_string().as_bytes())).unwrap();
    assert_eq!(out["do"], serde_json::json!([]));
}

#[test]
fn input_the_entry_point_cannot_read_is_one_fixed_error() {
    let mut client = Client::default();
    for input in [&b""[..], b"{", br#"{"happened":{"event":{"pressed":{"sign_in":{"address":"x","email":"y","password":"hunter2hunter2"}}}}}"#, br#"{"start":{}}"#] {
        assert_eq!(client.call_json(input), br#"{"error":"input"}"#.to_vec());
    }
}

// ---------------------------------------------------------------------------------------
// The sending half: owed (C8's events, C6's Diagnostics)
// ---------------------------------------------------------------------------------------

#[test]
#[ignore = "owed: C8. Events waiting are sent from the tick, 500 to a request, each answered by itself (13.3)"]
fn events_waiting_are_sent_in_batches_answered_one_by_one() {}

#[test]
#[ignore = "owed: C8. On \"the server's data was put back\" every event still held is sent again (13.3)"]
fn start_again_sends_every_event_still_held() {}

#[test]
#[ignore = "owed: C8. Nothing written under an earlier life of the account is sent (12.5)"]
fn events_of_an_earlier_life_are_not_sent() {}

#[test]
#[ignore = "owed: C6. Diagnostics: a few minutes after a fault, else daily; never within ten minutes; thirty a day; 64 KiB (14.4)"]
fn diagnostics_are_sent_at_their_own_pace() {}

#[test]
#[ignore = "owed: C8 and C6. Offline sends nothing; an account switch sends nothing of the old account's"]
fn the_sending_half_keeps_the_fence() {}
