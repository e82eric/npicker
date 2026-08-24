use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::action::{
    ActionController, ActionDefinition, ActionResolution, ActionState, PickerState, PreparedAction,
};
pub use crate::key_binding::KeyModifiers;
use crate::key_binding::{format_key_chord, KeyChord, KeyName};
use crate::preview::{
    NativeWindowId, PreviewEvent, PreviewFactory, PreviewRoutes, PreviewUpdate, SelectionPreview,
};
use crate::request::{PickerRequest, PickerResponse};
use crate::PickerItem;
use anyhow::{bail, Result};
use crossbeam_channel::{unbounded, Receiver, Sender};
use nfm_search_core::fuzzy_search_session::FuzzySearchUpdate;
use nfm_search_core::search::{resolve_match_positions, SearchResult};
use nfm_search_core::source::SearchSource;
use nfm_search_core::store::{ItemsSource, SearchCompletion};
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
    Exit(i32),
}

pub trait UiEventSink: Send + Sync {
    fn publish(&self, event: UiEvent);
}

#[cfg(test)]
struct RecordingUiEventSink(Sender<UiEvent>);

#[cfg(test)]
impl UiEventSink for RecordingUiEventSink {
    fn publish(&self, event: UiEvent) {
        let _ = self.0.send(event);
    }
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

pub(crate) struct PickerRender {
    header: Option<String>,
    row_count: usize,
}

pub(crate) struct PickerQueryUpdate {
    effective_query: String,
    completions: Vec<SearchCompletion>,
}

pub struct ViewModel {
    state: Mutex<State>,
    request_generation: AtomicU64,
    search_update_tx: Sender<FuzzySearchUpdate>,
    ui_events: Arc<dyn UiEventSink>,
    internal_events: Sender<ViewModelEvent>,
    actions: ActionController,
    bindings: HashMap<KeyChord, String>,
}

pub type PickerAction<I> =
    Arc<dyn Fn(Option<&I>) -> Result<PickerActionOutcome, String> + Send + Sync>;
pub type PickerRefresh<I> =
    Arc<dyn Fn() -> Result<Arc<dyn SearchSource<Item = I>>, String> + Send + Sync>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PickerActionOutcome {
    None,
    Refresh,
    RefreshWithToast(String),
    Toast(String),
}
pub struct SourceTransition<I> {
    pub source: Arc<dyn SearchSource<Item = I>>,
    pub interactions: PickerInteractions<I>,
    pub picker_state: PickerState,
    pub clear_query: bool,
}

impl<I: PickerItem> SourceTransition<I> {
    pub fn new(
        source: Arc<dyn SearchSource<Item = I>>,
        picker_state: PickerState,
        interactions: PickerInteractions<I>,
        clear_query: bool,
    ) -> SourceTransition<I> {
        SourceTransition {
            source,
            interactions,
            picker_state,
            clear_query,
        }
    }
}
pub type SourceResolver<I> =
    Arc<dyn Fn(PickerState) -> Result<SourceTransition<I>, String> + Send + Sync>;
pub struct PickerInteractions<I> {
    pub actions: HashMap<String, ActionDefinition<I>>,
    pub action_bindings: HashMap<KeyChord, String>,
    pub source_resolver: Option<SourceResolver<I>>,
    pub bindings: HashMap<KeyChord, PickerAction<I>>,
    pub refresh: Option<PickerRefresh<I>>,
    pub preview_factory: Arc<PreviewFactory>,
    pub preview_routes: PreviewRoutes<I>,
}

impl<I> Clone for PickerInteractions<I> {
    fn clone(&self) -> Self {
        Self {
            actions: self.actions.clone(),
            action_bindings: self.action_bindings.clone(),
            source_resolver: self.source_resolver.clone(),
            bindings: self.bindings.clone(),
            refresh: self.refresh.clone(),
            preview_factory: Arc::clone(&self.preview_factory),
            preview_routes: self.preview_routes.clone(),
        }
    }
}

impl<I> Default for PickerInteractions<I> {
    fn default() -> Self {
        Self {
            actions: HashMap::new(),
            action_bindings: HashMap::new(),
            source_resolver: None,
            bindings: HashMap::new(),
            refresh: None,
            preview_factory: Arc::new(PreviewFactory::default()),
            preview_routes: PreviewRoutes::default(),
        }
    }
}

pub(crate) enum ViewModelEvent {
    Preview(PreviewEvent),
    Action(SessionActionEvent),
}

pub(crate) struct SessionActionEvent {
    generation: u64,
    result: Result<SessionActionResolution, String>,
}

pub(crate) struct PreparedResponse {
    send: Box<dyn FnOnce() -> Result<(), String> + Send>,
}

impl PreparedResponse {
    fn new<I: Send + 'static>(
        sender: Sender<PickerResponse<I>>,
        response: PickerResponse<I>,
    ) -> Self {
        Self {
            send: Box::new(move || {
                sender
                    .send(response)
                    .map_err(|_| "picker response receiver disconnected".to_owned())
            }),
        }
    }

