//! Signing in to the Nexus server, and everything this app keeps of it (design 8, 9.4, 6.3,
//! 11; slice W7): **this app's side of the shared library's client**
//! (`nexus_core::nexus::client`), which decides all of it. This file drives it by the same
//! four calls, in the same order, as the phone does (design 5.5): `start` with what was kept;
//! `happened` whenever something does (a press, an answer, a failure, Offline, a tick, the
//! music server refusing the login, the backend found again); then **do** each thing it
//! gives back, which is one of five: ask, keep, forget, show, hand; and `stop` at the app's
//! end. Nothing here decides: what is asked, when, what an answer means, what is kept and
//! forgotten, what a person is told, and the rule for a refused music login are the core's.
//! What is here is what is this app's by nature:
//!
//! - **The transport** (`call`): each ask sent once, in a process of its own under the
//!   component's deadline, HTTPS only, to the backend's own paths only, nothing followed.
//! - **Where secrets are kept** (`config::write_kept`: Windows' Credential Manager). **What
//!   the core says to keep and forget after one thing that happened is put in place as one
//!   item, in one step** (`config::SignedIn`: the session, the music login with its number,
//!   the account, and the folder the person's things are under), so that a write cut short
//!   leaves the one before it whole (design 11). This device's own secret is kept beside it,
//!   and a sign-out the backend was not yet told of is kept beside it too (the core's
//!   `ToTell`), to end that session and nothing else. **The password typed is kept nowhere:**
//!   it goes from the field to the core as `Typed` (wiped when let go of), into the ask's
//!   request, through the ask's own process, once, over HTTPS, to the address typed; the
//!   window empties the field when it is sent.
//! - **The person's folder** (design 11): decided once, when a sign-in first leaves both an
//!   account and a music login (`config::folder_for`), by who they are to the backend, with
//!   the narrow carry-over of the folder this PC had before.
//! - **The window**: the sign-in screen, choosing a password, the Account rows, the list of
//!   devices, changing the password; and the player, handed a music login (the controller's
//!   `signed_in_with`, `music_login_renewed`, `music_set_aside`, `sign_out`).
//!
//! **Playing never waits for any of this** (design 6): a cold start is the controller's, from
//! what is kept on the disk, before this is started; the core is started after the window's
//! first turn, and everything it asks goes out on threads and in processes of its own.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use nexus_core::nexus::account::{self, DeviceKind, DeviceSecret, MusicAddress, MusicSalt, MusicToken, Name, Session};
use nexus_core::nexus::client::{
    self as core, AccountFile, AnswerBody, App, Ask, Client, Do, Failure, Forget, Given, Hand, Happened, Keep, Kept, ListState, Method, PasswordState, Press, Random, Screen,
    Shown, SignInState, ToTell, Typed,
};
use nexus_core::nexus::proto::Version;
use nexus_core::origin::{Address, HandoverSource, MusicBase};
use slint::{ComponentHandle, ModelRc, VecModel};
use zeroize::Zeroizing;

use super::{call, note};
use crate::config::{self, SignedIn};
use crate::ui;

/// Where this device's own secret is kept.
const DEVICE: &str = "nexus-device";
/// Where a sign-out the backend was not yet told of is kept.
const TO_TELL: &str = "nexus-sign-out-note";
/// The core is started this long after the window's first turn, so that nothing it asks is
/// on the way to the first page (as the component's look was).
const START_AFTER: Duration = Duration::from_secs(5);

thread_local! {
    static CLIENT: RefCell<Client> = RefCell::new(Client::default());
    static STARTED: Cell<bool> = const { Cell::new(false) };
    static OFFLINE: Cell<bool> = const { Cell::new(false) };
    static TIMERS: RefCell<Vec<slint::Timer>> = const { RefCell::new(Vec::new()) };
    /// The notice of what this server records, as last shown: shown once.
    static NOTICE: RefCell<String> = const { RefCell::new(String::new()) };
    /// The words about the music login, as last said.
    static MUSIC_WORDS: RefCell<Option<String>> = const { RefCell::new(None) };
}

