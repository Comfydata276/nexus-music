// Release builds are a windowed app: no console window behind it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod audio;
mod config;
mod controller;
mod covers;
#[cfg(windows)]
mod media;
mod pages;
mod playback;
mod pool;
mod subsonic;

/// Everything generated from the `.slint` files.
pub mod ui {
    slint::include_modules!();
}

use slint::ComponentHandle;

fn main() -> Result<(), slint::PlatformError> {
    let window = ui::MainWindow::new()?;
    let controller = controller::Controller::install(window);
    controller.ui.run()?;
    // Remember where playback was for next time.
    controller.save_queue();
    controller.save_settings();
    Ok(())
}
