use crate::core::scenario::{Event, MouseButton, Scenario, TimedEvent};
use crate::storage::{read_json, write_json};

#[test]
fn json_preserves_all_event_kinds_in_order() {
    let scenario = Scenario {
        events: [
            Event::MouseMove { x: -1920, y: 400 },
            Event::MouseDown {
                button: MouseButton::Left,
            },
            Event::MouseUp {
                button: MouseButton::Left,
            },
            Event::Scroll { x: 0, y: -3 },
            Event::KeyDown { key: "KeyC".into() },
            Event::KeyUp { key: "KeyC".into() },
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

    assert_eq!(value["version"], 1);
    assert_eq!(read_json(json.as_slice()).unwrap(), scenario);
}

#[test]
fn json_rejects_unknown_versions() {
    let json = br#"{"version":2,"events":[]}"#;
    let error = read_json(json.as_slice()).unwrap_err();

    assert!(error.to_string().contains("unsupported scenario version"));
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
