//! Signing in to the Nexus server, and everything this app keeps of it (design 8.5 to 8.10,
//! 9.4, 6.3; slice W7).
//!
//! **The app's one sign-in is to the Nexus server.** A person types its address, their email
//! and their password (or the one-time code the admin gave them, and then chooses a
//! password). The Nexus server answers with a session and **the music login for this
//! device** (the music server's address, a name, and a salt and token made for it), and the
//! app plays with that. The shapes of every call and answer, and the rules for an email, a
//! password and a code, are the shared library's (`nexus_core::nexus::account`): nothing of
//! them is written here.
//!
//! **What is kept, and where** (design 11). Where this PC keeps secrets (`config::write_kept`:
//! Windows' Credential Manager): **one thing, put in place in one step**
//! (`config::SignedIn`: the Nexus origin and the backend's id, the account, the session, and
//! the music login with its number, and the folder the person's things are under), which a
//! cold start reads; and beside it this device's own secret ([`DEVICE`], made once, before
//! the first sign-in, and kept across signing out so that signing in again is the same
//! device). A sign-out the backend could not be told of leaves one small note that can end
//! that session and nothing else (`config::SignOutNote`). **The password typed is kept nowhere:**
//! it goes from the field into the call's request, in memory wiped when it is let go of,
//! through the call's own process (`call`), once, over HTTPS, to the address typed; the
//! field is emptied when it is sent.
//!
//! **Playing never waits for any of this** (design 6): a cold start is the controller's, from
//! the music login on the disk; everything here happens after, on threads of its own and in
//! processes of its own, and is let go of when the account changes (its generation, below).
//!
//! **A refused music login** (design 6.3, R10): the music client stops sending it at the
//! first refusal (`subsonic::Client::was_refused`) and the controller tells the window; here
//! the backend is asked for the current login, once, and the client is replaced by one built
//! from it, which tries once. A login handed back whose count has not gone up is the one
//! that was refused, and is not tried. With the backend out of reach, what is on this PC
//! plays and the person is told it will be renewed when the backend can be reached.
//!
//! **It never signs anyone out because the music server refused** (design 6.3). Three things
//! are kept apart: the network failing (what is on the PC plays); the music server refusing
//! (the rule above); and **the backend itself saying, to this session, that this session
//! has ended** (forced out, a password changed elsewhere, its data put back), which alone
//! signs this PC out, as signing out by hand does. An answer that is not a whole reply of
//! the backend's, or that comes from another backend's id, or for an account no longer
//! signed in, is none of these.
//!
//! What C10 (`nexus_core::nexus::client`) is to decide, once it lands, is marked `C10:`.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use nexus_core::nexus::account::{
    self, ChangePassword, Credential, DeviceId, DeviceKind, DeviceSecret, Email, ForceOut, GetMusicLogin, ListDevices, Name, OneTimeCode, Password, Receipt, Refusal, Reply,
    RequestHead, Session, SignIn, SignInAnswer, SignOut,
};
use nexus_core::nexus::proto::{SPEAKS, Version};
use slint::{ComponentHandle, ModelRc, VecModel};
use zeroize::Zeroizing;

use super::{call, note};
use crate::config::{self, SignedIn};
use crate::ui;

/// Where this device's own secret is kept.
const DEVICE: &str = "nexus-device";
/// The deadline over a call the person is waiting on: a sign-in may wait for the backend's
/// slow password check (design 8.2).
pub const PERSON_DEADLINE: Duration = Duration::from_secs(20);
/// A sign-out note older than this is let go of (design 11: the session has ended by itself).
const NOTE_LIFE_SECS: i64 = 90 * 24 * 3600;

/// A sign-in with a one-time code: the session it gave, which may only choose a password.
struct Setup {
    nexus: String,
    backend: String,
    email: Email,
    device: u64,
    session: Session,
    restore: u64,
}

thread_local! {
    /// What the last sign-in left, as kept (`config::SignedIn`); none when signed out.
    static KEPT: RefCell<Option<SignedIn>> = const { RefCell::new(None) };
    static SETUP: RefCell<Option<Setup>> = const { RefCell::new(None) };
    /// Each try at signing in is numbered, and so is each sign-in and signing out: an answer
    /// asked for under an earlier number is dropped.
    static GENERATION: Cell<u64> = const { Cell::new(0) };
    /// A music login asked for, and not yet handed: one question at a time (C10: the rule
    /// for a refused music login).
    static RENEWING: Cell<bool> = const { Cell::new(false) };
    /// It could not be asked (the backend out of reach): asked again later.
    static RENEW_LATER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
    static RENEW_TRIES: Cell<u32> = const { Cell::new(0) };
    /// The number of the music login the music server refused, set aside: a login is put in
    /// place for it only if its number is higher.
    static SET_ASIDE: Cell<Option<u64>> = const { Cell::new(None) };
    /// A sign-out note waiting to be taken: tried again on this timer.
    static NOTE_LATER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
}

fn paths() -> config::Paths {
    crate::controller::with(|ctl| ctl.paths.clone()).unwrap_or_else(config::Paths::new)
}

fn kept() -> Option<SignedIn> {
    KEPT.with(|slot| slot.borrow().clone())
}

fn next_generation() -> u64 {
    GENERATION.with(|counter| {
        counter.set(counter.get() + 1);
        counter.get()
    })
}

fn session_of(kept: &SignedIn) -> Option<Session> {
    Session::from_hex(&kept.session)
}

