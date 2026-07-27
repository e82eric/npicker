use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::preview::{PreviewController, PreviewUpdate};
use crate::request::{PickerRequest, PickerResponse};
use crate::source_store::{AnyItemSource, SharedStore};
use anyhow::{bail, Result};
use crossbeam_channel::{unbounded, Receiver, Sender};
use nfm_search_core::fuzzy_search_session::{FuzzySearchSession, FuzzySearchUpdate};
use nfm_search_core::search::SearchResult;
use nfm_search_core::timing;

const PICKER_DISPLAY_LIMIT: usize = 7;

#[derive(Clone, Debug)]
pub enum UiEvent {
    Show,
    Results(UiUpdate),
    Preview(PreviewUpdate),
    PreviewViewport { top_line: usize },
    Close,
}

#[derive(Clone, Debug, Default)]
pub struct UiCounters {
    pub displayed: usize,
    pub matched: usize,
    pub published: usize,
    pub scanning: bool,
}

#[derive(Clone, Debug, Default)]
pub struct UiUpdate {
    pub results: Vec<SearchResult>,
    pub counters: UiCounters,
    pub selected_row: usize,
}

pub struct ViewModel {
    state: Mutex<State>,
    request_generation: AtomicU64,
    search_update_tx: Sender<FuzzySearchUpdate>,
    search_update_rx: Receiver<FuzzySearchUpdate>,
    events_tx: Sender<UiEvent>,
    events_rx: Receiver<UiEvent>,
    preview: Option<PreviewController>,
}

struct State {
    active: Option<ActiveRequest>,
    search_text: String,
    cursor_position: usize,
    cursor_selection_anchor: Option<usize>,
    results: Vec<SearchResult>,
    counters: UiCounters,
    selected: usize,
    viewport_start: usize,
    preview_item: Option<String>,
    preview_generation: u64,
    preview_line_count: usize,
    preview_top_line: usize,
    preview_visible_rows: usize,
    preview_truncated: bool,
}

#[derive(Clone)]
pub struct SearchInputState {
    pub text: String,
    pub cursor_position: usize,
    pub selection: Option<Range<usize>>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct KeyModifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputCommand {
    Accept,
    Cancel,
    MoveUp,
    MoveDown,
    MoveLeft,
    MoveRight,
    MoveHome,
    MoveEnd,
    Backspace,
    Delete,
    PreviewPageUp,
    PreviewPageDown,
}

struct ActiveRequest {
    id: u64,
    response_tx: Sender<PickerResponse>,
    search_session: FuzzySearchSession<AnyItemSource, SharedStore>,
}

impl ViewModel {
    pub fn new() -> Arc<Self> {
        Self::new_with_preview(None)
    }

    pub fn new_with_preview(preview_command: Option<String>) -> Arc<Self> {
        let (events_tx, events_rx) = unbounded();
        let (search_update_tx, search_update_rx) = unbounded();
        let (preview, preview_rx) = if let Some(command) = preview_command {
            let (preview_tx, preview_rx) = unbounded();
            (
                Some(PreviewController::new(command, preview_tx)),
                Some(preview_rx),
            )
        } else {
            (None, None)
        };

        let this = Arc::new(Self {
            state: Mutex::new(State {
                active: None,
                search_text: String::new(),
                results: Vec::new(),
                counters: UiCounters::default(),
                selected: 0,
                viewport_start: 0,
                cursor_position: 0,
                cursor_selection_anchor: None,
                preview_item: None,
                preview_generation: 0,
                preview_line_count: 0,
                preview_top_line: 0,
                preview_visible_rows: 0,
                preview_truncated: false,
            }),
            request_generation: AtomicU64::new(0),
            search_update_tx,
            search_update_rx,
            events_tx,
            events_rx,
            preview,
        });

        if let Some(preview_rx) = preview_rx {
            let weak = Arc::downgrade(&this);
            thread::spawn(move || {
                while let Ok(update) = preview_rx.recv() {
                    let Some(view_model) = weak.upgrade() else {
                        break;
                    };
                    view_model.apply_preview_update(&update);
                    if view_model.events_tx.send(UiEvent::Preview(update)).is_err() {
                        break;
                    }
                }
            });
        }
        this.spawn_search_update_thread();
        this
    }

