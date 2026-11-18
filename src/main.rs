// Release builds are a windowed app: no console window behind it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod asked;
mod added;
mod audio;
mod config;
mod controller;
mod covers;
mod discover;
mod downloads;
mod import;
#[cfg(windows)]
mod frame;
mod gate;
#[cfg(windows)]
mod instance;
#[cfg(windows)]
mod media;
mod lidarr;
mod lyrics;
mod listenbrainz;
mod mixes;
mod musicbrainz;
mod offline;
mod pages;
#[cfg(windows)]
mod picker;
mod playback;
mod plays;
mod pool;
mod search;
mod songcache;
mod subsonic;
mod sync;
#[cfg(windows)]
mod taskbar;
mod update;

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
    // Opening the app a second time shows the copy that's already running.
    #[cfg(windows)]
    if !instance::claim() {
        return Ok(());
    }
    apply_interface_size();
    let window = ui::MainWindow::new()?;
    let controller = controller::Controller::install(window);
    controller.ui.show()?;
    // The loop is ended explicitly (see `Controller::quit`): with a tray icon present it
    // would otherwise outlive the window.
    slint::run_event_loop_until_quit()?;
    // Remember where playback was for next time.
    controller.save_queue();
    controller.save_settings();
    Ok(())
}
