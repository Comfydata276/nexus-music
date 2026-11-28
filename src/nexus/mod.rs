//! The Nexus server's component: this app's side of the backend of our own that sits beside
//! the music server, under `/nexus/` at the music server's own origin (design 4, 5 and 6).
//!
//! **Off unless the app is started with `NEXUS_MUSIC_NEXUS=1`** (as linking is with
//! `NEXUS_MUSIC_LINK=1`), and kept off for everyone until the checks that only a real
//! Windows PC can run have been run there (README, "Known, and accepted"). Off, nothing of it
//! is made: no timer, no thread, no process, no request, no row, and the window does not
//! track the account for it; and `--nexus-probe` is not obeyed (the app starts as it would
//! with any argument it does not know).
//!
//! At this stage (slice W1) it is an empty shell, and does one thing: for the account the
//! window has published it asks the backend's `hello`, and says under Settings, Account,
//! whether there is a Nexus server at that address. It reads, sends and keeps nothing else:
//! no sign-in, no plays, no file.
//!
//! **When it asks.** Five seconds after an account is published (not on the window's first
//! tick), then an hour after each answer, for as long as the account is in use. That goes on
//! while the window is hidden in the notification area; nothing wakes a sleeping PC for it.
//! Not while Offline mode is on (it looks again a minute later), and never for an address
//! that is not HTTPS (`proto::hello_url`), for which the row says "None found" at once.
//!
//! **A look is dispatched on the window's thread, and that is the boundary.** At that moment
//! the account the window publishes must be the one the look is for, and Offline mode must be
//! off; a look dispatched is a look started, whatever happens on the window afterwards, and
//! its answer is shown only if, when it comes back to the window's thread, that account is
//! still published (and its generation still the one in use) and Offline mode is still off.
//! The row is shown only while the account it was set for is signed in.
//!
//! **How it asks: in a process of its own, under one deadline.** Each look runs this same
//! program (the file it was started from, unchanged since) again with exactly two arguments,
//! `--nexus-probe <address>`, and no standard input, output or error. That process makes one
//! agent, sends one `GET`, and says what it found by its exit status alone: [`FOUND`],
//! [`NO_BACKEND`]; anything else is no answer. The component's supervisor, a thread of its
//! own, gives it [`DEADLINE`] from before it is started: whatever it is doing then (being
//! started, a name look-up, a forward proxy, connecting, TLS and the certificate check, the
//! headers or the body) it is killed, and reaped for at most [`REAP`] without ever blocking;
//! an answer that comes after the deadline is no answer. When the app quits, a look still
//! running is killed and reaped the same way ([`shutdown`]). So:
//!
//! - a panic, which the shipped build turns into an abort (`panic = "abort"` in Cargo.toml),
//!   ends that look and never this process;
//! - nothing a look waits on outlives the deadline: the process, its threads (ureq's own for
//!   a timed name look-up among them), its connection and its pool go with it;
//! - one look is one request on the wire: a fresh process has no connection to reuse, and
//!   ureq sends again only by following a redirect, which is switched off.
//!
//! What the look's process is given: the environment (a forward proxy's variables are
//! honoured, as by the app; Windows needs its own variables to reach the network), the
//! current folder, and on Unix no descriptor but the three standard ones, all `/dev/null`
//! (Rust opens every other with close-on-exec). On Windows, Rust's standard library makes
//! the handles it opens not inheritable and starts the process with no console window
//! (`CREATE_NO_WINDOW`); which other handles it inherits is to be checked there (README).
//!
//! **Nothing that plays, queues, downloads or counts music waits for it** (design 5):
//!
//! - It is not made by the controller, and nothing on those paths names it (a test below
//!   reads their source). `main` installs it after the controller is made.
//! - On the window's thread it does only this: reads three properties of the window and
//!   whether Offline mode is on (`try_borrow` of the settings: busy counts as on), arms a
//!   timer, starts a thread, and sets its own row. Nothing waits for the thread.
//! - It reaches the rest of the app through one seam: the row under Settings
//!   (`Settings.nexus-server`, for `Settings.nexus-for`), which it sets and nothing else reads.

