#![allow(unsafe_op_in_unsafe_fn, unused_unsafe)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::Receiver;
use skia_safe::{
    surfaces, Canvas, Color, Data, Font, FontMgr, FontStyle, Image, Paint, PaintStyle, RRect,
    Rect as SkRect, Surface,
};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateDIBSection, CreateSolidBrush, DeleteDC, DeleteObject,
    EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, MonitorFromWindow, SelectObject,
    UpdateWindow, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, GetKeyState, SendInput, SetFocus, INPUT, INPUT_MOUSE, KEYEVENTF_KEYUP, VK_BACK,
    VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_MENU, VK_NEXT,
    VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCaretBlinkTime, GetForegroundWindow,
    GetMessageW, GetWindowLongPtrW, KillTimer, LoadCursorW, PostMessageW, PostQuitMessage,
    RegisterClassW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, ShowWindow, TranslateMessage,
    UpdateLayeredWindow, CREATESTRUCTW, GWLP_USERDATA, IDC_ARROW, MSG, SW_HIDE, SW_SHOW,
    ULW_OPAQUE, WM_APP, WM_CHAR, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_NCCREATE,
    WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::key_binding::{KeyChord, KeyName};
use crate::preview::{NativeWindowId, PreviewLine, PreviewUpdate};
use crate::preview_document::PreviewStyle;
use crate::view_model::{
    DisplaySearchResult, KeyModifiers, PreviewView, UiCounters, UiEvent, ViewModel,
};

const DEFAULT_WIDTH: i32 = 1600;
const MIN_WIDTH: i32 = 480;
const MIN_HEIGHT: i32 = 360;
const MONITOR_HORIZONTAL_MARGIN: i32 = 80;
const MONITOR_VERTICAL_MARGIN: i32 = 160;
const DISPLAY_ROWS: i32 = 7;
const PREVIEW_ROWS: i32 = 20;
const PADDING: f32 = 8.0;
const PANEL_BORDER: f32 = 2.0;
const PANEL_HORIZONTAL_PADDING: f32 = 16.0;
const PANEL_VERTICAL_PADDING: f32 = 10.0;
const PANEL_GAP: f32 = 12.0;
const QUERY_VERTICAL_PADDING: f32 = 8.0;
const RESULT_HORIZONTAL_PADDING: f32 = 12.0;
const RESULT_VERTICAL_PADDING: f32 = 5.0;
const RESULT_GAP: f32 = 3.0;
const SELECTED_ACCENT_WIDTH: f32 = 3.0;
const COUNTER_WIDTH: f32 = 300.0;
const WM_UI_UPDATE: u32 = WM_APP + 1;
const WM_UI_BRING_TO_FOREGROUND: u32 = WM_APP + 2;
const WM_CURSOR_BLINK: u32 = WM_APP + 3;
const WM_SHOW_ROOT: u32 = WM_APP + 4;
const WM_EXIT: u32 = WM_APP + 5;
const WM_PREVIEW_LOADING: u32 = WM_APP + 6;
const WM_INDEXING_SPINNER: u32 = WM_APP + 7;
const WM_TOAST: u32 = WM_APP + 8;
const PREVIEW_LOADING_DELAY_MS: u32 = 400;
const INDEXING_SPINNER_INTERVAL_MS: u32 = 80;
const INDEXING_SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const DEFAULT_HEIGHT: i32 = 320;
const COLOR_BACKGROUND: u32 = 0x282828;
const COLOR_TEXT: u32 = 0xebdbb2;
const COLOR_BORDER: u32 = 0x928374;
const COLOR_MATCH: u32 = 0xfb4934;
const COLOR_SELECTED: u32 = 0x3c3836;
const COLOR_SELECTED_ACCENT: u32 = 0xb8bb26;
const TOAST_HORIZONTAL_PADDING: f32 = 18.0;
const TOAST_VERTICAL_PADDING: f32 = 12.0;
const DEFAULT_LOCATION_VALUE: i32 = i32::MIN;

static PREFERRED_CENTER_X: AtomicI32 = AtomicI32::new(DEFAULT_LOCATION_VALUE);
static PREFERRED_CENTER_Y: AtomicI32 = AtomicI32::new(DEFAULT_LOCATION_VALUE);
static DESIRED_WINDOW_HEIGHT: AtomicI32 = AtomicI32::new(DEFAULT_HEIGHT);

pub fn set_preferred_center(x: i32, y: i32) {
    PREFERRED_CENTER_X.store(x, Ordering::Relaxed);
    PREFERRED_CENTER_Y.store(y, Ordering::Relaxed);
}

pub fn run(
    view_model: Arc<ViewModel>,
    completion: Option<Receiver<i32>>,
    preview_available: bool,
    preview_visible: bool,
) -> Result<i32> {
    let exit_code;
    unsafe {
        let class_name = wide_null("NfmRustD2DPicker");
        let hinstance = GetModuleHandleW(None)?;
        let cursor = LoadCursorW(None, IDC_ARROW)?;
        let wnd_class = WNDCLASSW {
            hCursor: cursor,
            hbrBackground: CreateSolidBrush(COLORREF(COLOR_BACKGROUND)),
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
            screen_location.scale(),
            preview_available,
            preview_visible,
        )?);
        let state_ptr = Box::into_raw(state);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED,
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
                shared
                    .lock()
                    .expect("shared UI state poisoned")
                    .events
                    .push(event);
                unsafe {
                    let _ = PostMessageW(Some(hwnd), WM_UI_UPDATE, WPARAM(0), LPARAM(0));
                }
            }
        });

        if let Some(completion) = completion {
            std::thread::spawn(move || {
                if let Ok(code) = completion.recv() {
                    let hwnd = HWND(raw_hwnd as *mut c_void);
                    unsafe {
                        let _ = PostMessageW(Some(hwnd), WM_EXIT, WPARAM(code as usize), LPARAM(0));
                    }
                }
            });
        }

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        exit_code = msg.wParam.0 as i32;
    }

    Ok(exit_code)
}

struct GdiBackBuffer {
    dc: HDC,
    bitmap: HBITMAP,
    previous: HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
}

