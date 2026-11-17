use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use super::safety::PressedInputGuard;
use super::scenario::{Event, Scenario};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackOutcome {
    Completed,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatMode {
    Count(u32),
    Infinite,
}

#[derive(Debug, Clone, Copy)]
pub struct PlaybackOptions {
    pub speed: f64,
    pub repeats: RepeatMode,
}

impl Default for PlaybackOptions {
    fn default() -> Self {
        Self {
            speed: 1.0,
            repeats: RepeatMode::Count(1),
        }
    }
}

impl PlaybackOptions {
    pub(crate) fn validate(self, scenario: &Scenario) -> io::Result<()> {
        if !self.speed.is_finite() || self.speed <= 0.0 || self.repeats == RepeatMode::Count(0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Требуются положительная конечная скорость и хотя бы один запуск",
            ));
        }
        if self.repeats == RepeatMode::Infinite && scenario.events.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Пустой сценарий нельзя повторять бесконечно",
            ));
        }
        let total = scenario
            .events
            .iter()
            .map(|event| u128::from(event.delay_micros))
            .sum();
        target_time(Instant::now(), total, self.speed)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PlaybackPass {
    pub number: u64,
    pub started_at: Instant,
}

pub struct Playback {
    /// Итог можно опрашивать через `try_recv` из потока интерфейса.
    pub result: mpsc::Receiver<io::Result<PlaybackOutcome>>,
    stopped: Arc<AtomicBool>,
    worker: Thread,
    pass: Arc<Mutex<Option<PlaybackPass>>>,
}

impl Playback {
    pub fn current_pass(&self) -> Option<PlaybackPass> {
        *self.pass.lock().unwrap()
    }

    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
        self.worker.unpark();
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Запускает отдельный поток. Адаптер должен быстро возвращать управление,
/// так как остановка прерывает ожидание, а текущий вызов адаптера завершается сам.
///
/// Идентификаторы модификаторов: `ControlLeft`, `ControlRight`, `ShiftLeft`,
/// `ShiftRight`, `AltLeft`, `AltRight`, `MetaLeft`, `MetaRight`.
pub fn play(
    scenario: Scenario,
    speed: f64,
    repeats: u32,
    send: impl FnMut(&Event) -> io::Result<()> + Send + 'static,
) -> io::Result<Playback> {
    play_with_options(
        scenario,
        PlaybackOptions {
            speed,
            repeats: RepeatMode::Count(repeats),
        },
        send,
    )
}

pub fn play_with_options(
    scenario: Scenario,
    options: PlaybackOptions,
    send: impl FnMut(&Event) -> io::Result<()> + Send + 'static,
) -> io::Result<Playback> {
    play_with_guard(scenario, options, |_| Ok(()), send)
}

pub(crate) fn play_with_guard(
    scenario: Scenario,
    options: PlaybackOptions,
    mut before_event: impl FnMut(&Event) -> io::Result<()> + Send + 'static,
    mut send: impl FnMut(&Event) -> io::Result<()> + Send + 'static,
) -> io::Result<Playback> {
    options.validate(&scenario)?;

    let (sender, result) = mpsc::channel();
    let stopped = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stopped);
    let pass = Arc::new(Mutex::new(None));
    let worker_pass = Arc::clone(&pass);
    let worker = thread::Builder::new()
        .name("keyo-playback".into())
        .spawn(move || {
            let outcome = run(
                &scenario,
                options,
                &worker_stop,
                &worker_pass,
                &mut before_event,
                &mut send,
            );
            let _ = sender.send(outcome);
        })?;
    Ok(Playback {
        result,
        stopped,
        worker: worker.thread().clone(),
        pass,
    })
}

fn run(
    scenario: &Scenario,
    options: PlaybackOptions,
    stopped: &AtomicBool,
    pass: &Mutex<Option<PlaybackPass>>,
    before_event: &mut impl FnMut(&Event) -> io::Result<()>,
    send: &mut impl FnMut(&Event) -> io::Result<()>,
) -> io::Result<PlaybackOutcome> {
    if scenario.events.is_empty() {
        return Ok(PlaybackOutcome::Completed);
    }

    let mut number = 0_u64;
    while !stopped.load(Ordering::Relaxed) {
        number = number.saturating_add(1);
        let start = Instant::now();
        *pass.lock().unwrap() = Some(PlaybackPass {
            number,
            started_at: start,
        });
        let mut elapsed_micros = 0;
        let mut input = PressedInputGuard::new(send);
        for timed in &scenario.events {
            elapsed_micros += u128::from(timed.delay_micros);
            let deadline = target_time(start, elapsed_micros, options.speed)?;
            if !wait_until(deadline, stopped) {
                input.release_all()?;
                return Ok(PlaybackOutcome::Stopped);
            }
            before_event(&timed.event)?;
            input.send(&timed.event)?;
        }
        input.release_all()?;
        if matches!(options.repeats, RepeatMode::Count(count) if number == u64::from(count)) {
            return Ok(PlaybackOutcome::Completed);
        }
        if elapsed_micros == 0 && options.repeats == RepeatMode::Infinite {
            thread::yield_now();
        }
    }
    Ok(PlaybackOutcome::Stopped)
}

