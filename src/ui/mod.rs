use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufReader};
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, FontData, FontDefinitions, FontFamily, RichText, Theme};
use windows_sys::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError, GetSaveFileNameW, OFN_EXPLORER, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST,
    OPENFILENAMEW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONQUESTION, MB_YESNO,
};

use crate::core::player::{PlaybackOptions, PlaybackOutcome, PlaybackPass, RepeatMode};
use crate::core::scenario::{CoordinateSpace, Scenario};
use crate::platform::windows::hotkeys::{
    Action as HotkeyAction, HotkeyEvent, HotkeyService, HotkeySettings,
};
use crate::platform::windows::target::{open_windows, pick_window, OpenWindow};
use crate::platform::windows::{
    play_with_target, record_with_target, validate_playback, PlaybackUpdate, RecordingUpdate,
};
use crate::storage::{read_json, write_json_new, write_json_replace};

mod editor;
mod open_dialog;
mod overlay;
mod placement;
mod recent;
mod settings;
use editor::{Editor, EditorAction};
use placement::{RestorePlacement, WindowSettings};
use recent::RecentScenarios;

const WINDOW_SIZE: [f32; 2] = [620.0, 590.0];

pub fn run() -> eframe::Result {
    let recent = RecentScenarios::load();
    let windows = WindowSettings::load();
    let hotkey_file = profile_file("hotkeys.json");
    let hotkey_settings = settings::load(hotkey_file.as_deref());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(WINDOW_SIZE)
            .with_resizable(false)
            .with_maximize_button(false),
        ..Default::default()
    };
    eframe::run_native(
        "Keyo",
        options,
        Box::new(move |context| {
            configure_style(&context.egui_ctx);
            let wake = context.egui_ctx.clone();
            let (hotkeys, hotkey_error) = match HotkeyService::start(
                hotkey_settings.clone(),
                move |settings| settings::save(hotkey_file.as_deref(), settings),
                move || wake.request_repaint_of(egui::ViewportId::ROOT),
            ) {
                Ok(service) => (Some(service), None),
                Err(error) => (
                    None,
                    Some(format!("Не удалось запустить горячие клавиши. {error}")),
                ),
            };
            Ok(Box::new(MainWindow {
                recent,
                windows,
                hotkeys,
                hotkey_settings,
                hotkey_error,
                ..Default::default()
            }))
        }),
    )
}

fn profile_file(name: &str) -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|dir| PathBuf::from(dir).join("Keyo").join(name))
}

#[derive(Clone)]
struct Loaded {
    name: String,
    path: PathBuf,
    duration: Duration,
    scenario: Arc<Scenario>,
}

impl Loaded {
    fn new(path: PathBuf, scenario: Arc<Scenario>) -> Self {
        let duration = Self::duration_of(&scenario);
        let name = path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned();
        Self {
            name,
            path,
            duration,
            scenario,
        }
    }

    fn duration_of(scenario: &Scenario) -> Duration {
        Duration::from_micros(u64::try_from(scenario.duration_micros()).unwrap_or(u64::MAX))
    }
}

#[derive(Default)]
enum Status {
    #[default]
    Ready,
    Opening,
    Preparing,
    Countdown(u8),
    SelectingWindow(u8),
    WindowSelected,
    WindowSelectionCancelled,
    Playing(PlaybackPass),
    Recording,
    RecordingReady,
    RecordingCancelled,
    Unsaved,
    Saving,
    Saved,
    Stopping,
    Finished(PlaybackOutcome),
    Error(String),
}

enum Message {
    Opened(io::Result<Option<Loaded>>),
    TargetSelected(io::Result<Option<OpenWindow>>),
    Progress(Status),
    Finished(io::Result<PlaybackOutcome>),
    Recorded(io::Result<Option<Scenario>>),
    Saved(io::Result<Option<Loaded>>),
}

struct Job {
    messages: Receiver<Message>,
    stop: Option<Arc<AtomicBool>>,
    recording: bool,
}

struct PlaybackSettings {
    speed: f64,
    count: u32,
    infinite: bool,
}

impl Default for PlaybackSettings {
    fn default() -> Self {
        Self {
            speed: 1.0,
            count: 1,
            infinite: false,
        }
    }
}

impl PlaybackSettings {
    fn remaining_seconds(&self, duration: Duration, pass: u64, elapsed: Duration) -> u64 {
        let pass_seconds = duration.as_secs_f64() / self.speed;
        let current_remaining = (pass_seconds - elapsed.as_secs_f64()).max(0.0);
        let future_passes = u64::from(self.count).saturating_sub(pass);
        (current_remaining + pass_seconds * future_passes as f64).ceil() as u64
    }

    fn options(&self) -> PlaybackOptions {
        PlaybackOptions {
            speed: self.speed,
            repeats: if self.infinite {
                RepeatMode::Infinite
            } else {
                RepeatMode::Count(self.count)
            },
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.store(true, Ordering::Relaxed);
        }
    }
}

#[derive(Default)]
struct MainWindow {
    loaded: Option<Loaded>,
    job: Option<Job>,
    status: Status,
    closing: bool,
    pending_recording: Option<Arc<Scenario>>,
    dialog_owner: isize,
    playback: PlaybackSettings,
    playback_overlay: Option<overlay::Overlay>,
    playback_overlay_error: Option<String>,
    playback_target: Option<OpenWindow>,
    record_relative: bool,
    editor: Option<Editor>,
    original: Option<Loaded>,
    close_prompt: bool,
    recent: RecentScenarios,
    recent_error: Option<String>,
    windows: WindowSettings,
    restore_main: RestorePlacement,
    hotkeys: Option<HotkeyService>,
    hotkey_settings: HotkeySettings,
    hotkey_error: Option<String>,
    settings_dialog: Option<settings::SettingsDialog>,
    hotkeys_resume_at: Option<Instant>,
}

enum CloseDecision {
    Save,
    Discard,
    Stay,
}

fn close_dialog(context: &egui::Context, message: &str, can_save: bool) -> Option<CloseDecision> {
    let dialog = egui::Modal::new(egui::Id::unique(("keyo-close", context.viewport_id()))).show(
        context,
        |ui| {
            ui.heading("Сохранить изменения?");
            ui.label(message);
            ui.horizontal_wrapped(|ui| {
                if ui
                    .add_enabled(can_save, egui::Button::new("Сохранить и закрыть"))
                    .clicked()
                {
                    Some(CloseDecision::Save)
                } else if ui.button("Закрыть без сохранения").clicked() {
                    Some(CloseDecision::Discard)
                } else if ui.button("Отмена").clicked() {
                    Some(CloseDecision::Stay)
                } else {
                    None
                }
            })
            .inner
        },
    );
    let dismissed = dialog.should_close();
    dialog.inner.or(dismissed.then_some(CloseDecision::Stay))
}

impl MainWindow {
    fn poll_hotkeys(&mut self, context: &egui::Context) {
        while let Some(event) = self
            .hotkeys
            .as_ref()
            .and_then(|service| service.events.try_recv().ok())
        {
            match event {
                HotkeyEvent::Triggered(action) => {
                    if action == HotkeyAction::Record
                        && self.recording_active()
                        && matches!(self.status, Status::Preparing | Status::Countdown(_))
                    {
                        self.stop();
                        continue;
                    }
                    let available = self.job.is_none()
                        && self.settings_dialog.is_none()
                        && self.editor.is_none()
                        && !self.close_prompt
                        && !self.closing
                        && self
                            .hotkeys_resume_at
                            .is_none_or(|when| Instant::now() >= when);
                    if !available {
                        continue;
                    }
                    match action {
                        HotkeyAction::Record if self.can_open() => self.record(context),
                        HotkeyAction::Play if self.can_play() => self.play(context),
                        _ => {}
                    }
                }
                HotkeyEvent::StartupError(error) => self.hotkey_error = Some(error),
                HotkeyEvent::Applied(Ok(settings)) => {
                    self.hotkey_settings = settings;
                    self.settings_dialog = None;
                    self.hotkey_error = None;
                    self.hotkeys_resume_at = Some(Instant::now() + Duration::from_millis(60));
                }
                HotkeyEvent::Applied(Err(error)) => {
                    self.hotkey_error = Some(error.to_string());
                    if let Some(dialog) = &mut self.settings_dialog {
                        dialog.applying = false;
                    }
                }
            }
        }
    }

