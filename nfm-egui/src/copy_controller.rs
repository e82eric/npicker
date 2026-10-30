//! Shared execution of copy-mode input, independent of rendering and clipboard APIs.
//!
//! A controller borrows the host's mode, immutable document and current viewport
//! for an input batch. Hosts can suspend that state for a picker or search UI
//! without duplicating motion/operator execution. Apply returned effects before
//! painting; document failures are errors, never successful empty selections.
use crate::copy_mode::*;
use std::cell::RefCell;
use std::time::{Duration, Instant};

/// The document operations needed to execute copy actions. Unlike a renderable
/// `CopyDocument`, this adapter may borrow a native host for a synchronous batch.
pub trait CopyNavigationDocument {
    fn text_row(&self, row: u64) -> Result<Option<TextRow>, String>;
    fn resolve_cell(&self, cell: CellPosition, right: bool) -> Result<CellPosition, String>;
    fn selection_text(&self, selection: &CopySelection) -> Result<String, String>;
    /// The selection for a text object the host registered with
    /// `CopyMode::set_host_objects`, or `None` when there is nothing to select.
    fn host_object(
        &self,
        _key: char,
        _object: WordObject,
        _count: u32,
        _cursor: CellPosition,
    ) -> Result<Option<CopySelection>, String> {
        Ok(None)
    }
}

impl<T: crate::copy_document::CopyDocument + ?Sized> CopyNavigationDocument for T {
    fn text_row(&self, row: u64) -> Result<Option<TextRow>, String> {
        crate::copy_document::CopyDocument::text_row(self, row).map(|row| row.map(|row| row.text))
    }
    fn resolve_cell(&self, cell: CellPosition, right: bool) -> Result<CellPosition, String> {
        crate::copy_document::CopyDocument::resolve_cell(self, cell, right)
    }
    fn selection_text(&self, selection: &CopySelection) -> Result<String, String> {
        crate::copy_document::CopyDocument::selection_text(self, selection)
    }
}

/// Effects that require host integration. Movement, selections, operators and
/// search navigation have already updated `CopyMode` when these are returned.
pub enum CopyEffect {
    BeginSearch {
        forward: bool,
    },
    CopyText {
        text: String,
        selection: CopySelection,
    },
    Search {
        pattern: String,
        forward: bool,
        count: u32,
        regex: bool,
    },
    ClearSearch {
        cancel_pending: bool,
    },
    QuickSelect {
        origin: CellPosition,
    },
    OpenLineSplit {
        cursor: CellPosition,
    },
    Exit,
}

#[derive(Default)]
pub struct CopyUpdate {
    pub changed: bool,
    /// An explicit physical row offset. Hosts should publish this together with
    /// decorations so movement cannot briefly expose an undecorated viewport.
    pub viewport_top: Option<u64>,
    /// Cursor-driven reveal. Reflowing hosts translate the full document cell
    /// instead of using viewport_top, which assumes the document's row layout.
    pub reveal: Option<CellPosition>,
    pub effects: Vec<CopyEffect>,
    pub repaint_after: Option<Duration>,
}

pub struct CopyController<'a, D: CopyNavigationDocument + ?Sized> {
    mode: &'a mut CopyMode,
    document: &'a D,
    view: View,
    error: RefCell<Option<String>>,
}

impl<'a, D: CopyNavigationDocument + ?Sized> CopyController<'a, D> {
    /// `view` describes the currently displayed physical viewport, while row
    /// wrap metadata in `document` supplies logical-line semantics.
    pub fn new(mode: &'a mut CopyMode, document: &'a D, view: View) -> Self {
        Self {
            mode,
            document,
            view,
            error: RefCell::new(None),
        }
    }

    pub fn begin_input_batch(&mut self) {
        self.mode.begin_input_batch();
    }

    /// Decode and execute one event. Call `begin_input_batch` once for the batch.
    pub fn input(&mut self, event: &egui::Event) -> Result<CopyUpdate, String> {
        let action = self.mode.input(event);
        self.execute(action)
    }

