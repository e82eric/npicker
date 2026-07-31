use std::num::NonZeroU32;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::Receiver;
use skia_safe::{
    surfaces, Canvas, Color, Data, Font, FontMgr, FontStyle, Image, Paint, PaintStyle, RRect,
    Rect as SkRect, Surface,
};
use softbuffer::{Context as SoftContext, Surface as SoftSurface};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, Modifiers, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, NamedKey};
use winit::window::{Window, WindowAttributes, WindowId, WindowLevel};

use crate::key_binding::{KeyChord, KeyName};
use crate::preview::{NativeWindowId, PreviewLine, PreviewUpdate};
use crate::preview_document::PreviewStyle;
use crate::view_model::{InputCommand, KeyModifiers, PreviewView, UiCounters, UiEvent, ViewModel};
use nfm_search_core::search::SearchResult;

const DEFAULT_WIDTH: i32 = 1600;
const MIN_WIDTH: i32 = 480;
const MIN_HEIGHT: i32 = 240;
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
const DEFAULT_HEIGHT: i32 = 320;
const COLOR_BACKGROUND: u32 = 0x282828;
const COLOR_TEXT: u32 = 0xebdbb2;
const COLOR_BORDER: u32 = 0x928374;
const COLOR_MATCH: u32 = 0xfb4934;
const COLOR_SELECTED: u32 = 0x3c3836;
const COLOR_SELECTED_ACCENT: u32 = 0xb8bb26;
const DEFAULT_LOCATION_VALUE: i32 = i32::MIN;
#[cfg(windows)]
const THUMBNAIL_PADDING: f32 = 8.0;

static PREFERRED_CENTER_X: AtomicI32 = AtomicI32::new(DEFAULT_LOCATION_VALUE);
static PREFERRED_CENTER_Y: AtomicI32 = AtomicI32::new(DEFAULT_LOCATION_VALUE);
static DESIRED_WINDOW_HEIGHT: AtomicI32 = AtomicI32::new(DEFAULT_HEIGHT);

pub fn set_preferred_center(x: i32, y: i32) {
    PREFERRED_CENTER_X.store(x, Ordering::Relaxed);
    PREFERRED_CENTER_Y.store(y, Ordering::Relaxed);
}

#[derive(Debug)]
enum AppEvent {
    Ui(UiEvent),
    Exit(i32),
}

pub fn run(
    view_model: Arc<ViewModel>,
    completion: Option<Receiver<i32>>,
    preview_available: bool,
    preview_visible: bool,
) -> Result<i32> {
    let mut builder = EventLoop::<AppEvent>::with_user_event();
    #[cfg(windows)]
    {
        use winit::platform::windows::EventLoopBuilderExtWindows;
        builder.with_any_thread(true);
    }
    let event_loop = builder.build()?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let proxy = event_loop.create_proxy();
    forward_ui_events(view_model.subscribe(), proxy.clone());
    if let Some(completion) = completion {
        forward_completion(completion, proxy.clone());
    }

    let mut app = PickerApp::new(view_model, preview_available, preview_visible)?;
    event_loop.run_app(&mut app)?;
    Ok(app.exit_code)
}

fn forward_ui_events(events: Receiver<UiEvent>, proxy: EventLoopProxy<AppEvent>) {
    std::thread::spawn(move || {
        while let Ok(event) = events.recv() {
            if proxy.send_event(AppEvent::Ui(event)).is_err() {
                break;
            }
        }
    });
}

fn forward_completion(completion: Receiver<i32>, proxy: EventLoopProxy<AppEvent>) {
    std::thread::spawn(move || {
        if let Ok(code) = completion.recv() {
            let _ = proxy.send_event(AppEvent::Exit(code));
        }
    });
}

