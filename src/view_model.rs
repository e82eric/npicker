use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::action::{
    ActionController, ActionEvent, ActionResolution, ActionSelection, ActionService, ActionState,
    PickerState,
};
use crate::key_binding::KeyChord;
pub use crate::key_binding::KeyModifiers;
use crate::preview::{
    NativeWindowId, PreviewCoordinator, PreviewEvent, PreviewService, PreviewUpdate,
};
#[cfg(windows)]
use crate::request::FileSystemPickerRequest;
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
    Preview(PreviewView),
    PreviewVisibilityChanged { visible: bool },
    Hide,
}

#[derive(Clone, Debug)]
pub enum PreviewView {
    Text {
        update: PreviewUpdate,
        top_line: usize,
    },
    NativeWindow(Option<NativeWindowId>),
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
    preview: PreviewCoordinator,
    actions: ActionController,
    bindings: HashMap<KeyChord, String>,
}

pub(crate) enum ViewModelEvent {
    Preview(PreviewEvent),
    Action(ActionEvent),
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
    preview_generation: u64,
    preview_line_count: usize,
    preview_top_line: usize,
    preview_visible_rows: usize,
    preview_truncated: bool,
    preview_update: Option<PreviewUpdate>,
    preview_visible: bool,
    action_generation: Option<u64>,
}

#[derive(Clone)]
pub struct SearchInputState {
    pub text: String,
    pub cursor_position: usize,
    pub selection: Option<Range<usize>>,
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
    TogglePreview,
}

struct ActiveRequest {
    id: u64,
    response_tx: Sender<PickerResponse>,
    search_session: FuzzySearchSession<AnyItemSource, SharedStore>,
    store: Arc<SharedStore>,
    picker_state: PickerState,
}

impl ViewModel {
    pub fn new(preview_service: PreviewService) -> Arc<Self> {
        Self::new_with_services(preview_service, ActionService::default(), true)
    }

    pub fn new_with_preview_visibility(
        preview_service: PreviewService,
        preview_visible: bool,
    ) -> Arc<Self> {
        Self::new_with_services(preview_service, ActionService::default(), preview_visible)
    }

    pub fn new_with_services(
        preview_service: PreviewService,
        action_service: ActionService,
        preview_visible: bool,
    ) -> Arc<Self> {
        Self::new_with_services_and_bindings(
            preview_service,
            action_service,
            HashMap::new(),
            preview_visible,
        )
    }