    /// Execute an already decoded action (also supports host binding overrides).
    pub fn execute(&mut self, action: CopyAction) -> Result<CopyUpdate, String> {
        if self.mode.generation.is_none()
            || self.view.total_rows != self.mode.total_rows()
            || self.view.viewport_rows == 0
            || self.view.columns == 0
            || self
                .mode
                .positioned_viewport_top(ViewportPosition::Center, self.view)
                .is_none()
        {
            return Err("Copy document is unavailable or its viewport changed".into());
        }
        let mut update = CopyUpdate::default();
        match action {
            CopyAction::BeginSearch { forward } => {
                update.effects.push(CopyEffect::BeginSearch { forward })
            }
            CopyAction::Exit => update.effects.push(CopyEffect::Exit),
            CopyAction::Motion {
                motion,
                count,
                explicit_count,
            } => match self.motion(motion, count, explicit_count) {
                MotionEvaluation::Complete(result) | MotionEvaluation::Partial(result) => {
                    self.move_to(motion, result.target)?;
                    self.reveal(&mut update);
                }
                MotionEvaluation::InvalidSnapshot => {
                    return Err("Unable to resolve copy motion".into());
                }
                MotionEvaluation::NoMovement => {}
            },
            CopyAction::Jump {
                jump,
                repeat,
                count,
            } => {
                if let Some(target) = self.jump(&jump, repeat, count) {
                    self.move_to(Motion::CharacterJump, target)?;
                    self.reveal(&mut update);
                }
            }
            CopyAction::Position(position) => {
                let top = self
                    .mode
                    .positioned_viewport_top(position, self.view)
                    .ok_or("Unable to position copy viewport")?;
                update.viewport_top = Some(top);
                update.changed = true;
            }
            CopyAction::Scroll { down, count } => {
                let (top, requested) = self
                    .mode
                    .scrolled_viewport_target(down, count, self.view)
                    .ok_or("Unable to scroll copy viewport")?;
                let target = self.document.resolve_cell(requested, false)?;
                self.mode
                    .moved(if down { Motion::Down } else { Motion::Up }, target);
                update.viewport_top = Some(top);
                update.changed = true;
            }
            CopyAction::SelectObject {
                object,
                kind,
                count,
            } => {
                if let Some(mut selection) = self.object(object, kind, count)? {
                    selection.range.start =
                        self.document.resolve_cell(selection.range.start, false)?;
                    selection.range.end = self.document.resolve_cell(selection.range.end, false)?;
                    self.mode
                        .select_visual_object(selection.range, selection.trim);
                    self.reveal(&mut update);
                }
            }
            CopyAction::YankVisual => {
                let selection = self
                    .mode
                    .visual_selection_to(self.mode.cursor, |y| self.row(y));
                check(&self.error)?;
                if let Some(selection) = selection {
                    if let Some(text) = self.mode.reflow_selection_text().map(str::to_owned) {
                        self.yank_text(selection, text, &mut update);
                    } else {
                        self.yank(selection, &mut update)?;
                    }
                    self.mode.visual_anchor = None;
                }
            }
            CopyAction::Yank(command) => {
                if let Some(selection) = self.yank_selection(command)? {
                    self.yank(selection, &mut update)?;
                }
            }
            CopyAction::SearchWord { forward, count } => {
                if let Some(word) = self.mode.word_under_cursor(|y| self.row(y)) {
                    update.effects.push(CopyEffect::Search {
                        pattern: word_search_pattern(&word),
                        forward,
                        count,
                        regex: true,
                    });
                }
            }
            CopyAction::NavigateSearch { reverse, count } => {
                if let Some(target) = self.mode.navigate_search(reverse, count) {
                    self.move_to(Motion::CharacterJump, target)?;
                    self.reveal(&mut update);
                }
            }
            CopyAction::SwapEndpoint => {
                self.move_to(Motion::CharacterJump, self.mode.cursor)?;
                self.reveal(&mut update);
            }
            CopyAction::HintYank { origin } => {
                update.effects.push(CopyEffect::QuickSelect { origin })
            }
            CopyAction::OpenLineSplit => update.effects.push(CopyEffect::OpenLineSplit {
                cursor: self.mode.cursor,
            }),
            CopyAction::ClearSearch { cancel_pending } => {
                update.changed = true;
                update
                    .effects
                    .push(CopyEffect::ClearSearch { cancel_pending });
            }
            CopyAction::Redraw | CopyAction::ToggleVisual => update.changed = true,
            CopyAction::None => {}
        }
        check(&self.error)?;
        if let Some(top) = update.viewport_top {
            self.view.viewport_top = top;
        }
        self.view.cursor = self.mode.cursor;
        Ok(update)
    }

