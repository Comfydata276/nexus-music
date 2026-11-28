use std::collections::BTreeSet;
use super::*;
use crate::nexus::proto::{API, SPEAKS, plain};

const CODE: &str = "CDFHJKMN3479";

fn signin(credential: Credential) -> SignIn {
    SignIn {
        credential,
        device_secret: DeviceSecret::from_bytes([33; SECRET_BYTES]),
        device_name: Name::new("Ada's phone").unwrap(),
        device_kind: DeviceKind::Phone,
        app_version: Version { major: 1, minor: 2, patch: 3 },
        speaks: SPEAKS,
        restore: None,
    }
}

fn round_trip<T: wire::Message + fmt::Debug + PartialEq>(value: T) {
    let body = encode(&value).unwrap();
    assert_eq!(read::<T>(body.bytes()), Ok(value));
}

#[test]
fn every_account_path_is_plain_and_has_its_own_name() {
    assert_eq!(SIGN_IN, "/nexus/api/signin");
    assert_eq!(PATHS.len(), 6);
    let mut seen = BTreeSet::new();
    for path in PATHS {
        assert!(path.starts_with(API));
        assert_eq!(plain(path), Ok(()));
        assert!(seen.insert(path));
    }
}

#[test]
fn a_password_and_a_code_each_make_one_sign_in_shape() {
    round_trip(signin(Credential::Password {
        email: Email::new("  ADA@example.org ").unwrap(),
        password: Password::new("ten characters 🔑").unwrap(),
    }));
    round_trip(signin(Credential::Code(OneTimeCode::new(CODE).unwrap())));
}

const PASSWORD: &str = "a made-up password";
const SALT: &str = "AdaMusicSalt2026";
const ADDRESS: &str = "https://music.example.org:8443/navidrome";

fn email() -> Email {
    Email::new("ada@example.org").unwrap()
}

fn person() -> Person {
    Person { email: email(), name: Name::new("Ada").unwrap(), admin: false }
}

fn session() -> Session {
    Session::from_bytes([45; SECRET_BYTES])
}

fn music() -> MusicLogin {
    MusicLogin {
        address: MusicAddress::new(ADDRESS).unwrap(),
        name: Name::new("ada").unwrap(),
        salt: MusicSalt::new(SALT).unwrap(),
        token: MusicToken::from_bytes([57; TOKEN_BYTES]),
        count: 27,
    }
}

fn receipt() -> Receipt {
    Receipt {
        stamp: Stamp {
            backend: BackendId::from_bytes([17; 16]),
            restore: 8,
            account: email(),
            life: 3,
            change: 6,
            tag: StampTag::from_bytes([69; SECRET_BYTES]),
        },
        music_login_count: 27,
    }
}

fn done<T>(result: T) -> Reply<T> {
    Reply::Done { result, receipt: receipt() }
}

fn good_sign_in() -> SignInReply {
    done(SignInAnswer::SignedIn(SignedIn {
        person: person(),
        device: DeviceId::new(7).unwrap(),
        session: session(),
        music: music(),
        notice: Notice::new("What this server records\nWhat you play.").unwrap(),
    }))
}

fn head() -> RequestHead {
    RequestHead { speaks: SPEAKS, restore: 8 }
}

fn id(number: u64) -> DeviceId {
    DeviceId::new(number).unwrap()
}

fn request_id() -> RequestId {
    RequestId::new(9).unwrap()
}

fn device(number: u64) -> Device {
    Device {
        id: id(number),
        name: Name::new("Ada's PC").unwrap(),
        kind: DeviceKind::Pc,
        app_version: Version { major: 1, minor: 2, patch: 3 },
        last_seen_ms: 5,
        signed_in: true,
    }
}

