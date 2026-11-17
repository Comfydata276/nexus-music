//! Previous, play/pause and next buttons on the app's thumbnail in the Windows taskbar.

use std::cell::RefCell;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Shell::{
    DefSubclassProc, ITaskbarList3, SetWindowSubclass, THB_FLAGS, THB_ICON, THB_TOOLTIP, THBF_DISABLED,
    THBF_ENABLED, THBN_CLICKED, THUMBBUTTON, TaskbarList,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, HICON, ICONINFO, RegisterWindowMessageW, WM_COMMAND,
};
use windows::core::w;

use crate::controller::post;

const PREVIOUS: u32 = 0;
const TOGGLE: u32 = 1;
const NEXT: u32 = 2;

struct Bar {
    list: ITaskbarList3,
    hwnd: HWND,
    previous: HICON,
    play: HICON,
    pause: HICON,
    next: HICON,
    /// The buttons exist on the current taskbar button.
    added: bool,
    playing: bool,
    has_track: bool,
    /// Windows broadcasts this message when it (re)creates the app's taskbar button.
    button_created: u32,
}

thread_local! {
    static BAR: RefCell<Option<Bar>> = const { RefCell::new(None) };
}

/// The shapes, on a 32 by 32 grid.
#[derive(Clone, Copy)]
enum Glyph {
    Previous,
    Play,
    Pause,
    Next,
}

fn in_triangle(p: (f32, f32), a: (f32, f32), b: (f32, f32), c: (f32, f32)) -> bool {
    let side = |p: (f32, f32), q: (f32, f32), r: (f32, f32)| (p.0 - r.0) * (q.1 - r.1) - (q.0 - r.0) * (p.1 - r.1);
    let (d1, d2, d3) = (side(p, a, b), side(p, b, c), side(p, c, a));
    !((d1 < 0.0 || d2 < 0.0 || d3 < 0.0) && (d1 > 0.0 || d2 > 0.0 || d3 > 0.0))
}

fn covers(glyph: Glyph, x: f32, y: f32) -> bool {
    let bar = |left: f32, right: f32| x >= left && x < right && (7.0..25.0).contains(&y);
    match glyph {
        Glyph::Play => in_triangle((x, y), (10.0, 6.0), (10.0, 26.0), (26.0, 16.0)),
        Glyph::Pause => bar(8.5, 13.5) || bar(18.5, 23.5),
        Glyph::Next => in_triangle((x, y), (7.0, 7.0), (7.0, 25.0), (21.0, 16.0)) || bar(21.5, 25.5),
        Glyph::Previous => in_triangle((x, y), (25.0, 7.0), (25.0, 25.0), (11.0, 16.0)) || bar(6.5, 10.5),
    }
}

/// Draws a glyph into an icon, smoothing its edges by sampling each pixel sixteen times.
fn make_icon(glyph: Glyph, light_taskbar: bool) -> Option<HICON> {
    const SIZE: usize = 32;
    let shade: u8 = if light_taskbar { 0x20 } else { 0xff };
    let mut pixels = Vec::with_capacity(SIZE * SIZE * 4);
    for py in 0..SIZE {
        for px in 0..SIZE {
            let mut hits = 0u32;
            for sy in 0..4 {
                for sx in 0..4 {
                    let (x, y) = (px as f32 + (sx as f32 + 0.5) / 4.0, py as f32 + (sy as f32 + 0.5) / 4.0);
                    hits += covers(glyph, x, y) as u32;
                }
            }
            let alpha = (hits * 255 / 16) as u8;
            // Blue, green, red, alpha; colour scaled by alpha as Windows expects.
            let c = (shade as u32 * alpha as u32 / 255) as u8;
            pixels.extend_from_slice(&[c, c, c, alpha]);
        }
    }
    let mask = vec![0u8; SIZE * SIZE / 8];
    unsafe {
        let color = CreateBitmap(SIZE as i32, SIZE as i32, 1, 32, Some(pixels.as_ptr().cast()));
        let mono = CreateBitmap(SIZE as i32, SIZE as i32, 1, 1, Some(mask.as_ptr().cast()));
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mono, hbmColor: color };
        let icon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mono.into());
        icon
    }
}

