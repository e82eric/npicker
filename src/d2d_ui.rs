#![allow(unsafe_op_in_unsafe_fn, unused_unsafe)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    D2DERR_RECREATE_TARGET, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_UNKNOWN, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Bitmap, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1SolidColorBrush,
    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, D2D1_DRAW_TEXT_OPTIONS_CLIP,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_FEATURE_LEVEL_DEFAULT,
    D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_ROUNDED_RECT,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_ITALIC, DWRITE_FONT_STYLE_NORMAL,
    DWRITE_FONT_WEIGHT_BOLD, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_TEXT_ALIGNMENT_TRAILING, DWRITE_TEXT_METRICS, DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, EndPaint, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, MonitorFromWindow,
    UpdateWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT,
};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_WICPixelFormat32bppPBGRA, IWICImagingFactory,
    WICBitmapDitherTypeNone, WICBitmapPaletteTypeMedianCut, WICDecodeMetadataCacheOnLoad,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    keybd_event, GetKeyState, SendInput, SetFocus, INPUT, INPUT_MOUSE, KEYEVENTF_KEYUP, VK_BACK,
    VK_CONTROL, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_MENU, VK_NEXT,
    VK_PRIOR, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetCaretBlinkTime, GetClientRect,
    GetForegroundWindow, GetMessageW, GetWindowLongPtrW, LoadCursorW, PostMessageW,
    PostQuitMessage, RegisterClassW, SetForegroundWindow, SetTimer, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, TranslateMessage, CREATESTRUCTW, GWLP_USERDATA, IDC_ARROW, MSG,
    SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW, SW_HIDE, WM_APP, WM_CHAR, WM_DESTROY, WM_KEYDOWN,
    WM_NCCREATE, WM_PAINT, WM_TIMER, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};
use windows_numerics::Vector2;

use crossbeam_channel::Receiver;

use crate::key_binding::{KeyChord, KeyName};
use crate::preview::{NativeWindowId, PreviewLine, PreviewUpdate};
use crate::preview_document::PreviewStyle;
use crate::view_model::{InputCommand, KeyModifiers, PreviewView, UiCounters, UiEvent, ViewModel};
use nfm_search_core::search::SearchResult;

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
const DEFAULT_HEIGHT: i32 = 320;
const COLOR_BACKGROUND: u32 = 0x282828;
const COLOR_TEXT: u32 = 0xebdbb2;
const COLOR_BORDER: u32 = 0x928374;
const COLOR_MATCH: u32 = 0xfb4934;
const COLOR_SELECTED: u32 = 0x3c3836;
const COLOR_SELECTED_ACCENT: u32 = 0xb8bb26;
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
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
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
            preview_available,
            preview_visible,
        )?);
        let state_ptr = Box::into_raw(state);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
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
        CoUninitialize();
    }

    Ok(exit_code)
}

struct WindowState {
    view_model: Arc<ViewModel>,
    shared: Arc<Mutex<SharedUiState>>,
    factory: ID2D1Factory,
    dwrite_factory: IDWriteFactory,
    wic_factory: IWICImagingFactory,
    target: Option<ID2D1HwndRenderTarget>,
    text_format: IDWriteTextFormat,
    bold_text_format: IDWriteTextFormat,
    italic_text_format: IDWriteTextFormat,
    bold_italic_text_format: IDWriteTextFormat,
    counter_text_format: IDWriteTextFormat,
    text_brush: Option<ID2D1SolidColorBrush>,
    muted_brush: Option<ID2D1SolidColorBrush>,
    highlight_brush: Option<ID2D1SolidColorBrush>,
    selected_brush: Option<ID2D1SolidColorBrush>,
    border_brush: Option<ID2D1SolidColorBrush>,
    accent_brush: Option<ID2D1SolidColorBrush>,
    style_brush: Option<ID2D1SolidColorBrush>,
    results: Vec<SearchResult>,
    counters: UiCounters,
    selected_row: usize,
    visible: bool,
    last_window_location: Option<ScreenLocation>,
    queued_foreground_after_first_paint: bool,
    logged_first_items_paint: bool,
    cursor_visible: bool,
    preview_available: bool,
    preview_enabled: bool,
    preview_generation: u64,
    preview_lines: Arc<[PreviewLine]>,
    preview_image_encoded: Option<Arc<[u8]>>,
    preview_bitmap: Option<ID2D1Bitmap>,
    preview_top_line: usize,
    preview_loading: bool,
    preview_truncated: bool,
    preview_error: Option<String>,
    pending_native_preview: Option<NativeWindowId>,
    native_thumbnail: Option<DwmThumbnailPreview>,
    pending_high_surrogate: Option<u16>,
    input_ready_at: Instant,
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