    /// Apply correlated search feedback. An optional anchor keeps incremental
    /// edits searching from the original cursor without exposing that position
    /// to the renderer between results. Stale feedback leaves state untouched.
    pub fn finish_search(
        &mut self,
        revision: u64,
        matches: Vec<CellRange>,
        anchor: Option<CellPosition>,
    ) -> Result<CopyUpdate, String> {
        if !self.mode.search_accepts_result(revision) {
            return Ok(CopyUpdate::default());
        }
        let found = !matches.is_empty();
        let current = self.mode.cursor;
        if let Some(anchor) = anchor {
            self.mode.cursor = anchor;
        }
        let target = self.mode.finish_search(revision, matches);
        self.mode.cursor = current;
        let mut update = CopyUpdate {
            changed: true,
            ..Default::default()
        };
        if let Some(target) = target.filter(|_| found) {
            self.move_to(Motion::CharacterJump, target)?;
            self.reveal(&mut update);
        }
        Ok(update)
    }

    /// Complete a quick-select hint through the same document resolution,
    /// logical-line selection and yank pipeline as ordinary copy actions.
    pub fn select_quick(
        &mut self,
        purpose: crate::quick_select::Purpose,
        cell: CellPosition,
    ) -> Result<CopyUpdate, String> {
        let target = self.document.resolve_cell(cell, false)?;
        let mut update = CopyUpdate::default();
        match purpose {
            crate::quick_select::Purpose::Navigate => {
                self.mode.cancel_pending_search();
                update.effects.push(CopyEffect::ClearSearch {
                    cancel_pending: true,
                });
                self.mode.clear_pending_for_navigation();
                self.move_to(Motion::CharacterJump, target)?;
                self.reveal(&mut update);
            }
            crate::quick_select::Purpose::YankLines { origin } => {
                let range = self
                    .mode
                    .linewise_range_between(origin, target, |y| self.row(y));
                check(&self.error)?;
                if let Some(range) = range {
                    self.yank(
                        CopySelection {
                            range,
                            linewise: true,
                            rectangle: false,
                            trim: true,
                        },
                        &mut update,
                    )?;
                    self.mode.visual_anchor = None;
                }
            }
        }
        Ok(update)
    }

    /// Scroll by a physical-row delta, retaining the configured cursor margin.
    pub fn scroll(&mut self, delta: isize) -> Result<CopyUpdate, String> {
        self.execute(CopyAction::Scroll {
            down: delta >= 0,
            count: delta.unsigned_abs().min(u32::MAX as usize) as u32,
        })
    }

