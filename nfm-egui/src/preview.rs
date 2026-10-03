use crate::{Appearance, Selection};
use crossbeam_channel::{Sender, bounded};
use egui::{Color32, Context};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

#[derive(Clone, Debug, PartialEq)]
pub struct PreviewCell {
    pub text: String,
    pub foreground: Color32,
    pub background: Color32,
    pub underline_color: Color32,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub faint: bool,
}
impl Default for PreviewCell {
    fn default() -> Self {
        Self {
            text: String::new(),
            foreground: Color32::WHITE,
            background: Color32::TRANSPARENT,
            underline_color: Color32::WHITE,
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            inverse: false,
            invisible: false,
            faint: false,
        }
    }
}

#[derive(Clone, Debug)]
pub enum Preview {
    Text(String),
    /// Styled spans per line; scrolling is handled by the control.
    StyledText(Vec<Vec<PreviewCell>>),
    Image(Arc<PreviewImage>),
    /// A viewport of styled terminal cells, already scrolled by the provider.
    Grid {
        columns: usize,
        cells: Vec<PreviewCell>,
        total_rows: usize,
    },
    Empty,
}

pub struct PreviewImage {
    pub image: egui::ColorImage,
    texture: std::sync::OnceLock<egui::TextureHandle>,
}
impl PreviewImage {
    pub fn new(image: egui::ColorImage) -> Self {
        Self {
            image,
            texture: std::sync::OnceLock::new(),
        }
    }
}
impl std::fmt::Debug for PreviewImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreviewImage")
            .field("size", &self.image.size)
            .finish()
    }
}

#[derive(Clone)]
pub struct PreviewRequest<T> {
    pub selection: Selection<T>,
    /// Cache version for preview contents; zero for append-only sources.
    /// The selection still retains its actual searched snapshot version.
    pub document_version: u64,
    /// No viewport has been presented for this selection yet.
    pub initial_page: bool,
    pub columns: u16,
    pub rows: u16,
    /// Row offset from the start of the document.
    pub scroll_offset: usize,
    pub foreground: Color32,
    pub background: Color32,
}

/// Retain this token while reading host snapshots or running a child process.
/// Providers must check it periodically and terminate their own child processes.
#[derive(Clone)]
pub struct Cancellation {
    revision: Arc<AtomicU64>,
    expected: u64,
    stopped: Arc<AtomicBool>,
}
impl Cancellation {
    #[cfg(test)]
    pub(crate) fn test() -> Self {
        Self {
            revision: Arc::new(AtomicU64::new(0)),
            expected: 0,
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }
    #[cfg(test)]
    pub(crate) fn cancel_for_test(&self) {
        self.stopped.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
            || self.revision.load(Ordering::Acquire) != self.expected
    }
}

/// A rendered document and the actual origin of its viewport.
pub struct PreviewViewport {
    pub document: Preview,
    /// Actual first row, after centering or clamping the requested page.
    pub first_row: usize,
}

/// Providers own data or closable leases. Closing cancels work without joining
/// the worker; a borrowed native pointer alone is not a valid lease.
pub trait PreviewProvider<T>: Send + Sync + 'static {
    fn preview(&self, request: PreviewRequest<T>, cancel: Cancellation) -> Result<Preview, String>;
    /// Immutable copy document matching the published item, including host snapshots.
    fn copy_document(
        &self,
        _version: u64,
        _index: usize,
    ) -> Result<Option<Arc<dyn crate::copy_document::CopyDocument>>, String> {
        Ok(None)
    }
    fn page_step(&self, rows: usize) -> usize {
        rows.saturating_sub(1).max(1)
    }
    fn preview_viewport(
        &self,
        request: PreviewRequest<T>,
        cancel: Cancellation,
    ) -> Result<PreviewViewport, String> {
        let first_row = request.scroll_offset;
        self.preview(request, cancel)
            .map(|document| PreviewViewport {
                document,
                first_row,
            })
    }
}
impl<T, F> PreviewProvider<T> for F
where
    F: Fn(PreviewRequest<T>, Cancellation) -> Result<Preview, String> + Send + Sync + 'static,
{
    fn preview(&self, request: PreviewRequest<T>, cancel: Cancellation) -> Result<Preview, String> {
        self(request, cancel)
    }
}