use std::cell::{Cell, RefCell};
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use nexus_core::nexus::proto;
use slint::ComponentHandle;

use crate::ui;

mod call;
mod signin;

/// How the program is told it is a look, and at what.
const PROBE_ARG: &str = "--nexus-probe";
/// The first look after an account is published, so that it is never on the first tick.
const FIRST_AFTER: Duration = Duration::from_secs(5);
/// Between an answer and the next look.
const AGAIN: Duration = Duration::from_secs(3600);
/// How soon to look again when Offline mode was on, or a look could not be started.
const LATER: Duration = Duration::from_secs(60);
/// How soon to look again when another look (of an account let go of) is still running.
const BUSY: Duration = Duration::from_secs(1);
/// The one deadline over a look, from before its process is started to its exit.
pub const DEADLINE: Duration = Duration::from_secs(10);
/// The most a killed look's process is waited for, a poll at a time, before it is let go.
const REAP: Duration = Duration::from_secs(2);
/// Inside the look's process, ureq's own limits, so that it normally ends by itself first.
const CONNECT: Duration = Duration::from_secs(5);
const WHOLE: Duration = Duration::from_secs(8);
/// What the look's process says by its exit status.
pub const FOUND: i32 = 10;
pub const NO_BACKEND: i32 = 11;
const NO_ANSWER: i32 = 12;

/// What the row under Settings says.
const NONE_FOUND: &str = "None found";

/// What one look found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The backend's own `hello`.
    Found,
    /// An answer, and not the backend's.
    NoBackend,
    /// No answer by the deadline, or the look's process failed.
    NoAnswer,
}

/// The component for the account in use: held on the window's thread only.
struct Running {
    generation: u64,
    /// The account it is for, as the window published it.
    account: String,
    /// Where `hello` is asked: `None` for an address that is not HTTPS.
    url: Option<String>,
    timer: slint::Timer,
}

thread_local! {
    static RUNNING: RefCell<Option<Running>> = const { RefCell::new(None) };
    static GENERATION: Cell<u64> = const { Cell::new(0) };
    static TRIGGER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
}

/// A look is running (of any account): one at a time.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// The look's process while it runs: the supervisor's, and `shutdown`'s at the app's end.
static CHILD: Mutex<Option<Child>> = Mutex::new(None);
/// This program's own file as it was when the component was installed: a look is only ever
/// started from it, unchanged.
static OWN: OnceLock<Option<(PathBuf, u64, Option<SystemTime>)>> = OnceLock::new();

fn own_file() -> Option<(PathBuf, u64, Option<SystemTime>)> {
    let path = std::env::current_exe().ok()?;
    let meta = std::fs::metadata(&path).ok()?;
    Some((path, meta.len(), meta.modified().ok()))
}

/// Installs the component on `ui`: what is kept of the last sign-in read, the window's
/// sign-in screen and Account settings handed to it (`signin`), and its look started once
/// the event loop runs (for an account the controller published while it was being made),
/// and each time the window says the account has changed. Called by `main` after the
/// controller is made (and a kept music login entered); never by it.
pub fn install(ui: &ui::MainWindow) {
    OWN.get_or_init(own_file);
    signin::install(ui);
    let app = ui.global::<ui::App>();
    // Only now does the window track the account (app.slint: `account`).
    app.set_nexus_on(true);
    let weak = ui.as_weak();
    app.on_account_changed(move || {
        if let Some(late) = test_ms("NEXUS_TEST_LATE_FOLLOW_MS") {
            // For the tests only: the window's account-changed handler made to come late, so
            // that looks come due between an account's going and the handler, every time.
            let weak = weak.clone();
            slint::Timer::single_shot(late, move || {
                if let Some(ui) = weak.upgrade() {
                    follow(&ui);
                }
            });
        } else if let Some(ui) = weak.upgrade() {
            follow(&ui);
        }
    });
    let weak = ui.as_weak();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            follow(&ui);
        }
    });
    trigger_for_tests(ui);
}