    fn spawn_search_update_thread(self: &Arc<Self>) {
        let this = Arc::clone(self);
        let rx = this.search_update_rx.clone();

        thread::spawn(move || {
            while let Ok(event) = rx.recv() {
                this.apply_search_update(event);
            }
        });
    }

    pub fn subscribe(&self) -> Receiver<UiEvent> {
        self.events_rx.clone()
    }

    pub fn run_request<R>(self: &Arc<Self>, request: &R) -> Result<PickerResponse>
    where
        R: PickerRequest,
    {
        timing::begin_request();
        let search_len = request
            .search_string()
            .as_ref()
            .map_or(0, |value| value.len());
        timing::write(format!("begin_request search_len={search_len}",));

        let request_id = self.request_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let (response_tx, response_rx) = unbounded();
        let store = request.run();

        let query = request.search_string().unwrap_or_default().to_owned();
        let session = FuzzySearchSession::new(
            request_id,
            Arc::clone(&store),
            query.clone(),
            self.search_update_tx.clone(),
        );
        {
            let mut state = self.state.lock().expect("view model poisoned");
            if state.active.is_some() {
                bail!("A picker request is already active.");
            }

            let cursor_position = query.len();
            state.search_text = query;
            state.results.clear();
            state.counters = UiCounters::default();
            state.selected = 0;
            state.cursor_selection_anchor = None;
            state.cursor_position = cursor_position;
            state.viewport_start = 0;
            state.active = Some(ActiveRequest {
                id: request_id,
                response_tx,
                search_session: session.clone(),
            });
        }
        self.update_preview_selection(None);

        let _ = self.events_tx.send(UiEvent::Show);
        session.start();

        let response = response_rx
            .recv()
            .unwrap_or_else(|_| PickerResponse::cancelled());
        timing::write(format!("response status={}", response.status));
        Ok(response)
    }

    fn update_search_text(self: &Arc<Self>, update: impl FnOnce(&mut State) -> bool) {
        let search_update = {
            let mut state = self.state.lock().expect("view model poisoned");

            let changed = update(&mut state);

            if changed {
                state.selected = 0;
                state.viewport_start = 0;

                state
                    .active
                    .as_ref()
                    .map(|active| (active.search_session.clone(), state.search_text.clone()))
            } else {
                None
            }
        };

        if let Some((search_session, search_text)) = search_update {
            self.update_preview_selection(None);
            search_session.set_query(search_text);
        }
    }

    pub fn insert_text(self: &Arc<Self>, text: &str) {
        let text: String = text
            .chars()
            .filter(|ch| !ch.is_control() && *ch != '\u{7f}')
            .collect();
        if text.is_empty() {
            return;
        }
        self.update_search_text(|state| {
            let cursor = state.cursor_position;
            if cursor > state.search_text.len() || !state.search_text.is_char_boundary(cursor) {
                return false;
            }
            if let Some(anchor) = state.cursor_selection_anchor.take() {
                let start = anchor.min(cursor);
                let end = anchor.max(cursor);
                if !state.search_text.is_char_boundary(start)
                    || !state.search_text.is_char_boundary(end)
                {
                    return false;
                }
                state.search_text.replace_range(start..end, &text);
                state.cursor_position = start + text.len();
            } else {
                state.search_text.insert_str(cursor, &text);
                state.cursor_position = cursor + text.len();
            }
            true
        });
    }

