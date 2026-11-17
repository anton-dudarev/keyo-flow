use std::collections::BTreeSet;

use eframe::egui::{self, Align, RichText};

use crate::core::scenario::{CoordinateSpace, Event, MouseButton, Scenario, TimedEvent};
use crate::platform::windows::{key_names, validate_event};

use super::{
    close_dialog,
    placement::{RestorePlacement, WindowPlacement},
    CloseDecision,
};

pub(super) const EDITOR_SIZE: [f32; 2] = [820.0, 550.0];
pub(super) const EDITOR_MIN_SIZE: [f32; 2] = [680.0, 360.0];
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
    selection: BTreeSet<usize>,
    anchor: usize,
    delay: String,
    error: Option<String>,
    form: Option<PositionForm>,
    reveal: Option<usize>,
    close_prompt: bool,
    restore_placement: RestorePlacement,
}

impl Editor {
    pub(super) fn new(scenario: Scenario) -> Self {
        let mut editor = Self {
            scenario,
            changed: false,
            selected: 0,
            selection: BTreeSet::new(),
            anchor: 0,
            delay: String::new(),
            error: None,
            form: None,
            reveal: None,
            close_prompt: false,
            restore_placement: RestorePlacement::default(),
        };
        editor.select(0);
        editor
    }

    fn select(&mut self, index: usize) {
        self.selection.clear();
        if index < self.scenario.events.len() {
            self.selection.insert(index);
        }
        self.anchor = index;
        self.set_active(index);
    }

    fn set_active(&mut self, index: usize) {
        self.selected = index;
        self.delay = self
            .scenario
            .events
            .get(index)
            .map_or_else(String::new, |event| format_delay(event.delay_micros));
        self.error = None;
    }

    fn select_with_modifiers(&mut self, index: usize, modifiers: egui::Modifiers) {
        if modifiers.shift {
            if !modifiers.ctrl {
                self.selection.clear();
            }
            self.selection
                .extend(self.anchor.min(index)..=self.anchor.max(index));
            self.set_active(index);
        } else if modifiers.ctrl {
            self.anchor = index;
            if !self.selection.remove(&index) {
                self.selection.insert(index);
            }
            self.set_active(
                self.selection
                    .get(&index)
                    .copied()
                    .or_else(|| self.selection.first().copied())
                    .unwrap_or(index),
            );
        } else {
            self.select(index);
        }
    }

    fn delete_selection(&mut self) {
        let Some(&first) = self.selection.first() else {
            return;
        };
        if self.scenario.remove_events(&self.selection) {
            self.changed = true;
            self.select(first.min(self.scenario.events.len().saturating_sub(1)));
        } else {
            self.error = Some("Суммарная задержка следующего события слишком велика.".into());
        }
    }