    fn can_play(&self) -> bool {
        self.job.is_none()
            && self.pending_recording.is_none()
            && self
                .loaded
                .as_ref()
                .is_some_and(|loaded| !loaded.scenario.events.is_empty())
    }

    fn show_settings(&mut self, context: &egui::Context) {
        let Some(dialog) = &mut self.settings_dialog else {
            return;
        };
        match dialog.show(context, self.hotkey_error.as_deref()) {
            Some(settings::Choice::Apply) => {
                let result = self
                    .hotkeys
                    .as_ref()
                    .ok_or_else(|| io::Error::other("Служба горячих клавиш недоступна"))
                    .and_then(|service| service.apply(dialog.draft.clone()));
                match result {
                    Ok(()) => dialog.applying = true,
                    Err(error) => self.hotkey_error = Some(error.to_string()),
                }
            }
            Some(settings::Choice::Cancel) => self.settings_dialog = None,
            None => {}
        }
    }

    fn can_open(&self) -> bool {
        self.job.is_none() && self.pending_recording.is_none() && self.original.is_none()
    }

    fn open(&mut self, context: &egui::Context) {
        // SAFETY: получаем HWND активного окна в потоке интерфейса.
        let owner = unsafe { GetActiveWindow() } as isize;
        let last_directory = self.recent.last_open_directory.clone();
        self.start_job(context, None, false, Status::Opening, move |send| {
            send(Message::Opened(load_selected(
                owner,
                last_directory.as_deref(),
            )));
        });
    }

    fn open_recent(&mut self, context: &egui::Context, path: PathBuf) {
        self.start_job(context, None, false, Status::Opening, move |send| {
            send(Message::Opened(load_path(path).map(Some)));
        });
    }

    fn set_loaded(&mut self, loaded: Loaded, opened: bool) {
        let remembered = if opened {
            self.recent.remember_opened(&loaded.path)
        } else {
            self.recent.remember(&loaded.path)
        };
        self.recent_error = remembered
            .err()
            .map(|error| format!("Не удалось сохранить список недавних. {error}"));
        self.loaded = Some(loaded);
    }

    fn select_window(&mut self, context: &egui::Context) {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        self.start_job(
            context,
            Some(stop),
            false,
            Status::SelectingWindow(3),
            move |send| {
                let result = pick_window(&worker_stop, |seconds| {
                    send(Message::Progress(Status::SelectingWindow(seconds)));
                });
                send(Message::TargetSelected(result));
            },
        );
    }

    fn play(&mut self, context: &egui::Context) {
        let Some(loaded) = &self.loaded else { return };
        self.playback_overlay_error = None;
        let scenario = Arc::clone(&loaded.scenario);
        let options = self.playback.options();
        let target = self.playback_target.as_ref().map(|window| window.target);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        self.start_job(context, Some(stop), false, Status::Preparing, move |send| {
            let result = play_with_target(
                (*scenario).clone(),
                options,
                &worker_stop,
                target,
                |update| {
                    send(Message::Progress(match update {
                        PlaybackUpdate::Countdown(seconds) => Status::Countdown(seconds),
                        PlaybackUpdate::Playing(pass) => Status::Playing(pass),
                    }));
                },
            );
            send(Message::Finished(result));
        });
    }

    fn record(&mut self, context: &egui::Context) {
        let target = if self.record_relative {
            let Some(window) = &self.playback_target else {
                self.status = Status::Error("Для записи координат окна выбери целевое окно".into());
                return;
            };
            Some(window.target)
        } else {
            None
        };
        // SAFETY: сохраняем HWND окна, из которого пользователь запустил запись.
        self.dialog_owner = unsafe { GetActiveWindow() } as isize;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let shortcut = self.hotkey_settings.recording_shortcut();
        self.start_job(context, Some(stop), true, Status::Preparing, move |send| {
            let result = record_with_target(&worker_stop, shortcut, target, |update| {
                send(Message::Progress(match update {
                    RecordingUpdate::Countdown(seconds) => Status::Countdown(seconds),
                    RecordingUpdate::Recording => Status::Recording,
                }));
            });
            send(Message::Recorded(result));
        });
    }

    fn save_recording(&mut self, context: &egui::Context) {
        let Some(scenario) = &self.pending_recording else {
            return;
        };
        self.save_copy(context, self.dialog_owner, Arc::clone(scenario));
    }

    fn save_current(&mut self, context: &egui::Context, confirm_overwrite: bool) {
        if self.pending_recording.is_some() {
            self.save_recording(context);
            return;
        }
        let Some(loaded) = &self.loaded else { return };
        let path = loaded.path.clone();
        let owner = self.dialog_owner;
        let scenario = Arc::clone(&loaded.scenario);
        // Черновик редактора переносится только после успешного сохранения.
        let edited = self.editor.as_ref().map(|editor| editor.scenario.clone());
        self.start_job(context, None, false, Status::Saving, move |send| {
            let scenario = edited.map_or(scenario, Arc::new);
            let result = validate_playback(&scenario).and_then(|()| {
                let saved = if confirm_overwrite {
                    save_to_path(&path, &scenario, |path| confirm_replace(owner, path))?
                } else {
                    write_json_replace(&path, &scenario)?;
                    true
                };
                Ok(saved.then(|| Loaded::new(path, scenario)))
            });
            send(Message::Saved(result));
        });
    }

    fn has_unsaved_changes(&self) -> bool {
        self.pending_recording.is_some()
            || self.original.is_some()
            || self.editor.as_ref().is_some_and(Editor::has_unsaved_input)
    }

    fn handle_close_request(&mut self, context: &egui::Context) {
        if self.input_active() {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.closing = true;
            self.stop();
        } else if self.has_unsaved_changes() {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.close_prompt = true;
        }
    }

    fn resolve_close(&mut self, choice: CloseDecision, context: &egui::Context) {
        self.close_prompt = false;
        match choice {
            CloseDecision::Save => {
                self.save_current(context, false);
                self.closing = self.job.is_some();
            }
            CloseDecision::Discard => {
                self.editor = None;
                self.original = None;
                self.pending_recording = None;
                context.send_viewport_cmd(egui::ViewportCommand::Close);
            }
            CloseDecision::Stay => {}
        }
    }

    fn save_copy(&mut self, context: &egui::Context, owner: isize, scenario: Arc<Scenario>) {
        self.start_job(context, None, false, Status::Saving, move |send| {
            let result = select_save_json(owner).and_then(|path| {
                let Some(path) = path else { return Ok(None) };
                validate_playback(&scenario)?;
                let saved = save_to_path(&path, &scenario, |path| confirm_replace(owner, path))?;
                Ok(saved.then(|| Loaded::new(path, scenario)))
            });
            send(Message::Saved(result));
        });
    }

    fn finish_editing(&mut self, action: EditorAction) {
        let editor = self.editor.take().unwrap();
        if matches!(action, EditorAction::Apply) && editor.changed {
            let loaded = self.loaded.as_mut().unwrap();
            self.original.get_or_insert_with(|| loaded.clone());
            loaded.duration = Loaded::duration_of(&editor.scenario);
            loaded.scenario = Arc::new(editor.scenario);
            self.status = Status::Unsaved;
        }
    }

