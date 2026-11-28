//! 8.3's "one answer, the same work" measured (the review of S8, C4): a wrong password for an
//! account switched on, an email with no account, and a wrong password for an account switched
//! off, over HTTP to the program, interleaved, 200 of each with an email of its own (so no wait
//! is met), first with nothing else asked ("serial"), then with both password workers kept
//! busy by other sign-ins ("loaded"); and then one fixed email of each kind guessed over and
//! over, whose answers are mostly "come back in N seconds". Each distribution is printed: n,
//! percentiles, the mean, and for each pair the chance that one kind's answer is the slower
//! (0.5 is "no telling them apart") with the difference of means and its 95% interval.
//!
//! Run by hand, in the release profile (Argon2 as it runs in the container):
//! `cargo test --release --manifest-path server/Cargo.toml --test timing -- --ignored --nocapture`

use std::io::{BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nexus_core::nexus::account::{self, Credential, DeviceKind, DeviceSecret, Email, Name, Password, PasswordDoor, SignIn};
use nexus_core::nexus::kept::{DataChange, Effects, Status, Update};
use nexus_core::nexus::proto::{SPEAKS, Version};
use nexus_music_server::accounts::check::PasswordWorkers;
use nexus_music_server::accounts::setup;
use nexus_music_server::store::keeper::{First, Keeper};

const PROGRAM: &str = env!("CARGO_BIN_EXE_nexus-music-server");
const PER_KIND: usize = 200;
const GUESS: &str = "a guess that is not anyone's password";

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64
}

fn post(at: SocketAddr, body: &[u8]) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(at).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    let head = format!("POST {} HTTP/1.1\r\nHost: nexus\r\nX-Nexus-Scheme: https\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", account::SIGN_IN, body.len());
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    let mut from = BufReader::new(stream);
    let head = nexus_music_server::probe::read_head(&mut from).unwrap().expect("an answer");
    let mut bytes = Vec::new();
    nexus_music_server::probe::body(&head, from).unwrap().read_to_end(&mut bytes).unwrap();
    (head.status, bytes)
}

fn sign_in(email: &str, password: &str, device: [u8; 32]) -> Vec<u8> {
    let request = SignIn {
        credential: Credential::Password { email: Email::new(email).unwrap(), password: Password::presented(password).unwrap() },
        device_secret: DeviceSecret::from_bytes(device),
        device_name: Name::new("A PC").unwrap(),
        device_kind: DeviceKind::Pc,
        app_version: Version::parse("0.41.0").unwrap(),
        speaks: SPEAKS,
        restore: None,
    };
    account::encode(&request).unwrap().bytes().to_vec()
}

fn device(tag: u8, n: usize) -> [u8; 32] {
    let mut secret = [tag; 32];
    secret[..8].copy_from_slice(&(n as u64).to_be_bytes());
    secret
}

