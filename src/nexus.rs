//! The Nexus server's component: this app's side of the backend of our own that sits beside
//! the music server, under `/nexus/` at the music server's own origin (design 4, 5 and 6).
//!
//! At this stage (slice W1) it is an empty shell, and does one thing: it asks the backend's
//! `hello` once when an account is published and then hourly, and says under Settings whether
//! there is a Nexus server at that address. It reads, sends and keeps nothing else: no
//! sign-in, no plays, no file.
//!
//! **Nothing that plays, queues, downloads or counts music waits for it** (design 5). That is
//! held by how it is made, and proven by the tests of design 5.3 (`tests/e13a/`), run with its
//! worker held in a call that never returns:
//!
//! - It is not made by the controller, and nothing on those paths names it (a test below
//!   reads their source). `main` installs it once the controller has been made, and it starts
//!   for an account only after the window has published that account: the window's own
//!   `account-changed`, which fires after `controller::enter` (or `leave`) has returned.
//! - It has a thread of its own for each account, an HTTP agent of its own (HTTPS only, no
//!   redirect followed, its own short timeouts, its own connections and name look-ups), and
//!   nothing shared with the app's own agents, pools or files.
//! - When the account changes it is let go of at once, without waiting for anything: its
//!   thread is told to stop and is never joined. Whatever it still brings back is dropped,
//!   because it is checked against the account it was started for, on the window's thread.
//! - It reaches the rest of the app through one seam: the row under Settings
//!   (`Settings.nexus-server`), which it sets and nothing else reads.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use nexus_core::nexus::proto;
use slint::ComponentHandle;

use crate::ui;

/// How often `hello` is asked again while an account is in use.
const AGAIN: Duration = Duration::from_secs(3600);
/// Its own timeouts: to connect, and for the whole of one `hello` from the first byte sent to
/// the last read. A real `hello` is a few hundred bytes on a connection to the music server's
/// own proxy; anything slower than this is not a backend worth showing.
const CONNECT: Duration = Duration::from_secs(5);
const WHOLE: Duration = Duration::from_secs(10);

/// What the row under Settings says.
const NONE_FOUND: &str = "None found";

/// The component for the account in use, if one is: held on the window's thread only.
struct Running {
    /// The account it was started for, as the window published it.
    account: String,
    /// Raised when it is let go of: whatever its thread brings back after this is dropped.
    stop: Arc<AtomicBool>,
    /// Dropped with it, which wakes its thread from waiting for the next hour.
    _wake: mpsc::Sender<()>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

thread_local! {
    static RUNNING: RefCell<Option<Running>> = const { RefCell::new(None) };
}

/// Installs the component on `ui`: once now, for an account the controller has already
/// published (a cold start with a saved sign-in), and again each time the window says the
/// account has changed. Called by `main` after the controller has been made; never by it.
pub fn install(ui: &ui::MainWindow) {
    let weak = ui.as_weak();
    ui.global::<ui::App>().on_account_changed(move || {
        if let Some(ui) = weak.upgrade() {
            follow(&ui);
        }
    });
    // Not inside `install` itself: once the event loop is running, and the window is up.
    let weak = ui.as_weak();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            follow(&ui);
        }
    });
}

/// The account as the window has it: empty when nobody is signed in.
fn published(ui: &ui::MainWindow) -> (String, String) {
    let app = ui.global::<ui::App>();
    if !app.get_signed_in() {
        return (String::new(), String::new());
    }
    let server = app.get_server().to_string();
    (format!("{server}\n{}", app.get_username()), server)
}

