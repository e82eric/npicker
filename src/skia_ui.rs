#![allow(unsafe_op_in_unsafe_fn, unused_unsafe)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use skia_safe::{
    Canvas, Color, Font, FontMgr, FontStyle, Paint, PaintStyle, RRect, Rect as SkRect, Surface,
    surfaces,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BITMAPINFO, BITMAPINFOHEADER, BeginPaint, DIB_RGB_COLORS, EndPaint, GetMonitorInfoW,
    InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint, MonitorFromWindow,
    PAINTSTRUCT, SRCCOPY, StretchDIBits,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, INPUT, INPUT_MOUSE, KEYEVENTF_KEYUP, SendInput, SetFocus, VK_CONTROL, VK_MENU,
    VK_SHIFT, keybd_event,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CreateWindowExW, DefWindowProcW, DispatchMessageW, GWLP_USERDATA,
    GetCaretBlinkTime, GetClientRect, GetForegroundWindow, GetMessageW, GetWindowLongPtrW,
    IDC_ARROW, LoadCursorW, MSG, PostMessageW, RegisterClassW, SW_HIDE, SWP_NOACTIVATE,
    SWP_NOZORDER, SWP_SHOWWINDOW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, TranslateMessage, WM_APP, WM_CHAR, WM_DESTROY, WM_KEYDOWN, WM_NCCREATE, WM_PAINT,
    WM_TIMER, WNDCLASSW, WS_EX_TOPMOST, WS_POPUP,
};
use windows::core::PCWSTR;

use crate::search::SearchResult;
use crate::view_model::{KeyModifiers, UiCounters, UiEvent, ViewModel};

const DEFAULT_WIDTH: i32 = 1600;
const MIN_WIDTH: i32 = 480;
const MIN_HEIGHT: i32 = 360;
const MONITOR_HORIZONTAL_MARGIN: i32 = 80;
const MONITOR_VERTICAL_MARGIN: i32 = 160;
const ROW_HEIGHT: f32 = 24.0;
const DISPLAY_ROWS: i32 = 15;
const PADDING: f32 = 8.0;
const COUNTER_WIDTH: f32 = 300.0;
const WM_UI_UPDATE: u32 = WM_APP + 21;
const WM_UI_BRING_TO_FOREGROUND: u32 = WM_APP + 22;
const WM_CURSOR_BLINK: u32 = WM_APP + 23;
const WM_SHOW_ROOT: u32 = WM_APP + 24;
const DEFAULT_HEIGHT: i32 = 48 + 15 * ROW_HEIGHT as i32 + 10 + (8 * 4);
const COLOR_BACKGROUND: u32 = 0x282828;
const COLOR_TEXT: u32 = 0xa89984;
const COLOR_HIGHLIGHT: u32 = 0xffa500;
const COLOR_SELECTED: u32 = 0x504945;
const SKIA_BADGE_TEXT: &str = "SKIA";
const DEFAULT_LOCATION_VALUE: i32 = i32::MIN;

static PREFERRED_CENTER_X: AtomicI32 = AtomicI32::new(DEFAULT_LOCATION_VALUE);
static PREFERRED_CENTER_Y: AtomicI32 = AtomicI32::new(DEFAULT_LOCATION_VALUE);
static DESIRED_WINDOW_HEIGHT: AtomicI32 = AtomicI32::new(DEFAULT_HEIGHT);

pub fn set_preferred_center(x: i32, y: i32) {
    PREFERRED_CENTER_X.store(x, Ordering::Relaxed);
    PREFERRED_CENTER_Y.store(y, Ordering::Relaxed);
}

