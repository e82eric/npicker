use std::collections::HashMap;
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::action::{
    ActionController, ActionEvent, ActionResolution, ActionService, ActionState, PickerState,
};
pub use crate::key_binding::KeyModifiers;
use crate::key_binding::{KeyChord, KeyName};
use crate::preview::{
    NativeWindowId, PreviewCoordinator, PreviewEvent, PreviewService, PreviewUpdate,
};
#[cfg(windows)]
use crate::request::FileSystemPickerRequest;
use crate::request::{PickerRequest, PickerResponse};
use crate::selection::SelectedItem;
use crate::source_store::{AnyItemSource, SharedStore};
use crate::structured_store::CompletionSuggestion;
use anyhow::{bail, Result};
use crossbeam_channel::{unbounded, Receiver, Sender};
use nfm_search_core::fuzzy_search_session::{FuzzySearchSession, FuzzySearchUpdate};
use nfm_search_core::search::{resolve_match_positions, SearchResult};
use nfm_search_core::timing;

const PICKER_DISPLAY_LIMIT: usize = 7;

#[derive(Clone, Debug)]
pub enum UiEvent {
    Show,
    Focus,
    Results(UiUpdate),
    Preview(PreviewView),
    PreviewVisibilityChanged { visible: bool },
    ShowToast { text: String, duration: Duration },
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
    pub results: Vec<DisplaySearchResult>,
    pub counters: UiCounters,
    pub selected_row: usize,
    pub header: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DisplaySearchResult {
    pub result: SearchResult,
    pub positions: Vec<usize>,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerActionOutcome {
    None,
    Refresh,
    RefreshWithToast(String),
    Toast(String),
}

pub type PickerAction =
    Arc<dyn Fn(Option<&SelectedItem>) -> Result<PickerActionOutcome, String> + Send + Sync>;
pub type PickerRefresh = Arc<dyn Fn() -> Result<Arc<SharedStore>, String> + Send + Sync>;
pub type PickerPreviewFormatter =
    Arc<dyn Fn(&SelectedItem) -> Result<String, String> + Send + Sync>;

#[derive(Clone)]
pub struct PickerInteractions {
    pub bindings: HashMap<KeyChord, PickerAction>,
    pub refresh: Option<PickerRefresh>,
    pub preview: Option<PickerPreviewFormatter>,
}

impl Default for PickerInteractions {
    fn default() -> Self {
        Self {
            bindings: HashMap::new(),
            refresh: None,
            preview: None,
        }
    }
}

pub(crate) enum ViewModelEvent {
    Preview(PreviewEvent),
    Action(ActionEvent),
}

struct State {
    active: Option<ActiveRequest>,
    search_text: String,
    result_query: String,
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
    suggestions: Vec<CompletionSuggestion>,
    suggestion_selected: usize,
}

#[derive(Clone)]
pub struct SearchInputState {
    pub text: String,
    pub cursor_position: usize,
    pub selection: Option<Range<usize>>,
}

#[derive(Clone, Debug, Default)]
pub struct AutocompleteState {
    pub suggestions: Vec<AutocompleteItem>,
    pub selected: usize,
}

#[derive(Clone, Debug, Default)]
pub struct AutocompleteItem {
    pub text: String,
    pub positions: Vec<usize>,
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
    CopySelection,
}

fn default_command(chord: KeyChord) -> Option<InputCommand> {
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
        KeyName::Character('p') if chord.modifiers.ctrl => Some(InputCommand::TogglePreview),
        KeyName::Character('c') if chord.modifiers.ctrl => Some(InputCommand::CopySelection),
        _ => None,
    }
}

fn command_suppresses_repeat(command: InputCommand) -> bool {
    matches!(
        command,
        InputCommand::Accept
            | InputCommand::Cancel
            | InputCommand::TogglePreview
            | InputCommand::CopySelection
    )
}

struct ActiveRequest {
    id: u64,
    response_tx: Sender<PickerResponse>,
    search_session: FuzzySearchSession<AnyItemSource, SharedStore>,
    store: Arc<SharedStore>,
    picker_state: PickerState,
    interactions: PickerInteractions,
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
                result_query: String::new(),
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
                suggestions: Vec::new(),
                suggestion_selected: 0,
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
            state.suggestions.clear();
            state.suggestion_selected = 0;
        }
        match event.result {
            Ok(ActionResolution::None) => {}
            Ok(ActionResolution::Complete) => match event.state.selection {
                Some(selection) => self.complete(PickerResponse::selected(selection)),
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
        self.run_request_with_interactions(request, PickerInteractions::default())
    }

    pub fn run_request_with_interactions<R>(
        self: &Arc<Self>,
        request: &R,
        interactions: PickerInteractions,
    ) -> Result<PickerResponse>
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
            state.result_query = state.search_text.clone();
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
                interactions,
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
        timing::write(format!("response status={}", response.status_label()));
        Ok(response)
    }