fn paths() -> config::Paths {
    crate::controller::with(|ctl| ctl.paths.clone()).unwrap_or_else(config::Paths::new)
}

fn offline_now() -> bool {
    super::offline()
}

fn given() -> Given {
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|time| time.as_millis() as u64).unwrap_or_default();
    let local_offset_minutes = chrono::Local::now().offset().local_minus_utc() / 60;
    let mut bytes = Zeroizing::new([0u8; 32]);
    let random = getrandom::fill(&mut *bytes).ok().map(|()| Random::new(*bytes));
    Given { now_ms, local_offset_minutes, random }
}

fn app() -> App {
    let chosen = crate::controller::with(|ctl| ctl.settings.borrow().device_name.trim().to_string()).unwrap_or_default();
    let computer = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_default();
    let device_name = [chosen.as_str(), computer.trim(), "Windows PC"].into_iter().find_map(Name::new).unwrap_or_else(|| Name::new("PC").expect("a name"));
    App { version: Version::parse(env!("CARGO_PKG_VERSION")).expect("the app's own version"), kind: DeviceKind::Pc, device_name }
}

// ---- what is kept, read back --------------------------------------------------------------------

/// The core's music login from the one kept.
fn core_music(music: &config::MusicLogin) -> Option<account::MusicLogin> {
    Some(account::MusicLogin {
        address: MusicAddress::new(&music.server)?,
        name: Name::new(&music.username)?,
        salt: MusicSalt::new(&music.salt)?,
        token: MusicToken::from_hex(&music.token)?,
        count: music.count,
    })
}

fn kept_music(music: &account::MusicLogin) -> config::MusicLogin {
    config::MusicLogin {
        server: music.address.as_str().to_string(),
        username: music.name.as_str().to_string(),
        salt: music.salt.expose().to_string(),
        token: music.token.expose_hex().to_string(),
        count: music.count,
    }
}

/// What the core is started with: everything kept, read back.
fn read_kept(paths: &config::Paths) -> Kept {
    let record = config::load_sign_in(paths).unwrap_or_default();
    let device_secret = config::read_kept(paths, DEVICE).ok().flatten().and_then(|hex| DeviceSecret::from_hex(hex.trim()));
    let to_tell = config::read_kept(paths, TO_TELL).ok().flatten().and_then(|text| {
        let value: serde_json::Value = serde_json::from_str(&text).ok()?;
        Some(ToTell { session: Session::from_hex(value.get("session")?.as_str()?)?, backend: value.get("backend")?.as_str()?.to_string(), restore: value.get("restore")?.as_u64()? })
    });
    Kept {
        device_secret,
        session: record.session.as_deref().and_then(Session::from_hex),
        music: record.music.as_ref().and_then(core_music),
        account: record.account.and_then(|value| serde_json::from_value::<AccountFile>(value).ok()),
        to_tell,
        offline: offline_now(),
    }
}

/// The Nexus origin that the account published now was signed in through: where the
/// component's look asks `hello`. `None` when the sign-in kept is not for that account.
pub fn backend_for(server: &str, name: &str) -> Option<String> {
    let record = config::load_sign_in(&paths())?;
    let music = record.music.as_ref()?;
    let account: AccountFile = serde_json::from_value(record.account?).ok()?;
    (crate::subsonic::normalize_server_url(&music.server) == server && music.username == name).then_some(account.backend)
}

// ---- installed with the component ---------------------------------------------------------------

