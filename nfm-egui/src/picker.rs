use crate::{
    Appearance, ItemsSource, Placement, PreviewProvider, PreviewRequest, SearchSnapshotProvider,
    SearchSortMode,
};
use crate::{
    preview::{self, Preview, PreviewWorker},
    session::{Session, Update},
};
use egui::{Context, Id, Key, Modifiers};
use nfm_search_core::search::resolve_match_positions;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

const PANEL_RADIUS: u8 = 8;
const DIVIDER_HEIGHT: f32 = 15.0;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Layout {
    #[default]
    QueryBottom,
    QueryTop,
}
/// An interaction the host can override.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerTrigger {
    Key(Key),
    DoubleClick,
}
#[derive(Clone, Debug)]
pub struct PickerBinding {
    pub trigger: PickerTrigger,
    pub modifiers: Modifiers,
    pub action: usize,
}
#[derive(Clone, Debug)]
pub struct PickerConfig {
    pub layout: Layout,
    /// Host metadata; the Ghostty-style panel has no separate title row.
    pub title: String,
    pub initial_query: String,
    pub result_rows: usize,
    /// Hosts can enable this for theme/action lists that accept a single click.
    pub accept_on_click: bool,
    /// Custom interactions reported to the host without closing the picker.
    pub bindings: Vec<PickerBinding>,
    pub preview_rows: usize,
    pub preview_visible: bool,
    pub sort: SearchSortMode,
}
impl Default for PickerConfig {
    fn default() -> Self {
        Self {
            layout: Layout::QueryBottom,
            title: "Picker".into(),
            initial_query: String::new(),
            result_rows: 7,
            accept_on_click: false,
            bindings: Vec::new(),
            preview_rows: 12,
            preview_visible: true,
            sort: SearchSortMode::Score,
        }
    }
}
#[derive(Clone)]
pub struct Selection<T> {
    pub item: T,
    pub text: String,
    pub index: usize,
    pub source_version: u64,
    pub snapshot: Arc<dyn std::any::Any + Send + Sync>,
}
impl<T> Selection<T> {
    pub fn source_snapshot<S: Send + Sync + 'static>(&self) -> Option<Arc<S>> {
        self.snapshot.clone().downcast().ok()
    }
}
impl<T: std::fmt::Debug> std::fmt::Debug for Selection<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Selection")
            .field("item", &self.item)
            .field("text", &self.text)
            .field("index", &self.index)
            .field("source_version", &self.source_version)
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Debug)]
pub enum PickerEvent<T> {
    Accepted(Selection<T>),
    CustomAction {
        action: usize,
        selection: Selection<T>,
    },
    Cancelled,
    CopyRequested(Selection<T>),
}
pub struct PickerOutput<T> {
    /// Content paint commands were built this frame; this is not GPU presentation.
    pub preview_rendered: bool,
    /// Document identity of the worker preview drawn this frame.
    pub preview_selection: Option<(u64, usize)>,
    /// Host-rendered preview bounds in egui points; hidden while help is open.
    pub preview_rect: Option<egui::Rect>,
    pub event: Option<PickerEvent<T>>,
    frame_selection: Option<Selection<T>>,
    pub wants_keyboard: bool,
    pub busy: bool,
    pub matched: usize,
    pub total: usize,
    pub rect: egui::Rect,
}

impl<T> PickerOutput<T> {
    /// The event's owned selection, or the current selection captured this frame.
    /// Event selections retain their exact source snapshot after the picker closes.
    pub fn selection(&self) -> Option<&Selection<T>> {
        match &self.event {
            Some(PickerEvent::Accepted(selection))
            | Some(PickerEvent::CustomAction { selection, .. })
            | Some(PickerEvent::CopyRequested(selection)) => Some(selection),
            _ => self.frame_selection.as_ref(),
        }
    }
}

