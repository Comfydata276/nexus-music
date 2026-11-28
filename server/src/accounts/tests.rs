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