/// At the app's end: a look still running is killed, and reaped without blocking for more
/// than [`REAP`]. Called by `main` after the window has closed.
pub fn shutdown() {
    signin::stop();
    call::shutdown();
    let child = CHILD.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take();
    if let Some(mut child) = child {
        end(&mut child);
        note(format_args!("shutdown {}", child.id()));
    }
}

/// The account as the window publishes it now: empty when nobody is signed in.
fn published(ui: &ui::MainWindow) -> String {
    let app = ui.global::<ui::App>();
    if !app.get_signed_in() {
        return String::new();
    }
    format!("{}\n{}", app.get_server(), app.get_username())
}

/// Lets go of the component of an account that is no longer the one published, and starts
/// one for the account that is. Never waits for anything.
fn follow(ui: &ui::MainWindow) {
    let account = published(ui);
    if RUNNING.with(|running| running.borrow().as_ref().is_some_and(|running| running.account == account)) {
        return;
    }
    let old = RUNNING.with(|running| running.borrow_mut().take());
    drop(old);
    let settings = ui.global::<ui::Settings>();
    settings.set_nexus_server("".into());
    settings.set_nexus_for("".into());
    if account.is_empty() {
        note(format_args!("published 0 -"));
        return;
    }
    let generation = GENERATION.with(|counter| {
        counter.set(counter.get() + 1);
        counter.get()
    });
    // The backend this account was signed in through: where it is asked.
    let (server, name) = account.split_once('\n').unwrap_or_default();
    let url = signin::backend_for(server, name).and_then(|backend| proto::hello_url(&backend));
    note(format_args!("published {generation} {}", url.as_deref().unwrap_or("-")));
    if url.is_none() {
        // Not HTTPS: there can be none, and nothing is asked.
        settings.set_nexus_server(NONE_FOUND.into());
        settings.set_nexus_for(account.as_str().into());
    }
    let asks = url.is_some();
    RUNNING.with(|running| *running.borrow_mut() = Some(Running { generation, account, url, timer: slint::Timer::default() }));
    if asks {
        schedule(ui, generation, FIRST_AFTER);
    }
}

/// Arms the timer of the component of `generation`, if that is still the one in use.
fn schedule(ui: &ui::MainWindow, generation: u64, after: Duration) {
    let weak = ui.as_weak();
    RUNNING.with(|running| {
        if let Some(running) = running.borrow().as_ref().filter(|running| running.generation == generation) {
            running.timer.start(slint::TimerMode::SingleShot, after, move || {
                if let Some(ui) = weak.upgrade() {
                    due(&ui, generation);
                }
            });
        }
    });
}

/// What a look that is due may do, from what holds on the window's thread at that moment.
#[derive(Debug, PartialEq, Eq)]
enum Dispatch {
    /// Not the generation in use, or not the account published now: nothing for it.
    NotOurs,
    Offline,
    Busy,
    Go,
}

fn decide(current: bool, ours: bool, offline: bool, busy: bool) -> Dispatch {
    match () {
        _ if !current || !ours => Dispatch::NotOurs,
        _ if offline => Dispatch::Offline,
        _ if busy => Dispatch::Busy,
        _ => Dispatch::Go,
    }
}

/// A look is due. Dispatched here, on the window's thread, or not at all.
fn due(ui: &ui::MainWindow, generation: u64) {
    if let Some(ms) = test_var("NEXUS_TEST_SABOTAGE_UI_MS").and_then(|ms| ms.parse().ok()) {
        // A deliberate fault, to show that the tests catch one: the window's thread held.
        std::thread::sleep(Duration::from_millis(ms));
    }
    let Some((account, url)) = RUNNING.with(|running| {
        running.borrow().as_ref().filter(|running| running.generation == generation).and_then(|running| Some((running.account.clone(), running.url.clone()?)))
    }) else {
        return;
    };
    // The account published now, which a change of account may not yet have told us of.
    let ours = published(ui) == account || test_var("NEXUS_TEST_SABOTAGE_NO_GENERATION").is_some();
    let offline = offline() && test_var("NEXUS_TEST_SABOTAGE_NO_OFFLINE").is_none();
    match decide(true, ours, offline, IN_FLIGHT.load(Ordering::SeqCst)) {
        Dispatch::NotOurs => {
            note(format_args!("not-ours {generation}"));
            // The window has published another account: follow it now.
            follow(ui);
        }
        Dispatch::Offline => {
            note(format_args!("offline {generation}"));
            schedule(ui, generation, later());
        }
        Dispatch::Busy => schedule(ui, generation, test_ms("NEXUS_TEST_PROBE_AGAIN_MS").unwrap_or(BUSY)),
        Dispatch::Go => {
            IN_FLIGHT.store(true, Ordering::SeqCst);
            note(format_args!("dispatch {generation} offline=false"));
            let weak = ui.as_weak();
            let started = std::thread::Builder::new().name("nexus-probe".into()).spawn(move || {
                hold_for_tests();
                let outcome = supervise(&url, generation, deadline());
                IN_FLIGHT.store(false, Ordering::SeqCst);
                hold_result_for_tests(outcome);
                let _ = weak.upgrade_in_event_loop(move |ui| commit(&ui, generation, &account, outcome));
            });
            if started.is_err() {
                IN_FLIGHT.store(false, Ordering::SeqCst);
                schedule(ui, generation, later());
            }
        }
    }
}