fn head_of(kept: &SignedIn) -> RequestHead {
    RequestHead { speaks: SPEAKS, restore: kept.restore }
}

/// This device's own secret: made once, before it first signs in (design 8.6), and kept.
fn device_secret(paths: &config::Paths) -> Option<DeviceSecret> {
    if let Ok(Some(hex)) = config::read_kept(paths, DEVICE)
        && let Some(secret) = DeviceSecret::from_hex(hex.trim())
    {
        return Some(secret);
    }
    let mut bytes = Zeroizing::new([0u8; account::SECRET_BYTES]);
    getrandom::fill(&mut *bytes).ok()?;
    let secret = DeviceSecret::from_bytes(*bytes);
    // (Not kept, it is still this sign-in's; the next one is then a new device.)
    config::write_kept(paths, DEVICE, &secret.expose_hex());
    Some(secret)
}

/// What this PC is called in the person's list of devices.
fn device_name() -> Name {
    let chosen = crate::controller::with(|ctl| ctl.settings.borrow().device_name.trim().to_string()).unwrap_or_default();
    let computer = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_default();
    [chosen.as_str(), computer.trim(), "Windows PC"].into_iter().find_map(Name::new).unwrap_or_else(|| Name::new("PC").expect("a name"))
}

fn app_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).expect("the app's own version")
}

// ---- installed with the component ---------------------------------------------------------------

/// Reads what the last sign-in left, and takes over the window's sign-in, sign-out, devices
/// and password callbacks. A sign-out note left waiting is tried again.
pub fn install(ui: &ui::MainWindow) {
    let paths = paths();
    KEPT.with(|slot| *slot.borrow_mut() = config::load_sign_in(&paths));
    show_kept(ui);
    let app = ui.global::<ui::App>();
    let weak = ui.as_weak();
    app.on_sign_in(move |address, email, password| {
        let password = Zeroizing::new(password.to_string());
        if let Some(ui) = weak.upgrade() {
            sign_in(&ui, &address, Some(&email), &password);
        }
    });
    let weak = ui.as_weak();
    app.on_sign_in_code(move |address, code| {
        let code = Zeroizing::new(code.to_string());
        if let Some(ui) = weak.upgrade() {
            sign_in(&ui, &address, None, &code);
        }
    });
    let weak = ui.as_weak();
    app.on_choose_password(move |new, again| {
        let (new, again) = (Zeroizing::new(new.to_string()), Zeroizing::new(again.to_string()));
        if let Some(ui) = weak.upgrade() {
            choose_password(&ui, &new, &again);
        }
    });
    let weak = ui.as_weak();
    app.on_cancel_choose(move || {
        SETUP.with(|slot| slot.borrow_mut().take());
        next_generation();
        if let Some(ui) = weak.upgrade() {
            let app = ui.global::<ui::App>();
            app.set_choosing_password(false);
            app.set_login_busy(false);
        }
    });
    let weak = ui.as_weak();
    app.on_sign_out(move || {
        if let Some(ui) = weak.upgrade() {
            sign_out(&ui, None);
        }
    });
    let weak = ui.as_weak();
    app.on_music_login_refused(move || {
        if let Some(ui) = weak.upgrade() {
            music_refused(&ui);
        }
    });
    let settings = ui.global::<ui::Settings>();
    let weak = ui.as_weak();
    settings.on_load_devices(move || {
        if let Some(ui) = weak.upgrade() {
            load_devices(&ui, true);
        }
    });
    let weak = ui.as_weak();
    settings.on_force_out(move |id| {
        if let Some(ui) = weak.upgrade() {
            force_out(&ui, &id);
        }
    });
    let weak = ui.as_weak();
    settings.on_change_password(move |current, new, again| {
        let (current, new, again) = (Zeroizing::new(current.to_string()), Zeroizing::new(new.to_string()), Zeroizing::new(again.to_string()));
        if let Some(ui) = weak.upgrade() {
            change_password(&ui, &current, &new, &again);
        }
    });
    // (After the event loop's first turn, never on the way to the first page.)
    let weak = ui.as_weak();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            send_sign_out_note(&ui);
        }
    });
}

/// Puts what is kept on the window: the address and email of the last sign-in (for the
/// sign-in screen, and the Account row).
fn show_kept(ui: &ui::MainWindow) {
    let (address, email) = crate::controller::with(|ctl| {
        let settings = ctl.settings.borrow();
        (settings.sign_in_address.clone(), settings.sign_in_email.clone())
    })
    .unwrap_or_default();
    let app = ui.global::<ui::App>();
    let (address, email) = kept().map(|kept| (kept.nexus, kept.account)).unwrap_or((address, email));
    app.set_nexus_address(address.as_str().into());
    app.set_nexus_email(email.as_str().into());
}

/// The Nexus origin that the account published now was signed in through: where the
/// component's look asks `hello`. `None` when no sign-in of this PC is for that account.
pub fn backend_for(server: &str, name: &str) -> Option<String> {
    KEPT.with(|slot| {
        let slot = slot.borrow();
        let kept = slot.as_ref()?;
        let client = crate::subsonic::normalize_server_url(&kept.music.server);
        (client == server && kept.music.username == name).then(|| kept.nexus.clone())
    })
}

// ---- asking -------------------------------------------------------------------------------------

