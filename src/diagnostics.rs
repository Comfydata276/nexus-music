//! The same clean report for Settings, local export, and the Nexus client.

use std::cell::{Cell, RefCell};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

use chrono::Utc;
use nexus_core::nexus::{account::DeviceKind, diag, proto::Version};
use slint::ComponentHandle;

pub(crate) use diag::{FaultPlace, FaultShape, Line, Note, SoundOutput};

use crate::controller::{Controller, post, with};
use crate::{config, pool::Pool, ui};

mod sending;
#[allow(unused_imports)] // The live client is installed by W7.
pub(crate) use sending::{attach, detach, forget_choice, keep_schedule_for_window as keep_schedule, load_kept, on_worker, publish_hello, read_for_window as read};

thread_local! {
    static STORE: RefCell<ReportStore> = RefCell::default();
    static REFRESH: slint::Timer = slint::Timer::default();
}

static CHECKPOINT_PENDING: AtomicBool = AtomicBool::new(false);
const RECORD_FILE: &str = "diagnostics-record.txt";

// Keep report building and file I/O off the window thread. One worker keeps repeated
// exports and previews in the order they were requested.
static WORKER: OnceLock<Pool> = OnceLock::new();

fn worker() -> &'static Pool {
    WORKER.get_or_init(|| Pool::new("diagnostics", 1, false))
}

/// Finish the last checkpoint and any requested export after the window loop ends.
pub fn finish(ctl: &Controller) {
    REFRESH.with(slint::Timer::stop);
    sending::in_use(false);
    sending::detach();
    let paths = ctl.paths.clone();
    let session = Session::capture(ctl);
    let (done, wait) = mpsc::channel();
    worker().spawn(move || {
        let _ = checkpoint(&paths, session);
        let _ = done.send(());
    });
    let _ = wait.recv_timeout(Duration::from_secs(5));
}

pub fn install(ctl: &Controller) {
    static PREVIEW: AtomicU64 = AtomicU64::new(0);
    let paths = ctl.paths.clone();
    worker().spawn(move || {
        if let Some(folder) = paths.crash_log.parent() {
            config::clear_leftovers(folder, |name| matches!(name, RECORD_FILE | "diagnostics.txt"), &[]);
        }
    });
    sending::install(ctl);
    ctl.ui.global::<ui::Settings>().on_preview_diagnostics(|| {
        with(|ctl| {
            let session = Session::capture(ctl);
            let paths = ctl.paths.clone();
            let generation = PREVIEW.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            worker().spawn(move || {
                let text = preview_text(&paths, session);
                post(move |ctl| {
                    if PREVIEW.load(Ordering::Relaxed) == generation {
                        ctl.ui.global::<ui::Settings>().set_diagnostics_preview(text.into());
                    }
                });
            });
        });
    });
    refresh(ctl);
    REFRESH.with(|timer| timer.start(slint::TimerMode::Repeated, Duration::from_secs(2), || {
        with(|ctl| refresh(ctl));
    }));
}

fn refresh(ctl: &Controller) {
    // Bound the periodic work to one outstanding snapshot, even if the disk stalls.
    if !CHECKPOINT_PENDING.swap(true, Ordering::Relaxed) {
        let paths = ctl.paths.clone();
        let session = Session::capture(ctl);
        worker().spawn(move || {
            let _ = checkpoint(&paths, session);
            CHECKPOINT_PENDING.store(false, Ordering::Relaxed);
        });
    }
    let player = ctl.ui.global::<ui::Player>();
    sending::in_use(ctl.diagnostics_window_visible() || player.get_playing() || player.get_buffering());
    sending::report_fault();
}

pub fn save(ctl: &Controller) {
    let session = Session::capture(ctl);
    let paths = ctl.paths.clone();
    worker().spawn(move || {
        let saved = export_report(&paths, session);
        if saved.is_ok() {
            let program = if cfg!(windows) { "notepad" } else { "xdg-open" };
            let _ = std::process::Command::new(program).arg(&paths.diagnostics).spawn();
            post(|ctl| ctl.toast("Diagnostics saved."));
        } else {
            post(|ctl| {
                fault(FaultPlace::App, FaultShape::Write);
                ctl.toast("Couldn't save diagnostics.");
            });
        }
    });
}