    pub fn handle_command(self: &Arc<Self>, command: InputCommand, modifiers: KeyModifiers) {
        match command {
            InputCommand::Accept => self.select_current(),
            InputCommand::Cancel => self.cancel(),
            InputCommand::MoveUp => self.move_selection(1),
            InputCommand::MoveDown => self.move_selection(-1),
            InputCommand::MoveLeft if modifiers.ctrl => {
                self.move_cursor_previous_word(modifiers.shift)
            }
            InputCommand::MoveLeft => self.move_cursor_left(modifiers.shift),
            InputCommand::MoveRight if modifiers.ctrl => {
                self.move_cursor_next_word(modifiers.shift)
            }
            InputCommand::MoveRight => self.move_cursor_right(modifiers.shift),
            InputCommand::MoveEnd => self.move_cursor_end(modifiers.shift),
            InputCommand::MoveHome => self.move_cursor_start(modifiers.shift),
            InputCommand::Backspace => self.handle_backspace(modifiers),
            InputCommand::Delete => self.handle_delete(modifiers),
            InputCommand::PreviewPageUp => self.page_preview(-1),
            InputCommand::PreviewPageDown => self.page_preview(1),
        }
    }

    pub fn set_preview_visible_rows(&self, visible_rows: usize) {
        let top_line = {
            let mut state = self.state.lock().expect("view model poisoned");
            if state.preview_visible_rows == visible_rows {
                return;
            }
            state.preview_visible_rows = visible_rows;
            state.clamp_preview_viewport();
            state.preview_top_line
        };
        let _ = self.events_tx.send(UiEvent::PreviewViewport { top_line });
    }

    fn page_preview(&self, direction: isize) {
        let top_line = {
            let mut state = self.state.lock().expect("view model poisoned");
            let page = state.preview_content_rows().saturating_sub(1).max(1);
            let maximum = state
                .preview_line_count
                .saturating_sub(state.preview_content_rows());
            state.preview_top_line = if direction < 0 {
                state.preview_top_line.saturating_sub(page)
            } else {
                state.preview_top_line.saturating_add(page).min(maximum)
            };
            state.preview_top_line
        };
        let _ = self.events_tx.send(UiEvent::PreviewViewport { top_line });
    }

    fn apply_preview_update(&self, update: &PreviewUpdate) {
        let mut state = self.state.lock().expect("view model poisoned");
        let generation = match update {
            PreviewUpdate::Clear { generation }
            | PreviewUpdate::Ready { generation, .. }
            | PreviewUpdate::Error { generation, .. } => *generation,
        };
        if generation != state.preview_generation {
            return;
        }
        match update {
            PreviewUpdate::Clear { .. } | PreviewUpdate::Error { .. } => {
                state.preview_line_count = 0;
                state.preview_top_line = 0;
                state.preview_truncated = false;
            }
            PreviewUpdate::Ready {
                lines, truncated, ..
            } => {
                state.preview_line_count = lines.len();
                state.preview_top_line = 0;
                state.preview_truncated = *truncated;
                state.clamp_preview_viewport();
            }
        }
    }

