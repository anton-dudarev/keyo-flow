use eframe::egui::{self, Align, RichText};

use crate::core::scenario::{Event, MouseButton, Scenario, TimedEvent};
use crate::platform::windows::validate_event;

use super::{close_dialog, CloseDecision};

pub(super) const EDITOR_SIZE: [f32; 2] = [820.0, 550.0];
const ROW_HEIGHT: f32 = 25.0;

pub(super) enum EditorAction {
    Apply,
    Cancel,
    SaveAndClose,
}

pub(super) struct Editor {
    pub scenario: Scenario,
    pub changed: bool,
    selected: usize,
    delay: String,
    error: Option<String>,
    adding: Option<NewPosition>,
    reveal: Option<usize>,
    close_prompt: bool,
}

impl Editor {
    pub(super) fn new(scenario: Scenario) -> Self {
        let mut editor = Self {
            scenario,
            changed: false,
            selected: 0,
            delay: String::new(),
            error: None,
            adding: None,
            reveal: None,
            close_prompt: false,
        };
        editor.select(0);
        editor
    }

    fn select(&mut self, index: usize) {
        self.selected = index;
        self.delay = self
            .scenario
            .events
            .get(index)
            .map_or_else(String::new, |event| format_delay(event.delay_micros));
        self.error = None;
    }

    pub(super) fn can_apply(&self) -> bool {
        self.adding.is_none()
            && (self.scenario.events.is_empty() || parse_delay(&self.delay).is_some())
    }

    pub(super) fn has_unsaved_input(&self) -> bool {
        self.changed || !self.can_apply()
    }

    fn insert_after_selection(&mut self, event: TimedEvent) {
        let index = (self.selected + 1).min(self.scenario.events.len());
        self.scenario.events.insert(index, event);
        self.changed = true;
        self.select(index);
        self.reveal = Some(index);
    }

    pub(super) fn show_save_error(&mut self, message: String) {
        self.error = Some(message);
    }