pub struct Picker<S: ItemsSource + Send + Sync + 'static> {
    id: Id,
    session: Session<S>,
    config: PickerConfig,
    query: String,
    revision: u64,
    transition_pending: bool,
    displayed: Option<Update<S>>,
    completion_snapshot: Option<Arc<S>>,
    selected: usize,
    visible_start: usize,
    focus_query: bool,
    closed: bool,
    preview: Option<PreviewWorker<S::Item>>,
    preview_key: Option<(u64, usize, u16, u16, usize, u32, u32)>,
    scroll_offset: usize,
    error: Option<String>,
    visible_result_rows: usize,
    visible_preview_rows: usize,
    external_preview: bool,
    external_document: Option<Result<Preview, String>>,
    external_loading: bool,
    preview_rect: Option<egui::Rect>,
    preview_rendered: bool,
    copied_until: Option<Instant>,
    suggestions: Vec<nfm_search_core::store::SearchCompletion>,
    suggestion_selected: usize,
    query_cursor: usize,
    query_rect: egui::Rect,
    query_text_pos: egui::Pos2,
    dismissed_completion: Option<(String, usize)>,
    keybinding_help_visible: bool,
    keybinding_help_offset: usize,
    keybinding_help_rows: usize,
    additional_keybinding_help: Vec<(String, String)>,
    copy_notice: String,
    preview_viewport: Option<crate::preview_copy::PreviewCopyMode>,
    preview_focus: bool,
    cursor_scroll_padding: u32,
}
impl<S> Picker<S>
where
    S: ItemsSource + Send + Sync + 'static,
    S::Item: Send + 'static,
{
    pub fn new<P: SearchSnapshotProvider<S> + ?Sized>(
        id: impl egui::AsId,
        ctx: &Context,
        provider: Arc<P>,
        config: PickerConfig,
    ) -> Self {
        let query = config.initial_query.clone();
        let query_cursor = query.len();
        Self {
            id: Id::new(id),
            session: Session::new(provider, query.clone(), config.sort, ctx.clone()),
            config,
            query,
            revision: 0,
            transition_pending: false,
            displayed: None,
            completion_snapshot: None,
            selected: 0,
            visible_start: 0,
            focus_query: true,
            closed: false,
            preview: None,
            preview_key: None,
            scroll_offset: 0,
            error: None,
            visible_result_rows: 7,
            visible_preview_rows: 12,
            external_preview: false,
            external_document: None,
            external_loading: false,
            preview_rect: None,
            preview_rendered: false,
            copied_until: None,
            suggestions: Vec::new(),
            suggestion_selected: 0,
            query_cursor,
            query_rect: egui::Rect::NOTHING,
            query_text_pos: egui::Pos2::ZERO,
            dismissed_completion: None,
            keybinding_help_visible: false,
            keybinding_help_offset: 0,
            keybinding_help_rows: 1,
            additional_keybinding_help: Vec::new(),
            copy_notice: "Copied".into(),
            preview_viewport: None,
            preview_focus: false,
            cursor_scroll_padding: crate::copy_mode::SCROLL_PADDING_ROWS,
        }
    }
    /// Change sources without clearing the presented rows or preview. Stale
    /// selections remain disabled until the replacement has results or finishes.
    pub fn replace_source<P: SearchSnapshotProvider<S> + ?Sized>(
        &mut self,
        ctx: &Context,
        provider: Arc<P>,
        query: impl Into<String>,
    ) {
        self.leave_preview_focus();
        self.session.stop();
        self.query = query.into();
        self.query_cursor = self.query.len();
        self.session = Session::new(provider, self.query.clone(), self.config.sort, ctx.clone());
        self.revision = 0;
        self.transition_pending = true;
        if let Some(displayed) = &mut self.displayed {
            displayed.revision = u64::MAX;
        }
        if let Some(preview) = &self.preview {
            self.preview = Some(preview.restart_retaining_document(ctx.clone()));
        }
        self.preview_key = None;
        self.scroll_offset = 0;
        if let Some(preview) = &mut self.preview {
            preview.document_selection = None;
        }
        self.completion_snapshot = None;
        self.suggestions.clear();
        self.suggestion_selected = 0;
        self.dismissed_completion = None;
        self.keybinding_help_visible = false;
        self.error = None;
        self.closed = false;
        self.focus_query = true;
        self.copied_until = None;
        self.store_query_cursor(ctx);
        ctx.request_repaint();
    }
    /// Reserve preview space for a native host renderer without a worker.
    pub fn set_external_preview(&mut self, enabled: bool) {
        self.external_preview = enabled;
    }
    /// Paint a host-produced preview using the shared renderer. Paging and
    /// native snapshot lifetimes remain the host's responsibility.
    pub fn set_external_preview_document(
        &mut self,
        document: Option<Result<Preview, String>>,
        loading: bool,
    ) {
        let same = match (self.external_document.as_ref(), document.as_ref()) {
            (Some(Ok(Preview::Text(old))), Some(Ok(Preview::Text(new)))) => old == new,
            (Some(Ok(Preview::StyledText(old))), Some(Ok(Preview::StyledText(new)))) => old == new,
            _ => false,
        };
        if !same {
            self.leave_preview_focus();
        }
        self.external_document = document;
        self.external_loading = loading;
    }
    pub fn set_preview_provider(
        &mut self,
        ctx: &Context,
        provider: Arc<dyn PreviewProvider<S::Item>>,
    ) {
        self.leave_preview_focus();
        let old = self.preview.take();
        let mut next = PreviewWorker::new(provider, ctx.clone());
        if let Some(old) = old {
            old.stop();
            next.document = old.document.clone();
            next.document_scroll_offset = old.document_scroll_offset;
        }
        self.preview = Some(next);
        self.preview_key = None;
        self.scroll_offset = 0;
        if let Some(preview) = &mut self.preview {
            preview.document_selection = None;
        }
    }
    pub fn set_error(&mut self, error: Option<String>) {
        self.error = error;
    }
    pub fn query(&self) -> &str {
        &self.query
    }
    pub fn set_query(&mut self, query: impl Into<String>) {
        let query = query.into();
        if query != self.query {
            self.leave_preview_focus();
            self.query = query;
            self.query_cursor = self.query.len();
            self.suggestion_selected = 0;
            self.dismissed_completion = None;
            let submit_timer = crate::timing::Span::new("nfm_search_submit", self.query.len());
            self.revision = self.session.set_query(self.query.clone());
            drop(submit_timer);
            self.selected = 0;
            self.visible_start = 0;
            self.scroll_offset = 0;
            if let Some(preview) = &mut self.preview {
                preview.document_selection = None;
            }
            // Clearing the published document identity also invalidates its request key.
            // The same selected item must be republished for copy/quick-select access.
            self.preview_key = None;
            // Keep old rows and preview painted while the new query runs.
            // selection() still rejects acceptance until the revision matches.
        }
    }
    pub fn completions(&self, cursor: usize) -> Vec<nfm_search_core::store::SearchCompletion> {
        if !self.query.is_char_boundary(cursor) {
            return Vec::new();
        }
        self.completion_snapshot
            .as_ref()
            .map_or_else(Vec::new, |snapshot| {
                snapshot.completions(&self.query, cursor)
            })
    }
    pub fn apply_completion(
        &mut self,
        completion: &nfm_search_core::store::SearchCompletion,
    ) -> bool {
        let range = completion.replace.clone();
        if range.start > range.end
            || range.end > self.query.len()
            || !self.query.is_char_boundary(range.start)
            || !self.query.is_char_boundary(range.end)
        {
            return false;
        }
        let mut query = self.query.clone();
        query.replace_range(range.clone(), &completion.replacement);
        self.set_query(query);
        self.query_cursor = range.start + completion.replacement.len();
        self.focus_query = true;
        true
    }
    fn refresh_completions(&mut self) {
        let _timer = crate::timing::Span::new("nfm_completions", self.query.len());
        if self
            .dismissed_completion
            .as_ref()
            .is_some_and(|(query, cursor)| query == &self.query && *cursor == self.query_cursor)
        {
            self.suggestions.clear();
            return;
        }
        let suggestions = self.completions(self.query_cursor);
        if suggestions != self.suggestions {
            self.suggestion_selected = self
                .suggestion_selected
                .min(suggestions.len().saturating_sub(1));
            self.suggestions = suggestions;
        }
    }
    fn store_query_cursor(&self, ctx: &Context) {
        let id = self.id.with("query");
        if let Some(mut state) = egui::text_edit::TextEditState::load(ctx, id) {
            let cursor = self.query[..self.query_cursor.min(self.query.len())]
                .chars()
                .count();
            state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(cursor),
                )));
            state.store(ctx, id);
        }
    }
    fn completion_ui(&mut self, ctx: &Context, bounds: egui::Rect, appearance: &Appearance) {
        if self.suggestions.is_empty() || !self.query_rect.is_positive() {
            return;
        }
        let row_height = appearance.typography.preview_row_height(ctx);
        let visible = self
            .suggestions
            .len()
            .min(6)
            .min(((bounds.height() - 16.0) / row_height).floor().max(1.0) as usize);
        let width = self
            .suggestions
            .iter()
            .map(|s| {
                ctx.fonts_mut(|fonts| {
                    fonts
                        .layout_no_wrap(
                            s.text.clone(),
                            appearance.typography.normal.clone(),
                            appearance.palette.text,
                        )
                        .size()
                        .x
                })
            })
            .fold(0.0_f32, f32::max);
        let width = (width + 24.0)
            .max(280.0)
            .min(self.query_rect.width())
            .min(bounds.width())
            .max(1.0);
        let height = visible as f32 * row_height + 16.0;
        let token_start = self.suggestions[self.suggestion_selected]
            .replace
            .start
            .min(self.query.len());
        let prefix = &self.query[..token_start];
        let prefix_width = ctx.fonts_mut(|fonts| {
            fonts
                .layout_no_wrap(
                    prefix.into(),
                    appearance.typography.normal.clone(),
                    appearance.palette.text,
                )
                .size()
                .x
        });
        let x = (self.query_text_pos.x + prefix_width)
            .clamp(bounds.left(), (bounds.right() - width).max(bounds.left()));
        let y = match self.config.layout {
            Layout::QueryBottom => (self.query_rect.top() - height - 4.0).max(bounds.top()),
            Layout::QueryTop => {
                (self.query_rect.bottom() + 4.0).min((bounds.bottom() - height).max(bounds.top()))
            }
        };
        let first = self
            .suggestion_selected
            .saturating_sub(visible - 1)
            .min(self.suggestions.len() - visible);
        let mut chosen = None;
        let popup_id = self.id.with("autocomplete");
        let previous_size = ctx.memory(|memory| memory.area_rect(popup_id).map(|rect| rect.size()));
        let area = egui::Area::new(popup_id)
            .order(egui::Order::Tooltip)
            .fade_in(false)
            .fixed_pos(egui::pos2(x, y))
            .show(ctx, |ui| {
                ui.set_clip_rect(ui.clip_rect().intersect(bounds));
                egui::Frame::new()
                    .fill(appearance.palette.background)
                    .stroke(egui::Stroke::new(1.0, appearance.palette.divider))
                    .corner_radius(8)
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.set_width((width - 24.0).max(1.0));
                        ui.spacing_mut().item_spacing.y = 0.0;
                        for index in first..first + visible {
                            let suggestion = &self.suggestions[index];
                            let (rect, response) = ui.allocate_exact_size(
                                egui::vec2((width - 24.0).max(1.0), row_height),
                                egui::Sense::click(),
                            );
                            let selected = index == self.suggestion_selected;
                            if selected {
                                ui.painter().rect_filled(
                                    rect,
                                    6,
                                    appearance.palette.selection_background,
                                );
                            }
                            let job = highlighted_layout(
                                &suggestion.text,
                                &suggestion.positions,
                                selected,
                                appearance,
                            );
                            let galley = ui.painter().layout_job(job);
                            ui.painter().with_clip_rect(rect.intersect(bounds)).galley(
                                egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0),
                                galley,
                                appearance.palette.text,
                            );
                            if response.clicked() {
                                chosen = Some(suggestion.clone());
                            }
                        }
                    });
            });
        if previous_size.is_none_or(|size| {
            (size.x - area.response.rect.width()).abs() > 0.5
                || (size.y - area.response.rect.height()).abs() > 0.5
        }) {
            ctx.request_discard("resolve autocomplete size before presentation");
        }
        if let Some(completion) = chosen {
            if self.apply_completion(&completion) {
                self.store_query_cursor(ctx);
                self.refresh_completions();
                ctx.request_repaint();
            }
        }
    }
    /// Add host actions to the shared keyboard help overlay.
    /// True while keyboard input navigates and selects preview text.
    /// Set the preview cursor scroll margin. Zero disables the margin.
    pub fn set_cursor_scroll_padding(&mut self, rows: u32) {
        self.cursor_scroll_padding = rows;
        if let Some(viewport) = &mut self.preview_viewport {
            viewport.set_scroll_padding(rows);
        }
    }

    pub fn preview_focused(&self) -> bool {
        self.preview_focus
    }

    // Document changes discard the viewport; focus changes keep it in place.
    fn leave_preview_focus(&mut self) {
        self.deactivate_preview_focus();
        self.preview_viewport = None;
    }

    fn deactivate_preview_focus(&mut self) {
        if let Some(viewport) = &mut self.preview_viewport {
            self.scroll_offset = viewport.top as usize;
            viewport.hide_decorations();
        }
        self.preview_focus = false;
        self.focus_query = true;
    }

    fn toggle_preview_focus(&mut self, ctx: &Context) {
        if self.preview_focus {
            self.deactivate_preview_focus();
            return;
        }
        self.ensure_preview_viewport();
        if self.preview_viewport.is_some() {
            self.preview_focus = true;
            self.focus_query = false;
            self.dismissed_completion = Some((self.query.clone(), self.query_cursor));
            self.suggestions.clear();
            ctx.memory_mut(|memory| memory.request_focus(self.id.with("preview-copy")));
        }
    }

    fn ensure_preview_viewport(&mut self) {
        if self.preview_viewport.is_some() {
            return;
        }
        if !self.config.preview_visible
            || self.visible_preview_rows == 0
            || self.keybinding_help_visible
            || !self.current_results()
        {
            return;
        }
        let mut copy = None;
        if let Some(worker) = &self.preview {
            if let Ok(Some(document)) = worker.copy_document() {
                copy = crate::preview_copy::PreviewCopyMode::host(
                    document,
                    worker.document_scroll_offset,
                    self.visible_preview_rows,
                );
            }
        }
        if copy.is_none() {
            let document = self.external_document.as_ref().or_else(|| {
                self.preview
                    .as_ref()
                    .filter(|worker| !worker.loading)
                    .and_then(|worker| worker.document.as_ref())
            });
            if let Some(Ok(document)) = document {
                copy = crate::preview_copy::PreviewCopyMode::text(
                    document,
                    self.preview
                        .as_ref()
                        .map_or(0, |worker| worker.document_scroll_offset),
                    self.visible_preview_rows,
                );
            }
        }
        if let Some(viewport) = &mut copy {
            viewport.set_scroll_padding(self.cursor_scroll_padding);
        }
        self.preview_viewport = copy;
    }

    pub fn keybinding_help_visible(&self) -> bool {
        self.keybinding_help_visible
    }
    pub fn preview_visible(&self) -> bool {
        self.config.preview_visible
    }
    /// Update presentation when the host switches between picker views.
    pub fn set_presentation(
        &mut self,
        result_rows: usize,
        preview_rows: usize,
        preview_visible: bool,
    ) {
        self.config.result_rows = result_rows.clamp(1, 256);
        self.config.preview_rows = preview_rows.clamp(1, 256);
        if !preview_visible {
            self.leave_preview_focus();
        }
        self.config.preview_visible = preview_visible;
    }
    pub fn set_keybinding_help(&mut self, bindings: Vec<(String, String)>) {
        self.additional_keybinding_help = bindings;
    }
    fn keybinding_help_ui(&mut self, ctx: &Context, bounds: egui::Rect, appearance: &Appearance) {
        let mut bindings: Vec<(String, String)> = [
            ("Enter", "Select / enter directory"),
            ("Escape", "Cancel / dismiss suggestions"),
            ("Up / Down", "Select result / suggestion"),
            ("Tab", "Complete suggestion"),
            ("Left / Right", "Move query cursor"),
            ("Ctrl+Left / Right", "Move cursor by word"),
            ("Shift+Left / Right", "Select query text"),
            ("Home / End", "Query start / end"),
            ("Backspace / Delete", "Delete query character"),
            ("Ctrl+Backspace / Delete", "Delete query word"),
            ("Ctrl+C", "Copy selection"),
            ("Ctrl+Shift+C", "Copy document (preview focus)"),
            ("Ctrl+V", "Paste query text"),
            ("Ctrl+P", "Toggle preview"),
            ("Ctrl+W", "Focus preview / query"),
            ("h j k l / arrows", "Move preview copy cursor"),
            (
                "v / Shift+V / Ctrl+Q",
                "Select characters / lines / rectangle",
            ),
            ("y / yy / yiw", "Yank preview selection / line / word"),
            ("Alt+Space", "Quick Select preview"),
            ("/ / ? / n / Shift+N", "Search preview / next / previous"),
            ("Ctrl+R (search)", "Toggle literal / regex search"),
            ("Ctrl+N / Ctrl+P (search)", "Next / previous match"),
            ("Ctrl+Page Up / Down", "Scroll preview"),
            ("Ctrl+Shift+/", "Toggle key bindings"),
        ]
        .into_iter()
        .map(|(key, description)| (key.into(), description.into()))
        .collect();
        bindings.extend(self.additional_keybinding_help.clone());
        let row_height = appearance.typography.preview_row_height(ctx);
        let rect = bounds.intersect(ctx.content_rect()).shrink(12.0);
        if !rect.is_positive() {
            return;
        }
        let padding = 9.0;
        self.keybinding_help_rows = (((rect.height() - 2.0 * padding) / row_height).floor()
            as usize)
            .saturating_sub(2)
            .max(1)
            .min(bindings.len());
        self.keybinding_help_offset = self
            .keybinding_help_offset
            .min(bindings.len().saturating_sub(self.keybinding_help_rows));
        let height = ((self.keybinding_help_rows + 2) as f32 * row_height + 2.0 * padding)
            .min(rect.height());
        let panel = egui::Rect::from_center_size(rect.center(), egui::vec2(rect.width(), height));
        // Fixed rows avoid label wrapping and inherited widget spacing exceeding
        // the row budget when preview visibility changes the picker height.
        let painter = ctx
            .layer_painter(egui::LayerId::new(
                egui::Order::Tooltip,
                self.id.with("keybinding-help"),
            ))
            .with_clip_rect(panel);
        painter.rect_filled(panel, 8, appearance.palette.background);
        painter.rect_stroke(
            panel,
            8,
            egui::Stroke::new(1.0, appearance.palette.accent),
            egui::StrokeKind::Inside,
        );
        let content = panel.shrink(padding);
        let center = |row: usize| {
            egui::pos2(
                content.left(),
                content.top() + (row as f32 + 0.5) * row_height,
            )
        };
        painter.text(
            center(0),
            egui::Align2::LEFT_CENTER,
            "Key bindings",
            appearance.typography.bold.clone(),
            appearance.palette.accent,
        );
        painter.text(
            center(1),
            egui::Align2::LEFT_CENTER,
            "Up/Down, Page Up/Down, Home/End to scroll; Escape to close",
            appearance.typography.normal.clone(),
            appearance.palette.muted,
        );
        for (row, (key, description)) in bindings
            .iter()
            .skip(self.keybinding_help_offset)
            .take(self.keybinding_help_rows)
            .enumerate()
        {
            painter.text(
                center(row + 2),
                egui::Align2::LEFT_CENTER,
                format!("{key:<26} {description}"),
                appearance.typography.normal.clone(),
                appearance.palette.text,
            );
        }
    }
    pub fn notify_copied(&mut self, ctx: &Context) {
        self.copy_notice = "Copied".into();
        self.copied_until = Some(Instant::now() + Duration::from_secs(2));
        ctx.request_repaint();
        ctx.request_repaint_after(Duration::from_secs(2));
    }
    pub fn is_closed(&self) -> bool {
        self.closed
    }
    pub fn close(&mut self) {
        self.leave_preview_focus();
        self.closed = true;
        self.session.stop();
        if let Some(preview) = &self.preview {
            preview.stop();
        }
    }
    fn current_results(&self) -> bool {
        self.displayed
            .as_ref()
            .is_some_and(|d| d.revision == self.revision && d.query == self.query)
    }
    fn displayed_selection(&self) -> Option<Selection<S::Item>> {
        let displayed = self.displayed.as_ref()?;
        let result = displayed.output.results.get(self.selected)?;
        Some(Selection {
            item: displayed.snapshot.item(result.node_index)?,
            text: result.path.clone(),
            index: result.node_index,
            source_version: displayed.snapshot.version(),
            snapshot: displayed.snapshot.clone(),
        })
    }
    /// Monotonic timestamps for the initial search and first nonempty result.
    pub fn search_timings(&self) -> crate::SearchTimings {
        self.session.timings()
    }
    /// Optional host wake path independent of egui's coalesced repaint callback.
    pub fn set_search_wake_callback(
        &self,
        callback: Arc<dyn Fn(crate::SearchTimings) + Send + Sync>,
    ) {
        self.session.set_wake_callback(callback);
    }
    pub fn set_bindings(&mut self, bindings: Vec<PickerBinding>) {
        self.config.bindings = bindings;
    }
    fn double_click_action(&self) -> Option<usize> {
        self.config
            .bindings
            .iter()
            .find(|binding| binding.trigger == PickerTrigger::DoubleClick)
            .map(|binding| binding.action)
    }

    pub fn selection(&self) -> Option<Selection<S::Item>> {
        self.current_results()
            .then(|| self.displayed_selection())
            .flatten()
    }
    fn poll(&mut self) {
        let _poll = crate::timing::Span::new(
            "nfm_poll",
            self.displayed
                .as_ref()
                .map_or(0, |d| d.output.results.len()),
        );
        if let Some(update) = self.session.take()
            && update.revision == self.revision
            && update.query == self.query
            && (!self.transition_pending || update.done || !update.output.results.is_empty())
        {
            if self.transition_pending && update.output.results.is_empty() {
                if let Some(preview) = &mut self.preview {
                    preview.clear();
                }
            }
            self.transition_pending = false;
            let same_query = self
                .displayed
                .as_ref()
                .is_some_and(|d| d.revision == update.revision);
            let stable_indices = self.displayed.as_ref().is_some_and(|d| {
                d.snapshot.version() == update.snapshot.version() || update.append_only
            });
            let old_index = self
                .displayed
                .as_ref()
                .and_then(|d| d.output.results.get(self.selected))
                .map(|r| r.node_index);
            let selection_timer =
                crate::timing::Span::new("nfm_selection_restore", update.output.results.len());
            self.selected = if same_query && stable_indices {
                old_index
                    .and_then(|index| {
                        update
                            .output
                            .results
                            .iter()
                            .position(|r| r.node_index == index)
                    })
                    .unwrap_or(0)
            } else {
                0
            };
            drop(selection_timer);
            let new_index = update
                .output
                .results
                .get(self.selected)
                .map(|r| r.node_index);
            if old_index != new_index || !same_query || !stable_indices {
                self.leave_preview_focus();
            }
            let snapshot_timer = crate::timing::Span::new(
                "nfm_completion_snapshot_replace",
                update.output.results.len(),
            );
            self.completion_snapshot = Some(update.snapshot.clone());
            drop(snapshot_timer);
            let count = self
                .displayed
                .as_ref()
                .map_or(0, |d| d.output.results.len());
            let old = self.displayed.replace(update);
            let disposal_timer = crate::timing::Span::new("nfm_old_update_drop", count);
            drop(old);
            drop(disposal_timer);
            self.keep_selected_visible();
        }
        if let Some(preview) = &mut self.preview {
            let loading = preview.loading;
            preview.poll();
            if loading && !preview.loading {
                if !self.preview_focus {
                    self.preview_viewport = None;
                }
            }
            if loading && !preview.loading && preview.document.as_ref().is_some_and(|d| d.is_ok()) {
                self.scroll_offset = preview.document_scroll_offset;
                if let Some(key) = &mut self.preview_key {
                    key.4 = self.scroll_offset;
                }
            }
        }
    }
    fn keep_selected_visible(&mut self) {
        if self.selected < self.visible_start {
            self.visible_start = self.selected;
        } else if self.selected >= self.visible_start + self.visible_result_rows {
            self.visible_start = self.selected + 1 - self.visible_result_rows;
        }
    }
    fn move_selection(&mut self, delta: isize) {
        let len = self
            .displayed
            .as_ref()
            .map_or(0, |d| d.output.results.len());
        if len > 0 {
            let next = self.selected.saturating_add_signed(delta).min(len - 1);
            if next != self.selected {
                self.leave_preview_focus();
                self.selected = next;
                self.scroll_offset = 0;
                if let Some(preview) = &mut self.preview {
                    preview.document_selection = None;
                }
                self.keep_selected_visible();
            }
        }
    }
    fn scroll_preview(&mut self, direction: isize) {
        let rows = self.visible_preview_rows.max(1);
        if let Some(viewport) = &mut self.preview_viewport {
            let step = self.preview.as_ref().map_or(rows, |p| p.page_step(rows));
            viewport.scroll(direction.saturating_mul(step as isize), rows);
            self.scroll_offset = viewport.top as usize;
            return;
        }
        let Some(Ok(document)) = self.preview.as_ref().and_then(|p| p.document.as_ref()) else {
            return;
        };
        let total = match document {
            Preview::Text(text) => text.lines().count(),
            Preview::StyledText(lines) => lines.len(),
            Preview::Image(_) => 0,
            Preview::Grid { total_rows, .. } => *total_rows,
            Preview::Empty => 0,
        };
        self.scroll_offset = self
            .scroll_offset
            .saturating_add_signed(
                direction * self.preview.as_ref().unwrap().page_step(rows) as isize,
            )
            .min(total.saturating_sub(rows));
    }
    fn refresh_preview(&mut self, width: f32, appearance: &Appearance) {
        let _timer = crate::timing::Span::new("nfm_preview_prepare", self.visible_result_rows);
        if self.preview_viewport.is_some() {
            return;
        }
        if !self.current_results() {
            return;
        }
        let Some(selection) = self.selection() else {
            if self.preview_key.take().is_some()
                && let Some(preview) = &mut self.preview
            {
                preview.clear();
            }
            return;
        };
        let Some(preview) = &mut self.preview else {
            return;
        };
        let columns = (width / appearance.typography.cell_width.max(1.0))
            .floor()
            .clamp(1.0, 4096.0) as u16;
        let rows = self.visible_preview_rows.clamp(1, 256) as u16;
        let foreground = appearance.palette.text;
        let background = appearance.palette.background;
        // Append-only publications do not change an existing selected item's
        // preview. Avoid restarting it for every incoming candidate batch.
        let version = if self.displayed.as_ref().is_some_and(|d| d.append_only) {
            0
        } else {
            selection.source_version
        };
        let key = (
            version,
            selection.index,
            columns,
            rows,
            self.scroll_offset,
            u32::from_le_bytes(foreground.to_array()),
            u32::from_le_bytes(background.to_array()),
        );
        if self.preview_key != Some(key) {
            self.preview_key = Some(key);
            preview.request(PreviewRequest {
                initial_page: preview.document_selection != Some((version, selection.index)),
                document_version: version,
                selection,
                columns,
                rows,
                scroll_offset: self.scroll_offset,
                foreground,
                background,
            });
        }
    }
    fn busy(&self) -> bool {
        !self.closed
            && (!self.current_results()
                || self.displayed.as_ref().is_none_or(|d| !d.done)
                || (self.visible_preview_rows > 0
                    && (self.external_loading || self.preview.as_ref().is_some_and(|p| p.loading))))
    }
    fn preview_ui(&mut self, ui: &mut egui::Ui, appearance: &Appearance) {
        // Allocate every row, including loading/empty/short documents, so the
        // query and selected row never move when asynchronous content arrives.
        let height =
            self.visible_preview_rows as f32 * appearance.typography.preview_row_height(ui.ctx());
        self.preview_rect = Some(
            egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), height))
                .intersect(ui.clip_rect()),
        );
        ui.allocate_ui_with_layout(
            egui::vec2(ui.available_width(), height),
            egui::Layout::top_down(egui::Align::LEFT),
            |ui| {
                ui.set_min_height(height);
                ui.set_max_height(height);
                self.ensure_preview_viewport();
                if let Some(copy) = &mut self.preview_viewport {
                    self.preview_rendered = true;
                    copy.paint_layers(
                        ui,
                        appearance,
                        self.visible_preview_rows,
                        self.preview_focus,
                    );
                    self.scroll_offset = copy.top as usize;
                    return;
                }
                let document = self.external_document.as_ref().or_else(|| {
                    self.preview
                        .as_ref()
                        .and_then(|worker| worker.document.as_ref())
                });
                if let Some(document) = document {
                    match document {
                        Ok(document) => {
                            self.preview_rendered = true;
                            preview::paint(
                                ui,
                                document,
                                appearance,
                                self.visible_preview_rows,
                                self.preview
                                    .as_ref()
                                    .map_or(0, |worker| worker.document_scroll_offset),
                            );
                        }
                        Err(error) => {
                            ui.colored_label(appearance.palette.error, error);
                        }
                    }
                }
            },
        );
    }
    fn results_ui(
        &mut self,
        ui: &mut egui::Ui,
        appearance: &Appearance,
        accept: &mut bool,
        double_click: &mut bool,
    ) {
        let _timer = crate::timing::Span::new("nfm_result_rows", self.visible_result_rows);
        let row_height = appearance.typography.preview_row_height(ui.ctx());
        let mut first_rect = None;
        for slot in 0..self.visible_result_rows {
            let index = if self.config.layout == Layout::QueryBottom {
                self.visible_start + self.visible_result_rows - 1 - slot
            } else {
                self.visible_start + slot
            };
            let (rect, _) = ui.allocate_exact_size(
                egui::vec2(ui.available_width(), row_height),
                egui::Sense::hover(),
            );
            first_rect.get_or_insert(rect);
            let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
            if let Some(displayed) = &self.displayed
                && let Some(result) = displayed.output.results.get(index)
            {
                let response =
                    ui.interact(rect, self.id.with(("row", index)), egui::Sense::click());
                if index == self.selected {
                    painter.rect_filled(rect, 4, appearance.palette.selection_background);
                    painter.rect_filled(
                        egui::Rect::from_center_size(
                            egui::pos2(rect.left() + 1.5, rect.center().y),
                            egui::vec2(3.0, 16.0),
                        ),
                        1.5,
                        appearance.palette.accent,
                    );
                }
                let text_timer = crate::timing::Span::new("nfm_result_text", 1);
                let display = displayed
                    .snapshot
                    .display_text(result.node_index, &displayed.query)
                    .unwrap_or_else(|| result.path.clone());
                drop(text_timer);
                let match_timer =
                    crate::timing::Span::new("nfm_result_match_layout", display.len());
                let job = result_layout(
                    &display,
                    &displayed.snapshot.effective_query(&displayed.query),
                    index == self.selected,
                    appearance,
                );
                drop(match_timer);
                let font_timer = crate::timing::Span::new("nfm_result_font_layout", display.len());
                let galley = painter.layout_job(job);
                drop(font_timer);
                painter.galley(
                    egui::pos2(rect.left() + 12.0, rect.center().y - galley.size().y / 2.0),
                    galley,
                    appearance.palette.text,
                );
                response.widget_info(|| {
                    egui::WidgetInfo::selected(
                        egui::WidgetType::Button,
                        ui.is_enabled(),
                        index == self.selected,
                        &display,
                    )
                });
                if response.clicked() {
                    self.leave_preview_focus();
                    self.selected = index;
                    self.focus_query = true;
                    self.scroll_offset = 0;
                    if let Some(preview) = &mut self.preview {
                        preview.document_selection = None;
                    }
                    if self.config.accept_on_click && self.double_click_action().is_none() {
                        *accept = true;
                    }
                }
                if response.double_clicked() {
                    self.selected = index;
                    if self.double_click_action().is_some() {
                        *double_click = true;
                    } else {
                        *accept = true;
                    }
                }
            } else if slot == self.visible_result_rows - 1
                && self
                    .displayed
                    .as_ref()
                    .is_none_or(|d| d.output.results.is_empty())
            {
                let text = self.error.as_deref().unwrap_or(
                    if self.current_results() && self.displayed.as_ref().is_some_and(|d| d.done) {
                        "No matches"
                    } else {
                        " "
                    },
                );
                painter.text(
                    rect.left_center(),
                    egui::Align2::LEFT_CENTER,
                    text,
                    appearance.typography.normal.clone(),
                    if self.error.is_some() {
                        appearance.palette.error
                    } else {
                        appearance.palette.muted
                    },
                );
            }
        }
        if self
            .copied_until
            .is_some_and(|until| until > Instant::now())
            && let Some(top) = first_rect
        {
            let galley = ui.painter().layout_no_wrap(
                self.copy_notice.clone(),
                appearance.typography.normal.clone(),
                appearance.palette.success,
            );
            let toast = egui::Rect::from_min_size(
                egui::pos2(top.right() - galley.size().x - 20.0, top.top() + 2.0),
                galley.size() + egui::vec2(16.0, 6.0),
            );
            ui.painter()
                .rect_filled(toast, 3, appearance.palette.selection_background);
            ui.painter().galley(
                toast.min + egui::vec2(8.0, 3.0),
                galley,
                appearance.palette.success,
            );
        }
        let hovered = first_rect.is_some_and(|first| {
            ui.rect_contains_pointer(egui::Rect::from_min_size(
                first.min,
                egui::vec2(first.width(), self.visible_result_rows as f32 * row_height),
            ))
        });
        let wheel = ui.input(|input| {
            if hovered {
                input.smooth_scroll_delta.y
            } else {
                0.0
            }
        });
        if ui.is_enabled() && wheel.abs() > 0.0 {
            self.move_selection(if wheel > 0.0 { 1 } else { -1 });
        }
    }
    fn query_ui(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &Context,
        input_id: Id,
        appearance: &Appearance,
        complete: bool,
        interactive: bool,
    ) {
        let _timer = crate::timing::Span::new("nfm_query_ui", self.visible_result_rows);
        let before = self.query.clone();
        let row_height = appearance.typography.preview_row_height(ctx);
        ui.horizontal(|ui| {
            let prompt_width =
                ui.fonts_mut(|fonts| fonts.glyph_width(&appearance.typography.normal, '>'));
            let (prompt_rect, _) =
                ui.allocate_exact_size(egui::vec2(prompt_width, row_height), egui::Sense::hover());
            ui.painter().text(
                prompt_rect.left_center(),
                egui::Align2::LEFT_CENTER,
                ">",
                appearance.typography.normal.clone(),
                appearance.palette.accent,
            );
            let total = self.displayed.as_ref().map_or(0, |d| d.output.total);
            let matched = if self.current_results() {
                self.displayed.as_ref().map_or(0, |d| d.output.matched)
            } else {
                0
            };
            let count = format!("{matched}/{total}");
            let count_size = ui
                .painter()
                .layout_no_wrap(
                    format!("{total}/{total}"),
                    appearance.typography.normal.clone(),
                    appearance.palette.text,
                )
                .size();
            let status_width = count_size.x + 24.0;
            let query_width =
                (ui.available_width() - status_width - ui.spacing().item_spacing.x).max(1.0);
            let query_interactive = interactive && !self.preview_focused();
            let edit = egui::TextEdit::singleline(&mut self.query)
                .id(input_id)
                .interactive(query_interactive)
                .font(appearance.typography.normal.clone())
                .vertical_align(egui::Align::Center)
                .margin(0)
                .min_size(egui::vec2(query_width, row_height))
                .desired_width(query_width)
                .frame(egui::Frame::NONE)
                .event_filter(egui::EventFilter {
                    tab: true,
                    escape: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                })
                .show(ui);
            if interactive && self.focus_query && !self.preview_focused() {
                edit.response.request_focus();
                self.focus_query = false;
            }
            if before != self.query {
                let query = std::mem::replace(&mut self.query, before.clone());
                self.set_query(query);
            }
            let cursor = edit.cursor_range.map_or(self.query.len(), |range| {
                self.query
                    .char_indices()
                    .nth(range.primary.index.into())
                    .map_or(self.query.len(), |(offset, _)| offset)
            });
            self.query_cursor = cursor;
            self.query_rect = edit.response.rect;
            self.query_text_pos = edit.galley_pos;
            if !self.preview_focused() {
                self.refresh_completions();
            }
            if complete
                && let Some(completion) = self.suggestions.get(self.suggestion_selected).cloned()
                && self.apply_completion(&completion)
            {
                let cursor = self.query[..completion.replace.start + completion.replacement.len()]
                    .chars()
                    .count();
                let mut state = edit.state.clone();
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(
                        egui::text::CCursor::new(cursor),
                    )));
                state.store(ctx, input_id);
                self.refresh_completions();
            }
            let (status_rect, _) =
                ui.allocate_exact_size(egui::vec2(status_width, row_height), egui::Sense::hover());
            if self.error.is_none() && self.busy() {
                let size = ui
                    .painter()
                    .layout_no_wrap(
                        count.clone(),
                        appearance.typography.normal.clone(),
                        appearance.palette.muted,
                    )
                    .size();
                let center =
                    egui::pos2(status_rect.right() - size.x - 14.0, status_rect.center().y);
                paint_loading_indicator(
                    ui,
                    egui::Rect::from_center_size(center, egui::vec2(12.0, 12.0)),
                    appearance.palette.muted,
                );
            }
            ui.painter().text(
                status_rect.right_center(),
                egui::Align2::RIGHT_CENTER,
                count,
                appearance.typography.normal.clone(),
                appearance.palette.muted,
            );
        });
    }
    /// Draw within an existing host egui pass; no window, GL or platform ownership.
    /// Paint the picker. Disabled controls retain background rendering and
    /// worker updates without consuming input or requesting keyboard focus.
    pub fn show(
        &mut self,
        ctx: &Context,
        placement: Placement,
        appearance: &Appearance,
        disabled: bool,
    ) -> PickerOutput<S::Item> {
        self.show_mode(ctx, placement, appearance, !disabled)
    }
    fn show_mode(
        &mut self,
        ctx: &Context,
        placement: Placement,
        appearance: &Appearance,
        interactive: bool,
    ) -> PickerOutput<S::Item> {
        let _show = crate::timing::Span::new(
            "nfm_show",
            self.displayed
                .as_ref()
                .map_or(0, |d| d.output.results.len()),
        );
        self.poll();
        if self
            .preview_viewport
            .as_ref()
            .is_some_and(|copy| !copy.valid())
        {
            self.leave_preview_focus();
        }
        self.refresh_completions();
        let input_timer = crate::timing::Span::new("nfm_input_dispatch", self.query.len());
        let input_id = self.id.with("query");
        let focus_id = if self.preview_focused() {
            self.id.with("preview-copy")
        } else {
            input_id
        };
        // The host decides which pane is interactive. Restore query focus before
        // processing input, including the first frame after a pane is uncovered.
        ctx.memory_mut(|memory| {
            if interactive && !self.closed {
                memory.request_focus(focus_id);
            } else {
                if memory.has_focus(input_id) {
                    memory.surrender_focus(input_id);
                }
                if memory.has_focus(self.id.with("preview-copy")) {
                    memory.surrender_focus(self.id.with("preview-copy"));
                }
            }
        });
        let focused = ctx.memory(|m| {
            m.has_focus(input_id) || (m.focused().is_none() && m.had_focus_last_frame(input_id))
        }) || self.focus_query
            || self.preview_focused();
        let mut event = None;
        let mut accept = false;
        let mut double_click = false;
        let mut copy = false;
        let mut complete = false;
        let direction = if self.config.layout == Layout::QueryBottom {
            -1
        } else {
            1
        };
        let mut custom_action = None;
        if interactive && !self.closed && focused && !self.keybinding_help_visible {
            ctx.input_mut(|input| {
                let mut events = std::mem::take(&mut input.events).into_iter().peekable();
                while let Some(event) = events.next() {
                    let binding = if let egui::Event::Key { key, modifiers, .. } = &event {
                        self.config.bindings.iter().find(|binding| {
                            binding.trigger == PickerTrigger::Key(*key)
                                && (
                                    binding.modifiers.ctrl,
                                    binding.modifiers.shift,
                                    binding.modifiers.alt,
                                    binding.modifiers.mac_cmd,
                                ) == (
                                    modifiers.ctrl,
                                    modifiers.shift,
                                    modifiers.alt,
                                    modifiers.mac_cmd,
                                )
                        })
                    } else {
                        None
                    };
                    if let Some(binding) = binding {
                        let egui::Event::Key {
                            pressed, repeat, ..
                        } = event
                        else {
                            unreachable!()
                        };
                        if pressed && !repeat && custom_action.is_none() {
                            custom_action = Some(binding.action);
                        }
                        if pressed && matches!(events.peek(), Some(egui::Event::Text(_))) {
                            events.next();
                        }
                    } else {
                        input.events.push(event);
                    }
                }
            });
        }
        let mut preview_handled = false;
        if interactive && !self.closed && focused {
            if !self.keybinding_help_visible
                && ctx.input_mut(|input| input.consume_key(Modifiers::ALT, Key::Space))
            {
                // Some hosts emit a text space alongside Alt+Space. It belongs
                // to the shortcut and must never edit the query on cancellation.
                ctx.input_mut(|input| {
                    input
                        .events
                        .retain(|event| !matches!(event, egui::Event::Text(text) if text == " "))
                });
                self.ensure_preview_viewport();
                let from_copy = self.preview_focus;
                if let Some(viewport) = &mut self.preview_viewport {
                    if viewport.quick_select_active() {
                        if viewport.cancel_quick_select() {
                            self.deactivate_preview_focus();
                        }
                    } else if viewport.open_quick_select(from_copy, self.visible_preview_rows) {
                        self.preview_focus = true;
                        self.focus_query = false;
                        self.suggestions.clear();
                        ctx.memory_mut(|memory| memory.request_focus(self.id.with("preview-copy")));
                    }
                }
            }
            if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::W)) {
                self.toggle_preview_focus(ctx);
            }
            if self.preview_focused() && !self.keybinding_help_visible {
                let help = ctx.input_mut(|input| {
                    input.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::Slash)
                        || input.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::Questionmark)
                });
                if help {
                    self.keybinding_help_visible = true;
                    self.keybinding_help_offset = 0;
                } else {
                    let hide = ctx.input_mut(|input| {
                        input.consume_key(Modifiers::CTRL, Key::P)
                            || input.consume_key(Modifiers::CTRL, Key::Space)
                    });
                    if hide {
                        self.config.preview_visible = false;
                        self.leave_preview_focus();
                        if let Some(preview) = &mut self.preview {
                            preview.clear();
                        }
                        self.preview_key = None;
                    } else {
                        if ctx.input_mut(|input| {
                            input.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::C)
                        }) {
                            self.preview_viewport.as_ref().unwrap().copy_all(ctx);
                            ctx.input_mut(|input| {
                                input
                                    .events
                                    .retain(|event| !matches!(event, egui::Event::Copy))
                            });
                        }
                        let events = ctx.input(|input| input.events.clone());
                        if self.preview_viewport.as_mut().unwrap().input(
                            ctx,
                            &events,
                            self.visible_preview_rows,
                        ) {
                            self.deactivate_preview_focus();
                        }
                    }
                    ctx.input_mut(|input| {
                        input.events.retain(|event| {
                            !matches!(
                                event,
                                egui::Event::Key { .. }
                                    | egui::Event::Text(_)
                                    | egui::Event::Paste(_)
                                    | egui::Event::Copy
                                    | egui::Event::Cut
                            )
                        })
                    });
                    preview_handled = true;
                }
            }
        }
        if interactive && !self.closed && focused && !preview_handled {
            ctx.input_mut(|input| {
                if input.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::Slash)
                    || input.consume_key(Modifiers::CTRL | Modifiers::SHIFT, Key::Questionmark)
                {
                    self.keybinding_help_visible = !self.keybinding_help_visible;
                    self.keybinding_help_offset = 0;
                    self.dismissed_completion = Some((self.query.clone(), self.query_cursor));
                    self.suggestions.clear();
                    self.focus_query = true;
                }
                if self.keybinding_help_visible {
                    if input.consume_key(Modifiers::NONE, Key::Escape) {
                        self.keybinding_help_visible = false;
                        self.focus_query = true;
                    }
                    for (key, delta) in [
                        (Key::ArrowUp, -1),
                        (Key::ArrowDown, 1),
                        (Key::PageUp, -(self.keybinding_help_rows as isize)),
                        (Key::PageDown, self.keybinding_help_rows as isize),
                    ] {
                        if input.consume_key(Modifiers::NONE, key) {
                            self.keybinding_help_offset =
                                self.keybinding_help_offset.saturating_add_signed(delta);
                        }
                    }
                    if input.consume_key(Modifiers::NONE, Key::Home) {
                        self.keybinding_help_offset = 0;
                    }
                    if input.consume_key(Modifiers::NONE, Key::End) {
                        self.keybinding_help_offset = usize::MAX;
                    }
                    input.events.clear();
                    return;
                }
                if input.consume_key(Modifiers::CTRL, Key::C) {
                    copy = true;
                }
                let suggestions_open = !self.suggestions.is_empty();
                if input.consume_key(Modifiers::NONE, Key::Escape) {
                    if suggestions_open {
                        self.dismissed_completion = Some((self.query.clone(), self.query_cursor));
                        self.suggestions.clear();
                        self.suggestion_selected = 0;
                        self.focus_query = true;
                    } else {
                        event = Some(PickerEvent::Cancelled);
                    }
                }
                if input.consume_key(Modifiers::NONE, Key::Enter) {
                    if suggestions_open {
                        complete = true;
                    } else {
                        accept = true;
                    }
                }
                if input.consume_key(Modifiers::NONE, Key::Tab) {
                    complete = true;
                }
                if suggestions_open {
                    if input.consume_key(Modifiers::NONE, Key::ArrowUp) {
                        self.suggestion_selected = self
                            .suggestion_selected
                            .checked_sub(1)
                            .unwrap_or(self.suggestions.len().saturating_sub(1));
                    }
                    if input.consume_key(Modifiers::NONE, Key::ArrowDown)
                        && !self.suggestions.is_empty()
                    {
                        self.suggestion_selected =
                            (self.suggestion_selected + 1) % self.suggestions.len();
                    }
                }
                if input.consume_key(Modifiers::CTRL, Key::P)
                    || input.consume_key(Modifiers::CTRL, Key::Space)
                {
                    self.config.preview_visible = !self.config.preview_visible;
                    if !self.config.preview_visible {
                        if let Some(preview) = &mut self.preview {
                            preview.clear();
                        }
                        self.preview_key = None;
                    }
                }
                if input.consume_key(Modifiers::CTRL, Key::PageUp) {
                    self.scroll_preview(-1);
                }
                if input.consume_key(Modifiers::CTRL, Key::PageDown) {
                    self.scroll_preview(1);
                }
                for (key, modifiers, delta) in [
                    (Key::N, Modifiers::CTRL, 1),
                    (Key::ArrowDown, Modifiers::NONE, direction),
                    (Key::ArrowUp, Modifiers::NONE, -direction),
                    (
                        Key::PageDown,
                        Modifiers::NONE,
                        direction * self.visible_result_rows as isize,
                    ),
                    (
                        Key::PageUp,
                        Modifiers::NONE,
                        -direction * self.visible_result_rows as isize,
                    ),
                ] {
                    if input.consume_key(modifiers, key) {
                        self.move_selection(delta);
                    }
                }

                input.events.retain(|e| {
                    if matches!(e, egui::Event::Copy) {
                        copy = true;
                        false
                    } else {
                        true
                    }
                });
            });
        }
        drop(input_timer);
        let layout_timer = crate::timing::Span::new("nfm_layout", self.visible_result_rows);
        let row_height = appearance.typography.preview_row_height(ctx);
        let available = (placement.bounds.height() - 2.0 * placement.margin.max(0.0)).max(0.0);
        let header = self
            .displayed
            .as_ref()
            .and_then(|displayed| displayed.snapshot.header(&self.query));
        let header_height = if header.is_some() { row_height } else { 0.0 };
        self.visible_result_rows = self.config.result_rows.clamp(1, 100).min(
            ((available - 56.0 - header_height) / row_height - 1.0)
                .floor()
                .max(1.0) as usize,
        );
        let preview_row_height = appearance.typography.preview_row_height(ctx);
        let remaining =
            (available - (self.visible_result_rows + 1) as f32 * row_height - 56.0 - header_height)
                .max(0.0);
        self.preview_rect = None;
        self.preview_rendered = false;
        self.visible_preview_rows =
            if (self.preview.is_some() || self.external_preview) && self.config.preview_visible {
                self.config
                    .preview_rows
                    .clamp(1, 256)
                    .min((remaining / preview_row_height).floor() as usize)
            } else {
                0
            };
        self.keep_selected_visible();
        let has_preview = self.visible_preview_rows > 0;
        if !has_preview {
            self.leave_preview_focus();
        }
        let height = (self.visible_result_rows + 1) as f32 * row_height
            + self.visible_preview_rows as f32 * preview_row_height
            + if has_preview { 56.0 } else { 41.0 }
            + header_height;
        let rect = placement.rect(height);
        if has_preview {
            self.refresh_preview((rect.width() - 38.0).max(1.0), appearance);
        }
        drop(layout_timer);
        let build_timer = crate::timing::Span::new("nfm_build_ui", self.visible_result_rows);
        if !self.closed && rect.is_positive() {
            let previous_size =
                ctx.memory(|memory| memory.area_rect(self.id).map(|rect| rect.size()));
            let area = egui::Area::new(self.id)
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
                    egui::Frame::new()
                        .fill(appearance.palette.background)
                        .stroke(egui::Stroke::new(1.0, appearance.palette.divider))
                        .corner_radius(PANEL_RADIUS)
                        .inner_margin(egui::Margin::symmetric(18, 10))
                        .show(ui, |ui| {
                            ui.set_width((rect.width() - 38.0).max(1.0));
                            ui.spacing_mut().item_spacing.y = 0.0;
                            ui.visuals_mut().override_text_color = Some(appearance.palette.text);
                            ui.visuals_mut().text_cursor.stroke =
                                egui::Stroke::new(2.0, appearance.palette.text);
                            ui.visuals_mut().selection.bg_fill =
                                appearance.palette.selection_background;
                            ui.visuals_mut().selection.stroke.color =
                                appearance.palette.selection_text;
                            if self.config.layout == Layout::QueryTop {
                                self.query_ui(ui, ctx, input_id, appearance, complete, interactive);
                                divider(ui, appearance);
                            }
                            if has_preview {
                                self.preview_ui(ui, appearance);
                                divider(ui, appearance);
                            }
                            if let Some(header) = &header {
                                let (rect, _) = ui.allocate_exact_size(
                                    egui::vec2(ui.available_width(), row_height),
                                    egui::Sense::hover(),
                                );
                                ui.painter()
                                    .with_clip_rect(rect.intersect(ui.clip_rect()))
                                    .text(
                                        rect.left_center() + egui::vec2(12.0, 0.0),
                                        egui::Align2::LEFT_CENTER,
                                        header,
                                        appearance.typography.bold.clone(),
                                        appearance.palette.accent,
                                    );
                                ui.painter()
                                    .with_clip_rect(rect.intersect(ui.clip_rect()))
                                    .line_segment(
                                        [
                                            egui::pos2(rect.left() + 12.0, rect.bottom() - 1.0),
                                            egui::pos2(rect.right() - 12.0, rect.bottom() - 1.0),
                                        ],
                                        egui::Stroke::new(
                                            1.5,
                                            egui::Color32::from_rgb(0xd7, 0x99, 0x21),
                                        ),
                                    );
                            }
                            if interactive {
                                self.results_ui(ui, appearance, &mut accept, &mut double_click);
                            } else {
                                ui.scope_builder(egui::UiBuilder::new().disabled(), |ui| {
                                    self.results_ui(ui, appearance, &mut accept, &mut double_click)
                                });
                            }
                            if self.config.layout == Layout::QueryBottom {
                                divider(ui, appearance);
                                self.query_ui(ui, ctx, input_id, appearance, complete, interactive);
                            }
                        });
                });
            if event.is_none()
                && !accept
                && previous_size.is_none_or(|size| {
                    (size.x - area.response.rect.width()).abs() > 0.5
                        || (size.y - area.response.rect.height()).abs() > 0.5
                })
            {
                ctx.request_discard("resolve shared picker size before presentation");
            }
        }
        if interactive && self.keybinding_help_visible {
            self.keybinding_help_ui(ctx, rect, appearance);
        }
        if interactive
            && !self.closed
            && !self.keybinding_help_visible
            && !self.preview_focused()
            && event.is_none()
        {
            self.completion_ui(ctx, placement.bounds, appearance);
        }
        if event.is_none()
            && let Some(selection) = self.selection()
        {
            if double_click {
                if let Some(action) = self.double_click_action() {
                    event = Some(PickerEvent::CustomAction { action, selection });
                }
            } else if accept {
                event = Some(PickerEvent::Accepted(selection));
            } else if copy {
                event = Some(PickerEvent::CopyRequested(selection));
            } else if let Some(action) = custom_action {
                event = Some(PickerEvent::CustomAction { action, selection });
            }
        }
        if matches!(
            event,
            Some(PickerEvent::Accepted(_)) | Some(PickerEvent::Cancelled)
        ) {
            self.close();
        }
        let busy = self.busy();
        if busy {
            ctx.request_repaint_after(Duration::from_millis(50));
        }
        let frame_selection = match &event {
            Some(PickerEvent::Accepted(_))
            | Some(PickerEvent::CustomAction { .. })
            | Some(PickerEvent::CopyRequested(_)) => None,
            _ => self.selection(),
        };
        drop(build_timer);
        PickerOutput {
            frame_selection,
            preview_rendered: self.preview_rendered,
            preview_selection: self
                .preview
                .as_ref()
                .filter(|worker| !worker.loading)
                .and_then(|worker| worker.document_selection),
            preview_rect: if self.closed || self.keybinding_help_visible {
                None
            } else {
                self.preview_rect
            },
            event,
            wants_keyboard: interactive
                && !self.closed
                && (self.preview_focused() || ctx.memory(|m| m.has_focus(input_id))),
            busy,
            matched: if self.current_results() {
                self.displayed.as_ref().map_or(0, |d| d.output.matched)
            } else {
                0
            },
            total: self.displayed.as_ref().map_or(0, |d| d.output.total),
            rect,
        }
    }
}
fn divider(ui: &mut egui::Ui, appearance: &Appearance) {
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), DIVIDER_HEIGHT),
        egui::Sense::hover(),
    );
    ui.painter().line_segment(
        [rect.left_center(), rect.right_center()],
        egui::Stroke::new(1.5, appearance.palette.divider.gamma_multiply(0.8)),
    );
}
fn paint_loading_indicator(ui: &egui::Ui, rect: egui::Rect, color: egui::Color32) {
    let center = rect.center();
    let dot_size = rect.height() / 4.0;
    let dot_step = (rect.height() - dot_size) / 2.0;
    let phase = (ui.input(|input| input.time) * 12.0) as usize % 8;
    for (dot, (x, y)) in [
        (-1.0, -1.0),
        (0.0, -1.0),
        (1.0, -1.0),
        (1.0, 0.0),
        (1.0, 1.0),
        (0.0, 1.0),
        (-1.0, 1.0),
        (-1.0, 0.0),
    ]
    .into_iter()
    .enumerate()
    {
        let alpha = 1.0 - ((dot + 8 - phase) % 8) as f32 * 0.11;
        ui.painter().rect_filled(
            egui::Rect::from_center_size(
                center + egui::vec2(x * dot_step, y * dot_step),
                egui::vec2(dot_size, dot_size),
            ),
            0.0,
            color.gamma_multiply(alpha),
        );
    }
    ui.ctx().request_repaint_after(Duration::from_millis(80));
}
fn result_layout(
    text: &str,
    query: &str,
    selected: bool,
    appearance: &Appearance,
) -> egui::text::LayoutJob {
    let positions = resolve_match_positions(query, text);
    highlighted_layout(text, &positions, selected, appearance)
}
fn highlighted_layout(
    text: &str,
    positions: &[usize],
    selected: bool,
    appearance: &Appearance,
) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    for (offset, character) in text.char_indices() {
        let matched = positions.contains(&offset);
        job.append(
            &character.to_string(),
            0.0,
            egui::TextFormat {
                font_id: if matched {
                    appearance.typography.bold.clone()
                } else {
                    appearance.typography.normal.clone()
                },
                color: if matched {
                    appearance.palette.match_highlight
                } else if selected {
                    appearance.palette.selection_text
                } else {
                    appearance.palette.text
                },
                ..Default::default()
            },
        );
    }
    job
}
#[cfg(test)]
mod query_parity_tests {
    use super::*;
    use nfm_picker_sources::structured::{
        StructuredSchema, StructuredStreamingSnapshot, StructuredStreamingStore,
    };
    use nfm_search_core::snapshot_store::SnapshotStore;
    fn make() -> (Context, Picker<StructuredStreamingSnapshot>) {
        let ctx = Context::default();
        let mut rows = StructuredStreamingStore::new(
            StructuredSchema::new(vec!["Name".into(), "Status".into(), "CPU".into()]).unwrap(),
        );
        for row in [
            ["alpha one", "Running", "10"],
            ["beta", "Stopped", "20"],
            ["猫 server", "Running", "30"],
        ] {
            rows.add_record(&csv::StringRecord::from(row.to_vec()))
                .unwrap();
        }
        let store = Arc::new(SnapshotStore::new());
        store.publish(rows.snapshot());
        store.complete();
        (
            ctx.clone(),
            Picker::new(
                "query-parity",
                &ctx,
                store,
                PickerConfig {
                    preview_visible: false,
                    ..Default::default()
                },
            ),
        )
    }
    fn wait(picker: &mut Picker<StructuredStreamingSnapshot>) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !picker.current_results() {
            picker.poll();
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn draw(
        ctx: &Context,
        picker: &mut Picker<StructuredStreamingSnapshot>,
        events: Vec<egui::Event>,
    ) -> (
        egui::FullOutput,
        Option<PickerEvent<nfm_picker_sources::structured::StructuredPickerItem>>,
    ) {
        let mut event = None;
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
                let output = picker.show(
                    ui.ctx(),
                    Placement::new(ui.ctx().content_rect()),
                    &Appearance::default(),
                    false,
                );
                if output.event.is_some() {
                    event = output.event;
                }
            },
        );
        frame.textures_delta.clear();
        (frame, event)
    }
    fn key(key: Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }
    fn modified_key(key: Key, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }
    #[test]
    fn alt_space_quick_select_restores_focus_and_selects_without_accepting() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        picker.set_external_preview(true);
        let text = (0..60)
            .map(|row| format!("first{row} second{row}\n"))
            .collect();
        picker.set_external_preview_document(Some(Ok(Preview::Text(text))), false);
        draw(&ctx, &mut picker, vec![]);
        let selected = picker.selected;
        let (_, event) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Space, Modifiers::ALT)],
        );
        assert!(event.is_none());
        assert!(picker.preview_focused());
        assert!(
            picker
                .preview_viewport
                .as_ref()
                .unwrap()
                .quick_select_active()
        );
        draw(&ctx, &mut picker, vec![key(Key::PageDown)]);
        assert!(picker.preview_viewport.as_ref().unwrap().top > 0);
        draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        assert!(!picker.preview_focused());
        assert_eq!(picker.preview_viewport.as_ref().unwrap().top, 0);
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Space, Modifiers::ALT)],
        );
        let (label, target) = picker
            .preview_viewport
            .as_ref()
            .unwrap()
            .first_quick_hint()
            .unwrap();
        let (_, event) = draw(&ctx, &mut picker, vec![egui::Event::Text(label)]);
        assert!(event.is_none());
        assert!(picker.preview_focused());
        assert!(
            !picker
                .preview_viewport
                .as_ref()
                .unwrap()
                .quick_select_active()
        );
        assert_eq!(
            picker.preview_viewport.as_ref().unwrap().mode.cursor,
            target
        );
        assert_eq!(picker.query(), "");
        assert_eq!(picker.selected, selected);
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Space, Modifiers::ALT)],
        );
        assert!(
            picker
                .preview_viewport
                .as_ref()
                .unwrap()
                .quick_select_active()
        );
        draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        assert!(picker.preview_focused());
        assert_eq!(
            picker.preview_viewport.as_ref().unwrap().mode.cursor,
            target
        );
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        assert!(!picker.preview_focused());
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Space, Modifiers::ALT)],
        );
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Space, Modifiers::ALT)],
        );
        assert!(!picker.preview_focused());
    }

    #[test]
    fn leaving_preview_focus_keeps_viewport_and_results_paging_uses_it() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        picker.set_external_preview(true);
        let text = (0..100).map(|row| format!("row{row:03}\n")).collect();
        picker.set_external_preview_document(Some(Ok(Preview::Text(text))), false);
        draw(&ctx, &mut picker, vec![]);
        assert!(!picker.preview_focused());
        assert!(picker.preview_viewport.is_some());
        picker.toggle_preview_focus(&ctx);
        picker.preview_viewport.as_mut().unwrap().top = 40;
        picker.preview_viewport.as_mut().unwrap().mode.cursor.y = 40;
        let document = picker.preview_viewport.as_ref().unwrap() as *const _;
        picker.toggle_preview_focus(&ctx);
        assert!(!picker.preview_focused());
        assert_eq!(picker.scroll_offset, 40);
        draw(&ctx, &mut picker, vec![]);
        assert_eq!(picker.preview_viewport.as_ref().unwrap().top, 40);
        assert_eq!(
            picker.preview_viewport.as_ref().unwrap() as *const _,
            document
        );
        picker.scroll_preview(1);
        let top = picker.preview_viewport.as_ref().unwrap().top;
        assert!(top > 40);
        picker.toggle_preview_focus(&ctx);
        draw(&ctx, &mut picker, vec![]);
        assert!(picker.preview_focused());
        assert_eq!(picker.preview_viewport.as_ref().unwrap().top, top);
    }

    #[test]
    fn ctrl_w_toggles_only_once_when_egui_repeats_a_layout_pass() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        picker.set_external_preview(true);
        picker.set_external_preview_document(Some(Ok(Preview::Text("preview text".into()))), false);
        draw(&ctx, &mut picker, vec![]);
        for focused in [true, false] {
            let mut passes = 0;
            let mut output = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1600.0, 900.0),
                    )),
                    events: vec![modified_key(Key::W, Modifiers::CTRL)],
                    ..Default::default()
                },
                |ui| {
                    picker.show(
                        ui.ctx(),
                        Placement::new(ui.ctx().content_rect()),
                        &Appearance::default(),
                        false,
                    );
                    passes += 1;
                    if passes == 1 {
                        ui.ctx().request_discard("focus regression test");
                    }
                },
            );
            output.textures_delta.clear();
            assert!(passes >= 2);
            assert_eq!(picker.preview_focused(), focused);
        }
    }

    #[test]
    fn ctrl_w_focuses_preview_yanks_and_returns_to_query_without_accepting() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        picker.set_external_preview(true);
        picker.set_external_preview_document(
            Some(Ok(Preview::Text("alpha beta\nsecond line\nlast".into()))),
            false,
        );
        draw(&ctx, &mut picker, vec![]);
        let selected = picker.selected;
        let (_, event) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        assert!(picker.preview_focused());
        assert!(event.is_none());
        // Hosts may publish the same external document on every frame.
        picker.set_external_preview_document(
            Some(Ok(Preview::Text("alpha beta\nsecond line\nlast".into()))),
            false,
        );
        assert!(picker.preview_focused());
        draw(
            &ctx,
            &mut picker,
            vec![key(Key::W), egui::Event::Text("w".into())],
        );
        assert_eq!(picker.preview_viewport.as_ref().unwrap().mode.cursor.x, 6);
        assert_eq!(picker.query(), "");
        assert_eq!(picker.selected, selected);
        let (frame, event) = draw(
            &ctx,
            &mut picker,
            vec![key(Key::Y), key(Key::I), key(Key::W)],
        );
        assert!(event.is_none());
        assert!(
            frame
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText("beta".into()))
        );
        draw(&ctx, &mut picker, vec![key(Key::V), key(Key::ArrowRight)]);
        let (frame, _) = draw(&ctx, &mut picker, vec![key(Key::Y)]);
        assert!(
            frame
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText("be".into()))
        );
        // Enter while preview-focused never accepts the picker item.
        assert!(draw(&ctx, &mut picker, vec![key(Key::Enter)]).1.is_none());
        assert!(!picker.is_closed());
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        assert!(!picker.preview_focused());
        draw(&ctx, &mut picker, vec![egui::Event::Text("typed".into())]);
        assert_eq!(picker.query(), "typed");
        assert!(!picker.is_closed());
    }

    #[test]
    fn results_query_and_preview_share_padded_font_height() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        picker.set_external_preview(true);
        picker.set_external_preview_document(Some(Ok(Preview::Text("one\ntwo".into()))), false);
        let mut appearance = Appearance::default();
        appearance.typography.normal = egui::FontId::monospace(15.0);
        appearance.typography.bold = egui::FontId::monospace(15.0);
        appearance.typography.row_height = 30.0;
        let mut bounds = None;
        let mut line_height = 0.0;
        let mut frame = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 800.0),
                )),
                ..Default::default()
            },
            |ui| {
                line_height = ui.fonts_mut(|fonts| fonts.row_height(&appearance.typography.normal));
                bounds = picker
                    .show(
                        ui.ctx(),
                        Placement::new(ui.ctx().content_rect()),
                        &appearance,
                        false,
                    )
                    .preview_rect;
            },
        );
        frame.textures_delta.clear();
        let height = bounds.unwrap().height();
        assert!(
            (height / picker.visible_preview_rows as f32 - (line_height + 6.0)).abs()
                <= 0.51 / ctx.pixels_per_point(),
            "preview height={height}, rows={}, font height={line_height}",
            picker.visible_preview_rows
        );
        assert!(height < picker.visible_preview_rows as f32 * 30.0);
        assert!(
            (picker.query_rect.height() - height / picker.visible_preview_rows as f32).abs() < 1.0
        );
    }

    #[test]
    fn preview_search_escape_hide_and_missing_preview_preserve_picker_focus() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        assert!(!picker.preview_focused());
        picker.config.preview_visible = true;
        picker.set_external_preview(true);
        picker.set_external_preview_document(
            Some(Ok(Preview::Text("one two\nthree two".into()))),
            false,
        );
        draw(&ctx, &mut picker, vec![]);
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        draw(
            &ctx,
            &mut picker,
            vec![key(Key::Slash), egui::Event::Text("/".into())],
        );
        draw(
            &ctx,
            &mut picker,
            vec![egui::Event::Text("two".into()), key(Key::Enter)],
        );
        assert_eq!(
            picker.preview_viewport.as_ref().unwrap().mode.cursor,
            crate::copy_mode::CellPosition { x: 4, y: 0 }
        );
        draw(&ctx, &mut picker, vec![key(Key::N)]);
        assert_eq!(
            picker.preview_viewport.as_ref().unwrap().mode.cursor,
            crate::copy_mode::CellPosition { x: 6, y: 1 }
        );
        assert_eq!(picker.query(), "");
        draw(&ctx, &mut picker, vec![key(Key::Escape)]); // Clear search.
        assert!(picker.preview_focused());
        draw(&ctx, &mut picker, vec![key(Key::Escape)]); // Return to query.
        assert!(!picker.preview_focused());
        assert!(!picker.is_closed());
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::P, Modifiers::CTRL)],
        );
        assert!(!picker.preview_focused());
        assert!(!picker.preview_visible());
        assert!(!picker.is_closed());
    }
    #[test]
    fn query_edit_keeps_preview_shortcuts_working_when_selected_item_is_unchanged() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        let resolutions = Arc::new(AtomicUsize::new(0));
        let calls = resolutions.clone();
        picker.set_preview_provider(
            &ctx,
            Arc::new(crate::byte_preview::BytePreviewProvider::new(
                move |_: &Selection<nfm_picker_sources::structured::StructuredPickerItem>, _| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(crate::byte_preview::ByteDocument::complete(
                        b"preview words\nsecond line".to_vec(),
                    ))
                },
            )),
        );
        let settle = |picker: &mut Picker<StructuredStreamingSnapshot>| {
            wait(picker);
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                draw(&ctx, picker, vec![]);
                if !picker.preview.as_ref().unwrap().loading {
                    break;
                }
                assert!(Instant::now() < deadline, "preview did not finish");
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        draw(&ctx, &mut picker, vec![egui::Event::Text("a".into())]);
        settle(&mut picker);
        let selected = picker.selection().unwrap().index;
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        assert!(picker.preview_focused());
        draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        assert!(!picker.preview_focused());

        draw(&ctx, &mut picker, vec![egui::Event::Text("l".into())]);
        assert_eq!(picker.query(), "al");
        settle(&mut picker);
        assert_eq!(picker.selection().unwrap().index, selected);
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        assert!(
            picker.preview_focused(),
            "Ctrl+W stopped reopening the unchanged preview"
        );
        draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        assert!(!picker.preview_focused());
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Space, Modifiers::ALT)],
        );
        assert!(
            picker.preview_focused(),
            "Alt+Space stopped opening quick select"
        );
        assert!(
            picker
                .preview_viewport
                .as_ref()
                .unwrap()
                .quick_select_active()
        );
        assert_eq!(picker.selection().unwrap().index, selected);
        assert_eq!(
            resolutions.load(Ordering::SeqCst),
            1,
            "query edits reran the selected item's preview source"
        );
    }
    #[test]
    fn ansi_preview_copy_focus_pins_document_yanks_soft_wraps_and_survives_resize() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.config.preview_visible = true;
        let provider = Arc::new(crate::byte_preview::BytePreviewProvider::new(
            |_: &Selection<nfm_picker_sources::structured::StructuredPickerItem>, _| {
                Ok(crate::byte_preview::ByteDocument::complete(
                    format!("{}猫e\u{301}tail\nlast", "a".repeat(130)).into_bytes(),
                ))
            },
        ));
        picker.set_preview_provider(&ctx, provider.clone());
        let deadline = Instant::now() + Duration::from_secs(5);
        while picker.preview.as_ref().unwrap().document.is_none() {
            draw(&ctx, &mut picker, vec![]);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let (version, index) = picker.preview.as_ref().unwrap().document_selection.unwrap();
        let revision = provider
            .with_ansi_document(version, index, |doc| doc.revision())
            .unwrap()
            .unwrap();
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        assert!(picker.preview_focused());
        let (frame, _) = draw(&ctx, &mut picker, vec![key(Key::Y), key(Key::Y)]);
        assert!(
            frame
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText(format!(
                    "{}猫e\u{301}tail\n",
                    "a".repeat(130)
                )))
        );
        let _ = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(650.0, 800.0),
                )),
                ..Default::default()
            },
            |ui| {
                picker.show(
                    ui.ctx(),
                    Placement::new(ui.ctx().content_rect()),
                    &Appearance::default(),
                    false,
                );
            },
        );
        assert!(picker.preview_focused());
        assert_eq!(
            provider
                .with_ansi_document(version, index, |doc| doc.revision())
                .unwrap(),
            Some(revision)
        );
        let (frame, _) = draw(&ctx, &mut picker, vec![key(Key::Y), key(Key::Y)]);
        assert!(
            frame
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText(format!(
                    "{}猫e\u{301}tail\n",
                    "a".repeat(130)
                )))
        );
        picker.set_query("changed");
        assert!(!picker.preview_focused());
    }

    #[test]
    fn source_transition_retains_rows_and_preview_but_blocks_stale_acceptance() {
        let (ctx, mut picker) = make();
        picker.config.preview_visible = true;
        picker.set_preview_provider(
            &ctx,
            Arc::new(
                |request: PreviewRequest<nfm_picker_sources::structured::StructuredPickerItem>,
                 _: crate::Cancellation| {
                    Ok(Preview::Text(request.selection.item.fields["Name"].clone()))
                },
            ),
        );
        wait(&mut picker);
        let deadline = Instant::now() + Duration::from_secs(5);
        while picker.preview.as_ref().unwrap().document.is_none() {
            draw(&ctx, &mut picker, vec![]);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        let old_preview = picker
            .preview
            .as_ref()
            .unwrap()
            .document
            .clone()
            .unwrap()
            .unwrap();
        let old_text = picker.displayed_selection().unwrap().text;
        // Enter closes the worker; replacing its source must reopen safely.
        let (_, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        assert!(matches!(event, Some(PickerEvent::Accepted(_))));
        let store = Arc::new(SnapshotStore::new());
        store.publish(StructuredStreamingSnapshot::from_rows(vec!["Name".into()], vec![]).unwrap());
        picker.replace_source(&ctx, store.clone(), "");
        draw(&ctx, &mut picker, vec![]);
        assert!(picker.selection().is_none());
        assert_eq!(picker.displayed_selection().unwrap().text, old_text);
        assert_eq!(
            preview::plain_text(
                picker
                    .preview
                    .as_ref()
                    .unwrap()
                    .document
                    .as_ref()
                    .unwrap()
                    .as_ref()
                    .unwrap()
            ),
            preview::plain_text(&old_preview)
        );
        let (_, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        assert!(event.is_none());
        assert!(!picker.is_closed());
        store.publish(
            StructuredStreamingSnapshot::from_rows(
                vec!["Name".into()],
                vec![vec!["replacement".into()]],
            )
            .unwrap(),
        );
        store.complete();
        wait(&mut picker);
        assert_eq!(
            picker.selection().unwrap().item.fields["Name"],
            "replacement"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            draw(&ctx, &mut picker, vec![]);
            if picker
                .preview
                .as_ref()
                .unwrap()
                .document
                .as_ref()
                .and_then(|d| d.as_ref().ok())
                .and_then(preview::plain_text)
                .as_deref()
                == Some("replacement")
            {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        // An empty completed directory must eventually remove retained content.
        let empty = Arc::new(SnapshotStore::new());
        empty.publish(StructuredStreamingSnapshot::from_rows(vec!["Name".into()], vec![]).unwrap());
        empty.complete();
        picker.replace_source(&ctx, empty, "");
        wait(&mut picker);
        draw(&ctx, &mut picker, vec![]);
        assert!(picker.selection().is_none());
        assert!(picker.displayed.as_ref().unwrap().output.results.is_empty());
        assert!(picker.preview.as_ref().unwrap().document.is_none());
    }
    #[test]
    fn shifted_slash_opens_help_without_editing_or_cancelling_the_query() {
        let (ctx, mut picker) = make();
        picker.set_query("beta");
        wait(&mut picker);
        draw(&ctx, &mut picker, vec![]);
        let shifted_slash = egui::Event::Key {
            key: Key::Questionmark,
            physical_key: Some(Key::Slash),
            pressed: true,
            repeat: false,
            modifiers: Modifiers::CTRL | Modifiers::SHIFT,
        };
        let (_, event) = draw(&ctx, &mut picker, vec![shifted_slash.clone()]);
        assert!(event.is_none());
        assert!(picker.keybinding_help_visible());
        assert_eq!(picker.query(), "beta");
        let (_, event) = draw(&ctx, &mut picker, vec![shifted_slash]);
        assert!(event.is_none());
        assert!(!picker.keybinding_help_visible());
        assert_eq!(picker.query(), "beta");
        assert!(!picker.is_closed());
    }
    #[test]
    fn help_rows_fit_after_the_preview_expands_and_collapses() {
        let (ctx, mut picker) = make();
        let mut appearance = Appearance::default();
        appearance.typography.normal = egui::FontId::monospace(15.0);
        appearance.typography.bold = egui::FontId::monospace(15.0);
        appearance.typography.row_height = 30.0;
        picker
            .additional_keybinding_help
            .extend((0..40).map(|row| (format!("Host {row}"), "Host action".into())));
        for (width, height) in [
            (1600.0, 610.0),
            (1600.0, 296.0),
            (600.0, 296.0),
            (1600.0, 610.0),
        ] {
            let bounds = egui::Rect::from_min_size(egui::pos2(8.0, 8.0), egui::vec2(width, height));
            let mut frame = ctx.run_ui(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        bounds.max.to_vec2() + egui::vec2(8.0, 8.0),
                    )),
                    ..Default::default()
                },
                |ui| picker.keybinding_help_ui(ui.ctx(), bounds, &appearance),
            );
            frame.textures_delta.clear();
            assert!(!frame.shapes.is_empty());
            let mut text_rows = 0;
            for shape in &frame.shapes {
                assert!(bounds.contains_rect(shape.clip_rect));
                if let egui::Shape::Text(text) = &shape.shape {
                    text_rows += 1;
                    assert!(text.pos.y >= shape.clip_rect.top());
                    assert!(text.pos.y + text.galley.size().y <= shape.clip_rect.bottom());
                }
            }
            assert_eq!(text_rows, picker.keybinding_help_rows + 2);
            assert!(picker.keybinding_help_rows < 56);
        }
    }
    #[test]
    fn preview_copy_and_help_shortcuts_preserve_the_menu_and_selection() {
        let (ctx, mut picker) = make();
        picker.config.preview_visible = true;
        picker.set_keybinding_help(vec![(
            "Ctrl+U".into(),
            "Parent directory / drive list".into(),
        )]);
        picker.set_preview_provider(
            &ctx,
            Arc::new(
                |_: PreviewRequest<nfm_picker_sources::structured::StructuredPickerItem>,
                 _: crate::Cancellation| {
                    Ok(Preview::StyledText(vec![
                        vec![crate::PreviewCell {
                            text: "colored 猫".into(),
                            ..Default::default()
                        }],
                        vec![crate::PreviewCell {
                            text: "second line".into(),
                            ..Default::default()
                        }],
                    ]))
                },
            ),
        );
        wait(&mut picker);
        let deadline = Instant::now() + Duration::from_secs(5);
        while picker.preview.as_ref().unwrap().document.is_none() {
            draw(&ctx, &mut picker, vec![]);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        picker
            .additional_keybinding_help
            .extend((0..40).map(|index| (format!("Host {index}"), "Host action".into())));
        let selection = picker.selection().unwrap().index;
        let (output, event) = draw(
            &ctx,
            &mut picker,
            vec![
                modified_key(Key::C, Modifiers::CTRL | Modifiers::SHIFT),
                egui::Event::Copy,
            ],
        );
        assert!(matches!(event, Some(PickerEvent::CopyRequested(_))));
        assert!(
            !output
                .platform_output
                .commands
                .iter()
                .any(|command| matches!(command, egui::OutputCommand::CopyText(_)))
        );
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        let (output, event) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::C, Modifiers::CTRL | Modifiers::SHIFT)],
        );
        assert!(event.is_none());
        assert!(output.platform_output.commands.iter().any(|command| matches!(command, egui::OutputCommand::CopyText(text) if text == "colored 猫\nsecond line")));
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        let (_, event) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::C, Modifiers::CTRL)],
        );
        assert!(matches!(event, Some(PickerEvent::CopyRequested(_))));
        let (output, event) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::Slash, Modifiers::CTRL | Modifiers::SHIFT)],
        );
        assert!(event.is_none());
        assert!(picker.keybinding_help_visible());
        assert!(!output.shapes.is_empty());
        draw(&ctx, &mut picker, vec![key(Key::End)]);
        assert!(picker.keybinding_help_offset > 0);
        draw(&ctx, &mut picker, vec![key(Key::Home)]);
        assert_eq!(picker.keybinding_help_offset, 0);
        draw(
            &ctx,
            &mut picker,
            vec![
                key(Key::ArrowDown),
                egui::Event::Text("ignored".into()),
                key(Key::Enter),
            ],
        );
        assert_eq!(picker.selection().unwrap().index, selection);
        assert_eq!(picker.query, "");
        let (_, event) = draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        assert!(event.is_none());
        assert!(!picker.keybinding_help_visible());
        assert!(!picker.is_closed());
        let (_, event) = draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        assert!(matches!(event, Some(PickerEvent::Cancelled)));
    }
    #[test]
    fn typed_command_filter_accepts_the_command_name() {
        let ctx = Context::default();
        let snapshot = StructuredStreamingSnapshot::from_rows(
            ["Name", "Type", "KeyBinding", "Description"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            vec![
                vec![
                    "fixture_first".into(),
                    "Action".into(),
                    "Ctrl+A".into(),
                    "First fixture".into(),
                ],
                vec![
                    "fixture_selected".into(),
                    "Action".into(),
                    "Ctrl+B".into(),
                    "Selected fixture".into(),
                ],
            ],
        )
        .unwrap();
        let store = Arc::new(SnapshotStore::new());
        store.publish(snapshot);
        store.complete();
        let mut picker = Picker::new(
            "swm-commands-test",
            &ctx,
            store,
            PickerConfig {
                preview_visible: false,
                ..Default::default()
            },
        );
        draw(&ctx, &mut picker, vec![]);
        draw(
            &ctx,
            &mut picker,
            vec![egui::Event::Text("/:Name==fixture_selected ".into())],
        );
        wait(&mut picker);
        draw(&ctx, &mut picker, vec![]);
        assert_eq!(picker.query, "/:Name==fixture_selected ");
        assert!(
            picker.suggestions.is_empty(),
            "suggestions {:?}",
            picker.suggestions
        );
        let (_, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        assert!(
            matches!(event, Some(PickerEvent::Accepted(selection)) if selection.item.fields["Name"] == "fixture_selected")
        );
    }
    #[test]
    fn host_document_uses_shared_preview_painter_and_copy_without_worker() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.set_external_preview(true);
        picker.config.preview_visible = true;
        picker.set_external_preview_document(
            Some(Ok(Preview::Text("host preview fixture".into()))),
            false,
        );
        let (frame, _) = draw(&ctx, &mut picker, vec![]);
        let painted: String = frame
            .shapes
            .iter()
            .filter_map(|shape| match &shape.shape {
                egui::Shape::Text(text) => Some(text.galley.text()),
                _ => None,
            })
            .collect();
        assert!(painted.contains("host preview fixture"));
        let (unfocused, _) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::C, Modifiers::CTRL | Modifiers::SHIFT)],
        );
        assert!(
            !unfocused
                .platform_output
                .commands
                .iter()
                .any(|command| matches!(command, egui::OutputCommand::CopyText(_)))
        );
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::W, Modifiers::CTRL)],
        );
        let (frame, _) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::C, Modifiers::CTRL | Modifiers::SHIFT)],
        );
        assert!(
            frame
                .platform_output
                .commands
                .contains(&egui::OutputCommand::CopyText(
                    "host preview fixture".into()
                ))
        );
        assert!(picker.preview.is_none());
        picker.set_external_preview_document(None, true);
        draw(&ctx, &mut picker, vec![]);
        assert!(picker.busy());
        picker.set_external_preview_document(None, false);
        assert!(!picker.busy());
    }
    #[test]
    fn external_preview_bounds_follow_toggle_help_and_close_without_a_worker() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        picker.set_external_preview(true);
        picker.config.preview_visible = true;
        draw(&ctx, &mut picker, vec![]);
        assert!(picker.preview.is_none());
        assert!(picker.preview_rect.unwrap().is_positive());
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::P, Modifiers::CTRL)],
        );
        assert!(picker.preview_rect.is_none());
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::P, Modifiers::CTRL)],
        );
        assert!(picker.preview_rect.unwrap().is_positive());
        picker.keybinding_help_visible = true;
        let mut bounds = Some(egui::Rect::EVERYTHING);
        let mut frame = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 800.0),
                )),
                ..Default::default()
            },
            |ui| {
                bounds = picker
                    .show(
                        ui.ctx(),
                        Placement::new(ui.ctx().content_rect()),
                        &Appearance::default(),
                        false,
                    )
                    .preview_rect;
            },
        );
        frame.textures_delta.clear();
        assert!(bounds.is_none(), "native preview must not cover help");
        picker.close();
        let mut frame = ctx.run_ui(egui::RawInput::default(), |ui| {
            bounds = picker
                .show(
                    ui.ctx(),
                    Placement::new(ui.ctx().content_rect()),
                    &Appearance::default(),
                    false,
                )
                .preview_rect;
        });
        frame.textures_delta.clear();
        assert!(bounds.is_none());
    }
    #[test]
    fn preview_toggle_scroll_and_missing_preview_copy_match_skia() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        draw(&ctx, &mut picker, vec![]);
        let (output, event) = draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::C, Modifiers::CTRL | Modifiers::SHIFT)],
        );
        assert!(matches!(event, Some(PickerEvent::CopyRequested(_))));
        assert!(
            output
                .platform_output
                .commands
                .iter()
                .all(|command| !matches!(command, egui::OutputCommand::CopyText(_)))
        );
        picker.set_preview_provider(
            &ctx,
            Arc::new(
                |_: PreviewRequest<nfm_picker_sources::structured::StructuredPickerItem>,
                 _: crate::Cancellation| {
                    Ok(Preview::Text(
                        (0..100).map(|row| format!("line {row}\n")).collect(),
                    ))
                },
            ),
        );
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::P, Modifiers::CTRL)],
        );
        assert!(picker.config.preview_visible);
        let deadline = Instant::now() + Duration::from_secs(5);
        while picker.preview.as_ref().unwrap().document.is_none() {
            draw(&ctx, &mut picker, vec![]);
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::PageDown, Modifiers::CTRL)],
        );
        assert_eq!(
            picker.scroll_offset,
            picker.visible_preview_rows.saturating_sub(1).max(1)
        );
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::PageUp, Modifiers::CTRL)],
        );
        assert_eq!(picker.scroll_offset, 0);
        draw(
            &ctx,
            &mut picker,
            vec![modified_key(Key::P, Modifiers::CTRL)],
        );
        assert!(!picker.config.preview_visible);
    }
    #[test]
    fn all_skia_expression_operators_sorts_display_and_quoted_values_use_shared_parser() {
        let (_, mut picker) = make();
        for (query, expected) in [
            ("/:Status==Running ", 2),
            ("/:Status!=Stopped ", 2),
            ("/:Name=~^beta ", 1),
            ("/:Name!~^beta ", 2),
            ("/:CPU>20 ", 1),
            ("/:CPU>=20 ", 2),
            ("/:CPU<20 ", 1),
            ("/:CPU<=20 ", 2),
            ("/:Status==Running,Stopped ", 3),
            ("/:Name==\"猫 server\" ", 1),
            ("/:Status==Running /:CPU>=20 ", 1),
            ("server /:Status==Running ", 1),
        ] {
            picker.set_query(query);
            wait(&mut picker);
            assert_eq!(
                picker.displayed.as_ref().unwrap().output.matched,
                expected,
                "{query}"
            );
        }
        for (query, expected) in [("/!Ascending==CPU ", "10"), ("/!Descending==CPU ", "30")] {
            picker.set_query(query);
            wait(&mut picker);
            assert_eq!(picker.selection().unwrap().item.fields["CPU"], expected);
        }
        picker.set_query("/#Display==PID,CPU /!Descending==CPU ");
        wait(&mut picker);
        // Unknown display columns are ignored by the shared parser.
        let displayed = picker.displayed.as_ref().unwrap();
        assert!(displayed.snapshot.header(&picker.query).contains("CPU"));
        assert!(!displayed.snapshot.header(&picker.query).contains("Status"));
    }
    #[test]
    fn popup_navigation_enter_tab_and_escape_match_skia() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        draw(&ctx, &mut picker, vec![])
            .0
            .drop_without_applying_deltas();
        let (frame, _) = draw(&ctx, &mut picker, vec![egui::Event::Text("/".into())]);
        assert_eq!(picker.suggestions.len(), 3);
        assert!(frame.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text.contains("Display columns"))));
        frame.drop_without_applying_deltas();
        draw(&ctx, &mut picker, vec![key(Key::ArrowDown)])
            .0
            .drop_without_applying_deltas();
        assert_eq!(picker.suggestion_selected, 1);
        let (frame, event) = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        frame.drop_without_applying_deltas();
        assert!(event.is_none());
        assert_eq!(picker.query(), "/!");
        assert!(!picker.is_closed());
        draw(&ctx, &mut picker, vec![key(Key::ArrowDown)])
            .0
            .drop_without_applying_deltas();
        draw(&ctx, &mut picker, vec![key(Key::Tab)])
            .0
            .drop_without_applying_deltas();
        assert_eq!(picker.query(), "/!Descending==");
        let (frame, event) = draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        frame.drop_without_applying_deltas();
        assert!(event.is_none());
        assert!(picker.suggestions.is_empty());
        assert!(!picker.is_closed());
        let (frame, event) = draw(&ctx, &mut picker, vec![key(Key::Escape)]);
        frame.drop_without_applying_deltas();
        assert!(matches!(event, Some(PickerEvent::Cancelled)));
    }
    #[test]
    fn mouse_completion_and_six_row_popup_match_skia() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        draw(&ctx, &mut picker, vec![])
            .0
            .drop_without_applying_deltas();
        draw(&ctx, &mut picker, vec![egui::Event::Text("/".into())])
            .0
            .drop_without_applying_deltas();
        let rect = ctx
            .memory(|memory| memory.area_rect(picker.id.with("autocomplete")))
            .unwrap();
        let pos = rect.min
            + egui::vec2(
                30.0,
                8.0 + 1.5 * Appearance::default().typography.row_height,
            );
        draw(&ctx, &mut picker, vec![egui::Event::PointerMoved(pos)])
            .0
            .drop_without_applying_deltas();
        draw(
            &ctx,
            &mut picker,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            }],
        )
        .0
        .drop_without_applying_deltas();
        draw(
            &ctx,
            &mut picker,
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Modifiers::NONE,
            }],
        )
        .0
        .drop_without_applying_deltas();
        assert_eq!(picker.query(), "/!");
        let (_, mut picker) = make();
        wait(&mut picker);
        let ctx = Context::default();
        draw(&ctx, &mut picker, vec![])
            .0
            .drop_without_applying_deltas();
        draw(&ctx, &mut picker, vec![egui::Event::Text("/:Name=".into())])
            .0
            .drop_without_applying_deltas();
        assert_eq!(picker.suggestions.len(), 8);
        let rect = ctx
            .memory(|memory| memory.area_rect(picker.id.with("autocomplete")))
            .unwrap();
        assert!(
            rect.height() <= 6.0 * Appearance::default().typography.preview_row_height(&ctx) + 20.0
        );
        for _ in 0..7 {
            draw(&ctx, &mut picker, vec![key(Key::ArrowDown)])
                .0
                .drop_without_applying_deltas();
        }
        assert_eq!(picker.suggestion_selected, 7);
        draw(&ctx, &mut picker, vec![key(Key::Enter)])
            .0
            .drop_without_applying_deltas();
        assert_eq!(picker.query(), "/:Name<");
    }
    #[test]
    fn completing_in_middle_of_unicode_query_preserves_suffix_and_cursor() {
        let (ctx, mut picker) = make();
        wait(&mut picker);
        draw(&ctx, &mut picker, vec![])
            .0
            .drop_without_applying_deltas();
        draw(
            &ctx,
            &mut picker,
            vec![egui::Event::Text("猫 /:Sta tail".into())],
        )
        .0
        .drop_without_applying_deltas();
        for _ in 0..5 {
            draw(&ctx, &mut picker, vec![key(Key::ArrowLeft)])
                .0
                .drop_without_applying_deltas();
        }
        assert_eq!(picker.suggestions[0].text, "Status");
        draw(&ctx, &mut picker, vec![key(Key::Tab)])
            .0
            .drop_without_applying_deltas();
        assert_eq!(picker.query(), "猫 /:Status= tail");
        assert_eq!(picker.query_cursor, "猫 /:Status=".len());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use nfm_search_core::{snapshot_store::SnapshotStore, store::FlatSnapshot};
    use std::time::{Duration, Instant};

    struct Versioned {
        version: u64,
        data: FlatSnapshot<u64>,
    }
    impl ItemsSource for Versioned {
        type Item = u64;
        fn version(&self) -> u64 {
            self.version
        }
        fn len(&self) -> usize {
            self.data.len()
        }
        fn is_empty(&self) -> bool {
            self.data.is_empty()
        }
        fn get_string<'a>(
            &'a self,
            index: usize,
            stack: &'a mut [u8],
            heap: &'a mut Vec<u8>,
        ) -> &'a [u8] {
            self.data.get_string(index, stack, heap)
        }
        fn get_string_lossy(&self, index: usize, out: &mut Vec<u8>) -> String {
            self.data.get_string_lossy(index, out)
        }
        fn item(&self, index: usize) -> Option<u64> {
            self.data.item(index)
        }
    }
    fn snapshot(version: u64, text: &str, payload: u64) -> Arc<Versioned> {
        Arc::new(Versioned {
            version,
            data: FlatSnapshot::from_items([(text, payload)]),
        })
    }
    fn wait(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "worker timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn make() -> (Context, Arc<SnapshotStore<Versioned>>, Picker<Versioned>) {
        let ctx = Context::default();
        let store = Arc::new(SnapshotStore::new());
        store.publish(snapshot(1, "alpha", 42));
        store.complete();
        let picker = Picker::new("test", &ctx, store.clone(), PickerConfig::default());
        (ctx, store, picker)
    }
    #[test]
    fn results_retain_payloads_from_their_actual_snapshot() {
        let (_, store, mut picker) = make();
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        let old = Arc::downgrade(&picker.displayed.as_ref().unwrap().snapshot);
        store.publish(snapshot(2, "beta", 99));
        // Before the UI receives a new result, alpha still belongs to snapshot 1.
        assert_eq!(picker.selection().unwrap().item, 42);
        assert!(old.upgrade().is_some());
        wait(|| {
            picker.poll();
            picker.selection().is_some_and(|s| s.source_version == 2)
        });
        assert_eq!(picker.selection().unwrap().item, 99);
        picker.close();
    }
    #[test]
    fn edits_invalidate_selection_and_reject_stale_updates() {
        let (_, _, mut picker) = make();
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        let stale = picker.displayed.take().unwrap();
        picker.set_query("beta");
        assert!(picker.selection().is_none());
        picker.displayed = Some(stale);
        assert!(picker.selection().is_none());
        wait(|| {
            picker.poll();
            picker
                .displayed
                .as_ref()
                .is_some_and(|d| d.revision == picker.revision)
        });
        assert!(picker.selection().is_none());
    }
    fn draw(
        ctx: &Context,
        picker: &mut Picker<Versioned>,
        events: Vec<egui::Event>,
    ) -> PickerOutput<u64> {
        let mut result = None;
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1000.0, 800.0),
                )),
                events,
                ..Default::default()
            },
            |ui| {
                let ctx = ui.ctx();
                let output = picker.show(
                    ctx,
                    Placement::new(ctx.content_rect()),
                    &Appearance::default(),
                    false,
                );
                if result
                    .as_ref()
                    .is_none_or(|old: &PickerOutput<u64>| old.event.is_none())
                {
                    result = Some(output);
                }
            },
        );
        output.drop_without_applying_deltas();
        result.unwrap()
    }
    fn key(key: Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }
    #[test]
    fn keyboard_accept_returns_payload_and_closes_once() {
        let (ctx, _, mut picker) = make();
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        draw(&ctx, &mut picker, vec![]);
        let output = draw(&ctx, &mut picker, vec![key(Key::Enter)]);
        assert!(matches!(
            output.event,
            Some(PickerEvent::Accepted(Selection { item: 42, .. }))
        ));
        assert!(picker.is_closed());
        assert!(
            draw(&ctx, &mut picker, vec![key(Key::Enter)])
                .event
                .is_none()
        );
    }
    #[test]
    fn escape_wins_over_enter_and_query_edits_cannot_accept_old_results() {
        let (ctx, _, mut picker) = make();
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        draw(&ctx, &mut picker, vec![]);
        let output = draw(
            &ctx,
            &mut picker,
            vec![egui::Event::Text("zz".into()), key(Key::Enter)],
        );
        assert!(output.event.is_none());
        assert!(!picker.is_closed());
        assert!(picker.selection().is_none());
        assert!(matches!(
            draw(&ctx, &mut picker, vec![key(Key::Escape), key(Key::Enter)]).event,
            Some(PickerEvent::Cancelled)
        ));
    }
}