/// An answer has come back: shown only if it is for the generation in use and the account
/// the window publishes now, and Offline mode is still off.
fn commit(ui: &ui::MainWindow, generation: u64, account: &str, outcome: Outcome) {
    let current = RUNNING.with(|running| running.borrow().as_ref().is_some_and(|running| running.generation == generation));
    // (A deliberate fault, to show that the test of a late answer catches it.)
    let sabotaged = test_var("NEXUS_TEST_SABOTAGE_NO_GENERATION").is_some();
    if !sabotaged && (!current || published(ui) != account) {
        note(format_args!("dropped {generation} {outcome:?}"));
        return;
    }
    if offline() && test_var("NEXUS_TEST_SABOTAGE_NO_OFFLINE").is_none() {
        note(format_args!("dropped-offline {generation} {outcome:?}"));
        schedule(ui, generation, later());
        return;
    }
    let settings = ui.global::<ui::Settings>();
    // The address the answer was asked at: that of the account it was for.
    let (server, name) = account.split_once('\n').unwrap_or_default();
    let url = signin::backend_for(server, name).and_then(|backend| proto::hello_url(&backend)).unwrap_or_default();
    settings.set_nexus_server(words(outcome, &url).into());
    // (With the fault above, the late answer is put under the account published now.)
    settings.set_nexus_for(if sabotaged { published(ui) } else { account.to_string() }.into());
    note(format_args!("shown {generation} {outcome:?}"));
    schedule(ui, generation, again());
    if outcome == Outcome::Found && !sabotaged {
        signin::after_look(ui);
    }
}

/// Whether Offline mode is switched on, as the controller's settings have it (the window's
/// switch is not told of it at a start). Read, never borrowed for long, and never waited for:
/// held elsewhere at this moment, it counts as on, and the look waits for the next turn.
fn offline() -> bool {
    crate::controller::with(|ctl| ctl.settings.try_borrow().map(|settings| settings.offline).unwrap_or(true)).unwrap_or(true)
}

/// What the row says.
fn words(outcome: Outcome, url: &str) -> String {
    match (outcome, url.strip_suffix(proto::HELLO)) {
        (Outcome::Found, Some(origin)) => format!("Found at {origin}{}", proto::ROOT),
        _ => NONE_FOUND.to_string(),
    }
}

/// The program a look is run from: this one, from the file it was started from, unchanged
/// since the component was installed. `None` if that cannot be said (the file replaced by an
/// update, or gone): then there is no look.
fn own_program() -> Option<PathBuf> {
    let (path, len, modified) = OWN.get()?.clone()?;
    let now = own_file()?;
    (now.0 == path && now.1 == len && now.2 == modified && !path.to_string_lossy().ends_with(" (deleted)")).then_some(path)
}

/// One look: this program run as `--nexus-probe <url>`, supervised.
fn supervise(url: &str, generation: u64, deadline: Duration) -> Outcome {
    let began = Instant::now();
    let Some(program) = own_program() else { return Outcome::NoAnswer };
    let mut command = Command::new(program);
    command.arg(PROBE_ARG).arg(url);
    run(command, generation, began, deadline)
}