    fn update_layout(&self, container: Rect) -> Result<()> {
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
                left: (container.x + 8.0).round() as i32,
                top: (container.y + 8.0).round() as i32,
                right: (container.x + container.width - 8.0).round() as i32,
                bottom: (container.y + container.height - 8.0).round() as i32,
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
        left: f32,
        top: f32,
        width: f32,
        _height: f32,
        preview_available: bool,
        preview_visible: bool,
    ) -> Result<Self> {
        unsafe {
            let factory: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)
                .context("D2D1CreateFactory failed")?;
            let dwrite_factory: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)
                .context("DWriteCreateFactory failed")?;
            let wic_factory: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)
                    .context("creating WIC imaging factory failed")?;
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
            let bold_text_format = dwrite_factory.CreateTextFormat(
                PCWSTR(wide_null("Cascadia Mono").as_ptr()),
                None,
                DWRITE_FONT_WEIGHT_BOLD,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                15.0,
                PCWSTR(wide_null("en-us").as_ptr()),
            )?;
            bold_text_format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            let italic_text_format = dwrite_factory.CreateTextFormat(
                PCWSTR(wide_null("Cascadia Mono").as_ptr()),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_ITALIC,
                DWRITE_FONT_STRETCH_NORMAL,
                15.0,
                PCWSTR(wide_null("en-us").as_ptr()),
            )?;
            italic_text_format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
            let bold_italic_text_format = dwrite_factory.CreateTextFormat(
                PCWSTR(wide_null("Cascadia Mono").as_ptr()),
                None,
                DWRITE_FONT_WEIGHT_BOLD,
                DWRITE_FONT_STYLE_ITALIC,
                DWRITE_FONT_STRETCH_NORMAL,
                15.0,
                PCWSTR(wide_null("en-us").as_ptr()),
            )?;
            bold_italic_text_format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP)?;
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

            let desired_height =
                desired_window_height(text_height, PADDING, DISPLAY_ROWS, preview_visible);
            DESIRED_WINDOW_HEIGHT.store(desired_height, Ordering::Relaxed);

            let window = Rect {
                x: left,
                y: top,
                width,
                height: desired_height as f32,
            };

            let app_layout =
                calculate_layout(window, PADDING, DISPLAY_ROWS, text_height, preview_visible);

            Ok(Self {
                view_model,
                shared,
                factory,
                dwrite_factory,
                wic_factory,
                target: None,
                text_format,
                bold_text_format,
                italic_text_format,
                bold_italic_text_format,
                counter_text_format,
                text_brush: None,
                muted_brush: None,
                highlight_brush: None,
                selected_brush: None,
                border_brush: None,
                accent_brush: None,
                style_brush: None,
                results: Vec::new(),
                counters: UiCounters::default(),
                selected_row: 0,
                visible: false,
                last_window_location: None,
                queued_foreground_after_first_paint: false,
                logged_first_items_paint: false,
                cursor_visible: false,
                preview_available,
                preview_enabled: preview_visible,
                preview_generation: 0,
                preview_lines: Arc::from([]),
                preview_image_encoded: None,
                preview_bitmap: None,
                preview_top_line: 0,
                preview_loading: false,
                preview_truncated: false,
                preview_error: None,
                pending_native_preview: None,
                native_thumbnail: None,
                pending_high_surrogate: None,
                input_ready_at: Instant::now(),
                layout: app_layout,
            })
        }
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
                    self.results = update.results;
                    self.counters = update.counters;
                    self.selected_row = update.selected_row;
                }
                UiEvent::Preview(PreviewView::Text { update, top_line }) => {
                    match update {
                        PreviewUpdate::Clear { generation } => {
                            self.preview_generation = generation;
                            self.preview_lines = Arc::from([]);
                            self.preview_image_encoded = None;
                            self.preview_bitmap = None;
                            self.preview_loading = true;
                            self.preview_truncated = false;
                            self.preview_error = None;
                        }
                        PreviewUpdate::Ready {
                            generation,
                            lines,
                            truncated,
                            ..
                        } if generation == self.preview_generation => {
                            self.preview_lines = lines;
                            self.preview_image_encoded = None;
                            self.preview_bitmap = None;
                            self.preview_loading = false;
                            self.preview_truncated = truncated;
                        }
                        PreviewUpdate::ImageReady {
                            generation,
                            encoded,
                        } if generation == self.preview_generation => {
                            self.preview_loading = false;
                            self.preview_error = None;
                            self.preview_lines = Arc::from([]);
                            self.preview_image_encoded = Some(encoded);
                            self.preview_bitmap = None;
                        }
                        PreviewUpdate::Error {
                            generation,
                            message,
                        } if generation == self.preview_generation => {
                            self.preview_loading = false;
                            self.preview_error = Some(message);
                        }
                        _ => {}
                    }
                    self.preview_top_line = top_line;
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
                UiEvent::Hide => unsafe {
                    self.visible = false;
                    if let Some(thumbnail) = &mut self.native_thumbnail {
                        thumbnail.unregister();
                    }
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    self.queued_foreground_after_first_paint = false;
                    self.logged_first_items_paint = false;
                },
            }
        }
        unsafe {
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
                self.preview_enabled,
            );
            let preview_rows = self
                .layout
                .preview_box
                .map(|preview| (preview.height / self.layout.text_height).floor() as usize)
                .unwrap_or(0);
            self.view_model.set_preview_visible_rows(preview_rows);
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
        let _ = InvalidateRect(Some(hwnd), None, false);
        let _ = UpdateWindow(hwnd);

        bring_to_foreground(hwnd);
        let _ = SetFocus(Some(hwnd));

        let blink_ms = GetCaretBlinkTime();
        if SetTimer(Some(hwnd), WM_CURSOR_BLINK as usize, blink_ms, None) != 0 {}
        self.input_ready_at = Instant::now() + Duration::from_millis(150);
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
        self.highlight_brush = Some(target.CreateSolidColorBrush(&rgb(COLOR_MATCH), None)?);
        self.selected_brush = Some(target.CreateSolidColorBrush(&rgb(COLOR_SELECTED), None)?);
        self.border_brush = Some(target.CreateSolidColorBrush(&rgb(COLOR_BORDER), None)?);
        self.accent_brush = Some(target.CreateSolidColorBrush(&rgb(COLOR_SELECTED_ACCENT), None)?);
        self.style_brush = Some(target.CreateSolidColorBrush(&rgb(COLOR_TEXT), None)?);
        self.target = Some(target);
        Ok(())
    }

    fn discard_device_resources(&mut self) {
        self.preview_bitmap = None;
        self.style_brush = None;
        self.accent_brush = None;
        self.border_brush = None;
        self.selected_brush = None;
        self.highlight_brush = None;
        self.muted_brush = None;
        self.text_brush = None;
        self.target = None;
    }

    unsafe fn draw_preview(&self, target: &ID2D1HwndRenderTarget) {
        let (Some(border), Some(preview)) = (self.layout.preview_border, self.layout.preview_box)
        else {
            return;
        };
        draw_rounded_rectangle(
            target,
            self.border_brush.as_ref().expect("border brush"),
            border,
            8.0,
        );
        if let Some(bitmap) = &self.preview_bitmap {
            let size = bitmap.GetSize();
            if size.width > 0.0 && size.height > 0.0 {
                let scale = (preview.width / size.width).min(preview.height / size.height);
                let width = size.width * scale;
                let height = size.height * scale;
                let destination = D2D_RECT_F {
                    left: preview.x + (preview.width - width) / 2.0,
                    top: preview.y + (preview.height - height) / 2.0,
                    right: preview.x + (preview.width + width) / 2.0,
                    bottom: preview.y + (preview.height + height) / 2.0,
                };
                target.DrawBitmap(
                    bitmap,
                    Some(&destination),
                    1.0,
                    D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                    None,
                );
            }
            return;
        }
        if let Some(message) = self.preview_error.as_deref() {
            draw_text(
                target,
                self.highlight_brush.as_ref().expect("highlight brush"),
                &self.text_format,
                message,
                preview,
            );
            return;
        }
        if self.preview_loading && self.preview_lines.is_empty() {
            draw_text(
                target,
                self.muted_brush.as_ref().expect("muted brush"),
                &self.text_format,
                "Loading preview...",
                preview,
            );
            return;
        }

        let max_rows = (preview.height / self.layout.text_height).floor() as usize;
        let content_rows = if self.preview_truncated {
            max_rows.saturating_sub(1)
        } else {
            max_rows
        };
        for (row, line) in self
            .preview_lines
            .iter()
            .skip(self.preview_top_line)
            .take(content_rows)
            .enumerate()
        {
            let top = preview.y + row as f32 * self.layout.text_height;
            let mut x = preview.x;
            for span in &line.spans {
                let format = match (span.style.bold, span.style.italic) {
                    (true, true) => &self.bold_italic_text_format,
                    (true, false) => &self.bold_text_format,
                    (false, true) => &self.italic_text_format,
                    (false, false) => &self.text_format,
                };
                let width = measure_text_width(&self.dwrite_factory, format, &span.text);
                if x >= preview.x + preview.width {
                    break;
                }
                let (foreground, background) = resolved_preview_colors(&span.style);
                let brush = self.style_brush.as_ref().expect("style brush");
                if let Some(background) = background {
                    brush.SetColor(&rgb(background));
                    target.FillRectangle(
                        &D2D_RECT_F {
                            left: x,
                            top,
                            right: (x + width).min(preview.x + preview.width),
                            bottom: top + self.layout.text_height,
                        },
                        brush,
                    );
                }
                if !span.style.hidden {
                    brush.SetColor(&rgb(foreground));
                    draw_text(
                        target,
                        brush,
                        format,
                        &span.text,
                        Rect {
                            x,
                            y: top,
                            width: (preview.x + preview.width - x).max(0.0),
                            height: self.layout.text_height,
                        },
                    );
                    if span.style.underline || span.style.strikethrough {
                        let y = if span.style.strikethrough {
                            top + self.layout.text_height * 0.55
                        } else {
                            top + self.layout.text_height - 1.5
                        };
                        target.DrawLine(
                            Vector2 { X: x, Y: y },
                            Vector2 {
                                X: (x + width).min(preview.x + preview.width),
                                Y: y,
                            },
                            brush,
                            1.0,
                            None,
                        );
                    }
                }
                x += width;
            }
        }
        if self.preview_truncated {
            draw_text(
                target,
                self.muted_brush.as_ref().expect("muted brush"),
                &self.text_format,
                "… preview truncated at 4,000 lines or 1 MiB",
                Rect {
                    x: preview.x,
                    y: preview.y + content_rows as f32 * self.layout.text_height,
                    width: preview.width,
                    height: self.layout.text_height,
                },
            );
        }
    }

    unsafe fn ensure_preview_bitmap(&mut self) -> Result<()> {
        if self.preview_bitmap.is_some() || self.preview_image_encoded.is_none() {
            return Ok(());
        }
        let encoded = self.preview_image_encoded.as_ref().expect("image checked");
        let stream = self.wic_factory.CreateStream()?;
        stream.InitializeFromMemory(encoded)?;
        let decoder = self.wic_factory.CreateDecoderFromStream(
            &stream,
            std::ptr::null(),
            WICDecodeMetadataCacheOnLoad,
        )?;
        let frame = decoder.GetFrame(0)?;
        let converter = self.wic_factory.CreateFormatConverter()?;
        converter.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeMedianCut,
        )?;
        let target = self
            .target
            .as_ref()
            .context("render target not initialized")?;
        self.preview_bitmap = Some(target.CreateBitmapFromWicBitmap(&converter, None)?);
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
                if let Err(error) = thumbnail.update_layout(preview) {
                    self.preview_error = Some(format!("Window preview unavailable: {error}"));
                }
            }
        }
    }

    unsafe fn paint(&mut self, hwnd: HWND) -> Result<()> {
        self.ensure_target(hwnd)?;
        if let Err(error) = self.ensure_preview_bitmap() {
            self.preview_image_encoded = None;
            self.preview_error = Some(format!("image preview failed: {error}"));
        }
        let Some(target) = self.target.as_ref() else {
            return Ok(());
        };

        target.BeginDraw();
        target.Clear(Some(&rgb(COLOR_BACKGROUND)));
        self.draw_preview(target);

        let search_state = self.view_model.current_search_text();
        let search_string = search_state.text;
        let prompt = "> ";
        let prompt_width = measure_text_width(&self.dwrite_factory, &self.text_format, prompt);

        let radius = 8.0;

        draw_rounded_rectangle(
            target,
            self.border_brush.as_ref().expect("border brush"),
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
                left: self.layout.search_box.x
                    + prompt_width
                    + prefix_metrics.widthIncludingTrailingWhitespace,
                top: self.layout.search_box.y,
                right: self.layout.search_box.x
                    + prompt_width
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
            prompt,
            self.layout.search_box,
        );
        draw_text(
            target,
            self.text_brush.as_ref().expect("brush created"),
            &self.text_format,
            &search_string,
            Rect {
                x: self.layout.search_box.x + prompt_width,
                y: self.layout.search_box.y,
                width: self.layout.search_box.width - prompt_width - COUNTER_WIDTH,
                height: self.layout.search_box.height,
            },
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
                    X: self.layout.search_box.x + prompt_width + cursor_prefix_width + 1.5,
                    Y: self.layout.search_box.y,
                },
                Vector2 {
                    X: self.layout.search_box.x + prompt_width + cursor_prefix_width + 1.5,
                    Y: self.layout.search_box.y + self.layout.search_box.height,
                },
                self.text_brush.as_ref().expect("brush created"),
                1.0,
                None,
            );
        }

        let counter_text = if self.counters.scanning {
            format!(
                "{}/{} indexing…",
                self.counters.displayed, self.counters.published
            )
        } else {
            format!("{}/{}", self.counters.matched, self.counters.published)
        };
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
            self.border_brush.as_ref().expect("border brush"),
            self.layout.list_border,
            radius,
        );

        for visual_row in 0..DISPLAY_ROWS as usize {
            let result_index = DISPLAY_ROWS as usize - visual_row - 1;
            let result = self.results.get(result_index);
            let top = self.layout.list_box.y + visual_row as f32 * self.layout.row_size.height;
            let row_height = self.layout.row_size.height - RESULT_GAP;
            let text_top = top + RESULT_VERTICAL_PADDING;
            if result.is_some() && result_index == self.selected_row {
                let selected = Rect {
                    x: self.layout.list_box.x,
                    y: top,
                    width: self.layout.row_size.width,
                    height: row_height,
                };
                fill_rounded_rectangle(
                    target,
                    self.selected_brush.as_ref().expect("brush created"),
                    selected,
                    6.0,
                );
                fill_rounded_rectangle(
                    target,
                    self.accent_brush.as_ref().expect("accent brush"),
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
            let item_text_rect = Rect {
                x: self.layout.list_box.x + RESULT_HORIZONTAL_PADDING,
                y: text_top,
                width: (self.layout.list_box.width - RESULT_HORIZONTAL_PADDING * 2.0).max(0.0),
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
                self.layout.list_box.x + RESULT_HORIZONTAL_PADDING,
                text_top,
                self.layout.list_box.width,
                self.layout.text_height,
            );
        }

        if let Err(error) = target.EndDraw(None, None) {
            if error.code() == D2DERR_RECREATE_TARGET {
                self.discard_device_resources();
                let _ = InvalidateRect(Some(hwnd), None, false);
                return Ok(());
            }
            return Err(error.into());
        }
        self.sync_native_thumbnail(hwnd);

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

fn fallback_command(chord: KeyChord, preview_available: bool) -> Option<InputCommand> {
    match chord.key {
        KeyName::Enter => Some(InputCommand::Accept),
        KeyName::Escape => Some(InputCommand::Cancel),
        KeyName::Up => Some(InputCommand::MoveUp),
        KeyName::Down => Some(InputCommand::MoveDown),
        KeyName::Left => Some(InputCommand::MoveLeft),
        KeyName::Right => Some(InputCommand::MoveRight),
        KeyName::Home => Some(InputCommand::MoveHome),
        KeyName::End => Some(InputCommand::MoveEnd),
        KeyName::Backspace => Some(InputCommand::Backspace),
        KeyName::Delete => Some(InputCommand::Delete),
        KeyName::PageUp if chord.modifiers.ctrl => Some(InputCommand::PreviewPageUp),
        KeyName::PageDown if chord.modifiers.ctrl => Some(InputCommand::PreviewPageDown),
        KeyName::Character('p') if chord.modifiers.ctrl && preview_available => {
            Some(InputCommand::TogglePreview)
        }
        _ => None,
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
                    if !state.view_model.handle_key(chord, repeat) {
                        if let Some(command) = fallback_command(chord, state.preview_available) {
                            if !(repeat
                                && matches!(
                                    command,
                                    InputCommand::Accept
                                        | InputCommand::Cancel
                                        | InputCommand::TogglePreview
                                ))
                            {
                                state.view_model.handle_command(command, modifiers);
                            }
                        }
                    }
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

fn fill_rounded_rectangle(
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
        target.FillRoundedRectangle(&rounded_rect, brush);
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

fn measure_text_width(factory: &IDWriteFactory, format: &IDWriteTextFormat, text: &str) -> f32 {
    let text = wide(text);
    unsafe {
        let Ok(layout) = factory.CreateTextLayout(&text, format, f32::MAX, 1000.0) else {
            return 0.0;
        };
        let mut metrics = DWRITE_TEXT_METRICS::default();
        if layout.GetMetrics(&mut metrics).is_err() {
            return 0.0;
        }
        metrics.widthIncludingTrailingWhitespace
    }
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
