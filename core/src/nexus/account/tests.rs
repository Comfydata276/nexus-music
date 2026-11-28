use super::*;
use crate::nexus::proto::{API, SPEAKS, plain};

const CODE: &str = "ABCDEFGH3479";

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