    fn start_job(
        &mut self,
        context: &egui::Context,
        stop: Option<Arc<AtomicBool>>,
        recording: bool,
        status: Status,
        work: impl FnOnce(&(dyn Fn(Message) + Sync)) + Send + 'static,
    ) {
        let (sender, messages) = mpsc::channel();
        let context = context.clone();
        let worker = thread::Builder::new()
            .name("keyo-gui-task".into())
            .spawn(move || {
                work(&|message| {
                    let _ = sender.send(message);
                    context.request_repaint();
                });
                drop(sender);
                context.request_repaint();
            });
        match worker {
            Ok(_) => {
                self.job = Some(Job {
                    messages,
                    stop,
                    recording,
                });
                self.status = status;
            }
            Err(error) => {
                self.status = Status::Error(format!("Не удалось запустить задачу. {error}"))
            }
        }
    }

    fn stop(&mut self) {
        if let Some(stop) = self.job.as_ref().and_then(|job| job.stop.as_ref()) {
            stop.store(true, Ordering::Relaxed);
            self.status = Status::Stopping;
        }
    }

    fn poll_messages(&mut self) {
        while let Some(job) = &self.job {
            match job.messages.try_recv() {
                Ok(Message::Progress(status)) => {
                    if !matches!(self.status, Status::Stopping) {
                        self.status = status;
                    }
                }
                Ok(Message::Opened(result)) => {
                    self.job = None;
                    self.status = match result {
                        Ok(loaded) => {
                            if let Some(loaded) = loaded {
                                self.set_loaded(loaded, true);
                            }
                            Status::Ready
                        }
                        Err(error) => {
                            Status::Error(format!("Не удалось открыть сценарий. {error}"))
                        }
                    };
                }
                Ok(Message::Finished(result)) => {
                    self.job = None;
                    self.status = match result {
                        Ok(outcome) => Status::Finished(outcome),
                        Err(error) => Status::Error(format!("Воспроизведение прервано. {error}")),
                    };
                }
                Ok(Message::TargetSelected(result)) => {
                    self.job = None;
                    self.status = match result {
                        _ if matches!(self.status, Status::Stopping) => {
                            Status::WindowSelectionCancelled
                        }
                        Ok(Some(window)) => {
                            self.playback_target = Some(window);
                            Status::WindowSelected
                        }
                        Ok(None) => Status::WindowSelectionCancelled,
                        Err(error) => Status::Error(format!("Не удалось выбрать окно. {error}")),
                    };
                }
                Ok(Message::Recorded(result)) => {
                    self.job = None;
                    self.status = match result {
                        Ok(Some(scenario)) => {
                            self.pending_recording = Some(Arc::new(scenario));
                            Status::RecordingReady
                        }
                        Ok(None) => Status::RecordingCancelled,
                        Err(error) => {
                            Status::Error(format!("Не удалось записать сценарий. {error}"))
                        }
                    };
                }
                Ok(Message::Saved(result)) => {
                    self.job = None;
                    self.status = match result {
                        Ok(Some(loaded)) => {
                            self.pending_recording = None;
                            self.original = None;
                            self.editor = None;
                            self.set_loaded(loaded, false);
                            Status::Saved
                        }
                        Ok(None) => {
                            self.closing = false;
                            Status::Unsaved
                        }
                        Err(error) => {
                            self.closing = false;
                            let error = format!("Не удалось сохранить сценарий. {error}");
                            if let Some(editor) = &mut self.editor {
                                editor.show_save_error(error.clone());
                            }
                            Status::Error(error)
                        }
                    };
                }
                Err(TryRecvError::Disconnected) => {
                    self.job = None;
                    self.closing = false;
                    self.status = Status::Error("Фоновая задача завершилась без результата".into());
                }
                Err(TryRecvError::Empty) => break,
            }
        }
    }
}

