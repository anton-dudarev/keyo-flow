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
