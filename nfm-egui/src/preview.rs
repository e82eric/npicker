use crate::styled_document::{DocumentLayout, StyledDocument};
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
    /// Plain text. The preview worker lays it out as a `StyledDocument` that
    /// pages, copies and numbers its lines like any other. Lines scroll
    /// sideways rather than wrap, up to 4096 cells wide.
    Text(String),
    Image(Arc<PreviewImage>),
    /// A viewport of styled terminal cells, already scrolled by the provider.
    Grid {
        columns: usize,
        cells: Vec<PreviewCell>,
        total_rows: usize,
        /// The rows' logical lines, for the line-number gutter. None leaves the
        /// page unnumbered, as for pages cut from a larger host snapshot.
        lines: Option<GridLines>,
    },
    Empty,
}

/// Logical lines of a grid page's rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GridLines {
    /// One entry per page row: the zero-based logical line that row starts,
    /// or None for a soft-wrapped continuation row.
    pub starts: Vec<Option<u64>>,
    /// Logical lines in the whole document; sizes the gutter.
    pub total: u64,
}
impl GridLines {
    /// Lines of a page of `rows` rows from `first`, in a document of `total`
    /// rows that never wrap.
    pub fn unwrapped(first: usize, rows: usize, total: usize) -> Self {
        let end = first.saturating_add(rows).min(total).max(first);
        Self {
            starts: (first..end).map(|row| Some(row as u64)).collect(),
            total: total as u64,
        }
    }
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
    /// Wrap long text lines at `columns` rather than scroll them sideways.
    /// `Preview::Text` follows it; text providers that page their own grid
    /// should too. Terminal output wraps at `columns` regardless.
    pub wrap_text: bool,
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
    /// A token that is never cancelled, for synchronous helpers.
    pub fn never() -> Self {
        Self {
            revision: Arc::new(AtomicU64::new(0)),
            expected: 0,
            stopped: Arc::new(AtomicBool::new(false)),
        }
    }
    #[cfg(test)]
    pub(crate) fn test() -> Self {
        Self::never()
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
    text: Option<Arc<DocumentLayout>>,
}

/// Widest `Preview::Text` line shown without wrapping when wrapping is off;
/// longer lines wrap.
pub(crate) const MAX_TEXT_COLUMNS: usize = 4096;

/// Run the provider, turning text into a page of a text layout. Returns the
/// page, its first row and, for text, the layout behind it. Text wraps at the
/// request's columns when it asks to; otherwise it scrolls sideways.
fn render<T: 'static>(
    provider: &dyn PreviewProvider<T>,
    request: PreviewRequest<T>,
    cancel: Cancellation,
) -> (Result<Preview, String>, usize, Option<Arc<DocumentLayout>>) {
    let rows = usize::from(request.rows);
    let max_columns = if request.wrap_text {
        usize::from(request.columns).clamp(1, MAX_TEXT_COLUMNS)
    } else {
        MAX_TEXT_COLUMNS
    };
    let (foreground, background) = (request.foreground, request.background);
    let scroll_offset = request.scroll_offset;
    let text = match provider.preview_viewport(request, cancel.clone()) {
        Ok(PreviewViewport {
            document: Preview::Text(text),
            ..
        }) => text,
        Ok(page) => return (Ok(page.document), page.first_row, None),
        Err(error) => return (Err(error), scroll_offset, None),
    };
    let layout = match StyledDocument::from_text(&text).text_layout(max_columns, &cancel) {
        Ok(layout) => layout,
        Err(error) => return (Err(error), scroll_offset, None),
    };
    let first = layout.first_row(scroll_offset, rows);
    (
        Ok(layout.grid(first, rows, foreground, background, None)),
        first,
        Some(layout),
    )
}