impl eframe::App for MainWindow {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.windows.main.update(ctx, &mut self.restore_main, false);
        self.poll_hotkeys(ctx);
        let input_was_active = self.input_active();
        self.poll_messages();
        if input_was_active && !self.input_active() {
            self.hotkeys_resume_at = Some(Instant::now() + Duration::from_millis(60));
        }
        if ctx.input(|input| input.viewport().close_requested()) {
            self.handle_close_request(ctx);
        }
        if self.closing && self.job.is_none() {
            self.closing = false;
            if self.has_unsaved_changes() {
                self.close_prompt = true;
                if matches!(self.status, Status::RecordingReady) {
                    self.status = Status::Unsaved;
                }
            } else {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        if !self.recording_active() && ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.stop();
        }
        if matches!(self.status, Status::RecordingReady) && !self.close_prompt {
            self.save_recording(ctx);
        }
        if let Some(labels) = self
            .playback_indicator()
            .filter(|_| self.playback_overlay_error.is_none())
        {
            let progress = self.progress(ctx);
            if let Err(error) = overlay::Overlay::show(
                &mut self.playback_overlay,
                ctx.pixels_per_point(),
                ctx.theme() == Theme::Dark,
                labels,
                progress,
            ) {
                self.playback_overlay = None;
                self.playback_overlay_error =
                    Some(format!("Не удалось показать индикатор. {error}"));
            }
        } else {
            self.playback_overlay = None;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        ui.add_enabled_ui(
            self.editor.is_none() && self.settings_dialog.is_none(),
            |ui| self.draw(ui),
        );
        if self.settings_dialog.is_some() && !self.close_prompt {
            self.show_settings(&context);
        }
        let file_name = self.loaded.as_ref().map(|loaded| loaded.name.as_str());
        if self.close_prompt {
            let can_save = self.job.is_none() && self.editor.as_ref().is_none_or(Editor::can_apply);
            if self.job.is_none() {
                let message = match file_name.filter(|_| self.pending_recording.is_none()) {
                    Some(name) => format!("Сохранение перезапишет файл «{name}»."),
                    None => "Запись ещё не сохранена. Keyo предложит выбрать файл.".into(),
                };
                if let Some(choice) = close_dialog(&context, &message, can_save) {
                    self.resolve_close(choice, &context);
                }
            }
        } else if let Some(action) = self.editor.as_mut().and_then(|editor| {
            editor.show(
                &context,
                file_name.unwrap_or_default(),
                self.job.is_some(),
                &mut self.windows.editor,
            )
        }) {
            if matches!(action, EditorAction::SaveAndClose) {
                self.save_current(&context, false);
            } else {
                self.finish_editing(action);
            }
        }
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        let _ = self.windows.save();
    }
}

impl MainWindow {
    fn draw(&mut self, ui: &mut egui::Ui) {
        let context = ui.ctx().clone();
        let dark = ui.visuals().dark_mode;
        let accent = if dark {
            Color32::from_rgb(96, 205, 255)
        } else {
            Color32::from_rgb(0, 95, 184)
        };
        let green = playback_color(dark);
        let (red, button_text) = if dark {
            (Color32::from_rgb(255, 179, 180), Color32::BLACK)
        } else {
            (Color32::from_rgb(164, 38, 44), Color32::WHITE)
        };
        let (surface, border) = card_colors(dark);
        let card = egui::Frame::new()
            .fill(surface)
            .stroke(egui::Stroke::new(1.0, border))
            .corner_radius(8)
            .inner_margin(16);
        let weak = if dark {
            Color32::from_gray(188)
        } else {
            Color32::from_gray(89)
        };
        let active = self.input_active();
        let has_events = self
            .loaded
            .as_ref()
            .is_some_and(|loaded| !loaded.scenario.events.is_empty());
        egui::Frame::new().inner_margin(25).show(ui, |ui| {
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 36.0),
                egui::Layout::right_to_left(Align::Center),
                |ui| {
                    if self.pending_recording.is_none()
                        && self.original.is_none()
                        && action_button(
                            ui,
                            self.job.is_none(),
                            ActionIcon::Settings,
                            "",
                            surface,
                            ui.visuals().text_color(),
                            36.0,
                        )
                        .on_hover_text("Настройки")
                        .clicked()
                    {
                        self.settings_dialog = Some(settings::SettingsDialog {
                            draft: self.hotkey_settings.clone(),
                            applying: false,
                        });
                    }
                    if self.pending_recording.is_some() {
                        if ui
                            .add_enabled(self.job.is_none(), egui::Button::new("Сохранить JSON"))
                            .clicked()
                        {
                            self.save_recording(&context);
                        }
                        if ui
                            .add_enabled(self.job.is_none(), egui::Button::new("Отбросить запись"))
                            .clicked()
                        {
                            self.pending_recording = None;
                            self.status = Status::Ready;
                        }
                    } else if self.original.is_some() {
                        if ui
                            .add_enabled(
                                self.job.is_none(),
                                egui::Button::new("Сохранить изменения"),
                            )
                            .clicked()
                        {
                            self.save_current(&context, true);
                        }
                        if ui
                            .add_enabled(self.job.is_none(), egui::Button::new("Отбросить правки"))
                            .clicked()
                        {
                            self.loaded = self.original.take();
                            self.status = Status::Ready;
                        }
                    } else if action_button(
                        ui,
                        self.job.is_none(),
                        ActionIcon::Record,
                        "Записать",
                        red,
                        button_text,
                        111.0,
                    )
                    .clicked()
                    {
                        self.record(&context);
                    }
                    let (icon, label, fill) = if active {
                        (ActionIcon::Stop, "Стоп", red)
                    } else {
                        (ActionIcon::Play, "Воспроизвести", green)
                    };
                    let enabled =
                        (active && !matches!(self.status, Status::Stopping)) || self.can_play();
                    if action_button(ui, enabled, icon, label, fill, button_text, 140.0).clicked() {
                        if active {
                            self.stop();
                        } else {
                            self.play(&context);
                        }
                    }
                    ui.with_layout(egui::Layout::left_to_right(Align::Center), |ui| {
                        ui.label(RichText::new("Сценарий").size(29.0).strong());
                    });
                },
            );
            ui.add_space(15.0);
            card.show(ui, |ui| {
                ui.allocate_ui_with_layout(
                    egui::vec2(ui.available_width(), 40.0),
                    egui::Layout::right_to_left(Align::Center),
                    |ui| {
                        if ui
                            .add_enabled(
                                self.can_open(),
                                egui::Button::new("Открыть JSON").min_size(egui::vec2(120.0, 36.0)),
                            )
                            .clicked()
                        {
                            self.open(&context);
                        }
                        if let Some(loaded) = &self.loaded {
                            if ui
                                .add_enabled(
                                    self.job.is_none() && self.pending_recording.is_none(),
                                    egui::Button::new("Редактировать")
                                        .min_size(egui::vec2(116.0, 36.0)),
                                )
                                .clicked()
                            {
                                // SAFETY: здесь ещё активно основное окно, владелец диалога сохранения.
                                self.dialog_owner = unsafe { GetActiveWindow() } as isize;
                                self.editor = Some(Editor::new((*loaded.scenario).clone()));
                            }
                        }
                        if let Some(loaded) = &self.loaded {
                            ui.with_layout(egui::Layout::top_down(Align::Min), |ui| {
                                ui.add(
                                    egui::Label::new(RichText::new(&loaded.name).strong())
                                        .truncate(),
                                )
                                .on_hover_text(&loaded.name);
                                let seconds = format!("{:.2}", loaded.duration.as_secs_f64())
                                    .replace('.', ",");
                                ui.label(
                                    RichText::new(format!(
                                        "{} · {seconds} с{}",
                                        event_count(loaded.scenario.events.len()),
                                        if self.original.is_some() {
                                            " · есть правки"
                                        } else {
                                            ""
                                        },
                                    ))
                                    .color(weak),
                                );
                                if loaded.scenario.coordinates == CoordinateSpace::Window {
                                    ui.label(RichText::new("Координаты целевого окна").color(weak));
                                }
                            });
                        } else {
                            ui.with_layout(egui::Layout::left_to_right(Align::Center), |ui| {
                                ui.label(RichText::new("Сценарий не выбран").strong());
                            });
                        }
                    },
                );
            });
            ui.add_space(12.0);
            let can_configure = self.job.is_none() && self.pending_recording.is_none();
            ui.add_enabled_ui(can_configure, |ui| {
                ui.horizontal(|ui| {
                    let speed_label = ui.label("Скорость");
                    ui.add(
                        egui::DragValue::new(&mut self.playback.speed)
                            .range(0.25..=4.0)
                            .speed(0.05)
                            .max_decimals(2)
                            .suffix("×"),
                    )
                    .labelled_by(speed_label.id);
                    let count_label = ui.label("Запуски");
                    ui.add_enabled(
                        !self.playback.infinite,
                        egui::DragValue::new(&mut self.playback.count).range(1..=u32::MAX),
                    )
                    .labelled_by(count_label.id);
                    ui.checkbox(&mut self.playback.infinite, "Повторять бесконечно");
                });
                self.draw_playback_target(ui);
                ui.checkbox(
                    &mut self.record_relative,
                    "Записывать координаты относительно окна",
                );
            });
            let text = self.status_text();
            if !text.is_empty() {
                ui.add_space(12.0);
                if matches!(self.status, Status::Error(_)) {
                    ui.colored_label(ui.visuals().error_fg_color, text);
                } else {
                    ui.label(RichText::new(text).strong());
                }
            }
            if let Some(error) = &self.playback_overlay_error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            }
            if active {
                let hint = if matches!(self.status, Status::SelectingWindow(_)) {
                    "Отмена выбора: Esc"
                } else if self.recording_active() {
                    if self.hotkey_settings.recording_shortcut().is_some() {
                        "Для остановки повтори сочетание записи или нажми «Стоп»"
                    } else {
                        "Нажми «Стоп», чтобы завершить запись"
                    }
                } else if matches!(self.status, Status::Countdown(_)) {
                    "Переключись в целевое окно. Остановка: Esc или верхний левый угол"
                } else {
                    "Остановка: Esc или верхний левый угол основного монитора"
                };
                ui.label(RichText::new(hint).color(weak));
            } else if has_events && matches!(self.status, Status::Ready | Status::Saved) {
                ui.label(RichText::new("После запуска переключись в целевое окно.").color(weak));
            }
            if let Some(fraction) = self.progress(&context) {
                ui.add_space(12.0);
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .desired_height(4.0)
                        .fill(accent),
                );
            }
            ui.add_space(12.0);
            ui.label(RichText::new("Недавние сценарии").strong());
            card.show(ui, |ui| {
                ui.set_min_width(ui.available_width());
                self.draw_recent(ui);
            });
        });
    }

    fn draw_playback_target(&mut self, ui: &mut egui::Ui) {
        let mut pick = false;
        let mut changed = false;
        ui.horizontal(|ui| {
            let label = ui.label("Целевое окно");
            let selected = self.playback_target.as_ref();
            egui::ComboBox::from_id_salt("playback_target")
                .selected_text(selected.map_or("Любое активное окно", |window| {
                    window.title.as_str()
                }))
                .width(300.0)
                .truncate()
                .show_ui(ui, |ui| {
                    ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                    if ui.button("Выбрать переключением").clicked() {
                        pick = true;
                        ui.close();
                    }
                    if ui
                        .selectable_label(self.playback_target.is_none(), "Любое активное окно")
                        .clicked()
                    {
                        self.playback_target = None;
                        changed = true;
                        ui.close();
                    }
                    for window in open_windows() {
                        let selected = self
                            .playback_target
                            .as_ref()
                            .is_some_and(|selected| selected.target == window.target);
                        if ui
                            .selectable_label(selected, &window.title)
                            .on_hover_text(&window.title)
                            .clicked()
                        {
                            self.playback_target = Some(window);
                            changed = true;
                            ui.close();
                        }
                    }
                })
                .response
                .labelled_by(label.id);
        });
        if pick {
            self.select_window(ui.ctx());
        } else if changed
            && matches!(
                self.status,
                Status::WindowSelected | Status::WindowSelectionCancelled
            )
        {
            self.status = Status::Ready;
        }
    }

    fn draw_recent(&mut self, ui: &mut egui::Ui) {
        if let Some(error) = &self.recent_error {
            ui.colored_label(ui.visuals().error_fg_color, error);
        }
        if self.recent.paths.is_empty() {
            ui.label("Недавних сценариев пока нет");
            return;
        }
        let mut chosen = None;
        ui.add_enabled_ui(self.can_open(), |ui| {
            egui::ScrollArea::vertical()
                .max_height(275.0)
                .show(ui, |ui| {
                    for path in &self.recent.paths {
                        let name = path
                            .file_name()
                            .unwrap_or(path.as_os_str())
                            .to_string_lossy();
                        let selected = self
                            .loaded
                            .as_ref()
                            .is_some_and(|loaded| &loaded.path == path);
                        let row = ui
                            .add_sized(
                                [ui.available_width(), 22.0],
                                egui::Button::selectable(selected, name).truncate(),
                            )
                            .on_hover_text(path.to_string_lossy());
                        if row.double_clicked()
                            || (row.has_focus()
                                && ui.input(|input| input.key_pressed(egui::Key::Enter)))
                        {
                            chosen = Some(path.clone());
                        }
                    }
                });
        });
        if let Some(path) = chosen {
            self.open_recent(ui.ctx(), path);
        }
    }
}

