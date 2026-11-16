use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, Thread};
use std::time::{Duration, Instant};

use super::safety::ModifierGuard;
use super::scenario::{Event, Scenario};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackOutcome {
    Completed,
    Stopped,
}

pub struct Playback {
    /// Итог можно опрашивать через `try_recv` из потока интерфейса.
    pub result: mpsc::Receiver<io::Result<PlaybackOutcome>>,
    stopped: Arc<AtomicBool>,
    worker: Thread,
}

impl Playback {
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
    mut send: impl FnMut(&Event) -> io::Result<()> + Send + 'static,
) -> io::Result<Playback> {
    if !speed.is_finite() || speed <= 0.0 || repeats == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Требуются положительная конечная скорость и хотя бы один повтор",
        ));
    }

    let (sender, result) = mpsc::channel();
    let stopped = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stopped);
    let worker = thread::Builder::new()
        .name("keyo-playback".into())
        .spawn(move || {
            let outcome = run(&scenario, speed, repeats, &worker_stop, &mut send);
            let _ = sender.send(outcome);
        })?;
    Ok(Playback {
        result,
        stopped,
        worker: worker.thread().clone(),
    })
}

fn run(
    scenario: &Scenario,
    speed: f64,
    repeats: u32,
    stopped: &AtomicBool,
    send: &mut impl FnMut(&Event) -> io::Result<()>,
) -> io::Result<PlaybackOutcome> {
    if scenario.events.is_empty() {
        return Ok(PlaybackOutcome::Completed);
    }

    for _ in 0..repeats {
        let start = Instant::now();
        let mut elapsed_micros = 0;
        let mut input = ModifierGuard::new(send);
        for timed in &scenario.events {
            elapsed_micros += u128::from(timed.delay_micros);
            let deadline = target_time(start, elapsed_micros, speed)?;
            if !wait_until(deadline, stopped) {
                input.release_all_modifiers()?;
                return Ok(PlaybackOutcome::Stopped);
            }
            input.send(&timed.event)?;
        }
        input.release_all_modifiers()?;
    }
    Ok(PlaybackOutcome::Completed)
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