type Observation = (u64, diag::Line);

thread_local! {
    // Window and native media callbacks add closed kinds without touching a worker or a file.
    static SOUND_OUTPUT: Cell<SoundOutput> = const { Cell::new(SoundOutput::Unknown) };
    static OBSERVED: RefCell<diag::Record> = RefCell::default();
}

pub fn observe(line: diag::Line) {
    if let Ok(at) = u64::try_from(Utc::now().timestamp_millis()) {
        OBSERVED.with_borrow_mut(|observed| observed.note(at, line));
        if matches!(line, Line::Fault(_)) {
            sending::note_fault(at);
        }
    }
}

/// The crash log can contain old free text. Read the timestamp through its typed projection.
pub(crate) fn prior_crash(path: &Path) {
    let path = path.to_owned();
    worker().spawn(move || {
        let Some(at) = crash_observations(&path).map(|(at, _)| at).max() else { return };
        post(move |_| sending::note_fault(at));
    });
}

pub(crate) use sending::{offline_changed, publish_account};

pub(crate) fn fault(place: FaultPlace, shape: FaultShape) {
    use crate::journal::FaultPlace as JournalPlace;
    let journal_place = match place {
        FaultPlace::Playback => JournalPlace::Playback,
        FaultPlace::AudioOutput => JournalPlace::AudioOutput,
        FaultPlace::Library => JournalPlace::Library,
        FaultPlace::Downloads => JournalPlace::Downloads,
        FaultPlace::Foreground => JournalPlace::Foreground,
        FaultPlace::MusicServer => JournalPlace::MusicServer,
        FaultPlace::NexusServer => JournalPlace::NexusServer,
        FaultPlace::Journal => JournalPlace::Journal,
        FaultPlace::Updates | FaultPlace::App => JournalPlace::App,
    };
    crate::journal::fault(crate::journal::Fault { place: journal_place, shape });
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
    about: diag::About,
    observations: diag::Record,
}

impl Session {
    /// Copy only settings and counts on the window thread. The reader uses the
    /// checkpoint and does not call the controller or player.
    fn capture(ctl: &Controller) -> Self {
        let settings = ctl.settings.borrow();
        let signed_in = ctl.client().is_some();
        let mut about = Self::about(&settings, signed_in);
        let pc = about.pc.as_mut().expect("PC report");
        pc.music_version = if signed_in {
            diag::music_version(&ctl.ui.global::<ui::App>().get_server_version())
        } else {
            None
        };
        pc.unreachable = ctl.unreachable.get();
        pc.output = SOUND_OUTPUT.get();
        pc.library_songs = crate::search::current().map_or(0, |index| index.library.songs.len() as u64);
        pc.downloads = ctl.downloads.borrow().downloaded_count() as u64;
        pc.pending = (ctl.pending.borrow().len() as u64).saturating_add(ctl.sending.borrow().len() as u64);
        let mut observations = OBSERVED.take();
        count_notices(&mut observations, ctl.said.borrow_mut().drain(..));
        Self { about, observations }
    }

    fn about(settings: &config::Settings, signed_in: bool) -> diag::About {
        diag::About {
            version: Version {
                major: env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap_or_default(),
                minor: env!("CARGO_PKG_VERSION_MINOR").parse().unwrap_or_default(),
                patch: env!("CARGO_PKG_VERSION_PATCH").parse().unwrap_or_default(),
            },
            device: DeviceKind::Pc,
            os: os_version(),
            signed_in,
            https: settings.server.starts_with("https://"),
            offline: settings.offline,
            local_permission: None,
            android_auto: None,
            loss: Default::default(),
            pc: Some(diag::PcDetails {
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
            }),
        }
    }
}

fn count_notices(record: &mut diag::Record, notices: impl IntoIterator<Item = (i64, String)>) {
    for (at, _) in notices {
        if let Some(at) = u64::try_from(at).ok().and_then(|at| at.checked_mul(1_000)) {
            record.note(at, Line::Note(Note::AppMessage));
        }
    }
}

#[derive(Default)]
struct ReportStore {
    pending: diag::Record,
    current: bool,
}