fn target_time(start: Instant, elapsed_micros: u128, speed: f64) -> io::Result<Instant> {
    Duration::try_from_secs_f64(elapsed_micros as f64 / 1_000_000.0 / speed)
        .ok()
        .and_then(|offset| start.checked_add(offset))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Задержка слишком велика для заданной скорости",
            )
        })
}

fn wait_until(deadline: Instant, stopped: &AtomicBool) -> bool {
    while !stopped.load(Ordering::Relaxed) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        thread::park_timeout(remaining);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::scenario::TimedEvent;

    #[test]
    fn losing_target_focus_releases_held_input_without_sending_the_next_event() {
        let key = String::from("ControlLeft");
        let down = Event::KeyDown { key: key.clone() };
        let (sender, receiver) = mpsc::channel();
        let mut checks = 0;
        let playback = play_with_guard(
            Scenario {
                coordinates: Default::default(),
                events: vec![
                    TimedEvent {
                        delay_micros: 0,
                        event: down.clone(),
                    },
                    TimedEvent {
                        delay_micros: 0,
                        event: Event::Scroll { x: 0, y: 1 },
                    },
                ],
            },
            PlaybackOptions::default(),
            move |_| {
                checks += 1;
                if checks > 1 {
                    return Err(io::Error::other("Окно потеряло фокус"));
                }
                Ok(())
            },
            move |event| {
                sender.send(event.clone()).unwrap();
                Ok(())
            },
        )
        .unwrap();
        let result = playback
            .result
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert_eq!(result.unwrap_err().to_string(), "Окно потеряло фокус");
        assert_eq!(
            receiver.try_iter().collect::<Vec<_>>(),
            [down, Event::KeyUp { key }]
        );
    }

    #[test]
    fn infinite_playback_can_stop_in_the_middle_of_a_delay() {
        let key = String::from("A");
        let down = Event::KeyDown { key: key.clone() };
        let (sender, receiver) = mpsc::channel();
        let playback = play_with_options(
            Scenario {
                coordinates: Default::default(),
                events: vec![
                    TimedEvent {
                        delay_micros: 0,
                        event: down.clone(),
                    },
                    TimedEvent {
                        delay_micros: 60_000_000,
                        event: Event::Scroll { x: 0, y: 1 },
                    },
                ],
            },
            PlaybackOptions {
                speed: 2.0,
                repeats: RepeatMode::Infinite,
            },
            move |event| {
                sender.send(event.clone()).unwrap();
                Ok(())
            },
        )
        .unwrap();
        let timeout = Duration::from_secs(3);
        assert_eq!(receiver.recv_timeout(timeout).unwrap(), down);
        playback.stop();
        assert_eq!(
            playback.result.recv_timeout(timeout).unwrap().unwrap(),
            PlaybackOutcome::Stopped
        );
        assert_eq!(
            receiver.try_iter().collect::<Vec<_>>(),
            [Event::KeyUp { key }]
        );
    }

    #[test]
    fn infinite_mode_runs_multiple_passes_and_rejects_empty_scenarios() {
        let options = PlaybackOptions {
            speed: 1.0,
            repeats: RepeatMode::Infinite,
        };
        assert_eq!(
            play_with_options(
                Scenario {
                    coordinates: Default::default(),
                    events: vec![]
                },
                options,
                |_| unreachable!()
            )
            .err()
            .unwrap()
            .kind(),
            io::ErrorKind::InvalidInput
        );
        let event = Event::Scroll { x: 0, y: 1 };
        let (sender, receiver) = mpsc::channel();
        let playback = play_with_options(
            Scenario {
                coordinates: Default::default(),
                events: vec![TimedEvent {
                    delay_micros: 5_000,
                    event: event.clone(),
                }],
            },
            options,
            move |event| {
                let _ = sender.send(event.clone());
                Ok(())
            },
        )
        .unwrap();
        let timeout = Duration::from_secs(3);
        for _ in 0..3 {
            assert_eq!(receiver.recv_timeout(timeout).unwrap(), event);
        }
        playback.stop();
        assert_eq!(
            playback.result.recv_timeout(timeout).unwrap().unwrap(),
            PlaybackOutcome::Stopped
        );
        assert!(playback.current_pass().unwrap().number >= 3);
    }

    #[test]
    fn scales_cumulative_deadlines_from_the_start_of_the_repeat() {
        let start = Instant::now();
        assert_eq!(
            target_time(start, 400_000, 2.0).unwrap(),
            start + Duration::from_millis(200)
        );
        assert_eq!(
            target_time(start, 1_000_000, 2.0).unwrap(),
            start + Duration::from_millis(500)
        );
        assert!(target_time(start, 1_000_000, f64::MIN_POSITIVE).is_err());
    }
}
