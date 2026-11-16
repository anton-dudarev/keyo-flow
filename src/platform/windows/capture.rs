use std::cell::RefCell;
use std::sync::mpsc::{self, SyncSender};
use std::thread;

use super::*;
use crate::core::recorder::Recorder;

thread_local! {
    static SINK: RefCell<Option<SyncSender<Packet>>> = const { RefCell::new(None) };
    static LOST_EVENTS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

enum Packet {
    Started(Instant),
    Keyboard(Instant, u32, KBDLLHOOKSTRUCT),
    Mouse(Instant, u32, MSLLHOOKSTRUCT),
}

pub fn record() -> io::Result<Option<Scenario>> {
    let scenario = record_with_updates(&AtomicBool::new(false), |update| match update {
        RecordingUpdate::Countdown(3) => {
            println!(
                "Запись начнётся через 3 секунды. Переключись в тестовое окно. Остановка: Esc."
            );
        }
        RecordingUpdate::Countdown(_) => {}
        RecordingUpdate::Recording => println!("Запись началась. Нажми Esc, чтобы завершить."),
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

pub fn record_with_updates(
    cancelled: &AtomicBool,
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
            .spawn_scoped(scope, move || capture(sender, cancelled, &mut report))?;
        let scenario = collect(receiver);
        worker
            .join()
            .map_err(|_| io::Error::other("Поток перехвата завершился с ошибкой"))??;
        Ok(scenario)
    })?;
    if let Some(scenario) = &scenario {
        validate(scenario, desktop)?;
    }
    Ok(scenario)
}

fn collect(receiver: mpsc::Receiver<Packet>) -> Option<Scenario> {
    let mut recorder = None;
    let mut wheel = Wheel::default();
    for packet in receiver {
        match packet {
            Packet::Started(at) => {
                recorder = Some(Recorder::new(at));
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
        }
    }
    recorder.map(Recorder::finish)
}

fn capture(
    sender: SyncSender<Packet>,
    cancelled: &AtomicBool,
    report: &mut impl FnMut(RecordingUpdate),
) -> io::Result<()> {
    let _stop = StopHook::install()?;
    let _mouse = MouseHook::install()?;
    let stop = || cancelled.load(Ordering::Relaxed) || stop_requested();
    if !countdown_with(stop, wait_for_messages, |seconds| {
        report(RecordingUpdate::Countdown(seconds));
    }) {
        return Ok(());
    }
    sender
        .send(Packet::Started(Instant::now()))
        .map_err(|_| io::Error::other("Получатель событий записи недоступен"))?;
    SINK.with(|sink| sink.replace(Some(sender)));
    report(RecordingUpdate::Recording);
    while !stop() {
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

fn push(make_packet: impl FnOnce(Instant) -> Packet) {
    if STOPPED.load(Ordering::Relaxed) {
        return;
    }
    SINK.with(|sink| {
        if let Some(sender) = sink.borrow().as_ref() {
            if sender.try_send(make_packet(Instant::now())).is_err() {
                LOST_EVENTS.with(|lost| lost.set(true));
                STOPPED.store(true, Ordering::Relaxed);
            }
        }
    });
}

pub(super) fn keyboard(message: u32, input: KBDLLHOOKSTRUCT) {
    push(|at| Packet::Keyboard(at, message, input));
}

pub(super) fn is_physical_keyboard(flags: u32) -> bool {
    flags & LLKHF_INJECTED == 0
}

fn is_physical_mouse(flags: u32) -> bool {
    flags & LLMHF_INJECTED == 0
}

struct MouseHook(HHOOK);

impl MouseHook {
    fn install() -> io::Result<Self> {
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
        if is_physical_mouse(input.flags) {
            push(|at| Packet::Mouse(at, message as u32, input));
        }
    }
    // SAFETY: передаём следующему обработчику параметры Windows без изменений.
    unsafe { CallNextHookEx(null_mut(), code, message, data) }
}

fn keyboard_event(message: u32, input: KBDLLHOOKSTRUCT) -> Option<Event> {
    let extended = input.flags & LLKHF_EXTENDED != 0;
    let code = match u16::try_from(input.vkCode).ok()? {
        VK_ESCAPE => return None,
        VK_SHIFT if input.scanCode == 0x36 => VK_RSHIFT,
        VK_SHIFT => VK_LSHIFT,
        VK_CONTROL if extended => VK_RCONTROL,
        VK_CONTROL => VK_LCONTROL,
        VK_MENU if extended => VK_RMENU,
        VK_MENU => VK_LMENU,
        code => code,
    };
    let key = match u8::try_from(code).ok()? {
        letter @ b'A'..=b'Z' => format!("Key{}", char::from(letter)),
        digit @ b'0'..=b'9' => format!("Digit{}", char::from(digit)),
        _ => NAMED_KEYS
            .iter()
            .find(|&&(_, value)| code == value)?
            .0
            .into(),
    };
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
            // JSON v1 хранит только целые шаги, остаток пригодится при следующем событии.
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
    fn collecting_closed_input_retains_recorded_events() {
        let (sender, receiver) = mpsc::channel();
        let start = Instant::now();
        sender.send(Packet::Started(start)).unwrap();
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
        let scenario = collect(receiver).unwrap();
        assert_eq!(scenario.events.len(), 1);
        assert_eq!(scenario.events[0].delay_micros, 20_000);
        assert_eq!(
            scenario.events[0].event,
            Event::KeyDown { key: "KeyA".into() }
        );
    }

    #[test]
    fn filters_injected_keyboard_and_mouse_input() {
        assert!(!is_physical_keyboard(LLKHF_INJECTED | LLKHF_EXTENDED));
        assert!(is_physical_keyboard(LLKHF_EXTENDED));
        assert!(!is_physical_mouse(LLMHF_INJECTED));
        assert!(is_physical_mouse(0));
    }

    #[test]
    fn keyboard_capture_uses_playback_identifiers_and_skips_escape() {
        let escape = KBDLLHOOKSTRUCT {
            vkCode: u32::from(VK_ESCAPE),
            ..Default::default()
        };
        assert_eq!(keyboard_event(WM_KEYDOWN, escape), None);
        let control = KBDLLHOOKSTRUCT {
            vkCode: u32::from(VK_CONTROL),
            flags: LLKHF_EXTENDED,
            ..Default::default()
        };
        let Some(Event::KeyUp { key }) = keyboard_event(WM_KEYUP, control) else {
            panic!("Ожидалось отпускание правого Ctrl");
        };
        assert_eq!(keyboard_code(&key).unwrap().0, VK_RCONTROL);
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
