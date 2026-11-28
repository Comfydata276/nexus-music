//! Experiment X2 (design 21), slice S3's tests: the data and the store kept apart, put back,
//! lost, cut off and filled up. Real files in temporary folders, real SQLite, and real
//! processes: each kill is a child process stopped (`abort`: nothing is flushed and nothing
//! unwinds) at one of the points between two durable steps, found by counting them in a run
//! that is not stopped. Sign-ins are C3r's own: a receipt from a checked fingerprint (here a
//! made-up one standing for Argon2id's output). Made-up names only.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use nexus_core::nexus::account::{AccountId, CODE_LIFETIME_MS, DeviceId, DeviceSecret, GrantSecret, RequestId, SECRET_BYTES, SESSION_IDLE_MS as SESSION_MS};
use nexus_core::nexus::kept::{Access, Account, AdminAction, AdminTarget, Bearer, Denied, FreshSession, Fingerprint, GrantIntent, GrantUse, LeaveFor, SessionTarget, Status, Turn};
use nexus_music_server::identity;
use nexus_music_server::store::data::{self, Change, Data, Migration};
use nexus_music_server::store::keeper::{self, Failed, Keeper, Started};
use nexus_music_server::store::{apart, copies, crash};

/// A made-up time, in milliseconds.
const T0: u64 = 1_800_000_000_000;
const MINUTE: u64 = 60_000;

/// The admin, and two people.
const ROOT: u64 = 1;
const ALICE: u64 = 2;
const BOB: u64 = 3;

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
        if let Ok(ids) = fs::read(self.path.join("ids")) {
            fs::write(other.path.join("ids"), ids).unwrap();
        }
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

fn fp(n: u8) -> Fingerprint {
    Fingerprint::of(&[n; SECRET_BYTES])
}

fn email(account: u64) -> &'static str {
    match account {
        ROOT => "root@example.org",
        ALICE => "alice@example.org",
        _ => "bob@example.org",
    }
}

/// A password as the data keeps it: its "Argon2id output" is `fp(n)`'s bytes.
fn password(n: u8) -> data::Password {
    data::Password { hash: *fp(n).as_bytes(), salt: [n; 16], memory: 19 * 1024, passes: 2, lanes: 1 }
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

/// One decision on an account, kept (whatever it answered) before its answer.
fn decide<T>(keeper: &mut Keeper, account: u64, now: u64, extra: &[Change<'_>], action: impl FnOnce(Turn<'_>) -> Result<T, Denied>) -> Result<Result<T, Denied>, Failed> {
    let mut loaded = keeper.load(account)?;
    let (restore, clock) = (keeper.restore(), keeper.clock(now));
    loaded.request(restore, clock, action).apply(|effects| keeper.persist(effects, extra, now))
}

/// The one grant secret these tests hand out (8.8): only its fingerprint is kept.
static GRANT: std::sync::LazyLock<GrantSecret> = std::sync::LazyLock::new(|| GrantSecret::from_bytes([77; SECRET_BYTES]));

fn dev(device: u64) -> DeviceId {
    DeviceId::new(device).unwrap_or(DeviceId::new(u64::MAX).unwrap())
}

/// The one request number these tests' grants are asked and used under (8.8, 12.2).
fn request() -> RequestId {
    RequestId::new(9).unwrap()
}

fn bearer(device: u64, session: &Fingerprint) -> Bearer<'_> {
    Bearer { device: dev(device), hash: session, grant: Some(GrantUse { request_id: request(), secret: &GRANT }) }
}

fn secret(n: u8) -> DeviceSecret {
    DeviceSecret::from_bytes([n; SECRET_BYTES])
}

fn fresh(keeper: &mut Keeper, session: u8) -> FreshSession {
    FreshSession { id: keeper.fresh_session().unwrap(), hash: fp(session), sign_out: fp(session.wrapping_add(100)) }
}

/// The session `device` of `account` has now, as the data and the store have it.
fn session_of(keeper: &mut Keeper, account: u64, device: u64) -> SessionTarget {
    let rows = keeper.load(account).unwrap().rows();
    let session = rows.devices.get(&dev(device)).and_then(|row| row.session.as_ref()).map(|session| session.id);
    SessionTarget { device: dev(device), session: session.unwrap_or(nexus_core::nexus::account::SessionId::new(u64::MAX).unwrap()) }
}

/// The account as the admin names it: its number and its revision now.
fn target(keeper: &mut Keeper, account: u64) -> AdminTarget {
    keeper.load(account).map(|loaded| loaded.state().admin_target()).unwrap_or(AdminTarget::Account { id: AccountId::new(account).unwrap(), revision: 0 })
}

fn failed(why: Denied) -> Failed {
    Failed(format!("{why:?}"))
}

/// A device signs in with a password: C3r's check, its receipt, and the sign-in. Its id.
fn sign_in(keeper: &mut Keeper, account: u64, secret: u8, pass: u8, session: u8, now: u64) -> Result<u64, Failed> {
    let vacant = keeper.vacant_device();
    // The verifier the data keeps, as S8 reads it: none, and nothing can sign in.
    let kept = keeper.data().password(account).map_err(|_| Failed("unreadable".into()))?.ok_or_else(|| Failed("no password".into()))?;
    let expected = Fingerprint::from_bytes(kept.hash);
    let check = decide(keeper, account, now, &[], |turn| turn.check_sign_in(&self::secret(secret), dev(vacant), expected))?.map_err(failed)?;
    let receipt = check.check(fp(pass), keeper.clock(now)).map_err(failed)?;
    let fresh = fresh(keeper, session);
    let signed = decide(keeper, account, now, &[], |turn| turn.sign_in(receipt, fresh, None))?.map_err(failed)?;
    Ok(signed.device.get())
}

/// The fingerprint of a one-time code as the data keeps it, found as S8 finds it: by the
/// code typed, among the account's codes. None if the data has no such code (finding 11).
fn stored_code(keeper: &Keeper, account: u64, code: u8) -> Option<Fingerprint> {
    let typed = [code; SECRET_BYTES];
    let found: Option<Vec<u8>> = keeper
        .data()
        .connection()
        .query_row("SELECT code FROM codes WHERE account = ?1 AND code = ?2", rusqlite::params![account as i64, typed.as_slice()], |row| row.get(0))
        .ok();
    found.map(|bytes| Fingerprint::from_bytes(bytes.try_into().unwrap()))
}

/// A new account's person redeems the code they were given with the password they choose,
/// in one step (8.5), and is signed in by it.
fn first_password(keeper: &mut Keeper, account: u64, secret: u8, code: u8, pass: u8, session: u8, now: u64) -> Result<u64, Failed> {
    let vacant = keeper.vacant_device();
    let code_fp = stored_code(keeper, account, code).ok_or_else(|| Failed("no such code in the data".into()))?;
    let live = keeper.load(account)?.state().code.map_or(0, |code| code.number);
    let check = decide(keeper, account, now, &[], |turn| turn.check_code(&self::secret(secret), dev(vacant), live, code_fp.clone()))?.map_err(failed)?;
    let receipt = check.check(code_fp, keeper.clock(now)).map_err(failed)?;
    let chosen = password(pass);
    let fresh = fresh(keeper, session);
    let signed = decide(keeper, account, now, &[Change::Password { account, password: Some(&chosen) }], |turn| turn.redeem_code(receipt, fp(pass), fresh, None))?.map_err(failed)?;
    Ok(signed.device.get())
}

/// Leave for one thing (8.8), on a session, by its password typed again.
fn leave(keeper: &mut Keeper, account: u64, device: u64, session: u8, pass: u8, purpose: LeaveFor, now: u64) -> Result<(), Failed> {
    let held = fp(session);
    let check = decide(keeper, account, now, &[], |turn| turn.check_leave(bearer(device, &held), GrantIntent { request_id: request(), change: purpose }, fp(pass)))?.map_err(failed)?;
    let receipt = check.check(fp(pass), keeper.clock(now)).map_err(failed)?;
    decide(keeper, account, now, &[], |turn| turn.grant_leave(receipt, &GRANT))?.map_err(failed)
}

fn admin_leave(keeper: &mut Keeper, action: AdminAction, account: u64) -> LeaveFor {
    LeaveFor::Admin { action, target: target(keeper, account) }
}