    pub fn new_with_services_and_bindings(
        preview_service: PreviewService,
        action_service: ActionService,
        bindings: HashMap<KeyChord, String>,
        preview_visible: bool,
    ) -> Arc<Self> {
        let (events_tx, events_rx) = unbounded();
        let (internal_events_tx, internal_events_rx) = unbounded();
        let (search_update_tx, search_update_rx) = unbounded();
        let preview = preview_service.into_coordinator(internal_events_tx.clone());
        let actions = action_service.into_controller(internal_events_tx.clone());

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
                preview_generation: 0,
                preview_line_count: 0,
                preview_top_line: 0,
                preview_visible_rows: 0,
                preview_truncated: false,
                preview_update: None,
                preview_visible,
                action_generation: None,
            }),
            request_generation: AtomicU64::new(0),
            search_update_tx,
            search_update_rx,
            events_tx,
            events_rx,
            preview,
            actions,
            bindings,
        });

        this.spawn_event_thread(internal_events_rx);
        this.spawn_search_update_thread();
        this
    }

    fn spawn_event_thread(self: &Arc<Self>, events: Receiver<ViewModelEvent>) {
        let weak = Arc::downgrade(self);
        thread::spawn(move || {
            while let Ok(event) = events.recv() {
                let Some(view_model) = weak.upgrade() else {
                    break;
                };
                match event {
                    ViewModelEvent::Preview(event) => view_model.handle_preview_event(event),
                    ViewModelEvent::Action(event) => view_model.handle_action_event(event),
                }
            }
        });
    }

    fn spawn_search_update_thread(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        let updates = self.search_update_rx.clone();
        thread::spawn(move || {
            while let Ok(update) = updates.recv() {
                let Some(view_model) = weak.upgrade() else {
                    break;
                };
                view_model.apply_search_update(update);
            }
        });
    }

    fn handle_action_event(&self, event: ActionEvent) {
        {
            let mut state = self.state.lock().expect("view model poisoned");
            if state.action_generation != Some(event.generation) {
                return;
            }
            state.action_generation = None;
        }
        match event.result {
            Ok(ActionResolution::None) => {}
            Ok(ActionResolution::Complete) => match event.state.selection {
                Some(selection) => self.complete_accept(selection.value),
                None => eprintln!("action resolver cannot complete without a selection"),
            },
            Ok(ActionResolution::FileWalker { roots }) => self.transition_to_filewalker(roots),
            Err(message) => eprintln!("action resolver error: {message}"),
        }
    }

    fn handle_preview_event(&self, event: PreviewEvent) {
        let view = match event {
            PreviewEvent::Command(update) => self
                .apply_preview_update(&update)
                .map(|top_line| PreviewView::Text { update, top_line }),
            PreviewEvent::NativeWindow(window) => Some(PreviewView::NativeWindow(window)),
        };
        if let Some(view) = view {
            let _ = self.events_tx.send(UiEvent::Preview(view));
        }
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
        let initial_update = {
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
            state.action_generation = None;
            state.active = Some(ActiveRequest {
                id: request_id,
                response_tx,
                search_session: session.clone(),
                store: Arc::clone(&store),
                picker_state: request.picker_state(),
            });
            state.visible_update()
        };
        self.clear_preview_selection();

        // A long-lived UI may still contain the previous picker's rows. Publish the cleared
        // state before showing the next request instead of waiting for its first search update.
        let _ = self.events_tx.send(UiEvent::Results(initial_update));
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
            self.clear_preview_selection();
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
            InputCommand::TogglePreview => self.toggle_preview(),
        }
    }

    pub fn handle_key(&self, chord: KeyChord, repeat: bool) -> bool {
        let Some(action) = self.bindings.get(&chord) else {
            return false;
        };
        if !repeat {
            self.invoke_action(action);
        }
        true
    }

    pub fn invoke_action(&self, name: &str) {
        if !self.actions.contains(name) {
            eprintln!("unknown action: {name}");
            return;
        }
        let (generation, action_state) = {
            let mut state = self.state.lock().expect("view model poisoned");
            if state.action_generation.is_some() {
                return;
            }
            let Some(active) = state.active.as_ref() else {
                return;
            };
            let selection = state.results.get(state.selected).map(|result| {
                let metadata = active
                    .store
                    .snapshot()
                    .and_then(|source| source.delimited_metadata(result.node_index));
                match metadata {
                    Some(metadata) => ActionSelection {
                        item: metadata
                            .preview_item
                            .clone()
                            .unwrap_or_else(|| metadata.value.clone()),
                        value: metadata.value,
                        line: metadata.preview_center_line,
                    },
                    None => ActionSelection {
                        item: result.path.clone(),
                        value: result.path.clone(),
                        line: None,
                    },
                }
            });
            let picker = active.picker_state.clone();
            let query = state.search_text.clone();
            let generation = self
                .actions
                .reserve(name)
                .expect("action was checked before locking state");
            state.action_generation = Some(generation);
            (
                generation,
                ActionState {
                    selection,
                    picker,
                    query,
                },
            )
        };
        self.actions.request(name, generation, action_state);
    }

    fn toggle_preview(&self) {
        let visible = {
            let mut state = self.state.lock().expect("view model poisoned");
            state.preview_visible = !state.preview_visible;
            state.preview_visible
        };
        let _ = self
            .events_tx
            .send(UiEvent::PreviewVisibilityChanged { visible });
    }

    pub fn set_preview_visible(&self, visible: bool) {
        let changed = {
            let mut state = self.state.lock().expect("view model poisoned");
            if state.preview_visible == visible {
                false
            } else {
                state.preview_visible = visible;
                true
            }
        };
        if changed {
            let _ = self
                .events_tx
                .send(UiEvent::PreviewVisibilityChanged { visible });
        }
    }

    pub fn set_preview_visible_rows(&self, visible_rows: usize) {
        let view = {
            let mut state = self.state.lock().expect("view model poisoned");
            if state.preview_visible_rows == visible_rows {
                return;
            }
            state.preview_visible_rows = visible_rows;
            state.clamp_preview_viewport();
            state.preview_view()
        };
        if let Some(view) = view {
            let _ = self.events_tx.send(UiEvent::Preview(view));
        }
    }

    fn page_preview(&self, direction: isize) {
        let view = {
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
            state.preview_view()
        };
        if let Some(view) = view {
            let _ = self.events_tx.send(UiEvent::Preview(view));
        }
    }

    fn apply_preview_update(&self, update: &PreviewUpdate) -> Option<usize> {
        let mut state = self.state.lock().expect("view model poisoned");
        if let PreviewUpdate::Clear { generation } = update {
            state.preview_generation = *generation;
        } else {
            let generation = match update {
                PreviewUpdate::Ready { generation, .. }
                | PreviewUpdate::ImageReady { generation, .. }
                | PreviewUpdate::Error { generation, .. } => *generation,
                PreviewUpdate::Clear { .. } => unreachable!(),
            };
            if generation != state.preview_generation {
                return None;
            }
        }
        match update {
            PreviewUpdate::Clear { .. } | PreviewUpdate::Error { .. } => {
                state.preview_line_count = 0;
                state.preview_top_line = 0;
                state.preview_truncated = false;
            }
            PreviewUpdate::ImageReady { .. } => {
                state.preview_line_count = 0;
                state.preview_top_line = 0;
                state.preview_truncated = false;
            }
            PreviewUpdate::Ready {
                lines,
                truncated,
                center_line,
                ..
            } => {
                state.preview_line_count = lines.len();
                state.preview_truncated = *truncated;
                state.preview_top_line = center_line
                    .map(|line| {
                        line.saturating_sub(1)
                            .saturating_sub(state.preview_content_rows() / 2)
                    })
                    .unwrap_or(0);
                state.clamp_preview_viewport();
            }
        }
        state.preview_update = Some(update.clone());
        Some(state.preview_top_line)
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
        let value = {
            let state = self.state.lock().expect("view model poisoned");
            let Some(active) = state.active.as_ref() else {
                return;
            };
            state.results.get(state.selected).map(|result| {
                active
                    .store
                    .snapshot()
                    .and_then(|source| source.delimited_metadata(result.node_index))
                    .map_or_else(|| result.path.clone(), |metadata| metadata.value)
            })
        };
        match value {
            Some(value) => self.complete_accept(value),
            None => self.complete_cancelled(),
        }
    }

    fn complete_accept(&self, value: String) {
        self.complete(PickerResponse::selected(value));
    }

    fn complete_cancelled(&self) {
        self.complete(PickerResponse::cancelled());
    }

    fn complete(&self, response: PickerResponse) {
        let active = {
            let mut state = self.state.lock().expect("view model poisoned");
            state.active.take()
        };
        let Some(active) = active else {
            self.hide();
            return;
        };
        active.search_session.stop();
        self.clear_preview_selection();
        // Queue the UI hide before releasing the waiting FFI thread. This
        // preserves lifecycle ordering without making the model manipulate a
        // native window directly.
        self.hide();
        let _ = active.response_tx.send(response);
    }

    pub fn hide(&self) {
        let _ = self.events_tx.send(UiEvent::Hide);
    }

    pub fn cancel(&self) {
        self.actions.cancel();
        self.complete_cancelled();
    }

    #[cfg(windows)]
    fn transition_to_filewalker(&self, roots: Vec<String>) {
        let request = FileSystemPickerRequest {
            root_directories: roots,
            max_depth: i32::MAX,
            directories_only: false,
            files_only: false,
            search_string: None,
        };
        let store = request.run();
        let request_id = self.request_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let session = FuzzySearchSession::new(
            request_id,
            Arc::clone(&store),
            String::new(),
            self.search_update_tx.clone(),
        );
        let old_session = {
            let mut state = self.state.lock().expect("view model poisoned");
            let Some(active) = state.active.take() else {
                return;
            };
            let old_session = active.search_session;
            state.search_text.clear();
            state.results.clear();
            state.counters = UiCounters::default();
            state.selected = 0;
            state.viewport_start = 0;
            state.cursor_position = 0;
            state.cursor_selection_anchor = None;
            state.active = Some(ActiveRequest {
                id: request_id,
                response_tx: active.response_tx,
                search_session: session.clone(),
                store,
                picker_state: PickerState::Filewalker {
                    roots: request.root_directories,
                },
            });
            old_session
        };
        old_session.stop();
        self.clear_preview_selection();
        session.start();
    }

    #[cfg(not(windows))]
    fn transition_to_filewalker(&self, _roots: Vec<String>) {
        eprintln!("action requested filewalker, which is only available on Windows");
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
        let source = {
            let state = self.state.lock().expect("view model poisoned");
            state
                .active
                .as_ref()
                .and_then(|active| active.store.snapshot())
        };
        self.preview
            .selected_result_changed(update.results.get(update.selected_row), source.as_deref());
        let _ = self.events_tx.send(UiEvent::Results(update));
    }

    fn clear_preview_selection(&self) {
        self.preview.clear();
    }
}

