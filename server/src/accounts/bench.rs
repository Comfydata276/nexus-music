//! A bench for the accounts' own tests: the real keeper, thread that writes, workers and
//! [`Accounts`], in this process, on a clock the test turns (no HTTP: the door's part is
//! `tests/signin.rs`'s). Made-up names only.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nexus_core::nexus::account::{
    self, CODE_LIFETIME_MS, Credential, DeviceKind, DeviceSecret, Email, Name, OneTimeCode, Password, Refusal, Reply, RequestHead, SignIn, SignedIn,
};
use nexus_core::nexus::proto::{SPEAKS, Version};

use super::check::PasswordWorkers;
use super::desk::Desk;
use super::session::{Accounts, Answer};
use super::setup::{self, CodeKey, SetUpCode};
use crate::store::keeper::{First, Keeper};

/// A time a long way from 0, so that going back is possible.
pub const START: u64 = 1_800_000_000_000;

pub struct Bench {
    pub accounts: Arc<Accounts>,
    pub now: Arc<AtomicU64>,
    pub runtime: tokio::runtime::Runtime,
    root: PathBuf,
}

/// An account made for the bench, and its live code.
pub struct Made {
    pub number: u64,
    pub email: &'static str,
    pub code: OneTimeCode,
}

impl Bench {
    /// A backend set up, with `accounts` made (email, admin), each with a live code.
    pub fn new(name: &str, accounts: &[(&'static str, bool)]) -> (Bench, Vec<Made>) {
        let root = std::env::temp_dir().join(format!("nexus-bench-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::create_dir_all(root.join("state")).unwrap();
        let (mut keeper, _) = Keeper::open(&root.join("state"), &root.join("data"), START).unwrap();
        keeper.mark_set_up(START).unwrap();
        let key = CodeKey::of(&keeper);
        let made = accounts
            .iter()
            .map(|&(email, admin)| {
                let (code, print) = setup::make_code(&key).unwrap();
                let number = keeper.make_account(email, "Someone", admin, First::Code(&print, START + CODE_LIFETIME_MS), START).unwrap();
                Made { number, email, code }
            })
            .collect();
        let workers = PasswordWorkers::new(setup::dummy_salt_key(&keeper)).unwrap();
        let backend = keeper.backend();
        let desk = Desk::open(keeper).unwrap();
        let now = Arc::new(AtomicU64::new(START));
        let read = now.clone();
        let accounts = Arc::new(Accounts {
            desk,
            workers,
            set_up_code: SetUpCode::at_start(true, START).0,
            clock: Arc::new(move || read.load(Ordering::SeqCst)),
            backend,
            seen: Arc::default(),
        });
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        (Bench { accounts, now, runtime, root }, made)
    }

    pub fn set(&self, now: u64) {
        self.now.store(now, Ordering::SeqCst);
    }

    pub fn advance(&self, by: u64) {
        self.now.fetch_add(by, Ordering::SeqCst);
    }

    pub fn sign_in(&self, credential: Credential, device: u8) -> Answer {
        self.runtime.block_on(self.accounts.sign_in(sign_in(credential, device), false))
    }
}

impl Drop for Bench {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

pub fn sign_in(credential: Credential, device: u8) -> SignIn {
    SignIn {
        credential,
        device_secret: DeviceSecret::from_bytes([device; 32]),
        device_name: Name::new("A made-up device").unwrap(),
        device_kind: DeviceKind::Pc,
        app_version: Version::parse("0.41.0").unwrap(),
        speaks: SPEAKS,
        restore: None,
    }
}

pub fn with_code(code: &OneTimeCode, new_password: &str) -> Credential {
    Credential::Code { code: OneTimeCode::new(code.expose()).unwrap(), new_password: Password::new(new_password).unwrap() }
}

pub fn with_password(email: &str, password: &str) -> Credential {
    Credential::Password { email: Email::new(email).unwrap(), password: Password::presented(password).unwrap() }
}

pub fn head(restore: u64) -> RequestHead {
    RequestHead { speaks: SPEAKS, restore }
}

pub fn signed_in(answer: &Answer) -> SignedIn {
    match account::read_reply::<SignedIn>(answer.status, answer.body.bytes()).unwrap() {
        Reply::Done { result, .. } => result,
        other => panic!("not signed in: {other:?}"),
    }
}

pub fn refusal<T: std::fmt::Debug>(reply: Reply<T>) -> Refusal {
    match reply {
        Reply::Refused(refusal) => refusal,
        other => panic!("not refused: {other:?}"),
    }
}