/// Lets go of the component of an account that is no longer the one in use, and starts one
/// for the account that is. Never waits for anything.
fn follow(ui: &ui::MainWindow) {
    let (account, server) = published(ui);
    let same = RUNNING.with(|running| running.borrow().as_ref().is_some_and(|running| running.account == account));
    if same {
        return;
    }
    // Let go of first: dropping it raises its flag and wakes its thread, and nothing waits.
    let old = RUNNING.with(|running| running.borrow_mut().take());
    drop(old);
    ui.global::<ui::Settings>().set_nexus_server("".into());
    if account.is_empty() {
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let (wake, woken) = mpsc::channel::<()>();
    let thread_stop = stop.clone();
    let weak = ui.as_weak();
    let started = std::thread::Builder::new().name("nexus".into()).spawn(move || run(&server, &thread_stop, &woken, weak));
    if started.is_ok() {
        RUNNING.with(|running| *running.borrow_mut() = Some(Running { account, stop, _wake: wake }));
    }
}

/// The component's thread for one account: `hello` now and hourly, until it is let go of.
fn run(server: &str, stop: &Arc<AtomicBool>, woken: &mpsc::Receiver<()>, ui: slint::Weak<ui::MainWindow>) {
    let address = hello_address(server);
    let agent = address.as_ref().map(|_| agent());
    loop {
        let shown = match (&address, &agent) {
            (Some(address), Some(agent)) => words(&look(agent, address), address),
            // Not HTTPS, or not an address it can make `hello`'s from: there is none.
            _ => NONE_FOUND.to_string(),
        };
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let flag = stop.clone();
        let _ = ui.upgrade_in_event_loop(move |ui| {
            // Checked here, on the window's thread, where it is let go of: an answer that
            // arrives after another account was published is dropped.
            if !flag.load(Ordering::SeqCst) {
                ui.global::<ui::Settings>().set_nexus_server(shown.into());
            }
        });
        match woken.recv_timeout(AGAIN) {
            Err(mpsc::RecvTimeoutError::Timeout) if !stop.load(Ordering::SeqCst) => continue,
            _ => return,
        }
    }
}

/// `https://<the music server's host and port>/nexus/api/hello`, from the address the account
/// was published with, whatever path the music server itself is under (design 6.1). `None`
/// for anything but HTTPS, and for an address with a name and password in it.
fn hello_address(server: &str) -> Option<String> {
    let rest = server.strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    Some(format!("https://{authority}{}", proto::HELLO))
}

/// An agent of the component's own: HTTPS only, no redirect followed (a redirect is an answer,
/// never a place to go), short timeouts of its own, and its own connections and look-ups.
fn agent() -> ureq::Agent {
    use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
    let tls = TlsConfig::builder().provider(TlsProvider::NativeTls).root_certs(RootCerts::PlatformVerifier).build();
    ureq::Agent::config_builder()
        .tls_config(tls)
        .https_only(true)
        .max_redirects(0)
        .max_redirects_will_error(false)
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT))
        .timeout_global(Some(WHOLE))
        .max_idle_connections(1)
        .max_idle_connections_per_host(1)
        .user_agent(concat!("NexusMusic/", env!("CARGO_PKG_VERSION")))
        .build()
        .new_agent()
}

/// Asks `hello` once: what the answer says is there, or `None` when nothing answered (no
/// connection, a timeout, TLS refused). The transport's own reader for the body is handed to
/// the shared library, which reads it to its end or one byte past the most a `hello` can be.
fn look(agent: &ureq::Agent, address: &str) -> Option<proto::Found> {
    hold_for_tests();
    let mut response = agent.get(address).call().ok()?;
    let status = response.status().as_u16();
    Some(proto::classify(status, response.body_mut().as_reader()))
}

/// What the row says for what was found. Nothing answering is "none found" too.
fn words(found: &Option<proto::Found>, address: &str) -> String {
    match found {
        Some(proto::Found::Backend(_)) => {
            let root = address.strip_suffix(proto::HELLO).map(|origin| format!("{origin}{}", proto::ROOT)).unwrap_or_default();
            format!("Found at {root}")
        }
        Some(proto::Found::NoBackend(_)) | None => NONE_FOUND.to_string(),
    }
}

/// For the tests of design 5.3 only, in a development build: `NEXUS_TEST_HOLD_NEXUS` holds the
/// component's worker in a call that never returns, as a backend that never lets go would.
fn hold_for_tests() {
    if cfg!(debug_assertions) && std::env::var_os("NEXUS_TEST_HOLD_NEXUS").is_some() {
        loop {
            std::thread::park();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_is_asked_at_the_origin_of_an_https_address_only() {
        assert_eq!(hello_address("https://music.example.org").as_deref(), Some("https://music.example.org/nexus/api/hello"));
        assert_eq!(hello_address("https://music.example.org:4443/navidrome").as_deref(), Some("https://music.example.org:4443/nexus/api/hello"));
        assert_eq!(hello_address("https://[2001:db8::1]:4443").as_deref(), Some("https://[2001:db8::1]:4443/nexus/api/hello"));
        assert_eq!(hello_address("http://music.example.org"), None);
        assert_eq!(hello_address("https://"), None);
        assert_eq!(hello_address("https://name:secret@music.example.org"), None);
        assert_eq!(proto::plain(proto::HELLO), Ok(()));
    }

    #[test]
    fn the_row_says_found_only_for_the_backend() {
        let address = "https://music.example.org/nexus/api/hello";
        let hello = br#"{"nexus":"0.1.0","id":"5c0ffee05c0ffee05c0ffee05c0ffee0","speaks":[1,1],"has":[]}"#;
        let found = |status, body: &[u8]| Some(proto::classify(status, body));
        assert_eq!(words(&found(200, hello), address), "Found at https://music.example.org/nexus/");
        // What Navidrome answers there, a proxy's errors, a body cut short, nothing at all.
        assert_eq!(words(&found(302, b""), address), NONE_FOUND);
        assert_eq!(words(&found(405, b""), address), NONE_FOUND);
        assert_eq!(words(&found(502, b"<html>"), address), NONE_FOUND);
        assert_eq!(words(&found(200, &hello[..40]), address), NONE_FOUND);
        assert_eq!(words(&None, address), NONE_FOUND);
    }

    /// The paths of design 5.2 do not name the component (a check that is kept, and is not
    /// the proof: the proof is `tests/e13a/`, with the component's worker held).
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
            for word in ["nexus::", "crate::nexus", "super::nexus", "nexus_server", "nexus-server", "NEXUS_TEST_HOLD"] {
                assert!(!text.contains(word), "{name} names the Nexus server's component ({word})");
            }
        }
    }
}
