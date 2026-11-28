//! Signing in to the Nexus server, and everything this app keeps of it (design 8, 9.4, 6.3,
//! 11; slices W7 and W7b): **this app's side of the shared library's client**
//! (`nexus_core::nexus::client`), which decides all of it. This file drives it by the same
//! four calls, in the same order, as the phone does (design 5.5): `start` with what was kept;
//! `happened` whenever something does; then **do** each thing it gives back, in the order
//! given; and `stop` at the app's end. Nothing here decides.
//!
//! **How it is driven** (the review of W7, MUST 2 to 4):
//!
//! - **The machine is one value behind one lock** ([`MACHINE`]), never held across anything
//!   that waits. Anything may tell it something ([`tell`]): the window, an ask's thread, the
//!   journal's reader.
//! - **What it gives back is done by one worker, in order** ([`worker`]): each list exactly
//!   as given, one list after another. What is kept and forgotten (Credential Manager, the
//!   folder decision) is done there, off the window's thread. What needs the window (the
//!   player, the screens, the journal and Diagnostics, which live on the window's thread)
//!   is run there and **waited for** before the next thing in the list.
//! - **A keep or a forget that is not durable stops what follows it.** Nothing later in that
//!   list is done; the lists the machine gave after it are dropped; the machine is stopped
//!   and started again from what is actually kept, so that it never believes something is
//!   kept that is not. The person is told. It is tried again, with a growing pause, until it
//!   holds.
//! - **`start` comes first, and the player is fed only by `Hand`.** Nothing reads a music
//!   login for the player anywhere else; a cold start is the machine's start, done the moment
//!   the window exists. Until its first screen the window shows neither the sign-in form nor
//!   the player (`App.nexus-starting`).
//! - **`waits_for` at the send itself.** An ask's own process is granted its send by the
//!   machine **immediately before its request's first byte goes**, after it has connected
//!   (`call::send`); refused, it sends nothing. Offline reaches the machine at the switch,
//!   before anything else is done (W6's `diagnostics::offline_changed`, attached here).
//!   Quitting stops the machine before anything else.
//!
//! What is this app's own: the transport (`call`), where secrets are kept (`vault`), the
//! person's folder (design 11, [`folder_for`]) and the window.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{LazyLock, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use nexus_core::nexus::account::{self, AccountId, DeviceKind, DeviceSecret, MusicLogin, Name, Session, SessionId, SignOutNote, SignOutSecret};
use nexus_core::nexus::client::{
    self as core, AnswerBody, App, Ask, Client, DeviceKey, Do, Failure, Forget, Given, Hand, Happened, JournalDo, Keep, Kept, ListState, Look, Method, Note,
    PasswordState, Press, Random, Saved, Screen, Shown, SignInState, Typed,
};
use nexus_core::nexus::proto::{self, Version};
use nexus_core::origin::{Address, HandoverSource};
use slint::{ComponentHandle, ModelRc, VecModel};
use zeroize::Zeroizing;

use super::{call, note, vault};
use crate::config::{self, SignedIn};
use crate::ui;

/// Where this device's own secret is kept.
const DEVICE: &str = "nexus-device";
/// What signing in left.
const SIGNED_IN: &str = "nexus-sign-in";
/// Where the sign-outs not yet taken are kept: one slot each, a stable name for each, one
/// more than the core keeps (it keeps the new one before it drops the oldest).
const NOTE_SLOTS: usize = core::NOTES_MAX + 1;
/// What the first build of W7 kept its notes in, as one list: read once, moved into slots.
const OLD_NOTES: &str = "nexus-sign-out-notes";
/// The first asks of a start go this long after it, so that nothing asked is on the way to
/// the first page (as the component's look was).
const START_AFTER: Duration = Duration::from_secs(5);
/// How long the worker waits for the window to do one thing before it looks again whether
/// the app is ending.
const WINDOW_WAIT: Duration = Duration::from_millis(500);

fn note_slot(slot: usize) -> String {
    format!("nexus-note-{slot}")
}

// ---- the machine -------------------------------------------------------------------------------

struct Machine {
    client: Client,
    /// Moved by every start and stop of the machine: a list of things to do from before is
    /// not done.
    epoch: u64,
    started: bool,
    /// Who this app is, as last told by the window.
    app: Option<App>,
    /// Each ask in flight, by number: where it was sent (what an answer handed over was
    /// handed by).
    asks: HashMap<u64, String>,
    /// When this machine's start was: its first asks wait [`START_AFTER`] from it.
    started_at: Option<Instant>,
    /// Starts in a row that ended in a keep that did not hold.
    failures: u32,
}

static MACHINE: LazyLock<Mutex<Machine>> =
    LazyLock::new(|| Mutex::new(Machine { client: Client::default(), epoch: 0, started: false, app: None, asks: HashMap::new(), started_at: None, failures: 0 }));

fn machine() -> MutexGuard<'static, Machine> {
    MACHINE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn epoch_now() -> u64 {
    machine().epoch
}

/// Offline mode as the person last set it: read at each start of the machine.
static OFFLINE: AtomicBool = AtomicBool::new(false);
/// The app is ending: nothing more is done, sent, or waited for.
static QUITTING: AtomicBool = AtomicBool::new(false);
/// The music login the player holds: its number and the generation it was handed under.
/// A refusal carries back the pair of the login refused (`music_refused`), not this.
static HANDED: Mutex<Option<(u64, u64)>> = Mutex::new(None);
/// Counted for the tests: how many asks were refused at the send.
static REFUSED_AT_SEND: AtomicU64 = AtomicU64::new(0);

