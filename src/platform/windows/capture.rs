use std::cell::RefCell;
use std::collections::BTreeSet;
use std::num::NonZeroU8;
use std::sync::mpsc::{self, SyncSender};
use std::sync::OnceLock;
use std::thread;

use windows_sys::Win32::Foundation::RECT;

use super::target::{ClientArea, PlaybackTarget};
use super::*;
use crate::core::recorder::Recorder;
use crate::core::scenario::{KeyboardKey, ScanCode};

thread_local! {
    static SINK: RefCell<Option<CaptureSink>> = const { RefCell::new(None) };
    static LOST_EVENTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

enum Packet {
    Started(Instant, CoordinateSpace),
    Keyboard(Instant, u32, KBDLLHOOKSTRUCT),
    Mouse(Instant, u32, MSLLHOOKSTRUCT),
    StopShortcut(u32),
}

struct CaptureSink {
    sender: SyncSender<Packet>,
    shortcut: Option<StopShortcut>,
    window: Option<(PlaybackTarget, ClientArea)>,
}

pub fn record() -> io::Result<Option<Scenario>> {
    let scenario = record_with_updates(&AtomicBool::new(false), |update| match update {
        RecordingUpdate::Countdown(3) => {
            println!(
                "Запись начнётся через 3 секунды. Для остановки вернись в консоль и нажми Ctrl+C."
            );
        }
        RecordingUpdate::Countdown(_) => {}
        RecordingUpdate::Recording => {
            println!("Запись началась. Чтобы завершить, нажми Ctrl+C в консоли.")
        }
    })?;
    if scenario.is_none() {
        println!("Запись отменена до запуска.");
    }
    Ok(scenario)
}

pub enum RecordingUpdate {
    Countdown(u8),
    Recording,
}

pub(crate) struct RecordingStopClick {
    pub(crate) area: RECT,
    pub(crate) at: Instant,
}

pub fn record_with_updates(
    cancelled: &AtomicBool,
    report: impl FnMut(RecordingUpdate) + Send,
) -> io::Result<Option<Scenario>> {
    record_with_target(cancelled, None, None, &OnceLock::new(), report)
}

pub(crate) fn record_with_target(
    cancelled: &AtomicBool,
    shortcut: Option<Chord>,
    target: Option<PlaybackTarget>,
    stop_click: &OnceLock<RecordingStopClick>,
    mut report: impl FnMut(RecordingUpdate) + Send,
) -> io::Result<Option<Scenario>> {
    if cancelled.load(Ordering::Relaxed) {
        return Ok(None);
    }
    let _active = ACTIVE
        .try_lock()
        .map_err(|_| io::Error::other("Адаптер Windows уже занят или завершился с ошибкой"))?;
    let desktop = Desktop::current()?;
    let (sender, receiver) = mpsc::sync_channel(4096);
    let scenario = thread::scope(|scope| -> io::Result<Option<Scenario>> {
        let worker = thread::Builder::new()
            .name("keyo-capture".into())
            .spawn_scoped(scope, move || {
                capture(sender, cancelled, shortcut, target, &mut report)
            })?;
        let scenario = collect(receiver, stop_click);
        worker
            .join()
            .map_err(|_| io::Error::other("Поток перехвата завершился с ошибкой"))??;
        Ok(scenario)
    })?;
    if let Some(scenario) = &scenario {
        validate(scenario, Some(desktop))?;
    }
    Ok(scenario)
}

fn collect(
    receiver: mpsc::Receiver<Packet>,
    stop_click: &OnceLock<RecordingStopClick>,
) -> Option<Scenario> {
    let mut recorder = None;
    let mut coordinates = CoordinateSpace::Desktop;
    let mut wheel = Wheel::default();
    let mut stop_modifiers = None;
    let mut started_at = None;
    for packet in receiver {
        match packet {
            Packet::Started(at, space) => {
                recorder = Some(Recorder::new(at));
                started_at = Some(at);
                coordinates = space;
            }
            Packet::Keyboard(at, message, input) => {
                if let (Some(recorder), Some(event)) =
                    (recorder.as_mut(), keyboard_event(message, input))
                {
                    recorder.push(at, event);
                }
            }
            Packet::Mouse(at, message, input) => {
                if let (Some(recorder), Some(event)) =
                    (recorder.as_mut(), mouse_event(message, input, &mut wheel))
                {
                    if matches!(event, Event::MouseMove { .. }) {
                        recorder.push(at, event);
                    } else {
                        recorder.push_mouse_action(at, (input.pt.x, input.pt.y), event);
                    }
                }
            }
            Packet::StopShortcut(modifiers) => stop_modifiers = Some(modifiers),
        }
    }
    recorder.map(|recorder| {
        let mut scenario = recorder.finish();
        scenario.coordinates = coordinates;
        if let Some(modifiers) = stop_modifiers {
            remove_stop_modifiers(&mut scenario, modifiers);
        } else if let (CoordinateSpace::Desktop, Some(start), Some(click)) =
            (coordinates, started_at, stop_click.get())
        {
            remove_stop_click(&mut scenario, start, click);
        }
        crate::core::timeline::compact(&mut scenario);
        scenario
    })
}

fn remove_stop_click(scenario: &mut Scenario, start: Instant, click: &RecordingStopClick) {
    let in_button = |(x, y)| {
        (click.area.left..click.area.right).contains(&x)
            && (click.area.top..click.area.bottom).contains(&y)
    };
    let stop_micros = click.at.saturating_duration_since(start).as_micros();
    let mut elapsed = 0;
    let mut position = None;
    let mut down = None;
    let mut stop = None;
    for (index, timed) in scenario.events.iter().enumerate() {
        elapsed += u128::from(timed.delay_micros);
        match timed.event {
            Event::MouseMove { x, y } => position = Some((x, y)),
            Event::MouseDown {
                button: MouseButton::Left,
            } => down = position.filter(|&point| in_button(point)).map(|_| index),
            Event::MouseUp {
                button: MouseButton::Left,
            } => {
                // shortcut: GUI должен обработать клик за секунду, иначе нужен идентификатор сообщения Win32.
                let recent = stop_micros
                    .checked_sub(elapsed)
                    .is_some_and(|age| age <= 1_000_000);
                if let Some(down) = down
                    .take()
                    .filter(|_| recent && position.is_some_and(in_button))
                {
                    stop = Some((down, index));
                }
            }
            _ => {}
        }
    }
    if let Some((down, up)) = stop {
        scenario.remove_events(&BTreeSet::from([down, up]));
    }
}

fn capture(
    sender: SyncSender<Packet>,
    cancelled: &AtomicBool,
    shortcut: Option<Chord>,
    target: Option<PlaybackTarget>,
    report: &mut impl FnMut(RecordingUpdate),
) -> io::Result<()> {
    let _stop = StopHook::install(false)?;
    let _mouse = MouseHook::install()?;
    let stop = || cancelled.load(Ordering::Relaxed) || stop_requested();
    if !countdown_with(stop, wait_for_messages, |seconds| {
        report(RecordingUpdate::Countdown(seconds));
    }) {
        return Ok(());
    }
    let window = if let Some(target) = target {
        target.check_focus()?;
        Some((target, target.client_area()?))
    } else {
        None
    };
    let coordinates = if window.is_some() {
        CoordinateSpace::Window
    } else {
        CoordinateSpace::Desktop
    };
    sender
        .send(Packet::Started(Instant::now(), coordinates))
        .map_err(|_| io::Error::other("Получатель событий записи недоступен"))?;
    SINK.with(|sink| {
        sink.replace(Some(CaptureSink {
            sender,
            shortcut: shortcut.map(StopShortcut::from_pressed_keys),
            window,
        }))
    });
    report(RecordingUpdate::Recording);
    while !stop() {
        if let Some((target, area)) = window {
            if target.check_focus().is_err() || target.client_area().ok() != Some(area) {
                break;
            }
        }
        wait_for_messages();
    }
    SINK.with(|sink| sink.replace(None));
    if LOST_EVENTS.with(std::cell::Cell::get) {
        return Err(io::Error::other(
            "Очередь перехвата переполнена. Запись не сохранена",
        ));
    }
    Ok(())
}

fn capture_mouse(message: u32, mut input: MSLLHOOKSTRUCT) {
    if STOPPED.load(Ordering::Relaxed) {
        return;
    }
    SINK.with(|sink| {
        if let Some(capture) = sink.borrow().as_ref() {
            if let Some((target, area)) = capture.window {
                if !target.is_foreground() {
                    STOPPED.store(true, Ordering::Relaxed);
                    return;
                }
                let Some((x, y)) = area.normalize(input.pt.x, input.pt.y) else {
                    if message != WM_MOUSEMOVE {
                        STOPPED.store(true, Ordering::Relaxed);
                    }
                    return;
                };
                input.pt.x = x;
                input.pt.y = y;
            }
            enqueue(
                &capture.sender,
                Packet::Mouse(Instant::now(), message, input),
            );
        }
    });
}

pub(super) fn keyboard(message: u32, input: KBDLLHOOKSTRUCT) {
    if STOPPED.load(Ordering::Relaxed) {
        return;
    }
    SINK.with(|sink| {
        let mut sink = sink.borrow_mut();
        let Some(capture) = sink.as_mut() else { return };
        if capture
            .window
            .is_some_and(|(target, _)| !target.is_foreground())
        {
            STOPPED.store(true, Ordering::Relaxed);
            return;
        }
        if let Some(shortcut) = &mut capture.shortcut {
            if shortcut.handle(message, input) {
                enqueue(
                    &capture.sender,
                    Packet::StopShortcut(shortcut.chord.modifiers),
                );
                STOPPED.store(true, Ordering::Relaxed);
                return;
            }
        }
        enqueue(
            &capture.sender,
            Packet::Keyboard(Instant::now(), message, input),
        );
    });
}

fn enqueue(sender: &SyncSender<Packet>, packet: Packet) {
    if sender.try_send(packet).is_err() {
        LOST_EVENTS.with(|lost| lost.set(true));
        STOPPED.store(true, Ordering::Relaxed);
    }
}

const MODIFIER_KEYS: [(u16, u32); 8] = [
    (VK_LCONTROL, MOD_CONTROL),
    (VK_RCONTROL, MOD_CONTROL),
    (VK_LMENU, MOD_ALT),
    (VK_RMENU, MOD_ALT),
    (VK_LSHIFT, MOD_SHIFT),
    (VK_RSHIFT, MOD_SHIFT),
    (VK_LWIN, MOD_WIN),
    (VK_RWIN, MOD_WIN),
];

struct StopShortcut {
    chord: Chord,
    pressed_modifiers: [bool; 8],
    primary_down: bool,
}

impl StopShortcut {
    fn from_pressed_keys(chord: Chord) -> Self {
        // SAFETY: читаем состояние клавиш вне обратного вызова перехвата.
        let is_down = |code| unsafe { GetAsyncKeyState(code) < 0 };
        Self {
            chord,
            pressed_modifiers: MODIFIER_KEYS.map(|(code, _)| is_down(i32::from(code))),
            primary_down: is_down(chord.code as i32),
        }
    }

    fn handle(&mut self, message: u32, input: KBDLLHOOKSTRUCT) -> bool {
        let Some(code) = capture_key_code(input) else {
            return false;
        };
        let down = match message {
            WM_KEYDOWN | WM_SYSKEYDOWN => true,
            WM_KEYUP | WM_SYSKEYUP => false,
            _ => return false,
        };
        if let Some(index) = MODIFIER_KEYS.iter().position(|&(key, _)| key == code) {
            self.pressed_modifiers[index] = down;
        }
        if u32::from(code) != self.chord.code {
            return false;
        }
        let fresh_press = down && !self.primary_down;
        self.primary_down = down;
        let modifiers = MODIFIER_KEYS.iter().zip(self.pressed_modifiers).fold(
            0,
            |mask, ((_, flag), pressed)| if pressed { mask | flag } else { mask },
        );
        fresh_press && modifiers == self.chord.modifiers
    }
}

fn remove_stop_modifiers(scenario: &mut Scenario, modifiers: u32) {
    let mut remove = BTreeSet::new();
    for (index, timed) in scenario.events.iter().enumerate().rev() {
        match &timed.event {
            Event::MouseMove { .. } => {}
            Event::KeyDown { key }
                if keyboard_code(&key.name).is_ok_and(|(code, _)| {
                    MODIFIER_KEYS
                        .iter()
                        .any(|&(modifier, flag)| modifier == code && modifiers & flag != 0)
                }) =>
            {
                remove.insert(index);
            }
            _ => break,
        }
    }
    scenario.remove_events(&remove);
}

pub(super) fn is_physical_keyboard(flags: u32) -> bool {
    flags & LLKHF_INJECTED == 0
}

fn is_physical_mouse(flags: u32) -> bool {
    flags & LLMHF_INJECTED == 0
}

pub(super) struct MouseHook(HHOOK);

impl MouseHook {
    pub(super) fn install() -> io::Result<Self> {
        // SAFETY: Windows получает статический обработчик с ABI Windows.
        let handle = unsafe {
            let module = GetModuleHandleW(std::ptr::null());
            SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), module, 0)
        };
        if handle.is_null() {
            return Err(os_error("Не удалось подключить перехват мыши"));
        }
        Ok(Self(handle))
    }
}

