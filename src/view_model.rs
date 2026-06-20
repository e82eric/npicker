use std::ops::Range;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::ipc::{PickerRequest, PickerResponse};
use crate::search::{
    DISPLAY_LIMIT, RESULT_LIMIT, SearchOutput, SearchResult, search, search_range,
};
use crate::store::ItemsSource;
use crate::timing;
use crate::walker::SharedStore;
use anyhow::{Result, bail};
use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_BACK, VK_DELETE, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_RETURN, VK_RIGHT, VK_UP,
};

#[derive(Clone, Debug)]
pub enum UiEvent {
    Show,
    Results(UiUpdate),
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
    search_version: AtomicU64,
    search_signal_tx: Sender<()>,
    search_signal_rx: Receiver<()>,
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
    store: Arc<SharedStore>,
    response_tx: Sender<PickerResponse>,
}

#[derive(Default)]
struct SearchCache {
    query: String,
    searched_len: usize,
    results: Vec<SearchResult>,
    matched: usize,
}

impl ViewModel {
    pub fn new() -> Self {
        let (events_tx, events_rx) = unbounded();
        let (search_signal_tx, search_signal_rx) = bounded(1);
        Self {
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
            search_version: AtomicU64::new(0),
            search_signal_tx,
            search_signal_rx,
            events_tx,
            events_rx,
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
        self.search_version.store(0, Ordering::Release);
        let (response_tx, response_rx) = unbounded();
        let store = request.run();

        {
            let mut state = self.state.lock().expect("view model poisoned");
            if state.active.is_some() {
                bail!("A picker request is already active.");
            }

            state.search_text = request.search_string().unwrap_or_default().to_owned();
            state.results.clear();
            state.counters = UiCounters::default();
            state.selected = 0;
            state.cursor_selection_anchor = None;
            state.cursor_position = 0;
            state.viewport_start = 0;
            state.active = Some(ActiveRequest {
                id: request_id,
                store: Arc::clone(&store),
                response_tx,
            });
        }

        let _ = self.events_tx.send(UiEvent::Show);
        self.signal_search();
        self.spawn_search_loop(request_id);

        let response = response_rx
            .recv()
            .unwrap_or_else(|_| PickerResponse::cancelled());
        timing::write(format!("response status={}", response.status));
        Ok(response)
    }

    pub fn handle_char(self: &Arc<Self>, ch: usize) {
        match ch as u32 {
            0x7f => {}
            13 | 27 => {}
            ch if ch >= 0x20 => {
                if let Some(ch) = char::from_u32(ch) {
                    let changed = {
                        let mut state = self.state.lock().expect("view model poisoned");

                        let cursor = state.cursor_position;

                        if cursor <= state.search_text.len()
                            && state.search_text.is_char_boundary(cursor)
                        {
                            state.search_text.insert(cursor, ch);
                            state.cursor_position = cursor + ch.len_utf8();

                            state.selected = 0;
                            state.viewport_start = 0;

                            true
                        } else {
                            false
                        }
                    };

                    if changed {
                        self.search_version.fetch_add(1, Ordering::AcqRel);
                        self.signal_search();
                    }
                }
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
        let changed = {
            let mut state = self.state.lock().expect("view model poisoned");

            if modifiers.ctrl {
                Self::move_cursor_previous_word_in_state(&mut state, true);
            }

            let cursor = state.cursor_position;

            if (cursor == 0 && state.cursor_selection_anchor.is_none())
                || cursor > state.search_text.len()
            {
                false
            } else if !state.search_text.is_char_boundary(cursor) {
                false
            } else {
                if let Some(selection) = state.cursor_selection_anchor {
                    let start = selection.min(cursor);
                    let end = selection.max(cursor);
                    state.search_text.replace_range(start..end, "");
                    state.cursor_selection_anchor = None;
                    state.cursor_position = start;
                    true
                } else {
                    let prev_cursor = state.search_text[..cursor]
                        .char_indices()
                        .last()
                        .map(|(i, _)| i);

                    if let Some(prev_cursor) = prev_cursor {
                        state.search_text.drain(prev_cursor..cursor);
                        state.cursor_position = prev_cursor;

                        state.selected = 0;
                        state.viewport_start = 0;

                        true
                    } else {
                        false
                    }
                }
            }
        };

        if changed {
            self.search_version.fetch_add(1, Ordering::AcqRel);
            self.signal_search();
        }
    }

    fn handle_delete(self: &Arc<Self>, modifiers: KeyModifiers) {
        let changed = {
            let mut state = self.state.lock().expect("view model poisoned");

            if modifiers.ctrl {
                Self::move_cursor_next_word_in_state(&mut state, true);
            }

            let cursor = state.cursor_position;

            if (cursor == state.search_text.len() && state.cursor_selection_anchor.is_none())
                || cursor > state.search_text.len()
            {
                false
            } else if !state.search_text.is_char_boundary(cursor) {
                false
            } else {
                if let Some(selection) = state.cursor_selection_anchor {
                    let start = selection.min(cursor);
                    let end = selection.max(cursor);
                    state.search_text.replace_range(start..end, "");
                    state.cursor_selection_anchor = None;
                    state.cursor_position = start;
                    true
                } else {
                    if let Some(ch) = state.search_text[cursor..].chars().next() {
                        let next_cursor = cursor + ch.len_utf8();
                        state.search_text.replace_range(cursor..next_cursor, "");
                        state.selected = 0;
                        state.viewport_start = 0;
                        true
                    } else {
                        false
                    }
                }
            }
        };

        if changed {
            self.search_version.fetch_add(1, Ordering::AcqRel);
            self.signal_search();
        }
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
        {
            let mut state = self.state.lock().expect("view model poisoned");
            if let Some(active) = state.active.take() {
                let response = state
                    .results
                    .get(state.selected)
                    .map(|result| PickerResponse::selected(result.path.clone()))
                    .unwrap_or_else(PickerResponse::cancelled);
                let _ = active.response_tx.send(response);
            }
        }

        self.request_generation.fetch_add(1, Ordering::AcqRel);
        self.signal_search();
        let _ = self.events_tx.send(UiEvent::Close);
    }

    pub fn cancel(&self) {
        let mut state = self.state.lock().expect("view model poisoned");
        if let Some(active) = state.active.take() {
            let _ = active.response_tx.send(PickerResponse::cancelled());
        }
        self.request_generation.fetch_add(1, Ordering::AcqRel);
        self.signal_search();
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

    fn signal_search(&self) {
        let _ = self.search_signal_tx.try_send(());
    }

    fn spawn_search_loop(self: &Arc<Self>, request_id: u64) {
        let this = Arc::clone(self);

        std::thread::spawn(move || {
            let mut last_completed_version = None;
            let mut last_snapshot_version = None;
            let mut cache = SearchCache::default();
            loop {
                if this.request_generation.load(Ordering::Acquire) != request_id {
                    return;
                }

                let (version, snapshot_version, scanning) = {
                    let state = this.state.lock().expect("view model poisoned");
                    let Some(active) = state.active.as_ref() else {
                        return;
                    };
                    if active.id != request_id {
                        return;
                    }
                    (
                        this.search_version.load(Ordering::Acquire),
                        active.store.snapshot_version(),
                        !active.store.is_done(),
                    )
                };

                if last_completed_version != Some(version)
                    || last_snapshot_version != Some(snapshot_version)
                {
                    this.run_search_generation(request_id, version, &mut cache);
                    last_completed_version = Some(version);
                    last_snapshot_version = Some(snapshot_version);
                }

                if scanning {
                    let _ = this
                        .search_signal_rx
                        .recv_timeout(Duration::from_millis(50));
                } else if this
                    .search_signal_rx
                    .recv_timeout(Duration::from_millis(250))
                    .is_err()
                {
                    // Periodically re-check request lifetime even when the source is complete.
                }
            }
        });
    }

    fn run_search_generation(&self, request_id: u64, version: u64, cache: &mut SearchCache) {
        let (store, query) = {
            let state = self.state.lock().expect("view model poisoned");
            let Some(active) = state.active.as_ref() else {
                return;
            };
            if active.id != request_id {
                return;
            }
            (Arc::clone(&active.store), state.search_text.clone())
        };

        let snapshot = store.snapshot();
        let scanning = !store.is_done();
        let total = snapshot.len();
        let use_incremental = cache.query == query && cache.searched_len <= total;
        let output = if use_incremental {
            let Some(delta) = search_range(
                Arc::clone(&snapshot),
                &query,
                cache.searched_len..total,
                || {
                    self.request_generation.load(Ordering::Acquire) != request_id
                        || self.search_version.load(Ordering::Acquire) != version
                },
            ) else {
                return;
            };
            merge_cached_search(cache, &query, delta)
        } else {
            let Some(output) = search(Arc::clone(&snapshot), &query, || {
                self.request_generation.load(Ordering::Acquire) != request_id
                    || self.search_version.load(Ordering::Acquire) != version
            }) else {
                return;
            };
            *cache = SearchCache {
                query: query.clone(),
                searched_len: output.total,
                results: output.results.clone(),
                matched: output.matched,
            };
            output
        };
        if self.request_generation.load(Ordering::Acquire) != request_id
            || self.search_version.load(Ordering::Acquire) != version
        {
            return;
        }

        let counters = UiCounters {
            displayed: output.results.len(),
            matched: output.matched,
            published: output.total,
            scanning,
        };

        {
            let mut state = self.state.lock().expect("view model poisoned");
            let Some(active) = state.active.as_ref() else {
                return;
            };
            if active.id != request_id {
                return;
            }
            state.results = output.results;
            state.counters = counters.clone();
            state.selected = state.selected.min(state.results.len().saturating_sub(1));
            state.ensure_selection_visible();
            let update = state.visible_update();
            let _ = self.events_tx.send(UiEvent::Results(update));
        }
    }
}

fn merge_cached_search(cache: &mut SearchCache, query: &str, delta: SearchOutput) -> SearchOutput {
    cache.query = query.to_owned();
    cache.searched_len = delta.total;
    cache.matched += delta.matched;

    if query.is_empty() {
        if cache.results.len() < RESULT_LIMIT {
            let remaining = RESULT_LIMIT - cache.results.len();
            cache
                .results
                .extend(delta.results.into_iter().take(remaining));
        }
    } else {
        cache.results.extend(delta.results);
        cache.results.sort_by(compare_search_results);
        cache.results.truncate(RESULT_LIMIT);
    }

    SearchOutput {
        results: cache.results.clone(),
        matched: cache.matched,
        total: cache.searched_len,
    }
}

fn compare_search_results(left: &SearchResult, right: &SearchResult) -> std::cmp::Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| left.path.len().cmp(&right.path.len()))
        .then_with(|| left.node_index.cmp(&right.node_index))
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
