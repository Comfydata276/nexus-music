use super::e3::*;
use super::*;

use super::super::account::SALT_BYTES;

fn pair() -> SaltedToken {
    SaltedToken::from_password(
        b"Example password",
        MusicSalt::from_random([0x2b; SALT_BYTES]),
    )
}

fn outcome(ping: &[u8], user: &[u8], name: &str) -> Result<(), Outcome> {
    ping_answer(Some((200, ping)))?;
    user_answer(Some((200, user)), name)
}

#[test]
fn a_token_requires_a_salt_and_covers_both_byte_sequences_in_order() {
    // Independently computed using Python hashlib over password bytes and the
    // actual hex salt, including lengths on both sides of the MD5 padding boundary.
    let cases = [
        (vec![b'x'; 0], "04fa475d966e0de92f609e98eb23225b"),
        (vec![b'x'; 1], "052adda21a989ef8474c05355f08a807"),
        (vec![b'x'; 55], "dc96c90b2b3492653e931ad56f204dba"),
        (vec![b'x'; 56], "18dbdc31f5947fd81bacc853df572caa"),
        (vec![b'x'; 63], "60af6ce67cc133c4c706bb5f5d136b11"),
        (vec![b'x'; 64], "8fe6828e9b3aab08ba70db1ac75fc4f6"),
        (vec![b'x'; 119], "7c587d7653b0b00f337baf9aad71f2b0"),
        (vec![b'x'; 120], "cc417f88c75658f2f4671e2ae35210ee"),
        (vec![b'x'; 128], "b7832b8fe11fc208c4f71df47d6061ac"),
        (vec![b'x'; 1000], "7bf3c5e76b80243aca5596babf08747c"),
        (
            "Example 密碼".as_bytes().to_vec(),
            "1a38628e96838ae63a8d4797308a5090",
        ),
    ];
    for (password, expected) in cases {
        let salt = MusicSalt::from_random([0x2b; SALT_BYTES]);
        let pair = SaltedToken::from_password(&password, salt);
        assert_eq!(pair.salt().as_str(), &"2b".repeat(SALT_BYTES));
        assert_eq!(&*pair.token().expose_hex(), expected);
        assert_eq!(pair.token().expose_hex().len(), 32);
    }
    let empty_bytes = MusicSalt::from_random([0; SALT_BYTES]);
    let pair = SaltedToken::from_password(b"Example password", empty_bytes);
    assert_eq!(pair.salt().as_str(), &"00".repeat(SALT_BYTES));
    assert_ne!(
        &*pair.token().expose_hex(),
        "f6b97dc773771138cd1eabbef027b6fc"
    );
}

#[test]
fn approved_login_posts_have_fixed_paths_and_escape_the_bound_name() {
    let pair = pair();
    let name = "alice & + 山";
    let address = "https://music.example.test";
    let post = login_post(address, name, &pair, false);
    let credentials = format!(
        "u=alice%20%26%20%2B%20%E5%B1%B1&s={}&t={}",
        pair.salt().as_str(),
        *pair.token().expose_hex()
    );
    assert_eq!(post.path(), "/rest/ping");
    assert_eq!(post.form(), format!("{FORM}&{credentials}"));

    let post = login_post(address, name, &pair, true);
    assert_eq!(post.path(), "/rest/getUser");
    assert_eq!(
        post.form(),
        format!("{FORM}&username=alice%20%26%20%2B%20%E5%B1%B1&{credentials}")
    );
    let account =
        br#"{"subsonic-response":{"status":"ok","user":{"username":"alice & + \u5c71"}}}"#;
    assert_eq!(user_answer(Some((200, account)), name), Ok(()));
}

#[test]
fn a_stream_classifies_a_complete_refusal_apart_from_other_failures() {
    assert_eq!(classify_start(200, REFUSED, true), ReplyStart::Refused);
    assert_eq!(classify_start(200, REFUSED, false), ReplyStart::Other);
    assert_eq!(classify_start(401, REFUSED, true), ReplyStart::Other);
    assert_eq!(classify_start(200, FORBIDDEN, true), ReplyStart::NotAllowed);
    assert_eq!(classify_start(403, b"", false), ReplyStart::NotAllowed);
    for status in [429, 503] {
        assert_eq!(classify_start(status, b"<html>", false), ReplyStart::Busy);
    }
    assert_eq!(
        classify_start(416, b"", false),
        ReplyStart::RangeUnavailable
    );
    for (status, prefix) in [
        (200, XML_REFUSED),
        (200, b"ID3"),
        (206, b"ID3"),
        (502, b"<html>"),
    ] {
        assert_eq!(classify_start(status, prefix, true), ReplyStart::Other);
    }
}

