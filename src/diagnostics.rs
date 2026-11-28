//! The same clean report for "What is sent" and "Save diagnostics".

use std::cell::{Cell, RefCell};
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

pub(crate) use diag::{FaultPlace, FaultShape, Line, Note, SoundOutput};

use crate::controller::{Controller, post, with};
use crate::{config, pool::Pool, ui};

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
                let text = session.text(&path)
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
    static SOUND_OUTPUT: Cell<SoundOutput> = const { Cell::new(SoundOutput::Unknown) };
    static OBSERVED: RefCell<VecDeque<Observation>> = const { RefCell::new(VecDeque::new()) };
}

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

pub(crate) fn fault(place: FaultPlace, shape: FaultShape) {
    observe(Line::Fault(diag::Fault::new(place, shape)));
}

pub(crate) fn connection_shape(error: &crate::subsonic::ApiError) -> FaultShape {
    match error {
        crate::subsonic::ApiError::Network(_) => FaultShape::Connection,
        crate::subsonic::ApiError::NotSubsonic(_) => FaultShape::UnexpectedAnswer,
        crate::subsonic::ApiError::Server { .. } => FaultShape::Refused,
    }
}

pub(crate) fn music_connection_failed(error: &crate::subsonic::ApiError) {
    fault(FaultPlace::MusicServer, connection_shape(error));
}

pub(crate) fn note_sound_output(kind: SoundOutput) {
    SOUND_OUTPUT.set(kind);
}

struct Session {
    https: bool,
    offline: bool,
    signed_in: bool,
    pc: diag::PcDetails,
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
        let mut session = Self::from_observations(&settings, ctl.client().is_some(), &said, observations);
        session.pc.music_version = if session.signed_in {
            diag::music_version(&ctl.ui.global::<ui::App>().get_server_version())
        } else {
            None
        };
        session.pc.unreachable = ctl.unreachable.get();
        session.pc.output = SOUND_OUTPUT.get();
        session.pc.library_songs = crate::search::current().map_or(0, |index| index.library.songs.len() as u64);
        session.pc.downloads = ctl.downloads.borrow().downloaded_count() as u64;
        session.pc.pending = (ctl.pending.borrow().len() as u64).saturating_add(ctl.sending.borrow().len() as u64);
        session
    }

    fn from_observations(
        settings: &config::Settings,
        signed_in: bool,
        said: &VecDeque<(i64, String)>,
        mut observations: Vec<Observation>,
    ) -> Self {
        observations.extend(said.iter().filter_map(|(at, _)| {
            let at = u64::try_from(*at).ok()?.checked_mul(1_000)?;
            Some((at, Line::Note(Note::AppMessage)))
        }));
        Self {
            https: settings.server.starts_with("https://"),
            offline: settings.offline,
            signed_in,
            pc: diag::PcDetails {
                exclusive: settings.exclusive,
                bitrate_kbps: settings.max_bitrate(),
                gapless: settings.gapless,
                crossfade_ms: (settings.crossfade * 1_000.0).round() as u32,
                levelling: settings.normalize.then(|| {
                    let level = match settings.volume_level {
                        0 => diag::Level::Quiet,
                        2 => diag::Level::Loud,
                        _ => diag::Level::Normal,
                    };
                    let mode = match settings.level_mode {
                        1 => diag::LevelBy::Song,
                        2 => diag::LevelBy::Album,
                        _ => diag::LevelBy::Automatic,
                    };
                    (level, mode)
                }),
                equaliser: settings.equalizer,
                mono: settings.mono,
                cache_limit_mb: settings.cache_limit_mb,
                sharing: settings.sync,
                requests: settings.requests,
                ..diag::PcDetails::default()
            },
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
                pc: Some(self.pc),
            },
            record,
        }
    }

    fn text(&self, crash_path: &Path) -> Result<String, diag::Wrong> {
        self.report(crash_path).text()
    }

    fn write_report(&self, crash_path: &Path, path: &Path) -> std::io::Result<()> {
        let text = self.text(crash_path)
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
        let location = panic.location().map(|at| (at.file(), at.line()));
        let at = Utc::now().timestamp_millis().try_into().unwrap_or_default();
        let _ = append_fault(&log, at, location, message);
    }));
}

fn append_fault(log: &Path, at: u64, location: Option<(&str, u32)>, message: &str) -> std::io::Result<()> {
    let fault = windows_observations::panic(location, message);
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
    use nexus_core::nexus::diag::{self, Fault, FaultPlace, Line, Source, SourceFile};

    use super::Observation;

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
        let location = location.rsplit_once(':')
            .and_then(|(file, line)| Some((file, line.parse().ok()?)));
        Some((at, panic(location, message)))
    }

    pub(super) fn panic(location: Option<(&str, u32)>, message: &str) -> Line {
        let source = location.map(|(path, line)| Source {
            file: SourceFile::from_location(path, env!("CARGO_MANIFEST_DIR")),
            line,
        });
        let place = match source.map(|source| source.file) {
            Some(SourceFile::Playback | SourceFile::Engine | SourceFile::Source | SourceFile::Dsp) => FaultPlace::Playback,
            Some(SourceFile::Exclusive) => FaultPlace::AudioOutput,
            Some(SourceFile::Downloads | SourceFile::Songcache) => FaultPlace::Downloads,
            Some(SourceFile::Subsonic | SourceFile::Offline) => FaultPlace::MusicServer,
            Some(SourceFile::Nexus) => FaultPlace::NexusServer,
            Some(SourceFile::Actions | SourceFile::Pages | SourceFile::Search | SourceFile::Playlists | SourceFile::Covers) => FaultPlace::Library,
            _ => FaultPlace::App,
        };
        Line::Fault(Fault { place, shape: diag::classify_rust_panic(message), source })
    }
}

#[cfg(test)]
mod tests;
