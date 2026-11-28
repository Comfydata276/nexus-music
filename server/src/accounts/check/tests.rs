use super::*;
use nexus_core::nexus::account::{
    self, CODES_PER_SECOND, NotAllowed, OTHER_PASSWORDS_WAITING, PASSWORD_WAIT_MEMORY_MS,
    PASSWORD_WAIT_NAMES, SignInReply,
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed},
        mpsc::{self, Receiver, Sender},
    },
    task::Poll,
};
use tokio::runtime::{Builder, Runtime};

const GOOD: &str = "synthetic correct password";
const BAD: &str = "synthetic incorrect password";

fn runtime() -> Runtime {
    Builder::new_current_thread().enable_time().build().unwrap()
}

fn password(text: &str) -> Password {
    Password::new(text).unwrap()
}

fn email(name: &str) -> Email {
    Email::new(&format!("{name}@example.test")).unwrap()
}

fn device() -> Option<DeviceId> {
    DeviceId::new(1)
}

fn stored(profile: Profile) -> StoredPassword {
    StoredPassword {
        hash: [1; HASH_BYTES],
        salt: [5; PASSWORD_SALT_BYTES],
        memory: profile.memory,
        passes: profile.passes,
        lanes: profile.lanes,
    }
}

fn fake_hash(password: &Password, _: &Salt, _: Profile) -> Result<Hash, Refusal> {
    Ok(Zeroizing::new([u8::from(password.expose() == GOOD); HASH_BYTES]))
}

fn pool(clock: &Arc<AtomicU64>, slow: Box<Slow>) -> PasswordWorkers {
    let time = Arc::clone(clock);
    PasswordWorkers::with_engine(Engine {
        clock: Arc::new(move || time.load(Relaxed)),
        waits: Mutex::default(),
        slow,
        dummy_key: DummySaltKey::from_bytes([27; 32]),
    }).unwrap()
}

struct Gate {
    started: Receiver<()>,
    release: Sender<()>,
}

impl Gate {
    fn wait(&self) {
        self.started.recv_timeout(Duration::from_secs(3)).unwrap();
    }

    fn release(&self) {
        self.release.send(()).unwrap();
    }
}

fn gated_pool(clock: &Arc<AtomicU64>) -> (PasswordWorkers, Gate, Gate) {
    fn gate() -> (Gate, impl Fn() + Send + Sync) {
        let (started, reached) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let released = Mutex::new(released);
        let gate = Gate { started: reached, release };
        let block = move || {
            started.send(()).unwrap();
            let _ = released.lock().unwrap().recv_timeout(Duration::from_secs(3));
        };
        (gate, block)
    }

    let (known, stop_known) = gate();
    let (other, stop_other) = gate();
    let workers = pool(clock, Box::new(move |password, salt, profile| {
        match thread::current().name() {
            Some("nexus-password-known") => stop_known(),
            Some("nexus-password-other") => stop_other(),
            _ => unreachable!("work escaped the two workers"),
        }
        fake_hash(password, salt, profile)
    }));
    (workers, known, other)
}

fn submit(
    workers: &PasswordWorkers,
    door: PasswordDoor,
    name: &str,
    text: &str,
    has_password: bool,
) -> Result<Pending, Refusal> {
    let stored = has_password.then(|| stored(CURRENT));
    workers.lane(door).submit(&email(name), if door == PasswordDoor::KnownDevice { device() } else { None }, password(text), Work::Check(stored))
}

fn busy(seconds: Option<u64>) -> Refusal {
    Refusal::Busy { retry_after_seconds: seconds.map(|seconds| account::Wait::new(seconds).unwrap()) }
}

fn matched(outcome: Answer) {
    assert!(matches!(outcome, Ok(Completed::Checked(Checked::Match { replacement: None, .. }))));
}