#[cfg(test)]
mod extension_tests {
    use super::*;
    use nfm_picker_sources::structured::{StructuredSchema, StructuredStreamingStore};
    use nfm_search_core::{snapshot_store::SnapshotStore, store::FlatSnapshot};
    use std::time::{Duration, Instant};
    fn wait(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    #[test]
    fn structured_sources_supply_headers_completions_and_owned_payloads() {
        let mut data = StructuredStreamingStore::new(
            StructuredSchema::new(vec!["Name".into(), "Status".into()]).unwrap(),
        );
        data.add_record(&csv::StringRecord::from(vec!["alpha", "Running"]))
            .unwrap();
        let store = Arc::new(SnapshotStore::new());
        store.publish(data.snapshot());
        store.complete();
        let mut picker = Picker::new(
            "structured",
            &Context::default(),
            store,
            PickerConfig::default(),
        );
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        picker.set_query("/:Status==Run");
        let completion = picker.completions(picker.query().len()).remove(0);
        assert!(picker.apply_completion(&completion));
        assert_eq!(picker.query(), "/:Status==Running ");
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        assert_eq!(picker.selection().unwrap().item.fields["Name"], "alpha");
        let invalid = nfm_search_core::store::SearchCompletion {
            text: String::new(),
            positions: vec![],
            replacement: "bad".into(),
            replace: 1..usize::MAX,
        };
        assert!(!picker.apply_completion(&invalid));
    }
    #[test]
    fn preview_owns_searched_snapshot_after_picker_and_store_drop() {
        let ctx = Context::default();
        let store = Arc::new(SnapshotStore::new());
        store.publish(Arc::new(FlatSnapshot::from_items([("alpha", 42u64)])));
        store.complete();
        let (started, rx) = crossbeam_channel::bounded(1);
        let (release, gate) = crossbeam_channel::bounded(1);
        let mut picker = Picker::new("owned", &ctx, store.clone(), PickerConfig::default());
        picker.set_preview_provider(
            &ctx,
            Arc::new(
                move |request: PreviewRequest<u64>, cancel: crate::Cancellation| {
                    let snapshot = request
                        .selection
                        .source_snapshot::<FlatSnapshot<u64>>()
                        .unwrap();
                    started
                        .send((Arc::downgrade(&snapshot), cancel.clone()))
                        .unwrap();
                    gate.recv().unwrap();
                    assert_eq!(snapshot.item(request.selection.index), Some(42));
                    Ok(Preview::Empty)
                },
            ),
        );
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        picker.refresh_preview(400.0, &Appearance::default());
        let (snapshot, token) = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(store);
        drop(picker);
        assert!(token.is_cancelled());
        assert!(snapshot.upgrade().is_some());
        release.send(()).unwrap();
        wait(|| snapshot.upgrade().is_none());
    }
    #[test]
    fn small_viewports_reduce_rows_and_hide_preview_before_query() {
        let ctx = Context::default();
        let store = Arc::new(SnapshotStore::new());
        store.publish(Arc::new(FlatSnapshot::from_items([("alpha", 42u64)])));
        store.complete();
        let mut picker = Picker::new("resize", &ctx, store, PickerConfig::default());
        picker.set_preview_provider(
            &ctx,
            Arc::new(|_: PreviewRequest<u64>, _: crate::Cancellation| Ok(Preview::Empty)),
        );
        let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 200.0));
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(bounds),
                ..Default::default()
            },
            |ui| {
                let response = picker.show(
                    ui.ctx(),
                    Placement::new(bounds),
                    &Appearance::default(),
                    false,
                );
                assert!(bounds.contains_rect(response.rect));
            },
        );
        output.drop_without_applying_deltas();
        assert_eq!(picker.visible_preview_rows, 0);
        assert!(picker.visible_result_rows < 7);
    }
}

