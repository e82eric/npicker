//! An output page: one pane showing a document that grows, such as what a
//! program prints. It has no results and no query. The host makes an
//! [`OutputWriter`], hands it to whatever produces the text, and shows an
//! [`Output`] made with it.
//!
//! The page is always for reading: the keys move a cursor through the text,
//! search it and copy from it, as they do in a picker's focused preview.
//! While its end is in view it follows the text as more arrives.
//!
//! Like a picker, it never closes itself: Escape reports
//! [`OutputEvent::Cancelled`] and the host decides what comes next. Dropping
//! the page closes its writer, which is how the producer learns to stop.
use crate::copy_mode::CellPosition;
use crate::picker::{
    IdSlot, PANEL_FRAME_HEIGHT, PANEL_FRAME_WIDTH, paint_loading_indicator, panel_frame,
    panel_style,
};
use crate::preview_copy::PreviewCopyMode;
use crate::styled_document::StyledDocument;
use crate::{Appearance, Cancellation, Placement};
use egui::{Context, Event, Id, Key};
use std::sync::{Arc, Mutex};

/// What an output page is taller than its rows of text by, in points: for a
/// host that sizes a window before there is a frame to ask [`Output::height`]
/// in.
pub const OUTPUT_FRAME_HEIGHT: f32 = PANEL_FRAME_HEIGHT;

/// The text an output page shows, which its producer keeps publishing as it
/// grows, from any thread. The producer learns that the page is gone with
/// [`OutputWriter::on_close`] or [`OutputWriter::is_closed`], to stop.
#[derive(Clone, Default)]
pub struct OutputWriter(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    version: u64,
    document: Option<Arc<StyledDocument>>,
    finished: bool,
    closed: bool,
    ctx: Option<Context>,
    on_close: Vec<Box<dyn FnOnce() + Send>>,
}

