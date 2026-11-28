//! The specification: every sequence of section 6 (and the slice notes' C10), fed in as
//! events, with the things to do checked; and each finding of the review of `b620845`, as
//! the sequence that showed it. Made-up names and addresses only.

use super::*;
use crate::nexus::account::{Device, MusicSalt, MusicToken, Notice, Person, RefusedWithReceipt, SignOutSecret, SignedIn, Stamp, StampTag, Wait};
use crate::nexus::proto::Hello;

const NEXUS: &str = "https://nexus.example.org";
const MUSIC: &str = "https://music.example.org/navidrome";
const EMAIL: &str = "ada@example.org";
const PASSWORD: &str = "a long enough password";
const SESSION: [u8; 32] = [0x5e; 32];
const SIGN_OUT_SECRET: [u8; 32] = [0x50; 32];
const SALT: [u8; 32] = [0x51; 32];
const START_MS: u64 = 1_800_000_000_000;
const DAY: u64 = 24 * 60 * 60 * 1000;
const ACCOUNT: u64 = 21;
const SESSION_ID: u64 = 70;

fn backend() -> BackendId {
    BackendId::from_bytes([7; 16])
}

fn stranger() -> BackendId {
    BackendId::from_bytes([9; 16])
}

fn account() -> AccountId {
    AccountId::new(ACCOUNT).unwrap()
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
        salt: MusicSalt::from_random(SALT),
        token: MusicToken::from_bytes(token(count)),
        count,
    }
}

fn receipt_full(backend: BackendId, who: AccountId, restore: u64, life: u64, count: u64) -> Receipt {
    Receipt { stamp: Stamp { backend, restore, account: who, life, change: 1, tag: StampTag::from_bytes([1; 32]) }, music_login_count: count }
}

fn receipt(count: u64) -> Receipt {
    receipt_full(backend(), account(), 1, 1, count)
}

fn person() -> Person {
    Person { id: account(), revision: 1, email: Email::new(EMAIL).unwrap(), name: Name::new("Ada").unwrap(), admin: false }
}

fn revoke() -> SignOutNote {
    SignOutNote { session: SessionId::new(SESSION_ID).unwrap(), secret: SignOutSecret::from_bytes(SIGN_OUT_SECRET) }
}

fn hello_from(id: BackendId, has: &[&str]) -> (u16, Vec<u8>) {
    (200, Hello::new(Version { major: 0, minor: 2, patch: 0 }, id, SPEAKS, has).unwrap().to_json().into_bytes())
}

fn hello() -> (u16, Vec<u8>) {
    hello_from(backend(), &["accounts"])
}

/// A reply, with the status account gives its kind.
fn reply<T: Serialize>(reply: Reply<T>) -> (u16, Vec<u8>) {
    (reply.status(), serde_json::to_vec(&reply).unwrap())
}

fn done<T: Serialize>(result: T, receipt: Receipt) -> (u16, Vec<u8>) {
    reply(Reply::Done { result, receipt })
}

fn refused(refusal: Refusal) -> (u16, Vec<u8>) {
    reply(Reply::<SignedOut>::Refused(refusal))
}

fn refused_stamped(refusal: Refusal, receipt: Receipt) -> (u16, Vec<u8>) {
    reply(Reply::<SignedOut>::RefusedWithReceipt(RefusedWithReceipt::new(refusal, receipt).unwrap()))
}

fn session_ended() -> (u16, Vec<u8>) {
    refused(Refusal::NotSignedIn(NotSignedIn::SessionEnded))
}

fn signed_in_as(count: u64, music: MusicLoginResult) -> (u16, Vec<u8>) {
    done(
        SignedIn {
            session: Session::from_bytes(SESSION),
            session_id: SessionId::new(SESSION_ID).unwrap(),
            sign_out: revoke(),
            person: person(),
            device: DeviceId::new(3).unwrap(),
            restore: 1,
            music,
            notice: Notice::new("What you play is recorded.").unwrap(),
        },
        receipt(count),
    )
}

fn signed_in_body(count: u64) -> (u16, Vec<u8>) {
    signed_in_as(count, MusicLoginResult::Login(login(count)))
}

fn music_body(count: u64) -> (u16, Vec<u8>) {
    done(MusicLoginResult::Login(login(count)), receipt(count))
}

fn devices_body(count: u64, devices: Vec<Device>) -> (u16, Vec<u8>) {
    done(DevicePage::new(devices, false).unwrap(), receipt(count))
}

fn device(id: u64, session: Option<u64>, this_device: bool) -> Device {
    Device {
        id: DeviceId::new(id).unwrap(),
        name: Name::new(&format!("Device {id}")).unwrap(),
        kind: DeviceKind::Pc,
        app_version: Version { major: 0, minor: 41, patch: 0 },
        last_seen_ms: START_MS - DAY,
        session: session.and_then(SessionId::new),
        this_device,
    }
}

fn this_device() -> Device {
    device(3, Some(SESSION_ID), true)
}

fn saved(count: u64) -> Saved {
    Saved {
        nexus: NEXUS.to_owned(),
        backend_id: backend().to_hex(),
        account: account(),
        email: EMAIL.to_owned(),
        name: "Ada".to_owned(),
        device: 3,
        restore: 1,
        life: 1,
        change: 1,
        notice: None,
        session: Some(Session::from_bytes(SESSION)),
        sign_out: Some(revoke()),
        music: Some(login(count)),
    }
}

fn kept_signed_in(count: u64) -> Kept {
    Kept { device_secret: Some(DeviceSecret::from_bytes([0xd5; 32])), saved: Some(saved(count)), notes: Vec::new(), offline: false }
}

/// A device under test: the machine, the clock, and every thing it was told to do.
struct Rig {
    client: Client,
    kind: DeviceKind,
    now: u64,
    /// Everything ever given back, as JSON, in order.
    log: Vec<String>,
    shown: Option<Shown>,
    /// The generation the player was last handed a login under.
    handed: u64,
}

impl Rig {
    fn new(kind: DeviceKind) -> Rig {
        Rig { client: Client::default(), kind, now: START_MS, log: Vec::new(), shown: None, handed: 0 }
    }

    fn given(&self) -> Given {
        Given { now_ms: self.now, local_offset_minutes: 600, random: Some(Random::new([0x42; 32])) }
    }

    fn take(&mut self, out: Vec<Do>) -> Vec<Do> {
        for thing in &out {
            self.log.push(serde_json::to_string(thing).unwrap());
            match thing {
                Do::Show(shown) => self.shown = Some(shown.clone()),
                Do::Hand(Hand::Login { generation, .. }) => self.handed = *generation,
                _ => {}
            }
        }
        out
    }

    fn start(&mut self, kept: Kept) -> Vec<Do> {
        let given = self.given();
        let out = self.client.start(app(self.kind), kept, &given);
        self.take(out)
    }

    /// Starts, and answers the `hello` asked before anything secret: gives back what that
    /// answer brought (the session's check, renewals, notes).
    fn start_verified(&mut self, kept: Kept) -> Vec<Do> {
        let out = self.start(kept);
        let verify = ask_to(&out, proto::HELLO).id;
        self.reply(verify, hello())
    }

    fn happen(&mut self, event: Happened) -> Vec<Do> {
        let given = self.given();
        let out = self.client.happened(event, &given);
        self.take(out)
    }

    fn press(&mut self, press: Press) -> Vec<Do> {
        self.happen(Happened::Pressed(press))
    }

    fn reply(&mut self, ask: u64, (status, body): (u16, Vec<u8>)) -> Vec<Do> {
        self.happen(Happened::Answered { ask, status, body: AnswerBody::new(body) })
    }

    fn fail(&mut self, ask: u64, how: How) -> Vec<Do> {
        self.happen(Happened::Failed { ask, failure: Failure { how, inside: None, permission: None } })
    }

    fn refuse(&mut self, count: u64) -> Vec<Do> {
        let generation = self.handed;
        self.happen(Happened::MusicRefused { count, generation })
    }

    fn sign_in_press(&mut self) -> Vec<Do> {
        self.press(sign_in("nexus.example.org", EMAIL))
    }

    /// A whole sign-in, answered: gives back the last answer's things to do.
    fn sign_in(&mut self, count: u64) -> Vec<Do> {
        let out = self.sign_in_press();
        let hello_ask = only_ask(&out).id;
        let out = self.reply(hello_ask, hello());
        let sign_in = only_ask(&out).id;
        self.reply(sign_in, signed_in_body(count))
    }

    fn shown(&self) -> &Shown {
        self.shown.as_ref().unwrap()
    }
}

fn sign_in(address: &str, email: &str) -> Press {
    Press::SignIn { address: address.to_owned(), email: email.to_owned(), password: Typed::new(PASSWORD), look: Look::default(), retry: false }
}

