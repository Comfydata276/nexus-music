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
    assert_eq!(PATHS.len(), 7);
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
    round_trip(signin(code_credential()));
}

const PASSWORD: &str = "a made-up password";
const SALT: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const ADDRESS: &str = "https://music.example.org:8443/navidrome";

fn email() -> Email {
    Email::new("ada@example.org").unwrap()
}

fn person() -> Person {
    Person { id: AccountId::new(4).unwrap(), revision: 16, email: email(), name: Name::new("Ada").unwrap(), admin: false }
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
            account: AccountId::new(4).unwrap(),
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

fn code_credential() -> Credential {
    Credential::Code {
        code: OneTimeCode::new(CODE).unwrap(),
        new_password: Password::new(PASSWORD).unwrap(),
    }
}

fn sid(number: u64) -> SessionId {
    SessionId::new(number).unwrap()
}

fn sign_out_note() -> SignOutNote {
    SignOutNote { session: sid(11), secret: SignOutSecret::from_bytes([81; SECRET_BYTES]) }
}

fn good_sign_in() -> SignInReply {
    done(SignedIn {
        person: person(),
        device: id(7),
        session: session(),
        session_id: sid(11),
        sign_out: sign_out_note(),
        restore: 8,
        music: MusicLoginResult::Login(music()),
        notice: Notice::new("What this server records\nWhat you play.").unwrap(),
    })
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
        session: Some(sid(number)),
        this_device: false,
    }
}

#[test]
fn every_call_and_success_reply_round_trips() {
    round_trip(good_sign_in());
    for music in [MusicLoginResult::NoneSetYet, MusicLoginResult::UnusableNow] {
        let Reply::Done { mut result, receipt } = good_sign_in() else { unreachable!() };
        result.music = music;
        round_trip(Reply::Done { result, receipt });
    }
    round_trip(SignOut { head: head(), request_id: request_id(), revoke: None });
    round_trip(SignOut { head: head(), request_id: request_id(), revoke: Some(sign_out_note()) });
    round_trip(done(SignedOut {}));
    round_trip(ListDevices { head: head(), after: Some(id(7)) });
    round_trip(ListDevices { head: head(), after: None });
    let mut inactive = device(9);
    inactive.session = None;
    let mut own = device(8);
    own.this_device = true;
    round_trip(done(DevicePage::new(vec![own, inactive], true).unwrap()));
    round_trip(ForceOut { head: head(), device: id(9), session: sid(11) });
    round_trip(done(ForcedOut { music_cut_off: true }));
    round_trip(done(ForcedOut { music_cut_off: false }));
    round_trip(ChangePassword {
        head: head(),
        request_id: request_id(),
        current_password: Password::new(PASSWORD).unwrap(),
        new_password: Password::new("an even newer password").unwrap(),
    });
    round_trip(done(PasswordChanged {}));
    round_trip(GetMusicLogin { head: head() });
    round_trip(done(MusicLoginResult::Login(music())));
    round_trip(done(MusicLoginResult::NoneSetYet));
    round_trip(done(MusicLoginResult::UnusableNow));
    round_trip(music());
    round_trip(GrantRequest {
        head: head(),
        change: GrantChange::ChangePassword { account: person().id },
        password: Password::new(PASSWORD).unwrap(),
    });
    round_trip(GrantedChange {
        head: head(),
        request_id: request_id(),
        change: GrantChange::DeleteListeningData { account: person().id },
        grant: GrantSecret::from_bytes([21; SECRET_BYTES]),
    });
    round_trip(done(GrantIssued { grant: GrantSecret::from_bytes([21; SECRET_BYTES]) }));
}

#[test]
fn a_code_needs_its_new_password_and_change_needs_its_current_password() {
    let sample = encode(&signin(code_credential())).unwrap();
    let mut json: serde_json::Value = serde_json::from_slice(sample.bytes()).unwrap();
    json["credential"]["code"] = serde_json::json!(CODE);
    assert_eq!(read::<SignIn>(json.to_string().as_bytes()), Err(BadMessage::Form));
    let incomplete = br#"{"head":{"speaks":[1,1],"restore":0},"request_id":1,"current_password":null,"new_password":"a valid new password"}"#;
    assert_eq!(read::<ChangePassword>(incomplete), Err(BadMessage::Form));
}

