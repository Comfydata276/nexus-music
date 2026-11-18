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

/// How much Windows is set to enlarge things on the main display (1.0 is 100%).
pub fn system_scale() -> f32 {
    use windows_sys::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForSystem, SetProcessDpiAwarenessContext,
    };
    unsafe {
        // Without declaring this first, Windows reports 100% whatever the real setting is.
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        (GetDpiForSystem() as f32 / 96.0).clamp(1.0, 4.0)
    }
}

/// Colours the window's title bar and border to match the theme (Windows 11; on Windows 10
/// the title bar goes dark instead). Colours are 0xRRGGBB.
pub fn tint_title_bar(window: &slint::Window, background: u32, text: u32) {
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;
    // Not all of these are in every version of the bindings, so they're spelled out.
    const USE_IMMERSIVE_DARK_MODE: u32 = 20;
    const BORDER_COLOR: u32 = 34;
    const CAPTION_COLOR: u32 = 35;
    const TEXT_COLOR: u32 = 36;
    let window = window.window_handle();
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else { return };
    let hwnd = win32.hwnd.get() as *mut std::ffi::c_void;
    // Windows wants blue, green, red.
    let colorref = |rgb: u32| ((rgb & 0xff) << 16) | (rgb & 0xff00) | ((rgb >> 16) & 0xff);
    let set = |attribute: u32, value: u32| unsafe {
        DwmSetWindowAttribute(hwnd, attribute, (&raw const value).cast(), 4);
    };
    set(USE_IMMERSIVE_DARK_MODE, 1);
    set(CAPTION_COLOR, colorref(background));
    set(BORDER_COLOR, colorref(background));
    set(TEXT_COLOR, colorref(text));
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