pub(crate) struct PreviewWorker<T> {
    provider: Arc<dyn PreviewProvider<T>>,
    revision: Arc<AtomicU64>,
    stopped: Arc<AtomicBool>,
    pending: Arc<Mutex<Option<Job<T>>>>,
    latest: Arc<Mutex<Option<ResultUpdate>>>,
    wake: Sender<()>,
    /// The presented page; text results arrive as pages of `text`.
    pub document: Option<Result<Preview, String>>,
    /// The layout behind a text result's pages.
    text: Option<Arc<DocumentLayout>>,
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
                let (result, scroll_offset, text) =
                    render(&*worker_provider, job.request, cancel.clone());
                if !cancel.is_cancelled() {
                    *worker_latest.lock().unwrap() = Some(ResultUpdate {
                        revision: job.revision,
                        selection,
                        result,
                        scroll_offset,
                        text,
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
            text: None,
            loading: false,
            document_scroll_offset: 0,
            document_selection: None,
        }
    }
    pub fn restart_retaining_document(&self, ctx: Context) -> Self {
        self.stop();
        let mut next = Self::new(self.provider.clone(), ctx);
        next.document = self.document.clone();
        next.text = self.text.clone();
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
        self.text = None;
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
            self.text = update.text;
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
        if let Some(text) = &self.text {
            return Ok(Some(text.clone()));
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

/// Paint a page. Text never reaches here: the worker pages it as a grid.
pub(crate) fn paint(ui: &mut egui::Ui, preview: &Preview, appearance: &Appearance, rows: usize) {
    let typography = &appearance.typography;
    let row_height = typography.preview_row_height(ui.ctx());
    match preview {
        Preview::Empty | Preview::Text(_) => {}
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

/// Input bound for `parse_ansi`; byte previews configure their own.
pub const MAX_ANSI_BYTES: usize = 20 * 1024 * 1024;
/// Retained line bound for `parse_ansi`.
pub const MAX_ANSI_LINES: usize = 8192;

/// Parse ANSI output (pure Rust; see `ansi.rs`) and render the
/// request's viewport, wrapping long lines at `request.columns`.
pub fn parse_ansi<T>(bytes: &[u8], request: &PreviewRequest<T>) -> Result<Preview, String> {
    if request.columns == 0 || request.rows == 0 {
        return Err("Invalid preview dimensions".into());
    }
    let rows = usize::from(request.rows);
    let document = crate::styled_document::StyledDocument::from_ansi(
        bytes,
        MAX_ANSI_BYTES,
        MAX_ANSI_LINES + rows,
    )?;
    let layout = document.layout(usize::from(request.columns), &Cancellation::never())?;
    Ok(layout.grid(
        layout.first_row(request.scroll_offset, rows),
        rows,
        request.foreground,
        request.background,
        None,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    #[test]
    fn image_texture_is_uploaded_once_and_grid_rows_are_limited() {
        let ctx = Context::default();
        let image = Arc::new(PreviewImage::new(egui::ColorImage::new(
            [64, 32],
            vec![Color32::RED; 64 * 32],
        )));
        let preview = Preview::Image(image.clone());
        let row = |text: &str| -> Vec<PreviewCell> {
            text.chars()
                .map(|ch| PreviewCell {
                    text: ch.into(),
                    foreground: Color32::RED,
                    ..Default::default()
                })
                .collect()
        };
        let styled = Preview::Grid {
            columns: 16,
            cells: [row("visible red line"), row("hidden last line")].concat(),
            total_rows: 2,
            lines: None,
        };
        let frame = ctx.run_ui(egui::RawInput::default(), |ui| {
            paint(ui, &preview, &Appearance::default(), 5);
            paint(ui, &styled, &Appearance::default(), 1);
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
            paint(ui, &preview, &Appearance::default(), 5)
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
            wrap_text: true,
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
        let page = worker.document.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(plain_text(page).as_deref(), Some("1"));
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
    #[test]
    fn text_wraps_at_the_pane_width_or_scrolls_sideways() {
        let provider = |_: PreviewRequest<usize>, _: Cancellation| {
            Ok(Preview::Text(format!("{}\nshort", "x".repeat(100))))
        };
        // The request is 40 columns wide.
        let shape = |wrap_text: bool| match render(
            &provider,
            PreviewRequest {
                wrap_text,
                ..request(0)
            },
            Cancellation::never(),
        ) {
            (Ok(Preview::Grid { columns, total_rows, .. }), _, Some(_)) => (columns, total_rows),
            other => panic!("expected a text page, got {:?}", other.0),
        };
        assert_eq!(shape(true), (40, 4));
        assert_eq!(shape(false), (100, 2));
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
            wrap_text: true,
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
            // Rows "<item>0".."<item>19", so a page can start at row 1.
            Ok(Preview::Text(
                (0..20)
                    .map(|row| format!("{}{row}", request.selection.text))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ))
        });
        let first_row = |worker: &PreviewWorker<usize>| {
            let page = worker.document.as_ref().unwrap().as_ref().unwrap();
            plain_text(page).unwrap().lines().next().unwrap().to_owned()
        };
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
        assert_eq!(first_row(&worker), "00");
        assert_eq!(worker.document_scroll_offset, 0);
        release.send(()).unwrap();
        while worker.loading {
            worker.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(first_row(&worker), "11");
        assert_eq!(worker.document_scroll_offset, 1);
    }
}

#[cfg(test)]
mod text_tests {
    use super::*;
    use std::time::{Duration, Instant};
    fn request(scroll_offset: usize) -> PreviewRequest<usize> {
        PreviewRequest {
            document_version: 1,
            initial_page: false,
            selection: Selection {
                item: 0,
                text: "0".into(),
                index: 0,
                source_version: 1,
                snapshot: Arc::new(()),
            },
            columns: 40,
            rows: 3,
            scroll_offset,
            foreground: Color32::RED,
            background: Color32::BLACK,
            wrap_text: true,
        }
    }
    fn wait(worker: &mut PreviewWorker<usize>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while worker.loading {
            worker.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn text_is_paged_numbered_and_copied_as_a_document() {
        let provider = Arc::new(move |_: PreviewRequest<usize>, _: Cancellation| {
            Ok(Preview::Text("one\ntwo\nthree\nfour\nfive".into()))
        });
        let mut worker = PreviewWorker::new(provider, Context::default());
        worker.request(request(1));
        wait(&mut worker);
        let Some(Ok(Preview::Grid {
            cells,
            total_rows,
            lines,
            ..
        })) = &worker.document
        else {
            panic!("text should arrive as a grid page");
        };
        assert_eq!(*total_rows, 5);
        assert_eq!(cells[0].foreground, Color32::RED);
        assert_eq!(
            lines.as_ref().unwrap(),
            &GridLines {
                starts: vec![Some(1), Some(2), Some(3)],
                total: 5,
            }
        );
        let page = worker.document.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(plain_text(page).as_deref(), Some("two\nthree\nfour"));
        let document = worker.copy_document().unwrap().unwrap();
        assert_eq!(document.metadata().unwrap().total_rows, 5);
        assert_eq!(document.line_number(4), Some(4));

        // Scrolling past the end clamps to the last page.
        worker.request(request(9));
        wait(&mut worker);
        let page = worker.document.as_ref().unwrap().as_ref().unwrap();
        assert_eq!(plain_text(page).as_deref(), Some("three\nfour\nfive"));
        assert_eq!(worker.document_scroll_offset, 2);
    }
}

/// Text represented by a preview, with ANSI styling removed. Images have no text.
#[cfg(test)]
pub(crate) fn plain_text(preview: &Preview) -> Option<String> {
    match preview {
        Preview::Text(text) => Some(text.clone()),
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