impl ReportStore {
    fn checkpoint(&mut self, path: &Path, session: Session) -> std::io::Result<()> {
        self.pending.merge_later(session.observations);
        let previous = match read_record(path) {
            Ok(report) => Some(report),
            Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData) => None,
            Err(error) => return Err(error),
        };
        if self.pending.entries().is_empty() && previous.as_ref().is_some_and(|report| report.about == session.about) {
            return Ok(());
        }
        let mut record = previous.map_or_else(diag::Record::default, |report| report.record);
        record.merge_later(self.pending.clone());
        let text = diag::Report { about: session.about, record }.text().map_err(invalid_report)?;
        if let Some(folder) = path.parent() {
            fs::create_dir_all(folder)?;
        }
        config::write_surely(path, text.as_bytes())?;
        self.pending.clear();
        Ok(())
    }
}

fn record_path(paths: &config::Paths) -> std::path::PathBuf {
    paths.crash_log.with_file_name(RECORD_FILE)
}

fn checkpoint(paths: &config::Paths, session: Session) -> std::io::Result<()> {
    STORE.with_borrow_mut(|store| {
        let result = store.checkpoint(&record_path(paths), session);
        store.current = result.is_ok();
        result
    })
}

fn read_limited(path: &Path, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    fs::File::open(path)?.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    Ok(bytes)
}

fn invalid_report(_: diag::Wrong) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "The diagnostics report could not be read")
}

fn read_record(path: &Path) -> std::io::Result<diag::Report> {
    let bytes = read_limited(path, diag::CALL_BYTES)?;
    let text = std::str::from_utf8(&bytes).map_err(|_| std::io::ErrorKind::InvalidData)?;
    diag::Report::read(text).map_err(invalid_report)
}

fn crash_observations(path: &Path) -> impl Iterator<Item = Observation> {
    recent_crash_text(path).unwrap_or_default().lines()
        .filter_map(windows_observations::fault).collect::<Vec<_>>().into_iter()
}

fn report_text(paths: &config::Paths) -> std::io::Result<String> {
    render(read_record(&record_path(paths))?, &paths.crash_log)
}

// Keep the loss totals visible even when the profile cannot write its checkpoint.
// Only the local commands can use the in-memory observations in this case.
fn local_text(paths: &config::Paths, session: Session) -> std::io::Result<String> {
    let about = session.about;
    if checkpoint(paths, session).is_ok() {
        return report_text(paths);
    }
    let mut record = read_record(&record_path(paths)).map_or_else(|_| diag::Record::default(), |report| report.record);
    STORE.with_borrow(|store| record.merge_later(store.pending.clone()));
    render(diag::Report { about, record }, &paths.crash_log)
}

fn render(mut report: diag::Report, crash_path: &Path) -> std::io::Result<String> {
    report.about.loss = crate::journal::losses();

    // Keep the latest observations in order across the record and the separate crash tail.
    let mut entries = report.record.entries().iter().copied().collect::<Vec<_>>();
    entries.extend(crash_observations(crash_path).map(|(at, line)| diag::Entry {
        first_ms: at, last_ms: at, count: 1, line,
    }));
    entries.sort_by_key(|entry| entry.last_ms);
    report.record.clear();
    for entry in entries {
        report.record.note(entry.first_ms, entry.line);
        if entry.count > 1 {
            report.record.note_times(entry.last_ms, entry.line, entry.count - 1);
        }
    }
    report.text().map_err(invalid_report)
}

fn preview_text(paths: &config::Paths, session: Session) -> String {
    local_text(paths, session).unwrap_or_else(|_| "Diagnostics could not be read.".into())
}

fn export_report(paths: &config::Paths, session: Session) -> std::io::Result<()> {
    let text = local_text(paths, session)?;
    config::write_surely(&paths.diagnostics, text.as_bytes())
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
            Some(SourceFile::Journal | SourceFile::JournalReader | SourceFile::JournalWriter) => FaultPlace::Journal,
            Some(SourceFile::Actions | SourceFile::Pages | SourceFile::Search | SourceFile::Playlists | SourceFile::Covers) => FaultPlace::Library,
            _ => FaultPlace::App,
        };
        Line::Fault(Fault { place, shape: diag::classify_rust_panic(message), source })
    }
}

#[cfg(test)]
mod tests;