struct Job<T> {
    revision: u64,
    request: PreviewRequest<T>,
}
struct ResultUpdate {
    revision: u64,
    selection: (u64, usize),
    result: Result<Preview, String>,
    scroll_offset: usize,
}

pub(crate) struct PreviewWorker<T> {
    provider: Arc<dyn PreviewProvider<T>>,
    revision: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
    pending: Arc<Mutex<Option<Job<T>>>>,
    latest: Arc<Mutex<Option<ResultUpdate>>>,
    wake: Sender<()>,
    pub document: Option<Result<Preview, String>>,
    pub loading: bool,
    pub document_scroll_offset: usize,
    pub document_selection: Option<(u64, usize)>,
}
impl<T: Clone + Send + 'static> PreviewWorker<T> {
    pub fn new(provider: Arc<dyn PreviewProvider<T>>, ctx: Context) -> Self {
        let revision = Arc::new(AtomicU64::new(0));
        let stopped = Arc::new(AtomicBool::new(false));
        let pending = Arc::new(Mutex::new(None::<Job<T>>));
        let latest = Arc::new(Mutex::new(None::<ResultUpdate>));
        let (wake, rx) = bounded(1);
        let (worker_revision, worker_stopped, worker_pending, worker_latest) = (
            revision.clone(),
            stopped.clone(),
            pending.clone(),
            latest.clone(),
        );
        let worker_provider = provider.clone();
        std::thread::spawn(move || {
            while rx.recv().is_ok() {
                if worker_stopped.load(Ordering::Acquire) {
                    break;
                }
                let Some(job) = worker_pending.lock().unwrap().take() else {
                    continue;
                };
                let cancel = Cancellation {
                    revision: worker_revision.clone(),
                    expected: job.revision,
                    stopped: worker_stopped.clone(),
                };
                let selection = (job.request.document_version, job.request.selection.index);
                let scroll_offset = job.request.scroll_offset;
                let result = worker_provider.preview_viewport(job.request, cancel.clone());
                let scroll_offset = result.as_ref().map_or(scroll_offset, |page| page.first_row);
                let result = result.map(|page| page.document);
                if !cancel.is_cancelled() {
                    *worker_latest.lock().unwrap() = Some(ResultUpdate {
                        revision: job.revision,
                        selection,
                        result,
                        scroll_offset,
                    });
                    ctx.request_repaint();
                }
            }
        });
        Self {
            provider,
            revision,
            stopped,
            pending,
            latest,
            wake,
            document: None,
            loading: false,
            document_scroll_offset: 0,
            document_selection: None,
        }
    }
    pub fn restart_retaining_document(&self, ctx: Context) -> Self {
        self.stop();
        let mut next = Self::new(self.provider.clone(), ctx);
        next.document = self.document.clone();
        next.document_scroll_offset = self.document_scroll_offset;
        next
    }
    pub fn request(&mut self, request: PreviewRequest<T>) {
        let revision = self.revision.fetch_add(1, Ordering::AcqRel) + 1;
        *self.pending.lock().unwrap() = Some(Job { revision, request });
        self.loading = true;
        let _ = self.wake.try_send(());
    }
    pub fn clear(&mut self) {
        self.revision.fetch_add(1, Ordering::AcqRel);
        *self.pending.lock().unwrap() = None;
        self.document = None;
        self.document_selection = None;
        self.loading = false;
    }
    pub fn poll(&mut self) {
        if let Some(update) = self.latest.lock().unwrap().take()
            && update.revision == self.revision.load(Ordering::Acquire)
        {
            self.document_selection = Some(update.selection);
            self.document_scroll_offset = update.scroll_offset;
            self.document = Some(update.result);
            self.loading = false;
        }
    }
    pub fn copy_document(
        &self,
    ) -> Result<Option<Arc<dyn crate::copy_document::CopyDocument>>, String> {
        let Some((version, index)) = self.document_selection else {
            return Ok(None);
        };
        if self.loading {
            return Ok(None);
        }
        self.provider.copy_document(version, index)
    }
    pub fn page_step(&self, rows: usize) -> usize {
        self.provider.page_step(rows)
    }
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
    }
}
impl<T> Drop for PreviewWorker<T> {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.wake.try_send(());
    }
}