#[test]
fn all_measured_e3_replies_are_classified_in_their_request_context() {
    for (reply, name) in [(ALICE, "Alice"), (BOB, "bob smith"), (ADMIN, "Exp-Admin")] {
        assert_eq!(outcome(OK_WITH_VERSION, reply, name), Ok(()));
        assert_eq!(outcome(OK, reply, name), Ok(()));
        assert_eq!(outcome(reply, ALICE, "Alice"), Err(Outcome::CouldNotAsk));
    }
    for response in [REFUSED, REFUSED_WITH_VERSION] {
        // E3.2 and E3.4 measured the same code for bad credentials and lockout.
        assert_eq!(outcome(response, ALICE, "Alice"), Err(Outcome::Refused));
        assert_eq!(outcome(OK, response, "Alice"), Err(Outcome::Refused));
    }
    for response in [
        MISSING_C,
        MISSING_U,
        MISSING_V,
        FORBIDDEN,
        SONG_MISSING,
        SONG,
        XML_LARGE,
        XML_MISSING_C,
        XML_MISSING_U,
        XML_MISSING_V,
        XML_OK,
        XML_REFUSED,
    ] {
        assert_eq!(outcome(response, ALICE, "Alice"), Err(Outcome::CouldNotAsk));
        assert_eq!(outcome(OK, response, "Alice"), Err(Outcome::CouldNotAsk));
    }
    assert_eq!(
        outcome(OK, OK_WITH_VERSION, "Alice"),
        Err(Outcome::CouldNotAsk)
    );
    for name in ["alice", "ALICE", "Alice ", "Bob Smith", "bob  smith"] {
        assert_eq!(outcome(OK, ALICE, name), Err(Outcome::NameDiffers));
        assert_eq!(outcome(OK, BOB, name), Err(Outcome::NameDiffers));
    }
}

#[test]
fn malformed_ambiguous_oversized_or_incomplete_envelopes_never_authenticate() {
    let oversized = [b' '; ANSWER_MAX_BYTES + 1];
    let deep = format!(
        r#"{{"subsonic-response":{{"status":"ok","user":{{"username":"Alice","deep":{}0{}}}}}}}"#,
        "[".repeat(proto::HELLO_MAX_DEPTH),
        "]".repeat(proto::HELLO_MAX_DEPTH)
    );
    for body in [
        b"".as_slice(),
        b"[]",
        b"null",
        br#"{"subsonic-response":{"status":"ok"}"#,
        br#"{"subsonic-response":{"status":"ok"}} {}"#,
        br#"{"subsonic-response":{"status":"ok","status":"ok"}}"#,
        br#"{"subsonic-response":{"status":"ok","user":{"username":"Alice","x":{"a":0,"\u0061":1}}}}"#,
        b"{\"subsonic-response\":{\"status\":\"ok\",\"user\":{\"username\":\"Alice\",\"x\":\"\xff\"}}}",
        br#"{"subsonic-response":{"status":"ok","user":{"username":3}}}"#,
        br#"{"subsonic-response":{"status":"ok","user":{"username":""}}}"#,
        br#"{"subsonic-response":{"status":"ok","version":1}}"#,
        br#"{"subsonic-response":{"status":"ok","openSubsonic":"true"}}"#,
        br#"{"subsonic-response":{"status":"ok","error":{"code":40,"message":"x"}}}"#,
        br#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"x"},"user":{"username":"Alice"}}}"#,
        br#"{"subsonic-response":{"status":"failed","error":{"code":"40","message":"x"}}}"#,
        br#"{"subsonic-response":{"status":"failed","error":{"code":40}}}"#,
        br#"{"subsonic-response":{"status":"ok"},"extra":0}"#,
        &oversized,
        deep.as_bytes(),
    ] {
        assert_eq!(outcome(body, ALICE, "Alice"), Err(Outcome::CouldNotAsk));
        assert_eq!(outcome(OK, body, "Alice"), Err(Outcome::CouldNotAsk));
    }
    for status in [201, 302, 400, 401, 429, 500] {
        assert_eq!(ping_answer(Some((status, OK))), Err(Outcome::CouldNotAsk));
        assert_eq!(
            user_answer(Some((status, ALICE)), "Alice"),
            Err(Outcome::CouldNotAsk)
        );
    }
    assert_eq!(ping_answer(None), Err(Outcome::CouldNotAsk));
    assert_eq!(user_answer(None, "Alice"), Err(Outcome::CouldNotAsk));
}

