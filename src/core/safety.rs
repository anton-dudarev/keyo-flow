use std::io;

use super::scenario::Event;

pub(super) struct ModifierGuard<'a, SendEvent: FnMut(&Event) -> io::Result<()>> {
    send: &'a mut SendEvent,
    pressed: Vec<&'a str>,
}

impl<'a, SendEvent: FnMut(&Event) -> io::Result<()>> ModifierGuard<'a, SendEvent> {
    pub(super) fn new(send: &'a mut SendEvent) -> Self {
        Self {
            send,
            pressed: Vec::new(),
        }
    }

    pub(super) fn send(&mut self, event: &'a Event) -> io::Result<()> {
        if let Event::KeyDown { key } = event {
            if is_modifier(key) && !self.pressed.contains(&key.as_str()) {
                // Адаптер может вернуть ошибку после того, как ОС уже приняла нажатие.
                self.pressed.push(key);
            }
        }
        (self.send)(event)?;
        if let Event::KeyUp { key } = event {
            self.pressed.retain(|pressed| *pressed != key);
        }
        Ok(())
    }

    pub(super) fn release_all_modifiers(&mut self) -> io::Result<()> {
        let mut result = Ok(());
        while let Some(key) = self.pressed.pop() {
            let release = (self.send)(&Event::KeyUp { key: key.into() });
            result = result.and(release);
        }
        result
    }
}

impl<SendEvent: FnMut(&Event) -> io::Result<()>> Drop for ModifierGuard<'_, SendEvent> {
    fn drop(&mut self) {
        let _ = self.release_all_modifiers();
    }
}

fn is_modifier(key: &str) -> bool {
    matches!(
        key,
        "ControlLeft"
            | "ControlRight"
            | "ShiftLeft"
            | "ShiftRight"
            | "AltLeft"
            | "AltRight"
            | "MetaLeft"
            | "MetaRight"
    )
}
