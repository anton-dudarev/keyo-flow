#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
fn main() -> std::process::ExitCode {
    use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    let result = keyo_flow::platform::windows::enable_dpi_awareness()
        .map_err(|error| error.to_string())
        .and_then(|()| keyo_flow::ui::run().map_err(|error| error.to_string()));
    if let Err(error) = result {
        let text: Vec<u16> = format!("Не удалось запустить Keyo. {error}\0")
            .encode_utf16()
            .collect();
        let title: Vec<u16> = "Keyo\0".encode_utf16().collect();
        // SAFETY: обе строки завершаются нулём и доступны до закрытия диалога.
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONERROR,
            )
        };
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("Графический интерфейс Keyo доступен только в Windows.");
    std::process::ExitCode::FAILURE
}
