// Release builds are a windowed app: no console window behind it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod asked;
mod added;
mod audio;
mod bulk;
mod config;
mod controller;
mod corehttp;
mod covers;
mod discover;
mod downloads;
mod import;
#[cfg(windows)]
mod frame;
mod gate;
#[cfg(windows)]
mod instance;
mod jam;
mod keep;
mod link;
#[cfg(windows)]
mod media;
mod lidarr;
mod lyrics;
mod listenbrainz;
mod mixes;
mod musicbrainz;
// The Nexus server's component: installed here, after the controller, and named by nothing
// that plays music (see its own notes).
mod nexus;
mod offline;
mod pages;
#[cfg(test)]
mod pins;
#[cfg(windows)]
mod picker;
mod playback;
mod playlists;
mod plays;
mod pool;
mod saver;
mod search;
mod songcache;
mod subsonic;
mod sync;
#[cfg(windows)]
mod taskbar;
mod update;
mod ways;

/// Everything generated from the `.slint` files.
pub mod ui {
    slint::include_modules!();
}

use slint::ComponentHandle;

/// Applies the "Interface size" setting. The toolkit reads its scale once, before the first
/// window exists, which is why changing the setting takes a restart.
fn apply_interface_size() {
    let paths = config::Paths::new();
    let scale = config::Settings::load(&paths.settings).ui_scale;
    if (scale - 1.0).abs() < 0.01 || std::env::var_os("SLINT_SCALE_FACTOR").is_some() {
        return;
    }
    #[cfg(windows)]
    let system = instance::system_scale();
    #[cfg(not(windows))]
    let system = 1.0;
    let factor = (system * scale.clamp(0.75, 1.5)).clamp(0.5, 4.0);
    // Safe here: nothing else is running yet.
    unsafe { std::env::set_var("SLINT_SCALE_FACTOR", format!("{factor:.3}")) };
}

/// Has anything that brings the app down written down what it was first. A release build has
/// no window to print to and simply vanishes; this leaves a note of why, which "Save
/// diagnostics" in Settings includes.
fn keep_a_crash_log() {
    let log = config::Paths::new().crash_log;
    std::panic::set_hook(Box::new(move |panic| {
        use std::io::Write;
        let what = panic.payload().downcast_ref::<&str>().map(|text| text.to_string()).or_else(|| panic.payload().downcast_ref::<String>().cloned());
        let place = panic.location().map(|at| format!("{}:{}", at.file(), at.line())).unwrap_or_default();
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        if let Some(folder) = log.parent() {
            let _ = std::fs::create_dir_all(folder);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
            let _ = writeln!(
                file,
                "{} version {} thread {thread} at {place}: {}",
                chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
                env!("CARGO_PKG_VERSION"),
                what.unwrap_or_else(|| "no message".into())
            );
        }
    }));
}

fn main() -> Result<(), slint::PlatformError> {
    // Started as one look of the Nexus server's component: that, and nothing of the app.
    if let Some(code) = nexus::probe_mode() {
        std::process::exit(code);
    }
    keep_a_crash_log();
    // Opening the app a second time shows the copy that's already running.
    #[cfg(windows)]
    if !instance::claim() {
        return Ok(());
    }
    apply_interface_size();
    let window = ui::MainWindow::new()?;
    let controller = controller::Controller::install(window);
    // After the controller is made (and a saved sign-in entered), never by it; and only when
    // it is switched on (NEXUS_MUSIC_NEXUS=1).
    if nexus::switched_on() {
        nexus::install(&controller.ui);
    }
    controller.ui.show()?;
    // The loop is ended explicitly (see `Controller::quit`): with a tray icon present it
    // would otherwise outlive the window.
    slint::run_event_loop_until_quit()?;
    // Remember where playback was for next time.
    controller.save_queue();
    // The last write of the settings, and the one that waits for the disk.
    controller.save_settings();
    controller.downloads.borrow_mut().save_if_due(true);
    // The queue is written by a thread of its own: wait for it to finish.
    controller.saver.flush();
    Ok(())
}
