//! Keeps Nexus Music to one running copy: opening it again brings the first to the front.

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows_sys::Win32::System::Threading::{CreateEventW, INFINITE, SetEvent, WaitForSingleObject};
use windows_sys::Win32::UI::WindowsAndMessaging::{ASFW_ANY, AllowSetForegroundWindow, SetForegroundWindow};

/// A named event that the first copy creates and later copies find.
const NAME: &str = "Local\\NexusMusic.Running";

struct Signal(HANDLE);
// The handle is only ever waited on, from the one thread it is moved to.
unsafe impl Send for Signal {}

/// Returns false when another copy is already running; that copy has been asked to show
/// its window, and this one should exit.
pub fn claim() -> bool {
    // A separate profile (used for testing) is a separate app.
    if std::env::var_os("NEXUS_MUSIC_HOME").is_some() {
        return true;
    }
    let name: Vec<u16> = NAME.encode_utf16().chain(Some(0)).collect();
    let handle = unsafe { CreateEventW(std::ptr::null(), 0, 0, name.as_ptr()) };
    if handle.is_null() {
        return true;
    }
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        unsafe {
            // Windows only lets a background app take the foreground if the app in front allows it.
            AllowSetForegroundWindow(ASFW_ANY);
            SetEvent(handle);
        }
        return false;
    }
    let signal = Signal(handle);
    let _ = std::thread::Builder::new().name("instance".into()).spawn(move || {
        let signal = signal;
        while unsafe { WaitForSingleObject(signal.0, INFINITE) } == 0 {
            crate::controller::post(|ctl| ctl.show_window());
        }
    });
    true
}

/// Brings the window in front of other apps.
pub fn focus(ui: &crate::ui::MainWindow) {
    let window = ui.window().window_handle();
    let Ok(handle) = window.window_handle() else { return };
    if let RawWindowHandle::Win32(win32) = handle.as_raw() {
        unsafe {
            SetForegroundWindow(win32.hwnd.get() as *mut std::ffi::c_void);
        }
    }
}
