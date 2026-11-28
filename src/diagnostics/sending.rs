//! The Diagnostics effects of the same client that owns the Nexus sign-in.
//! Its Ask uses that client's normal backend transport.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use nexus_core::nexus::{
    account::NexusOrigin,
    client::{DiagSchedule, Forget, Happened, Kept},
    diag,
    proto::{BackendId, Hello},
};
use slint::ComponentHandle;

use super::{STORE, read_limited, report_text, worker};
use crate::{config, controller::{Controller, post, with}, ui};

const CHOICE_FILE: &str = "diagnostics-choice.json";
const SCHEDULE_FILE: &str = "diagnostics-schedule.json";

type Account = (String, BackendId);
type EventSink = Rc<dyn Fn(Happened)>;

#[derive(Default)]
struct Settings {
    choice: Option<bool>,
    choice_changed: bool,
    choice_loaded: bool,
    account: Option<Account>,
    defaults: HashMap<Account, bool>,
    offline: bool,
    in_use: bool,
    send: Option<EventSink>,
}

impl Settings {
    fn display(&self) -> (bool, &'static str) {
        let default = self.account.as_ref().and_then(|account| self.defaults.get(account)).copied();
        let on = diag::enabled(self.choice, default);
        let detail = if self.account.is_none() {
            "Versions, settings and fault counts. Nothing is sent until you sign in to a Nexus server."
        } else if self.offline {
            "Versions, settings and fault counts. Nothing is sent in Offline mode."
        } else if !on {
            "Versions, settings and fault counts. Sending is off."
        } else if default.is_none() {
            "Versions, settings and fault counts. Waiting to hear from your Nexus server."
        } else {
            "Sends versions, settings and fault counts."
        };
        (on, detail)
    }
}

thread_local! {
    static SETTINGS: RefCell<Settings> = RefCell::default();
    static FAULT: Cell<Option<u64>> = const { Cell::new(None) };
    // Only the diagnostics worker touches this. Its queue orders Keep before Read.
    static SCHEDULE_KEPT: Cell<bool> = const { Cell::new(false) };
}

pub(super) fn install(ctl: &Controller) {
    SETTINGS.with_borrow_mut(|state| state.offline = ctl.settings.borrow().offline);
    paint();
    let paths = ctl.paths.clone();
    on_worker(move || {
        if let Some(folder) = paths.settings.parent() {
            config::clear_leftovers(folder, |name| matches!(name, CHOICE_FILE | SCHEDULE_FILE), &[]);
        }
        let choice = read_profile::<bool>(&choice_path(&paths), 8).ok().flatten();
        post(move |_| {
            SETTINGS.with_borrow_mut(|state| {
                if !state.choice_changed {
                    state.choice = choice;
                }
                state.choice_loaded = true;
            });
            paint();
        });
    });
    ctl.ui.global::<ui::Settings>().on_set_send_diagnostics(choose);
}

fn choose(on: bool) {
    SETTINGS.with_borrow_mut(|state| {
        state.choice = Some(on);
        state.choice_changed = true;
    });
    if let Some(path) = with(|ctl| choice_path(&ctl.paths)) {
        on_worker(move || {
            if save_choice(&path, Some(on)).is_err() {
                post(|ctl| ctl.toast("Couldn't save your diagnostics choice."));
            }
        });
    }
    emit(Happened::DiagnosticsSwitch(Some(on)));
    paint();
}

fn paint() {
    let (on, detail) = SETTINGS.with_borrow(Settings::display);
    with(|ctl| {
        let ui = ctl.ui.global::<ui::Settings>();
        ui.set_send_diagnostics(on);
        ui.set_diagnostics_detail(detail.into());
    });
}

fn emit(event: Happened) {
    let send = SETTINGS.with_borrow(|state| state.send.clone());
    if let Some(send) = send {
        send(event);
    }
}

/// Bind after W7 starts the live client, before accepting more window events.
#[allow(dead_code)] // W7 owns the live client.
pub(crate) fn attach(send: impl Fn(Happened) + 'static) {
    let (choice, in_use) = SETTINGS.with_borrow_mut(|state| {
        state.send = Some(Rc::new(send));
        (state.choice_loaded.then_some(state.choice), state.in_use)
    });
    if let Some(choice) = choice {
        emit(Happened::DiagnosticsSwitch(choice));
    }
    emit(Happened::InUse(in_use));
    report_fault();
}

pub(crate) fn detach() {
    SETTINGS.with_borrow_mut(|state| {
        state.send = None;
        state.account = None;
        state.defaults.clear();
    });
    paint();
}

/// Called by W8's account publication on the window thread.
pub(crate) fn publish_account(signed_in: Option<(&str, BackendId)>) {
    let account = signed_in.and_then(|(address, backend)| Some((origin(address)?, backend)));
    SETTINGS.with_borrow_mut(|state| state.account = account);
    paint();
}