/// Takes over the window's sign-in, sign-out, devices and password callbacks, and starts the
/// core after the window's first turn.
pub fn install(ui: &ui::MainWindow) {
    let app_global = ui.global::<ui::App>();
    let weak = ui.as_weak();
    app_global.on_sign_in(move |address, email, password| {
        let press = Press::SignIn { address: address.to_string(), email: email.to_string(), password: Typed::new(&password) };
        press_on(&weak, press);
    });
    let weak = ui.as_weak();
    app_global.on_sign_in_code(move |address, code| press_on(&weak, Press::SignInWithCode { address: address.to_string(), code: Typed::new(&code) }));
    let weak = ui.as_weak();
    app_global.on_choose_password(move |new, again| press_on(&weak, Press::ChoosePassword { password: Typed::new(&new), again: Typed::new(&again) }));
    // Leaving the choice of a password is signing out of the session that a code gave.
    let weak = ui.as_weak();
    app_global.on_cancel_choose(move || press_on(&weak, Press::SignOut));
    let weak = ui.as_weak();
    app_global.on_sign_out(move || press_on(&weak, Press::SignOut));
    let weak = ui.as_weak();
    app_global.on_music_login_refused(move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some(count) = config::load_sign_in(&paths()).and_then(|record| record.music.map(|music| music.count)) else { return };
        happened(&ui, Happened::MusicRefused { count });
    });
    let settings = ui.global::<ui::Settings>();
    let weak = ui.as_weak();
    settings.on_load_devices(move || press_on(&weak, Press::OpenDevices));
    let weak = ui.as_weak();
    settings.on_force_out(move |id| {
        if let Ok(device) = id.parse() {
            press_on(&weak, Press::ForceOut { device });
        }
    });
    let weak = ui.as_weak();
    settings.on_change_password(move |current, new, again| {
        press_on(&weak, Press::ChangePassword { current: Typed::new(&current), new: Typed::new(&new), again: Typed::new(&again) });
    });
    // What the last sign-in left, on the sign-in screen at once (nothing is asked for it).
    let (address, email) = crate::controller::with(|ctl| {
        let settings = ctl.settings.borrow();
        (settings.sign_in_address.clone(), settings.sign_in_email.clone())
    })
    .unwrap_or_default();
    app_global.set_nexus_address(address.as_str().into());
    app_global.set_nexus_email(email.as_str().into());
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::SingleShot, super::test_ms("NEXUS_TEST_START_MS").unwrap_or(START_AFTER), move || {
        if let Some(ui) = weak.upgrade() {
            start(&ui);
        }
    });
    TIMERS.with(|timers| timers.borrow_mut().push(timer));
}

/// `start`, once: with what was kept. Then a tick, and Offline watched.
fn start(ui: &ui::MainWindow) {
    if STARTED.with(|started| started.replace(true)) {
        return;
    }
    let kept = read_kept(&paths());
    OFFLINE.with(|offline| offline.set(kept.offline));
    note(format_args!("client started"));
    let dos = CLIENT.with(|client| client.borrow_mut().start(app(), kept, &given()));
    run(ui, dos);
    let weak = ui.as_weak();
    let tick = slint::Timer::default();
    tick.start(slint::TimerMode::Repeated, Duration::from_millis(core::TICK_MS), move || {
        if let Some(ui) = weak.upgrade() {
            happened(&ui, Happened::Tick);
        }
    });
    // Offline mode switched by the person, told as soon as it is seen.
    let weak = ui.as_weak();
    let watch = slint::Timer::default();
    watch.start(slint::TimerMode::Repeated, Duration::from_millis(500), move || {
        let now = offline_now();
        if OFFLINE.with(|offline| offline.replace(now)) != now
            && let Some(ui) = weak.upgrade()
        {
            happened(&ui, Happened::Offline(now));
        }
    });
    TIMERS.with(|timers| timers.borrow_mut().extend([tick, watch]));
}

/// The app's end: nothing more is acted on.
pub fn stop() {
    CLIENT.with(|client| client.borrow_mut().stop());
}