    fn handle_backspace(self: &Arc<Self>, modifiers: KeyModifiers) {
        self.update_search_text(|state| {
            if modifiers.ctrl {
                //TODO: update to calculate this instead of relying of selection
                Self::move_cursor_previous_word_in_state(state, true);
            }

            let cursor = state.cursor_position;

            if (cursor == 0 && state.cursor_selection_anchor.is_none())
                || cursor > state.search_text.len()
                || !state.search_text.is_char_boundary(cursor)
            {
                return false;
            }

            if let Some(selection) = state.cursor_selection_anchor {
                let start = selection.min(cursor);
                let end = selection.max(cursor);

                if !state.search_text.is_char_boundary(start)
                    || !state.search_text.is_char_boundary(end)
                {
                    return false;
                }

                state.search_text.replace_range(start..end, "");
                state.cursor_selection_anchor = None;
                state.cursor_position = start;
                return true;
            }

            let Some(prev_cursor) = state.search_text[..cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
            else {
                return false;
            };

            state.search_text.drain(prev_cursor..cursor);
            state.cursor_position = prev_cursor;

            true
        });
    }

    fn handle_delete(self: &Arc<Self>, modifiers: KeyModifiers) {
        self.update_search_text(|state| {
            if modifiers.ctrl {
                Self::move_cursor_next_word_in_state(state, true);
            }

            let cursor = state.cursor_position;

            if (cursor == state.search_text.len() && state.cursor_selection_anchor.is_none())
                || cursor > state.search_text.len()
                || !state.search_text.is_char_boundary(cursor)
            {
                return false;
            }
            if let Some(selection) = state.cursor_selection_anchor {
                let start = selection.min(cursor);
                let end = selection.max(cursor);

                if !state.search_text.is_char_boundary(start)
                    || !state.search_text.is_char_boundary(end)
                {
                    return false;
                }

                state.search_text.replace_range(start..end, "");
                state.cursor_selection_anchor = None;
                state.cursor_position = start;
                return true;
            }

            let Some(ch) = state.search_text[cursor..].chars().next() else {
                return false;
            };
            let next_cursor = cursor + ch.len_utf8();
            state.search_text.replace_range(cursor..next_cursor, "");
            true
        });
    }

    pub fn move_selection(&self, delta: isize) {
        let update = {
            let mut state = self.state.lock().expect("view model poisoned");
            if state.results.is_empty() {
                state.selected = 0;
                state.viewport_start = 0;
                return;
            }

            let next = state.selected as isize + delta;
            state.selected = next.clamp(0, state.results.len() as isize - 1) as usize;
            state.ensure_selection_visible();
            state.visible_update()
        };
        self.publish_results(update);
    }

    fn update_cursor_selection_anchor(state: &mut State, extend_selection: bool) {
        if extend_selection {
            if state.cursor_selection_anchor.is_none() {
                state.cursor_selection_anchor = Some(state.cursor_position);
            }
        } else {
            state.cursor_selection_anchor = None;
        }
    }

    fn move_cursor_left(&self, extend_selection: bool) {
        let mut state = self.state.lock().expect("view model poisoned");

        let cursor = state.cursor_position;
        Self::update_cursor_selection_anchor(&mut state, extend_selection);

        if cursor == 0 || cursor > state.search_text.len() {
            return;
        }

        if !state.search_text.is_char_boundary(cursor) {
            return;
        }

        if let Some((prev_cursor, _)) = state.search_text[..cursor].char_indices().last() {
            state.cursor_position = prev_cursor;
        }
    }

    fn move_cursor_right(&self, extend_selection: bool) {
        let mut state = self.state.lock().expect("view model poisoned");

        let cursor = state.cursor_position;
        Self::update_cursor_selection_anchor(&mut state, extend_selection);

        if cursor >= state.search_text.len() {
            return;
        }

        if !state.search_text.is_char_boundary(cursor) {
            return;
        }

        if let Some(ch) = state.search_text[cursor..].chars().next() {
            state.cursor_position = cursor + ch.len_utf8();
        }
    }

    fn move_cursor_start(&self, extend_selection: bool) {
        let mut state = self.state.lock().expect("view model poisoned");
        Self::update_cursor_selection_anchor(&mut state, extend_selection);
        state.cursor_position = 0;
    }

    fn move_cursor_end(&self, extend_selection: bool) {
        let mut state = self.state.lock().expect("view model poisoned");
        Self::update_cursor_selection_anchor(&mut state, extend_selection);
        state.cursor_position = state.search_text.len();
    }

    fn move_cursor_next_word_in_state(state: &mut State, extend_selection: bool) {
        let mut cursor = state.cursor_position.min(state.search_text.len());
        Self::update_cursor_selection_anchor(state, extend_selection);

        while cursor < state.search_text.len() {
            let Some(ch) = state.search_text[cursor..].chars().next() else {
                break;
            };

            if !ch.is_whitespace() {
                break;
            }

            cursor += ch.len_utf8();
        }

        while cursor < state.search_text.len() {
            let Some(ch) = state.search_text[cursor..].chars().next() else {
                break;
            };

            if ch.is_whitespace() {
                break;
            }

            cursor += ch.len_utf8();
        }

        state.cursor_position = cursor;
    }

    fn move_cursor_next_word(&self, extend_selection: bool) {
        let mut state = self.state.lock().expect("view model poisoned");
        Self::move_cursor_next_word_in_state(&mut state, extend_selection);
    }

    fn move_cursor_previous_word_in_state(state: &mut State, extend_selection: bool) {
        let mut cursor = state.cursor_position.min(state.search_text.len());

        Self::update_cursor_selection_anchor(state, extend_selection);

        // If on whitespace, first go back to first non-space char.
        while cursor > 0 {
            let Some((prev, ch)) = state.search_text[..cursor].char_indices().last() else {
                break;
            };

            if !ch.is_whitespace() {
                break;
            }

            cursor = prev;
        }

        // Then go back to the start of the word.
        while cursor > 0 {
            let Some((prev, ch)) = state.search_text[..cursor].char_indices().last() else {
                break;
            };

            if ch.is_whitespace() {
                break;
            }

            cursor = prev;
        }

        state.cursor_position = cursor;
    }

    fn move_cursor_previous_word(&self, extend_selection: bool) {
        let mut state = self.state.lock().expect("view model poisoned");
        Self::move_cursor_previous_word_in_state(&mut state, extend_selection);
    }

    pub fn select_current(&self) {
        let session_to_stop = {
            let mut state = self.state.lock().expect("view model poisoned");
            if let Some(active) = state.active.take() {
                let response = state
                    .results
                    .get(state.selected)
                    .map(|result| PickerResponse::selected(result.path.clone()))
                    .unwrap_or_else(PickerResponse::cancelled);
                let _ = active.response_tx.send(response);
                Some(active.search_session)
            } else {
                None
            }
        };

        if let Some(session_to_stop) = session_to_stop {
            session_to_stop.stop();
        }

        self.update_preview_selection(None);
        let _ = self.events_tx.send(UiEvent::Close);
    }

    pub fn cancel(&self) {
        let session_to_stop = {
            let mut state = self.state.lock().expect("view model poisoned");
            if let Some(active) = state.active.take() {
                let _ = active.response_tx.send(PickerResponse::cancelled());
                Some(active.search_session)
            } else {
                None
            }
        };

        if let Some(session_to_stop) = session_to_stop {
            session_to_stop.stop();
        }

        self.update_preview_selection(None);
        let _ = self.events_tx.send(UiEvent::Close);
    }

    #[allow(dead_code)]
    pub fn current_results(&self) -> Vec<SearchResult> {
        self.state
            .lock()
            .expect("view model poisoned")
            .results
            .clone()
    }

    pub fn current_search_text(&self) -> SearchInputState {
        let state = self.state.lock().expect("view model poisoned");
        let anchor = state.cursor_selection_anchor;
        let mut selection = None;

        if let Some(anchor) = anchor {
            let start = anchor.min(state.cursor_position);
            let end = anchor.max(state.cursor_position);
            selection = Some(Range { start, end });
        }

        SearchInputState {
            text: state.search_text.clone(),
            cursor_position: state.cursor_position,
            selection,
        }
    }

    fn apply_search_update(&self, search_update: FuzzySearchUpdate) {
        let ui_update = {
            let mut state = self.state.lock().expect("view model poisoned");

            let Some(active) = state.active.as_ref() else {
                return;
            };

            if search_update.session_id != active.id {
                return;
            }

            state.results = search_update.results;
            state.counters = UiCounters {
                displayed: state.results.len(),
                matched: search_update.matched,
                published: search_update.searched,
                scanning: !search_update.source_done,
            };
            state.selected = state.selected.min(state.results.len().saturating_sub(1));
            state.ensure_selection_visible();

            let ui_update = state.visible_update();
            ui_update
        };
        self.publish_results(ui_update);
    }

    fn publish_results(&self, update: UiUpdate) {
        let selected_item = update
            .results
            .get(update.selected_row)
            .map(|result| result.path.clone());
        self.update_preview_selection(selected_item);
        let _ = self.events_tx.send(UiEvent::Results(update));
    }

    fn update_preview_selection(&self, selected_item: Option<String>) {
        let changed = {
            let mut state = self.state.lock().expect("view model poisoned");
            if state.preview_item == selected_item {
                false
            } else {
                state.preview_item = selected_item.clone();
                true
            }
        };
        if !changed {
            return;
        }
        if let Some(preview) = &self.preview {
            let generation = if let Some(selected_item) = selected_item {
                preview.request(selected_item)
            } else {
                preview.cancel()
            };
            {
                let mut state = self.state.lock().expect("view model poisoned");
                state.preview_generation = generation;
                state.preview_line_count = 0;
                state.preview_top_line = 0;
                state.preview_truncated = false;
            }
            let _ = self
                .events_tx
                .send(UiEvent::Preview(PreviewUpdate::Clear { generation }));
        }
    }
}

impl State {
    fn preview_content_rows(&self) -> usize {
        if self.preview_truncated {
            self.preview_visible_rows.saturating_sub(1)
        } else {
            self.preview_visible_rows
        }
    }