#[test]
fn bad_credentials_have_one_fixed_reply_and_cannot_carry_a_receipt() {
    let response: SignInReply = Reply::Refused(Refusal::NotAllowed(NotAllowed::WrongLogin));
    let body = encode(&response).unwrap();
    assert_eq!(body.bytes(), br#"{"refused":{"not_allowed":"wrong_login"}}"#);
    assert_eq!(response.status(), 403);
    assert_eq!(read_reply::<SignInAnswer>(403, body.bytes()), Ok(response));
    assert!(RefusedWithReceipt::new(Refusal::NotAllowed(NotAllowed::WrongLogin), receipt()).is_none());
    let reply = serde_json::json!({
        "refused_with_receipt": {"reason": {"not_allowed": "wrong_login"}, "receipt": receipt()}
    });
    assert_eq!(read::<SignInReply>(reply.to_string().as_bytes()), Err(BadMessage::Form));
}

#[test]
fn each_answer_kind_has_its_own_status_and_refusal_round_trips() {
    for (status, reason) in [
        (400, Refusal::Malformed),
        (401, Refusal::NotSignedIn(NotSignedIn::SessionEnded)),
        (401, Refusal::NotSignedIn(NotSignedIn::DataPutBack)),
        (403, Refusal::NotAllowed(NotAllowed::Denied)),
        (403, Refusal::NotAllowed(NotAllowed::WrongLogin)),
        (403, Refusal::NotAllowed(NotAllowed::AccountOff)),
        (403, Refusal::NotAllowed(NotAllowed::NoSuchDevice)),
        (403, Refusal::NotAllowed(NotAllowed::PasswordNotAllowed)),
        (403, Refusal::NotAllowed(NotAllowed::Incompatible { speaks: Speaks::new(21, 32).unwrap() })),
        (429, Refusal::Busy { retry_after_seconds: Some(Wait::new(60).unwrap()) }),
        (429, Refusal::Busy { retry_after_seconds: None }),
        (503, Refusal::NotDealtWith),
    ] {
        let reply = Reply::<SignInAnswer>::Refused(reason);
        let body = encode(&reply).unwrap();
        assert_eq!(reply.status(), status);
        assert_eq!(read_reply::<SignInAnswer>(status, body.bytes()), Ok(reply));
        assert_eq!(read_reply::<SignInAnswer>(200, body.bytes()), Err(BadMessage::Status(200)));
    }
    let sample = good_sign_in();
    let body = encode(&sample).unwrap();
    assert_eq!(sample.status(), 200);
    assert_eq!(read_reply::<SignInAnswer>(200, body.bytes()), Ok(sample));
    assert_eq!(read_reply::<SignInAnswer>(429, body.bytes()), Err(BadMessage::Status(429)));
    round_trip(Reply::<SignedOut>::RefusedWithReceipt(
        RefusedWithReceipt::new(Refusal::NotAllowed(NotAllowed::Denied), receipt()).unwrap(),
    ));
    assert!(RefusedWithReceipt::new(Refusal::NotSignedIn(NotSignedIn::DataPutBack), receipt()).is_none());

    let body = br#"{"refused":"not_dealt_with"}"#;
    for status in [201, 204, 302, 404, 405, 502] {
        assert_eq!(read_reply::<SignInAnswer>(status, body), Err(BadMessage::Status(status)));
    }
    assert_eq!(read_reply::<SignInAnswer>(200, b""), Err(BadMessage::Form));
    assert_eq!(read_reply::<SignInAnswer>(400, b"<html>"), Err(BadMessage::Form));
    for raw in [
        br#"{"refused":{"busy":{"retry_after_seconds":0}}}"#.as_slice(),
        br#"{"refused":{"busy":{"retry_after_seconds":61}}}"#,
        br#"{"refused":{"busy":{"retry_after_seconds":"10"}}}"#,
        br#"{"refused":{"not_allowed":{"incompatible":{"speaks":[0,1]}}}}"#,
        br#"{"refused":{"not_allowed":{"incompatible":{"speaks":[2,1]}}}}"#,
        br#"{"refused":{"not_allowed":"no_such_account"}}"#,
    ] {
        assert_eq!(read::<SignInReply>(raw), Err(BadMessage::Form));
    }
}

#[test]
fn only_a_complete_utf8_body_without_duplicate_keys_is_read() {
    type Answer = Reply<SignedOut>;
    let accepted = br#"{"refused":"not_dealt_with","future":{"a":[1,2,"x"]}}"#;
    // A valid body can have whitespace, and known keys are compared after unescaping.
    assert!(read::<Answer>(accepted).is_ok());
    assert!(read::<Answer>(br#" { "r\u0065fused": "not_dealt_with" } "#).is_ok());

    for end in 0..accepted.len() {
        assert_eq!(read::<Answer>(&accepted[..end]), Err(BadMessage::Form));
    }
    for raw in [
        br#"{"refused":"not_dealt_with"}{}"#.as_slice(),
        br#"{"refused":"not_dealt_with",}"#,
        br#"{"refused":"not_dealt_with","refused":"busy"}"#,
        br#"{"refused":"not_dealt_with","r\u0065fused":"busy"}"#,
        br#"{"refused":"not_dealt_with","later":null,"later":false}"#,
        br#"{"refused":"not_dealt_with","later":{"key":1,"k\u0065y":2}}"#,
        br#"{"refused":"not_dealt_with","later":[{"k":true,"k":false}]}"#,
        br#"{"refused":"not_dealt_with","later":[1,]}"#,
        br#"{"refused":"not_dealt_with","later":"\ud800"}"#,
        br#"{"refused":"not_dealt_with","later":"\udc00"}"#,
        br#"{"refused":"not_dealt_with","later":"\ud801\u007f"}"#,
        br#"{"refused":"not_dealt_with","later":"\ud801\ud801"}"#,
        br#"{"refused":"not_dealt_with","later":"\uabcdx\q"}"#,
        b"{\"refused\":\"not_dealt_with\",\"later\":\"\xff\"}",
        b"{\"refused\":\"not_dealt_with\",\"later\":\"\x01\"}",
        b"{\"refused\":\"not_dealt_with\",\"\xff\":true}",
    ] {
        assert_eq!(read::<Answer>(raw), Err(BadMessage::Form));
    }
}

#[test]
fn both_known_and_unknown_parts_obey_the_body_and_depth_limits() {
    type Answer = Reply<SignedOut>;
    let depth = |count: usize, open: &str, close: &str| {
        format!(
            "{{\"refused\":\"not_dealt_with\",\"later\":{}null{}}}",
            open.repeat(count),
            close.repeat(count)
        )
    };
    for (open, close) in [("[", "]"), ("{\"nested\":", "}")] {
        assert!(read::<Answer>(depth(15, open, close).as_bytes()).is_ok());
        assert_eq!(read::<Answer>(depth(16, open, close).as_bytes()), Err(BadMessage::Form));
    }
    let mut bytes = br#"{"refused":"not_dealt_with"}"#.to_vec();
    bytes.resize(SMALL_REPLY_MAX_BYTES, b' ');
    assert!(read::<Answer>(&bytes).is_ok());
    bytes.push(b' ');
    assert_eq!(read::<Answer>(&bytes), Err(BadMessage::TooLarge));
}

#[test]
fn escaped_passwords_keep_their_exact_characters() {
    let raw = format!(
        r#"{{"credential":{{"password":{{"email":"ADA@example.org","password":"words\u00e9\ud83d\udd11\b\t\n\r\f\\\"\/"}}}},"device_secret":"{}","device_name":"Ada's phone","device_kind":"phone","app_version":"1.2.3","speaks":[1,1],"restore":null}}"#,
        "21".repeat(32),
    );
    let value = read::<SignIn>(raw.as_bytes()).unwrap();
    let Credential::Password { email: account, password } = value.credential else {
        panic!("password credential required");
    };
    assert_eq!(account, email());
    assert_eq!(password, Password::new("wordsé🔑\u{08}\t\n\r\u{0c}\\\"/").unwrap());
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
    bad_fields(&signin(code_credential()), &["/credential/code", "/credential/code/code", "/credential/code/new_password"]);
    bad_fields(&SignOut { head: head(), request_id: request_id(), revoke: None }, &[
        "/head", "/head/speaks", "/head/restore", "/request_id", "/revoke",
    ]);
    bad_fields(&ListDevices { head: head(), after: Some(id(7)) }, &["/head", "/after"]);
    bad_fields(&ForceOut { head: head(), device: id(7), session: sid(11) }, &["/head", "/device", "/session"]);
    bad_fields(&GetMusicLogin { head: head() }, &["/head"]);
    bad_fields(&ChangePassword {
        head: head(),
        request_id: request_id(),
        current_password: Password::new(PASSWORD).unwrap(),
        new_password: Password::new(PASSWORD).unwrap(),
    }, &["/head", "/request_id", "/current_password", "/new_password"]);
    assert_eq!(read::<ForceOut>(br#"{"head":{"speaks":[1,1],"restore":0},"device":0,"session":1}"#), Err(BadMessage::Form));
    assert_eq!(read::<SignOut>(br#"{"head":{"speaks":[1,1],"restore":0},"request_id":0,"revoke":null}"#), Err(BadMessage::Form));
    assert_eq!(read::<SignOut>(br#"{"head":[[1,1],0],"request_id":1,"revoke":null}"#), Err(BadMessage::Form));

    let setup = encode(&signin(code_credential())).unwrap();
    let mut competing: serde_json::Value = serde_json::from_slice(setup.bytes()).unwrap();
    competing["credential"]["password"] = serde_json::json!({
        "email": "ada@example.org", "password": PASSWORD
    });
    assert_eq!(read::<SignIn>(competing.to_string().as_bytes()), Err(BadMessage::Form));
}

#[test]
fn each_known_answer_field_is_required_and_typed() {
    bad_fields(&good_sign_in(), &[
        "/done/result/session",
        "/done/result/session_id",
        "/done/result/sign_out",
        "/done/result/sign_out/session",
        "/done/result/sign_out/secret",
        "/done/result/restore",
        "/done/result/person",
        "/done/result/person/id",
        "/done/result/person/revision",
        "/done/result/person/email",
        "/done/result/person/name",
        "/done/result/person/admin",
        "/done/result/device",
        "/done/result/music",
        "/done/result/music/login",
        "/done/result/music/login/address",
        "/done/result/music/login/name",
        "/done/result/music/login/salt",
        "/done/result/music/login/token",
        "/done/result/music/login/count",
        "/done/result/notice",
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
        "/done/result/devices/0/session",
        "/done/result/devices/0/this_device",
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
    assert!(Password::new("               ").unwrap().allowed_for(&email(), ""));
    for text in ["ada@example.org", "ADA@EXAMPLE.ORG", "  Ada@example.org\u{2003}"] {
        assert!(!Password::new(text).unwrap().allowed_for(&email(), ""));
    }
    assert!(Password::new("ada@another.example.org").unwrap().allowed_for(&email(), ""));
    assert_ne!(Password::new(PASSWORD), Password::new(&format!("{PASSWORD}\0")));
    assert_ne!(Password::new("b made-up password"), Password::new(PASSWORD));
    assert_ne!(Password::new("a made-up passworD"), Password::new(PASSWORD));
}

#[test]
fn codes_use_twelve_crockford_symbols_and_normalize_typed_aliases() {
    let alphabet: BTreeSet<u8> = CODE_ALPHABET.iter().copied().collect();
    assert_eq!(alphabet.len(), 32);
    for &byte in CODE_ALPHABET {
        assert!(OneTimeCode::new(&char::from(byte).to_string().repeat(12)).is_some());
    }
    assert_eq!(OneTimeCode::new("oIlCDFHJKMPq").unwrap().expose(), "011CDFHJKMPQ");
    assert_eq!(OneTimeCode::new("cdfhjkmn3479"), OneTimeCode::new(CODE));
    for bad in ["CDFHJKMN347U", "CDFHJKMN347-", "CDFHJKMN347", "CDFHJKMN34799"] {
        assert!(OneTimeCode::new(bad).is_none());
    }
    assert_ne!(OneTimeCode::new(CODE), OneTimeCode::new("CDFHJKMN3478"));
    assert_eq!(OneTimeCode::from_random(0).expose(), "000000000000");
    assert_eq!(OneTimeCode::from_random(u64::MAX).expose(), "ZZZZZZZZZZZZ");
    assert_eq!(OneTimeCode::from_random(32).expose(), "000000000010");
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
    for good in [ADDRESS, "https://[2001:db8::1]:443/music", "https://music.example.org/a-b"] {
        assert!(MusicAddress::new(good).is_some());
    }
    for bad in [
        "http://music.example.org", "https://", "https:///music",
        "https://music.example.org ", "https://music.example.org/music box",
        "https://ada:example@music.example.org", "https://music.example.org?q=1",
        "https://music.example.org/#music", "https://music.example.org\\x",
        "https://music.example.org:65536", "https://[::::]/music",
        "https://music.example.org/a%", "https://music.example.org/a%xy", "https://music.example.org/a%20b",
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
    assert!(MusicSalt::new(&SALT.to_ascii_uppercase()).is_none());
    assert!(MusicSalt::new(&"ab".repeat(31)).is_none());
    assert_eq!(MusicSalt::from_random([0xab; SALT_BYTES]).as_str(), "ab".repeat(SALT_BYTES));

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
    let code = signin(code_credential());
    let reply = good_sign_in();
    for (printed, hidden) in [
        (format!("{value:?}"), PASSWORD.to_string()),
        (format!("{value:#?}"), "21".repeat(32)),
        (format!("{code:?}"), CODE.to_string()),
        (format!("{reply:?}"), SALT.to_string()),
        (format!("{reply:#?}"), "2d".repeat(32)),
        (format!("{reply:?}"), "39".repeat(16)),
        (format!("{reply:?}"), "45".repeat(32)),
        (format!("{reply:?}"), "51".repeat(32)),
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
        receipt.stamp.account = AccountId::new(5).unwrap();
    }
    assert!(matches!(encode(&value), Err(BadMessage::Form)));
    // Deliberately bypass the checked encoder to get the same bad object to the reader.
    let bytes = serde_json::to_vec(&value).unwrap();
    assert_eq!(read::<SignInReply>(&bytes), Err(BadMessage::Form));
    let mut wrong_count = music();
    wrong_count.count = 5;
    let value = done(MusicLoginResult::Login(wrong_count));
    assert!(matches!(encode(&value), Err(BadMessage::Form)));
    assert_eq!(read::<MusicLoginReply>(&serde_json::to_vec(&value).unwrap()), Err(BadMessage::Form));
}

#[test]
fn session_values_use_strict_numbers_version_and_protocol_range() {
    let sample = SignOut {
        head: RequestHead { speaks: Speaks::new(u32::MAX, u32::MAX).unwrap(), restore: u64::MAX },
        request_id: RequestId::new(u64::MAX).unwrap(),
        revoke: None,
    };
    round_trip(sample);
    for text in [
        r#"{"head":{"speaks":[1],"restore":3},"request_id":2,"revoke":null}"#,
        r#"{"head":{"speaks":[1,1,1],"restore":3},"request_id":2,"revoke":null}"#,
        r#"{"head":{"speaks":[1,4294967296],"restore":3},"request_id":2,"revoke":null}"#,
        r#"{"head":{"speaks":[1,1],"restore":18446744073709551616},"request_id":2,"revoke":null}"#,
        r#"{"head":{"speaks":[1,1],"restore":-1},"request_id":2,"revoke":null}"#,
        r#"{"head":{"speaks":[1,1],"restore":1.0},"request_id":2,"revoke":null}"#,
        r#"{"head":{"speaks":[1,1],"restore":3},"request_id":-1,"revoke":null}"#,
    ] {
        assert_eq!(read::<SignOut>(text.as_bytes()), Err(BadMessage::Form));
    }
    let sample = signin(code_credential());
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
    let wire = encode(&SignOut { head: head(), request_id: request_id(), revoke: None }).unwrap();
    assert_eq!(wire.bytes(), br#"{"head":{"speaks":[1,1],"restore":8},"request_id":9,"revoke":null}"#);
}

#[test]
fn nexus_origins_come_from_the_typed_name_and_music_is_checked_on_handover() {
    let nexus = NexusOrigin::typed("Nexus.example.org:8443").unwrap();
    let sign_in = nexus.call(SIGN_IN).unwrap();
    assert_eq!(sign_in.wire(), "https://nexus.example.org:8443/nexus/api/signin");
    for input in [
        "http://nexus.example.org", "nexus.example.org/extra", "nexus.example.org?",
        "nexus.example.org#test", "ada@nexus.example.org",
    ] {
        assert!(NexusOrigin::typed(input).is_err());
    }
    assert!(nexus.call("/elsewhere").is_err());
    let music = MusicAddress::new(ADDRESS).unwrap();
    assert!(music.handed_over(&nexus, HandoverSource::DeviceTokenAnswer(&sign_in)).is_ok());
    assert!(music.handed_over(&nexus, HandoverSource::Other).is_err());
    let other = NexusOrigin::typed("other.example.org").unwrap().call(SIGN_IN).unwrap();
    assert!(music.handed_over(&nexus, HandoverSource::DeviceTokenAnswer(&other)).is_err());
}

#[test]
fn a_new_password_is_checked_against_the_backend_list() {
    let list = "not this password\r\n a made-up password \r\nanother long password";
    assert!(!Password::new(PASSWORD).unwrap().allowed_for(&email(), list));
    assert!(!Password::new(" A MADE-UP PASSWORD ").unwrap().allowed_for(&email(), list));
    assert!(Password::new("a different password").unwrap().allowed_for(&email(), list));
}

#[test]
fn request_and_reply_ceilings_depend_on_the_call_and_status() {
    fn padded(body: &WireBody, len: usize) -> Vec<u8> {
        let mut bytes = body.bytes().to_vec();
        assert!(bytes.len() <= len);
        bytes.resize(len, b' ');
        bytes
    }

    let request = encode(&signin(code_credential())).unwrap();
    assert!(read::<SignIn>(&padded(&request, PASSWORD_REQUEST_MAX_BYTES)).is_ok());
    assert_eq!(read::<SignIn>(&padded(&request, PASSWORD_REQUEST_MAX_BYTES + 1)), Err(BadMessage::TooLarge));
    let request = encode(&ListDevices { head: head(), after: None }).unwrap();
    assert!(read::<ListDevices>(&padded(&request, SMALL_REQUEST_MAX_BYTES)).is_ok());
    assert_eq!(read::<ListDevices>(&padded(&request, SMALL_REQUEST_MAX_BYTES + 1)), Err(BadMessage::TooLarge));

    let first = encode(&good_sign_in()).unwrap();
    assert!(read_reply::<SignInAnswer>(200, &padded(&first, SIGN_IN_REPLY_MAX_BYTES)).is_ok());
    assert_eq!(read_reply::<SignInAnswer>(200, &padded(&first, SIGN_IN_REPLY_MAX_BYTES + 1)), Err(BadMessage::TooLarge));

    let list = encode(&done(DevicePage::new(vec![device(1)], false).unwrap())).unwrap();
    assert!(read_reply::<DevicePage>(200, &padded(&list, LIST_REPLY_MAX_BYTES)).is_ok());
    assert_eq!(read_reply::<DevicePage>(200, &padded(&list, LIST_REPLY_MAX_BYTES + 1)), Err(BadMessage::TooLarge));

    let refusal = encode(&Reply::<DevicePage>::Refused(Refusal::Malformed)).unwrap();
    assert!(read_reply::<DevicePage>(400, &padded(&refusal, SMALL_REPLY_MAX_BYTES)).is_ok());
    assert_eq!(read_reply::<DevicePage>(400, &padded(&refusal, SMALL_REPLY_MAX_BYTES + 1)), Err(BadMessage::TooLarge));
}

#[test]
fn sign_out_can_only_revoke_the_session_named_by_its_separate_secret() {
    let Reply::Done { mut result, receipt } = good_sign_in() else { unreachable!() };
    result.sign_out.session = sid(12);
    let wrong_id = Reply::Done { result, receipt };
    assert!(matches!(encode(&wrong_id), Err(BadMessage::Form)));
    assert_eq!(read::<SignInReply>(&serde_json::to_vec(&wrong_id).unwrap()), Err(BadMessage::Form));

    let Reply::Done { mut result, receipt } = good_sign_in() else { unreachable!() };
    result.sign_out.secret = SignOutSecret::from_bytes(*result.session.expose_bytes());
    assert!(matches!(encode(&Reply::Done { result, receipt }), Err(BadMessage::Form)));
    // The note is a body credential. Its encoding is not a session Authorization value.
    assert!(Session::from_authorization(sign_out_note().secret.expose_hex().as_str()).is_none());
}

#[test]
fn a_force_out_retry_keeps_the_old_session_number() {
    let force = ForceOut { head: head(), device: id(7), session: sid(11) };
    let encoded = encode(&force).unwrap();
    let live = Device { session: Some(sid(12)), ..device(7) };
    let retried: ForceOut = read(encoded.bytes()).unwrap();
    assert_eq!(retried.device, live.id);
    assert_ne!(Some(retried.session), live.session);
}

fn load(worker_busy: bool, account_busy: bool, waiting: usize) -> PasswordLoad {
    PasswordLoad { worker_busy, account_busy, waiting }
}

#[test]
fn the_known_device_worker_is_isolated_and_has_no_startup_closure() {
    let email = email();
    let mut waits = PasswordWaits::default();
    assert_eq!(PasswordDoor::for_device(true), PasswordDoor::KnownDevice);
    assert_eq!(PasswordDoor::for_device(false), PasswordDoor::Other);
    assert_eq!(admit_password_check(PasswordDoor::KnownDevice, load(false, false, 0), &waits, &email, 0), PasswordAdmission::Check);
    waits.record_wrong(PasswordDoor::KnownDevice, &email, 0);
    assert_eq!(waits.retry_after(&email, 0), None);

    waits.record_wrong(PasswordDoor::Other, &email, 0);
    assert_eq!(admit_password_check(PasswordDoor::Other, load(true, true, usize::MAX), &waits, &email, 0),
        PasswordAdmission::ComeBack(Wait::new(1).unwrap()));
    assert_eq!(admit_password_check(PasswordDoor::KnownDevice, load(false, false, usize::MAX), &waits, &email, 0),
        PasswordAdmission::Check);
    for busy in [load(true, false, 0), load(false, true, 0)] {
        assert_eq!(admit_password_check(PasswordDoor::KnownDevice, busy, &waits, &email, 0), PasswordAdmission::Busy);
    }
}

#[test]
fn the_other_door_waits_for_its_worker_for_ten_seconds_and_holds_eight() {
    let email = email();
    let waits = PasswordWaits::default();
    let door = PasswordDoor::Other;
    assert_eq!(admit_password_check(door, load(false, false, 0), &waits, &email, 0), PasswordAdmission::Check);
    for waiting in 0..OTHER_PASSWORDS_WAITING {
        assert_eq!(admit_password_check(door, load(true, false, waiting), &waits, &email, 0), PasswordAdmission::WaitForWorker);
        assert_eq!(admit_password_check(door, load(false, true, waiting), &waits, &email, 0), PasswordAdmission::WaitForWorker);
    }
    assert_eq!(admit_password_check(door, load(true, false, OTHER_PASSWORDS_WAITING), &waits, &email, 0), PasswordAdmission::Busy);
    assert_eq!(admit_password_check(door, load(false, false, 1), &waits, &email, 0), PasswordAdmission::WaitForWorker);
    assert_eq!(resume_password_check(0, true, false, &waits, &email, 9_999), PasswordAdmission::WaitForWorker);
    assert_eq!(resume_password_check(0, false, false, &waits, &email, 9_999), PasswordAdmission::Check);
    assert_eq!(resume_password_check(0, false, false, &waits, &email, 10_000), PasswordAdmission::Busy);
}

#[test]
fn wrong_password_waits_double_to_a_minute_without_preventing_the_next_check() {
    let email = email();
    let mut waits = PasswordWaits::default();
    let door = PasswordDoor::Other;
    let available = load(false, false, 0);
    let mut now = 100;
    for seconds in [1, 2, 4, 8, 16, 32, 60, 60, 60] {
        assert_eq!(admit_password_check(door, available, &waits, &email, now), PasswordAdmission::Check);
        waits.record_wrong(door, &email, now);
        let outcome = admit_password_check(door, load(true, false, 8), &waits, &email, now);
        assert_eq!(outcome.refusal(), Some(Refusal::Busy { retry_after_seconds: Wait::new(seconds) }));
        assert_eq!(resume_password_check(now, false, false, &waits, &email, now), outcome);
        let until = now + seconds * 1000;
        assert_eq!(waits.retry_after(&email, until - 1).map(Wait::seconds), Some(1));
        assert_eq!(waits.retry_after(&email, until), None);
        now = until;
    }
    assert_eq!(admit_password_check(door, available, &waits, &email, now), PasswordAdmission::Check);
}

#[test]
fn wait_records_forget_at_an_hour_and_evict_the_oldest_name_at_the_cap() {
    let first = email();
    let mut waits = PasswordWaits::default();
    let door = PasswordDoor::Other;
    waits.record_wrong(door, &first, 0);
    for i in 1..=PASSWORD_WAIT_NAMES {
        let other = Email::new(&format!("ada-{i}@example.org")).unwrap();
        waits.record_wrong(door, &other, i as u64);
    }
    assert!(!waits.by_email.contains_key(&first));
    assert_eq!(waits.by_email.len(), PASSWORD_WAIT_NAMES);
    assert_eq!(waits.oldest.len(), PASSWORD_WAIT_NAMES);

    let last = Email::new(&format!("ada-{PASSWORD_WAIT_NAMES}@example.org")).unwrap();
    let end = PASSWORD_WAIT_NAMES as u64 + PASSWORD_WAIT_MEMORY_MS;
    waits.expire(end - 1);
    assert!(waits.by_email.contains_key(&last));
    waits.expire(end);
    assert!(waits.by_email.is_empty());
    waits.record_wrong(door, &last, end);
    assert_eq!(waits.retry_after(&last, end).map(Wait::seconds), Some(1));
}

#[test]
fn the_independent_code_door_takes_ten_in_any_rolling_second() {
    let mut limit = CodeLimit::default();
    let first = 999;
    for time in first..first + 10 {
        assert_eq!(limit.admit(time), Ok(()));
    }
    assert_eq!(limit.admit(first + 10), Err(Wait::new(1).unwrap()));
    assert_eq!(limit.admit(first - 1), Err(Wait::new(1).unwrap()));
    for time in first..first + 10 {
        assert_eq!(limit.admit(time + 999), Err(Wait::new(1).unwrap()));
        assert_eq!(limit.admit(time + 1000), Ok(()));
    }
    assert_eq!(CodeLimit::default().admit(0), Ok(()));
    assert_eq!(CodeLimit::default().admit(u64::MAX), Ok(()));
}

#[test]
fn a_dummy_salt_is_stable_for_a_canonical_name_and_is_keyed() {
    let key = DummySaltKey::from_bytes([7; SECRET_BYTES]);
    let first = key.salt_for(&email());
    assert_eq!(first.as_ref(), key.salt_for(&Email::new(" Ada@Example.Org ").unwrap()).as_ref());
    assert_ne!(first.as_ref(), key.salt_for(&Email::new("another@example.org").unwrap()).as_ref());
    assert_ne!(first.as_ref(), DummySaltKey::from_bytes([8; SECRET_BYTES]).salt_for(&email()).as_ref());
    assert_eq!(format!("{key:?}"), "DummySaltKey(..)");
}
