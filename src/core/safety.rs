use std::io;

use super::scenario::Event;

pub(super) struct PressedInputGuard<'a, SendEvent: FnMut(&Event) -> io::Result<()>> {
    send: &'a mut SendEvent,
    releases: Vec<Event>,
}

impl<'a, SendEvent: FnMut(&Event) -> io::Result<()>> PressedInputGuard<'a, SendEvent> {
    pub(super) fn new(send: &'a mut SendEvent) -> Self {
        Self {
            send,
            releases: Vec::new(),
        }
    }

    pub(super) fn send(&mut self, event: &Event) -> io::Result<()> {
        let release = match event {
            Event::KeyDown { key } => Some(Event::KeyUp { key: key.clone() }),
            Event::MouseDown { button } => Some(Event::MouseUp { button: *button }),
            _ => None,
        };
        if let Some(release) = release {
            if !self.releases.contains(&release) {
                // ОС могла принять нажатие до того, как адаптер вернул ошибку.
                self.releases.push(release);
            }
        }
        (self.send)(event)?;
        if matches!(event, Event::KeyUp { .. } | Event::MouseUp { .. }) {
            self.releases.retain(|release| release != event);
        }
        Ok(())
    }

    pub(super) fn release_all(&mut self) -> io::Result<()> {
        let mut result = Ok(());
        while let Some(release) = self.releases.pop() {
            result = result.and((self.send)(&release));
        }
        result
    }
}

impl<SendEvent: FnMut(&Event) -> io::Result<()>> Drop for PressedInputGuard<'_, SendEvent> {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}