/// What a list of things to do came from.
#[derive(Clone)]
enum Cause {
    /// A start: what is handed over was kept, and was handed by `handed_at` then.
    Start,
    /// The answer to the ask sent there.
    Answer(String),
    Other,
}

struct Batch {
    epoch: u64,
    dos: Vec<Do>,
    cause: Cause,
}

impl Batch {
    fn of(epoch: u64, dos: Vec<Do>, cause: Cause) -> Batch {
        Batch { epoch, dos, cause }
    }
}

enum Job {
    /// The app has started: what the window knew of the old sign-in, for the folder rule.
    Legacy(config::Legacy),
    Start,
    Batch(Batch),
}

static WORKER: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();

fn send_job(job: Job) {
    if let Some(sender) = WORKER.get() {
        let _ = sender.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).send(job);
    }
}

/// Tells the machine, from any thread, and has what it says done in order.
pub fn tell(event: Happened) {
    if QUITTING.load(Ordering::SeqCst) {
        return;
    }
    if let Happened::Offline(on) = event {
        OFFLINE.store(on, Ordering::SeqCst);
    }
    let cause = match &event {
        Happened::Answered { ask, .. } => machine().asks.get(ask).cloned().map_or(Cause::Other, Cause::Answer),
        _ => Cause::Other,
    };
    let batch = {
        let mut machine = machine();
        if !machine.started {
            return;
        }
        if let Happened::Answered { ask, .. } | Happened::Failed { ask, .. } = &event {
            machine.asks.remove(ask);
        }
        let dos = machine.client.happened(event, &given());
        Batch::of(machine.epoch, dos, cause)
    };
    if !batch.dos.is_empty() {
        send_job(Job::Batch(batch));
    }
}

/// Offline mode switched by the person (before anything else is done for it when it goes on,
/// after when it goes off): kept for the machine's next start. The machine itself is told by
/// Diagnostics' own call at the switch (`diagnostics::offline_changed`), attached here.
pub fn offline_is(on: bool) {
    OFFLINE.store(on, Ordering::SeqCst);
}

fn given() -> Given {
    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|time| time.as_millis() as u64).unwrap_or_default();
    let local_offset_minutes = chrono::Local::now().offset().local_minus_utc() / 60;
    let mut bytes = Zeroizing::new([0u8; 32]);
    let random = getrandom::fill(&mut *bytes).ok().map(|()| Random::new(*bytes));
    Given { now_ms, local_offset_minutes, random }
}

/// Who this app is (on the window's thread: the device's name is a setting).
fn app() -> App {
    let chosen = crate::controller::with(|ctl| ctl.settings.borrow().device_name.trim().to_string()).unwrap_or_default();
    let computer = std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_default();
    let device_name = [chosen.as_str(), computer.trim(), "Windows PC"].into_iter().find_map(Name::new).unwrap_or_else(|| Name::new("PC").expect("a name"));
    App { version: Version::parse(env!("CARGO_PKG_VERSION")).expect("the app's own version"), kind: DeviceKind::Pc, device_name }
}

// ---- what is kept, read back --------------------------------------------------------------------

/// What this app keeps beside the machine's own, as last read or written: the record of the
/// sign-in (its folder, where its music login was handed), and which note is in which slot.
#[derive(Default)]
struct View {
    record: SignedIn,
    /// The note in each slot, by its backend and session; `None` for a free slot.
    slots: Vec<Option<(String, u64)>>,
    /// Slots that hold something that does not read: not free until it is forgotten.
    unreadable: Vec<usize>,
}

static VIEW: LazyLock<Mutex<View>> = LazyLock::new(|| Mutex::new(View { slots: vec![None; NOTE_SLOTS], ..View::default() }));

