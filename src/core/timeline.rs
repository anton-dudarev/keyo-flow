use std::collections::HashMap;
use std::io;

use super::scenario::{Event, MousePoint, Scenario, TimedEvent};

pub(super) fn ordered(events: Vec<(u128, Event)>) -> io::Result<Vec<TimedEvent>> {
    let mut events = events;
    events.sort_by_key(|(at, _)| *at);
    let mut previous_end = 0;
    events
        .into_iter()
        .map(|(at, event)| {
            let delay = at
                .checked_sub(previous_end)
                .ok_or_else(|| io::Error::other("Событие пересекает длительное действие"))?;
            let delay_micros =
                u64::try_from(delay).map_err(|_| io::Error::other("Слишком большая задержка"))?;
            previous_end = at + u128::from(event.duration_micros());
            Ok(TimedEvent {
                delay_micros,
                event,
            })
        })
        .collect()
}

pub(crate) fn compact(scenario: &mut Scenario) {
    let mut pending = HashMap::new();
    let mut pending_mouse = HashMap::new();
    let mut at = 0_u128;
    let mut previous_time = None;
    let mut order = 0;
    let mut rows = Vec::with_capacity(scenario.events.len());
    for timed in std::mem::take(&mut scenario.events) {
        at += u128::from(timed.delay_micros);
        order = if previous_time == Some(at) {
            order + 1
        } else {
            0
        };
        previous_time = Some(at);
        let end = at + u128::from(timed.event.duration_micros());
        match &timed.event {
            Event::MouseDown { button } if !pending_mouse.contains_key(button) => {
                pending_mouse.insert(*button, (rows.len(), at));
            }
            Event::MouseUp { button } => {
                if let Some((index, began)) = pending_mouse.remove(button) {
                    rows[index] = (
                        began,
                        Event::MouseHold {
                            button: *button,
                            duration_micros: (at - began) as u64,
                            release_order: order,
                        },
                    );
                    at = end;
                    continue;
                }
            }
            Event::KeyDown { key } if !pending.contains_key(key) => {
                pending.insert(key.clone(), (rows.len(), at));
            }
            Event::KeyUp { key } => {
                if let Some((index, began)) = pending.remove(key) {
                    rows[index] = (
                        began,
                        Event::KeySpan {
                            key: key.clone(),
                            duration_micros: (at - began) as u64,
                            release_order: order,
                        },
                    );
                    at = end;
                    continue;
                }
            }
            _ => {}
        }
        rows.push((at, timed.event));
        at = end;
    }
    let mut rows = rows.into_iter().peekable();
    let mut previous_end = 0;
    while let Some((at, mut event)) = rows.next() {
        if let Event::MouseMove { x, y } = event {
            let mut points = vec![MousePoint {
                offset_micros: 0,
                x,
                y,
            }];
            let mut last = at;
            while rows.peek().is_some_and(|(next, event)| {
                matches!(event, Event::MouseMove { .. }) && *next - last <= 100_000
            }) {
                let (next, Event::MouseMove { x, y }) = rows.next().unwrap() else {
                    unreachable!()
                };
                points.push(MousePoint {
                    offset_micros: (next - at) as u64,
                    x,
                    y,
                });
                last = next;
            }
            event = if points.len() > 1 {
                Event::MousePath { points }
            } else {
                Event::MouseMove { x, y }
            };
        }
        let end = at + u128::from(event.duration_micros());
        scenario.events.push(TimedEvent {
            delay_micros: (at - previous_end) as u64,
            event,
        });
        previous_end = end;
    }
}

