//! Setting up, recovery, and one-time codes (design 8.4, 8.5; slice S8a).
//!
//! - **Setting up happens once.** While the store kept apart says the backend has never been
//!   set up, a set-up code is made at each start: 60 random bits, good for fifteen minutes
//!   and one use, printed once in the log ([`SetUpCode`]). A restart makes a new one and the
//!   old dies with the program, since it is only ever in memory. Setting up is one step: the
//!   store first records "set up" ([`Keeper::mark_set_up`]), then the data gains the first
//!   admin. Two at once: one wins (the thread that writes does one at a time, and the second
//!   finds the store set up). A program that dies between the two halves is set up with no
//!   admin, which is recovery, never a second first start.
//! - **The owner's commands** ([`set_up_command`], [`recover`]), run in the container while the
//!   backend is stopped, need no sign-in and print nothing to the log: what they make is said
//!   on the command's own terminal. Recovery makes a one-time code for any account by its
//!   email, the owner's own included, and makes the account (an admin's) if there is none.
//! - **A one-time code** ([`make_code`]) is twelve characters of C7's alphabet: 60 random bits.
//!   The data keeps only its fingerprint, keyed with a key derived from the store kept apart's
//!   ([`CodeKey`]), so that a copy of the data alone gives nobody a way to try codes.

use std::sync::Mutex;

use hmac::{Hmac, KeyInit, Mac};
use nexus_core::nexus::account::{CODE_LIFETIME_MS, Email, OneTimeCode, SECRET_BYTES};
use nexus_core::nexus::kept::{DataChange, Effects, Fingerprint, LiveCode, Status, Update};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::store::data::Change;
use crate::store::keeper::{First, Keeper};

/// How long the set-up code is good for, from the start that made it (8.4).
pub const SET_UP_CODE_MS: u64 = 15 * 60 * 1000;

/// Random bytes from the system, or nothing.
pub fn random<const N: usize>() -> Option<Zeroizing<[u8; N]>> {
    let mut bytes = Zeroizing::new([0u8; N]);
    getrandom::fill(bytes.as_mut()).ok()?;
    Some(bytes)
}

/// 60 random bits as a code.
fn random_code() -> Option<OneTimeCode> {
    let bytes = random::<8>()?;
    Some(OneTimeCode::from_random(u64::from_be_bytes(*bytes) & ((1 << 60) - 1)))
}

/// The key one-time codes are fingerprinted with: derived from the store kept apart's key that
/// seals, under a name of its own, so that it is never the key that seals itself.
pub struct CodeKey(Zeroizing<[u8; SECRET_BYTES]>);

impl std::fmt::Debug for CodeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CodeKey(..)")
    }
}

fn derive(from: &[u8; SECRET_BYTES], name: &[u8]) -> Zeroizing<[u8; SECRET_BYTES]> {
    let from = Zeroizing::new(*from);
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(from.as_slice()).expect("HMAC takes a key of any length");
    mac.update(name);
    Zeroizing::new(mac.finalize().into_bytes().into())
}

impl CodeKey {
    pub fn of(keeper: &Keeper) -> Self {
        CodeKey(derive(&keeper.seal_key(), b"Nexus Music one-time code key v1\0"))
    }

    /// The keyed fingerprint of a code, as the data keeps it.
    pub fn print(&self, code: &OneTimeCode) -> [u8; 32] {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(self.0.as_slice()).expect("HMAC takes a key of any length");
        mac.update(b"code\0");
        mac.update(code.expose_bytes());
        mac.finalize().into_bytes().into()
    }
}

/// The key the same slow work is done with for an email that has no account (8.3, S8b's
/// `DummySaltKey`): derived from the store kept apart's key, so it is the same at every start.
pub fn dummy_salt_key(keeper: &Keeper) -> nexus_core::nexus::account::DummySaltKey {
    nexus_core::nexus::account::DummySaltKey::from_bytes(*derive(&keeper.seal_key(), b"Nexus Music dummy password salt key v1\0"))
}

/// A new one-time code, and its keyed fingerprint.
pub fn make_code(key: &CodeKey) -> Option<(OneTimeCode, [u8; 32])> {
    let code = random_code()?;
    let print = key.print(&code);
    Some((code, print))
}

/// The set-up code of this start, held in memory only (8.4).
pub struct SetUpCode {
    live: Mutex<Option<Live>>,
}

