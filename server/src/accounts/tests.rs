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

/// A1 (the review of S8): a code seen as expired stays expired when the clock is then set back
/// across its deadline.
#[test]
fn a_code_seen_expired_stays_expired_through_a_clock_set_back() {
    let (bench, made) = Bench::new("expired", &[("late@example.test", false)]);
    let until = bench::START + account::CODE_LIFETIME_MS;
    bench.set(until);
    assert_eq!(bench.sign_in(bench::with_code(&made[0].code, PASSWORD), 1).status, 403);
    bench.set(until - 60_000);
    let again = bench.sign_in(bench::with_code(&made[0].code, PASSWORD), 1);
    assert_eq!(again.status, 403, "the code was good again after the clock went back");
}

/// B3 (the review of S8): a redemption whose account changes while the new password's
/// fingerprint is made (here its revision moves, as a role change moves it) does not sign in;
/// and of two redemptions of one code at once, at most one does.
#[test]
fn a_redemption_is_fenced_by_a_change_made_during_its_slow_work_and_a_code_is_redeemed_once() {
    use nexus_core::nexus::kept::{DataChange, Effects, Update};
    let (bench, made) = Bench::new("redeem-fence", &[("fence@example.test", false), ("twice@example.test", false)]);
    let number = made[0].number;
    let desk = &bench.accounts.desk;
    let (release, held) = std::sync::mpsc::channel::<()>();
    let accounts = bench.accounts.clone();
    let code = account::OneTimeCode::new(made[0].code.expose()).unwrap();
    bench.runtime.block_on(async {
        // Hold the thread that writes; the redemption's first step waits behind it, and the
        // revision's change behind that.
        let holding = tokio::spawn({
            let accounts = accounts.clone();
            async move { accounts.desk.ask(move |_| held.recv().ok()).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let redeeming = tokio::spawn({
            let accounts = accounts.clone();
            async move { accounts.sign_in(bench::sign_in(bench::with_code(&code, PASSWORD), 1), false).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let changing = desk.ask(move |keeper| {
            let account = keeper.load(number).unwrap();
            let mut moved = account.apart();
            moved.account.revision += 1;
            let mut rows = account.rows();
            rows.account = moved.account.clone();
            let effects = Effects { high_water: bench::START, updates: vec![Update { apart_first: moved.clone(), data: rows, change: DataChange::Keep, music: None, apart_last: moved }] };
            keeper.persist(&effects, &[], bench::START).unwrap();
        });
        release.send(()).unwrap();
        changing.await.unwrap();
        holding.await.unwrap().unwrap();
        let answer = redeeming.await.unwrap();
        assert_ne!(answer.status, 200, "signed in under the revision before the change");
    });
    // Two at once, from two devices: one at most.
    let first = account::OneTimeCode::new(made[1].code.expose()).unwrap();
    let second = account::OneTimeCode::new(made[1].code.expose()).unwrap();
    let (a, b) = bench.runtime.block_on(async {
        let one = tokio::spawn({
            let accounts = accounts.clone();
            async move { accounts.sign_in(bench::sign_in(bench::with_code(&first, PASSWORD), 2), false).await.status }
        });
        let two = tokio::spawn({
            let accounts = accounts.clone();
            async move { accounts.sign_in(bench::sign_in(bench::with_code(&second, PASSWORD), 3), false).await.status }
        });
        (one.await.unwrap(), two.await.unwrap())
    });
    assert_eq!([a, b].iter().filter(|status| **status == 200).count(), 1, "{a} {b}");
}

/// C5 (the review of S8) and M2 (the lead's ruling of 14:41): an account that is not an
/// admin's, with its right password, at the admin's page (whose own origin and marker are the
/// door's, `routes`), gets exactly the answer of a wrong password, and the try is recorded as a
/// wrong one: the next, at once, is told to come back.
#[test]
fn a_correct_password_of_an_account_not_an_admin_s_at_the_page_is_a_wrong_password() {
    let (bench, made) = Bench::new("page-not-admin", &[("member@example.test", false), ("other@example.test", false)]);
    bench::signed_in(&bench.sign_in(bench::with_code(&made[0].code, PASSWORD), 1));
    bench::signed_in(&bench.sign_in(bench::with_code(&made[1].code, PASSWORD), 3));
    bench.advance(2_000);
    let page = |email: &str, password: &str| bench.runtime.block_on(bench.accounts.sign_in(bench::sign_in(bench::with_password(email, password), 2), true));
    // The same answer, byte for byte, as a wrong password at the page (another account's, so
    // that the two waits are apart).
    let wrong = page(made[1].email, "not the password at all, a guess");
    let right = page(made[0].email, PASSWORD);
    assert_eq!(right.status, 403);
    assert_eq!((right.status, right.body.bytes()), (wrong.status, wrong.body.bytes()));
    // Recorded as a wrong try: the next is told to come back, even with the right password.
    let next = page(made[0].email, PASSWORD);
    assert_eq!(next.status, 429, "the page's refusal of a non-admin was not recorded as a wrong try");
}

/// S9-2 (the review of S9): an account that is not an admin's, signed in, learns nothing from
/// an admin's call of which accounts there are, or of whether the address people use is set:
/// the caller is found not to be an admin before anything of the target is looked at.
#[test]
fn an_account_not_an_admin_s_learns_nothing_of_accounts_or_settings_from_an_admin_s_call() {
    use nexus_core::nexus::admin::{self, Action, Applied, Denial, MusicPassword, NamedChange, TypedMusicLogin};
    let (bench, made) = Bench::new("not-admin-calls", &[("member@example.test", false), ("other@example.test", false)]);
    let member = bench::signed_in(&bench.sign_in(bench::with_code(&made[0].code, PASSWORD), 1));
    let copy = || account::Session::from_bytes(*member.session.expose_bytes());
    let id = |number: u64| account::AccountId::new(number).unwrap();
    let refused = |answer: &super::session::Answer| match admin::read_reply::<Applied>(answer.status, answer.body.bytes()).unwrap() {
        admin::Reply::Refused(refusal) => refusal,
        other => panic!("not refused: {other:?}"),
    };
    let grant = |target: u64, request: u64| {
        let change = NamedChange { request_id: RequestId::new(request).unwrap(), action: Action::SetMusicLogin(id(target)) };
        let request = account::GrantRequest { head: bench::head(member.restore), change, password: account::Password::presented(PASSWORD).unwrap() };
        bench.runtime.block_on(bench.accounts.admin_grant(copy(), request))
    };
    let there = grant(made[1].number, 1);
    let not_there = grant(9_999, 2);
    assert_eq!(refused(&there), admin::Refusal::NotAllowed(Denial::Denied));
    assert_eq!((there.status, there.body.bytes()), (not_there.status, not_there.body.bytes()));
    // Setting a login, with no address people use set: the same, whatever the target.
    let set = |target: u64, request: u64| {
        let change = TypedMusicLogin { account: id(target), name: account::Name::new("tester").unwrap(), password: MusicPassword::new("a made-up music password").unwrap() };
        let request = account::GrantedChange { head: bench::head(member.restore), request_id: RequestId::new(request).unwrap(), change, grant: account::GrantSecret::from_bytes([7; 32]) };
        bench.runtime.block_on(bench.accounts.set_music_login(copy(), request))
    };
    let there = set(made[1].number, 3);
    let not_there = set(9_999, 4);
    assert_eq!(refused(&there), admin::Refusal::NotAllowed(Denial::Denied), "the address people use was looked at first");
    assert_eq!((there.status, there.body.bytes()), (not_there.status, not_there.body.bytes()));
}

/// S9-3 (the review of S9): with a music server that takes every connection and never answers,
/// at most `CHECKS_AT_ONCE` logins are being checked at once: one more is told "busy" at once,
/// a sign-in goes through meanwhile, and each check ends "not dealt with" when its
/// unanswered connection closes.
#[test]
fn a_music_server_that_never_answers_holds_two_checks_at_most_and_sign_in_goes_on() {
    use nexus_core::nexus::admin::{self, Action, Applied, MusicPassword, MusicServerAddress, NamedChange, TypedMusicLogin};
    use std::time::Duration;
    let hold = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    hold.set_nonblocking(true).unwrap();
    let internal = format!("http://{}", hold.local_addr().unwrap());
    // Keep both requests open until the test has observed busy and a successful sign-in.
    // The timeouts are watchdogs for a broken test, not the way it releases the checks.
    let watchdog = Duration::from_secs(30);
    let client = crate::music::MusicClient::trusting(tokio_rustls::rustls::RootCertStore::empty()).within(3 * watchdog);
    let music = super::music::MusicSide::new(client, internal).within(3 * watchdog);
    let (bench, made) = Bench::with_music("checks-at-once", &[("admin@example.test", true), ("person@example.test", false)], music);
    let hold = bench.runtime.block_on(async { tokio::net::TcpListener::from_std(hold).unwrap() });
    bench::signed_in(&bench.sign_in(bench::with_code(&made[0].code, PASSWORD), 1));
    let admins: Vec<_> = (1..=3).map(|device| bench::signed_in(&bench.sign_in_at_the_page(bench::with_password(made[0].email, PASSWORD), device))).collect();
    let copy = |n: usize| account::Session::from_bytes(*admins[n].session.expose_bytes());
    let restore = admins[0].restore;
    let grant = |n: usize, request: u64, action: Action| {
        let change = NamedChange { request_id: RequestId::new(request).unwrap(), action };
        let request = account::GrantRequest { head: bench::head(restore), change, password: account::Password::presented(PASSWORD).unwrap() };
        let answer = bench.runtime.block_on(bench.accounts.admin_grant(copy(n), request));
        match admin::read_reply::<account::GrantIssued>(answer.status, answer.body.bytes()).unwrap() {
            admin::Reply::Done(account::GrantIssued { grant }) => grant,
            other => panic!("no grant: {other:?}"),
        }
    };
    let public = account::MusicAddress::new("https://music.example.test").unwrap();
    let secret = grant(0, 1, Action::SetMusicServer(public.clone()));
    let set = account::GrantedChange { head: bench::head(restore), request_id: RequestId::new(1).unwrap(), change: MusicServerAddress { public }, grant: secret };
    assert_eq!(bench.runtime.block_on(bench.accounts.set_music_server(copy(0), set)).status, 200);
    let person = account::AccountId::new(made[1].number).unwrap();
    let login = |n: usize| {
        let request = 10 + n as u64;
        let secret = grant(n, request, Action::SetMusicLogin(person));
        let change = TypedMusicLogin { account: person, name: account::Name::new("tester").unwrap(), password: MusicPassword::new("a made-up music password").unwrap() };
        (copy(n), account::GrantedChange { head: bench::head(restore), request_id: RequestId::new(request).unwrap(), change, grant: secret })
    };
    let (third, third_request) = login(2);
    let checking: Vec<_> = (0..2).map(login).map(|(session, request)| {
        let accounts = bench.accounts.clone();
        bench.runtime.spawn(async move { accounts.set_music_login(session, request).await.status })
    }).collect();
    let held = bench.runtime.block_on(async {
        tokio::time::timeout(watchdog, async { [hold.accept().await.unwrap().0, hold.accept().await.unwrap().0] })
            .await.expect("the two checks never reached the music server")
    });
    let busy = bench.runtime.block_on(async {
        tokio::time::timeout(watchdog, bench.accounts.set_music_login(third, third_request)).await.expect("the third check did not return busy")
    });
    assert_eq!(busy.status, 429, "{:?}", admin::read_reply::<Applied>(busy.status, busy.body.bytes()));
    bench::signed_in(&bench.sign_in(bench::with_code(&made[1].code, PASSWORD), 9));
    assert!(checking.iter().all(|check| !check.is_finished()), "sign-in finished while both checks were still waiting");
    drop(held);
    for check in checking {
        assert_eq!(bench.runtime.block_on(check).unwrap(), 503);
    }
}

/// C1 (the review of S8; the lead's ruling of 15:38), (a) and (b): a sign-out with neither a
/// session nor a note, a call with a session that does not resolve, and a note whose secret is
/// not its session's are each answered without waiting for the thread that writes (held busy
/// here), with the bytes it would have said.
#[test]
fn calls_that_are_no_one_s_are_answered_without_the_thread_that_writes() {
    use std::time::Duration;
    let (bench, made) = Bench::new("no-one-s", &[("someone@example.test", false)]);
    let signed = bench::signed_in(&bench.sign_in(bench::with_code(&made[0].code, PASSWORD), 1));
    let at_once = |answer: super::session::Answer| (answer.status, answer.body.bytes().to_vec());
    // The answers with the thread free, to compare.
    let nobody = account::Session::from_bytes([0x42; 32]);
    let list = |session: account::Session| bench.accounts.devices(session, account::ListDevices { head: bench::head(signed.restore), after: None });
    let neither = || bench.accounts.sign_out(None, SignOut { head: bench::head(signed.restore), request_id: RequestId::new(1).unwrap(), revoke: None });
    let forged = SignOutNote { session: signed.sign_out.session, secret: SignOutSecret::from_bytes([0x5a; 32]) };
    let note = |note: SignOutNote| bench.accounts.sign_out(None, SignOut { head: bench::head(signed.restore), request_id: RequestId::new(2).unwrap(), revoke: Some(note) });
    let expected = at_once(bench.runtime.block_on(list(account::Session::from_bytes([0x42; 32]))));
    assert_eq!(expected.0, 401);
    // The thread that writes, held.
    let (release, held) = std::sync::mpsc::channel::<()>();
    let accounts = bench.accounts.clone();
    let holding = bench.runtime.spawn(async move { accounts.desk.ask(move |_| held.recv().ok()).await });
    std::thread::sleep(Duration::from_millis(50));
    bench.runtime.block_on(async {
        let within = Duration::from_millis(500);
        let gone = tokio::time::timeout(within, list(nobody)).await.expect("a session that does not resolve waited for the thread that writes");
        assert_eq!(at_once(gone), expected);
        let none = tokio::time::timeout(within, neither()).await.expect("a sign-out with neither waited for the thread that writes");
        assert_eq!(at_once(none), expected);
        let forged = tokio::time::timeout(within, note(forged)).await.expect("a forged note waited for the thread that writes");
        assert_eq!(at_once(forged), expected);
    });
    release.send(()).unwrap();
    bench.runtime.block_on(holding).unwrap().unwrap();
    // The live session, and its right note, still work.
    assert_eq!(bench.runtime.block_on(list(account::Session::from_bytes(*signed.session.expose_bytes()))).status, 200);
    assert_eq!(at_once(bench.runtime.block_on(note(signed.sign_out))), expected);
    assert_eq!(bench.runtime.block_on(list(signed.session)).status, 401, "the right note did not end its session");
}

/// C1 (d): a device is "known" (the protected lane, a reserved slot) only to the account it
/// signed in to, and no longer once it is forced out; it is known again when it signs in.
#[test]
fn a_device_is_known_to_its_own_account_and_not_once_forced_out() {
    let (bench, made) = Bench::new("known", &[("first@example.test", false), ("second@example.test", false)]);
    let one = bench::signed_in(&bench.sign_in(bench::with_code(&made[0].code, PASSWORD), 1));
    let two = bench::signed_in(&bench.sign_in(bench::with_password(made[0].email, PASSWORD), 2));
    let email = |n: usize| account::Email::new(made[n].email).unwrap();
    let secret = |device: u8| account::DeviceSecret::from_bytes([device; 32]);
    let admission = &bench.accounts.admission;
    assert!(admission.known_device(&email(0), &secret(1)) && admission.known_device(&email(0), &secret(2)));
    assert!(!admission.known_device(&email(1), &secret(1)), "a device known to one account was known to another");
    let force = account::ForceOut { head: bench::head(one.restore), device: two.device, session: two.session_id };
    assert_eq!(bench.runtime.block_on(bench.accounts.force_out(one.session, force)).status, 200);
    assert!(!admission.known_device(&email(0), &secret(2)), "a device forced out was still known");
    assert!(admission.known_device(&email(0), &secret(1)));
    bench::signed_in(&bench.sign_in(bench::with_password(made[0].email, PASSWORD), 2));
    assert!(admission.known_device(&email(0), &secret(2)));
}

/// The server's side of `admin::Account.music` (S10b's shape): none set, set, or to be typed
/// again, by the rule a device is handed one by.
#[test]
fn the_page_is_told_whether_a_music_login_is_set_or_to_be_typed_again() {
    use nexus_core::nexus::admin::{self, Action, MusicPassword, MusicServerAddress, NamedChange, TypedMusicLogin};
    use super::music::{MusicState, music_state};
    // A stand-in music server: `ping` and `getUser` (for `tester`) as Navidrome answers them.
    let standing = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let internal = format!("http://{}", standing.local_addr().unwrap());
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for mut stream in standing.incoming().flatten() {
            let mut read = Vec::new();
            let mut chunk = [0u8; 4096];
            while !read.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(got) => read.extend_from_slice(&chunk[..got]),
                }
            }
            let body = if read.starts_with(b"POST /rest/getUser ") {
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1","user":{"username":"tester"}}}"#
            } else {
                r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#
            };
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        }
    });
    let client = crate::music::MusicClient::trusting(tokio_rustls::rustls::RootCertStore::empty());
    let (bench, made) = Bench::with_music("music-state", &[("admin@example.test", true), ("person@example.test", false)], super::music::MusicSide::new(client, internal.clone()));
    bench::signed_in(&bench.sign_in(bench::with_code(&made[0].code, PASSWORD), 1));
    let signed = bench::signed_in(&bench.sign_in_at_the_page(bench::with_password(made[0].email, PASSWORD), 2));
    let copy = || account::Session::from_bytes(*signed.session.expose_bytes());
    let grant = |request: u64, action: Action| {
        let change = NamedChange { request_id: RequestId::new(request).unwrap(), action };
        let request = account::GrantRequest { head: bench::head(signed.restore), change, password: account::Password::presented(PASSWORD).unwrap() };
        let answer = bench.runtime.block_on(bench.accounts.admin_grant(copy(), request));
        match admin::read_reply::<account::GrantIssued>(answer.status, answer.body.bytes()).unwrap() {
            admin::Reply::Done(account::GrantIssued { grant }) => grant,
            other => panic!("no grant: {other:?}"),
        }
    };
    let public = account::MusicAddress::new("https://music.example.test").unwrap();
    let set = account::GrantedChange { head: bench::head(signed.restore), request_id: RequestId::new(1).unwrap(), change: MusicServerAddress { public: public.clone() }, grant: grant(1, Action::SetMusicServer(public)) };
    assert_eq!(bench.runtime.block_on(bench.accounts.set_music_server(copy(), set)).status, 200);
    let person = account::AccountId::new(made[1].number).unwrap();
    let change = TypedMusicLogin { account: person, name: account::Name::new("tester").unwrap(), password: MusicPassword::new("a made-up music password").unwrap() };
    let login = account::GrantedChange { head: bench::head(signed.restore), request_id: RequestId::new(2).unwrap(), change, grant: grant(2, Action::SetMusicLogin(person)) };
    assert_eq!(bench.runtime.block_on(bench.accounts.set_music_login(copy(), login)).status, 200);
    let numbers = [made[0].number, made[1].number];
    let states = bench.runtime.block_on(bench.accounts.desk.ask(move |keeper| {
        numbers.map(|number| {
            let state = keeper.load(number).unwrap().state().clone();
            (music_state(keeper, number, &state, &internal), music_state(keeper, number, &state, "http://another.example.test"))
        })
    })).unwrap();
    assert_eq!(states[0], (MusicState::NoneSet, MusicState::NoneSet));
    assert_eq!(states[1], (MusicState::Set, MusicState::TypeAgain), "checked at another NEXUS_MUSIC_SERVER: to be typed again");
}

/// core-fu3 (the audit of the admin's page, M1), on the thread that writes, with no listener in
/// front: an admin's APP session is refused every admin call by kept, the same bytes whether the
/// account it names is there or not; the same admin at the page is answered.
#[test]
fn an_admin_s_app_session_is_refused_every_admin_call_by_kept_whatever_it_names() {
    use nexus_core::nexus::admin::{self, Action, Applied, Denial, MusicPassword, MusicServerAddress, NamedChange, TypedMusicLogin};
    let (bench, made) = Bench::new("app-not-page", &[("admin@example.test", true), ("person@example.test", false)]);
    let app = bench::signed_in(&bench.sign_in(bench::with_code(&made[0].code, PASSWORD), 1));
    let page = bench::signed_in(&bench.sign_in_at_the_page(bench::with_password(made[0].email, PASSWORD), 2));
    let copy = |signed: &account::SignedIn| account::Session::from_bytes(*signed.session.expose_bytes());
    let id = |number: u64| account::AccountId::new(number).unwrap();
    let refused = |answer: &super::session::Answer| match admin::read_reply::<Applied>(answer.status, answer.body.bytes()).unwrap() {
        admin::Reply::Refused(refusal) => refusal,
        other => panic!("not refused: {other:?}"),
    };
    let grant = |signed: &account::SignedIn, target: u64, request: u64| {
        let change = NamedChange { request_id: RequestId::new(request).unwrap(), action: Action::SetMusicLogin(id(target)) };
        let request = account::GrantRequest { head: bench::head(signed.restore), change, password: account::Password::presented(PASSWORD).unwrap() };
        bench.runtime.block_on(bench.accounts.admin_grant(copy(signed), request))
    };
    let set = |signed: &account::SignedIn, target: u64, request: u64| {
        let change = TypedMusicLogin { account: id(target), name: account::Name::new("tester").unwrap(), password: MusicPassword::new("a made-up music password").unwrap() };
        let request = account::GrantedChange { head: bench::head(signed.restore), request_id: RequestId::new(request).unwrap(), change, grant: account::GrantSecret::from_bytes([7; 32]) };
        bench.runtime.block_on(bench.accounts.set_music_login(copy(signed), request))
    };
    let address = |signed: &account::SignedIn| {
        let public = account::MusicAddress::new("https://music.example.test").unwrap();
        let request = account::GrantedChange { head: bench::head(signed.restore), request_id: RequestId::new(9).unwrap(), change: MusicServerAddress { public }, grant: account::GrantSecret::from_bytes([7; 32]) };
        bench.runtime.block_on(bench.accounts.set_music_server(copy(signed), request))
    };
    let read = |signed: &account::SignedIn| bench.runtime.block_on(bench.accounts.get_music_server(copy(signed), admin::GetMusicServer { head: bench::head(signed.restore) }));
    let answers = [grant(&app, made[1].number, 1), grant(&app, 9_999, 2), set(&app, made[1].number, 3), set(&app, 9_999, 4), address(&app), read(&app)];
    for answer in &answers {
        assert_eq!(refused(answer), admin::Refusal::NotAllowed(Denial::Denied));
        assert_eq!((answer.status, answer.body.bytes()), (answers[0].status, answers[0].body.bytes()));
    }
    // The same admin, at the page: a grant, and the read.
    assert_eq!(grant(&page, made[1].number, 5).status, 200);
    assert_eq!(read(&page).status, 200);
}

/// S9c item 3 (S13's ask; core-fu3): the grant for each of requests' two admin actions carries
/// what kept binds it to, made on the thread that writes, and is spent through kept as S13's
/// setters spend it.
mod requests_grants {
    use super::*;
    use crate::accounts::rows;
    use nexus_core::nexus::account::{AccountId, GrantIssued, GrantSecret, Session, SignedIn};
    use nexus_core::nexus::admin::{self, Action, Denial, LidarrKey, LidarrKeyChange, LidarrSealingKey, NamedChange, RequestsEnabled};
    use nexus_core::nexus::kept::{Bearer, Denied, Fingerprint, GrantUse};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    const LIDARR: &str = "http://lidarr:8686";

    fn copy(signed: &SignedIn) -> Session {
        Session::from_bytes(*signed.session.expose_bytes())
    }

    /// The admin's page session, and the person's account number.
    fn page(bench: &Bench, made: &[bench::Made]) -> SignedIn {
        bench::signed_in(&bench.sign_in(bench::with_code(&made[0].code, PASSWORD), 1));
        bench::signed_in(&bench.sign_in_at_the_page(bench::with_password(made[0].email, PASSWORD), 2))
    }

    fn grant(bench: &Bench, signed: &SignedIn, request: u64, action: Action) -> Result<GrantSecret, admin::Refusal> {
        let change = NamedChange { request_id: RequestId::new(request).unwrap(), action };
        let request = account::GrantRequest { head: bench::head(signed.restore), change, password: account::Password::presented(PASSWORD).unwrap() };
        let answer = bench.runtime.block_on(bench.accounts.admin_grant(copy(signed), request));
        match admin::read_reply::<GrantIssued>(answer.status, answer.body.bytes()).unwrap() {
            admin::Reply::Done(GrantIssued { grant }) => Ok(grant),
            admin::Reply::Refused(refusal) => Err(refusal),
        }
    }

    /// Spends a switch grant as S13's setter does: the switch's revision read and the change
    /// made in one turn of the thread that writes.
    fn switch(bench: &Bench, signed: &SignedIn, request: u64, grant: GrantSecret, account: u64, enabled: bool) -> Result<(), Denied> {
        let (session, restore, now) = (copy(signed), signed.restore, bench.now.load(Ordering::SeqCst));
        bench.runtime.block_on(bench.accounts.desk.ask(move |keeper| {
            let hash = Fingerprint::of(session.expose_bytes());
            let (number, device) = rows::session_by_hash(keeper.data().connection(), hash.as_bytes()).unwrap().unwrap();
            let mut caller = keeper.load(number).unwrap();
            let current = keeper.requests_switch(account).unwrap();
            let mut other = (account != number).then(|| keeper.load(account).unwrap());
            let bearer = Bearer { device, hash: &hash, grant: Some(GrantUse { request_id: RequestId::new(request).unwrap(), secret: &grant }) };
            caller
                .request(restore, keeper.clock(now), |turn| turn.set_requests_enabled(bearer, AccountId::new(account).unwrap(), enabled, current.revision, other.as_mut()))
                .apply(|effects| keeper.persist(effects, &[], now))
                .unwrap()
        })).unwrap()
    }

    /// Spends a Lidarr key grant as S13's setter does.
    fn set_lidarr(bench: &Bench, signed: &SignedIn, request: u64, grant: GrantSecret, key: &str) -> Result<(), Denied> {
        let (session, restore, now, key) = (copy(signed), signed.restore, bench.now.load(Ordering::SeqCst), LidarrKey::new(key).unwrap());
        let address = admin::InternalAddress::new(LIDARR).unwrap();
        bench.runtime.block_on(bench.accounts.desk.ask(move |keeper| {
            let hash = Fingerprint::of(session.expose_bytes());
            let (number, device) = rows::session_by_hash(keeper.data().connection(), hash.as_bytes()).unwrap().unwrap();
            let mut caller = keeper.load(number).unwrap();
            let binding = keeper.lidarr(Some(&address)).unwrap().binding.unwrap();
            let sealing = LidarrSealingKey::new(keeper.seal_key());
            let bearer = Bearer { device, hash: &hash, grant: Some(GrantUse { request_id: RequestId::new(request).unwrap(), secret: &grant }) };
            caller
                .request(restore, keeper.clock(now), |turn| turn.set_lidarr(bearer, &binding, &key, &sealing, [request as u8; 24]))
                .apply(|effects| keeper.persist(effects, &[], now))
                .unwrap()
        })).unwrap()
    }

    #[test]
    fn a_switch_grant_is_bound_to_the_switch_s_revision() {
        let (bench, made) = Bench::new("switch-grant", &[("admin@example.test", true), ("person@example.test", false)]);
        let signed = page(&bench, &made);
        let person = made[1].number;
        let off = |enabled| Action::SetRequestsEnabled(RequestsEnabled { account: AccountId::new(person).unwrap(), enabled });
        // Granted and spent: the switch is off, at revision 1.
        let first = grant(&bench, &signed, 1, off(false)).unwrap();
        assert_eq!(switch(&bench, &signed, 1, first, person, false), Ok(()));
        let now = bench.runtime.block_on(bench.accounts.desk.ask(move |keeper| keeper.requests_switch(person).unwrap())).unwrap();
        assert_eq!((now.enabled, now.revision), (false, 1));
        // A grant made at revision 1, the switch moved meanwhile by another: refused when spent.
        let stale = grant(&bench, &signed, 2, off(true)).unwrap();
        let other = grant(&bench, &signed, 3, off(true)).unwrap();
        assert_eq!(switch(&bench, &signed, 3, other, person, true), Ok(()));
        assert!(switch(&bench, &signed, 2, stale, person, true).is_err(), "a grant for a revision gone by was spent");
        // For the admin's own account too; and an account there is not: refused at the grant.
        let own = Action::SetRequestsEnabled(RequestsEnabled { account: AccountId::new(made[0].number).unwrap(), enabled: false });
        let mine = grant(&bench, &signed, 4, own).unwrap();
        assert_eq!(switch(&bench, &signed, 4, mine, made[0].number, false), Ok(()));
        let nobody = Action::SetRequestsEnabled(RequestsEnabled { account: AccountId::new(9_999).unwrap(), enabled: false });
        assert_eq!(grant(&bench, &signed, 5, nobody), Err(admin::Refusal::NotAllowed(Denial::NoSuchAccount)));
    }

    #[test]
    fn a_lidarr_grant_is_bound_to_the_key_the_address_and_the_revision() {
        let (bench, made) = Bench::with_lidarr("lidarr-grant", &[("admin@example.test", true)], LIDARR);
        let signed = page(&bench, &made);
        let key = |text: &str| Action::SetLidarr(LidarrKeyChange { key: LidarrKey::new(text).unwrap() });
        let first = grant(&bench, &signed, 1, key("made-up-lidarr-key-1")).unwrap();
        // Another key than the one granted: refused.
        assert!(set_lidarr(&bench, &signed, 1, first, "made-up-lidarr-key-2").is_err());
        let first = grant(&bench, &signed, 2, key("made-up-lidarr-key-1")).unwrap();
        assert_eq!(set_lidarr(&bench, &signed, 2, GrantSecret::from_bytes(*first.expose_bytes()), "made-up-lidarr-key-1"), Ok(()));
        // Spent once only; and a grant made before the key's revision moved is refused.
        assert!(set_lidarr(&bench, &signed, 2, first, "made-up-lidarr-key-1").is_err());
        let stale = grant(&bench, &signed, 3, key("made-up-lidarr-key-3")).unwrap();
        let other = grant(&bench, &signed, 4, key("made-up-lidarr-key-4")).unwrap();
        assert_eq!(set_lidarr(&bench, &signed, 4, other, "made-up-lidarr-key-4"), Ok(()));
        assert!(set_lidarr(&bench, &signed, 3, stale, "made-up-lidarr-key-3").is_err());
        // The key kept opens at the configured address, and is the last one set.
        let address = admin::InternalAddress::new(LIDARR).unwrap();
        let opened = bench.runtime.block_on(bench.accounts.desk.ask(move |keeper| {
            let kept = keeper.lidarr(Some(&address)).unwrap();
            LidarrSealingKey::new(keeper.seal_key()).open(&kept.binding.unwrap(), &kept.sealed.unwrap()).map(|key| key.expose().to_string())
        })).unwrap();
        assert_eq!(opened.as_deref(), Some("made-up-lidarr-key-4"));
    }

    #[test]
    fn with_no_lidarr_set_up_a_key_is_not_granted() {
        let (bench, made) = Bench::new("no-lidarr", &[("admin@example.test", true)]);
        let signed = page(&bench, &made);
        let action = Action::SetLidarr(LidarrKeyChange { key: LidarrKey::new("made-up-lidarr-key").unwrap() });
        assert_eq!(grant(&bench, &signed, 1, action), Err(admin::Refusal::NotAllowed(Denial::Denied)));
    }

    /// `admin_grant_with`: what binds the grant is made only once the caller is known to be a
    /// live admin at the page; its refusal is the answer; its intent is issued as every grant.
    #[test]
    fn a_grant_with_its_own_binding_is_made_only_for_a_live_admin_at_the_page() {
        let (bench, made) = Bench::new("grant-with", &[("admin@example.test", true), ("person@example.test", false)]);
        let signed = page(&bench, &made);
        let app = bench::signed_in(&bench.sign_in(bench::with_password(made[0].email, PASSWORD), 3));
        let person = AccountId::new(made[1].number).unwrap();
        let admin_number = made[0].number;
        let change = |request| NamedChange { request_id: RequestId::new(request).unwrap(), action: Action::SetMusicLogin(person) };
        let ask = |signed: &SignedIn, request: u64, refuse: bool, called: Arc<AtomicBool>| {
            let request = account::GrantRequest { head: bench::head(signed.restore), change: change(request), password: account::Password::presented(PASSWORD).unwrap() };
            bench.runtime.block_on(bench.accounts.admin_grant_with(copy(signed), request, move |keeper, caller, change| {
                called.store(true, Ordering::SeqCst);
                assert_eq!(caller.state().id.get(), admin_number);
                if refuse {
                    return Err(admin::Refusal::NotAllowed(Denial::Conflict));
                }
                let other = keeper.load(person.get()).unwrap();
                NamedChange::grant_intent(change, Some(other.state())).ok_or(admin::Refusal::Malformed)
            }))
        };
        let called = Arc::new(AtomicBool::new(false));
        assert_eq!(ask(&app, 1, false, called.clone()).status, 403);
        assert!(!called.load(Ordering::SeqCst), "made for an app's session");
        let refused = ask(&signed, 2, true, called.clone());
        assert!(called.load(Ordering::SeqCst));
        assert!(matches!(admin::read_reply::<GrantIssued>(refused.status, refused.body.bytes()).unwrap(), admin::Reply::Refused(admin::Refusal::NotAllowed(Denial::Conflict))));
        let issued = ask(&signed, 3, false, Arc::new(AtomicBool::new(false)));
        assert!(matches!(admin::read_reply::<GrantIssued>(issued.status, issued.body.bytes()).unwrap(), admin::Reply::Done(_)));
    }
}