pub fn run(view_model: Arc<ViewModel>) -> Result<()> {
    unsafe {
        let class_name = wide_null("NfmRustSkiaPicker");
        let hinstance = GetModuleHandleW(None)?;
        let cursor = LoadCursorW(None, IDC_ARROW)?;
        let wnd_class = WNDCLASSW {
            hCursor: cursor,
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            lpfnWndProc: Some(wnd_proc),
            ..Default::default()
        };

        if RegisterClassW(&wnd_class) == 0 {
            return Err(anyhow!("RegisterClassW failed"));
        }

        let screen_location = calculate_window_location();
        let shared = Arc::new(Mutex::new(SharedUiState::default()));
        let state = Box::new(WindowState::new(
            Arc::clone(&view_model),
            Arc::clone(&shared),
            screen_location.x as f32,
            screen_location.y as f32,
            screen_location.width as f32,
            screen_location.height as f32,
        )?);
        let state_ptr = Box::into_raw(state);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(wide_null("nfm rust skia picker").as_ptr()),
            WS_POPUP,
            screen_location.x,
            screen_location.y,
            screen_location.width,
            screen_location.height,
            None,
            None,
            Some(hinstance.into()),
            Some(state_ptr as *const c_void),
        )
        .context("CreateWindowExW failed")?;

        let events = view_model.subscribe();
        let raw_hwnd = hwnd.0 as usize;
        std::thread::spawn(move || {
            while let Ok(event) = events.recv() {
                let hwnd = HWND(raw_hwnd as *mut c_void);
                {
                    let mut shared = shared.lock().expect("shared UI state poisoned");
                    match event {
                        UiEvent::Show => {
                            shared.visible = true;
                            unsafe {
                                let _ =
                                    PostMessageW(Some(hwnd), WM_SHOW_ROOT, WPARAM(0), LPARAM(0));
                            }
                        }
                        UiEvent::Results(update) => {
                            shared.results = update.results;
                            shared.counters = update.counters;
                            shared.selected_row = update.selected_row;
                            unsafe {
                                let _ =
                                    PostMessageW(Some(hwnd), WM_UI_UPDATE, WPARAM(0), LPARAM(0));
                            }
                        }
                        UiEvent::Close => {
                            shared.visible = false;
                            unsafe {
                                let _ =
                                    PostMessageW(Some(hwnd), WM_UI_UPDATE, WPARAM(0), LPARAM(0));
                            }
                        }
                    }
                }
            }
        });

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    Ok(())
}