    fn row(&self, y: u64) -> Option<TextRow> {
        record(self.document.text_row(y), &self.error).flatten()
    }
    fn motion(&self, motion: Motion, count: u32, explicit: bool) -> MotionEvaluation {
        self.mode.evaluate_motion(
            motion,
            count,
            explicit,
            Some(self.view),
            |y| self.row(y),
            |cell, right| record(self.document.resolve_cell(cell, right), &self.error),
        )
    }
    fn move_to(&mut self, motion: Motion, target: CellPosition) -> Result<(), String> {
        let target = self.document.resolve_cell(target, false)?;
        let document = self.document;
        let error = &self.error;
        self.mode.moved_with_rows(motion, target, |y| {
            record(document.text_row(y), error).flatten()
        });
        check(error)
    }
    fn reveal(&self, update: &mut CopyUpdate) {
        update.changed = true;
        update.reveal = Some(self.mode.cursor);
        update.viewport_top = Some(revealed_viewport_top(
            self.mode.cursor,
            self.view,
            self.mode.scroll_padding(),
        ));
    }
    fn jump(&self, jump: &Jump, repeat: bool, count: u32) -> Option<CellPosition> {
        let mut working = self.mode.clone();
        for index in 0..count.max(1) {
            let target = working.jump_target(jump, repeat || index > 0, |y| self.row(y))?;
            let target = record(self.document.resolve_cell(target, false), &self.error)?;
            working.moved(Motion::CharacterJump, target);
        }
        Some(working.cursor)
    }
    fn object(
        &self,
        object: WordObject,
        kind: CopyTextObject,
        count: u32,
    ) -> Result<Option<CopySelection>, String> {
        if let CopyTextObject::Host(key) = kind {
            return self
                .document
                .host_object(key, object, count, self.mode.cursor);
        }
        let selection = self
            .mode
            .text_object_selection(count, object, kind, |y| self.row(y));
        check(&self.error)?;
        Ok(selection)
    }
    fn yank_selection(&self, command: YankCommand) -> Result<Option<CopySelection>, String> {
        let range = match command {
            YankCommand::Lines(count) => self
                .mode
                .yank_lines_range(count, |y| self.row(y))
                .map(|range| (range, true)),
            YankCommand::RowEnd(count) => self
                .mode
                .yank_to_row_end_range(count, |y| self.row(y))
                .map(|range| (range, false)),
            YankCommand::Object {
                object,
                kind,
                count,
            } => return self.object(object, kind, count),
            YankCommand::Motion {
                motion,
                count,
                explicit_count,
            } => match self.motion(motion, count, explicit_count) {
                MotionEvaluation::Complete(result) => {
                    self.mode.yank_motion_range(result, |y| self.row(y))
                }
                // Ordinary motions may advance partially; operators must finish.
                MotionEvaluation::Partial(_) | MotionEvaluation::NoMovement => None,
                MotionEvaluation::InvalidSnapshot => {
                    return Err("Unable to resolve yank motion".into());
                }
            },
            YankCommand::Jump { jump, count } => {
                self.jump(&jump, false, count).and_then(|target| {
                    self.mode
                        .yank_motion_range(Motion::CharacterJump.result(target), |y| self.row(y))
                })
            }
        };
        check(&self.error)?;
        Ok(range.map(|(range, linewise)| CopySelection {
            range,
            linewise,
            trim: true,
            rectangle: false,
        }))
    }
    fn yank(&mut self, selection: CopySelection, update: &mut CopyUpdate) -> Result<(), String> {
        let text = self.document.selection_text(&selection)?;
        self.yank_text(selection, text, update);
        Ok(())
    }
    fn yank_text(&mut self, selection: CopySelection, text: String, update: &mut CopyUpdate) {
        if selection.rectangle {
            self.mode
                .highlight_rectangle_yank(selection.range, Instant::now());
        } else {
            self.mode.highlight_yank(selection.range, Instant::now());
        }
        update.changed = true;
        update.repaint_after = Some(YANK_HIGHLIGHT_DURATION);
        update
            .effects
            .push(CopyEffect::CopyText { text, selection });
    }
}

/// Shared cursor reveal policy, including padding and document-edge clamping.
pub fn revealed_viewport_top(cursor: CellPosition, view: View, padding: u32) -> u64 {
    let height = u64::from(view.viewport_rows)
        .max(1)
        .min(view.total_rows.max(1));
    let padding = u64::from(padding).min((height - 1) / 2);
    let mut top = view
        .viewport_top
        .min(view.total_rows.saturating_sub(height));
    if cursor.y < top.saturating_add(padding) {
        top = cursor.y.saturating_sub(padding);
    } else if cursor.y >= top.saturating_add(height - padding) {
        top = cursor.y.saturating_sub(height - 1 - padding);
    }
    top.min(view.total_rows.saturating_sub(height))
}