    fn update_search_text(self: &Arc<Self>, update: impl FnOnce(&mut State) -> bool) {
        let search_update = {
            let mut state = self.state.lock().expect("view model poisoned");

            let changed = update(&mut state);

            if changed {
                state.selected = 0;
                state.viewport_start = 0;
                let completions = state
                    .active
                    .as_ref()
                    .and_then(|active| active.store.snapshot())
                    .map(|source| source.completions(&state.search_text, state.cursor_position))
                    .unwrap_or_default();
                state.suggestions = completions;
                state.suggestion_selected = 0;

                state
                    .active
                    .as_ref()
                    .map(|active| (active.search_session.clone(), state.search_text.clone()))
            } else {
                None
            }
        };

        if let Some((search_session, search_text)) = search_update {
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
            InputCommand::CopySelection => self.copy_selection(),
        }
    }

    pub fn handle_key(self: &Arc<Self>, chord: KeyChord, repeat: bool) -> bool {
        if !chord.modifiers.ctrl && !chord.modifiers.alt && !chord.modifiers.shift {
            let has_suggestions = !self
                .state
                .lock()
                .expect("view model poisoned")
                .suggestions
                .is_empty();
            if has_suggestions {
                match chord.key {
                    KeyName::Up => {
                        self.move_suggestion(-1);
                        return true;
                    }
                    KeyName::Down => {
                        self.move_suggestion(1);
                        return true;
                    }
                    KeyName::Enter if !repeat => {
                        self.apply_suggestion();
                        return true;
                    }
                    KeyName::Escape if !repeat => {
                        let mut state = self.state.lock().expect("view model poisoned");
                        state.suggestions.clear();
                        state.suggestion_selected = 0;
                        return true;
                    }
                    _ => {}
                }
            }
        }
        if let Some(action) = self.bindings.get(&chord) {
            if !repeat {
                self.invoke_action(action);
            }
            return true;
        }
        let picker_action = {
            let state = self.state.lock().expect("view model poisoned");
            state
                .active
                .as_ref()
                .and_then(|active| active.interactions.bindings.get(&chord))
                .cloned()
        };
        if let Some(action) = picker_action {
            if !repeat {
                self.invoke_picker_action(&action);
            }
            return true;
        }
        let Some(command) = default_command(chord) else {
            return false;
        };
        if repeat && command_suppresses_repeat(command) {
            return true;
        }
        self.handle_command(command, chord.modifiers);
        true
    }

    fn move_suggestion(&self, delta: isize) {
        let mut state = self.state.lock().expect("view model poisoned");
        let len = state.suggestions.len();
        if len == 0 {
            return;
        }
        state.suggestion_selected = if delta < 0 {
            state
                .suggestion_selected
                .checked_sub(delta.unsigned_abs())
                .unwrap_or(len - 1)
        } else {
            (state.suggestion_selected + delta as usize) % len
        };
    }

    fn apply_suggestion(self: &Arc<Self>) {
        let suggestion = {
            let state = self.state.lock().expect("view model poisoned");
            state.suggestions.get(state.suggestion_selected).cloned()
        };
        let Some(suggestion) = suggestion else {
            return;
        };
        self.update_search_text(|state| {
            if suggestion.replace.end > state.search_text.len() {
                return false;
            }
            state
                .search_text
                .replace_range(suggestion.replace.clone(), &suggestion.replacement);
            state.cursor_position = suggestion.replace.start + suggestion.replacement.len();
            state.cursor_selection_anchor = None;
            true
        });
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
            let selection = Self::selected_item(&state);
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
        let selection = {
            let state = self.state.lock().expect("view model poisoned");
            Self::selected_item(&state)
        };
        match selection {
            Some(selection) => self.complete(PickerResponse::selected(selection)),
            None => self.complete_cancelled(),
        }
    }