fn press_on(weak: &slint::Weak<ui::MainWindow>, press: Press) {
    if let Some(ui) = weak.upgrade() {
        // A press before the core has started starts it first.
        start(&ui);
        happened(&ui, Happened::Pressed(press));
    }
}

/// Tells the core, and does what it says.
fn happened(ui: &ui::MainWindow, event: Happened) {
    if !STARTED.with(Cell::get) {
        return;
    }
    let dos = CLIENT.with(|client| client.borrow_mut().happened(event, &given()));
    run(ui, dos);
}

/// After each look that found the backend: it may be reachable again (what was owed is
/// asked, by the core's rule).
pub fn after_look(ui: &ui::MainWindow) {
    happened(ui, Happened::Network { up: true });
}

// ---- doing what the core says -------------------------------------------------------------------

/// Does each thing, in order, except that every keep and forget among them is put in place
/// first, together, as one item in one step.
fn run(ui: &ui::MainWindow, dos: Vec<Do>) {
    let paths = paths();
    if dos.iter().any(|thing| matches!(thing, Do::Keep(_) | Do::Forget(_))) {
        let mut record = config::load_sign_in(&paths).unwrap_or_default();
        let before = record.clone();
        for thing in &dos {
            match thing {
                Do::Keep(Keep::DeviceSecret(secret)) => {
                    config::write_kept(&paths, DEVICE, &secret.expose_hex());
                }
                Do::Keep(Keep::Session(session)) => record.session = Some(session.expose_hex().to_string()),
                Do::Keep(Keep::Music(music)) => record.music = Some(kept_music(music)),
                Do::Keep(Keep::Account(file)) => record.account = serde_json::to_value(file).ok(),
                Do::Keep(Keep::ToTell(tell)) => {
                    let kept = serde_json::to_string(tell).ok().is_some_and(|text| config::write_kept(&paths, TO_TELL, &Zeroizing::new(text)));
                    note(format_args!("sign-out note kept {kept}"));
                }
                Do::Forget(Forget::Session) => record.session = None,
                Do::Forget(Forget::Music) => record.music = None,
                Do::Forget(Forget::Account) => record.account = None,
                Do::Forget(Forget::ToTell) => {
                    config::write_kept(&paths, TO_TELL, "");
                }
                _ => {}
            }
        }
        if record.account.is_none() && record.music.is_none() {
            record.folder.clear();
        }
        if record.folder.is_empty() {
            record.folder = folder_for(&record);
        }
        if record != before {
            let kept = config::save_sign_in(&paths, &record);
            note(format_args!("kept {kept} session={} music={} account={}", record.session.is_some(), record.music.is_some(), record.account.is_some()));
            if !kept {
                crate::controller::with(|ctl| ctl.toast("This PC couldn't keep your sign-in: you'll need to sign in again next time."));
            }
        }
    }
    for thing in dos {
        match thing {
            Do::Ask(ask) => dispatch(ui, ask),
            Do::Hand(hand) => hand_over(hand),
            Do::Forget(Forget::Music) => {
                // From the player too: what is on this PC is not touched.
                note(format_args!("music login forgotten"));
                crate::controller::with(|ctl| ctl.sign_out(""));
            }
            Do::Show(shown) => show(ui, shown),
            Do::Keep(_) | Do::Forget(_) => {}
        }
    }
}

/// The folder for the person a record is for, once it holds both an account and a music
/// login (design 11): the one this PC had, carried over once, or one of their own.
fn folder_for(record: &SignedIn) -> String {
    let (Some(music), Some(account)) = (&record.music, &record.account) else { return String::new() };
    let Ok(account) = serde_json::from_value::<AccountFile>(account.clone()) else { return String::new() };
    let server = crate::subsonic::normalize_server_url(&music.server);
    let decided = crate::controller::with(|ctl| {
        let (folder, carried) = config::folder_for(&ctl.settings.borrow(), &account.backend_id, &account.email, &server, &music.username);
        if carried {
            ctl.settings.borrow_mut().carried_over = Some((format!("{}\n{}", account.backend_id, account.email), folder.clone()));
            ctl.save_settings();
        }
        (folder, carried)
    });
    let (folder, carried) = decided.unwrap_or_default();
    note(format_args!("folder decided, carried over {carried}"));
    folder
}