impl GdiBackBuffer {
    unsafe fn new(width: i32, height: i32) -> Result<Self> {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                biSizeImage: (width * height * 4) as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        let dc = CreateCompatibleDC(None);
        if dc.0.is_null() {
            return Err(anyhow!("CreateCompatibleDC failed"));
        }
        let mut bits = std::ptr::null_mut();
        let bitmap = match CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(bitmap) => bitmap,
            Err(error) => {
                let _ = DeleteDC(dc);
                return Err(error.into());
            }
        };
        let previous = SelectObject(dc, HGDIOBJ(bitmap.0));
        Ok(Self {
            dc,
            bitmap,
            previous,
            bits: bits.cast(),
            width,
            height,
        })
    }

    unsafe fn present(
        &mut self,
        hwnd: HWND,
        surface: &mut Surface,
        location: ScreenLocation,
    ) -> Result<()> {
        let pixmap = surface
            .peek_pixels()
            .context("Skia raster pixels are not readable")?;
        let source = pixmap
            .bytes()
            .context("Skia raster pixels are not readable")?;
        let row_bytes = self.width as usize * 4;
        for row in 0..self.height as usize {
            std::ptr::copy_nonoverlapping(
                source.as_ptr().add(row * pixmap.row_bytes()),
                self.bits.add(row * row_bytes),
                row_bytes,
            );
        }
        UpdateLayeredWindow(
            hwnd,
            None,
            Some(&POINT {
                x: location.x,
                y: location.y,
            }),
            Some(&SIZE {
                cx: location.width,
                cy: location.height,
            }),
            Some(self.dc),
            Some(&POINT { x: 0, y: 0 }),
            COLORREF(0),
            None,
            ULW_OPAQUE,
        )
        .context("UpdateLayeredWindow failed")
    }
}

impl Drop for GdiBackBuffer {
    fn drop(&mut self) {
        unsafe {
            let _ = SelectObject(self.dc, self.previous);
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.dc);
        }
    }
}

struct WindowState {
    view_model: Arc<ViewModel>,
    shared: Arc<Mutex<SharedUiState>>,
    surface: Option<Surface>,
    back_buffer: Option<GdiBackBuffer>,
    font: Font,
    bold_font: Font,
    italic_font: Font,
    bold_italic_font: Font,
    counter_font: Font,
    text_paint: Paint,
    muted_paint: Paint,
    highlight_paint: Paint,
    selected_paint: Paint,
    selected_accent_paint: Paint,
    stroke_paint: Paint,
    results: Vec<DisplaySearchResult>,
    counters: UiCounters,
    selected_row: usize,
    indexing_spinner_frame: usize,
    visible: bool,
    window_shown: bool,
    last_window_location: Option<ScreenLocation>,
    logged_first_items_paint: bool,
    cursor_visible: bool,
    preview_available: bool,
    preview_enabled: bool,
    preview_generation: u64,
    preview_lines: Arc<[PreviewLine]>,
    preview_image: Option<Image>,
    preview_top_line: usize,
    preview_loading: bool,
    preview_loading_visible: bool,
    preview_truncated: bool,
    preview_error: Option<String>,
    pending_native_preview: Option<NativeWindowId>,
    native_thumbnail: Option<DwmThumbnailPreview>,
    pending_high_surrogate: Option<u16>,
    input_ready_at: Instant,
    toast: Option<Toast>,
    layout: Layout,
}

struct Toast {
    text: String,
    expires_at: Instant,
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
    preview_border: Option<Rect>,
    preview_box: Option<Rect>,
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
    events: Vec<UiEvent>,
}

struct DwmThumbnailPreview {
    destination: HWND,
    thumbnail: Option<isize>,
    source: Option<isize>,
}

impl DwmThumbnailPreview {
    fn new(destination: HWND) -> Self {
        Self {
            destination,
            thumbnail: None,
            source: None,
        }
    }

    fn set_source(&mut self, source: Option<isize>) -> Result<()> {
        use windows::Win32::Graphics::Dwm::DwmRegisterThumbnail;
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;

        if self.source == source {
            return Ok(());
        }
        self.unregister();
        let Some(source) = source else {
            return Ok(());
        };
        let source_hwnd = HWND(source as *mut c_void);
        if !unsafe { IsWindow(Some(source_hwnd)) }.as_bool() {
            return Err(anyhow!("preview source window is no longer available"));
        }
        self.thumbnail = Some(
            unsafe { DwmRegisterThumbnail(self.destination, source_hwnd) }
                .context("DwmRegisterThumbnail failed")?,
        );
        self.source = Some(source);
        Ok(())
    }

    fn update_layout(&self, container: Rect, scale: f32) -> Result<()> {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Dwm::{
            DwmQueryThumbnailSourceSize, DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES,
            DWM_TNP_RECTDESTINATION, DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE,
        };
        let Some(thumbnail) = self.thumbnail else {
            return Ok(());
        };
        let source = unsafe { DwmQueryThumbnailSourceSize(thumbnail) }
            .context("DwmQueryThumbnailSourceSize failed")?;
        let destination = fit_thumbnail_rect(
            RECT {
                left: ((container.x + 8.0) * scale).round() as i32,
                top: ((container.y + 8.0) * scale).round() as i32,
                right: ((container.x + container.width - 8.0) * scale).round() as i32,
                bottom: ((container.y + container.height - 8.0) * scale).round() as i32,
            },
            source,
        )
        .ok_or_else(|| anyhow!("thumbnail has no drawable area"))?;
        let properties = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_SOURCECLIENTAREAONLY,
            rcDestination: destination,
            fVisible: true.into(),
            fSourceClientAreaOnly: true.into(),
            ..Default::default()
        };
        unsafe { DwmUpdateThumbnailProperties(thumbnail, &properties) }
            .context("DwmUpdateThumbnailProperties failed")
    }

    fn unregister(&mut self) {
        if let Some(thumbnail) = self.thumbnail.take() {
            let _ = unsafe { windows::Win32::Graphics::Dwm::DwmUnregisterThumbnail(thumbnail) };
        }
        self.source = None;
    }
}

impl Drop for DwmThumbnailPreview {
    fn drop(&mut self) {
        self.unregister();
    }
}

