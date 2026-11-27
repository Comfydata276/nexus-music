//! The Nexus server's component: this app's side of the backend of our own that sits beside
//! the music server, under `/nexus/` at the music server's own origin (design 4, 5 and 6).
//!
//! **Off unless the app is started with `NEXUS_MUSIC_NEXUS=1`** (as linking is with
//! `NEXUS_MUSIC_LINK=1`), and kept off for everyone until the checks that only a real
//! Windows PC can run have been run (README, "Known, and accepted"). Off, nothing of it is
//! made: no timer, no thread, no process, no request, no row under Settings.
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
//! **How it asks: in a process of its own, with one deadline over all of it.** Each look runs
//! this same program again with `--nexus-probe <address>`, which makes one agent, sends one
//! `GET`, reads the answer and says `found <id>` or `none`, and exits. The component kills
//! that process if it has not finished within [`DEADLINE`], whatever it is doing: a name
//! look-up, a forward proxy, connecting, TLS and the certificate check (on Windows Schannel's
//! may fetch certificates of its own), the headers or the body. So:
//!
//! - a panic, which the shipped build turns into an abort (`panic = "abort"` in Cargo.toml),
//!   ends that look and never this process;
//! - nothing a look waits on outlives the deadline: the process, its threads (ureq's own for
//!   a timed name look-up among them), its connection and its pool go with it;
//! - one look is one request on the wire: a fresh process has no connection to reuse, and
//!   ureq sends again only by following a redirect, which is switched off.
//!
//! **Nothing that plays, queues, downloads or counts music waits for it** (design 5):
//!
//! - It is not made by the controller, and nothing on those paths names it (a test below
//!   reads their source). `main` installs it after the controller is made, and it starts for
//!   an account only after the window has published that account: the window's own
//!   `account-changed`, which fires after `controller::enter` (or `leave`) has returned.
//! - On the window's thread it does only this: reads three properties of the window and
//!   whether Offline mode is on (`try_borrow` of the settings: busy counts as on), arms a
//!   timer, starts a thread, and sets its own row. The thread
//!   waits for the process; nothing waits for the thread.
//! - When the account changes its timer is dropped at once, and a look still running is let
//!   go of (never waited for). Each look carries the generation of the account it was asked
//!   for, and its answer is shown only if, on the window's thread when it arrives, that is
//!   still the generation in use and the window still publishes that account. The row itself
//!   is shown only while the account it was set for is the one published.
//! - It reaches the rest of the app through one seam: the row under Settings
//!   (`Settings.nexus-server`, for `Settings.nexus-for`), which it sets and nothing else reads.

use std::cell::{Cell, RefCell};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use nexus_core::nexus::proto;
use slint::ComponentHandle;

use crate::ui;

/// The switch: the component is made only when this is `1`.
const SWITCH: &str = "NEXUS_MUSIC_NEXUS";
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
/// The one deadline over everything a look does, from starting its process to its exit.
pub const DEADLINE: Duration = Duration::from_secs(10);
/// Inside the look's process, ureq's own limits, so that it normally ends by itself first.
const CONNECT: Duration = Duration::from_secs(5);
const WHOLE: Duration = Duration::from_secs(8);

/// What the row under Settings says.
const NONE_FOUND: &str = "None found";

/// Whether the component is switched on for this run of the app.
pub fn switched_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var(SWITCH).is_ok_and(|value| value == "1"))
}

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
}

/// Looks running now, of any account. One at a time.
static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

/// Installs the component on `ui`, if it is switched on: once the event loop runs (for an
/// account the controller published while it was being made), and each time the window says
/// the account has changed. Called by `main` after the controller is made; never by it.
pub fn install(ui: &ui::MainWindow) {
    if !switched_on() {
        return;
    }
    let weak = ui.as_weak();
    ui.global::<ui::App>().on_account_changed(move || {
        if let Some(ui) = weak.upgrade() {
            follow(&ui);
        }
    });
    let weak = ui.as_weak();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            follow(&ui);
        }
    });
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
    // Let go of the old: its timer stops with it, and a look of its still running is dropped
    // when it answers (its generation is no longer the one in use).
    let old = RUNNING.with(|running| running.borrow_mut().take());
    drop(old);
    let settings = ui.global::<ui::Settings>();
    settings.set_nexus_server("".into());
    settings.set_nexus_for("".into());
    if account.is_empty() {
        return;
    }
    let generation = GENERATION.with(|counter| {
        counter.set(counter.get() + 1);
        counter.get()
    });
    let server = account.split('\n').next().unwrap_or_default();
    let url = proto::hello_url(server);
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

