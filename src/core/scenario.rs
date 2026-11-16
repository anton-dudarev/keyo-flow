use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scenario {
    pub events: Vec<TimedEvent>,
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
