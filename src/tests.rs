use crate::core::scenario::{Event, MouseButton, Scenario, TimedEvent};
use crate::storage::{read_json, write_json};

#[test]
fn recorded_scenario_round_trips_without_overwriting_existing_file() {
    use crate::storage::write_json_new;
    use std::{fs, io};

    let path = std::env::temp_dir().join(format!("keyo-record-{}.json", std::process::id()));
    let scenario = Scenario {
        coordinates: Default::default(),
        events: vec![TimedEvent {
            delay_micros: 5_000,
            event: Event::MouseMove { x: -90, y: 5 },
        }],
    };
    write_json_new(&path, &scenario).unwrap();
    let replacement = write_json_new(
        &path,
        &Scenario {
            coordinates: Default::default(),
            events: vec![],
        },
    );
    let contents = fs::read(&path);
    fs::remove_file(&path).unwrap();

    assert_eq!(
        replacement.unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(read_json(contents.unwrap().as_slice()).unwrap(), scenario);
}

#[test]
fn recorder_preserves_click_position_and_relative_delays() {
    use crate::core::recorder::Recorder;
    use std::time::{Duration, Instant};

    let start = Instant::now();
    let at = |millis| start + Duration::from_millis(millis);
    let mut recorder = Recorder::new(start);
    recorder.push(at(2), Event::MouseMove { x: -90, y: 5 });
    recorder.push(at(4), Event::MouseMove { x: -70, y: 5 });
    recorder.push(at(6), Event::MouseMove { x: -50, y: 5 });
    recorder.push_mouse_action(
        at(7),
        (-40, 5),
        Event::MouseDown {
            button: MouseButton::Left,
        },
    );
    recorder.push_mouse_action(
        at(11),
        (-40, 5),
        Event::MouseUp {
            button: MouseButton::Left,
        },
    );
    recorder.push(at(17), Event::MouseMove { x: -20, y: 5 });
    recorder.push(at(18), Event::MouseMove { x: -10, y: 5 });

    let events = recorder.finish().events;
    assert_eq!(
        events
            .into_iter()
            .map(|timed| (timed.delay_micros, timed.event))
            .collect::<Vec<_>>(),
        [
            (2_000, Event::MouseMove { x: -90, y: 5 }),
            (5_000, Event::MouseMove { x: -40, y: 5 }),
            (
                0,
                Event::MouseDown {
                    button: MouseButton::Left
                }
            ),
            (
                4_000,
                Event::MouseUp {
                    button: MouseButton::Left
                }
            ),
            (6_000, Event::MouseMove { x: -20, y: 5 }),
            (1_000, Event::MouseMove { x: -10, y: 5 }),
        ]
    );
}

#[test]
fn json_preserves_all_event_kinds_in_order() {
    let scenario = Scenario {
        coordinates: Default::default(),
        events: [
            Event::MouseMove { x: -1920, y: 400 },
            Event::MouseDown {
                button: MouseButton::Left,
            },
            Event::MouseUp {
                button: MouseButton::Left,
            },
            Event::Scroll { x: 0, y: -3 },
            Event::KeyDown { key: "C".into() },
            Event::KeyUp { key: "C".into() },
        ]
        .into_iter()
        .enumerate()
        .map(|(index, event)| TimedEvent {
            delay_micros: index as u64 * 1250,
            event,
        })
        .collect(),
    };

    let mut json = Vec::new();
    write_json(&mut json, &scenario).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&json).unwrap();

    assert_eq!(value["version"], 2);
    assert_eq!(read_json(json.as_slice()).unwrap(), scenario);
}

#[test]
fn json_rejects_unknown_versions() {
    for json in [
        br#"{"version":1,"events":[]}"#,
        br#"{"version":4,"events":[]}"#,
    ] {
        let error = read_json(json.as_slice()).unwrap_err();
        assert!(error.to_string().contains("unsupported scenario version"));
    }
}

#[test]
fn window_scenario_round_trips_and_rejects_out_of_range_coordinates() {
    let valid = br#"{"version":3,"events":[{"delay_micros":0,"event":{"type":"mouse_move","x":500000,"y":1000000}}]}"#;
    let scenario = read_json(valid.as_slice()).unwrap();
    let mut json = Vec::new();
    write_json(&mut json, &scenario).unwrap();
    let value: serde_json::Value = serde_json::from_slice(&json).unwrap();
    assert_eq!(value["version"], 3);
    assert_eq!(read_json(json.as_slice()).unwrap(), scenario);

    let invalid = br#"{"version":3,"events":[{"delay_micros":0,"event":{"type":"mouse_move","x":-1,"y":1000001}}]}"#;
    assert!(read_json(invalid.as_slice()).is_err());
}