fn retry_press() -> Press {
    Press::SignIn { address: NEXUS.to_owned(), email: EMAIL.to_owned(), password: Typed::new(PASSWORD), look: Look::default(), retry: true }
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

fn asks_to(out: &[Do], path: &str) -> usize {
    asks(out).iter().filter(|ask| ask.url.ends_with(path)).count()
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

fn kept_note(out: &[Do]) -> Option<Note> {
    out.iter().find_map(|thing| match thing {
        Do::Keep(Keep::Note(note)) => Some(note.copy()),
        _ => None,
    })
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn body_of(ask: &Ask) -> serde_json::Value {
    serde_json::from_str(ask.body.as_ref().unwrap().expose()).unwrap()
}

/// Signed in, the session's check answered at this count.
fn running(count: u64) -> Rig {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start_verified(kept_signed_in(count));
    let check = only_ask(&out).id;
    rig.reply(check, devices_body(count, vec![this_device()]));
    rig
}

// ---------------------------------------------------------------------------------------
// Starting: playing never waits (R1, 6.1); the backend checked before anything secret (7.1)
// ---------------------------------------------------------------------------------------

#[test]
fn cold_start_plays_from_what_sign_in_left() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(kept_signed_in(4));
    // The very first thing: the player is handed the kept login, before anything is asked.
    assert!(matches!(&out[0], Do::Hand(Hand::Login { login, .. }) if login.count == 4), "{out:?}");
    // And the one thing asked is hello: nothing that carries the session yet.
    let verify = only_ask(&out);
    assert_eq!(verify.url, "https://nexus.example.org/nexus/api/hello");
    assert!(verify.authorization.is_none() && verify.body.is_none());
    assert_eq!(rig.shown().screen, Screen::SignedIn);
    assert!(rig.shown().plays);
    let verify = verify.id;
    let out = rig.reply(verify, hello());
    let check = only_ask(&out);
    assert!(check.url.ends_with(account::LIST_DEVICES));
    assert!(check.authorization.is_some());
}

#[test]
fn a_different_backend_at_the_remembered_address_is_sent_nothing_secret() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(kept_signed_in(4));
    let verify = only_ask(&out).id;
    let out = rig.reply(verify, hello_from(stranger(), &["accounts"]));
    assert!(asks(&out).is_empty(), "no session goes to it: {out:?}");
    assert!(forgets(&out).is_empty() && kept_saved(&out).is_empty(), "nothing is forgotten");
    assert!(handed(&out).is_empty(), "the music login goes on playing");
    assert_eq!(rig.shown().screen, Screen::SignIn);
    assert_eq!(rig.shown().sign_in.state, SignInState::StartAgain);
    assert!(rig.shown().plays);
    assert!(asks(&rig.refuse(4)).is_empty());
    assert!(asks(&rig.happen(Happened::Network { up: true })).is_empty());
    rig.now += DAY;
    assert!(asks(&rig.happen(Happened::Tick)).is_empty());
    assert!(!rig.log.iter().any(|line| line.contains("\"forget\"")));
}

#[test]
fn a_sign_in_to_the_remembered_address_now_another_backend_is_a_sign_in_to_a_new_one() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let mut kept = kept_signed_in(4);
    if let Some(saved) = kept.saved.as_mut() {
        saved.session = None;
        saved.sign_out = None;
    }
    rig.start(kept);
    let hello_ask = only_ask(&rig.sign_in_press()).id;
    let out = rig.reply(hello_ask, hello_from(stranger(), &["accounts"]));
    let sign_in = only_ask(&out);
    assert_eq!(body_of(sign_in)["restore"], serde_json::Value::Null);
    assert!(kept_saved(&out).is_empty());
}

#[test]
fn a_cold_start_with_the_backend_down_keeps_everything_and_says_since_when() {
    let mut rig = Rig::new(DeviceKind::Pc);
    let out = rig.start(kept_signed_in(4));
    let verify = only_ask(&out).id;
    let out = rig.fail(verify, How::Timeout);
    assert!(forgets(&out).is_empty());
    assert!(handed(&out).is_empty());
    // 1_800_000_000_000 ms is 08:00 UTC; ten hours east is 18:00.
    assert_eq!(rig.shown().row, "Nexus server: not reachable since 18:00. Your music plays as usual.");
    assert_eq!(rig.shown().screen, Screen::SignedIn);
    for _ in 0..10 {
        rig.now += 6 * 60 * 60 * 1000;
        assert!(asks(&rig.happen(Happened::Tick)).is_empty());
    }
}

#[test]
fn a_tick_alone_asks_nothing_of_the_session() {
    let mut rig = running(4);
    for _ in 0..30 {
        rig.now += 6 * 60 * 60 * 1000;
        assert!(asks(&rig.happen(Happened::Tick)).is_empty());
    }
}

#[test]
fn a_cold_start_with_offline_on_hands_the_login_and_asks_nothing() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let mut kept = kept_signed_in(4);
    kept.offline = true;
    let out = rig.start(kept);
    assert!(matches!(&out[0], Do::Hand(Hand::Login { .. })));
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
    let out = rig.client.happened(Happened::Pressed(sign_in(NEXUS, EMAIL)), &given);
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
    let hello_ask = only_ask(&out);
    assert_eq!(hello_ask.url, "https://nexus.example.org/nexus/api/hello");
    assert_eq!(hello_ask.method, Method::Get);
    assert!(hello_ask.authorization.is_none() && hello_ask.body.is_none());
    assert_eq!(hello_ask.max_body, proto::HELLO_MAX_BYTES);
    assert_eq!(rig.shown().sign_in.state, SignInState::Working);
    let hello_ask = hello_ask.id;

    let out = rig.reply(hello_ask, hello());
    let sign_in = only_ask(&out);
    assert_eq!(sign_in.url, "https://nexus.example.org/nexus/api/signin");
    assert_eq!(sign_in.method, Method::Post);
    assert!(sign_in.authorization.is_none());
    assert_eq!(sign_in.within_ms, SIGN_IN_WITHIN_MS);
    assert_eq!(sign_in.max_body, account::SIGN_IN_REPLY_MAX_BYTES);
    let body = body_of(sign_in);
    assert_eq!(body["credential"]["password"]["email"], EMAIL);
    assert_eq!(body["device_secret"], hex(&[0x42; 32]));
    assert_eq!(body["restore"], serde_json::Value::Null);
    let sign_in = sign_in.id;

    let out = rig.reply(sign_in, signed_in_body(2));
    let kept = kept_saved(&out);
    assert_eq!(kept.len(), 1, "kept as one thing: {out:?}");
    assert_eq!(kept[0].nexus, NEXUS);
    assert_eq!(kept[0].backend_id, backend().to_hex());
    assert_eq!(kept[0].account, account());
    assert_eq!(kept[0].session.as_ref().map(|session| *session.expose_bytes()), Some(SESSION));
    assert_eq!(kept[0].sign_out.as_ref().map(|note| note.session.get()), Some(SESSION_ID));
    assert_eq!(kept[0].music.as_ref().map(|login| login.count), Some(2));
    let keep_at = out.iter().position(|thing| matches!(thing, Do::Keep(Keep::Saved(_)))).unwrap();
    let hand_at = out.iter().position(|thing| matches!(thing, Do::Hand(Hand::Login { .. }))).unwrap();
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
fn signed_in_with_no_music_login_says_which_and_stays_signed_in() {
    for (music, words) in [(MusicLoginResult::NoneSetYet, "no music sign-in for you yet"), (MusicLoginResult::UnusableNow, "can't be used just now")] {
        let mut rig = Rig::new(DeviceKind::Pc);
        rig.start(Kept::default());
        let hello_ask = only_ask(&rig.sign_in_press()).id;
        let sign_in = only_ask(&rig.reply(hello_ask, hello())).id;
        let out = rig.reply(sign_in, signed_in_as(0, music));
        assert!(kept_saved(&out)[0].music.is_none());
        assert!(handed(&out).is_empty());
        assert_eq!(rig.shown().screen, Screen::SignedIn);
        assert!(rig.shown().music.as_deref().unwrap().contains(words));
    }
}

#[test]
fn what_may_be_typed_as_the_address() {
    for (typed, state) in [
        ("http://nexus.example.org", SignInState::NeedsHttps),
        ("HTTP://nexus.example.org", SignInState::NeedsHttps),
        ("", SignInState::BadAddress),
        ("https://nexus.example.org/music", SignInState::BadAddress),
        ("https://nexus.example.org?x=1", SignInState::BadAddress),
        ("https://ada:pw@nexus.example.org", SignInState::BadAddress),
        ("https://nexus example.org", SignInState::BadAddress),
    ] {
        let mut rig = Rig::new(DeviceKind::Pc);
        rig.start(Kept::default());
        let out = rig.press(sign_in(typed, EMAIL));
        assert!(asks(&out).is_empty(), "{typed}");
        assert_eq!(rig.shown().sign_in.state, state, "{typed}");
    }
    for typed in ["nexus.example.org", "https://nexus.example.org/", " https://Nexus.Example.org ", "nexus.example.org:8443"] {
        let mut rig = Rig::new(DeviceKind::Pc);
        rig.start(Kept::default());
        let out = rig.press(sign_in(typed, EMAIL));
        assert!(only_ask(&out).url.starts_with("https://nexus.example.org"), "{typed}");
        assert!(only_ask(&out).url.ends_with("/nexus/api/hello"), "{typed}");
    }
}