pub(crate) fn paint(
    ui: &mut egui::Ui,
    preview: &Preview,
    appearance: &Appearance,
    rows: usize,
    scroll_offset: usize,
) {
    let typography = &appearance.typography;
    let grid = if matches!(preview, Preview::Text(_) | Preview::StyledText(_)) {
        crate::preview_copy::PreviewCopyMode::text_grid(preview, scroll_offset, rows, appearance)
    } else {
        None
    };
    let preview = grid.as_ref().unwrap_or(preview);
    let row_height = typography.preview_row_height(ui.ctx());
    match preview {
        Preview::Empty => {}
        Preview::Image(image) => {
            let texture = image.texture.get_or_init(|| {
                ui.ctx().load_texture(
                    "nfm-preview",
                    image.image.clone(),
                    egui::TextureOptions::LINEAR,
                )
            });
            let available = egui::vec2(ui.available_width(), rows as f32 * row_height);
            let original = egui::vec2(image.image.size[0] as f32, image.image.size[1] as f32);
            let scale = (available.x / original.x)
                .min(available.y / original.y)
                .min(1.0);
            let (rect, _) = ui.allocate_exact_size(available, egui::Sense::hover());
            let target = egui::Rect::from_center_size(rect.center(), original * scale);
            ui.painter().image(
                texture.id(),
                target,
                egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                Color32::WHITE,
            );
        }
        Preview::StyledText(lines) => {
            for spans in lines.iter().skip(scroll_offset).take(rows) {
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), row_height),
                    egui::Sense::hover(),
                );
                let mut job = egui::text::LayoutJob::default();
                job.wrap.max_width = f32::INFINITY;
                for span in spans {
                    let (fg, bg) = if span.inverse {
                        (span.background, span.foreground)
                    } else {
                        (span.foreground, span.background)
                    };
                    let mut format = egui::TextFormat {
                        font_id: if span.bold {
                            typography.bold.clone()
                        } else {
                            typography.normal.clone()
                        },
                        color: if span.invisible {
                            bg
                        } else if span.faint {
                            fg.gamma_multiply(0.6)
                        } else {
                            fg
                        },
                        background: bg,
                        italics: span.italic,
                        ..Default::default()
                    };
                    if span.underline {
                        format.underline = egui::Stroke::new(1.0, span.underline_color);
                    }
                    if span.strikethrough {
                        format.strikethrough = egui::Stroke::new(1.0, fg);
                    }
                    job.append(&span.text, 0.0, format);
                }
                let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
                ui.painter()
                    .with_clip_rect(rect.intersect(ui.clip_rect()))
                    .galley(
                        egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0),
                        galley,
                        appearance.palette.text,
                    );
            }
        }
        Preview::Text(text) => {
            for line in text.lines().skip(scroll_offset).take(rows) {
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), row_height),
                    egui::Sense::hover(),
                );
                ui.painter()
                    .with_clip_rect(rect.intersect(ui.clip_rect()))
                    .text(
                        rect.left_center(),
                        egui::Align2::LEFT_CENTER,
                        line,
                        typography.normal.clone(),
                        appearance.palette.text,
                    );
            }
        }
        Preview::Grid { columns, cells, .. } => {
            if *columns == 0 {
                return;
            }
            for row in cells.chunks(*columns).take(rows) {
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), row_height),
                    egui::Sense::hover(),
                );
                let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
                for (x, cell) in row.iter().enumerate() {
                    let rect = egui::Rect::from_min_size(
                        rect.min + egui::vec2(x as f32 * typography.cell_width.max(1.0), 0.0),
                        egui::vec2(typography.cell_width.max(1.0), row_height),
                    );
                    painter.rect_filled(
                        rect,
                        0.0,
                        if cell.inverse {
                            cell.foreground
                        } else {
                            cell.background
                        },
                    );
                }
                for (x, cell) in row.iter().enumerate() {
                    let cell_rect = egui::Rect::from_min_size(
                        rect.min + egui::vec2(x as f32 * typography.cell_width.max(1.0), 0.0),
                        egui::vec2(typography.cell_width.max(1.0), row_height),
                    );
                    let (fg, _bg) = if cell.inverse {
                        (cell.background, cell.foreground)
                    } else {
                        (cell.foreground, cell.background)
                    };
                    if !cell.invisible {
                        let mut format = egui::TextFormat {
                            font_id: if cell.bold {
                                typography.bold.clone()
                            } else {
                                typography.normal.clone()
                            },
                            color: if cell.faint {
                                fg.gamma_multiply(0.6)
                            } else {
                                fg
                            },
                            italics: cell.italic,
                            ..Default::default()
                        };
                        // Decorations span cells below, including blank cells.
                        format.valign = egui::Align::Center;
                        let job = egui::text::LayoutJob::single_section(cell.text.clone(), format);
                        let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
                        let pos = egui::pos2(
                            cell_rect.left(),
                            cell_rect.center().y - galley.size().y / 2.0,
                        );
                        painter.galley(pos, galley, fg);
                        if cell.underline {
                            painter.line_segment(
                                [
                                    cell_rect.left_bottom() - egui::vec2(0.0, 2.0),
                                    cell_rect.right_bottom() - egui::vec2(0.0, 2.0),
                                ],
                                egui::Stroke::new(1.0, cell.underline_color),
                            );
                        }
                        if cell.strikethrough {
                            painter.line_segment(
                                [cell_rect.left_center(), cell_rect.right_center()],
                                egui::Stroke::new(1.0, fg),
                            );
                        }
                    }
                }
            }
        }
    }
}