fn fit_thumbnail_rect(
    container: windows::Win32::Foundation::RECT,
    source: windows::Win32::Foundation::SIZE,
) -> Option<windows::Win32::Foundation::RECT> {
    use windows::Win32::Foundation::RECT;
    let container_width = container.right - container.left;
    let container_height = container.bottom - container.top;
    if container_width <= 0 || container_height <= 0 || source.cx <= 0 || source.cy <= 0 {
        return None;
    }
    let scale =
        (container_width as f64 / source.cx as f64).min(container_height as f64 / source.cy as f64);
    let width = (source.cx as f64 * scale).round().max(1.0) as i32;
    let height = (source.cy as f64 * scale).round().max(1.0) as i32;
    let left = container.left + (container_width - width) / 2;
    let top = container.top + (container_height - height) / 2;
    Some(RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    })
}

fn calculate_layout(
    window: Rect,
    padding: f32,
    number_of_items: i32,
    text_height: f32,
    preview_enabled: bool,
) -> Layout {
    let preview_border = preview_enabled.then(|| {
        let maximum_height =
            PREVIEW_ROWS as f32 * text_height + PANEL_VERTICAL_PADDING * 2.0 + PANEL_BORDER * 2.0;
        let minimum_height = text_height + PANEL_VERTICAL_PADDING * 2.0 + PANEL_BORDER * 2.0;
        let height_without_preview =
            desired_window_height(text_height, padding, number_of_items, false) as f32;
        let available_height = window.height - height_without_preview - PANEL_GAP;
        Rect {
            x: padding,
            y: padding,
            width: (window.width - padding * 2.0).max(0.0),
            height: available_height.clamp(minimum_height, maximum_height),
        }
    });
    let preview_box = preview_border.map(|border| Rect {
        x: border.x + PANEL_BORDER + PANEL_HORIZONTAL_PADDING,
        y: border.y + PANEL_BORDER + PANEL_VERTICAL_PADDING,
        width: (border.width - (PANEL_BORDER + PANEL_HORIZONTAL_PADDING) * 2.0).max(0.0),
        height: (border.height - (PANEL_BORDER + PANEL_VERTICAL_PADDING) * 2.0).max(0.0),
    });
    let list_top = preview_border
        .map(|border| border.y + border.height + PANEL_GAP)
        .unwrap_or(padding);
    let list_border_width = (window.width - (padding * 2.0)).max(0.0);
    let row_size = Size {
        height: text_height + (RESULT_VERTICAL_PADDING * 2.0) + RESULT_GAP,
        width: (list_border_width - ((PANEL_BORDER + PANEL_HORIZONTAL_PADDING) * 2.0)).max(0.0),
    };
    let list_border = Rect {
        x: padding,
        y: list_top,
        width: (window.width - (padding * 2.0)).max(0.0),
        height: (number_of_items as f32 * (text_height + RESULT_VERTICAL_PADDING * 2.0))
            + ((number_of_items - 1).max(0) as f32 * RESULT_GAP)
            + (PANEL_VERTICAL_PADDING * 2.0)
            + (PANEL_BORDER * 2.0),
    };
    let list_box = Rect {
        x: list_border.x + PANEL_BORDER + PANEL_HORIZONTAL_PADDING,
        y: list_border.y + PANEL_BORDER + PANEL_VERTICAL_PADDING,
        width: row_size.width,
        height: (list_border.height - ((PANEL_BORDER + PANEL_VERTICAL_PADDING) * 2.0)).max(0.0),
    };
    let search_border = Rect {
        x: padding,
        y: list_border.y + list_border.height + PANEL_GAP,
        width: (window.width - (padding * 2.0)).max(0.0),
        height: text_height + (QUERY_VERTICAL_PADDING * 2.0) + (PANEL_BORDER * 2.0),
    };
    let search_box = Rect {
        x: search_border.x + PANEL_BORDER + PANEL_HORIZONTAL_PADDING,
        y: search_border.y + PANEL_BORDER + QUERY_VERTICAL_PADDING,
        width: (search_border.width - ((PANEL_BORDER + PANEL_HORIZONTAL_PADDING) * 2.0)).max(0.0),
        height: text_height,
    };
    Layout {
        window,
        preview_border,
        preview_box,
        search_border,
        search_box,
        list_border,
        list_box,
        row_size,
        number_of_items,
        text_height,
    }
}

fn desired_window_height(
    text_height: f32,
    padding: f32,
    number_of_items: i32,
    preview_enabled: bool,
) -> i32 {
    let search_border_height = text_height + (QUERY_VERTICAL_PADDING * 2.0) + (PANEL_BORDER * 2.0);
    let list_border_height = (number_of_items as f32
        * (text_height + RESULT_VERTICAL_PADDING * 2.0))
        + ((number_of_items - 1).max(0) as f32 * RESULT_GAP)
        + (PANEL_VERTICAL_PADDING * 2.0)
        + (PANEL_BORDER * 2.0);
    let preview_height = if preview_enabled {
        PREVIEW_ROWS as f32 * text_height
            + PANEL_VERTICAL_PADDING * 2.0
            + PANEL_BORDER * 2.0
            + PANEL_GAP
    } else {
        0.0
    };
    (padding + preview_height + list_border_height + PANEL_GAP + search_border_height + padding)
        .ceil() as i32
}

