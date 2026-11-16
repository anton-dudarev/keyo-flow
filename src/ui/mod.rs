use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, BufReader};
use std::mem::size_of;
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, FontData, FontDefinitions, FontFamily, RichText, Theme};
use windows_sys::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError, GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR,
    OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetActiveWindow;

use crate::core::player::PlaybackOutcome;
use crate::core::scenario::Scenario;
use crate::platform::windows::{play_with_updates, validate_playback, PlaybackUpdate};
use crate::storage::read_json;

const WINDOW_SIZE: [f32; 2] = [620.0, 255.0];

pub fn run() -> eframe::Result {
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
        Box::new(|context| {
            configure_style(&context.egui_ctx);
            Ok(Box::<MainWindow>::default())
        }),
    )
}

struct Loaded {
    name: String,
    duration: Duration,
    scenario: Arc<Scenario>,
}

#[derive(Default)]
enum Status {
    #[default]
    Ready,
    Opening,
    Preparing,
    Countdown(u8),
    Playing(Instant),
    Stopping,
    Finished(PlaybackOutcome),
    Error(String),
}

enum Message {
    Opened(io::Result<Option<Loaded>>),
    Progress(PlaybackUpdate),
    Finished(io::Result<PlaybackOutcome>),
}

struct Job {
    messages: Receiver<Message>,
    stop: Option<Arc<AtomicBool>>,
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
}

impl MainWindow {
    fn open(&mut self, context: &egui::Context) {
        // SAFETY: получаем HWND активного окна в потоке интерфейса.
        let owner = unsafe { GetActiveWindow() } as isize;
        self.start_job(context, None, Status::Opening, move |send| {
            send(Message::Opened(load_selected(owner)));
        });
    }

    fn play(&mut self, context: &egui::Context) {
        let Some(loaded) = &self.loaded else { return };
        let scenario = Arc::clone(&loaded.scenario);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        self.start_job(context, Some(stop), Status::Preparing, move |send| {
            let result = play_with_updates((*scenario).clone(), &worker_stop, |update| {
                send(Message::Progress(update));
            });
            send(Message::Finished(result));
        });
    }