/// Whether Windows itself (taskbar, Start) is using its light theme.
fn light_taskbar() -> bool {
    let mut value = 0u32;
    let mut size = 4u32;
    let read = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("SystemUsesLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut value).cast()),
            Some(&mut size),
        )
    };
    read.is_ok() && value == 1
}

fn tip(text: &str) -> [u16; 260] {
    let mut out = [0u16; 260];
    for (slot, unit) in out.iter_mut().zip(text.encode_utf16().take(259)) {
        *slot = unit;
    }
    out
}

impl Bar {
    fn buttons(&self) -> [THUMBBUTTON; 3] {
        let flags = if self.has_track { THBF_ENABLED } else { THBF_DISABLED };
        let button = |id: u32, icon: HICON, text: &str| THUMBBUTTON {
            dwMask: THB_ICON | THB_TOOLTIP | THB_FLAGS,
            iId: id,
            iBitmap: 0,
            hIcon: icon,
            szTip: tip(text),
            dwFlags: flags,
        };
        [
            button(PREVIOUS, self.previous, "Previous"),
            if self.playing { button(TOGGLE, self.pause, "Pause") } else { button(TOGGLE, self.play, "Play") },
            button(NEXT, self.next, "Next"),
        ]
    }

    fn show(&mut self) {
        let buttons = self.buttons();
        unsafe {
            if self.added {
                let _ = self.list.ThumbBarUpdateButtons(self.hwnd, &buttons);
            } else if self.list.ThumbBarAddButtons(self.hwnd, &buttons).is_ok() {
                self.added = true;
            }
        }
    }
}

/// Sees the window's messages before the toolkit does, to catch clicks on the buttons.
unsafe extern "system" fn on_message(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if message == WM_COMMAND && ((wparam.0 >> 16) & 0xffff) as u32 == THBN_CLICKED {
        match (wparam.0 & 0xffff) as u32 {
            PREVIOUS => post(|ctl| ctl.previous_track()),
            TOGGLE => post(|ctl| ctl.toggle_play()),
            NEXT => post(|ctl| ctl.next_track()),
            _ => {}
        }
        return LRESULT(0);
    }
    BAR.with(|bar| {
        // `try_borrow_mut`: adding the buttons can itself send the window a message.
        if let Ok(mut bar) = bar.try_borrow_mut() {
            if let Some(bar) = bar.as_mut().filter(|bar| message == bar.button_created) {
                bar.added = false;
                bar.show();
            }
        }
    });
    unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
}

/// Sets the buttons up for the app's window. Call once the window exists.
pub fn install(ui: &crate::ui::MainWindow) {
    let window = ui.window().window_handle();
    let Ok(handle) = window.window_handle() else { return };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else { return };
    let hwnd = HWND(win32.hwnd.get() as *mut _);
    unsafe {
        let Ok(list) = CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_INPROC_SERVER) else {
            return;
        };
        if list.HrInit().is_err() {
            return;
        }
        let light = light_taskbar();
        let (Some(previous), Some(play), Some(pause), Some(next)) = (
            make_icon(Glyph::Previous, light),
            make_icon(Glyph::Play, light),
            make_icon(Glyph::Pause, light),
            make_icon(Glyph::Next, light),
        ) else {
            return;
        };
        let button_created = RegisterWindowMessageW(w!("TaskbarButtonCreated"));
        let mut bar =
            Bar { list, hwnd, previous, play, pause, next, added: false, playing: false, has_track: false, button_created };
        bar.show();
        BAR.with(|slot| *slot.borrow_mut() = Some(bar));
        let _ = SetWindowSubclass(hwnd, Some(on_message), 0x4e4d, 0);
    }
}

/// Updates the play/pause button and whether the buttons can be pressed.
pub fn update(playing: bool, has_track: bool) {
    BAR.with(|bar| {
        if let Ok(mut bar) = bar.try_borrow_mut() {
            if let Some(bar) = bar.as_mut() {
                if bar.playing != playing || bar.has_track != has_track || !bar.added {
                    bar.playing = playing;
                    bar.has_track = has_track;
                    bar.show();
                }
            }
        }
    });
}