impl WindowState {
    fn new(
        view_model: Arc<ViewModel>,
        shared: Arc<Mutex<SharedUiState>>,
        _left: f32,
        _top: f32,
        width: f32,
        _height: f32,
        scale_factor: f32,
        preview_available: bool,
        preview_visible: bool,
    ) -> Result<Self> {
        let font_manager = FontMgr::default();
        let typeface = font_manager
            .legacy_make_typeface("Cascadia Mono", FontStyle::normal())
            .or_else(|| font_manager.legacy_make_typeface(None, FontStyle::normal()))
            .context("failed to create Skia typeface")?;
        let make_font = |style| {
            let face = font_manager
                .legacy_make_typeface("Cascadia Mono", style)
                .unwrap_or_else(|| typeface.clone());
            let mut font = Font::new(face, 15.0);
            font.set_subpixel(true);
            font
        };
        let font = make_font(FontStyle::normal());
        let bold_font = make_font(FontStyle::bold());
        let italic_font = make_font(FontStyle::italic());
        let bold_italic_font = make_font(FontStyle::bold_italic());
        let counter_font = Font::new(typeface, 15.0);
        let text_height = font.metrics().0;
        let desired_height =
            desired_window_height(text_height, PADDING, DISPLAY_ROWS, preview_visible);
        DESIRED_WINDOW_HEIGHT.store(desired_height, Ordering::Relaxed);

        let window = Rect {
            x: 0.0,
            y: 0.0,
            width: width / scale_factor,
            height: desired_height as f32,
        };

        let app_layout =
            calculate_layout(window, PADDING, DISPLAY_ROWS, text_height, preview_visible);

        Ok(Self {
            view_model,
            shared,
            surface: None,
            back_buffer: None,
            font,
            bold_font,
            italic_font,
            bold_italic_font,
            counter_font,
            text_paint: fill_paint(COLOR_TEXT),
            muted_paint: fill_paint(COLOR_TEXT),
            highlight_paint: fill_paint(COLOR_MATCH),
            selected_paint: fill_paint(COLOR_SELECTED),
            selected_accent_paint: fill_paint(COLOR_SELECTED_ACCENT),
            stroke_paint: stroke_paint(COLOR_BORDER, PANEL_BORDER),
            results: Vec::new(),
            counters: UiCounters::default(),
            selected_row: 0,
            indexing_spinner_frame: 0,
            visible: false,
            window_shown: false,
            last_window_location: None,
            logged_first_items_paint: false,
            cursor_visible: false,
            preview_available,
            preview_enabled: preview_visible,
            preview_generation: 0,
            preview_lines: Arc::from([]),
            preview_image: None,
            preview_top_line: 0,
            preview_loading: false,
            preview_loading_visible: false,
            preview_truncated: false,
            preview_error: None,
            pending_native_preview: None,
            native_thumbnail: None,
            pending_high_surrogate: None,
            input_ready_at: Instant::now(),
            toast: None,
            layout: app_layout,
        })
    }

