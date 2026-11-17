use std::{cell::RefCell, io, ptr::null_mut};

use eframe::egui::{self, Color32};
use windows_sys::core::w;
use windows_sys::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW,
    EndPaint, FillRect, InvalidateRect, SelectObject, SetBkMode, SetTextColor, SetWindowRgn,
    UpdateWindow, ANTIALIASED_QUALITY, DEFAULT_CHARSET, DRAW_TEXT_FORMAT, DT_CALCRECT,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, FW_NORMAL,
    FW_SEMIBOLD, HDC, PAINTSTRUCT, TRANSPARENT,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW,
    RegisterClassW, SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, CREATESTRUCTW,
    GWLP_USERDATA, HWND_TOPMOST, LWA_ALPHA, SWP_NOACTIVATE, SWP_SHOWWINDOW, WM_NCCREATE, WM_PAINT,
    WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT, WS_POPUP,
};

#[derive(PartialEq)]
pub(super) struct Labels {
    pub(super) title: String,
    pub(super) detail: Option<String>,
    pub(super) repeats: String,
}

#[derive(PartialEq)]
struct Card {
    scale: f32,
    dark: bool,
    labels: Labels,
    progress: Option<f32>,
}

pub(super) struct Overlay {
    window: HWND,
    card: Box<RefCell<Card>>,
    bounds: (i32, i32, i32, i32),
}

impl Overlay {
    pub(super) fn show(
        active: &mut Option<Self>,
        scale: f32,
        dark: bool,
        labels: Labels,
        progress: Option<f32>,
    ) -> io::Result<()> {
        let area = super::placement::foreground_work_area()
            .ok_or_else(|| io::Error::other("Не удалось определить рабочую область монитора"))?;
        let size = (egui::vec2(300.0, 80.0) * scale).min(area.size());
        let margin = 16.0 * scale;
        let origin = egui::pos2(area.right() - size.x - margin, area.top() + margin)
            .clamp(area.min, area.max - size);
        let bounds = (
            origin.x.round() as i32,
            origin.y.round() as i32,
            size.x.round() as i32,
            size.y.round() as i32,
        );
        let card = Card {
            scale,
            dark,
            labels,
            progress,
        };

        let overlay = match active {
            Some(overlay) => {
                if *overlay.card.borrow() == card && overlay.bounds == bounds {
                    return Ok(());
                }
                *overlay.card.borrow_mut() = card;
                overlay
            }
            None => active.insert(Self::new(card)?),
        };
        if overlay.bounds != bounds {
            let (x, y, width, height) = bounds;
            // SAFETY: окно принадлежит Overlay. При успехе Windows забирает регион.
            unsafe {
                let diameter = (16.0 * scale).round().max(1.0) as i32;
                let region = CreateRoundRectRgn(0, 0, width + 1, height + 1, diameter, diameter);
                if region.is_null() {
                    return Err(window_error("CreateRoundRectRgn"));
                }
                if SetWindowRgn(overlay.window, region, 0) == 0 {
                    let error = window_error("SetWindowRgn");
                    DeleteObject(region);
                    return Err(error);
                }
                if SetWindowPos(
                    overlay.window,
                    HWND_TOPMOST,
                    x,
                    y,
                    width,
                    height,
                    SWP_NOACTIVATE | SWP_SHOWWINDOW,
                ) == 0
                {
                    return Err(window_error("SetWindowPos"));
                }
            }
            overlay.bounds = bounds;
        }
        // SAFETY: окно принадлежит UI-потоку. Принудительно рисуем слой сразу после показа.
        if unsafe { InvalidateRect(overlay.window, std::ptr::null(), 0) } == 0 {
            return Err(window_error("InvalidateRect"));
        }
        if unsafe { UpdateWindow(overlay.window) } == 0 {
            return Err(window_error("UpdateWindow"));
        }
        Ok(())
    }

    fn new(card: Card) -> io::Result<Self> {
        let card = Box::new(RefCell::new(card));
        let name = w!("KeyoPlaybackIndicator");
        // SAFETY: класс и процедура живут до завершения процесса. Box сохраняет адрес после перемещения.
        let window = unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            let class = WNDCLASSW {
                lpfnWndProc: Some(window_proc),
                hInstance: instance,
                lpszClassName: name,
                ..Default::default()
            };
            if RegisterClassW(&class) == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(ERROR_CLASS_ALREADY_EXISTS as i32) {
                    return Err(error);
                }
            }
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
                name,
                w!("Воспроизведение — Keyo"),
                WS_POPUP,
                0,
                0,
                0,
                0,
                null_mut(),
                null_mut(),
                instance,
                (&*card as *const RefCell<Card>).cast(),
            )
        };
        if window.is_null() {
            return Err(window_error("CreateWindowExW"));
        }
        let overlay = Self {
            window,
            card,
            bounds: (0, 0, 0, 0),
        };
        // SAFETY: окно создано в текущем потоке с WS_EX_LAYERED.
        if unsafe { SetLayeredWindowAttributes(window, 0, 255, LWA_ALPHA) } == 0 {
            return Err(window_error("SetLayeredWindowAttributes"));
        }
        Ok(overlay)
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        // SAFETY: окно уничтожается в UI-потоке до освобождения Box<Card>.
        unsafe { DestroyWindow(self.window) };
    }
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: сообщения и буферы предоставляет Win32. Указатель Card жив до DestroyWindow.
    unsafe {
        if message == WM_NCCREATE {
            let creation = &*(lparam as *const CREATESTRUCTW);
            SetWindowLongPtrW(window, GWLP_USERDATA, creation.lpCreateParams as isize);
        } else if message == WM_PAINT {
            let card = GetWindowLongPtrW(window, GWLP_USERDATA) as *const RefCell<Card>;
            if let Some(card) = card.as_ref() {
                let mut paint = PAINTSTRUCT::default();
                let dc = BeginPaint(window, &mut paint);
                if !dc.is_null() {
                    let mut client = RECT::default();
                    if GetClientRect(window, &mut client) != 0 {
                        draw_card(dc, client, &card.borrow());
                    }
                    EndPaint(window, &paint);
                }
                return 0;
            }
        }
        DefWindowProcW(window, message, wparam, lparam)
    }
}