/// Asks the backend on a thread of its own, and hands what came back to `then` on the
/// window's thread: `None` for no answer, or for one that is not a whole, valid reply of the
/// backend's (`account::read_reply`).
#[allow(clippy::too_many_arguments)]
fn ask<T: Send + 'static>(
    ui: &ui::MainWindow,
    nexus: String,
    path: &'static str,
    session: Option<Zeroizing<String>>,
    body: account::WireBody,
    deadline: Duration,
    read: fn(u16, &[u8]) -> Option<T>,
    then: impl FnOnce(&ui::MainWindow, Option<T>) + Send + 'static,
) {
    let weak = ui.as_weak();
    let started = std::thread::Builder::new().name("nexus-ask".into()).spawn(move || {
        let answer = call::call(&nexus, path, session.as_deref().map(String::as_str), body.bytes(), deadline);
        drop(body);
        drop(session);
        let read = answer.and_then(|(status, bytes)| read(status, &bytes));
        let _ = weak.upgrade_in_event_loop(move |ui| then(&ui, read));
    });
    if started.is_err() {
        note(format_args!("ask {path} not started"));
    }
}

macro_rules! reader {
    ($name:ident, $kind:ty) => {
        fn $name(status: u16, body: &[u8]) -> Option<Reply<$kind>> {
            account::read_reply::<$kind>(status, body).ok()
        }
    };
}
reader!(read_sign_in, SignInAnswer);
reader!(read_signed_out, account::SignedOut);
reader!(read_devices, account::DevicePage);
reader!(read_forced_out, account::ForcedOut);
reader!(read_password, account::PasswordChanged);
reader!(read_music, account::MusicLogin);

/// What an answer to a call under the kept session comes to, once its receipt is taken in.
enum Under<T> {
    Done(T),
    Refused(Refusal),
    /// The backend said, to this session, that it has ended: this PC has been signed out.
    Ended,
    NoAnswer,
}

/// Takes in an answer under the session kept at `generation`: its receipt, and the backend
/// saying the session has ended. `None` if the sign-in has changed since it was asked under
/// (an answer for an account no longer signed in is dropped).
fn under<T>(ui: &ui::MainWindow, generation: u64, reply: Option<Reply<T>>) -> Option<Under<T>> {
    if GENERATION.with(Cell::get) != generation || kept().is_none() {
        return None;
    }
    let (receipt, outcome) = match reply {
        None => return Some(Under::NoAnswer),
        Some(Reply::Done { result, receipt }) => (Some(receipt), Under::Done(result)),
        Some(Reply::Refused(reason)) => (None, Under::Refused(reason)),
        Some(Reply::RefusedWithReceipt(refused)) => {
            if !ours(refused.receipt()) {
                return Some(Under::NoAnswer);
            }
            take_receipt(ui, refused.receipt());
            (None, Under::Refused(clone_refusal(refused.reason())))
        }
    };
    if let Some(receipt) = &receipt {
        // Another backend's answer at this address: not the backend, and nothing is taken.
        if !ours(receipt) {
            note(format_args!("an answer from another backend's id"));
            return Some(Under::NoAnswer);
        }
        take_receipt(ui, receipt);
    }
    // C10: which refusals end a session.
    if let Under::Refused(Refusal::NotSignedIn | Refusal::StartAgain) = outcome {
        ended(ui);
        return Some(Under::Ended);
    }
    Some(outcome)
}

/// Whether a receipt is from the backend this PC signed in to, about the person signed in.
fn ours(receipt: &Receipt) -> bool {
    kept().is_some_and(|kept| receipt.stamp.backend.to_hex() == kept.backend && receipt.stamp.account.as_str() == kept.account)
}

fn clone_refusal(reason: &Refusal) -> Refusal {
    match reason {
        Refusal::WrongLogin => Refusal::WrongLogin,
        Refusal::TooManyTries { wait } => Refusal::TooManyTries { wait: *wait },
        Refusal::AccountOff => Refusal::AccountOff,
        Refusal::Busy => Refusal::Busy,
        Refusal::StartAgain => Refusal::StartAgain,
        Refusal::NotSignedIn => Refusal::NotSignedIn,
        Refusal::NoSuchDevice => Refusal::NoSuchDevice,
        Refusal::PasswordNotAllowed => Refusal::PasswordNotAllowed,
        Refusal::PasswordSetupOnly => Refusal::PasswordSetupOnly,
        Refusal::BadRequest => Refusal::BadRequest,
        Refusal::NoMusicLogin => Refusal::NoMusicLogin,
        Refusal::Incompatible { speaks } => Refusal::Incompatible { speaks: *speaks },
    }
}

/// What a receipt tells (design 12.3, 9.4): the restore number; and the number of the
/// person's music login, which, higher than the one this PC has, means its login is out of
/// date, and the current one is asked for.
fn take_receipt(ui: &ui::MainWindow, receipt: &Receipt) {
    let Some(mut kept) = kept() else { return };
    let risen = receipt.music_login_count > kept.music.count;
    if kept.restore != receipt.stamp.restore {
        kept.restore = receipt.stamp.restore;
        put(&kept);
    }
    if risen {
        note(format_args!("count risen to {}", receipt.music_login_count));
        renew(ui, false);
    }
}

/// The kept sign-in put in place, whole, in one step, and held here.
fn put(kept: &SignedIn) {
    config::save_sign_in(&paths(), kept);
    KEPT.with(|slot| *slot.borrow_mut() = Some(kept.clone()));
}

/// The backend said, to this session, that it has ended (design 6.3, 8.7): this PC signs out,
/// as by hand, and is told why. The backend has it already, so no note is kept.
fn ended(ui: &ui::MainWindow) {
    note(format_args!("session ended"));
    sign_out(ui, Some(ENDED));
}

