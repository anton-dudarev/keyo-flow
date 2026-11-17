use std::io;

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlaybackTarget {
    window: isize,
    process: u32,
    thread: u32,
}

impl PlaybackTarget {
    pub(super) fn check_focus(self) -> io::Result<()> {
        let window = self.window as HWND;
        let mut process = 0;
        // SAFETY: Win32 проверяет дескриптор и записывает идентификатор в локальную переменную.
        let thread = unsafe { GetWindowThreadProcessId(window, &mut process) };
        if thread != self.thread || process != self.process {
            return Err(io::Error::other("Выбранное тестовое окно было закрыто"));
        }
        // SAFETY: получение активного окна не требует входных данных.
        if unsafe { GetForegroundWindow() } != window {
            return Err(io::Error::other("Выбранное тестовое окно не активно"));
        }
        Ok(())
    }
}

#[cfg(feature = "gui")]
pub(crate) struct OpenWindow {
    pub(crate) target: PlaybackTarget,
    pub(crate) title: String,
}

#[cfg(feature = "gui")]
pub(crate) fn open_windows() -> Vec<OpenWindow> {
    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::LPARAM;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowTextW, IsWindowVisible,
    };

    unsafe extern "system" fn collect(window: HWND, data: LPARAM) -> BOOL {
        let mut process = 0;
        // SAFETY: EnumWindows передаёт HWND, а буфер идентификатора жив до конца вызова.
        let thread = unsafe { GetWindowThreadProcessId(window, &mut process) };
        // SAFETY: IsWindowVisible принимает HWND без разыменования указателя в Rust.
        if thread == 0 || process == std::process::id() || unsafe { IsWindowVisible(window) } == 0 {
            return 1;
        }
        let mut title = [0; 512];
        // SAFETY: передаём доступный для записи буфер с его фактической длиной.
        let len = unsafe { GetWindowTextW(window, title.as_mut_ptr(), title.len() as i32) };
        let title = String::from_utf16_lossy(&title[..len.max(0) as usize]);
        if !title.trim().is_empty() {
            // SAFETY: EnumWindows синхронно передаёт указатель на локальный список из open_windows.
            let windows = unsafe { &mut *(data as *mut Vec<OpenWindow>) };
            windows.push(OpenWindow {
                target: PlaybackTarget {
                    window: window as isize,
                    process,
                    thread,
                },
                title,
            });
        }
        1
    }

    let mut windows = Vec::new();
    // SAFETY: список жив на протяжении синхронного перечисления окон.
    unsafe {
        EnumWindows(
            Some(collect),
            &mut windows as *mut Vec<OpenWindow> as LPARAM,
        )
    };
    windows
}
