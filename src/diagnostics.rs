//! The report in "What is sent" and the fuller file in "Save diagnostics".

use std::collections::VecDeque;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, mpsc};
use std::time::Duration;

use chrono::{DateTime, Utc};
use nexus_core::nexus::{diag, events::DeviceKind, proto::Version};
use slint::ComponentHandle;

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
    let written = Utc::now();
    let session = Session::capture(ctl);
    let page = ctl.ui.global::<ui::Settings>();
    let local = LocalDetails {
        written,
        server_version: ctl.ui.global::<ui::App>().get_server_version().into(),
        output_status: page.get_output_status().into(),
        downloads: ctl.downloads.borrow().summary(),
        cache_size: page.get_cache_size().into(),
    };
    let crash_path = ctl.paths.crash_log.clone();
    let path = ctl.paths.diagnostics.clone();
    worker().spawn(move || {
        let result = local.write(&session, &crash_path, &path);
        match result {
            Ok(()) => {
                let program = if cfg!(windows) { "notepad" } else { "xdg-open" };
                let _ = std::process::Command::new(program).arg(&path).spawn();
                post(move |ctl| ctl.toast(format!("Saved to {}", path.display())));
            }
            Err(e) => post(move |ctl| ctl.toast(format!("Couldn't write {}. {e}", path.display()))),
        }
    });
}

struct Options {
    https: bool,
    offline: bool,
    exclusive: bool,
    bitrate: u32,
    gapless: bool,
    crossfade: f32,
    normalize: bool,
    equalizer: bool,
    cache_limit_mb: u32,
    sync: bool,
    requests: bool,
}

impl From<&config::Settings> for Options {
    fn from(settings: &config::Settings) -> Self {
        Self {
            https: settings.server.starts_with("https://"),
            offline: settings.offline,
            exclusive: settings.exclusive,
            bitrate: settings.max_bitrate(),
            gapless: settings.gapless,
            crossfade: settings.crossfade,
            normalize: settings.normalize,
            equalizer: settings.equalizer,
            cache_limit_mb: settings.cache_limit_mb,
            sync: settings.sync,
            requests: settings.requests,
        }
    }
}

/// An in-memory copy taken on the window's thread. Only the messages carry free text.
struct Session {
    options: Options,
    signed_in: bool,
    unreachable: bool,
    songs: usize,
    pending: usize,
    said: VecDeque<(i64, String)>,
}

impl Session {
    fn capture(ctl: &Controller) -> Self {
        Self {
            options: Options::from(&*ctl.settings.borrow()),
            signed_in: ctl.client().is_some(),
            unreachable: ctl.unreachable.get(),
            songs: crate::search::current().map_or(0, |index| index.library.songs.len()),
            pending: ctl.pending.borrow().len() + ctl.sending.borrow().len(),
            said: ctl.said.borrow().clone(),
        }
    }

    /// Settings renders this shared report. W6 can pair it with the backend's
    /// observations and pass the same report to C6's upload constructor.
    fn report(&self, crash_path: &Path) -> diag::Report {
        let crash = recent_crash_text(crash_path).unwrap_or_default();
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
                https: self.options.https,
                offline: self.options.offline,
                local_permission: None,
                android_auto: None,
            },
            record: windows_observations::record(&self.said, &crash),
        }
    }
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

// Adapt the old Windows toast and crash formats to C6's typed record. C6 owns the
// vocabulary, counting, limits and rendering. Only recognized observations enter it.
mod windows_observations {
    use std::collections::VecDeque;

    use chrono::NaiveDateTime;
    use nexus_core::nexus::diag::{Fault, FaultPlace, FaultShape, Line, Record};

    pub(super) fn record(said: &VecDeque<(i64, String)>, crash: &str) -> Record {
        let messages = said.iter().filter_map(|(at, text)| {
            let at = u64::try_from(*at).ok()?.checked_mul(1_000)?;
            Some((at, message(text)?))
        });
        let mut observations: Vec<_> = crash.lines().filter_map(fault).chain(messages).collect();
        observations.sort_by_key(|(at, _)| *at);
        let mut record = Record::default();
        for (at, line) in observations {
            record.note(at, line);
        }
        record
    }

