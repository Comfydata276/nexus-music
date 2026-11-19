//! The Windows "open file" and "save file" windows.

use std::path::PathBuf;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_OVERWRITEPROMPT, FOS_PICKFOLDERS, FileOpenDialog,
    FileSaveDialog,
    IFileOpenDialog, IFileSaveDialog, SIGDN_FILESYSPATH,
};
use windows::core::{HSTRING, w};

/// The app's window, for a system window to sit in front of.
fn owner(ui: &crate::ui::MainWindow) -> Option<HWND> {
    let window = ui.window().window_handle();
    match window.window_handle().ok().map(|handle| handle.as_raw()) {
        Some(RawWindowHandle::Win32(win32)) => Some(HWND(win32.hwnd.get() as *mut _)),
        _ => None,
    }
}

/// Asks for a folder. `None` if the window couldn't be shown at all; `Some(None)` if it was
/// closed without choosing.
pub fn pick_folder(ui: &crate::ui::MainWindow, title: &str) -> Option<Option<PathBuf>> {
    let owner = owner(ui);
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        let _ = dialog.SetTitle(&HSTRING::from(title));
        if dialog.Show(owner).is_err() {
            return Some(None);
        }
        let item = dialog.GetResult().ok()?;
        let shown = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = shown.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(shown.0 as *const _));
        Some(path)
    }
}

/// Asks where to save a spreadsheet file, offering `name`. `None` if the window couldn't be
/// shown at all; `Some(None)` if it was closed without choosing.
pub fn pick_save(ui: &crate::ui::MainWindow, name: &str) -> Option<Option<PathBuf>> {
    let owner = owner(ui);
    unsafe {
        let dialog: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_FORCEFILESYSTEM | FOS_OVERWRITEPROMPT).ok()?;
        let kinds = [COMDLG_FILTERSPEC { pszName: w!("Spreadsheet (*.csv)"), pszSpec: w!("*.csv") }];
        let _ = dialog.SetFileTypes(&kinds);
        let _ = dialog.SetDefaultExtension(w!("csv"));
        let _ = dialog.SetFileName(&HSTRING::from(name));
        let _ = dialog.SetTitle(w!("Export playlist"));
        // An error here is the window being closed without a choice.
        if dialog.Show(owner).is_err() {
            return Some(None);
        }
        let item = dialog.GetResult().ok()?;
        let shown = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = shown.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(shown.0 as *const _));
        Some(path)
    }
}

/// Asks for one or more files. `None` if the window couldn't be shown at all; an empty
/// list if it was closed without choosing.
pub fn pick_exports(ui: &crate::ui::MainWindow) -> Option<Vec<PathBuf>> {
    let owner = owner(ui);
    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let options = dialog.GetOptions().ok()?;
        dialog.SetOptions(options | FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM).ok()?;
        let kinds = [
            COMDLG_FILTERSPEC { pszName: w!("Exported playlists (*.csv, *.json)"), pszSpec: w!("*.csv;*.json") },
            COMDLG_FILTERSPEC { pszName: w!("All files"), pszSpec: w!("*.*") },
        ];
        let _ = dialog.SetFileTypes(&kinds);
        let _ = dialog.SetTitle(w!("Choose the playlists you exported from Spotify"));
        // An error here is the window being closed without a choice.
        if dialog.Show(owner).is_err() {
            return Some(Vec::new());
        }
        let chosen = dialog.GetResults().ok()?;
        let mut paths = Vec::new();
        for at in 0..chosen.GetCount().ok()? {
            let Ok(item) = chosen.GetItemAt(at) else { continue };
            let Ok(name) = item.GetDisplayName(SIGDN_FILESYSPATH) else { continue };
            if let Ok(text) = name.to_string() {
                paths.push(PathBuf::from(text));
            }
            CoTaskMemFree(Some(name.0 as *const _));
        }
        Some(paths)
    }
}