/// The admin does `action` to `target`, under leave already granted for it.
fn administer(keeper: &mut Keeper, ids: &Ids, action: AdminAction, target_number: u64, extra: &[Change<'_>], now: u64) -> Result<(), Failed> {
    let named = target(keeper, target_number);
    let mut other = keeper.load(target_number)?;
    let held = fp(30);
    let (restore, clock) = (keeper.restore(), keeper.clock(now));
    let mut root = keeper.load(ROOT)?;
    root.request(restore, clock, |turn| turn.administer(bearer(ids.root, &held), action, named, Some(&mut other)))
        .apply(|effects| keeper.persist(effects, extra, now))?
        .map_err(failed)
}

/// The devices the world made, written beside it for the child processes.
#[derive(Debug, Clone, Copy)]
struct Ids {
    root: u64,
    a1: u64,
    a2: u64,
    b1: u64,
}

impl Ids {
    fn of(root: &Root) -> Ids {
        let text = fs::read_to_string(root.path.join("ids")).unwrap_or_default();
        let n: Vec<u64> = text.split(' ').filter_map(|word| word.trim().parse().ok()).collect();
        match n.as_slice() {
            [root, a1, a2, b1] => Ids { root: *root, a1: *a1, a2: *a2, b1: *b1 },
            _ => Ids { root: 0, a1: 0, a2: 0, b1: 0 },
        }
    }

    /// Every (account, device, session) of the world, and the ones scenarios add.
    fn sessions(&self) -> [(u64, u64, u8); 6] {
        [(ROOT, self.root, 30), (ALICE, self.a1, 1), (ALICE, self.a2, 3), (ALICE, self.a2, 7), (BOB, self.b1, 5), (BOB, self.b1, 9)]
    }
}

/// The admin (its device, session 30); Alice's PC (1) and phone (3); Bob's PC (5). Each
/// account made by the admin's command with a code, its password chosen with it.
fn world(root: &Root) -> Ids {
    let mut keeper = opened(root, T0);
    keeper.mark_set_up(T0).unwrap();
    let until = T0 + 7 * 24 * 60 * MINUTE;
    for (number, admin, code) in [(ROOT, true, 41u8), (ALICE, false, 42), (BOB, false, 43)] {
        assert_eq!(keeper.make_account(email(number), "A made-up name", admin, Some((&[code; 32], until)), T0).unwrap(), number);
    }
    let r = first_password(&mut keeper, ROOT, 31, 41, 32, 30, T0).unwrap();
    let a1 = first_password(&mut keeper, ALICE, 11, 42, 12, 1, T0).unwrap();
    let a2 = sign_in(&mut keeper, ALICE, 13, 12, 3, T0).unwrap();
    let b1 = first_password(&mut keeper, BOB, 21, 43, 22, 5, T0).unwrap();
    fs::write(root.path.join("ids"), format!("{r} {a1} {a2} {b1}")).unwrap();
    let ids = Ids { root: r, a1, a2, b1 };
    assert_eq!(readers(&mut keeper, &ids, T0 + 1), [30, 1, 3, 5]);
    keeper.close();
    ids
}

/// Whether `session` may read on `device` of `account`, as C3r decides from both stores.
fn reads(keeper: &mut Keeper, account: u64, device: u64, session: u8, now: u64) -> bool {
    let Ok(mut loaded) = keeper.load(account) else { return false };
    let held = fp(session);
    let (restore, clock) = (keeper.restore(), keeper.clock(now));
    matches!(loaded.request(restore, clock, |turn| turn.arrive(bearer(device, &held), Access::Read).map(|_| ())).apply(|_| Ok::<_, ()>(())), Ok(Ok(())))
}

/// When the data says a device was last heard from.
fn last_heard(keeper: &Keeper, device: u64) -> Option<u64> {
    keeper.data().connection().query_row("SELECT last_heard FROM devices WHERE id = ?1", [device as i64], |row| row.get::<_, Option<i64>>(0)).unwrap().map(|at| at as u64)
}

fn readers(keeper: &mut Keeper, ids: &Ids, now: u64) -> Vec<u8> {
    ids.sessions().into_iter().filter(|&(account, device, session)| reads(keeper, account, device, session, now)).map(|(_, _, session)| session).collect()
}

fn state(keeper: &mut Keeper, account: u64) -> Option<nexus_core::nexus::kept::AccountState> {
    keeper.load(account).ok().map(|loaded: Account| loaded.state().clone())
}

/// Opens, and then opens again: the second start must find the two the same age.
fn settled(root: &Root, now: u64) -> (Keeper, Started) {
    let (keeper, started) = open(root, now);
    keeper.close();
    if std::env::var_os("X2_DEBUG").is_some() {
        let keeper = opened(root, now);
        let names = keeper.data().names().unwrap();
        let view = keeper.data().view().unwrap();
        let store = match apart::read(&root.state()) { apart::Reading::Found(store) => store, _ => panic!() };
        let sv = store.view(&names);
        eprintln!("mark {} changes {} restore {} {}", sv.mark, view.changes, sv.restore, view.restore);
        for (n, a) in &sv.accounts {
            match view.accounts.get(n) {
                None => eprintln!("{n}: store only, {:?}", a.account.status),
                Some(r) => {
                    // By number, and which of its parts differ: never the states themselves,
                    // which hold the account's email (finding 12).
                    if r.account != a.account {
                        let (s, d) = (&a.account, &r.account);
                        let differ: Vec<&str> = [
                            ("life", s.life != d.life),
                            ("status", s.status != d.status),
                            ("passwords", s.password_changes != d.password_changes),
                            ("reset", s.password_reset_required != d.password_reset_required),
                            ("code", s.code != d.code),
                            ("admin", s.admin != d.admin),
                            ("music", s.music_login != d.music_login || s.music_recovery != d.music_recovery),
                            ("revision", s.revision != d.revision),
                        ]
                        .into_iter()
                        .filter_map(|(name, differs)| differs.then_some(name))
                        .collect();
                        eprintln!("{n}: state differs in {differ:?}");
                    }
                    eprintln!("{n}: devices store {:?} data {:?}", a.devices.keys().collect::<Vec<_>>(), r.devices.keys().collect::<Vec<_>>());
                }
            }
        }
        keeper.close();
    }
    let (keeper, again) = open(root, now);
    assert_eq!(again, Started::Ready, "the stores were not brought together by the first start ({started:?})");
    (keeper, started)
}

// ---- The child: one scenario, stopped or failed at the n-th point ----

static POINTS: AtomicUsize = AtomicUsize::new(0);
/// Points are counted only while this is set: a scenario may make its world first, uncut.
static ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Runs in a child process only (`X2_CHILD` names the scenario).
#[test]
fn x2_child() {
    let Ok(scenario) = std::env::var("X2_CHILD") else { return };
    let root = Root::at(PathBuf::from(std::env::var("X2_ROOT").unwrap()));
    let number = |name: &str| std::env::var(name).ok().and_then(|value| value.parse::<usize>().ok()).unwrap_or(0);
    let (kill_at, fail_at) = (number("X2_KILL_AT"), number("X2_FAIL_AT"));
    let log = PathBuf::from(std::env::var("X2_POINTS").unwrap());
    crash::set(Box::new(move |name| {
        if !ARMED.load(Ordering::SeqCst) {
            return false;
        }
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
    let now = T0 + 5 * MINUTE;
    let ids = Ids::of(root);
    let keeper = || opened(root, now);
    match scenario {
        "first start" => Keeper::open(&root.state(), &root.data(), now).map(|(keeper, _)| keeper.close()).map_err(|why| why.0),
        "an account made" => keeper().make_account("carol@example.org", "Carol", false, Some((&[44; 32], now + MINUTE)), now).map(|_| ()).map_err(failed),
        "signing in" => sign_in(&mut keeper(), ALICE, 13, 12, 7, now).map(|_| ()).map_err(failed),
        "signing out" => {
            let held = fp(1);
            decide(&mut keeper(), ALICE, now, &[], |turn| turn.sign_out(bearer(ids.a1, &held))).map_err(failed)?.map_err(|why| format!("{why:?}"))
        }
        "forcing out" => {
            let held = fp(1);
            let mut keeper = keeper();
            let other = session_of(&mut keeper, ALICE, ids.a2);
            decide(&mut keeper, ALICE, now, &[], |turn| turn.force_out(bearer(ids.a1, &held), other)).map_err(failed)?.map_err(|why| format!("{why:?}"))
        }
        "a password changed" => {
            let (held, chosen) = (fp(1), password(14));
            decide(&mut keeper(), ALICE, now, &[Change::Password { account: ALICE, password: Some(&chosen) }], |turn| turn.change_password(bearer(ids.a1, &held)))
                .map_err(failed)?
                .map_err(|why| format!("{why:?}"))
        }
        "listening data deleted" => {
            let held = fp(1);
            decide(&mut keeper(), ALICE, now, &[], |turn| turn.delete_history(bearer(ids.a1, &held))).map_err(failed)?.map_err(|why| format!("{why:?}"))
        }
        "an account switched off" => administer(&mut keeper(), &ids, AdminAction::SetOn(false), BOB, &[], now).map_err(failed),
        "an account deleted" => administer(&mut keeper(), &ids, AdminAction::DeleteAccount, BOB, &[], now).map_err(failed),
        "a password reset" => administer(&mut keeper(), &ids, AdminAction::ResetPassword, BOB, &[Change::Code { account: BOB, number: 2, code: &[45; 32], made_at: now, until: now + CODE_LIFETIME_MS }], now).map_err(failed),
        "a device heard from" => keeper().heard(ids.b1, now + 2 * MINUTE).map_err(failed),
        "an event kept" => an_event(&mut keeper(), BOB, ids.b1, 3, now).map(|_| ()).map_err(failed),
        "a large account made" => keeper().make_account("large@example.org", &"L".repeat(40 * 1024), false, None, now).map(|_| ()).map_err(failed),
        "an older copy found at a start" => Keeper::open(&root.state(), &root.data(), now).map(|(keeper, _)| keeper.close()).map_err(|why| why.0),
        "put-back" => keeper::put_back(&root.state(), &root.data(), &the_copy(root), now).map(|_| ()).map_err(|why| why.0),
        "put-back with a reader" => {
            // Erin made while another reader holds an older moment of the data: her step
            // stays in the file SQLite writes ahead in, and the program stops with it there.
            ARMED.store(false, Ordering::SeqCst);
            let reader = rusqlite::Connection::open(data::place(&root.data())).unwrap();
            reader.execute_batch("BEGIN").unwrap();
            let _: i64 = reader.query_row("SELECT count(*) FROM accounts", [], |row| row.get(0)).unwrap();
            let mut writer = keeper();
            writer.make_account("erin@example.org", "Erin", false, None, now).unwrap();
            drop(writer);
            ARMED.store(true, Ordering::SeqCst);
            let done = keeper::put_back(&root.state(), &root.data(), &the_copy(root), now).map(|_| ()).map_err(|why| why.0);
            drop(reader);
            done
        }
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
    // Named for the setup too: two tests may cut the same scenario at once.
    let name = format!("{}-{}", setup.path.file_name().unwrap().to_string_lossy(), scenario.replace(' ', "-"));
    let dry = setup.copied(&format!("{name}-dry"));
    let (points, ended) = child(scenario, &dry, 0, 0, None);
    assert!(ended, "{scenario}: the run that is not cut did not end by itself");
    assert!(!points.is_empty(), "{scenario}: passed no point");
    for at in 1..=points.len() {
        let root = setup.copied(&format!("{name}-{at}"));
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
fn an_account_made_cut_at_any_point_is_made_or_not_and_its_number_is_never_given_twice() {
    let setup = Root::new("make");
    let ids = world(&setup);
    at_every_point("an account made", &setup, |root, cut| {
        let (mut keeper, _) = settled(root, T0 + 20 * MINUTE);
        let carol = keeper.account_named("carol@example.org").unwrap();
        if cut.whole() {
            assert_eq!(carol, Some(4));
        }
        // Made once at most: asked again, an email the data has is refused.
        let again = match carol {
            Some(number) => {
                assert!(keeper.make_account("carol@example.org", "Carol", false, None, T0 + 21 * MINUTE).is_err());
                number
            }
            None => keeper.make_account("carol@example.org", "Carol", false, None, T0 + 21 * MINUTE).unwrap(),
        };
        assert_eq!(again, carol.unwrap_or(if cut.took() { 5 } else { 4 }), "at {}", cut.at);
        assert_eq!(readers(&mut keeper, &ids, T0 + 21 * MINUTE), [30, 1, 3, 5]);
    });
}

#[test]
fn a_sign_in_cut_at_any_point_works_only_once_the_store_has_it_and_ends_the_one_before() {
    let setup = Root::new("sign-in");
    let ids = world(&setup);
    at_every_point("signing in", &setup, |root, cut| {
        let (mut keeper, _) = settled(root, T0 + 20 * MINUTE);
        let read = readers(&mut keeper, &ids, T0 + 21 * MINUTE);
        assert_eq!(read.contains(&7), cut.whole() || cut.gave(), "{read:?} at {}", cut.at);
        assert!(!(read.contains(&3) && read.contains(&7)), "{read:?}");
        assert!(read.contains(&1) && read.contains(&5), "{read:?}");
    });
}

#[test]
fn every_ending_cut_at_any_point_holds_once_the_store_has_it() {
    let setup = Root::new("endings");
    let ids = world(&setup);
    for (scenario, ended) in [("signing out", 1u8), ("forcing out", 3)] {
        at_every_point(scenario, &setup, |root, cut| {
            let (mut keeper, _) = settled(root, T0 + 20 * MINUTE);
            let read = readers(&mut keeper, &ids, T0 + 21 * MINUTE);
            assert_eq!(read.contains(&ended), !(cut.took() || cut.whole()), "{scenario}: {read:?} at {}", cut.at);
            assert!(read.contains(&5) && read.contains(&30), "{scenario}: {read:?}");
        });
    }
}

#[test]
fn a_change_under_leave_cut_at_any_point_holds_once_the_store_has_it() {
    let setup = Root::new("leave");
    let ids = world(&setup);
    let with_leave = |name: &str, purpose: LeaveFor| {
        let copy = setup.copied(name);
        let mut keeper = opened(&copy, T0 + 4 * MINUTE);
        leave(&mut keeper, ALICE, ids.a1, 1, 12, purpose, T0 + 4 * MINUTE).unwrap();
        keeper.close();
        copy
    };
    let changing = with_leave("leave-password", LeaveFor::ChangePassword);
    at_every_point("a password changed", &changing, |root, cut| {
        // Within the grant's life (granted at T0 + 4 minutes, for LEAVE_MS).
        let (mut keeper, _) = settled(root, T0 + 6 * MINUTE);
        let read = readers(&mut keeper, &ids, T0 + 6 * MINUTE);
        let alice = state(&mut keeper, ALICE).unwrap();
        if cut.took() || cut.whole() {
            // Her other device is signed out, and the old password never works again.
            assert!(!read.contains(&3), "{read:?} at {}", cut.at);
            assert_eq!(alice.password_changes, 2, "at {}", cut.at);
            assert!(sign_in(&mut keeper, ALICE, 13, 12, 8, T0 + 6 * MINUTE).is_err(), "the old password signed in at {}", cut.at);
            // The grant was spent: tried again while it would still be live, it is refused.
            let (held, chosen) = (fp(1), password(15));
            let again = decide(&mut keeper, ALICE, T0 + 6 * MINUTE, &[Change::Password { account: ALICE, password: Some(&chosen) }], |turn| turn.change_password(bearer(ids.a1, &held)));
            assert!(!matches!(again, Ok(Ok(()))), "at {}: a spent grant changed the password again", cut.at);
        } else {
            assert_eq!((alice.password_changes, read.contains(&3)), (1, true), "at {}", cut.at);
        }
        if cut.whole() {
            assert!(read.contains(&1));
            sign_in(&mut keeper, ALICE, 13, 14, 8, T0 + 6 * MINUTE).unwrap();
        }
    });
    let deleting = with_leave("leave-history", LeaveFor::DeleteHistory);
    at_every_point("listening data deleted", &deleting, |root, cut| {
        let (mut keeper, _) = settled(root, T0 + 20 * MINUTE);
        let life = state(&mut keeper, ALICE).unwrap().life;
        assert_eq!(life, u64::from(cut.took() || cut.whole()), "at {}", cut.at);
    });
}

#[test]
fn what_the_admin_takes_away_cut_at_any_point_holds_once_the_store_has_it() {
    let setup = Root::new("admin");
    let ids = world(&setup);
    for (scenario, action) in [("an account switched off", AdminAction::SetOn(false)), ("an account deleted", AdminAction::DeleteAccount), ("a password reset", AdminAction::ResetPassword)] {
        let prepared = setup.copied(&format!("admin-{}", scenario.replace(' ', "-")));
        {
            let mut keeper = opened(&prepared, T0 + 4 * MINUTE);
            let purpose = admin_leave(&mut keeper, action.clone(), BOB);
            leave(&mut keeper, ROOT, ids.root, 30, 32, purpose, T0 + 4 * MINUTE).unwrap();
            keeper.close();
        }
        at_every_point(scenario, &prepared, |root, cut| {
            let (mut keeper, _) = settled(root, T0 + 6 * MINUTE);
            let read = readers(&mut keeper, &ids, T0 + 6 * MINUTE);
            let bob = state(&mut keeper, BOB);
            let done = cut.took() || cut.whole();
            assert_eq!(read.contains(&5), !done, "{scenario}: {read:?} at {}", cut.at);
            assert!(read.contains(&1) && read.contains(&3), "{scenario}: {read:?}");
            if done {
                // The grant was spent: the same action again, while it would still be live,
                // is refused (finding 11).
                let again = administer(&mut keeper, &ids, action.clone(), BOB, &[], T0 + 6 * MINUTE);
                assert!(again.is_err(), "{scenario} at {}: a spent grant was used again", cut.at);
                match scenario {
                    "an account switched off" => assert_eq!(bob.map(|bob| bob.status), Some(Status::Off)),
                    "an account deleted" => assert_eq!(keeper.account_named(email(BOB)).unwrap(), None, "at {}", cut.at),
                    _ => {
                        assert!(bob.unwrap().password_reset_required);
                        assert!(sign_in(&mut keeper, BOB, 21, 22, 9, T0 + 6 * MINUTE).is_err());
                    }
                }
            }
        });
    }
}

#[test]
fn a_change_the_data_alone_has_cut_at_any_point_leaves_both_agreeing() {
    let setup = Root::new("heard");
    let ids = world(&setup);
    at_every_point("a device heard from", &setup, |root, cut| {
        let (mut keeper, _) = settled(root, T0 + 20 * MINUTE);
        assert_eq!(readers(&mut keeper, &ids, T0 + 21 * MINUTE), [30, 1, 3, 5]);
        // The step itself, in the data: all of it or none (finding 11).
        let heard = last_heard(&keeper, ids.b1);
        assert!(heard.is_none() || heard == Some(T0 + 7 * MINUTE), "at {}: {heard:?}", cut.at);
        if cut.whole() || cut.points[..cut.at].iter().any(|point| point == "data: committed") {
            assert_eq!(heard, Some(T0 + 7 * MINUTE), "at {}", cut.at);
        }
    });
}

#[test]
fn an_older_copy_found_at_a_start_cut_at_any_point_is_restored_once_under_one_number() {
    let setup = Root::new("older");
    let ids = world(&setup);
    let copy = snapshot(&setup, &["nexus.db"]);
    {
        let mut keeper = opened(&setup, T0 + 2 * MINUTE);
        let held = fp(1);
        decide(&mut keeper, ALICE, T0 + 2 * MINUTE, &[], |turn| turn.sign_out(bearer(ids.a1, &held))).unwrap().unwrap();
        keeper.close();
    }
    put_by_hand(&setup, &copy);
    at_every_point("an older copy found at a start", &setup, |root, cut| {
        let (mut keeper, _) = settled(root, T0 + 20 * MINUTE);
        assert_eq!(keeper.restore(), 1, "at {}: one new restore number, however often it was begun", cut.at);
        assert_eq!(readers(&mut keeper, &ids, T0 + 21 * MINUTE), Vec::<u8>::new(), "at {}", cut.at);
    });
}

#[test]
fn a_put_back_cut_at_any_point_is_finished_by_the_next_start_or_begun_again_and_done_once() {
    let setup = Root::new("putback");
    let ids = world(&setup);
    copies::nightly(&setup.data(), T0 + 2 * MINUTE).unwrap();
    {
        let mut keeper = opened(&setup, T0 + 3 * MINUTE);
        let held = fp(1);
        decide(&mut keeper, ALICE, T0 + 3 * MINUTE, &[], |turn| turn.sign_out(bearer(ids.a1, &held))).unwrap().unwrap();
        keeper.close();
    }
    at_every_point("put-back", &setup, |root, cut| {
        let now = T0 + 20 * MINUTE;
        // 12.4: the next start finishes a put-back whose plan reached the store; one cut
        // before that changed nothing, and the owner runs it again.
        match unlocked(|| Keeper::open(&root.state(), &root.data(), now)) {
            Ok((keeper, _)) if keeper.restore() == 0 => {
                keeper.close();
                unlocked(|| keeper::put_back(&root.state(), &root.data(), &the_copy(root), now)).unwrap();
            }
            Ok((keeper, _)) => keeper.close(),
            Err(why) => panic!("at {}: {why}", cut.at),
        }
        let (mut keeper, _) = settled(root, now);
        assert_eq!(keeper.restore(), 1, "at {}", cut.at);
        assert_eq!(readers(&mut keeper, &ids, now), Vec::<u8>::new(), "at {}", cut.at);
    });
}

#[test]
fn taking_the_data_as_it_is_cut_at_any_point_ends_up_taken_once() {
    let setup = Root::new("take");
    let ids = world(&setup);
    fs::remove_file(apart::place(&setup.state())).unwrap();
    at_every_point("take-as-is", &setup, |root, cut| {
        let now = T0 + 20 * MINUTE;
        if let Err(why) = unlocked(|| Keeper::open(&root.state(), &root.data(), now)) {
            assert!(why.0.contains("take-as-is"), "at {}: {why}", cut.at);
            unlocked(|| keeper::take_as_is(&root.state(), &root.data(), now)).unwrap();
        }
        let (mut keeper, _) = settled(root, now);
        assert_eq!(keeper.restore(), 1, "at {}", cut.at);
        assert_eq!(readers(&mut keeper, &ids, now), Vec::<u8>::new(), "at {}", cut.at);
        // With the store lost nothing says which passwords changed since: every account
        // waits for a new one-time code (12.4), and no verifier of the data's is left.
        assert!(sign_in(&mut keeper, ALICE, 11, 12, 8, now).is_err(), "at {}: the old password signed in", cut.at);
        assert_eq!(count(&keeper, "SELECT count(*) FROM accounts WHERE number >= ?1 AND password IS NOT NULL", 1), 0, "at {}", cut.at);
        assert!(state(&mut keeper, ALICE).unwrap().password_reset_required, "at {}", cut.at);
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
        let connection = rusqlite::Connection::open(data::place(&root.data())).unwrap();
        let form: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        drop(connection);
        let before: usize = fs::read_dir(copies::folder(&root.data())).map(|entries| entries.flatten().filter(|entry| entry.file_name().to_string_lossy().starts_with("before-form-")).count()).unwrap_or(0);
        assert!(form == 1 || form == 2, "{form}");
        if form == 2 {
            assert_eq!(before, 1, "at {}: changed with no copy made first", cut.at);
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
        let ids = world(&root);
        // Copied by hand while the backend is open: its last steps are in the -wal.
        let mut keeper = opened(&root, T0 + 2 * MINUTE);
        sign_in(&mut keeper, ALICE, 13, 12, 7, T0 + 2 * MINUTE).unwrap();
        let copy = snapshot(&root, files);
        // Then a device forced out, and nothing else changed (X2).
        let held = fp(1);
        let other = session_of(&mut keeper, ALICE, ids.a2);
        decide(&mut keeper, ALICE, T0 + 3 * MINUTE, &[], |turn| turn.force_out(bearer(ids.a1, &held), other)).unwrap().unwrap();
        keeper.close();
        put_by_hand(&root, &copy);
        let (mut keeper, started) = settled(&root, T0 + 4 * MINUTE);
        assert!(matches!(started, Started::Restored { restore: 1, .. }), "{files:?}: {started:?}");
        assert_eq!(readers(&mut keeper, &ids, T0 + 5 * MINUTE), Vec::<u8>::new(), "{files:?}");
        keeper.close();
    }
}

#[test]
fn what_was_taken_away_since_a_copy_is_taken_away_again_when_it_is_put_back() {
    for change in ["deleted", "switched off", "password changed", "password reset", "code used", "listening data deleted"] {
        let root = Root::new(&format!("since-{}", change.replace(' ', "-")));
        let ids = world(&root);
        let mut keeper = opened(&root, T0 + 2 * MINUTE);
        // Carol has a code waiting, not yet used.
        let carol = keeper.make_account("carol@example.org", "Carol", false, Some((&[44; 32], T0 + 60 * MINUTE)), T0 + 2 * MINUTE).unwrap();
        let kind = match change {
            "deleted" => Some(AdminAction::DeleteAccount),
            "switched off" => Some(AdminAction::SetOn(false)),
            "password reset" => Some(AdminAction::ResetPassword),
            _ => None,
        };
        if let Some(action) = kind.clone() {
            let purpose = admin_leave(&mut keeper, action.clone(), BOB);
            leave(&mut keeper, ROOT, ids.root, 30, 32, purpose, T0 + 2 * MINUTE).unwrap();
        } else if change == "password changed" {
            leave(&mut keeper, BOB, ids.b1, 5, 22, LeaveFor::ChangePassword, T0 + 2 * MINUTE).unwrap();
        } else if change == "listening data deleted" {
            leave(&mut keeper, BOB, ids.b1, 5, 22, LeaveFor::DeleteHistory, T0 + 2 * MINUTE).unwrap();
        }
        keeper.close();
        let copy = snapshot(&root, &["nexus.db"]);
        let mut keeper = opened(&root, T0 + 3 * MINUTE);
        let now = T0 + 3 * MINUTE;
        let held = fp(5);
        match change {
            "code used" => {
                first_password(&mut keeper, carol, 51, 44, 52, 53, now).unwrap();
            }
            "password changed" => {
                let chosen = password(23);
                decide(&mut keeper, BOB, now, &[Change::Password { account: BOB, password: Some(&chosen) }], |turn| turn.change_password(bearer(ids.b1, &held))).unwrap().unwrap();
            }
            "listening data deleted" => decide(&mut keeper, BOB, now, &[], |turn| turn.delete_history(bearer(ids.b1, &held))).unwrap().unwrap(),
            _ => administer(&mut keeper, &ids, kind.unwrap(), BOB, &[], now).unwrap(),
        }
        keeper.close();
        put_by_hand(&root, &copy);
        let (mut keeper, started) = settled(&root, T0 + 4 * MINUTE);
        assert!(matches!(started, Started::Restored { .. }), "{change}: {started:?}");
        let now = T0 + 5 * MINUTE;
        assert_eq!(readers(&mut keeper, &ids, now), Vec::<u8>::new(), "{change}");
        let bob = state(&mut keeper, BOB);
        match change {
            "deleted" => assert_eq!(keeper.account_named(email(BOB)).unwrap(), None, "{change}"),
            "switched off" => assert_eq!(bob.unwrap().status, Status::Off),
            "password changed" | "password reset" => {
                // The copy's password is the old one: it must not work (12.4 step 3).
                assert!(sign_in(&mut keeper, BOB, 21, 22, 9, now).is_err(), "{change}: the old password signed in");
            }
            "code used" => {
                // The code the copy holds again stays used (the review's item 5).
                let vacant = keeper.vacant_device();
                let code = Fingerprint::from_bytes([44; SECRET_BYTES]);
                let answer = decide(&mut keeper, carol, now, &[], |turn| turn.check_code(&secret(51), dev(vacant), 0, code.clone())).unwrap();
                assert!(answer.is_err(), "a used code was taken again");
                // And the copy's code itself is gone from the data (finding 11).
                assert!(stored_code(&keeper, carol, 44).is_none(), "the used code's fingerprint is back in the data");
                assert!(first_password(&mut keeper, carol, 51, 44, 52, 53, now).is_err());
            }
            _ => assert!(bob.unwrap().history_held, "listening data deleted since is held"),
        }
        // Alice is untouched but signs in again, as everyone does (12.4 step 2).
        sign_in(&mut keeper, ALICE, 11, 12, 8, now).unwrap();
    }
}

#[test]
fn a_change_the_data_alone_has_is_seen_once_the_store_has_its_mark() {
    // The gap S3 reported, closed by the design's mark (12.3): a copy from before a device was
    // last heard from is found older, once the store has heard (at most a minute later).
    let root = Root::new("mark");
    let ids = world(&root);
    let copy = snapshot(&root, &["nexus.db"]);
    let mut keeper = opened(&root, T0 + 2 * MINUTE);
    keeper.heard(ids.b1, T0 + 2 * MINUTE).unwrap();
    assert_eq!(last_heard(&keeper, ids.b1), Some(T0 + 2 * MINUTE), "the step was not written");
    keeper.close();
    put_by_hand(&root, &copy);
    let (keeper, started) = settled(&root, T0 + 3 * MINUTE);
    assert!(matches!(started, Started::Restored { .. }), "{started:?}");
    assert_eq!(last_heard(&keeper, ids.b1), None, "the copy is from before it");
    keeper.close();
    // Within the minute, the store has not heard: the same age to the comparison. The design
    // gives that case to a device's stamp (12.4).
    let root = Root::new("mark-within");
    let ids = world(&root);
    let mut keeper = opened(&root, T0 + 2 * MINUTE);
    keeper.heard(ids.b1, T0 + 2 * MINUTE).unwrap();
    keeper.close();
    let copy = snapshot(&root, &["nexus.db"]);
    let mut keeper = opened(&root, T0 + 2 * MINUTE + 10_000);
    keeper.heard(ids.b1, T0 + 2 * MINUTE + 10_000).unwrap();
    // Stopped as a kill would, before the minute was out: the store has not heard.
    drop(keeper);
    put_by_hand(&root, &copy);
    assert_eq!(settled(&root, T0 + 3 * MINUTE).1, Started::Ready);
}

/// Finding 6: in idle time the one thread that writes gives the store the mark once its
/// minute is out, though nothing else is asked; a kill after that cannot hide the change.
#[test]
fn the_last_change_the_data_alone_has_is_marked_in_idle_time() {
    let root = Root::new("mark-idle");
    let ids = world(&root);
    let mut keeper = opened(&root, T0 + 2 * MINUTE);
    keeper.heard(ids.b1, T0 + 2 * MINUTE).unwrap();
    keeper.close();
    let copy = snapshot(&root, &["nexus.db"]);
    let mut keeper = opened(&root, T0 + 2 * MINUTE + 10_000);
    keeper.heard(ids.b1, T0 + 2 * MINUTE + 10_000).unwrap();
    let due = keeper.mark_due().expect("a mark is due");
    keeper.idle(due - 1).unwrap();
    assert_eq!(keeper.mark_due(), Some(due), "written before its minute was out");
    keeper.idle(due).unwrap();
    assert_eq!(keeper.mark_due(), None);
    drop(keeper);
    put_by_hand(&root, &copy);
    let (_, started) = settled(&root, T0 + 7 * 24 * 60 * MINUTE);
    assert!(matches!(started, Started::Restored { .. }), "{started:?}");
}

#[test]
fn the_store_lost_while_the_data_is_kept_stops_until_the_data_is_taken_as_it_is() {
    let root = Root::new("store-lost");
    let ids = world(&root);
    let id = identity::read(&root.state()).unwrap().unwrap();
    fs::remove_file(apart::place(&root.state())).unwrap();
    let why = unlocked(|| Keeper::open(&root.state(), &root.data(), T0 + 5 * MINUTE)).err().unwrap();
    assert!(why.0.contains("is missing, and there is data") && why.0.contains("take-as-is"), "{why}");
    fs::remove_file(root.state().join(identity::FILE)).unwrap();
    unlocked(|| keeper::take_as_is(&root.state(), &root.data(), T0 + 5 * MINUTE)).unwrap();
    let (mut keeper, _) = settled(&root, T0 + 6 * MINUTE);
    assert_eq!(keeper.backend(), id);
    assert!(keeper.set_up());
    assert_eq!(readers(&mut keeper, &ids, T0 + 7 * MINUTE), Vec::<u8>::new());
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
fn the_data_lost_while_the_store_is_kept_is_recovery_never_a_first_start() {
    let root = Root::new("data-lost");
    world(&root);
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
    assert!(keeper.set_up(), "still set up: the first admin's code is not offered again");
    // Each account is made again by the admin, under a new number.
    assert!(keeper.make_account(email(ALICE), "Alice", false, None, T0 + 9 * MINUTE).unwrap() > BOB);
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
    assert_ne!(keeper.backend(), before);
    assert!(!keeper.set_up());
}

#[test]
fn the_data_of_another_backend_is_refused_and_left() {
    let one = Root::new("one");
    let other = Root::new("other");
    world(&one);
    world(&other);
    let copy = copies::nightly(&other.data(), T0 + 5 * MINUTE).unwrap();
    let why = unlocked(|| keeper::put_back(&one.state(), &one.data(), &copy.path, T0 + 5 * MINUTE)).err().unwrap();
    assert!(why.0.contains("another backend's"), "{why}");
    copy_folder(&other.data(), &one.data());
    let why = unlocked(|| Keeper::open(&one.state(), &one.data(), T0 + 5 * MINUTE)).err().unwrap();
    assert!(why.0.contains("another backend's"), "{why}");
}

#[test]
fn a_put_back_by_the_command_ends_every_session_and_is_refused_while_serving() {
    let root = Root::new("put-back-command");
    let ids = world(&root);
    let copy = copies::nightly(&root.data(), T0 + 2 * MINUTE).unwrap();
    {
        let keeper = opened(&root, T0 + 3 * MINUTE);
        let why = keeper::put_back(&root.state(), &root.data(), &copy.path, T0 + 3 * MINUTE).err().unwrap();
        assert!(why.0.contains("is in use"), "{why}");
        let why = keeper::take_as_is(&root.state(), &root.data(), T0 + 3 * MINUTE).err().unwrap();
        assert!(why.0.contains("is in use"), "{why}");
        keeper.close();
    }
    let said = unlocked(|| keeper::put_back(&root.state(), &root.data(), &copy.path, T0 + 4 * MINUTE)).unwrap();
    assert!(said.iter().any(|line| line.contains("restore number 1")), "{said:?}");
    let (mut keeper, started) = settled(&root, T0 + 5 * MINUTE);
    assert_eq!(started, Started::Ready);
    assert_eq!(readers(&mut keeper, &ids, T0 + 5 * MINUTE), Vec::<u8>::new());
    let replaced = fs::read_dir(copies::folder(&root.data())).unwrap().flatten().filter(|entry| entry.file_name().to_string_lossy().starts_with("replaced-")).count();
    assert!(replaced >= 1);
}

#[test]
fn the_nightly_copy_taken_while_writes_go_on_is_whole_when_put_back() {
    let root = Root::new("copy-under-writes");
    world(&root);
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writing = {
        let (stop, state, data) = (stop.clone(), root.state(), root.data());
        std::thread::spawn(move || {
            let mut keeper = unlocked(|| Keeper::open(&state, &data, T0 + 5 * MINUTE)).unwrap().0;
            let mut made = 0u64;
            while !stop.load(Ordering::SeqCst) && made < 300 {
                keeper.make_account(&format!("listener-{made}@example.org"), "A made-up listener", false, Some((&[(made % 200) as u8 + 50; 32], T0 + 60 * MINUTE)), T0 + 5 * MINUTE + made).unwrap();
                made += 1;
            }
            keeper.close();
            made
        })
    };
    let mut made_copies = Vec::new();
    for at in 0..5 {
        std::thread::sleep(std::time::Duration::from_millis(30));
        made_copies.push(copies::nightly(&root.data(), T0 + 6 * MINUTE + at).unwrap());
    }
    stop.store(true, Ordering::SeqCst);
    assert!(writing.join().unwrap() > 0);
    for copy in made_copies {
        // Each is one moment: every account made has its code, all or nothing.
        let connection = data::open_to_read(&copy.path, "the copy").unwrap();
        let lonely: i64 = connection.query_row("SELECT count(*) FROM accounts WHERE NOT EXISTS (SELECT 1 FROM codes WHERE codes.account = accounts.number)", [], |row| row.get(0)).unwrap();
        drop(connection);
        assert!(lonely <= 3, "accounts without the code made in their own step: {lonely}");
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
    let why = unlocked(|| keeper::put_back(&root.state(), &root.data(), &data::place(&root.data()), T0 + 5 * MINUTE)).err().unwrap();
    assert!(why.0.contains("newer Nexus server"), "{why}");
    fs::write(data::place(&root.data()), b"not a database, made up").unwrap();
    let why = unlocked(|| Keeper::open(&root.state(), &root.data(), T0 + 5 * MINUTE)).err().unwrap();
    assert!(why.0.contains("can't be read") || why.0.contains("not the Nexus server's data") || why.0.contains("damaged"), "{why}");
}

#[test]
fn a_clock_set_back_between_runs_reopens_nothing_and_stops_no_copy() {
    let root = Root::new("clock");
    let ids = world(&root);
    // Bob's session, last used at T0, is ended 90 days on (8.7); a start can make it look up to
    // an hour younger than it is (when it was last used is written at most once an hour).
    // (The world was made by a backend just started, whose clock is the store's plus a
    // minute: Bob signed in at T0 and a minute.)
    let late = T0 + MINUTE + SESSION_MS + 60 * MINUTE;
    {
        // Asked at that time, and what the decision did kept (the ending, and the time).
        let mut keeper = opened(&root, late);
        let held = fp(5);
        let answer = decide(&mut keeper, BOB, late, &[], |turn| turn.arrive(bearer(ids.b1, &held), Access::Read).map(|_| ())).unwrap();
        assert!(answer.is_err());
        keeper.close();
    }
    // The clock set back a day, and the program started again: it is not reopened.
    let back = late - 24 * 60 * MINUTE;
    let mut keeper = opened(&root, back);
    // The store's time, plus the minute it may lag by (the ruling of 13:59): never less.
    let effective = keeper.clock(back).effective();
    assert!((late..=late + MINUTE).contains(&effective), "{effective}");
    assert!(!reads(&mut keeper, BOB, ids.b1, 5, back), "a time that had passed came back");
    keeper.close();
    copies::nightly(&root.data(), late).unwrap();
    assert!(copies::due(&root.data(), back));
    for night in 0..9 {
        copies::nightly(&root.data(), back + night).unwrap();
    }
    let kept = copies::list(&root.data());
    assert_eq!(kept.len(), copies::KEPT);
    assert_eq!(kept.last().unwrap().made_at, back + 8);
}

// ---- Full disks ----

const WRITING: &[&str] = &["an account made", "signing in", "signing out", "forcing out", "a device heard from", "the nightly copy"];

#[test]
fn a_full_store_folder_at_each_write_fails_that_step_and_leaves_both_agreeing() {
    let setup = Root::new("full-store");
    let ids = world(&setup);
    for scenario in WRITING {
        let dry = setup.copied("full-store-dry");
        let (points, _) = child(scenario, &dry, 0, 0, None);
        for at in 1..=points.len() {
            let root = setup.copied(&format!("full-store-{at}"));
            let (_, ended) = child(scenario, &root, 0, at, None);
            assert!(ended, "{scenario}: failing at {at} ({}) stopped the program", points[at - 1]);
            let outcome = fs::read_to_string(root.path.join("outcome")).unwrap();
            eprintln!("X2 full store folder, {scenario}, failing at {at} ({}): {}", points[at - 1], if outcome.starts_with("Err") { "Err" } else { "Ok" });
            assert!(outcome.starts_with("Err"), "{scenario}: failing at {} gave {outcome}", points[at - 1]);
            let (mut keeper, _) = settled(&root, T0 + 20 * MINUTE);
            let read = readers(&mut keeper, &ids, T0 + 21 * MINUTE);
            assert!(read.contains(&5) && read.contains(&30), "{scenario} at {at}: {read:?}");
            // What the step was for (finding 11): a failed step never gives; what it takes
            // away is taken once the store's first write of it was in place.
            let cut = Cut { at, points: points.clone() };
            match *scenario {
                // Given only once the store's last write of it is in place (its answer, not
                // given, never reaches the device).
                "signing in" => assert_eq!(read.contains(&7), cut.gave(), "{scenario} failing at {at}: {read:?}"),
                "signing out" => assert_eq!(read.contains(&1), !cut.took(), "{scenario} failing at {at}: {read:?}"),
                "forcing out" => assert_eq!(read.contains(&3), !cut.took(), "{scenario} failing at {at}: {read:?}"),
                _ => {}
            }
        }
    }
}

#[test]
fn a_full_data_disk_fails_a_real_transaction_and_leaves_nothing_half_done() {
    // Room for the file SQLite shares between connections (32 KiB) and no more: the step that
    // makes an account with a 40 KiB name to show cannot be written ahead, and fails as on a
    // full disk; the backend says so, and nothing of it is kept (finding 11).
    let setup = Root::new("full-data-real");
    let ids = world(&setup);
    let (_, _) = child("a large account made", &setup, 0, 0, Some(64));
    let outcome = fs::read_to_string(setup.path.join("outcome")).unwrap_or_else(|_| "(no outcome)".into());
    assert!(outcome.starts_with("Err") && outcome.contains("The data couldn't be written"), "{outcome}");
    let (mut keeper, _) = settled(&setup, T0 + 20 * MINUTE);
    assert_eq!(keeper.account_named("large@example.org").unwrap(), None);
    assert_eq!(readers(&mut keeper, &ids, T0 + 21 * MINUTE), [30, 1, 3, 5]);
    keeper.close();
}

#[test]
fn a_full_data_disk_fails_writes_and_leaves_nothing_half_done() {
    let setup = Root::new("full-data");
    let ids = world(&setup);
    let size = fs::metadata(data::place(&setup.data())).unwrap().len();
    for blocks in [16u32, 64, 160, (size / 512) as u32 + 8] {
        for scenario in WRITING {
            let root = setup.copied(&format!("full-data-{blocks}"));
            let (_, _) = child(scenario, &root, 0, 0, Some(blocks));
            let outcome = fs::read_to_string(root.path.join("outcome")).unwrap_or_else(|_| "(no outcome)".into());
            eprintln!("X2 full data disk, {blocks} blocks, {scenario}: {}", if outcome.starts_with("Err") { "Err" } else if outcome.starts_with("Ok") { "Ok" } else { "no outcome" });
            let (mut keeper, _) = settled(&root, T0 + 20 * MINUTE);
            let read = readers(&mut keeper, &ids, T0 + 21 * MINUTE);
            assert!(read.contains(&5) && read.contains(&30), "{scenario} with {blocks} blocks: {read:?} ({outcome})");
            keeper.close();
        }
    }
}

#[test]
fn opening_and_reading_writes_neither_store() {
    let root = Root::new("reading");
    let ids = world(&root);
    let mut keeper = opened(&root, T0 + 5 * MINUTE);
    let _ = readers(&mut keeper, &ids, T0 + 5 * MINUTE);
    keeper.close();
    let store = fs::read(apart::place(&root.state())).unwrap();
    let data_bytes = fs::read(data::place(&root.data())).unwrap();
    opened(&root, T0 + 5 * MINUTE).close();
    assert_eq!(fs::read(apart::place(&root.state())).unwrap(), store);
    assert_eq!(fs::read(data::place(&root.data())).unwrap(), data_bytes);
}

// ---- The review of S3 (S3b): each finding as a case that fails without its fix ----

fn store_of(root: &Root) -> apart::Store {
    match apart::read(&root.state()) {
        apart::Reading::Found(store) => store,
        other => panic!("the store can't be read: {other:?}"),
    }
}

fn count(keeper: &Keeper, sql: &str, account: u64) -> i64 {
    keeper.data().connection().query_row(sql, [account as i64], |row| row.get(0)).unwrap()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).unwrap_or_else(|why| panic!("{}: {why}", path.display())).permissions().mode() & 0o777
}

/// Finding 8: the data, its copies and the files SQLite writes beside it are the owner's
/// alone from the moment they are made, whatever the umask; files made wider before are
/// tightened at the start.
#[cfg(unix)]
#[test]
fn the_data_and_its_copies_are_the_owners_alone_from_creation() {
    use std::os::unix::fs::PermissionsExt;
    let root = Root::new("private");
    let ids = world(&root);
    let keeper = opened(&root, T0 + MINUTE);
    for file in [data::FILE, "nexus.db-wal", "nexus.db-shm", "lock"] {
        assert_eq!(mode(&root.data().join(file)), 0o600, "{file} while serving");
    }
    for file in [apart::FILE, "lock"] {
        assert_eq!(mode(&root.state().join(file)), 0o600, "{file}");
    }
    keeper.close();
    let copy = copies::nightly(&root.data(), T0 + 2 * MINUTE).unwrap();
    assert_eq!(mode(&copy.path), 0o600, "the nightly copy");
    assert_eq!(mode(&copies::folder(&root.data())), 0o700, "the copies' folder");
    // Made wider by an older program, or by hand: tightened before anything is served.
    fs::set_permissions(data::place(&root.data()), fs::Permissions::from_mode(0o644)).unwrap();
    fs::set_permissions(&copy.path, fs::Permissions::from_mode(0o644)).unwrap();
    fs::set_permissions(copies::folder(&root.data()), fs::Permissions::from_mode(0o755)).unwrap();
    let mut keeper = opened(&root, T0 + 3 * MINUTE);
    assert_eq!(mode(&data::place(&root.data())), 0o600);
    assert_eq!(mode(&copy.path), 0o600);
    assert_eq!(mode(&copies::folder(&root.data())), 0o700);
    assert_eq!(readers(&mut keeper, &ids, T0 + 3 * MINUTE), [30, 1, 3, 5]);
    keeper.close();
    // A put-back's copy of the data it replaced, too.
    unlocked(|| keeper::put_back(&root.state(), &root.data(), &copy.path, T0 + 4 * MINUTE)).unwrap();
    for entry in fs::read_dir(copies::folder(&root.data())).unwrap().flatten() {
        assert_eq!(mode(&entry.path()), 0o600, "{:?}", entry.file_name());
    }
    assert_eq!(mode(&data::place(&root.data())), 0o600, "the data put back");
}

/// Finding 12: what the design keeps out of logs is not in a change's `Debug`.
#[test]
fn a_changes_debug_holds_no_code_name_or_email() {
    let root = Root::new("debug");
    world(&root);
    let mut keeper = opened(&root, T0 + MINUTE);
    let rows = keeper.load(ALICE).unwrap().rows();
    let state = rows.account.clone();
    let code = [0xab; 32];
    let pass = password(0xcd);
    let changes = [
        Change::Account { number: ALICE, shown: "Alice Made-Up", state: &state },
        Change::Install { number: ALICE, rows: &rows, at: T0 },
        Change::Describe { device: 7, name: "Alice's made-up phone", kind: "phone", version: "9.9" },
        Change::Code { account: ALICE, number: 0, code: &code, made_at: T0, until: T0 + 1 },
        Change::Password { account: ALICE, password: Some(&pass) },
    ];
    let shown = format!("{changes:?}");
    for secret in ["alice", "Alice", "171", "ab, ab", "abab", "205", "cdcd"] {
        assert!(!shown.contains(secret), "{secret} in {shown}");
    }
    keeper.close();
}

/// Finding 7: one clock. A step the data alone keeps, at a time far on, does not leave the
/// running backend at one time and a restarted one at another.
#[test]
fn a_data_only_step_far_on_gives_the_same_clock_before_and_after_a_restart() {
    let root = Root::new("one-clock");
    let ids = world(&root);
    let far = T0 + SESSION_MS + MINUTE;
    let back = T0 + 2 * MINUTE;
    let mut keeper = opened(&root, T0 + MINUTE);
    keeper.heard(ids.a1, far).unwrap();
    // The clock put right without a restart: Bob's session last used at T0.
    let (clock, before) = (keeper.clock(back).effective(), reads(&mut keeper, BOB, ids.b1, 5, back));
    assert!(clock < far, "a step the data alone keeps moved the clock");
    keeper.close();
    let mut keeper = opened(&root, back);
    let (again, after) = (keeper.clock(back).effective(), reads(&mut keeper, BOB, ids.b1, 5, back));
    assert_eq!(before, after, "the answer before a restart and after it");
    // After a restart the clock is the store's plus the minute it may lag by: never behind
    // the running backend's, never the data's far time.
    assert!(again >= clock && again <= clock + MINUTE && again < far, "{clock} then {again}");
    keeper.close();
}

/// Finding 6: the data-change mark reaches the store in idle time, and when the backend
/// stops, not only when something else is written a minute later.
#[test]
fn the_last_change_the_data_alone_has_is_marked_when_the_backend_stops() {
    let root = Root::new("mark-stop");
    let ids = world(&root);
    let mut keeper = opened(&root, T0 + 2 * MINUTE);
    keeper.heard(ids.b1, T0 + 2 * MINUTE).unwrap();
    keeper.close();
    let copy = snapshot(&root, &["nexus.db"]);
    let mut keeper = opened(&root, T0 + 2 * MINUTE + 10_000);
    keeper.heard(ids.b1, T0 + 2 * MINUTE + 10_000).unwrap();
    // Nothing more is asked; a week later the backend is stopped.
    keeper.close();
    put_by_hand(&root, &copy);
    let (_, started) = settled(&root, T0 + 7 * 24 * 60 * MINUTE);
    assert!(matches!(started, Started::Restored { .. }), "{started:?}");
}

/// Finding 10: the store is never published at a size its reader refuses.
#[test]
fn the_store_is_never_written_larger_than_it_can_be_read() {
    let root = Root::new("store-bound");
    world(&root);
    let mut big = store_of(&root);
    let template = big.accounts[&ALICE].clone();
    let before = fs::read(apart::place(&root.state())).unwrap();
    for number in 10..80_000 {
        let mut tombstone = template.clone();
        tombstone.apart.account.status = Status::Deleted;
        big.accounts.insert(number, tombstone);
    }
    big.next_account = 80_000;
    assert!(apart::write(&root.state(), &big).is_err(), "a store too large to read back was written");
    assert_eq!(fs::read(apart::place(&root.state())).unwrap(), before);
    assert!(matches!(apart::read(&root.state()), apart::Reading::Found(_)));
}

/// Finding 1, 12.4: with the store lost, taking the data as it is leaves every account
/// waiting for a new one-time code: no password and no code the data held still works.
#[test]
fn with_the_store_lost_no_old_password_or_code_works_after_taking_the_data() {
    let root = Root::new("recovery-fence");
    let ids = world(&root);
    let mut keeper = opened(&root, T0 + MINUTE);
    let carol = keeper.make_account("carol@example.org", "Carol", false, Some((&[44; 32], T0 + 60 * MINUTE)), T0 + MINUTE).unwrap();
    keeper.close();
    fs::remove_file(apart::place(&root.state())).unwrap();
    unlocked(|| keeper::take_as_is(&root.state(), &root.data(), T0 + 2 * MINUTE)).unwrap();
    let (mut keeper, _) = settled(&root, T0 + 3 * MINUTE);
    let now = T0 + 3 * MINUTE;
    assert!(sign_in(&mut keeper, ALICE, 11, 12, 8, now).is_err(), "the old password signed in");
    assert!(sign_in(&mut keeper, ROOT, 31, 32, 8, now).is_err(), "the admin's old password signed in");
    let vacant = keeper.vacant_device();
    let code = Fingerprint::from_bytes([44; SECRET_BYTES]);
    assert!(decide(&mut keeper, carol, now, &[], |turn| turn.check_code(&secret(51), dev(vacant), 0, code.clone())).unwrap().is_err(), "the old code was taken");
    for account in [ROOT, ALICE, BOB, carol] {
        assert_eq!(count(&keeper, "SELECT count(*) FROM codes WHERE account = ?1", account), 0, "{account}'s codes");
        assert_eq!(count(&keeper, "SELECT count(*) FROM accounts WHERE number = ?1 AND password IS NOT NULL", account), 0, "{account}'s verifier");
        assert!(state(&mut keeper, account).unwrap().password_reset_required);
    }
    assert_eq!(readers(&mut keeper, &ids, now), Vec::<u8>::new());
}

/// Finding 1: when a readable store of this backend remains, what it says was taken away is
/// kept when the data is taken as it is (here: the id file lost, and a copy from before an
/// account was deleted put back by hand).
#[test]
fn a_readable_store_of_this_backend_keeps_what_it_took_away_when_the_data_is_taken() {
    let root = Root::new("recovery-keeps");
    let ids = world(&root);
    let copy = snapshot(&root, &["nexus.db"]);
    let seal = store_of(&root).seal_key();
    {
        let mut keeper = opened(&root, T0 + 4 * MINUTE);
        let purpose = admin_leave(&mut keeper, AdminAction::DeleteAccount, BOB);
        leave(&mut keeper, ROOT, ids.root, 30, 32, purpose, T0 + 4 * MINUTE).unwrap();
        administer(&mut keeper, &ids, AdminAction::DeleteAccount, BOB, &[], T0 + 4 * MINUTE).unwrap();
        keeper.close();
    }
    fs::remove_file(root.state().join(identity::FILE)).unwrap();
    put_by_hand(&root, &copy);
    unlocked(|| keeper::take_as_is(&root.state(), &root.data(), T0 + 5 * MINUTE)).unwrap();
    let (mut keeper, _) = settled(&root, T0 + 6 * MINUTE);
    assert_eq!(keeper.account_named(email(BOB)).unwrap(), None, "the deleted account came back");
    assert_eq!(keeper.seal_key(), seal);
    // And the final kept's adoption makes every account wait for a new one-time code even
    // so (the stricter reading: a readable store does not say the data's verifiers are its).
    assert!(sign_in(&mut keeper, ALICE, 11, 12, 8, T0 + 6 * MINUTE).is_err());
    assert!(state(&mut keeper, ALICE).unwrap().password_reset_required);
}

/// Finding 2: taking the data as it is, cut at any point, never loses a readable store's
/// authority (its sealing key, its tombstones).
#[test]
fn taking_the_data_with_a_readable_store_cut_at_any_point_keeps_that_store() {
    let setup = Root::new("take-readable");
    let ids = world(&setup);
    let seal = store_of(&setup).seal_key();
    {
        let mut keeper = opened(&setup, T0 + 4 * MINUTE);
        let purpose = admin_leave(&mut keeper, AdminAction::DeleteAccount, BOB);
        leave(&mut keeper, ROOT, ids.root, 30, 32, purpose, T0 + 4 * MINUTE).unwrap();
        administer(&mut keeper, &ids, AdminAction::DeleteAccount, BOB, &[], T0 + 4 * MINUTE).unwrap();
        keeper.close();
    }
    fs::remove_file(setup.state().join(identity::FILE)).unwrap();
    at_every_point("take-as-is", &setup, |root, cut| {
        let now = T0 + 20 * MINUTE;
        if unlocked(|| Keeper::open(&root.state(), &root.data(), now)).map(|(keeper, _)| keeper.close()).is_err() {
            unlocked(|| keeper::take_as_is(&root.state(), &root.data(), now)).unwrap_or_else(|why| panic!("at {}: {why}", cut.at));
        }
        let (keeper, _) = settled(root, now);
        assert_eq!(keeper.seal_key(), seal, "at {}: the readable store's key was lost", cut.at);
        assert_eq!(keeper.account_named(email(BOB)).unwrap(), None, "at {}", cut.at);
        keeper.close();
    });
}

/// Finding 3: a copy put back is checked, and its restore planned, before anything is
/// changed: one the store cannot take is refused with nothing changed.
#[test]
fn a_copy_that_cannot_be_put_back_is_refused_before_anything_is_changed() {
    let root = Root::new("put-back-checked");
    world(&root);
    // A copy at a restore number this store never gave.
    let ahead = root.copied("put-back-checked-ahead");
    let nightly = copies::nightly(&ahead.data(), T0 + MINUTE).unwrap();
    unlocked(|| keeper::put_back(&ahead.state(), &ahead.data(), &nightly.path, T0 + 2 * MINUTE)).unwrap();
    let ahead_copy = copies::nightly(&ahead.data(), T0 + 3 * MINUTE).unwrap();
    // A copy with an account this store never made.
    let other = root.copied("put-back-checked-unknown");
    opened(&other, T0 + MINUTE).make_account("dave@example.org", "Dave", false, None, T0 + MINUTE).unwrap();
    let unknown_copy = copies::nightly(&other.data(), T0 + 2 * MINUTE).unwrap();
    for copy in [&ahead_copy.path, &unknown_copy.path] {
        let store = fs::read(apart::place(&root.state())).unwrap();
        let live = fs::read(data::place(&root.data())).unwrap();
        let why = unlocked(|| keeper::put_back(&root.state(), &root.data(), copy, T0 + 4 * MINUTE)).err().expect("put back");
        assert!(why.0.contains("Nothing was changed"), "{why}");
        assert_eq!(fs::read(apart::place(&root.state())).unwrap(), store);
        assert_eq!(fs::read(data::place(&root.data())).unwrap(), live);
        assert_eq!(settled(&root, T0 + 5 * MINUTE).1, Started::Ready);
    }
}

/// Finding 4: a put-back cut at any point and run again moves the restore number and each
/// device's number once.
#[test]
fn a_put_back_cut_and_run_again_moves_the_numbers_once() {
    let setup = Root::new("putback-again");
    let ids = world(&setup);
    copies::nightly(&setup.data(), T0 + 2 * MINUTE).unwrap();
    let device = store_of(&setup).accounts[&ALICE].apart.devices[&dev(ids.a1)].number;
    at_every_point("put-back", &setup, |root, cut| {
        let now = T0 + 20 * MINUTE;
        if !cut.whole() {
            unlocked(|| keeper::put_back(&root.state(), &root.data(), &the_copy(root), now)).unwrap_or_else(|why| panic!("at {}: {why}", cut.at));
        }
        let (keeper, _) = settled(root, now);
        assert_eq!(keeper.restore(), 1, "at {}", cut.at);
        keeper.close();
        assert_eq!(store_of(root).accounts[&ALICE].apart.devices[&dev(ids.a1)].number, device + 1, "at {}", cut.at);
    });
}

/// Finding 5: the data a put-back replaces is kept whole, as one checked file, even with
/// committed steps SQLite could not fold back (another reader held them) and a cut anywhere.
#[test]
fn the_data_a_put_back_replaces_is_kept_whole_at_any_cut() {
    let setup = Root::new("putback-keeps");
    world(&setup);
    copies::nightly(&setup.data(), T0 + 2 * MINUTE).unwrap();
    at_every_point("put-back with a reader", &setup, |root, cut| {
        let now = T0 + 20 * MINUTE;
        if !cut.whole() {
            unlocked(|| keeper::put_back(&root.state(), &root.data(), &the_copy(root), now)).unwrap_or_else(|why| panic!("at {}: {why}", cut.at));
        }
        settled(root, now).0.close();
        let kept: Vec<PathBuf> = fs::read_dir(copies::folder(&root.data()))
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.file_name().unwrap().to_string_lossy().starts_with("replaced-") && path.extension().is_some_and(|e| e == "db"))
            .collect();
        assert!(!kept.is_empty(), "at {}: nothing kept", cut.at);
        for path in kept {
            let connection = data::open_to_read(&path, "the data replaced").unwrap_or_else(|why| panic!("at {}: {why}", cut.at));
            let erin: i64 = connection.query_row("SELECT count(*) FROM accounts WHERE email = 'erin@example.org'", [], |row| row.get(0)).unwrap();
            assert_eq!(erin, 1, "at {}: {} lacks the last step", cut.at, path.display());
        }
    });
}

/// Finding 9: a start refuses another backend's data before changing a byte of it.
#[test]
fn another_backends_copy_at_the_datas_place_is_refused_with_nothing_changed() {
    let one = Root::new("foreign-one");
    let other = Root::new("foreign-other");
    world(&one);
    world(&other);
    let copy = copies::nightly(&other.data(), T0 + 5 * MINUTE).unwrap();
    for file in [data::FILE, "nexus.db-wal", "nexus.db-shm"] {
        let _ = fs::remove_file(one.data().join(file));
    }
    fs::copy(&copy.path, data::place(&one.data())).unwrap();
    let names = |root: &Root| {
        let mut names: Vec<String> = fs::read_dir(root.data()).unwrap().flatten().map(|entry| entry.file_name().into_string().unwrap()).collect();
        names.sort();
        names
    };
    let (bytes, before) = (fs::read(data::place(&one.data())).unwrap(), names(&one));
    let why = unlocked(|| Keeper::open(&one.state(), &one.data(), T0 + 6 * MINUTE)).err().unwrap();
    assert!(why.0.contains("another backend's"), "{why}");
    assert_eq!(fs::read(data::place(&one.data())).unwrap(), bytes, "the foreign file was changed");
    assert_eq!(names(&one), before);
}

// ---- Other slices' rows through the one writer (S12/S7, S9) ----

/// A listening event for `account` on `device`, as S12 keeps one, through the one writer.
fn an_event(keeper: &mut Keeper, account: u64, device: u64, n: u8, now: u64) -> Result<bool, Failed> {
    keeper.write_data(now, |tx| {
        let id = format!("{n:032x}");
        let added = tx.execute(
            "INSERT OR IGNORE INTO listening_events (account, device, event_id, at_ms, is_start, line) VALUES (?1, ?2, ?3, ?4, 1, 'a made-up track')",
            rusqlite::params![account as i64, device as i64, id, now as i64],
        )?;
        Ok((added > 0, added > 0))
    })
}

fn events(keeper: &Keeper, account: u64) -> i64 {
    count(keeper, "SELECT count(*) FROM listening_events WHERE account = ?1", account)
}

#[test]
fn another_slices_step_through_the_one_writer_is_counted_or_kept_not_at_all() {
    let root = Root::new("write-data");
    let ids = world(&root);
    let mut keeper = opened(&root, T0 + 2 * MINUTE);
    let before = keeper.data().meta().unwrap();
    // Changed: counted, written at the store's time (a far clock moves nothing), all of it.
    let far = T0 + 400 * 24 * 60 * MINUTE;
    assert!(an_event(&mut keeper, BOB, ids.b1, 1, far).unwrap());
    let after = keeper.data().meta().unwrap();
    assert_eq!(after.changes, before.changes + 1);
    assert!(after.written_at < far, "a data-only step moved the data's time");
    assert!(keeper.clock(T0 + 2 * MINUTE).effective() < far, "a data-only step moved the clock");
    // Not changed: nothing kept, nothing counted.
    assert!(!an_event(&mut keeper, BOB, ids.b1, 1, T0 + 3 * MINUTE).unwrap());
    assert_eq!(keeper.data().meta().unwrap().changes, after.changes);
    // A step that fails part-way keeps none of it.
    let failed = keeper.write_data(T0 + 3 * MINUTE, |tx| {
        tx.execute("INSERT INTO listening_events (account, device, event_id, at_ms, is_start, line) VALUES (?1, 1, ?2, 1, 1, 'x')", rusqlite::params![BOB as i64, format!("{:032x}", 2)])?;
        tx.execute("INSERT INTO listening_events (account, device, event_id, at_ms, is_start, line) VALUES (999, 1, 'too short', 1, 1, 'x')", [])?;
        Ok(((), true))
    });
    assert!(failed.is_err());
    assert_eq!(events(&keeper, BOB), 1);
    assert_eq!(count(&keeper, "SELECT starts FROM listening_totals WHERE account = ?1", BOB), 1);
    keeper.close();
}

#[test]
fn an_event_kept_after_a_copy_shows_the_copy_older_once_idle_time_gives_the_mark() {
    // Findings 6 and 7, for another slice's rows.
    let root = Root::new("write-data-mark");
    let ids = world(&root);
    let mut keeper = opened(&root, T0 + 2 * MINUTE);
    keeper.heard(ids.b1, T0 + 2 * MINUTE).unwrap();
    keeper.close();
    let copy = snapshot(&root, &["nexus.db"]);
    let mut keeper = opened(&root, T0 + 2 * MINUTE + 10_000);
    an_event(&mut keeper, BOB, ids.b1, 7, T0 + 2 * MINUTE + 10_000).unwrap();
    let due = keeper.mark_due().expect("the step is not yet marked");
    keeper.idle(due).unwrap();
    drop(keeper);
    put_by_hand(&root, &copy);
    let (keeper, started) = settled(&root, T0 + 9 * MINUTE);
    assert!(matches!(started, Started::Restored { .. }), "{started:?}");
    assert_eq!(events(&keeper, BOB), 0);
}

#[test]
fn an_events_step_cut_at_any_point_is_kept_whole_or_not_at_all() {
    let setup = Root::new("event-cut");
    let ids = world(&setup);
    at_every_point("an event kept", &setup, |root, cut| {
        let (keeper, _) = settled(root, T0 + 20 * MINUTE);
        let (kept, totals) = (events(&keeper, BOB), count(&keeper, "SELECT coalesce(sum(starts), 0) FROM listening_totals WHERE account = ?1", BOB));
        assert_eq!(kept, totals, "at {}: the count and the events disagree", cut.at);
        if cut.whole() || cut.points[..cut.at].iter().any(|point| point == "data: committed") {
            assert_eq!(kept, 1, "at {}", cut.at);
        }
        let _ = ids;
    });
}

#[test]
fn deleting_listening_data_takes_the_events_in_its_own_step_and_a_put_back_holds_them() {
    let root = Root::new("events-life");
    let ids = world(&root);
    let mut keeper = opened(&root, T0 + 2 * MINUTE);
    for n in 0..3 {
        an_event(&mut keeper, BOB, ids.b1, n, T0 + 2 * MINUTE).unwrap();
    }
    an_event(&mut keeper, ALICE, ids.a1, 9, T0 + 2 * MINUTE).unwrap();
    keeper.close();
    let copy = snapshot(&root, &["nexus.db"]);
    let mut keeper = opened(&root, T0 + 3 * MINUTE);
    leave(&mut keeper, BOB, ids.b1, 5, 22, LeaveFor::DeleteHistory, T0 + 3 * MINUTE).unwrap();
    let held = fp(5);
    decide(&mut keeper, BOB, T0 + 3 * MINUTE, &[], |turn| turn.delete_history(bearer(ids.b1, &held))).unwrap().unwrap();
    assert_eq!((events(&keeper, BOB), events(&keeper, ALICE)), (0, 1), "the events and the life moved in one step");
    assert_eq!(count(&keeper, "SELECT count(*) FROM listening_totals WHERE account = ?1", BOB), 0);
    keeper.close();
    // The copy from before holds them again: held, not served, until Bob answers (12.5).
    put_by_hand(&root, &copy);
    let (mut keeper, _) = settled(&root, T0 + 4 * MINUTE);
    assert!(state(&mut keeper, BOB).unwrap().history_held);
    assert_eq!(events(&keeper, BOB), 3, "held, not deleted, by the restore");
}

#[test]
fn diagnostics_past_their_age_go_in_idle_time_a_bounded_number_at_a_time() {
    let root = Root::new("diag-age");
    let ids = world(&root);
    let mut keeper = opened(&root, T0 + 2 * MINUTE);
    let now = T0 + 2 * MINUTE;
    keeper
        .write_data(now, |tx| {
            for n in 0..(keeper::EXPIRED_PER_IDLE + 3) {
                tx.execute(
                    "INSERT INTO diagnostic_reports (account, device, restore, request_id, received_ms, text) VALUES (?1, ?2, 0, ?3, ?4, 'a made-up report')",
                    rusqlite::params![BOB as i64, ids.b1 as i64, n.to_be_bytes().as_slice(), n],
                )?;
            }
            tx.execute(
                "INSERT INTO diagnostic_reports (account, device, restore, request_id, received_ms, text) VALUES (?1, ?2, 0, ?3, ?4, 'a recent one')",
                rusqlite::params![BOB as i64, ids.b1 as i64, (-1i64).to_be_bytes().as_slice(), now as i64],
            )?;
            Ok(((), true))
        })
        .unwrap();
    let reports = |keeper: &Keeper| count(keeper, "SELECT count(*) FROM diagnostic_reports WHERE account = ?1", BOB);
    keeper.idle(now).unwrap();
    assert_eq!(reports(&keeper), 4, "at most EXPIRED_PER_IDLE go at once");
    keeper.idle(now).unwrap();
    assert_eq!(reports(&keeper), 1, "the recent one stays");
    keeper.close();
}

#[test]
fn a_music_login_is_kept_only_with_its_own_account_count_and_where_it_was_checked() {
    use nexus_core::nexus::account::{MusicAddress, Name};
    use nexus_core::nexus::kept::{DataChange, Effects, Update};
    use nexus_core::nexus::music::{CredentialBinding, MusicInstallation, SealedCredential};
    let root = Root::new("music-login");
    world(&root);
    let mut keeper = opened(&root, T0 + 2 * MINUTE);
    let loaded = keeper.load(ALICE).unwrap();
    let (mut rows, mut apart) = (loaded.rows(), loaded.apart());
    rows.account.music_login = 1;
    apart.account.music_login = 1;
    let install = |account: u64, count: u64| MusicInstallation {
        binding: CredentialBinding {
            backend: keeper.backend(),
            account: AccountId::new(account).unwrap(),
            address: MusicAddress::new("https://music.example.org").unwrap(),
            name: Name::new("alice").unwrap(),
            count,
        },
        sealed: SealedCredential::from_bytes(&[7; 64]).unwrap(),
    };
    let effects = |install: MusicInstallation| Effects {
        high_water: keeper.clock(T0 + 2 * MINUTE).effective(),
        updates: vec![Update { apart_first: apart.clone(), data: rows.clone(), change: DataChange::InstallMusicLogin(install), music: None, apart_last: apart.clone() }],
    };
    let checked = [Change::MusicChecked { account: ALICE, internal: "http://navidrome:4533" }];
    let logins = |keeper: &Keeper| count(keeper, "SELECT count(*) FROM music_logins WHERE account = ?1", ALICE);
    let (wrong_account, wrong_count, right) = (effects(install(BOB, 1)), effects(install(ALICE, 2)), effects(install(ALICE, 1)));
    assert!(keeper.persist(&wrong_account, &checked, T0 + 2 * MINUTE).is_err(), "sealed for another account");
    assert!(keeper.persist(&wrong_count, &checked, T0 + 2 * MINUTE).is_err(), "sealed under another count");
    assert!(keeper.persist(&right, &[], T0 + 2 * MINUTE).is_err(), "with no address it was checked at");
    assert_eq!(logins(&keeper), 0, "nothing was kept by a refused one");
    assert_eq!(state(&mut keeper, ALICE).unwrap().music_login, 0);
    keeper.persist(&right, &checked, T0 + 2 * MINUTE).unwrap();
    assert_eq!(logins(&keeper), 1);
    let (count_kept, internal): (i64, String) =
        keeper.data().connection().query_row("SELECT count, internal FROM music_logins WHERE account = ?1", [ALICE as i64], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    assert_eq!((count_kept, internal.as_str()), (1, "http://navidrome:4533"));
    assert_eq!(state(&mut keeper, ALICE).unwrap().music_login, 1);
    keeper.close();
}

// ---- The write filter (the review of S3b, B) ----

fn description(keeper: &Keeper, device: u64) -> String {
    keeper.data().connection().query_row("SELECT name FROM devices WHERE id = ?1", [device as i64], |row| row.get(0)).unwrap()
}

fn verifier(keeper: &Keeper, account: u64) -> Option<[u8; 32]> {
    keeper.data().password(account).unwrap().map(|password| password.hash)
}

#[test]
fn a_refused_decision_that_still_touches_a_session_keeps_no_verifier_code_or_description_given_with_it() {
    let root = Root::new("filter-touch");
    let ids = world(&root);
    // Two hours on, past the hourly write of "last used": the refused change still writes.
    let now = T0 + 120 * MINUTE;
    let mut keeper = opened(&root, now);
    let (old, name, codes) = (verifier(&keeper, ALICE), description(&keeper, ids.a2), count(&keeper, "SELECT count(*) FROM codes WHERE account = ?1", BOB));
    let (held, chosen) = (fp(1), password(66));
    let removals = [Change::Password { account: BOB, password: None }, Change::Codes { account: BOB, keep: None }];
    for removal in &removals {
        let extra = [Change::Password { account: ALICE, password: Some(&chosen) }, removal.clone()];
        let answer = decide(&mut keeper, ALICE, now, &extra, |turn| turn.change_password(bearer(ids.a1, &held)));
        assert!(answer.is_err(), "a removal given by the caller was taken: {answer:?}");
    }
    let changes = keeper.data().meta().unwrap().changes;
    let extra = [
        Change::Password { account: ALICE, password: Some(&chosen) },
        Change::Describe { device: ids.a2, name: "renamed by a refused call", kind: "pc", version: "0" },
    ];
    let answer = decide(&mut keeper, ALICE, now, &extra, |turn| turn.change_password(bearer(ids.a1, &held))).unwrap();
    assert!(answer.is_err(), "the change had no grant");
    assert!(keeper.data().meta().unwrap().changes > changes, "the refused decision wrote nothing: the case does not hold");
    assert_eq!(verifier(&keeper, ALICE), old);
    assert_eq!(description(&keeper, ids.a2), name);
    assert_eq!(count(&keeper, "SELECT count(*) FROM codes WHERE account = ?1", BOB), codes);
    assert!(verifier(&keeper, BOB).is_some());
    keeper.close();
}

#[test]
fn exactly_one_verifier_for_a_change_and_only_the_live_code_for_a_reset() {
    let root = Root::new("filter-one");
    let ids = world(&root);
    let now = T0 + 4 * MINUTE;
    let mut keeper = opened(&root, now);
    leave(&mut keeper, ALICE, ids.a1, 1, 12, LeaveFor::ChangePassword, now).unwrap();
    let (held, first, second) = (fp(1), password(66), password(67));
    let old = verifier(&keeper, ALICE);
    let two = [Change::Password { account: ALICE, password: Some(&first) }, Change::Password { account: ALICE, password: Some(&second) }];
    assert!(decide(&mut keeper, ALICE, now, &two, |turn| turn.change_password(bearer(ids.a1, &held))).is_err(), "two verifiers were taken");
    assert_eq!(verifier(&keeper, ALICE), old);
    let other = [Change::Password { account: ALICE, password: Some(&first) }, Change::Password { account: BOB, password: Some(&second) }];
    assert!(decide(&mut keeper, ALICE, now, &other, |turn| turn.change_password(bearer(ids.a1, &held))).is_err(), "a verifier for another account was taken");
    let bob = verifier(&keeper, BOB);
    assert!(decide(&mut keeper, ALICE, now, &[], |turn| turn.change_password(bearer(ids.a1, &held))).is_err(), "a change with no verifier was kept");
    decide(&mut keeper, ALICE, now, &two[..1], |turn| turn.change_password(bearer(ids.a1, &held))).unwrap().unwrap();
    assert_eq!(verifier(&keeper, ALICE), Some(first.hash));
    assert_eq!(verifier(&keeper, BOB), bob);
    // A reset: only the live code the decision made, by its number and deadline.
    let purpose = admin_leave(&mut keeper, AdminAction::ResetPassword, BOB);
    leave(&mut keeper, ROOT, ids.root, 30, 32, purpose, now).unwrap();
    let live = |number| [Change::Code { account: BOB, number, code: &[46; 32], made_at: now, until: now + CODE_LIFETIME_MS }];
    assert!(administer(&mut keeper, &ids, AdminAction::ResetPassword, BOB, &live(99), now).is_err(), "a code under another number was taken");
    let late = [Change::Code { account: BOB, number: 2, code: &[46; 32], made_at: now, until: now + CODE_LIFETIME_MS + 1 }];
    assert!(administer(&mut keeper, &ids, AdminAction::ResetPassword, BOB, &late, now).is_err(), "a code with another deadline was taken");
    assert!(stored_code(&keeper, BOB, 46).is_none());
    administer(&mut keeper, &ids, AdminAction::ResetPassword, BOB, &live(2), now).unwrap();
    assert!(stored_code(&keeper, BOB, 46).is_some());
    keeper.close();
}

/// The review of S3b, 7: a step that changes a row carries the clock to the store first,
/// though a decision that changed none moved the running clock by less than a minute.
#[test]
fn a_row_written_carries_the_running_clock_to_the_store_first() {
    let root = Root::new("row-clock");
    let ids = world(&root);
    let mut keeper = opened(&root, T0 + 2 * MINUTE);
    // (A start's clock is the store's plus a minute: a first row written brings them level.)
    keeper.heard(ids.a1, T0 + 2 * MINUTE).unwrap();
    let stored = store_of(&root).high_water;
    assert_eq!(stored, keeper.clock(0).effective());
    // A decision with no row to change, half a minute on from the store's clock: not written.
    let at = keeper.clock(0).effective() + MINUTE / 2;
    let held = fp(5);
    let _ = decide(&mut keeper, BOB, at, &[], |turn| turn.arrive(bearer(ids.b1, &held), Access::Read).map(|_| ())).unwrap();
    assert_eq!(store_of(&root).high_water, stored, "a decision with no row wrote the clock before its minute");
    let running = keeper.clock(0).effective();
    assert!(running >= at);
    // A device heard from at a time of its own: the running clock reaches the store first.
    keeper.heard(ids.b1, T0 + 3 * MINUTE).unwrap();
    assert_eq!(store_of(&root).high_water, running);
    let written = keeper.data().meta().unwrap().written_at;
    assert!(written <= running, "{written} > {running}");
    assert_eq!(last_heard(&keeper, ids.b1), Some(T0 + 3 * MINUTE), "the observation's own time is kept");
    keeper.close();
}

/// The re-review's 8: a link at the data's place is refused, never followed, and what it
/// points at is left as it is.
#[cfg(unix)]
#[test]
fn a_link_at_the_datas_place_is_refused_and_its_target_left_as_it_is() {
    use std::os::unix::fs::PermissionsExt;
    let root = Root::new("link");
    world(&root);
    let elsewhere = root.path.join("elsewhere.db");
    fs::rename(data::place(&root.data()), &elsewhere).unwrap();
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o644)).unwrap();
    std::os::unix::fs::symlink(&elsewhere, data::place(&root.data())).unwrap();
    let bytes = fs::read(&elsewhere).unwrap();
    let why = unlocked(|| Keeper::open(&root.state(), &root.data(), T0 + 5 * MINUTE)).err().unwrap();
    assert!(why.0.contains("is a link"), "{why}");
    assert_eq!(mode(&elsewhere), 0o644);
    assert_eq!(fs::read(&elsewhere).unwrap(), bytes);
    // A copy that is a link is refused too.
    fs::remove_file(data::place(&root.data())).unwrap();
    fs::rename(&elsewhere, data::place(&root.data())).unwrap();
    let nightly = copies::nightly(&root.data(), T0 + 6 * MINUTE).unwrap();
    let moved = root.path.join("copy-elsewhere.db");
    fs::rename(&nightly.path, &moved).unwrap();
    std::os::unix::fs::symlink(&moved, &nightly.path).unwrap();
    let why = unlocked(|| Keeper::open(&root.state(), &root.data(), T0 + 7 * MINUTE)).err().unwrap();
    assert!(why.0.contains("is a link"), "{why}");
}

/// The re-review's C, 3 and 4: a put-back cut after its plan is in the store goes on only
/// with the very copy it planned: never with another file waiting, other data in place, or
/// another backend's data at the wrong folder.
#[test]
fn a_put_back_under_way_goes_on_only_with_the_copy_it_planned() {
    let setup = Root::new("putback-which");
    let ids = world(&setup);
    let other = Root::new("putback-which-other");
    world(&other);
    copies::nightly(&setup.data(), T0 + 2 * MINUTE).unwrap();
    // The data goes on after the copy: it is not the copy.
    let mut keeper = opened(&setup, T0 + 3 * MINUTE);
    keeper.heard(ids.b1, T0 + 3 * MINUTE).unwrap();
    keeper.close();
    let dry = setup.copied("putback-which-dry");
    let (points, _) = child("put-back", &dry, 0, 0, None);
    let begun = points.iter().position(|point| point == "put-back: begun").expect("no point before the copy is put in place") + 1;
    let cut = |name: &str| {
        let root = setup.copied(name);
        let (_, ended) = child("put-back", &root, begun, 0, None);
        assert!(!ended);
        assert!(matches!(store_of(&root).pending, Some(compare::Pending::PutBack { .. })));
        root
    };
    let refused = |root: &Root, what: &str| {
        let live = fs::read(data::place(&root.data())).unwrap_or_default();
        let store = fs::read(apart::place(&root.state())).unwrap();
        let why = unlocked(|| Keeper::open(&root.state(), &root.data(), T0 + 20 * MINUTE)).err().unwrap_or_else(|| panic!("{what}: served"));
        let again = unlocked(|| keeper::put_back(&root.state(), &root.data(), &the_copy(root), T0 + 20 * MINUTE)).err().unwrap_or_else(|| panic!("{what}: the command went on"));
        assert_eq!(fs::read(data::place(&root.data())).unwrap_or_default(), live, "{what}: the data was changed");
        assert_eq!(fs::read(apart::place(&root.state())).unwrap(), store, "{what}: the store was changed");
        (why.0, again.0)
    };
    // Another file waiting in the copy's place.
    let root = cut("putback-which-swapped");
    fs::write(data::incoming(&root.data()), b"made up").unwrap();
    let (why, again) = refused(&root, "another file waiting");
    assert!(why.contains("not the copy it planned") && again.contains("not the copy it planned"), "{why} / {again}");
    // The copy gone, and the data it was to replace still in place.
    let root = cut("putback-which-incumbent");
    fs::remove_file(data::incoming(&root.data())).unwrap();
    let (why, _) = refused(&root, "the incumbent");
    assert!(why.contains("not the copy it planned"), "{why}");
    // The wrong folder at NEXUS_DATA: another backend's data, no copy waiting.
    let root = cut("putback-which-foreign");
    fs::remove_file(data::incoming(&root.data())).unwrap();
    for file in [data::FILE, "nexus.db-wal", "nexus.db-shm"] {
        let _ = fs::remove_file(root.data().join(file));
    }
    fs::copy(copies::nightly(&other.data(), T0 + 3 * MINUTE).unwrap().path, data::place(&root.data())).unwrap();
    let (why, again) = refused(&root, "another backend's data");
    assert!(why.contains("another backend's") && again.contains("another backend's"), "{why} / {again}");
    // And the copy planned, untouched: the start finishes it.
    let root = cut("putback-which-right");
    let (keeper, started) = settled(&root, T0 + 20 * MINUTE);
    assert_eq!(keeper.restore(), 1, "{started:?}");
}

/// The re-review's 10: at the edge of the store's room, what takes leave away is still
/// written, and what gives leave (a session) is refused.
#[test]
fn at_the_stores_edge_a_reset_is_written_and_a_sign_in_is_refused() {
    let root = Root::new("edge");
    let ids = world(&root);
    {
        let mut keeper = opened(&root, T0 + 2 * MINUTE);
        let purpose = admin_leave(&mut keeper, AdminAction::ResetPassword, BOB);
        leave(&mut keeper, ROOT, ids.root, 30, 32, purpose, T0 + 2 * MINUTE).unwrap();
        keeper.close();
    }
    // Tombstones of accounts long gone, to just past the room kept for growth.
    let mut big = store_of(&root);
    let mut tombstone = big.accounts[&BOB].clone();
    tombstone.apart.account.status = Status::Deleted;
    tombstone.apart.devices.clear();
    let mut number = 100;
    while apart::write_within(&root.state(), &big, apart::GROWTH_MAX_BYTES).is_ok() {
        let line = 300;
        for _ in 0..((apart::GROWTH_MAX_BYTES as usize - fs::metadata(apart::place(&root.state())).unwrap().len() as usize) / line / 2).max(50) {
            big.accounts.insert(number, tombstone.clone());
            number += 1;
        }
        big.next_account = number;
    }
    apart::write(&root.state(), &big).unwrap();
    let now = T0 + 3 * MINUTE;
    let mut keeper = opened(&root, now);
    assert!(sign_in(&mut keeper, ALICE, 15, 12, 9, now).is_err(), "a session was given past the room for growth");
    assert!(leave(&mut keeper, ALICE, ids.a1, 1, 12, LeaveFor::ChangePassword, now).is_err(), "a grant was given past the room for growth");
    let until = keeper.clock(now).effective() + CODE_LIFETIME_MS;
    administer(&mut keeper, &ids, AdminAction::ResetPassword, BOB, &[Change::Code { account: BOB, number: 2, code: &[47; 32], made_at: now, until }], now).unwrap();
    assert!(!reads(&mut keeper, BOB, ids.b1, 5, now), "the reset did not take Bob's session away");
    assert!(state(&mut keeper, BOB).unwrap().password_reset_required);
    keeper.close();
}
