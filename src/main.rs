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
mod diagnostics;
mod discover;
mod downloads;
mod import;
#[cfg(windows)]
mod frame;
mod gate;
#[cfg(windows)]
mod instance;
mod keep;
#[cfg(windows)]
mod media;
mod lidarr;
mod lyrics;
mod listenbrainz;
mod mixes;
mod musicbrainz;
// The Nexus server's component, which signs in: installed here, after the controller, and
// named by nothing that plays music (see its own notes).
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

fn main() -> Result<(), slint::PlatformError> {
    // Started as one look of the Nexus server's component: that, and nothing of the app.
    if let Some(code) = nexus::probe_mode() {
        std::process::exit(code);
    }
    diagnostics::install_panic_hook();
    // Opening the app a second time shows the copy that's already running.
    #[cfg(windows)]
    if !instance::claim() {
        return Ok(());
    }
    apply_interface_size();
    let window = ui::MainWindow::new()?;
    let controller = controller::Controller::install(window);
    diagnostics::install(&controller.ui);
    // After the controller is made (and a kept sign-in entered), never by it.
    nexus::install(&controller.ui);
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
    diagnostics::finish();
    // What the Nexus server's component still has running is ended (at most two seconds).
    nexus::shutdown();
    Ok(())
}