#[cfg(test)]
mod streaming_presentation_tests {
    use super::*;
    use nfm_search_core::{snapshot_store::SnapshotStore, store::FlatSnapshot};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counted {
        version: u64,
        data: FlatSnapshot<u64>,
        reads: Arc<AtomicUsize>,
    }
    impl ItemsSource for Counted {
        type Item = u64;
        fn version(&self) -> u64 {
            self.version
        }
        fn len(&self) -> usize {
            self.data.len()
        }
        fn is_empty(&self) -> bool {
            self.data.is_empty()
        }
        fn get_string<'a>(
            &'a self,
            index: usize,
            stack: &'a mut [u8],
            heap: &'a mut Vec<u8>,
        ) -> &'a [u8] {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.data.get_string(index, stack, heap)
        }
        fn get_string_lossy(&self, index: usize, out: &mut Vec<u8>) -> String {
            self.reads.fetch_add(1, Ordering::Relaxed);
            self.data.get_string_lossy(index, out)
        }
        fn item(&self, index: usize) -> Option<u64> {
            self.data.item(index)
        }
    }
    fn snapshot(version: u64, len: u64, reads: &Arc<AtomicUsize>) -> Arc<Counted> {
        Arc::new(Counted {
            version,
            data: FlatSnapshot::from_items((0..len).map(|i| (format!("item {i:03}"), i))),
            reads: reads.clone(),
        })
    }
    fn wait(mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !condition() {
            assert!(Instant::now() < deadline, "worker timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn draw(ctx: &Context, picker: &mut Picker<Counted>) -> (egui::Rect, egui::Rect) {
        let bounds = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1000.0, 800.0));
        let frame = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(bounds),
                ..Default::default()
            },
            |ui| {
                picker.show(
                    ui.ctx(),
                    Placement::new(bounds),
                    &Appearance::default(),
                    false,
                );
            },
        );
        frame.drop_without_applying_deltas();
        (
            ctx.memory(|m| m.area_rect(picker.id)).unwrap(),
            ctx.read_response(picker.id.with("query")).unwrap().rect,
        )
    }
    #[test]
    fn streams_before_completion_searches_only_delta_and_keeps_selection_preview() {
        let ctx = Context::default();
        let reads = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(SnapshotStore::new_append_only());
        store.publish(snapshot(1, 100, &reads));
        let mut picker = Picker::new(
            "stream",
            &ctx,
            store.clone(),
            PickerConfig {
                initial_query: "item".into(),
                ..Default::default()
            },
        );
        let previews = Arc::new(AtomicUsize::new(0));
        let calls = previews.clone();
        picker.set_preview_provider(
            &ctx,
            Arc::new(
                move |request: PreviewRequest<u64>, _: crate::Cancellation| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(Preview::Text(request.selection.text))
                },
            ),
        );
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        assert!(!picker.displayed.as_ref().unwrap().done);
        picker.move_selection(50);
        picker.refresh_preview(600.0, &Appearance::default());
        wait(|| {
            picker.poll();
            !picker.preview.as_ref().unwrap().loading
        });
        let geometry = draw(&ctx, &mut picker);
        // Draw may resize the initial preview request to the actual viewport.
        wait(|| {
            picker.poll();
            !picker.preview.as_ref().unwrap().loading
        });
        let initial_calls = previews.load(Ordering::SeqCst);
        reads.store(0, Ordering::SeqCst);
        store.publish(snapshot(2, 101, &reads));
        wait(|| {
            picker.poll();
            picker
                .displayed
                .as_ref()
                .is_some_and(|d| d.snapshot.version() == 2)
        });
        assert_eq!(picker.selection().unwrap().item, 50);
        assert_eq!(picker.displayed.as_ref().unwrap().output.matched, 101);
        assert!(
            reads.load(Ordering::SeqCst) < 10,
            "stream append rescanned previous items"
        );
        let next_geometry = draw(&ctx, &mut picker);
        assert_eq!(geometry.0, next_geometry.0);
        assert_eq!(geometry.1.min.y, next_geometry.1.min.y);
        wait(|| {
            picker.poll();
            !picker.preview.as_ref().unwrap().loading
        });
        assert_eq!(
            previews.load(Ordering::SeqCst),
            initial_calls,
            "unrelated appended candidates restarted the selected preview"
        );
        store.complete();
        wait(|| {
            picker.poll();
            picker.displayed.as_ref().is_some_and(|d| d.done)
        });
        assert_eq!(picker.selection().unwrap().item, 50);
    }
    #[test]
    fn pending_query_retains_rows_and_geometry_but_blocks_acceptance() {
        let ctx = Context::default();
        let reads = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(SnapshotStore::new_append_only());
        store.publish(snapshot(1, 3, &reads));
        store.complete();
        let mut picker = Picker::new("pending", &ctx, store, PickerConfig::default());
        wait(|| {
            picker.poll();
            picker.selection().is_some()
        });
        let before = draw(&ctx, &mut picker);
        picker.set_query("does-not-match");
        assert_eq!(picker.displayed.as_ref().unwrap().output.results.len(), 3);
        assert!(picker.selection().is_none());
        assert!(picker.busy());
        wait(|| {
            picker.poll();
            picker.current_results()
        });
        assert!(picker.displayed.as_ref().unwrap().output.results.is_empty());
        let after = draw(&ctx, &mut picker);
        assert_eq!(before.0, after.0);
        assert_eq!(before.1.min.y, after.1.min.y);
    }
}