enum ActionIcon {
    Play,
    Record,
    Stop,
    Settings,
}

fn action_button(
    ui: &mut egui::Ui,
    enabled: bool,
    icon: ActionIcon,
    label: &str,
    fill: Color32,
    foreground: Color32,
    width: f32,
) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| {
        let icon_id = egui::IdSalt::new("action-icon");
        let atom = egui::Atom::custom(icon_id, egui::vec2(16.0, 16.0));
        let button = if matches!(icon, ActionIcon::Settings) {
            egui::Button::new(atom)
        } else {
            egui::Button::new((atom, RichText::new(label).color(foreground)))
        }
        .gap(7.0)
        .min_size(egui::vec2(width, 36.0))
        .corner_radius(6)
        .fill(fill)
        .atom_ui(ui);
        if let Some(rect) = button.rect(icon_id) {
            let center = rect.center();
            let painter = ui.painter();
            match icon {
                ActionIcon::Play => {
                    painter.add(egui::Shape::convex_polygon(
                        vec![
                            center + egui::vec2(-4.0, -5.0),
                            center + egui::vec2(5.0, 0.0),
                            center + egui::vec2(-4.0, 5.0),
                        ],
                        foreground,
                        egui::Stroke::new(1.25, foreground),
                    ));
                }
                ActionIcon::Record => {
                    painter.circle_filled(center, 5.5, foreground);
                }
                ActionIcon::Stop => {
                    painter.rect_filled(
                        egui::Rect::from_center_size(center, egui::vec2(10.0, 10.0)),
                        2,
                        foreground,
                    );
                }
                ActionIcon::Settings => {
                    let teeth = (0..32)
                        .map(|step| {
                            let angle = step as f32 * std::f32::consts::TAU / 32.0;
                            let radius = if matches!(step % 4, 1 | 2) { 7.0 } else { 5.4 };
                            center + egui::vec2(angle.cos(), angle.sin()) * radius
                        })
                        .collect();
                    let stroke = egui::Stroke::new(1.4, foreground);
                    painter.add(egui::Shape::closed_line(teeth, stroke));
                    painter.circle_stroke(center, 2.3, stroke);
                }
            }
        }
        if matches!(icon, ActionIcon::Settings) {
            button.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::Button, enabled, "Настройки")
            });
        }
        button.response
    })
    .inner
}

impl MainWindow {
    fn input_active(&self) -> bool {
        self.job.as_ref().is_some_and(|job| job.stop.is_some())
    }

    fn playback_indicator(&self) -> Option<overlay::Labels> {
        if let Status::Playing(pass) = self.status {
            if self.hotkey_settings.show_playback_indicator {
                let (title, detail) = self.playback_labels(pass);
                let completed = pass.number.saturating_sub(1);
                let repeats = if self.playback.infinite {
                    format!("{completed}/∞")
                } else {
                    format!("{completed}/{}", self.playback.count)
                };
                return Some(overlay::Labels {
                    title,
                    detail,
                    repeats,
                });
            }
        }
        None
    }

    fn playback_labels(&self, pass: PlaybackPass) -> (String, Option<String>) {
        if self.playback.infinite {
            return (format!("Запуск {}", pass.number), Some("без лимита".into()));
        }
        let label = format!("Запуск {} из {}", pass.number, self.playback.count);
        let remaining = self.loaded.as_ref().map(|loaded| {
            let seconds = self.playback.remaining_seconds(
                loaded.duration,
                pass.number,
                pass.started_at.elapsed(),
            );
            let clock = format!("{:02}:{:02}", seconds / 60 % 60, seconds % 60);
            let hours = seconds / 3600;
            if hours > 0 {
                format!("осталось ≈ {hours}:{clock}")
            } else {
                format!("осталось ≈ {clock}")
            }
        });
        (label, remaining)
    }

    fn recording_active(&self) -> bool {
        self.job.as_ref().is_some_and(|job| job.recording)
    }

    fn status_text(&self) -> String {
        match &self.status {
            Status::Ready if self.loaded.is_none() => String::new(),
            Status::Ready
                if self
                    .loaded
                    .as_ref()
                    .is_some_and(|loaded| loaded.scenario.events.is_empty()) =>
            {
                "В сценарии нет событий".into()
            }
            Status::Ready => "Готово к запуску".into(),
            Status::Opening => "Открытие сценария…".into(),
            Status::Preparing if self.recording_active() => "Подготовка записи…".into(),
            Status::Preparing => "Проверка сценария…".into(),
            Status::Countdown(seconds) if self.recording_active() => {
                format!("Запись через {seconds} с")
            }
            Status::Countdown(seconds) => format!("Запуск через {seconds} с"),
            Status::SelectingWindow(seconds) => {
                format!("Переключись в целевое окно. Выбор через {seconds} с")
            }
            Status::WindowSelected => "Целевое окно выбрано".into(),
            Status::WindowSelectionCancelled => "Выбор целевого окна отменён".into(),
            Status::Playing(pass) => {
                let (label, detail) = self.playback_labels(*pass);
                match detail {
                    Some(detail) => format!("{label} · {detail}"),
                    None => label,
                }
            }
            Status::Recording => "Идёт запись".into(),
            Status::RecordingReady => "Запись завершена".into(),
            Status::RecordingCancelled => "Запись отменена до запуска".into(),
            Status::Unsaved if self.original.is_some() => "Сохрани копию или отбрось правки".into(),
            Status::Unsaved => "Сохрани или отбрось запись".into(),
            Status::Saving => "Сохранение сценария…".into(),
            Status::Saved => "Сценарий сохранён".into(),
            Status::Stopping => "Остановка…".into(),
            Status::Finished(PlaybackOutcome::Completed) => "Воспроизведение завершено".into(),
            Status::Finished(PlaybackOutcome::Stopped) => "Воспроизведение остановлено".into(),
            Status::Error(error) => error.clone(),
        }
    }

    fn progress(&self, context: &egui::Context) -> Option<f32> {
        match self.status {
            Status::Countdown(seconds) | Status::SelectingWindow(seconds) => {
                Some(f32::from(3 - seconds) / 3.0)
            }
            Status::Playing(pass) => {
                let duration = self.loaded.as_ref()?.duration;
                if duration.is_zero() {
                    return None;
                }
                context.request_repaint_after(Duration::from_millis(80));
                let fraction = (pass.started_at.elapsed().as_secs_f64() * self.playback.speed
                    / duration.as_secs_f64())
                .min(0.99);
                if self.playback.infinite {
                    Some(fraction as f32)
                } else {
                    Some(
                        ((pass.number.saturating_sub(1) as f64 + fraction)
                            / f64::from(self.playback.count)) as f32,
                    )
                }
            }
            _ => None,
        }
    }
}