    fn apply_pending(&mut self, hwnd: HWND) {
        let events = {
            let mut pending = self.shared.lock().expect("shared UI state poisoned");
            std::mem::take(&mut pending.events)
        };
        for event in events {
            match event {
                UiEvent::Show => unsafe {
                    self.visible = true;
                    self.show_root(hwnd);
                },
                UiEvent::Results(update) => {
                    let was_scanning = self.counters.scanning;
                    self.results = update.results;
                    self.counters = update.counters;
                    self.selected_row = update.selected_row;
                    unsafe {
                        if self.counters.scanning && !was_scanning {
                            self.indexing_spinner_frame = 0;
                            let _ = SetTimer(
                                Some(hwnd),
                                WM_INDEXING_SPINNER as usize,
                                INDEXING_SPINNER_INTERVAL_MS,
                                None,
                            );
                        } else if !self.counters.scanning && was_scanning {
                            self.indexing_spinner_frame = 0;
                            let _ = KillTimer(Some(hwnd), WM_INDEXING_SPINNER as usize);
                        }
                    }
                }
                UiEvent::Preview(PreviewView::Text { update, top_line }) => {
                    let preserve_previous_view = matches!(&update, PreviewUpdate::Clear { .. });
                    match update {
                        PreviewUpdate::Clear { generation } => {
                            self.preview_generation = generation;
                            self.preview_loading = true;
                            self.preview_loading_visible = false;
                            self.preview_error = None;
                            unsafe {
                                let _ = SetTimer(
                                    Some(hwnd),
                                    WM_PREVIEW_LOADING as usize,
                                    PREVIEW_LOADING_DELAY_MS,
                                    None,
                                );
                            }
                        }
                        PreviewUpdate::Ready {
                            generation,
                            lines,
                            truncated,
                            ..
                        } if generation == self.preview_generation => {
                            self.preview_lines = lines;
                            self.preview_image = None;
                            self.preview_loading = false;
                            self.preview_loading_visible = false;
                            self.preview_truncated = truncated;
                            unsafe {
                                let _ = KillTimer(Some(hwnd), WM_PREVIEW_LOADING as usize);
                            }
                        }
                        PreviewUpdate::ImageReady {
                            generation,
                            encoded,
                        } if generation == self.preview_generation => {
                            self.preview_loading = false;
                            self.preview_loading_visible = false;
                            self.preview_error = None;
                            self.preview_lines = Arc::from([]);
                            self.preview_truncated = false;
                            self.preview_image = Image::from_encoded(Data::new_copy(&encoded));
                            if self.preview_image.is_none() {
                                self.preview_error =
                                    Some("preview produced an unsupported or invalid image".into());
                            }
                            unsafe {
                                let _ = KillTimer(Some(hwnd), WM_PREVIEW_LOADING as usize);
                            }
                        }
                        PreviewUpdate::Error {
                            generation,
                            message,
                        } if generation == self.preview_generation => {
                            self.preview_loading = false;
                            self.preview_loading_visible = false;
                            self.preview_lines = Arc::from([]);
                            self.preview_image = None;
                            self.preview_truncated = false;
                            self.preview_error = Some(message);
                            unsafe {
                                let _ = KillTimer(Some(hwnd), WM_PREVIEW_LOADING as usize);
                            }
                        }
                        _ => {}
                    }
                    if !preserve_previous_view {
                        self.preview_top_line = top_line;
                    }
                }
                UiEvent::Preview(PreviewView::NativeWindow(source)) => {
                    self.pending_native_preview = source;
                }
                UiEvent::PreviewVisibilityChanged { visible } => unsafe {
                    if self.preview_available && self.preview_enabled != visible {
                        self.preview_enabled = visible;
                        let height = desired_window_height(
                            self.layout.text_height,
                            PADDING,
                            DISPLAY_ROWS,
                            visible,
                        );
                        DESIRED_WINDOW_HEIGHT.store(height, Ordering::Relaxed);
                        self.show_root(hwnd);
                    }
                },
                UiEvent::ShowToast { text, duration } => unsafe {
                    self.toast = Some(Toast {
                        text,
                        expires_at: Instant::now() + duration,
                    });
                    let milliseconds = duration.as_millis().clamp(1, u32::MAX as u128) as u32;
                    let _ = SetTimer(Some(hwnd), WM_TOAST as usize, milliseconds, None);
                },
                UiEvent::Hide => unsafe {
                    self.visible = false;
                    self.window_shown = false;
                    if let Some(thumbnail) = &mut self.native_thumbnail {
                        thumbnail.unregister();
                    }
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    let _ = KillTimer(Some(hwnd), WM_PREVIEW_LOADING as usize);
                    let _ = KillTimer(Some(hwnd), WM_INDEXING_SPINNER as usize);
                    let _ = KillTimer(Some(hwnd), WM_TOAST as usize);
                    self.toast = None;
                    self.logged_first_items_paint = false;
                },
            }
        }
        unsafe {
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }

    unsafe fn show_root(&mut self, hwnd: HWND) {
        let first_show = !self.window_shown;
        let location = calculate_window_location();
        if self.last_window_location != Some(location) {
            self.surface = None;
            self.back_buffer = None;
            self.last_window_location = Some(location);
            self.layout = calculate_layout(
                Rect {
                    x: 0.0,
                    y: 0.0,
                    width: location.width as f32 / location.scale(),
                    height: location.height as f32 / location.scale(),
                },
                PADDING,
                DISPLAY_ROWS,
                self.layout.text_height,
                self.preview_enabled,
            );
            let preview_rows = self
                .layout
                .preview_box
                .map(|preview| (preview.height / self.layout.text_height).floor() as usize)
                .unwrap_or(0);
            self.view_model.set_preview_visible_rows(preview_rows);
        }

        // UpdateLayeredWindow submits the complete buffer and new window geometry as one
        // compositor update, avoiding an independently visible resize operation.
        if let Err(error) = self.paint(hwnd) {
            eprintln!("failed to paint picker: {error:#}");
            let _ = InvalidateRect(Some(hwnd), None, false);
            let _ = UpdateWindow(hwnd);
        }

        if first_show && self.visible {
            self.window_shown = true;
            let _ = ShowWindow(hwnd, SW_SHOW);
            bring_to_foreground(hwnd);
            let _ = SetFocus(Some(hwnd));
        }
        self.start_input(hwnd);
    }

    unsafe fn start_input(&mut self, hwnd: HWND) {
        let blink_ms = GetCaretBlinkTime();
        if SetTimer(Some(hwnd), WM_CURSOR_BLINK as usize, blink_ms, None) != 0 {}
        self.input_ready_at = Instant::now() + Duration::from_millis(150);
    }

    fn ensure_surface(&mut self) -> Result<()> {
        let location = self
            .last_window_location
            .context("window location not initialized")?;
        let width = location.width.max(1);
        let height = location.height.max(1);
        let needs_surface = self
            .surface
            .as_ref()
            .map(|surface| surface.width() != width || surface.height() != height)
            .unwrap_or(true);
        if needs_surface {
            self.surface = surfaces::raster_n32_premul((width, height));
            if self.surface.is_none() {
                return Err(anyhow!("failed to create Skia raster surface"));
            }
            self.back_buffer = Some(unsafe { GdiBackBuffer::new(width, height)? });
        }
        Ok(())
    }

    fn sync_native_thumbnail(&mut self, hwnd: HWND) {
        if self.native_thumbnail.is_none() {
            self.native_thumbnail = Some(DwmThumbnailPreview::new(hwnd));
        }
        let thumbnail = self.native_thumbnail.as_mut().expect("thumbnail created");
        if !self.visible || !self.preview_enabled {
            thumbnail.unregister();
            return;
        }
        let source = self.pending_native_preview.map(|window| window.0);
        if let Err(error) = thumbnail.set_source(source) {
            self.preview_error = Some(format!("Window preview unavailable: {error}"));
            return;
        }
        if source.is_some() {
            if let Some(preview) = self.layout.preview_box {
                let scale = self
                    .last_window_location
                    .map(ScreenLocation::scale)
                    .unwrap_or(1.0);
                if let Err(error) = thumbnail.update_layout(preview, scale) {
                    self.preview_error = Some(format!("Window preview unavailable: {error}"));
                }
            }
        }
    }

    unsafe fn paint(&mut self, hwnd: HWND) -> Result<()> {
        self.ensure_surface()?;
        let scale = self
            .last_window_location
            .context("window location not initialized")?
            .scale();
        let Some(surface) = self.surface.as_mut() else {
            return Ok(());
        };
        let canvas = surface.canvas();
        canvas.clear(skia_color(COLOR_BACKGROUND));
        let frame_save_count = canvas.save();
        canvas.reset_matrix();
        canvas.scale((scale, scale));

        if let (Some(border), Some(area)) = (self.layout.preview_border, self.layout.preview_box) {
            draw_skia_round_rect(canvas, &self.stroke_paint, border, 8.0);
            if let Some(image) = &self.preview_image {
                let scale =
                    (area.width / image.width() as f32).min(area.height / image.height() as f32);
                let width = image.width() as f32 * scale;
                let height = image.height() as f32 * scale;
                canvas.draw_image_rect(
                    image,
                    None,
                    SkRect::from_xywh(
                        area.x + (area.width - width) / 2.0,
                        area.y + (area.height - height) / 2.0,
                        width,
                        height,
                    ),
                    &self.text_paint,
                );
            } else {
                let visible_rows = (area.height / self.layout.text_height).floor() as usize;
                let content_rows = if self.preview_truncated {
                    visible_rows.saturating_sub(1)
                } else {
                    visible_rows
                };
                for (row, line) in self
                    .preview_lines
                    .iter()
                    .skip(self.preview_top_line)
                    .take(content_rows)
                    .enumerate()
                {
                    draw_skia_preview_line(
                        canvas,
                        &self.font,
                        &self.bold_font,
                        &self.italic_font,
                        &self.bold_italic_font,
                        line,
                        Rect {
                            x: area.x,
                            y: area.y + row as f32 * self.layout.text_height,
                            width: area.width,
                            height: self.layout.text_height,
                        },
                    );
                }
                let status = if self.preview_loading_visible {
                    Some(("Loading preview…", &self.muted_paint))
                } else if let Some(error) = self.preview_error.as_deref() {
                    Some((error, &self.highlight_paint))
                } else if self.preview_truncated {
                    Some((
                        "… preview truncated at 4,000 lines or 1 MiB",
                        &self.muted_paint,
                    ))
                } else {
                    None
                };
                if let Some((text, paint)) = status {
                    let row = if self.preview_truncated {
                        content_rows
                    } else {
                        0
                    };
                    draw_skia_text(
                        canvas,
                        &self.font,
                        paint,
                        text,
                        Rect {
                            x: area.x,
                            y: area.y + row as f32 * self.layout.text_height,
                            width: area.width,
                            height: self.layout.text_height,
                        },
                        TextAlign::Left,
                    );
                }
            }
        }

        let search = self.view_model.current_search_text();
        let prompt = if self.counters.scanning {
            format!(
                "{} ",
                INDEXING_SPINNER_FRAMES
                    [self.indexing_spinner_frame % INDEXING_SPINNER_FRAMES.len()]
            )
        } else {
            "> ".to_owned()
        };
        let prompt_width = skia_text_width(&self.font, &self.text_paint, &prompt);
        draw_skia_round_rect(canvas, &self.stroke_paint, self.layout.search_border, 8.0);
        if let Some(selection) = search.selection {
            let prefix_width = skia_text_width(
                &self.font,
                &self.text_paint,
                &search.text[..selection.start],
            );
            let selection_width = skia_text_width(
                &self.font,
                &self.text_paint,
                &search.text[selection.start..selection.end],
            );
            draw_skia_rect(
                canvas,
                &self.selected_paint,
                Rect {
                    x: self.layout.search_box.x + prompt_width + prefix_width,
                    y: self.layout.search_box.y,
                    width: selection_width,
                    height: self.layout.search_box.height,
                },
            );
        }
        draw_skia_text(
            canvas,
            &self.font,
            &self.text_paint,
            &prompt,
            self.layout.search_box,
            TextAlign::Left,
        );
        let query = Rect {
            x: self.layout.search_box.x + prompt_width,
            y: self.layout.search_box.y,
            width: (self.layout.search_box.width - prompt_width - COUNTER_WIDTH).max(0.0),
            height: self.layout.search_box.height,
        };
        draw_skia_text(
            canvas,
            &self.font,
            &self.text_paint,
            &search.text,
            query,
            TextAlign::Left,
        );
        if self.cursor_visible {
            let cursor_x = query.x
                + skia_text_width(
                    &self.font,
                    &self.text_paint,
                    &search.text[..search.cursor_position],
                );
            canvas.draw_line(
                (cursor_x + 1.0, query.y),
                (cursor_x + 1.0, query.y + query.height),
                &self.text_paint,
            );
        }
        let counter = if self.counters.scanning {
            format!("{}/{}", self.counters.displayed, self.counters.published)
        } else {
            format!("{}/{}", self.counters.matched, self.counters.published)
        };
        draw_skia_text(
            canvas,
            &self.counter_font,
            &self.muted_paint,
            &counter,
            Rect {
                x: self.layout.search_box.x + self.layout.search_box.width - COUNTER_WIDTH,
                y: self.layout.search_box.y,
                width: COUNTER_WIDTH,
                height: self.layout.text_height,
            },
            TextAlign::Right,
        );

        draw_skia_round_rect(canvas, &self.stroke_paint, self.layout.list_border, 8.0);
        for visual_row in 0..DISPLAY_ROWS as usize {
            let index = DISPLAY_ROWS as usize - visual_row - 1;
            let result = self.results.get(index);
            let top = self.layout.list_box.y + visual_row as f32 * self.layout.row_size.height;
            let text_top = top + RESULT_VERTICAL_PADDING;
            if result.is_some() && index == self.selected_row {
                let selected = Rect {
                    x: self.layout.list_box.x,
                    y: top,
                    width: self.layout.row_size.width,
                    height: self.layout.row_size.height - RESULT_GAP,
                };
                draw_skia_round_rect(canvas, &self.selected_paint, selected, 6.0);
                draw_skia_round_rect(
                    canvas,
                    &self.selected_accent_paint,
                    Rect {
                        x: selected.x,
                        y: selected.y + RESULT_VERTICAL_PADDING,
                        width: SELECTED_ACCENT_WIDTH,
                        height: (selected.height - RESULT_VERTICAL_PADDING * 2.0).max(0.0),
                    },
                    SELECTED_ACCENT_WIDTH / 2.0,
                );
            }
            let Some(result) = result else {
                continue;
            };
            let text_rect = Rect {
                x: self.layout.list_box.x + RESULT_HORIZONTAL_PADDING,
                y: text_top,
                width: (self.layout.list_box.width - RESULT_HORIZONTAL_PADDING * 2.0).max(0.0),
                height: self.layout.text_height,
            };
            draw_skia_text(
                canvas,
                &self.font,
                &self.text_paint,
                &result.result.path,
                text_rect,
                TextAlign::Left,
            );
            draw_skia_highlights(
                canvas,
                &self.font,
                &self.highlight_paint,
                &result.result.path,
                &result.positions,
                text_rect,
            );
        }
        if let Some(toast) = &self.toast {
            let maximum_width = (self.layout.window.width - PADDING * 4.0).max(0.0);
            let text_width = skia_text_width(&self.font, &self.text_paint, &toast.text);
            let width = (text_width + TOAST_HORIZONTAL_PADDING * 2.0).min(maximum_width);
            let height = self.layout.text_height + TOAST_VERTICAL_PADDING * 2.0;
            let rect = Rect {
                x: (self.layout.window.width - width) / 2.0,
                y: (self.layout.window.height - height) / 2.0,
                width,
                height,
            };
            draw_skia_round_rect(canvas, &self.selected_paint, rect, 8.0);
            draw_skia_round_rect(canvas, &self.stroke_paint, rect, 8.0);
            draw_skia_text(
                canvas,
                &self.font,
                &self.text_paint,
                &toast.text,
                Rect {
                    x: rect.x + TOAST_HORIZONTAL_PADDING,
                    y: rect.y + TOAST_VERTICAL_PADDING,
                    width: (rect.width - TOAST_HORIZONTAL_PADDING * 2.0).max(0.0),
                    height: self.layout.text_height,
                },
                TextAlign::Left,
            );
        }
        canvas.restore_to_count(frame_save_count);
        self.back_buffer
            .as_mut()
            .context("GDI back buffer not initialized")?
            .present(
                hwnd,
                surface,
                self.last_window_location
                    .context("window location not initialized")?,
            )?;
        self.sync_native_thumbnail(hwnd);
        if self.visible && !self.logged_first_items_paint && !self.results.is_empty() {
            self.logged_first_items_paint = true;
            let _ = PostMessageW(Some(hwnd), WM_UI_BRING_TO_FOREGROUND, WPARAM(0), LPARAM(0));
        }
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

fn key_chord(vkey: usize, modifiers: KeyModifiers) -> Option<KeyChord> {
    let key = match vkey as u16 {
        value if value == VK_RETURN.0 => KeyName::Enter,
        value if value == VK_ESCAPE.0 => KeyName::Escape,
        value if value == VK_UP.0 => KeyName::Up,
        value if value == VK_DOWN.0 => KeyName::Down,
        value if value == VK_LEFT.0 => KeyName::Left,
        value if value == VK_RIGHT.0 => KeyName::Right,
        value if value == VK_HOME.0 => KeyName::Home,
        value if value == VK_END.0 => KeyName::End,
        value if value == VK_BACK.0 => KeyName::Backspace,
        value if value == VK_DELETE.0 => KeyName::Delete,
        value if value == VK_PRIOR.0 => KeyName::PageUp,
        value if value == VK_NEXT.0 => KeyName::PageDown,
        value @ 0x41..=0x5a => KeyName::Character((value as u8 as char).to_ascii_lowercase()),
        value @ 0x30..=0x39 => KeyName::Character(value as u8 as char),
        _ => return None,
    };
    Some(KeyChord { key, modifiers })
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
        WM_ERASEBKGND => LRESULT(1),
        WM_DPICHANGED => {
            let state = window_state(hwnd);
            if !state.is_null() {
                let state = &mut *state;
                state.last_window_location = None;
                state.surface = None;
                state.back_buffer = None;
                if state.visible {
                    state.show_root(hwnd);
                }
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
            } else if wparam.0 == WM_PREVIEW_LOADING as usize {
                let _ = KillTimer(Some(hwnd), WM_PREVIEW_LOADING as usize);
                let state = window_state(hwnd);
                if !state.is_null() {
                    let state = &mut *state;
                    if state.preview_loading {
                        state.preview_lines = Arc::from([]);
                        state.preview_image = None;
                        state.preview_top_line = 0;
                        state.preview_truncated = false;
                        state.preview_loading_visible = true;
                        let _ = state.paint(hwnd);
                    }
                }
            } else if wparam.0 == WM_INDEXING_SPINNER as usize {
                let state = window_state(hwnd);
                if !state.is_null() {
                    let state = &mut *state;
                    if state.counters.scanning {
                        state.indexing_spinner_frame =
                            (state.indexing_spinner_frame + 1) % INDEXING_SPINNER_FRAMES.len();
                        let _ = state.paint(hwnd);
                    } else {
                        let _ = KillTimer(Some(hwnd), WM_INDEXING_SPINNER as usize);
                    }
                }
            } else if wparam.0 == WM_TOAST as usize {
                let _ = KillTimer(Some(hwnd), WM_TOAST as usize);
                let state = window_state(hwnd);
                if !state.is_null() {
                    let state = &mut *state;
                    if state
                        .toast
                        .as_ref()
                        .is_some_and(|toast| Instant::now() >= toast.expires_at)
                    {
                        state.toast = None;
                        let _ = state.paint(hwnd);
                    }
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
                if Instant::now() < (*state).input_ready_at {
                    return LRESULT(0);
                }
                let unit = wparam.0 as u16;
                if unit >= 0x20 {
                    let state = &mut *state;
                    if (0xd800..=0xdbff).contains(&unit) {
                        state.pending_high_surrogate = Some(unit);
                    } else {
                        let units = state
                            .pending_high_surrogate
                            .take()
                            .map_or_else(|| vec![unit], |high| vec![high, unit]);
                        let text = String::from_utf16_lossy(&units);
                        state.view_model.insert_text(&text);
                    }
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            let state = window_state(hwnd);
            if !state.is_null() {
                if Instant::now() < (*state).input_ready_at {
                    return LRESULT(0);
                }
                let modifiers = current_modifiers();
                if let Some(chord) = key_chord(wparam.0, modifiers) {
                    let repeat = (lparam.0 as u32 & (1 << 30)) != 0;
                    let state = &mut *state;
                    state.view_model.handle_key(chord, repeat);
                }
                let _ = (*state).paint(hwnd);
            }
            LRESULT(0)
        }
        WM_EXIT => {
            PostQuitMessage(wparam.0 as i32);
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
    dpi: u32,
}

impl ScreenLocation {
    fn scale(self) -> f32 {
        self.dpi as f32 / 96.0
    }
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
            let mut dpi_x = 96;
            let mut dpi_y = 96;
            if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_err() {
                dpi_x = 96;
            }
            let scale = dpi_x as f32 / 96.0;
            let work = info.rcWork;
            let mon_width = work.right - work.left;
            let mon_height = work.bottom - work.top;
            let horizontal_margin = (MONITOR_HORIZONTAL_MARGIN as f32 * scale).round() as i32;
            let vertical_margin = (MONITOR_VERTICAL_MARGIN as f32 * scale).round() as i32;
            let minimum_width = (MIN_WIDTH as f32 * scale).round() as i32;
            let minimum_height = (MIN_HEIGHT as f32 * scale).round() as i32;
            let width = (DEFAULT_WIDTH as f32 * scale)
                .round()
                .min((mon_width - horizontal_margin).max(minimum_width) as f32)
                as i32;
            let desired_height = DESIRED_WINDOW_HEIGHT.load(Ordering::Relaxed);
            let height = (desired_height as f32 * scale)
                .round()
                .min((mon_height - vertical_margin).max(minimum_height) as f32)
                as i32;
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
                dpi: dpi_x,
            };
        }
    }

    ScreenLocation {
        x: 100,
        y: 100,
        width: DEFAULT_WIDTH,
        height: DEFAULT_HEIGHT,
        dpi: 96,
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

#[derive(Clone, Copy)]
enum TextAlign {
    Left,
    Right,
}

fn fill_paint(color: u32) -> Paint {
    let mut paint = Paint::default();
    paint.set_anti_alias(true);
    paint.set_style(PaintStyle::Fill);
    paint.set_color(skia_color(color));
    paint
}

fn stroke_paint(color: u32, width: f32) -> Paint {
    let mut paint = fill_paint(color);
    paint.set_style(PaintStyle::Stroke);
    paint.set_stroke_width(width);
    paint
}

fn skia_color(value: u32) -> Color {
    Color::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

fn skia_text_width(font: &Font, paint: &Paint, text: &str) -> f32 {
    font.measure_str(text, Some(paint)).0
}

fn draw_skia_text(
    canvas: &Canvas,
    font: &Font,
    paint: &Paint,
    text: &str,
    rect: Rect,
    align: TextAlign,
) {
    let save = canvas.save();
    canvas.clip_rect(
        SkRect::from_xywh(rect.x, rect.y, rect.width, rect.height),
        None,
        false,
    );
    let width = skia_text_width(font, paint, text);
    let x = match align {
        TextAlign::Left => rect.x,
        TextAlign::Right => rect.x + rect.width - width,
    };
    let (_, metrics) = font.metrics();
    canvas.draw_str(text, (x, rect.y - metrics.ascent), font, paint);
    canvas.restore_to_count(save);
}

fn draw_skia_rect(canvas: &Canvas, paint: &Paint, rect: Rect) {
    canvas.draw_rect(
        SkRect::from_xywh(rect.x, rect.y, rect.width, rect.height),
        paint,
    );
}

fn draw_skia_round_rect(canvas: &Canvas, paint: &Paint, rect: Rect, radius: f32) {
    canvas.draw_rrect(
        RRect::new_rect_xy(
            SkRect::from_xywh(rect.x, rect.y, rect.width, rect.height),
            radius,
            radius,
        ),
        paint,
    );
}

fn draw_skia_highlights(
    canvas: &Canvas,
    font: &Font,
    paint: &Paint,
    text: &str,
    positions: &[usize],
    rect: Rect,
) {
    for &position in positions {
        if !text.is_char_boundary(position) {
            continue;
        }
        let Some(character) = text[position..].chars().next() else {
            continue;
        };
        let prefix = &text[..position];
        let x = rect.x + skia_text_width(font, paint, prefix);
        if x >= rect.x + rect.width {
            break;
        }
        draw_skia_text(
            canvas,
            font,
            paint,
            &character.to_string(),
            Rect {
                x,
                y: rect.y,
                width: rect.x + rect.width - x,
                height: rect.height,
            },
            TextAlign::Left,
        );
    }
}

fn draw_skia_preview_line(
    canvas: &Canvas,
    normal: &Font,
    bold: &Font,
    italic: &Font,
    bold_italic: &Font,
    line: &PreviewLine,
    rect: Rect,
) {
    let save = canvas.save();
    canvas.clip_rect(
        SkRect::from_xywh(rect.x, rect.y, rect.width, rect.height),
        None,
        false,
    );
    let mut x = rect.x;
    for span in &line.spans {
        let font = match (span.style.bold, span.style.italic) {
            (true, true) => bold_italic,
            (true, false) => bold,
            (false, true) => italic,
            (false, false) => normal,
        };
        let (foreground, background) = resolved_preview_colors(&span.style);
        let paint = fill_paint(foreground);
        let width = skia_text_width(font, &paint, &span.text);
        if let Some(background) = background {
            draw_skia_rect(
                canvas,
                &fill_paint(background),
                Rect {
                    x,
                    y: rect.y,
                    width,
                    height: rect.height,
                },
            );
        }
        draw_skia_text(
            canvas,
            font,
            &paint,
            &span.text,
            Rect {
                x,
                y: rect.y,
                width: (rect.x + rect.width - x).max(0.0),
                height: rect.height,
            },
            TextAlign::Left,
        );
        x += width;
        if x >= rect.x + rect.width {
            break;
        }
    }
    canvas.restore_to_count(save);
}

fn resolved_preview_colors(style: &PreviewStyle) -> (u32, Option<u32>) {
    let foreground = style.foreground.map(ansi_color_value).unwrap_or(COLOR_TEXT);
    let background = style.background.map(ansi_color_value);
    let (foreground, background) = if style.inverse {
        (background.unwrap_or(COLOR_BACKGROUND), Some(foreground))
    } else {
        (foreground, background)
    };
    (
        if style.dim {
            dim_color(foreground)
        } else {
            foreground
        },
        background,
    )
}

fn ansi_color_value(color: crate::preview_document::AnsiColor) -> u32 {
    let (red, green, blue) = color.rgb();
    ((red as u32) << 16) | ((green as u32) << 8) | blue as u32
}

fn dim_color(color: u32) -> u32 {
    let red = ((color >> 16) & 0xff) * 2 / 3;
    let green = ((color >> 8) & 0xff) * 2 / 3;
    let blue = (color & 0xff) * 2 / 3;
    (red << 16) | (green << 8) | blue
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::{calculate_layout, fit_thumbnail_rect, Rect, DISPLAY_ROWS, PADDING};
    use windows::Win32::Foundation::{RECT, SIZE};

    #[test]
    fn preview_layout_places_preview_above_results() {
        let layout = calculate_layout(
            Rect {
                x: 0.0,
                y: 0.0,
                width: 1600.0,
                height: 800.0,
            },
            PADDING,
            DISPLAY_ROWS,
            20.0,
            true,
        );
        let preview = layout.preview_border.expect("preview enabled");
        assert!(preview.y < layout.list_border.y);
        assert!(layout.list_border.y < layout.search_border.y);
    }

    #[test]
    fn thumbnail_fit_preserves_aspect_ratio_and_centers() {
        let fitted = fit_thumbnail_rect(
            RECT {
                left: 10,
                top: 20,
                right: 410,
                bottom: 320,
            },
            SIZE { cx: 1600, cy: 900 },
        )
        .expect("drawable thumbnail");
        assert_eq!(fitted.left, 10);
        assert_eq!(fitted.right, 410);
        assert_eq!(fitted.top, 57);
        assert_eq!(fitted.bottom, 282);
    }

    #[test]
    fn thumbnail_fit_rejects_empty_source_or_destination() {
        assert!(fit_thumbnail_rect(RECT::default(), SIZE { cx: 100, cy: 100 }).is_none());
        assert!(fit_thumbnail_rect(
            RECT {
                left: 0,
                top: 0,
                right: 100,
                bottom: 100,
            },
            SIZE::default(),
        )
        .is_none());
    }
}