// ---- the words (C10: the shared library's, for a PC and a phone, once it has them) --------------

const ENDED: &str = "Your Nexus server has signed this PC out. Sign in again.";
const RENEW_WAITING: &str = "Your music sign-in needs renewing. It will be, when your Nexus server can be reached.";
const NO_NEWER: &str = "Your music server no longer takes your sign-in, and your Nexus server has no newer one yet. Music on this PC plays meanwhile.";
const UNREACHABLE: &str = "Your Nexus server could not be reached. Check the address, and that it is running.";
const NOT_A_BACKEND: &str = "That address didn't answer as a Nexus server.";
const OFFLINE: &str = "Offline mode is on.";

fn refusal_words(reason: &Refusal) -> String {
    match reason {
        Refusal::WrongLogin => "Wrong email or password.".into(),
        Refusal::TooManyTries { wait } => {
            let minutes = wait.ms().div_ceil(60_000).max(1);
            format!("Too many tries. Wait {minutes} minute{} and try again.", if minutes == 1 { "" } else { "s" })
        }
        Refusal::AccountOff => "This account is switched off. Ask your Nexus server's admin.".into(),
        Refusal::Busy => "Your Nexus server is busy. Try again in a moment.".into(),
        Refusal::Incompatible { .. } => "This copy of Nexus Music is too old for your Nexus server. Update it. Your music plays as usual.".into(),
        Refusal::PasswordNotAllowed => "That password isn't allowed: it needs at least 10 characters, and can't be your email.".into(),
        Refusal::NoMusicLogin => "Your account has no music sign-in yet. Ask your Nexus server's admin to set one.".into(),
        Refusal::NoSuchDevice => "That device is no longer on your account.".into(),
        Refusal::PasswordSetupOnly => "Choose your password first.".into(),
        Refusal::StartAgain | Refusal::NotSignedIn => "Sign in again.".into(),
        Refusal::BadRequest => "Your Nexus server didn't understand that.".into(),
    }
}

/// The checks of a new password the backend makes too, made here first so that a person is
/// told at once (the rules are the shared library's).
fn new_password(new: &str, again: &str, email: &Email) -> Result<Password, &'static str> {
    if new != again {
        return Err("The two passwords don't match.");
    }
    let password = Password::new(new).ok_or("A password needs at least 10 characters, and at most 128.")?;
    if !password.allowed_for(email) {
        return Err("Your password can't be your email.");
    }
    Ok(password)
}

// ---- signing in ---------------------------------------------------------------------------------

/// Signs in with an email and password (`email` given) or a one-time code (`email` none,
/// `secret` the code). The password goes in one request and is kept nowhere.
fn sign_in(ui: &ui::MainWindow, address: &str, email: Option<&str>, secret: &str) {
    let app = ui.global::<ui::App>();
    if app.get_login_busy() {
        return;
    }
    let fail = |text: &str| app.set_login_error(text.into());
    let Some(nexus) = call::origin(address) else {
        return fail("Type your Nexus server's address, such as nexus.example.org. It must be one that takes HTTPS, with nothing after the name.");
    };
    let credential = match email {
        Some(email) => {
            let Some(email) = Email::new(email) else { return fail("Type the email your account was made with.") };
            // A password that cannot be one is no account's (design 8.2): wrong, unsent.
            let Some(password) = Password::new(secret) else { return fail(&refusal_words(&Refusal::WrongLogin)) };
            Credential::Password { email, password }
        }
        None => {
            // C10 (or C7): how a code typed is tidied.
            let typed: Zeroizing<String> = Zeroizing::new(secret.chars().filter(|c| !c.is_whitespace() && *c != '-').collect::<String>().to_ascii_uppercase());
            let Some(code) = OneTimeCode::new(&typed) else { return fail("That isn't a one-time code: it is twelve letters and digits.") };
            Credential::Code(code)
        }
    };
    let paths = paths();
    let Some(device_secret) = device_secret(&paths) else { return fail("This PC couldn't make the secret it signs in with.") };
    let request = SignIn {
        credential,
        device_secret,
        device_name: device_name(),
        device_kind: DeviceKind::Pc,
        app_version: app_version(),
        speaks: SPEAKS,
        // C10: whether a sign-in names a restore number kept from before.
        restore: None,
    };
    let Ok(body) = account::encode(&request) else { return fail(NOT_A_BACKEND) };
    drop(request);
    let generation = next_generation();
    app.set_login_busy(true);
    app.set_login_error("".into());
    note(format_args!("sign-in asked"));
    ask(ui, nexus.clone(), account::SIGN_IN, None, body, deadline(PERSON_DEADLINE), read_sign_in, move |ui, reply| {
        // An answer to an older try is dropped.
        if GENERATION.with(Cell::get) == generation {
            signed_in(ui, nexus, reply);
        }
    });
}