    fn send(self) -> Result<(), String> {
        (self.send)()
    }
}

pub(crate) enum SessionActionResolution {
    None,
    Complete(PreparedResponse),
    Toast(String),
    Refresh {
        session: Arc<dyn PickerSession>,
        toast: Option<String>,
    },
    Picker {
        session: Arc<dyn PickerSession>,
        clear_query: bool,
    },
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
    suggestions: Vec<SearchCompletion>,
    suggestion_selected: usize,
    keybinding_help_visible: bool,
    keybinding_help_offset: usize,
    keybinding_help_visible_rows: usize,
}

#[derive(Clone, Debug)]
pub struct KeyBindingHelp {
    pub chord: String,
    pub description: String,
    pub heading: bool,
}

#[derive(Clone, Debug)]
pub struct KeyBindingHelpState {
    pub entries: Vec<KeyBindingHelp>,
    pub first: usize,
    pub total: usize,
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
    ToggleKeyBindingHelp,
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
        KeyName::Character('/') if chord.modifiers.ctrl && chord.modifiers.shift => {
            Some(InputCommand::ToggleKeyBindingHelp)
        }
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
            | InputCommand::ToggleKeyBindingHelp
    )
}

struct ActiveRequest {
    id: u64,
    picker: Arc<dyn PickerSession>,
}

pub(crate) trait PickerSession: Send + Sync {
    fn start_search(&self, id: u64, query: String, updates: Sender<FuzzySearchUpdate>);
    fn set_query(&self, query: String);
    fn stop_search(&self);
    fn prepare_selection(&self, index: usize) -> Option<PreparedResponse>;
    fn prepare_cancelled(&self) -> PreparedResponse;
    fn has_action(&self, name: &str) -> bool;
    fn prepare_action(
        &self,
        name: &str,
        index: Option<usize>,
        query: String,
    ) -> Option<PreparedAction>;
    fn value(&self, index: usize) -> Option<String>;
    fn header(&self, query: &str) -> Option<String>;
    fn render(&self, query: &str, indices: &[usize], rows: &mut [Option<String>]) -> PickerRender;
    fn query_update(&self, query: &str, input: &str, cursor: usize) -> PickerQueryUpdate;
    fn has_picker_action(&self, chord: &KeyChord) -> bool;
    fn picker_binding_help(&self) -> Vec<(KeyChord, String)>;
    fn prepare_picker_action(
        &self,
        chord: &KeyChord,
        index: Option<usize>,
        query: String,
    ) -> Option<PreparedAction>;
    fn selection_changed(&self, index: Option<usize>);
    fn clear_preview(&self);
}

pub(crate) struct TypedPickerSession<I: PickerItem> {
    source: Arc<dyn SearchSource<Item = I>>,
    interactions: PickerInteractions<I>,
    picker_state: PickerState,
    preview: Arc<dyn SelectionPreview<I>>,
    preview_events: Sender<ViewModelEvent>,
    response_tx: Sender<PickerResponse<I>>,
}

impl<I: PickerItem> TypedPickerSession<I> {
    pub(crate) fn new(
        source: Arc<dyn SearchSource<Item = I>>,
        interactions: PickerInteractions<I>,
        picker_state: PickerState,
        events: Sender<ViewModelEvent>,
        response_tx: Sender<PickerResponse<I>>,
    ) -> Arc<Self> {
        let preview = interactions
            .preview_factory
            .create(events.clone(), interactions.preview_routes.clone());
        Arc::new(Self {
            source,
            interactions,
            picker_state,
            preview,
            preview_events: events,
            response_tx,
        })
    }
}