#[test]
fn an_email_and_a_password_are_held_to_account_rules_before_anything_is_sent() {
    let mut rig = Rig::new(DeviceKind::Pc);
    rig.start(Kept::default());
    assert!(asks(&rig.press(sign_in(NEXUS, "not an email"))).is_empty());
    assert_eq!(rig.shown().sign_in.state, SignInState::BadEmail);
    let out = rig.press(Press::SignIn { address: NEXUS.to_owned(), email: EMAIL.to_owned(), password: Typed::new(""), look: Look::default(), retry: false });
    assert!(asks(&out).is_empty());
    assert_eq!(rig.shown().sign_in.state, SignInState::NoPassword);
    // A password typed to sign in is presented as it is, of any length: the backend judges it.
    let out = rig.press(Press::SignIn { address: NEXUS.to_owned(), email: EMAIL.to_owned(), password: Typed::new("short"), look: Look::default(), retry: false });
    assert_eq!(asks_to(&out, proto::HELLO), 1);
    rig.press(Press::FormClosed);
    let mut rig = Rig::new(DeviceKind::Pc);
    rig.start(Kept::default());
    let long = "a".repeat(TYPED_MAX_BYTES + 1);
    assert!(asks(&rig.press(sign_in(&long, EMAIL))).is_empty());
    assert!(rig.shown().sign_in.address.len() <= TYPED_MAX_BYTES);
    assert!(asks(&rig.press(sign_in(NEXUS, &long))).is_empty());
    assert!(rig.shown().sign_in.email.len() <= TYPED_MAX_BYTES);
}

#[test]
fn what_is_at_the_address_if_not_a_backend_that_has_accounts() {
    let cases: Vec<((u16, Vec<u8>), SignInState)> = vec![
        ((302, Vec::new()), SignInState::NoNexusServer),
        ((404, b"not found".to_vec()), SignInState::NoNexusServer),
        ((200, b"<html>Navidrome</html>".to_vec()), SignInState::NoNexusServer),
        (hello_from(backend(), &["plays"]), SignInState::ServerTooOld),
    ];
    for (answer, state) in cases {
        let mut rig = Rig::new(DeviceKind::Pc);
        rig.start(Kept::default());
        let hello_ask = only_ask(&rig.sign_in_press()).id;
        let out = rig.reply(hello_ask, answer);
        assert!(asks(&out).is_empty());
        assert_eq!(rig.shown().sign_in.state, state);
    }
    let mut rig = Rig::new(DeviceKind::Pc);
    rig.start(Kept::default());
    let hello_ask = only_ask(&rig.sign_in_press()).id;
    let too_new = Hello::new(Version { major: 9, minor: 0, patch: 0 }, backend(), Speaks::new(5, 6).unwrap(), &["accounts"]).unwrap();
    rig.reply(hello_ask, (200, too_new.to_json().into_bytes()));
    assert_eq!(rig.shown().sign_in.state, SignInState::AppTooOld);
    assert!(rig.shown().sign_in.words.contains("too old for your Nexus server"));
}

#[test]
fn each_refusal_of_a_sign_in_is_its_own_sentence() {
    let cases = [
        (Refusal::NotAllowed(NotAllowed::WrongLogin), SignInState::WrongLogin, "Wrong email or password."),
        (Refusal::NotAllowed(NotAllowed::AccountOff), SignInState::AccountOff, "switched off"),
        (Refusal::Busy { retry_after_seconds: None }, SignInState::Busy, "in a moment"),
        (Refusal::Busy { retry_after_seconds: Wait::new(8) }, SignInState::Busy, "in 8 seconds"),
    ];
    for (refusal, state, words) in cases {
        let mut rig = Rig::new(DeviceKind::Pc);
        rig.start(Kept::default());
        let hello_ask = only_ask(&rig.sign_in_press()).id;
        let sign_in = only_ask(&rig.reply(hello_ask, hello())).id;
        let out = rig.reply(sign_in, refused(refusal));
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
fn come_back_in_n_seconds_is_one_retry_by_the_form_and_no_more() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let hello_ask = only_ask(&rig.sign_in_press()).id;
    let first = only_ask(&rig.reply(hello_ask, hello())).id;
    rig.reply(first, refused(Refusal::Busy { retry_after_seconds: Wait::new(5) }));
    assert_eq!(rig.shown().sign_in.wait_s, Some(5));
    assert!(!rig.shown().sign_in.send_again);
    // Too soon: nothing. A retry press before it is allowed: nothing.
    rig.now += 4_000;
    rig.happen(Happened::WaitPassed);
    assert!(!rig.shown().sign_in.send_again);
    assert!(asks(&rig.press(retry_press())).is_empty());
    // The wait passed: the form may send once more, and does.
    rig.now += 1_000;
    rig.happen(Happened::WaitPassed);
    assert!(rig.shown().sign_in.send_again);
    let hello_ask = only_ask(&rig.press(retry_press())).id;
    let second = only_ask(&rig.reply(hello_ask, hello())).id;
    // A second wait is the person's: shown, with no retry armed.
    rig.reply(second, refused(Refusal::Busy { retry_after_seconds: Wait::new(5) }));
    assert_eq!(rig.shown().sign_in.wait_s, None);
    assert!(rig.shown().sign_in.words.contains("in 5 seconds"));
    rig.now += 10_000;
    rig.happen(Happened::WaitPassed);
    assert!(!rig.shown().sign_in.send_again);
    assert!(asks(&rig.press(retry_press())).is_empty());
}

#[test]
fn a_waiting_retry_is_cancelled_by_closing_the_form_offline_stop_or_a_new_press() {
    fn busy(rig: &mut Rig) {
        rig.start(Kept::default());
        let hello_ask = only_ask(&rig.sign_in_press()).id;
        let sign_in = only_ask(&rig.reply(hello_ask, hello())).id;
        rig.reply(sign_in, refused(Refusal::Busy { retry_after_seconds: Wait::new(2) }));
        assert_eq!(rig.shown().sign_in.wait_s, Some(2));
        rig.now += 2_000;
    }
    fn retried(rig: &mut Rig) -> bool {
        rig.happen(Happened::WaitPassed);
        rig.shown.as_ref().is_some_and(|shown| shown.sign_in.send_again) || !asks(&rig.press(retry_press())).is_empty()
    }
    let mut rig = Rig::new(DeviceKind::Phone);
    busy(&mut rig);
    rig.press(Press::FormClosed);
    assert!(!retried(&mut rig));
    let mut rig = Rig::new(DeviceKind::Phone);
    busy(&mut rig);
    rig.happen(Happened::Offline(true));
    rig.happen(Happened::Offline(false));
    assert!(!retried(&mut rig));
    let mut rig = Rig::new(DeviceKind::Phone);
    busy(&mut rig);
    rig.client.stop();
    rig.start(Kept::default());
    assert!(!retried(&mut rig));
    let mut rig = Rig::new(DeviceKind::Phone);
    busy(&mut rig);
    rig.press(sign_in(NEXUS, "not an email"));
    assert!(!retried(&mut rig));
}

#[test]
fn two_sign_ins_pressed_quickly_are_one_try() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let first = rig.sign_in_press();
    assert_eq!(asks(&first).len(), 1);
    assert!(asks(&rig.sign_in_press()).is_empty());
    let hello_ask = only_ask(&first).id;
    let out = rig.reply(hello_ask, hello());
    assert_eq!(asks(&out).len(), 1);
    assert!(asks(&rig.sign_in_press()).is_empty());
}

#[test]
fn a_code_press_while_a_try_is_under_way_changes_nothing() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let hello_ask = only_ask(&rig.sign_in_press()).id;
    rig.reply(hello_ask, hello());
    assert_eq!(rig.shown().sign_in.state, SignInState::Working);
    let out = rig.press(Press::SignInWithCode {
        address: NEXUS.to_owned(),
        code: Typed::new("ABCD-EFGH-JKMN"),
        password: Typed::new("one long new password"),
        again: Typed::new("another long new password"),
        look: Look::default(),
        retry: false,
    });
    assert!(asks(&out).is_empty());
    assert_eq!(rig.shown().sign_in.state, SignInState::Working, "the try under way is still shown");
}

#[test]
fn an_answer_to_an_older_try_is_dropped() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let hello_ask = only_ask(&rig.sign_in_press()).id;
    let first = only_ask(&rig.reply(hello_ask, hello())).id;
    rig.fail(first, How::Timeout);
    let hello_ask = only_ask(&rig.sign_in_press()).id;
    let second = only_ask(&rig.reply(hello_ask, hello())).id;
    assert_ne!(first, second);
    let out = rig.reply(first, signed_in_body(1));
    assert!(out.is_empty(), "{out:?}");
    let out = rig.reply(second, signed_in_body(1));
    assert_eq!(kept_saved(&out).len(), 1);
}

#[test]
fn the_local_network_permission_is_asked_for_only_when_no_connection_was_opened() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let hello_ask = only_ask(&rig.sign_in_press()).id;
    rig.happen(Happened::Failed { ask: hello_ask, failure: Failure { how: How::Connect, inside: Some(true), permission: Some(false) } });
    assert_eq!(rig.shown().sign_in.state, SignInState::NeedsPermission);
    assert!(rig.shown().sign_in.ask_permission);
    for failure in [
        Failure { how: How::Connect, inside: Some(false), permission: Some(false) },
        Failure { how: How::Connect, inside: Some(true), permission: Some(true) },
        Failure { how: How::Timeout, inside: Some(true), permission: Some(false) },
    ] {
        let mut rig = Rig::new(DeviceKind::Phone);
        rig.start(Kept::default());
        let hello_ask = only_ask(&rig.sign_in_press()).id;
        rig.happen(Happened::Failed { ask: hello_ask, failure });
        assert_eq!(rig.shown().sign_in.state, SignInState::CantReach, "{failure:?}");
        assert!(!rig.shown().sign_in.ask_permission);
    }
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let hello_ask = only_ask(&rig.sign_in_press()).id;
    rig.happen(Happened::Failed { ask: hello_ask, failure: Failure { how: How::Secure, inside: Some(true), permission: Some(false) } });
    assert_eq!(rig.shown().sign_in.state, SignInState::NotSecure);
    assert!(!rig.shown().sign_in.ask_permission);
    let mut rig = Rig::new(DeviceKind::Pc);
    rig.start(Kept::default());
    let hello_ask = only_ask(&rig.sign_in_press()).id;
    rig.happen(Happened::Failed { ask: hello_ask, failure: Failure { how: How::Connect, inside: Some(true), permission: Some(false) } });
    assert!(!rig.shown().sign_in.ask_permission);
}