impl Drop for MouseHook {
    fn drop(&mut self) {
        // SAFETY: дескриптор принадлежит этому экземпляру.
        unsafe { UnhookWindowsHookEx(self.0) };
    }
}

unsafe extern "system" fn mouse_hook(code: i32, message: WPARAM, data: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: для HC_ACTION Windows передаёт указатель на MSLLHOOKSTRUCT.
        let input = unsafe { *(data as *const MSLLHOOKSTRUCT) };
        let physical = is_physical_mouse(input.flags);
        if message as u32 == WM_MOUSEMOVE {
            super::corner::mouse_moved(input.pt, physical);
        }
        if physical {
            capture_mouse(message as u32, input);
        }
    }
    // SAFETY: передаём следующему обработчику параметры Windows без изменений.
    unsafe { CallNextHookEx(null_mut(), code, message, data) }
}

fn capture_key_code(input: KBDLLHOOKSTRUCT) -> Option<u16> {
    let extended = input.flags & LLKHF_EXTENDED != 0;
    Some(match u16::try_from(input.vkCode).ok()? {
        VK_SHIFT if input.scanCode == 0x36 => VK_RSHIFT,
        VK_SHIFT => VK_LSHIFT,
        VK_CONTROL if extended => VK_RCONTROL,
        VK_CONTROL => VK_LCONTROL,
        VK_MENU if extended => VK_RMENU,
        VK_MENU => VK_LMENU,
        code => code,
    })
}