impl<I: PickerItem> PickerSession for TypedPickerSession<I> {
    fn start_search(&self, id: u64, query: String, updates: Sender<FuzzySearchUpdate>) {
        self.source.start_search(id, query, updates)
    }
    fn set_query(&self, query: String) {
        self.source.set_query(query);
    }
    fn stop_search(&self) {
        self.source.stop_search();
    }
    fn prepare_selection(&self, index: usize) -> Option<PreparedResponse> {
        let item = self
            .source
            .snapshot()
            .and_then(|source| source.item(index))?;
        Some(PreparedResponse::new(
            self.response_tx.clone(),
            PickerResponse::Selected(item),
        ))
    }
    fn prepare_cancelled(&self) -> PreparedResponse {
        PreparedResponse::new(self.response_tx.clone(), PickerResponse::Cancelled)
    }
    fn has_action(&self, name: &str) -> bool {
        self.interactions.actions.contains_key(name)
    }
    fn prepare_action(
        &self,
        name: &str,
        index: Option<usize>,
        query: String,
    ) -> Option<PreparedAction> {
        let definition = self.interactions.actions.get(name).cloned()?;
        let item = index.and_then(|index| self.source.snapshot()?.item(index));
        let response_tx = self.response_tx.clone();
        let events = self.preview_events.clone();
        let resolver = self.interactions.source_resolver.clone();
        let preview_events = self.preview_events.clone();
        let transition_response_tx = self.response_tx.clone();
        Some(PreparedAction::new(
            definition,
            ActionState {
                selection: item,
                picker: self.picker_state.clone(),
                query,
            },
            move |event| {
                let result = match event.result {
                    Ok(ActionResolution::None) => Ok(SessionActionResolution::None),
                    Ok(ActionResolution::Complete) => match event.state.selection {
                        Some(item) => Ok(SessionActionResolution::Complete(PreparedResponse::new(
                            response_tx,
                            PickerResponse::Selected(item),
                        ))),
                        None => Err("action resolver cannot complete without a selection".into()),
                    },
                    Ok(ActionResolution::Picker(picker)) => match resolver {
                        Some(resolve) => resolve(picker).map(|transition| {
                            let session = Self::new(
                                transition.source,
                                transition.interactions,
                                transition.picker_state,
                                preview_events,
                                transition_response_tx,
                            ) as Arc<dyn PickerSession>;
                            SessionActionResolution::Picker {
                                session,
                                clear_query: transition.clear_query,
                            }
                        }),
                        None => Err("no picker source resolver is configured".into()),
                    },
                    Err(message) => Err(message),
                };
                let _ = events.send(ViewModelEvent::Action(SessionActionEvent {
                    generation: event.generation,
                    result,
                }));
            },
        ))
    }
    fn value(&self, index: usize) -> Option<String> {
        Some(self.source.snapshot()?.item(index)?.value().to_owned())
    }
    fn header(&self, query: &str) -> Option<String> {
        self.source.snapshot()?.header(query)
    }
    fn render(&self, query: &str, indices: &[usize], rows: &mut [Option<String>]) -> PickerRender {
        let Some(snapshot) = self.source.snapshot() else {
            return PickerRender {
                header: None,
                row_count: indices.len().min(rows.len()),
            };
        };
        let header = snapshot.header(query);
        let row_count = indices
            .len()
            .min(rows.len())
            .min(PICKER_DISPLAY_LIMIT - usize::from(header.is_some()));
        for (row, &index) in rows[..row_count].iter_mut().zip(indices) {
            *row = snapshot.display_text(index, query);
        }
        PickerRender { header, row_count }
    }
    fn query_update(&self, query: &str, input: &str, cursor: usize) -> PickerQueryUpdate {
        let Some(snapshot) = self.source.snapshot() else {
            return PickerQueryUpdate {
                effective_query: query.to_owned(),
                completions: Vec::new(),
            };
        };
        PickerQueryUpdate {
            effective_query: snapshot.effective_query(query),
            completions: snapshot.completions(input, cursor),
        }
    }
    fn has_picker_action(&self, chord: &KeyChord) -> bool {
        self.interactions.bindings.contains_key(chord)
            || self.interactions.action_bindings.contains_key(chord)
    }
    fn picker_binding_help(&self) -> Vec<(KeyChord, String)> {
        self.interactions
            .action_bindings
            .iter()
            .map(|(chord, action)| (*chord, action.clone()))
            .chain(
                self.interactions
                    .bindings
                    .keys()
                    .map(|chord| (*chord, "Picker action".to_owned())),
            )
            .collect()
    }
    fn prepare_picker_action(
        &self,
        chord: &KeyChord,
        index: Option<usize>,
        query: String,
    ) -> Option<PreparedAction> {
        if let Some(action) = self.interactions.action_bindings.get(chord) {
            return self.prepare_action(action, index, query);
        }
        let action = Arc::clone(self.interactions.bindings.get(chord)?);
        let item = index.and_then(|i| self.source.snapshot()?.item(i));
        let refresh = self.interactions.refresh.clone();
        let interactions = self.interactions.clone();
        let picker_state = self.picker_state.clone();
        let preview_events = self.preview_events.clone();
        let response_tx = self.response_tx.clone();
        let events = self.preview_events.clone();
        Some(PreparedAction::from_task(
            move || {
                let outcome = action(item.as_ref())?;
                match outcome {
                    PickerActionOutcome::None => Ok(SessionActionResolution::None),
                    PickerActionOutcome::Toast(text) => Ok(SessionActionResolution::Toast(text)),
                    PickerActionOutcome::Refresh | PickerActionOutcome::RefreshWithToast(_) => {
                        let source = refresh.as_ref().ok_or_else(|| {
                            "picker action requested refresh without a refresh handler".to_owned()
                        })?()?;
                        let toast = match outcome {
                            PickerActionOutcome::RefreshWithToast(text) => Some(text),
                            _ => None,
                        };
                        let session = Self::new(
                            source,
                            interactions,
                            picker_state,
                            preview_events,
                            response_tx,
                        ) as Arc<dyn PickerSession>;
                        Ok(SessionActionResolution::Refresh { session, toast })
                    }
                }
            },
            move |generation, result| {
                let _ = events.send(ViewModelEvent::Action(SessionActionEvent {
                    generation,
                    result,
                }));
            },
        ))
    }
    fn selection_changed(&self, index: Option<usize>) {
        let item = index.and_then(|i| self.source.snapshot()?.item(i));
        self.preview.selection_changed(item.as_ref());
    }
    fn clear_preview(&self) {
        self.preview.clear();
    }
}