    pub(super) fn show(
        &mut self,
        context: &egui::Context,
        file_name: &str,
        saving: bool,
    ) -> Option<EditorAction> {
        context.show_viewport_immediate(
            egui::ViewportId::from_hash_of("keyo-scenario-editor"),
            egui::ViewportBuilder::default()
                .with_title("Редактор сценария — Keyo")
                .with_inner_size(EDITOR_SIZE)
                .with_min_inner_size([680.0, 360.0])
                .with_resizable(true)
                .with_maximize_button(true),
            |ui, _| {
                if saving {
                    if ui.input(|input| input.viewport().close_requested()) {
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    }
                    ui.label("Сохранение сценария…");
                    ui.add_enabled_ui(false, |ui| self.draw(ui));
                    return None;
                }
                if ui.input(|input| input.viewport().close_requested()) {
                    if !self.has_unsaved_input() {
                        return Some(EditorAction::Cancel);
                    }
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::CancelClose);
                    self.close_prompt = true;
                }
                let action = self.draw(ui);
                if self.close_prompt {
                    if let Some(choice) = close_dialog(
                        ui.ctx(),
                        &format!("Сохранение перезапишет файл «{file_name}»."),
                        self.can_apply(),
                    ) {
                        self.close_prompt = false;
                        return match choice {
                            CloseDecision::Save => Some(EditorAction::SaveAndClose),
                            CloseDecision::Discard => Some(EditorAction::Cancel),
                            CloseDecision::Stay => None,
                        };
                    }
                }
                action
            },
        )
    }

    pub(super) fn draw(&mut self, ui: &mut egui::Ui) -> Option<EditorAction> {
        let mut action = None;
        egui::Panel::top("editor-header")
            .exact_size(58.0)
            .show(ui, |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 36.0),
                    egui::Layout::right_to_left(Align::Center),
                    |ui| {
                        if ui
                            .add_enabled(self.can_apply(), egui::Button::new("Готово"))
                            .clicked()
                        {
                            action = Some(EditorAction::Apply);
                        }
                        if ui.button("Отмена").clicked() {
                            action = Some(EditorAction::Cancel);
                        }
                        ui.with_layout(egui::Layout::left_to_right(Align::Center), |ui| {
                            ui.label(RichText::new("Редактор сценария").size(24.0).strong());
                        });
                    },
                );
            });
        let controls_height = if self.adding.is_some() { 125.0 } else { 70.0 };
        egui::Panel::bottom("editor-controls")
            .exact_size(controls_height)
            .show(ui, |ui| {
                self.controls(ui);
                ui.allocate_ui(egui::vec2(ui.available_width(), 20.0), |ui| {
                    if let Some(error) = &self.error {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                });
            });
        egui::CentralPanel::default().show(ui, |ui| self.rows(ui));
        action
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        if let Some(new) = &mut self.adding {
            new.fields(ui);
            let (insert, cancel) = ui
                .horizontal(|ui| {
                    (
                        ui.button("Добавить").clicked().then(|| new.event()),
                        ui.button("Отменить добавление").clicked(),
                    )
                })
                .inner;
            match insert {
                Some(Ok(event)) => {
                    self.insert_after_selection(event);
                    self.adding = None;
                }
                Some(Err(error)) => self.error = Some(error),
                None if cancel => {
                    self.adding = None;
                    self.error = None;
                }
                None => {}
            }
            return;
        }
        ui.horizontal(|ui| {
            if ui.button("Добавить позицию").clicked() {
                self.adding = Some(NewPosition::default());
                self.error = None;
            }
            if self.scenario.events.is_empty() {
                return;
            }
            let label = ui.label("Задержка, мс");
            if ui
                .add(egui::TextEdit::singleline(&mut self.delay).desired_width(118.0))
                .labelled_by(label.id)
                .changed()
            {
                if let Some(delay) = parse_delay(&self.delay) {
                    let event = &mut self.scenario.events[self.selected];
                    self.changed |= event.delay_micros != delay;
                    event.delay_micros = delay;
                    self.error = None;
                } else {
                    self.error = Some(
                        "Задержка должна быть неотрицательной, до трёх дробных знаков.".into(),
                    );
                }
            }
            if ui.button("Удалить событие").clicked() {
                if self.scenario.remove_event(self.selected) {
                    self.changed = true;
                    self.select(
                        self.selected
                            .min(self.scenario.events.len().saturating_sub(1)),
                    );
                } else {
                    self.error =
                        Some("Суммарная задержка следующего события слишком велика.".into());
                }
            }
        });
    }

    fn rows(&mut self, ui: &mut egui::Ui) {
        if self.scenario.events.is_empty() {
            ui.label("В сценарии нет событий");
            return;
        }
        let mut selected = None;
        let mut moved = None;
        let mut list = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(row) = self.reveal.take() {
            list = list.vertical_scroll_offset(row.saturating_sub(2) as f32 * ROW_HEIGHT);
        }
        ui.spacing_mut().item_spacing.y = 0.0;
        list.show_rows(ui, ROW_HEIGHT, self.scenario.events.len(), |ui, rows| {
            let pointer = ui.input(|input| input.pointer.interact_pos());
            if let Some(pointer) = pointer {
                let clip = ui.clip_rect();
                if clip.contains(pointer)
                    && egui::DragAndDrop::has_payload_of_type::<usize>(ui.ctx())
                {
                    let delta = if pointer.y - clip.top() < 24.0 {
                        12.0
                    } else if clip.bottom() - pointer.y < 24.0 {
                        -12.0
                    } else {
                        0.0
                    };
                    if delta != 0.0 {
                        ui.scroll_with_delta(egui::vec2(0.0, delta));
                        ui.ctx().request_repaint();
                    }
                }
            }
            for index in rows {
                let event = &self.scenario.events[index];
                let label = format!(
                    "{}   {} · {} мс",
                    index + 1,
                    describe(&event.event),
                    format_delay(event.delay_micros)
                );
                let response = ui.add_sized(
                    [ui.available_width(), ROW_HEIGHT],
                    egui::Button::selectable(index == self.selected, label)
                        .sense(egui::Sense::click_and_drag())
                        .truncate(),
                );
                if self.adding.is_some() {
                    continue;
                }
                if response.clicked() {
                    selected = Some(index);
                }
                response.dnd_set_drag_payload(index);
                let Some(pointer) = pointer else { continue };
                let after = pointer.y >= response.rect.center().y;
                if response.dnd_hover_payload::<usize>().is_some() {
                    let y = if after {
                        response.rect.bottom()
                    } else {
                        response.rect.top()
                    };
                    ui.painter().hline(
                        response.rect.x_range(),
                        y,
                        egui::Stroke::new(2.0, ui.visuals().selection.stroke.color),
                    );
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                }
                if let Some(from) = response.dnd_release_payload::<usize>() {
                    moved = Some((*from, index + usize::from(after)));
                }
            }
        });
        if let Some((from, boundary)) = moved {
            if let Some(index) = self.scenario.move_event(from, boundary) {
                self.changed = true;
                self.select(index);
            }
        } else if let Some(index) = selected {
            self.select(index);
        }
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ActionKind {
    #[default]
    Move,
    MouseDown,
    MouseUp,
    Scroll,
    KeyDown,
    KeyUp,
}

impl ActionKind {
    const ALL: [Self; 6] = [
        Self::Move,
        Self::MouseDown,
        Self::MouseUp,
        Self::Scroll,
        Self::KeyDown,
        Self::KeyUp,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Move => "Перемещение курсора",
            Self::MouseDown => "Нажатие кнопки мыши",
            Self::MouseUp => "Отпускание кнопки мыши",
            Self::Scroll => "Прокрутка",
            Self::KeyDown => "Нажатие клавиши",
            Self::KeyUp => "Отпускание клавиши",
        }
    }
}

