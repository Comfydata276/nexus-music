//! The same clean report for "What is sent" and "Save diagnostics".

use std::cell::RefCell;
use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

use chrono::Utc;
use nexus_core::nexus::{account::DeviceKind, diag, events::LossCounts, proto::Version};
use slint::ComponentHandle;

use crate::controller::{Controller, post, with};
use crate::{config, pool::Pool, ui};

#[cfg(windows)]
pub(crate) use diag::{Line, Note};

// Keep report building and file I/O off the window thread. One worker keeps repeated
// exports and previews in the order they were requested.
static WORKER: OnceLock<Pool> = OnceLock::new();

fn worker() -> &'static Pool {
    WORKER.get_or_init(|| Pool::new("diagnostics", 1, false))
}

/// Let an export finish if the person quits as soon as they have asked for it.
pub fn finish() {
    if let Some(worker) = WORKER.get() {
        let (done, wait) = mpsc::channel();
        worker.spawn(move || {
            let _ = done.send(());
        });
        let _ = wait.recv_timeout(Duration::from_secs(5));
    }
}

pub fn install(window: &ui::MainWindow) {
    static PREVIEW: AtomicU64 = AtomicU64::new(0);
    window.global::<ui::Settings>().on_preview_diagnostics(|| {
        with(|ctl| {
            let session = Session::capture(ctl);
            let path = ctl.paths.crash_log.clone();
            let generation = PREVIEW.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            worker().spawn(move || {
                let text = session.report(&path).text()
                    .unwrap_or_else(|_| "Diagnostics could not be read.".into());
                post(move |ctl| {
                    if PREVIEW.load(Ordering::Relaxed) == generation {
                        ctl.ui.global::<ui::Settings>().set_diagnostics_preview(text.into());
                    }
                });
            });
        });
    });
}

pub fn save(ctl: &Controller) {
    let session = Session::capture(ctl);
    let crash_path = ctl.paths.crash_log.clone();
    let path = ctl.paths.diagnostics.clone();
    worker().spawn(move || {
        if session.write_report(&crash_path, &path).is_ok() {
            let program = if cfg!(windows) { "notepad" } else { "xdg-open" };
            let _ = std::process::Command::new(program).arg(&path).spawn();
            post(|ctl| ctl.toast("Diagnostics saved."));
        } else {
            post(|ctl| ctl.toast("Couldn't save diagnostics."));
        }
    });
}

type Observation = (u64, diag::Line);

thread_local! {
    // Called by the native window and media callbacks on the window's thread.
    static OBSERVED: RefCell<VecDeque<Observation>> = const { RefCell::new(VecDeque::new()) };
}

#[cfg(any(windows, test))]
pub fn observe(line: diag::Line) {
    if let Ok(at) = u64::try_from(Utc::now().timestamp_millis()) {
        OBSERVED.with_borrow_mut(|observed| {
            if observed.len() == diag::MAX_ENTRIES {
                observed.pop_front();
            }
            observed.push_back((at, line));
        });
    }
}

struct Session {
    https: bool,
    offline: bool,
    signed_in: bool,
    observations: Vec<Observation>,
}

impl Session {
    /// Take the small in-memory snapshot on the window thread. No raw message leaves it.
    fn capture(ctl: &Controller) -> Self {
        let settings = ctl.settings.borrow();
        let said = ctl.said.borrow();
        let observations = OBSERVED.with_borrow(|observed| {
            observed.iter().copied().collect::<Vec<_>>()
        });
        Self::from_observations(&settings, ctl.client().is_some(), &said, observations)
    }

    fn from_observations(
        settings: &config::Settings,
        signed_in: bool,
        said: &VecDeque<(i64, String)>,
        mut observations: Vec<Observation>,
    ) -> Self {
        observations.extend(said.iter().filter_map(|(at, text)| {
            let at = u64::try_from(*at).ok()?.checked_mul(1_000)?;
            Some((at, windows_observations::message(text)))
        }));
        Self {
            https: settings.server.starts_with("https://"),
            offline: settings.offline,
            signed_in,
            observations,
        }
    }

    fn report(&self, crash_path: &Path) -> diag::Report {
        let crash = recent_crash_text(crash_path).unwrap_or_default();
        let mut observations: Vec<_> = crash.lines().filter_map(windows_observations::fault).collect();
        observations.extend_from_slice(&self.observations);
        observations.sort_by_key(|(at, _)| *at);
        let mut record = diag::Record::default();
        for (at, line) in observations {
            record.note(at, line);
        }
        diag::Report {
            about: diag::About {
                version: Version {
                    major: env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or_default(),
                    minor: env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or_default(),
                    patch: env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or_default(),
                },
                device: DeviceKind::Pc,
                os: os_version(),
                signed_in: self.signed_in,
                https: self.https,
                offline: self.offline,
                local_permission: None,
                android_auto: None,
                // The Windows journal will supply its totals when W8 exists.
                loss: LossCounts::default(),
            },
            record,
        }
    }

    fn write_report(&self, crash_path: &Path, path: &Path) -> std::io::Result<()> {
        let text = self.report(crash_path).text()
            .map_err(|_| std::io::Error::other("The diagnostics report could not be built"))?;
        if let Some(folder) = path.parent() {
            fs::create_dir_all(folder)?;
        }
        fs::write(path, text)
    }
}

/// Record the time, C6's shape, and the app area before opening the crash log.
pub fn install_panic_hook() {
    let log = config::Paths::new().crash_log;
    std::panic::set_hook(Box::new(move |panic| {
        let message = panic.payload().downcast_ref::<&str>().copied()
            .or_else(|| panic.payload().downcast_ref::<String>().map(String::as_str))
            .unwrap_or("no payload");
        let path = panic.location().map(|at| at.file()).unwrap_or_default();
        let at = Utc::now().timestamp_millis().try_into().unwrap_or_default();
        let _ = append_fault(&log, at, path, message);
    }));
}

