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
//! - **Where secrets are kept** (`config::write_kept`: Windows' Credential Manager). What
//!   the core says to keep is kept as it says, **each in one step**: what signing in left
//!   (`Keep::Saved`, whole, with the folder the person's things are under and a plain copy
//!   of the music login for a cold start, in one item: `config::SignedIn`); this device's
//!   own secret beside it; and the sign-outs the backend has not taken yet (`Keep::Note`,
//!   by the session each ends), in an item of their own, written **before** anything that
//!   follows them in the same list (the core puts a sign-out's note before the forgetting
//!   of what signing in left). **The password typed is kept nowhere:** it goes from the
//!   form to the core as `Typed` (wiped when let go of), into the ask's request, through the
//!   ask's own process, once, over HTTPS, to the address typed. The form empties its
//!   password fields when the try is answered, and lets go of them when it closes.
//! - **The person's folder** (design 11): decided once, when a sign-in first leaves both an
//!   account and a music login (`config::folder_for`), by who they are to the backend (its
//!   id and the account's number), with the core's narrow carry-over of the folder this PC
//!   had before (`client::carries_over`).
//! - **The window**: the sign-in screen, choosing a password, the Account rows, the list of
//!   devices, changing the password; and the player, handed a music login (the controller's
//!   `signed_in_with`, `music_login_renewed`, `music_set_aside`, `sign_out`).
//!
//! **Playing never waits for any of this** (design 6): a cold start is the controller's, from
//! what is kept on the disk, before this is started; the core is started after the window's
//! first turn, and everything it asks goes out on threads and in processes of its own.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use nexus_core::nexus::account::{self, AccountId, DeviceKind, DeviceSecret, MusicLogin, Name, Session, SessionId, SignOutNote, SignOutSecret};
use nexus_core::nexus::client::{
    self as core, AnswerBody, App, Ask, Client, DeviceKey, Do, Failure, Forget, Given, Hand, Happened, Keep, Kept, ListState, Look, Method, Note, PasswordState, Press,
    Random, Saved, Screen, Shown, SignInState, Typed,
};
use nexus_core::nexus::proto::Version;
use slint::{ComponentHandle, ModelRc, VecModel};
use zeroize::Zeroizing;

use super::{call, note};
use crate::config::{self, SignedIn};
use crate::ui;

/// Where this device's own secret is kept.
const DEVICE: &str = "nexus-device";
/// The core is started this long after the window's first turn, so that nothing it asks is
/// on the way to the first page (as the component's look was).
const START_AFTER: Duration = Duration::from_secs(5);

thread_local! {
    static CLIENT: RefCell<Client> = RefCell::new(Client::default());
    static STARTED: Cell<bool> = const { Cell::new(false) };
    static OFFLINE: Cell<bool> = const { Cell::new(false) };
    static TIMERS: RefCell<Vec<slint::Timer>> = const { RefCell::new(Vec::new()) };
    /// The countdown the form shows for the core's "come back in N seconds".
    static WAIT: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
    /// The generation the music login in the player was handed under: what a refusal of it
    /// carries back.
    static HANDED: Cell<Option<(u64, u64)>> = const { Cell::new(None) };
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

fn kept_music(music: &MusicLogin) -> config::MusicLogin {
    config::MusicLogin {
        server: music.address.as_str().to_string(),
        username: music.name.as_str().to_string(),
        salt: music.salt.expose().to_string(),
        token: music.token.expose_hex().to_string(),
        count: music.count,
    }
}

// C10: reading back what `Keep` gave, as the phone's binding reads it (`client::json`'s
// `kept`), is to be the core's own (asked of C10 in the bulletin); until then it is read
// here, field for field, as that reader reads it.
fn revoke(value: &serde_json::Value) -> Option<SignOutNote> {
    Some(SignOutNote { session: SessionId::new(value.get("session")?.as_u64()?)?, secret: SignOutSecret::from_hex(value.get("secret")?.as_str()?)? })
}

fn read_saved(value: &serde_json::Value) -> Option<Saved> {
    let text = |key: &str| value.get(key).and_then(|field| field.as_str()).map(str::to_string);
    let number = |key: &str| value.get(key).and_then(|field| field.as_u64());
    let music = value.get("music").filter(|music| !music.is_null()).and_then(|music| account::read::<MusicLogin>(music.to_string().as_bytes()).ok());
    Some(Saved {
        nexus: text("nexus")?,
        backend_id: text("backend_id")?,
        account: AccountId::new(number("account")?)?,
        email: text("email")?,
        name: text("name")?,
        device: number("device")?,
        restore: number("restore")?,
        life: number("life")?,
        change: number("change")?,
        notice: text("notice"),
        session: text("session").and_then(|hex| Session::from_hex(&hex)),
        sign_out: value.get("sign_out").and_then(revoke),
        music,
    })
}

fn read_note(value: &serde_json::Value) -> Option<Note> {
    Some(Note {
        request: value.get("request")?.as_u64()?,
        revoke: revoke(value.get("revoke")?)?,
        nexus: value.get("nexus")?.as_str()?.to_string(),
        backend_id: value.get("backend_id")?.as_str()?.to_string(),
        restore: value.get("restore")?.as_u64()?,
        made_ms: value.get("made_ms")?.as_u64()?,
    })
}

fn read_device(text: &str) -> Option<DeviceKey> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let backend_id = match value.get("backend_id")? {
        serde_json::Value::Null => None,
        id => Some(id.as_str()?.to_string()),
    };
    Some(DeviceKey { secret: DeviceSecret::from_hex(value.get("secret")?.as_str()?)?, backend_id })
}