impl State {
    fn preview_view(&self) -> Option<PreviewView> {
        self.preview_update.clone().map(|update| PreviewView::Text {
            update,
            top_line: self.preview_top_line,
        })
    }

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
    use crate::preview::{NativeWindowId, PreviewLine};

    fn modifiers(ctrl: bool, shift: bool) -> KeyModifiers {
        KeyModifiers {
            ctrl,
            shift,
            alt: false,
        }
    }

    #[test]
    fn semantic_input_inserts_unicode_and_replaces_selection() {
        let view_model = ViewModel::new(PreviewService::default());
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
        let view_model = ViewModel::new(PreviewService::default());
        view_model.insert_text("one two");
        view_model.handle_command(InputCommand::Backspace, modifiers(true, false));
        assert_eq!(view_model.current_search_text().text, "one ");
        view_model.handle_command(InputCommand::MoveHome, modifiers(false, false));
        view_model.handle_command(InputCommand::Delete, modifiers(true, false));
        assert_eq!(view_model.current_search_text().text, " ");
    }

    #[test]
    fn preview_paging_overlaps_one_row_and_clamps_to_document() {
        let view_model = ViewModel::new(PreviewService::default());
        view_model.set_preview_visible_rows(20);
        while view_model.events_rx.try_recv().is_ok() {}
        {
            let mut state = view_model.state.lock().unwrap();
            state.preview_line_count = 50;
            state.preview_update = Some(PreviewUpdate::Ready {
                generation: 1,
                lines: (0..50).map(|_| PreviewLine::default()).collect(),
                truncated: false,
                center_line: None,
            });
        }

        view_model.handle_command(InputCommand::PreviewPageDown, KeyModifiers::default());
        assert!(matches!(
            view_model.events_rx.try_recv(),
            Ok(UiEvent::Preview(PreviewView::Text { top_line: 19, .. }))
        ));

        view_model.handle_command(InputCommand::PreviewPageDown, KeyModifiers::default());
        assert!(matches!(
            view_model.events_rx.try_recv(),
            Ok(UiEvent::Preview(PreviewView::Text { top_line: 30, .. }))
        ));

        view_model.handle_command(InputCommand::PreviewPageUp, KeyModifiers::default());
        assert!(matches!(
            view_model.events_rx.try_recv(),
            Ok(UiEvent::Preview(PreviewView::Text { top_line: 11, .. }))
        ));
    }

