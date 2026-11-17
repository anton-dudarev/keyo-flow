use std::fs::{self, File};
use std::io::{self, BufReader};
use std::mem::size_of;
use std::time::{Duration, Instant};

use eframe::egui::{self, Pos2, Rect, Vec2, ViewportCommand, ViewportInfo};
use serde::{Deserialize, Serialize};
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};

use crate::storage::write_replacement;

use super::profile_file;

#[derive(Default, Deserialize, Serialize)]
#[serde(default)]
pub(super) struct WindowSettings {
    pub(super) main: WindowPlacement,
    pub(super) editor: WindowPlacement,
}

impl WindowSettings {
    pub(super) fn load() -> Self {
        profile_file("windows.json")
            .and_then(|path| serde_json::from_reader(BufReader::new(File::open(path).ok()?)).ok())
            .unwrap_or_default()
    }

    pub(super) fn save(&self) -> io::Result<()> {
        let Some(file) = profile_file("windows.json") else {
            return Ok(());
        };
        fs::create_dir_all(file.parent().unwrap())?;
        write_replacement(&file, |writer| {
            serde_json::to_writer(writer, self).map_err(io::Error::from)
        })
    }
}

#[derive(Default, Deserialize, Serialize)]
pub(super) struct WindowPlacement {
    geometry: Option<Geometry>,
    maximized: bool,
}

#[derive(Default)]
pub(super) enum RestorePlacement {
    #[default]
    Pending,
    Moving {
        target: Pos2,
        deadline: Instant,
    },
    Done,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
struct Geometry {
    position: [f32; 2],
    inner_size: [f32; 2],
    frame: [f32; 2],
}

impl Geometry {
    fn valid(self) -> bool {
        Pos2::from(self.position).is_finite()
            && Vec2::from(self.inner_size).is_finite()
            && self
                .inner_size
                .iter()
                .all(|size| (64.0..=50_000.0).contains(size))
            && self.frame.iter().all(|size| (0.0..=200.0).contains(size))
    }

    fn fit(self, area: Rect, scale: f32, resize: bool) -> (Pos2, Vec2) {
        let frame = Vec2::from(self.frame);
        let mut size = Vec2::from(self.inner_size);
        if resize {
            size = size.min((area.size() / scale - frame).max(Vec2::splat(64.0)));
        }
        let position = clamp_position(self.position.into(), (size + frame) * scale, area);
        (position, size)
    }
}

impl WindowPlacement {
    pub(super) fn update(
        &mut self,
        context: &egui::Context,
        restoration: &mut RestorePlacement,
        resize: bool,
    ) {
        let scale = context.pixels_per_point();
        match *restoration {
            RestorePlacement::Pending => {
                if let Some(saved) = self.geometry.filter(|geometry| geometry.valid()) {
                    let previous = Pos2::from(saved.position);
                    if let Some(area) = work_area(previous) {
                        let target = clamp_position(previous, egui::vec2(80.0, 40.0), area);
                        context.send_viewport_cmd(ViewportCommand::OuterPosition(target / scale));
                        *restoration = RestorePlacement::Moving {
                            target,
                            deadline: Instant::now() + Duration::from_millis(250),
                        };
                        context.request_repaint();
                        return;
                    }
                }
                *restoration = RestorePlacement::Done;
            }
            RestorePlacement::Moving { target, deadline } => {
                let moved = context.input(|input| {
                    input
                        .viewport()
                        .outer_rect
                        .is_some_and(|rect| (rect.min * scale).distance(target) < 16.0)
                });
                if !moved && Instant::now() < deadline {
                    context.request_repaint_after(Duration::from_millis(16));
                    return;
                }
                if let (Some(saved), Some(area)) = (self.geometry, work_area(target)) {
                    // Масштаб берём после переноса окна на монитор назначения.
                    let (position, size) = saved.fit(area, scale, resize);
                    if resize {
                        let minimum = Vec2::from(super::editor::EDITOR_MIN_SIZE);
                        if size.x < minimum.x || size.y < minimum.y {
                            context.send_viewport_cmd(ViewportCommand::MinInnerSize(
                                size.min(minimum),
                            ));
                        }
                        context.send_viewport_cmd(ViewportCommand::InnerSize(size));
                    }
                    context.send_viewport_cmd(ViewportCommand::OuterPosition(position / scale));
                    if self.maximized && resize {
                        context.send_viewport_cmd(ViewportCommand::Maximized(true));
                    }
                }
                *restoration = RestorePlacement::Done;
                context.request_repaint();
                return;
            }
            RestorePlacement::Done => {}
        }
        context.input(|input| self.observe(input.viewport(), scale));
    }