fn keyboard_event(message: u32, input: KBDLLHOOKSTRUCT) -> Option<Event> {
    let extended = input.flags & LLKHF_EXTENDED != 0;
    let code = capture_key_code(input)?;
    let name = match u8::try_from(code).ok()? {
        character @ (b'A'..=b'Z' | b'0'..=b'9') => char::from(character).to_string(),
        _ if code == VK_RETURN && extended => "NumpadEnter".into(),
        _ => NAMED_KEYS
            .iter()
            .find(|&&(_, value)| code == value)?
            .0
            .into(),
    };
    // Pause использует последовательность E1, которую SendInput не принимает как обычный скан-код.
    let scan_code = if code == VK_PAUSE {
        None
    } else {
        u8::try_from(input.scanCode)
            .ok()
            .and_then(NonZeroU8::new)
            .map(|code| ScanCode { code, extended })
    };
    let key = KeyboardKey { name, scan_code };
    match message {
        WM_KEYDOWN | WM_SYSKEYDOWN => Some(Event::KeyDown { key }),
        WM_KEYUP | WM_SYSKEYUP => Some(Event::KeyUp { key }),
        _ => None,
    }
}

#[derive(Default)]
struct Wheel {
    x: i32,
    y: i32,
}

fn mouse_event(message: u32, input: MSLLHOOKSTRUCT, wheel: &mut Wheel) -> Option<Event> {
    let high_word = (input.mouseData >> 16) as u16;
    let (button, pressed) = match message {
        WM_MOUSEMOVE => {
            return Some(Event::MouseMove {
                x: input.pt.x,
                y: input.pt.y,
            })
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let remainder = if message == WM_MOUSEHWHEEL {
                &mut wheel.x
            } else {
                &mut wheel.y
            };
            // Формат сценария хранит целые шаги, остаток пригодится при следующем событии.
            let delta = *remainder + i32::from(high_word as i16);
            *remainder = delta % WHEEL_DELTA as i32;
            let steps = i64::from(delta / WHEEL_DELTA as i32);
            return (steps != 0).then_some(if message == WM_MOUSEHWHEEL {
                Event::Scroll { x: steps, y: 0 }
            } else {
                Event::Scroll { x: 0, y: steps }
            });
        }
        WM_LBUTTONDOWN => (MouseButton::Left, true),
        WM_LBUTTONUP => (MouseButton::Left, false),
        WM_RBUTTONDOWN => (MouseButton::Right, true),
        WM_RBUTTONUP => (MouseButton::Right, false),
        WM_MBUTTONDOWN => (MouseButton::Middle, true),
        WM_MBUTTONUP => (MouseButton::Middle, false),
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            if !matches!(high_word, 1 | 2) {
                return None;
            }
            (
                MouseButton::Other(high_word as u8),
                message == WM_XBUTTONDOWN,
            )
        }
        _ => return None,
    };
    Some(if pressed {
        Event::MouseDown { button }
    } else {
        Event::MouseUp { button }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recording_combines_adjacent_keys_and_preserves_overlapping_actions() {
        let (sender, receiver) = mpsc::channel();
        let start = Instant::now();
        sender
            .send(Packet::Started(start, CoordinateSpace::Desktop))
            .unwrap();
        for (micros, message, code, scan_code) in [
            (700, WM_KEYDOWN, u32::from(b'A'), 0x1e),
            (50_825, WM_KEYUP, u32::from(b'A'), 0x1e),
            (51_000, WM_KEYDOWN, u32::from(VK_LCONTROL), 0x1d),
            (52_000, WM_KEYDOWN, u32::from(b'C'), 0x2e),
            (53_000, WM_KEYUP, u32::from(b'C'), 0x2e),
            (54_000, WM_KEYUP, u32::from(VK_LCONTROL), 0x1d),
        ] {
            sender
                .send(Packet::Keyboard(
                    start + Duration::from_micros(micros),
                    message,
                    KBDLLHOOKSTRUCT {
                        vkCode: code,
                        scanCode: scan_code,
                        ..Default::default()
                    },
                ))
                .unwrap();
        }
        drop(sender);
        let scenario = collect(receiver, &OnceLock::new()).unwrap();
        let physical = |name: &str, code| KeyboardKey {
            name: name.into(),
            scan_code: Some(ScanCode {
                code: NonZeroU8::new(code).unwrap(),
                extended: false,
            }),
        };
        let events = &scenario.events;
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].delay_micros, 700);
        assert_eq!(
            events[0].event,
            Event::KeySpan {
                key: physical("A", 0x1e),
                duration_micros: 50_125,
                release_order: 0,
            }
        );
        assert_eq!(events[1].delay_micros, 50_300);
        assert_eq!(
            events[1].event,
            Event::KeySpan {
                key: physical("ControlLeft", 0x1d),
                duration_micros: 3000,
                release_order: 0,
            }
        );
        assert_eq!(
            events[2].event,
            Event::KeySpan {
                key: physical("C", 0x2e),
                duration_micros: 1000,
                release_order: 0,
            }
        );
        assert_eq!(scenario.duration_micros(), 54_000);
    }

    #[test]
    fn configured_win_shortcut_stops_only_on_a_fresh_press_with_matching_modifiers() {
        let mut shortcut = StopShortcut {
            chord: Chord {
                modifiers: MOD_WIN | MOD_SHIFT,
                code: u32::from(b'K'),
            },
            pressed_modifiers: [false; 8],
            primary_down: true,
        };
        let key = |vk: u32| KBDLLHOOKSTRUCT {
            vkCode: vk,
            ..Default::default()
        };
        assert!(!shortcut.handle(WM_KEYDOWN, key(u32::from(VK_ESCAPE))));
        assert!(!shortcut.handle(WM_KEYDOWN, key(u32::from(VK_RWIN))));
        assert!(!shortcut.handle(WM_KEYDOWN, key(u32::from(VK_SHIFT))));
        assert!(!shortcut.handle(WM_KEYDOWN, key(u32::from(b'K'))));
        assert!(!shortcut.handle(WM_KEYUP, key(u32::from(b'K'))));
        assert!(!shortcut.handle(WM_KEYDOWN, key(u32::from(VK_RCONTROL))));
        assert!(!shortcut.handle(WM_KEYDOWN, key(u32::from(b'K'))));
        assert!(!shortcut.handle(WM_KEYUP, key(u32::from(b'K'))));
        assert!(!shortcut.handle(WM_KEYUP, key(u32::from(VK_RCONTROL))));
        assert!(shortcut.handle(WM_KEYDOWN, key(u32::from(b'K'))));
    }

    #[test]
    fn excludes_terminal_shortcut_modifiers_but_keeps_previous_recorded_input() {
        let (sender, receiver) = mpsc::channel();
        let start = Instant::now();
        sender
            .send(Packet::Started(start, CoordinateSpace::Desktop))
            .unwrap();
        for (millis, message, key) in [
            (1, WM_KEYDOWN, VK_LCONTROL),
            (2, WM_KEYDOWN, VK_ESCAPE),
            (3, WM_KEYUP, VK_ESCAPE),
            (4, WM_KEYUP, VK_LCONTROL),
            (5, WM_KEYDOWN, VK_RWIN),
            (6, WM_KEYDOWN, VK_LSHIFT),
        ] {
            sender
                .send(Packet::Keyboard(
                    start + Duration::from_millis(millis),
                    message,
                    KBDLLHOOKSTRUCT {
                        vkCode: u32::from(key),
                        ..Default::default()
                    },
                ))
                .unwrap();
        }
        sender
            .send(Packet::StopShortcut(MOD_WIN | MOD_SHIFT))
            .unwrap();
        drop(sender);
        let keys: Vec<_> = collect(receiver, &OnceLock::new())
            .unwrap()
            .events
            .into_iter()
            .map(|timed| (timed.delay_micros, timed.event))
            .collect();
        assert_eq!(
            keys,
            [
                (
                    1000,
                    Event::KeySpan {
                        key: "ControlLeft".into(),
                        duration_micros: 3000,
                        release_order: 0,
                    }
                ),
                (
                    1000,
                    Event::KeySpan {
                        key: "Escape".into(),
                        duration_micros: 1000,
                        release_order: 0,
                    }
                ),
            ]
        );
    }
    #[test]
    fn capture_distinguishes_numeric_keypad_enter_and_supports_function_keys() {
        for (code, flags, name) in [
            (VK_RETURN, 0, "Enter"),
            (VK_RETURN, LLKHF_EXTENDED, "NumpadEnter"),
            (VK_F12, 0, "F12"),
            (VK_OEM_COMMA, 0, "Comma"),
        ] {
            let input = KBDLLHOOKSTRUCT {
                vkCode: u32::from(code),
                flags,
                ..Default::default()
            };
            assert_eq!(
                keyboard_event(WM_KEYUP, input),
                Some(Event::KeyUp { key: name.into() })
            );
        }
    }
    #[test]
    fn recording_uses_short_digit_names() {
        let input = KBDLLHOOKSTRUCT {
            vkCode: u32::from(b'1'),
            ..Default::default()
        };
        assert_eq!(
            keyboard_event(WM_KEYDOWN, input),
            Some(Event::KeyDown { key: "1".into() })
        );
    }

    #[test]
    fn desktop_recording_removes_only_the_click_that_stopped_it() {
        let (sender, receiver) = mpsc::channel();
        let start = Instant::now();
        sender
            .send(Packet::Started(start, CoordinateSpace::Desktop))
            .unwrap();
        for (index, message) in [WM_LBUTTONDOWN, WM_LBUTTONUP, WM_LBUTTONDOWN, WM_LBUTTONUP]
            .into_iter()
            .enumerate()
        {
            sender
                .send(Packet::Mouse(
                    start + Duration::from_millis((index as u64 + 1) * 10),
                    message,
                    MSLLHOOKSTRUCT {
                        pt: windows_sys::Win32::Foundation::POINT {
                            x: 50 + index as i32,
                            y: 80,
                        },
                        ..Default::default()
                    },
                ))
                .unwrap();
        }
        drop(sender);
        let stop_click = OnceLock::from(RecordingStopClick {
            area: RECT {
                left: 45,
                top: 75,
                right: 55,
                bottom: 85,
            },
            at: start + Duration::from_millis(45),
        });
        let scenario = collect(receiver, &stop_click).unwrap();
        let actual = scenario.expand_recorded().unwrap();
        let mut elapsed = 0;
        let buttons: Vec<_> = actual
            .events
            .into_iter()
            .filter_map(|timed| {
                elapsed += timed.delay_micros;
                matches!(timed.event, Event::MouseDown { .. } | Event::MouseUp { .. })
                    .then_some(elapsed)
            })
            .collect();
        assert_eq!(buttons, [10_000, 20_000]);
    }

    #[test]
    fn stop_click_filter_keeps_an_old_click_in_the_same_area() {
        let start = Instant::now();
        let mut recorder = Recorder::new(start);
        for (delay, event) in [
            (
                10,
                Event::MouseDown {
                    button: MouseButton::Left,
                },
            ),
            (
                20,
                Event::MouseUp {
                    button: MouseButton::Left,
                },
            ),
        ] {
            recorder.push_mouse_action(start + Duration::from_millis(delay), (50, 80), event);
        }
        let mut scenario = recorder.finish();
        let original = scenario.clone();
        remove_stop_click(
            &mut scenario,
            start,
            &RecordingStopClick {
                area: RECT {
                    left: 45,
                    top: 75,
                    right: 55,
                    bottom: 85,
                },
                at: start + Duration::from_secs(2),
            },
        );
        assert_eq!(scenario, original);
    }

    #[test]
    fn collecting_closed_input_retains_recorded_events() {
        let (sender, receiver) = mpsc::channel();
        let start = Instant::now();
        sender
            .send(Packet::Started(start, CoordinateSpace::Desktop))
            .unwrap();
        sender
            .send(Packet::Keyboard(
                start + Duration::from_millis(20),
                WM_KEYDOWN,
                KBDLLHOOKSTRUCT {
                    vkCode: u32::from(b'A'),
                    ..Default::default()
                },
            ))
            .unwrap();
        drop(sender);
        let scenario = collect(receiver, &OnceLock::new()).unwrap();
        assert_eq!(scenario.events.len(), 1);
        assert_eq!(scenario.events[0].delay_micros, 20_000);
        assert_eq!(scenario.events[0].event, Event::KeyDown { key: "A".into() });
    }

    #[test]
    fn filters_injected_keyboard_and_mouse_input() {
        assert!(!is_physical_keyboard(LLKHF_INJECTED | LLKHF_EXTENDED));
        assert!(is_physical_keyboard(LLKHF_EXTENDED));
        assert!(!is_physical_mouse(LLMHF_INJECTED));
        assert!(is_physical_mouse(0));
    }

    #[test]
    fn keyboard_capture_records_escape_and_uses_playback_identifiers() {
        let escape = KBDLLHOOKSTRUCT {
            vkCode: u32::from(VK_ESCAPE),
            ..Default::default()
        };
        assert_eq!(
            keyboard_event(WM_KEYDOWN, escape),
            Some(Event::KeyDown {
                key: "Escape".into()
            })
        );
        assert_eq!(
            keyboard_event(WM_KEYUP, escape),
            Some(Event::KeyUp {
                key: "Escape".into()
            })
        );
        let control = KBDLLHOOKSTRUCT {
            vkCode: u32::from(VK_CONTROL),
            scanCode: 0x1d,
            flags: LLKHF_EXTENDED,
            ..Default::default()
        };
        let Some(Event::KeyUp { key }) = keyboard_event(WM_KEYUP, control) else {
            panic!("Ожидалось отпускание правого Ctrl");
        };
        assert_eq!(keyboard_code(&key.name).unwrap().0, VK_RCONTROL);
        assert_eq!(
            key.scan_code,
            Some(ScanCode {
                code: NonZeroU8::new(0x1d).unwrap(),
                extended: true,
            })
        );
    }

    #[test]
    fn high_resolution_scroll_accumulates_whole_notches_per_axis() {
        let mut wheel = Wheel::default();
        let input = MSLLHOOKSTRUCT {
            mouseData: u32::from(-60_i16 as u16) << 16,
            ..Default::default()
        };
        assert_eq!(mouse_event(WM_MOUSEWHEEL, input, &mut wheel), None);
        assert_eq!(mouse_event(WM_MOUSEHWHEEL, input, &mut wheel), None);
        assert_eq!(
            mouse_event(WM_MOUSEWHEEL, input, &mut wheel),
            Some(Event::Scroll { x: 0, y: -1 })
        );
        assert_eq!(
            mouse_event(WM_MOUSEHWHEEL, input, &mut wheel),
            Some(Event::Scroll { x: -1, y: 0 })
        );
    }
}