#[cfg(test)]
mod output_selection_tests {
    use super::*;

    fn selection(item: usize) -> Selection<usize> {
        Selection {
            item,
            text: item.to_string(),
            index: item,
            source_version: item as u64,
            snapshot: Arc::new(item),
        }
    }
    fn output(event: Option<PickerEvent<usize>>) -> PickerOutput<usize> {
        PickerOutput {
            event,
            frame_selection: Some(selection(1)),
            preview_rendered: false,
            preview_selection: None,
            preview_rect: None,
            wants_keyboard: false,
            busy: false,
            matched: 0,
            total: 0,
            rect: egui::Rect::NOTHING,
        }
    }
    #[test]
    fn selection_prefers_event_snapshot_and_borrows_without_cloning() {
        for event in [
            PickerEvent::Accepted(selection(2)),
            PickerEvent::CustomAction {
                action: 42,
                selection: selection(2),
            },
            PickerEvent::CopyRequested(selection(2)),
        ] {
            let output = output(Some(event));
            let selected = output.selection().unwrap();
            let event_selection = match output.event.as_ref().unwrap() {
                PickerEvent::Accepted(s)
                | PickerEvent::CustomAction { selection: s, .. }
                | PickerEvent::CopyRequested(s) => s,
                _ => unreachable!(),
            };
            assert!(std::ptr::eq(selected, event_selection));
            assert_eq!(*selected.source_snapshot::<usize>().unwrap(), 2);
        }
    }
    #[test]
    fn selection_uses_frame_snapshot_without_a_selection_event() {
        for event in [None, Some(PickerEvent::Cancelled)] {
            let mut output = output(event);
            assert_eq!(output.selection().unwrap().item, 1);
            output.frame_selection = None;
            assert!(output.selection().is_none());
        }
    }
}
