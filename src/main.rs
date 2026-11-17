// Release builds are a windowed app: no console window behind it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod audio;
mod config;
mod controller;
mod covers;
mod downloads;
#[cfg(windows)]
mod instance;
#[cfg(windows)]
mod media;
mod pages;
mod playback;
mod pool;
mod subsonic;
mod update;

/// Everything generated from the `.slint` files.
pub mod ui {
    slint::include_modules!();
}

use slint::ComponentHandle;

fn main() -> Result<(), slint::PlatformError> {
    // Opening the app a second time shows the copy that's already running.
    #[cfg(windows)]
    if !instance::claim() {
        return Ok(());
    }
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