struct WindowState {
    view_model: Arc<ViewModel>,
    surface: Option<Surface>,
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
    results: Vec<SearchResult>,
    counters: UiCounters,
    selected_row: usize,
    cursor_visible: bool,
    preview_enabled: bool,
    preview_generation: u64,
    preview_lines: Arc<[PreviewLine]>,
    preview_image: Option<Image>,
    preview_top_line: usize,
    preview_loading: bool,
    preview_truncated: bool,
    preview_error: Option<String>,
    scale_factor: f64,
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
        width: f32,
        scale_factor: f64,
        preview_enabled: bool,
    ) -> Result<Self> {
        let font_manager = FontMgr::default();
        let typeface = font_manager
            .legacy_make_typeface("Cascadia Mono", FontStyle::normal())
            .or_else(|| font_manager.legacy_make_typeface(None, FontStyle::normal()))
            .context("failed to create Skia typeface")?;
        let mut font = Font::new(typeface.clone(), 15.0);
        font.set_subpixel(true);
        let mut bold_font = Font::new(
            font_manager
                .legacy_make_typeface("Cascadia Mono", FontStyle::bold())
                .unwrap_or_else(|| typeface.clone()),
            15.0,
        );
        bold_font.set_subpixel(true);
        let mut italic_font = Font::new(
            font_manager
                .legacy_make_typeface("Cascadia Mono", FontStyle::italic())
                .unwrap_or_else(|| typeface.clone()),
            15.0,
        );
        italic_font.set_subpixel(true);
        let mut bold_italic_font = Font::new(
            font_manager
                .legacy_make_typeface("Cascadia Mono", FontStyle::bold_italic())
                .unwrap_or_else(|| typeface.clone()),
            15.0,
        );
        bold_italic_font.set_subpixel(true);
        let counter_font = Font::new(typeface, 15.0);
        let text_height = font.metrics().0;

        let desired_height =
            desired_window_height(text_height, PADDING, DISPLAY_ROWS, preview_enabled);
        DESIRED_WINDOW_HEIGHT.store(desired_height, Ordering::Relaxed);

        let window = Rect {
            x: 0.0,
            y: 0.0,
            width,
            height: desired_height as f32,
        };

        let app_layout =
            calculate_layout(window, PADDING, DISPLAY_ROWS, text_height, preview_enabled);
        let preview_visible_rows = app_layout
            .preview_box
            .map(|preview| (preview.height / text_height).floor() as usize)
            .unwrap_or(0);
        view_model.set_preview_visible_rows(preview_visible_rows);

        Ok(Self {
            view_model,
            surface: None,
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
            cursor_visible: false,
            preview_enabled,
            preview_generation: 0,
            preview_lines: Arc::from([]),
            preview_image: None,
            preview_top_line: 0,
            preview_loading: false,
            preview_truncated: false,
            preview_error: None,
            scale_factor,
            layout: app_layout,
        })
    }

    fn apply_update(&mut self, update: crate::view_model::UiUpdate) {
        self.results = update.results;
        self.counters = update.counters;
        self.selected_row = update.selected_row;
    }

    fn begin_preview(&mut self, generation: u64) {
        self.preview_generation = generation;
        self.preview_lines = Arc::from([]);
        self.preview_image = None;
        self.preview_top_line = 0;
        self.preview_loading = true;
        self.preview_truncated = false;
        self.preview_error = None;
    }

    fn finish_preview(&mut self, generation: u64, lines: Arc<[PreviewLine]>, truncated: bool) {
        if generation == self.preview_generation {
            self.preview_lines = lines;
            self.preview_image = None;
            self.preview_top_line = 0;
            self.preview_loading = false;
            self.preview_truncated = truncated;
            self.preview_error = None;
        }
    }

    fn finish_image_preview(&mut self, generation: u64, encoded: Arc<[u8]>) {
        if generation != self.preview_generation {
            return;
        }
        self.preview_lines = Arc::from([]);
        self.preview_top_line = 0;
        self.preview_loading = false;
        self.preview_truncated = false;
        self.preview_image = Image::from_encoded(Data::new_copy(&encoded));
        self.preview_error = self
            .preview_image
            .is_none()
            .then(|| "preview produced an unsupported or invalid image".into());
    }

    fn fail_preview(&mut self, generation: u64, message: String) {
        if generation == self.preview_generation {
            self.preview_lines = Arc::from([]);
            self.preview_image = None;
            self.preview_top_line = 0;
            self.preview_loading = false;
            self.preview_truncated = false;
            self.preview_error = Some(message);
        }
    }

    #[cfg(windows)]
    fn set_native_preview_error(&mut self, message: String) {
        self.preview_loading = false;
        self.preview_lines = Arc::from([]);
        self.preview_image = None;
        self.preview_error = Some(message);
    }

    #[cfg(windows)]
    fn clear_native_preview_error(&mut self) {
        self.preview_error = None;
    }

    fn ensure_surface(&mut self, size: PhysicalSize<u32>, scale_factor: f64) -> Result<()> {
        let width = size.width.max(1) as i32;
        let height = size.height.max(1) as i32;
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
            self.scale_factor = scale_factor;
            let logical_width = width as f32 / scale_factor as f32;
            let logical_height = height as f32 / scale_factor as f32;
            self.layout = calculate_layout(
                Rect {
                    x: 0.0,
                    y: 0.0,
                    width: logical_width,
                    height: logical_height,
                },
                PADDING,
                DISPLAY_ROWS,
                self.layout.text_height,
                self.preview_enabled,
            );
            let preview_visible_rows = self
                .layout
                .preview_box
                .map(|preview| (preview.height / self.layout.text_height).floor() as usize)
                .unwrap_or(0);
            self.view_model
                .set_preview_visible_rows(preview_visible_rows);
        }
        Ok(())
    }

    fn paint(
        &mut self,
        size: PhysicalSize<u32>,
        scale_factor: f64,
        soft_surface: &mut SoftSurface<Arc<Window>, Arc<Window>>,
    ) -> Result<()> {
        self.ensure_surface(size, scale_factor)?;
        self.draw_to_surface()?;
        self.present(size, soft_surface)?;
        Ok(())
    }

    fn draw_to_surface(&mut self) -> Result<()> {
        let Some(surface) = self.surface.as_mut() else {
            return Ok(());
        };

        let canvas = surface.canvas();
        canvas.clear(skia_color(COLOR_BACKGROUND));
        let frame_save_count = canvas.save();
        canvas.reset_matrix();
        canvas.scale((self.scale_factor as f32, self.scale_factor as f32));

        let search_state = self.view_model.current_search_text();
        let search_string = search_state.text;
        let radius = 8.0;
        let counter_text = if self.counters.scanning {
            format!("… {}/{}", self.counters.matched, self.counters.published)
        } else {
            format!("{}/{}", self.counters.matched, self.counters.published)
        };
        let counter_width =
            measure_text_width(&self.counter_font, &self.muted_paint, &counter_text);
        let counter_rect = Rect {
            x: self.layout.search_box.x + self.layout.search_box.width - counter_width,
            y: self.layout.search_box.y,
            width: counter_width,
            height: self.layout.text_height,
        };
        let prompt_prefix = "> ";
        let prompt_prefix_width = measure_text_width(&self.font, &self.text_paint, prompt_prefix);
        let query_rect = Rect {
            x: self.layout.search_box.x + prompt_prefix_width,
            y: self.layout.search_box.y,
            width: (counter_rect.x
                - self.layout.search_box.x
                - prompt_prefix_width
                - PANEL_HORIZONTAL_PADDING)
                .max(0.0),
            height: self.layout.search_box.height,
        };

        if let (Some(preview_border), Some(preview_box)) =
            (self.layout.preview_border, self.layout.preview_box)
        {
            draw_rounded_rectangle(canvas, &self.stroke_paint, preview_border, radius);
            if let Some(image) = &self.preview_image {
                let scale = (preview_box.width / image.width() as f32)
                    .min(preview_box.height / image.height() as f32);
                let width = image.width() as f32 * scale;
                let height = image.height() as f32 * scale;
                let destination = SkRect::from_xywh(
                    preview_box.x + (preview_box.width - width) / 2.0,
                    preview_box.y + (preview_box.height - height) / 2.0,
                    width,
                    height,
                );
                canvas.draw_image_rect(image, None, destination, &self.text_paint);
            }
            let visible_rows = (preview_box.height / self.layout.text_height).floor() as usize;
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
                draw_preview_line(
                    canvas,
                    &self.font,
                    &self.bold_font,
                    &self.italic_font,
                    &self.bold_italic_font,
                    line,
                    Rect {
                        x: preview_box.x,
                        y: preview_box.y + row as f32 * self.layout.text_height,
                        width: preview_box.width,
                        height: self.layout.text_height,
                    },
                );
            }
            let status = if self.preview_loading {
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
                draw_text(
                    canvas,
                    &self.font,
                    paint,
                    text,
                    Rect {
                        x: preview_box.x,
                        y: preview_box.y + row as f32 * self.layout.text_height,
                        width: preview_box.width,
                        height: self.layout.text_height,
                    },
                    TextAlign::Left,
                );
            }
        }

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
                x: query_rect.x + prefix_width,
                y: query_rect.y,
                width: selected_width,
                height: query_rect.height,
            };
            draw_filled_rectangle(canvas, &self.selected_paint, rect);
        }

        draw_text(
            canvas,
            &self.font,
            &self.text_paint,
            prompt_prefix,
            self.layout.search_box,
            TextAlign::Left,
        );
        draw_text(
            canvas,
            &self.font,
            &self.text_paint,
            &search_string,
            query_rect,
            TextAlign::Left,
        );

        if self.cursor_visible {
            let search_up_to_cursor = &search_string[..search_state.cursor_position];
            let cursor_prefix_width =
                measure_text_width(&self.font, &self.text_paint, search_up_to_cursor);
            draw_text(
                canvas,
                &self.font,
                &self.text_paint,
                "_",
                Rect {
                    x: query_rect.x + cursor_prefix_width,
                    y: query_rect.y,
                    width: query_rect.width - cursor_prefix_width,
                    height: query_rect.height,
                },
                TextAlign::Left,
            );
        }

        draw_text(
            canvas,
            &self.counter_font,
            &self.muted_paint,
            &counter_text,
            counter_rect,
            TextAlign::Right,
        );

        draw_rounded_rectangle(canvas, &self.stroke_paint, self.layout.list_border, radius);

        for visual_row in 0..DISPLAY_ROWS as usize {
            let result_index = DISPLAY_ROWS as usize - visual_row - 1;
            let result = self.results.get(result_index);
            let top = self.layout.list_box.y + visual_row as f32 * self.layout.row_size.height;
            let row_height = self.layout.row_size.height - RESULT_GAP;
            let text_top = top + RESULT_VERTICAL_PADDING;
            if result.is_some() && result_index == self.selected_row {
                let rect = Rect {
                    x: self.layout.list_box.x,
                    y: top,
                    width: self.layout.row_size.width,
                    height: row_height,
                };
                draw_rounded_rectangle(canvas, &self.selected_paint, rect, 6.0);
                draw_rounded_rectangle(
                    canvas,
                    &self.selected_accent_paint,
                    Rect {
                        x: rect.x,
                        y: rect.y + RESULT_VERTICAL_PADDING,
                        width: SELECTED_ACCENT_WIDTH,
                        height: (rect.height - RESULT_VERTICAL_PADDING * 2.0).max(0.0),
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
                item_text_rect.x,
                text_top,
                item_text_rect.width,
                self.layout.text_height,
            );
        }

        canvas.restore_to_count(frame_save_count);
        Ok(())
    }

    fn present(
        &mut self,
        size: PhysicalSize<u32>,
        soft_surface: &mut SoftSurface<Arc<Window>, Arc<Window>>,
    ) -> Result<()> {
        let Some(surface) = self.surface.as_mut() else {
            return Ok(());
        };
        let Some(pixmap) = surface.peek_pixels() else {
            return Ok(());
        };
        let width = NonZeroU32::new(size.width.max(1)).expect("nonzero width");
        let height = NonZeroU32::new(size.height.max(1)).expect("nonzero height");
        soft_surface
            .resize(width, height)
            .map_err(|error| anyhow!("softbuffer resize failed: {error}"))?;
        let mut buffer = soft_surface
            .buffer_mut()
            .map_err(|error| anyhow!("softbuffer buffer acquisition failed: {error}"))?;
        let bytes = pixmap
            .bytes()
            .ok_or_else(|| anyhow!("Skia raster pixels are not readable"))?;
        let row_bytes = pixmap.row_bytes();
        let pixel_width = size.width as usize;
        copy_bgra_to_softbuffer(
            bytes,
            row_bytes,
            pixel_width,
            size.height as usize,
            &mut buffer,
        )?;
        buffer
            .present()
            .map_err(|error| anyhow!("softbuffer present failed: {error}"))?;
        Ok(())
    }
}

