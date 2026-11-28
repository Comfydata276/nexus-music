//! Experiment X2 (design 14.2), slice S3's tests: the data and the store kept apart, put back,
//! lost, cut off and filled up. Real files in temporary folders, real SQLite, and real
//! processes: each kill is a child process stopped (`abort`, as a kill: nothing is flushed and
//! nothing unwinds) at one of the points between two durable steps, found by counting them in
//! a run that is not stopped. Made-up names only.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use nexus_core::nexus::kept::{Access, Denied, Device, Fingerprint, Purpose, SECRET_BYTES, Turn};
use nexus_music_server::data::{self, Data, Migration, Opened};
use nexus_music_server::keeper::{self, Failed, Keeper, Started};
use nexus_music_server::{apart, copies, crash, identity};

/// A made-up time, in milliseconds.
const T0: u64 = 1_800_000_000_000;
const MINUTE: u64 = 60_000;

const ALICE: u64 = 1;
const BOB: u64 = 2;
/// Alice's PC (sessions 1 working, 2 waiting), her phone (3), Bob's PC (5).
const A1: u64 = 1;
const A2: u64 = 2;
const B1: u64 = 3;

struct Root {
    path: PathBuf,
}

impl Root {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!("nexus-x2-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(path.join("data")).unwrap();
        fs::create_dir_all(path.join("state")).unwrap();
        Root { path }
    }

    fn at(path: PathBuf) -> Self {
        Root { path }
    }

    fn data(&self) -> PathBuf {
        self.path.join("data")
    }

    fn state(&self) -> PathBuf {
        self.path.join("state")
    }

