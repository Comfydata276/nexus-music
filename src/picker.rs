//! The Windows "open file" window, for choosing exported playlists.

use std::path::PathBuf;

use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
};
use windows::core::w;

/// Asks for one or more files. `None` if the window couldn't be shown at all; an empty
/// list if it was closed without choosing.
pub fn pick_exports(ui: &crate::ui::MainWindow) -> Option<Vec<PathBuf>> {
    let window = ui.window().window_handle();
    let owner = match window.window_handle().ok().map(|handle| handle.as_raw()) {
        Some(RawWindowHandle::Win32(win32)) => Some(HWND(win32.hwnd.get() as *mut _)),
        _ => None,
    };
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
