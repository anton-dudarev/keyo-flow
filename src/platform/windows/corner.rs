#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::player::{play_with_options, PlaybackOptions, PlaybackOutcome, RepeatMode};
    use crate::core::scenario::{Event, Scenario, TimedEvent};
    use std::sync::mpsc;

    #[test]
    fn only_physical_input_arms_the_corner_and_it_must_hold() {
        let start = Instant::now();
        let corner = (7, 7);
        let mut dwell = CornerDwell::default();
        dwell.moved(corner, false, start);
        assert!(!dwell.ready(start + HOLD_TIME));
        dwell.moved(corner, true, start);
        assert!(!dwell.ready(start + Duration::from_millis(249)));
        assert!(dwell.ready(start + HOLD_TIME));
    }

    #[test]
    fn leaving_the_corner_cancels_dwell_even_when_the_scenario_moves_the_mouse() {
        let start = Instant::now();
        let mut dwell = CornerDwell::default();
        dwell.moved((0, 0), true, start);
        dwell.moved((-1, 0), false, start + Duration::from_millis(50));
        dwell.moved((0, 0), false, start + Duration::from_millis(80));
        assert!(!dwell.ready(start + HOLD_TIME));
        let reentered = start + HOLD_TIME;
        dwell.moved((0, 0), true, reentered);
        assert!(!dwell.ready(reentered));
        assert!(dwell.ready(reentered + HOLD_TIME));
    }

    #[test]
    fn corner_signal_stops_an_unlimited_run_during_a_long_delay() {
        let key = String::from("ControlLeft");
        let (sender, events) = mpsc::channel();
        let scenario = Scenario {
            events: vec![
                TimedEvent {
                    delay_micros: 0,
                    event: Event::KeyDown { key: key.clone() },
                },
                TimedEvent {
                    delay_micros: 60_000_000,
                    event: Event::Scroll { x: 0, y: 1 },
                },
            ],
        };
        let playback = play_with_options(
            scenario,
            PlaybackOptions {
                speed: 1.0,
                repeats: RepeatMode::Infinite,
            },
            move |event| {
                sender.send(event.clone()).unwrap();
                Ok(())
            },
        )
        .unwrap();
        let timeout = Duration::from_secs(3);
        assert_eq!(
            events.recv_timeout(timeout).unwrap(),
            Event::KeyDown { key: key.clone() }
        );
        let start = Instant::now();
        let mut dwell = CornerDwell::default();
        dwell.moved((0, 0), true, start);
        if dwell.ready(start + HOLD_TIME) {
            playback.stop();
        }
        assert_eq!(
            playback.result.recv_timeout(timeout).unwrap().unwrap(),
            PlaybackOutcome::Stopped
        );
        assert_eq!(
            events.try_iter().collect::<Vec<_>>(),
            [Event::KeyUp { key }]
        );
    }
}
use std::cell::RefCell;
use std::io;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::WindowsAndMessaging::GetPhysicalCursorPos;

use super::capture::MouseHook;

const CORNER_SIZE: i32 = 8;
const HOLD_TIME: Duration = Duration::from_millis(250);

thread_local! {
    static DWELL: RefCell<Option<CornerDwell>> = const { RefCell::new(None) };
}

pub(super) struct CornerStop {
    _hook: MouseHook,
}

impl CornerStop {
    pub(super) fn install() -> io::Result<Self> {
        let hook = MouseHook::install()?;
        DWELL.with(|dwell| dwell.replace(Some(CornerDwell::default())));
        Ok(Self { _hook: hook })
    }

    pub(super) fn requested(&self) -> bool {
        DWELL.with(|dwell| {
            let mut dwell = dwell.borrow_mut();
            let Some(dwell) = dwell.as_mut() else {
                return false;
            };
            if !dwell.ready(Instant::now()) {
                return false;
            }
            let mut cursor = POINT::default();
            // SAFETY: передаём указатель на доступный для записи буфер POINT.
            if unsafe { GetPhysicalCursorPos(&mut cursor) } != 0 && in_corner((cursor.x, cursor.y))
            {
                return true;
            }
            dwell.entered = None;
            false
        })
    }
}

impl Drop for CornerStop {
    fn drop(&mut self) {
        DWELL.with(|dwell| dwell.replace(None));
    }
}

pub(super) fn mouse_moved(position: POINT, physical: bool) {
    DWELL.with(|dwell| {
        if let Some(dwell) = dwell.borrow_mut().as_mut() {
            dwell.moved((position.x, position.y), physical, Instant::now());
        }
    });
}

#[derive(Default)]
struct CornerDwell {
    entered: Option<Instant>,
}

impl CornerDwell {
    fn moved(&mut self, position: (i32, i32), physical: bool, now: Instant) {
        if !in_corner(position) {
            self.entered = None;
        } else if physical {
            self.entered.get_or_insert(now);
        }
    }

    fn ready(&self, now: Instant) -> bool {
        self.entered
            .is_some_and(|entered| now.duration_since(entered) >= HOLD_TIME)
    }
}

fn in_corner((x, y): (i32, i32)) -> bool {
    (0..CORNER_SIZE).contains(&x) && (0..CORNER_SIZE).contains(&y)
}