#[cfg(windows)]
unsafe extern "system" fn windows_popup_subclass(
    hwnd: windows::Win32::Foundation::HWND,
    message: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
    _subclass_id: usize,
    _reference_data: usize,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::Foundation::LRESULT;
    use windows::Win32::UI::Shell::DefSubclassProc;
    use windows::Win32::UI::WindowsAndMessaging::{WM_NCACTIVATE, WM_NCPAINT};

    match message {
        // This borderless popup has no non-client area. Windows can otherwise
        // briefly paint an active caption when the user clicks the window.
        WM_NCPAINT => LRESULT(0),
        WM_NCACTIVATE => LRESULT(1),
        _ => unsafe { DefSubclassProc(hwnd, message, wparam, lparam) },
    }
}

#[cfg(windows)]
fn configure_windows_popup(window: &Window, visible: bool) -> Result<()> {
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMNCRP_DISABLED, DWMWA_NCRENDERING_POLICY,
    };
    use windows::Win32::UI::Shell::SetWindowSubclass;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, SetWindowLongPtrW, SetWindowPos, GWL_EXSTYLE, GWL_STYLE,
        SET_WINDOW_POS_FLAGS, SWP_FRAMECHANGED, SWP_HIDEWINDOW, SWP_NOACTIVATE, SWP_NOMOVE,
        SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, WINDOW_EX_STYLE, WINDOW_STYLE, WS_CLIPCHILDREN,
        WS_CLIPSIBLINGS, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    };
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = window
        .window_handle()
        .context("failed to get the Win32 window handle")?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return Err(anyhow!("winit did not return a Win32 window handle"));
    };
    let hwnd = windows::Win32::Foundation::HWND(handle.hwnd.get() as *mut _);
    let style = WINDOW_STYLE(WS_POPUP.0 | WS_CLIPSIBLINGS.0 | WS_CLIPCHILDREN.0);
    let ex_style = WINDOW_EX_STYLE(WS_EX_TOPMOST.0 | WS_EX_TOOLWINDOW.0);
    let visibility = if visible {
        SWP_SHOWWINDOW
    } else {
        SWP_HIDEWINDOW
    };
    unsafe {
        SetWindowLongPtrW(hwnd, GWL_STYLE, style.0 as isize);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex_style.0 as isize);
        SetWindowSubclass(hwnd, Some(windows_popup_subclass), 1, 0)
            .ok()
            .context("failed to install the Win32 popup subclass")?;
        let non_client_policy = DWMNCRP_DISABLED;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_NCRENDERING_POLICY,
            &non_client_policy as *const _ as *const std::ffi::c_void,
            std::mem::size_of_val(&non_client_policy) as u32,
        )
        .context("failed to disable DWM non-client rendering")?;
        SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SET_WINDOW_POS_FLAGS(
                SWP_FRAMECHANGED.0
                    | SWP_NOMOVE.0
                    | SWP_NOSIZE.0
                    | SWP_NOZORDER.0
                    | SWP_NOACTIVATE.0
                    | visibility.0,
            ),
        )
        .context("failed to apply the Win32 popup style")?;
        let applied_style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let applied_ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if applied_style & WS_POPUP.0 == 0 || applied_ex_style & WS_EX_TOOLWINDOW.0 == 0 {
            return Err(anyhow!(
                "Win32 rejected popup styles (style={applied_style:#010x}, \
                 ex_style={applied_ex_style:#010x})"
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn focus_windows_popup(window: &Window) -> Result<()> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        keybd_event, SendInput, SetFocus, INPUT, INPUT_MOUSE, KEYEVENTF_KEYUP, VK_MENU,
    };
    use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = window
        .window_handle()
        .context("failed to get the Win32 window handle")?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return Err(anyhow!("winit did not return a Win32 window handle"));
    };
    let hwnd = windows::Win32::Foundation::HWND(handle.hwnd.get() as *mut _);

    unsafe {
        keybd_event(VK_MENU.0 as u8, 0, Default::default(), 0);
        keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
        SetForegroundWindow(hwnd)
            .ok()
            .context("SetForegroundWindow failed")?;
        SetFocus(Some(hwnd)).context("SetFocus failed")?;
        let input = INPUT {
            r#type: INPUT_MOUSE,
            ..Default::default()
        };
        SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
    }
    Ok(())
}

