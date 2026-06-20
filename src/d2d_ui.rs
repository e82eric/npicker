#![allow(unsafe_op_in_unsafe_fn, unused_unsafe)]

use std::ffi::c_void;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D_RECT_F, D2D_SIZE_U, D2D1_ALPHA_MODE_UNKNOWN, D2D1_COLOR_F, D2D1_PIXEL_FORMAT,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1_DRAW_TEXT_OPTIONS_CLIP, D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_ROUNDED_RECT, D2D1CreateFactory, ID2D1Factory,
    ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::DirectWrite::{
    DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_NORMAL, DWRITE_MEASURING_MODE_NATURAL, DWRITE_TEXT_ALIGNMENT_TRAILING,
    DWRITE_TEXT_METRICS, DWRITE_WORD_WRAPPING_NO_WRAP, DWriteCreateFactory, IDWriteFactory,
    IDWriteTextFormat,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetMonitorInfoW, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromWindow, PAINTSTRUCT,
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
use windows_numerics::Vector2;

use crate::search::SearchResult;
use crate::view_model::{KeyModifiers, UiCounters, UiEvent, ViewModel};

const DEFAULT_WIDTH: i32 = 1600;
const MIN_WIDTH: i32 = 480;
const MIN_HEIGHT: i32 = 360;
const MONITOR_HORIZONTAL_MARGIN: i32 = 80;
const MONITOR_VERTICAL_MARGIN: i32 = 160;
const ROW_HEIGHT: f32 = 24.0;
const COUNTER_WIDTH: f32 = 300.0;
const WM_UI_UPDATE: u32 = WM_APP + 1;
const WM_UI_BRING_TO_FOREGROUND: u32 = WM_APP + 2;
const WM_CURSOR_BLINK: u32 = WM_APP + 3;
const WM_SHOW_ROOT: u32 = WM_APP + 4;
const DEFAULT_HEIGHT: i32 = 48 + 15 * ROW_HEIGHT as i32 + 10 + (8 * 4);
const COLOR_BACKGROUND: u32 = 0x282828;
const COLOR_TEXT: u32 = 0xa89984;
const COLOR_HIGHLIGHT: u32 = 0xffa500;
const COLOR_SELECTED: u32 = 0x504945;

pub fn run(view_model: Arc<ViewModel>) -> Result<()> {
    unsafe {
        let class_name = wide_null("NfmRustD2DPicker");
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
            PCWSTR(wide_null("nfm rust picker").as_ptr()),
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
    factory: ID2D1Factory,
    dwrite_factory: IDWriteFactory,
    target: Option<ID2D1HwndRenderTarget>,
    text_format: IDWriteTextFormat,
    counter_text_format: IDWriteTextFormat,
    text_brush: Option<ID2D1SolidColorBrush>,
    muted_brush: Option<ID2D1SolidColorBrush>,
    highlight_brush: Option<ID2D1SolidColorBrush>,
    selected_brush: Option<ID2D1SolidColorBrush>,
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

impl WindowState {
    fn new(
        view_model: Arc<ViewModel>,
        shared: Arc<Mutex<SharedUiState>>,
        left: f32,
        top: f32,
        width: f32,
        height: f32,
    ) -> Result<Self> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
                .context("D2D1CreateFactory failed")?;
            let dwrite_factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)
                .context("DWriteCreateFactory failed")?;
            let text_format = dwrite_factory.CreateTextFormat(
                PCWSTR(wide_null("Cascadia Mono").as_ptr()),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                15.0,
                PCWSTR(wide_null("en-us").as_ptr()),
            )?;
            text_format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            let counter_text_format = dwrite_factory.CreateTextFormat(
                PCWSTR(wide_null("Cascadia Mono").as_ptr()),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                15.0,
                PCWSTR(wide_null("en-us").as_ptr()),
            )?;
            counter_text_format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            counter_text_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_TRAILING)?;

            let sample = wide_null("Hg");
            let layout = dwrite_factory.CreateTextLayout(&sample, &text_format, 1000.0, 1000.0)?;

            let mut metrics = DWRITE_TEXT_METRICS::default();
            let _ = layout.GetMetrics(&mut metrics)?;

            let text_height = metrics.height;

            let window = Rect {
                x: left,
                y: top,
                width,
                height,
            };

            let app_layout = calculate_layout(window, 8.0, 15, text_height);

            Ok(Self {
                view_model,
                shared,
                factory,
                dwrite_factory,
                target: None,
                text_format,
                counter_text_format,
                text_brush: None,
                muted_brush: None,
                highlight_brush: None,
                selected_brush: None,
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
            if let Some(target) = self.target.as_ref() {
                let _ = target.Resize(&D2D_SIZE_U {
                    width: location.width.max(1) as u32,
                    height: location.height.max(1) as u32,
                });
            }
            self.last_window_location = Some(location);
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

    unsafe fn ensure_target(&mut self, hwnd: HWND) -> Result<()> {
        if self.target.is_some() {
            return Ok(());
        }

        let mut rect = RECT::default();
        GetClientRect(hwnd, &mut rect)?;
        let width = (rect.right - rect.left).max(1) as u32;
        let height = (rect.bottom - rect.top).max(1) as u32;

        let render_props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_UNKNOWN,
                alphaMode: D2D1_ALPHA_MODE_UNKNOWN,
            },
            dpiX: 0.0,
            dpiY: 0.0,
            usage: Default::default(),
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd,
            pixelSize: D2D_SIZE_U { width, height },
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        let target = self
            .factory
            .CreateHwndRenderTarget(&render_props, &hwnd_props)?;

        self.text_brush = Some(target.CreateSolidColorBrush(&rgb(COLOR_TEXT), None)?);
        self.muted_brush = Some(target.CreateSolidColorBrush(&rgb(COLOR_TEXT), None)?);
        self.highlight_brush = Some(target.CreateSolidColorBrush(&rgb(COLOR_HIGHLIGHT), None)?);
        self.selected_brush = Some(target.CreateSolidColorBrush(&rgb(COLOR_SELECTED), None)?);
        self.target = Some(target);
        Ok(())
    }

    unsafe fn paint(&mut self, hwnd: HWND) -> Result<()> {
        self.ensure_target(hwnd)?;
        let Some(target) = self.target.as_ref() else {
            return Ok(());
        };

        target.BeginDraw();
        target.Clear(Some(&rgb(COLOR_BACKGROUND)));

        let search_state = self.view_model.current_search_text();
        let search_string = search_state.text;

        let radius = 8.0;

        draw_rounded_rectangle(
            target,
            self.highlight_brush.as_ref().expect("highlight brush"),
            self.layout.search_border,
            radius,
        );

        if let Some(selection) = search_state.selection {
            let prefix = &search_string[..selection.start];
            let prefix_utf16: Vec<u16> = prefix.encode_utf16().collect();
            let selected_text = &search_string[selection.start..selection.end];
            let selected_text_utf16: Vec<u16> = selected_text.encode_utf16().collect();

            let prefix_layout = self
                .dwrite_factory
                .CreateTextLayout(
                    &prefix_utf16,
                    &self.text_format,
                    1.0,
                    self.layout.row_size.height,
                )
                .expect("dwrite_factory.layout");

            let selected_text_layout = self
                .dwrite_factory
                .CreateTextLayout(
                    &selected_text_utf16,
                    &self.text_format,
                    1.0,
                    self.layout.text_height,
                )
                .expect("dwrite_factory.layout");

            let mut prefix_metrics = DWRITE_TEXT_METRICS::default();
            let mut selected_text_metrics = DWRITE_TEXT_METRICS::default();

            let _ = prefix_layout
                .GetMetrics(&mut prefix_metrics)
                .expect("dwrite_factory.layout.GetMetrics");
            let _ = selected_text_layout
                .GetMetrics(&mut selected_text_metrics)
                .expect("dwrite_factory.layout.GetMetrics");

            let rect = D2D_RECT_F {
                left: self.layout.search_box.x + prefix_metrics.widthIncludingTrailingWhitespace,
                top: self.layout.search_box.y,
                right: self.layout.search_box.x
                    + prefix_metrics.widthIncludingTrailingWhitespace
                    + selected_text_metrics.widthIncludingTrailingWhitespace,
                bottom: self.layout.search_box.y + self.layout.search_box.height,
            };
            target.FillRectangle(&rect, self.selected_brush.as_ref().expect("brush created"));
        }

        draw_text(
            target,
            self.text_brush.as_ref().expect("brush created"),
            &self.text_format,
            &format!("{search_string}"),
            self.layout.search_box,
        );

        if self.cursor_visible {
            let search_up_to_cursor = &search_string[..search_state.cursor_position];
            let wide: Vec<u16> = search_up_to_cursor.encode_utf16().collect();
            let layout = self
                .dwrite_factory
                .CreateTextLayout(&wide, &self.text_format, 1.0, self.layout.text_height)
                .expect("dwrite_factory.layout");
            let mut metrics = DWRITE_TEXT_METRICS::default();
            let _ = layout
                .GetMetrics(&mut metrics)
                .expect("dwrite_factory.layout.GetMetrics");
            let cursor_prefix_width = metrics.widthIncludingTrailingWhitespace;

            target.DrawLine(
                Vector2 {
                    X: self.layout.search_box.x + cursor_prefix_width + 1.5,
                    Y: self.layout.search_box.y,
                },
                Vector2 {
                    X: self.layout.search_box.x + cursor_prefix_width + 1.5,
                    Y: self.layout.search_box.y + self.layout.search_box.height,
                },
                self.text_brush.as_ref().expect("brush created"),
                1.0,
                None,
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
            target,
            self.muted_brush.as_ref().expect("brush created"),
            &self.counter_text_format,
            &counter_text,
            counter_rect,
        );

        draw_rounded_rectangle(
            target,
            self.highlight_brush.as_ref().expect("highlight brush"),
            self.layout.list_border,
            radius,
        );

        for (index, result) in self.results.iter().enumerate() {
            let top = self.layout.list_box.y + index as f32 * self.layout.row_size.height;
            let text_top = top + ((self.layout.row_size.height - self.layout.text_height) / 2.0);
            if index == self.selected_row {
                let rect = D2D_RECT_F {
                    left: self.layout.list_box.x,
                    top,
                    right: self.layout.list_box.x + self.layout.row_size.width,
                    bottom: top + self.layout.row_size.height,
                };
                target.FillRectangle(&rect, self.selected_brush.as_ref().expect("brush created"));
            }

            let item_text_rect = Rect {
                x: self.layout.list_box.x,
                y: text_top,
                width: self.layout.search_box.x + self.layout.search_box.width,
                height: self.layout.text_height,
            };

            draw_text(
                target,
                self.text_brush.as_ref().expect("brush created"),
                &self.text_format,
                &result.path,
                item_text_rect,
            );

            draw_position_highlights(
                target,
                &self.dwrite_factory,
                self.highlight_brush.as_ref().expect("brush created"),
                &self.text_format,
                &result.path,
                &result.positions,
                self.layout.list_box.x,
                text_top,
                self.layout.list_box.width,
                ROW_HEIGHT,
            );
        }

        target.EndDraw(None, None)?;

        Ok(())
    }
}

fn is_key_down(vkey: i32) -> bool {
    unsafe { (GetKeyState(vkey) as u16 & 0x8000) != 0 }
}

fn current_modifiers() -> KeyModifiers {
    KeyModifiers {
        ctrl: is_key_down(VK_CONTROL.0 as i32),
        shift: is_key_down(VK_SHIFT.0 as i32),
        alt: is_key_down(VK_MENU.0 as i32), // VK_MENU is Alt
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
                let _ = (*state).paint(hwnd);
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
                    let _ = (*state).paint(hwnd);
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
                let _ = (*state).paint(hwnd);
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
    let hwnd = GetForegroundWindow();
    if !hwnd.is_invalid() {
        let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        if !monitor.is_invalid() {
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };

            if GetMonitorInfoW(monitor, &mut info).as_bool() {
                let work = info.rcWork;
                let mon_width = work.right - work.left;
                let mon_height = work.bottom - work.top;
                let width =
                    DEFAULT_WIDTH.min((mon_width - MONITOR_HORIZONTAL_MARGIN).max(MIN_WIDTH));
                let height =
                    DEFAULT_HEIGHT.min((mon_height - MONITOR_VERTICAL_MARGIN).max(MIN_HEIGHT));

                return ScreenLocation {
                    x: work.left + (mon_width - width) / 2,
                    y: work.top + (mon_height - height) / 2,
                    width,
                    height,
                };
            }
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
    target: &ID2D1HwndRenderTarget,
    dwrite_factory: &IDWriteFactory,
    brush: &ID2D1SolidColorBrush,
    format: &IDWriteTextFormat,
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

    let text_wide = wide(text);
    let Ok(layout) =
        (unsafe { dwrite_factory.CreateTextLayout(&text_wide, format, width, height) })
    else {
        return;
    };

    for &position in positions {
        if position >= text_wide.len() {
            continue;
        }

        let mut point_x = 0.0f32;
        let mut point_y = 0.0f32;
        let mut metrics = Default::default();
        if unsafe {
            layout
                .HitTestTextPosition(
                    position as u32,
                    false,
                    &mut point_x,
                    &mut point_y,
                    &mut metrics,
                )
                .is_err()
        } {
            continue;
        }

        if point_x >= width {
            break;
        }

        let rect = D2D_RECT_F {
            left: left + point_x,
            top,
            right: (left + point_x + metrics.width).min(left + width),
            bottom: top + height,
        };
        unsafe {
            target.DrawText(
                &text_wide[position..position + 1],
                format,
                &rect,
                brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }
}

fn draw_rounded_rectangle(
    target: &ID2D1HwndRenderTarget,
    brush: &ID2D1SolidColorBrush,
    rect: Rect,
    radius: f32,
) {
    let rounded_rect = D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left: rect.x,
            top: rect.y,
            right: rect.x + rect.width,
            bottom: rect.y + rect.height,
        },
        radiusX: radius,
        radiusY: radius,
    };

    unsafe {
        target.DrawRoundedRectangle(&rounded_rect, brush, 1.2, None);
    }
}

fn draw_text(
    target: &ID2D1HwndRenderTarget,
    brush: &ID2D1SolidColorBrush,
    format: &IDWriteTextFormat,
    text: &str,
    rect: Rect,
) {
    let text = wide(text);
    let d2d_rect = D2D_RECT_F {
        left: rect.x,
        top: rect.y,
        right: rect.x + rect.width,
        bottom: rect.y + rect.height,
    };

    unsafe {
        target.DrawText(
            &text,
            format,
            &d2d_rect,
            brush,
            D2D1_DRAW_TEXT_OPTIONS_CLIP,
            DWRITE_MEASURING_MODE_NATURAL,
        );
    }
}

fn rgb(value: u32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((value >> 16) & 0xff) as f32 / 255.0,
        g: ((value >> 8) & 0xff) as f32 / 255.0,
        b: (value & 0xff) as f32 / 255.0,
        a: 1.0,
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().collect()
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