#[test]
fn a_saved_session_on_the_home_network_without_the_permission_asks_for_it() {
    // The phone's walk: signed in, the backend's name now leads inside, the permission is
    // not held. Whatever asked, it is the one permission decision, on the screen that is up.
    let blocked = Failure { how: How::Connect, inside: Some(true), permission: Some(false) };
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(kept_signed_in(4));
    let verify = only_ask(&out).id;
    rig.happen(Happened::Failed { ask: verify, failure: blocked });
    assert!(rig.shown().ask_permission);
    assert!(rig.shown().row.contains("home network"));
    assert_eq!(rig.shown().screen, Screen::SignedIn);
    let out = rig.happen(Happened::Network { up: true });
    let verify = ask_to(&out, proto::HELLO).id;
    let out = rig.reply(verify, hello());
    assert!(!rig.shown().ask_permission);
    let check = ask_to(&out, account::LIST_DEVICES).id;
    rig.reply(check, devices_body(4, vec![this_device()]));
    let renew = ask_to(&rig.refuse(4), account::MUSIC_LOGIN).id;
    rig.happen(Happened::Failed { ask: renew, failure: blocked });
    assert!(rig.shown().ask_permission);
    assert!(!rig.shown().music_ask_permission, "the backend's, not the music server's");
}

#[test]
fn a_code_is_redeemed_with_its_new_password_in_one_request() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let differ = rig.press(Press::SignInWithCode {
        address: NEXUS.to_owned(),
        code: Typed::new("ABCD-EFGH-JKMN"),
        password: Typed::new("my new long password"),
        again: Typed::new("my new long passw0rd"),
        look: Look::default(),
        retry: false,
    });
    assert!(asks(&differ).is_empty());
    assert_eq!(rig.shown().sign_in.state, SignInState::PasswordsDiffer);
    let out = rig.press(Press::SignInWithCode {
        address: NEXUS.to_owned(),
        code: Typed::new("abcd efgh jkmn"),
        password: Typed::new("my new long password"),
        again: Typed::new("my new long password"),
        look: Look::default(),
        retry: false,
    });
    let hello_ask = only_ask(&out).id;
    let out = rig.reply(hello_ask, hello());
    let sign_in = only_ask(&out);
    let body = body_of(sign_in);
    assert_eq!(body["credential"]["code"]["code"], "ABCDEFGHJKMN");
    assert_eq!(body["credential"]["code"]["new_password"], "my new long password");
    let sign_in = sign_in.id;
    let out = rig.reply(sign_in, signed_in_body(1));
    assert_eq!(kept_saved(&out).len(), 1);
    assert!(asks(&out).is_empty(), "one press, one request");
    assert_eq!(rig.shown().screen, Screen::SignedIn);
}

// ---------------------------------------------------------------------------------------
// The fence: what is no longer waited for is neither sent nor acted on
// ---------------------------------------------------------------------------------------

#[test]
fn an_answer_arriving_after_sign_out_is_thrown_away() {
    let mut rig = running(1);
    let music = ask_to(&rig.refuse(1), account::MUSIC_LOGIN).id;
    let out = rig.press(Press::SignOut);
    assert!(forgets(&out).contains(&Forget::Saved));
    let out = rig.reply(music, music_body(2));
    assert!(kept_saved(&out).is_empty() && handed(&out).is_empty(), "{out:?}");
    assert!(!rig.client.waits_for(music));
}

#[test]
fn the_account_switched_while_an_ask_is_in_flight() {
    let mut rig = Rig::new(DeviceKind::Pc);
    let out = rig.start_verified(kept_signed_in(1));
    let check = only_ask(&out).id;
    let generation = rig.client.generation();
    rig.client.stop();
    assert!(rig.client.generation() > generation);
    assert!(!rig.client.waits_for(check), "not sent once stopped");
    assert!(rig.reply(check, session_ended()).is_empty());
    rig.start(kept_signed_in(7));
    let out = rig.reply(check, session_ended());
    assert!(out.is_empty(), "{out:?}");
    assert_eq!(rig.shown().screen, Screen::SignedIn);
}

#[test]
fn what_was_queued_is_not_sent_after_offline_or_a_stop_but_a_sign_out() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let hello_ask = only_ask(&rig.sign_in_press()).id;
    let sign_in = only_ask(&rig.reply(hello_ask, hello())).id;
    assert!(rig.client.waits_for(sign_in));
    rig.happen(Happened::Offline(true));
    assert!(!rig.client.waits_for(sign_in), "the password is not sent after Offline went on");

    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start_verified(kept_signed_in(1));
    let check = only_ask(&out).id;
    let out = rig.press(Press::SignOut);
    let tell = ask_to(&out, account::SIGN_OUT).id;
    rig.happen(Happened::Offline(true));
    assert!(!rig.client.waits_for(check));
    assert!(rig.client.waits_for(tell), "the deliberate sign-out still goes");
    rig.client.stop();
    assert!(rig.client.waits_for(tell), "and across an account change");
    assert!(rig.reply(tell, done(SignedOut {}, receipt(1))).is_empty());
    assert!(!rig.client.waits_for(tell));
}

#[test]
fn offline_switched_on_with_an_ask_in_flight() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let hello_ask = only_ask(&rig.sign_in_press()).id;
    let out = rig.happen(Happened::Offline(true));
    assert!(asks(&out).is_empty());
    assert_eq!(rig.shown().sign_in.state, SignInState::Offline);
    let out = rig.reply(hello_ask, hello());
    assert!(asks(&out).is_empty());
    assert!(asks(&rig.sign_in_press()).is_empty());
    assert!(asks(&rig.happen(Happened::Tick)).is_empty());
}

#[test]
fn an_answer_is_taken_once_and_one_larger_than_its_ask_allowed_is_cut() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start_verified(kept_signed_in(1));
    let check = only_ask(&out).id;
    assert!(!rig.reply(check, devices_body(1, vec![this_device()])).is_empty());
    assert!(rig.reply(check, session_ended()).is_empty());
    assert_eq!(rig.shown().screen, Screen::SignedIn);
    let out = rig.refuse(1);
    let renew = ask_to(&out, account::MUSIC_LOGIN);
    assert_eq!(renew.max_body, account::SMALL_REPLY_MAX_BYTES);
    let renew = renew.id;
    let mut big = music_body(2);
    big.1.resize(account::SMALL_REPLY_MAX_BYTES + 1, b' ');
    let out = rig.reply(renew, big);
    assert!(handed(&out).is_empty() && kept_saved(&out).is_empty());
}

// ---------------------------------------------------------------------------------------
// Endings: only the backend, to this session, signs a device out
// ---------------------------------------------------------------------------------------

#[test]
fn a_forced_out_device_stops_only_when_told() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start_verified(kept_signed_in(1));
    let check = only_ask(&out).id;
    let out = rig.fail(check, How::Connect);
    assert!(forgets(&out).is_empty());
    let out = rig.happen(Happened::Network { up: true });
    let check = ask_to(&out, account::LIST_DEVICES).id;
    let out = rig.reply(check, (502, b"Bad Gateway".to_vec()));
    assert!(forgets(&out).is_empty());
    let out = rig.happen(Happened::Network { up: true });
    let check = ask_to(&out, account::LIST_DEVICES).id;
    let out = rig.reply(check, (401, b"{\"refused\":{\"not_signed_in\":\"session_ended\"},\"refused\":1}".to_vec()));
    assert!(forgets(&out).is_empty());
    assert_eq!(rig.shown().screen, Screen::SignedIn);
    let out = rig.happen(Happened::Network { up: true });
    let check = ask_to(&out, account::LIST_DEVICES).id;
    let out = rig.reply(check, session_ended());
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
    let verify = ask_to(&out, proto::HELLO).id;
    let out = rig.reply(verify, hello());
    let check = ask_to(&out, account::LIST_DEVICES).id;
    let out = rig.reply(check, session_ended());
    assert_eq!(forgets(&out), vec![Forget::Saved]);
    assert_eq!(rig.shown().sign_in.state, SignInState::ForcedOut);
}

#[test]
fn the_backend_put_back_from_a_copy_means_signing_in_again_with_the_music_kept() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start_verified(kept_signed_in(3));
    let check = only_ask(&out).id;
    let out = rig.reply(check, refused(Refusal::NotSignedIn(NotSignedIn::DataPutBack)));
    assert!(forgets(&out).is_empty(), "the music login is not forgotten");
    let kept = kept_saved(&out);
    assert_eq!(kept.len(), 1);
    assert!(kept[0].session.is_none() && kept[0].sign_out.is_none());
    assert_eq!(kept[0].music.as_ref().map(|login| login.count), Some(3));
    assert!(handed(&out).is_empty());
    assert_eq!(rig.shown().screen, Screen::SignIn);
    assert_eq!(rig.shown().sign_in.state, SignInState::StartAgain);
    assert!(rig.shown().plays);

    let mut rig = Rig::new(DeviceKind::Phone);
    let mut kept = kept_signed_in(3);
    if let Some(saved) = kept.saved.as_mut() {
        saved.session = None;
        saved.sign_out = None;
    }
    let out = rig.start(kept);
    assert!(matches!(&out[0], Do::Hand(Hand::Login { .. })));
    assert!(asks(&out).is_empty());
    assert_eq!(rig.shown().sign_in.state, SignInState::StartAgain);
    let hello_ask = only_ask(&rig.sign_in_press()).id;
    let out = rig.reply(hello_ask, hello());
    assert_eq!(body_of(only_ask(&out))["restore"], 1);
}