    pub(super) fn can_apply(&self) -> bool {
        self.form.is_none()
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

    fn begin_edit(&mut self, index: usize) {
        self.select(index);
        self.form = Some(PositionForm {
            position: PositionFields::from_event(&self.scenario.events[index]),
            replace: Some(index),
        });
    }

    fn apply_position(&mut self, event: TimedEvent) {
        let form = self.form.take().unwrap();
        if let Some(index) = form.replace {
            let previous = &mut self.scenario.events[index];
            self.changed |= *previous != event;
            *previous = event;
            self.select(index);
        } else {
            self.insert_after_selection(event);
        }
    }

    pub(super) fn show_save_error(&mut self, message: String) {
        self.error = Some(message);
    }

    pub(super) fn show(
        &mut self,
        context: &egui::Context,
        file_name: &str,
        saving: bool,
        placement: &mut WindowPlacement,
    ) -> Option<EditorAction> {
        context.show_viewport_immediate(
            egui::ViewportId::from_hash_of("keyo-scenario-editor"),
            egui::ViewportBuilder::default()
                .with_title("Редактор сценария — Keyo")
                .with_inner_size(EDITOR_SIZE)
                .with_min_inner_size(EDITOR_MIN_SIZE)
                .with_resizable(true)
                .with_maximize_button(true),
            |ui, class| {
                if class != egui::ViewportClass::EmbeddedWindow {
                    placement.update(ui.ctx(), &mut self.restore_placement, true);
                }
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
        let editing_row = self
            .form
            .as_ref()
            .is_some_and(|form| form.replace.is_some());
        if editing_row && !self.close_prompt && !egui::Popup::is_any_open(ui.ctx()) {
            let background = ui.interact(
                ui.max_rect(),
                ui.scope_id().with("exit-row-edit"),
                egui::Sense::click(),
            );
            if background.clicked()
                || ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
            {
                self.form = None;
                self.error = None;
            }
        }
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
        let controls_height = if self.form.is_some() { 125.0 } else { 70.0 };
        let mut editing_text = false;
        egui::Panel::bottom("editor-controls")
            .exact_size(controls_height)
            .show(ui, |ui| {
                editing_text = self.controls(ui);
                ui.allocate_ui(egui::vec2(ui.available_width(), 20.0), |ui| {
                    if let Some(error) = &self.error {
                        ui.colored_label(ui.visuals().error_fg_color, error);
                    }
                });
            });
        egui::CentralPanel::default().show(ui, |ui| self.rows(ui));
        if !editing_text
            && self.form.is_none()
            && !self.close_prompt
            && !egui::Popup::is_any_open(ui.ctx())
        {
            if ui.input_mut(|input| input.consume_key(egui::Modifiers::CTRL, egui::Key::A)) {
                self.selection = (0..self.scenario.events.len()).collect();
                ui.ctx().request_repaint();
            }
            if ui.input_mut(|input| input.consume_key(egui::Modifiers::NONE, egui::Key::Delete)) {
                self.delete_selection();
                ui.ctx().request_repaint();
            }
        }
        action
    }

    fn controls(&mut self, ui: &mut egui::Ui) -> bool {
        if let Some(form) = &mut self.form {
            let coordinates = self.scenario.coordinates;
            form.position.fields(ui, coordinates);
            let (apply, cancel) = if form.replace.is_some() {
                ("Применить", "Отменить правку")
            } else {
                ("Добавить", "Отменить добавление")
            };
            let (result, cancelled) = ui
                .horizontal(|ui| {
                    (
                        ui.button(apply)
                            .clicked()
                            .then(|| form.position.event(coordinates)),
                        ui.button(cancel).clicked(),
                    )
                })
                .inner;
            match result {
                Some(Ok(event)) => self.apply_position(event),
                Some(Err(error)) => self.error = Some(error),
                None if cancelled => {
                    self.form = None;
                    self.error = None;
                }
                None => {}
            }
            return true;
        }
        ui.horizontal(|ui| {
            if ui.button("Добавить позицию").clicked() {
                self.form = Some(PositionForm::default());
                self.error = None;
            }
            if self.scenario.events.is_empty() {
                return false;
            }
            let label = ui.label("Задержка, мс");
            let delay = ui
                .add_enabled(
                    !self.selection.is_empty(),
                    egui::TextEdit::singleline(&mut self.delay).desired_width(118.0),
                )
                .labelled_by(label.id);
            if delay.changed() {
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
            if ui
                .add_enabled(!self.selection.is_empty(), egui::Button::new("Изменить"))
                .clicked()
            {
                self.begin_edit(self.selected);
            }
            let delete_label = if self.selection.len() > 1 {
                "Удалить выбранные"
            } else {
                "Удалить событие"
            };
            if ui
                .add_enabled(!self.selection.is_empty(), egui::Button::new(delete_label))
                .clicked()
            {
                self.delete_selection();
            }
            delay.has_focus()
        })
        .inner
    }

    fn rows(&mut self, ui: &mut egui::Ui) {
        if self.scenario.events.is_empty() {
            ui.label("В сценарии нет событий");
            return;
        }
        let mut selected = None;
        let mut moved = None;
        let mut editing = None;
        let modifiers = ui.input(|input| input.modifiers);
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
                    egui::Button::selectable(self.selection.contains(&index), label)
                        .sense(egui::Sense::click_and_drag())
                        .truncate(),
                );
                if self.form.is_some() {
                    continue;
                }
                if response.double_clicked() && !modifiers.ctrl && !modifiers.shift {
                    editing = Some(index);
                } else if response.clicked() {
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
        if let Some(index) = editing {
            self.begin_edit(index);
        } else if let Some((from, boundary)) = moved {
            if let Some(index) = self.scenario.move_event(from, boundary) {
                self.changed = true;
                self.select(index);
            }
        } else if let Some(index) = selected {
            self.select_with_modifiers(index, modifiers);
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

#[derive(Default)]
struct PositionForm {
    position: PositionFields,
    replace: Option<usize>,
}

struct PositionFields {
    kind: ActionKind,
    x: i64,
    y: i64,
    button: MouseButton,
    key: String,
    key_search: String,
    key_picker_open: bool,
    delay: String,
}

impl Default for PositionFields {
    fn default() -> Self {
        Self {
            kind: ActionKind::Move,
            x: 0,
            y: 0,
            button: MouseButton::Left,
            key: "A".into(),
            key_search: String::new(),
            key_picker_open: false,
            delay: "0".into(),
        }
    }
}

impl PositionFields {
    fn from_event(timed: &TimedEvent) -> Self {
        let mut position = Self {
            delay: format_delay(timed.delay_micros),
            ..Self::default()
        };
        position.kind = match &timed.event {
            Event::MouseMove { x, y } => {
                position.x = i64::from(*x);
                position.y = i64::from(*y);
                ActionKind::Move
            }
            Event::MouseDown { button } | Event::MouseUp { button } => {
                position.button = *button;
                if matches!(timed.event, Event::MouseDown { .. }) {
                    ActionKind::MouseDown
                } else {
                    ActionKind::MouseUp
                }
            }
            Event::Scroll { x, y } => {
                position.x = *x;
                position.y = *y;
                ActionKind::Scroll
            }
            Event::KeyDown { key } | Event::KeyUp { key } => {
                position.key.clone_from(key);
                if matches!(timed.event, Event::KeyDown { .. }) {
                    ActionKind::KeyDown
                } else {
                    ActionKind::KeyUp
                }
            }
        };
        position
    }

    fn fields(&mut self, ui: &mut egui::Ui, coordinates: CoordinateSpace) {
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
                if self.kind == ActionKind::Move && coordinates == CoordinateSpace::Window {
                    ui.label("Доли окна, от 0 до 1 000 000");
                }
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
                self.key_picker(ui);
            }
        });
    }

    fn key_picker(&mut self, ui: &mut egui::Ui) {
        let popup = egui::ComboBox::from_id_salt("key-picker")
            .selected_text("Выбрать клавишу")
            .height(265.0)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show_ui(ui, |ui| {
                let search = ui.add(
                    egui::TextEdit::singleline(&mut self.key_search).hint_text("Поиск клавиши"),
                );
                if !self.key_picker_open {
                    search.request_focus();
                }
                let search_changed = search.changed();
                let query = self.key_search.trim().to_lowercase();
                let mut results = egui::ScrollArea::vertical()
                    .max_height(200.0)
                    .auto_shrink([true, false]);
                if search_changed {
                    results = results.vertical_scroll_offset(0.0);
                }
                results.show(ui, |ui| {
                    let mut found = false;
                    for name in key_names() {
                        let label = key_label(name);
                        if !query.is_empty() && !label.to_lowercase().contains(&query) {
                            continue;
                        }
                        found = true;
                        if ui
                            .selectable_label(self.key.eq_ignore_ascii_case(name), label)
                            .clicked()
                        {
                            self.key = name.into();
                            self.key_search.clear();
                            ui.close();
                        }
                    }
                    if !found {
                        ui.label("Клавиши не найдены");
                    }
                });
            });
        self.key_picker_open = popup.inner.is_some();
    }

    fn to_timed_event(&self) -> Result<TimedEvent, String> {
        let delay_micros = parse_delay(&self.delay).ok_or("Проверь задержку в миллисекундах.")?;
        let event = match self.kind {
            ActionKind::Move => Event::MouseMove {
                x: i32::try_from(self.x).map_err(|_| "Координата X вне допустимого диапазона.")?,
                y: i32::try_from(self.y).map_err(|_| "Координата Y вне допустимого диапазона.")?,
            },
            ActionKind::MouseDown => Event::MouseDown {
                button: self.button,
            },
            ActionKind::MouseUp => Event::MouseUp {
                button: self.button,
            },
            ActionKind::Scroll => Event::Scroll {
                x: self.x,
                y: self.y,
            },
            ActionKind::KeyDown | ActionKind::KeyUp => {
                let name = self.key.trim();
                let key = key_names()
                    .find(|candidate| candidate.eq_ignore_ascii_case(name))
                    .unwrap_or(name)
                    .to_owned();
                if self.kind == ActionKind::KeyDown {
                    Event::KeyDown { key }
                } else {
                    Event::KeyUp { key }
                }
            }
        };
        Ok(TimedEvent {
            delay_micros,
            event,
        })
    }

    fn event(&self, coordinates: CoordinateSpace) -> Result<TimedEvent, String> {
        let event = self.to_timed_event()?;
        validate_event(coordinates, &event.event).map_err(|error| error.to_string())?;
        Ok(event)
    }
}

fn key_label(name: &str) -> &str {
    match name {
        "Space" => "Space · Пробел",
        "Backspace" => "Backspace · Удалить слева",
        "Delete" => "Delete · Удалить справа",
        "Enter" => "Enter · Ввод",
        "ArrowUp" => "ArrowUp · Стрелка вверх",
        "ArrowDown" => "ArrowDown · Стрелка вниз",
        "ArrowLeft" => "ArrowLeft · Стрелка влево",
        "ArrowRight" => "ArrowRight · Стрелка вправо",
        "ControlLeft" => "ControlLeft · Левый Ctrl",
        "ControlRight" => "ControlRight · Правый Ctrl",
        "Escape" => "Escape · Esc",
        _ => name,
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
    fn shift_selects_ranges_both_ways_and_ctrl_toggles_individual_rows() {
        let mut editor = editor_with_rows();
        editor.select(2);
        editor.select_with_modifiers(4, egui::Modifiers::SHIFT);
        assert_eq!(editor.selection, BTreeSet::from([2, 3, 4]));
        editor.select_with_modifiers(0, egui::Modifiers::SHIFT);
        assert_eq!(editor.selection, BTreeSet::from([0, 1, 2]));
        editor.select_with_modifiers(1, egui::Modifiers::CTRL);
        editor.select_with_modifiers(4, egui::Modifiers::CTRL);
        assert_eq!(editor.selection, BTreeSet::from([0, 2, 4]));
        editor.delete_selection();
        assert_eq!(
            editor
                .scenario
                .events
                .iter()
                .map(|event| event.delay_micros)
                .collect::<Vec<_>>(),
            [30, 70]
        );
    }

    fn editor_with_rows() -> Editor {
        Editor::new(Scenario {
            coordinates: Default::default(),
            events: [10, 20, 30, 40, 50]
                .into_iter()
                .map(|delay_micros| TimedEvent {
                    delay_micros,
                    event: Event::Scroll { x: 0, y: 1 },
                })
                .collect(),
        })
    }

    #[test]
    fn select_all_and_delete_apply_to_rows_or_text_according_to_focus() {
        let context = egui::Context::default();
        let mut editor = editor_with_rows();
        let ctrl = egui::Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        };
        let press = |key, modifiers| egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        };
        frame(&context, &mut editor, vec![press(egui::Key::A, ctrl)]);
        assert_eq!(editor.selection.len(), 5);
        frame(
            &context,
            &mut editor,
            vec![press(egui::Key::Delete, egui::Modifiers::NONE)],
        );
        assert!(editor.scenario.events.is_empty());

        let context = egui::Context::default();
        let mut editor = editor_with_rows();
        let output = frame(&context, &mut editor, vec![]);
        let delay = point(&output, "0,01", false);
        frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerMoved(delay)],
        );
        frame(&context, &mut editor, vec![button(delay, true)]);
        frame(&context, &mut editor, vec![button(delay, false)]);
        frame(&context, &mut editor, vec![press(egui::Key::A, ctrl)]);
        frame(
            &context,
            &mut editor,
            vec![press(egui::Key::Delete, egui::Modifiers::NONE)],
        );
        assert!(editor.delay.is_empty());
        assert_eq!(editor.scenario.events.len(), 5);
        assert_eq!(editor.selection, BTreeSet::from([0]));
    }

    fn frame(
        context: &egui::Context,
        editor: &mut Editor,
        events: Vec<egui::Event>,
    ) -> egui::FullOutput {
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
    }

    fn point(output: &egui::FullOutput, prefix: &str, lower_half: bool) -> egui::Pos2 {
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
    }

    fn button(pos: egui::Pos2, pressed: bool) -> egui::Event {
        egui::Event::PointerButton {
            pos,
            pressed,
            button: egui::PointerButton::Primary,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn clicking_empty_editor_space_leaves_the_row_form() {
        let context = egui::Context::default();
        let original = TimedEvent {
            delay_micros: 0,
            event: Event::KeyDown { key: "A".into() },
        };
        let mut editor = Editor::new(Scenario {
            coordinates: Default::default(),
            events: vec![original.clone()],
        });
        editor.begin_edit(0);
        let empty = egui::pos2(400.0, 220.0);
        frame(&context, &mut editor, vec![]);
        frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerMoved(empty)],
        );
        frame(&context, &mut editor, vec![button(empty, true)]);
        frame(&context, &mut editor, vec![button(empty, false)]);
        assert!(editor.form.is_none());
        assert_eq!(editor.scenario.events, [original]);
    }

    #[test]
    fn typing_after_opening_the_picker_goes_to_search() {
        let context = egui::Context::default();
        let mut editor = Editor::new(Scenario {
            coordinates: Default::default(),
            events: vec![TimedEvent {
                delay_micros: 0,
                event: Event::KeyDown { key: "A".into() },
            }],
        });
        editor.begin_edit(0);
        let output = frame(&context, &mut editor, vec![]);
        let picker = point(&output, "Выбрать клавишу", false);
        frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerMoved(picker)],
        );
        frame(&context, &mut editor, vec![button(picker, true)]);
        frame(&context, &mut editor, vec![button(picker, false)]);
        frame(
            &context,
            &mut editor,
            vec![egui::Event::Text("MediaStop".into())],
        );
        assert_eq!(
            editor.form.as_ref().unwrap().position.key_search,
            "MediaStop"
        );
        frame(
            &context,
            &mut editor,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(editor.form.is_some());
        assert!(!egui::Popup::is_any_open(&context));
    }