fn append_fault(log: &Path, at: u64, path: &str, message: &str) -> std::io::Result<()> {
    let fault = diag::Line::Fault(diag::Fault {
        place: windows_observations::place(path),
        shape: diag::classify_rust_panic(message),
    });
    let line = format!("{at}\t{fault}\n");
    if let Some(folder) = log.parent() {
        fs::create_dir_all(folder)?;
    }
    fs::OpenOptions::new().create(true).append(true).open(log)?.write_all(line.as_bytes())
}

const UNKNOWN_OS: Version = Version { major: 0, minor: 0, patch: 0 };

#[cfg(not(windows))]
fn os_version() -> Version {
    UNKNOWN_OS
}

#[cfg(windows)]
fn os_version() -> Version {
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

    windows::core::link!("ntdll.dll" "system" fn RtlGetVersion(info: *mut OSVERSIONINFOW) -> i32);
    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // The initialized C structure has the size the system call expects and lives
    // through the call. Only the numeric version fields are taken from the answer.
    if unsafe { RtlGetVersion(&mut info) } == 0 {
        Version { major: info.dwMajorVersion, minor: info.dwMinorVersion, patch: info.dwBuildNumber }
    } else {
        UNKNOWN_OS
    }
}

const CRASH_WINDOW: u64 = 64 * 1024;

/// Only the recent part is needed for the preview, even when the local log is large.
/// If the read started mid-line, discard that first fragment.
fn recent_crash_text(path: &Path) -> std::io::Result<String> {
    let mut log = fs::File::open(path)?;
    let offset = log.metadata()?.len().saturating_sub(CRASH_WINDOW);
    log.seek(SeekFrom::Start(offset.saturating_sub(1)))?;
    let mut tail = Vec::new();
    log.take(CRASH_WINDOW + u64::from(offset > 0)).read_to_end(&mut tail)?;
    let text = String::from_utf8_lossy(&tail);
    let text = if offset > 0 {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    Ok(text.to_owned())
}

// Decode Windows' observations into C6's vocabulary. The log can still contain records
// from older versions. Only the time, a fixed place and C6's classified shape leave here.
mod windows_observations {
    use chrono::NaiveDateTime;
    use nexus_core::nexus::diag::{self, Fault, FaultPlace, FaultShape, Line, Note};

    use super::Observation;

    pub(super) fn message(text: &str) -> Line {
        use FaultPlace::*;
        use FaultShape::*;

        if text.starts_with("Your downloads are now in ") {
            return Line::Note(Note::DownloadsFolderChanged);
        }

        const FAULTS: &[(&str, FaultPlace, FaultShape)] = &[
            ("Lost the connection to the server.", MusicServer, Connection),
            ("Can't reach your server.", MusicServer, Unavailable),
            ("Can't reach the server.", MusicServer, Unavailable),
            ("Couldn't play ", Playback, Other),
            ("No sound device found,", AudioOutput, NoSound),
            ("That isn't downloaded,", Playback, Unavailable),
            ("Couldn't write ", App, Write),
            ("Your downloads have moved to ", Downloads, Write),
            ("Not enough room there: the downloads come to ", Downloads, Write),
        ];
        FAULTS.iter().find_map(|&(prefix, place, shape)| {
            text.starts_with(prefix).then_some(Line::Fault(Fault { place, shape }))
        }).unwrap_or(Line::Note(Note::AppMessage))
    }

    pub(super) fn fault(line: &str) -> Option<Observation> {
        if let Some((at, text)) = line.split_once('\t') {
            if let (Ok(at), Some(line @ Line::Fault(_))) = (at.parse(), diag::clean_line(text)) {
                return Some((at, line));
            }
        }
        legacy_fault(line)
    }

    fn legacy_fault(line: &str) -> Option<Observation> {
        let (date, rest) = line.split_once(" UTC version ")?;
        if date.len() != 19 {
            return None;
        }
        let at = NaiveDateTime::parse_from_str(date, "%Y-%m-%d %H:%M:%S").ok()?.and_utc();
        let at = u64::try_from(at.timestamp_millis()).ok()?;
        let (_, rest) = rest.split_once(" thread ")?;
        let (_, rest) = rest.split_once(" at ")?;
        let (location, message) = rest.split_once(": ")?;
        let path = location.rsplit_once(':')
            .filter(|(_, line)| line.parse::<u32>().is_ok())
            .map_or("", |(file, _)| file);
        Some((at, Line::Fault(Fault {
            place: place(path),
            shape: diag::classify_rust_panic(message),
        })))
    }

    pub(super) fn place(path: &str) -> FaultPlace {
        match path.rsplit(['/', '\\']).next() {
            Some("playback.rs" | "engine.rs" | "source.rs" | "dsp.rs") => FaultPlace::Playback,
            Some("exclusive.rs" | "media.rs") => FaultPlace::AudioOutput,
            Some("downloads.rs" | "songcache.rs") => FaultPlace::Downloads,
            Some("subsonic.rs" | "offline.rs") => FaultPlace::MusicServer,
            Some("nexus.rs" | "signin.rs") => FaultPlace::NexusServer,
            Some("actions.rs" | "pages.rs" | "search.rs" | "playlists.rs" | "covers.rs") => FaultPlace::Library,
            Some("journal.rs") => FaultPlace::Journal,
            _ => FaultPlace::App,
        }
    }
}

#[cfg(test)]
mod tests;