struct Live {
    print: Fingerprint,
    until: u64,
}

impl SetUpCode {
    /// For a backend not yet set up, a code made now and good for [`SET_UP_CODE_MS`]; the code
    /// itself is given back once, to be printed in the log, and is not kept.
    pub fn at_start(set_up: bool, now: u64) -> (SetUpCode, Option<OneTimeCode>) {
        let made = (!set_up).then(random_code).flatten();
        let live = made.as_ref().map(|code| Live { print: Fingerprint::of(&padded(code)), until: now.saturating_add(SET_UP_CODE_MS) });
        (SetUpCode { live: Mutex::new(live) }, made)
    }

    /// Takes the code if `typed` is it and it is still good: once. Any other answer leaves it as
    /// it was.
    pub fn take(&self, typed: &OneTimeCode, now: u64) -> bool {
        let Ok(mut live) = self.live.lock() else { return false };
        let matches = live.as_ref().is_some_and(|live| live.print == Fingerprint::of(&padded(typed)) && now < live.until);
        if matches {
            *live = None;
        }
        matches
    }

    /// Whether a set-up code is still to be had at `now`.
    pub fn open(&self, now: u64) -> bool {
        self.live.lock().is_ok_and(|live| live.as_ref().is_some_and(|live| now < live.until))
    }
}

/// A code's twelve bytes, as the 32 a fingerprint is made over.
fn padded(code: &OneTimeCode) -> Zeroizing<[u8; SECRET_BYTES]> {
    let mut bytes = Zeroizing::new([0u8; SECRET_BYTES]);
    bytes[..code.expose_bytes().len()].copy_from_slice(code.expose_bytes());
    bytes
}

/// Why a set-up was not done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotSetUp {
    /// The backend was set up already (by another set-up that won, or before).
    Already,
    /// The store or the data could not be written; nothing was given.
    Failed(String),
}

/// The one step of setting up (8.4), on the thread that writes: the store first records that
/// the backend is set up, then the data gains the first admin, with `first`. The number of
/// the account made.
pub fn set_up(keeper: &mut Keeper, email: &Email, shown: &str, first: First<'_>, now: u64) -> Result<u64, NotSetUp> {
    if keeper.set_up() {
        return Err(NotSetUp::Already);
    }
    keeper.mark_set_up(now).map_err(|failed| NotSetUp::Failed(failed.0))?;
    keeper.make_account(email.as_str(), shown, true, first, now).map_err(|failed| NotSetUp::Failed(failed.0))
}

/// `set-up <email> [<name>]`: sets up without printing anything to the log (8.4). The first
/// admin is made with a one-time code, said here once, with which they choose their password
/// as they first sign in (8.5). Refused once the backend is set up: then it is `recover`.
pub fn set_up_command(keeper: &mut Keeper, email: &str, shown: Option<&str>, now: u64) -> Result<Vec<String>, String> {
    let email = Email::new(email).ok_or("That is not an email: one @, nothing either side of it missing, no spaces, 254 bytes at most.")?;
    if keeper.set_up() {
        return Err("The backend is set up already. To get a one-time code for an account, run `nexus-music-server recover <email>`.".into());
    }
    let key = CodeKey::of(keeper);
    let (code, print) = make_code(&key).ok_or("There is no randomness to make a code with.")?;
    let shown = shown.map_or_else(|| default_name(&email), str::to_owned);
    let until = now.saturating_add(CODE_LIFETIME_MS);
    match set_up(keeper, &email, &shown, First::Code(&print, until), now) {
        Ok(_) => Ok(vec![
            "The backend is set up. Its first admin is made.".into(),
            format!("Sign in to it from the app with this email and the one-time code {}, and choose your password there. The code is good for seven days and once; it is not shown again.", spaced(&code).as_str()),
        ]),
        Err(NotSetUp::Already) => Err("The backend is set up already.".into()),
        Err(NotSetUp::Failed(why)) => Err(format!("It could not be set up: {why}")),
    }
}

/// A name to show for an account made by a command with none given: the email's first half.
fn default_name(email: &Email) -> String {
    email.as_str().split('@').next().filter(|name| !name.is_empty()).unwrap_or("Admin").to_owned()
}