#[test]
fn every_call_and_success_reply_round_trips() {
    round_trip(good_sign_in());
    round_trip(done(SignInAnswer::PasswordSetup(PasswordSetup {
        session: session(),
        person: person(),
        device: id(7),
    })));
    round_trip(SignOut { head: head(), request_id: request_id() });
    round_trip(done(SignedOut {}));
    round_trip(ListDevices { head: head(), after: Some(id(7)) });
    round_trip(ListDevices { head: head(), after: None });
    round_trip(done(DevicePage::new(vec![device(8), device(9)], true).unwrap()));
    round_trip(ForceOut { head: head(), device: id(9) });
    round_trip(done(ForcedOut { music_cut_off: true }));
    round_trip(done(ForcedOut { music_cut_off: false }));
    round_trip(ChangePassword {
        head: head(),
        request_id: request_id(),
        current_password: Some(Password::new(PASSWORD).unwrap()),
        new_password: Password::new("an even newer password").unwrap(),
    });
    round_trip(ChangePassword {
        head: head(),
        request_id: request_id(),
        current_password: None,
        new_password: Password::new("an even newer password").unwrap(),
    });
    round_trip(done(PasswordChanged {}));
    round_trip(GetMusicLogin { head: head() });
    round_trip(done(music()));
    round_trip(music());
}

#[test]
fn the_code_session_has_no_music_or_regular_sign_in_fields() {
    let value = done(SignInAnswer::PasswordSetup(PasswordSetup {
        session: session(),
        person: person(),
        device: id(7),
    }));
    let body = encode(&value).unwrap();
    let json: serde_json::Value = serde_json::from_slice(body.bytes()).unwrap();
    let result = json.pointer("/done/result").unwrap();
    assert!(result.get("signed_in").is_none());
    let setup = result.get("password_setup").unwrap().as_object().unwrap();
    assert_eq!(setup.keys().collect::<Vec<_>>(), ["device", "person", "session"]);
}