#[test]
fn a_login_set_aside_stays_aside_through_start_again() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start_verified(kept_signed_in(4));
    let check = only_ask(&out).id;
    rig.refuse(4);
    let out = rig.reply(check, refused(Refusal::NotSignedIn(NotSignedIn::DataPutBack)));
    assert!(handed(&out).is_empty(), "4 is not handed back as usable");
    assert!(rig.shown().car.is_some(), "the car keeps its line");
    assert!(rig.shown().music.as_deref().unwrap().contains("Sign in"));
}

#[test]
fn another_backend_answering_at_the_origin_is_not_ours() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start_verified(kept_signed_in(1));
    let check = only_ask(&out).id;
    let body = done(DevicePage::new(vec![], false).unwrap(), receipt_full(stranger(), account(), 1, 1, 1));
    let out = rig.reply(check, body);
    assert!(forgets(&out).is_empty() && kept_saved(&out).is_empty() && asks(&out).is_empty());
    assert!(rig.shown().row.contains("a different server answers"));
    assert!(asks(&rig.happen(Happened::Network { up: true })).is_empty());
    assert!(asks(&rig.refuse(1)).is_empty());
}

#[test]
fn a_receipt_never_goes_back_and_another_accounts_is_not_taken() {
    let mut rig = Rig::new(DeviceKind::Pc);
    let out = rig.start_verified(kept_signed_in(4));
    let check = only_ask(&out).id;
    let renew = ask_to(&rig.refuse(4), account::MUSIC_LOGIN).id;
    let out = rig.reply(check, done(DevicePage::new(vec![this_device()], false).unwrap(), receipt_full(backend(), account(), 1, 2, 4)));
    assert_eq!(kept_saved(&out)[0].life, 2);
    let out = rig.reply(renew, done(MusicLoginResult::Login(login(4)), receipt_full(backend(), account(), 1, 1, 4)));
    assert!(kept_saved(&out).iter().all(|saved| saved.life == 2));
    let out = rig.happen(Happened::Network { up: true });
    let renew = ask_to(&out, account::MUSIC_LOGIN).id;
    let other = AccountId::new(99).unwrap();
    let out = rig.reply(renew, done(MusicLoginResult::Login(login(5)), receipt_full(backend(), other, 1, 2, 5)));
    assert!(handed(&out).is_empty() && kept_saved(&out).is_empty());
}

#[test]
fn too_old_for_one_server_does_not_stop_signing_in_to_another() {
    let mut rig = Rig::new(DeviceKind::Pc);
    let out = rig.start_verified(kept_signed_in(4));
    let check = only_ask(&out).id;
    rig.reply(check, refused(Refusal::NotAllowed(NotAllowed::Incompatible { speaks: Speaks::new(7, 9).unwrap() })));
    assert!(rig.shown().row.contains("too old"));
    assert!(asks(&rig.happen(Happened::Network { up: true })).iter().all(|ask| ask.url.ends_with(proto::HELLO)));
    rig.press(Press::SignOut);
    let out = rig.press(sign_in("other.example.net", EMAIL));
    assert_eq!(asks_to(&out, proto::HELLO), 1, "the other server is asked");
}

// ---------------------------------------------------------------------------------------
// Signing out
// ---------------------------------------------------------------------------------------

#[test]
fn signing_out_keeps_the_note_before_forgetting_and_tells_the_backend_with_it() {
    let mut rig = Rig::new(DeviceKind::Pc);
    rig.start_verified(kept_signed_in(1));
    let out = rig.press(Press::SignOut);
    let keep_at = out.iter().position(|thing| matches!(thing, Do::Keep(Keep::Note(_)))).unwrap();
    let forget_at = out.iter().position(|thing| matches!(thing, Do::Forget(Forget::Saved))).unwrap();
    assert!(keep_at < forget_at, "a death between the two leaves the session endable");
    let note = kept_note(&out).unwrap();
    assert_eq!(note.session(), SESSION_ID);
    let written = serde_json::to_string(&note).unwrap();
    assert!(!written.contains(&hex(&SESSION)), "the note cannot sign anyone in");
    let tell = ask_to(&out, account::SIGN_OUT);
    assert!(tell.authorization.is_none(), "told without the session");
    assert_eq!(body_of(tell)["revoke"]["secret"], hex(&SIGN_OUT_SECRET));
    let tell = tell.id;
    assert_eq!(rig.shown().sign_in.state, SignInState::SignedOut);
    let out = rig.reply(tell, done(SignedOut {}, receipt(1)));
    assert_eq!(forgets(&out), vec![Forget::Note { session: SESSION_ID }]);
}

#[test]
fn a_note_is_cleared_only_by_its_own_backends_done_a_gone_session_or_its_age() {
    let keeps: Vec<(u16, Vec<u8>)> = vec![
        refused(Refusal::Busy { retry_after_seconds: Wait::new(3) }),
        refused(Refusal::NotDealtWith),
        refused(Refusal::Malformed),
        done(SignedOut {}, receipt_full(stranger(), account(), 1, 1, 1)),
        (502, Vec::new()),
    ];
    for answer in keeps {
        let mut rig = Rig::new(DeviceKind::Pc);
        rig.start_verified(kept_signed_in(1));
        let tell = ask_to(&rig.press(Press::SignOut), account::SIGN_OUT).id;
        let out = rig.reply(tell, answer.clone());
        assert!(forgets(&out).is_empty(), "kept after {:?}", String::from_utf8_lossy(&answer.1));
    }
    let mut rig = Rig::new(DeviceKind::Pc);
    rig.start_verified(kept_signed_in(1));
    let tell = ask_to(&rig.press(Press::SignOut), account::SIGN_OUT).id;
    assert_eq!(forgets(&rig.reply(tell, session_ended())), vec![Forget::Note { session: SESSION_ID }]);
}

#[test]
fn signing_out_offline_leaves_a_note_told_when_it_can_be() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let mut kept = kept_signed_in(1);
    kept.offline = true;
    rig.start(kept);
    let out = rig.press(Press::SignOut);
    assert!(forgets(&out).contains(&Forget::Saved));
    assert!(asks(&out).is_empty());
    let note = kept_note(&out).unwrap();
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(Kept { notes: vec![note.copy()], offline: true, ..Kept::default() });
    assert!(asks(&out).is_empty());
    let out = rig.happen(Happened::Offline(false));
    let verify = only_ask(&out).id;
    let out = rig.reply(verify, hello());
    let tell = only_ask(&out);
    assert!(tell.url.ends_with(account::SIGN_OUT) && tell.authorization.is_none());
    let tell = tell.id;
    rig.fail(tell, How::Connect);
    assert!(asks(&rig.happen(Happened::Tick)).is_empty());
    rig.now += TELL_EVERY_MS;
    let tell = only_ask(&rig.happen(Happened::Tick)).id;
    let out = rig.reply(tell, done(SignedOut {}, receipt(1)));
    assert_eq!(forgets(&out), vec![Forget::Note { session: SESSION_ID }]);
}

#[test]
fn a_note_is_never_told_to_another_backend() {
    let mut rig = Rig::new(DeviceKind::Pc);
    let note = Note { request: 5, revoke: revoke(), nexus: NEXUS.to_owned(), backend_id: backend().to_hex(), restore: 1, made_ms: START_MS };
    let out = rig.start(Kept { notes: vec![note], ..Kept::default() });
    let verify = only_ask(&out).id;
    let out = rig.reply(verify, hello_from(stranger(), &["accounts"]));
    assert!(asks(&out).is_empty());
    assert!(forgets(&out).is_empty(), "kept, for its own backend");
}

#[test]
fn an_expired_or_unusable_note_is_forgotten_at_start() {
    for offline in [false, true] {
        let mut rig = Rig::new(DeviceKind::Pc);
        let old = Note { request: 5, revoke: revoke(), nexus: NEXUS.to_owned(), backend_id: backend().to_hex(), restore: 1, made_ms: START_MS - NOTE_LIFE_MS };
        let out = rig.start(Kept { notes: vec![old], offline, ..Kept::default() });
        assert_eq!(forgets(&out), vec![Forget::Note { session: SESSION_ID }]);
        assert!(asks(&out).is_empty());
    }
}