    fn observe(&mut self, viewport: &ViewportInfo, scale: f32) {
        if viewport.minimized == Some(true) {
            return;
        }
        self.maximized = viewport.maximized.unwrap_or(self.maximized);
        if self.maximized {
            return;
        }
        if let (Some(outer), Some(inner)) = (viewport.outer_rect, viewport.inner_rect) {
            let geometry = Geometry {
                position: (outer.min * scale).into(),
                inner_size: inner.size().into(),
                frame: (outer.size() - inner.size()).max(Vec2::ZERO).into(),
            };
            if geometry.valid() {
                self.geometry = Some(geometry);
            }
        }
    }
}

fn clamp_position(position: Pos2, size: Vec2, area: Rect) -> Pos2 {
    position.clamp(area.min, (area.max - size).max(area.min))
}

fn work_area(position: Pos2) -> Option<Rect> {
    let point = POINT {
        x: position.x.round() as i32,
        y: position.y.round() as i32,
    };
    let mut info = MONITORINFO {
        cbSize: size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    // SAFETY: передаём координаты в физических пикселях и валидный буфер MONITORINFO.
    let ok = unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        !monitor.is_null() && GetMonitorInfoW(monitor, &mut info) != 0
    };
    let area = info.rcWork;
    (ok && area.right > area.left && area.bottom > area.top).then(|| {
        Rect::from_min_max(
            egui::pos2(area.left as f32, area.top as f32),
            egui::pos2(area.right as f32, area.bottom as f32),
        )
    })
}

#[cfg(test)]
mod tests {
    use eframe::egui;

    use super::*;

    #[test]
    fn placement_on_a_removed_monitor_fits_in_the_current_work_area() {
        let original = Geometry {
            position: [-1900.0, 30.0],
            inner_size: [1100.0, 830.0],
            frame: [16.0, 40.0],
        };
        let area = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1400.0, 900.0));
        let (position, size) = original.fit(area, 1.5, true);
        assert_eq!(position, egui::Pos2::ZERO);
        assert_eq!(size, egui::vec2(1400.0 / 1.5 - 16.0, 560.0));
    }

    #[test]
    fn stores_physical_position_and_keeps_normal_size_when_maximized() {
        let mut settings = WindowSettings::default();
        let rect = |x, y, width, height| {
            egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(width, height))
        };
        settings.editor.observe(
            &ViewportInfo {
                outer_rect: Some(rect(-900.0, 100.0, 816.0, 600.0)),
                inner_rect: Some(rect(-892.0, 130.0, 800.0, 560.0)),
                ..Default::default()
            },
            1.5,
        );
        let normal = settings.editor.geometry.unwrap();
        assert_eq!(normal.position, [-1350.0, 150.0]);
        assert_eq!(normal.inner_size, [800.0, 560.0]);
        settings.editor.observe(
            &ViewportInfo {
                outer_rect: Some(rect(-1000.0, 0.0, 1000.0, 800.0)),
                maximized: Some(true),
                ..Default::default()
            },
            1.5,
        );
        settings.editor.observe(
            &ViewportInfo {
                minimized: Some(true),
                maximized: Some(false),
                ..Default::default()
            },
            1.5,
        );
        let restored: WindowSettings =
            serde_json::from_slice(&serde_json::to_vec(&settings).unwrap()).unwrap();
        assert!(restored.editor.maximized);
        assert_eq!(restored.editor.geometry, Some(normal));
        let area = rect(-1920.0, 0.0, 1920.0, 1100.0);
        assert_eq!(
            normal.fit(area, 1.5, false),
            (egui::pos2(-1350.0, 150.0), egui::vec2(800.0, 560.0))
        );
    }
}