/// A look is due: started on a thread of its own, unless Offline mode is on or another look
/// is still running.
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
    // (A deliberate fault, to show that the test of Offline mode catches it.)
    if offline() && test_var("NEXUS_TEST_SABOTAGE_NO_OFFLINE").is_none() {
        note(format_args!("offline {generation}"));
        schedule(ui, generation, later());
        return;
    }
    if IN_FLIGHT.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        schedule(ui, generation, BUSY);
        return;
    }
    let weak = ui.as_weak();
    let started = std::thread::Builder::new().name("nexus-probe".into()).spawn(move || {
        hold_for_tests();
        note(format_args!("start {generation}"));
        let outcome = probe(&url);
        note(format_args!("end {generation} {outcome:?}"));
        IN_FLIGHT.store(0, Ordering::SeqCst);
        hold_result_for_tests(outcome);
        let _ = weak.upgrade_in_event_loop(move |ui| commit(&ui, generation, &account, outcome));
    });
    if started.is_err() {
        IN_FLIGHT.store(0, Ordering::SeqCst);
        schedule(ui, generation, later());
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
    let server = account.split('\n').next().unwrap_or_default();
    let url = proto::hello_url(server).unwrap_or_default();
    settings.set_nexus_server(words(outcome, &url).into());
    // (With the fault above, the late answer is put under the account published now.)
    settings.set_nexus_for(if sabotaged { published(ui) } else { account.to_string() }.into());
    note(format_args!("shown {generation} {outcome:?}"));
    schedule(ui, generation, again());
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

/// One look: this program run as `--nexus-probe <url>`, killed at [`DEADLINE`].
fn probe(url: &str) -> Outcome {
    let Ok(program) = std::env::current_exe() else { return Outcome::NoAnswer };
    let mut command = Command::new(program);
    command.arg(PROBE_ARG).arg(url);
    run(command, DEADLINE)
}

/// Runs a look's process: its answer if it ended well by `deadline`; otherwise it is killed
/// and there was no answer. A process that aborts (a panic, in the shipped build) or exits
/// otherwise than well is no answer too.
fn run(mut command: Command, deadline: Duration) -> Outcome {
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // No console window for it.
        command.creation_flags(0x0800_0000);
    }
    let began = Instant::now();
    let Ok(mut child) = command.spawn() else { return Outcome::NoAnswer };
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if began.elapsed() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Outcome::NoAnswer;
            }
        }
    };
    let mut said = Vec::new();
    if let Some(out) = child.stdout.take() {
        let _ = out.take(256).read_to_end(&mut said);
    }
    if !status.success() {
        return Outcome::NoAnswer;
    }
    read_said(&said)
}

/// What a look's process said: `found <id>` or `none`, on one line.
fn read_said(said: &[u8]) -> Outcome {
    let Ok(text) = std::str::from_utf8(said) else { return Outcome::NoAnswer };
    match text.strip_suffix('\n').unwrap_or(text).split_once(' ') {
        Some(("found", id)) if proto::BackendId::from_hex(id).is_some() => Outcome::Found,
        None if text.trim_end() == "none" => Outcome::NoBackend,
        _ => Outcome::NoAnswer,
    }
}

/// When the program was started as a look (`--nexus-probe <address>`), the look itself, and
/// the code to exit with; `None` when it was started as the app. `main` asks this first.
pub fn probe_mode() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    let first = args.next()?;
    if cfg!(debug_assertions) && first == "--nexus-probe-timed" {
        // For the tests of each phase (tests/e13a/phases.py): one look, as the component
        // makes it, with how long it took.
        let url = args.next()?.into_string().ok()?;
        let began = Instant::now();
        let outcome = probe(&url);
        println!("{outcome:?} {}", began.elapsed().as_millis());
        return Some(0);
    }
    if first != PROBE_ARG {
        return None;
    }
    let url = args.next()?.into_string().ok()?;
    if args.next().is_some() {
        return Some(2);
    }
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
    use std::io::Write;
    let said = match look(&url) {
        Some(proto::Found::Backend(hello)) => format!("found {}\n", hello.id.to_hex()),
        Some(proto::Found::NoBackend(_)) => "none\n".to_string(),
        // Nothing answered: nothing is said, and the look ends otherwise than well.
        None => return Some(1),
    };
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(said.as_bytes()).and_then(|_| out.flush());
    Some(0)
}

/// Asks `hello` once, in the look's process. The status is classified before anything of
/// the body is opened: any status but 200 is "no backend" with the body never read.
fn look(url: &str) -> Option<proto::Found> {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build();
    let agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .https_only(true)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT))
        .timeout_global(Some(WHOLE))
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