struct Sample {
    ms: f64,
    status: u16,
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn report(phase: &str, kinds: &[(&str, &[Sample])]) {
    println!("--- {phase}");
    for (name, samples) in kinds {
        let mut ms: Vec<f64> = samples.iter().map(|s| s.ms).collect();
        ms.sort_by(f64::total_cmp);
        let mean = ms.iter().sum::<f64>() / ms.len() as f64;
        let statuses: std::collections::BTreeMap<u16, usize> = samples.iter().fold(Default::default(), |mut m, s| {
            *m.entry(s.status).or_default() += 1;
            m
        });
        println!(
            "{name:32} n {:3} p10 {:7.2} p25 {:7.2} median {:7.2} p75 {:7.2} p90 {:7.2} max {:7.2} mean {:7.2} ms  statuses {statuses:?}",
            ms.len(),
            percentile(&ms, 0.10),
            percentile(&ms, 0.25),
            percentile(&ms, 0.5),
            percentile(&ms, 0.75),
            percentile(&ms, 0.9),
            ms[ms.len() - 1],
            mean
        );
    }
    for i in 0..kinds.len() {
        for j in i + 1..kinds.len() {
            let (a, b) = (kinds[i].1, kinds[j].1);
            // The chance that a random answer of the first kind is slower than one of the
            // second (Mann-Whitney's AUC).
            let mut wins = 0.0;
            for x in a {
                for y in b {
                    wins += if x.ms > y.ms { 1.0 } else if x.ms == y.ms { 0.5 } else { 0.0 };
                }
            }
            let auc = wins / (a.len() * b.len()) as f64;
            let stats = |s: &[Sample]| {
                let mean = s.iter().map(|x| x.ms).sum::<f64>() / s.len() as f64;
                let var = s.iter().map(|x| (x.ms - mean).powi(2)).sum::<f64>() / (s.len() - 1) as f64;
                (mean, var / s.len() as f64)
            };
            let ((ma, va), (mb, vb)) = (stats(a), stats(b));
            let half = 1.96 * (va + vb).sqrt();
            println!("  {} vs {}: slower-chance {auc:.3}; mean difference {:+.2} ms (95% {:+.2} to {:+.2})", kinds[i].0, kinds[j].0, ma - mb, ma - mb - half, ma - mb + half);
        }
    }
}

#[test]
#[ignore = "a measurement, run by hand in the release profile"]
fn three_kinds_of_wrong_login_take_the_same_time_serial_and_loaded() {
    let root = std::env::temp_dir().join(format!("nexus-server-timing-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("data")).unwrap();
    std::fs::create_dir_all(root.join("state")).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let settings: Vec<(&str, String)> = vec![
        ("NEXUS_MUSIC_SERVER", "http://127.0.0.1:9".into()),
        ("NEXUS_PROXY", "127.0.0.1".into()),
        ("NEXUS_DATA", root.join("data").display().to_string()),
        ("NEXUS_STATE", root.join("state").display().to_string()),
        ("NEXUS_LISTEN", format!("127.0.0.1:{port}")),
    ];

    // The accounts, each with a password of its own (and so a salt of its own): for each of the
    // two phases, 200 switched on and 200 switched off; one fixed target of each kind; and two
    // whose devices sign in over and over to keep the known-device worker busy.
    let on = |phase: usize, n: usize| format!("on-{phase}-{n}@example.org");
    let off = |phase: usize, n: usize| format!("off-{phase}-{n}@example.org");
    let made = Instant::now();
    {
        let (mut keeper, _) = Keeper::open(&root.join("state"), &root.join("data"), now_ms()).unwrap();
        keeper.mark_set_up(now_ms()).unwrap();
        let workers = PasswordWorkers::new(setup::dummy_salt_key(&keeper)).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let mut emails: Vec<(String, bool)> = Vec::new();
        for phase in 0..3 {
            let count = if phase == 2 { 1 } else { PER_KIND };
            for n in 0..count {
                emails.push((on(phase, n), true));
                emails.push((off(phase, n), false));
            }
        }
        emails.push(("busy-0@example.org".into(), true));
        emails.push(("busy-1@example.org".into(), true));
        for (email, switched_on) in &emails {
            let password = format!("the right password of {email}");
            let typed = Email::new(email).unwrap();
            let verifier = runtime.block_on(workers.fingerprint(&typed, PasswordDoor::RedeemedCode, None, Password::new(&password).unwrap())).unwrap();
            let number = keeper.make_account(email, "Someone", false, First::Password(&verifier), now_ms()).unwrap();
            if !switched_on {
                let account = keeper.load(number).unwrap();
                let mut moved = account.apart();
                moved.account.status = Status::Off;
                moved.account.revision += 1;
                let mut rows = account.rows();
                rows.account = moved.account.clone();
                let effects = Effects { high_water: now_ms(), updates: vec![Update { apart_first: moved.clone(), data: rows, change: DataChange::Keep, music: None, apart_last: moved }] };
                keeper.persist(&effects, &[], now_ms()).unwrap();
            }
        }
        keeper.close();
    }
    println!("{} accounts made in {:.1} s", PER_KIND * 4 + 4, made.elapsed().as_secs_f64());

    let mut child: Child = {
        let mut command = Command::new(PROGRAM);
        command.env_clear();
        for (name, value) in &settings {
            command.env(name, value);
        }
        command.stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap()
    };
    let at: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let started = Instant::now();
    while TcpStream::connect(at).is_err() {
        assert!(started.elapsed() < Duration::from_secs(20));
        std::thread::sleep(Duration::from_millis(50));
    }
    // The busy accounts' devices sign in once, so that they are known.
    for n in 0..2 {
        let email = format!("busy-{n}@example.org");
        assert_eq!(post(at, &sign_in(&email, &format!("the right password of {email}"), device(0xb0, n))).0, 200);
    }

    let mut answers = std::collections::BTreeSet::new();
    let mut stranger = 0usize;
    let mut measure = |phase: usize, answers: &mut std::collections::BTreeSet<Vec<u8>>| -> [Vec<Sample>; 3] {
        let mut kinds: [Vec<Sample>; 3] = Default::default();
        for n in 0..PER_KIND {
            for (kind, email) in [(0, on(phase, n)), (1, format!("no-{phase}-{}@example.org", { stranger += 1; stranger })), (2, off(phase, n))] {
                let asked = Instant::now();
                let (status, body) = post(at, &sign_in(&email, GUESS, device(0x5a, kind * 100_000 + n)));
                kinds[kind].push(Sample { ms: asked.elapsed().as_secs_f64() * 1000.0, status });
                if status == 403 {
                    answers.insert(body);
                }
            }
        }
        kinds
    };

    let serial = measure(0, &mut answers);
    // Loaded: the known-device worker kept busy by the busy accounts' right sign-ins, and the
    // other door's worker by strangers' guesses, from four threads, while the same is measured.
    let stop = Arc::new(AtomicBool::new(false));
    let background = Arc::new(AtomicUsize::new(0));
    let mut threads = Vec::new();
    for t in 0..4 {
        let (stop, background) = (stop.clone(), background.clone());
        threads.push(std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(Ordering::SeqCst) {
                n += 1;
                let body = if t < 2 {
                    let email = format!("busy-{t}@example.org");
                    sign_in(&email, &format!("the right password of {email}"), device(0xb0, t))
                } else {
                    sign_in(&format!("load-{t}-{n}@example.org"), GUESS, device(0xc0 + t as u8, n))
                };
                post(at, &body);
                background.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    let loaded = measure(1, &mut answers);
    stop.store(true, Ordering::SeqCst);
    for thread in threads {
        thread.join().unwrap();
    }
    let background = background.load(Ordering::SeqCst);

    // One fixed email of each kind, guessed over and over, interleaved: after the first, the
    // answers are "come back in N seconds" while the wait lasts.
    let mut fixed: [Vec<Sample>; 3] = Default::default();
    for n in 0..PER_KIND {
        for (kind, email) in [(0, on(2, 0)), (1, "no-2-0@example.org".to_string()), (2, off(2, 0))] {
            let asked = Instant::now();
            let (status, _) = post(at, &sign_in(&email, GUESS, device(0x6a, kind * 100_000 + n)));
            fixed[kind].push(Sample { ms: asked.elapsed().as_secs_f64() * 1000.0, status });
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&root);

    let names = ["wrong password, switched on", "unknown email", "wrong password, switched off"];
    report("serial (nothing else asked)", &[(names[0], &serial[0]), (names[1], &serial[1]), (names[2], &serial[2])]);
    report(&format!("loaded (both workers kept busy: {background} other sign-ins meanwhile)"), &[(names[0], &loaded[0]), (names[1], &loaded[1]), (names[2], &loaded[2])]);
    report("one fixed email of each kind, guessed over and over (waits)", &[(names[0], &fixed[0]), (names[1], &fixed[1]), (names[2], &fixed[2])]);
    assert_eq!(answers.len(), 1, "the wrong logins are not one answer");
}