/// What the core is started with: everything kept, read back. A part that is there and
/// does not read is told to the core as such (`Kept::unreadable`): it is treated as absent,
/// and the core says to forget it. What is kept never stops a start.
fn read_kept(paths: &config::Paths) -> Kept {
    let mut unreadable = Vec::new();
    let device_secret = match config::read_kept(paths, DEVICE) {
        Ok(Some(text)) => read_device(&text).or_else(|| {
            unreadable.push(Forget::DeviceSecret);
            None
        }),
        _ => None,
    };
    let record = config::load_sign_in(paths).unwrap_or_default();
    let saved = record.saved.as_ref().and_then(|value| {
        read_saved(value).or_else(|| {
            unreadable.push(Forget::Saved);
            None
        })
    });
    let notes: Vec<Note> = config::load_notes(paths)
        .iter()
        .enumerate()
        .filter_map(|(index, value)| {
            read_note(value).or_else(|| {
                unreadable.push(Forget::UnreadableNote { index });
                None
            })
        })
        .collect();
    Kept { device_secret, saved, notes, offline: offline_now(), unreadable }
}

/// The Nexus origin that the account published now was signed in through: where the
/// component's look asks `hello`. `None` when the sign-in kept is not for that account.
pub fn backend_for(server: &str, name: &str) -> Option<String> {
    let record = config::load_sign_in(&paths())?;
    let music = record.music.as_ref()?;
    let nexus = record.saved.as_ref()?.get("nexus")?.as_str()?.to_string();
    (crate::subsonic::normalize_server_url(&music.server) == server && music.username == name).then_some(nexus)
}

// ---- installed with the component ---------------------------------------------------------------