    fn selected_item(state: &State) -> Option<SelectedItem> {
        let active = state.active.as_ref()?;
        let result = state.results.get(state.selected)?;
        let source = active.store.snapshot()?;
        let metadata = source.delimited_metadata(result.node_index);
        let structured_value = source.structured_value(result.node_index);
        let (item, value, line) = match metadata {
            Some(metadata) => (
                metadata
                    .preview_item
                    .unwrap_or_else(|| metadata.value.clone()),
                metadata.value,
                metadata.preview_center_line,
            ),
            None => {
                let value = structured_value.unwrap_or_else(|| result.path.clone());
                (value.clone(), value, None)
            }
        };
        Some(SelectedItem {
            item,
            value,
            line,
            fields: source.item_fields(result.node_index),
        })
    }

    fn copy_selection(&self) {
        let value = {
            let state = self.state.lock().expect("view model poisoned");
            Self::selected_item(&state).map(|item| item.value)
        };
        let Some(value) = value else {
            return;
        };
        let text = match crate::clipboard::copy_text(&value) {
            Ok(()) => format!("Copied '{value}' to clipboard"),
            Err(error) => format!("Copy failed: {error}"),
        };
        let _ = self.events_tx.send(UiEvent::ShowToast {
            text,
            duration: Duration::from_secs(3),
        });
    }

    fn invoke_picker_action(&self, action: &PickerAction) {
        let item = {
            let state = self.state.lock().expect("view model poisoned");
            Self::selected_item(&state)
        };

        match action(item.as_ref()) {
            Ok(PickerActionOutcome::None) => {}
            Ok(PickerActionOutcome::Refresh) => self.refresh_active_source(),
            Ok(PickerActionOutcome::RefreshWithToast(text)) => {
                self.show_toast(text);
                self.refresh_active_source();
            }
            Ok(PickerActionOutcome::Toast(text)) => self.show_toast(text),
            Err(error) => self.show_toast(error),
        }
    }

    fn refresh_active_source(&self) {
        let refresh = {
            let state = self.state.lock().expect("view model poisoned");
            let Some(active) = state.active.as_ref() else {
                return;
            };
            let Some(refresh) = active.interactions.refresh.clone() else {
                return;
            };
            refresh
        };

        match refresh() {
            Ok(store) => self.replace_active_source(store),
            Err(error) => self.show_toast(error),
        }
    }