fn signed_in(ui: &ui::MainWindow, nexus: String, reply: Option<Reply<SignInAnswer>>) {
    let app = ui.global::<ui::App>();
    app.set_login_busy(false);
    match reply {
        None => app.set_login_error(UNREACHABLE.into()),
        Some(Reply::Refused(reason)) => app.set_login_error(refusal_words(&reason).into()),
        Some(Reply::RefusedWithReceipt(refused)) => app.set_login_error(refusal_words(refused.reason()).into()),
        Some(Reply::Done { result: SignInAnswer::PasswordSetup(setup), receipt }) => {
            note(format_args!("sign-in: choose a password"));
            SETUP.with(|slot| {
                *slot.borrow_mut() = Some(Setup {
                    nexus,
                    backend: receipt.stamp.backend.to_hex(),
                    email: setup.person.email,
                    device: setup.device.get(),
                    session: setup.session,
                    restore: receipt.stamp.restore,
                });
            });
            app.set_choosing_password(true);
        }
        Some(Reply::Done { result: SignInAnswer::SignedIn(signed), receipt }) => {
            let account::SignedIn { session, person, device, music, notice } = signed;
            let notice = notice.as_str().to_string();
            take_session(ui, nexus, person.email.as_str(), device.get(), &session, &receipt, &music);
            crate::controller::with(|ctl| ctl.toast(notice));
        }
    }
}

/// Keeps a session the backend has just given, with the music login it handed, in one
/// step, and signs in with it.
fn take_session(ui: &ui::MainWindow, nexus: String, email: &str, device: u64, session: &Session, receipt: &Receipt, music: &account::MusicLogin) {
    let login = music_login(music);
    let (server, user) = (crate::subsonic::normalize_server_url(&login.server), login.username.clone());
    let backend = receipt.stamp.backend.to_hex();
    // The person's folder: the one this PC had, carried over once, or one of their own.
    let (folder, carried) = crate::controller::with(|ctl| config::folder_for(&ctl.settings.borrow(), &backend, email, &server, &user)).unwrap_or_default();
    if carried {
        note(format_args!("folder carried over"));
        crate::controller::with(|ctl| ctl.settings.borrow_mut().carried_over = Some((format!("{backend}\n{email}"), folder.clone())));
    }
    let kept = SignedIn {
        nexus,
        backend,
        account: email.to_string(),
        device,
        session: session.expose_hex().to_string(),
        restore: receipt.stamp.restore,
        music: login,
        folder,
    };
    next_generation();
    RENEWING.with(|renewing| renewing.set(false));
    SET_ASIDE.with(|number| number.set(None));
    stop_renewing_later();
    KEPT.with(|slot| *slot.borrow_mut() = Some(kept.clone()));
    note(format_args!("signed in, device {device}"));
    let saved = crate::controller::with(|ctl| ctl.signed_in_with(kept)).unwrap_or(false);
    show_kept(ui);
    if !saved {
        crate::controller::with(|ctl| ctl.toast("This PC couldn't keep your sign-in: you'll need to sign in again next time."));
    }
}

fn music_login(music: &account::MusicLogin) -> config::MusicLogin {
    config::MusicLogin {
        server: music.address.as_str().to_string(),
        username: music.name.as_str().to_string(),
        salt: music.salt.expose().to_string(),
        token: music.token.expose_hex().to_string(),
        count: music.count,
    }
}

/// After a sign-in with a one-time code: the password chosen, then the music login asked for
/// under the same session. The new password is sent once.
fn choose_password(ui: &ui::MainWindow, new: &str, again: &str) {
    let app = ui.global::<ui::App>();
    if app.get_login_busy() {
        return;
    }
    let Some((nexus, email, head, session)) = SETUP.with(|slot| {
        slot.borrow().as_ref().map(|setup| (setup.nexus.clone(), setup.email.clone(), RequestHead { speaks: SPEAKS, restore: setup.restore }, setup.session.authorization()))
    }) else {
        app.set_choosing_password(false);
        return;
    };
    let password = match new_password(new, again, &email) {
        Ok(password) => password,
        Err(why) => return app.set_login_error(why.into()),
    };
    let Ok(body) = account::encode(&ChangePassword { head, current_password: None, new_password: password }) else { return };
    let generation = next_generation();
    app.set_login_busy(true);
    app.set_login_error("".into());
    ask(ui, nexus.clone(), account::CHANGE_PASSWORD, Some(session.clone()), body, deadline(PERSON_DEADLINE), read_password, move |ui, reply| {
        if GENERATION.with(Cell::get) != generation {
            return;
        }
        let app = ui.global::<ui::App>();
        let fail = |text: String| {
            app.set_login_busy(false);
            app.set_login_error(text.into());
        };
        match reply {
            Some(Reply::Done { .. }) => {
                // The password is set: the music login, under the same session.
                let Some(head) = SETUP.with(|slot| slot.borrow().as_ref().map(|setup| RequestHead { speaks: SPEAKS, restore: setup.restore })) else { return };
                let Ok(body) = account::encode(&GetMusicLogin { head }) else { return };
                ask(ui, nexus.clone(), account::MUSIC_LOGIN, Some(session), body, deadline(PERSON_DEADLINE), read_music, move |ui, reply| {
                    if GENERATION.with(Cell::get) != generation {
                        return;
                    }
                    let app = ui.global::<ui::App>();
                    app.set_login_busy(false);
                    let setup = SETUP.with(|slot| slot.borrow_mut().take());
                    app.set_choosing_password(false);
                    match (reply, setup) {
                        (Some(Reply::Done { result, receipt }), Some(setup)) if receipt.stamp.backend.to_hex() == setup.backend => {
                            take_session(ui, nexus, setup.email.as_str(), setup.device, &setup.session, &receipt, &result);
                        }
                        // C10: what a password-setup session may do once the password is set.
                        _ => app.set_login_error("Your password is set. Sign in with it.".into()),
                    }
                });
            }
            Some(Reply::Refused(reason)) => fail(refusal_words(&reason)),
            Some(Reply::RefusedWithReceipt(refused)) => fail(refusal_words(refused.reason())),
            None => fail(UNREACHABLE.into()),
        }
    });
}