fn playback_color(dark: bool) -> Color32 {
    if dark {
        Color32::from_rgb(108, 213, 140)
    } else {
        Color32::from_rgb(16, 112, 51)
    }
}

fn card_colors(dark: bool) -> (Color32, Color32) {
    if dark {
        (Color32::from_gray(43), Color32::from_gray(69))
    } else {
        (Color32::from_gray(252), Color32::from_gray(215))
    }
}

fn event_count(count: usize) -> String {
    let noun = match (count % 100, count % 10) {
        (11..=14, _) => "событий",
        (_, 1) => "событие",
        (_, 2..=4) => "события",
        _ => "событий",
    };
    format!("{count} {noun}")
}

fn configure_style(context: &egui::Context) {
    context.set_theme(egui::ThemePreference::System);
    for (theme, color) in [(Theme::Light, 243), (Theme::Dark, 32)] {
        let mut style = (*context.style_of(theme)).clone();
        style.visuals.panel_fill = Color32::from_gray(color);
        style.spacing.item_spacing = egui::vec2(10.0, 4.0);
        context.set_style_of(theme, style);
    }

    if let Some(windows_dir) = std::env::var_os("WINDIR") {
        if let Ok(font) = fs::read(PathBuf::from(windows_dir).join("Fonts/SegUIVar.ttf")) {
            let mut fonts = FontDefinitions::default();
            fonts.font_data.insert(
                "Segoe UI Variable".into(),
                Arc::new(FontData::from_owned(font)),
            );
            fonts
                .families
                .entry(FontFamily::Proportional)
                .or_default()
                .insert(0, "Segoe UI Variable".into());
            context.set_fonts(fonts);
        }
    }
}

fn load_selected(owner: isize, last_directory: Option<&Path>) -> io::Result<Option<Loaded>> {
    let executable = std::env::current_exe()?;
    let directory = RecentScenarios::initial_directory(last_directory, &executable);
    let Some(path) = open_dialog::select(owner, directory.as_deref()).map_err(io::Error::other)?
    else {
        return Ok(None);
    };
    load_path(path).map(Some)
}

fn load_path(path: PathBuf) -> io::Result<Loaded> {
    let scenario = read_json(BufReader::new(File::open(&path)?))?;
    validate_playback(&scenario)?;
    Ok(Loaded::new(path, Arc::new(scenario)))
}

fn save_to_path(
    path: &Path,
    scenario: &Scenario,
    confirm: impl FnOnce(&Path) -> bool,
) -> io::Result<bool> {
    match write_json_new(path, scenario) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if !confirm(path) {
                return Ok(false);
            }
            write_json_replace(path, scenario)?;
            Ok(true)
        }
        Err(error) => Err(error),
    }
}

fn confirm_replace(owner: isize, path: &Path) -> bool {
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    let text: Vec<u16> = format!("Файл «{name}» уже существует. Перезаписать сценарий?\0")
        .encode_utf16()
        .collect();
    let title: Vec<u16> = "Подтверждение перезаписи — Keyo\0".encode_utf16().collect();
    // SAFETY: обе строки завершаются нулём и остаются доступны до закрытия диалога.
    unsafe {
        MessageBoxW(
            owner as _,
            text.as_ptr(),
            title.as_ptr(),
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
        ) == IDYES
    }
}