fn copy_bgra_to_softbuffer(
    source: &[u8],
    source_row_bytes: usize,
    width: usize,
    height: usize,
    target: &mut [u32],
) -> Result<()> {
    let required_source = source_row_bytes
        .checked_mul(height)
        .ok_or_else(|| anyhow!("Skia pixel dimensions overflow"))?;
    let required_target = width
        .checked_mul(height)
        .ok_or_else(|| anyhow!("softbuffer dimensions overflow"))?;
    if source.len() < required_source || target.len() < required_target {
        return Err(anyhow!(
            "pixel buffer is smaller than its declared dimensions"
        ));
    }
    for y in 0..height {
        let source = &source[y * source_row_bytes..y * source_row_bytes + width * 4];
        let target = &mut target[y * width..(y + 1) * width];
        for (pixel, bgra) in target.iter_mut().zip(source.chunks_exact(4)) {
            *pixel = u32::from(bgra[2]) << 16 | u32::from(bgra[1]) << 8 | u32::from(bgra[0]);
        }
    }
    Ok(())
}

#[cfg(windows)]
struct DwmThumbnailPreview {
    destination: windows::Win32::Foundation::HWND,
    source: Option<isize>,
    thumbnail: Option<isize>,
}

#[cfg(windows)]
impl DwmThumbnailPreview {
    fn new(window: &Window) -> Result<Self> {
        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

        let handle = window
            .window_handle()
            .context("failed to get thumbnail destination HWND")?;
        let RawWindowHandle::Win32(handle) = handle.as_raw() else {
            return Err(anyhow!("winit did not return a Win32 window handle"));
        };
        Ok(Self {
            destination: windows::Win32::Foundation::HWND(handle.hwnd.get() as *mut _),
            source: None,
            thumbnail: None,
        })
    }

