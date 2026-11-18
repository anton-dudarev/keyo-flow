use std::collections::HashMap;
use std::io;

use serde::{Deserialize, Serialize};

use super::scenario::{Event, KeyIdentity, MouseButton, Scenario, TimedEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TimingVariation {
    pub delays_percent: u8,
    pub holds_percent: u8,
    pub seed: u32,
}

impl Default for TimingVariation {
    fn default() -> Self {
        Self {
            delays_percent: 0,
            holds_percent: 0,
            seed: 1,
        }
    }
}

#[derive(PartialEq, Eq, Hash)]
enum Pressed<'a> {
    Key(KeyIdentity<'a>),
    Mouse(MouseButton),
}

impl TimingVariation {
    pub(crate) fn validate(self) -> io::Result<()> {
        if self.delays_percent > 100 || self.holds_percent > 100 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Разброс должен быть от 0 до 100%",
            ));
        }
        Ok(())
    }

    pub(crate) fn apply(
        self,
        scenario: &Scenario,
        random: &mut TimingRandom,
    ) -> io::Result<Scenario> {
        self.validate()?;
        if self.delays_percent == 0 && self.holds_percent == 0 {
            return Ok(scenario.clone());
        }
        let mut pressed = HashMap::new();
        let mut releases = Vec::with_capacity(scenario.events.len());
        let mut times = Vec::with_capacity(scenario.events.len());
        let mut at = 0;
        for (index, timed) in scenario.events.iter().enumerate() {
            // Паузы внутри удержаний сохраняются до отдельной корректировки отпускания.
            at += if pressed.is_empty() {
                random.duration(timed.delay_micros, self.delays_percent)
            } else {
                u128::from(timed.delay_micros)
            };
            times.push(at);
            let mut released = None;
            match &timed.event {
                Event::KeyDown { key } => {
                    pressed.entry(Pressed::Key(key.identity())).or_insert(index);
                }
                Event::MouseDown { button } => {
                    pressed.entry(Pressed::Mouse(*button)).or_insert(index);
                }
                Event::KeyUp { key } => {
                    if let Some(down) = pressed.remove(&Pressed::Key(key.identity())) {
                        released = Some(down);
                    }
                }
                Event::MouseUp { button } => {
                    if let Some(down) = pressed.remove(&Pressed::Mouse(*button)) {
                        released = Some(down);
                    }
                }
                Event::MouseMove { .. } | Event::Scroll { .. } => {}
                _ => {
                    return Err(io::Error::other(
                        "Перед применением разброса нужно раскрыть события сценария",
                    ))
                }
            }
            releases.push(released.map(|down| (down, pressed.is_empty())));
        }
        let original = times.clone();
        let mut shift = 0_i128;
        for index in 0..times.len() {
            let shifted = |at: u128, shift: i128| -> io::Result<u128> {
                let at = i128::try_from(at)
                    .map_err(|_| io::Error::other("Временная шкала слишком велика"))?;
                u128::try_from(at + shift)
                    .map_err(|_| io::Error::other("Получено отрицательное время"))
            };
            times[index] = shifted(original[index], shift)?;
            if let Some((down, released_all)) = releases[index] {
                let duration = u64::try_from(original[index] - original[down])
                    .map_err(|_| io::Error::other("Удержание слишком длительное"))?;
                let wanted = times[down] + random.duration(duration, self.holds_percent);
                let lower = times[index - 1];
                let upper = if released_all {
                    u128::MAX
                } else {
                    original
                        .get(index + 1)
                        .map(|at| shifted(*at, shift))
                        .transpose()?
                        .unwrap_or(u128::MAX)
                };
                let adjusted = wanted.clamp(lower, upper);
                // После завершения всех удержаний переносим остаток шкалы вместе с паузами.
                if released_all {
                    shift += i128::try_from(adjusted)
                        .map_err(|_| io::Error::other("Время слишком велико"))?
                        - i128::try_from(times[index])
                            .map_err(|_| io::Error::other("Время слишком велико"))?;
                }
                times[index] = adjusted;
            }
        }
        let mut previous = 0;
        let events = scenario
            .events
            .iter()
            .zip(times)
            .map(|(timed, at)| {
                let delay_micros = u64::try_from(at - previous)
                    .map_err(|_| io::Error::other("Задержка с учётом разброса слишком велика"))?;
                previous = at;
                Ok(TimedEvent {
                    delay_micros,
                    event: timed.event.clone(),
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Scenario {
            coordinates: scenario.coordinates,
            events,
        })
    }
}

pub(crate) struct TimingRandom(u64);

impl TimingRandom {
    pub(crate) fn new(seed: u32) -> Self {
        Self(u64::from(seed))
    }

    fn duration(&mut self, micros: u64, percent: u8) -> u128 {
        if percent == 0 || micros == 0 {
            return u128::from(micros);
        }
        // Ограниченное нормальное распределение: заданный предел соответствует трём сигмам.
        let deviation = loop {
            let first = ((self.next() >> 11) as f64 + 1.0) / ((1_u64 << 53) as f64 + 1.0);
            let second = (self.next() >> 11) as f64 / (1_u64 << 53) as f64;
            let value = (-2.0 * first.ln()).sqrt() * (std::f64::consts::TAU * second).cos();
            if value.abs() <= 3.0 {
                break value;
            }
        };
        let value =
            (micros as f64 * (1.0 + deviation * f64::from(percent) / 300.0)).round() as u128;
        let minimum = u128::from(micros) * u128::from(100 - percent) / 100;
        let maximum = u128::from(micros) * (100 + u128::from(percent)) / 100;
        value.clamp(minimum, maximum)
    }

    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut value = self.0;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
        value ^= value >> 31;
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::scenario::{CoordinateSpace, Event, Scenario, TimedEvent};

    fn sample() -> Scenario {
        Scenario {
            coordinates: CoordinateSpace::Desktop,
            events: vec![
                TimedEvent {
                    delay_micros: 100_000,
                    event: Event::KeyDown { key: "A".into() },
                },
                TimedEvent {
                    delay_micros: 100_000,
                    event: Event::KeyUp { key: "A".into() },
                },
            ],
        }
    }

    #[test]
    fn following_idle_pauses_stay_within_their_own_bounds() {
        let mut source = sample();
        source.events.extend(sample().events);
        let settings = TimingVariation {
            delays_percent: 10,
            holds_percent: 50,
            seed: 7,
        };
        let mut random = TimingRandom::new(settings.seed);
        for _ in 0..100 {
            let result = settings.apply(&source, &mut random).unwrap();
            for (index, timed) in result.events.iter().enumerate() {
                let limits = if index % 2 == 0 {
                    90_000..=110_000
                } else {
                    50_000..=150_000
                };
                assert!(limits.contains(&timed.delay_micros));
            }
        }
    }

    #[test]
    fn overlap_and_dragging_keep_event_order_and_positions() {
        let original = Scenario {
            coordinates: CoordinateSpace::Desktop,
            events: vec![
                TimedEvent {
                    delay_micros: 1000,
                    event: Event::KeyDown {
                        key: "ControlLeft".into(),
                    },
                },
                TimedEvent {
                    delay_micros: 1000,
                    event: Event::MouseDown {
                        button: MouseButton::Left,
                    },
                },
                TimedEvent {
                    delay_micros: 1000,
                    event: Event::MouseMove { x: 150, y: 200 },
                },
                TimedEvent {
                    delay_micros: 1000,
                    event: Event::MouseUp {
                        button: MouseButton::Left,
                    },
                },
                TimedEvent {
                    delay_micros: 1000,
                    event: Event::KeyUp {
                        key: "ControlLeft".into(),
                    },
                },
            ],
        };
        for seed in 0..100 {
            let settings = TimingVariation {
                delays_percent: 100,
                holds_percent: 100,
                seed,
            };
            let result = settings
                .apply(&original, &mut TimingRandom::new(seed))
                .unwrap();
            assert_eq!(
                result.events.iter().map(|t| &t.event).collect::<Vec<_>>(),
                original.events.iter().map(|t| &t.event).collect::<Vec<_>>()
            );
            let at: Vec<_> = result
                .events
                .iter()
                .scan(0_u128, |time, event| {
                    *time += u128::from(event.delay_micros);
                    Some(*time)
                })
                .collect();
            assert!(at[0] <= at[1] && at[1] <= at[2] && at[2] <= at[3] && at[3] <= at[4]);
        }
        assert_eq!(original.events[0].delay_micros, 1000);
    }

    #[test]
    fn invalid_percentages_and_overflow_return_errors() {
        let invalid = TimingVariation {
            delays_percent: 101,
            ..Default::default()
        };
        assert!(invalid.apply(&sample(), &mut TimingRandom::new(1)).is_err());
        let mut source = sample();
        source.events[0].delay_micros = u64::MAX;
        let settings = TimingVariation {
            delays_percent: 100,
            ..Default::default()
        };
        assert!((0..20).any(|seed| settings
            .apply(&source, &mut TimingRandom::new(seed))
            .is_err()));
    }

    #[test]
    fn zero_variation_is_exact_and_seed_reproduces_a_sequence_of_passes() {
        let source = sample();
        let mut random = TimingRandom::new(1);
        assert_eq!(
            TimingVariation::default()
                .apply(&source, &mut random)
                .unwrap(),
            source
        );
        let settings = TimingVariation {
            delays_percent: 10,
            holds_percent: 10,
            seed: 42,
        };
        let mut first = TimingRandom::new(settings.seed);
        let mut second = TimingRandom::new(settings.seed);
        let a = settings.apply(&source, &mut first).unwrap();
        let b = settings.apply(&source, &mut first).unwrap();
        assert_eq!(a, settings.apply(&source, &mut second).unwrap());
        assert_eq!(b, settings.apply(&source, &mut second).unwrap());
        assert_ne!(a, b);
        for result in [a, b] {
            assert!((90_000..=110_000).contains(&result.events[0].delay_micros));
            assert!((90_000..=110_000).contains(&result.events[1].delay_micros));
            assert_eq!(
                result.events.iter().map(|t| &t.event).collect::<Vec<_>>(),
                source.events.iter().map(|t| &t.event).collect::<Vec<_>>()
            );
        }
    }
}