/// Sends one ask, on a thread of its own, and tells the core what came of it. Dispatched here,
/// on the window's thread, and only while Offline mode is off; its answer is told only if
/// the core still waits for it, in the generation it was asked under.
fn dispatch(ui: &ui::MainWindow, ask: Ask) {
    let (id, generation) = (ask.id, ask.generation);
    if offline_now() {
        note(format_args!("ask {id} not sent: offline"));
        later_failed(ui, id, generation);
        return;
    }
    let deadline = super::test_ms("NEXUS_TEST_DEADLINE_MS").unwrap_or(Duration::from_millis(ask.within_ms));
    let weak = ui.as_weak();
    let started = std::thread::Builder::new().name("nexus-ask".into()).spawn(move || {
        let request = call::Request {
            get: ask.method == Method::Get,
            url: &ask.url,
            authorization: ask.authorization.as_ref().map(|hidden| hidden.expose()),
            body: ask.body.as_ref().map(|hidden| hidden.expose()),
            max_body: ask.max_body,
        };
        let answer = call::send(&request, deadline);
        drop(ask);
        let _ = weak.upgrade_in_event_loop(move |ui| match answer {
            Ok((status, body)) => {
                if CLIENT.with(|client| client.borrow().generation() == generation && client.borrow().waits_for(id)) {
                    happened(&ui, Happened::Answered { ask: id, status, body: AnswerBody::new(body.to_vec()) });
                } else {
                    note(format_args!("ask {id} answer dropped"));
                }
            }
            Err(how) => failed(&ui, id, generation, how),
        });
    });
    if started.is_err() {
        later_failed(ui, id, generation);
    }
}

/// An ask that was not sent: told as no connection, in the next turn (never inside the
/// call that gave it).
fn later_failed(ui: &ui::MainWindow, id: u64, generation: u64) {
    let weak = ui.as_weak();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            failed(&ui, id, generation, core::How::Connect);
        }
    });
}

fn failed(ui: &ui::MainWindow, id: u64, generation: u64, how: core::How) {
    if CLIENT.with(|client| client.borrow().generation() == generation && client.borrow().waits_for(id)) {
        happened(ui, Happened::Failed { ask: id, failure: Failure { how, inside: None, permission: None } });
    }
}

/// Hands the player what the core says: a music login to use, or one to send nothing more
/// with.
fn hand_over(hand: Hand) {
    match hand {
        Hand::Login(music) => {
            let Some(record) = config::load_sign_in(&paths()) else { return };
            let account = record.account.and_then(|value| serde_json::from_value::<AccountFile>(value).ok());
            // The music server's address is read as one the backend handed over (design 7.1):
            // one under the Nexus server's own paths is never used, so the music login is
            // never sent to the backend's.
            if let Some(account) = &account {
                let checked = Address::parse(&format!("{}{}", account.backend, account::MUSIC_LOGIN))
                    .map_err(|_| nexus_core::origin::AddressError::HandoverSource)
                    .and_then(|sent| MusicBase::handed_over(sent.origin(), HandoverSource::DeviceTokenAnswer(&sent), music.address.as_str()));
                if let Err(why) = checked {
                    note(format_args!("music address not used: {}", why.reason()));
                    crate::controller::with(|ctl| ctl.toast(why.handover_words()));
                    return;
                }
            }
            let login = kept_music(&music);
            let folder = record.folder.clone();
            let (nexus, email) = account.map(|account| (account.backend, account.email)).unwrap_or_default();
            note(format_args!("music login handed, number {}", login.count));
            crate::controller::with(|ctl| {
                if ctl.client().is_some() && ctl.account() == folder {
                    ctl.music_login_renewed(&login);
                } else {
                    ctl.signed_in_with(&login, &folder, &nexus, &email);
                }
            });
        }
        Hand::Refused { count } => {
            note(format_args!("music login set aside, number {count}"));
            let held = config::load_sign_in(&paths()).and_then(|record| record.music.map(|music| music.count));
            if held == Some(count) {
                crate::controller::with(|ctl| ctl.music_set_aside());
            }
        }
    }
}