#[test]
fn bad_credentials_have_one_fixed_reply_and_cannot_carry_a_receipt() {
    let response: SignInReply = Reply::Refused(Refusal::WrongLogin);
    let body = encode(&response).unwrap();
    assert_eq!(body.bytes(), br#"{"refused":"wrong_login"}"#);
    assert_eq!(read_reply::<SignInAnswer>(200, body.bytes()), Ok(response));
    assert!(RefusedWithReceipt::new(Refusal::WrongLogin, receipt()).is_none());
    let reply = serde_json::json!({
        "refused_with_receipt": {"reason": "wrong_login", "receipt": receipt()}
    });
    assert_eq!(read::<SignInReply>(reply.to_string().as_bytes()), Err(BadMessage::Form));
}

#[test]
fn every_refusal_round_trips_and_other_statuses_are_nothing() {
    for reason in [
        Refusal::WrongLogin,
        Refusal::TooManyTries { wait: Wait::new(1).unwrap() },
        Refusal::AccountOff,
        Refusal::Busy,
        Refusal::StartAgain,
        Refusal::NotSignedIn,
        Refusal::NoSuchDevice,
        Refusal::PasswordNotAllowed,
        Refusal::PasswordSetupOnly,
        Refusal::BadRequest,
        Refusal::NoMusicLogin,
        Refusal::Incompatible { speaks: Speaks::new(21, 32).unwrap() },
    ] {
        round_trip(Reply::<SignInAnswer>::Refused(reason));
    }
    round_trip(Reply::<SignedOut>::RefusedWithReceipt(
        RefusedWithReceipt::new(Refusal::StartAgain, receipt()).unwrap(),
    ));

    let body = br#"{"refused":"wrong_login"}"#;
    for status in [201, 204, 302, 400, 401, 404, 405, 502] {
        assert_eq!(read_reply::<SignInAnswer>(status, body), Err(BadMessage::Status(status)));
    }
    assert_eq!(read_reply::<SignInAnswer>(200, b""), Err(BadMessage::Form));
    assert_eq!(read_reply::<SignInAnswer>(200, b"<html>"), Err(BadMessage::Form));
    for raw in [
        br#"{"refused":{"too_many_tries":{"wait":0}}}"#.as_slice(),
        br#"{"refused":{"too_many_tries":{"wait":900001}}}"#,
        br#"{"refused":{"too_many_tries":{"wait":"10"}}}"#,
        br#"{"refused":{"incompatible":{"speaks":[0,1]}}}"#,
        br#"{"refused":{"incompatible":{"speaks":[2,1]}}}"#,
        br#"{"refused":"no_such_account"}"#,
    ] {
        assert_eq!(read::<SignInReply>(raw), Err(BadMessage::Form));
    }
}

#[test]
fn only_a_complete_utf8_body_without_duplicate_keys_is_read() {
    type Answer = Reply<SignedOut>;
    let accepted = br#"{"refused":"wrong_login","future":{"a":[1,2,"x"]}}"#;
    // A valid body can have whitespace, and known keys are compared after unescaping.
    assert!(read::<Answer>(accepted).is_ok());
    assert!(read::<Answer>(br#" { "r\u0065fused": "wrong_login" } "#).is_ok());

    for end in 0..accepted.len() {
        assert_eq!(read::<Answer>(&accepted[..end]), Err(BadMessage::Form));
    }
    for raw in [
        br#"{"refused":"wrong_login"}{}"#.as_slice(),
        br#"{"refused":"wrong_login",}"#,
        br#"{"refused":"wrong_login","refused":"busy"}"#,
        br#"{"refused":"wrong_login","r\u0065fused":"busy"}"#,
        br#"{"refused":"wrong_login","later":null,"later":false}"#,
        br#"{"refused":"wrong_login","later":{"key":1,"k\u0065y":2}}"#,
        br#"{"refused":"wrong_login","later":[{"k":true,"k":false}]}"#,
        br#"{"refused":"wrong_login","later":[1,]}"#,
        br#"{"refused":"wrong_login","later":"\ud800"}"#,
        br#"{"refused":"wrong_login","later":"\udc00"}"#,
        br#"{"refused":"wrong_login","later":"\ud801\u007f"}"#,
        br#"{"refused":"wrong_login","later":"\ud801\ud801"}"#,
        br#"{"refused":"wrong_login","later":"\uabcdx\q"}"#,
        b"{\"refused\":\"wrong_login\",\"later\":\"\xff\"}",
        b"{\"refused\":\"wrong_login\",\"later\":\"\x01\"}",
        b"{\"refused\":\"wrong_login\",\"\xff\":true}",
    ] {
        assert_eq!(read::<Answer>(raw), Err(BadMessage::Form));
    }
}

#[test]
fn both_known_and_unknown_parts_obey_the_body_and_depth_limits() {
    type Answer = Reply<SignedOut>;
    let depth = |count: usize, open: &str, close: &str| {
        format!(
            "{{\"refused\":\"wrong_login\",\"later\":{}null{}}}",
            open.repeat(count),
            close.repeat(count)
        )
    };
    for (open, close) in [("[", "]"), ("{\"nested\":", "}")] {
        assert!(read::<Answer>(depth(15, open, close).as_bytes()).is_ok());
        assert_eq!(read::<Answer>(depth(16, open, close).as_bytes()), Err(BadMessage::Form));
    }
    let mut bytes = br#"{"refused":"wrong_login"}"#.to_vec();
    bytes.resize(BODY_MAX_BYTES, b' ');
    assert!(read::<Answer>(&bytes).is_ok());
    bytes.push(b' ');
    assert_eq!(read::<Answer>(&bytes), Err(BadMessage::TooLarge));
}

#[test]
fn escaped_passwords_keep_their_exact_characters() {
    let raw = format!(
        r#"{{"credential":{{"password":{{"email":"ADA@example.org","password":"word\u00e9\ud83d\udd11\b\t\n\r\f\\\"\/"}}}},"device_secret":"{}","device_name":"Ada's phone","device_kind":"phone","app_version":"1.2.3","speaks":[1,1],"restore":null}}"#,
        "21".repeat(32),
    );
    let value = read::<SignIn>(raw.as_bytes()).unwrap();
    let Credential::Password { email: account, password } = value.credential else {
        panic!("password credential required");
    };
    assert_eq!(account, email());
    assert_eq!(password, Password::new("wordé🔑\u{08}\t\n\r\u{0c}\\\"/").unwrap());
}

/// Try a value of the wrong type and then omit each named field.
fn bad_fields<T: wire::Message>(sample: &T, paths: &[&str]) {
    let body = encode(sample).unwrap();
    let json: serde_json::Value = serde_json::from_slice(body.bytes()).unwrap();
    for &path in paths {
        let mut bad = json.clone();
        *bad.pointer_mut(path).unwrap() = serde_json::json!({"not": "the right type"});
        assert!(matches!(read::<T>(bad.to_string().as_bytes()), Err(BadMessage::Form)), "{path}");
        let (parent, field) = path.rsplit_once('/').unwrap();
        let mut missing = json.clone();
        missing.pointer_mut(parent).unwrap().as_object_mut().unwrap().remove(field);
        assert!(matches!(read::<T>(missing.to_string().as_bytes()), Err(BadMessage::Form)), "{path}");
    }
}

#[test]
fn each_known_request_field_is_required_and_typed() {
    bad_fields(&signin(Credential::Password {
        email: email(),
        password: Password::new(PASSWORD).unwrap(),
    }), &[
        "/credential",
        "/credential/password",
        "/credential/password/email",
        "/credential/password/password",
        "/device_secret",
        "/device_name",
        "/device_kind",
        "/app_version",
        "/speaks",
        "/restore",
    ]);
    bad_fields(&signin(Credential::Code(OneTimeCode::new(CODE).unwrap())), &["/credential/code"]);
    bad_fields(&SignOut { head: head(), request_id: request_id() }, &[
        "/head", "/head/speaks", "/head/restore", "/request_id",
    ]);
    bad_fields(&ListDevices { head: head(), after: Some(id(7)) }, &["/head", "/after"]);
    bad_fields(&ForceOut { head: head(), device: id(7) }, &["/head", "/device"]);
    bad_fields(&GetMusicLogin { head: head() }, &["/head"]);
    bad_fields(&ChangePassword {
        head: head(),
        request_id: request_id(),
        current_password: None,
        new_password: Password::new(PASSWORD).unwrap(),
    }, &["/head", "/request_id", "/current_password", "/new_password"]);
    assert_eq!(read::<ForceOut>(br#"{"head":{"speaks":[1,1],"restore":0},"device":0}"#), Err(BadMessage::Form));
    assert_eq!(read::<SignOut>(br#"{"head":{"speaks":[1,1],"restore":0},"request_id":0}"#), Err(BadMessage::Form));
    assert_eq!(read::<SignOut>(br#"{"head":[[1,1],0],"request_id":1}"#), Err(BadMessage::Form));

    let setup = encode(&signin(Credential::Code(OneTimeCode::new(CODE).unwrap()))).unwrap();
    let mut competing: serde_json::Value = serde_json::from_slice(setup.bytes()).unwrap();
    competing["credential"]["password"] = serde_json::json!({
        "email": "ada@example.org", "password": PASSWORD
    });
    assert_eq!(read::<SignIn>(competing.to_string().as_bytes()), Err(BadMessage::Form));
}

#[test]
fn each_known_answer_field_is_required_and_typed() {
    bad_fields(&good_sign_in(), &[
        "/done/result/signed_in/session",
        "/done/result/signed_in/person",
        "/done/result/signed_in/person/email",
        "/done/result/signed_in/person/name",
        "/done/result/signed_in/person/admin",
        "/done/result/signed_in/device",
        "/done/result/signed_in/music",
        "/done/result/signed_in/music/address",
        "/done/result/signed_in/music/name",
        "/done/result/signed_in/music/salt",
        "/done/result/signed_in/music/token",
        "/done/result/signed_in/music/count",
        "/done/result/signed_in/notice",
        "/done/receipt",
        "/done/receipt/music_login_count",
        "/done/receipt/stamp",
        "/done/receipt/stamp/backend",
        "/done/receipt/stamp/restore",
        "/done/receipt/stamp/account",
        "/done/receipt/stamp/life",
        "/done/receipt/stamp/change",
        "/done/receipt/stamp/tag",
    ]);
    bad_fields(&done(DevicePage::new(vec![device(7)], false).unwrap()), &[
        "/done/result/devices/0/id",
        "/done/result/devices/0/name",
        "/done/result/devices/0/kind",
        "/done/result/devices/0/app_version",
        "/done/result/devices/0/last_seen_ms",
        "/done/result/devices/0/signed_in",
        "/done/result/more",
    ]);
    bad_fields(&done(ForcedOut { music_cut_off: false }), &["/done/result/music_cut_off"]);
}

#[test]
fn an_email_is_a_bounded_tidied_name() {
    assert_eq!(tidy_email("  ÜSER@Example.Org\u{2003}").as_deref(), Some("Üser@example.org"));
    for bad in ["", "@", "ada@", "@example.org", "a@@b", "a b@example.org", "ad\u{7f}a@example.org"] {
        assert!(tidy_email(bad).is_none());
    }
    let exact = format!("{}@b", "é".repeat(126));
    assert_eq!(exact.len(), EMAIL_MAX_BYTES);
    assert_eq!(tidy_email(&format!(" {exact} ")), Some(exact.clone()));
    assert!(tidy_email(&format!("{exact}b")).is_none());
}

#[test]
fn a_new_password_counts_characters_and_cannot_be_the_email() {
    assert!(Password::new(&"🔑".repeat(PASSWORD_MIN_CHARS - 1)).is_none());
    assert!(Password::new(&"🔑".repeat(PASSWORD_MIN_CHARS)).is_some());
    assert!(Password::new(&"🔑".repeat(PASSWORD_MAX_CHARS)).is_some());
    assert!(Password::new(&"🔑".repeat(PASSWORD_MAX_CHARS + 1)).is_none());
    assert!(Password::new("          ").unwrap().allowed_for(&email()));
    for text in ["ada@example.org", "ADA@EXAMPLE.ORG", "  Ada@example.org\u{2003}"] {
        assert!(!Password::new(text).unwrap().allowed_for(&email()));
    }
    assert!(Password::new("ada@another.example.org").unwrap().allowed_for(&email()));
    assert_ne!(Password::new(PASSWORD), Password::new(&format!("{PASSWORD}\0")));
    assert_ne!(Password::new("b made-up password"), Password::new(PASSWORD));
    assert_ne!(Password::new("a made-up passworD"), Password::new(PASSWORD));
}

#[test]
fn codes_use_exactly_twelve_symbols_and_exclude_each_look_alike() {
    let alphabet: BTreeSet<u8> = CODE_ALPHABET.iter().copied().collect();
    assert_eq!(alphabet.len(), 21);
    for &byte in CODE_ALPHABET {
        assert!(OneTimeCode::new(&char::from(byte).to_string().repeat(12)).is_some());
    }
    for bad in b"ABEGILOQSTZ0125" {
        assert!(!alphabet.contains(bad));
        assert!(OneTimeCode::new(&format!("CDFHJKMN347{}", char::from(*bad))).is_none());
    }
    assert!(OneTimeCode::new("cdfhjkmn3479").is_none());
    assert!(OneTimeCode::new("CDFHJKMN347").is_none());
    assert!(OneTimeCode::new("CDFHJKMN34799").is_none());
    assert_ne!(OneTimeCode::new(CODE), OneTimeCode::new("CDFHJKMN3478"));
}

#[test]
fn names_notices_and_music_addresses_obey_their_byte_bounds() {
    assert!(Name::new(&"é".repeat(NAME_MAX_BYTES / 2)).is_some());
    assert!(Name::new(&"é".repeat(NAME_MAX_BYTES / 2 + 1)).is_none());
    for bad in ["", "  ", "Ada\nPC", "Ada\0PC"] {
        assert!(Name::new(bad).is_none());
    }
    assert!(Notice::new(&"x".repeat(NOTICE_MAX_BYTES)).is_some());
    assert!(Notice::new(&"x".repeat(NOTICE_MAX_BYTES + 1)).is_none());
    assert!(Notice::new("Ada\nMusic").is_some());
    assert!(Notice::new("Ada\rMusic").is_none());
    assert!(Notice::new("").is_none());

    let base = "https://music.example.org/";
    let exact = format!("{base}{}", "a".repeat(MUSIC_ADDRESS_MAX_BYTES - base.len()));
    assert!(MusicAddress::new(&exact).is_some());
    assert!(MusicAddress::new(&format!("{exact}x")).is_none());
    for good in [ADDRESS, "https://[2001:db8::1]:443/music", "https://music.example.org/a%20b"] {
        assert!(MusicAddress::new(good).is_some());
    }
    for bad in [
        "http://music.example.org", "https://", "https:///music",
        "https://music.example.org ", "https://music.example.org/music box",
        "https://ada:example@music.example.org", "https://music.example.org?q=1",
        "https://music.example.org/#music", "https://music.example.org\\x",
        "https://music.example.org:65536", "https://[::::]/music",
        "https://music.example.org/a%", "https://music.example.org/a%xy",
        "https://music.example.org/no\u{7f}",
    ] {
        assert!(MusicAddress::new(bad).is_none());
    }
}

#[test]
fn binary_secrets_have_one_hex_spelling_and_sessions_have_one_header() {
    let secret = "ab".repeat(SECRET_BYTES);
    let device = DeviceSecret::from_hex(&secret).unwrap();
    let session = Session::from_hex(&secret).unwrap();
    assert_eq!(device.expose_bytes(), &[0xab; SECRET_BYTES]);
    assert_eq!(session.expose_hex().as_str(), secret);
    let header = session.authorization();
    assert_eq!(header.as_str(), format!("Bearer {secret}"));
    assert_eq!(Session::from_authorization(&header), Some(session.clone()));
    assert_eq!(Session::from_authorization(&format!("bEaReR {secret}")), Some(session));
    for wrong in [secret.clone(), format!("Bearer  {secret}"), format!("Bearer {secret} "), format!("Basic {secret}"), format!("Bearer {}", secret.to_ascii_uppercase())] {
        assert!(Session::from_authorization(&wrong).is_none());
    }
    for wrong in ["ab".repeat(31), "ab".repeat(33), "Ab".repeat(32), "g1".repeat(32)] {
        assert!(DeviceSecret::from_hex(&wrong).is_none());
        assert!(StampTag::from_hex(&wrong).is_none());
    }
    assert!(MusicToken::from_hex(&"ab".repeat(16)).is_some());
    assert!(MusicToken::from_hex(&"ab".repeat(15)).is_none());
    assert!(MusicToken::from_hex(&"AB".repeat(16)).is_none());
    assert!(MusicSalt::new(SALT).is_some());
    assert!(MusicSalt::new("AdaMusicSalt202!").is_none());
    assert!(MusicSalt::new("AdaMusicSalt202").is_none());

    let mut last_differs = [0xab; SECRET_BYTES];
    *last_differs.last_mut().unwrap() ^= 1;
    assert_ne!(device, DeviceSecret::from_bytes(last_differs));
}

#[test]
fn debug_and_wire_errors_reveal_no_secret() {
    let value = signin(Credential::Password {
        email: email(), password: Password::new(PASSWORD).unwrap(),
    });
    let body = encode(&value).unwrap();
    assert_eq!(format!("{body:?}"), "WireBody(..)");
    let code = signin(Credential::Code(OneTimeCode::new(CODE).unwrap()));
    let reply = good_sign_in();
    for (printed, hidden) in [
        (format!("{value:?}"), PASSWORD.to_string()),
        (format!("{value:#?}"), "21".repeat(32)),
        (format!("{code:?}"), CODE.to_string()),
        (format!("{reply:?}"), SALT.to_string()),
        (format!("{reply:#?}"), "2d".repeat(32)),
        (format!("{reply:?}"), "39".repeat(16)),
        (format!("{reply:?}"), "45".repeat(32)),
        (format!("{:?}", session().authorization()), "2d".repeat(32)),
    ] {
        assert!(!printed.contains(&hidden));
    }
    let body = format!(r#"{{"refused":"{PASSWORD}"}}"#);
    let bad = read::<SignInReply>(body.as_bytes()).unwrap_err();
    assert_eq!(format!("{bad:?}"), "Form");
}

#[test]
fn a_list_page_is_bounded_sorted_and_has_no_repeated_id() {
    assert!(DevicePage::new(Vec::new(), false).is_some());
    assert!(DevicePage::new(Vec::new(), true).is_none());
    assert!(DevicePage::new(vec![device(1), device(1)], true).is_none());
    assert!(DevicePage::new(vec![device(2), device(1)], false).is_none());
    let full = (1..=DEVICES_PER_PAGE as u64).map(device).collect();
    let full = DevicePage::new(full, true).unwrap();
    assert_eq!(full.next(), Some(id(DEVICES_PER_PAGE as u64)));
    round_trip(done(full));
    let over = (1..=(DEVICES_PER_PAGE as u64 + 1)).map(device).collect::<Vec<_>>();
    let body = serde_json::json!({
        "done": {"result": {"devices": over, "more": false}, "receipt": receipt()}
    });
    assert_eq!(read::<ListDevicesReply>(body.to_string().as_bytes()), Err(BadMessage::Form));
    assert!(DevicePage::new(over, false).is_none());
}

#[test]
fn a_login_must_agree_with_its_stamp_and_count_in_both_directions() {
    let mut value = good_sign_in();
    if let Reply::Done { receipt, .. } = &mut value {
        receipt.stamp.account = Email::new("another@example.org").unwrap();
    }
    assert!(matches!(encode(&value), Err(BadMessage::Form)));
    // Deliberately bypass the checked encoder to get the same bad object to the reader.
    let bytes = serde_json::to_vec(&value).unwrap();
    assert_eq!(read::<SignInReply>(&bytes), Err(BadMessage::Form));
    let mut wrong_count = music();
    wrong_count.count = 5;
    let value = done(wrong_count);
    assert!(matches!(encode(&value), Err(BadMessage::Form)));
    assert_eq!(read::<MusicLoginReply>(&serde_json::to_vec(&value).unwrap()), Err(BadMessage::Form));
}

#[test]
fn the_fifth_wrong_password_locks_until_the_first_miss_is_fifteen_minutes_old() {
    let first = 600;
    let mut failures = AccountFailures::default();
    assert!(failures.is_expired(first));
    for n in 0..u64::from(WRONG_PASSWORDS) {
        let now = first + 37 * n;
        assert_eq!(failures.retry_after(now), None);
        failures.record_wrong(now);
    }
    assert_eq!(failures.retry_after(first - 1).map(Wait::ms), Some(WRONG_PASSWORD_WINDOW_MS));
    let end = first + WRONG_PASSWORD_WINDOW_MS;
    assert_eq!(failures.retry_after(end - 1).map(Wait::ms), Some(1));
    assert!(!failures.is_expired(end - 1));
    assert_eq!(failures.retry_after(end), None);
    assert!(failures.is_expired(end));
    failures.record_wrong(end);
    assert_eq!(failures.retry_after(end + 1), None);
}

#[test]
fn successful_checks_leave_the_wrong_password_count_and_its_start_alone() {
    let mut failures = AccountFailures::default();
    for _ in 0..4 {
        failures.record_wrong(100);
    }
    // A successful password check only asks the gate. It does not update failures.
    assert_eq!(failures.retry_after(110), None);
    failures.record_wrong(120);
    assert_eq!(failures.retry_after(121).map(Wait::ms), Some(WRONG_PASSWORD_WINDOW_MS - 21));
    assert_eq!(AccountFailures::default().retry_after(121), None);
}

fn wait_ms(result: Result<(), Refusal>) -> Option<u64> {
    match result {
        Ok(()) => None,
        Err(Refusal::TooManyTries { wait }) => Some(wait.ms()),
        Err(other) => panic!("unexpected refusal {other:?}"),
    }
}

#[test]
fn a_restart_takes_no_sign_ins_until_twenty_seconds_have_passed() {
    let start = 1000;
    let mut limit = SignInLimit::new(start);
    assert_eq!(wait_ms(limit.admit(start - 1)), Some(20_000));
    assert_eq!(wait_ms(limit.admit(start)), Some(20_000));
    assert_eq!(wait_ms(limit.admit(start + 19_999)), Some(1));
    assert_eq!(limit.admit(start + 20_000), Ok(()));
    let mut restart = SignInLimit::new(start + 25_000);
    assert_eq!(wait_ms(restart.admit(start + 25_000)), Some(20_000));
    let mut last = SignInLimit::new(u64::MAX - STARTUP_QUIET_MS);
    assert_eq!(wait_ms(last.admit(u64::MAX - 1)), Some(1));
    assert_eq!(last.admit(u64::MAX), Ok(()));
}

#[test]
fn every_rolling_minute_takes_at_most_thirty_sign_ins() {
    let mut limit = SignInLimit::new(0);
    let first = 59_999;
    for time in first..first + 30 {
        assert_eq!(limit.admit(time), Ok(()));
    }
    assert_eq!(wait_ms(limit.admit(first + 30)), Some(59_970));
    assert_eq!(wait_ms(limit.admit(first + 1)), Some(59_970));
    for oldest in first..first + 30 {
        assert_eq!(wait_ms(limit.admit(oldest + 60_000 - 1)), Some(1));
        assert_eq!(limit.admit(oldest + 60_000), Ok(()));
    }
}

#[test]
fn password_work_uses_two_places_then_eight_waiting_places() {
    assert_eq!(admit_password_check(0, 0), PasswordAdmission::Check);
    assert_eq!(admit_password_check(1, 0), PasswordAdmission::Check);
    for waiting in 0..8 {
        assert_eq!(admit_password_check(2, waiting), PasswordAdmission::Wait);
    }
    assert_eq!(admit_password_check(2, 8), PasswordAdmission::Busy);
    assert_eq!(admit_password_check(1, 1), PasswordAdmission::Wait);
    assert_eq!(admit_password_check(usize::MAX, usize::MAX), PasswordAdmission::Busy);
}

#[test]
fn session_values_use_strict_numbers_version_and_protocol_range() {
    let sample = SignOut {
        head: RequestHead { speaks: Speaks::new(u32::MAX, u32::MAX).unwrap(), restore: u64::MAX },
        request_id: RequestId::new(u64::MAX).unwrap(),
    };
    round_trip(sample);
    for text in [
        r#"{"head":{"speaks":[1],"restore":3},"request_id":2}"#,
        r#"{"head":{"speaks":[1,1,1],"restore":3},"request_id":2}"#,
        r#"{"head":{"speaks":[1,4294967296],"restore":3},"request_id":2}"#,
        r#"{"head":{"speaks":[1,1],"restore":18446744073709551616},"request_id":2}"#,
        r#"{"head":{"speaks":[1,1],"restore":-1},"request_id":2}"#,
        r#"{"head":{"speaks":[1,1],"restore":1.0},"request_id":2}"#,
        r#"{"head":{"speaks":[1,1],"restore":3},"request_id":-1}"#,
    ] {
        assert_eq!(read::<SignOut>(text.as_bytes()), Err(BadMessage::Form));
    }
    let sample = signin(Credential::Code(OneTimeCode::new(CODE).unwrap()));
    let mut json = serde_json::to_value(sample).unwrap();
    for version in ["01.2.3", "1.2", "1.2.3-alpha", "4294967296.2.3"] {
        json["app_version"] = serde_json::Value::String(version.to_owned());
        assert_eq!(read::<SignIn>(json.to_string().as_bytes()), Err(BadMessage::Form));
    }
    json["app_version"] = serde_json::Value::String("4294967295.4294967295.4294967295".to_owned());
    assert!(read::<SignIn>(json.to_string().as_bytes()).is_ok());
}

#[test]
fn the_sign_in_and_retry_wire_spellings_are_fixed() {
    let value = signin(Credential::Password { email: email(), password: Password::new(PASSWORD).unwrap() });
    let body = encode(&value).unwrap();
    let expected = format!(
        r#"{{"credential":{{"password":{{"email":"ada@example.org","password":"{PASSWORD}"}}}},"device_secret":"{}","device_name":"Ada's phone","device_kind":"phone","app_version":"1.2.3","speaks":[1,1],"restore":null}}"#,
        "21".repeat(SECRET_BYTES),
    );
    assert!(body.bytes() == expected.as_bytes());
    let wire = encode(&SignOut { head: head(), request_id: request_id() }).unwrap();
    assert_eq!(wire.bytes(), br#"{"head":{"speaks":[1,1],"restore":8},"request_id":9}"#);
}
