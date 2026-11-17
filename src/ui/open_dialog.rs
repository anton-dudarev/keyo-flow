use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

use windows::core::{w, HRESULT, PCWSTR};
use windows::Win32::Foundation::{ERROR_CANCELLED, HWND};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName, FOS_FILEMUSTEXIST,
    FOS_FORCEFILESYSTEM, FOS_NOCHANGEDIR, FOS_PATHMUSTEXIST, SIGDN_FILESYSPATH,
};

struct Apartment;

impl Drop for Apartment {
    fn drop(&mut self) {
        // SAFETY: уравновешиваем успешный CoInitializeEx в этом же потоке.
        unsafe { CoUninitialize() };
    }
}

pub(super) fn select(
    owner: isize,
    directory: Option<&Path>,
) -> windows::core::Result<Option<PathBuf>> {
    // SAFETY: COM и все объекты диалога работают в одном потоке. Строки живут до завершения вызовов.
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let _apartment = Apartment;
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
        dialog.SetTitle(w!("Открыть сценарий Keyo"))?;
        dialog.SetFileTypes(&[COMDLG_FILTERSPEC {
            pszName: w!("Сценарии JSON (*.json)"),
            pszSpec: w!("*.json"),
        }])?;
        dialog.SetDefaultExtension(w!("json"))?;
        dialog.SetOptions(
            dialog.GetOptions()?
                | FOS_FORCEFILESYSTEM
                | FOS_NOCHANGEDIR
                | FOS_PATHMUSTEXIST
                | FOS_FILEMUSTEXIST,
        )?;
        if let Some(directory) = directory {
            let wide: Vec<_> = directory.as_os_str().encode_wide().chain([0]).collect();
            let folder: IShellItem = SHCreateItemFromParsingName(PCWSTR(wide.as_ptr()), None)?;
            dialog.SetFolder(&folder)?;
        }
        match dialog.Show((owner != 0).then_some(HWND(owner as _))) {
            Err(error) if error.code() == HRESULT::from_win32(ERROR_CANCELLED.0) => {
                return Ok(None)
            }
            result => result?,
        }
        let path = dialog.GetResult()?.GetDisplayName(SIGDN_FILESYSPATH)?;
        let result = PathBuf::from(OsString::from_wide(path.as_wide()));
        CoTaskMemFree(Some(path.0.cast()));
        Ok(Some(result))
    }
}
