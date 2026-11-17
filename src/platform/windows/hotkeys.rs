#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_win_combinations_and_rejects_duplicates() {
        let record = Shortcut {
            win: true,
            shift: true,
            key: "k".into(),
            ..Default::default()
        };
        let settings = HotkeySettings {
            record: record.clone(),
            ..Default::default()
        };
        let targets = settings.targets().unwrap();
        assert_eq!(targets[0].1.modifiers, MOD_WIN | MOD_SHIFT);
        assert_eq!(targets[0].1.code, u32::from(b'K'));
        assert!(HotkeySettings {
            record: record.clone(),
            play: record
        }
        .targets()
        .is_err());
    }
}
use std::io;
use std::ptr::null_mut;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE, WM_HOTKEY};

use super::{invalid, key_names, keyboard_code, Chord};

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct Shortcut {
    pub(crate) ctrl: bool,
    pub(crate) alt: bool,
    pub(crate) shift: bool,
    pub(crate) win: bool,
    pub(crate) key: String,
}

#[derive(Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub(crate) struct HotkeySettings {
    pub(crate) record: Shortcut,
    pub(crate) play: Shortcut,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    Record,
    Play,
}

impl Action {
    pub(crate) fn title(self) -> &'static str {
        match self {
            Self::Record => "Записать сценарий",
            Self::Play => "Воспроизвести сценарий",
        }
    }
}

impl Shortcut {
    fn chord(&self) -> io::Result<Option<Chord>> {
        let name = self.key.trim();
        if name.is_empty() {
            return Ok(None);
        }
        let name = key_names()
            .find(|key| key.eq_ignore_ascii_case(name))
            .ok_or_else(|| invalid("Неизвестная клавиша"))?;
        let (code, _) = keyboard_code(name)?;
        if matches!(
            code,
            VK_ESCAPE
                | VK_F12
                | VK_LWIN
                | VK_RWIN
                | VK_LCONTROL
                | VK_RCONTROL
                | VK_LSHIFT
                | VK_RSHIFT
                | VK_LMENU
                | VK_RMENU
        ) || name == "NumpadEnter"
        {
            return Err(invalid(
                "Эту клавишу нельзя назначить основной для глобального сочетания",
            ));
        }
        let modifiers = (if self.ctrl { MOD_CONTROL } else { 0 })
            | (if self.alt { MOD_ALT } else { 0 })
            | (if self.shift { MOD_SHIFT } else { 0 })
            | (if self.win { MOD_WIN } else { 0 });
        if modifiers == 0 && !(VK_F1..=VK_F24).contains(&code) {
            return Err(invalid(
                "Выбери хотя бы один модификатор либо функциональную клавишу",
            ));
        }
        Ok(Some(Chord {
            modifiers,
            code: u32::from(code),
        }))
    }
}

impl HotkeySettings {
    pub(crate) fn recording_shortcut(&self) -> Option<Chord> {
        self.record.chord().ok().flatten()
    }

    fn targets(&self) -> io::Result<Vec<(Action, Chord)>> {
        let mut targets = Vec::new();
        for (action, shortcut) in [(Action::Record, &self.record), (Action::Play, &self.play)] {
            if let Some(chord) = shortcut.chord().map_err(|error| {
                io::Error::new(error.kind(), format!("«{}». {error}", action.title()))
            })? {
                if targets.iter().any(|&(_, previous)| previous == chord) {
                    return Err(invalid(
                        "Для записи и воспроизведения нужны разные сочетания",
                    ));
                }
                targets.push((action, chord));
            }
        }
        Ok(targets)
    }
}

pub(crate) enum HotkeyEvent {
    Triggered(Action),
    StartupError(String),
    Applied(io::Result<HotkeySettings>),
}

pub(crate) struct HotkeyService {
    commands: Sender<HotkeySettings>,
    pub(crate) events: Receiver<HotkeyEvent>,
}