// ---- signing out --------------------------------------------------------------------------------

/// Signs out on this PC (design 11): at once and lastingly, whether or not the backend can
/// be told. The kept sign-in goes, and the controller signs out (the queue and Recently
/// played go; downloads stay). The backend is then told once; if it cannot be, a note that
/// can end that one session is kept and tried again. `ended`: the backend itself said the
/// session has ended, so it is not told, and the person is told why.
fn sign_out(ui: &ui::MainWindow, ended: Option<&str>) {
    let Some(kept) = KEPT.with(|slot| slot.borrow_mut().take()) else { return };
    next_generation();
    RENEWING.with(|renewing| renewing.set(false));
    SET_ASIDE.with(|number| number.set(None));
    stop_renewing_later();
    let paths = paths();
    if ended.is_none() {
        // Kept before the sign-in is let go of, so that nothing is lost between the two.
        let note = config::SignOutNote { nexus: kept.nexus.clone(), backend: kept.backend.clone(), session: kept.session.clone(), restore: kept.restore, at: chrono::Utc::now().timestamp() };
        config::save_sign_out_note(&paths, &note);
    }
    ui.global::<ui::Settings>().set_devices(ModelRc::default());
    crate::controller::with(|ctl| ctl.sign_out(ended.unwrap_or("")));
    show_kept(ui);
    if ended.is_none() {
        send_sign_out_note(ui);
    }
}

/// Tells the backend of a sign-out it has not had yet: one call, under the note's session
/// alone. Taken, or refused as a session already ended, the note goes; with no answer it is
/// tried again in an hour, until it is 90 days old. Not in Offline mode.
fn send_sign_out_note(ui: &ui::MainWindow) {
    let paths = paths();
    let Some(note_kept) = config::load_sign_out_note(&paths) else { return };
    if chrono::Utc::now().timestamp() - note_kept.at > NOTE_LIFE_SECS {
        return config::forget_sign_out_note(&paths);
    }
    let Some(session) = Session::from_hex(&note_kept.session) else { return config::forget_sign_out_note(&paths) };
    if super::offline() {
        return note_later(ui);
    }
    let Ok(body) = account::encode(&SignOut { head: RequestHead { speaks: SPEAKS, restore: note_kept.restore } }) else { return };
    note(format_args!("sign-out note sent"));
    let backend = note_kept.backend.clone();
    ask(ui, note_kept.nexus.clone(), account::SIGN_OUT, Some(session.authorization()), body, deadline(super::CALL_DEADLINE), read_signed_out, move |ui, reply| {
        let taken = match &reply {
            Some(Reply::Done { receipt, .. }) => receipt.stamp.backend.to_hex() == backend,
            // (Ended already, or the backend's data put back: nothing more to end.)
            Some(Reply::Refused(Refusal::NotSignedIn | Refusal::StartAgain)) => true,
            _ => false,
        };
        let paths = paths();
        if taken {
            if config::load_sign_out_note(&paths).is_some_and(|now| now.session == note_kept.session) {
                config::forget_sign_out_note(&paths);
            }
            note(format_args!("sign-out note taken"));
        } else {
            note_later(ui);
        }
    });
}

fn note_later(ui: &ui::MainWindow) {
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::SingleShot, super::test_ms("NEXUS_TEST_RENEW_MS").unwrap_or(Duration::from_secs(3600)), move || {
        if let Some(ui) = weak.upgrade() {
            send_sign_out_note(&ui);
        }
    });
    NOTE_LATER.with(|slot| *slot.borrow_mut() = Some(timer));
}

// ---- a refused music login (design 6.3) ---------------------------------------------------------

/// The music server refused the music login in use: its number is set aside (the client has
/// already stopped sending it), and the current login is asked for, once. Never a sign-out.
fn music_refused(ui: &ui::MainWindow) {
    let Some(kept) = kept() else { return };
    note(format_args!("music login refused, number {}", kept.music.count));
    let first = SET_ASIDE.with(|number| number.replace(Some(kept.music.count))) != Some(kept.music.count);
    if first {
        renew(ui, true);
    }
}