    fn clamp_preview_viewport(&mut self) {
        self.preview_top_line = self.preview_top_line.min(
            self.preview_line_count
                .saturating_sub(self.preview_content_rows()),
        );
    }

    fn ensure_selection_visible(&mut self) {
        if self.results.is_empty() {
            self.selected = 0;
            self.viewport_start = 0;
            return;
        }

        let max_viewport_start = self.results.len().saturating_sub(PICKER_DISPLAY_LIMIT);
        self.viewport_start = self.viewport_start.min(max_viewport_start);

        if self.selected < self.viewport_start {
            self.viewport_start = self.selected;
        } else if self.selected >= self.viewport_start + PICKER_DISPLAY_LIMIT {
            self.viewport_start = self.selected + 1 - PICKER_DISPLAY_LIMIT;
        }
    }

    fn visible_update(&self) -> UiUpdate {
        let visible_results: Vec<_> = self
            .results
            .iter()
            .skip(self.viewport_start)
            .take(PICKER_DISPLAY_LIMIT)
            .cloned()
            .collect();
        let selected_row = if visible_results.is_empty() {
            0
        } else {
            self.selected.saturating_sub(self.viewport_start)
        };

        UiUpdate {
            results: visible_results,
            counters: UiCounters {
                displayed: self.results.len().min(PICKER_DISPLAY_LIMIT),
                ..self.counters.clone()
            },
            selected_row,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn modifiers(ctrl: bool, shift: bool) -> KeyModifiers {
        KeyModifiers {
            ctrl,
            shift,
            alt: false,
        }
    }

    #[test]
    fn semantic_input_inserts_unicode_and_replaces_selection() {
        let view_model = ViewModel::new();
        view_model.insert_text("ab");
        view_model.handle_command(InputCommand::MoveLeft, modifiers(false, true));
        view_model.insert_text("é");

        let state = view_model.current_search_text();
        assert_eq!(state.text, "aé");
        assert_eq!(state.cursor_position, "aé".len());
        assert_eq!(state.selection, None);
    }

    #[test]
    fn semantic_input_moves_and_deletes_by_word() {
        let view_model = ViewModel::new();
        view_model.insert_text("one two");
        view_model.handle_command(InputCommand::Backspace, modifiers(true, false));
        assert_eq!(view_model.current_search_text().text, "one ");
        view_model.handle_command(InputCommand::MoveHome, modifiers(false, false));
        view_model.handle_command(InputCommand::Delete, modifiers(true, false));
        assert_eq!(view_model.current_search_text().text, " ");
    }

    #[test]
    fn preview_paging_overlaps_one_row_and_clamps_to_document() {
        let view_model = ViewModel::new();
        view_model.set_preview_visible_rows(20);
        while view_model.events_rx.try_recv().is_ok() {}
        {
            let mut state = view_model.state.lock().unwrap();
            state.preview_line_count = 50;
        }

        view_model.handle_command(InputCommand::PreviewPageDown, KeyModifiers::default());
        assert!(matches!(
            view_model.events_rx.try_recv(),
            Ok(UiEvent::PreviewViewport { top_line: 19 })
        ));

        view_model.handle_command(InputCommand::PreviewPageDown, KeyModifiers::default());
        assert!(matches!(
            view_model.events_rx.try_recv(),
            Ok(UiEvent::PreviewViewport { top_line: 30 })
        ));

        view_model.handle_command(InputCommand::PreviewPageUp, KeyModifiers::default());
        assert!(matches!(
            view_model.events_rx.try_recv(),
            Ok(UiEvent::PreviewViewport { top_line: 11 })
        ));
    }
}