#[test]
fn known_device_runs_while_the_stranger_and_its_queue_are_full() {
    use PasswordDoor::{KnownDevice, Other};
    let (workers, known, other) = gated_pool(&Arc::new(AtomicU64::new(0)));
    let running = submit(&workers, Other, "stranger", BAD, false).unwrap();
    other.wait();
    let waiting: Vec<_> = (0..OTHER_PASSWORDS_WAITING)
        .map(|n| submit(&workers, Other, &format!("queued{n}"), BAD, false).unwrap())
        .collect();

    let owner = submit(&workers, KnownDevice, "owner", GOOD, true).unwrap();
    known.wait();
    known.release();
    matched(runtime().block_on(owner.finish()));
    assert_eq!(workers.other.state.lock().unwrap().waiting.len(), OTHER_PASSWORDS_WAITING);

    drop(waiting);
    other.release();
    assert!(matches!(runtime().block_on(running.finish()), Ok(Completed::Checked(Checked::Wrong))));
}

#[test]
fn ninth_waiter_is_busy_and_eight_wait_no_longer_than_ten_seconds() {
    use PasswordDoor::Other;
    let clock = Arc::new(AtomicU64::new(0));
    let (workers, _, other) = gated_pool(&clock);
    let running = submit(&workers, Other, "running", GOOD, true).unwrap();
    other.wait();
    let waiting: Vec<_> = (0..OTHER_PASSWORDS_WAITING)
        .map(|n| submit(&workers, Other, &format!("waiter{n}"), GOOD, true).unwrap())
        .collect();
    assert_eq!(submit(&workers, Other, "ninth", GOOD, true).err(), Some(busy(None)));

    let rt = runtime();
    clock.store(PASSWORD_QUEUE_MS - 1, Relaxed);
    assert_eq!(submit(&workers, Other, "ninth", GOOD, true).err(), Some(busy(None)));
    clock.store(PASSWORD_QUEUE_MS, Relaxed);
    for pending in waiting {
        assert_eq!(rt.block_on(pending.finish()).unwrap_err(), busy(None));
    }
    assert!(workers.other.state.lock().unwrap().waiting.is_empty());
    assert!(other.started.try_recv().is_err());
    other.release();
    matched(rt.block_on(running.finish()));
}

