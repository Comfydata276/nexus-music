//! The app's own title bar on Windows.
//!
//! The window keeps its normal frame (so resizing, snapping, the shadow and rounded corners
//! all still come from Windows), but the title bar Windows would draw is taken out and the
//! top of the app's interface is treated as one: its empty parts drag the window.

use std::cell::Cell;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi};
use windows::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, HTCAPTION, HTCLIENT, HTTOP, HTTOPLEFT, HTTOPRIGHT, IsZoomed, NCCALCSIZE_PARAMS,
    SM_CXPADDEDBORDER, SM_CYSIZEFRAME, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SetWindowPos, WM_NCCALCSIZE, WM_NCHITTEST,
};

/// Where things are in the top bar, in the interface's own units.
#[derive(Clone, Copy, Default)]
struct Layout {
    /// The app is drawing the title bar (as opposed to Windows).
    enabled: bool,
    height: f32,
    /// Left of this are the back and forward buttons.
    nav_right: f32,
    search_left: f32,
    search_right: f32,
    /// Right of this are the settings and window buttons.
    controls_left: f32,
    /// Screen pixels per interface unit.
    scale: f32,
}

thread_local! {
    static LAYOUT: Cell<Layout> = const { Cell::new(Layout {
        enabled: false, height: 0.0, nav_right: 0.0, search_left: 0.0, search_right: 0.0, controls_left: 0.0, scale: 1.0,
    }) };
    static WINDOW: Cell<isize> = const { Cell::new(0) };
}

/// Thickness of the window's resize border, in screen pixels.
unsafe fn border(hwnd: HWND) -> i32 {
    unsafe {
        let dpi = GetDpiForWindow(hwnd);
        GetSystemMetricsForDpi(SM_CYSIZEFRAME, dpi) + GetSystemMetricsForDpi(SM_CXPADDEDBORDER, dpi)
    }
}

unsafe extern "system" fn on_message(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    let layout = LAYOUT.get();
    if !layout.enabled {
        return unsafe { DefSubclassProc(hwnd, message, wparam, lparam) };
    }
    unsafe {
        match message {
            // Windows asks how much of the window is frame. Let it take the sides and bottom
            // as usual, then give the top back to the app: that removes the title bar.
            WM_NCCALCSIZE if wparam.0 != 0 => {
                let params = lparam.0 as *mut NCCALCSIZE_PARAMS;
                let top = (*params).rgrc[0].top;
                let result = DefSubclassProc(hwnd, message, wparam, lparam);
                // A maximised window hangs over the screen's edge by the border's thickness.
                let inset = if IsZoomed(hwnd).as_bool() { border(hwnd) } else { 0 };
                (*params).rgrc[0].top = top + inset;
                result
            }
            // Windows asks what part of the window the pointer is over.
            WM_NCHITTEST => {
                let hit = DefSubclassProc(hwnd, message, wparam, lparam);
                if hit.0 != HTCLIENT as isize {
                    return hit;
                }
                let mut point = POINT {
                    x: (lparam.0 & 0xffff) as i16 as i32,
                    y: ((lparam.0 >> 16) & 0xffff) as i16 as i32,
                };
                let _ = ScreenToClient(hwnd, &mut point);
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                // The top edge still resizes the window, as the missing title bar's did.
                let edge = border(hwnd);
                if !IsZoomed(hwnd).as_bool() && point.y < edge {
                    return LRESULT(if point.x < edge * 2 {
                        HTTOPLEFT
                    } else if point.x >= client.right - edge * 2 {
                        HTTOPRIGHT
                    } else {
                        HTTOP
                    } as isize);
                }
                let (x, y) = (point.x as f32 / layout.scale, point.y as f32 / layout.scale);
                let on_control = x < layout.nav_right
                    || (x >= layout.search_left && x < layout.search_right)
                    || x >= layout.controls_left;
                LRESULT(if y < layout.height && !on_control { HTCAPTION } else { HTCLIENT } as isize)
            }
            _ => DefSubclassProc(hwnd, message, wparam, lparam),
        }
    }
}

/// Has Windows work out the window's frame again, after the title bar is taken out or put back.
fn reframe() {
    let hwnd = HWND(WINDOW.get() as *mut _);
    if !hwnd.is_invalid() {
        unsafe {
            let flags = SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE;
            let _ = SetWindowPos(hwnd, None, 0, 0, 0, 0, flags);
        }
    }
}

/// Starts watching the window's messages. Returns false if that wasn't possible, in which
/// case Windows' own title bar stays.
pub fn install(ui: &crate::ui::MainWindow, enabled: bool) -> bool {
    let window = ui.window().window_handle();
    let Ok(handle) = window.window_handle() else { return false };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else { return false };
    let hwnd = HWND(win32.hwnd.get() as *mut _);
    if !unsafe { SetWindowSubclass(hwnd, Some(on_message), 0x4e46, 0) }.as_bool() {
        return false;
    }
    WINDOW.set(hwnd.0 as isize);
    set_enabled(enabled);
    true
}

/// Switches between the app's title bar and Windows' own.
pub fn set_enabled(enabled: bool) {
    let mut layout = LAYOUT.get();
    if layout.enabled != enabled {
        layout.enabled = enabled;
        LAYOUT.set(layout);
        reframe();
    }
}

/// Notes where the top bar's controls are; everything else in the bar drags the window.
pub fn set_layout(height: f32, nav_right: f32, search_left: f32, search_right: f32, controls_left: f32, scale: f32) {
    let enabled = LAYOUT.get().enabled;
    LAYOUT.set(Layout { enabled, height, nav_right, search_left, search_right, controls_left, scale: scale.max(0.25) });
}

pub fn set_scale(scale: f32) {
    let mut layout = LAYOUT.get();
    layout.scale = scale.max(0.25);
    LAYOUT.set(layout);
}