#[test]
fn secrets_and_secret_bearing_forms_have_redacted_debug() {
    let pair = pair();
    let post = login_post("https://music.example.test", "Alice", &pair, false);
    let text = format!("{:?} {pair:?} {post:?}", pair.salt());
    for secret in [
        pair.salt().as_str(),
        &*pair.token().expose_hex(),
        post.form(),
    ] {
        assert!(!text.contains(secret));
    }
}

#[test]
fn a_sealed_password_opens_only_under_every_part_of_its_current_binding() {
    let key = SealingKey::new([7; 32]);
    let bind = CredentialBinding {
        backend: BackendId::from_bytes([1; 16]),
        account: AccountId::new(2).unwrap(),
        address: MusicAddress::new("https://music.example.test").unwrap(),
        name: Name::new("Alice").unwrap(),
        count: 4,
    };
    let password = b"Example password";
    let sealed = key.seal(&bind, password, [5; NONCE_BYTES]).unwrap();
    assert_eq!(&*key.open(&bind, &sealed).unwrap(), password);
    let decoded = SealedCredential::from_bytes(sealed.as_bytes()).unwrap();
    assert_eq!(decoded, sealed);
    for moved in [
        CredentialBinding {
            backend: BackendId::from_bytes([2; 16]),
            ..bind.clone()
        },
        CredentialBinding {
            account: AccountId::new(3).unwrap(),
            ..bind.clone()
        },
        CredentialBinding {
            address: MusicAddress::new("https://other.example.test").unwrap(),
            ..bind.clone()
        },
        CredentialBinding {
            name: Name::new("Bob").unwrap(),
            ..bind.clone()
        },
        CredentialBinding {
            count: 5,
            ..bind.clone()
        },
    ] {
        assert!(key.open(&moved, &sealed).is_none());
    }
    let mut tampered = sealed.as_bytes().to_vec();
    *tampered.last_mut().unwrap() ^= 1;
    assert!(
        key.open(&bind, &SealedCredential::from_bytes(&tampered).unwrap())
            .is_none()
    );
    assert!(SealedCredential::from_bytes(&[0; NONCE_BYTES]).is_none());
    assert!(key.seal(&bind, b"", [6; NONCE_BYTES]).is_none());
    assert_eq!(
        format!("{key:?} {sealed:?}"),
        "SealingKey(..) SealedCredential(..)"
    );
}

#[test]
fn hand_out_uses_only_the_current_account_and_seal_and_takes_a_fresh_salt() {
    let backend = BackendId::from_bytes([5; 16]);
    let account = AccountId::new(1).unwrap();
    let address = MusicAddress::new("https://music.example.test").unwrap();
    let binding = CredentialBinding {
        backend,
        account,
        address: address.clone(),
        name: Name::new("Alice").unwrap(),
        count: 1,
    };
    let key = SealingKey::new([6; 32]);
    let login = MusicInstallation {
        sealed: key.seal(&binding, b"Example password", [7; 24]).unwrap(),
        binding,
    };
    let mut state = AccountState::new(
        backend,
        1,
        account,
        crate::nexus::account::Email::new("alice@example.test").unwrap(),
        None,
    );
    let salt = |n| MusicSalt::from_random([n; SALT_BYTES]);
    assert_eq!(
        hand_out(&state, None, &key, &address, salt(1)),
        MusicLoginResult::NoneSetYet
    );
    state.music_login = 1;
    for n in [2, 3] {
        let MusicLoginResult::Login(handed) =
            hand_out(&state, Some(&login), &key, &address, salt(n))
        else {
            panic!("current credential was not handed out");
        };
        assert_eq!(handed.salt, salt(n));
        assert_eq!(
            &handed.token,
            SaltedToken::from_password(b"Example password", salt(n)).token()
        );
    }
    let other = MusicAddress::new("https://different.example.test").unwrap();
    assert_eq!(
        hand_out(&state, Some(&login), &key, &other, salt(4)),
        MusicLoginResult::UnusableNow
    );
    assert_eq!(
        hand_out(
            &state,
            Some(&login),
            &SealingKey::new([8; 32]),
            &address,
            salt(5)
        ),
        MusicLoginResult::UnusableNow
    );
    state.music_login += 1;
    assert_eq!(
        hand_out(&state, Some(&login), &key, &address, salt(6)),
        MusicLoginResult::UnusableNow
    );
}