    fn start_job(
        &mut self,
        context: &egui::Context,
        stop: Option<Arc<AtomicBool>>,
        status: Status,
        work: impl FnOnce(&dyn Fn(Message)) + Send + 'static,
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
                self.job = Some(Job { messages, stop });
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
                Ok(Message::Progress(update)) => {
                    if !matches!(self.status, Status::Stopping) {
                        self.status = match update {
                            PlaybackUpdate::Countdown(seconds) => Status::Countdown(seconds),
                            PlaybackUpdate::Playing => Status::Playing(Instant::now()),
                        };
                    }
                }
                Ok(Message::Opened(result)) => {
                    self.job = None;
                    self.status = match result {
                        Ok(loaded) => {
                            if let Some(loaded) = loaded {
                                self.loaded = Some(loaded);
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
                Err(TryRecvError::Disconnected) => {
                    self.job = None;
                    self.status = Status::Error("Фоновая задача завершилась без результата".into());
                }
                Err(TryRecvError::Empty) => break,
            }
        }
    }
}

impl eframe::App for MainWindow {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_messages();
        if ctx.input(|input| input.viewport().close_requested()) && self.playback_active() {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.closing = true;
            self.stop();
        }
        if self.closing && self.job.is_none() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if ctx.input(|input| input.key_pressed(egui::Key::Escape)) {
            self.stop();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.draw(ui);
    }

    fn clear_color(&self, visuals: &egui::Visuals) -> [f32; 4] {
        visuals.panel_fill.to_normalized_gamma_f32()
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
        let surface = if dark {
            Color32::from_gray(43)
        } else {
            Color32::from_gray(252)
        };
        let border = if dark {
            Color32::from_gray(69)
        } else {
            Color32::from_gray(215)
        };
        let weak = if dark {
            Color32::from_gray(188)
        } else {
            Color32::from_gray(89)
        };
        egui::Frame::new().inner_margin(25).show(ui, |ui| {
            ui.label(RichText::new("Сценарий").size(29.0).strong());
            ui.add_space(15.0);
            egui::Frame::new()
                .fill(surface)
                .stroke(egui::Stroke::new(1.0, border))
                .corner_radius(8)
                .inner_margin(16)
                .show(ui, |ui| {
                    ui.allocate_ui_with_layout(
                        egui::vec2(ui.available_width(), 40.0),
                        egui::Layout::right_to_left(Align::Center),
                        |ui| {
                            if ui
                                .add_enabled(
                                    self.job.is_none(),
                                    egui::Button::new("Открыть JSON")
                                        .min_size(egui::vec2(120.0, 36.0)),
                                )
                                .clicked()
                            {
                                self.open(&context);
                            }
                            ui.with_layout(egui::Layout::top_down(Align::Min), |ui| {
                                if let Some(loaded) = &self.loaded {
                                    ui.add(
                                        egui::Label::new(RichText::new(&loaded.name).strong())
                                            .truncate(),
                                    )
                                    .on_hover_text(&loaded.name);
                                    let seconds = format!("{:.2}", loaded.duration.as_secs_f64())
                                        .replace('.', ",");
                                    ui.label(
                                        RichText::new(format!(
                                            "{} · {seconds} с",
                                            event_count(loaded.scenario.events.len()),
                                        ))
                                        .color(weak),
                                    );
                                } else {
                                    ui.label(RichText::new("Сценарий не выбран").strong());
                                }
                            });
                        },
                    );
                });
            ui.add_space(16.0);
            ui.allocate_ui_with_layout(
                egui::vec2(ui.available_width(), 44.0),
                egui::Layout::right_to_left(Align::Center),
                |ui| {
                    let active = self.playback_active();
                    let (label, fill, foreground) = if active {
                        let fill = if dark {
                            Color32::from_rgb(255, 179, 180)
                        } else {
                            Color32::from_rgb(164, 38, 44)
                        };
                        (
                            "Стоп",
                            fill,
                            if dark { Color32::BLACK } else { Color32::WHITE },
                        )
                    } else {
                        (
                            "Воспроизвести",
                            accent,
                            if dark { Color32::BLACK } else { Color32::WHITE },
                        )
                    };
                    let has_events = self
                        .loaded
                        .as_ref()
                        .is_some_and(|loaded| !loaded.scenario.events.is_empty());
                    let enabled = (active && !matches!(self.status, Status::Stopping))
                        || (self.job.is_none() && has_events);
                    if ui
                        .add_enabled(
                            enabled,
                            egui::Button::new(RichText::new(label).color(foreground))
                                .min_size(egui::vec2(140.0, 36.0))
                                .corner_radius(6)
                                .fill(fill),
                        )
                        .clicked()
                    {
                        if active {
                            self.stop();
                        } else {
                            self.play(&context);
                        }
                    }
                    ui.with_layout(egui::Layout::top_down(Align::Min), |ui| {
                        let text = self.status_text();
                        if matches!(self.status, Status::Error(_)) {
                            ui.label(RichText::new(text).color(ui.visuals().error_fg_color));
                        } else {
                            ui.label(RichText::new(text).strong());
                        }
                        if active {
                            let hint = if matches!(self.status, Status::Countdown(_)) {
                                "Переключись в тестовое окно. Esc — остановить"
                            } else {
                                "Esc — остановить"
                            };
                            ui.label(RichText::new(hint).color(weak));
                        } else if has_events && !matches!(self.status, Status::Error(_)) {
                            ui.label(
                                RichText::new("После запуска переключись в тестовое окно.")
                                    .color(weak),
                            );
                        }
                    });
                },
            );
            if let Some(fraction) = self.progress(&context) {
                ui.add_space(12.0);
                ui.add(
                    egui::ProgressBar::new(fraction)
                        .desired_height(4.0)
                        .fill(accent),
                );
            }
        });
    }
}

impl MainWindow {
    fn playback_active(&self) -> bool {
        self.job.as_ref().is_some_and(|job| job.stop.is_some())
    }

    fn status_text(&self) -> String {
        match &self.status {
            Status::Ready if self.loaded.is_none() => "Открой JSON для воспроизведения".into(),
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
            Status::Preparing => "Проверка сценария…".into(),
            Status::Countdown(seconds) => format!("Запуск через {seconds} с"),
            Status::Playing(_) => "Идёт воспроизведение".into(),
            Status::Stopping => "Остановка…".into(),
            Status::Finished(PlaybackOutcome::Completed) => "Воспроизведение завершено".into(),
            Status::Finished(PlaybackOutcome::Stopped) => "Воспроизведение остановлено".into(),
            Status::Error(error) => error.clone(),
        }
    }

    fn progress(&self, context: &egui::Context) -> Option<f32> {
        match self.status {
            Status::Countdown(seconds) => Some(f32::from(3 - seconds) / 3.0),
            Status::Playing(start) => {
                let duration = self.loaded.as_ref()?.duration;
                if duration.is_zero() {
                    return None;
                }
                context.request_repaint_after(Duration::from_millis(80));
                Some((start.elapsed().as_secs_f32() / duration.as_secs_f32()).min(0.99))
            }
            _ => None,
        }
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

fn load_selected(owner: isize) -> io::Result<Option<Loaded>> {
    let Some(path) = select_json(owner)? else {
        return Ok(None);
    };
    let scenario = read_json(BufReader::new(File::open(&path)?))?;
    validate_playback(&scenario)?;
    let duration = scenario
        .events
        .iter()
        .map(|event| Duration::from_micros(event.delay_micros))
        .sum();
    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned();
    Ok(Some(Loaded {
        name,
        duration,
        scenario: Arc::new(scenario),
    }))
}

fn select_json(owner: isize) -> io::Result<Option<PathBuf>> {
    let mut path = vec![0u16; 32_768];
    let filter: Vec<u16> = "Сценарии JSON (*.json)\0*.json\0\0"
        .encode_utf16()
        .collect();
    let title: Vec<u16> = "Открыть сценарий Keyo\0".encode_utf16().collect();
    let mut dialog = OPENFILENAMEW {
        lStructSize: size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner as _,
        lpstrFilter: filter.as_ptr(),
        lpstrFile: path.as_mut_ptr(),
        nMaxFile: path.len() as u32,
        lpstrTitle: title.as_ptr(),
        Flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };
    // SAFETY: все указатели диалога остаются действительными до завершения вызова.
    if unsafe { GetOpenFileNameW(&mut dialog) } == 0 {
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
    fn playback_controls_fit_inside_the_window() {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, WINDOW_SIZE.into());
        for (theme, running, label, fill, scale) in [
            (Theme::Light, false, "Воспроизвести", [0, 95, 184], 1.0),
            (Theme::Light, true, "Стоп", [164, 38, 44], 1.25),
            (Theme::Dark, false, "Воспроизвести", [96, 205, 255], 1.25),
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
                    duration: Duration::from_millis(5_050),
                    scenario: Arc::new(scenario),
                }),
                job: running.then(|| Job {
                    messages,
                    stop: Some(Arc::new(AtomicBool::new(false))),
                }),
                status: if running {
                    Status::Countdown(2)
                } else {
                    Status::Ready
                },
                closing: false,
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
    fn stop_keeps_the_job_active_until_worker_confirms_cleanup() {
        let (sender, messages) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let mut window = MainWindow {
            job: Some(Job {
                messages,
                stop: Some(Arc::clone(&stop)),
            }),
            ..Default::default()
        };
        sender
            .send(Message::Progress(PlaybackUpdate::Countdown(2)))
            .unwrap();
        window.poll_messages();
        assert!(matches!(window.status, Status::Countdown(2)));

        window.stop();
        assert!(stop.load(Ordering::Relaxed));
        sender
            .send(Message::Progress(PlaybackUpdate::Playing))
            .unwrap();
        window.poll_messages();
        assert!(matches!(window.status, Status::Stopping));
        assert!(window.playback_active());

        sender
            .send(Message::Finished(Ok(PlaybackOutcome::Stopped)))
            .unwrap();
        window.poll_messages();
        assert!(matches!(
            window.status,
            Status::Finished(PlaybackOutcome::Stopped)
        ));
        assert!(!window.playback_active());
    }

    #[test]
    fn opening_a_scenario_updates_metadata_and_releases_loading_state() {
        let (sender, messages) = mpsc::channel();
        let mut window = MainWindow {
            job: Some(Job {
                messages,
                stop: None,
            }),
            status: Status::Opening,
            ..Default::default()
        };
        let scenario = read_json(include_bytes!("../../scenario.json").as_slice()).unwrap();
        sender
            .send(Message::Opened(Ok(Some(Loaded {
                name: "scenario.json".into(),
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
