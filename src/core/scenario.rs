use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    pub events: Vec<TimedEvent>,
}

impl Scenario {
    /// Перемещает событие вместе с задержкой к границе между исходными строками.
    pub fn move_event(&mut self, from: usize, boundary: usize) -> Option<usize> {
        if from >= self.events.len() || boundary > self.events.len() {
            return None;
        }
        let to = boundary - usize::from(boundary > from);
        if from == to {
            return None;
        }
        let event = self.events.remove(from);
        self.events.insert(to, event);
        Some(to)
    }

    /// Удаляет событие, сохраняя исходные моменты всех оставшихся событий.
    pub fn remove_event(&mut self, index: usize) -> bool {
        let Some(event) = self.events.get(index) else {
            return false;
        };
        if let Some(next) = self.events.get(index + 1) {
            let Some(delay) = event.delay_micros.checked_add(next.delay_micros) else {
                return false;
            };
            self.events[index + 1].delay_micros = delay;
        }
        self.events.remove(index);
        true
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimedEvent {
    /// Time since the preceding event or since recording started.
    pub delay_micros: u64,
    pub event: Event,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    /// Absolute physical pixels in the virtual desktop.
    MouseMove {
        x: i32,
        y: i32,
    },
    MouseDown {
        button: MouseButton,
    },
    MouseUp {
        button: MouseButton,
    },
    Scroll {
        x: i64,
        y: i64,
    },
    /// Key identifier shared by the recording and playback adapters.
    KeyDown {
        key: String,
    },
    KeyUp {
        key: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Other(u8),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_rows_in_both_directions_with_their_delays() {
        let mut scenario = Scenario {
            events: [10, 20, 30, 40]
                .into_iter()
                .map(|delay_micros| TimedEvent {
                    delay_micros,
                    event: Event::MouseMove { x: 0, y: 0 },
                })
                .collect(),
        };
        assert_eq!(scenario.move_event(0, 4), Some(3));
        assert_eq!(scenario.move_event(2, 0), Some(0));
        assert_eq!(scenario.move_event(1, 2), None);
        assert_eq!(
            scenario
                .events
                .iter()
                .map(|event| event.delay_micros)
                .collect::<Vec<_>>(),
            [40, 20, 30, 10],
        );
    }

    #[test]
    fn deleting_an_event_preserves_the_next_events_timestamp() {
        let mut scenario = Scenario {
            events: vec![
                TimedEvent {
                    delay_micros: 10,
                    event: Event::MouseMove { x: 1, y: 2 },
                },
                TimedEvent {
                    delay_micros: 20,
                    event: Event::MouseMove { x: 3, y: 4 },
                },
                TimedEvent {
                    delay_micros: 40,
                    event: Event::MouseMove { x: 5, y: 6 },
                },
            ],
        };
        assert!(scenario.remove_event(1));
        assert_eq!(scenario.events[0].delay_micros, 10);
        assert_eq!(scenario.events[1].delay_micros, 60);
        assert_eq!(scenario.events[1].event, Event::MouseMove { x: 5, y: 6 });
    }

    #[test]
    fn deleting_an_event_leaves_scenario_intact_when_delays_overflow() {
        let original = Scenario {
            events: vec![
                TimedEvent {
                    delay_micros: u64::MAX,
                    event: Event::MouseMove { x: 1, y: 2 },
                },
                TimedEvent {
                    delay_micros: 1,
                    event: Event::MouseMove { x: 3, y: 4 },
                },
            ],
        };
        let mut draft = original.clone();
        assert!(!draft.remove_event(0));
        assert_eq!(draft, original);
    }
}
