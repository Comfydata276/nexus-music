//! The accounts' own sequences, on the [`bench`](super::bench): a clock the test turns, and
//! the real keeper, writer and workers. Each test names the finding or rule it pins.

use nexus_core::nexus::account::{self, NotSignedIn, Refusal, RequestId, SESSION_IDLE_MS, SignOut, SignOutNote, SignOutSecret, SignedOut};

use super::bench::{self, Bench};

const PASSWORD: &str = "a made-up password for the bench";

/// B1 (the review of S8): a note naming a session that has passed its idle deadline, with a
/// secret that is not that session's, is told "that session has ended" with no receipt, and
/// so is the right note: the same bytes, nothing of the account.
#[test]
fn a_note_is_told_nothing_of_the_account_whatever_comes_of_it() {
    let (bench, made) = Bench::new("note", &[("note@example.test", false), ("other@example.test", false)]);
    let first = bench::signed_in(&bench.sign_in(bench::with_code(&made[0].code, PASSWORD), 1));
    bench::signed_in(&bench.sign_in(bench::with_code(&made[1].code, PASSWORD), 2));
    let note = |note: SignOutNote, request: u64| SignOut { head: bench::head(first.restore), request_id: RequestId::new(request).unwrap(), revoke: Some(note) };

    // The first session goes idle past its deadline; its row is still there. The other
    // account signs in afresh: its session is live.
    bench.advance(SESSION_IDLE_MS + 1);
    let second = bench::signed_in(&bench.sign_in(bench::with_password(made[1].email, PASSWORD), 3));
    let forged = SignOutNote { session: first.sign_out.session, secret: SignOutSecret::from_bytes([0x5a; 32]) };
    let wrong = bench.runtime.block_on(bench.accounts.sign_out(None, note(forged, 1)));
    assert_eq!(wrong.status, 401);
    let reply = account::read_reply::<SignedOut>(wrong.status, wrong.body.bytes()).unwrap();
    assert_eq!(bench::refusal(reply), Refusal::NotSignedIn(NotSignedIn::SessionEnded));

    // The right note for the other account's live session: the very same bytes.
    let right = bench.runtime.block_on(bench.accounts.sign_out(None, note(second.sign_out, 2)));
    assert_eq!((right.status, right.body.bytes()), (wrong.status, wrong.body.bytes()));
    let list = account::ListDevices { head: bench::head(first.restore), after: None };
    assert_eq!(bench.runtime.block_on(bench.accounts.devices(second.session, list)).status, 401, "the right note ended its session");
    // A note for a session there never was: the same again.
    let none = SignOutNote { session: account::SessionId::new(9_999).unwrap(), secret: SignOutSecret::from_bytes([1; 32]) };
    let nothing = bench.runtime.block_on(bench.accounts.sign_out(None, note(none, 3)));
    assert_eq!((nothing.status, nothing.body.bytes()), (wrong.status, wrong.body.bytes()));
}

fn change(bench: &Bench, session: &account::Session, restore: u64, request: u64, current: &str, new: &str) -> u16 {
    let request = account::ChangePassword {
        head: bench::head(restore),
        request_id: RequestId::new(request).unwrap(),
        current_password: account::Password::presented(current).unwrap(),
        new_password: account::Password::new(new).unwrap(),
    };
    let copy = account::Session::from_bytes(*session.expose_bytes());
    bench.runtime.block_on(bench.accounts.change_password(copy, request)).status
}

fn lists(bench: &Bench, session: &account::Session, restore: u64) -> u16 {
    let copy = account::Session::from_bytes(*session.expose_bytes());
    bench.runtime.block_on(bench.accounts.devices(copy, account::ListDevices { head: bench::head(restore), after: None })).status
}