#[test]
fn notes_for_two_sessions_are_kept_apart_and_a_new_sign_in_settles_neither() {
    let mut rig = Rig::new(DeviceKind::Pc);
    let mut kept = kept_signed_in(1);
    kept.offline = true;
    rig.start(kept);
    rig.press(Press::SignOut);
    rig.happen(Happened::Offline(false));
    let out = rig.press(sign_in(NEXUS, "grace@example.org"));
    let hello_ask = ask_to(&out, proto::HELLO).id;
    let out = rig.reply(hello_ask, hello());
    let sign_in = ask_to(&out, account::SIGN_IN).id;
    let other = SignOutNote { session: SessionId::new(SESSION_ID + 1).unwrap(), secret: SignOutSecret::from_bytes([0x61; 32]) };
    let answer = done(
        SignedIn {
            session: Session::from_bytes([0x62; 32]),
            session_id: SessionId::new(SESSION_ID + 1).unwrap(),
            sign_out: other,
            person: person(),
            device: DeviceId::new(3).unwrap(),
            restore: 1,
            music: MusicLoginResult::Login(login(1)),
            notice: Notice::new("Recorded.").unwrap(),
        },
        receipt(1),
    );
    let out = rig.reply(sign_in, answer);
    assert!(!forgets(&out).iter().any(|forget| matches!(forget, Forget::Note { .. })), "B's sign-in proves nothing of A's session");
    let out = rig.press(Press::SignOut);
    assert!(!forgets(&out).contains(&Forget::Note { session: SESSION_ID }), "B's sign-out does not displace A's note");
    assert_eq!(kept_note(&out).map(|note| note.session()), Some(SESSION_ID + 1));
}

#[test]
fn a_late_sign_out_for_the_first_account_cannot_touch_the_second() {
    let mut rig = Rig::new(DeviceKind::Pc);
    rig.start_verified(kept_signed_in(1));
    let out = rig.press(Press::SignOut);
    let tell = ask_to(&out, account::SIGN_OUT).id;
    let out = rig.press(sign_in("other.example.net", "grace@example.org"));
    let hello_ask = only_ask(&out).id;
    let sign_in = only_ask(&rig.reply(hello_ask, hello())).id;
    rig.reply(sign_in, signed_in_body(1));
    assert_eq!(rig.shown().screen, Screen::SignedIn);
    let out = rig.reply(tell, session_ended());
    assert_eq!(forgets(&out), vec![Forget::Note { session: SESSION_ID }]);
    assert_eq!(rig.shown().screen, Screen::SignedIn);
}

// ---------------------------------------------------------------------------------------
// A refused music login (6.3): the one rule
// ---------------------------------------------------------------------------------------

#[test]
fn a_refused_music_login_is_asked_for_once() {
    let mut rig = running(4);
    let out = rig.refuse(4);
    assert!(matches!(handed(&out)[..], [Hand::Refused { count: 4, .. }]));
    let renew = only_ask(&out);
    assert!(renew.url.ends_with(account::MUSIC_LOGIN));
    let renew = renew.id;
    for _ in 0..9 {
        let out = rig.refuse(4);
        assert!(asks(&out).is_empty() && handed(&out).is_empty(), "{out:?}");
    }
    assert!(rig.shown().car.is_some());
    assert_eq!(rig.shown().screen, Screen::SignedIn, "a refusal never signs anyone out");
    let out = rig.reply(renew, music_body(5));
    assert_eq!(asks(&out).len(), 0, "the renewal's own answer asks nothing more");
    assert_eq!(kept_saved(&out)[0].music.as_ref().map(|login| login.count), Some(5));
    assert!(matches!(handed(&out)[..], [Hand::Login { login, .. }] if login.count == 5));
    assert!(rig.shown().car.is_none() && rig.shown().music.is_none());
    let out = rig.refuse(4);
    assert!(asks(&out).is_empty() && handed(&out).is_empty(), "not a second hand-over");
}

#[test]
fn a_late_report_after_the_new_login_was_itself_refused_hands_nothing_back() {
    let mut rig = running(4);
    let renew = only_ask(&rig.refuse(4)).id;
    rig.reply(renew, music_body(5));
    let out = rig.refuse(5);
    assert!(matches!(handed(&out)[..], [Hand::Refused { count: 5, .. }]));
    for _ in 0..5 {
        let out = rig.refuse(4);
        assert!(handed(&out).is_empty(), "5 is not reinstalled: {out:?}");
    }
}

#[test]
fn a_report_from_the_account_before_does_not_touch_the_next() {
    let mut rig = running(4);
    let before = rig.handed;
    rig.client.stop();
    let out = rig.start_verified(kept_signed_in(4));
    let check = only_ask(&out).id;
    rig.reply(check, devices_body(4, vec![this_device()]));
    assert_ne!(rig.handed, before);
    let out = rig.happen(Happened::MusicRefused { count: 4, generation: before });
    assert!(out.is_empty(), "B's login is not set aside by A's late report: {out:?}");
    let out = rig.happen(Happened::MusicLooked { look: Look { inside: Some(true), permission: Some(false) }, count: 4, generation: before });
    assert!(out.is_empty());
    assert!(!rig.shown().music_ask_permission);
}

#[test]
fn a_renewal_with_a_moved_number_in_its_own_answer_asks_nothing_more() {
    let mut rig = running(4);
    let renew = only_ask(&rig.refuse(4)).id;
    let out = rig.reply(renew, music_body(5));
    assert_eq!(asks(&out).len(), 0);
    // A stamped "busy" at a higher number, again and again: no ask from the answer itself.
    let mut rig = running(4);
    let mut renew = only_ask(&rig.refuse(4)).id;
    for _ in 0..3 {
        let out = rig.reply(renew, refused_stamped(Refusal::Busy { retry_after_seconds: None }, receipt(5)));
        assert_eq!(asks(&out).len(), 0, "{out:?}");
        let out = rig.happen(Happened::Network { up: true });
        assert_eq!(asks_to(&out, account::MUSIC_LOGIN), 1);
        renew = ask_to(&out, account::MUSIC_LOGIN).id;
        if let Some(check) = asks(&out).iter().find(|ask| ask.url.ends_with(account::LIST_DEVICES)).map(|ask| ask.id) {
            rig.reply(check, devices_body(4, vec![this_device()]));
        }
    }
}

#[test]
fn a_newer_number_learnt_while_a_renewal_is_under_way_is_asked_for_once_it_is_in() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start_verified(kept_signed_in(4));
    let check = only_ask(&out).id;
    let renew = only_ask(&rig.refuse(4)).id;
    let out = rig.reply(check, devices_body(5, vec![this_device()]));
    assert_eq!(asks(&out).len(), 0, "one renewal at a time");
    let out = rig.reply(renew, music_body(4));
    let again = only_ask(&out);
    assert!(again.url.ends_with(account::MUSIC_LOGIN), "the announced 5 is asked for");
    let again = again.id;
    let out = rig.reply(again, music_body(5));
    assert!(matches!(handed(&out)[..], [Hand::Login { login, .. }] if login.count == 5));
    assert_eq!(asks(&out).len(), 0);
    // The other order: the renewal for 4 first (nothing newer), then the check at 5.
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start_verified(kept_signed_in(4));
    let check = only_ask(&out).id;
    let renew = only_ask(&rig.refuse(4)).id;
    let out = rig.reply(renew, music_body(4));
    assert_eq!(asks(&out).len(), 0);
    let out = rig.reply(check, devices_body(5, vec![this_device()]));
    assert_eq!(asks_to(&out, account::MUSIC_LOGIN), 1);
}

#[test]
fn a_refused_login_with_nothing_newer_stays_aside_until_the_network_comes_back() {
    let mut rig = running(4);
    let renew = only_ask(&rig.refuse(4)).id;
    let out = rig.reply(renew, music_body(4));
    assert!(handed(&out).is_empty(), "the same number is not handed again: {out:?}");
    assert_eq!(asks(&out).len(), 0);
    assert!(rig.shown().music.as_deref().unwrap().contains("no newer one"));
    for _ in 0..5 {
        rig.now += DAY;
        assert!(asks(&rig.happen(Happened::Tick)).is_empty());
        assert!(asks(&rig.refuse(4)).is_empty());
    }
    // Offline off by itself does not count (the lead's ruling).
    rig.happen(Happened::Offline(true));
    let out = rig.happen(Happened::Offline(false));
    assert_eq!(asks_to(&out, account::MUSIC_LOGIN), 0);
    if let Some(check) = asks(&out).first().map(|ask| ask.id) {
        rig.reply(check, devices_body(4, vec![this_device()]));
    }
    let out = rig.happen(Happened::Network { up: true });
    let renew = ask_to(&out, account::MUSIC_LOGIN).id;
    let check = ask_to(&out, account::LIST_DEVICES).id;
    let out = rig.reply(renew, music_body(4));
    assert!(handed(&out).is_empty());
    let out = rig.reply(check, devices_body(6, vec![this_device()]));
    let renew = only_ask(&out).id;
    let out = rig.reply(renew, music_body(6));
    assert!(matches!(handed(&out)[..], [Hand::Login { login, .. }] if login.count == 6));
}

#[test]
fn a_refused_login_with_the_backend_down_plays_what_is_here_and_retries_nothing() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start_verified(kept_signed_in(4));
    let check = only_ask(&out).id;
    rig.fail(check, How::Connect);
    let renew = only_ask(&rig.refuse(4)).id;
    let out = rig.fail(renew, How::Connect);
    assert!(asks(&out).is_empty());
    assert!(forgets(&out).is_empty());
    assert!(rig.shown().music.as_deref().unwrap().contains("when your Nexus server can be reached"));
    assert!(rig.shown().plays);
    for _ in 0..20 {
        assert!(asks(&rig.refuse(4)).is_empty());
        rig.now += TICK_MS;
        assert!(asks(&rig.happen(Happened::Tick)).is_empty());
    }
    let out = rig.happen(Happened::Network { up: true });
    assert_eq!(asks_to(&out, account::MUSIC_LOGIN), 1);
}