    /// A copy of both folders, as files, under another name.
    fn copied(&self, name: &str) -> Root {
        let other = Root::new(name);
        copy_folder(&self.data(), &other.data());
        copy_folder(&self.state(), &other.state());
        other
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        if std::env::var_os("X2_CHILD").is_none() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

fn copy_folder(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_folder(&entry.path(), &to.join(entry.file_name()));
        } else {
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

fn hash(n: u8) -> Fingerprint {
    Fingerprint::of(&[n; SECRET_BYTES])
}

/// `run`, again while it is refused only because a folder is in use. (In this test program,
/// a child forked by another test holds the lock file of a folder this thread has just let
/// go of until that child's program begins; the backend itself starts no child.)
fn unlocked<T>(mut run: impl FnMut() -> Result<T, keeper::Refused>) -> Result<T, keeper::Refused> {
    for _ in 0..100 {
        match run() {
            Err(why) if why.0.contains("is in use") => std::thread::sleep(std::time::Duration::from_millis(20)),
            other => return other,
        }
    }
    run()
}

fn open(root: &Root, now: u64) -> (Keeper, Started) {
    match unlocked(|| Keeper::open(&root.state(), &root.data(), now)) {
        Ok(opened) => opened,
        Err(why) => panic!("refused: {why}"),
    }
}

fn opened(root: &Root, now: u64) -> Keeper {
    open(root, now).0
}

/// One decision on a device held in memory, kept (whatever it answered) before its answer.
fn decide<T>(keeper: &mut Keeper, device: &mut Device, id: u64, now: u64, action: impl FnOnce(Turn<'_>) -> T) -> Result<T, Failed> {
    let scope = keeper.authority(id)?;
    let clock = keeper.clock(now);
    device.decide(&scope, clock, action).apply(|effects| keeper.persist(effects))
}

fn name(account: u64) -> &'static str {
    if account == ALICE { "alice" } else { "bob" }
}

fn connect(keeper: &mut Keeper, id: u64, account: u64, session: u8, now: u64) -> Result<Device, Failed> {
    let (mut device, _) = keeper.load(id)?;
    let ticket = decide(keeper, &mut device, id, now, |turn| turn.begin(Purpose::Connect))?.map_err(|why| Failed(format!("{why:?}")))?;
    decide(keeper, &mut device, id, now, |turn| turn.finish(ticket, name(account), Some(hash(session))))?.map_err(|why| Failed(format!("{why:?}")))?;
    Ok(device)
}

fn again(keeper: &mut Keeper, id: u64, account: u64, old: u8, new: u8, now: u64) -> Result<(), Failed> {
    let (mut device, _) = keeper.load(id)?;
    let ticket = decide(keeper, &mut device, id, now, |turn| turn.begin(Purpose::Again(hash(old))))?.map_err(|why| Failed(format!("{why:?}")))?;
    decide(keeper, &mut device, id, now, |turn| turn.finish(ticket, name(account), Some(hash(new))))?.map_err(|why| Failed(format!("{why:?}")))
}

fn leave_to_delete(keeper: &mut Keeper, id: u64, account: u64, session: u8, now: u64) -> Result<(), Failed> {
    let (mut device, _) = keeper.load(id)?;
    let ticket = decide(keeper, &mut device, id, now, |turn| turn.begin(Purpose::Delete(hash(session))))?.map_err(|why| Failed(format!("{why:?}")))?;
    decide(keeper, &mut device, id, now, |turn| turn.finish(ticket, name(account), None))?.map_err(|why| Failed(format!("{why:?}")))
}

/// Whether `session` may read on device `id` now, as C3 decides from both stores. Nothing is
/// kept of the asking, so a waiting session is not promoted by it.
fn reads(keeper: &mut Keeper, id: u64, session: u8, now: u64) -> bool {
    let Ok((mut device, scope)) = keeper.load(id) else { return false };
    let clock = keeper.clock(now);
    let held = keeper.authority(id).map(|scope| keeper.held(account_of(keeper, &scope.account))).unwrap_or(false);
    matches!(device.decide(&scope, clock, |turn| turn.arrive(&hash(session), Access::Read, held)).apply(|_| Ok::<_, ()>(())), Ok(Ok(_)))
}

fn account_of(keeper: &Keeper, name: &str) -> u64 {
    keeper.data().account_named(name).ok().flatten().map_or(0, |(number, _)| number)
}

/// Alice, with her PC (1 working, 2 waiting) and her phone (3); Bob, with his PC (5).
fn world(root: &Root) {
    let mut keeper = opened(root, T0);
    assert_eq!(keeper.account("alice", T0).unwrap().number, ALICE);
    assert_eq!(keeper.device(ALICE, &hash(11), "a made-up PC", "pc", "0.41.0", T0).unwrap(), (A1, true));
    connect(&mut keeper, A1, ALICE, 1, T0).unwrap();
    again(&mut keeper, A1, ALICE, 1, 2, T0 + 1).unwrap();
    assert_eq!(keeper.device(ALICE, &hash(12), "a made-up phone", "phone", "0.41.0", T0).unwrap(), (A2, true));
    connect(&mut keeper, A2, ALICE, 3, T0 + 2).unwrap();
    assert_eq!(keeper.account("bob", T0).unwrap().number, BOB);
    assert_eq!(keeper.device(BOB, &hash(21), "another made-up PC", "pc", "0.41.0", T0).unwrap(), (B1, true));
    connect(&mut keeper, B1, BOB, 5, T0 + 3).unwrap();
    for (id, session) in [(A1, 1), (A1, 2), (A2, 3), (B1, 5)] {
        assert!(reads(&mut keeper, id, session, T0 + 4), "{id} {session}");
    }
    keeper.close();
}

/// Who reads, of every session of the world.
fn readers(keeper: &mut Keeper, now: u64) -> Vec<u8> {
    [(A1, 1), (A1, 2), (A2, 3), (A2, 4), (B1, 5), (A1, 7), (A2, 7)].into_iter().filter(|&(id, session)| reads(keeper, id, session, now)).map(|(_, session)| session).collect()
}

/// Opens, and then opens again: the second start must find the two the same age.
fn settled(root: &Root, now: u64) -> (Keeper, Started) {
    let (keeper, started) = open(root, now);
    keeper.close();
    let (keeper, again) = open(root, now);
    assert_eq!(again, Started::Ready, "the stores were not brought together by the first start ({started:?})");
    (keeper, started)
}

// ---- The child: one scenario, stopped or failed at the n-th point ----

static POINTS: AtomicUsize = AtomicUsize::new(0);

/// Runs in a child process only (`X2_CHILD` names the scenario).
#[test]
fn x2_child() {
    let Ok(scenario) = std::env::var("X2_CHILD") else { return };
    let root = Root::at(PathBuf::from(std::env::var("X2_ROOT").unwrap()));
    let number = |name: &str| std::env::var(name).ok().and_then(|value| value.parse::<usize>().ok()).unwrap_or(0);
    let (kill_at, fail_at) = (number("X2_KILL_AT"), number("X2_FAIL_AT"));
    let log = PathBuf::from(std::env::var("X2_POINTS").unwrap());
    crash::set(Box::new(move |name| {
        let n = POINTS.fetch_add(1, Ordering::SeqCst) + 1;
        use std::io::Write;
        let mut file = fs::OpenOptions::new().create(true).append(true).open(&log).unwrap();
        writeln!(file, "{name}").unwrap();
        drop(file);
        if n == kill_at {
            std::process::abort();
        }
        n == fail_at
    }));
    let outcome = run(&scenario, &root);
    fs::write(root.path.join("outcome"), format!("{outcome:?}")).unwrap();
}

fn run(scenario: &str, root: &Root) -> Result<(), String> {
    let failed = |failed: Failed| failed.0;
    let now = T0 + 10 * MINUTE;
    match scenario {
        "first start" => Keeper::open(&root.state(), &root.data(), now).map(|(keeper, _)| keeper.close()).map_err(|why| why.0),
        "an account made" => opened(root, now).account("carol", now).map(|_| ()).map_err(failed),
        "a device made" => opened(root, now).device(ALICE, &hash(13), "a made-up tablet", "phone", "0.41.0", now).map(|_| ()).map_err(failed),
        "a device connecting" => connect(&mut opened(root, now), A2, ALICE, 7, now).map(|_| ()).map_err(failed),
        "proving again" => again(&mut opened(root, now), A2, ALICE, 3, 4, now).map_err(failed),
        "a waiting session taking the old one's place" => {
            let mut keeper = opened(root, now);
            let (mut device, _) = keeper.load(A1).map_err(failed)?;
            decide(&mut keeper, &mut device, A1, now, |turn| turn.arrive(&hash(2), Access::Read, false).map(|_| ())).map_err(failed)?.map_err(|why| format!("{why:?}"))
        }
        "a device's sessions ended" => {
            let mut keeper = opened(root, now);
            let (mut device, _) = keeper.load(A1).map_err(failed)?;
            decide(&mut keeper, &mut device, A1, now, |turn| turn.end()).map_err(failed)?.map_err(|why| format!("{why:?}"))
        }
        "an account's history deleted" => {
            let mut keeper = opened(root, now);
            let (mut device, _) = keeper.load(A1).map_err(failed)?;
            decide(&mut keeper, &mut device, A1, now, |turn| turn.delete(&hash(1))).map_err(failed)?.map_err(|why| format!("{why:?}"))
        }
        "a device removed" => opened(root, now).remove_device(A2).map_err(failed),
        "a held history kept" => opened(root, now).keep_held(ALICE).map_err(failed),
        "an older copy found at a start" => Keeper::open(&root.state(), &root.data(), now).map(|(keeper, _)| keeper.close()).map_err(|why| why.0),
        "put-back" => keeper::put_back(&root.state(), &root.data(), &the_copy(root), now).map(|_| ()).map_err(|why| why.0),
        "take-as-is" => keeper::take_as_is(&root.state(), &root.data(), now).map(|_| ()).map_err(|why| why.0),
        "the nightly copy" => copies::nightly(&root.data(), now).map(|_| ()),
        "a change of form" => Data::open_with(&root.data(), now, &[data::MIGRATIONS[0], FORM_2]).map(|_| ()),
        other => Err(format!("no scenario {other}")),
    }
}

/// The first nightly copy there is: the one a put-back scenario puts back.
fn the_copy(root: &Root) -> PathBuf {
    copies::list(&root.data()).first().map(|copy| copy.path.clone()).unwrap_or_default()
}

/// A form 2, for the tests alone.
const FORM_2: Migration = Migration { form: 2, sql: "ALTER TABLE devices ADD COLUMN a_test_note TEXT;" };

/// Runs `scenario` in a child on `root`, stopped at the `kill`-th point or failing at the
/// `fail`-th (0: neither). The points it passed, and whether it ended by itself.
fn child(scenario: &str, root: &Root, kill: usize, fail: usize, limit_blocks: Option<u32>) -> (Vec<String>, bool) {
    let log = root.path.join(format!("points-{kill}-{fail}"));
    let _ = fs::remove_file(&log);
    let program = std::env::current_exe().unwrap();
    let mut command = match limit_blocks {
        // A full disk, by the kernel's own limit on how large this process may make a file:
        // every write past it fails (EFBIG), as on a disk with no room left.
        Some(blocks) => {
            let mut command = Command::new("sh");
            command.args(["-c", &format!("trap '' XFSZ; ulimit -f {blocks}; exec \"$0\" \"$@\""), program.to_str().unwrap()]);
            command
        }
        None => Command::new(program),
    };
    command
        .args(["--exact", "x2_child", "--nocapture", "--test-threads=1"])
        .env("X2_CHILD", scenario)
        .env("X2_ROOT", &root.path)
        .env("X2_KILL_AT", kill.to_string())
        .env("X2_FAIL_AT", fail.to_string())
        .env("X2_POINTS", &log);
    let output = command.output().unwrap();
    let points = fs::read_to_string(&log).unwrap_or_default().lines().map(str::to_string).collect();
    (points, output.status.success())
}

/// Where a cut fell, in the points a whole run passes.
struct Cut {
    at: usize,
    points: Vec<String>,
}

impl Cut {
    fn renames(&self) -> Vec<usize> {
        self.points.iter().enumerate().filter(|(_, name)| *name == "store: renamed").map(|(at, _)| at + 1).collect()
    }

    /// Whether the store's first write of the last decision was on the disk: what it took
    /// away has been taken.
    fn took(&self) -> bool {
        self.renames().first().is_some_and(|&at| self.at >= at)
    }

    /// Whether the store's last write was on the disk: what the change gives is given.
    fn gave(&self) -> bool {
        self.renames().last().is_some_and(|&at| self.at >= at)
    }

    fn whole(&self) -> bool {
        self.at == 0
    }
}

/// For every point `scenario` passes on a copy of `setup`: a child stopped there, then a start;
/// `check` is given the start and where the cut fell. Then the run that is not cut.
fn at_every_point(scenario: &str, setup: &Root, check: impl Fn(&Root, &Cut)) -> usize {
    let dry = setup.copied(&format!("{}-dry", scenario.replace(' ', "-")));
    let (points, ended) = child(scenario, &dry, 0, 0, None);
    assert!(ended, "{scenario}: the run that is not cut did not end by itself");
    assert!(!points.is_empty(), "{scenario}: passed no point");
    for at in 1..=points.len() {
        let root = setup.copied(&format!("{}-{at}", scenario.replace(' ', "-")));
        let (_, ended) = child(scenario, &root, at, 0, None);
        assert!(!ended, "{scenario}: not stopped at point {at} ({})", points[at - 1]);
        check(&root, &Cut { at, points: points.clone() });
    }
    check(&dry, &Cut { at: 0, points: points.clone() });
    points.len()
}

// ---- Kills at every point ----

#[test]
fn a_first_start_cut_at_any_point_starts_whole() {
    let setup = Root::new("first");
    let points = at_every_point("first start", &setup, |root, _| {
        let (keeper, started) = settled(root, T0 + 20 * MINUTE);
        assert!(matches!(started, Started::Made | Started::Ready), "{started:?}");
        assert_eq!(identity::read(&root.state()).unwrap(), Some(keeper.backend()));
        keeper.close();
    });
    assert!(points >= 8, "{points}");
}

#[test]
fn making_an_account_or_a_device_cut_at_any_point_leaves_it_made_or_not_and_never_half() {
    let setup = Root::new("make");
    world(&setup);
    at_every_point("an account made", &setup, |root, cut| {
        let (mut keeper, _) = settled(root, T0 + 20 * MINUTE);
        let carol = keeper.data().account_named("carol").unwrap();
        if cut.whole() {
            assert!(carol.is_some());
        }
        // Made again (or found), it is one account, with the number the store gave first.
        let again = keeper.account("carol", T0 + 21 * MINUTE).unwrap();
        assert_eq!(again.number, 3);
        assert_eq!(readers(&mut keeper, T0 + 21 * MINUTE), [1, 2, 3, 5]);
    });
    at_every_point("a device made", &setup, |root, cut| {
        let (mut keeper, _) = settled(root, T0 + 20 * MINUTE);
        let found = keeper.data().device_with_secret(ALICE, &hash(13)).unwrap();
        if cut.whole() {
            assert_eq!(found, Some(4));
        }
        // An id the store gave is never given again, even to the same device made again.
        let (id, _) = keeper.device(ALICE, &hash(13), "a made-up tablet", "phone", "0.41.0", T0 + 21 * MINUTE).unwrap();
        // 4 if the store never gave it; 5 if it did and the data never had it.
        let expected = found.unwrap_or(if cut.took() { 5 } else { 4 });
        assert_eq!(id, expected, "at {}", cut.at);
        assert_eq!(readers(&mut keeper, T0 + 21 * MINUTE), [1, 2, 3, 5]);
    });
}

#[test]
fn a_grant_cut_at_any_point_works_only_once_the_store_has_it() {
    let setup = Root::new("grant");
    world(&setup);
    // Connecting: Alice's phone ends 3 and is given 7.
    at_every_point("a device connecting", &setup, |root, cut| {
        let (mut keeper, _) = settled(root, T0 + 20 * MINUTE);
        let now = T0 + 21 * MINUTE;
        let read = readers(&mut keeper, now);
        assert_eq!(read.contains(&7), cut.gave() || cut.whole(), "{read:?} at {}", cut.at);
        assert!(!(read.contains(&3) && read.contains(&7)));
        assert!(read.contains(&5) && read.contains(&1));
    });
    // Proving again: the phone's 3 goes on working; 4 waits, and works only once the store
    // has it.
    at_every_point("proving again", &setup, |root, cut| {
        let (mut keeper, _) = settled(root, T0 + 20 * MINUTE);
        let read = readers(&mut keeper, T0 + 21 * MINUTE);
        assert!(read.contains(&3), "{read:?}");
        assert_eq!(read.contains(&4), cut.gave() || cut.whole(), "{read:?} at {}", cut.at);
    });
}

#[test]
fn every_ending_cut_at_any_point_holds_once_the_store_has_it() {
    let setup = Root::new("endings");
    world(&setup);
    // The first use of 2 ends 1 (7.5 step 2).
    at_every_point("a waiting session taking the old one's place", &setup, |root, cut| {
        let (mut keeper, _) = settled(root, T0 + 20 * MINUTE);
        let read = readers(&mut keeper, T0 + 21 * MINUTE);
        if cut.took() || cut.whole() {
            assert!(!read.contains(&1), "1 came back at {}: {read:?}", cut.at);
        } else {
            assert!(read.contains(&1), "{read:?} at {}", cut.at);
        }
        if cut.gave() || cut.whole() {
            assert!(read.contains(&2), "{read:?} at {}", cut.at);
        }
        assert!(read.contains(&3) && read.contains(&5));
    });
    // Disconnecting, or the owner ending a device.
    at_every_point("a device's sessions ended", &setup, |root, cut| {
        let (mut keeper, _) = settled(root, T0 + 20 * MINUTE);
        let read = readers(&mut keeper, T0 + 21 * MINUTE);
        if cut.took() || cut.whole() {
            assert_eq!(read, [3, 5], "at {}", cut.at);
        } else {
            assert_eq!(read, [1, 2, 3, 5], "at {}", cut.at);
        }
    });
    at_every_point("a device removed", &setup, |root, cut| {
        let (mut keeper, _) = settled(root, T0 + 20 * MINUTE);
        let read = readers(&mut keeper, T0 + 21 * MINUTE);
        let present = keeper.data().device_with_secret(ALICE, &hash(12)).unwrap().is_some();
        if cut.took() || cut.whole() {
            assert_eq!((present, read.contains(&3)), (false, false), "at {}", cut.at);
            assert!(keeper.load(A2).is_err());
        } else {
            assert_eq!((present, read.contains(&3)), (true, true), "at {}", cut.at);
        }
    });
}

#[test]
fn a_deletion_cut_at_any_point_is_whole_once_the_store_has_it() {
    let setup = Root::new("delete");
    world(&setup);
    {
        let mut keeper = opened(&setup, T0 + 5 * MINUTE);
        leave_to_delete(&mut keeper, A1, ALICE, 1, T0 + 9 * MINUTE).unwrap();
        keeper.close();
    }
    at_every_point("an account's history deleted", &setup, |root, cut| {
        let (mut keeper, _) = settled(root, T0 + 15 * MINUTE);
        let read = readers(&mut keeper, T0 + 16 * MINUTE);
        let alice = keeper.data().account_named("alice").unwrap();
        if cut.took() || cut.whole() {
            // 11.1: the life moved first; then everything of the account went, in one step,
            // or (cut before) it went at this start.
            assert_eq!(alice, None, "at {}", cut.at);
            assert_eq!(read, [5], "at {}", cut.at);
            // She connects again: the same number, the next life, nothing of before.
            let again = keeper.account("alice", T0 + 16 * MINUTE).unwrap();
            assert_eq!((again.number, again.life, again.held), (ALICE, 1, false));
            assert_eq!(keeper.data().device_with_secret(ALICE, &hash(11)).unwrap(), None);
        } else {
            assert_eq!(alice, Some((ALICE, 0)), "at {}", cut.at);
            assert_eq!(read, [1, 2, 3, 5], "at {}", cut.at);
        }
    });
}

/// The world, with a copy of its data made by hand (the program stopped), then Alice's
/// history deleted, then that copy put in place of the data by hand.
fn held_world(name: &str) -> Root {
    let root = Root::new(name);
    world(&root);
    let copy = root.path.join("copy");
    copy_folder(&root.data(), &copy);
    let mut keeper = opened(&root, T0 + 5 * MINUTE);
    leave_to_delete(&mut keeper, A1, ALICE, 1, T0 + 5 * MINUTE).unwrap();
    let (mut device, _) = keeper.load(A1).unwrap();
    decide(&mut keeper, &mut device, A1, T0 + 6 * MINUTE, |turn| turn.delete(&hash(1))).unwrap().unwrap();
    keeper.close();
    fs::remove_dir_all(root.data()).unwrap();
    copy_folder(&copy, &root.data());
    fs::remove_dir_all(&copy).unwrap();
    root
}

#[test]
fn an_older_copy_found_at_a_start_cut_at_any_point_is_restored_once_under_one_number() {
    let setup = held_world("older");
    at_every_point("an older copy found at a start", &setup, |root, _| {
        let (mut keeper, _) = settled(root, T0 + 20 * MINUTE);
        assert_eq!(keeper.restore(), 1, "one new restore number, however often it was begun");
        assert!(keeper.held(ALICE) && !keeper.held(BOB));
        assert_eq!(readers(&mut keeper, T0 + 21 * MINUTE), Vec::<u8>::new());
    });
}

#[test]
fn keeping_a_held_history_cut_at_any_point_leaves_it_held_or_kept() {
    let setup = held_world("keep");
    opened(&setup, T0 + 10 * MINUTE).close();
    at_every_point("a held history kept", &setup, |root, cut| {
        let (keeper, _) = settled(root, T0 + 20 * MINUTE);
        let life = keeper.data().account_named("alice").unwrap().map(|(_, life)| life);
        if cut.whole() {
            assert_eq!((keeper.held(ALICE), life), (false, Some(1)));
        } else {
            // Kept in the data first, then let go in the store: held, or kept, nothing between.
            assert!(keeper.held(ALICE) || life == Some(1), "at {}", cut.at);
        }
    });
}

#[test]
fn a_put_back_cut_at_any_point_is_refused_until_run_again_and_then_done_once() {
    let setup = Root::new("putback");
    world(&setup);
    copies::nightly(&setup.data(), T0 + 5 * MINUTE).unwrap();
    {
        // After the copy: Alice's PC disconnects.
        let mut keeper = opened(&setup, T0 + 6 * MINUTE);
        let (mut device, _) = keeper.load(A1).unwrap();
        decide(&mut keeper, &mut device, A1, T0 + 6 * MINUTE, |turn| turn.end()).unwrap().unwrap();
        keeper.close();
    }
    at_every_point("put-back", &setup, |root, cut| {
        let now = T0 + 20 * MINUTE;
        match unlocked(|| Keeper::open(&root.state(), &root.data(), now)) {
            // Cut before it had begun: as it was. The owner runs it again.
            Ok((keeper, _)) if keeper.restore() == 0 => {
                keeper.close();
                unlocked(|| keeper::put_back(&root.state(), &root.data(), &the_copy(root), now)).unwrap();
            }
            Ok((keeper, _)) => keeper.close(),
            Err(why) => {
                assert!(why.0.contains("put-back was cut off"), "at {}: {why}", cut.at);
                unlocked(|| keeper::put_back(&root.state(), &root.data(), &the_copy(root), now)).unwrap();
            }
        }
        let (mut keeper, _) = settled(root, now);
        assert_eq!(keeper.restore(), 1, "at {}", cut.at);
        // The copy had Alice's PC connected: put back, it is ended all the same.
        assert_eq!(readers(&mut keeper, now), Vec::<u8>::new(), "at {}", cut.at);
        assert!(keeper.data().device_with_secret(ALICE, &hash(11)).unwrap().is_some());
    });
}

#[test]
fn taking_the_data_as_it_is_cut_at_any_point_ends_up_taken_once() {
    let setup = Root::new("take");
    world(&setup);
    fs::remove_file(apart::place(&setup.state())).unwrap();
    at_every_point("take-as-is", &setup, |root, cut| {
        let now = T0 + 20 * MINUTE;
        if let Err(why) = unlocked(|| Keeper::open(&root.state(), &root.data(), now)) {
            assert!(why.0.contains("take-as-is"), "at {}: {why}", cut.at);
            unlocked(|| keeper::take_as_is(&root.state(), &root.data(), now)).unwrap();
        }
        let (mut keeper, _) = settled(root, now);
        assert_eq!(keeper.restore(), 1, "at {}", cut.at);
        assert!(keeper.held(ALICE) && keeper.held(BOB), "at {}", cut.at);
        assert_eq!(readers(&mut keeper, now), Vec::<u8>::new(), "at {}", cut.at);
    });
}

#[test]
fn the_nightly_copy_cut_at_any_point_leaves_whole_copies_or_none() {
    let setup = Root::new("nightly");
    world(&setup);
    at_every_point("the nightly copy", &setup, |root, cut| {
        copies::prune(&root.data());
        let found = copies::list(&root.data());
        assert!(found.len() <= 1);
        if cut.whole() {
            assert_eq!(found.len(), 1);
        }
        for copy in found {
            data::open_to_read(&copy.path, "the copy").unwrap();
        }
        let names: Vec<String> = fs::read_dir(copies::folder(&root.data())).map(|entries| entries.flatten().map(|entry| entry.file_name().into_string().unwrap()).collect()).unwrap_or_default();
        assert!(names.iter().all(|name| name.starts_with("nightly-")), "{names:?}");
        settled(root, T0 + 20 * MINUTE).0.close();
    });
}

#[test]
fn a_change_of_form_cut_at_any_point_leaves_the_old_form_or_the_new_with_a_copy_made_first() {
    let setup = Root::new("form");
    world(&setup);
    at_every_point("a change of form", &setup, |root, cut| {
        let mut connection = rusqlite::Connection::open(data::place(&root.data())).unwrap();
        let form: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        let before: usize = fs::read_dir(copies::folder(&root.data())).map(|entries| entries.flatten().filter(|entry| entry.file_name().to_string_lossy().starts_with("before-form-")).count()).unwrap_or(0);
        let _ = &mut connection;
        drop(connection);
        assert!(form == 1 || form == 2, "{form}");
        if form == 2 {
            assert_eq!(before, 1, "at {}: changed with no copy made first", cut.at);
            // This program knows form 1 only: it refuses form 2, before reading anything else.
            let why = unlocked(|| Keeper::open(&root.state(), &root.data(), T0 + 20 * MINUTE)).err().unwrap();
            assert!(why.0.contains("made by a newer Nexus server"), "{why}");
        } else {
            settled(root, T0 + 20 * MINUTE).0.close();
        }
        if cut.whole() {
            assert_eq!(form, 2);
        }
    });
}

// ---- The data put back by hand, and from a copy ----

/// The data folder's files as they are now, by hand, while the program may be running.
fn snapshot(root: &Root, files: &[&str]) -> PathBuf {
    let into = root.path.join(format!("snapshot-{}", files.join("+")));
    fs::create_dir_all(&into).unwrap();
    for file in files {
        let from = root.data().join(file);
        if from.exists() {
            fs::copy(&from, into.join(file)).unwrap();
        }
    }
    into
}

fn put_by_hand(root: &Root, snapshot: &Path) {
    for file in [data::FILE, "nexus.db-wal", "nexus.db-shm"] {
        let _ = fs::remove_file(root.data().join(file));
    }
    for entry in fs::read_dir(snapshot).unwrap() {
        let entry = entry.unwrap();
        fs::copy(entry.path(), root.data().join(entry.file_name())).unwrap();
    }
}

#[test]
fn the_data_put_back_by_hand_with_and_without_its_wal_is_found_older_and_nothing_ended_comes_back() {
    for files in [&["nexus.db", "nexus.db-wal", "nexus.db-shm"][..], &["nexus.db", "nexus.db-wal"], &["nexus.db"]] {
        let root = Root::new(&format!("by-hand-{}", files.len()));
        world(&root);
        // Copied by hand while the backend is open: its last steps are in the -wal.
        let mut keeper = opened(&root, T0 + 5 * MINUTE);
        connect(&mut keeper, A2, ALICE, 7, T0 + 5 * MINUTE).unwrap();
        let copy = snapshot(&root, files);
        // Then sessions ended, and nothing else changed (X2).
        let (mut device, _) = keeper.load(A1).unwrap();
        decide(&mut keeper, &mut device, A1, T0 + 6 * MINUTE, |turn| turn.end()).unwrap().unwrap();
        keeper.close();
        put_by_hand(&root, &copy);
        let (mut keeper, started) = settled(&root, T0 + 7 * MINUTE);
        assert!(matches!(started, Started::Restored { restore: 1, .. }), "{files:?}: {started:?}");
        assert_eq!(readers(&mut keeper, T0 + 8 * MINUTE), Vec::<u8>::new(), "{files:?}");
        keeper.close();
    }
}

#[test]
fn a_copy_from_before_a_removed_device_does_not_bring_it_back() {
    let root = Root::new("removed");
    world(&root);
    let copy = snapshot(&root, &["nexus.db"]);
    let mut keeper = opened(&root, T0 + 5 * MINUTE);
    keeper.remove_device(A2).unwrap();
    keeper.close();
    put_by_hand(&root, &copy);
    let (mut keeper, started) = settled(&root, T0 + 6 * MINUTE);
    assert!(matches!(started, Started::Restored { .. }), "{started:?}");
    assert_eq!(keeper.data().device_with_secret(ALICE, &hash(12)).unwrap(), None);
    assert!(keeper.load(A2).is_err());
    // It connects again with its secret: a new device, never the removed one's id.
    let (id, made) = keeper.device(ALICE, &hash(12), "a made-up phone", "phone", "0.41.0", T0 + 7 * MINUTE).unwrap();
    assert!(made && id > B1);
}

#[test]
fn a_copy_from_before_a_deletion_holds_that_history_until_its_person_answers() {
    let root = held_world("deleted");
    let (mut keeper, started) = settled(&root, T0 + 7 * MINUTE);
    assert!(matches!(started, Started::Restored { held: 1, .. }), "{started:?}");
    assert!(keeper.held(ALICE) && !keeper.held(BOB));
    // The copy holds her history again, held; nothing of it is given. Her devices were
    // erased with it, and are not brought back: each connects again as a new device.
    assert!(keeper.data().account_named("alice").unwrap().is_some());
    assert!(keeper.load(A1).is_err() && keeper.load(A2).is_err());
    assert_eq!(keeper.data().device_with_secret(ALICE, &hash(11)).unwrap(), None);
    assert_eq!(readers(&mut keeper, T0 + 8 * MINUTE), Vec::<u8>::new());
    let (pc, made) = keeper.device(ALICE, &hash(11), "a made-up PC", "pc", "0.41.0", T0 + 8 * MINUTE).unwrap();
    assert!(made && pc > B1);
    // While held, a session it is given does not work: its scope has the store's life, the
    // held rows the copy's (S6 decides how the person is asked).
    let _ = connect(&mut keeper, pc, ALICE, 8, T0 + 8 * MINUTE);
    assert!(!reads(&mut keeper, pc, 8, T0 + 9 * MINUTE));
    // Bob's is not held: his device connects again and reads.
    connect(&mut keeper, B1, BOB, 9, T0 + 8 * MINUTE).unwrap();
    assert!(reads(&mut keeper, B1, 9, T0 + 9 * MINUTE));
    // The answer "keep": then it is served.
    keeper.keep_held(ALICE).unwrap();
    assert!(!keeper.held(ALICE));
    connect(&mut keeper, pc, ALICE, 10, T0 + 9 * MINUTE).unwrap();
    assert!(reads(&mut keeper, pc, 10, T0 + 9 * MINUTE));
}

#[test]
fn a_held_history_is_refused_by_c3_for_every_session_until_kept() {
    let root = held_world("held-refused");
    let mut keeper = opened(&root, T0 + 7 * MINUTE);
    keeper.keep_held(BOB).unwrap();
    let (pc, _) = keeper.device(BOB, &hash(22), "a made-up laptop", "pc", "0.41.0", T0 + 8 * MINUTE).unwrap();
    connect(&mut keeper, pc, BOB, 11, T0 + 8 * MINUTE).unwrap();
    // What later slices hand C3 for a held account: the hold refuses every read and send.
    let (mut device, scope) = keeper.load(pc).unwrap();
    let clock = keeper.clock(T0 + 8 * MINUTE);
    let answer = device.decide(&scope, clock, |turn| turn.arrive(&hash(11), Access::Read, true).map(|_| ())).apply(|_| Ok::<_, ()>(()));
    assert_eq!(answer, Ok(Err(Denied::HistoryHeld)));
    assert!(reads(&mut keeper, pc, 11, T0 + 8 * MINUTE));
}

#[test]
fn the_store_lost_while_the_data_is_kept_stops_until_the_data_is_taken_as_it_is() {
    let root = Root::new("store-lost");
    world(&root);
    let id = identity::read(&root.state()).unwrap().unwrap();
    fs::remove_file(apart::place(&root.state())).unwrap();
    let why = unlocked(|| Keeper::open(&root.state(), &root.data(), T0 + 5 * MINUTE)).err().unwrap();
    assert!(why.0.contains("is missing, and there is data") && why.0.contains("take-as-is"), "{why}");
    // The id lost with it: taken from the data.
    fs::remove_file(root.state().join(identity::FILE)).unwrap();
    let said = unlocked(|| keeper::take_as_is(&root.state(), &root.data(), T0 + 5 * MINUTE)).unwrap();
    assert!(said.iter().any(|line| line.contains("2 accounts' history is held")), "{said:?}");
    let (mut keeper, _) = settled(&root, T0 + 6 * MINUTE);
    assert_eq!(keeper.backend(), id);
    assert!(keeper.held(ALICE) && keeper.held(BOB));
    assert_eq!(readers(&mut keeper, T0 + 7 * MINUTE), Vec::<u8>::new());
    // Taken once: running it again is refused, the store being there and agreeing.
    keeper.close();
    let why = unlocked(|| keeper::take_as_is(&root.state(), &root.data(), T0 + 8 * MINUTE)).err().unwrap();
    assert!(why.0.contains("nothing to take"), "{why}");
}

#[test]
fn an_unreadable_store_is_left_as_it_is_and_nothing_is_served() {
    let root = Root::new("store-damaged");
    world(&root);
    let mut bytes = fs::read(apart::place(&root.state())).unwrap();
    let last = bytes.len() - 2;
    bytes[last] ^= 1;
    fs::write(apart::place(&root.state()), &bytes).unwrap();
    let why = unlocked(|| Keeper::open(&root.state(), &root.data(), T0 + 5 * MINUTE)).err().unwrap();
    assert!(why.0.contains("can't be read") && why.0.contains("left as it is"), "{why}");
    assert_eq!(fs::read(apart::place(&root.state())).unwrap(), bytes);
    let said = unlocked(|| keeper::take_as_is(&root.state(), &root.data(), T0 + 5 * MINUTE)).unwrap();
    assert!(said.iter().any(|line| line.contains("set-aside")), "{said:?}");
    settled(&root, T0 + 6 * MINUTE).0.close();
}

#[test]
fn the_data_lost_while_the_store_is_kept_stops_until_put_back_or_taken_as_none() {
    let root = Root::new("data-lost");
    world(&root);
    {
        let mut keeper = opened(&root, T0 + 5 * MINUTE);
        leave_to_delete(&mut keeper, A1, ALICE, 1, T0 + 5 * MINUTE).unwrap();
        let (mut device, _) = keeper.load(A1).unwrap();
        decide(&mut keeper, &mut device, A1, T0 + 6 * MINUTE, |turn| turn.delete(&hash(1))).unwrap().unwrap();
        keeper.close();
    }
    for file in fs::read_dir(root.data()).unwrap() {
        let file = file.unwrap();
        if file.file_type().unwrap().is_file() && file.file_name() != "lock" {
            fs::remove_file(file.path()).unwrap();
        }
    }
    let why = unlocked(|| Keeper::open(&root.state(), &root.data(), T0 + 7 * MINUTE)).err().unwrap();
    assert!(why.0.contains("There is no data") && why.0.contains("put-back"), "{why}");
    unlocked(|| keeper::take_as_is(&root.state(), &root.data(), T0 + 7 * MINUTE)).unwrap();
    let (mut keeper, _) = settled(&root, T0 + 8 * MINUTE);
    assert_eq!(keeper.restore(), 1);
    assert_eq!(readers(&mut keeper, T0 + 8 * MINUTE), Vec::<u8>::new());
    // Alice goes on from the life the store kept; her old devices' ids are not given again.
    let alice = keeper.account("alice", T0 + 9 * MINUTE).unwrap();
    assert!(alice.life >= 1, "{alice:?}");
    let (id, made) = keeper.device(alice.number, &hash(11), "a made-up PC", "pc", "0.41.0", T0 + 9 * MINUTE).unwrap();
    assert!(made && id > B1);
}

#[test]
fn both_folders_lost_is_a_new_backend_with_a_new_id() {
    let root = Root::new("both-lost");
    world(&root);
    let before = identity::read(&root.state()).unwrap().unwrap();
    fs::remove_dir_all(root.data()).unwrap();
    fs::remove_dir_all(root.state()).unwrap();
    fs::create_dir_all(root.data()).unwrap();
    fs::create_dir_all(root.state()).unwrap();
    let (keeper, started) = settled(&root, T0 + 5 * MINUTE);
    assert_eq!(started, Started::Made);
    // Design 6.3: a device that had connected before sees another id, stops, and asks again.
    assert_ne!(keeper.backend(), before);
}

#[test]
fn the_data_of_another_backend_is_refused_and_left() {
    let one = Root::new("one");
    let other = Root::new("other");
    world(&one);
    world(&other);
    copy_folder(&other.data(), &one.data());
    let why = unlocked(|| Keeper::open(&one.state(), &one.data(), T0 + 5 * MINUTE)).err().unwrap();
    assert!(why.0.contains("another backend's"), "{why}");
    // A copy of the other's data is refused by put-back too.
    let copy = copies::nightly(&other.data(), T0 + 5 * MINUTE).unwrap();
    let why = unlocked(|| keeper::put_back(&one.state(), &one.data(), &copy.path, T0 + 5 * MINUTE)).err().unwrap();
    assert!(why.0.contains("another backend's"), "{why}");
}

#[test]
fn a_put_back_by_the_command_ends_every_session_and_holds_what_was_deleted_since() {
    let root = Root::new("put-back-command");
    world(&root);
    let copy = copies::nightly(&root.data(), T0 + 5 * MINUTE).unwrap();
    {
        let mut keeper = opened(&root, T0 + 6 * MINUTE);
        leave_to_delete(&mut keeper, A1, ALICE, 1, T0 + 6 * MINUTE).unwrap();
        let (mut device, _) = keeper.load(A1).unwrap();
        decide(&mut keeper, &mut device, A1, T0 + 7 * MINUTE, |turn| turn.delete(&hash(1))).unwrap().unwrap();
        // While the backend runs, the command is refused.
        let why = keeper::put_back(&root.state(), &root.data(), &copy.path, T0 + 7 * MINUTE).err().unwrap();
        assert!(why.0.contains("is in use"), "{why}");
        let why = keeper::take_as_is(&root.state(), &root.data(), T0 + 7 * MINUTE).err().unwrap();
        assert!(why.0.contains("is in use"), "{why}");
        keeper.close();
    }
    let said = unlocked(|| keeper::put_back(&root.state(), &root.data(), &copy.path, T0 + 8 * MINUTE)).unwrap();
    assert!(said.iter().any(|line| line.contains("restore number 1")), "{said:?}");
    assert!(said.iter().any(|line| line.contains("1 accounts' history is held")), "{said:?}");
    let (mut keeper, started) = settled(&root, T0 + 9 * MINUTE);
    assert_eq!(started, Started::Ready);
    assert!(keeper.held(ALICE));
    assert_eq!(readers(&mut keeper, T0 + 9 * MINUTE), Vec::<u8>::new());
    // The data it replaced is kept.
    let replaced = fs::read_dir(copies::folder(&root.data())).unwrap().flatten().filter(|entry| entry.file_name().to_string_lossy().starts_with("replaced-")).count();
    assert!(replaced >= 1);
}

#[test]
fn the_nightly_copy_taken_while_writes_go_on_is_whole_when_put_back() {
    let root = Root::new("copy-under-writes");
    world(&root);
    let data_folder = root.data();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writing = {
        let stop = stop.clone();
        let root_state = root.state();
        let data = root.data();
        std::thread::spawn(move || {
            let mut keeper = unlocked(|| Keeper::open(&root_state, &data, T0 + 5 * MINUTE)).unwrap().0;
            let mut made = 0u64;
            while !stop.load(Ordering::SeqCst) && made < 400 {
                let name = format!("listener-{made}");
                let account = keeper.account(&name, T0 + 5 * MINUTE + made).unwrap().number;
                keeper.device(account, &Fingerprint::of(&[(made % 250) as u8 + 1; SECRET_BYTES]), "a made-up phone", "phone", "0.41.0", T0 + 5 * MINUTE + made).unwrap();
                made += 1;
            }
            keeper.close();
            made
        })
    };
    let mut made_copies = Vec::new();
    for at in 0..5 {
        std::thread::sleep(std::time::Duration::from_millis(30));
        made_copies.push(copies::nightly(&data_folder, T0 + 6 * MINUTE + at).unwrap());
    }
    stop.store(true, Ordering::SeqCst);
    let made = writing.join().unwrap();
    assert!(made > 0);
    for copy in made_copies {
        // Each is one moment of the data: every account made has its device, all or nothing.
        let connection = data::open_to_read(&copy.path, "the copy").unwrap();
        let accounts: i64 = connection.query_row("SELECT count(*) FROM accounts", [], |row| row.get(0)).unwrap();
        let devices: i64 = connection.query_row("SELECT count(*) FROM devices", [], |row| row.get(0)).unwrap();
        let lonely: i64 = connection.query_row("SELECT count(*) FROM accounts WHERE NOT EXISTS (SELECT 1 FROM devices WHERE devices.account = accounts.number)", [], |row| row.get(0)).unwrap();
        drop(connection);
        assert!(lonely <= 1, "more than the one account a copy can catch between its two steps: {lonely}");
        assert!(accounts >= 2 && devices >= 3);
        // Put back, it starts, and is found older (accounts were made after it).
        let restored = root.copied(&format!("copy-under-writes-{}", copy.number));
        unlocked(|| keeper::put_back(&restored.state(), &restored.data(), &copy.path, T0 + 10 * MINUTE)).unwrap();
        settled(&restored, T0 + 11 * MINUTE).0.close();
    }
}

#[test]
fn two_on_one_data_folder_are_refused_whichever_folder_they_share() {
    let root = Root::new("two");
    world(&root);
    let keeper = opened(&root, T0 + 5 * MINUTE);
    let why = Keeper::open(&root.state(), &root.data(), T0 + 5 * MINUTE).err().unwrap();
    assert!(why.0.contains("NEXUS_STATE is in use"), "{why}");
    let other_state = root.path.join("other-state");
    fs::create_dir_all(&other_state).unwrap();
    let why = Keeper::open(&other_state, &root.data(), T0 + 5 * MINUTE).err().unwrap();
    assert!(why.0.contains("NEXUS_DATA is in use"), "{why}");
    keeper.close();
    opened(&root, T0 + 6 * MINUTE).close();
}

#[test]
fn a_database_from_a_newer_version_is_refused_before_anything_else_is_read() {
    let root = Root::new("newer");
    world(&root);
    {
        let connection = rusqlite::Connection::open(data::place(&root.data())).unwrap();
        connection.execute_batch("PRAGMA user_version = 99; PRAGMA journal_mode = DELETE;").unwrap();
    }
    let before = fs::read(data::place(&root.data())).unwrap();
    let store_before = fs::read(apart::place(&root.state())).unwrap();
    let why = unlocked(|| Keeper::open(&root.state(), &root.data(), T0 + 5 * MINUTE)).err().unwrap();
    assert!(why.0.contains("form 99, made by a newer Nexus server") && why.0.contains("Nothing was read from it or changed"), "{why}");
    assert_eq!(fs::read(data::place(&root.data())).unwrap(), before);
    assert_eq!(fs::read(apart::place(&root.state())).unwrap(), store_before);
    // A copy in a newer form is refused by put-back too.
    let why = unlocked(|| keeper::put_back(&root.state(), &root.data(), &data::place(&root.data()), T0 + 5 * MINUTE)).err().unwrap();
    assert!(why.0.contains("newer Nexus server"), "{why}");
    // And a file that is not the Nexus server's data at all.
    fs::write(data::place(&root.data()), b"not a database, made up").unwrap();
    let why = unlocked(|| Keeper::open(&root.state(), &root.data(), T0 + 5 * MINUTE)).err().unwrap();
    assert!(why.0.contains("can't be read") || why.0.contains("not the Nexus server's data") || why.0.contains("damaged"), "{why}");
}

#[test]
fn a_clock_set_back_between_runs_reopens_nothing_and_stops_no_copy() {
    use nexus_core::nexus::kept::SEND_MS;
    let root = Root::new("clock");
    world(&root);
    let late = T0 + 3 + SEND_MS;
    {
        // Bob's 5 (proven at T0 + 3) may no longer send at T0 + 3 + 7 days.
        let mut keeper = opened(&root, late);
        let (mut device, _) = keeper.load(B1).unwrap();
        let answer = decide(&mut keeper, &mut device, B1, late, |turn| turn.arrive(&hash(5), Access::Send, false).map(|_| ())).unwrap();
        assert_eq!(answer, Err(Denied::ProveAgain));
        keeper.close();
    }
    // The clock set back a day, and the program started again.
    let back = late - 24 * 60 * MINUTE;
    let mut keeper = opened(&root, back);
    assert_eq!(keeper.clock(back).effective(), late);
    let (mut device, _) = keeper.load(B1).unwrap();
    let answer = decide(&mut keeper, &mut device, B1, back, |turn| turn.arrive(&hash(5), Access::Send, false).map(|_| ())).unwrap();
    assert_eq!(answer, Err(Denied::ProveAgain), "a time that had passed came back");
    keeper.close();
    // A copy made at the later time; then, with the clock back, one is still due, and the
    // newest kept are the newest made.
    copies::nightly(&root.data(), late).unwrap();
    assert!(copies::due(&root.data(), back));
    for night in 0..9 {
        copies::nightly(&root.data(), back + night).unwrap();
    }
    let kept = copies::list(&root.data());
    assert_eq!(kept.len(), copies::KEPT);
    assert_eq!(kept.last().unwrap().made_at, back + 8);
    assert!(kept.iter().all(|copy| copy.made_at != late));
}

// ---- Full disks ----

/// The scenarios that write, each on a fresh copy of the world.
const WRITING: &[&str] = &[
    "an account made",
    "a device made",
    "a device connecting",
    "proving again",
    "a waiting session taking the old one's place",
    "a device's sessions ended",
    "a device removed",
    "the nightly copy",
];

#[test]
fn a_full_store_folder_at_each_write_fails_that_step_and_leaves_both_agreeing() {
    let setup = Root::new("full-store");
    world(&setup);
    for scenario in WRITING {
        let dry = setup.copied("full-store-dry");
        let (points, _) = child(scenario, &dry, 0, 0, None);
        for at in 1..=points.len() {
            let root = setup.copied(&format!("full-store-{at}"));
            let (_, ended) = child(scenario, &root, 0, at, None);
            assert!(ended, "{scenario}: failing at {at} ({}) stopped the program", points[at - 1]);
            let outcome = fs::read_to_string(root.path.join("outcome")).unwrap();
            eprintln!("X2 full store folder, {scenario}, failing at {} ({}): {outcome}", at, points[at - 1]);
            let failed_step = points[at - 1].starts_with("store:") || points[at - 1].starts_with("data:") || points[at - 1].starts_with("copy:");
            if failed_step {
                assert!(outcome.starts_with("Err"), "{scenario}: failing at {} gave {outcome}", points[at - 1]);
            }
            let (mut keeper, _) = settled(&root, T0 + 20 * MINUTE);
            // Nothing given that the store does not have; what it took stays taken.
            let read = readers(&mut keeper, T0 + 21 * MINUTE);
            assert!(read.contains(&5), "{scenario} at {at}: {read:?}");
            assert!(!read.contains(&7) || points[..at].iter().filter(|p| *p == "store: renamed").count() >= 3, "{scenario} at {at}: {read:?}");
        }
    }
}

#[test]
fn a_full_data_disk_fails_writes_and_leaves_nothing_half_done() {
    let setup = Root::new("full-data");
    world(&setup);
    let size = fs::metadata(data::place(&setup.data())).unwrap().len();
    for blocks in [16u32, 64, 160, (size / 512) as u32 + 8] {
        for scenario in WRITING {
            let root = setup.copied(&format!("full-data-{blocks}"));
            let (_, ended) = child(scenario, &root, 0, 0, Some(blocks));
            let outcome = fs::read_to_string(root.path.join("outcome")).unwrap_or_else(|_| "(no outcome)".into());
            eprintln!("X2 full data disk, {blocks} blocks, {scenario}: {outcome}");
            assert!(ended || outcome == "(no outcome)", "{scenario} with {blocks} blocks: {outcome}");
            let (mut keeper, _) = settled(&root, T0 + 20 * MINUTE);
            let read = readers(&mut keeper, T0 + 21 * MINUTE);
            assert!(read.contains(&5) && read.contains(&3) || *scenario == "a device removed" || *scenario == "a device connecting", "{scenario} with {blocks} blocks: {read:?} ({outcome})");
            keeper.close();
        }
    }
}

// ---- The gap: a change the data alone has ----

/// Design 11.2 says the backend finds out "either way" that the data is older. It does for
/// every change S3 makes (each passes through the store). A change kept in the data alone (a
/// device last heard from, and at S4 each play taken) is not: a copy from before it, put back,
/// is the same age to the comparison. This test holds what happens now; the report says it.
#[test]
fn gap_a_change_the_data_alone_has_is_not_seen_when_a_copy_from_before_it_is_put_back() {
    let root = Root::new("gap");
    world(&root);
    let copy = snapshot(&root, &["nexus.db"]);
    {
        let connection = rusqlite::Connection::open(data::place(&root.data())).unwrap();
        connection.execute("UPDATE devices SET last_heard = ?1", [T0 as i64 + 1]).unwrap();
    }
    put_by_hand(&root, &copy);
    let (_, started) = settled(&root, T0 + 5 * MINUTE);
    assert_eq!(started, Started::Ready, "the gap is closed: change this test and the report");
}

#[test]
fn opening_and_reading_the_data_writes_nothing() {
    let root = Root::new("read-only");
    world(&root);
    let mut keeper = opened(&root, T0 + 5 * MINUTE);
    let _ = readers(&mut keeper, T0 + 5 * MINUTE);
    keeper.close();
    let store = fs::read(apart::place(&root.state())).unwrap();
    let data_bytes = fs::read(data::place(&root.data())).unwrap();
    let keeper = opened(&root, T0 + 5 * MINUTE);
    keeper.close();
    assert_eq!(fs::read(apart::place(&root.state())).unwrap(), store);
    assert_eq!(fs::read(data::place(&root.data())).unwrap(), data_bytes);
    let _ = Opened::Absent;
}