    fn show_toast(&self, text: String) {
        let _ = self.events_tx.send(UiEvent::ShowToast {
            text,
            duration: Duration::from_secs(3),
        });
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

    pub fn focus(&self) {
        let _ = self.events_tx.send(UiEvent::Focus);
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
            state.result_query.clear();
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
                interactions: PickerInteractions::default(),
            });
            old_session
        };
        old_session.stop();
        self.clear_preview_selection();
        session.start();
    }

    fn replace_active_source(&self, store: Arc<SharedStore>) {
        let request_id = self.request_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let query = self
            .state
            .lock()
            .expect("view model poisoned")
            .search_text
            .clone();
        let session = FuzzySearchSession::new(
            request_id,
            Arc::clone(&store),
            query.clone(),
            self.search_update_tx.clone(),
        );
        let old_session = {
            let mut state = self.state.lock().expect("view model poisoned");
            let Some(active) = state.active.take() else {
                return;
            };
            let old_session = active.search_session;
            let picker_state = active.picker_state;
            let interactions = active.interactions;
            state.result_query = query;
            state.results.clear();
            state.counters = UiCounters::default();
            state.selected = 0;
            state.viewport_start = 0;
            state.suggestions.clear();
            state.suggestion_selected = 0;
            state.active = Some(ActiveRequest {
                id: request_id,
                response_tx: active.response_tx,
                search_session: session.clone(),
                store,
                picker_state,
                interactions,
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

    pub fn current_autocomplete(&self) -> AutocompleteState {
        let state = self.state.lock().expect("view model poisoned");
        AutocompleteState {
            suggestions: state
                .suggestions
                .iter()
                .map(|suggestion| AutocompleteItem {
                    text: suggestion.text.clone(),
                    positions: suggestion.positions.clone(),
                })
                .collect(),
            selected: state.suggestion_selected,
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
            let store = Arc::clone(&active.store);

            state.results = search_update.results;
            state.result_query = store
                .snapshot()
                .map(|source| source.effective_fuzzy_query(&search_update.query))
                .unwrap_or_else(|| search_update.query.clone());
            if let Some(source) = store.snapshot() {
                state.suggestions = source.completions(&state.search_text, state.cursor_position);
                state.suggestion_selected = state
                    .suggestion_selected
                    .min(state.suggestions.len().saturating_sub(1));
            }
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
        let (source, item, formatter) = {
            let state = self.state.lock().expect("view model poisoned");
            let source = state
                .active
                .as_ref()
                .and_then(|active| active.store.snapshot());
            let item = Self::selected_item(&state);
            let formatter = state
                .active
                .as_ref()
                .and_then(|active| active.interactions.preview.clone());
            (source, item, formatter)
        };
        if let Some(formatter) = formatter {
            self.preview
                .formatted_preview_changed(item.as_ref().map(|item| formatter(item)));
        } else {
            self.preview.selected_result_changed(
                update
                    .results
                    .get(update.selected_row)
                    .map(|display| &display.result),
                source.as_deref(),
            );
        }
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

        let display_limit = self.result_display_limit();
        let max_viewport_start = self.results.len().saturating_sub(display_limit);
        self.viewport_start = self.viewport_start.min(max_viewport_start);

        if self.selected < self.viewport_start {
            self.viewport_start = self.selected;
        } else if self.selected >= self.viewport_start + display_limit {
            self.viewport_start = self.selected + 1 - display_limit;
        }
    }

    fn structured_header(&self) -> Option<String> {
        self.active
            .as_ref()
            .and_then(|active| active.store.snapshot())
            .and_then(|source| source.structured_header(&self.search_text))
    }

    fn result_display_limit(&self) -> usize {
        PICKER_DISPLAY_LIMIT - usize::from(self.structured_header().is_some())
    }

    fn visible_update(&self) -> UiUpdate {
        let source = self
            .active
            .as_ref()
            .and_then(|active| active.store.snapshot());
        let header = source
            .as_ref()
            .and_then(|source| source.structured_header(&self.search_text));
        let display_limit = PICKER_DISPLAY_LIMIT - usize::from(header.is_some());
        let visible_results: Vec<_> = self
            .results
            .iter()
            .skip(self.viewport_start)
            .take(display_limit)
            .cloned()
            .map(|mut result| {
                if let Some(display) = source.as_ref().and_then(|source| {
                    source.structured_display(result.node_index, &self.search_text)
                }) {
                    result.path = display;
                }
                let positions = resolve_match_positions(&self.result_query, &result.path);
                DisplaySearchResult { result, positions }
            })
            .collect();
        let selected_row = if visible_results.is_empty() {
            0
        } else {
            self.selected.saturating_sub(self.viewport_start)
        };

        UiUpdate {
            results: visible_results,
            counters: UiCounters {
                displayed: self.results.len().min(display_limit),
                ..self.counters.clone()
            },
            selected_row,
            header,
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
    fn default_control_c_maps_to_copy_selection() {
        let command = default_command(KeyChord {
            key: KeyName::Character('c'),
            modifiers: modifiers(true, false),
        });
        assert_eq!(command, Some(InputCommand::CopySelection));
        assert!(command_suppresses_repeat(command.unwrap()));
    }

    #[test]
    fn picker_specific_shortcuts_are_not_builtin_commands() {
        for key in ['k', 'r'] {
            assert_eq!(
                default_command(KeyChord {
                    key: KeyName::Character(key),
                    modifiers: modifiers(true, false),
                }),
                None
            );
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
        view_model.select_current();
        let response = request_thread.join().unwrap().unwrap();
        assert_eq!(
            response,
            PickerResponse::Selected(SelectedItem {
                item: "00001234      100 app.exe Window title".into(),
                value: "00001234      100 app.exe Window title".into(),
                line: None,
                fields: HashMap::from([("NativeWindow".into(), "4660".into())]),
            })
        );
    }
}
