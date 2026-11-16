use std::io;
use std::mem::size_of;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::TryRecvError;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Console::{SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, GetThreadDpiAwarenessContext, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::core::player::{self, PlaybackOutcome};
use crate::core::scenario::{Event, MouseButton, Scenario};

static STOPPED: AtomicBool = AtomicBool::new(false);
static ACTIVE: Mutex<()> = Mutex::new(());

pub fn play(scenario: Scenario, dry_run: bool) -> io::Result<()> {
    let _active = ACTIVE
        .try_lock()
        .map_err(|_| io::Error::other("Адаптер Windows уже занят или завершился с ошибкой"))?;
    let desktop = Desktop::current()?;
    validate(&scenario, desktop)?;
    if dry_run {
        println!("Сценарий проверен. Событий: {}.", scenario.events.len());
        return Ok(());
    }
    if scenario.events.is_empty() {
        println!("В сценарии нет событий.");
        return Ok(());
    }

    let _stop = StopHook::install()?;
    println!("Запуск через 3 секунды. Переключись в тестовое окно. Остановка: Esc.");
    let ready = Instant::now() + Duration::from_secs(3);
    loop {
        if stop_requested() {
            println!("Воспроизведение отменено до запуска.");
            return Ok(());
        }
        if Instant::now() >= ready {
            break;
        }
        wait_for_messages();
    }

    let playback = player::play(scenario, 1.0, 1, move |event| {
        desktop.convert(event)?.send()
    })?;
    loop {
        if stop_requested() {
            playback.stop();
        }
        match playback.result.try_recv() {
            Ok(result) => {
                match result? {
                    PlaybackOutcome::Completed => println!("Воспроизведение завершено."),
                    PlaybackOutcome::Stopped => println!("Воспроизведение остановлено."),
                }
                return Ok(());
            }
            Err(TryRecvError::Empty) => wait_for_messages(),
            Err(TryRecvError::Disconnected) => {
                return Err(io::Error::other(
                    "Поток воспроизведения завершился без результата",
                ));
            }
        }
    }
}

fn validate(scenario: &Scenario, desktop: Desktop) -> io::Result<()> {
    let mut duration = Duration::ZERO;
    let start = Instant::now();
    for (index, timed) in scenario.events.iter().enumerate() {
        desktop.convert(&timed.event).map_err(|error| {
            io::Error::new(error.kind(), format!("Событие {}: {error}", index + 1))
        })?;
        duration = duration
            .checked_add(Duration::from_micros(timed.delay_micros))
            .ok_or_else(|| invalid("Длительность сценария слишком велика"))?;
        if start.checked_add(duration).is_none() {
            return Err(invalid("Длительность сценария слишком велика"));
        }
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct Desktop {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Desktop {
    fn current() -> io::Result<Self> {
        // SAFETY: функции работают с контекстом текущего процесса или потока.
        unsafe {
            let context = DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2;
            if AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), context) == 0
                && SetProcessDpiAwarenessContext(context) == 0
            {
                return Err(os_error("Не удалось настроить DPI для физического ввода"));
            }
            Ok(Self {
                x: GetSystemMetrics(SM_XVIRTUALSCREEN),
                y: GetSystemMetrics(SM_YVIRTUALSCREEN),
                width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
                height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
            })
        }
    }

    fn convert(self, event: &Event) -> io::Result<Inputs> {
        let (first, second) = match event {
            Event::MouseMove { x, y } => (
                mouse(
                    normalize(*x, self.x, self.width)?,
                    normalize(*y, self.y, self.height)?,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                ),
                None,
            ),
            Event::MouseDown { button } | Event::MouseUp { button } => {
                let down = matches!(event, Event::MouseDown { .. });
                let (press, release, data) = match button {
                    MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, 0),
                    MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, 0),
                    MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, 0),
                    MouseButton::Other(button @ (1 | 2)) => {
                        (MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, u32::from(*button))
                    }
                    MouseButton::Other(_) => return Err(invalid("Неподдерживаемая кнопка мыши")),
                };
                (mouse(0, 0, data, if down { press } else { release }), None)
            }
            Event::Scroll { x, y } => {
                let horizontal = mouse(0, 0, wheel_data(*x)?, MOUSEEVENTF_HWHEEL);
                let vertical = mouse(0, 0, wheel_data(*y)?, MOUSEEVENTF_WHEEL);
                if *y == 0 {
                    return Ok(Inputs::new((*x != 0).then_some(horizontal), None));
                }
                (vertical, (*x != 0).then_some(horizontal))
            }
            Event::KeyDown { key } | Event::KeyUp { key } => {
                let (code, mut flags) = keyboard_code(key)?;
                if matches!(event, Event::KeyUp { .. }) {
                    flags |= KEYEVENTF_KEYUP;
                }
                (
                    INPUT {
                        r#type: INPUT_KEYBOARD,
                        Anonymous: INPUT_0 {
                            ki: KEYBDINPUT {
                                wVk: code,
                                dwFlags: flags,
                                ..Default::default()
                            },
                        },
                    },
                    None,
                )
            }
        };
        Ok(Inputs::new(Some(first), second))
    }
}