fn select_save_json(owner: isize) -> io::Result<Option<PathBuf>> {
    let mut path = vec![0u16; 32_768];
    let filter: Vec<u16> = "Сценарии JSON (*.json)\0*.json\0\0"
        .encode_utf16()
        .collect();
    let title: Vec<u16> = "Сохранить сценарий Keyo\0".encode_utf16().collect();
    let extension: Vec<u16> = "json\0".encode_utf16().collect();
    let mut dialog = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner as _,
        lpstrFilter: filter.as_ptr(),
        lpstrFile: path.as_mut_ptr(),
        nMaxFile: path.len() as u32,
        lpstrTitle: title.as_ptr(),
        lpstrDefExt: extension.as_ptr(),
        Flags: OFN_EXPLORER | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    // SAFETY: все указатели диалога остаются действительными до завершения вызова.
    let selected = unsafe { GetSaveFileNameW(&mut dialog) };
    if selected == 0 {
        // SAFETY: запрашиваем ошибку только что закрытого диалога в том же потоке.
        let error = unsafe { CommDlgExtendedError() };
        return if error == 0 {
            Ok(None)
        } else {
            Err(io::Error::other(format!(
                "Ошибка диалога Windows 0x{error:x}"
            )))
        };
    }
    let length = path
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(path.len());
    Ok(Some(PathBuf::from(OsString::from_wide(&path[..length]))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indicator_counts_completed_repeats_instead_of_the_current_run() {
        let mut window = MainWindow {
            playback: PlaybackSettings {
                count: 1000,
                ..Default::default()
            },
            ..Default::default()
        };
        window.hotkey_settings.show_playback_indicator = true;
        for (number, expected) in [(1, "0/1000"), (123, "122/1000")] {
            window.status = Status::Playing(PlaybackPass {
                number,
                started_at: Instant::now(),
            });
            assert_eq!(window.playback_indicator().unwrap().repeats, expected);
        }
        window.playback.infinite = true;
        assert_eq!(window.playback_indicator().unwrap().repeats, "122/∞");
    }

    #[test]
    fn playback_indicator_is_optional_and_only_visible_during_playback() {
        let playing = || {
            Status::Playing(PlaybackPass {
                number: 2,
                started_at: Instant::now(),
            })
        };
        let mut window = MainWindow {
            status: playing(),
            playback: PlaybackSettings {
                count: 3,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(window.playback_indicator().is_none());
        window.hotkey_settings.show_playback_indicator = true;
        let labels = window.playback_indicator().unwrap();
        assert_eq!(labels.title, "Запуск 2 из 3");
        assert_eq!(labels.detail, None);
        window.playback.infinite = true;
        let labels = window.playback_indicator().unwrap();
        assert_eq!(labels.title, "Запуск 2");
        assert_eq!(labels.detail.as_deref(), Some("без лимита"));
        for status in [
            Status::Countdown(1),
            Status::Recording,
            Status::Stopping,
            Status::Finished(PlaybackOutcome::Completed),
            Status::Error("Ошибка".into()),
        ] {
            window.status = status;
            assert!(window.playback_indicator().is_none());
        }
    }

    #[test]
    fn cancelling_window_selection_clears_the_job_and_ignores_late_results() {
        let (sender, messages) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let mut window = MainWindow {
            job: Some(Job {
                messages,
                stop: Some(Arc::clone(&stop)),
                recording: false,
            }),
            status: Status::SelectingWindow(1),
            ..Default::default()
        };
        window.stop();
        assert!(stop.load(Ordering::Relaxed));
        sender
            .send(Message::TargetSelected(Err(io::Error::other(
                "Запоздалый результат",
            ))))
            .unwrap();
        window.poll_messages();
        assert!(window.job.is_none());
        assert!(window.playback_target.is_none());
        assert!(matches!(window.status, Status::WindowSelectionCancelled));
    }

    #[test]
    fn remaining_time_accounts_for_speed_current_run_and_future_runs() {
        let settings = PlaybackSettings {
            speed: 2.0,
            count: 5,
            infinite: false,
        };
        let duration = Duration::from_secs(50);
        assert_eq!(
            settings.remaining_seconds(duration, 2, Duration::from_secs(16)),
            84
        );
        assert_eq!(
            settings.remaining_seconds(duration, 2, Duration::from_secs(26)),
            75
        );
        assert_eq!(
            settings.remaining_seconds(duration, 5, Duration::from_millis(24_900)),
            1
        );
        assert_eq!(
            settings.remaining_seconds(Duration::ZERO, 1, Duration::ZERO),
            0
        );
    }

    #[test]
    fn recent_list_displays_ten_rows_and_opens_only_on_double_click() {
        let context = egui::Context::default();
        let mut window = MainWindow::default();
        let missing_dir = std::env::temp_dir().join(format!("keyo-missing-{}", std::process::id()));
        for index in (1..=10).rev() {
            window
                .recent
                .remember(&missing_dir.join(format!("recent-{index}.json")))
                .unwrap();
        }
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, WINDOW_SIZE.into());
        let frame = |window: &mut MainWindow, events| {
            let mut output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(rect),
                    events,
                    ..Default::default()
                },
                |ui| window.draw(ui),
            );
            output.textures_delta.clear();
            output
        };
        let output = frame(&mut window, vec![]);
        let mut visible = output.shapes.iter().filter_map(|clipped| {
            let egui::epaint::Shape::Text(text) = &clipped.shape else {
                return None;
            };
            let bounds = text.visual_bounding_rect();
            (text.galley.text().starts_with("recent-")
                && rect.contains_rect(bounds)
                && clipped.clip_rect.contains_rect(bounds))
            .then_some(bounds.center())
        });
        let row = visible.next().unwrap();
        assert_eq!(visible.count(), 9);
        assert!(window.loaded.is_none() && window.job.is_none());
        frame(&mut window, vec![egui::Event::PointerMoved(row)]);
        for click in 0..2 {
            for pressed in [true, false] {
                frame(
                    &mut window,
                    vec![egui::Event::PointerButton {
                        pos: row,
                        pressed,
                        button: egui::PointerButton::Primary,
                        modifiers: egui::Modifiers::NONE,
                    }],
                );
            }
            assert_eq!(window.job.is_some(), click == 1);
        }
        let result = window
            .job
            .as_ref()
            .unwrap()
            .messages
            .recv_timeout(Duration::from_secs(3))
            .unwrap();
        assert!(
            matches!(result, Message::Opened(Err(error)) if error.kind() == io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn replacing_a_saved_scenario_requires_confirmation() {
        let path = std::env::temp_dir().join(format!("keyo-confirm-{}.json", std::process::id()));
        let original = Scenario {
            coordinates: Default::default(),
            events: vec![],
        };
        let replacement = read_json(include_bytes!("../../scenario.json").as_slice()).unwrap();
        assert!(save_to_path(&path, &original, |_| unreachable!()).unwrap());
        let declined = save_to_path(&path, &replacement, |chosen| {
            assert_eq!(chosen, path);
            false
        })
        .unwrap();
        let remaining = read_json(fs::read(&path).unwrap().as_slice()).unwrap();
        let accepted = save_to_path(&path, &replacement, |_| true).unwrap();
        let saved = read_json(fs::read(&path).unwrap().as_slice()).unwrap();
        fs::remove_file(path).unwrap();

        assert!(!declined);
        assert_eq!(remaining, original);
        assert!(accepted);
        assert_eq!(saved, replacement);
    }

    #[test]
    fn failed_save_during_close_keeps_editor_open() {
        let (sender, messages) = mpsc::channel();
        let mut editor = Editor::new(Scenario {
            coordinates: Default::default(),
            events: vec![],
        });
        editor.changed = true;
        let mut window = MainWindow {
            editor: Some(editor),
            job: Some(Job {
                messages,
                stop: None,
                recording: false,
            }),
            closing: true,
            ..Default::default()
        };
        sender
            .send(Message::Saved(Err(io::Error::other(
                "Ошибка тестовой записи",
            ))))
            .unwrap();
        window.poll_messages();
        assert!(window.editor.as_ref().unwrap().changed);
        assert!(!window.closing);
        assert!(matches!(window.status, Status::Error(_)));
    }

    #[test]
    fn applying_edits_changes_playback_data_without_mutating_the_original() {
        let original =
            Arc::new(read_json(include_bytes!("../../scenario.json").as_slice()).unwrap());
        let mut editor = Editor::new((*original).clone());
        editor.scenario.events[0].delay_micros = 1_000_000;
        editor.changed = true;
        let mut window = MainWindow {
            loaded: Some(Loaded::new("scenario.json".into(), Arc::clone(&original))),
            editor: Some(editor),
            ..Default::default()
        };
        window.finish_editing(EditorAction::Apply);
        assert!(matches!(window.status, Status::Unsaved));
        assert_eq!(
            window.loaded.as_ref().unwrap().duration,
            Duration::from_millis(1050)
        );
        assert!(Arc::ptr_eq(
            &window.original.as_ref().unwrap().scenario,
            &original
        ));
        assert_eq!(original.events[0].delay_micros, 5_000_000);
    }

    #[test]
    fn editor_keeps_controls_visible_with_a_long_scenario() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, [680.0, 360.0].into());
        let context = egui::Context::default();
        configure_style(&context);
        let sample = read_json(include_bytes!("../../scenario.json").as_slice()).unwrap();
        let mut editor = Editor::new(Scenario {
            coordinates: Default::default(),
            events: (0..50)
                .flat_map(|_| sample.events.iter().cloned())
                .collect(),
        });
        let mut output = context.run_ui(
            egui::RawInput {
                screen_rect: Some(rect),
                ..Default::default()
            },
            |ui| {
                editor.draw(ui);
            },
        );
        output.textures_delta.clear();
        for label in ["Готово", "Удалить событие", "Добавить позицию"]
        {
            assert!(
                output.shapes.iter().any(|clipped| {
                    let egui::epaint::Shape::Text(text) = &clipped.shape else {
                        return false;
                    };
                    text.galley.text() == label
                        && rect.contains_rect(text.visual_bounding_rect())
                        && clipped.clip_rect.contains_rect(text.visual_bounding_rect())
                }),
                "Обрезана кнопка «{label}»"
            );
        }
    }

    #[test]
    fn close_only_waits_for_unsaved_work_or_running_input() {
        let context = egui::Context::default();
        let mut window = MainWindow::default();
        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            window.handle_close_request(ui.ctx());
        });
        output.textures_delta.clear();
        assert!(!output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::CancelClose));

        window.pending_recording = Some(Arc::new(Scenario {
            coordinates: Default::default(),
            events: vec![],
        }));
        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            window.handle_close_request(ui.ctx());
        });
        output.textures_delta.clear();
        assert!(window.close_prompt);
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::CancelClose));

        let mut output = context.run_ui(egui::RawInput::default(), |ui| {
            window.resolve_close(CloseDecision::Discard, ui.ctx());
        });
        output.textures_delta.clear();
        assert!(!window.has_unsaved_changes());
        assert!(output.viewport_output[&egui::ViewportId::ROOT]
            .commands
            .contains(&egui::ViewportCommand::Close));
    }

    #[test]
    fn unlimited_mode_keeps_the_count_for_the_next_finite_run() {
        let mut settings = PlaybackSettings {
            speed: 1.5,
            count: 3,
            infinite: true,
        };
        let options = settings.options();
        assert_eq!(options.repeats, RepeatMode::Infinite);
        assert_eq!(options.speed, 1.5);
        settings.infinite = false;
        assert_eq!(settings.options().repeats, RepeatMode::Count(3));
    }

    #[test]
    fn recording_survives_cancelled_save_and_becomes_ready_after_retry() {
        let scenario =
            Arc::new(read_json(include_bytes!("../../scenario.json").as_slice()).unwrap());
        let mut window = MainWindow {
            pending_recording: Some(Arc::clone(&scenario)),
            ..Default::default()
        };
        let (sender, messages) = mpsc::channel();
        window.job = Some(Job {
            messages,
            stop: None,
            recording: false,
        });
        sender.send(Message::Saved(Ok(None))).unwrap();
        window.poll_messages();
        assert!(window.pending_recording.is_some());
        assert!(window.job.is_none());
        assert!(matches!(window.status, Status::Unsaved));

        let (sender, messages) = mpsc::channel();
        window.job = Some(Job {
            messages,
            stop: None,
            recording: false,
        });
        sender
            .send(Message::Saved(Ok(Some(Loaded::new(
                "recorded.json".into(),
                scenario,
            )))))
            .unwrap();
        window.poll_messages();
        assert!(window.pending_recording.is_none());
        assert!(matches!(window.status, Status::Saved));
        let loaded = window.loaded.as_ref().unwrap();
        assert_eq!(loaded.name, "recorded.json");
        assert_eq!(loaded.duration, Duration::from_millis(5050));
    }

    #[test]
    fn playback_controls_fit_inside_the_window() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, WINDOW_SIZE.into());
        for (theme, running, label, fill, scale) in [
            (Theme::Light, false, "Воспроизвести", [16, 112, 51], 1.0),
            (Theme::Light, true, "Стоп", [164, 38, 44], 1.25),
            (Theme::Dark, false, "Воспроизвести", [108, 213, 140], 1.25),
            (Theme::Dark, true, "Стоп", [255, 179, 180], 1.5),
        ] {
            let context = egui::Context::default();
            configure_style(&context);
            context.set_theme(theme);
            let scenario = read_json(include_bytes!("../../scenario.json").as_slice()).unwrap();
            let (_sender, messages) = mpsc::channel();
            let mut window = MainWindow {
                loaded: Some(Loaded {
                    name: "scenario.json".into(),
                    path: "scenario.json".into(),
                    duration: Duration::from_millis(5_050),
                    scenario: Arc::new(scenario),
                }),
                job: running.then(|| Job {
                    messages,
                    stop: Some(Arc::new(AtomicBool::new(false))),
                    recording: false,
                }),
                status: if running {
                    Status::Countdown(2)
                } else {
                    Status::Ready
                },
                ..Default::default()
            };
            let mut input = egui::RawInput {
                screen_rect: Some(rect),
                ..Default::default()
            };
            input
                .viewports
                .get_mut(&egui::ViewportId::ROOT)
                .unwrap()
                .native_pixels_per_point = Some(scale);
            let mut output = context.run_ui(input, |ui| window.draw(ui));
            output.textures_delta.clear();
            let text_bounds = |label: &str| {
                output
                    .shapes
                    .iter()
                    .find_map(|clipped| {
                        let egui::epaint::Shape::Text(text) = &clipped.shape else {
                            return None;
                        };
                        (text.galley.text() == label).then(|| text.visual_bounding_rect())
                    })
                    .unwrap()
            };
            let playback = text_bounds(label);
            let record = text_bounds("Записать");
            assert!(playback.right() < record.left());
            assert!((playback.center().y - record.center().y).abs() < 2.0);
            assert!(output.shapes.iter().any(|clipped| {
                matches!(&clipped.shape, egui::epaint::Shape::Path(gear)
                    if gear.points.len() == 32 && gear.visual_bounding_rect().left() > record.right()
                    && (gear.visual_bounding_rect().center().y - record.center().y).abs() < 3.0)
            }));
            assert!(output.shapes.iter().any(|clipped| {
                let bounds = match &clipped.shape {
                    egui::epaint::Shape::Path(shape) if !running && shape.points.len() == 3 => {
                        shape.visual_bounding_rect()
                    }
                    egui::epaint::Shape::Rect(shape)
                        if running && shape.rect.size() == egui::vec2(10.0, 10.0) =>
                    {
                        shape.rect
                    }
                    _ => return false,
                };
                bounds.right() < playback.left()
                    && (bounds.center().y - playback.center().y).abs() < 3.0
            }));
            assert!(output.shapes.iter().any(|clipped| {
                matches!(&clipped.shape, egui::epaint::Shape::Circle(circle)
                    if circle.radius == 5.5 && circle.center.x < record.left()
                    && (circle.center.y - record.center().y).abs() < 3.0)
            }));
            assert!(
                output.shapes.iter().any(|clipped| {
                    let egui::epaint::Shape::Text(text) = &clipped.shape else {
                        return false;
                    };
                    text.galley.text() == label
                        && rect.shrink(18.0).contains_rect(text.visual_bounding_rect())
                        && clipped.clip_rect.contains_rect(text.visual_bounding_rect())
                }),
                "Кнопка «{label}» обрезана или отсутствует"
            );
            assert!(
                output.shapes.iter().any(|clipped| {
                    let egui::epaint::Shape::Rect(button) = &clipped.shape else {
                        return false;
                    };
                    button.fill == Color32::from_rgb(fill[0], fill[1], fill[2])
                        && button.rect.width() >= 140.0
                        && button.rect.height() >= 36.0
                        && rect.contains_rect(button.rect)
                        && clipped.clip_rect.contains_rect(button.rect)
                }),
                "Границы кнопки «{label}» не помещаются целиком"
            );
        }
    }

    #[test]
    fn empty_scenario_label_is_centered_on_the_open_button() {
        for theme in [Theme::Light, Theme::Dark] {
            let context = egui::Context::default();
            configure_style(&context);
            context.set_theme(theme);
            let mut window = MainWindow::default();
            let mut output = context.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        WINDOW_SIZE.into(),
                    )),
                    ..Default::default()
                },
                |ui| window.draw(ui),
            );
            output.textures_delta.clear();
            let y = |label: &str| {
                output
                    .shapes
                    .iter()
                    .find_map(|clipped| {
                        let egui::epaint::Shape::Text(text) = &clipped.shape else {
                            return None;
                        };
                        (text.galley.text() == label)
                            .then(|| text.visual_bounding_rect().center().y)
                    })
                    .unwrap()
            };
            assert!((y("Сценарий не выбран") - y("Открыть JSON")).abs() < 2.0);
        }
    }

    #[test]
    fn stop_keeps_the_job_active_until_worker_confirms_cleanup() {
        let (sender, messages) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let mut window = MainWindow {
            job: Some(Job {
                messages,
                stop: Some(Arc::clone(&stop)),
                recording: false,
            }),
            ..Default::default()
        };
        sender
            .send(Message::Progress(Status::Countdown(2)))
            .unwrap();
        window.poll_messages();
        assert!(matches!(window.status, Status::Countdown(2)));

        window.stop();
        assert!(stop.load(Ordering::Relaxed));
        sender
            .send(Message::Progress(Status::Playing(PlaybackPass {
                number: 1,
                started_at: std::time::Instant::now(),
            })))
            .unwrap();
        window.poll_messages();
        assert!(matches!(window.status, Status::Stopping));
        assert!(window.input_active());

        sender
            .send(Message::Finished(Ok(PlaybackOutcome::Stopped)))
            .unwrap();
        window.poll_messages();
        assert!(matches!(
            window.status,
            Status::Finished(PlaybackOutcome::Stopped)
        ));
        assert!(!window.input_active());
    }

    #[test]
    fn opening_a_scenario_updates_metadata_and_releases_loading_state() {
        let (sender, messages) = mpsc::channel();
        let mut window = MainWindow {
            job: Some(Job {
                messages,
                stop: None,
                recording: false,
            }),
            status: Status::Opening,
            ..Default::default()
        };
        let scenario = read_json(include_bytes!("../../scenario.json").as_slice()).unwrap();
        sender
            .send(Message::Opened(Ok(Some(Loaded {
                name: "scenario.json".into(),
                path: "scenario.json".into(),
                duration: Duration::from_millis(5_050),
                scenario: Arc::new(scenario),
            }))))
            .unwrap();
        window.poll_messages();
        assert!(window.job.is_none());
        assert!(matches!(window.status, Status::Ready));
        let loaded = window.loaded.as_ref().unwrap();
        assert_eq!(loaded.name, "scenario.json");
        assert_eq!(loaded.scenario.events.len(), 2);
    }
}