impl HotkeyService {
    pub(crate) fn start(
        initial: HotkeySettings,
        persist: impl Fn(&HotkeySettings) -> io::Result<()> + Send + 'static,
        wake: impl Fn() + Send + 'static,
    ) -> io::Result<Self> {
        let (commands, requests) = mpsc::channel();
        let (events, received) = mpsc::channel();
        thread::Builder::new()
            .name("keyo-hotkeys".into())
            .spawn(move || {
                let emit = |event| {
                    if events.send(event).is_ok() {
                        wake();
                    }
                };
                let mut active = Vec::new();
                match initial.targets() {
                    Ok(targets) => {
                        for (action, chord) in targets {
                            match Registration::register(active.len() as i32 + 1, action, chord) {
                                Ok(registration) => active.push(registration),
                                Err(error) => emit(HotkeyEvent::StartupError(error.to_string())),
                            }
                        }
                    }
                    Err(error) => emit(HotkeyEvent::StartupError(error.to_string())),
                }
                loop {
                    let mut message = MSG::default();
                    // SAFETY: читаем WM_HOTKEY из очереди этого потока в выделенный буфер.
                    while unsafe {
                        PeekMessageW(&mut message, null_mut(), WM_HOTKEY, WM_HOTKEY, PM_REMOVE)
                    } != 0
                    {
                        if let Some(registration) =
                            active.iter().find(|item| item.id == message.wParam as i32)
                        {
                            emit(HotkeyEvent::Triggered(registration.action));
                        }
                    }
                    let request = if active.is_empty() {
                        requests.recv().map_err(|_| RecvTimeoutError::Disconnected)
                    } else {
                        requests.recv_timeout(Duration::from_millis(15))
                    };
                    match request {
                        Ok(settings) => {
                            let result = apply(&mut active, &settings, &persist).map(|()| settings);
                            emit(HotkeyEvent::Applied(result));
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }
                }
            })?;
        Ok(Self {
            commands,
            events: received,
        })
    }

    pub(crate) fn apply(&self, settings: HotkeySettings) -> io::Result<()> {
        self.commands
            .send(settings)
            .map_err(|_| io::Error::other("Поток горячих клавиш недоступен"))
    }
}

fn apply(
    active: &mut Vec<Registration>,
    settings: &HotkeySettings,
    persist: &impl Fn(&HotkeySettings) -> io::Result<()>,
) -> io::Result<()> {
    let targets = settings.targets()?;
    let mut staged: Vec<Registration> = Vec::new();
    for &(action, chord) in &targets {
        if active
            .iter()
            .any(|registration| registration.chord == chord)
        {
            continue;
        }
        let id = (1..)
            .find(|id| active.iter().chain(&staged).all(|item| item.id != *id))
            .unwrap();
        staged.push(Registration::register(id, action, chord)?);
    }
    persist(settings)?;
    active.retain(|registration| {
        targets
            .iter()
            .any(|&(_, chord)| registration.chord == chord)
    });
    active.append(&mut staged);
    for registration in active {
        if let Some(&(action, _)) = targets
            .iter()
            .find(|&&(_, chord)| registration.chord == chord)
        {
            registration.action = action;
        }
    }
    Ok(())
}

struct Registration {
    id: i32,
    action: Action,
    chord: Chord,
}

impl Registration {
    fn register(id: i32, action: Action, chord: Chord) -> io::Result<Self> {
        // SAFETY: регистрация привязана к текущему потоку, идентификатор принадлежит Keyo.
        if unsafe { RegisterHotKey(null_mut(), id, chord.modifiers | MOD_NOREPEAT, chord.code) }
            == 0
        {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(error.kind(), format!(
                "Windows не приняла сочетание для «{}». Оно может быть занято или зарезервировано. {error}",
                action.title()
            )));
        }
        Ok(Self { id, action, chord })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: снимаем регистрацию в том же потоке, где она была установлена.
        unsafe { UnregisterHotKey(null_mut(), self.id) };
    }
}