fn draw_card(dc: HDC, client: RECT, card: &Card) {
    let px = |value: f32| (value * card.scale).round() as i32;
    let (surface, track) = super::card_colors(card.dark);
    let title = Color32::from_gray(if card.dark { 245 } else { 30 });
    let secondary = Color32::from_gray(if card.dark { 188 } else { 89 });
    let left = px(16.0);
    let right = client.right - left;
    fill(dc, client, surface);
    if right <= left {
        return;
    }
    // SAFETY: dc получен из BeginPaint и используется только в этом проходе отрисовки.
    unsafe { SetBkMode(dc, TRANSPARENT as i32) };
    let accent = super::playback_color(card.dark);
    let heading = RECT {
        left,
        top: px(8.0),
        right,
        bottom: px(33.0),
    };
    let counter_width = draw_label(
        dc,
        &card.labels.repeats,
        heading,
        px(16.0),
        FW_SEMIBOLD as i32,
        accent,
        DT_RIGHT,
    );
    draw_label(
        dc,
        &card.labels.title,
        RECT {
            right: (right - counter_width - px(12.0)).max(left),
            ..heading
        },
        px(16.0),
        FW_SEMIBOLD as i32,
        title,
        DT_LEFT,
    );
    if let Some(detail) = &card.labels.detail {
        draw_label(
            dc,
            detail,
            RECT {
                left,
                top: px(33.0),
                right,
                bottom: px(54.0),
            },
            px(14.0),
            FW_NORMAL as i32,
            secondary,
            DT_LEFT,
        );
    }
    if let Some(progress) = card.progress {
        let bar = RECT {
            left,
            top: client.bottom - px(17.0),
            right,
            bottom: client.bottom - px(12.0),
        };
        fill(dc, bar, track);
        let width = ((right - left) as f32 * progress.clamp(0.0, 1.0)).round() as i32;
        if width > 0 {
            fill(
                dc,
                RECT {
                    right: left + width,
                    ..bar
                },
                accent,
            );
        }
    }
}

fn colorref(color: Color32) -> u32 {
    u32::from_le_bytes([color.r(), color.g(), color.b(), 0])
}

fn fill(dc: HDC, rect: RECT, color: Color32) {
    // SAFETY: dc жив во время WM_PAINT. Кисть удаляется после FillRect.
    unsafe {
        let brush = CreateSolidBrush(colorref(color));
        if !brush.is_null() {
            FillRect(dc, &rect, brush);
            DeleteObject(brush);
        }
    }
}

fn draw_label(
    dc: HDC,
    text: &str,
    mut rect: RECT,
    size: i32,
    weight: i32,
    color: Color32,
    align: DRAW_TEXT_FORMAT,
) -> i32 {
    let wide: Vec<_> = text.encode_utf16().collect();
    // SAFETY: буферы доступны на время DrawTextW. Перед удалением шрифта восстанавливаем прежний.
    unsafe {
        let font = CreateFontW(
            -size,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET as u32,
            0,
            0,
            ANTIALIASED_QUALITY as u32,
            0,
            w!("Segoe UI"),
        );
        let previous = if font.is_null() {
            null_mut()
        } else {
            SelectObject(dc, font)
        };
        let mut measured = RECT::default();
        if align == DT_RIGHT {
            DrawTextW(
                dc,
                wide.as_ptr(),
                wide.len() as i32,
                &mut measured,
                DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
            );
        }
        SetTextColor(dc, colorref(color));
        DrawTextW(
            dc,
            wide.as_ptr(),
            wide.len() as i32,
            &mut rect,
            align | DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        if !previous.is_null() {
            SelectObject(dc, previous);
        }
        if !font.is_null() {
            DeleteObject(font);
        }
        measured.right
    }
}

fn window_error(call: &str) -> io::Error {
    io::Error::other(format!(
        "{call} завершился ошибкой ({})",
        io::Error::last_os_error()
    ))
}