/// Runs a look's process under a deadline counted from `began` (before it was started): its
/// exit status if it ended well within it; otherwise it is killed, reaped for at most
/// [`REAP`], and there was no answer. Never blocks: every wait is a poll.
fn run(mut command: Command, generation: u64, began: Instant, deadline: Duration) -> Outcome {
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window for it.
        command.creation_flags(0x0800_0000);
    }
    let Ok(child) = command.spawn() else { return Outcome::NoAnswer };
    note(format_args!("spawned {generation} {}", child.id()));
    *CHILD.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(child);
    let status = loop {
        let mut slot = CHILD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        // Ended by `shutdown`: the app is quitting.
        let Some(child) = slot.as_mut() else { return Outcome::NoAnswer };
        let id = child.id();
        let exited = child.try_wait();
        match exited {
            Ok(Some(status)) if began.elapsed() <= deadline => {
                slot.take();
                note(format_args!("ended {generation} {id} {:?}", status.code()));
                break status;
            }
            Ok(None) if began.elapsed() <= deadline => {}
            _ => {
                // Past the deadline (being started took some of it, or the answer came too
                // late), or it cannot be asked: killed, and there was no answer.
                let mut child = slot.take();
                drop(slot);
                if let Some(child) = child.as_mut() {
                    end(child);
                }
                note(format_args!("killed {generation} {id}"));
                return Outcome::NoAnswer;
            }
        }
        drop(slot);
        std::thread::sleep(Duration::from_millis(20));
    };
    read_status(status)
}