    #[test]
    fn escape_leaves_a_row_form_without_applying_it() {
        let context = egui::Context::default();
        let original = TimedEvent {
            delay_micros: 0,
            event: Event::KeyDown { key: "A".into() },
        };
        let mut editor = Editor::new(Scenario {
            coordinates: Default::default(),
            events: vec![original.clone()],
        });
        editor.begin_edit(0);
        editor.form.as_mut().unwrap().position.key = "B".into();
        frame(
            &context,
            &mut editor,
            vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
        );
        assert!(editor.form.is_none());
        assert_eq!(editor.scenario.events, [original]);
    }

    #[test]
    fn key_picker_keeps_list_height_after_clearing_search() {
        let context = egui::Context::default();
        let mut editor = Editor::new(Scenario {
            coordinates: Default::default(),
            events: vec![],
        });
        editor.form = Some(PositionForm {
            position: PositionFields {
                kind: ActionKind::KeyDown,
                key_search: "MediaStop".into(),
                ..Default::default()
            },
            replace: None,
        });
        let output = frame(&context, &mut editor, vec![]);
        let picker = point(&output, "Выбрать клавишу", false);
        frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerMoved(picker)],
        );
        frame(&context, &mut editor, vec![button(picker, true)]);
        frame(&context, &mut editor, vec![button(picker, false)]);
        let output = frame(&context, &mut editor, vec![]);
        let result_height = |output: &egui::FullOutput, name: &str| {
            output
                .shapes
                .iter()
                .rev()
                .find_map(|clipped| {
                    let egui::epaint::Shape::Text(text) = &clipped.shape else {
                        return None;
                    };
                    (text.galley.text() == name).then_some(clipped.clip_rect.height())
                })
                .unwrap()
        };
        let filtered_height = result_height(&output, "MediaStop");
        editor.form.as_mut().unwrap().position.key_search.clear();
        let output = frame(&context, &mut editor, vec![]);
        let cleared_height = result_height(&output, "B");
        assert!(
            filtered_height >= 180.0,
            "Высота после фильтрации: {filtered_height}"
        );
        assert!(
            cleared_height >= 180.0,
            "Высота после очистки: {cleared_height}"
        );
    }

    #[test]
    fn key_picker_selects_a_function_key_from_search_results() {
        let context = egui::Context::default();
        let mut editor = Editor::new(Scenario {
            coordinates: Default::default(),
            events: vec![],
        });
        editor.form = Some(PositionForm {
            position: PositionFields {
                kind: ActionKind::KeyDown,
                key_search: "f12".into(),
                ..Default::default()
            },
            replace: None,
        });
        let output = frame(&context, &mut editor, vec![]);
        let picker = point(&output, "Выбрать клавишу", false);
        frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerMoved(picker)],
        );
        frame(&context, &mut editor, vec![button(picker, true)]);
        frame(&context, &mut editor, vec![button(picker, false)]);
        let output = frame(&context, &mut editor, vec![]);
        let result = point(&output, "F12", false);
        frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerMoved(result)],
        );
        frame(&context, &mut editor, vec![button(result, true)]);
        frame(&context, &mut editor, vec![button(result, false)]);
        assert_eq!(editor.form.unwrap().position.key, "F12");
    }

    #[test]
    fn double_click_opens_existing_values_and_cancel_discards_form_changes() {
        let context = egui::Context::default();
        let original = TimedEvent {
            delay_micros: 2500,
            event: Event::KeyUp { key: "1".into() },
        };
        let mut editor = Editor::new(Scenario {
            coordinates: Default::default(),
            events: vec![original.clone()],
        });
        let output = frame(&context, &mut editor, vec![]);
        let row = point(&output, "1   ", false);
        frame(&context, &mut editor, vec![egui::Event::PointerMoved(row)]);
        for click in 0..2 {
            frame(&context, &mut editor, vec![button(row, true)]);
            frame(&context, &mut editor, vec![button(row, false)]);
            assert_eq!(editor.form.is_some(), click == 1);
        }
        let form = editor.form.as_mut().unwrap();
        assert_eq!(form.position.to_timed_event().unwrap(), original);
        form.position.key = "2".into();

        let output = frame(&context, &mut editor, vec![]);
        let cancel = point(&output, "Отменить правку", false);
        frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerMoved(cancel)],
        );
        frame(&context, &mut editor, vec![button(cancel, true)]);
        frame(&context, &mut editor, vec![button(cancel, false)]);
        assert!(editor.form.is_none());
        assert_eq!(editor.scenario.events, [original]);
        assert!(!editor.changed);
    }

    #[test]
    fn editing_replaces_the_selected_position_without_reordering() {
        let original = TimedEvent {
            delay_micros: 3750,
            event: Event::Scroll { x: -2, y: 3 },
        };
        let mut editor = Editor::new(Scenario {
            coordinates: Default::default(),
            events: vec![original.clone(), original.clone()],
        });
        editor.begin_edit(1);
        let form = editor.form.as_mut().unwrap();
        assert_eq!(form.position.to_timed_event().unwrap(), original);
        form.position.y = -6;
        let updated = form.position.to_timed_event().unwrap();
        editor.apply_position(updated.clone());
        assert_eq!(editor.scenario.events, [original, updated]);
        assert_eq!(editor.selected, 1);
        assert!(editor.changed);
    }

    #[test]
    fn editor_normalizes_single_latin_letters() {
        for (entered, expected) in [
            (" a ", "A"),
            ("1", "1"),
            ("Enter", "Enter"),
            ("space", "Space"),
            ("f12", "F12"),
        ] {
            let position = PositionFields {
                kind: ActionKind::KeyDown,
                key: entered.into(),
                ..Default::default()
            };
            assert_eq!(
                position.to_timed_event().unwrap().event,
                Event::KeyDown {
                    key: expected.into()
                }
            );
        }
    }

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
        let position = PositionFields {
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
            coordinates: Default::default(),
            events: vec![old.clone(), old.clone()],
        });
        editor.insert_after_selection(event.clone());
        assert_eq!(editor.scenario.events, [old.clone(), event.clone(), old]);
        assert_eq!(editor.selected, 1);
        assert_eq!(editor.delay, "1,25");
        let mut empty = Editor::new(Scenario {
            coordinates: Default::default(),
            events: vec![],
        });
        empty.insert_after_selection(event.clone());
        assert_eq!(empty.scenario.events, [event]);
        assert_eq!(empty.selected, 0);
    }

    #[test]
    fn pointer_drag_drops_after_a_non_adjacent_row() {
        let context = egui::Context::default();
        let mut editor = Editor::new(Scenario {
            coordinates: Default::default(),
            events: [10, 20, 30, 40]
                .into_iter()
                .map(|delay_micros| TimedEvent {
                    delay_micros,
                    event: Event::Scroll { x: 0, y: 1 },
                })
                .collect(),
        });
        let output = frame(&context, &mut editor, vec![]);
        let origin = point(&output, "1   ", false);
        let target = point(&output, "3   ", true);
        frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerMoved(origin)],
        );
        frame(&context, &mut editor, vec![button(origin, true)]);
        frame(
            &context,
            &mut editor,
            vec![egui::Event::PointerMoved(target)],
        );
        frame(&context, &mut editor, vec![button(target, false)]);
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