/// Shows what the core says is to be shown.
fn show(ui: &ui::MainWindow, shown: Shown) {
    let app = ui.global::<ui::App>();
    let settings = ui.global::<ui::Settings>();
    let form = &shown.sign_in;
    app.set_login_busy(form.state == SignInState::Working);
    app.set_login_error(form.words.as_str().into());
    if !form.address.is_empty() {
        app.set_nexus_address(form.address.as_str().into());
    }
    if !form.email.is_empty() {
        app.set_nexus_email(form.email.as_str().into());
    }
    app.set_choosing_password(shown.screen == Screen::ChoosePassword);
    // Signed out with music still held ("start again"): the player goes on, and the Account
    // row offers the sign-in screen.
    app.set_nexus_sign_in_wanted(shown.screen == Screen::SignIn && shown.plays);
    if shown.screen != Screen::SignIn {
        app.set_show_sign_in(false);
    }
    settings.set_nexus_row(shown.row.as_str().into());
    settings.set_nexus_who(shown.who.clone().unwrap_or_default().into());
    let music = shown.music.clone();
    if MUSIC_WORDS.with(|said| said.replace(music.clone())) != music
        && let Some(words) = &music
    {
        crate::controller::with(|ctl| ctl.toast(words.clone()));
    }
    settings.set_nexus_music(music.unwrap_or_default().into());
    let rows: Vec<ui::DeviceRow> = shown
        .devices
        .rows
        .iter()
        .map(|row| ui::DeviceRow {
            id: row.id.to_string().into(),
            name: row.name.as_str().into(),
            detail: format!("{}. {}", if row.kind == DeviceKind::Pc { "PC" } else { "Phone" }, row.seen).into(),
            here: row.this_one,
            can_force_out: row.can_force_out && !row.working,
        })
        .collect();
    settings.set_devices(ModelRc::new(VecModel::from(rows)));
    settings.set_devices_busy(shown.devices.state == ListState::Working || shown.devices.rows.iter().any(|row| row.working));
    settings.set_devices_note(shown.devices.words.as_str().into());
    settings.set_password_busy(shown.password.state == PasswordState::Working);
    settings.set_password_changed(shown.password.state == PasswordState::Changed);
    settings.set_password_note(shown.password.words.as_str().into());
    // What this server records (13.6): shown once, then said to have been seen.
    if let Some(notice) = shown.notice
        && NOTICE.with(|seen| seen.replace(notice.clone())) != notice
    {
        crate::controller::with(|ctl| ctl.toast(notice));
        let weak = ui.as_weak();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                happened(&ui, Happened::Pressed(Press::NoticeSeen));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A music login kept and read back is the one the core gave, and its debug form shows
    /// none of its secret parts.
    #[test]
    fn a_music_login_is_kept_as_the_core_gave_it() {
        let login = config::MusicLogin { server: "https://music.example.org".into(), username: "ada".into(), salt: "abcdefghijklmnop".into(), token: "0f".repeat(16), count: 3 };
        let back = kept_music(&core_music(&login).unwrap());
        assert_eq!(back, login);
        let record = SignedIn { session: Some("ab".repeat(32)), music: Some(login), account: None, folder: "nexus-ada".into() };
        let shown = format!("{record:?}");
        assert!(!shown.contains("abab") && !shown.contains("0f0f") && !shown.contains("abcdefgh"), "{shown}");
    }
}