/// B2 (the review of S8): a password "changed" to the one already set is refused as malformed,
/// however often it is sent, and never ends another session; a real change sent again after
/// another device has signed in with the new password ends nothing.
#[test]
fn a_change_to_the_same_password_is_refused_and_a_retried_change_ends_no_new_session() {
    const NEW: &str = "another made-up password for the bench";
    let (bench, made) = Bench::new("same-password", &[("same@example.test", false)]);
    let a = bench::signed_in(&bench.sign_in(bench::with_code(&made[0].code, PASSWORD), 1));
    let b = bench::signed_in(&bench.sign_in(bench::with_password(made[0].email, PASSWORD), 2));
    for _ in 0..2 {
        assert_eq!(change(&bench, &a.session, a.restore, 7, PASSWORD, PASSWORD), 400);
        assert_eq!(lists(&bench, &b.session, b.restore), 200, "a refused change ends no session");
    }
    // A real change: every other session ends. The other device signs in with the new one.
    assert_eq!(change(&bench, &a.session, a.restore, 8, PASSWORD, NEW), 200);
    assert_eq!(lists(&bench, &b.session, b.restore), 401);
    bench.advance(2_000);
    let c = bench::signed_in(&bench.sign_in(bench::with_password(made[0].email, NEW), 2));
    // The first call sent again: its present password is no longer the present one.
    assert_eq!(change(&bench, &a.session, a.restore, 8, PASSWORD, NEW), 403);
    assert_eq!(lists(&bench, &c.session, c.restore), 200, "the retried change ended a session made after it");
}

/// C1 (the review of S8): while strangers flood sign-in (new devices, made-up emails), a device
/// the backend has seen signs in, and a recovery code is redeemed, each at the first try: the
/// strangers fill only their own lane of the line, and are told "busy, come back in a second".
#[test]
fn a_known_device_and_a_recovery_code_get_through_a_flood_of_strangers() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let (bench, made) = Bench::new("flood", &[("known@example.test", false), ("recover@example.test", true)]);
    bench::signed_in(&bench.sign_in(bench::with_code(&made[0].code, PASSWORD), 1));
    bench.advance(2_000);
    let stop = Arc::new(AtomicBool::new(false));
    let busy = Arc::new(AtomicUsize::new(0));
    let asked = Arc::new(AtomicUsize::new(0));
    let mut flood = Vec::new();
    for n in 0..256u32 {
        let (accounts, stop, busy, asked) = (bench.accounts.clone(), stop.clone(), busy.clone(), asked.clone());
        flood.push(bench.runtime.spawn(async move {
            let mut round = 0u32;
            while !stop.load(Ordering::SeqCst) {
                round += 1;
                let email = format!("stranger{n}-{round}@example.test");
                let mut request = bench::sign_in(bench::with_password(&email, "not anyone's password at all"), 0);
                let mut secret = [0x77u8; 32];
                secret[..4].copy_from_slice(&n.to_be_bytes());
                secret[4..8].copy_from_slice(&round.to_be_bytes());
                request.device_secret = account::DeviceSecret::from_bytes(secret);
                let answer = accounts.sign_in(request, false).await;
                asked.fetch_add(1, Ordering::SeqCst);
                if answer.status == 429 {
                    busy.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            }
        }));
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    let known = bench.sign_in(bench::with_password(made[0].email, PASSWORD), 1);
    let recovered = bench.sign_in(bench::with_code(&made[1].code, PASSWORD), 9);
    stop.store(true, Ordering::SeqCst);
    for task in flood {
        bench.runtime.block_on(task).unwrap();
    }
    assert!(busy.load(Ordering::SeqCst) > 0, "the flood never filled its lane ({} asked)", asked.load(Ordering::SeqCst));
    assert_eq!(known.status, 200, "the known device was not let through: {:?}", account::read_reply::<account::SignedIn>(known.status, known.body.bytes()));
    assert_eq!(recovered.status, 200, "the recovery code was not let through: {:?}", account::read_reply::<account::SignedIn>(recovered.status, recovered.body.bytes()));
}