impl Scenario {
    pub(crate) fn merge_hold(&mut self, down: usize, up: usize) -> io::Result<()> {
        if down >= up || up >= self.events.len() {
            return Err(io::Error::other(
                "Выдели нажатие и следующее за ним отпускание",
            ));
        }
        let mut cursor = 0_u128;
        let mut rows = Vec::with_capacity(self.events.len());
        let mut release_order = 0;
        let mut previous = None;
        for (index, timed) in self.events.iter().enumerate() {
            cursor += u128::from(timed.delay_micros);
            release_order = if previous == Some(cursor) {
                release_order + 1
            } else {
                0
            };
            previous = Some(cursor);
            rows.push((cursor, timed.event.clone()));
            if index == up {
                break;
            }
            cursor += u128::from(timed.event.duration_micros());
        }
        let duration_micros = u64::try_from(rows[up].0 - rows[down].0)
            .map_err(|_| io::Error::other("Длительность удержания слишком велика"))?;
        let event = match (&rows[down].1, &rows[up].1) {
            (Event::KeyDown { key }, Event::KeyUp { .. }) => Event::KeySpan {
                key: key.clone(),
                duration_micros,
                release_order,
            },
            (Event::MouseDown { button }, Event::MouseUp { .. }) => Event::MouseHold {
                button: *button,
                duration_micros,
                release_order,
            },
            _ => {
                return Err(io::Error::other(
                    "Выдели нажатие и отпускание клавиши либо кнопки мыши",
                ))
            }
        };
        for timed in &self.events[up + 1..] {
            cursor += u128::from(timed.delay_micros);
            rows.push((cursor, timed.event.clone()));
            cursor += u128::from(timed.event.duration_micros());
        }
        rows[down].1 = event;
        rows.remove(up);
        self.events = ordered(rows)?;
        Ok(())
    }

    pub(crate) fn duration_micros(&self) -> u128 {
        let mut cursor = 0;
        let mut end = 0;
        for timed in &self.events {
            cursor += u128::from(timed.delay_micros);
            end = end.max(
                cursor
                    + u128::from(match timed.event {
                        Event::KeySpan {
                            duration_micros, ..
                        }
                        | Event::MouseHold {
                            duration_micros, ..
                        } => duration_micros,
                        _ => timed.event.duration_micros(),
                    }),
            );
            cursor += u128::from(timed.event.duration_micros());
        }
        end
    }

    pub(crate) fn expand_recorded(&self) -> io::Result<Self> {
        self.expand_row(None)
    }

    pub(crate) fn expand_row(&self, selected: Option<usize>) -> io::Result<Self> {
        let mut cursor = 0;
        let mut release = None;
        for (index, timed) in self.events.iter().enumerate() {
            cursor += u128::from(timed.delay_micros);
            if Some(index) == selected {
                if let Event::KeySpan {
                    duration_micros, ..
                }
                | Event::MouseHold {
                    duration_micros, ..
                } = timed.event
                {
                    release = Some(cursor + u128::from(duration_micros));
                }
            }
            cursor += u128::from(timed.event.duration_micros());
        }
        let mut at = 0;
        let mut rows = Vec::new();
        let mut position = None;
        let mut releases = Vec::new();
        for (index, timed) in self.events.iter().enumerate() {
            at += u128::from(timed.delay_micros);
            let crosses_release = release.is_some_and(|release| {
                at < release && release < at + u128::from(timed.event.duration_micros())
            });
            if selected.is_none_or(|selected| selected == index) || crosses_release {
                match &timed.event {
                    Event::MouseHold {
                        button,
                        duration_micros,
                        release_order,
                    } => {
                        rows.push((at, Event::MouseDown { button: *button }));
                        releases.push((
                            at + u128::from(*duration_micros),
                            *release_order,
                            Event::MouseUp { button: *button },
                        ));
                    }
                    Event::KeySpan {
                        key,
                        duration_micros,
                        release_order,
                    } => {
                        rows.push((at, Event::KeyDown { key: key.clone() }));
                        releases.push((
                            at + u128::from(*duration_micros),
                            *release_order,
                            Event::KeyUp { key: key.clone() },
                        ));
                    }
                    Event::KeyHold {
                        key,
                        duration_micros,
                    } => {
                        rows.push((at, Event::KeyDown { key: key.clone() }));
                        rows.push((
                            at + u128::from(*duration_micros),
                            Event::KeyUp { key: key.clone() },
                        ));
                    }
                    Event::MouseMoveSmooth { x, y, .. } => {
                        let from = position.ok_or_else(|| {
                            io::Error::other("Для плавного движения нужна начальная точка")
                        })?;
                        for (offset, event) in
                            smooth_steps(from, (*x, *y), timed.event.duration_micros())
                        {
                            rows.push((at + u128::from(offset), event));
                        }
                    }
                    Event::MousePath { points } => {
                        for point in points {
                            rows.push((
                                at + u128::from(point.offset_micros),
                                Event::MouseMove {
                                    x: point.x,
                                    y: point.y,
                                },
                            ));
                        }
                    }
                    event => rows.push((at, event.clone())),
                }
            } else {
                rows.push((at, timed.event.clone()));
            }
            position = timed.event.pointer_position().or(position);
            at += u128::from(timed.event.duration_micros());
        }
        rows.sort_by_key(|(at, _)| *at);
        releases.sort_by_key(|(at, order, _)| (*at, *order));
        let mut rows = rows.into_iter().peekable();
        let mut releases = releases.into_iter().peekable();
        let mut merged = Vec::new();
        while rows.peek().is_some() || releases.peek().is_some() {
            let at = rows
                .peek()
                .map(|(at, _)| *at)
                .unwrap_or(u128::MAX)
                .min(releases.peek().map(|(at, _, _)| *at).unwrap_or(u128::MAX));
            let mut group = Vec::new();
            while rows.peek().is_some_and(|(time, _)| *time == at) {
                group.push(rows.next().unwrap().1);
            }
            while releases.peek().is_some_and(|(time, _, _)| *time == at) {
                let (_, order, event) = releases.next().unwrap();
                let index = usize::try_from(order)
                    .unwrap_or(usize::MAX)
                    .min(group.len());
                group.insert(index, event);
            }
            merged.extend(group.into_iter().map(|event| (at, event)));
        }
        Ok(Self {
            coordinates: self.coordinates,
            events: ordered(merged)?,
        })
    }
}