fn view() -> MutexGuard<'static, View> {
    VIEW.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(super) fn kept_music(music: &MusicLogin) -> config::MusicLogin {
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
    let music = match value.get("music") {
        None | Some(serde_json::Value::Null) => None,
        Some(music) => Some(account::read::<MusicLogin>(music.to_string().as_bytes()).ok()?),
    };
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

/// What the machine is started with: everything kept, read back, on the worker. A part that
/// is there and does not read is told to the machine as such (`Kept::unreadable`), and the
/// machine says to forget it. **A store that could not be asked is not taken for an empty
/// one**: then nothing is started, and it is tried again (`Err`).
fn read_kept(paths: &config::Paths) -> Result<(Kept, View), vault::Trouble> {
    use vault::Trouble;
    vault::sweep(paths, &[DEVICE, SIGNED_IN, OLD_NOTES, &note_slot(0), &note_slot(1), &note_slot(2), &note_slot(3), &note_slot(4)]);
    let mut unreadable = Vec::new();
    let device_secret = match vault::read(paths, DEVICE) {
        Ok(Some(text)) => read_device(&text).or_else(|| {
            unreadable.push(Forget::DeviceSecret);
            None
        }),
        Ok(None) => None,
        Err(Trouble::Garbled) => {
            unreadable.push(Forget::DeviceSecret);
            None
        }
        Err(Trouble::Unreachable) => return Err(Trouble::Unreachable),
    };
    let mut view = View { slots: vec![None; NOTE_SLOTS], ..View::default() };
    let saved = match vault::read(paths, SIGNED_IN) {
        Ok(Some(text)) => match serde_json::from_str::<SignedIn>(&text).ok().and_then(|record| Some((read_saved(record.saved.as_ref()?)?, record))) {
            Some((saved, record)) => {
                view.record = record;
                Some(saved)
            }
            None => {
                unreadable.push(Forget::Saved);
                None
            }
        },
        Ok(None) => None,
        Err(Trouble::Garbled) => {
            unreadable.push(Forget::Saved);
            None
        }
        Err(Trouble::Unreachable) => return Err(Trouble::Unreachable),
    };
    let mut notes = Vec::new();
    for slot in 0..NOTE_SLOTS {
        match vault::read(paths, &note_slot(slot)) {
            Ok(Some(text)) => match serde_json::from_str::<serde_json::Value>(&text).ok().as_ref().and_then(read_note) {
                Some(kept) => {
                    view.slots[slot] = Some((kept.backend_id.clone(), kept.session()));
                    notes.push(kept);
                }
                None => {
                    view.unreadable.push(slot);
                    unreadable.push(Forget::UnreadableNote { index: slot });
                }
            },
            Ok(None) => {}
            Err(Trouble::Garbled) => {
                view.unreadable.push(slot);
                unreadable.push(Forget::UnreadableNote { index: slot });
            }
            Err(Trouble::Unreachable) => return Err(Trouble::Unreachable),
        }
    }
    // What the first build of W7 kept as one list: moved into slots, then forgotten.
    if let Ok(Some(text)) = vault::read(paths, OLD_NOTES) {
        let values: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
        let mut moved = true;
        for value in values {
            let Some(old) = read_note(&value) else { continue };
            if view.slots.iter().flatten().any(|(backend, session)| *backend == old.backend_id && *session == old.session()) {
                continue;
            }
            let Some(slot) = (0..NOTE_SLOTS).find(|slot| view.slots[*slot].is_none() && !view.unreadable.contains(slot)) else { break };
            if vault::write(paths, &note_slot(slot), &value.to_string()) {
                view.slots[slot] = Some((old.backend_id.clone(), old.session()));
                notes.push(old);
            } else {
                moved = false;
            }
        }
        if moved {
            vault::forget(paths, OLD_NOTES);
        }
    }
    let mut kept = Kept { device_secret, saved, notes, offline: OFFLINE.load(Ordering::SeqCst), unreadable, ..Kept::default() };
    crate::diagnostics::load_kept(paths, &mut kept);
    Ok((kept, view))
}

/// The Nexus origin that the account published now was signed in through: where the
/// component's look asks `hello`. `None` when the sign-in kept is not for that account.
pub fn backend_for(server: &str, name: &str) -> Option<String> {
    let view = view();
    let saved = view.record.saved.as_ref()?;
    let music = saved.get("music")?;
    let address = music.get("address")?.as_str()?;
    let user = music.get("name")?.as_str()?;
    let nexus = saved.get("nexus")?.as_str()?.to_string();
    (crate::subsonic::normalize_server_url(address) == server && user == name).then_some(nexus)
}

// ---- installed with the component ---------------------------------------------------------------

fn paths() -> Option<config::Paths> {
    crate::controller::with(|ctl| ctl.paths.clone())
}

/// Takes over the window's sign-in, sign-out, devices and password callbacks, and starts the
/// machine at once (from what is kept, on the worker: nothing is asked before its first
/// screen, and its first asks wait [`START_AFTER`]).
pub fn install(ui: &ui::MainWindow) {
    let Some(paths) = paths() else { return };
    let (sender, jobs) = mpsc::channel();
    if WORKER.set(Mutex::new(sender)).is_err() {
        return;
    }
    let started = std::thread::Builder::new().name("nexus-effects".into()).spawn(move || worker(paths, jobs));
    if started.is_err() {
        return;
    }
    let app_global = ui.global::<ui::App>();
    app_global.set_nexus_starting(true);
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
    app_global.on_form_closed(move || press_on(&weak, Press::FormClosed));
    let weak = ui.as_weak();
    app_global.on_sign_out(move || press_on(&weak, Press::SignOut));
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
    let (address, email, offline, legacy) = crate::controller::with(|ctl| {
        let settings = ctl.settings.borrow();
        (settings.sign_in_address.clone(), settings.sign_in_email.clone(), settings.offline, config::Legacy::of(&settings))
    })
    .unwrap_or_default();
    app_global.set_nexus_address(address.as_str().into());
    app_global.set_nexus_email(email.as_str().into());
    OFFLINE.store(offline, Ordering::SeqCst);
    machine().app = Some(app());
    send_job(Job::Legacy(legacy));
    send_job(Job::Start);
    // A tick: the machine keeps its own times. And what the journal has waiting.
    let tick = slint::Timer::default();
    tick.start(slint::TimerMode::Repeated, Duration::from_millis(core::TICK_MS), move || {
        tell(Happened::Tick);
    });
    let drain = slint::Timer::default();
    drain.start(slint::TimerMode::Repeated, Duration::from_millis(1000), move || {
        while let Some(waiting) = crate::journal::take_waiting() {
            tell(waiting);
        }
    });
    std::mem::forget(tick);
    std::mem::forget(drain);
}

/// The app is ending (from the first moment of quitting): the machine is stopped, so that no
/// ask is granted its send, and nothing more is done.
pub fn stop() {
    QUITTING.store(true, Ordering::SeqCst);
    let mut machine = machine();
    machine.client.stop();
    machine.started = false;
    machine.epoch += 1;
}

fn press_on(_weak: &slint::Weak<ui::MainWindow>, press: Press) {
    let app = app();
    machine().app = Some(app);
    tell(Happened::Pressed(press));
}

/// After a look that found the backend where the one before did not: the network is back
/// (what was owed is asked, by the core's rule). Only a change is told (SHOULD 10).
pub fn after_look(found: bool) {
    static LAST: Mutex<Option<bool>> = Mutex::new(None);
    let before = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).replace(found);
    if found && before == Some(false) {
        tell(Happened::Network { up: true });
    } else if !found && before == Some(true) {
        tell(Happened::Network { up: false });
    }
}

