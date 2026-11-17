use std::time::{Duration, Instant};

use super::scenario::{Event, Scenario, TimedEvent};

const MOVE_INTERVAL: Duration = Duration::from_millis(8);

pub struct Recorder {
    start: Instant,
    last_event_micros: u64,
    last_move: Option<(Instant, (i32, i32))>,
    pending_move: Option<(Instant, (i32, i32))>,
    events: Vec<TimedEvent>,
}

impl Recorder {
    pub fn new(start: Instant) -> Self {
        Self {
            start,
            last_event_micros: 0,
            last_move: None,
            pending_move: None,
            events: Vec::new(),
        }
    }

    pub fn push(&mut self, at: Instant, event: Event) {
        if let Event::MouseMove { x, y } = event {
            self.pending_move = Some((at, (x, y)));
            if self
                .last_move
                .is_none_or(|(previous, _)| at.duration_since(previous) >= MOVE_INTERVAL)
            {
                self.flush_move();
            }
        } else {
            self.flush_move();
            self.emit(at, event);
        }
    }

    pub fn push_mouse_action(&mut self, at: Instant, position: (i32, i32), event: Event) {
        if self
            .pending_move
            .is_some_and(|(_, pending)| pending != position)
            || (self.pending_move.is_none()
                && self
                    .last_move
                    .is_none_or(|(_, previous)| previous != position))
        {
            self.pending_move = Some((at, position));
        }
        self.push(at, event);
    }

    pub fn finish(mut self) -> Scenario {
        self.flush_move();
        Scenario {
            coordinates: Default::default(),
            events: self.events,
        }
    }

    fn flush_move(&mut self) {
        if let Some((at, (x, y))) = self.pending_move.take() {
            if self
                .last_move
                .is_none_or(|(_, previous)| previous != (x, y))
            {
                self.last_move = Some((at, (x, y)));
                self.emit(at, Event::MouseMove { x, y });
            }
        }
    }

    fn emit(&mut self, at: Instant, event: Event) {
        let elapsed = u64::try_from(at.duration_since(self.start).as_micros()).unwrap_or(u64::MAX);
        let delay_micros = elapsed.saturating_sub(self.last_event_micros);
        self.last_event_micros = self.last_event_micros.max(elapsed);
        self.events.push(TimedEvent {
            delay_micros,
            event,
        });
    }
}