struct WindowState {
    view_model: Arc<ViewModel>,
    shared: Arc<Mutex<SharedUiState>>,
    surface: Option<Surface>,
    font: Font,
    counter_font: Font,
    text_paint: Paint,
    muted_paint: Paint,
    highlight_paint: Paint,
    selected_paint: Paint,
    stroke_paint: Paint,
    results: Vec<SearchResult>,
    counters: UiCounters,
    selected_row: usize,
    visible: bool,
    last_window_location: Option<ScreenLocation>,
    queued_foreground_after_first_paint: bool,
    logged_first_items_paint: bool,
    cursor_visible: bool,
    layout: Layout,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Size {
    width: f32,
    height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[allow(dead_code)]
struct Layout {
    window: Rect,
    search_border: Rect,
    search_box: Rect,
    list_border: Rect,
    list_box: Rect,
    number_of_items: i32,
    row_size: Size,
    text_height: f32,
}

#[derive(Default)]
struct SharedUiState {
    visible: bool,
    results: Vec<SearchResult>,
    counters: UiCounters,
    selected_row: usize,
}

fn calculate_layout(window: Rect, padding: f32, number_of_items: i32, text_height: f32) -> Layout {
    let search_border = Rect {
        x: padding,
        y: padding,
        width: (window.width - (padding * 2.0)).max(0.0),
        height: text_height + (padding * 2.0),
    };

    let search_box = Rect {
        x: search_border.x + padding,
        y: search_border.y + padding,
        width: (search_border.width - (padding * 2.0)).max(0.0),
        height: search_border.height - (padding * 2.0),
    };

    let list_border_width = (window.width - (padding * 2.0)).max(0.0);
    let row_size = Size {
        height: text_height + padding,
        width: (list_border_width - (padding * 2.0)).max(0.0),
    };

    let list_border = Rect {
        x: padding,
        y: search_border.y + search_border.height + padding,
        width: (window.width - (padding * 2.0)).max(0.0),
        height: (number_of_items as f32 * row_size.height) + (padding * 2.0),
    };

    let list_box = Rect {
        x: list_border.x + padding,
        y: list_border.y + padding,
        width: (list_border.width - (padding * 2.0)).max(0.0),
        height: (list_border.height - (padding * 2.0)).max(0.0),
    };

    Layout {
        window,
        search_border,
        search_box,
        list_border,
        list_box,
        row_size,
        number_of_items,
        text_height,
    }
}

fn desired_window_height(text_height: f32, padding: f32, number_of_items: i32) -> i32 {
    let search_border_height = text_height + (padding * 2.0);
    let row_height = text_height + padding;
    let list_border_height = (number_of_items as f32 * row_height) + (padding * 2.0);
    (padding + search_border_height + padding + list_border_height + padding).ceil() as i32
}

impl WindowState {
    fn new(
        view_model: Arc<ViewModel>,
        shared: Arc<Mutex<SharedUiState>>,
        left: f32,
        top: f32,
        width: f32,
        _height: f32,
    ) -> Result<Self> {
        let typeface = FontMgr::default()
            .legacy_make_typeface("Cascadia Mono", FontStyle::normal())
            .or_else(|| FontMgr::default().legacy_make_typeface(None, FontStyle::normal()))
            .context("failed to create Skia typeface")?;
        let mut font = Font::new(typeface.clone(), 15.0);
        font.set_subpixel(true);
        let counter_font = Font::new(typeface, 15.0);
        let text_height = font.metrics().0;

        let desired_height = desired_window_height(text_height, PADDING, DISPLAY_ROWS);
        DESIRED_WINDOW_HEIGHT.store(desired_height, Ordering::Relaxed);

        let window = Rect {
            x: left,
            y: top,
            width,
            height: desired_height as f32,
        };

        let app_layout = calculate_layout(window, PADDING, DISPLAY_ROWS, text_height);

        Ok(Self {
            view_model,
            shared,
            surface: None,
            font,
            counter_font,
            text_paint: fill_paint(COLOR_TEXT),
            muted_paint: fill_paint(COLOR_TEXT),
            highlight_paint: fill_paint(COLOR_HIGHLIGHT),
            selected_paint: fill_paint(COLOR_SELECTED),
            stroke_paint: stroke_paint(COLOR_HIGHLIGHT, 1.2),
            results: Vec::new(),
            counters: UiCounters::default(),
            selected_row: 0,
            visible: false,
            last_window_location: None,
            queued_foreground_after_first_paint: false,
            logged_first_items_paint: false,
            cursor_visible: false,
            layout: app_layout,
        })
    }

    fn apply_pending(&mut self, hwnd: HWND) {
        {
            let pending = self.shared.lock().expect("shared UI state poisoned");
            self.visible = pending.visible;
            self.results = pending.results.clone();
            self.counters = pending.counters.clone();
            self.selected_row = pending.selected_row;
        }

        unsafe {
            if self.visible {
                // self.show_root(hwnd);
            } else {
                let _ = ShowWindow(hwnd, SW_HIDE);
                self.queued_foreground_after_first_paint = false;
                self.logged_first_items_paint = false;
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    unsafe fn show_root(&mut self, hwnd: HWND) {
        let location = calculate_window_location();
        if self.last_window_location != Some(location) {
            self.surface = None;
            self.last_window_location = Some(location);
            self.layout = calculate_layout(
                Rect {
                    x: location.x as f32,
                    y: location.y as f32,
                    width: location.width as f32,
                    height: location.height as f32,
                },
                PADDING,
                DISPLAY_ROWS,
                self.layout.text_height,
            );
        }

        let _ = SetWindowPos(
            hwnd,
            None,
            location.x,
            location.y,
            location.width,
            location.height,
            SWP_SHOWWINDOW | SWP_NOZORDER | SWP_NOACTIVATE,
        );

        bring_to_foreground(hwnd);
        let _ = SetFocus(Some(hwnd));

        let blink_ms = GetCaretBlinkTime();
        if SetTimer(Some(hwnd), WM_CURSOR_BLINK as usize, blink_ms, None) != 0 {}
    }

    unsafe fn ensure_surface(&mut self, hwnd: HWND) -> Result<()> {
        let mut rect = RECT::default();
        GetClientRect(hwnd, &mut rect)?;
        let width = (rect.right - rect.left).max(1);
        let height = (rect.bottom - rect.top).max(1);

        let needs_surface = self
            .surface
            .as_ref()
            .map(|surface| {
                let image_info = surface.image_info();
                image_info.width() != width || image_info.height() != height
            })
            .unwrap_or(true);

        if needs_surface {
            self.surface = surfaces::raster_n32_premul((width, height));
            if self.surface.is_none() {
                return Err(anyhow!("failed to create Skia raster surface"));
            }
            self.layout.window.width = width as f32;
            self.layout.window.height = height as f32;
        }

        Ok(())
    }

    unsafe fn paint(&mut self, hwnd: HWND, paint_struct: &PAINTSTRUCT) -> Result<()> {
        self.ensure_surface(hwnd)?;
        self.draw_to_surface()?;
        self.blit_surface(paint_struct);
        Ok(())
    }

    fn draw_to_surface(&mut self) -> Result<()> {
        let Some(surface) = self.surface.as_mut() else {
            return Ok(());
        };

        let canvas = surface.canvas();
        canvas.clear(skia_color(COLOR_BACKGROUND));

        let search_state = self.view_model.current_search_text();
        let search_string = search_state.text;

        let radius = 8.0;

        draw_rounded_rectangle(
            canvas,
            &self.stroke_paint,
            self.layout.search_border,
            radius,
        );

        if let Some(selection) = search_state.selection {
            let prefix = &search_string[..selection.start];
            let selected_text = &search_string[selection.start..selection.end];
            let prefix_width = measure_text_width(&self.font, &self.text_paint, prefix);
            let selected_width = measure_text_width(&self.font, &self.text_paint, selected_text);
            let rect = Rect {
                x: self.layout.search_box.x + prefix_width,
                y: self.layout.search_box.y,
                width: selected_width,
                height: self.layout.search_box.height,
            };
            draw_filled_rectangle(canvas, &self.selected_paint, rect);
        }

        draw_text(
            canvas,
            &self.font,
            &self.text_paint,
            &search_string,
            self.layout.search_box,
            TextAlign::Left,
        );

        if self.cursor_visible {
            let search_up_to_cursor = &search_string[..search_state.cursor_position];
            let cursor_prefix_width =
                measure_text_width(&self.font, &self.text_paint, search_up_to_cursor);

            canvas.draw_line(
                (
                    self.layout.search_box.x + cursor_prefix_width + 1.5,
                    self.layout.search_box.y,
                ),
                (
                    self.layout.search_box.x + cursor_prefix_width + 1.5,
                    self.layout.search_box.y + self.layout.search_box.height,
                ),
                &self.text_paint,
            );
        }

        let counter_text = format!("{}/{}", self.counters.matched, self.counters.published);
        let counter_rect = Rect {
            x: self.layout.search_box.x + self.layout.search_box.width - COUNTER_WIDTH,
            y: self.layout.search_box.y,
            width: COUNTER_WIDTH,
            height: self.layout.text_height,
        };
        draw_text(
            canvas,
            &self.counter_font,
            &self.muted_paint,
            &counter_text,
            counter_rect,
            TextAlign::Right,
        );

        let badge_rect = Rect {
            x: self.layout.search_box.x + self.layout.search_box.width - COUNTER_WIDTH - 56.0,
            y: self.layout.search_box.y,
            width: 48.0,
            height: self.layout.text_height,
        };
        draw_text(
            canvas,
            &self.counter_font,
            &self.highlight_paint,
            SKIA_BADGE_TEXT,
            badge_rect,
            TextAlign::Right,
        );

        draw_rounded_rectangle(canvas, &self.stroke_paint, self.layout.list_border, radius);

        for (index, result) in self.results.iter().enumerate() {
            let top = self.layout.list_box.y + index as f32 * self.layout.row_size.height;
            let text_top = top + ((self.layout.row_size.height - self.layout.text_height) / 2.0);
            if index == self.selected_row {
                let rect = Rect {
                    x: self.layout.list_box.x,
                    y: top,
                    width: self.layout.row_size.width,
                    height: self.layout.row_size.height,
                };
                draw_filled_rectangle(canvas, &self.selected_paint, rect);
            }

            let item_text_rect = Rect {
                x: self.layout.list_box.x,
                y: text_top,
                width: self.layout.search_box.x + self.layout.search_box.width,
                height: self.layout.text_height,
            };

            draw_text(
                canvas,
                &self.font,
                &self.text_paint,
                &result.path,
                item_text_rect,
                TextAlign::Left,
            );

            draw_position_highlights(
                canvas,
                &self.font,
                &self.highlight_paint,
                &result.path,
                &result.positions,
                self.layout.list_box.x,
                text_top,
                self.layout.list_box.width,
                self.layout.text_height,
            );
        }

        Ok(())
    }

    unsafe fn blit_surface(&mut self, paint_struct: &PAINTSTRUCT) {
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let Some(pixmap) = surface.peek_pixels() else {
            return;
        };

        let width = pixmap.width();
        let height = pixmap.height();
        if width <= 0 || height <= 0 {
            return;
        }

        let bitmap_info = bitmap_info(width, height);
        let _ = StretchDIBits(
            paint_struct.hdc,
            0,
            0,
            width,
            height,
            0,
            0,
            width,
            height,
            Some(pixmap.addr()),
            &bitmap_info,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
    }
}

fn is_key_down(vkey: i32) -> bool {
    unsafe { (GetKeyState(vkey) as u16 & 0x8000) != 0 }
}

fn current_modifiers() -> KeyModifiers {
    KeyModifiers {
        ctrl: is_key_down(VK_CONTROL.0 as i32),
        shift: is_key_down(VK_SHIFT.0 as i32),
        alt: is_key_down(VK_MENU.0 as i32),
    }
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_NCCREATE => {
            let create = lparam.0 as *const CREATESTRUCTW;
            let state = (*create).lpCreateParams as *mut WindowState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state as isize);
            LRESULT(1)
        }
        WM_PAINT => {
            let state = window_state(hwnd);
            if !state.is_null() {
                let mut paint = PAINTSTRUCT::default();
                BeginPaint(hwnd, &mut paint);
                let _ = (*state).paint(hwnd, &paint);
                let _ = EndPaint(hwnd, &paint);
            }
            LRESULT(0)
        }
        WM_UI_UPDATE => {
            let state = window_state(hwnd);
            if !state.is_null() {
                (*state).apply_pending(hwnd);
            }
            LRESULT(0)
        }
        WM_SHOW_ROOT => {
            let state = window_state(hwnd);
            if !state.is_null() {
                (*state).show_root(hwnd);
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == WM_CURSOR_BLINK as usize {
                let state = window_state(hwnd);
                if !state.is_null() {
                    let state = &mut *state;
                    state.cursor_visible = !state.cursor_visible;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
            LRESULT(0)
        }
        WM_UI_BRING_TO_FOREGROUND => {
            bring_to_foreground(hwnd);
            let _ = SetFocus(Some(hwnd));

            let blink_ms = GetCaretBlinkTime();
            if SetTimer(Some(hwnd), WM_CURSOR_BLINK as usize, blink_ms, None) != 0 {}

            LRESULT(0)
        }
        WM_CHAR => {
            let state = window_state(hwnd);
            if !state.is_null() {
                (*state).view_model.handle_char(wparam.0);
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let state = window_state(hwnd);
            if !state.is_null() {
                let modifiers = current_modifiers();
                (*state).view_model.handle_key(wparam.0, modifiers);
                let _ = InvalidateRect(Some(hwnd), None, false);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let state = window_state(hwnd);
            if !state.is_null() {
                let _ = Box::from_raw(state);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn window_state(hwnd: HWND) -> *mut WindowState {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScreenLocation {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

unsafe fn calculate_window_location() -> ScreenLocation {
    let preferred_x = PREFERRED_CENTER_X.load(Ordering::Relaxed);
    let preferred_y = PREFERRED_CENTER_Y.load(Ordering::Relaxed);
    let monitor = if preferred_x != DEFAULT_LOCATION_VALUE && preferred_y != DEFAULT_LOCATION_VALUE
    {
        MonitorFromPoint(
            POINT {
                x: preferred_x,
                y: preferred_y,
            },
            MONITOR_DEFAULTTONEAREST,
        )
    } else {
        MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTONEAREST)
    };

    if !monitor.is_invalid() {
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };

        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            let work = info.rcWork;
            let mon_width = work.right - work.left;
            let mon_height = work.bottom - work.top;
            let width = DEFAULT_WIDTH.min((mon_width - MONITOR_HORIZONTAL_MARGIN).max(MIN_WIDTH));
            let desired_height = DESIRED_WINDOW_HEIGHT.load(Ordering::Relaxed);
            let height = desired_height.min((mon_height - MONITOR_VERTICAL_MARGIN).max(MIN_HEIGHT));
            let center_x = if preferred_x != DEFAULT_LOCATION_VALUE {
                preferred_x
            } else {
                work.left + mon_width / 2
            };
            let center_y = if preferred_y != DEFAULT_LOCATION_VALUE {
                preferred_y
            } else {
                work.top + mon_height / 2
            };
            let x = (center_x - width / 2).clamp(work.left, work.right - width);
            let y = (center_y - height / 2).clamp(work.top, work.bottom - height);

            return ScreenLocation {
                x,
                y,
                width,
                height,
            };
        }
    }

    ScreenLocation {
        x: 100,
        y: 100,
        width: DEFAULT_WIDTH,
        height: DEFAULT_HEIGHT,
    }
}

unsafe fn bring_to_foreground(hwnd: HWND) {
    const VK_MENU: u8 = 0x12;
    keybd_event(VK_MENU, 0, Default::default(), 0);
    keybd_event(VK_MENU, 0, KEYEVENTF_KEYUP, 0);
    let _ = SetForegroundWindow(hwnd);

    let input = INPUT {
        r#type: INPUT_MOUSE,
        ..Default::default()
    };
    let _ = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
}

fn draw_position_highlights(
    canvas: &Canvas,
    font: &Font,
    paint: &Paint,
    text: &str,
    positions: &[usize],
    left: f32,
    top: f32,
    width: f32,
    height: f32,
) {
    if positions.is_empty() {
        return;
    }

    for &position in positions {
        if position >= text.len() || !text.is_char_boundary(position) {
            continue;
        }

        let Some(ch) = text[position..].chars().next() else {
            continue;
        };
        let prefix = &text[..position];
        let glyph = &text[position..position + ch.len_utf8()];
        let point_x = measure_text_width(font, paint, prefix);
        if point_x >= width {
            break;
        }

        let glyph_width = measure_text_width(font, paint, glyph).max(1.0);
        let rect = Rect {
            x: left + point_x,
            y: top,
            width: glyph_width.min(width - point_x),
            height,
        };
        draw_clipped_text_at(canvas, font, paint, text, left, top, height, rect);
    }
}

fn draw_rounded_rectangle(canvas: &Canvas, paint: &Paint, rect: Rect, radius: f32) {
    let sk_rect = sk_rect(rect);
    let rounded_rect = RRect::new_rect_xy(sk_rect, radius, radius);
    canvas.draw_rrect(rounded_rect, paint);
}

fn draw_filled_rectangle(canvas: &Canvas, paint: &Paint, rect: Rect) {
    canvas.draw_rect(sk_rect(rect), paint);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TextAlign {
    Left,
    Right,
}

fn draw_text(
    canvas: &Canvas,
    font: &Font,
    paint: &Paint,
    text: &str,
    rect: Rect,
    align: TextAlign,
) {
    let save_count = canvas.save();
    canvas.clip_rect(sk_rect(rect), None, true);
    let width = measure_text_width(font, paint, text);
    let x = match align {
        TextAlign::Left => rect.x,
        TextAlign::Right => rect.x + (rect.width - width).max(0.0),
    };
    let baseline = rect.y + text_baseline(font, rect.height);
    canvas.draw_str(text, (x, baseline), font, paint);
    canvas.restore_to_count(save_count);
}

fn draw_clipped_text_at(
    canvas: &Canvas,
    font: &Font,
    paint: &Paint,
    text: &str,
    x: f32,
    y: f32,
    height: f32,
    clip: Rect,
) {
    let save_count = canvas.save();
    canvas.clip_rect(sk_rect(clip), None, true);
    let baseline = y + text_baseline(font, height);
    canvas.draw_str(text, (x, baseline), font, paint);
    canvas.restore_to_count(save_count);
}

fn measure_text_width(font: &Font, paint: &Paint, text: &str) -> f32 {
    font.measure_str(text, Some(paint)).0
}

fn text_baseline(font: &Font, height: f32) -> f32 {
    let (_, metrics) = font.metrics();
    let text_height = metrics.descent - metrics.ascent;
    ((height - text_height) / 2.0) - metrics.ascent
}

fn fill_paint(value: u32) -> Paint {
    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    paint.set_style(PaintStyle::Fill);
    paint.set_color(skia_color(value));
    paint
}

fn stroke_paint(value: u32, width: f32) -> Paint {
    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    paint.set_style(PaintStyle::Stroke);
    paint.set_stroke_width(width);
    paint.set_color(skia_color(value));
    paint
}

fn sk_rect(rect: Rect) -> SkRect {
    SkRect::from_xywh(rect.x, rect.y, rect.width, rect.height)
}

fn skia_color(value: u32) -> Color {
    Color::from_argb(
        0xff,
        ((value >> 16) & 0xff) as u8,
        ((value >> 8) & 0xff) as u8,
        (value & 0xff) as u8,
    )
}

fn bitmap_info(width: i32, height: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: 0,
            biSizeImage: (width.max(0) as u32) * (height.max(0) as u32) * 4,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