#[test]
fn dropping_active_and_queued_callers_neither_frees_nor_leaks_the_worker() {
    use PasswordDoor::Other;
    let (workers, _, other) = gated_pool(&Arc::new(AtomicU64::new(0)));
    let rt = runtime();
    let name = email("active");
    let mut active = Box::pin(workers.check(&name, Other, None, password(BAD), None));
    rt.block_on(std::future::poll_fn(|cx| {
        assert!(active.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    }));
    other.wait();
    drop(active);

    let canceled = submit(&workers, Other, "cancelled", GOOD, true).unwrap();
    drop(canceled);
    let next = submit(&workers, Other, "next", GOOD, true).unwrap();
    assert_eq!(workers.other.state.lock().unwrap().waiting.len(), 1);
    assert!(other.started.try_recv().is_err());

    other.release();
    other.wait();
    other.release();
    matched(rt.block_on(next.finish()));
    assert_eq!(submit(&workers, Other, "active", GOOD, true).err(), Some(busy(Some(1))));
    assert!(workers.other.state.lock().unwrap().running.is_none());
}

#[test]
fn backoff_doubles_to_one_minute_and_correct_input_can_retry_after_it() {
    use PasswordDoor::Other;
    let clock = Arc::new(AtomicU64::new(0));
    let workers = pool(&clock, Box::new(fake_hash));
    let rt = runtime();
    let who = email("owner");
    for seconds in [1, 2, 4, 8, 16, 32, 60, 60] {
        assert!(matches!(rt.block_on(workers.check(&who, Other, None, password(BAD), Some(stored(CURRENT)))),
            Ok(Checked::Wrong)));
        assert_eq!(submit(&workers, Other, "owner", GOOD, true).err(), Some(busy(Some(seconds))));
        assert!(workers.other.state.lock().unwrap().waiting.is_empty());
        clock.fetch_add(seconds * 1000 - 1, Relaxed);
        assert_eq!(submit(&workers, Other, "owner", GOOD, true).err(), Some(busy(Some(1))));
        clock.fetch_add(1, Relaxed);
    }
    assert!(matches!(rt.block_on(workers.check(&who, Other, None, password(GOOD), Some(stored(CURRENT)))),
        Ok(Checked::Match { replacement: None, .. })));
}

#[test]
fn each_door_serializes_the_same_account_and_can_run_independently() {
    use PasswordDoor::{KnownDevice, Other};
    let (workers, known, other) = gated_pool(&Arc::new(AtomicU64::new(0)));
    let first_known = submit(&workers, KnownDevice, "owner", GOOD, true).unwrap();
    known.wait();
    let first_other = submit(&workers, Other, "owner", GOOD, true).unwrap();
    other.wait();

    assert_eq!(submit(&workers, KnownDevice, "owner", GOOD, true).err(), Some(busy(None)));
    let second_other = submit(&workers, Other, "owner", GOOD, true).unwrap();
    assert_eq!(workers.other.state.lock().unwrap().waiting.len(), 1);
    assert!(known.started.try_recv().is_err());
    assert!(other.started.try_recv().is_err());

    known.release();
    other.release();
    let rt = runtime();
    matched(rt.block_on(first_known.finish()));
    matched(rt.block_on(first_other.finish()));
    other.wait();
    other.release();
    matched(rt.block_on(second_other.finish()));
}

#[test]
fn known_backoff_is_keyed_by_account_and_verified_device_and_resets_on_restart() {
    use PasswordDoor::{KnownDevice, Other};
    let clock = Arc::new(AtomicU64::new(0));
    let workers = pool(&clock, Box::new(fake_hash));
    let rt = runtime();
    let who = email("owner");
    let another = email("another");
    let second_device = DeviceId::new(2);
    let check = |workers: &PasswordWorkers, who: &Email, door, device, text| {
        rt.block_on(workers.check(who, door, device, password(text), Some(stored(CURRENT))))
    };

    assert_eq!(check(&workers, &who, KnownDevice, None, GOOD).err(), Some(busy(None)));
    for seconds in [1, 2, 4, 8, 16, 32, 60] {
        assert!(matches!(check(&workers, &who, KnownDevice, device(), BAD), Ok(Checked::Wrong)));
        assert_eq!(check(&workers, &who, KnownDevice, device(), GOOD).err(), Some(busy(Some(seconds))));
        assert!(matches!(check(&workers, &who, KnownDevice, second_device, GOOD), Ok(Checked::Match { .. })));
        assert!(matches!(check(&workers, &another, KnownDevice, device(), GOOD), Ok(Checked::Match { .. })));
        assert!(matches!(check(&workers, &who, Other, None, GOOD), Ok(Checked::Match { .. })));
        if seconds != 60 {
            clock.fetch_add(seconds * 1_000, Relaxed);
        }
    }
    drop(workers);
    let restarted = pool(&clock, Box::new(fake_hash));
    assert!(matches!(check(&restarted, &who, KnownDevice, device(), GOOD), Ok(Checked::Match { .. })));
}

#[test]
fn a_queued_duplicate_comes_back_as_soon_as_the_in_flight_check_fails() {
    use PasswordDoor::Other;
    let (workers, _, other) = gated_pool(&Arc::new(AtomicU64::new(0)));
    let first = submit(&workers, Other, "same", BAD, true).unwrap();
    other.wait();
    let ahead = submit(&workers, Other, "ahead", GOOD, true).unwrap();
    let duplicate = submit(&workers, Other, "same", GOOD, true).unwrap();
    other.release();
    other.wait();
    let rt = runtime();
    assert_eq!(rt.block_on(duplicate.finish()).unwrap_err(), busy(Some(1)));
    assert!(matches!(rt.block_on(first.finish()), Ok(Completed::Checked(Checked::Wrong))));
    other.release();
    matched(rt.block_on(ahead.finish()));
}

#[test]
fn wait_memory_evicts_the_oldest_after_ten_thousand_and_expires_at_one_hour() {
    use PasswordDoor::{KnownDevice, Other};
    let clock = Arc::new(AtomicU64::new(0));
    let workers = pool(&clock, Box::new(fake_hash));
    let rt = runtime();
    for n in 0..=PASSWORD_WAIT_NAMES {
        clock.store(n as u64, Relaxed);
        let door = if n == 0 { KnownDevice } else { Other };
        let pending = submit(&workers, door, &format!("name-{n}"), BAD, false).unwrap();
        assert!(matches!(rt.block_on(pending.finish()), Ok(Completed::Checked(Checked::Wrong))));
    }

    // Re-recording an evicted name starts at one second. Retained names
    // double even if their original delay has elapsed.
    let mut waits = workers.other.engine.waits.lock().unwrap();
    waits.record_wrong(Other, &email("name-1"), None, PASSWORD_WAIT_NAMES as u64);
    assert_eq!(waits.retry_after(Other, &email("name-1"), None, PASSWORD_WAIT_NAMES as u64).unwrap().seconds(), 2);
    waits.record_wrong(KnownDevice, &email("name-0"), device(), PASSWORD_WAIT_NAMES as u64);
    assert_eq!(waits.retry_after(KnownDevice, &email("name-0"), device(), PASSWORD_WAIT_NAMES as u64).unwrap().seconds(), 1);
    let last = email(&format!("name-{PASSWORD_WAIT_NAMES}"));
    let after_hour = PASSWORD_WAIT_NAMES as u64 + PASSWORD_WAIT_MEMORY_MS;
    let next_to_last = email(&format!("name-{}", PASSWORD_WAIT_NAMES - 1));
    waits.record_wrong(Other, &next_to_last, None, after_hour - 2);
    assert_eq!(waits.retry_after(Other, &next_to_last, None, after_hour - 2).unwrap().seconds(), 2);
    waits.expire(after_hour);
    waits.record_wrong(Other, &last, None, after_hour);
    assert_eq!(waits.retry_after(Other, &last, None, after_hour).unwrap().seconds(), 1);
}

#[test]
fn rollback_cannot_shorten_backoff_and_u64_edges_do_not_overflow() {
    use PasswordDoor::Other;
    let clock = Arc::new(AtomicU64::new(u64::MAX - 500));
    let workers = pool(&clock, Box::new(fake_hash));
    let rt = runtime();
    let attempt = submit(&workers, Other, "edge", BAD, false).unwrap();
    assert!(matches!(rt.block_on(attempt.finish()), Ok(Completed::Checked(Checked::Wrong))));
    for now in [u64::MAX, 0, u64::MAX] {
        clock.store(now, Relaxed);
        assert_eq!(submit(&workers, Other, "edge", GOOD, true).err(), Some(busy(Some(1))));
    }
    assert!(workers.admit_code().is_ok());
    assert!(workers.admit_code().is_ok());
}

#[test]
fn code_limit_is_independent_and_a_new_fingerprint_uses_the_same_workers() {
    use PasswordDoor::{KnownDevice, Other};
    let clock = Arc::new(AtomicU64::new(0));
    let (workers, known, other) = gated_pool(&clock);
    let first = submit(&workers, Other, "incoming", BAD, false).unwrap();
    other.wait();
    let rt = runtime();
    for _ in 0..CODES_PER_SECOND {
        assert_eq!(workers.admit_code(), Ok(()));
    }
    assert_eq!(workers.admit_code(), Err(busy(Some(1))));
    clock.store(999, Relaxed);
    assert_eq!(workers.admit_code(), Err(busy(Some(1))));
    clock.store(1_000, Relaxed);
    assert_eq!(workers.admit_code(), Ok(()));

    let name = email("new");
    let mut pending = Box::pin(workers.fingerprint(&name, KnownDevice, device(), password(GOOD)));
    rt.block_on(std::future::poll_fn(|cx| {
        assert!(pending.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    }));
    known.wait();
    known.release();
    let first_hash = rt.block_on(pending).unwrap();
    assert!(Profile::stored(&first_hash) == CURRENT);

    let next = workers.known.submit(&name, device(), password(GOOD), Work::Fingerprint).unwrap();
    known.wait();
    known.release();
    let Completed::Fingerprint(second_hash) = rt.block_on(next.finish()).unwrap() else {
        panic!("expected a new verifier");
    };
    assert!(first_hash.salt != second_hash.salt);
    other.release();
    assert!(matches!(rt.block_on(first.finish()), Ok(Completed::Checked(Checked::Wrong))));
}

#[test]
fn a_legacy_match_rehashes_sequentially_on_the_reserved_worker() {
    use PasswordDoor::Other;
    let clock = Arc::new(AtomicU64::new(0));
    let (workers, _, other) = gated_pool(&clock);
    let legacy = Profile { passes: 1, ..CURRENT };
    let check = workers.other.submit(
        &email("owner"), None, password(GOOD), Work::Check(Some(stored(legacy))),
    ).unwrap();

    other.wait();
    other.release();
    other.wait();
    assert_eq!(workers.other.state.lock().unwrap().running.as_ref(), Some(&email("owner")));
    assert!(submit(&workers, Other, "stranger", BAD, false).unwrap().queued_at.is_some());
    other.release();
    let outcome = runtime().block_on(check.finish()).unwrap();
    let Completed::Checked(Checked::Match { derived, replacement: Some(replacement) }) = outcome else {
        panic!("expected a matched verifier and its replacement");
    };
    assert!(derived == Fingerprint::from_bytes([1; HASH_BYTES]));
    assert!(Profile::stored(&replacement) == CURRENT);
    assert!(replacement.salt != stored(legacy).salt);
}

fn wrong_reply(outcome: Checked) -> (u16, Vec<u8>) {
    assert!(matches!(outcome, Checked::Wrong));
    let reply = SignInReply::Refused(Refusal::NotAllowed(NotAllowed::WrongLogin));
    (reply.status(), account::encode(&reply).unwrap().bytes().to_vec())
}

#[test]
fn all_wrong_inputs_share_the_wire_answer_and_same_profile() {
    use PasswordDoor::Other;
    let clock = Arc::new(AtomicU64::new(0));
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&calls);
    let workers = pool(&clock, Box::new(move |password, salt, profile| {
        seen.lock().unwrap().push((*salt, profile));
        fake_hash(password, salt, profile)
    }));
    let rt = runtime();
    let expected = (403, br#"{"refused":{"not_allowed":"wrong_login"}}"#.to_vec());
    let off = stored(CURRENT);
    let unknown = email("unknown");
    let profiles = [("wrong", Some(stored(CURRENT))), ("unknown", None), ("off", Some(off))];
    for (name, verifier) in profiles {
        let who = email(name);
        assert_eq!(wrong_reply(rt.block_on(workers.check(&who, Other, None, password(BAD), verifier.clone())).unwrap()), expected);
        assert_eq!(submit(&workers, Other, name, BAD, verifier.is_some()).err(), Some(busy(Some(1))));
    }
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 3);
    assert!(calls.iter().all(|(_, profile)| *profile == CURRENT));
    assert!(calls[1].0 == *DummySaltKey::from_bytes([27; 32]).salt_for(&unknown));
    assert!(calls[0].0 == stored(CURRENT).salt);
    assert!(calls[2].0 == stored(CURRENT).salt);
}

// Run optimized with --ignored --nocapture. Balance the three classes across
// all nine FIFO positions in each set of batches.
#[test]
#[ignore = "real Argon2id timing measurement"]
fn measure_wrong_inputs_with_both_workers_busy() {
    use PasswordDoor::KnownDevice;
    const BATCHES: usize = 30;
    const CLASSES: usize = 3;
    let rt = runtime();
    let start = Instant::now();
    let running_known = Arc::new(AtomicBool::new(false));
    let overlap = Arc::new(AtomicUsize::new(0));
    let total = Arc::new(AtomicUsize::new(0));
    let hash_times = Arc::new(Mutex::new(Vec::new()));
    let times_on_worker = Arc::clone(&hash_times);
    let known_on_worker = Arc::clone(&running_known);
    let overlap_on_worker = Arc::clone(&overlap);
    let total_on_worker = Arc::clone(&total);
    let workers = PasswordWorkers::with_engine(Engine {
        clock: Arc::new(move || start.elapsed().as_millis().try_into().unwrap_or(u64::MAX)),
        waits: Mutex::default(),
        dummy_key: DummySaltKey::from_bytes([27; 32]),
        slow: Box::new(move |password, salt, profile| {
            if thread::current().name() == Some("nexus-password-known") {
                known_on_worker.store(true, Relaxed);
                let result = argon(password, salt, profile);
                known_on_worker.store(false, Relaxed);
                result
            } else {
                total_on_worker.fetch_add(1, Relaxed);
                if known_on_worker.load(Relaxed) {
                    overlap_on_worker.fetch_add(1, Relaxed);
                }
                let started = Instant::now();
                let result = argon(password, salt, profile);
                times_on_worker.lock().unwrap().push(started.elapsed());
                result
            }
        }),
    }).unwrap();

    let real = rt.block_on(workers.fingerprint(&email("real"), KnownDevice, device(), password(GOOD))).unwrap();
    let off = rt.block_on(workers.fingerprint(&email("off"), KnownDevice, device(), password(GOOD))).unwrap();
    let mut samples: [Vec<Duration>; CLASSES] = Default::default();
    thread::scope(|scope| {
        let (stop_tx, stop_rx) = mpsc::channel();
        let workers = &workers;
        scope.spawn(move || {
            let rt = runtime();
            let filler = email("filler");
            while matches!(stop_rx.try_recv(), Err(mpsc::TryRecvError::Empty)) {
                rt.block_on(workers.fingerprint(&filler, KnownDevice, device(), password(GOOD))).unwrap();
            }
        });
        let deadline = Instant::now() + Duration::from_secs(3);
        while !running_known.load(Relaxed) {
            assert!(Instant::now() < deadline, "known load never reached its worker");
            thread::yield_now();
        }

        for batch in 0..BATCHES {
            let mut pending = Vec::new();
            for place in 0..=OTHER_PASSWORDS_WAITING {
                let class = (batch + place) % CLASSES;
                let verifier = match class {
                    0 => Some(real.clone()),
                    1 => None,
                    2 => Some(off.clone()),
                    _ => unreachable!(),
                };
                let name = email(&format!("name-{batch:03}-{place}"));
                let began = Instant::now();
                let attempt = workers.other.submit(&name, None, password(BAD), Work::Check(verifier)).unwrap();
                pending.push((class, began, attempt));
            }
            for (class, began, attempt) in pending {
                let Completed::Checked(checked) = rt.block_on(attempt.finish()).unwrap() else {
                    panic!("expected a check");
                };
                samples[class].push(began.elapsed());
                assert_eq!(wrong_reply(checked), (403, br#"{"refused":{"not_allowed":"wrong_login"}}"#.to_vec()));
            }
        }
        stop_tx.send(()).unwrap();
    });

    let mut hash_samples: [Vec<Duration>; CLASSES] = Default::default();
    let times = hash_times.lock().unwrap();
    assert_eq!(times.len(), BATCHES * (1 + OTHER_PASSWORDS_WAITING));
    for (batch, times) in times.chunks_exact(1 + OTHER_PASSWORDS_WAITING).enumerate() {
        for (place, time) in times.iter().enumerate() {
            hash_samples[(batch + place) % CLASSES].push(*time);
        }
    }
    report_timings("queue + check", samples);
    report_timings("Argon2id only", hash_samples);
    println!("other hashes starting during known work: {}/{}", overlap.load(Relaxed), total.load(Relaxed));
}

fn report_timings(label: &str, mut samples: [Vec<Duration>; 3]) {
    println!("{label}");
    let labels = ["wrong", "unknown", "switched off"];
    for (label, samples) in labels.iter().zip(samples.iter_mut()) {
        samples.sort_unstable();
        let percentile = |percent: usize| samples[(samples.len() * percent).div_ceil(100) - 1].as_secs_f64() * 1_000.0;
        println!("{label}: n={} p10={:.2} p50={:.2} p90={:.2} p95={:.2} max={:.2} ms",
            samples.len(), percentile(10), percentile(50), percentile(90), percentile(95), percentile(100));
    }
    for (first, second) in [(0, 1), (0, 2), (1, 2)] {
        // Rank AUC for classifying a pair by response time alone.
        let mut score = 0;
        for left in &samples[first] {
            for right in &samples[second] {
                score += match left.cmp(right) {
                    std::cmp::Ordering::Less => 0,
                    std::cmp::Ordering::Equal => 1,
                    std::cmp::Ordering::Greater => 2,
                };
            }
        }
        let auc = score as f64 / (2 * samples[first].len() * samples[second].len()) as f64;
        println!("AUC {} vs {}: {auc:.3}", labels[first], labels[second]);
    }
}