/// W7 publishes a hello accepted by the live client, passing the origin of that ask.
#[allow(dead_code)] // W7 owns the live client.
pub(crate) fn publish_hello(address: &str, hello: &Hello) {
    if let Some(origin) = origin(address) {
        SETTINGS.with_borrow_mut(|state| {
            state.defaults.insert((origin, hello.id), diag::enabled(None, hello.diagnostics_default()));
        });
        paint();
    }
}

fn origin(address: &str) -> Option<String> {
    NexusOrigin::typed(address).ok().map(|origin| origin.origin().to_string())
}

pub(crate) fn offline_changed(on: bool) {
    SETTINGS.with_borrow_mut(|state| state.offline = on);
    emit(Happened::Offline(on));
    paint();
}

pub(super) fn in_use(on: bool) {
    if SETTINGS.with_borrow_mut(|state| std::mem::replace(&mut state.in_use, on)) != on {
        emit(Happened::InUse(on));
    }
}

pub(super) fn note_fault(at: u64) {
    FAULT.set(Some(FAULT.get().map_or(at, |prior| prior.min(at))));
}

pub(super) fn report_fault() {
    // Coalesce between snapshots. The schedule needs the first unreported fault's time.
    if SETTINGS.with_borrow(|state| state.send.is_some()) && let Some(at_ms) = FAULT.take() {
        emit(Happened::DiagnosticsFault { at_ms });
    }
}

pub(crate) fn on_worker(job: impl FnOnce() + Send + 'static) {
    worker().spawn(job);
}

fn choice_path(paths: &config::Paths) -> PathBuf {
    paths.settings.with_file_name(CHOICE_FILE)
}

fn schedule_path(paths: &config::Paths) -> PathBuf {
    paths.settings.with_file_name(SCHEDULE_FILE)
}

fn read_profile<T: serde::de::DeserializeOwned>(path: &Path, limit: usize) -> io::Result<Option<T>> {
    match read_limited(path, limit) {
        Ok(text) => serde_json::from_slice(&text).map_err(io::Error::from),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Runs off the window thread, before handing Kept to Client::start.
#[allow(dead_code)] // W7 owns the live client.
pub(crate) fn load_kept(paths: &config::Paths, kept: &mut Kept) {
    kept.diagnostics = match read_profile(&choice_path(paths), 8) {
        Ok(choice) => choice,
        Err(_) => {
            kept.unreadable.push(Forget::Diagnostics);
            None
        }
    };
    kept.diag_schedule = match read_profile(&schedule_path(paths), 2_048) {
        Ok(schedule) => schedule,
        Err(_) => {
            kept.unreadable.push(Forget::DiagSchedule);
            None
        }
    };
}

fn save_choice(path: &Path, choice: Option<bool>) -> io::Result<()> {
    config::write_surely(path, &serde_json::to_vec(&choice)?)
}

/// Forget an unreadable choice, unless a newer button press has already replaced it.
#[allow(dead_code)] // W7 owns the live client.
/// Says when it is done, and whether it held (W7c item 11: the sign-in's driver waits for it).
pub(crate) fn forget_choice() -> Option<std::sync::mpsc::Receiver<bool>> {
    let path = with(|ctl| choice_path(&ctl.paths))?;
    let (done, receipt) = std::sync::mpsc::channel();
    on_worker(move || {
        let held = read_profile::<bool>(&path, 8).is_ok() || save_choice(&path, None).is_ok();
        let _ = done.send(held);
    });
    Some(receipt)
}

fn save_schedule(path: &Path, schedule: Option<&DiagSchedule>) -> io::Result<()> {
    config::write_surely(path, &serde_json::to_vec(&schedule)?)
}

/// Queue this in the order core issued it. The subsequent read cannot start ahead of it.
#[allow(dead_code)] // W7 owns the live client.
/// Says when it is done, and whether it was written (W7c item 11: the sign-in's driver waits
/// for it).
pub(crate) fn keep_schedule(schedule: Option<DiagSchedule>) -> Option<std::sync::mpsc::Receiver<bool>> {
    let path = with(|ctl| schedule_path(&ctl.paths))?;
    let (done, receipt) = std::sync::mpsc::channel();
    on_worker(move || {
        let written = save_schedule(&path, schedule.as_ref()).is_ok();
        SCHEDULE_KEPT.set(written && schedule.is_some());
        let _ = done.send(written);
    });
    Some(receipt)
}

/// Queue the matching reply for W7 to feed back into the same live client.
#[allow(dead_code)] // W7 owns the live client.
pub(crate) fn read(read: u64, answer: impl FnOnce(Happened) + Send + 'static) {
    let paths = with(|ctl| ctl.paths.clone());
    on_worker(move || {
        let ready = SCHEDULE_KEPT.get() && STORE.with_borrow(|store| store.current);
        answer(read_for_machine(paths.as_ref(), read, ready));
    });
}

fn read_for_machine(paths: Option<&config::Paths>, read: u64, kept: bool) -> Happened {
    let text = paths.filter(|_| kept).and_then(|paths| report_text(paths).ok());
    Happened::DiagnosticsRead { read, text }
}

#[cfg(test)]
mod tests;
