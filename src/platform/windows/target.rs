use std::io;

use windows_sys::Win32::Foundation::{HWND, POINT, RECT};
use windows_sys::Win32::Graphics::Gdi::ClientToScreen;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetCursorPos, GetForegroundWindow, GetWindowThreadProcessId,
};

use crate::core::scenario::{CoordinateSpace, WINDOW_COORDINATE_MAX};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct ClientArea {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl ClientArea {
    pub(super) fn normalize(self, x: i32, y: i32) -> Option<(i32, i32)> {
        let normalize = |position, origin, size: i32| {
            let offset = i64::from(position) - i64::from(origin);
            if !(0..i64::from(size)).contains(&offset) {
                return None;
            }
            let span = i64::from((size - 1).max(1));
            Some(((offset * i64::from(WINDOW_COORDINATE_MAX) + span / 2) / span) as i32)
        };
        Some((
            normalize(x, self.x, self.width)?,
            normalize(y, self.y, self.height)?,
        ))
    }

    pub(super) fn resolve(self, x: i32, y: i32) -> io::Result<(i32, i32)> {
        CoordinateSpace::Window
            .validate_point(x, y)
            .map_err(super::invalid)?;
        let resolve = |fraction, origin, size: i32| {
            let span = i64::from(size - 1);
            let scale = i64::from(WINDOW_COORDINATE_MAX);
            let position = i64::from(origin) + (i64::from(fraction) * span + scale / 2) / scale;
            i32::try_from(position).map_err(|_| super::invalid("Координата окна вне диапазона"))
        };
        Ok((
            resolve(x, self.x, self.width)?,
            resolve(y, self.y, self.height)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_coordinates_follow_a_moved_and_resized_client_area() {
        let recorded = ClientArea {
            x: 200,
            y: 100,
            width: 101,
            height: 201,
        };
        let current = ClientArea {
            x: -500,
            y: 50,
            width: 201,
            height: 101,
        };
        assert_eq!(recorded.normalize(250, 200), Some((500_000, 500_000)));
        assert_eq!(current.resolve(500_000, 500_000).unwrap(), (-400, 100));
        assert_eq!(current.resolve(1_000_000, 1_000_000).unwrap(), (-300, 150));
        assert_eq!(recorded.normalize(301, 200), None);
        assert!(current.resolve(-1, 500_000).is_err());
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct PlaybackTarget {
    window: isize,
    process: u32,
    thread: u32,
}

impl PlaybackTarget {
    pub(super) fn is_foreground(self) -> bool {
        // SAFETY: получение активного окна не требует входных данных.
        unsafe { GetForegroundWindow() == self.window as HWND }
    }

    pub(super) fn check_pointer(self) -> io::Result<()> {
        let mut point = POINT::default();
        // SAFETY: Win32 записывает координаты в доступный локальный буфер.
        if unsafe { GetCursorPos(&mut point) } == 0 {
            return Err(super::os_error("Не удалось получить положение курсора"));
        }
        if self.client_area()?.normalize(point.x, point.y).is_none() {
            return Err(super::invalid(
                "Курсор находится за пределами тестового окна",
            ));
        }
        Ok(())
    }

    pub(super) fn client_area(self) -> io::Result<ClientArea> {
        let window = self.window as HWND;
        let mut rect = RECT::default();
        let mut origin = POINT::default();
        // SAFETY: Win32 получает дескриптор окна и два доступных для записи локальных буфера.
        if unsafe {
            GetClientRect(window, &mut rect) == 0 || ClientToScreen(window, &mut origin) == 0
        } {
            return Err(super::os_error(
                "Не удалось получить клиентскую область тестового окна",
            ));
        }
        if rect.right <= 0 || rect.bottom <= 0 {
            return Err(super::invalid("Клиентская область тестового окна пуста"));
        }
        Ok(ClientArea {
            x: origin.x,
            y: origin.y,
            width: rect.right,
            height: rect.bottom,
        })
    }

    pub(super) fn check_focus(self) -> io::Result<()> {
        let window = self.window as HWND;
        let mut process = 0;
        // SAFETY: Win32 проверяет дескриптор и записывает идентификатор в локальную переменную.
        let thread = unsafe { GetWindowThreadProcessId(window, &mut process) };
        if thread != self.thread || process != self.process {
            return Err(io::Error::other("Выбранное тестовое окно было закрыто"));
        }
        if !self.is_foreground() {
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
impl OpenWindow {
    fn from_handle(window: HWND) -> Option<Self> {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowTextW, IsWindowVisible};

        let mut process = 0;
        // SAFETY: Win32 проверяет дескриптор и записывает идентификатор в локальную переменную.
        let thread = unsafe { GetWindowThreadProcessId(window, &mut process) };
        // SAFETY: проверка видимости принимает HWND без разыменования указателя в Rust.
        if thread == 0 || process == std::process::id() || unsafe { IsWindowVisible(window) } == 0 {
            return None;
        }
        let mut title = [0; 512];
        // SAFETY: передаём доступный для записи буфер с его фактической длиной.
        let len = unsafe { GetWindowTextW(window, title.as_mut_ptr(), title.len() as i32) };
        Some(Self {
            target: PlaybackTarget {
                window: window as isize,
                process,
                thread,
            },
            title: String::from_utf16_lossy(&title[..len.max(0) as usize]),
        })
    }
}

#[cfg(feature = "gui")]
pub(crate) fn pick_window(
    cancelled: &std::sync::atomic::AtomicBool,
    report: impl FnMut(u8),
) -> io::Result<Option<OpenWindow>> {
    use std::sync::atomic::Ordering;

    let _active = super::ACTIVE
        .try_lock()
        .map_err(|_| io::Error::other("Адаптер Windows уже занят или завершился с ошибкой"))?;
    let _stop = super::StopHook::install(true)?;
    let stop = || cancelled.load(Ordering::Relaxed) || super::stop_requested();
    if !super::countdown_with(stop, super::wait_for_messages, report) || stop() {
        return Ok(None);
    }
    // SAFETY: получение активного окна не требует входных данных.
    let mut selected =
        OpenWindow::from_handle(unsafe { GetForegroundWindow() }).ok_or_else(|| {
            io::Error::other("Переключись на окно другого приложения до конца отсчёта")
        })?;
    if selected.title.trim().is_empty() {
        selected.title = "Окно без заголовка".into();
    }
    Ok(Some(selected))
}

#[cfg(feature = "gui")]
pub(crate) fn open_windows() -> Vec<OpenWindow> {
    use windows_sys::core::BOOL;
    use windows_sys::Win32::Foundation::LPARAM;
    use windows_sys::Win32::UI::WindowsAndMessaging::EnumWindows;

    unsafe extern "system" fn collect(window: HWND, data: LPARAM) -> BOOL {
        if let Some(window) =
            OpenWindow::from_handle(window).filter(|window| !window.title.trim().is_empty())
        {
            // SAFETY: EnumWindows синхронно передаёт указатель на локальный список из open_windows.
            let windows = unsafe { &mut *(data as *mut Vec<OpenWindow>) };
            windows.push(window);
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