struct NewPosition {
    kind: ActionKind,
    x: i32,
    y: i32,
    button: MouseButton,
    key: String,
    delay: String,
}

impl Default for NewPosition {
    fn default() -> Self {
        Self {
            kind: ActionKind::Move,
            x: 0,
            y: 0,
            button: MouseButton::Left,
            key: "KeyA".into(),
            delay: "0".into(),
        }
    }
}

impl NewPosition {
    fn fields(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            egui::ComboBox::from_label("Действие")
                .selected_text(self.kind.name())
                .show_ui(ui, |ui| {
                    for kind in ActionKind::ALL {
                        ui.selectable_value(&mut self.kind, kind, kind.name());
                    }
                });
            let label = ui.label("Задержка, мс");
            ui.add(egui::TextEdit::singleline(&mut self.delay).desired_width(108.0))
                .labelled_by(label.id);
        });
        ui.horizontal(|ui| match self.kind {
            ActionKind::Move | ActionKind::Scroll => {
                let label = ui.label("X");
                ui.add(egui::DragValue::new(&mut self.x))
                    .labelled_by(label.id);
                let label = ui.label("Y");
                ui.add(egui::DragValue::new(&mut self.y))
                    .labelled_by(label.id);
            }
            ActionKind::MouseDown | ActionKind::MouseUp => {
                egui::ComboBox::from_label("Кнопка мыши")
                    .selected_text(button_name(self.button))
                    .show_ui(ui, |ui| {
                        for button in [
                            MouseButton::Left,
                            MouseButton::Right,
                            MouseButton::Middle,
                            MouseButton::Other(1),
                            MouseButton::Other(2),
                        ] {
                            ui.selectable_value(&mut self.button, button, button_name(button));
                        }
                    });
            }
            ActionKind::KeyDown | ActionKind::KeyUp => {
                let label = ui.label("Клавиша");
                ui.add(egui::TextEdit::singleline(&mut self.key).desired_width(120.0))
                    .labelled_by(label.id);
                ui.label("Например, KeyA или Enter");
            }
        });
    }

    fn to_timed_event(&self) -> Result<TimedEvent, String> {
        let delay_micros = parse_delay(&self.delay).ok_or("Проверь задержку в миллисекундах.")?;
        let event = match self.kind {
            ActionKind::Move => Event::MouseMove {
                x: self.x,
                y: self.y,
            },
            ActionKind::MouseDown => Event::MouseDown {
                button: self.button,
            },
            ActionKind::MouseUp => Event::MouseUp {
                button: self.button,
            },
            ActionKind::Scroll => Event::Scroll {
                x: i64::from(self.x),
                y: i64::from(self.y),
            },
            ActionKind::KeyDown => Event::KeyDown {
                key: self.key.trim().into(),
            },
            ActionKind::KeyUp => Event::KeyUp {
                key: self.key.trim().into(),
            },
        };
        Ok(TimedEvent {
            delay_micros,
            event,
        })
    }

    fn event(&self) -> Result<TimedEvent, String> {
        let event = self.to_timed_event()?;
        validate_event(&event.event).map_err(|error| error.to_string())?;
        Ok(event)
    }
}

fn format_delay(micros: u64) -> String {
    let millis = micros / 1_000;
    let fraction = micros % 1_000;
    if fraction == 0 {
        return millis.to_string();
    }
    format!("{millis},{fraction:03}")
        .trim_end_matches('0')
        .to_owned()
}