impl OutputWriter {
    pub fn new() -> Self {
        Self::default()
    }
    /// Show `document` in place of the one before. The page keeps its place
    /// in it, or moves to the new end while the end was in view.
    pub fn publish(&self, document: Arc<StyledDocument>) {
        let ctx = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.version += 1;
            state.document = Some(document);
            state.ctx.clone()
        };
        if let Some(ctx) = ctx {
            ctx.request_repaint();
        }
    }
    /// Say that no more is coming, which stops the loading indicator.
    pub fn finish(&self) {
        let ctx = {
            let mut state = self.lock();
            state.finished = true;
            state.ctx.clone()
        };
        if let Some(ctx) = ctx {
            ctx.request_repaint();
        }
    }
    /// The document last published. The page reads `current`; this is for a
    /// producer's tests, to check what it published.
    pub fn document(&self) -> Option<Arc<StyledDocument>> {
        self.lock().document.clone()
    }
    /// Whether `finish` was called.
    pub fn is_finished(&self) -> bool {
        self.lock().finished
    }
    /// Whether the page showing it is gone.
    pub fn is_closed(&self) -> bool {
        self.lock().closed
    }
    /// Run `close` once the page is gone, or now if it is. It runs on the
    /// thread the page was dropped on, usually the UI thread.
    pub fn on_close(&self, close: impl FnOnce() + Send + 'static) {
        let mut state = self.lock();
        if state.closed {
            drop(state);
            close();
        } else {
            state.on_close.push(Box::new(close));
        }
    }
    /// Say that the page is gone, or wants no more: the producer stops, and
    /// nothing published after this is shown. What was published stays. A
    /// page does it when dropped; a host does it to stop the producer.
    pub fn close(&self) {
        let callbacks = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            state.ctx = None;
            std::mem::take(&mut state.on_close)
        };
        for callback in callbacks {
            callback();
        }
    }
    /// Repaint `ctx` when there is more to show.
    fn attach(&self, ctx: &Context) {
        let mut state = self.lock();
        if state.ctx.is_none() && !state.closed {
            state.ctx = Some(ctx.clone());
        }
    }
    /// The latest document and its version, and whether it is finished.
    fn current(&self) -> (u64, Option<Arc<StyledDocument>>, bool) {
        let state = self.lock();
        (state.version, state.document.clone(), state.finished)
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Clone, Debug)]
pub struct OutputConfig {
    /// Rows of text shown. A full-pane placement shows as many as fit.
    pub rows: usize,
}
impl Default for OutputConfig {
    fn default() -> Self {
        Self { rows: 24 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputEvent {
    /// Escape, with no selection or search of the page's own to leave first.
    Cancelled,
}

pub struct Output {
    id: Id,
    _id_slot: IdSlot,
    pub config: OutputConfig,
    writer: OutputWriter,
    /// The document as laid out for the pane, with the reader's place in it.
    viewport: Option<PreviewCopyMode>,
    /// What `viewport` was laid out from: the writer's version, and the
    /// pane's columns and rows.
    laid_out: Option<(u64, usize, usize)>,
    /// The document's end is in view, so the view moves with it.
    following: bool,
}

impl Output {
    /// Pages made with the same `id` may be alive together. The page shows
    /// what `writer` publishes and closes it when dropped.
    pub fn new(id: impl egui::AsId, config: OutputConfig, writer: OutputWriter) -> Self {
        let id_slot = IdSlot::take(Id::new(id));
        Self {
            id: id_slot.id(),
            _id_slot: id_slot,
            config,
            writer,
            viewport: None,
            laid_out: None,
            following: true,
        }
    }
    /// The height, in points, the page asks for. Hosts that size a window to
    /// it add the placement's margins.
    pub fn height(&self, ctx: &Context, appearance: &Appearance) -> f32 {
        self.config.rows.max(1) as f32 * appearance.typography.preview_row_height(ctx)
            + OUTPUT_FRAME_HEIGHT
    }
    /// Whether the keys typed so far are part of something unfinished, which
    /// laying the document out again would lose.
    fn engaged(viewport: &PreviewCopyMode) -> bool {
        let mode = &viewport.mode;
        viewport.search_input_active()
            || viewport.quick_select_active()
            || mode.visual_anchor.is_some()
            || mode.pending_g
            || mode.pending_z
            || mode.pending_count.is_some()
            || mode.pending_jump.is_some()
            || mode.pending_yank.is_some()
            || mode.pending_visual_object.is_some()
    }
    /// Lay the latest document out for a pane of `columns` by `rows` when
    /// either changed, keeping the reader's place, or the end while following
    /// it. A selection or search under way keeps the document it began on.
    ///
    /// Each new version is laid out whole, here on the UI thread: every line
    /// is wrapped again, not only the rows in view, and the producer built
    /// the whole document again before publishing it. Only painting is
    /// limited to the viewport. That is fine for what the page shows now, an
    /// answer or a command's output of a few thousand lines, where a pass is
    /// far shorter than a frame; it has not been measured near the 16 MiB a
    /// producer may publish, where it would be felt.
    ///
    /// If it becomes a problem, the likely fix is to work a block at a time
    /// rather than to append bytes: a paragraph, list or code fence that has
    /// ended does not change again, so its lines and their rows can be kept
    /// and only the last, open block redone, with one whole pass when the
    /// writer finishes. Markdown rules out a plain append, since later text
    /// can change how earlier text in the same block is laid out.
    fn refresh(&mut self, columns: usize, rows: usize) {
        let (version, document, _) = self.writer.current();
        if self.laid_out == Some((version, columns, rows)) {
            return;
        }
        if self.viewport.as_ref().is_some_and(Self::engaged) {
            return;
        }
        self.laid_out = Some((version, columns, rows));
        let place = self
            .viewport
            .as_ref()
            .map(|viewport| (viewport.top as usize, viewport.mode.cursor));
        let layout =
            document.and_then(|document| document.layout(columns, &Cancellation::never()).ok());
        let total = layout.as_ref().map_or(0, |layout| layout.total_rows());
        let last_page = total.saturating_sub(rows);
        let top = match place {
            Some((top, _)) if !self.following => top.min(last_page),
            _ => last_page,
        };
        self.viewport = layout.and_then(|layout| PreviewCopyMode::host(layout, top, rows));
        if let Some(viewport) = &mut self.viewport {
            // The cursor stays where it was, within the rows in view; at the
            // end while following it.
            let last_row = total.saturating_sub(1) as u64;
            let (first, last) = (top as u64, ((top + rows).saturating_sub(1) as u64).min(last_row));
            viewport.mode.cursor = match place {
                Some((_, cursor)) if !self.following => CellPosition {
                    x: cursor.x.min(columns.saturating_sub(1) as u32),
                    y: cursor.y.clamp(first, last),
                },
                _ => CellPosition { x: 0, y: last },
            };
        }
    }
    /// Whether the reader is at the document's end: its last row is in view
    /// and the cursor is on it. Moving up a row is enough to stop following.
    fn at_end(&self, rows: usize) -> bool {
        self.viewport.as_ref().is_none_or(|viewport| {
            viewport.top + rows as u64 >= viewport.rows()
                && viewport.mode.cursor.y + 1 >= viewport.rows()
        })
    }
    /// Draw within an existing host egui pass. A disabled page is drawn and
    /// takes no input. Returns what happened that the host answers, if
    /// anything did.
    pub fn show(
        &mut self,
        ctx: &Context,
        placement: Placement,
        appearance: &Appearance,
        disabled: bool,
    ) -> Option<OutputEvent> {
        let interactive = !disabled;
        self.writer.attach(ctx);
        let row_height = appearance.typography.preview_row_height(ctx).max(1.0);
        let cell_width = appearance.typography.cell_width.max(1.0);
        let rect = placement.rect(self.height(ctx, appearance));
        let inner = egui::vec2(
            (rect.width() - PANEL_FRAME_WIDTH).max(1.0),
            (rect.height() - PANEL_FRAME_HEIGHT).max(0.0),
        );
        let rows = ((inner.y / row_height).floor() as usize).max(1);
        let columns = (inner.x / cell_width).floor().clamp(1.0, 4096.0) as usize;
        self.refresh(columns, rows);

        let mut event = None;
        if interactive {
            let events = ctx.input(|input| input.events.clone());
            let leave = match &mut self.viewport {
                Some(viewport) => {
                    if events.iter().any(|event| {
                        matches!(event, Event::Key { key: Key::C, pressed: true, modifiers, .. }
                            if modifiers.command && modifiers.shift)
                    }) {
                        viewport.copy_all(ctx);
                    }
                    viewport.input(ctx, &events, rows)
                }
                // Nothing to read yet: Escape still leaves.
                None => events.iter().any(|event| {
                    matches!(
                        event,
                        Event::Key {
                            key: Key::Escape,
                            pressed: true,
                            ..
                        }
                    )
                }),
            };
            ctx.input_mut(|input| {
                input.events.retain(|event| {
                    !matches!(
                        event,
                        Event::Key { .. }
                            | Event::Text(_)
                            | Event::Paste(_)
                            | Event::Copy
                            | Event::Cut
                    )
                })
            });
            if leave {
                event = Some(OutputEvent::Cancelled);
            }
        }
        if rect.is_positive() {
            let finished = self.writer.is_finished();
            egui::Area::new(self.id)
                .fade_in(false)
                .order(if interactive {
                    egui::Order::Foreground
                } else {
                    egui::Order::Middle
                })
                .interactable(interactive)
                .fixed_pos(rect.min)
                .show(ctx, |ui| {
                    ui.set_clip_rect(rect.intersect(placement.bounds).intersect(ui.clip_rect()));
                    panel_frame(appearance).show(ui, |ui| {
                        ui.set_width(inner.x);
                        panel_style(ui, appearance);
                        let (pane, response) = ui.allocate_exact_size(
                            egui::vec2(inner.x, rows as f32 * row_height),
                            egui::Sense::hover(),
                        );
                        if let Some(viewport) = &mut self.viewport {
                            let wheel = ui.input(|input| input.smooth_scroll_delta.y);
                            if interactive && response.hovered() && wheel.abs() > 0.0 {
                                viewport.scroll(if wheel > 0.0 { -3 } else { 3 }, rows);
                            }
                            ui.scope_builder(
                                egui::UiBuilder::new()
                                    .max_rect(pane)
                                    .layout(egui::Layout::top_down(egui::Align::LEFT)),
                                |ui| viewport.paint_layers(ui, appearance, rows, interactive),
                            );
                        }
                        if !finished {
                            let center = egui::pos2(pane.right() - 8.0, pane.top() + 8.0);
                            paint_loading_indicator(
                                ui,
                                egui::Rect::from_center_size(center, egui::vec2(12.0, 12.0)),
                                appearance.palette.muted,
                            );
                            ui.ctx().request_repaint_after(std::time::Duration::from_millis(80));
                        }
                    });
                });
        }
        // Where the reader is now says whether to follow what comes next.
        self.following = self.at_end(rows);
        event
    }
    #[cfg(test)]
    fn top(&self) -> Option<u64> {
        self.viewport.as_ref().map(|viewport| viewport.top)
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        self.writer.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn draw(ctx: &Context, page: &mut Output, events: Vec<Event>) -> Option<OutputEvent> {
        let mut shown = None;
        let mut frame = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 800.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                let response = page.show(
                    ui.ctx(),
                    Placement::new(ui.ctx().content_rect()),
                    &Appearance::default(),
                    false,
                );
                shown = shown.or(response);
            },
        );
        frame.textures_delta.clear();
        shown
    }
    fn key(key: Key) -> Event {
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }
    fn lines(count: usize) -> Arc<StyledDocument> {
        let text: Vec<String> = (1..=count).map(|line| format!("line {line}")).collect();
        StyledDocument::from_text(&text.join("\n"))
    }
    fn page(rows: usize) -> (Output, OutputWriter) {
        let writer = OutputWriter::new();
        (
            Output::new("output-test", OutputConfig { rows }, writer.clone()),
            writer,
        )
    }

    #[test]
    fn the_page_follows_the_end_until_the_reader_moves_away_from_it() {
        let ctx = Context::default();
        let (mut page, writer) = page(5);
        // Nothing yet: it draws, and Escape still leaves.
        assert_eq!(draw(&ctx, &mut page, vec![]), None);
        assert_eq!(page.top(), None);

        writer.publish(lines(3));
        draw(&ctx, &mut page, vec![]);
        assert_eq!(page.top(), Some(0));
        // More than a page: the end stays in view as it grows.
        writer.publish(lines(12));
        draw(&ctx, &mut page, vec![]);
        assert_eq!(page.top(), Some(7));
        writer.publish(lines(20));
        draw(&ctx, &mut page, vec![]);
        assert_eq!(page.top(), Some(15));

        // The reader goes to the top: the page stays there as more arrives.
        draw(&ctx, &mut page, vec![key(Key::G), key(Key::G)]);
        assert_eq!(page.top(), Some(0));
        writer.publish(lines(40));
        draw(&ctx, &mut page, vec![]);
        assert_eq!(page.top(), Some(0));

        // Back at the end, it follows again.
        draw(
            &ctx,
            &mut page,
            vec![Event::Key {
                key: Key::G,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::SHIFT,
            }],
        );
        assert_eq!(page.top(), Some(35));
        writer.publish(lines(50));
        draw(&ctx, &mut page, vec![]);
        assert_eq!(page.top(), Some(45));

        // One row up, with the end still in view, is enough to stay put.
        draw(&ctx, &mut page, vec![key(Key::K)]);
        writer.publish(lines(60));
        draw(&ctx, &mut page, vec![]);
        assert!(page.top() < Some(50), "{:?}", page.top());
    }

    #[test]
    fn escape_is_reported_and_the_page_stays_as_it_is() {
        let ctx = Context::default();
        let (mut page, writer) = page(5);
        assert_eq!(
            draw(&ctx, &mut page, vec![key(Key::Escape)]),
            Some(OutputEvent::Cancelled)
        );
        writer.publish(lines(20));
        draw(&ctx, &mut page, vec![]);
        // A selection under way is left first.
        draw(&ctx, &mut page, vec![key(Key::V)]);
        assert_eq!(draw(&ctx, &mut page, vec![key(Key::Escape)]), None);
        assert_eq!(
            draw(&ctx, &mut page, vec![key(Key::Escape)]),
            Some(OutputEvent::Cancelled)
        );
        assert_eq!(page.top(), Some(15));
        assert!(!writer.is_closed());
    }

    #[test]
    fn a_selection_keeps_the_document_it_began_on() {
        let ctx = Context::default();
        let (mut page, writer) = page(5);
        writer.publish(lines(20));
        draw(&ctx, &mut page, vec![]);
        draw(&ctx, &mut page, vec![key(Key::V)]);
        writer.publish(lines(40));
        draw(&ctx, &mut page, vec![]);
        assert_eq!(page.top(), Some(15), "the selection's document stays");
        // Leaving the selection shows what arrived meanwhile.
        draw(&ctx, &mut page, vec![key(Key::Escape)]);
        draw(&ctx, &mut page, vec![]);
        assert_eq!(page.top(), Some(35));
    }

    #[test]
    fn dropping_the_page_closes_the_writer() {
        let (page, writer) = page(5);
        let stopped = Arc::new(AtomicBool::new(false));
        let flag = stopped.clone();
        writer.on_close(move || flag.store(true, Ordering::Release));
        assert!(!writer.is_closed());
        drop(page);
        assert!(writer.is_closed() && stopped.load(Ordering::Acquire));
        // Publishing after that is ignored, and a late listener runs at once.
        writer.publish(lines(1));
        let late = Arc::new(AtomicBool::new(false));
        let flag = late.clone();
        writer.on_close(move || flag.store(true, Ordering::Release));
        assert!(late.load(Ordering::Acquire));
    }
}