/// Kills a look's process and reaps it, a poll at a time, for at most [`REAP`].
fn end(child: &mut Child) {
    let _ = child.kill();
    let began = Instant::now();
    while began.elapsed() < REAP {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// What a look's process said by its exit status.
fn read_status(status: ExitStatus) -> Outcome {
    match status.code() {
        Some(FOUND) => Outcome::Found,
        Some(NO_BACKEND) => Outcome::NoBackend,
        _ => Outcome::NoAnswer,
    }
}

/// When the program was started as a look, the look itself and the code to exit with;
/// `None` when it was started as the app. `main` asks this first.
pub fn probe_mode() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if std::env::args_os().nth(1).is_some_and(|first| first == call::CALL_ARG) {
        args.next();
        OWN.get_or_init(own_file);
        return Some(call::in_process(args));
    }
    probe_mode_with(args, true)
}

fn probe_mode_with(mut args: impl Iterator<Item = OsString>, on: bool) -> Option<i32> {
    let first = args.next()?;
    if first != PROBE_ARG && first != "--nexus-probe-timed" {
        return None;
    }
    // Switched off, a look is not obeyed: the app starts as it would with any argument it
    // does not know.
    if !on {
        return None;
    }
    let url = args.next()?.into_string().ok()?;
    if args.next().is_some() {
        return Some(2);
    }
    if first == "--nexus-probe-timed" {
        if !cfg!(debug_assertions) {
            return Some(2);
        }
        // For the tests of each phase (tests/e13a/phases.py): one look, supervised as the
        // component supervises it, and how long it took.
        OWN.get_or_init(own_file);
        let began = Instant::now();
        let outcome = supervise(&url, 0, deadline());
        println!("{outcome:?} {}", began.elapsed().as_millis());
        return Some(0);
    }
    fds_for_tests();
    // For the tests only: the shipped build's panic (an abort), or a look that never ends.
    match test_var("NEXUS_TEST_PROBE_CHILD").as_deref() {
        Some("abort") => std::process::abort(),
        Some("stall") => loop {
            std::thread::park();
        },
        _ => {}
    }
    // Only ever an address this program makes.
    let origin = url.strip_suffix(proto::HELLO).unwrap_or_default();
    if proto::hello_url(origin).as_deref() != Some(url.as_str()) {
        return Some(2);
    }
    Some(match look(&url) {
        Some(proto::Found::Backend(_)) => FOUND,
        Some(proto::Found::NoBackend(_)) => NO_BACKEND,
        None => NO_ANSWER,
    })
}

/// Asks `hello` once, in the look's process. The status is classified before anything of
/// the body is opened: any status but 200 is "no backend" with the body never read.
fn look(url: &str) -> Option<proto::Found> {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build();
    let whole = test_var("NEXUS_TEST_PROBE_WHOLE_MS").and_then(|ms| ms.parse().ok()).map(Duration::from_millis);
    let agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .https_only(true)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .timeout_connect(Some(whole.unwrap_or(CONNECT)))
        .timeout_global(Some(whole.unwrap_or(WHOLE)))
        .max_idle_connections(0)
        .max_idle_connections_per_host(0)
        .user_agent(concat!("NexusMusic/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent();
    let mut response = agent.get(url).call().ok()?;
    let status = response.status().as_u16();
    if test_var("NEXUS_TEST_PROBE_BODY_FIRST").is_some() {
        // A deliberate fault, to show that tests/e13a/phases.py catches it: the body's reader
        // opened before the status is looked at, as W1's first version did.
        return Some(proto::classify(status, response.body_mut().as_reader()));
    }
    if status != 200 {
        return Some(proto::classify(status, std::io::empty()));
    }
    Some(proto::classify(status, response.body_mut().as_reader()))
}

// ---- for the tests of design 5.3 only, and only in a development build ----------------------

fn test_var(name: &str) -> Option<String> {
    if cfg!(debug_assertions) { std::env::var(name).ok() } else { None }
}

fn test_ms(name: &str) -> Option<Duration> {
    test_var(name).and_then(|ms| ms.parse().ok()).map(Duration::from_millis)
}

fn again() -> Duration {
    test_ms("NEXUS_TEST_PROBE_AGAIN_MS").unwrap_or(AGAIN)
}

fn later() -> Duration {
    test_ms("NEXUS_TEST_PROBE_AGAIN_MS").unwrap_or(LATER)
}

/// `NEXUS_TEST_DEADLINE_MS`: a longer deadline, so that one look held by the stand-in lasts
/// the whole of an action and the tests can show it was the same one at its end.
fn deadline() -> Duration {
    test_ms("NEXUS_TEST_DEADLINE_MS").unwrap_or(DEADLINE)
}

/// `NEXUS_TEST_TRIGGER=<file>`: when the file appears, it is taken away and a look is due at
/// once, so that the tests can have a look start just before an action.
fn trigger_for_tests(ui: &ui::MainWindow) {
    let Some(path) = test_var("NEXUS_TEST_TRIGGER") else { return };
    // One there already: a look in the first turn of the event loop, before the first frame.
    if std::fs::remove_file(&path).is_ok() {
        let weak = ui.as_weak();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade()
                && let Some(generation) = RUNNING.with(|running| running.borrow().as_ref().map(|running| running.generation))
            {
                due(&ui, generation);
            }
        });
    }
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_millis(100), move || {
        // (Taken only when there is an account to look for.)
        if let Some(generation) = RUNNING.with(|running| running.borrow().as_ref().map(|running| running.generation))
            && let Some(ui) = weak.upgrade()
            && std::fs::remove_file(&path).is_ok()
        {
            due(&ui, generation);
        }
    });
    TRIGGER.with(|slot| *slot.borrow_mut() = Some(timer));
}

/// `NEXUS_TEST_NEXUS_LOG=<file>`: what the component does, a line each, with the time.
fn note(what: std::fmt::Arguments<'_>) {
    if let Some(path) = test_var("NEXUS_TEST_NEXUS_LOG") {
        use std::io::Write;
        let now = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
        // One write for the whole line, so that lines from two threads never run together.
        let line = format!("{now:.3} {what}\n");
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = file.write_all(line.as_bytes());
        }
    }
}

/// `NEXUS_TEST_PROBE_FDS=<file>`: the look's process writes which descriptors it was given.
fn fds_for_tests() {
    if let Some(path) = test_var("NEXUS_TEST_PROBE_FDS") {
        let fds: Vec<String> = std::fs::read_dir("/proc/self/fd").into_iter().flatten().flatten().map(|entry| entry.file_name().to_string_lossy().into_owned()).collect();
        let _ = std::fs::write(path, fds.join(" "));
    }
}