mod playback {
    use super::*;
    use crate::core::player::{play, PlaybackOutcome};
    use std::io;
    use std::sync::mpsc;
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(3);

    fn scenario(events: &[(u64, Event)]) -> Scenario {
        Scenario {
            coordinates: Default::default(),
            events: events
                .iter()
                .map(|(delay_micros, event)| TimedEvent {
                    delay_micros: *delay_micros,
                    event: event.clone(),
                })
                .collect(),
        }
    }

    #[test]
    fn replays_events_in_order_for_each_repeat() {
        let first = Event::MouseMove { x: -10, y: 5 };
        let second = Event::Scroll { x: 0, y: -1 };
        let (sent, received) = mpsc::channel();
        let player = play(
            scenario(&[(0, first.clone()), (0, second.clone())]),
            2.0,
            2,
            move |event| {
                sent.send(event.clone()).unwrap();
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            player.result.recv_timeout(TIMEOUT).unwrap().unwrap(),
            PlaybackOutcome::Completed
        );
        assert_eq!(
            received.try_iter().collect::<Vec<_>>(),
            [first.clone(), second.clone(), first, second]
        );
    }

    #[test]
    fn stop_skips_pending_event_and_releases_modifier_before_reporting() {
        let key = String::from("ControlLeft");
        let down = Event::KeyDown { key: key.clone() };
        let (sent, received) = mpsc::channel();
        let player = play(
            scenario(&[
                (0, down.clone()),
                (60_000_000, Event::Scroll { x: 0, y: 1 }),
            ]),
            1.0,
            1,
            move |event| {
                sent.send(event.clone()).unwrap();
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(received.recv_timeout(TIMEOUT).unwrap(), down);
        player.stop();
        assert_eq!(
            player.result.recv_timeout(TIMEOUT).unwrap().unwrap(),
            PlaybackOutcome::Stopped
        );
        assert_eq!(
            received.try_iter().collect::<Vec<_>>(),
            [Event::KeyUp { key }]
        );
    }

    #[test]
    fn stop_releases_held_key_and_mouse_button_in_reverse_order() {
        let key = String::from("A");
        let button = MouseButton::Left;
        let down = Event::KeyDown { key: key.clone() };
        let mouse_down = Event::MouseDown { button };
        let (sent, received) = mpsc::channel();
        let player = play(
            scenario(&[
                (0, down.clone()),
                (0, mouse_down.clone()),
                (60_000_000, Event::Scroll { x: 0, y: 1 }),
            ]),
            1.0,
            1,
            move |event| {
                sent.send(event.clone()).unwrap();
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(received.recv_timeout(TIMEOUT).unwrap(), down);
        assert_eq!(received.recv_timeout(TIMEOUT).unwrap(), mouse_down);
        player.stop();
        assert_eq!(
            player.result.recv_timeout(TIMEOUT).unwrap().unwrap(),
            PlaybackOutcome::Stopped
        );
        assert_eq!(
            received.try_iter().collect::<Vec<_>>(),
            [Event::MouseUp { button }, Event::KeyUp { key }]
        );
    }

    #[test]
    fn send_error_releases_modifier_before_reporting() {
        let key = String::from("ShiftRight");
        let down = Event::KeyDown { key: key.clone() };
        let (sent, received) = mpsc::channel();
        let player = play(
            scenario(&[(0, down.clone()), (0, Event::Scroll { x: 0, y: 1 })]),
            1.0,
            1,
            move |event| {
                if matches!(event, Event::Scroll { .. }) {
                    return Err(io::Error::other("Ошибка подставного адаптера"));
                }
                sent.send(event.clone()).unwrap();
                Ok(())
            },
        )
        .unwrap();

        let error = player.result.recv_timeout(TIMEOUT).unwrap().unwrap_err();
        assert_eq!(error.to_string(), "Ошибка подставного адаптера");
        assert_eq!(
            received.try_iter().collect::<Vec<_>>(),
            [down, Event::KeyUp { key }]
        );
    }

    #[test]
    fn rejects_invalid_speed_and_repeat_count() {
        for (speed, repeats) in [
            (f64::NAN, 1),
            (f64::INFINITY, 1),
            (0.0, 1),
            (-1.0, 1),
            (1.0, 0),
        ] {
            let result = play(scenario(&[]), speed, repeats, |_| {
                panic!("Неожиданный ввод")
            });
            assert_eq!(result.err().unwrap().kind(), io::ErrorKind::InvalidInput);
        }
    }
}