fn record<T>(result: Result<T, String>, error: &RefCell<Option<String>>) -> Option<T> {
    match result {
        Ok(value) => Some(value),
        Err(message) => {
            error.borrow_mut().get_or_insert(message);
            None
        }
    }
}
fn check(error: &RefCell<Option<String>>) -> Result<(), String> {
    error.borrow().clone().map_or(Ok(()), Err)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Document {
        rows: Vec<TextRow>,
        fail_reads: bool,
        fail_copy: bool,
    }
    impl CopyNavigationDocument for Document {
        fn text_row(&self, y: u64) -> Result<Option<TextRow>, String> {
            if self.fail_reads {
                return Err("snapshot expired".into());
            }
            Ok(self.rows.get(y as usize).cloned())
        }
        fn resolve_cell(&self, cell: CellPosition, _right: bool) -> Result<CellPosition, String> {
            if self.fail_reads {
                return Err("snapshot expired".into());
            }
            Ok(CellPosition {
                x: cell.x.min(7),
                y: cell.y.min(self.rows.len() as u64 - 1),
            })
        }
        fn selection_text(&self, selection: &CopySelection) -> Result<String, String> {
            if self.fail_copy {
                return Err("copy failed".into());
            }
            let mut text = String::new();
            for y in selection.range.start.y..=selection.range.end.y {
                let row = &self.rows[y as usize];
                let left = if y == selection.range.start.y {
                    selection.range.start.x as usize
                } else {
                    0
                };
                let right = if y == selection.range.end.y {
                    selection.range.end.x as usize + 1
                } else {
                    row.text.len()
                };
                text.push_str(&row.text[left.min(row.text.len())..right.min(row.text.len())]);
                if selection.linewise || (y < selection.range.end.y && !row.wraps_to_next) {
                    text.push('\n');
                }
            }
            Ok(text)
        }
        /// `c` selects the cursor's whole row, like a one-row command.
        fn host_object(
            &self,
            key: char,
            _object: WordObject,
            _count: u32,
            cursor: CellPosition,
        ) -> Result<Option<CopySelection>, String> {
            Ok((key == 'c').then_some(CopySelection {
                range: CellRange {
                    start: CellPosition { x: 0, y: cursor.y },
                    end: CellPosition { x: 7, y: cursor.y },
                },
                linewise: true,
                trim: true,
                rectangle: false,
            }))
        }
    }
    fn document(lines: &[(&str, bool)]) -> Document {
        Document {
            rows: lines
                .iter()
                .map(|(text, wraps)| TextRow {
                    text: (*text).into(),
                    columns: (0..text.len() as u32).collect(),
                    wraps_to_next: *wraps,
                })
                .collect(),
            fail_reads: false,
            fail_copy: false,
        }
    }
    fn start(rows: u64, cursor: CellPosition) -> (CopyMode, View) {
        let view = View {
            cursor,
            total_rows: rows,
            columns: 8,
            viewport_rows: 3,
            viewport_top: 0,
        };
        let mut mode = CopyMode::default();
        mode.start(1, view);
        (mode, view)
    }
    fn key(key: egui::Key) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        }
    }

    #[test]
    fn shared_input_moves_logical_lines_and_publishes_padded_viewport() {
        let doc = document(&[
            ("abcdefgh", true),
            ("ijkl", false),
            ("next", false),
            ("end", false),
        ]);
        let (mut mode, view) = start(4, CellPosition { x: 1, y: 0 });
        let mut controller = CopyController::new(&mut mode, &doc, view);
        controller.begin_input_batch();
        let update = controller.input(&key(egui::Key::J)).unwrap();
        assert!(update.changed);
        assert_eq!(update.viewport_top, Some(1));
        assert_eq!(update.reveal, Some(CellPosition { x: 1, y: 2 }));
        assert_eq!(mode.cursor, CellPosition { x: 1, y: 2 });
    }
    #[test]
    fn host_objects_use_registered_keys_and_the_document_selection() {
        let doc = document(&[("PS> ls", false), ("out", false), ("PS> ", false)]);
        let (mut mode, view) = start(3, CellPosition { x: 0, y: 1 });
        assert!(mode.set_host_objects(&['w']).is_err());
        assert!(mode.set_host_objects(&['1']).is_err());
        let press = |mode: &mut CopyMode, keys: [egui::Key; 3]| {
            let mut controller = CopyController::new(mode, &doc, view);
            controller.begin_input_batch();
            keys.iter()
                .flat_map(|&k| controller.input(&key(k)).unwrap().effects)
                .collect::<Vec<_>>()
        };
        let yic = [egui::Key::Y, egui::Key::I, egui::Key::C];
        // Unregistered, `c` is not a text object.
        assert!(press(&mut mode, yic).is_empty());
        mode.set_host_objects(&['c']).unwrap();
        assert!(matches!(
            &press(&mut mode, yic)[..],
            [CopyEffect::CopyText { text, .. }] if text == "out\n"
        ));
        press(&mut mode, [egui::Key::V, egui::Key::I, egui::Key::C]);
        assert_eq!(
            mode.visual_selection_to(mode.cursor, |y| doc.rows.get(y as usize).cloned())
                .map(|selection| selection.range),
            Some(CellRange {
                start: CellPosition { x: 0, y: 1 },
                end: CellPosition { x: 7, y: 1 },
            })
        );
    }
    #[test]
    fn horizontal_navigation_reveals_full_cell_but_positioning_remains_row_only() {
        let doc = document(&[("abcdefgh", false)]);
        let (mut mode, view) = start(1, CellPosition { x: 0, y: 0 });
        let mut controller = CopyController::new(&mut mode, &doc, view);
        let update = controller
            .execute(CopyAction::Motion {
                motion: Motion::LastNonblank,
                count: 1,
                explicit_count: false,
            })
            .unwrap();
        assert_eq!(update.reveal, Some(CellPosition { x: 7, y: 0 }));
        let update = controller
            .execute(CopyAction::Position(ViewportPosition::Center))
            .unwrap();
        assert!(update.viewport_top.is_some());
        assert!(update.reveal.is_none());
    }
    #[test]
    fn visual_yank_copies_before_clearing_selection_and_retains_flash() {
        let doc = document(&[("hello", false)]);
        let (mut mode, view) = start(1, CellPosition { x: 1, y: 0 });
        mode.toggle_visual(VisualMode::Characterwise);
        mode.cursor.x = 3;
        let update = CopyController::new(&mut mode, &doc, view)
            .execute(CopyAction::YankVisual)
            .unwrap();
        let CopyEffect::CopyText { text, selection } = &update.effects[0] else {
            panic!("expected copy");
        };
        assert_eq!(text, "ell");
        assert_eq!(selection.range.start.x, 1);
        assert!(mode.visual_anchor.is_none());
        assert_eq!(mode.yank_highlight_range(), Some(selection.range));
        assert_eq!(update.repaint_after, Some(YANK_HIGHLIGHT_DURATION));
    }

    #[test]
    fn reflowed_visual_selections_keep_exact_text_through_repeated_resize_and_yank() {
        for visual_mode in [
            VisualMode::Characterwise,
            VisualMode::Linewise,
            VisualMode::Blockwise,
        ] {
            let (mut mode, mut view) = start(4, CellPosition { x: 1, y: 0 });
            mode.toggle_visual(visual_mode);
            let original = "界e\u{301}  \nkl  \n";
            for columns in [4, 16, 6] {
                view.columns = columns;
                view.total_rows = 8;
                let ranges = vec![
                    CellRange {
                        start: CellPosition { x: 0, y: 1 },
                        end: CellPosition { x: 2, y: 2 },
                    },
                    CellRange {
                        start: CellPosition { x: 1, y: 4 },
                        end: CellPosition { x: 3, y: 4 },
                    },
                ];
                let text = mode.reflow_selection_text().unwrap_or(original).to_owned();
                mode.apply_reflow(
                    view,
                    ranges[1].end,
                    Some(ranges[0].start),
                    ranges.clone(),
                    Some(text),
                );
                assert_eq!(mode.reflow_selection_text(), Some(original));
                assert_eq!(mode.reflow_selection_ranges(), Some(ranges.as_slice()));
                assert_eq!(mode.total_rows(), 8);
            }
            assert!(mode.swap_visual_endpoint());
            assert_eq!(mode.reflow_selection_text(), Some(original));
            // The old rectangle is no longer expressible by two corners. Copy
            // must use the captured selection text rather than its bounding box.
            let mut doc = document(&[("unrelated", false); 8]);
            doc.fail_copy = true;
            let update = CopyController::new(&mut mode, &doc, view)
                .execute(CopyAction::YankVisual)
                .unwrap();
            let CopyEffect::CopyText { text, .. } = &update.effects[0] else {
                panic!("expected copy");
            };
            assert_eq!(text, original);
            assert!(mode.reflow_selection_text().is_none());
        }
    }

    #[test]
    fn motion_after_reflow_updates_selection_in_the_new_layout() {
        let (mut mode, view) = start(2, CellPosition { x: 1, y: 0 });
        mode.toggle_visual(VisualMode::Blockwise);
        mode.apply_reflow(
            view,
            CellPosition { x: 3, y: 0 },
            Some(CellPosition { x: 1, y: 0 }),
            vec![CellRange {
                start: CellPosition { x: 1, y: 0 },
                end: CellPosition { x: 3, y: 0 },
            }],
            Some("old text".into()),
        );
        mode.moved(Motion::Right, mode.cursor);
        assert_eq!(mode.reflow_selection_text(), Some("old text"));
        mode.moved(Motion::Right, CellPosition { x: 4, y: 0 });
        assert!(mode.reflow_selection_text().is_none());
        assert_eq!(mode.visual_mode, VisualMode::Characterwise);
        let doc = document(&[("abcdefgh", false), ("next", false)]);
        let update = CopyController::new(&mut mode, &doc, view)
            .execute(CopyAction::YankVisual)
            .unwrap();
        let CopyEffect::CopyText { text, .. } = &update.effects[0] else {
            panic!("expected copy");
        };
        assert_eq!(text, "bcde");
    }
    #[test]
    fn failed_reads_and_copy_do_not_become_successful_empty_yanks() {
        let mut doc = document(&[("hello", false)]);
        let (mut mode, view) = start(1, CellPosition { x: 1, y: 0 });
        mode.toggle_visual(VisualMode::Characterwise);
        doc.fail_copy = true;
        assert!(
            CopyController::new(&mut mode, &doc, view)
                .execute(CopyAction::YankVisual)
                .is_err()
        );
        assert!(mode.visual_anchor.is_some());
        assert!(mode.yank_highlight_range().is_none());
        doc.fail_reads = true;
        assert!(
            CopyController::new(&mut mode, &doc, view)
                .execute(CopyAction::Motion {
                    motion: Motion::ForwardWord,
                    count: 1,
                    explicit_count: false,
                })
                .is_err()
        );
    }
    #[test]
    fn counted_operator_rejects_partial_motion_without_moving_or_copying() {
        let doc = document(&[("one two", false)]);
        let (mut mode, view) = start(1, CellPosition { x: 0, y: 0 });
        let mut controller = CopyController::new(&mut mode, &doc, view);
        assert!(matches!(
            controller.motion(Motion::ForwardWord, 10, true),
            MotionEvaluation::Partial(_)
        ));
        let update = controller
            .execute(CopyAction::Yank(YankCommand::Motion {
                motion: Motion::ForwardWord,
                count: 10,
                explicit_count: true,
            }))
            .unwrap();
        assert!(update.effects.is_empty());
        assert!(!update.changed);
        assert_eq!(mode.cursor, view.cursor);
        assert!(mode.yank_highlight_range().is_none());
    }
    #[test]
    fn slash_starts_search_but_remains_a_character_jump_target() {
        let doc = document(&[("a/b", false)]);
        let (mut mode, view) = start(1, CellPosition { x: 0, y: 0 });
        let mut controller = CopyController::new(&mut mode, &doc, view);
        assert!(matches!(
            controller.input(&key(egui::Key::Slash)).unwrap().effects[0],
            CopyEffect::BeginSearch { forward: true }
        ));
        controller.input(&key(egui::Key::F)).unwrap();
        controller.input(&egui::Event::Text("f".into())).unwrap();
        controller.input(&key(egui::Key::Slash)).unwrap();
        let update = controller.input(&egui::Event::Text("/".into())).unwrap();
        assert!(update.effects.is_empty());
        assert_eq!(mode.cursor.x, 1);
    }
    #[test]
    fn changed_document_dimensions_fail_before_reading_or_moving() {
        let doc = document(&[("hello", false)]);
        let (mut mode, mut view) = start(1, CellPosition { x: 1, y: 0 });
        view.columns = 4;
        assert!(
            CopyController::new(&mut mode, &doc, view)
                .execute(CopyAction::Motion {
                    motion: Motion::Right,
                    count: 1,
                    explicit_count: false,
                })
                .is_err()
        );
        assert_eq!(mode.cursor.x, 1);
    }
    #[test]
    fn accepting_a_hint_cancels_search_that_could_move_the_cursor_later() {
        let doc = document(&[("one two", false)]);
        let (mut mode, view) = start(1, CellPosition { x: 0, y: 0 });
        mode.begin_search(2, true, 1);
        let update = CopyController::new(&mut mode, &doc, view)
            .select_quick(
                crate::quick_select::Purpose::Navigate,
                CellPosition { x: 4, y: 0 },
            )
            .unwrap();
        assert!(matches!(
            update.effects[0],
            CopyEffect::ClearSearch {
                cancel_pending: true
            }
        ));
        assert!(!mode.search_accepts_result(2));
        assert_eq!(mode.cursor.x, 4);
    }
    #[test]
    fn quick_line_yank_uses_logical_ranges_and_clears_visual_selection() {
        let doc = document(&[("abcdefgh", true), ("ijkl", false), ("next", false)]);
        let (mut mode, view) = start(3, CellPosition { x: 2, y: 1 });
        mode.toggle_visual(VisualMode::Characterwise);
        let update = CopyController::new(&mut mode, &doc, view)
            .select_quick(
                crate::quick_select::Purpose::YankLines {
                    origin: view.cursor,
                },
                CellPosition { x: 1, y: 2 },
            )
            .unwrap();
        let CopyEffect::CopyText { selection, .. } = &update.effects[0] else {
            panic!("expected copy");
        };
        assert!(selection.linewise);
        assert_eq!(selection.range.start, CellPosition { x: 0, y: 0 });
        assert_eq!(selection.range.end.y, 2);
        assert!(mode.visual_anchor.is_none());
        assert_eq!(mode.cursor, view.cursor);
    }
    #[test]
    fn incremental_search_result_moves_atomically_and_ignores_stale_feedback() {
        let doc = document(&[("foo foo", false)]);
        let (mut mode, view) = start(1, CellPosition { x: 4, y: 0 });
        mode.begin_search(2, true, 1);
        let matches = vec![
            CellRange {
                start: CellPosition { x: 4, y: 0 },
                end: CellPosition { x: 6, y: 0 },
            },
            CellRange {
                start: CellPosition { x: 0, y: 0 },
                end: CellPosition { x: 2, y: 0 },
            },
        ];
        let mut controller = CopyController::new(&mut mode, &doc, view);
        assert!(
            !controller
                .finish_search(1, matches.clone(), Some(CellPosition { x: 0, y: 0 }))
                .unwrap()
                .changed
        );
        let update = controller
            .finish_search(2, matches, Some(CellPosition { x: 0, y: 0 }))
            .unwrap();
        assert!(update.changed);
        assert_eq!(mode.cursor, CellPosition { x: 4, y: 0 });
        mode.begin_search(3, true, 1);
        let update = CopyController::new(&mut mode, &doc, view)
            .finish_search(3, vec![], Some(CellPosition { x: 0, y: 0 }))
            .unwrap();
        assert!(update.viewport_top.is_none());
        assert_eq!(mode.cursor.x, 4);
    }
    #[test]
    fn search_navigation_and_host_actions_return_consistent_effects() {
        let doc = document(&[("one two", false)]);
        let (mut mode, view) = start(1, CellPosition { x: 0, y: 0 });
        mode.adopt_search(
            1,
            vec![CellRange {
                start: CellPosition { x: 4, y: 0 },
                end: CellPosition { x: 6, y: 0 },
            }],
        );
        let mut controller = CopyController::new(&mut mode, &doc, view);
        let update = controller
            .execute(CopyAction::NavigateSearch {
                reverse: false,
                count: 1,
            })
            .unwrap();
        assert_eq!(update.viewport_top, Some(0));
        let update = controller.execute(CopyAction::OpenLineSplit).unwrap();
        assert!(matches!(
            update.effects[0],
            CopyEffect::OpenLineSplit {
                cursor: CellPosition { x: 4, y: 0 }
            }
        ));
        assert!(matches!(
            controller.execute(CopyAction::Exit).unwrap().effects[0],
            CopyEffect::Exit
        ));
    }
}