    #[test]
    fn preview_visibility_toggle_publishes_the_new_state() {
        let view_model = ViewModel::new_with_preview_visibility(PreviewService::default(), false);

        view_model.handle_command(InputCommand::TogglePreview, KeyModifiers::default());
        assert!(matches!(
            view_model.events_rx.try_recv(),
            Ok(UiEvent::PreviewVisibilityChanged { visible: true })
        ));

        view_model.handle_command(InputCommand::TogglePreview, KeyModifiers::default());
        assert!(matches!(
            view_model.events_rx.try_recv(),
            Ok(UiEvent::PreviewVisibilityChanged { visible: false })
        ));
    }

    #[test]
    fn completed_preview_centers_the_requested_line() {
        let view_model = ViewModel::new(PreviewService::default());
        {
            let mut state = view_model.state.lock().unwrap();
            state.preview_generation = 7;
            state.preview_visible_rows = 20;
        }
        let lines: Arc<[PreviewLine]> = (0..100).map(|_| PreviewLine::default()).collect();

        let _ = view_model.apply_preview_update(&PreviewUpdate::Ready {
            generation: 7,
            lines,
            truncated: false,
            center_line: Some(42),
        });

        assert_eq!(view_model.state.lock().unwrap().preview_top_line, 31);
    }

    #[cfg(windows)]
    #[test]
    fn window_list_selection_emits_native_preview_target() {
        use crate::list_windows::WindowListItem;
        use crate::request::WindowListPickerRequest;
        use std::time::Duration;

        let view_model = ViewModel::new(PreviewService::new(
            crate::preview::PreviewConfig::NativeWindow,
        ));
        let events = view_model.subscribe();
        let runner = Arc::clone(&view_model);
        let request_thread = thread::spawn(move || {
            runner.run_request(&WindowListPickerRequest {
                items: vec![WindowListItem {
                    text: "00001234      100 app.exe Window title".into(),
                    hwnd: 0x1234,
                }],
            })
        });

        let mut selected = None;
        for _ in 0..20 {
            if let Ok(UiEvent::Preview(PreviewView::NativeWindow(window))) =
                events.recv_timeout(Duration::from_millis(100))
            {
                selected = window;
                break;
            }
        }
        assert_eq!(selected, Some(NativeWindowId(0x1234)));
        view_model.cancel();
        request_thread.join().unwrap().unwrap();
    }
}