/// Asks the backend for the current music login, unless that is already being asked.
fn renew(ui: &ui::MainWindow, refused: bool) {
    if RENEWING.with(Cell::get) {
        return;
    }
    if super::offline() {
        if refused {
            crate::controller::with(|ctl| ctl.toast(RENEW_WAITING));
        }
        return renew_later(ui);
    }
    let Some(kept) = kept() else { return };
    let generation = GENERATION.with(Cell::get);
    let Some(session) = session_of(&kept) else { return };
    let Ok(body) = account::encode(&GetMusicLogin { head: head_of(&kept) }) else { return };
    RENEWING.with(|renewing| renewing.set(true));
    note(format_args!("music login asked"));
    ask(ui, kept.nexus.clone(), account::MUSIC_LOGIN, Some(session.authorization()), body, deadline(super::CALL_DEADLINE), read_music, move |ui, reply| {
        if GENERATION.with(Cell::get) != generation {
            return;
        }
        RENEWING.with(|renewing| renewing.set(false));
        match under(ui, generation, reply) {
            None | Some(Under::Ended) => {}
            Some(Under::Done(music)) => {
                stop_renewing_later();
                let Some(mut kept) = kept() else { return };
                // C10: the rule for a refused music login. A login whose number is not above
                // the one set aside is the one refused: it stays set aside, and renewal waits
                // for the network to come back or the number to move.
                if SET_ASIDE.with(Cell::get).is_some_and(|aside| music.count <= aside) {
                    note(format_args!("nothing newer than {}", music.count));
                    crate::controller::with(|ctl| ctl.toast(NO_NEWER));
                    return;
                }
                if music.count < kept.music.count {
                    return;
                }
                kept.music = music_login(&music);
                SET_ASIDE.with(|number| number.set(None));
                note(format_args!("music login handed, number {}", music.count));
                KEPT.with(|slot| *slot.borrow_mut() = Some(kept.clone()));
                crate::controller::with(|ctl| ctl.music_login_renewed(kept));
            }
            Some(Under::Refused(reason)) => {
                crate::controller::with(|ctl| ctl.toast(refusal_words(&reason)));
            }
            Some(Under::NoAnswer) => {
                if SET_ASIDE.with(Cell::get).is_some() {
                    crate::controller::with(|ctl| ctl.toast(RENEW_WAITING));
                }
                renew_later(ui);
            }
        }
    });
}

/// The music login could not be asked for: asked again, a minute later at first and less
/// often after, until it is handed or the account changes.
fn renew_later(ui: &ui::MainWindow) {
    let tries = RENEW_TRIES.with(|tries| {
        tries.set(tries.get() + 1);
        tries.get()
    });
    let after = super::test_ms("NEXUS_TEST_RENEW_MS").unwrap_or(match tries {
        1 => Duration::from_secs(60),
        2 => Duration::from_secs(300),
        3 => Duration::from_secs(900),
        _ => Duration::from_secs(3600),
    });
    let generation = GENERATION.with(Cell::get);
    let weak = ui.as_weak();
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::SingleShot, after, move || {
        if let Some(ui) = weak.upgrade()
            && GENERATION.with(Cell::get) == generation
        {
            RENEW_LATER.with(|slot| slot.borrow_mut().take());
            renew(&ui, false);
        }
    });
    RENEW_LATER.with(|slot| *slot.borrow_mut() = Some(timer));
}

fn stop_renewing_later() {
    RENEW_LATER.with(|slot| slot.borrow_mut().take());
    RENEW_TRIES.with(|tries| tries.set(0));
}

/// After each look that found the backend: the session is checked (one call, whose receipt
/// says whether the music login's number has moved, and whether the session has ended), a
/// music login waiting to be renewed is asked for, and a sign-out note is sent.
pub fn after_look(ui: &ui::MainWindow) {
    if super::offline() {
        return;
    }
    if RENEW_LATER.with(|slot| slot.borrow().is_some()) {
        stop_renewing_later();
        renew(ui, false);
    }
    load_devices(ui, false);
}

// ---- devices, force out, change password --------------------------------------------------------

/// The list of devices: asked for when the person opens it (`by_hand`), and after each look.
fn load_devices(ui: &ui::MainWindow, by_hand: bool) {
    let settings = ui.global::<ui::Settings>();
    let Some(kept) = kept() else { return };
    let generation = GENERATION.with(Cell::get);
    let Some(session) = session_of(&kept) else { return };
    if super::offline() {
        if by_hand {
            settings.set_devices_note(OFFLINE.into());
        }
        return;
    }
    let Ok(body) = account::encode(&ListDevices { head: head_of(&kept), after: None }) else { return };
    if by_hand {
        settings.set_devices_busy(true);
    }
    note(format_args!("devices asked"));
    let limit = if by_hand { PERSON_DEADLINE } else { super::CALL_DEADLINE };
    ask(ui, kept.nexus.clone(), account::LIST_DEVICES, Some(session.authorization()), body, deadline(limit), read_devices, move |ui, reply| {
        let settings = ui.global::<ui::Settings>();
        if by_hand {
            settings.set_devices_busy(false);
        }
        match under(ui, generation, reply) {
            None | Some(Under::Ended) => {}
            Some(Under::Done(page)) => show_devices(ui, page.devices(), by_hand),
            Some(Under::Refused(reason)) if by_hand => settings.set_devices_note(refusal_words(&reason).into()),
            Some(Under::NoAnswer) if by_hand => settings.set_devices_note(UNREACHABLE.into()),
            Some(_) => {}
        }
    });
}

fn show_devices(ui: &ui::MainWindow, devices: &[account::Device], by_hand: bool) {
    let this = kept().map(|kept| kept.device).unwrap_or_default();
    let mut rows: Vec<ui::DeviceRow> = devices
        .iter()
        .map(|device| {
            let kind = match device.kind {
                DeviceKind::Pc => "PC",
                DeviceKind::Phone => "Phone",
            };
            let here = device.id.get() == this;
            let seen = if here { "This PC".to_string() } else { last_seen(device.last_seen_ms) };
            let state = if device.signed_in { "" } else { ", signed out" };
            ui::DeviceRow {
                id: device.id.get().to_string().into(),
                name: device.name.as_str().into(),
                detail: format!("{kind}, Nexus Music {}. {seen}{state}", device.app_version).into(),
                here,
                signed_in: device.signed_in,
            }
        })
        .collect();
    rows.sort_by_key(|row| !row.here);
    let settings = ui.global::<ui::Settings>();
    settings.set_devices(ModelRc::new(VecModel::from(rows)));
    if by_hand {
        settings.set_devices_note("".into());
    }
}