fn parse_delay(text: &str) -> Option<u64> {
    let text = text.trim();
    let (whole, fraction) = text.split_once([',', '.']).unwrap_or((text, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let micros = whole.parse::<u64>().ok()?.checked_mul(1_000)?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>().ok()? * 10_u64.pow(3 - fraction.len() as u32)
    };
    micros.checked_add(fraction)
}

fn describe(event: &Event) -> String {
    match event {
        Event::MouseMove { x, y } => format!("Перемещение курсора ({x}, {y})"),
        Event::MouseDown { button } => format!("Нажатие кнопки · {}", button_name(*button)),
        Event::MouseUp { button } => format!("Отпускание кнопки · {}", button_name(*button)),
        Event::Scroll { x, y } => format!("Прокрутка ({x}, {y})"),
        Event::KeyDown { key } => format!("Нажатие клавиши {key}"),
        Event::KeyUp { key } => format!("Отпускание клавиши {key}"),
    }
}

fn button_name(button: MouseButton) -> String {
    match button {
        MouseButton::Left => "левая".into(),
        MouseButton::Right => "правая".into(),
        MouseButton::Middle => "средняя".into(),
        MouseButton::Other(number) => format!("дополнительная {number}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_input_preserves_microsecond_precision_and_rejects_overflow() {
        for micros in [0, 1, 11, 1_001, 1_100, u64::MAX] {
            assert_eq!(parse_delay(&format_delay(micros)), Some(micros));
        }
        assert_eq!(parse_delay("1.25"), Some(1_250));
        for invalid in ["-1", "1,0001", "18446744073709551,616"] {
            assert_eq!(parse_delay(invalid), None);
        }
    }

    #[test]
    fn position_is_inserted_below_selection_and_is_selected() {
        let position = NewPosition {
            kind: ActionKind::KeyDown,
            delay: "1,25".into(),
            ..Default::default()
        };
        let event = position.to_timed_event().unwrap();
        let old = TimedEvent {
            delay_micros: 5,
            event: Event::Scroll { x: 0, y: 1 },
        };
        let mut editor = Editor::new(Scenario {
            events: vec![old.clone(), old.clone()],
        });
        editor.insert_after_selection(event.clone());
        assert_eq!(editor.scenario.events, [old.clone(), event.clone(), old]);
        assert_eq!(editor.selected, 1);
        assert_eq!(editor.delay, "1,25");
        let mut empty = Editor::new(Scenario { events: vec![] });
        empty.insert_after_selection(event.clone());
        assert_eq!(empty.scenario.events, [event]);
        assert_eq!(empty.selected, 0);
    }

    #[test]
    fn pointer_drag_drops_after_a_non_adjacent_row() {
        let context = egui::Context::default();
        let mut editor = Editor::new(Scenario {
            events: [10, 20, 30, 40]
                .into_iter()
                .map(|delay_micros| TimedEvent {
                    delay_micros,
                    event: Event::Scroll { x: 0, y: 1 },
                })
                .collect(),
        });
        let frame = |editor: &mut Editor, events| {
            let mut output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        EDITOR_SIZE.into(),
                    )),
                    events,
                    ..Default::default()
                },
                |ui| {
                    editor.draw(ui);
                },
            );
            output.textures_delta.clear();
            output
        };
        let output = frame(&mut editor, vec![]);
        let point = |prefix: &str, lower_half: bool| {
            output
                .shapes
                .iter()
                .find_map(|clipped| {
                    let egui::epaint::Shape::Text(text) = &clipped.shape else {
                        return None;
                    };
                    text.galley.text().starts_with(prefix).then(|| {
                        let bounds = text.visual_bounding_rect();
                        if lower_half {
                            bounds.center_bottom()
                        } else {
                            bounds.center()
                        }
                    })
                })
                .unwrap()
        };
        let origin = point("1   ", false);
        let target = point("3   ", true);
        let button = |pos, pressed| egui::Event::PointerButton {
            pos,
            pressed,
            button: egui::PointerButton::Primary,
            modifiers: egui::Modifiers::NONE,
        };
        frame(&mut editor, vec![egui::Event::PointerMoved(origin)]);
        frame(&mut editor, vec![button(origin, true)]);
        frame(&mut editor, vec![egui::Event::PointerMoved(target)]);
        frame(&mut editor, vec![button(target, false)]);
        assert_eq!(
            editor
                .scenario
                .events
                .iter()
                .map(|event| event.delay_micros)
                .collect::<Vec<_>>(),
            [20, 30, 10, 40],
        );
    }
}