/// ANSI parsing uses the standalone Ghostty VT library.
pub fn parse_ansi<T>(bytes: &[u8], request: &PreviewRequest<T>) -> Result<Preview, String> {
    let rgb = |c: Color32| ((c.r() as u32) << 16) | ((c.g() as u32) << 8) | c.b() as u32;
    let grid = nfm_preview_vt::parse(
        bytes,
        request.columns,
        request.rows,
        request.scroll_offset,
        rgb(request.foreground),
        rgb(request.background),
    )
    .map_err(|error| error.to_string())?;
    Ok(preview_from_grid(grid))
}
pub(crate) fn preview_from_grid(grid: nfm_preview_vt::Grid) -> Preview {
    let color =
        |value: u32| Color32::from_rgb((value >> 16) as u8, (value >> 8) as u8, value as u8);
    Preview::Grid {
        columns: grid.columns as usize,
        total_rows: grid.total_rows,
        cells: grid
            .cells
            .into_iter()
            .map(|cell| PreviewCell {
                text: cell.codepoints[..usize::from(cell.length).min(8)]
                    .iter()
                    .filter_map(|&c| char::from_u32(c))
                    .collect(),
                foreground: color(cell.foreground),
                background: color(cell.background),
                underline_color: color(cell.underline_color),
                bold: cell.flags & 1 != 0,
                italic: cell.flags & 2 != 0,
                strikethrough: cell.flags & 4 != 0,
                inverse: cell.flags & 8 != 0,
                invisible: cell.flags & 16 != 0,
                faint: cell.flags & 32 != 0,
                underline: cell.underline != 0,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    #[test]
    fn image_texture_is_uploaded_once_and_styled_text_scrolls() {
        let ctx = Context::default();
        let image = Arc::new(PreviewImage::new(egui::ColorImage::new(
            [64, 32],
            vec![Color32::RED; 64 * 32],
        )));
        let preview = Preview::Image(image.clone());
        let styled = Preview::StyledText(vec![
            vec![PreviewCell {
                text: "hidden first line".into(),
                ..Default::default()
            }],
            vec![PreviewCell {
                text: "visible red line".into(),
                foreground: Color32::RED,
                ..Default::default()
            }],
        ]);
        let frame = ctx.run_ui(egui::RawInput::default(), |ui| {
            paint(ui, &preview, &Appearance::default(), 5, 0);
            paint(ui, &styled, &Appearance::default(), 1, 1);
        });
        let id = image.texture.get().unwrap().id();
        assert!(
            frame
                .textures_delta
                .set
                .iter()
                .any(|(texture, _)| *texture == id)
        );
        let painted: String = frame
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text)
                    if text.galley.job.sections[0].format.color == Color32::RED =>
                {
                    Some(text.galley.text())
                }
                _ => None,
            })
            .collect();
        assert_eq!(painted.trim_end(), "visible red line");
        frame.drop_without_applying_deltas();
        let frame = ctx.run_ui(egui::RawInput::default(), |ui| {
            paint(ui, &preview, &Appearance::default(), 5, 0)
        });
        assert!(
            !frame
                .textures_delta
                .set
                .iter()
                .any(|(texture, _)| *texture == id)
        );
        frame.drop_without_applying_deltas();
    }
    fn request(index: usize) -> PreviewRequest<usize> {
        PreviewRequest {
            document_version: 1,
            initial_page: false,
            selection: Selection {
                item: index,
                text: index.to_string(),
                index,
                source_version: 1,
                snapshot: Arc::new(()),
            },
            columns: 40,
            rows: 12,
            scroll_offset: 0,
            foreground: Color32::WHITE,
            background: Color32::BLACK,
        }
    }
    #[test]
    fn superseded_preview_is_discarded_and_close_cancels_owned_work() {
        let (started, started_rx) = bounded(1);
        let (release, release_rx) = bounded(1);
        let provider = Arc::new(
            move |request: PreviewRequest<usize>, cancel: Cancellation| {
                if request.selection.index == 0 {
                    started.send(cancel).unwrap();
                    release_rx.recv().unwrap();
                }
                Ok(Preview::Text(request.selection.text))
            },
        );
        let mut worker = PreviewWorker::new(provider, Context::default());
        worker.request(request(0));
        let token = started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.request(request(1));
        assert!(token.is_cancelled());
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            worker.poll();
            if worker.document.is_some() {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(&worker.document, Some(Ok(Preview::Text(text))) if text == "1"));
        let (started, rx) = bounded(1);
        let provider = Arc::new(move |_: PreviewRequest<usize>, cancel: Cancellation| {
            started.send(cancel).unwrap();
            Ok(Preview::Empty)
        });
        let mut worker = PreviewWorker::new(provider, Context::default());
        worker.request(request(0));
        let token = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(worker);
        assert!(token.is_cancelled());
    }
}

