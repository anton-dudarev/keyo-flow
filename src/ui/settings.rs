use std::fs::{self, File};
use std::io::{self, BufReader};
use std::path::Path;

use eframe::egui;

use crate::platform::windows::hotkeys::{Action, HotkeySettings, Shortcut};
use crate::storage::write_replacement;

pub(super) fn load(file: Option<&Path>) -> HotkeySettings {
    file.and_then(|path| serde_json::from_reader(BufReader::new(File::open(path).ok()?)).ok())
        .unwrap_or_default()
}

pub(super) fn save(file: Option<&Path>, settings: &HotkeySettings) -> io::Result<()> {
    let Some(file) = file else {
        return Err(io::Error::other("Профиль Windows недоступен"));
    };
    fs::create_dir_all(file.parent().unwrap())?;
    write_replacement(file, |writer| {
        serde_json::to_writer(writer, settings).map_err(io::Error::from)
    })
}

pub(super) enum Choice {
    Apply,
    Cancel,
}

pub(super) struct SettingsDialog {
    pub(super) draft: HotkeySettings,
    pub(super) applying: bool,
}

impl SettingsDialog {
    pub(super) fn show(&mut self, context: &egui::Context, error: Option<&str>) -> Option<Choice> {
        let dialog = egui::Modal::new(egui::Id::unique("keyo-settings")).show(context, |ui| {
            ui.heading("Настройки");
            ui.label("Укажи клавишу, например K или F9, и нужные модификаторы.");
            ui.add_enabled_ui(!self.applying, |ui| {
                shortcut(ui, Action::Record.title(), &mut self.draft.record);
                ui.separator();
                shortcut(ui, Action::Play.title(), &mut self.draft.play);
                if let Some(error) = error {
                    ui.colored_label(ui.visuals().error_fg_color, error);
                }
                ui.horizontal(|ui| {
                    let save = ui.button("Сохранить");
                    let cancel = ui.button("Отмена");
                    if save.clicked() {
                        Some(Choice::Apply)
                    } else if cancel.clicked() {
                        Some(Choice::Cancel)
                    } else {
                        None
                    }
                })
                .inner
            })
            .inner
        });
        let dismissed = !self.applying && dialog.should_close();
        dialog.inner.or(dismissed.then_some(Choice::Cancel))
    }
}

fn shortcut(ui: &mut egui::Ui, title: &str, shortcut: &mut Shortcut) {
    ui.label(egui::RichText::new(title).strong());
    ui.horizontal(|ui| {
        ui.checkbox(&mut shortcut.ctrl, "Ctrl");
        ui.checkbox(&mut shortcut.alt, "Alt");
        ui.checkbox(&mut shortcut.shift, "Shift");
        ui.checkbox(&mut shortcut.win, "Win");
        let label = ui.label("Клавиша");
        ui.add(
            egui::TextEdit::singleline(&mut shortcut.key)
                .desired_width(115.0)
                .hint_text("Не назначена"),
        )
        .labelled_by(label.id);
        if ui.button("Очистить").clicked() {
            *shortcut = Shortcut::default();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_configured_shortcuts_in_the_profile() {
        let directory = std::env::temp_dir().join(format!("keyo-prefs-{}", std::process::id()));
        let file = directory.join("settings.json");
        let config = HotkeySettings {
            record: Shortcut {
                win: true,
                ctrl: true,
                key: "K".into(),
                ..Default::default()
            },
            play: Shortcut {
                shift: true,
                key: "F9".into(),
                ..Default::default()
            },
        };
        save(Some(&file), &config).unwrap();
        let restored = load(Some(&file));
        fs::remove_file(file).unwrap();
        fs::remove_dir(directory).unwrap();
        assert!(restored.record.win && restored.record.ctrl);
        assert_eq!(restored.record.key, "K");
        assert!(restored.play.shift);
        assert_eq!(restored.play.key, "F9");
    }
}