#[test]
fn the_music_login_refused_mid_song_with_offline_on_is_renewed_when_it_goes_off() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let mut kept = kept_signed_in(4);
    kept.offline = true;
    rig.start(kept);
    let out = rig.refuse(4);
    assert!(matches!(handed(&out)[..], [Hand::Refused { count: 4, .. }]));
    assert!(asks(&out).is_empty());
    let out = rig.happen(Happened::Offline(false));
    let verify = ask_to(&out, proto::HELLO).id;
    let out = rig.reply(verify, hello());
    assert_eq!(asks_to(&out, account::MUSIC_LOGIN), 1);
}

#[test]
fn a_moved_number_in_any_answer_is_one_renewal() {
    let mut rig = Rig::new(DeviceKind::Pc);
    let out = rig.start_verified(kept_signed_in(2));
    let check = only_ask(&out).id;
    let out = rig.reply(check, devices_body(3, vec![this_device()]));
    let renew = only_ask(&out);
    assert!(renew.url.ends_with(account::MUSIC_LOGIN));
    let renew = renew.id;
    let out = rig.reply(renew, music_body(3));
    assert!(matches!(handed(&out)[..], [Hand::Login { login, .. }] if login.count == 3));
    assert_eq!(asks(&out).len(), 0);
}

#[test]
fn a_refusal_without_anyone_signed_in_asks_nothing_and_says_how_to_renew() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let mut kept = kept_signed_in(4);
    if let Some(saved) = kept.saved.as_mut() {
        saved.session = None;
        saved.sign_out = None;
    }
    rig.start(kept);
    let out = rig.refuse(4);
    assert!(asks(&out).is_empty());
    assert!(matches!(handed(&out)[..], [Hand::Refused { count: 4, .. }]));
    assert!(rig.shown().music.as_deref().unwrap().contains("Sign in"));
}

// ---------------------------------------------------------------------------------------
// The rules that hold across every sequence
// ---------------------------------------------------------------------------------------