    fn source(&self) -> Option<isize> {
        self.source
    }

    fn set_source(&mut self, source: Option<isize>) -> Result<()> {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::Graphics::Dwm::DwmRegisterThumbnail;
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;

        self.unregister();
        let Some(source) = source else {
            return Ok(());
        };
        let source_hwnd = HWND(source as *mut _);
        if !unsafe { IsWindow(Some(source_hwnd)) }.as_bool() {
            return Err(anyhow!("source window no longer exists"));
        }
        let thumbnail = unsafe { DwmRegisterThumbnail(self.destination, source_hwnd) }
            .context("DwmRegisterThumbnail failed")?;
        self.source = Some(source);
        self.thumbnail = Some(thumbnail);
        Ok(())
    }

    fn update_layout(&mut self, preview_box: Rect, scale_factor: f64) -> Result<()> {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Dwm::{
            DwmQueryThumbnailSourceSize, DwmUpdateThumbnailProperties, DWM_THUMBNAIL_PROPERTIES,
            DWM_TNP_RECTDESTINATION, DWM_TNP_SOURCECLIENTAREAONLY, DWM_TNP_VISIBLE,
        };

        let Some(thumbnail) = self.thumbnail else {
            return Ok(());
        };
        let scale = scale_factor as f32;
        let padding = THUMBNAIL_PADDING;
        let container = RECT {
            left: ((preview_box.x + padding) * scale).round() as i32,
            top: ((preview_box.y + padding) * scale).round() as i32,
            right: ((preview_box.x + preview_box.width - padding) * scale).round() as i32,
            bottom: ((preview_box.y + preview_box.height - padding) * scale).round() as i32,
        };
        let source_size = unsafe { DwmQueryThumbnailSourceSize(thumbnail) }
            .context("DwmQueryThumbnailSourceSize failed")?;
        let destination = fit_thumbnail_rect(container, source_size)
            .ok_or_else(|| anyhow!("thumbnail source or destination has no area"))?;
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

#[cfg(windows)]
impl Drop for DwmThumbnailPreview {
    fn drop(&mut self) {
        self.unregister();
    }
}

#[cfg(windows)]
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

struct PickerApp {
    view_model: Arc<ViewModel>,
    renderer: WindowState,
    window: Option<Arc<Window>>,
    soft_context: Option<SoftContext<Arc<Window>>>,
    soft_surface: Option<SoftSurface<Arc<Window>, Arc<Window>>>,
    modifiers: Modifiers,
    next_blink: Instant,
    input_ready_at: Instant,
    visible: bool,
    preview_available: bool,
    exit_code: i32,
    pending_native_preview: Option<NativeWindowId>,
    #[cfg(windows)]
    native_thumbnail: Option<DwmThumbnailPreview>,
}

impl PickerApp {
    fn new(
        view_model: Arc<ViewModel>,
        preview_available: bool,
        preview_visible: bool,
    ) -> Result<Self> {
        Ok(Self {
            renderer: WindowState::new(
                Arc::clone(&view_model),
                DEFAULT_WIDTH as f32,
                1.0,
                preview_visible,
            )?,
            view_model,
            window: None,
            soft_context: None,
            soft_surface: None,
            modifiers: Modifiers::default(),
            next_blink: Instant::now() + Duration::from_millis(530),
            input_ready_at: Instant::now(),
            visible: false,
            preview_available,
            exit_code: 0,
            pending_native_preview: None,
            #[cfg(windows)]
            native_thumbnail: None,
        })
    }

    fn create_window(&mut self, event_loop: &ActiveEventLoop) -> Result<()> {
        if self.window.is_some() {
            return Ok(());
        }
        let height = DESIRED_WINDOW_HEIGHT
            .load(Ordering::Relaxed)
            .max(MIN_HEIGHT);
        let attributes = WindowAttributes::default()
            .with_title("")
            .with_visible(false)
            .with_decorations(false)
            .with_resizable(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_inner_size(LogicalSize::new(DEFAULT_WIDTH as f64, height as f64));
        #[cfg(windows)]
        let attributes = {
            use winit::platform::windows::WindowAttributesExtWindows;
            attributes.with_skip_taskbar(true)
        };
        let window = Arc::new(event_loop.create_window(attributes)?);
        #[cfg(windows)]
        configure_windows_popup(&window, false)?;
        window.set_ime_allowed(true);
        let context = SoftContext::new(Arc::clone(&window))
            .map_err(|error| anyhow!("softbuffer context creation failed: {error}"))?;
        let surface = SoftSurface::new(&context, Arc::clone(&window))
            .map_err(|error| anyhow!("softbuffer surface creation failed: {error}"))?;
        self.renderer.scale_factor = window.scale_factor();
        self.soft_surface = Some(surface);
        self.soft_context = Some(context);
        self.window = Some(window);
        #[cfg(windows)]
        {
            self.native_thumbnail = Some(DwmThumbnailPreview::new(
                self.window.as_ref().expect("window was just assigned"),
            )?);
            self.sync_native_thumbnail();
        }
        if self.visible {
            self.show();
        }
        Ok(())
    }

    fn show(&mut self) {
        self.visible = true;
        self.center_window();
        if let Some(window) = &self.window {
            #[cfg(windows)]
            if let Err(error) = configure_windows_popup(window, true) {
                eprintln!("failed to enforce Win32 popup styles: {error:#}");
            }
            #[cfg(windows)]
            if let Err(error) = focus_windows_popup(window) {
                eprintln!("failed to focus Win32 popup: {error:#}");
            }
            #[cfg(not(windows))]
            {
                window.set_visible(true);
                window.focus_window();
            }
            window.request_redraw();
        }
        #[cfg(windows)]
        self.sync_native_thumbnail();
        self.next_blink = Instant::now() + Duration::from_millis(530);
        self.input_ready_at = Instant::now() + Duration::from_millis(150);
    }

    fn hide(&mut self) {
        self.visible = false;
        if let Some(window) = &self.window {
            #[cfg(windows)]
            if let Err(error) = configure_windows_popup(window, false) {
                eprintln!("failed to hide Win32 popup: {error:#}");
            }
            #[cfg(not(windows))]
            window.set_visible(false);
        }
        #[cfg(windows)]
        if let Some(thumbnail) = &mut self.native_thumbnail {
            thumbnail.unregister();
        }
    }

    fn center_window(&self) {
        let Some(window) = &self.window else {
            return;
        };
        let monitor = preferred_monitor(window).or_else(|| window.current_monitor());
        let Some(monitor) = monitor else {
            return;
        };
        let scale = monitor.scale_factor();
        let monitor_size = monitor.size();
        let width = ((DEFAULT_WIDTH as f64 * scale) as u32)
            .min(
                monitor_size
                    .width
                    .saturating_sub(MONITOR_HORIZONTAL_MARGIN as u32),
            )
            .max((MIN_WIDTH as f64 * scale) as u32);
        let desired_height = DESIRED_WINDOW_HEIGHT.load(Ordering::Relaxed) as f64;
        let height = ((desired_height * scale) as u32)
            .min(
                monitor_size
                    .height
                    .saturating_sub(MONITOR_VERTICAL_MARGIN as u32),
            )
            .max((MIN_HEIGHT as f64 * scale) as u32);
        let _ = window.request_inner_size(PhysicalSize::new(width, height));
        let position = monitor.position();
        let preferred_x = PREFERRED_CENTER_X.load(Ordering::Relaxed);
        let preferred_y = PREFERRED_CENTER_Y.load(Ordering::Relaxed);
        let center_x = if preferred_x == DEFAULT_LOCATION_VALUE {
            position.x + monitor_size.width as i32 / 2
        } else {
            preferred_x
        };
        let center_y = if preferred_y == DEFAULT_LOCATION_VALUE {
            position.y + monitor_size.height as i32 / 2
        } else {
            preferred_y
        };
        let max_x = position.x + monitor_size.width.saturating_sub(width) as i32;
        let max_y = position.y + monitor_size.height.saturating_sub(height) as i32;
        let x = (center_x - width as i32 / 2).clamp(position.x, max_x.max(position.x));
        let y = (center_y - height as i32 / 2).clamp(position.y, max_y.max(position.y));
        window.set_outer_position(PhysicalPosition::new(x, y));
    }

    fn set_preview_visibility(&mut self, visible: bool) {
        if !self.preview_available || self.renderer.preview_enabled == visible {
            return;
        }
        self.renderer.preview_enabled = visible;
        let desired_height = desired_window_height(
            self.renderer.layout.text_height,
            PADDING,
            DISPLAY_ROWS,
            visible,
        );
        DESIRED_WINDOW_HEIGHT.store(desired_height, Ordering::Relaxed);
        self.renderer.surface = None;
        self.center_window();
        #[cfg(windows)]
        self.sync_native_thumbnail();
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn handle_keyboard(&mut self, event: winit::event::KeyEvent) {
        if event.state != ElementState::Pressed {
            return;
        }
        if Instant::now() < self.input_ready_at {
            return;
        }
        let modifiers = key_modifiers(self.modifiers);
        if let Some(chord) = event_key_chord(&event.logical_key, modifiers) {
            if self.view_model.handle_key(chord, event.repeat) {
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
                return;
            }
        }
        let command = match &event.logical_key {
            Key::Named(NamedKey::Enter) => Some(InputCommand::Accept),
            Key::Named(NamedKey::Escape) => Some(InputCommand::Cancel),
            Key::Named(NamedKey::ArrowUp) => Some(InputCommand::MoveUp),
            Key::Named(NamedKey::ArrowDown) => Some(InputCommand::MoveDown),
            Key::Named(NamedKey::ArrowLeft) => Some(InputCommand::MoveLeft),
            Key::Named(NamedKey::ArrowRight) => Some(InputCommand::MoveRight),
            Key::Named(NamedKey::Home) => Some(InputCommand::MoveHome),
            Key::Named(NamedKey::End) => Some(InputCommand::MoveEnd),
            Key::Named(NamedKey::Backspace) => Some(InputCommand::Backspace),
            Key::Named(NamedKey::Delete) => Some(InputCommand::Delete),
            Key::Named(NamedKey::PageUp) if modifiers.ctrl => Some(InputCommand::PreviewPageUp),
            Key::Named(NamedKey::PageDown) if modifiers.ctrl => Some(InputCommand::PreviewPageDown),
            Key::Character(value)
                if modifiers.ctrl && self.preview_available && value.eq_ignore_ascii_case("p") =>
            {
                Some(InputCommand::TogglePreview)
            }
            _ => None,
        };
        if let Some(command) = command {
            if event.repeat
                && matches!(
                    command,
                    InputCommand::Accept | InputCommand::Cancel | InputCommand::TogglePreview
                )
            {
                return;
            }
            self.view_model.handle_command(command, modifiers);
        } else if let Some(text) = event.text {
            self.view_model.insert_text(&text);
        }
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    #[cfg(windows)]
    fn sync_native_thumbnail(&mut self) {
        let Some(thumbnail) = &mut self.native_thumbnail else {
            return;
        };
        if !self.visible || !self.renderer.preview_enabled {
            thumbnail.unregister();
            return;
        }
        let source = self.pending_native_preview.map(|window| window.0);
        if thumbnail.source() != source {
            if let Err(error) = thumbnail.set_source(source) {
                self.renderer
                    .set_native_preview_error(format!("Window preview unavailable: {error}"));
                return;
            }
            self.renderer.clear_native_preview_error();
        }
        if let Some(preview_box) = self.renderer.layout.preview_box {
            if let Err(error) = thumbnail.update_layout(preview_box, self.renderer.scale_factor) {
                self.renderer
                    .set_native_preview_error(format!("Window preview unavailable: {error}"));
            }
        }
    }
}

fn event_key_chord(key: &Key, modifiers: KeyModifiers) -> Option<KeyChord> {
    let key = match key {
        Key::Character(value) if value.chars().count() == 1 => {
            KeyName::Character(value.chars().next()?.to_ascii_lowercase())
        }
        Key::Named(key) => named_key_name(*key)?,
        _ => return None,
    };
    Some(KeyChord { key, modifiers })
}

fn named_key_name(key: NamedKey) -> Option<KeyName> {
    match key {
        NamedKey::Enter => Some(KeyName::Enter),
        NamedKey::Escape => Some(KeyName::Escape),
        NamedKey::ArrowUp => Some(KeyName::Up),
        NamedKey::ArrowDown => Some(KeyName::Down),
        NamedKey::ArrowLeft => Some(KeyName::Left),
        NamedKey::ArrowRight => Some(KeyName::Right),
        NamedKey::Home => Some(KeyName::Home),
        NamedKey::End => Some(KeyName::End),
        NamedKey::Backspace => Some(KeyName::Backspace),
        NamedKey::Delete => Some(KeyName::Delete),
        NamedKey::PageUp => Some(KeyName::PageUp),
        NamedKey::PageDown => Some(KeyName::PageDown),
        _ => None,
    }
}

impl ApplicationHandler<AppEvent> for PickerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if let Err(error) = self.create_window(event_loop) {
            eprintln!("failed to create picker window: {error:#}");
            self.exit_code = 1;
            event_loop.exit();
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::Ui(UiEvent::Show) => self.show(),
            AppEvent::Ui(UiEvent::Results(update)) => {
                self.renderer.apply_update(update);
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            AppEvent::Ui(UiEvent::Preview(PreviewView::Text { update, top_line })) => {
                match update {
                    PreviewUpdate::Clear { generation } => self.renderer.begin_preview(generation),
                    PreviewUpdate::Ready {
                        generation,
                        lines,
                        truncated,
                        ..
                    } => self.renderer.finish_preview(generation, lines, truncated),
                    PreviewUpdate::ImageReady {
                        generation,
                        encoded,
                    } => self.renderer.finish_image_preview(generation, encoded),
                    PreviewUpdate::Error {
                        generation,
                        message,
                    } => self.renderer.fail_preview(generation, message),
                }
                self.renderer.preview_top_line = top_line;
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            AppEvent::Ui(UiEvent::Preview(PreviewView::NativeWindow(source))) => {
                self.pending_native_preview = source;
                #[cfg(windows)]
                self.sync_native_thumbnail();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            AppEvent::Ui(UiEvent::PreviewVisibilityChanged { visible }) => {
                self.set_preview_visibility(visible);
            }
            AppEvent::Ui(UiEvent::Close) => self.hide(),
            AppEvent::Exit(code) => {
                self.exit_code = code;
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        window_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(window) = self.window.as_ref().map(Arc::clone) else {
            return;
        };
        if window.id() != window_id {
            return;
        }
        match event {
            WindowEvent::RedrawRequested => {
                if let Some(surface) = self.soft_surface.as_mut() {
                    if let Err(error) =
                        self.renderer
                            .paint(window.inner_size(), window.scale_factor(), surface)
                    {
                        eprintln!("picker render failed: {error:#}");
                    }
                    #[cfg(windows)]
                    self.sync_native_thumbnail();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => self.handle_keyboard(event),
            WindowEvent::Ime(Ime::Commit(text)) => {
                self.view_model.insert_text(&text);
                window.request_redraw();
            }
            WindowEvent::ModifiersChanged(modifiers) => self.modifiers = modifiers,
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                self.renderer.surface = None;
                window.request_redraw();
            }
            WindowEvent::CloseRequested => self.view_model.cancel(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if !self.visible {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }
        let now = Instant::now();
        if now >= self.next_blink {
            self.renderer.cursor_visible = !self.renderer.cursor_visible;
            self.next_blink = now + Duration::from_millis(530);
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(self.next_blink));
    }
}

fn key_modifiers(modifiers: Modifiers) -> KeyModifiers {
    let state = modifiers.state();
    KeyModifiers {
        ctrl: state.control_key(),
        shift: state.shift_key(),
        alt: state.alt_key(),
    }
}

fn preferred_monitor(window: &Window) -> Option<winit::monitor::MonitorHandle> {
    let x = PREFERRED_CENTER_X.load(Ordering::Relaxed);
    let y = PREFERRED_CENTER_Y.load(Ordering::Relaxed);
    if x == DEFAULT_LOCATION_VALUE || y == DEFAULT_LOCATION_VALUE {
        return window.current_monitor();
    }
    window.available_monitors().min_by_key(|monitor| {
        let position = monitor.position();
        let size = monitor.size();
        let max_x = position.x + size.width as i32;
        let max_y = position.y + size.height as i32;
        let dx = if x < position.x {
            position.x - x
        } else if x > max_x {
            x - max_x
        } else {
            0
        };
        let dy = if y < position.y {
            position.y - y
        } else if y > max_y {
            y - max_y
        } else {
            0
        };
        i64::from(dx) * i64::from(dx) + i64::from(dy) * i64::from(dy)
    })
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

fn draw_preview_line(
    canvas: &Canvas,
    normal_font: &Font,
    bold_font: &Font,
    italic_font: &Font,
    bold_italic_font: &Font,
    line: &PreviewLine,
    rect: Rect,
) {
    let mut x = rect.x;
    let right = rect.x + rect.width;
    for span in &line.spans {
        if x >= right {
            break;
        }
        let font = match (span.style.bold, span.style.italic) {
            (true, true) => bold_italic_font,
            (true, false) => bold_font,
            (false, true) => italic_font,
            (false, false) => normal_font,
        };
        let (foreground, background) = resolved_preview_colors(&span.style);
        let text_paint = fill_paint(foreground);
        let width = measure_text_width(font, &text_paint, &span.text);
        let visible_width = width.min((right - x).max(0.0));
        if visible_width <= 0.0 {
            break;
        }
        let span_rect = Rect {
            x,
            y: rect.y,
            width: visible_width,
            height: rect.height,
        };
        if let Some(background) = background {
            draw_filled_rectangle(canvas, &fill_paint(background), span_rect);
        }
        if !span.style.hidden {
            draw_text(
                canvas,
                font,
                &text_paint,
                &span.text,
                span_rect,
                TextAlign::Left,
            );
            let decoration_height = 1.0;
            if span.style.underline {
                draw_filled_rectangle(
                    canvas,
                    &text_paint,
                    Rect {
                        x,
                        y: rect.y + rect.height - 2.0,
                        width: visible_width,
                        height: decoration_height,
                    },
                );
            }
            if span.style.strikethrough {
                draw_filled_rectangle(
                    canvas,
                    &text_paint,
                    Rect {
                        x,
                        y: rect.y + rect.height * 0.55,
                        width: visible_width,
                        height: decoration_height,
                    },
                );
            }
        }
        x += width;
    }
}

fn resolved_preview_colors(style: &PreviewStyle) -> (u32, Option<u32>) {
    let mut foreground = style.foreground.map(ansi_color_value).unwrap_or(COLOR_TEXT);
    let mut background = style.background.map(ansi_color_value);
    if style.inverse {
        let inverse_foreground = background.unwrap_or(COLOR_BACKGROUND);
        background = Some(foreground);
        foreground = inverse_foreground;
    }
    if style.dim {
        foreground = dim_color(foreground);
    }
    (foreground, background)
}

fn ansi_color_value(color: crate::preview_document::AnsiColor) -> u32 {
    let (red, green, blue) = color.rgb();
    u32::from(red) << 16 | u32::from(green) << 8 | u32::from(blue)
}

fn dim_color(color: u32) -> u32 {
    let dim = |component: u32| component * 3 / 5;
    dim((color >> 16) & 0xff) << 16 | dim((color >> 8) & 0xff) << 8 | dim(color & 0xff)
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

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::fit_thumbnail_rect;
    use super::{calculate_layout, copy_bgra_to_softbuffer, Rect};

    #[test]
    fn pixel_copy_converts_bgra_and_honors_row_stride() {
        let source = [
            0x33, 0x22, 0x11, 0xff, 0x66, 0x55, 0x44, 0xff, 0xaa, 0xaa, 0xaa, 0xaa, 0x99, 0x88,
            0x77, 0xff, 0xcc, 0xbb, 0xaa, 0xff, 0xbb, 0xbb, 0xbb, 0xbb,
        ];
        let mut target = [0; 4];
        copy_bgra_to_softbuffer(&source, 12, 2, 2, &mut target).unwrap();
        assert_eq!(target, [0x112233, 0x445566, 0x778899, 0xaabbcc]);
    }

    #[test]
    fn preview_layout_places_preview_above_results() {
        let layout = calculate_layout(
            Rect {
                x: 0.0,
                y: 0.0,
                width: 1200.0,
                height: 700.0,
            },
            8.0,
            7,
            18.0,
            true,
        );
        let preview = layout.preview_border.expect("preview border");
        assert!(preview.y + preview.height < layout.list_border.y);
        assert!(layout.list_border.y + layout.list_border.height < layout.search_border.y);
        assert!(layout.search_border.y + layout.search_border.height <= layout.window.height);
    }

    #[cfg(windows)]
    #[test]
    fn thumbnail_fit_preserves_aspect_ratio_and_centers() {
        use windows::Win32::Foundation::{RECT, SIZE};

        let wide = fit_thumbnail_rect(
            RECT {
                left: 10,
                top: 20,
                right: 410,
                bottom: 320,
            },
            SIZE { cx: 1600, cy: 900 },
        )
        .unwrap();
        assert_eq!(wide.right - wide.left, 400);
        assert_eq!(wide.bottom - wide.top, 225);
        assert_eq!(wide.top, 57);

        let tall = fit_thumbnail_rect(
            RECT {
                left: 0,
                top: 0,
                right: 400,
                bottom: 300,
            },
            SIZE { cx: 600, cy: 1200 },
        )
        .unwrap();
        assert_eq!(tall.right - tall.left, 150);
        assert_eq!(tall.bottom - tall.top, 300);
        assert_eq!(tall.left, 125);
    }

    #[cfg(windows)]
    #[test]
    fn thumbnail_fit_rejects_empty_source_or_destination() {
        use windows::Win32::Foundation::{RECT, SIZE};

        assert!(fit_thumbnail_rect(
            RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 100,
            },
            SIZE { cx: 100, cy: 100 },
        )
        .is_none());
        assert!(fit_thumbnail_rect(
            RECT {
                left: 0,
                top: 0,
                right: 100,
                bottom: 100,
            },
            SIZE { cx: 0, cy: 100 },
        )
        .is_none());
    }
}
