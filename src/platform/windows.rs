use std::io;
use std::mem::size_of;
use std::ptr::null_mut;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::TryRecvError;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Console::{
    GetConsoleWindow, SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{
    AreDpiAwarenessContextsEqual, GetThreadDpiAwarenessContext, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::core::player::{self, PlaybackOptions, PlaybackOutcome, PlaybackPass};
use crate::core::scenario::{CoordinateSpace, Event, MouseButton, Scenario};

mod capture;
mod corner;
#[cfg(feature = "gui")]
pub(crate) mod hotkeys;
pub(crate) mod target;
#[cfg(feature = "gui")]
pub(crate) use capture::record_with_target;
pub use capture::{record, record_with_updates, RecordingUpdate};

static STOPPED: AtomicBool = AtomicBool::new(false);
static ACTIVE: Mutex<()> = Mutex::new(());

thread_local! {
    static ESCAPE_STOPS: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Chord {
    modifiers: u32,
    code: u32,
}

pub enum PlaybackUpdate {
    Countdown(u8),
    Playing(PlaybackPass),
}

pub fn play(scenario: Scenario, dry_run: bool) -> io::Result<()> {
    if dry_run || scenario.events.is_empty() {
        validate_playback(&scenario)?;
        if dry_run {
            println!("Сценарий проверен. Событий: {}.", scenario.events.len());
        } else {
            println!("В сценарии нет событий.");
        }
        return Ok(());
    }

    let mut playing = false;
    let outcome = play_with_updates(
        scenario,
        PlaybackOptions::default(),
        &AtomicBool::new(false),
        |update| match update {
            PlaybackUpdate::Countdown(3) => {
                println!("Запуск через 3 секунды. Остановка: Esc или верхний левый угол основного монитора.");
            }
            PlaybackUpdate::Countdown(_) => {}
            PlaybackUpdate::Playing(_) => playing = true,
        },
    )?;
    match outcome {
        PlaybackOutcome::Completed => println!("Воспроизведение завершено."),
        PlaybackOutcome::Stopped if !playing => println!("Воспроизведение отменено до запуска."),
        PlaybackOutcome::Stopped => println!("Воспроизведение остановлено."),
    }
    Ok(())
}

pub fn validate_playback(scenario: &Scenario) -> io::Result<()> {
    validate(scenario, Desktop::current()?)
}

pub fn play_with_updates(
    scenario: Scenario,
    options: PlaybackOptions,
    cancelled: &AtomicBool,
    report: impl FnMut(PlaybackUpdate),
) -> io::Result<PlaybackOutcome> {
    play_with_target(scenario, options, cancelled, None, report)
}

pub(crate) fn play_with_target(
    scenario: Scenario,
    options: PlaybackOptions,
    cancelled: &AtomicBool,
    target: Option<target::PlaybackTarget>,
    mut report: impl FnMut(PlaybackUpdate),
) -> io::Result<PlaybackOutcome> {
    options.validate(&scenario)?;
    let window_coordinates = match scenario.coordinates {
        CoordinateSpace::Desktop => None,
        CoordinateSpace::Window => Some(target.ok_or_else(|| invalid(
            "Сценарий использует координаты окна. Выбери тестовое окно в графическом интерфейсе Keyo",
        ))?),
    };
    let _active = ACTIVE
        .try_lock()
        .map_err(|_| io::Error::other("Адаптер Windows уже занят или завершился с ошибкой"))?;
    let desktop = Desktop::current()?;
    validate(&scenario, desktop)?;
    if cancelled.load(Ordering::Relaxed) {
        return Ok(PlaybackOutcome::Stopped);
    }
    if scenario.events.is_empty() {
        return Ok(PlaybackOutcome::Completed);
    }

    let _stop = StopHook::install(true)?;
    let corner = corner::CornerStop::install()?;
    let stop = || cancelled.load(Ordering::Relaxed) || stop_requested() || corner.requested();
    if !countdown_with(stop, wait_for_messages, |seconds| {
        report(PlaybackUpdate::Countdown(seconds));
    }) {
        return Ok(PlaybackOutcome::Stopped);
    }
    if stop() {
        return Ok(PlaybackOutcome::Stopped);
    }

    let check_target = move || target.map_or(Ok(()), target::PlaybackTarget::check_focus);
    check_target()?;
    let playback = player::play_with_guard(
        scenario,
        options,
        move |event| {
            check_target()?;
            if let Some(window) = window_coordinates {
                if matches!(
                    event,
                    Event::MouseDown { .. } | Event::MouseUp { .. } | Event::Scroll { .. }
                ) {
                    window.check_pointer()?;
                }
            }
            Ok(())
        },
        move |event| {
            if let (Some(window), Event::MouseMove { x, y }) = (window_coordinates, event) {
                let (x, y) = window.client_area()?.resolve(*x, *y)?;
                return desktop.convert(&Event::MouseMove { x, y })?.send();
            }
            desktop.convert(event)?.send()
        },
    )?;
    let mut last_reported = 0;
    let mut focus_error = None;
    loop {
        if stop() {
            playback.stop();
        }
        if let Some(pass) = playback.current_pass() {
            if pass.number != last_reported {
                last_reported = pass.number;
                report(PlaybackUpdate::Playing(pass));
            }
        }
        match playback.result.try_recv() {
            Ok(result) => return focus_error.map_or(result, Err),
            Err(TryRecvError::Empty) => {
                if focus_error.is_none() && !stop() {
                    if let Err(error) = check_target() {
                        focus_error = Some(error);
                        playback.stop();
                    }
                }
                wait_for_messages();
            }
            Err(TryRecvError::Disconnected) => {
                return Err(io::Error::other(
                    "Поток воспроизведения завершился без результата",
                ));
            }
        }
    }
}

fn countdown_with(
    mut cancelled: impl FnMut() -> bool,
    mut wait: impl FnMut(),
    mut report: impl FnMut(u8),
) -> bool {
    let ready = Instant::now() + Duration::from_secs(3);
    let mut previous = 0;
    while !cancelled() {
        let remaining = ready.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        let seconds = remaining.as_nanos().div_ceil(1_000_000_000) as u8;
        if seconds != previous {
            previous = seconds;
            report(seconds);
        }
        wait();
    }
    false
}

pub fn enable_dpi_awareness() -> io::Result<()> {
    // SAFETY: функции работают с контекстом текущего процесса или потока.
    unsafe {
        let context = DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2;
        if AreDpiAwarenessContextsEqual(GetThreadDpiAwarenessContext(), context) == 0
            && SetProcessDpiAwarenessContext(context) == 0
        {
            return Err(os_error("Не удалось настроить DPI для физического ввода"));
        }
    }
    Ok(())
}

fn validate(scenario: &Scenario, desktop: Desktop) -> io::Result<()> {
    let mut duration = Duration::ZERO;
    let start = Instant::now();
    for (index, timed) in scenario.events.iter().enumerate() {
        validate_in_space(desktop, scenario.coordinates, &timed.event).map_err(|error| {
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
        enable_dpi_awareness()?;
        // SAFETY: читаем параметры виртуального экрана текущего сеанса.
        unsafe {
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

const NAMED_KEYS: &[(&str, u16)] = &[
    ("Space", VK_SPACE),
    ("Enter", VK_RETURN),
    ("Tab", VK_TAB),
    ("Escape", VK_ESCAPE),
    ("Backspace", VK_BACK),
    ("ControlLeft", VK_LCONTROL),
    ("ControlRight", VK_RCONTROL),
    ("ShiftLeft", VK_LSHIFT),
    ("ShiftRight", VK_RSHIFT),
    ("AltLeft", VK_LMENU),
    ("AltRight", VK_RMENU),
    ("MetaLeft", VK_LWIN),
    ("MetaRight", VK_RWIN),
    ("ArrowLeft", VK_LEFT),
    ("ArrowRight", VK_RIGHT),
    ("ArrowUp", VK_UP),
    ("ArrowDown", VK_DOWN),
    ("Home", VK_HOME),
    ("End", VK_END),
    ("PageUp", VK_PRIOR),
    ("PageDown", VK_NEXT),
    ("Insert", VK_INSERT),
    ("Delete", VK_DELETE),
    ("F1", VK_F1),
    ("F2", VK_F2),
    ("F3", VK_F3),
    ("F4", VK_F4),
    ("F5", VK_F5),
    ("F6", VK_F6),
    ("F7", VK_F7),
    ("F8", VK_F8),
    ("F9", VK_F9),
    ("F10", VK_F10),
    ("F11", VK_F11),
    ("F12", VK_F12),
    ("F13", VK_F13),
    ("F14", VK_F14),
    ("F15", VK_F15),
    ("F16", VK_F16),
    ("F17", VK_F17),
    ("F18", VK_F18),
    ("F19", VK_F19),
    ("F20", VK_F20),
    ("F21", VK_F21),
    ("F22", VK_F22),
    ("F23", VK_F23),
    ("F24", VK_F24),
    ("Numpad0", VK_NUMPAD0),
    ("Numpad1", VK_NUMPAD1),
    ("Numpad2", VK_NUMPAD2),
    ("Numpad3", VK_NUMPAD3),
    ("Numpad4", VK_NUMPAD4),
    ("Numpad5", VK_NUMPAD5),
    ("Numpad6", VK_NUMPAD6),
    ("Numpad7", VK_NUMPAD7),
    ("Numpad8", VK_NUMPAD8),
    ("Numpad9", VK_NUMPAD9),
    ("NumpadAdd", VK_ADD),
    ("NumpadSubtract", VK_SUBTRACT),
    ("NumpadMultiply", VK_MULTIPLY),
    ("NumpadDivide", VK_DIVIDE),
    ("NumpadDecimal", VK_DECIMAL),
    ("NumpadSeparator", VK_SEPARATOR),
    ("NumpadEnter", VK_RETURN),
    ("CapsLock", VK_CAPITAL),
    ("NumLock", VK_NUMLOCK),
    ("ScrollLock", VK_SCROLL),
    ("PrintScreen", VK_SNAPSHOT),
    ("Pause", VK_PAUSE),
    ("ContextMenu", VK_APPS),
    ("Semicolon", VK_OEM_1),
    ("Equal", VK_OEM_PLUS),
    ("Comma", VK_OEM_COMMA),
    ("Minus", VK_OEM_MINUS),
    ("Period", VK_OEM_PERIOD),
    ("Slash", VK_OEM_2),
    ("Backquote", VK_OEM_3),
    ("BracketLeft", VK_OEM_4),
    ("Backslash", VK_OEM_5),
    ("BracketRight", VK_OEM_6),
    ("Quote", VK_OEM_7),
    ("IntlBackslash", VK_OEM_102),
    ("VolumeMute", VK_VOLUME_MUTE),
    ("VolumeDown", VK_VOLUME_DOWN),
    ("VolumeUp", VK_VOLUME_UP),
    ("MediaNextTrack", VK_MEDIA_NEXT_TRACK),
    ("MediaPreviousTrack", VK_MEDIA_PREV_TRACK),
    ("MediaStop", VK_MEDIA_STOP),
    ("MediaPlayPause", VK_MEDIA_PLAY_PAUSE),
];

#[cfg(feature = "gui")]
pub(crate) fn key_names() -> impl Iterator<Item = &'static str> {
    [
        "A", "B", "C", "D", "E", "F", "G", "H", "I", "J", "K", "L", "M", "N", "O", "P", "Q", "R",
        "S", "T", "U", "V", "W", "X", "Y", "Z", "0", "1", "2", "3", "4", "5", "6", "7", "8", "9",
    ]
    .into_iter()
    .chain(NAMED_KEYS.iter().map(|&(name, _)| name))
}

fn keyboard_code(key: &str) -> io::Result<(u16, u32)> {
    let code = match key.as_bytes() {
        [character @ (b'A'..=b'Z' | b'0'..=b'9')] => u16::from(*character),
        _ => NAMED_KEYS
            .iter()
            .find_map(|&(name, code)| (name == key).then_some(code))
            .ok_or_else(|| invalid("Неподдерживаемый идентификатор клавиши"))?,
    };
    let extended = key == "NumpadEnter"
        || matches!(
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
                | VK_APPS
                | VK_DIVIDE
                | VK_NUMLOCK
                | VK_SNAPSHOT
                | VK_VOLUME_MUTE
                | VK_VOLUME_DOWN
                | VK_VOLUME_UP
                | VK_MEDIA_NEXT_TRACK
                | VK_MEDIA_PREV_TRACK
                | VK_MEDIA_STOP
                | VK_MEDIA_PLAY_PAUSE
        );
    Ok((code, if extended { KEYEVENTF_EXTENDEDKEY } else { 0 }))
}

#[cfg(feature = "gui")]
pub(crate) fn validate_event(coordinates: CoordinateSpace, event: &Event) -> io::Result<()> {
    validate_in_space(Desktop::current()?, coordinates, event)
}

fn validate_in_space(
    desktop: Desktop,
    coordinates: CoordinateSpace,
    event: &Event,
) -> io::Result<()> {
    if let (CoordinateSpace::Window, Event::MouseMove { x, y }) = (coordinates, event) {
        return coordinates.validate_point(*x, *y).map_err(invalid);
    }
    desktop.convert(event).map(|_| ())
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
            return Err(os_error(
                "Windows не приняла все события ввода (проверьте, запущено ли тестируемое окно от имени администратора)",
            ));
        }
        Ok(())
    }
}

struct StopHook {
    handle: HHOOK,
    console: bool,
}

impl StopHook {
    fn install(escape_stops: bool) -> io::Result<Self> {
        STOPPED.store(false, Ordering::Relaxed);
        ESCAPE_STOPS.with(|mode| mode.set(escape_stops));
        // SAFETY: обе функции получают статические обработчики с ABI Windows.
        unsafe {
            let module = GetModuleHandleW(std::ptr::null());
            let handle = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), module, 0);
            if handle.is_null() {
                return Err(os_error("Не удалось подключить аварийную клавишу Esc"));
            }
            let console = !GetConsoleWindow().is_null();
            if console && SetConsoleCtrlHandler(Some(console_handler), 1) == 0 {
                let error = os_error("Не удалось подключить обработчик Ctrl+C");
                UnhookWindowsHookEx(handle);
                return Err(error);
            }
            Ok(Self { handle, console })
        }
    }
}

impl Drop for StopHook {
    fn drop(&mut self) {
        // SAFETY: снимаем именно те обработчики, которые установили.
        unsafe {
            UnhookWindowsHookEx(self.handle);
            if self.console {
                SetConsoleCtrlHandler(Some(console_handler), 0);
            }
        }
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: для HC_ACTION Windows передаёт указатель на KBDLLHOOKSTRUCT.
        let event = unsafe { &*(data as *const KBDLLHOOKSTRUCT) };
        if capture::is_physical_keyboard(event.flags) {
            if event.vkCode == u32::from(VK_ESCAPE) && ESCAPE_STOPS.with(std::cell::Cell::get) {
                if matches!(message as u32, WM_KEYDOWN | WM_SYSKEYDOWN) {
                    STOPPED.store(true, Ordering::Relaxed);
                }
            } else {
                capture::keyboard(message as u32, *event);
            }
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

    #[test]
    fn playback_maps_function_and_numeric_keypad_keys() {
        assert_eq!(keyboard_code("F1").unwrap(), (VK_F1, 0));
        assert_eq!(keyboard_code("F24").unwrap(), (VK_F24, 0));
        assert_eq!(keyboard_code("Numpad1").unwrap(), (VK_NUMPAD1, 0));
        assert_eq!(
            keyboard_code("NumpadEnter").unwrap(),
            (VK_RETURN, KEYEVENTF_EXTENDEDKEY)
        );
        assert_eq!(
            keyboard_code("NumpadDivide").unwrap(),
            (VK_DIVIDE, KEYEVENTF_EXTENDEDKEY)
        );
        assert_eq!(keyboard_code("Backspace").unwrap(), (VK_BACK, 0));
        assert_eq!(
            keyboard_code("ArrowUp").unwrap(),
            (VK_UP, KEYEVENTF_EXTENDEDKEY)
        );
        assert_eq!(keyboard_code("Space").unwrap(), (VK_SPACE, 0));
    }

    #[test]
    fn playback_uses_short_letter_and_digit_names() {
        assert_eq!(keyboard_code("A").unwrap(), (u16::from(b'A'), 0));
        assert_eq!(keyboard_code("1").unwrap(), (u16::from(b'1'), 0));
        assert_eq!(keyboard_code("Enter").unwrap(), (VK_RETURN, 0));
        assert!(keyboard_code("KeyA").is_err());
        assert!(keyboard_code("Digit1").is_err());
    }

    #[test]
    fn cancellation_during_countdown_prevents_playback() {
        let cancelled = std::cell::Cell::new(false);
        let mut reported = Vec::new();
        let go = countdown_with(
            || cancelled.get(),
            || cancelled.set(true),
            |seconds| reported.push(seconds),
        );
        assert!(!go);
        assert_eq!(reported, [3]);
    }

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
            coordinates: Default::default(),
            events: vec![
                TimedEvent {
                    delay_micros: 0,
                    event: Event::KeyDown { key: "A".into() },
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