/// Runs most of the sequences above on one rig and gives back everything it was told.
fn a_long_run() -> Rig {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start(kept_signed_in(4));
    let verify = only_ask(&out).id;
    rig.fail(verify, How::Timeout);
    let out = rig.happen(Happened::Network { up: true });
    let verify = ask_to(&out, proto::HELLO).id;
    let out = rig.reply(verify, hello());
    let check = ask_to(&out, account::LIST_DEVICES).id;
    rig.reply(check, devices_body(4, vec![this_device(), device(9, Some(90), false)]));
    rig.press(Press::OpenDevices);
    let renew = only_ask(&rig.refuse(4)).id;
    rig.reply(renew, (302, Vec::new()));
    let out = rig.happen(Happened::Network { up: true });
    for ask in asks(&out).iter().map(|ask| ask.id).collect::<Vec<_>>() {
        rig.reply(ask, music_body(5));
    }
    rig.happen(Happened::Offline(true));
    rig.happen(Happened::Offline(false));
    let out = rig.press(Press::ForceOut { device: 9 });
    if let Some(id) = asks(&out).first().map(|ask| ask.id) {
        rig.reply(id, done(ForcedOut { music_cut_off: false }, receipt(5)));
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
    let salt = hex(&SALT);
    for line in &rig.log {
        let thing: serde_json::Value = serde_json::from_str(line).unwrap();
        if let Some(ask) = thing.get("ask") {
            let url = ask["url"].as_str().unwrap();
            assert!(url.starts_with("https://nexus.example.org/nexus/api/"), "{url}");
            assert!(!url.contains("music.example.org"));
            for secret in [hex(&token(4)), hex(&token(5)), hex(&token(6)), salt.clone()] {
                assert!(!line.contains(&secret), "the music login is in an ask: {line}");
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
    let ids: Vec<u64> = rig
        .log
        .iter()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()?.get("ask")?["id"].as_u64())
        .collect();
    assert!(!ids.is_empty());
    assert!(ids.windows(2).all(|pair| pair[0] < pair[1]));
}

#[test]
fn no_secret_is_shown_by_debug() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let start = rig.start(kept_signed_in(4));
    let mut debug = format!("{start:?}{:?}", kept_signed_in(4));
    debug.push_str(&format!("{:?}", sign_in(NEXUS, EMAIL)));
    rig.press(Press::SignOut);
    let out = rig.sign_in(4);
    debug.push_str(&format!("{out:?}{:?}{:?}", rig.client, Random::new([0x42; 32])));
    let (_, body) = signed_in_body(4);
    debug.push_str(&format!("{:?}", AnswerBody::new(body)));
    for secret in [hex(&SESSION), hex(&token(4)), hex(&SALT), PASSWORD.to_owned(), hex(&[0xd5; 32]), hex(&[0x42; 32]), hex(&SIGN_OUT_SECRET)] {
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
        refused(Refusal::Malformed).1,
        refused(Refusal::NotAllowed(NotAllowed::Incompatible { speaks: Speaks::new(7, 9).unwrap() })).1,
    ];
    for status in [200, 204, 301, 400, 401, 403, 429, 500, 503] {
        for body in &bodies {
            let mut rig = Rig::new(DeviceKind::Phone);
            let out = rig.start(kept_signed_in(4));
            let verify = only_ask(&out).id;
            let out = rig.reply(verify, (status, body.clone()));
            for ask in asks(&out).iter().map(|ask| ask.id).collect::<Vec<_>>() {
                rig.reply(ask, (status, body.clone()));
            }
            let out = rig.refuse(4);
            for ask in asks(&out).iter().map(|ask| ask.id).collect::<Vec<_>>() {
                rig.reply(ask, (status, body.clone()));
            }
            rig.press(Press::SignOut);
            let hello_ask = asks(&rig.sign_in_press()).first().map(|ask| ask.id);
            if let Some(hello_ask) = hello_ask {
                rig.reply(hello_ask, (status, body.clone()));
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// Devices and the password
// ---------------------------------------------------------------------------------------

#[test]
fn the_devices_list_and_forcing_one_out_by_its_session() {
    let mut rig = Rig::new(DeviceKind::Phone);
    let out = rig.start_verified(kept_signed_in(4));
    let check = only_ask(&out).id;
    assert!(asks(&rig.press(Press::OpenDevices)).is_empty());
    assert_eq!(rig.shown().devices.state, ListState::Working);
    rig.reply(check, devices_body(4, vec![this_device(), device(9, Some(90), false), device(12, None, false)]));
    let rows = &rig.shown().devices.rows;
    assert_eq!(rows.len(), 3);
    assert!(rows[0].this_one && !rows[0].can_force_out && rows[0].seen == "This device");
    assert!(rows[1].can_force_out && rows[1].seen == "Used yesterday" && rows[1].session == Some(90));
    assert!(!rows[2].can_force_out && rows[2].seen == "Signed out");
    assert!(asks(&rig.press(Press::ForceOut { device: 12 })).is_empty());
    assert!(asks(&rig.press(Press::ForceOut { device: 77 })).is_empty());
    let out = rig.press(Press::ForceOut { device: 9 });
    let force = only_ask(&out);
    assert!(force.url.ends_with(account::FORCE_OUT));
    assert_eq!(body_of(force)["session"], 90, "names the session it ends");
    let force = force.id;
    assert!(asks(&rig.press(Press::ForceOut { device: 9 })).is_empty(), "pressed twice: one request");
    rig.reply(force, done(ForcedOut { music_cut_off: false }, receipt(4)));
    let shown = rig.shown();
    assert!(!shown.devices.rows[1].can_force_out);
    assert!(shown.devices.words.contains("change that too"));
    let out = rig.press(Press::ForceOut { device: 3 });
    assert!(forgets(&out).contains(&Forget::Saved));
}

#[test]
fn changing_the_password() {
    let mut rig = running(4);
    let change = |current: &str, new: &str, again: &str| Press::ChangePassword { current: Typed::new(current), new: Typed::new(new), again: Typed::new(again) };
    assert!(asks(&rig.press(change(PASSWORD, "another long password", "another long passwor"))).is_empty());
    assert_eq!(rig.shown().password.state, PasswordState::Differ);
    assert!(asks(&rig.press(change(PASSWORD, "short", "short"))).is_empty());
    assert_eq!(rig.shown().password.state, PasswordState::NotAllowed);
    assert!(asks(&rig.press(change(PASSWORD, "ADA@example.org", "ADA@example.org"))).is_empty(), "not the email");
    let out = rig.press(change(PASSWORD, "another long password", "another long password"));
    let ask = only_ask(&out);
    assert!(ask.url.ends_with(account::CHANGE_PASSWORD));
    assert_eq!(body_of(ask)["new_password"], "another long password");
    let ask = ask.id;
    rig.reply(ask, refused(Refusal::NotAllowed(NotAllowed::WrongLogin)));
    assert_eq!(rig.shown().password.state, PasswordState::WrongCurrent);
    let ask = only_ask(&rig.press(change(PASSWORD, "another long password", "another long password"))).id;
    rig.reply(ask, refused(Refusal::Busy { retry_after_seconds: Wait::new(30) }));
    assert!(rig.shown().password.words.contains("30 seconds"));
    let ask = only_ask(&rig.press(change(PASSWORD, "another long password", "another long password"))).id;
    rig.reply(ask, done(PasswordChanged {}, receipt(4)));
    assert_eq!(rig.shown().password.state, PasswordState::Changed);
    assert_eq!(rig.shown().screen, Screen::SignedIn);
}

// ---------------------------------------------------------------------------------------
// The phone's entry point and asks
// ---------------------------------------------------------------------------------------

fn start_json(saved: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"start": {
        "app": {"version": "0.41.0", "kind": "phone", "device_name": "Ada's phone"},
        "kept": {"device_secret": hex(&[0xd5; 32]), "saved": saved, "notes": [], "offline": false},
        "given": {"now_ms": START_MS, "local_offset_minutes": 600, "random": hex(&[0x42; 32])}
    }})
}

fn call(client: &mut Client, value: &serde_json::Value) -> serde_json::Value {
    serde_json::from_slice(&client.call_json(value.to_string().as_bytes())).unwrap()
}

fn answer_json(ask: u64, (status, body): (u16, Vec<u8>)) -> serde_json::Value {
    serde_json::json!({"happened": {
        "event": {"answered": {"ask": ask, "status": status, "body": String::from_utf8(body).unwrap()}},
        "given": {"now_ms": START_MS + 1000}
    }})
}

fn first_ask(out: &serde_json::Value) -> u64 {
    out["do"].as_array().unwrap().iter().find_map(|thing| thing.get("ask")).unwrap()["id"].as_u64().unwrap()
}

#[test]
fn the_json_entry_point_makes_the_same_calls() {
    let mut client = Client::default();
    let out = call(&mut client, &start_json(serde_json::to_value(saved(4)).unwrap()));
    assert_eq!(out["do"][0]["hand"]["login"]["login"]["count"], 4);
    let verify = first_ask(&out);
    let out = call(&mut client, &answer_json(verify, hello()));
    let check = out["do"].as_array().unwrap().iter().find_map(|thing| thing.get("ask")).unwrap();
    assert_eq!(check["method"], "POST");
    assert_eq!(check["generation"], out["generation"]);
    let id = check["id"].as_u64().unwrap();
    let out = call(&mut client, &answer_json(id, session_ended()));
    assert!(out["do"].as_array().unwrap().iter().any(|thing| thing["forget"] == "saved"));
    let out = call(&mut client, &serde_json::json!({"stop": {}}));
    assert_eq!(out["generation"], client.generation());
    assert_eq!(out["do"], serde_json::json!([]));
}

#[test]
fn input_the_entry_point_cannot_read_is_one_fixed_error_and_changes_nothing() {
    let mut client = Client::default();
    let started = call(&mut client, &start_json(serde_json::to_value(saved(4)).unwrap()));
    let generation = client.generation();
    let verify = first_ask(&started);
    let huge = format!(r#"{{"happened":{{"event":{{"answered":{{"ask":1,"status":200,"body_bytes":[{}0]}}}},"given":{{"now_ms":1}}}}}}"#, "0,".repeat(json::CALL_JSON_MAX_BYTES / 2));
    let inputs = [
        String::new(),
        "{".to_owned(),
        r#"{"start":{}}"#.to_owned(),
        huge,
        r#"{"happened":{"event":{"music_looked":{"look":{"inside":true,"permission":false,"unexpected":1},"count":4,"generation":1}},"given":{"now_ms":1}}}"#.to_owned(),
        r#"{"happened":{"event":{"failed":{"ask":1,"failure":{"how":"connect","extra":true}}},"given":{"now_ms":1}}}"#.to_owned(),
        r#"{"happened":{"event":{"tick":{}},"given":{"now_ms":1,"extra":1}}}"#.to_owned(),
        r#"{"happened":{"event":{"answered":{"ask":1,"status":200,"body":"a","body_bytes":[1]}},"given":{"now_ms":1}}}"#.to_owned(),
    ];
    for input in &inputs {
        assert_eq!(client.call_json(input.as_bytes()), br#"{"error":"input"}"#.to_vec());
        assert_eq!(client.generation(), generation, "state untouched");
        assert!(client.waits_for(verify), "the ask is still waited for");
    }
}

#[test]
fn the_backends_line_is_judged_for_the_local_network_before_connecting() {
    let blocked = Look { inside: Some(true), permission: Some(false) };
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let out = rig.press(Press::SignIn { address: NEXUS.to_owned(), email: EMAIL.to_owned(), password: Typed::new(PASSWORD), look: blocked, retry: false });
    assert!(asks(&out).is_empty(), "nothing is asked: it would be dropped silently");
    assert_eq!(rig.shown().sign_in.state, SignInState::NeedsPermission);
    assert!(rig.shown().sign_in.ask_permission);
    for look in [Look { inside: Some(true), permission: Some(true) }, Look { inside: Some(false), permission: Some(false) }, Look::default()] {
        let mut rig = Rig::new(DeviceKind::Phone);
        rig.start(Kept::default());
        let out = rig.press(Press::SignIn { address: NEXUS.to_owned(), email: EMAIL.to_owned(), password: Typed::new(PASSWORD), look, retry: false });
        assert_eq!(asks(&out).len(), 1, "{look:?}");
    }
}

#[test]
fn the_music_servers_address_is_judged_apart_from_the_backends() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(kept_signed_in(4));
    let generation = rig.handed;
    rig.happen(Happened::MusicLooked { look: Look { inside: Some(true), permission: Some(false) }, count: 4, generation });
    assert!(rig.shown().music_ask_permission);
    assert!(!rig.shown().ask_permission);
    assert!(rig.shown().music.as_deref().unwrap().contains("home network"));
    assert_eq!(rig.shown().screen, Screen::SignedIn);
    rig.happen(Happened::MusicLooked { look: Look { inside: Some(true), permission: Some(true) }, count: 3, generation });
    assert!(rig.shown().music_ask_permission, "a look at a login no longer held is dropped");
    rig.happen(Happened::MusicLooked { look: Look { inside: Some(true), permission: Some(true) }, count: 4, generation });
    assert!(!rig.shown().music_ask_permission && rig.shown().music.is_none());
    let mut rig = Rig::new(DeviceKind::Pc);
    rig.start(kept_signed_in(4));
    let generation = rig.handed;
    rig.happen(Happened::MusicLooked { look: Look { inside: Some(true), permission: Some(false) }, count: 4, generation });
    assert!(!rig.shown().music_ask_permission);
}

#[test]
fn a_folder_carries_over_only_to_the_same_music_server_and_user() {
    let handed = login(1);
    assert!(carries_over("https://music.example.org/navidrome", "ada", &handed));
    assert!(carries_over("HTTPS://Music.Example.org/navidrome/", "ada", &handed));
    assert!(!carries_over("https://music.example.org/navidrome", "Ada", &handed));
    assert!(!carries_over("https://music.example.org/other", "ada", &handed));
    assert!(!carries_over("https://music.example.org:8443/navidrome", "ada", &handed));
    assert!(!carries_over("https://other.example.org/navidrome", "ada", &handed));
    assert!(!carries_over("music.example.org/navidrome", "ada", &handed));
    assert!(!carries_over("https://ada@music.example.org/navidrome", "ada", &handed));
}

#[test]
fn the_entry_point_says_whether_an_ask_is_waited_for() {
    let mut client = Client::default();
    let out = call(&mut client, &start_json(serde_json::to_value(saved(4)).unwrap()));
    let id = first_ask(&out);
    assert_eq!(call(&mut client, &serde_json::json!({"waits_for": {"ask": id}}))["waits"], true);
    client.stop();
    assert_eq!(call(&mut client, &serde_json::json!({"waits_for": {"ask": id}}))["waits"], false);
}

#[test]
fn what_keep_writes_is_what_start_reads() {
    let mut rig = Rig::new(DeviceKind::Phone);
    rig.start(Kept::default());
    let out = rig.sign_in(4);
    let saved_json = out
        .iter()
        .find_map(|thing| match thing {
            Do::Keep(Keep::Saved(_)) => Some(serde_json::to_value(thing).unwrap()["keep"]["saved"].clone()),
            _ => None,
        })
        .unwrap();
    let mut client = Client::default();
    let out = call(&mut client, &start_json(saved_json));
    assert_eq!(out["do"][0]["hand"]["login"]["login"]["count"], 4);
    let verify = first_ask(&out);
    let out = call(&mut client, &answer_json(verify, hello()));
    let check = out["do"].as_array().unwrap().iter().find_map(|thing| thing.get("ask")).unwrap();
    assert_eq!(check["authorization"], format!("Bearer {}", hex(&SESSION)));
    // And a note: written, read back, told.
    let out = rig.press(Press::SignOut);
    let note_json = out
        .iter()
        .find_map(|thing| match thing {
            Do::Keep(Keep::Note(_)) => Some(serde_json::to_value(thing).unwrap()["keep"]["note"].clone()),
            _ => None,
        })
        .unwrap();
    let mut client = Client::default();
    let start = serde_json::json!({"start": {
        "app": {"version": "0.41.0", "kind": "pc", "device_name": "Ada's PC"},
        "kept": {"notes": [note_json]},
        "given": {"now_ms": START_MS}
    }});
    let out = call(&mut client, &start);
    assert!(!out["do"].as_array().unwrap().iter().any(|thing| thing.get("forget").is_some()), "{out}");
    let verify = first_ask(&out);
    let out = call(&mut client, &answer_json(verify, hello()));
    let tell = out["do"].as_array().unwrap().iter().find_map(|thing| thing.get("ask")).unwrap();
    assert!(tell["url"].as_str().unwrap().ends_with(account::SIGN_OUT));
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