pub(super) fn smooth_steps(
    from: (i32, i32),
    to: (i32, i32),
    duration: u64,
) -> impl Iterator<Item = (u64, Event)> {
    // shortcut: длинный путь ограничен 600 шагами, пересмотри лимит при необходимости более частых шагов.
    let steps = duration.div_ceil(16_000).min(600);
    (1..=steps).map(move |step| {
        let t = step as f64 / steps as f64;
        let fraction = t * t * (3.0 - 2.0 * t);
        let interpolate = |start, end| {
            let start = f64::from(start);
            (start + (f64::from(end) - start) * fraction).round() as i32
        };
        (
            duration * step / steps,
            Event::MouseMove {
                x: interpolate(from.0, to.0),
                y: interpolate(from.1, to.1),
            },
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merging_uses_the_pressed_key_and_preserves_intermediate_actions() {
        let mut scenario = Scenario {
            coordinates: Default::default(),
            events: ordered(vec![
                (100, Event::KeyDown { key: "D".into() }),
                (200, Event::Scroll { x: 0, y: 1 }),
                (500, Event::KeyUp { key: "F".into() }),
                (700, Event::KeyDown { key: "A".into() }),
            ])
            .unwrap(),
        };
        let mut expected = scenario.clone();
        expected.events[2].event = Event::KeyUp { key: "D".into() };
        scenario.merge_hold(0, 2).unwrap();
        assert_eq!(scenario.events.len(), 3);
        assert!(
            matches!(&scenario.events[0].event, Event::KeySpan { key, duration_micros: 400, .. } if key == "D")
        );
        assert_eq!(scenario.expand_recorded().unwrap(), expected);
    }

    #[test]
    fn merging_mouse_buttons_uses_the_pressed_button_and_rejects_mixed_devices() {
        use super::super::scenario::MouseButton;
        let mut scenario = Scenario {
            coordinates: Default::default(),
            events: vec![
                TimedEvent {
                    delay_micros: 20,
                    event: Event::MouseDown {
                        button: MouseButton::Left,
                    },
                },
                TimedEvent {
                    delay_micros: 50,
                    event: Event::MouseUp {
                        button: MouseButton::Right,
                    },
                },
            ],
        };
        scenario.merge_hold(0, 1).unwrap();
        assert!(matches!(
            scenario.events[0].event,
            Event::MouseHold {
                button: MouseButton::Left,
                duration_micros: 50,
                ..
            }
        ));
        let mut invalid = Scenario {
            coordinates: Default::default(),
            events: vec![
                TimedEvent {
                    delay_micros: 0,
                    event: Event::KeyDown { key: "D".into() },
                },
                TimedEvent {
                    delay_micros: 50,
                    event: Event::MouseUp {
                        button: MouseButton::Right,
                    },
                },
            ],
        };
        let original = invalid.clone();
        assert!(invalid.merge_hold(0, 1).is_err());
        assert_eq!(invalid, original);
    }

    #[test]
    fn mouse_holds_preserve_dragging_and_simultaneous_releases() {
        use super::super::scenario::MouseButton;
        let original = Scenario {
            coordinates: Default::default(),
            events: ordered(vec![
                (
                    0,
                    Event::MouseDown {
                        button: MouseButton::Left,
                    },
                ),
                (
                    10,
                    Event::KeyDown {
                        key: "ShiftLeft".into(),
                    },
                ),
                (20, Event::MouseMove { x: 1, y: 2 }),
                (30, Event::MouseMove { x: 3, y: 4 }),
                (
                    30,
                    Event::MouseUp {
                        button: MouseButton::Left,
                    },
                ),
                (
                    30,
                    Event::KeyUp {
                        key: "ShiftLeft".into(),
                    },
                ),
            ])
            .unwrap(),
        };
        let mut compacted = original.clone();
        compact(&mut compacted);
        assert_eq!(compacted.events.len(), 3);
        assert!(matches!(
            compacted.events[0].event,
            Event::MouseHold {
                duration_micros: 30,
                ..
            }
        ));
        assert_eq!(compacted.expand_recorded().unwrap(), original);
        assert_eq!(
            compacted
                .expand_row(Some(0))
                .unwrap()
                .expand_recorded()
                .unwrap(),
            original
        );
    }

    #[test]
    fn compaction_preserves_overlapping_keys_and_mouse_path_timing() {
        let events = [
            (10, Event::KeyDown { key: "A".into() }),
            (20, Event::KeyDown { key: "B".into() }),
            (30, Event::MouseMove { x: 1, y: 2 }),
            (40, Event::KeyUp { key: "A".into() }),
            (50, Event::MouseMove { x: 3, y: 4 }),
            (60, Event::KeyUp { key: "B".into() }),
        ];
        let original = Scenario {
            coordinates: Default::default(),
            events: ordered(events.into()).unwrap(),
        };
        let mut compacted = original.clone();
        compact(&mut compacted);
        assert_eq!(compacted.events.len(), 3);
        assert_eq!(compacted.duration_micros(), 60);
        assert_eq!(compacted.expand_recorded().unwrap(), original);
        let split = compacted.expand_row(Some(0)).unwrap();
        assert_eq!(split.expand_recorded().unwrap(), original);
    }

    #[test]
    fn simultaneous_releases_keep_their_original_order() {
        let original = Scenario {
            coordinates: Default::default(),
            events: ordered(vec![
                (10, Event::KeyDown { key: "A".into() }),
                (20, Event::KeyDown { key: "B".into() }),
                (30, Event::KeyUp { key: "B".into() }),
                (30, Event::KeyUp { key: "A".into() }),
            ])
            .unwrap(),
        };
        let mut compacted = original.clone();
        compact(&mut compacted);
        assert_eq!(compacted.expand_recorded().unwrap(), original);
    }

    #[test]
    fn recorded_groups_round_trip_and_split_without_changing_overlaps() {
        let mut scenario = Scenario {
            coordinates: Default::default(),
            events: ordered(vec![
                (
                    0,
                    Event::KeyDown {
                        key: "ControlLeft".into(),
                    },
                ),
                (10, Event::MouseMove { x: 1, y: 2 }),
                (20, Event::MouseMove { x: 3, y: 4 }),
                (
                    20,
                    Event::KeyUp {
                        key: "ControlLeft".into(),
                    },
                ),
                (30, Event::MouseMove { x: 5, y: 6 }),
            ])
            .unwrap(),
        };
        let original = scenario.clone();
        compact(&mut scenario);
        let mut json = Vec::new();
        crate::storage::write_json(&mut json, &scenario).unwrap();
        let restored = crate::storage::read_json(json.as_slice()).unwrap();
        assert_eq!(restored, scenario);
        assert_eq!(
            restored
                .expand_row(Some(1))
                .unwrap()
                .expand_recorded()
                .unwrap(),
            original
        );
        assert_eq!(
            restored
                .expand_row(Some(0))
                .unwrap()
                .expand_recorded()
                .unwrap(),
            original
        );
    }
}