fn normalize(pixel: i32, origin: i32, size: i32) -> io::Result<i32> {
    let relative = i64::from(pixel) - i64::from(origin);
    if !(1..=65_536).contains(&size) || !(0..i64::from(size)).contains(&relative) {
        return Err(invalid(
            "Координата вне диапазона виртуального рабочего стола",
        ));
    }
    // Округление вверх удерживает значение в интервале нужного пикселя.
    Ok(((relative * 65_536 + i64::from(size) - 1) / i64::from(size)) as i32)
}

fn wheel_data(notches: i64) -> io::Result<u32> {
    notches
        .checked_mul(i64::from(WHEEL_DELTA))
        .and_then(|delta| i32::try_from(delta).ok())
        .map(|delta| delta as u32)
        .ok_or_else(|| invalid("Величина прокрутки выходит за диапазон Windows"))
}

fn keyboard_code(key: &str) -> io::Result<(u16, u32)> {
    let code = match key.as_bytes() {
        [b'K', b'e', b'y', letter @ b'A'..=b'Z'] => u16::from(*letter),
        [b'D', b'i', b'g', b'i', b't', digit @ b'0'..=b'9'] => u16::from(*digit),
        _ => match key {
            "Space" => VK_SPACE,
            "Enter" => VK_RETURN,
            "Tab" => VK_TAB,
            "Escape" => VK_ESCAPE,
            "Backspace" => VK_BACK,
            "ControlLeft" => VK_LCONTROL,
            "ControlRight" => VK_RCONTROL,
            "ShiftLeft" => VK_LSHIFT,
            "ShiftRight" => VK_RSHIFT,
            "AltLeft" => VK_LMENU,
            "AltRight" => VK_RMENU,
            "MetaLeft" => VK_LWIN,
            "MetaRight" => VK_RWIN,
            "ArrowLeft" => VK_LEFT,
            "ArrowRight" => VK_RIGHT,
            "ArrowUp" => VK_UP,
            "ArrowDown" => VK_DOWN,
            "Home" => VK_HOME,
            "End" => VK_END,
            "PageUp" => VK_PRIOR,
            "PageDown" => VK_NEXT,
            "Insert" => VK_INSERT,
            "Delete" => VK_DELETE,
            _ => return Err(invalid("Неподдерживаемый идентификатор клавиши")),
        },
    };
    let extended = matches!(
        code,
        VK_RCONTROL
            | VK_RMENU
            | VK_LWIN
            | VK_RWIN
            | VK_LEFT
            | VK_RIGHT
            | VK_UP
            | VK_DOWN
            | VK_HOME
            | VK_END
            | VK_PRIOR
            | VK_NEXT
            | VK_INSERT
            | VK_DELETE
    );
    Ok((code, if extended { KEYEVENTF_EXTENDEDKEY } else { 0 }))
}

fn mouse(dx: i32, dy: i32, data: u32, flags: u32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}

struct Inputs {
    values: [INPUT; 2],
    count: u32,
}

impl Inputs {
    fn new(first: Option<INPUT>, second: Option<INPUT>) -> Self {
        Self {
            values: [first.unwrap_or_default(), second.unwrap_or_default()],
            count: u32::from(first.is_some()) + u32::from(second.is_some()),
        }
    }

    fn send(&self) -> io::Result<()> {
        if self.count == 0 {
            return Ok(());
        }
        // SAFETY: массив содержит count инициализированных структур INPUT.
        let sent =
            unsafe { SendInput(self.count, self.values.as_ptr(), size_of::<INPUT>() as i32) };
        if sent != self.count {
            return Err(os_error("Windows не приняла все события ввода"));
        }
        Ok(())
    }
}

struct StopHook(HHOOK);

impl StopHook {
    fn install() -> io::Result<Self> {
        STOPPED.store(false, Ordering::Relaxed);
        // SAFETY: обе функции получают статические обработчики с ABI Windows.
        unsafe {
            let module = GetModuleHandleW(std::ptr::null());
            let handle = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), module, 0);
            if handle.is_null() {
                return Err(os_error("Не удалось подключить аварийную клавишу Esc"));
            }
            if SetConsoleCtrlHandler(Some(console_handler), 1) == 0 {
                let error = os_error("Не удалось подключить обработчик Ctrl+C");
                UnhookWindowsHookEx(handle);
                return Err(error);
            }
            Ok(Self(handle))
        }
    }
}