impl ViewModel {
    pub fn new(ui_events: Arc<dyn UiEventSink>) -> Arc<Self> {
        Self::new_with_bindings(HashMap::new(), true, ui_events)
    }

    pub fn new_with_preview_visibility(
        preview_visible: bool,
        ui_events: Arc<dyn UiEventSink>,
    ) -> Arc<Self> {
        Self::new_with_bindings(HashMap::new(), preview_visible, ui_events)
    }

    pub fn new_with_bindings(
        bindings: HashMap<KeyChord, String>,
        preview_visible: bool,
        ui_events: Arc<dyn UiEventSink>,
    ) -> Arc<Self> {
        let (internal_events_tx, internal_events_rx) = unbounded();
        let (search_update_tx, search_update_rx) = unbounded();
        let actions = ActionController::new();

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
                keybinding_help_visible: false,
                keybinding_help_offset: 0,
                keybinding_help_visible_rows: 1,
            }),
            request_generation: AtomicU64::new(0),
            search_update_tx,
            ui_events,
            internal_events: internal_events_tx,
            actions,
            bindings,
        });

        this.spawn_event_thread(internal_events_rx, search_update_rx);
        this
    }

    fn spawn_event_thread(
        self: &Arc<Self>,
        events: Receiver<ViewModelEvent>,
        search_updates: Receiver<FuzzySearchUpdate>,
    ) {
        let weak = Arc::downgrade(self);
        thread::spawn(move || loop {
            crossbeam_channel::select! {
                recv(events) -> event => {
                    let Ok(event) = event else { break };
                    let Some(view_model) = weak.upgrade() else { break };
                    match event {
                        ViewModelEvent::Preview(event) => view_model.handle_preview_event(event),
                        ViewModelEvent::Action(event) => view_model.handle_action_event(event),
                    }
                }
                recv(search_updates) -> update => {
                    let Ok(update) = update else { break };
                    let Some(view_model) = weak.upgrade() else { break };
                    view_model.apply_search_update(update);
                }
            }
        });
    }

    fn handle_action_event(&self, event: SessionActionEvent) {
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
            Ok(SessionActionResolution::None) => {}
            Ok(SessionActionResolution::Complete(response)) => self.finish_active(response),
            Ok(SessionActionResolution::Toast(text)) => self.show_toast(text),
            Ok(SessionActionResolution::Refresh { session, toast }) => {
                if let Some(text) = toast {
                    self.show_toast(text);
                }
                self.replace_active_session(session);
            }
            Ok(SessionActionResolution::Picker {
                session,
                clear_query,
            }) => self.replace_active_source(session, clear_query),
            Err(message) => self.show_toast(message),
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
            self.publish_ui_event(UiEvent::Preview(view));
        }
    }

    fn publish_ui_event(&self, event: UiEvent) {
        self.ui_events.publish(event);
    }

    pub fn run_request<R>(
        self: &Arc<Self>,
        request: &R,
    ) -> Result<PickerResponse<<R::Source as ItemsSource>::Item>>
    where
        R: PickerRequest,
    {
        self.run_request_with_interactions(request, PickerInteractions::default())
    }

    pub fn run_request_with_interactions<R>(
        self: &Arc<Self>,
        request: &R,
        interactions: PickerInteractions<<<R as PickerRequest>::Source as ItemsSource>::Item>,
    ) -> Result<PickerResponse<<R::Source as ItemsSource>::Item>>
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
        let picker: Arc<dyn PickerSession> = TypedPickerSession::new(
            store,
            interactions,
            request.picker_state(),
            self.internal_events.clone(),
            response_tx,
        );

        let query = request.search_string().unwrap_or_default().to_owned();
        let initial_update = {
            let mut state = self.state.lock().expect("view model poisoned");
            if state.active.is_some() {
                bail!("A picker request is already active.");
            }

            let cursor_position = query.len();
            state.search_text = query.clone();
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
                picker: Arc::clone(&picker),
            });
            state.visible_update()
        };
        picker.clear_preview();

        // A long-lived UI may still contain the previous picker's rows. Publish the cleared
        // state before showing the next request instead of waiting for its first search update.
        self.publish_ui_event(UiEvent::Results(initial_update));
        self.publish_ui_event(UiEvent::Show);
        picker.start_search(request_id, query, self.search_update_tx.clone());

        let response = response_rx.recv().unwrap_or(PickerResponse::Cancelled);
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
                    .map(|active| {
                        active
                            .picker
                            .query_update(
                                &state.search_text,
                                &state.search_text,
                                state.cursor_position,
                            )
                            .completions
                    })
                    .unwrap_or_default();
                state.suggestions = completions;
                state.suggestion_selected = 0;

                state
                    .active
                    .as_ref()
                    .map(|active| (Arc::clone(&active.picker), state.search_text.clone()))
            } else {
                None
            }
        };

        if let Some((picker, search_text)) = search_update {
            picker.set_query(search_text);
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
            InputCommand::ToggleKeyBindingHelp => {
                let mut state = self.state.lock().expect("view model poisoned");
                state.keybinding_help_visible = !state.keybinding_help_visible;
                state.keybinding_help_offset = 0;
                state.suggestions.clear();
                state.suggestion_selected = 0;
            }
        }
    }

    pub fn handle_key(self: &Arc<Self>, chord: KeyChord, repeat: bool) -> bool {
        let help_visible = self
            .state
            .lock()
            .expect("view model poisoned")
            .keybinding_help_visible;
        if help_visible {
            let mut state = self.state.lock().expect("view model poisoned");
            if !repeat && default_command(chord) == Some(InputCommand::ToggleKeyBindingHelp) {
                state.keybinding_help_visible = false;
            } else {
                match chord.key {
                    KeyName::Escape if !repeat => state.keybinding_help_visible = false,
                    KeyName::Up => {
                        state.keybinding_help_offset =
                            state.keybinding_help_offset.saturating_sub(1)
                    }
                    KeyName::Down => {
                        state.keybinding_help_offset =
                            state.keybinding_help_offset.saturating_add(1)
                    }
                    KeyName::PageUp => {
                        state.keybinding_help_offset = state
                            .keybinding_help_offset
                            .saturating_sub(state.keybinding_help_visible_rows)
                    }
                    KeyName::PageDown => {
                        state.keybinding_help_offset = state
                            .keybinding_help_offset
                            .saturating_add(state.keybinding_help_visible_rows)
                    }
                    KeyName::Home => state.keybinding_help_offset = 0,
                    KeyName::End => state.keybinding_help_offset = usize::MAX,
                    _ => {}
                }
            }
            return true;
        }
        if default_command(chord) == Some(InputCommand::ToggleKeyBindingHelp) {
            if !repeat {
                self.handle_command(InputCommand::ToggleKeyBindingHelp, chord.modifiers);
            }
            return true;
        }
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
        if repeat {
            let handled = {
                let state = self.state.lock().expect("view model poisoned");
                let Some(active) = state.active.as_ref() else {
                    return false;
                };
                active.picker.has_picker_action(&chord)
            };
            if handled {
                return true;
            }
        } else {
            let picker_action = {
                let state = self.state.lock().expect("view model poisoned");
                let Some(active) = state.active.as_ref() else {
                    return false;
                };
                let index = state
                    .results
                    .get(state.selected)
                    .map(|result| result.node_index);
                active
                    .picker
                    .prepare_picker_action(&chord, index, state.search_text.clone())
            };
            if let Some(action) = picker_action {
                self.request_action(action);
                return true;
            }
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
        {
            let mut state = self.state.lock().expect("view model poisoned");
            if state.action_generation.is_some() {
                return;
            }
            let Some(active) = state.active.as_ref() else {
                return;
            };
            if !active.picker.has_action(name) {
                eprintln!("unknown action: {name}");
                return;
            }
            let active_picker = Arc::clone(&active.picker);
            let index = state
                .results
                .get(state.selected)
                .map(|result| result.node_index);
            let query = state.search_text.clone();
            let Some(action) = active_picker.prepare_action(name, index, query) else {
                return;
            };
            let generation = self.actions.request(action);
            state.action_generation = Some(generation);
        }
    }

    fn request_action(&self, action: PreparedAction) {
        let mut state = self.state.lock().expect("view model poisoned");
        if state.action_generation.is_some() {
            return;
        }
        let generation = self.actions.request(action);
        state.action_generation = Some(generation);
    }

    fn toggle_preview(&self) {
        let visible = {
            let mut state = self.state.lock().expect("view model poisoned");
            state.preview_visible = !state.preview_visible;
            state.preview_visible
        };
        self.publish_ui_event(UiEvent::PreviewVisibilityChanged { visible });
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
            self.publish_ui_event(UiEvent::PreviewVisibilityChanged { visible });
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
            self.publish_ui_event(UiEvent::Preview(view));
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
            self.publish_ui_event(UiEvent::Preview(view));
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
            state.active.as_ref().and_then(|active| {
                state
                    .results
                    .get(state.selected)
                    .map(|result| (Arc::clone(&active.picker), result.node_index))
            })
        };
        let response = selection.and_then(|(picker, index)| picker.prepare_selection(index));
        match response {
            Some(response) => self.finish_active(response),
            None => self.complete_cancelled(),
        }
    }

    fn copy_selection(&self) {
        let value = {
            let state = self.state.lock().expect("view model poisoned");
            state.active.as_ref().and_then(|active| {
                state
                    .results
                    .get(state.selected)
                    .and_then(|result| active.picker.value(result.node_index))
            })
        };
        let Some(value) = value else {
            return;
        };
        let text = match crate::clipboard::copy_text(&value) {
            Ok(()) => format!("Copied '{value}' to clipboard"),
            Err(error) => format!("Copy failed: {error}"),
        };
        self.publish_ui_event(UiEvent::ShowToast {
            text,
            duration: Duration::from_secs(3),
        });
    }

    fn show_toast(&self, text: String) {
        self.publish_ui_event(UiEvent::ShowToast {
            text,
            duration: Duration::from_secs(3),
        });
    }

    fn complete_cancelled(&self) {
        let response = {
            let state = self.state.lock().expect("view model poisoned");
            state
                .active
                .as_ref()
                .map(|active| active.picker.prepare_cancelled())
        };
        if let Some(response) = response {
            self.finish_active(response);
        } else {
            self.hide();
        }
    }

    fn finish_active(&self, response: PreparedResponse) {
        let active = {
            let mut state = self.state.lock().expect("view model poisoned");
            state.active.take()
        };
        let Some(active) = active else {
            return;
        };
        active.picker.stop_search();
        active.picker.clear_preview();
        // Queue the UI hide before releasing the waiting FFI thread. This
        // preserves lifecycle ordering without making the model manipulate a
        // native window directly.
        self.hide();
        if let Err(message) = response.send() {
            eprintln!("{message}");
        }
    }

    pub fn hide(&self) {
        self.publish_ui_event(UiEvent::Hide);
    }

    pub fn focus(&self) {
        self.publish_ui_event(UiEvent::Focus);
    }

    pub fn exit(&self, code: i32) {
        self.publish_ui_event(UiEvent::Exit(code));
    }

    pub fn cancel(&self) {
        self.actions.cancel();
        self.complete_cancelled();
    }

    fn replace_active_source(&self, picker: Arc<dyn PickerSession>, clear_query: bool) {
        let request_id = self.request_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let query = if clear_query {
            String::new()
        } else {
            self.state
                .lock()
                .expect("view model poisoned")
                .search_text
                .clone()
        };
        let (old_picker, initial_update) = {
            let mut state = self.state.lock().expect("view model poisoned");
            let Some(active) = state.active.take() else {
                return;
            };
            if clear_query {
                state.cursor_position = 0;
                state.cursor_selection_anchor = None;
            }
            state.search_text = query.clone();
            state.result_query = query.clone();
            state.results.clear();
            state.counters = UiCounters::default();
            state.selected = 0;
            state.viewport_start = 0;
            state.suggestions.clear();
            state.suggestion_selected = 0;
            state.active = Some(ActiveRequest {
                id: request_id,
                picker: Arc::clone(&picker),
            });
            (active.picker, state.visible_update())
        };
        old_picker.stop_search();
        old_picker.clear_preview();
        //self.publish_ui_event(UiEvent::Results(initial_update));
        picker.start_search(request_id, query, self.search_update_tx.clone());
    }

    fn replace_active_session(&self, picker: Arc<dyn PickerSession>) {
        self.replace_active_source(picker, false);
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

    pub fn current_keybinding_help(&self, visible_rows: usize) -> Option<KeyBindingHelpState> {
        let mut state = self.state.lock().expect("view model poisoned");
        if !state.keybinding_help_visible {
            return None;
        }
        let mut sections: Vec<(&str, Vec<(String, String)>)> = Vec::new();
        let mut custom = self
            .bindings
            .iter()
            .map(|(chord, action)| (format_key_chord(*chord), action.clone()))
            .collect::<Vec<_>>();
        custom.sort();
        custom.dedup();
        if !custom.is_empty() {
            sections.push(("Custom", custom));
        }
        if let Some(active) = state.active.as_ref() {
            let mut picker = active
                .picker
                .picker_binding_help()
                .into_iter()
                .map(|(chord, action)| (format_key_chord(chord), action))
                .collect::<Vec<_>>();
            picker.sort();
            picker.dedup();
            if !picker.is_empty() {
                sections.push(("Picker", picker));
            }
        }
        sections.extend([
            (
                "General",
                vec![
                    ("Enter".to_owned(), "Accept selection".to_owned()),
                    ("Escape".to_owned(), "Close help / cancel".to_owned()),
                    ("Ctrl+Shift+/".to_owned(), "Toggle key bindings".to_owned()),
                ],
            ),
            (
                "Results",
                vec![
                    ("Up / Down".to_owned(), "Move selection".to_owned()),
                    ("Ctrl+C".to_owned(), "Copy selection".to_owned()),
                ],
            ),
            (
                "Preview",
                vec![
                    ("Ctrl+P".to_owned(), "Toggle preview".to_owned()),
                    (
                        "Ctrl+Page Up / Down".to_owned(),
                        "Scroll preview".to_owned(),
                    ),
                ],
            ),
            (
                "Search box",
                vec![
                    ("Left / Right".to_owned(), "Move cursor".to_owned()),
                    (
                        "Ctrl+Left / Right".to_owned(),
                        "Move cursor by word".to_owned(),
                    ),
                    ("Shift+Arrows".to_owned(), "Select query text".to_owned()),
                    ("Home / End".to_owned(), "Move to query boundary".to_owned()),
                    (
                        "Backspace / Delete".to_owned(),
                        "Delete query text".to_owned(),
                    ),
                ],
            ),
        ]);
        let mut seen_chords = HashSet::new();
        let sections = sections
            .into_iter()
            .filter_map(|(section, mut bindings)| {
                bindings.retain(|(chord, _)| seen_chords.insert(chord.to_ascii_lowercase()));
                (!bindings.is_empty()).then_some((section, bindings))
            })
            .collect::<Vec<_>>();
        let mut entries = Vec::new();
        for (index, (section, bindings)) in sections.into_iter().enumerate() {
            if index > 0 {
                entries.push(KeyBindingHelp {
                    chord: String::new(),
                    description: String::new(),
                    heading: true,
                });
            }
            entries.push(KeyBindingHelp {
                chord: section.to_owned(),
                description: String::new(),
                heading: true,
            });
            entries.extend(
                bindings
                    .into_iter()
                    .map(|(chord, description)| KeyBindingHelp {
                        chord,
                        description,
                        heading: false,
                    }),
            );
        }
        let total = entries.len();
        let visible_rows = visible_rows.max(1);
        state.keybinding_help_visible_rows = visible_rows;
        state.keybinding_help_offset = state
            .keybinding_help_offset
            .min(total.saturating_sub(visible_rows));
        let first = state.keybinding_help_offset;
        Some(KeyBindingHelpState {
            entries: entries.into_iter().skip(first).take(visible_rows).collect(),
            first,
            total,
        })
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

            let picker = Arc::clone(&active.picker);
            let query_update = picker.query_update(
                &search_update.query,
                &state.search_text,
                state.cursor_position,
            );
            state.results = search_update.results;
            state.result_query = query_update.effective_query;
            state.suggestions = query_update.completions;
            state.suggestion_selected = state
                .suggestion_selected
                .min(state.suggestions.len().saturating_sub(1));
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
        let selection = {
            let state = self.state.lock().expect("view model poisoned");
            state.active.as_ref().map(|active| {
                (
                    Arc::clone(&active.picker),
                    state
                        .results
                        .get(state.selected)
                        .map(|result| result.node_index),
                )
            })
        };
        if let Some((picker, index)) = selection {
            picker.selection_changed(index);
        }
        self.publish_ui_event(UiEvent::Results(update));
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
            .and_then(|active| active.picker.header(&self.search_text))
    }

    fn result_display_limit(&self) -> usize {
        PICKER_DISPLAY_LIMIT - usize::from(self.structured_header().is_some())
    }

    fn visible_update(&self) -> UiUpdate {
        let picker = self
            .active
            .as_ref()
            .map(|active| Arc::clone(&active.picker));
        let mut indices = [0; PICKER_DISPLAY_LIMIT];
        let mut visible_results = Vec::with_capacity(PICKER_DISPLAY_LIMIT);
        for (slot, result) in self
            .results
            .iter()
            .skip(self.viewport_start)
            .take(PICKER_DISPLAY_LIMIT)
            .enumerate()
        {
            indices[slot] = result.node_index;
            visible_results.push(DisplaySearchResult {
                result: result.clone(),
                positions: Vec::new(),
            });
        }
        let mut rows: [Option<String>; PICKER_DISPLAY_LIMIT] = std::array::from_fn(|_| None);
        let render = picker.as_ref().map_or_else(
            || PickerRender {
                header: None,
                row_count: visible_results.len(),
            },
            |picker| {
                picker.render(
                    &self.search_text,
                    &indices[..visible_results.len()],
                    &mut rows,
                )
            },
        );
        let header = render.header;
        let display_limit = PICKER_DISPLAY_LIMIT - usize::from(header.is_some());
        visible_results.truncate(render.row_count);
        for (result, display) in visible_results.iter_mut().zip(rows) {
            if let Some(display) = display {
                result.result.path = display;
            }
            result.positions = resolve_match_positions(&self.result_query, &result.result.path);
        }
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

    fn test_view_model() -> Arc<ViewModel> {
        let (sender, _receiver) = unbounded();
        ViewModel::new(Arc::new(RecordingUiEventSink(sender)))
    }

    fn test_view_model_with_events(preview_visible: bool) -> (Arc<ViewModel>, Receiver<UiEvent>) {
        let (sender, receiver) = unbounded();
        (
            ViewModel::new_with_preview_visibility(
                preview_visible,
                Arc::new(RecordingUiEventSink(sender)),
            ),
            receiver,
        )
    }

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
        let view_model = test_view_model();
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
        let view_model = test_view_model();
        view_model.insert_text("one two");
        view_model.handle_command(InputCommand::Backspace, modifiers(true, false));
        assert_eq!(view_model.current_search_text().text, "one ");
        view_model.handle_command(InputCommand::MoveHome, modifiers(false, false));
        view_model.handle_command(InputCommand::Delete, modifiers(true, false));
        assert_eq!(view_model.current_search_text().text, " ");
    }

    #[test]
    fn preview_paging_overlaps_one_row_and_clamps_to_document() {
        let (view_model, events) = test_view_model_with_events(true);
        view_model.set_preview_visible_rows(20);
        while events.try_recv().is_ok() {}
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
            events.try_recv(),
            Ok(UiEvent::Preview(PreviewView::Text { top_line: 19, .. }))
        ));

        view_model.handle_command(InputCommand::PreviewPageDown, KeyModifiers::default());
        assert!(matches!(
            events.try_recv(),
            Ok(UiEvent::Preview(PreviewView::Text { top_line: 30, .. }))
        ));

        view_model.handle_command(InputCommand::PreviewPageUp, KeyModifiers::default());
        assert!(matches!(
            events.try_recv(),
            Ok(UiEvent::Preview(PreviewView::Text { top_line: 11, .. }))
        ));
    }

    #[test]
    fn preview_visibility_toggle_publishes_the_new_state() {
        let (view_model, events) = test_view_model_with_events(false);

        view_model.handle_command(InputCommand::TogglePreview, KeyModifiers::default());
        assert!(matches!(
            events.try_recv(),
            Ok(UiEvent::PreviewVisibilityChanged { visible: true })
        ));

        view_model.handle_command(InputCommand::TogglePreview, KeyModifiers::default());
        assert!(matches!(
            events.try_recv(),
            Ok(UiEvent::PreviewVisibilityChanged { visible: false })
        ));
    }

    #[test]
    fn completed_preview_centers_the_requested_line() {
        let view_model = test_view_model();
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
        use crate::picker_snapshot::WindowPickerItem;
        use crate::request::WindowListPickerRequest;
        use std::time::Duration;

        let (view_model, events) = test_view_model_with_events(true);
        let runner = Arc::clone(&view_model);
        let request_thread = thread::spawn(move || {
            let request = WindowListPickerRequest {
                items: vec![WindowListItem {
                    text: "00001234      100 app.exe Window title".into(),
                    hwnd: 0x1234,
                }],
            };
            runner.run_request_with_interactions(
                &request,
                PickerInteractions {
                    preview_factory: Arc::new(PreviewFactory::new(
                        crate::preview::PreviewConfig::NativeWindow,
                    )),
                    preview_routes: PreviewRoutes {
                        native_window: Some(Arc::new(|item: &WindowPickerItem| {
                            Some(NativeWindowId(item.native_window))
                        })),
                        ..PreviewRoutes::default()
                    },
                    ..PickerInteractions::default()
                },
            )
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
            PickerResponse::Selected(WindowPickerItem {
                title: "00001234      100 app.exe Window title".into(),
                native_window: 4660,
            })
        );
    }
}
