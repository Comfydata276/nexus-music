use super::e3::*;
use super::*;

fn pair() -> SaltedToken {
    SaltedToken::from_password(b"Example password", Salt::from_random([0x2b; SALT_BYTES]))
}

fn probe(answer: Option<(u16, &[u8])>) -> Trust {
    TrustProbe::from_random([42; SALT_BYTES]).answer(answer)
}

fn ping<'a>(name: &'a str, pair: &'a SaltedToken) -> Ping<'a> {
    Ping::new(name, pair, Trust::RequiresCredentials)
        .ok()
        .unwrap()
}

fn outcome(ping_answer: &[u8], user_answer: &[u8], name: &str) -> Outcome {
    let pair = pair();
    match ping(name, &pair).answer(Some((200, ping_answer))) {
        Ok(call) => call.answer(Some((200, user_answer))),
        Err(outcome) => outcome,
    }
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
        let salt = Salt::from_random([0x2b; SALT_BYTES]);
        let pair = SaltedToken::from_password(&password, salt);
        assert_eq!(pair.salt(), &"2b".repeat(SALT_BYTES));
        assert_eq!(pair.token(), expected);
        assert_eq!(pair.token().len(), TOKEN_DIGITS);
    }
    let empty_bytes = Salt::from_random([0; SALT_BYTES]);
    let pair = SaltedToken::from_password(b"Example password", empty_bytes);
    assert_eq!(pair.salt(), &"00".repeat(SALT_BYTES));
    assert_ne!(pair.token(), "f6b97dc773771138cd1eabbef027b6fc");
}

#[test]
fn approved_login_posts_have_fixed_paths_and_escape_the_bound_name() {
    let pair = pair();
    let name = "alice & + 山";
    let ping = ping(name, &pair);
    let post = ping.post();
    let credentials = format!(
        "u=alice%20%26%20%2B%20%E5%B1%B1&s={}&t={}",
        pair.salt(),
        pair.token()
    );
    assert_eq!(post.path(), "/rest/ping");
    assert_eq!(post.form(), format!("{FORM}&{credentials}"));

    let user = ping.answer(Some((200, OK))).ok().unwrap();
    let post = user.post();
    assert_eq!(post.path(), "/rest/getUser");
    assert_eq!(
        post.form(),
        format!("{FORM}&username=alice%20%26%20%2B%20%E5%B1%B1&{credentials}")
    );
    let account =
        br#"{"subsonic-response":{"status":"ok","user":{"username":"alice & + \u5c71"}}}"#;
    assert_eq!(user.answer(Some((200, account))), Outcome::Verified);
}

#[test]
fn only_the_exact_missing_u_response_allows_credential_posts_after_the_probe() {
    let check = TrustProbe::from_random([42; SALT_BYTES]);
    let post = check.post();
    assert_eq!(post.path(), "/rest/ping");
    assert_eq!(post.form(), FORM);
    assert_eq!(check.header_value(), &"2a".repeat(SALT_BYTES));
    assert_eq!(
        check.answer(Some((200, MISSING_U))),
        Trust::RequiresCredentials
    );
    for response in [OK, OK_WITH_VERSION, ALICE, REFUSED, REFUSED_WITH_VERSION] {
        assert_eq!(probe(Some((200, response))), Trust::AcceptsHeader);
    }
    for response in [
        MISSING_C,
        MISSING_V,
        FORBIDDEN,
        SONG,
        SONG_MISSING,
        XML_MISSING_U,
    ] {
        assert_eq!(probe(Some((200, response))), Trust::Unknown);
    }
    assert_eq!(probe(None), Trust::Unknown);
    assert_eq!(probe(Some((302, MISSING_U))), Trust::Unknown);
    let pair = pair();
    assert_eq!(
        Ping::new("Alice", &pair, Trust::Unknown).err(),
        Some(Outcome::CouldNotAsk)
    );
    assert_eq!(
        Ping::new("Alice", &pair, Trust::AcceptsHeader).err(),
        Some(Outcome::UntrustedCheck)
    );
}

#[test]
fn all_measured_e3_replies_are_classified_in_their_request_context() {
    for (reply, name) in [(ALICE, "Alice"), (BOB, "bob smith"), (ADMIN, "Exp-Admin")] {
        assert_eq!(outcome(OK_WITH_VERSION, reply, name), Outcome::Verified);
        assert_eq!(outcome(OK, reply, name), Outcome::Verified);
        assert_eq!(outcome(reply, ALICE, "Alice"), Outcome::CouldNotAsk);
    }
    for response in [REFUSED, REFUSED_WITH_VERSION] {
        // E3.2 and E3.4 measured the same code for bad credentials and lockout.
        assert_eq!(outcome(response, ALICE, "Alice"), Outcome::Refused);
        assert_eq!(outcome(OK, response, "Alice"), Outcome::Refused);
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
        assert_eq!(outcome(response, ALICE, "Alice"), Outcome::CouldNotAsk);
        assert_eq!(outcome(OK, response, "Alice"), Outcome::CouldNotAsk);
    }
    assert_eq!(outcome(OK, OK_WITH_VERSION, "Alice"), Outcome::CouldNotAsk);
    for name in ["alice", "ALICE", "Alice ", "Bob Smith", "bob  smith"] {
        assert_eq!(outcome(OK, ALICE, name), Outcome::CouldNotAsk);
        assert_eq!(outcome(OK, BOB, name), Outcome::CouldNotAsk);
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
        assert_eq!(outcome(body, ALICE, "Alice"), Outcome::CouldNotAsk);
        assert_eq!(outcome(OK, body, "Alice"), Outcome::CouldNotAsk);
    }
    let pair = pair();
    for status in [201, 302, 400, 401, 429, 500] {
        assert_eq!(
            ping("Alice", &pair).answer(Some((status, OK))).err(),
            Some(Outcome::CouldNotAsk)
        );
        let get = ping("Alice", &pair).answer(Some((200, OK))).ok().unwrap();
        assert_eq!(get.answer(Some((status, ALICE))), Outcome::CouldNotAsk);
    }
    assert_eq!(
        ping("Alice", &pair).answer(None).err(),
        Some(Outcome::CouldNotAsk)
    );
    let get = ping("Alice", &pair).answer(Some((200, OK))).ok().unwrap();
    assert_eq!(get.answer(None), Outcome::CouldNotAsk);
}

#[test]
fn secrets_and_secret_bearing_forms_have_redacted_debug() {
    let pair = pair();
    let proof = TrustProbe::from_random([42; SALT_BYTES]);
    let post = ping("Alice", &pair).post();
    let text = format!(
        "{:?} {pair:?} {proof:?} {post:?}",
        Salt::from_random([43; SALT_BYTES])
    );
    for secret in [pair.salt(), pair.token(), proof.header_value(), post.form()] {
        assert!(!text.contains(secret));
    }
}