impl Drop for StopHook {
    fn drop(&mut self) {
        // SAFETY: снимаем именно те обработчики, которые установили.
        unsafe {
            UnhookWindowsHookEx(self.0);
            SetConsoleCtrlHandler(Some(console_handler), 0);
        }
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && matches!(message as u32, WM_KEYDOWN | WM_SYSKEYDOWN) {
        // SAFETY: для HC_ACTION Windows передаёт указатель на KBDLLHOOKSTRUCT.
        let event = unsafe { &*(data as *const KBDLLHOOKSTRUCT) };
        if event.vkCode == u32::from(VK_ESCAPE) && event.flags & LLKHF_INJECTED == 0 {
            STOPPED.store(true, Ordering::Relaxed);
        }
    }
    // SAFETY: исходные параметры передаются следующему хуку без изменений.
    unsafe { CallNextHookEx(null_mut(), code, message, data) }
}

unsafe extern "system" fn console_handler(event: u32) -> i32 {
    if matches!(event, CTRL_C_EVENT | CTRL_BREAK_EVENT) {
        STOPPED.store(true, Ordering::Relaxed);
        return 1;
    }
    0
}

fn stop_requested() -> bool {
    let mut message = MSG::default();
    // SAFETY: передаём валидный буфер MSG и читаем очередь текущего потока.
    while unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } != 0 {
        if message.message == WM_QUIT {
            STOPPED.store(true, Ordering::Relaxed);
        }
    }
    STOPPED.load(Ordering::Relaxed)
}

fn wait_for_messages() {
    // SAFETY: передаём пустой набор дескрипторов, ожидаем только сообщения или таймаут.
    unsafe {
        MsgWaitForMultipleObjectsEx(0, std::ptr::null(), 10, QS_ALLINPUT, MWMO_INPUTAVAILABLE)
    };
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn os_error(message: &str) -> io::Error {
    io::Error::other(format!("{message}: {}", io::Error::last_os_error()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::scenario::TimedEvent;

    const DESKTOP: Desktop = Desktop {
        x: -1920,
        y: -1080,
        width: 3840,
        height: 2160,
    };

    #[test]
    fn mouse_move_uses_physical_virtual_desktop_coordinates() {
        let inputs = DESKTOP
            .convert(&Event::MouseMove { x: -1920, y: -1 })
            .unwrap();
        assert_eq!(inputs.count, 1);
        // SAFETY: преобразование движения создаёт INPUT_MOUSE.
        let movement = unsafe { inputs.values[0].Anonymous.mi };
        assert_eq!(i64::from(movement.dx) * 3840 / 65_536, 0);
        assert_eq!(i64::from(movement.dy) * 2160 / 65_536, 1079);
        assert_eq!(
            movement.dwFlags,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK
        );
        assert!(DESKTOP
            .convert(&Event::MouseMove { x: 1920, y: 0 })
            .is_err());
    }

    #[test]
    fn scroll_converts_notches_to_signed_windows_deltas() {
        let inputs = DESKTOP.convert(&Event::Scroll { x: 2, y: -3 }).unwrap();
        assert_eq!(inputs.count, 2);
        // SAFETY: оба события прокрутки создают INPUT_MOUSE.
        let vertical = unsafe { inputs.values[0].Anonymous.mi };
        let horizontal = unsafe { inputs.values[1].Anonymous.mi };
        assert_eq!(
            (vertical.dwFlags, vertical.mouseData as i32),
            (MOUSEEVENTF_WHEEL, -360)
        );
        assert_eq!(
            (horizontal.dwFlags, horizontal.mouseData as i32),
            (MOUSEEVENTF_HWHEEL, 240)
        );
        assert!(DESKTOP
            .convert(&Event::Scroll { x: i64::MAX, y: 0 })
            .is_err());
    }

    #[test]
    fn key_release_preserves_extended_flag() {
        let inputs = DESKTOP
            .convert(&Event::KeyUp {
                key: "ControlRight".into(),
            })
            .unwrap();
        assert_eq!(inputs.count, 1);
        // SAFETY: преобразование отпускания клавиши создаёт INPUT_KEYBOARD.
        let key = unsafe { inputs.values[0].Anonymous.ki };
        assert_eq!(key.wVk, VK_RCONTROL);
        assert_eq!(key.dwFlags, KEYEVENTF_EXTENDEDKEY | KEYEVENTF_KEYUP);
    }

    #[test]
    fn validation_rejects_unsupported_late_event() {
        let scenario = Scenario {
            events: vec![
                TimedEvent {
                    delay_micros: 0,
                    event: Event::KeyDown { key: "KeyA".into() },
                },
                TimedEvent {
                    delay_micros: 1,
                    event: Event::KeyUp {
                        key: "Unknown".into(),
                    },
                },
            ],
        };
        assert!(validate(&scenario, DESKTOP)
            .unwrap_err()
            .to_string()
            .starts_with("Событие 2:"));
    }
}