/// `NEXUS_TEST_HOLD_NEXUS=<file>`: the look's thread is held in a call that never returns,
/// after writing which thread it is to the file.
fn hold_for_tests() {
    if let Some(path) = test_var("NEXUS_TEST_HOLD_NEXUS") {
        let thread = std::fs::read_link("/proc/thread-self").map(|link| link.display().to_string()).unwrap_or_default();
        let _ = std::fs::write(path, thread);
        loop {
            std::thread::park();
        }
    }
}

/// `NEXUS_TEST_HOLD_NEXUS_RESULT=<file>`: a look that found the backend waits, before its
/// answer is handed to the window, until the file exists (and says so in `<file>.waiting`).
fn hold_result_for_tests(outcome: Outcome) {
    if let (Some(path), Outcome::Found) = (test_var("NEXUS_TEST_HOLD_NEXUS_RESULT"), outcome) {
        let _ = std::fs::write(format!("{path}.waiting"), "");
        while std::fs::metadata(&path).is_err() {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_row_says_found_only_for_the_backend() {
        let url = proto::hello_url("https://music.example.org/navidrome").unwrap();
        assert_eq!(words(Outcome::Found, &url), "Found at https://music.example.org/nexus/");
        assert_eq!(words(Outcome::NoBackend, &url), NONE_FOUND);
        assert_eq!(words(Outcome::NoAnswer, &url), NONE_FOUND);
    }

    /// Dispatch is decided on the window's thread: never for an account no longer published,
    /// never in Offline mode, one at a time.
    #[test]
    fn a_look_is_dispatched_only_for_the_account_published_and_online() {
        assert_eq!(decide(true, true, false, false), Dispatch::Go);
        assert_eq!(decide(true, false, false, false), Dispatch::NotOurs);
        assert_eq!(decide(false, true, false, false), Dispatch::NotOurs);
        assert_eq!(decide(true, false, true, true), Dispatch::NotOurs);
        assert_eq!(decide(true, true, true, false), Dispatch::Offline);
        assert_eq!(decide(true, true, true, true), Dispatch::Offline);
        assert_eq!(decide(true, true, false, true), Dispatch::Busy);
    }

    /// Switched off, `--nexus-probe` is not obeyed: nothing is asked, and the app starts.
    #[test]
    fn switched_off_a_look_is_not_obeyed() {
        let args = |list: &[&str]| list.iter().map(OsString::from).collect::<Vec<_>>().into_iter();
        let url = "https://music.example.org/nexus/api/hello";
        assert_eq!(probe_mode_with(args(&[PROBE_ARG, url]), false), None);
        assert_eq!(probe_mode_with(args(&["--nexus-probe-timed", url]), false), None);
        assert_eq!(probe_mode_with(args(&[]), true), None);
        assert_eq!(probe_mode_with(args(&["--something"]), true), None);
        // Switched on, only exactly two arguments, and only an address this program makes.
        assert_eq!(probe_mode_with(args(&[PROBE_ARG, url, "more"]), true), Some(2));
        assert_eq!(probe_mode_with(args(&[PROBE_ARG, "https://music.example.org/elsewhere"]), true), Some(2));
        assert_eq!(probe_mode_with(args(&[PROBE_ARG, "http://music.example.org/nexus/api/hello"]), true), Some(2));
    }

    #[cfg(unix)]
    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    /// The look's process: its answer by its exit status alone, within the deadline, counted
    /// from before it was started; otherwise it is killed and there was no answer.
    #[cfg(unix)]
    #[test]
    fn a_look_is_bounded_and_contained() {
        let quick = Duration::from_secs(5);
        let now = Instant::now;
        assert_eq!(run(shell("exit 10"), 0, now(), quick), Outcome::Found);
        assert_eq!(run(shell("exit 11"), 0, now(), quick), Outcome::NoBackend);
        assert_eq!(run(shell("exit 12"), 0, now(), quick), Outcome::NoAnswer);
        assert_eq!(run(shell("exit 0"), 0, now(), quick), Outcome::NoAnswer);
        // A panic in the shipped build is an abort: that look, and nothing else, ends.
        assert_eq!(run(shell("kill -ABRT $$"), 0, now(), quick), Outcome::NoAnswer);
        // What it writes is not read: it goes nowhere.
        assert_eq!(run(shell("yes | head -c 10000000; exit 10"), 0, now(), quick), Outcome::Found);
        // A look that never ends is killed at the deadline, and so is one whose answer comes
        // after it; and a start that took the deadline up leaves no time for an answer.
        for script in ["sleep 30", "sleep 1; exit 10"] {
            let began = now();
            assert_eq!(run(shell(script), 0, began, Duration::from_millis(300)), Outcome::NoAnswer, "{script}");
            assert!(began.elapsed() < Duration::from_secs(3), "{script}: {:?}", began.elapsed());
        }
        let long_ago = now() - Duration::from_secs(20);
        assert_eq!(run(shell("exit 10"), 0, long_ago, Duration::from_secs(10)), Outcome::NoAnswer);
        // A program that cannot be started.
        assert_eq!(run(Command::new("/nonexistent/nexus-music"), 0, now(), quick), Outcome::NoAnswer);
        // A look ended by the app's end is no answer.
        let began = now();
        let looking = std::thread::spawn(move || run(shell("sleep 30"), 0, began, Duration::from_secs(30)));
        std::thread::sleep(Duration::from_millis(300));
        let child = CHILD.lock().unwrap().take();
        if let Some(mut child) = child {
            end(&mut child);
        }
        assert_eq!(looking.join().unwrap(), Outcome::NoAnswer);
        assert!(began.elapsed() < Duration::from_secs(5));
    }

    /// The paths of design 5.2 do not name the component (a check that is kept, and is not
    /// the proof: the proof is `tests/e13a/`, with the component's look held).
    #[test]
    fn nothing_that_plays_names_it() {
        let paths: &[(&str, &str)] = &[
            ("controller.rs", include_str!("../controller.rs")),
            ("config.rs", include_str!("../config.rs")),
            ("playback.rs", include_str!("../playback.rs")),
            ("plays.rs", include_str!("../plays.rs")),
            ("downloads.rs", include_str!("../downloads.rs")),
            ("offline.rs", include_str!("../offline.rs")),
            ("subsonic.rs", include_str!("../subsonic.rs")),
            ("sync.rs", include_str!("../sync.rs")),
            ("songcache.rs", include_str!("../songcache.rs")),
            ("media.rs", include_str!("../media.rs")),
            ("taskbar.rs", include_str!("../taskbar.rs")),
            ("audio/mod.rs", include_str!("../audio/mod.rs")),
            ("audio/engine.rs", include_str!("../audio/engine.rs")),
            ("audio/source.rs", include_str!("../audio/source.rs")),
            ("audio/dsp.rs", include_str!("../audio/dsp.rs")),
            ("audio/exclusive.rs", include_str!("../audio/exclusive.rs")),
        ];
        for (name, text) in paths {
            for word in ["nexus::", "crate::nexus", "super::nexus", "nexus_server", "nexus-server", "nexus_for", "nexus-for", "nexus_on", "nexus-on", "NEXUS_TEST_HOLD_NEXUS", "NEXUS_TEST_NEXUS", "NEXUS_TEST_PROBE", "NEXUS_TEST_SABOTAGE", "NEXUS_TEST_TRIGGER", "NEXUS_TEST_DEADLINE", "NEXUS_MUSIC_NEXUS"] {
                assert!(!text.contains(word), "{name} names the Nexus server's component ({word})");
            }
        }
        // `main` makes it after the controller (and a kept sign-in entered), and before the
        // window is shown; and the window tracks the account for it only once it is made.
        let main = include_str!("../main.rs");
        let (made, installed, shown) = (main.find("controller::Controller::install(window);"), main.find("nexus::install(&controller.ui);"), main.find("controller.ui.show()?;"));
        assert!(made.is_some() && installed > made && shown > installed, "main makes the component after the controller");
        assert!(include_str!("../../ui/app.slint").contains("property <string> account: App.nexus-on && App.signed-in ?"));
    }
}