#[cfg(test)]
mod retention_tests {
    use super::*;
    use std::time::{Duration, Instant};
    fn request(index: usize) -> PreviewRequest<usize> {
        PreviewRequest {
            document_version: 1,
            initial_page: false,
            selection: Selection {
                item: index,
                text: index.to_string(),
                index,
                source_version: 1,
                snapshot: Arc::new(()),
            },
            columns: 40,
            rows: 12,
            scroll_offset: index,
            foreground: Color32::WHITE,
            background: Color32::BLACK,
        }
    }
    #[test]
    fn preview_replacement_keeps_presented_document_and_scroll_until_ready() {
        let (started, rx) = bounded(1);
        let (release, gate) = bounded(1);
        let provider = Arc::new(move |request: PreviewRequest<usize>, _: Cancellation| {
            if request.selection.index == 1 {
                started.send(()).unwrap();
                gate.recv().unwrap();
            }
            Ok(Preview::Text(request.selection.text))
        });
        let mut worker = PreviewWorker::new(provider, Context::default());
        worker.request(request(0));
        let deadline = Instant::now() + Duration::from_secs(5);
        while worker.loading {
            worker.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        worker.request(request(1));
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(worker.loading);
        assert!(matches!(&worker.document, Some(Ok(Preview::Text(text))) if text == "0"));
        assert_eq!(worker.document_scroll_offset, 0);
        release.send(()).unwrap();
        while worker.loading {
            worker.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(&worker.document, Some(Ok(Preview::Text(text))) if text == "1"));
        assert_eq!(worker.document_scroll_offset, 1);
    }
}

/// Text represented by a preview, with ANSI styling removed. Images have no text.
#[cfg(test)]
pub(crate) fn plain_text(preview: &Preview) -> Option<String> {
    match preview {
        Preview::Text(text) => Some(text.clone()),
        Preview::StyledText(lines) => Some(
            lines
                .iter()
                .map(|line| {
                    line.iter()
                        .map(|cell| cell.text.as_str())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        Preview::Grid { columns, cells, .. } if *columns > 0 => Some(
            cells
                .chunks(*columns)
                .map(|line| {
                    line.iter()
                        .map(|cell| cell.text.as_str())
                        .collect::<String>()
                        .trim_end()
                        .to_owned()
                })
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}
