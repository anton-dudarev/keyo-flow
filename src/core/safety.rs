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
            if !self
                .releases
                .iter()
                .any(|event| same_release(event, &release))
            {
                // ОС могла принять нажатие до того, как адаптер вернул ошибку.
                self.releases.push(release);
            }
        }
        (self.send)(event)?;
        if matches!(event, Event::KeyUp { .. } | Event::MouseUp { .. }) {
            self.releases
                .retain(|release| !same_release(release, event));
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

fn same_release(first: &Event, second: &Event) -> bool {
    match (first, second) {
        (Event::KeyUp { key: first }, Event::KeyUp { key: second }) => {
            first.identity() == second.identity()
        }
        _ => first == second,
    }
}

impl<SendEvent: FnMut(&Event) -> io::Result<()>> Drop for PressedInputGuard<'_, SendEvent> {
    fn drop(&mut self) {
        let _ = self.release_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::scenario::{KeyboardKey, ScanCode};

    #[test]
    fn release_matches_the_physical_key_even_if_its_name_changes() {
        let key = |name: &str, extended| KeyboardKey {
            name: name.into(),
            scan_code: Some(ScanCode {
                code: 0x48.try_into().unwrap(),
                extended,
            }),
        };
        let mut sent = Vec::new();
        {
            let mut send = |event: &Event| {
                sent.push(event.clone());
                Ok(())
            };
            let mut guard = PressedInputGuard::new(&mut send);
            guard
                .send(&Event::KeyDown {
                    key: key("Numpad8", false),
                })
                .unwrap();
            guard
                .send(&Event::KeyDown {
                    key: key("Up", true),
                })
                .unwrap();
            guard
                .send(&Event::KeyUp {
                    key: key("Up", false),
                })
                .unwrap();
        }
        assert_eq!(
            sent[2..],
            [
                Event::KeyUp {
                    key: key("Up", false)
                },
                Event::KeyUp {
                    key: key("Up", true)
                },
            ]
        );
    }
}