/// The music server refused the login of this number, handed under this generation (the
/// pair the client was made with, not whatever was handed since): told to the machine.
pub fn music_refused(count: u64, generation: u64) {
    tell(Happened::MusicRefused { count, generation });
}

// ---- the worker: doing what the machine says, in order -----------------------------------------

fn worker(paths: config::Paths, jobs: Receiver<Job>) {
    let mut pending: Vec<Job> = Vec::new();
    loop {
        let job = match pending.pop() {
            Some(job) => job,
            None => match jobs.recv() {
                Ok(job) => job,
                Err(_) => return,
            },
        };
        if QUITTING.load(Ordering::SeqCst) {
            return;
        }
        match job {
            Job::Legacy(legacy) => {
                if let Err(why) = config::keep_legacy(&paths, &legacy) {
                    note(format_args!("legacy sign-in not kept: {why}"));
                }
            }
            Job::Start => {
                if let Some(batch) = start_machine(&paths) {
                    pending.push(Job::Batch(batch));
                }
            }
            Job::Batch(batch) => {
                if batch.epoch != epoch_now() {
                    continue;
                }
                let start = matches!(batch.cause, Cause::Start);
                let epoch = batch.epoch;
                if perform(&paths, batch) {
                    if start {
                        machine().failures = 0;
                        // Who is signed in, for the journal and Diagnostics: what was kept, as
                        // the start left it (a sign-in it forgot publishes nothing).
                        let saved = view().record.saved.clone();
                        on_window(epoch, move || {
                            let saved = saved.as_ref().and_then(read_saved);
                            crate::journal::publish_account(saved.as_ref());
                        });
                    }
                } else {
                    // A keep did not hold: every list the machine gave since is from a state
                    // that is not kept. They go; the machine starts again from what is.
                    while let Ok(job) = jobs.try_recv() {
                        if let Job::Legacy(_) = job {
                            pending.insert(0, job);
                        }
                    }
                    let failures = {
                        let mut machine = machine();
                        machine.client.stop();
                        machine.started = false;
                        machine.epoch += 1;
                        machine.failures += 1;
                        machine.failures
                    };
                    window_now(|| {
                        crate::diagnostics::detach();
                        crate::controller::with(|ctl| ctl.toast("Windows couldn't keep your sign-in (Credential Manager). Nexus Music will try again shortly."));
                    });
                    let pause = Duration::from_secs(1u64 << failures.min(8)).min(Duration::from_secs(300));
                    note(format_args!("keep failed; start again in {}s", pause.as_secs()));
                    if wait_or_quit(pause) {
                        return;
                    }
                    pending.push(Job::Start);
                }
            }
        }
    }
}

/// Sleeps, unless the app ends first (then says so).
fn wait_or_quit(pause: Duration) -> bool {
    let until = Instant::now() + pause;
    while Instant::now() < until {
        if QUITTING.load(Ordering::SeqCst) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100).min(until - Instant::now()));
    }
    QUITTING.load(Ordering::SeqCst)
}

/// The machine's start: what is kept, read on this thread; then `start`. What it says is
/// done as any list is.
fn start_machine(paths: &config::Paths) -> Option<Batch> {
    let (kept, view) = loop {
        match read_kept(paths) {
            Ok(read) => break read,
            Err(_) => {
                note(format_args!("kept unreadable for now: start later"));
                window_now(|| {
                    crate::controller::with(|ctl| ctl.toast("Windows Credential Manager couldn't be read. Nexus Music will try again shortly."));
                });
                if wait_or_quit(Duration::from_secs(10)) {
                    return None;
                }
            }
        }
    };
    *VIEW.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = view;
    let batch = {
        let mut machine = machine();
        let app = machine.app.clone()?;
        machine.client.stop();
        machine.epoch += 1;
        machine.started = true;
        machine.started_at = Some(Instant::now());
        machine.asks.clear();
        let dos = machine.client.start(app, kept, &given());
        Batch::of(machine.epoch, dos, Cause::Start)
    };
    note(format_args!("client started"));
    // Diagnostics sends through this machine, and tells it of Offline at the switch.
    on_window(batch.epoch, || crate::diagnostics::attach(tell));
    Some(batch)
}

/// Does each thing, in the order given. Says whether every keep and forget held: if one did
/// not, nothing after it was done.
fn perform(paths: &config::Paths, batch: Batch) -> bool {
    let Batch { epoch, dos, cause } = batch;
    for thing in dos {
        if epoch != epoch_now() || QUITTING.load(Ordering::SeqCst) {
            return true;
        }
        let held = match thing {
            Do::Keep(Keep::DeviceSecret(key)) => {
                let text = Zeroizing::new(serde_json::json!({"secret": key.secret.expose_hex().as_str(), "backend_id": key.backend_id}).to_string());
                vault::write(paths, DEVICE, &text)
            }
            Do::Forget(Forget::DeviceSecret) => vault::forget(paths, DEVICE),
            Do::Keep(Keep::Saved(saved)) => keep_saved(paths, epoch, saved, &cause),
            Do::Forget(Forget::Saved) => {
                let forgotten = vault::forget(paths, SIGNED_IN);
                if forgotten {
                    view().record = SignedIn::default();
                    on_window(epoch, || {
                        *HANDED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
                        crate::journal::publish_account(None);
                        note(format_args!("sign-in forgotten"));
                        // From the player too: what is on this PC is not touched.
                        crate::controller::with(|ctl| ctl.sign_out(""));
                    });
                }
                forgotten
            }
            Do::Keep(Keep::Note(kept_note)) => keep_note(paths, &kept_note),
            Do::Forget(Forget::Note { backend_id, session }) => forget_note(paths, &backend_id, session),
            Do::Forget(Forget::UnreadableNote { index }) => {
                let mut view = view();
                if let Some(at) = view.unreadable.iter().position(|slot| *slot == index) {
                    if !vault::forget(paths, &note_slot(index)) {
                        false
                    } else {
                        view.unreadable.remove(at);
                        true
                    }
                } else {
                    true
                }
            }
            Do::Keep(Keep::DiagSchedule(schedule)) => on_window(epoch, move || crate::diagnostics::keep_schedule(Some(schedule))),
            Do::Forget(Forget::DiagSchedule) => on_window(epoch, || crate::diagnostics::keep_schedule(None)),
            Do::Forget(Forget::Diagnostics) => on_window(epoch, crate::diagnostics::forget_choice),
            Do::Journal(journal) => {
                do_journal(epoch, journal);
                true
            }
            Do::Ask(ask) => {
                dispatch(ask);
                true
            }
            Do::Hand(hand) => {
                let cause = cause.clone();
                on_window(epoch, move || hand_over(hand, &cause));
                true
            }
            Do::Show(shown) => {
                on_window(epoch, move || show(shown));
                true
            }
        };
        if !held {
            note(format_args!("a keep or forget did not hold: nothing after it is done"));
            return false;
        }
    }
    true
}

