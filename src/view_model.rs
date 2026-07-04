use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::fuzzy_search_session::{FuzzySearchSession, SearchCounters, SearchUpdate};
use crate::ipc::{PickerRequest, PickerResponse};
use anyhow::{Result, bail};
use crossbeam_channel::{Receiver, Sender, unbounded};
use nfm_search_core::search::{DISPLAY_LIMIT, SearchResult};
use nfm_search_core::timing;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_BACK, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_RETURN, VK_RIGHT, VK_UP,
};

#[derive(Clone, Debug)]
pub enum UiEvent {
    Show,
    Results(UiUpdate),
    Close,
}

pub type UiCounters = SearchCounters;

#[derive(Clone, Debug, Default)]
pub struct UiUpdate {
    pub results: Vec<SearchResult>,
    pub counters: UiCounters,
    pub selected_row: usize,
}

pub struct ViewModel {
    state: Mutex<State>,
    request_generation: AtomicU64,
    search_update_tx: Sender<SearchUpdate>,
    search_update_rx: Receiver<SearchUpdate>,
    events_tx: Sender<UiEvent>,
    events_rx: Receiver<UiEvent>,
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

struct ActiveRequest {
    id: u64,
    response_tx: Sender<PickerResponse>,
    search_session: FuzzySearchSession,
}

impl ViewModel {
    pub fn new() -> Arc<Self> {
        let (events_tx, events_rx) = unbounded();
        let (search_update_tx, search_update_rx) = unbounded();

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
            }),
            request_generation: AtomicU64::new(0),
            search_update_tx,
            search_update_rx,
            events_tx,
            events_rx,
        });

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
            search_session.set_query(search_text);
        }
    }

    pub fn handle_char(self: &Arc<Self>, ch: usize) {
        match ch as u32 {
            0x7f => {}
            13 | 27 => {}
            ch if ch >= 0x20 => {
                let Some(ch) = char::from_u32(ch) else {
                    return;
                };

                self.update_search_text(|state| {
                    let cursor = state.cursor_position;

                    if cursor > state.search_text.len()
                        || !state.search_text.is_char_boundary(cursor)
                    {
                        return false;
                    }

                    state.search_text.insert(cursor, ch);
                    state.cursor_position = cursor + ch.len_utf8();

                    true
                });
            }

            _ => {}
        }
    }

    pub fn handle_key(self: &Arc<Self>, key: usize, modifiers: KeyModifiers) {
        match key as u16 {
            key if key == VK_RETURN.0 => self.select_current(),
            key if key == VK_ESCAPE.0 => self.cancel(),
            key if key == VK_UP.0 => self.move_selection(-1),
            key if key == VK_DOWN.0 => self.move_selection(1),
            key if key == VK_LEFT.0 && modifiers.ctrl => {
                self.move_cursor_previous_word(modifiers.shift)
            }
            key if key == VK_LEFT.0 => self.move_cursor_left(modifiers.shift),
            key if key == VK_RIGHT.0 && modifiers.ctrl => {
                self.move_cursor_next_word(modifiers.shift)
            }
            key if key == VK_RIGHT.0 => self.move_cursor_right(modifiers.shift),
            key if key == VK_END.0 => self.move_cursor_end(modifiers.shift),
            key if key == VK_HOME.0 => self.move_cursor_start(modifiers.shift),
            key if key == VK_BACK.0 => self.handle_backspace(modifiers),
            key if key == VK_DELETE.0 => self.handle_delete(modifiers),
            _ => {}
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
        let mut state = self.state.lock().expect("view model poisoned");
        if state.results.is_empty() {
            state.selected = 0;
            state.viewport_start = 0;
            return;
        }

        let next = state.selected as isize + delta;
        state.selected = next.clamp(0, state.results.len() as isize - 1) as usize;
        state.ensure_selection_visible();
        let update = state.visible_update();
        let _ = self.events_tx.send(UiEvent::Results(update));
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

    fn apply_search_update(&self, search_update: SearchUpdate) {
        let ui_update = {
            let mut state = self.state.lock().expect("view model poisoned");

            let Some(active) = state.active.as_ref() else {
                return;
            };

            if search_update.request_id != active.id {
                return;
            }

            state.results = search_update.results;
            state.counters = search_update.counters;
            state.selected = state.selected.min(state.results.len().saturating_sub(1));
            state.ensure_selection_visible();

            let ui_update = state.visible_update();
            ui_update
        };
        let _ = self.events_tx.send(UiEvent::Results(ui_update));
    }
}

impl State {
    fn ensure_selection_visible(&mut self) {
        if self.results.is_empty() {
            self.selected = 0;
            self.viewport_start = 0;
            return;
        }

        let max_viewport_start = self.results.len().saturating_sub(DISPLAY_LIMIT);
        self.viewport_start = self.viewport_start.min(max_viewport_start);

        if self.selected < self.viewport_start {
            self.viewport_start = self.selected;
        } else if self.selected >= self.viewport_start + DISPLAY_LIMIT {
            self.viewport_start = self.selected + 1 - DISPLAY_LIMIT;
        }
    }

    fn visible_update(&self) -> UiUpdate {
        let visible_results: Vec<_> = self
            .results
            .iter()
            .skip(self.viewport_start)
            .take(DISPLAY_LIMIT)
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
                displayed: self.results.len().min(DISPLAY_LIMIT),
                ..self.counters.clone()
            },
            selected_row,
        }
    }
}