    fn message(text: &str) -> Option<Line> {
        use FaultPlace::*;
        use FaultShape::*;

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
        })
    }

    fn fault(line: &str) -> Option<(u64, Line)> {
        // The panic hook writes this header before a free-form payload, which may span
        // multiple lines. Take the time and a known source area from the header.
        let (date, rest) = line.split_once(" UTC version ")?;
        if date.len() != 19 {
            return None;
        }
        let at = NaiveDateTime::parse_from_str(date, "%Y-%m-%d %H:%M:%S").ok()?.and_utc();
        let at = u64::try_from(at.timestamp_millis()).ok()?;
        let (_, rest) = rest.split_once(" thread ")?;
        let (_, rest) = rest.split_once(" at ")?;
        let (location, _) = rest.split_once(": ")?;
        let file = location.rsplit_once(':')
            .filter(|(_, line)| line.parse::<u32>().is_ok())
            .and_then(|(file, _)| file.rsplit(['/', '\\']).next());
        let place = match file {
            Some("playback.rs" | "engine.rs" | "source.rs" | "dsp.rs") => FaultPlace::Playback,
            Some("exclusive.rs" | "media.rs") => FaultPlace::AudioOutput,
            Some("downloads.rs" | "songcache.rs") => FaultPlace::Downloads,
            Some("subsonic.rs" | "offline.rs") => FaultPlace::MusicServer,
            Some("nexus.rs" | "signin.rs") => FaultPlace::NexusServer,
            Some("actions.rs" | "pages.rs" | "search.rs" | "playlists.rs" | "covers.rs") => FaultPlace::Library,
            Some("journal.rs") => FaultPlace::Journal,
            _ => FaultPlace::App,
        };
        // C6 does not have a shape for a Rust panic or its subtypes.
        Some((at, Line::Fault(Fault { place, shape: FaultShape::Other })))
    }
}

/// Strings only used in the local file. A server or device is free to choose these words.
struct LocalDetails {
    written: DateTime<Utc>,
    server_version: String,
    output_status: String,
    downloads: String,
    cache_size: String,
}

impl LocalDetails {
    fn write(&self, session: &Session, crash_path: &Path, path: &Path) -> std::io::Result<()> {
        let crash = fs::read_to_string(crash_path).ok();
        let text = self.text(session, crash.as_deref());
        if let Some(folder) = path.parent() {
            let _ = fs::create_dir_all(folder);
        }
        fs::write(path, text)
    }

    fn text(&self, session: &Session, crash: Option<&str>) -> String {
        let s = &session.options;
        let mut lines = vec![
            app_description(),
            format!("Written {}", self.written.format("%Y-%m-%d %H:%M:%S UTC")),
            String::new(),
            format!("Signed in: {}", yes(session.signed_in)),
            format!("Server version: {}", self.server_version),
            format!("Server address uses HTTPS: {}", yes(s.https)),
            format!("Offline mode: {}; server out of reach: {}", on(s.offline), on(session.unreachable)),
            format!("Sound output: {}", self.output_status),
            format!("Exclusive mode: {}; streaming limit: {} kbps (0 is none)", on(s.exclusive), s.bitrate),
            playback_options(s),
            format!("Library on this PC: {} songs", session.songs),
            format!("Downloads: {}", self.downloads),
            format!("Kept songs limit: {} MB; cache: {}", s.cache_limit_mb, self.cache_size),
            sharing_options(s),
            format!("Changes waiting to be sent: {}", session.pending),
            String::new(),
            "Recent messages:".into(),
        ];
        if session.said.is_empty() {
            lines.push("  (none this session)".into());
        }
        for (at, text) in &session.said {
            let when = DateTime::from_timestamp(*at, 0)
                .map(|at| at.format("%H:%M:%S").to_string())
                .unwrap_or_default();
            lines.push(format!("  {when} UTC  {text}"));
        }
        lines.push(String::new());
        lines.push("Times the app stopped by itself:".into());
        if let Some(log) = crash.filter(|log| !log.trim().is_empty()) {
            let all: Vec<_> = log.lines().collect();
            lines.extend(all[all.len().saturating_sub(40)..].iter().map(|line| format!("  {line}")));
        } else {
            lines.push("  (none noted)".into());
        }
        lines.join("\r\n")
    }
}

fn app_description() -> String {
    format!("Nexus Music {} on {} ({})", env!("CARGO_PKG_VERSION"), std::env::consts::OS, std::env::consts::ARCH)
}

fn yes(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn on(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn playback_options(s: &Options) -> String {
    format!("Gapless: {}; crossfade: {} s; levelling: {}; equaliser: {}", on(s.gapless), s.crossfade, on(s.normalize), on(s.equalizer))
}

fn sharing_options(s: &Options) -> String {
    format!("Sharing between devices: {}; requests through Lidarr: {}", on(s.sync), on(s.requests))
}

#[cfg(test)]
mod tests;