/// Runs `job` on the window's thread and waits for it there to be done (or not, if this
/// list's epoch has passed by the time it is reached). Says whether it ran.
fn on_window(epoch: u64, job: impl FnOnce() + Send + 'static) -> bool {
    let (done, finished) = mpsc::channel();
    let posted = slint::invoke_from_event_loop(move || {
        let ran = epoch == epoch_now() && !QUITTING.load(Ordering::SeqCst);
        if ran {
            job();
        }
        let _ = done.send(ran);
    });
    if posted.is_err() {
        return false;
    }
    loop {
        match finished.recv_timeout(WINDOW_WAIT) {
            Ok(ran) => return ran,
            Err(RecvTimeoutError::Timeout) if !QUITTING.load(Ordering::SeqCst) => {}
            Err(_) => return false,
        }
    }
}

/// The same, for what is not part of any list.
fn window_now(job: impl FnOnce() + Send + 'static) {
    let _ = slint::invoke_from_event_loop(job);
}

/// The journal's part: done by the journal on the window's thread (it lives there); what it
/// changes on its disk is waited for here before the next thing.
fn do_journal(epoch: u64, journal: JournalDo) {
    let (handed, receipt) = mpsc::channel();
    let ran = on_window(epoch, move || {
        let answered = crate::journal::handle_client(journal, tell);
        let receipt = match answered {
            Ok(receipt) => receipt,
            Err(JournalDo::ReadDiagnostics { read }) => {
                crate::diagnostics::read(read, tell);
                None
            }
            Err(_) => None,
        };
        let _ = handed.send(receipt);
    });
    if ran && let Ok(Some(receipt)) = receipt.recv() {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until && !QUITTING.load(Ordering::SeqCst) {
            match receipt.recv_timeout(WINDOW_WAIT) {
                Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

// ---- keeping ---------------------------------------------------------------------------------------

/// What signing in left, put in place whole, in one step, with the folder it is for and where
/// its music login was handed. Then the journal and Diagnostics are told who is signed in.
fn keep_saved(paths: &config::Paths, epoch: u64, saved: Saved, cause: &Cause) -> bool {
    let Ok(value) = serde_json::to_value(&saved) else { return false };
    let before = view().record.clone();
    let music = saved.music.as_ref();
    let same_music = before.saved.as_ref().and_then(|was| was.get("music")) == value.get("music");
    let handed_at = match cause {
        Cause::Answer(url) if music.is_some() && !same_music => url.clone(),
        _ if music.is_some() => before.handed_at.clone(),
        _ => String::new(),
    };
    let folder = match music {
        Some(login) => match folder_for(paths, &saved, login) {
            Some(folder) => folder,
            None => return false,
        },
        None => String::new(),
    };
    let record = SignedIn { saved: Some(value), folder, handed_at };
    let Ok(text) = serde_json::to_string(&record).map(Zeroizing::new) else { return false };
    if !vault::write(paths, SIGNED_IN, &text) {
        return false;
    }
    note(format_args!("kept session={} music={}", saved.session.is_some(), saved.music.is_some()));
    view().record = record;
    on_window(epoch, move || crate::journal::publish_account(Some(&saved)));
    true
}

/// A sign-out the backend has not taken: in its own slot (the one it had, else a free one).
fn keep_note(paths: &config::Paths, kept_note: &Note) -> bool {
    let (backend_id, session) = kept_note.key();
    let key = (backend_id.to_string(), session);
    let mut view = view();
    let slot = view.slots.iter().position(|slot| slot.as_ref() == Some(&key)).or_else(|| (0..NOTE_SLOTS).find(|slot| view.slots[*slot].is_none() && !view.unreadable.contains(slot)));
    let Some(slot) = slot else { return false };
    let Ok(text) = serde_json::to_string(kept_note) else { return false };
    if !vault::write(paths, &note_slot(slot), &text) {
        return false;
    }
    view.slots[slot] = Some(key);
    note(format_args!("sign-out note kept"));
    true
}

fn forget_note(paths: &config::Paths, backend_id: &str, session: u64) -> bool {
    let mut view = view();
    let Some(slot) = view.slots.iter().position(|slot| slot.as_ref().is_some_and(|(backend, kept)| backend == backend_id && *kept == session)) else { return true };
    if !vault::forget(paths, &note_slot(slot)) {
        return false;
    }
    view.slots[slot] = None;
    note(format_args!("sign-out note forgotten"));
    true
}

/// The folder for the person signed in (design 11, and the review's MUST 14): **the folder
/// this PC had before the Nexus server becomes theirs, once**, if the core says the login
/// they are handed names that same music server and user (`client::carries_over`), and
/// only if that has not been decided already: **the decision is made at the first music
/// login kept on this PC, recorded before anything that follows it, and never made again**,
/// whoever signs in later and whatever the settings say by then. Everyone else's folder is
/// named by who they are to the backend, the same at every sign-in. `None` if the decision
/// could not be kept.
fn folder_for(paths: &config::Paths, saved: &Saved, login: &MusicLogin) -> Option<String> {
    let who = config::Legacy::who(&saved.backend_id, saved.account.get());
    let mut legacy = config::load_legacy(paths).ok()?;
    if !legacy.decided {
        legacy.decided = true;
        if !legacy.server.is_empty() && !legacy.username.is_empty() && core::carries_over(&legacy.server, &legacy.username, login) {
            legacy.carried_to = Some(who.clone());
        }
        config::save_legacy(paths, &legacy).ok()?;
        note(format_args!("folder decided, carried over {}", legacy.carried_to.is_some()));
    }
    Some(match &legacy.carried_to {
        Some(owner) if *owner == who => legacy.stem(),
        _ => config::person_folder(&saved.backend_id, saved.account.get()),
    })
}

// ---- asking ------------------------------------------------------------------------------------

/// Sends one ask, on a thread of its own. **Its send is granted by the machine immediately
/// before the request's first byte goes** (`call::send`'s `grant`): not waited for any more
/// (Offline, a sign-out, a newer press, the app ending), it goes unsent and nothing is told.
/// Its answer or failure is told if the machine still waits for it.
fn dispatch(ask: Ask) {
    let (id, url) = (ask.id, ask.url.clone());
    let delay = {
        let mut machine = machine();
        machine.asks.insert(id, url.clone());
        machine.started_at.map(|at| (at + START_AFTER).saturating_duration_since(Instant::now())).unwrap_or_default()
    };
    let deadline = super::test_ms("NEXUS_TEST_DEADLINE_MS").unwrap_or(Duration::from_millis(ask.within_ms));
    let started = std::thread::Builder::new().name("nexus-ask".into()).spawn(move || {
        if !delay.is_zero() && wait_or_quit(delay) {
            return;
        }
        let waited = || !QUITTING.load(Ordering::SeqCst) && machine().client.waits_for(id);
        if !waited() {
            note(format_args!("ask {id} dropped unsent"));
            return;
        }
        let request = call::Request {
            get: ask.method == Method::Get,
            url: &ask.url,
            authorization: ask.authorization.as_ref().map(|hidden| hidden.expose()),
            body: ask.body.as_ref().map(|hidden| hidden.expose()),
            max_body: ask.max_body,
        };
        note(format_args!("dispatch ask{id}"));
        let answer = call::send(&request, deadline, &waited);
        drop(ask);
        match answer {
            Ok((status, body)) => {
                if !waited() {
                    note(format_args!("ask {id} answer dropped"));
                    return;
                }
                let hello = request_hello(&url, status, &body);
                tell(Happened::Answered { ask: id, status, body: AnswerBody::new(body.to_vec()) });
                if let Some(hello) = hello {
                    let origin = url.strip_suffix(proto::HELLO).unwrap_or_default().to_string();
                    window_now(move || crate::diagnostics::publish_hello(&origin, &hello));
                }
            }
            Err(call::Unsent::Refused) => {
                REFUSED_AT_SEND.fetch_add(1, Ordering::SeqCst);
                note(format_args!("ask {id} refused at the send"));
            }
            Err(call::Unsent::Failed(how)) => {
                if waited() {
                    tell(Happened::Failed { ask: id, failure: Failure { how, inside: None, permission: None } });
                }
            }
        }
    });
    if started.is_err() {
        tell(Happened::Failed { ask: id, failure: Failure { how: core::How::Connect, inside: None, permission: None } });
    }
}

/// `hello`'s answer, read as the machine reads it, for Diagnostics' default.
fn request_hello(url: &str, status: u16, body: &[u8]) -> Option<proto::Hello> {
    if !url.ends_with(proto::HELLO) {
        return None;
    }
    match proto::classify(status, body) {
        proto::Found::Backend(hello) => Some(hello),
        _ => None,
    }
}

// ---- the player ------------------------------------------------------------------------------------

/// Hands the player what the machine says: a music login to use, or one to send nothing more
/// with. **The music server's address is checked as one the backend handed over** (design
/// 7.1), by the request that actually handed it (at a start, the one recorded with it): one
/// under the Nexus server's own paths is never used, here or at any later start.
fn hand_over(hand: Hand, cause: &Cause) {
    match hand {
        Hand::Login { login, generation } => {
            let record = view().record.clone();
            let text = |key: &str| record.saved.as_ref().and_then(|saved| saved.get(key)).and_then(|value| value.as_str()).unwrap_or_default().to_string();
            let (nexus, email) = (text("nexus"), text("email"));
            let handed_at = match cause {
                Cause::Answer(url) => url.clone(),
                _ => record.handed_at.clone(),
            };
            let checked = account::NexusOrigin::typed(&nexus).map_err(|_| None).and_then(|origin| {
                let sent = Address::parse(&handed_at).map_err(|_| None)?;
                login.address.handed_over(&origin, HandoverSource::DeviceTokenAnswer(&sent)).map_err(Some)
            });
            if let Err(why) = checked {
                note(format_args!("music address not used: {}", why.as_ref().map_or("no request recorded", |why| why.reason())));
                *HANDED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
                crate::controller::with(|ctl| {
                    ctl.sign_out("");
                    ctl.toast(why.map_or_else(|| "The music server's address this account was given can't be used.".to_string(), |why| why.handover_words().to_string()));
                });
                return;
            }
            *HANDED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((login.count, generation));
            let music = kept_music(&login);
            let folder = record.folder.clone();
            let legacy = paths().and_then(|paths| config::load_legacy(&paths).ok()).is_some_and(|legacy| legacy.stem() == folder && legacy.carried_to.is_some());
            note(format_args!("music login handed, number {}", music.count));
            crate::controller::with(|ctl| {
                if ctl.client().is_some() && ctl.account() == folder {
                    ctl.music_login_renewed(&music, (music.count, generation));
                } else {
                    ctl.signed_in_with(&music, (music.count, generation), &folder, legacy, &nexus, &email);
                }
            });
        }
        Hand::NoLogin { generation } => {
            // No music login now: the old one leaves the player. What is on this PC is not
            // touched.
            note(format_args!("no music login, generation {generation}"));
            *HANDED.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            crate::journal::publish_account(None);
            crate::controller::with(|ctl| ctl.sign_out(""));
        }
        Hand::Refused { count, generation } => {
            note(format_args!("music login set aside, number {count}"));
            crate::controller::with(|ctl| ctl.music_set_aside(count, generation));
        }
    }
}

// ---- the screens ---------------------------------------------------------------------------------

thread_local! {
    /// The countdown the form shows for the core's "come back in N seconds".
    static WAIT: std::cell::RefCell<Option<slint::Timer>> = const { std::cell::RefCell::new(None) };
    /// The notice of what this server records, as last shown: shown once.
    static NOTICE: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// Shows what the machine says is to be shown (on the window's thread).
fn show(shown: Shown) {
    let Some(ui) = crate::controller::with(|ctl| ctl.ui.as_weak()).and_then(|weak| weak.upgrade()) else { return };
    let app = ui.global::<ui::App>();
    let settings = ui.global::<ui::Settings>();
    app.set_nexus_starting(false);
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
    // presses again. Nothing tries again by itself. When it reaches nought the words go too.
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
                    app.set_login_error("".into());
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
    // Signed in, whatever the player holds (MUST 12): the screen for an account with no music
    // login yet is the machine's state, not the player's.
    app.set_nexus_signed_in(shown.screen == Screen::SignedIn);
    app.set_nexus_plays(shown.plays);
    // Signed out with music still held ("the server's data was put back"): the player goes
    // on, and the Account row offers the sign-in screen.
    app.set_nexus_sign_in_wanted(shown.screen == Screen::SignIn && shown.plays);
    if shown.screen != Screen::SignIn {
        app.set_show_sign_in(false);
    }
    settings.set_nexus_row(shown.row.as_str().into());
    settings.set_nexus_who(shown.who.clone().unwrap_or_default().into());
    settings.set_nexus_music(shown.music.clone().unwrap_or_default().into());
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
        window_now(|| tell(Happened::Pressed(Press::NoticeSeen)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A profile of its own, and nothing kept under the names a test reads (on Windows the
    /// store is the user's own: the tests are run one at a time there).
    fn profile(name: &str) -> (std::path::PathBuf, config::Paths) {
        let dir = std::env::temp_dir().join(format!("nexus-signin-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let paths = config::Paths::in_dir(&dir);
        for kept in [DEVICE, SIGNED_IN, OLD_NOTES].into_iter().map(str::to_string).chain((0..NOTE_SLOTS).map(note_slot)) {
            assert!(vault::forget(&paths, &kept));
        }
        (dir, paths)
    }

    fn saved() -> Saved {
        let music = account::read::<MusicLogin>(
            br#"{"address":"https://music.example.org","name":"ada","salt":"0101010101010101010101010101010101010101010101010101010101010101","token":"0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f","count":3}"#,
        )
        .unwrap();
        Saved {
            nexus: "https://nexus.example.org".into(),
            backend_id: "5c".repeat(16),
            account: AccountId::new(7).unwrap(),
            email: "ada@example.org".into(),
            name: "Ada".into(),
            device: 2,
            restore: 1,
            life: 1,
            change: 1,
            notice: Some("n".repeat(2048)),
            session: Session::from_hex(&"ab".repeat(32)),
            sign_out: Some(SignOutNote { session: SessionId::new(4).unwrap(), secret: SignOutSecret::from_hex(&"cd".repeat(32)).unwrap() }),
            music: Some(music),
        }
    }

    /// What the core gave to keep reads back as it was, through the bytes the store keeps,
    /// at its largest valid size; and its debug form shows none of the secret parts.
    #[test]
    fn what_is_kept_reads_back_as_the_core_gave_it_through_the_store() {
        let (dir, paths) = profile("kept");
        let saved = saved();
        let value = serde_json::to_value(&saved).unwrap();
        let record = SignedIn { saved: Some(value.clone()), folder: "nexus-7-ab".into(), handed_at: "https://nexus.example.org/nexus/api/signin".into() };
        let text = serde_json::to_string(&record).unwrap();
        assert!(text.len() > vault::BLOB_MAX, "the record is larger than one credential");
        assert!(vault::write(&paths, SIGNED_IN, &text));
        for slot in 0..NOTE_SLOTS {
            let kept = Note {
                request: 1 + slot as u64,
                revoke: SignOutNote { session: SessionId::new(10 + slot as u64).unwrap(), secret: SignOutSecret::from_hex(&"ef".repeat(32)).unwrap() },
                nexus: format!("https://{}.example.org", "n".repeat(900)),
                backend_id: "5c".repeat(16),
                restore: 1,
                made_ms: 5,
            };
            assert!(keep_note(&paths, &kept), "slot {slot}");
        }
        let (kept, view) = read_kept(&paths).unwrap();
        assert_eq!(serde_json::to_value(kept.saved.as_ref().unwrap()).unwrap(), value);
        assert_eq!(view.record.folder, "nexus-7-ab");
        assert_eq!(kept.notes.len(), NOTE_SLOTS);
        assert!(kept.unreadable.is_empty());
        let shown = format!("{record:?}");
        assert!(!shown.contains("abab") && !shown.contains("0f0f") && !shown.contains("cdcd"), "{shown}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A note that does not read is forgotten by its own slot: the others are untouched,
    /// whichever order they are forgotten in.
    #[test]
    fn an_unreadable_note_is_forgotten_by_its_slot_and_the_others_stay() {
        let (dir, paths) = profile("slots");
        assert!(vault::write(&paths, &note_slot(0), "not a note"));
        assert!(vault::write(&paths, &note_slot(1), "{\"also\":\"not\"}"));
        let good = Note {
            request: 9,
            revoke: SignOutNote { session: SessionId::new(3).unwrap(), secret: SignOutSecret::from_hex(&"ef".repeat(32)).unwrap() },
            nexus: "https://nexus.example.org".into(),
            backend_id: "5c".repeat(16),
            restore: 1,
            made_ms: 5,
        };
        assert!(vault::write(&paths, &note_slot(2), &serde_json::to_string(&good).unwrap()));
        let (kept, read) = read_kept(&paths).unwrap();
        assert_eq!(kept.unreadable, vec![Forget::UnreadableNote { index: 0 }, Forget::UnreadableNote { index: 1 }]);
        *view() = read;
        let epoch = epoch_now();
        assert!(perform(&paths, Batch::of(epoch, vec![Do::Forget(Forget::UnreadableNote { index: 1 }), Do::Forget(Forget::UnreadableNote { index: 0 })], Cause::Other)));
        let (kept, _) = read_kept(&paths).unwrap();
        assert_eq!(kept.notes.len(), 1);
        assert!(kept.unreadable.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// MUST 14: the folder this PC had before is carried over once, at the first music login
    /// kept here, and the decision is never made again: not after a sign-out, not for a
    /// later account, whatever the settings name by then.
    #[test]
    fn the_folder_before_is_decided_once_and_survives_sign_out() {
        let (dir, paths) = profile("legacy");
        let legacy = config::Legacy { server: "https://music.example.org".into(), username: "ada".into(), decided: false, carried_to: None };
        config::keep_legacy(&paths, &legacy).unwrap();
        // Kept once: a later start's settings (another music user by then) change nothing.
        config::keep_legacy(&paths, &config::Legacy { server: "https://music.example.org".into(), username: "bob".into(), ..legacy.clone() }).unwrap();
        let mut bob = saved();
        bob.account = AccountId::new(8).unwrap();
        bob.music = Some(account::read::<MusicLogin>(
            br#"{"address":"https://music.example.org","name":"bob","salt":"0101010101010101010101010101010101010101010101010101010101010101","token":"0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f","count":1}"#,
        ).unwrap());
        // Bob first: not Ada's music user, so the folder before goes to nobody, for good.
        let first = folder_for(&paths, &bob, bob.music.as_ref().unwrap()).unwrap();
        assert_eq!(first, config::person_folder(&bob.backend_id, 8));
        let ada = saved();
        let later = folder_for(&paths, &ada, ada.music.as_ref().unwrap()).unwrap();
        assert_eq!(later, config::person_folder(&ada.backend_id, 7), "decided once: never carried over later");
        // And Bob's again after a sign-out (nothing of the sign-in kept): the same folder.
        assert_eq!(folder_for(&paths, &bob, bob.music.as_ref().unwrap()).unwrap(), first);
        // Had Ada come first, it would have been hers, and hers alone, at every sign-in.
        let _ = std::fs::remove_dir_all(&dir);
        let paths = config::Paths::in_dir(&dir);
        config::keep_legacy(&paths, &legacy).unwrap();
        let carried = folder_for(&paths, &ada, ada.music.as_ref().unwrap()).unwrap();
        assert_eq!(carried, legacy.stem());
        assert_eq!(folder_for(&paths, &ada, ada.music.as_ref().unwrap()).unwrap(), carried);
        assert_eq!(folder_for(&paths, &bob, bob.music.as_ref().unwrap()).unwrap(), config::person_folder(&bob.backend_id, 8));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A keep that does not hold stops what follows it in the same list.
    #[test]
    fn a_keep_that_does_not_hold_stops_what_follows() {
        let (dir, paths) = profile("stop");
        *view() = View { slots: vec![None; NOTE_SLOTS], ..View::default() };
        let kept = Note {
            request: 9,
            revoke: SignOutNote { session: SessionId::new(3).unwrap(), secret: SignOutSecret::from_hex(&"ef".repeat(32)).unwrap() },
            nexus: "https://nexus.example.org".into(),
            backend_id: "5c".repeat(16),
            restore: 1,
            made_ms: 5,
        };
        assert!(vault::write(&paths, DEVICE, "{}"));
        vault::fail_writes(Some("nexus-note"));
        let epoch = epoch_now();
        let held = perform(&paths, Batch::of(epoch, vec![Do::Keep(Keep::Note(kept)), Do::Forget(Forget::DeviceSecret)], Cause::Other));
        vault::fail_writes(None);
        assert!(!held);
        // What followed the keep was not done.
        assert!(vault::read(&paths, DEVICE).unwrap().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