/// Takes over the window's sign-in, sign-out, devices and password callbacks, and starts the
/// core after the window's first turn.
pub fn install(ui: &ui::MainWindow) {
    let app_global = ui.global::<ui::App>();
    let weak = ui.as_weak();
    app_global.on_sign_in(move |address, email, password| {
        let press = Press::SignIn { address: address.to_string(), email: email.to_string(), password: Typed::new(&password), look: Look::default() };
        press_on(&weak, press);
    });
    let weak = ui.as_weak();
    app_global.on_sign_in_code(move |address, code, password, again| {
        let press = Press::SignInWithCode {
            address: address.to_string(),
            code: Typed::new(&code),
            password: Typed::new(&password),
            again: Typed::new(&again),
            look: Look::default(),
        };
        press_on(&weak, press);
    });
    let weak = ui.as_weak();
    app_global.on_form_closed(move || {
        if let Some(ui) = weak.upgrade() {
            happened(&ui, Happened::Pressed(Press::FormClosed));
        }
    });
    let weak = ui.as_weak();
    app_global.on_sign_out(move || press_on(&weak, Press::SignOut));
    let weak = ui.as_weak();
    app_global.on_music_login_refused(move || {
        let Some(ui) = weak.upgrade() else { return };
        // A refusal at a cold start, before the core's own start: it is started for it.
        start(&ui);
        if let Some((count, generation)) = HANDED.with(Cell::get) {
            happened(&ui, Happened::MusicRefused { count, generation });
        }
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

/// Does each thing, in the order given. Each keep and forget is one write, whole.
fn run(ui: &ui::MainWindow, dos: Vec<Do>) {
    for thing in dos {
        match thing {
            Do::Keep(Keep::DeviceSecret(key)) => {
                let text = Zeroizing::new(serde_json::json!({"secret": key.secret.expose_hex().as_str(), "backend_id": key.backend_id}).to_string());
                config::write_kept(&paths(), DEVICE, &text);
            }
            Do::Forget(Forget::DeviceSecret) => {
                config::write_kept(&paths(), DEVICE, "");
            }
            Do::Forget(Forget::UnreadableNote { index }) => {
                let paths = paths();
                let mut notes = config::load_notes(&paths);
                if index < notes.len() && read_note(&notes[index]).is_none() {
                    notes.remove(index);
                    config::save_notes(&paths, &notes);
                }
            }
            Do::Keep(Keep::Saved(saved)) => keep_saved(&saved),
            Do::Keep(Keep::Note(kept_note)) => {
                let paths = paths();
                let (backend_id, session) = kept_note.key();
                let mut notes = config::load_notes(&paths);
                notes.retain(|value| !is_note(value, backend_id, session));
                if let Ok(value) = serde_json::to_value(&kept_note) {
                    notes.push(value);
                }
                let kept = config::save_notes(&paths, &notes);
                note(format_args!("sign-out note kept {kept}"));
            }
            Do::Forget(Forget::Note { backend_id, session }) => {
                let paths = paths();
                let mut notes = config::load_notes(&paths);
                notes.retain(|value| !is_note(value, &backend_id, session));
                config::save_notes(&paths, &notes);
                note(format_args!("sign-out note forgotten"));
            }
            Do::Forget(Forget::Saved) => {
                config::save_sign_in(&paths(), &SignedIn::default());
                HANDED.with(|handed| handed.set(None));
                // From the player too: what is on this PC is not touched.
                note(format_args!("sign-in forgotten"));
                crate::controller::with(|ctl| ctl.sign_out(""));
            }
            Do::Ask(ask) => dispatch(ui, ask),
            Do::Hand(hand) => hand_over(hand),
            Do::Show(shown) => show(ui, shown),
        }
    }
}

/// Whether a kept note is the one for this backend and session.
fn is_note(value: &serde_json::Value, backend_id: &str, session: u64) -> bool {
    value.get("backend_id").and_then(|id| id.as_str()) == Some(backend_id)
        && value.get("revoke").and_then(|revoke| revoke.get("session")).and_then(|id| id.as_u64()) == Some(session)
}

/// What signing in left, put in place whole, in one step, with the folder it is for.
fn keep_saved(saved: &Saved) {
    let paths = paths();
    let before = config::load_sign_in(&paths).unwrap_or_default();
    let Ok(value) = serde_json::to_value(saved) else { return };
    let same_person = before.saved.as_ref().is_some_and(|was| was.get("backend_id") == value.get("backend_id") && was.get("account") == value.get("account"));
    let music = saved.music.as_ref().map(kept_music);
    let mut folder = if same_person { before.folder.clone() } else { String::new() };
    if folder.is_empty()
        && let Some(login) = &saved.music
    {
        folder = folder_for(saved, login);
    }
    let record = SignedIn { saved: Some(value), music, folder };
    let kept = config::save_sign_in(&paths, &record);
    note(format_args!("kept {kept} session={} music={}", saved.session.is_some(), saved.music.is_some()));
    if !kept {
        crate::controller::with(|ctl| ctl.toast("This PC couldn't keep your sign-in: you'll need to sign in again next time."));
    }
}

/// The folder for the person signed in, the first time a music login is kept for them
/// (design 11): the one this PC had, carried over once, or one of their own.
fn folder_for(saved: &Saved, login: &MusicLogin) -> String {
    let decided = crate::controller::with(|ctl| {
        let same = {
            let settings = ctl.settings.borrow();
            core::carries_over(&settings.server, &settings.username, login)
        };
        let (folder, carried) = config::folder_for(&ctl.settings.borrow(), &saved.backend_id, saved.account.get(), same);
        if carried {
            ctl.settings.borrow_mut().carried_over = Some((format!("{}\n{}", saved.backend_id, saved.account.get()), folder.clone()));
            ctl.save_settings();
        }
        (folder, carried)
    });
    let (folder, carried) = decided.unwrap_or_default();
    note(format_args!("folder decided, carried over {carried}"));
    folder
}

/// Sends one ask, on a thread of its own, and tells the core what came of it. **The core is
/// asked whether it still waits for it immediately before it is sent**, here on the
/// window's thread (which makes every other call), and it is dropped unsent if not; and
/// nothing is sent while Offline mode is on. Its answer is told only if the core still
/// waits for it, in the generation it was asked under.
fn dispatch(ui: &ui::MainWindow, ask: Ask) {
    let (id, generation) = (ask.id, ask.generation);
    if !CLIENT.with(|client| client.borrow().waits_for(id)) {
        note(format_args!("ask {id} dropped unsent"));
        return;
    }
    if offline_now() {
        note(format_args!("ask {id} not sent: offline"));
        later_failed(ui, id, generation);
        return;
    }
    let deadline = super::test_ms("NEXUS_TEST_DEADLINE_MS").unwrap_or(Duration::from_millis(ask.within_ms));
    note(format_args!("dispatch ask{id} offline=false"));
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
                if CLIENT.with(|client| client.borrow().waits_for(id)) {
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
    let _ = generation;
    if CLIENT.with(|client| client.borrow().waits_for(id)) {
        happened(ui, Happened::Failed { ask: id, failure: Failure { how, inside: None, permission: None } });
    }
}

/// Hands the player what the core says: a music login to use, or one to send nothing more
/// with.
fn hand_over(hand: Hand) {
    match hand {
        Hand::Login { login, generation } => {
            let record = config::load_sign_in(&paths()).unwrap_or_default();
            let nexus = record.saved.as_ref().and_then(|saved| saved.get("nexus")).and_then(|nexus| nexus.as_str()).unwrap_or_default().to_string();
            let email = record.saved.as_ref().and_then(|saved| saved.get("email")).and_then(|email| email.as_str()).unwrap_or_default().to_string();
            // The music server's address is read as one the backend handed over (design 7.1):
            // one under the Nexus server's own paths is never used, so the music login is
            // never sent to the backend's.
            let checked = account::NexusOrigin::typed(&nexus).and_then(|origin| {
                let sent = origin.call(account::MUSIC_LOGIN)?;
                login.address.handed_over(&origin, nexus_core::origin::HandoverSource::DeviceTokenAnswer(&sent))
            });
            if let Err(why) = checked {
                note(format_args!("music address not used: {}", why.reason()));
                crate::controller::with(|ctl| ctl.toast(why.handover_words()));
                return;
            }
            HANDED.with(|handed| handed.set(Some((login.count, generation))));
            let music = kept_music(&login);
            let folder = record.folder.clone();
            note(format_args!("music login handed, number {}", music.count));
            crate::controller::with(|ctl| {
                if ctl.client().is_some() && ctl.account() == folder {
                    ctl.music_login_renewed(&music);
                } else {
                    ctl.signed_in_with(&music, &folder, &nexus, &email);
                }
            });
        }
        Hand::NoLogin { generation } => {
            // Another account signed in with no music login: the old one leaves the player.
            // What is on this PC is not touched.
            note(format_args!("no music login, generation {generation}"));
            HANDED.with(|handed| handed.set(None));
            crate::controller::with(|ctl| ctl.sign_out(""));
        }
        Hand::Refused { count, generation } => {
            note(format_args!("music login set aside, number {count}"));
            if HANDED.with(Cell::get) == Some((count, generation)) {
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
    // "Come back in N seconds": the form disables its button and counts down; the person
    // presses again. Nothing tries again by itself.
    match form.wait_s {
        Some(seconds) if WAIT.with(|slot| slot.borrow().is_none()) => {
            app.set_login_wait(seconds as i32);
            let weak = ui.as_weak();
            let timer = slint::Timer::default();
            timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
                let Some(ui) = weak.upgrade() else { return };
                let app = ui.global::<ui::App>();
                let left = app.get_login_wait() - 1;
                app.set_login_wait(left.max(0));
                if left <= 0 {
                    WAIT.with(|slot| slot.borrow_mut().take());
                }
            });
            WAIT.with(|slot| *slot.borrow_mut() = Some(timer));
        }
        None => {
            WAIT.with(|slot| slot.borrow_mut().take());
            app.set_login_wait(0);
        }
        _ => {}
    }
    // Signed out with music still held ("the server's data was put back"): the player goes
    // on, and the Account row offers the sign-in screen.
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

    /// What the core gave to keep reads back as it was, and its debug form shows none of the
    /// music login's secret parts.
    #[test]
    fn what_is_kept_reads_back_as_the_core_gave_it() {
        let music = account::read::<MusicLogin>(
            br#"{"address":"https://music.example.org","name":"ada","salt":"0101010101010101010101010101010101010101010101010101010101010101","token":"0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f","count":3}"#,
        )
        .unwrap();
        let saved = Saved {
            nexus: "https://nexus.example.org".into(),
            backend_id: "5c".repeat(16),
            account: AccountId::new(7).unwrap(),
            email: "ada@example.org".into(),
            name: "Ada".into(),
            device: 2,
            restore: 1,
            life: 1,
            change: 1,
            notice: None,
            session: Session::from_hex(&"ab".repeat(32)),
            sign_out: Some(SignOutNote { session: SessionId::new(4).unwrap(), secret: SignOutSecret::from_hex(&"cd".repeat(32)).unwrap() }),
            music: Some(music),
        };
        let value = serde_json::to_value(&saved).unwrap();
        let back = read_saved(&value).unwrap();
        assert_eq!(serde_json::to_value(&back).unwrap(), value);
        let record = SignedIn { saved: Some(value), music: back.music.as_ref().map(kept_music), folder: "nexus-7-ab".into() };
        let shown = format!("{record:?}");
        assert!(!shown.contains("abab") && !shown.contains("0f0f") && !shown.contains("cdcd"), "{shown}");
    }
}