fn last_seen(ms: u64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_millis_opt(ms as i64).single() {
        Some(at) if ms > 0 => format!("Last used {}", at.format("%-d %b %Y, %H:%M")),
        _ => "Not used yet".into(),
    }
}

fn force_out(ui: &ui::MainWindow, id: &str) {
    let settings = ui.global::<ui::Settings>();
    let Some(kept) = kept() else { return };
    let generation = GENERATION.with(Cell::get);
    let (Some(session), Some(device)) = (session_of(&kept), id.parse().ok().and_then(DeviceId::new)) else { return };
    if device.get() == kept.device {
        return;
    }
    if super::offline() {
        return settings.set_devices_note(OFFLINE.into());
    }
    let Ok(body) = account::encode(&ForceOut { head: head_of(&kept), device }) else { return };
    settings.set_devices_busy(true);
    note(format_args!("force out asked"));
    ask(ui, kept.nexus.clone(), account::FORCE_OUT, Some(session.authorization()), body, deadline(PERSON_DEADLINE), read_forced_out, move |ui, reply| {
        let settings = ui.global::<ui::Settings>();
        settings.set_devices_busy(false);
        match under(ui, generation, reply) {
            None | Some(Under::Ended) => {}
            Some(Under::Done(forced)) => {
                settings.set_devices_note(
                    if forced.music_cut_off {
                        "Signed out, and its music sign-in changed. Your other devices are handed the new one by themselves."
                    } else {
                        "Signed out. That device can no longer use Nexus Music, but it still knows a music sign-in that works: to cut it off from the music too, have your music server's password changed."
                    }
                    .into(),
                );
                load_devices(ui, false);
            }
            Some(Under::Refused(reason)) => {
                settings.set_devices_note(refusal_words(&reason).into());
                load_devices(ui, false);
            }
            Some(Under::NoAnswer) => settings.set_devices_note(UNREACHABLE.into()),
        }
    });
}

fn change_password(ui: &ui::MainWindow, current: &str, new: &str, again: &str) {
    let settings = ui.global::<ui::Settings>();
    if settings.get_password_busy() {
        return;
    }
    let Some(kept) = kept() else { return };
    let generation = GENERATION.with(Cell::get);
    let Some(session) = session_of(&kept) else { return };
    if super::offline() {
        return settings.set_password_note(OFFLINE.into());
    }
    let Some(email) = Email::new(&kept.account) else { return };
    let Some(current) = Password::new(current) else { return settings.set_password_note("That isn't your present password.".into()) };
    let password = match new_password(new, again, &email) {
        Ok(password) => password,
        Err(why) => return settings.set_password_note(why.into()),
    };
    let Ok(body) = account::encode(&ChangePassword { head: head_of(&kept), current_password: Some(current), new_password: password }) else { return };
    settings.set_password_busy(true);
    settings.set_password_note("".into());
    note(format_args!("password change asked"));
    ask(ui, kept.nexus.clone(), account::CHANGE_PASSWORD, Some(session.authorization()), body, deadline(PERSON_DEADLINE), read_password, move |ui, reply| {
        let settings = ui.global::<ui::Settings>();
        settings.set_password_busy(false);
        match under(ui, generation, reply) {
            None | Some(Under::Ended) => {}
            Some(Under::Done(_)) => {
                settings.set_password_changed(true);
                settings.set_password_note("Your password is changed. Your other devices have been signed out.".into());
            }
            Some(Under::Refused(Refusal::WrongLogin)) => settings.set_password_note("That isn't your present password.".into()),
            Some(Under::Refused(reason)) => settings.set_password_note(refusal_words(&reason).into()),
            Some(Under::NoAnswer) => settings.set_password_note(UNREACHABLE.into()),
        }
    });
}

/// A call's deadline, made longer for the tests of the stuck modes (as the look's is).
fn deadline(normal: Duration) -> Duration {
    super::test_ms("NEXUS_TEST_DEADLINE_MS").unwrap_or(normal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_password_is_checked_before_it_is_sent() {
        let email = Email::new("ada@example.org").unwrap();
        assert!(new_password("ten chars!!", "ten chars!", &email).is_err());
        assert!(new_password("short", "short", &email).is_err());
        assert!(new_password("ADA@example.org", "ADA@example.org", &email).is_err());
        assert!(new_password("a long enough one", "a long enough one", &email).is_ok());
    }

    #[test]
    fn what_is_kept_never_shows_its_secrets() {
        let login = config::MusicLogin { server: "https://music.example.org".into(), username: "ada".into(), salt: "s".repeat(16), token: "0f".repeat(16), count: 1 };
        let kept = SignedIn {
            nexus: "https://nexus.example.org".into(),
            backend: "5c".repeat(16),
            account: "ada@example.org".into(),
            device: 3,
            session: "ab".repeat(32),
            restore: 1,
            music: login.clone(),
            folder: "nexus-ada".into(),
        };
        let shown = format!("{kept:?} {login:?}");
        assert!(!shown.contains("abab") && !shown.contains("0f0f") && !shown.contains("ssss"), "{shown}");
    }

    #[test]
    fn a_refusal_is_said_plainly() {
        assert_eq!(refusal_words(&Refusal::WrongLogin), "Wrong email or password.");
        let wait = account::Wait::new(61_000).unwrap();
        assert_eq!(refusal_words(&Refusal::TooManyTries { wait }), "Too many tries. Wait 2 minutes and try again.");
    }
}