fn again() -> Duration {
    test_var("NEXUS_TEST_PROBE_AGAIN_MS").and_then(|ms| ms.parse().ok()).map(Duration::from_millis).unwrap_or(AGAIN)
}

fn later() -> Duration {
    test_var("NEXUS_TEST_PROBE_AGAIN_MS").and_then(|ms| ms.parse().ok()).map(Duration::from_millis).unwrap_or(LATER)
}

/// `NEXUS_TEST_NEXUS_LOG=<file>`: what the component does, a line each, with the time.
fn note(what: std::fmt::Arguments<'_>) {
    if let Some(path) = test_var("NEXUS_TEST_NEXUS_LOG") {
        use std::io::Write;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64();
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(file, "{now:.3} {what}");
        }
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

    #[test]
    fn a_looks_words_are_read_strictly() {
        assert_eq!(read_said(b"found 5c0ffee05c0ffee05c0ffee05c0ffee0\n"), Outcome::Found);
        assert_eq!(read_said(b"none\n"), Outcome::NoBackend);
        for said in [&b""[..], b"found\n", b"found 5C0FFEE05C0FFEE05C0FFEE05C0FFEE0\n", b"found zz\n", b"nothing\n", b"none more\n", b"\xff\n"] {
            assert_eq!(read_said(said), Outcome::NoAnswer, "{said:?}");
        }
    }

    #[cfg(unix)]
    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.arg("-c").arg(script);
        command
    }

    /// The process a look runs in: its answer when it ends well; otherwise no answer, by the
    /// deadline at the latest, the process killed.
    #[cfg(unix)]
    #[test]
    fn a_look_is_bounded_and_contained() {
        let quick = Duration::from_secs(5);
        assert_eq!(run(shell("echo found 5c0ffee05c0ffee05c0ffee05c0ffee0"), quick), Outcome::Found);
        assert_eq!(run(shell("echo none"), quick), Outcome::NoBackend);
        // A panic in the shipped build is an abort: that look, and nothing else, ends.
        assert_eq!(run(shell("echo found 5c0ffee05c0ffee05c0ffee05c0ffee0; kill -ABRT $$"), quick), Outcome::NoAnswer);
        assert_eq!(run(shell("exit 3"), quick), Outcome::NoAnswer);
        // A look that never ends, or never stops talking, is killed at the deadline.
        for script in ["sleep 30", "yes"] {
            let began = Instant::now();
            assert_eq!(run(shell(script), Duration::from_millis(300)), Outcome::NoAnswer);
            assert!(began.elapsed() < Duration::from_secs(2), "{script}: {:?}", began.elapsed());
        }
        // A program that cannot be started.
        assert_eq!(run(Command::new("/nonexistent/nexus-music"), quick), Outcome::NoAnswer);
    }

    /// The paths of design 5.2 do not name the component (a check that is kept, and is not
    /// the proof: the proof is `tests/e13a/`, with the component's look held).
    #[test]
    fn nothing_that_plays_names_it() {
        let paths: &[(&str, &str)] = &[
            ("controller.rs", include_str!("controller.rs")),
            ("config.rs", include_str!("config.rs")),
            ("playback.rs", include_str!("playback.rs")),
            ("plays.rs", include_str!("plays.rs")),
            ("downloads.rs", include_str!("downloads.rs")),
            ("offline.rs", include_str!("offline.rs")),
            ("subsonic.rs", include_str!("subsonic.rs")),
            ("sync.rs", include_str!("sync.rs")),
            ("songcache.rs", include_str!("songcache.rs")),
            ("media.rs", include_str!("media.rs")),
            ("taskbar.rs", include_str!("taskbar.rs")),
            ("audio/mod.rs", include_str!("audio/mod.rs")),
            ("audio/engine.rs", include_str!("audio/engine.rs")),
            ("audio/source.rs", include_str!("audio/source.rs")),
            ("audio/dsp.rs", include_str!("audio/dsp.rs")),
            ("audio/exclusive.rs", include_str!("audio/exclusive.rs")),
        ];
        for (name, text) in paths {
            for word in ["nexus::", "crate::nexus", "super::nexus", "nexus_server", "nexus-server", "nexus_for", "nexus-for", "NEXUS_TEST_HOLD_NEXUS", "NEXUS_TEST_NEXUS", "NEXUS_TEST_PROBE", "NEXUS_TEST_SABOTAGE", "NEXUS_MUSIC_NEXUS"] {
                assert!(!text.contains(word), "{name} names the Nexus server's component ({word})");
            }
        }
        // And `main` makes it only when it is switched on.
        let main = include_str!("main.rs");
        assert!(main.contains("if nexus::switched_on() {\n        nexus::install(&controller.ui);"));
    }
}