/// A code in two halves of six, as it is shown to a person; typed, either way is read.
pub fn spaced(code: &OneTimeCode) -> Zeroizing<String> {
    let text = code.expose();
    Zeroizing::new(format!("{}-{}", &text[..6], &text[6..]))
}

/// `recover <email>`: a one-time code for the account with this email, the owner's own
/// included, with no sign-in (8.4); the account is then an admin's and switched on, with
/// every session of it ended and its password gone until the code is used. With no account
/// by that email, an admin's is made (the data was emptied, or the last admin deleted).
pub fn recover(keeper: &mut Keeper, email: &str, now: u64) -> Result<Vec<String>, String> {
    let email = Email::new(email).ok_or("That is not an email: one @, nothing either side of it missing, no spaces, 254 bytes at most.")?;
    let key = CodeKey::of(keeper);
    let (code, print) = make_code(&key).ok_or("There is no randomness to make a code with.")?;
    let until = now.saturating_add(CODE_LIFETIME_MS);
    let said = format!(
        "Sign in to it from the app with this email and the one-time code {}, and choose a password there. The code is good for seven days and once; it is not shown again.",
        spaced(&code).as_str()
    );
    let Some(number) = keeper.account_named(email.as_str()).map_err(|failed| failed.0)? else {
        // Not set up, or set up and its first admin never made, or every account gone with
        // the data: an admin is made.
        if !keeper.set_up() {
            keeper.mark_set_up(now).map_err(|failed| failed.0)?;
        }
        keeper.make_account(email.as_str(), &default_name(&email), true, First::Code(&print, until), now).map_err(|failed| failed.0)?;
        return Ok(vec!["There was no account with that email: an admin's is made.".into(), said]);
    };
    owner_reset(keeper, number, &print, until, now)?;
    Ok(vec!["Every session of that account is ended, its password no longer works, and it is an admin's and switched on.".into(), said])
}

/// CARRIED (asked of C3r: an owner's recovery decision in `kept`, with no actor). Until it
/// lands this applies 8.7's row for a reset, the stricter reading, in its order: in the store
/// first, the revision and the count of passwords move, the new code is the live one and every
/// device that had a session moves on; then the data: every session goes, the password goes,
/// the new code's fingerprint is kept; and only then, as the giving it is, the account is
/// switched on and made an admin's (in the data first, then the store).
fn owner_reset(keeper: &mut Keeper, number: u64, print: &[u8; 32], until: u64, now: u64) -> Result<(), String> {
    let account = keeper.load(number).map_err(|failed| failed.0)?;
    let before = account.apart();
    if before.account.status == Status::Deleted {
        return Err("That account was deleted.".into());
    }
    let next = |n: u64| n.checked_add(1).ok_or("A count of that account can go no higher.".to_string());
    let mut first = before.clone();
    first.account.revision = next(first.account.revision)?;
    first.account.password_changes = next(first.account.password_changes)?;
    first.account.password_reset_required = true;
    first.account.code = Some(LiveCode { number: first.account.password_changes, used: false });
    for device in first.devices.values_mut() {
        if device.permit.take().is_some() {
            device.number = next(device.number)?;
        }
    }
    let mut rows = account.rows();
    rows.account = first.account.clone();
    rows.account.status = Status::On;
    rows.account.admin = true;
    for (id, row) in rows.devices.iter_mut() {
        if let Some(device) = first.devices.get(id) {
            row.number = device.number;
        }
        row.session = None;
    }
    let mut last = first.clone();
    last.account.status = Status::On;
    last.account.admin = true;
    let effects = Effects { high_water: now, updates: vec![Update { apart_first: first, data: rows, change: DataChange::ResetPassword, music: None, apart_last: last }] };
    let code_number = effects.updates[0].apart_last.account.password_changes;
    keeper
        .persist(&effects, &[Change::Code { account: number, number: code_number, code: print, made_at: now, until }], now)
        .map_err(|failed| failed.0)
}

/// What the log says at a start while there is no admin (19.4): the set-up code, once.
pub fn said_at_start(code: &OneTimeCode) -> Zeroizing<String> {
    Zeroizing::new(format!(
        "This backend is not set up. Its set-up code, good for fifteen minutes from now and once, is {}. Anyone who can read this log can read it while it lives; to set up without it, stop the backend and run `nexus-music-server set-up <email>`.",
        spaced(code).as_str()
    ))
}
