#![doc = include_str!("../docs/copy-mode/README.md")]
//! Copy-mode state machine copied from Ghostty's egui terminal shell.
//! Hosts provide immutable physical rows, resolve cell positions, and apply actions.
//! Original source is covered by the Ghostty license in LICENSE-ghostty-copy-mode.

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellPosition {
    pub x: u32,
    pub y: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct View {
    pub cursor: CellPosition,
    pub viewport_top: u64,
    pub total_rows: u64,
    pub viewport_rows: u32,
    pub columns: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellRange {
    pub start: CellPosition,
    pub end: CellPosition,
}

use regex::RegexSet;
use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

pub const YANK_HIGHLIGHT_DURATION: Duration = Duration::from_millis(250);
pub const SCROLL_PADDING_ROWS: u32 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Motion {
    Left,
    Down,
    Up,
    Right,
    RowStart,
    ScrollbackTop,
    ScrollbackBottom,
    ViewportTop,
    ViewportMiddle,
    ViewportBottom,
    PageUp,
    PageDown,
    HalfPageUp,
    HalfPageDown,
    FirstNonblank,
    LastNonblank,
    ForwardWord,
    BackwardWord,
    ForwardWordEnd,
    ForwardBigWord,
    BackwardBigWord,
    ForwardBigWordEnd,
    PreviousWordEnd,
    PreviousBigWordEnd,
    NextLineNonblank,
    PreviousLineNonblank,
    LineNonblank,
    CharacterJump,
    MatchingBracket,
}

impl Motion {
    pub fn result(self, target: CellPosition) -> MotionResult {
        let kind = if matches!(
            self,
            Self::Up
                | Self::Down
                | Self::ScrollbackTop
                | Self::ScrollbackBottom
                | Self::ViewportTop
                | Self::ViewportMiddle
                | Self::ViewportBottom
                | Self::PageUp
                | Self::PageDown
                | Self::HalfPageUp
                | Self::HalfPageDown
                | Self::NextLineNonblank
                | Self::PreviousLineNonblank
                | Self::LineNonblank
        ) {
            RangeKind::Linewise
        } else {
            RangeKind::Characterwise
        };
        let endpoint = if matches!(
            self,
            Self::ForwardWord | Self::ForwardBigWord | Self::BackwardWord | Self::BackwardBigWord
        ) {
            Endpoint::Exclusive
        } else {
            Endpoint::Inclusive
        };
        MotionResult {
            target,
            kind,
            endpoint,
        }
    }
    pub fn needs_view(self) -> bool {
        matches!(
            self,
            Self::ViewportTop
                | Self::ViewportMiddle
                | Self::ViewportBottom
                | Self::PageUp
                | Self::PageDown
                | Self::HalfPageUp
                | Self::HalfPageDown
        )
    }

    pub fn needs_text(self) -> bool {
        matches!(
            self,
            Self::Left
                | Self::Right
                | Self::RowStart
                | Self::FirstNonblank
                | Self::LastNonblank
                | Self::ForwardWord
                | Self::BackwardWord
                | Self::ForwardWordEnd
                | Self::ForwardBigWord
                | Self::BackwardBigWord
                | Self::ForwardBigWordEnd
                | Self::PreviousWordEnd
                | Self::PreviousBigWordEnd
                | Self::NextLineNonblank
                | Self::PreviousLineNonblank
                | Self::LineNonblank
                | Self::CharacterJump
                | Self::MatchingBracket
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RangeKind {
    Characterwise,
    Linewise,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Endpoint {
    Inclusive,
    Exclusive,
}

#[derive(Clone, Copy, Debug)]
pub struct MotionResult {
    pub target: CellPosition,
    pub kind: RangeKind,
    pub endpoint: Endpoint,
}

#[derive(Clone, Copy, Debug)]
pub enum MotionEvaluation {
    NoMovement,
    Complete(MotionResult),
    Partial(MotionResult),
    InvalidSnapshot,
}

#[derive(Clone, Debug)]
pub struct TextRow {
    pub text: String,
    pub columns: Vec<u32>,
    pub wraps_to_next: bool,
}

fn prompt_patterns() -> &'static RegexSet {
    static PATTERNS: OnceLock<RegexSet> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        // Copied from the reference WezTerm config.prompt_patterns.
        // Keep these here until the sample has Lua configuration.
        RegexSet::new([
            r"^PS [^>]*>\s*",
            r"^\d+:\d+>\s*",
            r"^›\s*",
            r"^────────────────────*",
        ])
        .expect("valid hardcoded prompt patterns")
    })
}

/// Prompt regexes operate on logical lines, so a soft-wrapped prompt is
/// matched once at its first physical row. The snapshot never changes.
pub fn find_prompt_boundaries(
    rows: u64,
    mut read: impl FnMut(u64) -> Option<TextRow>,
) -> Option<Vec<u64>> {
    let mut boundaries = Vec::new();
    let mut line = String::new();
    let mut line_start = 0;
    for y in 0..rows {
        let row = read(y)?;
        if y == 0 {
            line_start = y;
        }
        line.push_str(&row.text);
        if !row.wraps_to_next || y + 1 == rows {
            if prompt_patterns().is_match(&line) {
                boundaries.push(line_start);
            }
            line.clear();
            line_start = y + 1;
        }
    }
    Some(boundaries)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WordClass {
    Whitespace,
    Keyword,
    Punctuation,
}

#[derive(Clone, Copy, Debug)]
struct Glyph {
    x: u32,
    class: WordClass,
    nonblank: bool,
    byte_start: usize,
    byte_end: usize,
}

#[derive(Clone, Copy)]
enum Direction {
    Forward,
    Backward,
}

#[derive(Clone, Copy)]
struct ScannedGlyph {
    cell: CellPosition,
    class: WordClass,
    first: char,
    single: bool,
}

enum TextStep {
    Glyph(ScannedGlyph),
    LineBreak,
    EmptyRow,
}

/// Row-at-a-time traversal shared by motions and delimiter scans. Soft wraps
/// preserve word continuity; hard breaks are explicit events. Only the current
/// row's graphemes are retained, even when scanning a large scrollback.
struct TextScanner<'a, F> {
    read: &'a mut F,
    rows: u64,
    y: u64,
    direction: Direction,
    big: bool,
    glyphs: std::vec::IntoIter<ScannedGlyph>,
    wraps: bool,
    loaded: bool,
    finished: bool,
}

impl<'a, F: FnMut(u64) -> Option<TextRow>> TextScanner<'a, F> {
    fn new(read: &'a mut F, rows: u64, y: u64, direction: Direction, big: bool) -> Self {
        Self {
            read,
            rows,
            y,
            direction,
            big,
            glyphs: Vec::new().into_iter(),
            wraps: false,
            loaded: false,
            finished: false,
        }
    }

    fn load(&mut self) -> Option<()> {
        let row = (self.read)(self.y)?;
        self.wraps = row.wraps_to_next;
        let mut glyphs: Vec<_> = row
            .glyphs(self.big)
            .into_iter()
            .map(|glyph| {
                let mut chars = row.text[glyph.byte_start..glyph.byte_end].chars();
                ScannedGlyph {
                    cell: CellPosition {
                        x: glyph.x,
                        y: self.y,
                    },
                    class: glyph.class,
                    first: chars.next().unwrap(),
                    single: chars.next().is_none(),
                }
            })
            .collect();
        if matches!(self.direction, Direction::Backward) {
            glyphs.reverse();
        }
        self.glyphs = glyphs.into_iter();
        self.loaded = true;
        Some(())
    }
}

impl<F: FnMut(u64) -> Option<TextRow>> Iterator for TextScanner<'_, F> {
    type Item = TextStep;

    fn next(&mut self) -> Option<TextStep> {
        if self.finished {
            return None;
        }
        loop {
            if !self.loaded && (self.y >= self.rows || self.load().is_none()) {
                self.finished = true;
                return None;
            }
            if let Some(glyph) = self.glyphs.next() {
                return Some(TextStep::Glyph(glyph));
            }
            let previous_wraps = self.wraps;
            let next = match self.direction {
                Direction::Forward => self.y.checked_add(1).filter(|&y| y < self.rows),
                Direction::Backward => self.y.checked_sub(1),
            };
            let Some(y) = next else {
                self.finished = true;
                return None;
            };
            self.y = y;
            if self.load().is_none() {
                self.finished = true;
                return None;
            }
            let hard_break = match self.direction {
                Direction::Forward => !previous_wraps,
                Direction::Backward => !self.wraps,
            };
            if hard_break {
                return Some(TextStep::LineBreak);
            }
            if self.glyphs.len() == 0 {
                return Some(TextStep::EmptyRow);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WordObject {
    Inner,
    Around,
}

#[derive(Clone, Copy)]
pub struct YankPending {
    pub count: u32,
    pub object: Option<WordObject>,
    pub jump: Option<(bool, bool)>,
}

#[derive(Clone, Copy)]
struct LocatedGlyph {
    cell: CellPosition,
    class: WordClass,
    byte_start: usize,
    byte_end: usize,
}

/// Physical row coordinates remain the host/rendering API. This view joins only
/// explicit soft wraps; a preview's hard-broken rows are independent lines.
struct LogicalLine {
    start: u64,
    end: u64,
}
impl LogicalLine {
    fn column(&self, cell: CellPosition, width: u32) -> u64 {
        cell.y
            .saturating_sub(self.start)
            .saturating_mul(u64::from(width))
            + u64::from(cell.x)
    }
}

struct LogicalWordLine {
    text: String,
    glyphs: Vec<LocatedGlyph>,
}

struct WordSpan {
    start: usize,
    end: usize,
}

fn word_span_at(
    glyphs: &[LocatedGlyph],
    cursor: CellPosition,
    keywords_only: bool,
) -> Option<WordSpan> {
    let cursor_at = (cursor.y, cursor.x);
    let index = if keywords_only {
        // Search chooses a keyword on the cursor's physical row, then expands
        // through soft wraps. It must not choose a word on another row.
        glyphs
            .iter()
            .position(|glyph| {
                glyph.cell.y == cursor.y
                    && glyph.cell.x >= cursor.x
                    && glyph.class == WordClass::Keyword
            })
            .or_else(|| {
                glyphs.iter().rposition(|glyph| {
                    glyph.cell.y == cursor.y
                        && glyph.cell.x <= cursor.x
                        && glyph.class == WordClass::Keyword
                })
            })?
    } else {
        let cursor_index = glyphs
            .iter()
            .position(|glyph| (glyph.cell.y, glyph.cell.x) >= cursor_at)
            .or_else(|| glyphs.len().checked_sub(1))?;
        if glyphs[cursor_index].class == WordClass::Whitespace {
            glyphs[cursor_index..]
                .iter()
                .position(|glyph| glyph.class != WordClass::Whitespace)
                .map(|offset| cursor_index + offset)
                .or_else(|| {
                    glyphs[..cursor_index]
                        .iter()
                        .rposition(|glyph| glyph.class != WordClass::Whitespace)
                })?
        } else {
            cursor_index
        }
    };
    let mut start = index;
    while start > 0 && glyphs[start - 1].class == glyphs[index].class {
        start -= 1;
    }
    let mut end = index + 1;
    while end < glyphs.len() && glyphs[end].class == glyphs[index].class {
        end += 1;
    }
    Some(WordSpan { start, end })
}

#[derive(Clone, Copy)]
struct YankHighlight {
    range: CellRange,
    rectangle: bool,
    expires_at: Instant,
}

impl TextRow {
    fn grapheme_cells(&self) -> Vec<(u32, &str)> {
        self.text
            .grapheme_indices(true)
            .filter_map(|(byte, grapheme)| Some((*self.columns.get(byte)?, grapheme)))
            .collect()
    }

    fn glyphs(&self, big_word: bool) -> Vec<Glyph> {
        self.text
            .grapheme_indices(true)
            .filter_map(|(byte, grapheme)| {
                let x = *self.columns.get(byte)?;
                let class = if grapheme.chars().all(char::is_whitespace) {
                    WordClass::Whitespace
                } else if big_word
                    || grapheme
                        .chars()
                        .next()
                        .is_some_and(|ch| ch == '_' || ch.is_alphanumeric())
                {
                    WordClass::Keyword
                } else {
                    WordClass::Punctuation
                };
                Some(Glyph {
                    x,
                    class,
                    nonblank: grapheme != " ",
                    byte_start: byte,
                    byte_end: byte + grapheme.len(),
                })
            })
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Jump {
    pub target: String,
    pub forward: bool,
    pub till: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Default)]
pub enum VisualMode {
    #[default]
    Characterwise,
    Linewise,
    Blockwise,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ViewportPosition {
    Top,
    Center,
    Bottom,
}

pub struct CopySelection {
    pub range: CellRange,
    pub linewise: bool,
    pub trim: bool,
    pub rectangle: bool,
}

#[derive(Clone, Default)]
pub struct CopyMode {
    scroll_padding: Option<u32>,
    pub generation: Option<u64>,
    pub cursor: CellPosition,
    pub visual_anchor: Option<CellPosition>,
    pub visual_mode: VisualMode,
    pub visual_trim: bool,
    pub pending_visual_object: Option<(WordObject, u32)>,
    preferred_x: u32,
    preferred_logical_x: Option<u64>,
    columns: u32,
    rows: u64,
    pub pending_g: bool,
    pub pending_z: bool,
    pub pending_count: Option<u32>,
    pub pending_jump: Option<(bool, bool)>,
    pub pending_yank: Option<YankPending>,
    skip_trigger_text: bool,
    yank_highlight: Option<YankHighlight>,
    pub last_jump: Option<Jump>,
    search: crate::document_search::SearchSession,
}

impl CopyMode {
    /// Cursor scroll margin, reduced automatically for short viewports.
    pub fn scroll_padding(&self) -> u32 {
        self.scroll_padding.unwrap_or(SCROLL_PADDING_ROWS)
    }
    pub fn set_scroll_padding(&mut self, rows: u32) {
        self.scroll_padding = Some(rows);
    }

    pub fn start(&mut self, generation: u64, state: View) {
        self.generation = Some(generation);
        self.cursor = state.cursor;
        self.visual_anchor = None;
        self.visual_mode = VisualMode::Characterwise;
        self.visual_trim = true;
        self.pending_visual_object = None;
        self.preferred_x = state.cursor.x;
        self.preferred_logical_x = None;
        self.columns = state.columns;
        self.rows = state.total_rows;
        self.pending_g = false;
        self.pending_z = false;
        self.pending_count = None;
        self.pending_jump = None;
        self.pending_yank = None;
        self.skip_trigger_text = false;
        self.yank_highlight = None;
        self.last_jump = None;
        self.search = crate::document_search::SearchSession::default();
    }

    /// Adopt existing newest-first ranges through [`crate::document_search::SearchSession::adopt_search`].
    /// Replaces pending navigation without moving the copy cursor; normal navigation
    /// uses backward direction after adoption. Revision zero disables navigation.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nfm_egui::copy_mode::CopyMode;
    /// let mut mode = CopyMode::default();
    /// mode.adopt_search(1, Vec::new());
    /// assert!(mode.search_matches().is_empty());
    /// ```
    pub fn adopt_search(&mut self, revision: u64, matches: Vec<CellRange>) {
        self.search.adopt_search(revision, matches);
    }
    /// Begin pending navigation through [`crate::document_search::SearchSession::begin_search`].
    /// Queues the initial direction/count and clears matches. This changes copy-mode
    /// state only; submit worker work separately using the same nonzero revision.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nfm_egui::copy_mode::CopyMode;
    /// let mut mode = CopyMode::default();
    /// mode.begin_search(1, true, 1);
    /// assert!(mode.search_accepts_result(1));
    /// ```
    pub fn begin_search(&mut self, revision: u64, forward: bool, count: u32) {
        self.search.begin_search(revision, forward, count);
    }
    /// Cancel pending navigation through [`crate::document_search::SearchSession::cancel_pending_search`].
    /// Retains completed matches and does not cancel a search worker or move the cursor.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nfm_egui::copy_mode::CopyMode;
    /// let mut mode = CopyMode::default();
    /// mode.begin_search(1, true, 1);
    /// mode.cancel_pending_search();
    /// assert!(!mode.search_accepts_result(1));
    /// ```
    pub fn cancel_pending_search(&mut self) {
        self.search.cancel_pending_search();
    }
    /// Check pending query identity through [`crate::document_search::SearchSession::search_accepts_result`].
    /// Also verify your snapshot generation before applying feedback; this method only
    /// checks the waiting flag and query revision.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nfm_egui::copy_mode::CopyMode;
    /// let mut mode = CopyMode::default();
    /// mode.begin_search(1, true, 1);
    /// assert!(mode.search_accepts_result(1));
    /// assert!(!mode.search_accepts_result(2));
    /// ```
    pub fn search_accepts_result(&self, revision: u64) -> bool {
        self.search.search_accepts_result(revision)
    }
    /// Borrow newest-first inclusive match ranges through [`crate::document_search::SearchSession::search_matches`].
    /// No copy is made; a newly begun search has an empty range list until completion.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nfm_egui::copy_mode::CopyMode;
    /// let mut mode = CopyMode::default();
    /// mode.adopt_search(1, Vec::new());
    /// assert!(mode.search_matches().is_empty());
    /// ```
    pub fn search_matches(&self) -> &[CellRange] {
        self.search.search_matches()
    }
    /// Apply pending results through [`crate::document_search::SearchSession::finish_search`],
    /// using the current copy cursor as the starting position. Replays queued motions
    /// and returns a target; does not update the cursor. The host resolves the returned
    /// cell and applies it. Stale results return `None`; empty results return the cursor.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nfm_egui::copy_mode::CopyMode;
    /// let mut mode = CopyMode::default();
    /// mode.begin_search(1, true, 1);
    /// assert_eq!(mode.finish_search(1, Vec::new()), Some(mode.cursor));
    /// ```
    pub fn finish_search(
        &mut self,
        revision: u64,
        matches: Vec<CellRange>,
    ) -> Option<CellPosition> {
        self.search.finish_search(revision, matches, self.cursor)
    }
    /// Navigate through [`crate::document_search::SearchSession::navigate_search`] from
    /// the current copy cursor. `reverse` flips the original search direction. Queues
    /// the move while waiting; otherwise returns a target for the host to resolve and
    /// apply. This method does not move the cursor or paint decorations.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nfm_egui::copy_mode::CopyMode;
    /// let mut mode = CopyMode::default();
    /// mode.begin_search(1, true, 1);
    /// assert!(mode.navigate_search(false, 1).is_none());
    /// assert_eq!(mode.finish_search(1, Vec::new()), Some(mode.cursor));
    /// ```
    pub fn navigate_search(&mut self, reverse: bool, count: u32) -> Option<CellPosition> {
        self.search.navigate_search(self.cursor, reverse, count)
    }

    pub fn visual_selection_to(
        &self,
        cursor: CellPosition,
        read: impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CopySelection> {
        let range = match self.visual_mode {
            VisualMode::Characterwise => self.visual_range_to(cursor),
            VisualMode::Linewise => self.linewise_visual_range_to(cursor, read),
            VisualMode::Blockwise => self.visual_rectangle_to(cursor),
        }?;
        Some(CopySelection {
            range,
            linewise: self.visual_mode == VisualMode::Linewise,
            trim: self.visual_trim && self.visual_mode != VisualMode::Blockwise,
            rectangle: self.visual_mode == VisualMode::Blockwise,
        })
    }

    pub fn text_object_selection(
        &self,
        count: u32,
        object: WordObject,
        kind: CopyTextObject,
        prompt_boundaries: &[u64],
        read: impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CopySelection> {
        let range = match kind {
            CopyTextObject::Word(big) => self.word_object_range(count, object, big, read),
            CopyTextObject::Quote(quote) => self.quote_object_range(object, quote, read),
            CopyTextObject::Delimiter(open, close) => {
                self.delimiter_object_range(object, open, close, read)
            }
            CopyTextObject::Command => self.command_object_range(object, count, prompt_boundaries),
        }?;
        let linewise = matches!(kind, CopyTextObject::Command);
        Some(CopySelection {
            range,
            linewise,
            trim: linewise
                || (matches!(kind, CopyTextObject::Word(_)) && object == WordObject::Inner),
            rectangle: false,
        })
    }

    /// Coordinate-only viewport primitive. Document navigation should use
    /// evaluate_motion, which distinguishes soft wraps from hard line breaks.
    pub fn move_to(&self, motion: Motion, view: Option<View>) -> Option<(CellPosition, bool)> {
        self.simple_target(motion, 1, view)
    }
    fn simple_target(
        &self,
        motion: Motion,
        count: u32,
        view: Option<View>,
    ) -> Option<(CellPosition, bool)> {
        if self.columns == 0 || self.rows == 0 {
            return None;
        }
        let mut cell = self.cursor;
        let snap_right = matches!(motion, Motion::Right);
        let last_row = self.rows - 1;
        match motion {
            Motion::Left => cell.x = cell.x.saturating_sub(1),
            Motion::Right => cell.x = cell.x.saturating_add(1).min(self.columns - 1),
            Motion::RowStart => cell.x = 0,
            Motion::ScrollbackTop => cell.y = 0,
            Motion::ScrollbackBottom => cell.y = last_row,
            Motion::Up => {
                cell.y = cell.y.saturating_sub(u64::from(count));
                cell.x = self.preferred_x.min(self.columns - 1);
            }
            Motion::Down => {
                cell.y = cell.y.saturating_add(u64::from(count)).min(last_row);
                cell.x = self.preferred_x.min(self.columns - 1);
            }
            Motion::ViewportTop | Motion::ViewportMiddle | Motion::ViewportBottom => {
                let view = view?;
                if view.viewport_rows == 0
                    || view.total_rows != self.rows
                    || view.columns != self.columns
                {
                    return None;
                }
                let top = view.viewport_top.min(last_row);
                let bottom = top
                    .saturating_add(u64::from(view.viewport_rows - 1))
                    .min(last_row);
                cell.y = match motion {
                    Motion::ViewportTop => top.saturating_add(u64::from(count - 1)).min(bottom),
                    Motion::ViewportMiddle => top + (bottom - top + 1) / 2,
                    Motion::ViewportBottom => bottom.saturating_sub(u64::from(count - 1)).max(top),
                    _ => unreachable!(),
                };
                cell.x = self.preferred_x.min(self.columns - 1);
            }
            Motion::PageUp | Motion::PageDown | Motion::HalfPageUp | Motion::HalfPageDown => {
                let view = view?;
                if view.viewport_rows == 0
                    || view.total_rows != self.rows
                    || view.columns != self.columns
                {
                    return None;
                }
                let rows = u64::from(view.viewport_rows);
                let step = if matches!(motion, Motion::HalfPageUp | Motion::HalfPageDown) {
                    (rows / 2).max(1)
                } else {
                    rows
                };
                cell.y = if matches!(motion, Motion::PageUp | Motion::HalfPageUp) {
                    cell.y.saturating_sub(step.saturating_mul(u64::from(count)))
                } else {
                    cell.y
                        .saturating_add(step.saturating_mul(u64::from(count)))
                        .min(last_row)
                };
                cell.x = self.preferred_x.min(self.columns - 1);
            }
            Motion::FirstNonblank
            | Motion::LastNonblank
            | Motion::ForwardWord
            | Motion::BackwardWord
            | Motion::ForwardWordEnd => return None,
            Motion::ForwardBigWord
            | Motion::BackwardBigWord
            | Motion::ForwardBigWordEnd
            | Motion::PreviousWordEnd
            | Motion::PreviousBigWordEnd
            | Motion::NextLineNonblank
            | Motion::PreviousLineNonblank
            | Motion::LineNonblank
            | Motion::CharacterJump
            | Motion::MatchingBracket => return None,
        }
        Some((cell, snap_right))
    }

    /// Commit document-aware motion while preserving the logical column for j/k.
    pub fn moved_with_rows(
        &mut self,
        motion: Motion,
        cell: CellPosition,
        mut read: impl FnMut(u64) -> Option<TextRow>,
    ) {
        let goal = if matches!(motion, Motion::Up | Motion::Down) {
            self.preferred_logical_x.or_else(|| {
                let line = self.logical_line(self.cursor.y, &mut read)?;
                Some(line.column(self.cursor, self.columns))
            })
        } else {
            None
        };
        self.moved(motion, cell);
        self.preferred_logical_x = goal;
    }

    pub fn moved(&mut self, motion: Motion, cell: CellPosition) {
        self.preferred_logical_x = None;
        self.cursor = cell;
        if matches!(motion, Motion::Left | Motion::Right | Motion::RowStart) || motion.needs_text()
        {
            self.preferred_x = cell.x;
        }
    }

    pub fn highlight_yank(&mut self, range: CellRange, now: Instant) {
        self.yank_highlight = Some(YankHighlight {
            range,
            rectangle: false,
            expires_at: now + YANK_HIGHLIGHT_DURATION,
        });
    }

    pub fn yank_highlight_range(&self) -> Option<CellRange> {
        self.yank_highlight.map(|highlight| highlight.range)
    }

    pub fn highlight_rectangle_yank(&mut self, range: CellRange, now: Instant) {
        self.highlight_yank(range, now);
        self.yank_highlight.as_mut().unwrap().rectangle = true;
    }

    pub fn yank_highlight_is_rectangle(&self) -> bool {
        self.yank_highlight
            .is_some_and(|highlight| highlight.rectangle)
    }

    pub fn yank_highlight_remaining(&self, now: Instant) -> Option<Duration> {
        self.yank_highlight
            .map(|highlight| highlight.expires_at.saturating_duration_since(now))
    }

    pub fn clear_expired_yank_highlight(&mut self, now: Instant) -> bool {
        if self
            .yank_highlight
            .is_some_and(|highlight| now >= highlight.expires_at)
        {
            self.yank_highlight = None;
            return true;
        }
        false
    }

    pub fn toggle_visual(&mut self, mode: VisualMode) {
        self.pending_visual_object = None;
        self.visual_trim = true;
        if self.visual_anchor.is_some() && self.visual_mode == mode {
            self.visual_anchor = None;
        } else {
            if self.visual_anchor.is_none() {
                self.visual_anchor = Some(self.cursor);
            }
            self.visual_mode = mode;
        }
    }

    pub fn swap_visual_endpoint(&mut self) -> bool {
        let Some(anchor) = self.visual_anchor else {
            return false;
        };
        self.visual_anchor = Some(self.cursor);
        self.cursor = anchor;
        self.preferred_x = anchor.x;
        self.preferred_logical_x = None;
        true
    }

    pub fn swap_visual_horizontal_endpoint(&mut self) -> bool {
        if self.visual_mode != VisualMode::Blockwise {
            return self.swap_visual_endpoint();
        }
        let Some(anchor) = self.visual_anchor.as_mut() else {
            return false;
        };
        std::mem::swap(&mut anchor.x, &mut self.cursor.x);
        self.preferred_x = self.cursor.x;
        self.preferred_logical_x = None;
        true
    }

    pub fn visual_rectangle_to(&self, cursor: CellPosition) -> Option<CellRange> {
        let anchor = self.visual_anchor?;
        Some(CellRange {
            start: CellPosition {
                x: anchor.x.min(cursor.x),
                y: anchor.y.min(cursor.y),
            },
            end: CellPosition {
                x: anchor.x.max(cursor.x),
                y: anchor.y.max(cursor.y),
            },
        })
    }

    pub fn begin_visual_object(&mut self, object: WordObject) -> bool {
        if self.visual_anchor.is_none() {
            return false;
        }
        self.pending_visual_object = Some((object, self.take_count()));
        self.pending_g = false;
        self.pending_jump = None;
        true
    }

    /// Replace the selection with an object; repeated objects do not yet
    /// expand an existing selection as Vim does.
    pub fn select_visual_object(&mut self, range: CellRange, trim: bool) {
        self.visual_anchor = Some(range.start);
        self.visual_mode = VisualMode::Characterwise;
        self.visual_trim = trim;
        self.moved(Motion::CharacterJump, range.end);
        self.pending_visual_object = None;
        self.pending_count = None;
    }

    pub fn visual_range_to(&self, cursor: CellPosition) -> Option<CellRange> {
        let anchor = self.visual_anchor?;
        let (start, end) = if (anchor.y, anchor.x) <= (cursor.y, cursor.x) {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        Some(CellRange { start, end })
    }

    pub fn linewise_visual_range_to(
        &self,
        cursor: CellPosition,
        mut read: impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellRange> {
        self.logical_line_range_between(self.visual_anchor?.y, cursor.y, &mut read)
    }

    pub fn linewise_range_between(
        &self,
        start: CellPosition,
        end: CellPosition,
        mut read: impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellRange> {
        self.logical_line_range_between(start.y, end.y, &mut read)
    }
    fn logical_line(
        &self,
        row: u64,
        read: &mut impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<LogicalLine> {
        let (start, end) = self.logical_line_bounds(row, read)?;
        Some(LogicalLine { start, end })
    }

    fn logical_motion_target(
        &self,
        motion: Motion,
        count: u32,
        read: &mut impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellPosition> {
        let width = self.columns.checked_sub(1)?;
        let mut cell = self.cursor;
        match motion {
            Motion::Left => {
                if cell.x > 0 {
                    cell.x -= 1;
                } else if cell.y > 0 && read(cell.y - 1)?.wraps_to_next {
                    cell.y -= 1;
                    cell.x = width;
                }
            }
            Motion::Right => {
                // Resolve within the row as before; at the last grapheme of a
                // soft-wrapped row, move directly to its continuation (wide cells too).
                let row = read(cell.y)?;
                let last = row
                    .grapheme_cells()
                    .last()
                    .map(|(x, _)| *x)
                    .unwrap_or(width);
                if cell.x >= last && row.wraps_to_next && cell.y + 1 < self.rows {
                    cell = CellPosition {
                        x: 0,
                        y: cell.y + 1,
                    };
                } else {
                    cell.x = cell.x.saturating_add(1).min(width);
                }
            }
            Motion::RowStart => {
                cell = CellPosition {
                    x: 0,
                    y: self.logical_line(cell.y, read)?.start,
                }
            }
            Motion::Up | Motion::Down => {
                let mut line = self.logical_line(cell.y, read)?;
                let goal = self
                    .preferred_logical_x
                    .unwrap_or_else(|| line.column(cell, self.columns));
                let mut changed = false;
                for _ in 0..count.max(1) {
                    let row = if motion == Motion::Up {
                        line.start.checked_sub(1)
                    } else {
                        (line.end + 1 < self.rows).then_some(line.end + 1)
                    };
                    let Some(row) = row else {
                        break;
                    };
                    line = self.logical_line(row, read)?;
                    changed = true;
                }
                if !changed {
                    return Some(cell);
                }
                cell.y = line
                    .start
                    .saturating_add(goal / u64::from(self.columns))
                    .min(line.end);
                cell.x = if goal / u64::from(self.columns) > line.end - line.start {
                    width
                } else {
                    (goal % u64::from(self.columns)) as u32
                };
                if cell.y == line.end {
                    let end = read(cell.y)?
                        .glyphs(false)
                        .iter()
                        .rev()
                        .find(|g| g.nonblank)
                        .map_or(0, |g| g.x);
                    cell.x = cell.x.min(end);
                }
            }
            _ => return None,
        }
        Some(cell)
    }

    fn logical_line_bounds(
        &self,
        row: u64,
        read: &mut impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<(u64, u64)> {
        if row >= self.rows {
            return None;
        }
        let mut start = row;
        while start > 0 && read(start - 1)?.wraps_to_next {
            start -= 1;
        }
        let mut end = row;
        while end < self.rows - 1 && read(end)?.wraps_to_next {
            end += 1;
        }
        Some((start, end))
    }

    pub fn yank_lines_range(
        &self,
        count: u32,
        mut read: impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellRange> {
        let (start, mut end) = self.logical_line_bounds(self.cursor.y, &mut read)?;
        for _ in 1..count {
            if end >= self.rows - 1 {
                break;
            }
            (_, end) = self.logical_line_bounds(end + 1, &mut read)?;
        }
        Some(CellRange {
            start: CellPosition { x: 0, y: start },
            end: CellPosition {
                x: self.columns.checked_sub(1)?,
                y: end,
            },
        })
    }

    pub fn yank_to_row_end_range(
        &self,
        count: u32,
        mut read: impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellRange> {
        let end = self.line_content_target(Motion::LastNonblank, count, &mut read)?;
        ((end.y, end.x) >= (self.cursor.y, self.cursor.x)).then_some(CellRange {
            start: self.cursor,
            end,
        })
    }

    pub fn yank_motion_range(
        &self,
        result: MotionResult,
        mut read: impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<(CellRange, bool)> {
        if result.kind == RangeKind::Linewise {
            return Some((
                self.logical_line_range_between(self.cursor.y, result.target.y, &mut read)?,
                true,
            ));
        }
        let a = self.cursor;
        let b = result.target;
        let (start, mut end) = if (a.y, a.x) <= (b.y, b.x) {
            (a, b)
        } else {
            (b, a)
        };
        if result.endpoint == Endpoint::Exclusive {
            end = self.previous_text_cell(end, &mut read)?;
        }
        ((start.y, start.x) <= (end.y, end.x)).then_some((CellRange { start, end }, false))
    }

    fn logical_line_range_between(
        &self,
        a: u64,
        b: u64,
        read: &mut impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellRange> {
        let (a_start, a_end) = self.logical_line_bounds(a, read)?;
        let (b_start, b_end) = if a == b {
            (a_start, a_end)
        } else {
            self.logical_line_bounds(b, read)?
        };
        Some(CellRange {
            start: CellPosition {
                x: 0,
                y: a_start.min(b_start),
            },
            end: CellPosition {
                x: self.columns.checked_sub(1)?,
                y: a_end.max(b_end),
            },
        })
    }
    fn previous_text_cell(
        &self,
        cell: CellPosition,
        read: &mut impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellPosition> {
        for y in (0..=cell.y).rev() {
            let row = read(y)?;
            if let Some((x, _)) = row
                .grapheme_cells()
                .into_iter()
                .rev()
                .find(|(x, _)| y < cell.y || *x < cell.x)
            {
                return Some(CellPosition { x, y });
            }
        }
        None
    }

    pub fn word_object_range(
        &self,
        count: u32,
        object: WordObject,
        big: bool,
        mut read: impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellRange> {
        let line = self.logical_word_line(big, &mut read)?;
        let glyphs = &line.glyphs;
        let WordSpan { mut start, mut end } = word_span_at(glyphs, self.cursor, false)?;
        for _ in 1..count {
            let Some(next) = glyphs[end..]
                .iter()
                .position(|glyph| glyph.class != WordClass::Whitespace)
                .map(|offset| end + offset)
            else {
                break;
            };
            end = next + 1;
            while end < glyphs.len() && glyphs[end].class == glyphs[next].class {
                end += 1;
            }
        }
        if object == WordObject::Around {
            if end < glyphs.len() && glyphs[end].class == WordClass::Whitespace {
                while end < glyphs.len() && glyphs[end].class == WordClass::Whitespace {
                    end += 1;
                }
            } else {
                while start > 0 && glyphs[start - 1].class == WordClass::Whitespace {
                    start -= 1;
                }
            }
        }
        Some(CellRange {
            start: glyphs[start].cell,
            end: glyphs[end - 1].cell,
        })
    }

    pub fn quote_object_range(
        &self,
        object: WordObject,
        quote: char,
        mut read: impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellRange> {
        let (first, last) = self.logical_line_bounds(self.cursor.y, &mut read)?;
        let mut quotes = Vec::new();
        for y in first..=last {
            let row = read(y)?;
            let mut escaped = false;
            for (x, grapheme) in row.grapheme_cells() {
                if grapheme == "\\" {
                    escaped = !escaped;
                } else {
                    if grapheme.starts_with(quote) && !escaped {
                        quotes.push(CellPosition { x, y });
                    }
                    escaped = false;
                }
            }
        }
        let cursor = (self.cursor.y, self.cursor.x);
        for pair in quotes.chunks_exact(2) {
            if (pair[0].y, pair[0].x) <= cursor && cursor <= (pair[1].y, pair[1].x) {
                return self.bounded_object_range(pair[0], pair[1], object, &mut read);
            }
        }
        None
    }

    pub fn delimiter_object_range(
        &self,
        object: WordObject,
        open: char,
        close: char,
        mut read: impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellRange> {
        let cursor = (self.cursor.y, self.cursor.x);
        let mut depth = 0usize;
        let mut opener = None;
        for step in TextScanner::new(
            &mut read,
            self.rows,
            self.cursor.y,
            Direction::Backward,
            false,
        ) {
            let TextStep::Glyph(glyph) = step else {
                continue;
            };
            if (glyph.cell.y, glyph.cell.x) > cursor {
                continue;
            }
            if glyph.first == close {
                depth += 1;
            }
            if glyph.first == open {
                if depth == 0 {
                    opener = Some(glyph.cell);
                    break;
                }
                depth -= 1;
            }
        }
        let opener = opener?;
        depth = 0;
        let mut closer = None;
        for step in TextScanner::new(&mut read, self.rows, opener.y, Direction::Forward, false) {
            let TextStep::Glyph(glyph) = step else {
                continue;
            };
            if (glyph.cell.y, glyph.cell.x) < (opener.y, opener.x) {
                continue;
            }
            if glyph.first == open {
                depth += 1;
            }
            if glyph.first == close {
                depth -= 1;
                if depth == 0 {
                    closer = Some(glyph.cell);
                    break;
                }
            }
        }
        self.bounded_object_range(opener, closer?, object, &mut read)
    }
    pub fn command_object_range(
        &self,
        object: WordObject,
        count: u32,
        boundaries: &[u64],
    ) -> Option<CellRange> {
        let index = boundaries
            .partition_point(|&y| y <= self.cursor.y)
            .checked_sub(1)?;
        let prompt = boundaries[index];
        let mut end = self.rows.checked_sub(1)?;
        if let Some(&next) = boundaries.get(index.saturating_add(count as usize)) {
            end = next.checked_sub(1)?;
        }
        let start = if object == WordObject::Around {
            prompt
        } else {
            prompt + 1
        };
        if start > end {
            return None;
        }
        Some(CellRange {
            start: CellPosition { x: 0, y: start },
            end: CellPosition {
                x: self.columns.checked_sub(1)?,
                y: end,
            },
        })
    }

    pub fn total_rows(&self) -> u64 {
        self.rows
    }

    pub fn positioned_viewport_top(&self, position: ViewportPosition, view: View) -> Option<u64> {
        if view.viewport_rows == 0 || view.total_rows != self.rows || view.columns != self.columns {
            return None;
        }
        let height = u64::from(view.viewport_rows);
        let padding = u64::from(self.scroll_padding()).min((height - 1) / 2);
        let offset = match position {
            ViewportPosition::Top => padding,
            ViewportPosition::Center => height / 2,
            ViewportPosition::Bottom => height - 1 - padding,
        };
        Some(
            self.cursor
                .y
                .saturating_sub(offset)
                .min(self.rows.saturating_sub(height)),
        )
    }

    /// Scroll physical rows, moving the cursor only to keep it within the
    /// padded viewport. The caller resolves wide cells before publishing.
    pub fn scrolled_viewport_target(
        &self,
        down: bool,
        count: u32,
        view: View,
    ) -> Option<(u64, CellPosition)> {
        if self.rows == 0
            || self.columns == 0
            || view.viewport_rows == 0
            || view.total_rows != self.rows
            || view.columns != self.columns
        {
            return None;
        }
        let height = u64::from(view.viewport_rows).min(self.rows);
        let max_top = self.rows - height;
        let current = view.viewport_top.min(max_top);
        let top = if down {
            current.saturating_add(u64::from(count)).min(max_top)
        } else {
            current.saturating_sub(u64::from(count))
        };
        // A clamped scroll at either boundary must not move the cursor.
        if top == current {
            return Some((top, self.cursor));
        }
        let padding = u64::from(self.scroll_padding()).min((height - 1) / 2);
        let y = self
            .cursor
            .y
            .clamp(top + padding, top + height - 1 - padding);
        let x = if y == self.cursor.y {
            self.cursor.x
        } else {
            self.preferred_x.min(self.columns - 1)
        };
        Some((top, CellPosition { x, y }))
    }

    /// Evaluate without changing the displayed cursor. Operators can reject a
    /// partial result, while ordinary movement can keep the progress made.
    pub fn evaluate_motion(
        &self,
        motion: Motion,
        count: u32,
        explicit_count: bool,
        view: Option<View>,
        read: impl FnMut(u64) -> Option<TextRow>,
        mut resolve: impl FnMut(CellPosition, bool) -> Option<CellPosition>,
    ) -> MotionEvaluation {
        let count = count.max(1);
        // A counted motion often revisits the same rows. Keep a small cache
        // for this evaluation only; scanning never retains the whole history.
        let mut source = read;
        let mut rows: VecDeque<(u64, TextRow)> = VecDeque::new();
        let mut read = |y| {
            if let Some((_, row)) = rows.iter().find(|(cached, _)| *cached == y) {
                return Some(row.clone());
            }
            let row = source(y)?;
            if rows.len() == 4 {
                rows.pop_front();
            }
            rows.push_back((y, row.clone()));
            Some(row)
        };
        let direct = if explicit_count
            && matches!(motion, Motion::ScrollbackTop | Motion::ScrollbackBottom)
        {
            Some(
                self.numbered_logical_line_target(count, &mut read)
                    .map(|cell| (cell, false)),
            )
        } else if explicit_count && motion == Motion::MatchingBracket {
            Some(self.percent_row_target(count).map(|cell| (cell, false)))
        } else if matches!(motion, Motion::Up | Motion::Down) {
            Some(
                self.logical_motion_target(motion, count, &mut read)
                    .map(|cell| (cell, false)),
            )
        } else if motion == Motion::LastNonblank {
            Some(
                self.line_content_target(motion, count, &mut read)
                    .map(|cell| (cell, false)),
            )
        } else if matches!(
            motion,
            Motion::NextLineNonblank | Motion::PreviousLineNonblank | Motion::LineNonblank
        ) {
            Some(
                self.nonblank_line_target(motion, count, &mut read)
                    .map(|cell| (cell, false)),
            )
        } else {
            self.counted_simple_target(motion, count, view).map(Some)
        };
        if let Some(target) = direct {
            return match target {
                None => MotionEvaluation::NoMovement,
                Some((cell, right)) => match resolve(cell, right) {
                    Some(actual) => MotionEvaluation::Complete(motion.result(actual)),
                    None => MotionEvaluation::InvalidSnapshot,
                },
            };
        }
        let mut working = self.clone();
        let mut result = None;
        for _ in 0..count {
            let target = if motion.needs_text() {
                working
                    .text_target(motion, &mut read)
                    .map(|cell| (cell, motion == Motion::Right))
            } else {
                working.move_to(motion, view)
            };
            let Some((cell, right)) = target else {
                return result.map_or(MotionEvaluation::NoMovement, MotionEvaluation::Partial);
            };
            let Some(actual) = resolve(cell, right) else {
                return MotionEvaluation::InvalidSnapshot;
            };
            if actual == working.cursor {
                break;
            }
            working.moved_with_rows(motion, actual, &mut read);
            result = Some(motion.result(actual));
        }
        result.map_or(MotionEvaluation::NoMovement, MotionEvaluation::Complete)
    }

    pub fn command_target(
        &self,
        forward: bool,
        count: u32,
        boundaries: &[u64],
    ) -> Option<CellPosition> {
        let steps = usize::try_from(count.max(1)).ok()?;
        let index = if forward {
            boundaries
                .partition_point(|&row| row <= self.cursor.y)
                .checked_add(steps - 1)?
        } else {
            boundaries
                .partition_point(|&row| row < self.cursor.y)
                .checked_sub(steps)?
        };
        Some(CellPosition {
            x: 0,
            y: *boundaries.get(index)?,
        })
    }

    fn bounded_object_range(
        &self,
        start: CellPosition,
        end: CellPosition,
        object: WordObject,
        read: &mut impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellRange> {
        if object == WordObject::Around {
            return Some(CellRange { start, end });
        }
        let first = (start.y..=end.y).find_map(|y| {
            read(y)?
                .grapheme_cells()
                .into_iter()
                .find(|(x, _)| (y, *x) > (start.y, start.x))
                .map(|(x, _)| CellPosition { x, y })
        })?;
        let last = (start.y..=end.y).rev().find_map(|y| {
            read(y)?
                .grapheme_cells()
                .into_iter()
                .rev()
                .find(|(x, _)| (y, *x) < (end.y, end.x))
                .map(|(x, _)| CellPosition { x, y })
        })?;
        ((first.y, first.x) <= (last.y, last.x)).then_some(CellRange {
            start: first,
            end: last,
        })
    }

    pub fn add_count_digit(&mut self, digit: u32) -> bool {
        if digit == 0 && self.pending_count.is_none() {
            return false;
        }
        self.pending_count = Some(
            self.pending_count
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(digit),
        );
        true
    }

    pub fn take_count(&mut self) -> u32 {
        self.pending_count.take().unwrap_or(1).max(1)
    }

    fn numbered_logical_line_target(
        &self,
        count: u32,
        read: &mut impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellPosition> {
        let mut line = self.logical_line(0, read)?;
        for _ in 1..count.max(1) {
            if line.end + 1 >= self.rows {
                break;
            }
            line = self.logical_line(line.end + 1, read)?;
        }
        Some(CellPosition {
            x: self.preferred_x.min(self.columns.checked_sub(1)?),
            y: line.start,
        })
    }

    pub fn numbered_row_target(&self, count: u32) -> Option<CellPosition> {
        let last = self.rows.checked_sub(1)?;
        Some(CellPosition {
            x: self.preferred_x.min(self.columns.checked_sub(1)?),
            y: u64::from(count - 1).min(last),
        })
    }

    pub fn percent_row_target(&self, percent: u32) -> Option<CellPosition> {
        if percent == 0 || percent > 100 {
            return None;
        }
        let last = self.rows.checked_sub(1)?;
        Some(CellPosition {
            x: self.preferred_x.min(self.columns.checked_sub(1)?),
            y: u64::from(percent - 1).saturating_mul(last) / 99,
        })
    }

    pub fn counted_simple_target(
        &self,
        motion: Motion,
        count: u32,
        view: Option<View>,
    ) -> Option<(CellPosition, bool)> {
        // Horizontal counts resolve intermediate cells so a wide grapheme
        // counts as one motion rather than two terminal columns.
        if matches!(
            motion,
            Motion::Up
                | Motion::Down
                | Motion::PageUp
                | Motion::PageDown
                | Motion::HalfPageUp
                | Motion::HalfPageDown
                | Motion::ViewportTop
                | Motion::ViewportBottom
        ) {
            self.simple_target(motion, count.max(1), view)
        } else {
            None
        }
    }
    pub fn nonblank_line_target(
        &self,
        motion: Motion,
        count: u32,
        mut read: impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellPosition> {
        let mut line = self.logical_line(self.cursor.y, &mut read)?;
        let steps = match motion {
            Motion::NextLineNonblank | Motion::PreviousLineNonblank => count.max(1),
            Motion::LineNonblank => count.max(1) - 1,
            _ => return None,
        };
        for _ in 0..steps {
            let next = if motion == Motion::PreviousLineNonblank {
                line.start.checked_sub(1)
            } else {
                (line.end + 1 < self.rows).then_some(line.end + 1)
            };
            let Some(next) = next else {
                break;
            };
            line = self.logical_line(next, &mut read)?;
        }
        let mut mode = self.clone();
        mode.cursor = CellPosition {
            x: 0,
            y: line.start,
        };
        mode.line_content_target(Motion::FirstNonblank, 1, &mut read)
    }

    /// Content boundaries belong to the logical line, not its screen rows.
    fn line_content_target(
        &self,
        motion: Motion,
        count: u32,
        read: &mut impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellPosition> {
        let (mut start, mut end) = self.logical_line_bounds(self.cursor.y, read)?;
        if motion == Motion::LastNonblank {
            for _ in 1..count.max(1) {
                if end + 1 >= self.rows {
                    break;
                }
                (start, end) = self.logical_line_bounds(end + 1, read)?;
            }
            for y in (start..=end).rev() {
                if let Some(glyph) = read(y)?.glyphs(false).iter().rev().find(|g| g.nonblank) {
                    return Some(CellPosition { x: glyph.x, y });
                }
            }
            Some(CellPosition { x: 0, y: end })
        } else {
            for y in start..=end {
                if let Some(glyph) = read(y)?.glyphs(false).iter().find(|g| g.nonblank) {
                    return Some(CellPosition { x: glyph.x, y });
                }
            }
            Some(CellPosition { x: 0, y: start })
        }
    }

    fn logical_word_line(
        &self,
        big: bool,
        read: &mut impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<LogicalWordLine> {
        let (start, end) = self.logical_line_bounds(self.cursor.y, read)?;
        let mut line = LogicalWordLine {
            text: String::new(),
            glyphs: Vec::new(),
        };
        for y in start..=end {
            let row = read(y)?;
            let byte_base = line.text.len();
            for glyph in row.glyphs(big) {
                line.glyphs.push(LocatedGlyph {
                    cell: CellPosition { x: glyph.x, y },
                    class: glyph.class,
                    byte_start: byte_base + glyph.byte_start,
                    byte_end: byte_base + glyph.byte_end,
                });
            }
            line.text.push_str(&row.text);
        }
        Some(line)
    }

    pub fn word_under_cursor(
        &self,
        mut read: impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<String> {
        let line = self.logical_word_line(false, &mut read)?;
        let span = word_span_at(&line.glyphs, self.cursor, true)?;
        Some(
            line.text[line.glyphs[span.start].byte_start..line.glyphs[span.end - 1].byte_end]
                .to_owned(),
        )
    }
    pub fn text_target(
        &self,
        motion: Motion,
        mut read: impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellPosition> {
        match motion {
            Motion::Left | Motion::Right | Motion::RowStart => {
                self.logical_motion_target(motion, 1, &mut read)
            }
            Motion::FirstNonblank | Motion::LastNonblank => {
                self.line_content_target(motion, 1, &mut read)
            }
            Motion::ForwardWord => self.forward_word(&mut read, false),
            Motion::BackwardWord => self.backward_word(&mut read, false),
            Motion::ForwardWordEnd => self.forward_word_end(&mut read, false),
            Motion::ForwardBigWord => self.forward_word(&mut read, true),
            Motion::BackwardBigWord => self.backward_word(&mut read, true),
            Motion::ForwardBigWordEnd => self.forward_word_end(&mut read, true),
            Motion::PreviousWordEnd => self.previous_word_end(&mut read, false),
            Motion::PreviousBigWordEnd => self.previous_word_end(&mut read, true),
            Motion::NextLineNonblank | Motion::PreviousLineNonblank | Motion::LineNonblank => {
                self.nonblank_line_target(motion, 1, read)
            }
            Motion::MatchingBracket => self.matching_bracket(&mut read),
            _ => None,
        }
    }

    pub fn jump_target(
        &self,
        jump: &Jump,
        repeat: bool,
        mut read: impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellPosition> {
        let line = self.logical_word_line(false, &mut read)?;
        let cells = &line.glyphs;
        let mut cutoff = self.cursor;
        if repeat && jump.till {
            let adjacent = if jump.forward {
                cells
                    .iter()
                    .find(|g| (g.cell.y, g.cell.x) > (cutoff.y, cutoff.x))
            } else {
                cells
                    .iter()
                    .rev()
                    .find(|g| (g.cell.y, g.cell.x) < (cutoff.y, cutoff.x))
            };
            if let Some(g) = adjacent {
                cutoff = g.cell;
            }
        }
        let matches = |g: &LocatedGlyph| {
            let at = (g.cell.y, g.cell.x);
            (if jump.forward {
                at > (cutoff.y, cutoff.x)
            } else {
                at < (cutoff.y, cutoff.x)
            }) && line.text[g.byte_start..g.byte_end] == jump.target
        };
        let index = if jump.forward {
            cells.iter().position(matches)
        } else {
            cells.iter().rposition(matches)
        }?;
        let index = if jump.till {
            if jump.forward {
                index.checked_sub(1)?
            } else {
                index.checked_add(1)?
            }
        } else {
            index
        };
        Some(cells.get(index)?.cell)
    }

    fn matching_bracket(
        &self,
        read: &mut impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<CellPosition> {
        let line = self.logical_word_line(false, read)?;
        let (source_cell, source, target, forward) = line
            .glyphs
            .iter()
            .filter(|g| (g.cell.y, g.cell.x) >= (self.cursor.y, self.cursor.x))
            .find_map(|g| {
                let x = g.cell;
                match &line.text[g.byte_start..g.byte_end] {
                    "(" => Some((x, '(', ')', true)),
                    "[" => Some((x, '[', ']', true)),
                    "{" => Some((x, '{', '}', true)),
                    ")" => Some((x, ')', '(', false)),
                    "]" => Some((x, ']', '[', false)),
                    "}" => Some((x, '}', '{', false)),
                    _ => None,
                }
            })?;
        let direction = if forward {
            Direction::Forward
        } else {
            Direction::Backward
        };
        let mut depth = 0usize;
        for step in TextScanner::new(read, self.rows, source_cell.y, direction, false) {
            let TextStep::Glyph(glyph) = step else {
                continue;
            };
            if if forward {
                (glyph.cell.y, glyph.cell.x) < (source_cell.y, source_cell.x)
            } else {
                (glyph.cell.y, glyph.cell.x) > (source_cell.y, source_cell.x)
            } {
                continue;
            }
            if !glyph.single {
                continue;
            }
            if glyph.first == source {
                depth += 1;
            } else if glyph.first == target {
                depth -= 1;
                if depth == 0 {
                    return Some(glyph.cell);
                }
            }
        }
        None
    }

    fn forward_word(
        &self,
        read: &mut impl FnMut(u64) -> Option<TextRow>,
        big: bool,
    ) -> Option<CellPosition> {
        let mut previous = WordClass::Whitespace;
        for step in TextScanner::new(read, self.rows, self.cursor.y, Direction::Forward, big) {
            if matches!(step, TextStep::EmptyRow) {
                continue;
            }
            let TextStep::Glyph(glyph) = step else {
                previous = WordClass::Whitespace;
                continue;
            };
            if (glyph.cell.y, glyph.cell.x) <= (self.cursor.y, self.cursor.x) {
                previous = glyph.class;
                continue;
            }
            if glyph.class != WordClass::Whitespace && glyph.class != previous {
                return Some(glyph.cell);
            }
            previous = glyph.class;
        }
        None
    }

    fn backward_word(
        &self,
        read: &mut impl FnMut(u64) -> Option<TextRow>,
        big: bool,
    ) -> Option<CellPosition> {
        let mut scan =
            TextScanner::new(read, self.rows, self.cursor.y, Direction::Backward, big).peekable();
        while let Some(step) = scan.next() {
            let TextStep::Glyph(glyph) = step else {
                continue;
            };
            if (glyph.cell.y, glyph.cell.x) >= (self.cursor.y, self.cursor.x)
                || glyph.class == WordClass::Whitespace
            {
                continue;
            }
            let previous_class = match scan.peek() {
                Some(TextStep::Glyph(previous)) => previous.class,
                _ => WordClass::Whitespace,
            };
            if glyph.class != previous_class {
                return Some(glyph.cell);
            }
        }
        None
    }

    fn forward_word_end(
        &self,
        read: &mut impl FnMut(u64) -> Option<TextRow>,
        big: bool,
    ) -> Option<CellPosition> {
        let mut active_class = WordClass::Whitespace;
        let mut last: Option<CellPosition> = None;
        for step in TextScanner::new(read, self.rows, self.cursor.y, Direction::Forward, big) {
            let (class, cell) = match step {
                TextStep::Glyph(glyph) => {
                    if (glyph.cell.y, glyph.cell.x) < (self.cursor.y, self.cursor.x) {
                        continue;
                    }
                    (glyph.class, Some(glyph.cell))
                }
                TextStep::LineBreak => (WordClass::Whitespace, None),
                TextStep::EmptyRow => continue,
            };
            if class != active_class {
                if let Some(end) = last
                    && (end.y, end.x) > (self.cursor.y, self.cursor.x)
                {
                    return Some(end);
                }
                last = None;
            }
            active_class = class;
            if class != WordClass::Whitespace {
                last = cell;
            }
        }
        last.filter(|end| (end.y, end.x) > (self.cursor.y, self.cursor.x))
    }

    fn previous_word_end(
        &self,
        read: &mut impl FnMut(u64) -> Option<TextRow>,
        big: bool,
    ) -> Option<CellPosition> {
        // Include text to the right of the cursor to establish the class on
        // the other side of the first candidate, even across a soft wrap.
        let row = read(self.cursor.y)?;
        let mut next_class = if row.wraps_to_next && self.cursor.y + 1 < self.rows {
            read(self.cursor.y + 1)?
                .glyphs(big)
                .first()
                .map_or(WordClass::Whitespace, |glyph| glyph.class)
        } else {
            WordClass::Whitespace
        };
        for step in TextScanner::new(read, self.rows, self.cursor.y, Direction::Backward, big) {
            let TextStep::Glyph(glyph) = step else {
                next_class = WordClass::Whitespace;
                continue;
            };
            if (glyph.cell.y, glyph.cell.x) >= (self.cursor.y, self.cursor.x) {
                next_class = glyph.class;
                continue;
            }
            if glyph.class != WordClass::Whitespace && glyph.class != next_class {
                return Some(glyph.cell);
            }
            next_class = glyph.class;
        }
        None
    }
}

pub use crate::document_search::next_match_cell;

/// Decoration ranges are ordered newest row first. Only materialize visible
/// physical rows, even when a block extends across the entire scrollback.
pub fn rectangle_row_ranges(range: CellRange, top: u64, height: u64) -> Vec<CellRange> {
    let start = range.start.y.max(top);
    let end = range
        .end
        .y
        .min(top.saturating_add(height).saturating_sub(1));
    if height == 0 || start > end {
        return Vec::new();
    }
    (start..=end)
        .rev()
        .map(|y| CellRange {
            start: CellPosition {
                x: range.start.x,
                y,
            },
            end: CellPosition { x: range.end.x, y },
        })
        .collect()
}

pub fn word_search_pattern(word: &str) -> String {
    format!(r"(?i:\b{}\b)", regex::escape(word))
}

pub fn is_copy_modifier_key(key: egui::Key) -> bool {
    matches!(
        key,
        egui::Key::ShiftLeft
            | egui::Key::ShiftRight
            | egui::Key::ControlLeft
            | egui::Key::ControlRight
            | egui::Key::AltLeft
            | egui::Key::AltRight
            | egui::Key::SuperLeft
            | egui::Key::SuperRight
    )
}

pub fn copy_viewport_position_key(
    key: egui::Key,
    modifiers: egui::Modifiers,
) -> Option<ViewportPosition> {
    if modifiers != egui::Modifiers::NONE {
        return None;
    }
    match key {
        egui::Key::T => Some(ViewportPosition::Top),
        egui::Key::Z => Some(ViewportPosition::Center),
        egui::Key::B => Some(ViewportPosition::Bottom),
        _ => None,
    }
}

#[derive(Clone, Copy)]
pub enum CopyTextObject {
    Word(bool),
    Quote(char),
    Delimiter(char, char),
    Command,
}

pub fn copy_text_object_key(key: egui::Key, modifiers: egui::Modifiers) -> Option<CopyTextObject> {
    use egui::Key;
    if modifiers.ctrl || modifiers.alt || modifiers.mac_cmd {
        return None;
    }
    match key {
        Key::W => Some(CopyTextObject::Word(modifiers.shift)),
        Key::C if !modifiers.shift => Some(CopyTextObject::Command),
        Key::Quote => Some(CopyTextObject::Quote(if modifiers.shift {
            '"'
        } else {
            '\''
        })),
        Key::Backtick => Some(CopyTextObject::Quote('`')),
        Key::Num9 if modifiers.shift => Some(CopyTextObject::Delimiter('(', ')')),
        Key::Num0 if modifiers.shift => Some(CopyTextObject::Delimiter('(', ')')),
        Key::OpenBracket | Key::CloseBracket => Some(CopyTextObject::Delimiter('[', ']')),
        Key::OpenCurlyBracket | Key::CloseCurlyBracket => Some(CopyTextObject::Delimiter('{', '}')),
        Key::Comma | Key::Period if modifiers.shift => Some(CopyTextObject::Delimiter('<', '>')),
        _ => None,
    }
}

pub fn copy_scroll_direction(key: egui::Key, modifiers: egui::Modifiers) -> Option<bool> {
    // egui-winit also sets `command` for Ctrl on Windows and Linux.
    if !modifiers.ctrl || modifiers.shift || modifiers.alt || modifiers.mac_cmd {
        return None;
    }
    match key {
        egui::Key::E => Some(true),
        egui::Key::Y => Some(false),
        _ => None,
    }
}

pub fn copy_motion(key: egui::Key, modifiers: egui::Modifiers) -> Option<Motion> {
    use egui::Key;
    if modifiers.alt || modifiers.mac_cmd {
        return None;
    }
    if modifiers.ctrl {
        return if modifiers.shift {
            None
        } else {
            match key {
                Key::U => Some(Motion::HalfPageUp),
                Key::D => Some(Motion::HalfPageDown),
                _ => None,
            }
        };
    }
    if modifiers.command {
        return None;
    }
    if modifiers.shift {
        return match key {
            Key::H => Some(Motion::ViewportTop),
            Key::M => Some(Motion::ViewportMiddle),
            Key::L => Some(Motion::ViewportBottom),
            Key::Num6 => Some(Motion::FirstNonblank),
            Key::Num4 => Some(Motion::LastNonblank),
            Key::W => Some(Motion::ForwardBigWord),
            Key::B => Some(Motion::BackwardBigWord),
            Key::E => Some(Motion::ForwardBigWordEnd),
            Key::G => Some(Motion::ScrollbackBottom),
            Key::Num5 => Some(Motion::MatchingBracket),
            Key::Minus => Some(Motion::LineNonblank),
            Key::Equals | Key::Plus => Some(Motion::NextLineNonblank),
            _ => None,
        };
    }
    match key {
        Key::H | Key::ArrowLeft => Some(Motion::Left),
        Key::J | Key::ArrowDown => Some(Motion::Down),
        Key::K | Key::ArrowUp => Some(Motion::Up),
        Key::L | Key::ArrowRight => Some(Motion::Right),
        Key::W => Some(Motion::ForwardWord),
        Key::B => Some(Motion::BackwardWord),
        Key::E => Some(Motion::ForwardWordEnd),
        Key::Num0 => Some(Motion::RowStart),
        Key::Minus => Some(Motion::PreviousLineNonblank),
        Key::Plus => Some(Motion::NextLineNonblank),
        Key::PageUp => Some(Motion::PageUp),
        Key::PageDown => Some(Motion::PageDown),
        _ => None,
    }
}

pub fn copy_digit(key: egui::Key) -> Option<u32> {
    use egui::Key;
    Some(match key {
        Key::Num0 => 0,
        Key::Num1 => 1,
        Key::Num2 => 2,
        Key::Num3 => 3,
        Key::Num4 => 4,
        Key::Num5 => 5,
        Key::Num6 => 6,
        Key::Num7 => 7,
        Key::Num8 => 8,
        Key::Num9 => 9,
        _ => return None,
    })
}

pub fn copy_motion_with_prefix(
    mode: &mut CopyMode,
    key: egui::Key,
    modifiers: egui::Modifiers,
) -> Option<Motion> {
    if mode.pending_g {
        mode.pending_g = false;
        if key == egui::Key::G && modifiers == egui::Modifiers::NONE {
            return Some(Motion::ScrollbackTop);
        }
        if key == egui::Key::E && modifiers == egui::Modifiers::NONE {
            return Some(Motion::PreviousWordEnd);
        }
        if key == egui::Key::E
            && modifiers.shift
            && !modifiers.ctrl
            && !modifiers.alt
            && !modifiers.mac_cmd
        {
            return Some(Motion::PreviousBigWordEnd);
        }
    } else if key == egui::Key::G && modifiers == egui::Modifiers::NONE {
        mode.pending_g = true;
        return None;
    }
    copy_motion(key, modifiers)
}

pub fn copy_jump_prefix(key: egui::Key, modifiers: egui::Modifiers) -> Option<(bool, bool)> {
    if modifiers != egui::Modifiers::NONE
        && modifiers
            != (egui::Modifiers {
                shift: true,
                ..Default::default()
            })
    {
        return None;
    }
    match (key, modifiers.shift) {
        (egui::Key::F, false) => Some((true, false)),
        (egui::Key::F, true) => Some((false, false)),
        (egui::Key::T, false) => Some((true, true)),
        (egui::Key::T, true) => Some((false, true)),
        _ => None,
    }
}

pub enum CopyAction {
    BeginSearch {
        forward: bool,
    },
    OpenLineSplit,
    None,
    Exit,
    ClearSearch {
        cancel_pending: bool,
    },
    Redraw,
    Position(ViewportPosition),
    SelectObject {
        object: WordObject,
        kind: CopyTextObject,
        count: u32,
    },
    SwapEndpoint,
    Scroll {
        down: bool,
        count: u32,
    },
    ToggleVisual,
    HintYank {
        origin: CellPosition,
    },
    YankVisual,
    Yank(YankCommand),
    Jump {
        jump: Jump,
        repeat: bool,
        count: u32,
    },
    SearchWord {
        forward: bool,
        count: u32,
    },
    NavigateSearch {
        reverse: bool,
        count: u32,
    },
    Motion {
        motion: Motion,
        count: u32,
        explicit_count: bool,
    },
}

pub enum YankCommand {
    Lines(u32),
    RowEnd(u32),
    Object {
        object: WordObject,
        kind: CopyTextObject,
        count: u32,
    },
    Motion {
        motion: Motion,
        count: u32,
        explicit_count: bool,
    },
    Jump {
        jump: Jump,
        count: u32,
    },
}

impl CopyMode {
    pub fn clear_pending_for_navigation(&mut self) {
        self.visual_anchor = None;
        self.pending_yank = None;
        self.pending_visual_object = None;
        self.pending_jump = None;
        self.pending_count = None;
        self.pending_g = false;
        self.pending_z = false;
        self.skip_trigger_text = false;
    }

    pub fn command_navigation_count(&mut self) -> u32 {
        let count = self.take_count();
        self.pending_g = false;
        self.pending_z = false;
        self.pending_jump = None;
        self.pending_yank = None;
        self.pending_visual_object = None;
        count
    }

    pub fn begin_input_batch(&mut self) {
        self.skip_trigger_text = false;
    }

    pub fn input(&mut self, event: &egui::Event) -> CopyAction {
        if let egui::Event::Text(text) = event {
            if let Some(pending) = self.pending_yank
                && let Some((forward, till)) = pending.jump
            {
                if self.skip_trigger_text {
                    self.skip_trigger_text = false;
                    return CopyAction::None;
                }
                let mut graphemes = text.graphemes(true);
                if let Some(target) = graphemes.next()
                    && graphemes.next().is_none()
                {
                    self.pending_yank = None;
                    let jump = Jump {
                        target: target.to_owned(),
                        forward,
                        till,
                    };
                    self.last_jump = Some(jump.clone());
                    let count = pending.count.saturating_mul(self.take_count());
                    return CopyAction::Yank(YankCommand::Jump { jump, count });
                }
                return CopyAction::None;
            }
            if let Some((forward, till)) = self.pending_jump {
                if self.skip_trigger_text {
                    self.skip_trigger_text = false;
                    return CopyAction::None;
                }
                let mut graphemes = text.graphemes(true);
                if let Some(target) = graphemes.next()
                    && graphemes.next().is_none()
                {
                    self.pending_jump = None;
                    let jump = Jump {
                        target: target.to_owned(),
                        forward,
                        till,
                    };
                    self.last_jump = Some(jump.clone());
                    return CopyAction::Jump {
                        jump,
                        repeat: false,
                        count: self.take_count(),
                    };
                }
            }
            if self.pending_jump.is_none()
                && self.pending_yank.is_none()
                && self.pending_visual_object.is_none()
                && (text == "/" || text == "?")
            {
                self.clear_pending_for_navigation();
                return CopyAction::BeginSearch {
                    forward: text == "/",
                };
            }
            return CopyAction::None;
        }
        let egui::Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } = event
        else {
            return CopyAction::None;
        };
        let (key, modifiers) = (*key, *modifiers);
        if key == egui::Key::B
            && modifiers.ctrl
            && !modifiers.shift
            && !modifiers.alt
            && !modifiers.mac_cmd
        {
            self.clear_pending_for_navigation();
            return CopyAction::OpenLineSplit;
        }
        if is_copy_modifier_key(key) {
            return CopyAction::None;
        }
        if key == egui::Key::Escape {
            if self.pending_z {
                self.pending_z = false;
                self.pending_count = None;
                return CopyAction::None;
            }
            if self.pending_visual_object.take().is_some() || self.pending_yank.take().is_some() {
                self.pending_count = None;
                return CopyAction::None;
            }
            if self.visual_anchor.take().is_some() {
                self.pending_count = None;
                self.pending_g = false;
                self.pending_jump = None;
                return CopyAction::Redraw;
            }
            if !self.search.matches.is_empty() {
                let cancel_pending = self.search.waiting;
                self.search = crate::document_search::SearchSession::default();
                return CopyAction::ClearSearch { cancel_pending };
            }
            return CopyAction::Exit;
        }
        if self.pending_z {
            self.pending_z = false;
            return copy_viewport_position_key(key, modifiers)
                .map_or(CopyAction::None, CopyAction::Position);
        }
        if let Some((object, prefix_count)) = self.pending_visual_object {
            if modifiers == egui::Modifiers::NONE
                && let Some(digit) = copy_digit(key)
                && self.add_count_digit(digit)
            {
                return CopyAction::None;
            }
            self.pending_visual_object = None;
            let count = prefix_count.saturating_mul(self.take_count());
            return copy_text_object_key(key, modifiers).map_or(CopyAction::None, |kind| {
                CopyAction::SelectObject {
                    object,
                    kind,
                    count,
                }
            });
        }
        if self.pending_jump.is_some() {
            self.skip_trigger_text = false;
            return CopyAction::None;
        }
        if let Some(mut pending) = self.pending_yank {
            if pending.jump.is_some() {
                return CopyAction::None;
            }
            if modifiers == egui::Modifiers::NONE
                && let Some(digit) = copy_digit(key)
                && self.add_count_digit(digit)
            {
                return CopyAction::None;
            }
            if pending.object.is_none()
                && modifiers == egui::Modifiers::NONE
                && matches!(key, egui::Key::I | egui::Key::A)
            {
                pending.object = Some(if key == egui::Key::I {
                    WordObject::Inner
                } else {
                    WordObject::Around
                });
                self.pending_yank = Some(pending);
                return CopyAction::None;
            }
            if pending.object.is_none()
                && let Some(jump) = copy_jump_prefix(key, modifiers)
            {
                pending.jump = Some(jump);
                self.pending_yank = Some(pending);
                self.skip_trigger_text = true;
                return CopyAction::None;
            }
            let pending_motion = if pending.object.is_none() {
                copy_motion_with_prefix(self, key, modifiers)
            } else {
                None
            };
            if self.pending_g {
                self.pending_yank = Some(pending);
                return CopyAction::None;
            }
            self.pending_yank = None;
            let count = pending.count.saturating_mul(self.take_count());
            let command = if pending.object.is_none()
                && modifiers == egui::Modifiers::NONE
                && matches!(key, egui::Key::Semicolon | egui::Key::Comma)
            {
                self.last_jump.clone().map(|mut jump| {
                    if key == egui::Key::Comma {
                        jump.forward = !jump.forward;
                    }
                    YankCommand::Jump { jump, count }
                })
            } else {
                match (pending.object, key, modifiers) {
                    (None, egui::Key::Y, egui::Modifiers::NONE) => Some(YankCommand::Lines(count)),
                    (None, egui::Key::Num4, egui::Modifiers::SHIFT) => {
                        Some(YankCommand::RowEnd(count))
                    }
                    (Some(object), key, modifiers) => {
                        copy_text_object_key(key, modifiers).map(|kind| YankCommand::Object {
                            object,
                            kind,
                            count,
                        })
                    }
                    _ => pending_motion.map(|motion| YankCommand::Motion {
                        motion,
                        count,
                        explicit_count: pending.count > 1 || count > 1,
                    }),
                }
            };
            return command.map_or(CopyAction::None, CopyAction::Yank);
        }
        if !modifiers.ctrl
            && !modifiers.alt
            && !modifiers.command
            && matches!(key, egui::Key::Slash | egui::Key::Questionmark)
        {
            self.clear_pending_for_navigation();
            return CopyAction::BeginSearch {
                forward: key != egui::Key::Questionmark && !modifiers.shift,
            };
        }
        if !self.pending_g
            && key == egui::Key::O
            && (modifiers == egui::Modifiers::NONE || modifiers == egui::Modifiers::SHIFT)
        {
            self.pending_count = None;
            let swapped = if modifiers.shift {
                self.swap_visual_horizontal_endpoint()
            } else {
                self.swap_visual_endpoint()
            };
            return if swapped {
                CopyAction::SwapEndpoint
            } else {
                CopyAction::None
            };
        }
        if !self.pending_g
            && let Some(down) = copy_scroll_direction(key, modifiers)
        {
            return CopyAction::Scroll {
                down,
                count: self.take_count(),
            };
        }
        if modifiers == egui::Modifiers::NONE && key == egui::Key::Z {
            self.pending_z = true;
            self.pending_g = false;
            self.pending_count = None;
            return CopyAction::None;
        }
        if modifiers == egui::Modifiers::NONE
            && matches!(key, egui::Key::I | egui::Key::A)
            && self.begin_visual_object(if key == egui::Key::I {
                WordObject::Inner
            } else {
                WordObject::Around
            })
        {
            return CopyAction::None;
        }
        if key == egui::Key::Q
            && modifiers.ctrl
            && !modifiers.shift
            && !modifiers.alt
            && !modifiers.mac_cmd
        {
            self.pending_count = None;
            self.pending_g = false;
            self.toggle_visual(VisualMode::Blockwise);
            return CopyAction::ToggleVisual;
        }
        if key == egui::Key::V
            && (modifiers == egui::Modifiers::NONE || modifiers == egui::Modifiers::SHIFT)
        {
            self.pending_count = None;
            self.pending_g = false;
            self.toggle_visual(if modifiers.shift {
                VisualMode::Linewise
            } else {
                VisualMode::Characterwise
            });
            return CopyAction::ToggleVisual;
        }
        if !self.pending_g && key == egui::Key::Y && modifiers.shift {
            let origin = self.visual_anchor.unwrap_or(self.cursor);
            self.pending_count = None;
            return CopyAction::HintYank { origin };
        }
        if modifiers == egui::Modifiers::NONE && key == egui::Key::Y {
            self.pending_g = false;
            if self.visual_anchor.is_some() {
                self.pending_count = None;
                return CopyAction::YankVisual;
            }
            self.pending_yank = Some(YankPending {
                count: self.take_count(),
                object: None,
                jump: None,
            });
            return CopyAction::None;
        }
        if modifiers == egui::Modifiers::NONE
            && let Some(digit) = copy_digit(key)
            && self.add_count_digit(digit)
        {
            return CopyAction::None;
        }
        if let Some((forward, till)) = copy_jump_prefix(key, modifiers) {
            self.pending_g = false;
            self.pending_jump = Some((forward, till));
            self.skip_trigger_text = true;
            return CopyAction::None;
        }
        if modifiers == egui::Modifiers::NONE
            && matches!(key, egui::Key::Semicolon | egui::Key::Comma)
        {
            self.pending_g = false;
            let count = self.take_count();
            return self.last_jump.clone().map_or(CopyAction::None, |mut jump| {
                if key == egui::Key::Comma {
                    jump.forward = !jump.forward;
                }
                CopyAction::Jump {
                    jump,
                    repeat: true,
                    count,
                }
            });
        }
        if modifiers.shift
            && !modifiers.ctrl
            && !modifiers.alt
            && !modifiers.mac_cmd
            && matches!(key, egui::Key::Num8 | egui::Key::Num3)
        {
            self.pending_g = false;
            return CopyAction::SearchWord {
                forward: key == egui::Key::Num8,
                count: self.take_count(),
            };
        }
        if !modifiers.ctrl && !modifiers.alt && !modifiers.mac_cmd && key == egui::Key::N {
            self.pending_g = false;
            return CopyAction::NavigateSearch {
                reverse: modifiers.shift,
                count: self.take_count(),
            };
        }
        let explicit_count = self.pending_count.is_some();
        let motion = copy_motion_with_prefix(self, key, modifiers);
        let Some(motion) = motion else {
            if !self.pending_g {
                self.pending_count = None;
            }
            return CopyAction::None;
        };
        CopyAction::Motion {
            motion,
            count: self.take_count(),
            explicit_count,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn application_command_navigation_uses_counts_without_a_hardcoded_shortcut() {
        let anchor = CellPosition { x: 3, y: 9 };
        let mut mode = CopyMode {
            pending_count: Some(3),
            pending_g: true,
            pending_z: true,
            visual_anchor: Some(anchor),
            ..Default::default()
        };
        assert_eq!(mode.command_navigation_count(), 3);
        assert!(!mode.pending_g && !mode.pending_z);
        assert_eq!(mode.visual_anchor, Some(anchor));
        assert_eq!(mode.command_navigation_count(), 1);
        let mut mode = CopyMode::default();
        assert!(matches!(
            mode.input(&egui::Event::Key {
                key: egui::Key::ArrowUp,
                physical_key: Some(egui::Key::ArrowUp),
                modifiers: egui::Modifiers::CTRL | egui::Modifiers::SHIFT,
                pressed: true,
                repeat: false
            }),
            CopyAction::None
        ));
    }

    #[test]
    fn copy_input_preserves_counts_and_pending_yank_motions() {
        use super::{CopyAction, CopyMode, Motion, YankCommand};
        let key = |key| egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let mut mode = CopyMode::default();
        assert!(matches!(
            mode.input(&key(egui::Key::Num2)),
            CopyAction::None
        ));
        assert!(matches!(mode.input(&key(egui::Key::Y)), CopyAction::None));
        assert!(matches!(
            mode.input(&key(egui::Key::Num3)),
            CopyAction::None
        ));
        assert!(matches!(
            mode.input(&key(egui::Key::W)),
            CopyAction::Yank(YankCommand::Motion {
                motion: Motion::ForwardWord,
                count: 6,
                explicit_count: true,
            })
        ));
        assert!(mode.pending_yank.is_none());
        assert!(mode.pending_count.is_none());
    }

    #[test]
    fn ctrl_b_opens_line_split_without_moving_cursor_or_leaking_pending_commands() {
        let mut mode = super::CopyMode::default();
        mode.cursor = super::CellPosition { x: 7, y: 42 };
        mode.pending_count = Some(3);
        mode.pending_g = true;
        let key = |mods| egui::Event::Key {
            key: egui::Key::B,
            physical_key: Some(egui::Key::B),
            pressed: true,
            repeat: false,
            modifiers: mods,
        };
        assert!(matches!(
            mode.input(&key(egui::Modifiers::CTRL)),
            super::CopyAction::OpenLineSplit
        ));
        assert_eq!(mode.cursor, super::CellPosition { x: 7, y: 42 });
        // egui-winit sets the platform-neutral command bit alongside Ctrl on
        // Windows/Linux. It is an alias, not an additional held modifier.
        assert!(matches!(
            mode.input(&key(egui::Modifiers {
                ctrl: true,
                command: true,
                ..Default::default()
            })),
            super::CopyAction::OpenLineSplit
        ));
        assert!(mode.pending_count.is_none() && !mode.pending_g);
        assert!(matches!(
            mode.input(&key(egui::Modifiers::NONE)),
            super::CopyAction::Motion {
                motion: super::Motion::BackwardWord,
                ..
            }
        ));
        assert!(!matches!(
            mode.input(&key(egui::Modifiers::CTRL | egui::Modifiers::SHIFT)),
            super::CopyAction::OpenLineSplit
        ));
    }

    #[test]
    fn copy_input_waits_for_jump_text_after_the_prefix_key() {
        use super::{CopyAction, CopyMode};
        let key = egui::Event::Key {
            key: egui::Key::F,
            physical_key: Some(egui::Key::F),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let mut mode = CopyMode::default();
        assert!(matches!(mode.input(&key), CopyAction::None));
        assert!(mode.skip_trigger_text);
        assert!(matches!(
            mode.input(&egui::Event::Text("f".into())),
            CopyAction::None
        ));
        assert!(!mode.skip_trigger_text);
        assert!(
            matches!(mode.input(&egui::Event::Text("x".into())), CopyAction::Jump { jump, count: 1, .. } if jump.target == "x")
        );
    }

    #[test]
    fn copy_input_keeps_prefixes_and_escape_local_to_the_mode() {
        use super::{CopyAction, CopyMode, Motion, ViewportPosition, VisualMode};
        let key = |key| egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let mut mode = CopyMode::default();
        assert!(matches!(mode.input(&key(egui::Key::G)), CopyAction::None));
        assert!(matches!(
            mode.input(&key(egui::Key::G)),
            CopyAction::Motion {
                motion: Motion::ScrollbackTop,
                count: 1,
                ..
            }
        ));
        assert!(matches!(mode.input(&key(egui::Key::Z)), CopyAction::None));
        assert!(matches!(
            mode.input(&key(egui::Key::T)),
            CopyAction::Position(ViewportPosition::Top)
        ));
        mode.toggle_visual(VisualMode::Characterwise);
        assert!(matches!(
            mode.input(&key(egui::Key::Escape)),
            CopyAction::Redraw
        ));
        assert!(matches!(
            mode.input(&key(egui::Key::Escape)),
            CopyAction::Exit
        ));
    }

    use super::*;

    #[test]
    fn block_selection_normalizes_corners_and_swaps_endpoints() {
        let mut mode = copy_at(7, 50, 100);
        mode.toggle_visual(VisualMode::Blockwise);
        mode.moved(Motion::CharacterJump, CellPosition { x: 3, y: 55 });
        let rectangle = CellRange {
            start: CellPosition { x: 3, y: 50 },
            end: CellPosition { x: 7, y: 55 },
        };
        assert_eq!(mode.visual_rectangle_to(mode.cursor), Some(rectangle));
        assert!(mode.swap_visual_horizontal_endpoint());
        assert_eq!(mode.cursor, CellPosition { x: 7, y: 55 });
        assert_eq!(mode.visual_anchor, Some(CellPosition { x: 3, y: 50 }));
        assert_eq!(mode.visual_rectangle_to(mode.cursor), Some(rectangle));
        assert!(mode.swap_visual_endpoint());
        assert_eq!(mode.cursor, CellPosition { x: 3, y: 50 });
        assert_eq!(mode.visual_rectangle_to(mode.cursor), Some(rectangle));
        mode.toggle_visual(VisualMode::Linewise);
        assert_eq!(mode.visual_anchor, Some(CellPosition { x: 7, y: 55 }));
        mode.toggle_visual(VisualMode::Blockwise);
        mode.toggle_visual(VisualMode::Blockwise);
        assert!(mode.visual_rectangle_to(mode.cursor).is_none());
    }

    #[test]
    fn rectangle_decorations_are_clipped_and_ordered_newest_first() {
        let rectangle = CellRange {
            start: CellPosition { x: 2, y: 0 },
            end: CellPosition { x: 5, y: 1_000_000 },
        };
        let ranges = rectangle_row_ranges(rectangle, 50, 3);
        assert_eq!(ranges.len(), 3);
        for (range, y) in ranges.iter().zip([52, 51, 50]) {
            assert_eq!(
                *range,
                CellRange {
                    start: CellPosition { x: 2, y },
                    end: CellPosition { x: 5, y },
                }
            );
        }
        assert!(rectangle_row_ranges(rectangle, 50, 0).is_empty());
        assert!(rectangle_row_ranges(rectangle, 1_000_001, 3).is_empty());
    }

    #[test]
    fn rectangle_yank_flash_retains_shape_until_replaced_or_expired() {
        let mut mode = copy_at(2, 1, 10);
        let range = CellRange {
            start: CellPosition { x: 1, y: 1 },
            end: CellPosition { x: 5, y: 3 },
        };
        let now = Instant::now();
        mode.highlight_rectangle_yank(range, now);
        assert!(mode.yank_highlight_is_rectangle());
        assert_eq!(mode.yank_highlight_range(), Some(range));
        mode.highlight_yank(range, now);
        assert!(!mode.yank_highlight_is_rectangle());
        mode.highlight_rectangle_yank(range, now);
        assert!(mode.clear_expired_yank_highlight(now + YANK_HIGHLIGHT_DURATION));
        assert!(!mode.yank_highlight_is_rectangle());
    }

    #[test]
    fn visual_endpoint_swap_preserves_range_and_updates_preferred_column() {
        for visual in [VisualMode::Characterwise, VisualMode::Linewise] {
            let mut mode = copy_at(3, 50, 100);
            assert!(!mode.swap_visual_endpoint());
            mode.toggle_visual(visual);
            mode.moved(Motion::Right, CellPosition { x: 7, y: 55 });
            let range = mode.visual_range_to(mode.cursor);
            assert!(mode.swap_visual_endpoint());
            assert_eq!(mode.cursor, CellPosition { x: 3, y: 50 });
            assert_eq!(mode.visual_anchor, Some(CellPosition { x: 7, y: 55 }));
            assert_eq!(mode.visual_range_to(mode.cursor), range);
            assert_eq!(mode.visual_mode, visual);
            assert_eq!(mode.move_to(Motion::Down, None).unwrap().0.x, 3);
            assert!(mode.swap_visual_endpoint());
            assert_eq!(mode.cursor, CellPosition { x: 7, y: 55 });
        }
    }

    #[test]
    fn viewport_scrolling_counts_padding_boundaries_and_selection() {
        let mut mode = copy_at(3, 50, 100);
        mode.toggle_visual(VisualMode::Characterwise);
        let view = View {
            columns: mode.columns,
            total_rows: 100,
            viewport_top: 40,
            viewport_rows: 20,
            ..Default::default()
        };
        assert_eq!(
            mode.scrolled_viewport_target(true, 1, view),
            Some((41, mode.cursor))
        );
        assert_eq!(
            mode.scrolled_viewport_target(false, 1, view),
            Some((39, mode.cursor))
        );
        assert_eq!(
            mode.scrolled_viewport_target(true, 10, view),
            Some((50, CellPosition { x: 3, y: 52 }))
        );
        assert_eq!(
            mode.scrolled_viewport_target(false, 10, view),
            Some((30, CellPosition { x: 3, y: 47 }))
        );
        assert_eq!(
            mode.scrolled_viewport_target(true, u32::MAX, view),
            Some((80, CellPosition { x: 3, y: 82 }))
        );
        assert_eq!(
            mode.scrolled_viewport_target(false, u32::MAX, view),
            Some((0, CellPosition { x: 3, y: 17 }))
        );
        assert_eq!(mode.visual_anchor, Some(mode.cursor));
        assert_eq!(
            mode.scrolled_viewport_target(
                false,
                1,
                View {
                    viewport_top: 0,
                    ..view
                }
            ),
            Some((0, mode.cursor))
        );
        assert_eq!(
            mode.scrolled_viewport_target(
                true,
                1,
                View {
                    viewport_top: 80,
                    ..view
                }
            ),
            Some((80, mode.cursor))
        );
        assert_eq!(
            mode.scrolled_viewport_target(
                true,
                1,
                View {
                    viewport_rows: 1,
                    viewport_top: 49,
                    ..view
                }
            ),
            Some((50, mode.cursor))
        );
        assert!(
            mode.scrolled_viewport_target(
                true,
                1,
                View {
                    viewport_rows: 0,
                    ..view
                }
            )
            .is_none()
        );
    }

    #[test]
    fn viewport_padding_defaults_to_two_and_accepts_zero_or_custom_values() {
        let mut mode = copy_at(3, 50, 100);
        let view = View {
            columns: mode.columns,
            total_rows: 100,
            viewport_rows: 20,
            ..Default::default()
        };
        assert_eq!(mode.scroll_padding(), 2);
        assert_eq!(
            mode.positioned_viewport_top(ViewportPosition::Top, view),
            Some(48)
        );
        mode.set_scroll_padding(0);
        assert_eq!(
            mode.positioned_viewport_top(ViewportPosition::Top, view),
            Some(50)
        );
        mode.set_scroll_padding(4);
        assert_eq!(
            mode.positioned_viewport_top(ViewportPosition::Top, view),
            Some(46)
        );
    }

    #[test]
    fn viewport_positioning_preserves_cursor_and_selection_and_clamps() {
        let mut mode = copy_at(3, 50, 100);
        mode.toggle_visual(VisualMode::Characterwise);
        mode.moved(Motion::Down, CellPosition { x: 3, y: 52 });
        let cursor = mode.cursor;
        let anchor = mode.visual_anchor;
        let view = View {
            columns: mode.columns,
            total_rows: 100,
            viewport_rows: 20,
            ..Default::default()
        };
        assert_eq!(
            mode.positioned_viewport_top(ViewportPosition::Top, view),
            Some(50)
        );
        assert_eq!(
            mode.positioned_viewport_top(ViewportPosition::Center, view),
            Some(42)
        );
        assert_eq!(
            mode.positioned_viewport_top(ViewportPosition::Bottom, view),
            Some(35)
        );
        assert_eq!(mode.cursor, cursor);
        assert_eq!(mode.visual_anchor, anchor);
        mode.cursor.y = 2;
        assert_eq!(
            mode.positioned_viewport_top(ViewportPosition::Center, view),
            Some(0)
        );
        assert_eq!(
            mode.positioned_viewport_top(ViewportPosition::Bottom, view),
            Some(0)
        );
        mode.cursor.y = 99;
        assert_eq!(
            mode.positioned_viewport_top(ViewportPosition::Top, view),
            Some(80)
        );
        assert_eq!(
            mode.positioned_viewport_top(ViewportPosition::Center, view),
            Some(80)
        );
        assert_eq!(
            mode.positioned_viewport_top(ViewportPosition::Bottom, view),
            Some(80)
        );
        let empty_view = View {
            viewport_rows: 0,
            ..view
        };
        assert_eq!(
            mode.positioned_viewport_top(ViewportPosition::Top, empty_view),
            None
        );
        let mismatched = View {
            total_rows: 99,
            ..view
        };
        assert_eq!(
            mode.positioned_viewport_top(ViewportPosition::Top, mismatched),
            None
        );
        let short_view = View {
            total_rows: 5,
            viewport_rows: 20,
            ..view
        };
        let mode = copy_at(0, 2, 5);
        assert_eq!(
            mode.positioned_viewport_top(ViewportPosition::Top, short_view),
            Some(0)
        );
    }

    #[test]
    fn visual_word_objects_replace_selection_and_keep_motions_available() {
        for (object, end, trim) in [(WordObject::Inner, 2, true), (WordObject::Around, 3, false)] {
            let mut mode = copy_at(1, 0, 1);
            mode.toggle_visual(VisualMode::Linewise);
            assert!(mode.begin_visual_object(object));
            let (object, count) = mode.pending_visual_object.unwrap();
            let range = mode
                .word_object_range(count, object, false, |_| Some(row("foo bar", false)))
                .unwrap();
            mode.select_visual_object(range, trim);
            assert_eq!(mode.visual_mode, VisualMode::Characterwise);
            assert_eq!(mode.visual_anchor, Some(CellPosition { x: 0, y: 0 }));
            assert_eq!(mode.cursor, CellPosition { x: end, y: 0 });
            assert_eq!(mode.visual_range_to(mode.cursor), Some(range));
            assert_eq!(mode.visual_trim, trim);
            assert!(mode.pending_visual_object.is_none());
            // Subsequent motion extends the selection without losing its anchor.
            mode.moved(Motion::Right, CellPosition { x: end + 1, y: 0 });
            assert_eq!(mode.visual_range_to(mode.cursor).unwrap().end.x, end + 1);
        }
    }

    #[test]
    fn counted_visual_objects_cross_soft_wraps_and_reset_pending_state() {
        let mut mode = copy_at(1, 0, 2);
        assert!(!mode.begin_visual_object(WordObject::Inner));
        mode.toggle_visual(VisualMode::Characterwise);
        mode.add_count_digit(2);
        assert!(mode.begin_visual_object(WordObject::Inner));
        let (object, count) = mode.pending_visual_object.unwrap();
        assert_eq!(count, 2);
        assert!(mode.pending_count.is_none());
        let rows = [row("foo", true), row("bar baz qux", false)];
        let range = mode
            .word_object_range(count, object, false, |y| rows.get(y as usize).cloned())
            .unwrap();
        mode.select_visual_object(range, true);
        assert_eq!(range.start, CellPosition { x: 0, y: 0 });
        assert_eq!(range.end, CellPosition { x: 6, y: 1 });
        assert_eq!(mode.cursor, range.end);
        assert_eq!(mode.visual_range_to(mode.cursor), Some(range));
        mode.begin_visual_object(WordObject::Around);
        mode.toggle_visual(VisualMode::Linewise);
        assert!(mode.pending_visual_object.is_none());
    }

    #[test]
    fn evaluation_distinguishes_partial_motion_and_invalid_snapshot() {
        let mode = copy_at(0, 0, 1);
        let rows = [row("foo bar", false)];
        let read = |y: u64| rows.get(y as usize).cloned();
        let resolve = |cell, _| Some(cell);
        let MotionEvaluation::Partial(result) =
            mode.evaluate_motion(Motion::ForwardWord, 3, true, None, read, resolve)
        else {
            panic!("count exhaustion should preserve partial movement");
        };
        assert_eq!(result.target, CellPosition { x: 4, y: 0 });
        assert_eq!(result.endpoint, Endpoint::Exclusive);
        assert_eq!(mode.cursor, CellPosition { x: 0, y: 0 });
        assert!(matches!(
            mode.evaluate_motion(Motion::BackwardWord, 1, false, None, read, resolve,),
            MotionEvaluation::NoMovement
        ));
        assert!(matches!(
            mode.evaluate_motion(Motion::ForwardWord, 1, false, None, read, |_, _| None,),
            MotionEvaluation::InvalidSnapshot
        ));
    }

    #[test]
    fn counted_horizontal_motion_resolves_each_wide_cell() {
        let mut mode = copy_at(0, 0, 1);
        mode.columns = 6;
        let mut requested = Vec::new();
        let result = mode.evaluate_motion(
            Motion::Right,
            2,
            true,
            None,
            |_| Some(row("abcdef", false)),
            |mut cell, right| {
                requested.push((cell.x, right));
                if cell.x == 1 {
                    cell.x = 2;
                }
                Some(cell)
            },
        );
        let MotionEvaluation::Complete(result) = result else {
            panic!("expected a complete motion");
        };
        assert_eq!(result.target, CellPosition { x: 3, y: 0 });
        assert_eq!(requested, vec![(1, true), (3, true)]);
        assert_eq!(mode.cursor.x, 0);
    }

    #[test]
    fn counted_word_motion_reuses_rows_and_produces_yank_metadata() {
        let mode = copy_at(0, 0, 1);
        let mut reads = 0;
        let result = mode.evaluate_motion(
            Motion::ForwardWordEnd,
            2,
            true,
            None,
            |_| {
                reads += 1;
                Some(row("foo bar baz", false))
            },
            |cell, _| Some(cell),
        );
        let MotionEvaluation::Complete(result) = result else {
            panic!("expected a complete motion");
        };
        assert_eq!(result.target.x, 6);
        assert_eq!(result.kind, RangeKind::Characterwise);
        assert_eq!(result.endpoint, Endpoint::Inclusive);
        assert_eq!(reads, 1);
        let (range, linewise) = mode
            .yank_motion_range(result, |_| Some(row("foo bar baz", false)))
            .unwrap();
        assert_eq!(range.end.x, 6);
        assert!(!linewise);
    }

    #[test]
    fn word_scanning_distinguishes_soft_wraps_and_hard_breaks() {
        let rows = [row("foo", true), row("bar", false), row("baz", false)];
        let read = |y: u64| rows.get(y as usize).cloned();
        assert_eq!(
            copy_at(0, 0, 3).text_target(Motion::ForwardWord, read),
            Some(CellPosition { x: 0, y: 2 })
        );
        assert_eq!(
            copy_at(1, 1, 3).text_target(Motion::BackwardWord, read),
            Some(CellPosition { x: 0, y: 0 })
        );
        assert_eq!(
            copy_at(0, 2, 3).text_target(Motion::PreviousWordEnd, read),
            Some(CellPosition { x: 2, y: 1 })
        );
        let rows = [row("foo", false), row("", false), row("bar", false)];
        let read = |y: u64| rows.get(y as usize).cloned();
        assert_eq!(
            copy_at(0, 0, 3).text_target(Motion::ForwardWord, read),
            Some(CellPosition { x: 0, y: 2 })
        );
        assert_eq!(
            copy_at(0, 2, 3).text_target(Motion::BackwardWord, read),
            Some(CellPosition { x: 0, y: 0 })
        );
    }

    #[test]
    fn newer_yank_replaces_highlight_and_outlives_old_expiry() {
        let mut mode = CopyMode::default();
        let first = CellRange {
            start: CellPosition { x: 1, y: 2 },
            end: CellPosition { x: 3, y: 2 },
        };
        let second = CellRange {
            start: CellPosition { x: 5, y: 6 },
            end: CellPosition { x: 7, y: 6 },
        };
        let started = Instant::now();
        mode.highlight_yank(first, started);
        mode.highlight_yank(second, started + Duration::from_millis(100));
        assert!(!mode.clear_expired_yank_highlight(started + YANK_HIGHLIGHT_DURATION));
        assert_eq!(mode.yank_highlight_range(), Some(second));
        assert_eq!(
            mode.yank_highlight_remaining(started + YANK_HIGHLIGHT_DURATION),
            Some(Duration::from_millis(100))
        );
        assert!(mode.clear_expired_yank_highlight(started + Duration::from_millis(350)));
        assert!(mode.yank_highlight_range().is_none());
    }

    #[test]
    fn visual_range_includes_anchor_and_orders_reverse_motion() {
        let mut mode = CopyMode {
            cursor: CellPosition { x: 4, y: 8 },
            ..Default::default()
        };
        mode.toggle_visual(VisualMode::Characterwise);
        assert_eq!(
            mode.visual_range_to(mode.cursor).unwrap().start,
            mode.cursor
        );
        assert_eq!(mode.visual_range_to(mode.cursor).unwrap().end, mode.cursor);
        mode.moved(Motion::Up, CellPosition { x: 2, y: 5 });
        assert_eq!(
            mode.visual_range_to(mode.cursor).unwrap().start,
            mode.cursor
        );
        assert_eq!(
            mode.visual_range_to(mode.cursor).unwrap().end,
            CellPosition { x: 4, y: 8 }
        );
        mode.toggle_visual(VisualMode::Characterwise);
        assert!(mode.visual_range_to(mode.cursor).is_none());
    }

    #[test]
    fn linewise_visual_range_expands_wrapped_lines_in_both_directions() {
        let rows = [
            row("before", false),
            row("first", true),
            row("last", false),
            row("middle", false),
            row("next", true),
            row("end", false),
        ];
        let mut mode = copy_at(3, 2, rows.len() as u64);
        mode.toggle_visual(VisualMode::Linewise);
        let range = mode
            .linewise_visual_range_to(CellPosition { x: 2, y: 4 }, |y| {
                rows.get(y as usize).cloned()
            })
            .unwrap();
        assert_eq!(range.start, CellPosition { x: 0, y: 1 });
        assert_eq!(range.end, CellPosition { x: 19, y: 5 });

        let range = mode
            .linewise_visual_range_to(CellPosition { x: 2, y: 0 }, |y| {
                rows.get(y as usize).cloned()
            })
            .unwrap();
        assert_eq!(range.start, CellPosition { x: 0, y: 0 });
        assert_eq!(range.end, CellPosition { x: 19, y: 2 });

        mode.toggle_visual(VisualMode::Characterwise);
        assert_eq!(mode.visual_anchor, Some(CellPosition { x: 3, y: 2 }));
        mode.toggle_visual(VisualMode::Characterwise);
        assert!(mode.visual_anchor.is_none());
    }

    #[test]
    fn hint_yank_range_includes_both_logical_lines_without_moving_cursor() {
        let rows = [
            row("older", true),
            row("continuation", false),
            row("middle", false),
            row("origin", true),
            row("wrapped", false),
        ];
        let mode = copy_at(3, 3, rows.len() as u64);
        let origin = mode.cursor;
        let range = mode
            .linewise_range_between(origin, CellPosition { x: 2, y: 0 }, |y| {
                rows.get(y as usize).cloned()
            })
            .unwrap();
        assert_eq!(range.start, CellPosition { x: 0, y: 0 });
        assert_eq!(range.end, CellPosition { x: 19, y: 4 });
        assert_eq!(mode.cursor, origin);
    }

    fn row(text: &str, wraps_to_next: bool) -> TextRow {
        TextRow {
            text: text.into(),
            columns: (0..text.len() as u32).collect(),
            wraps_to_next,
        }
    }

    fn copy_at(x: u32, y: u64, rows: u64) -> CopyMode {
        let mut result = CopyMode::default();
        result.start(
            1,
            View {
                cursor: CellPosition { x, y },
                columns: 20,
                total_rows: rows,
                ..Default::default()
            },
        );
        result
    }

    #[test]
    fn copy_search_replays_queued_moves_and_rejects_stale_or_cancelled_results() {
        let matches = || {
            [8, 4, 1]
                .map(|y| {
                    let cell = CellPosition { x: 0, y };
                    CellRange {
                        start: cell,
                        end: cell,
                    }
                })
                .to_vec()
        };
        let mut mode = copy_at(0, 1, 10);
        mode.begin_search(7, true, 1);
        assert!(mode.navigate_search(false, 1).is_none());
        assert!(mode.navigate_search(true, 1).is_none());
        assert!(mode.finish_search(6, matches()).is_none());
        assert!(mode.search_accepts_result(7));
        let target = mode.finish_search(7, matches()).unwrap();
        assert_eq!(target, CellPosition { x: 0, y: 4 });
        mode.cursor = target;
        assert_eq!(
            mode.navigate_search(true, 1),
            Some(CellPosition { x: 0, y: 1 })
        );
        assert_eq!(
            mode.navigate_search(false, 2),
            Some(CellPosition { x: 0, y: 1 })
        );
        assert!(mode.finish_search(7, matches()).is_none());
        mode.begin_search(8, false, 1);
        mode.navigate_search(false, 2);
        mode.cancel_pending_search();
        assert!(mode.finish_search(8, matches()).is_none());
        assert!(mode.search_matches().is_empty());
        mode.begin_search(9, true, 1);
        assert_eq!(mode.finish_search(9, Vec::new()), Some(mode.cursor));
    }

    #[test]
    fn copy_escape_clears_adopted_search_after_leaving_visual_mode() {
        let escape = egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: Some(egui::Key::Escape),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        };
        let mut mode = copy_at(0, 4, 10);
        let cell = CellPosition { x: 0, y: 1 };
        mode.adopt_search(
            3,
            vec![CellRange {
                start: cell,
                end: cell,
            }],
        );
        assert_eq!(mode.navigate_search(false, 1), Some(cell));
        mode.toggle_visual(VisualMode::Characterwise);
        assert!(matches!(mode.input(&escape), CopyAction::Redraw));
        assert_eq!(mode.search_matches().len(), 1);
        assert!(matches!(
            mode.input(&escape),
            CopyAction::ClearSearch {
                cancel_pending: false
            }
        ));
        assert!(mode.search_matches().is_empty());
        assert!(mode.navigate_search(false, 1).is_none());
        assert!(matches!(mode.input(&escape), CopyAction::Exit));
    }

    #[test]
    fn copy_visual_selection_reports_ranges_and_yank_options_for_each_mode() {
        let rows = [row("foo", true), row(" bar", false)];
        let mut mode = copy_at(2, 1, 2);
        mode.visual_anchor = Some(CellPosition { x: 1, y: 0 });
        for visual_mode in [
            VisualMode::Characterwise,
            VisualMode::Linewise,
            VisualMode::Blockwise,
        ] {
            mode.visual_mode = visual_mode;
            let selection = mode
                .visual_selection_to(mode.cursor, |y| rows.get(y as usize).cloned())
                .unwrap();
            assert_eq!(selection.linewise, visual_mode == VisualMode::Linewise);
            assert_eq!(selection.rectangle, visual_mode == VisualMode::Blockwise);
            assert_eq!(selection.trim, visual_mode != VisualMode::Blockwise);
            let expected = if visual_mode == VisualMode::Linewise {
                CellRange {
                    start: CellPosition { x: 0, y: 0 },
                    end: CellPosition { x: 19, y: 1 },
                }
            } else {
                CellRange {
                    start: CellPosition { x: 1, y: 0 },
                    end: CellPosition { x: 2, y: 1 },
                }
            };
            assert_eq!(selection.range, expected);
        }
        mode.visual_anchor = None;
        assert!(mode.visual_selection_to(mode.cursor, |_| None).is_none());
    }

    #[test]
    fn copy_text_object_selection_preserves_word_quote_delimiter_and_command_yank_rules() {
        for (kind, text, x, expected_start, expected_end) in [
            (CopyTextObject::Word(false), "  foo  ", 3, 2, 4),
            (CopyTextObject::Quote('"'), "  \"foo\" ", 3, 3, 5),
            (CopyTextObject::Delimiter('(', ')'), " (foo) ", 3, 2, 4),
        ] {
            let mode = copy_at(x, 0, 1);
            let selection = mode
                .text_object_selection(1, WordObject::Inner, kind, &[], |_| Some(row(text, false)))
                .unwrap();
            assert_eq!(selection.range.start.x, expected_start);
            assert_eq!(selection.range.end.x, expected_end);
            assert!(!selection.linewise);
            assert!(!selection.rectangle);
            assert_eq!(selection.trim, matches!(kind, CopyTextObject::Word(_)));
            let around = mode
                .text_object_selection(1, WordObject::Around, kind, &[], |_| Some(row(text, false)))
                .unwrap();
            assert!(!around.trim);
        }
        let mode = copy_at(0, 1, 6);
        let command = mode
            .text_object_selection(
                1,
                WordObject::Inner,
                CopyTextObject::Command,
                &[0, 3],
                |_| None,
            )
            .unwrap();
        assert!(command.linewise && command.trim && !command.rectangle);
        assert!(
            mode.text_object_selection(1, WordObject::Inner, CopyTextObject::Command, &[], |_| {
                None
            })
            .is_none()
        );
    }

    #[test]
    fn yank_word_objects_distinguish_inner_around_and_punctuation() {
        let row = row("foo.bar  baz", false);
        let mode = copy_at(5, 0, 1);
        let read = |_| Some(row.clone());
        assert_eq!(
            mode.word_object_range(1, WordObject::Inner, false, read),
            Some(CellRange {
                start: CellPosition { x: 4, y: 0 },
                end: CellPosition { x: 6, y: 0 },
            })
        );
        assert_eq!(
            mode.word_object_range(1, WordObject::Around, false, read),
            Some(CellRange {
                start: CellPosition { x: 4, y: 0 },
                end: CellPosition { x: 8, y: 0 },
            })
        );
        let mode = copy_at(3, 0, 1);
        assert_eq!(
            mode.word_object_range(1, WordObject::Inner, false, read),
            Some(CellRange {
                start: CellPosition { x: 3, y: 0 },
                end: CellPosition { x: 3, y: 0 },
            })
        );
        let mode = copy_at(7, 0, 1);
        assert_eq!(
            mode.word_object_range(1, WordObject::Around, false, read),
            Some(CellRange {
                start: CellPosition { x: 7, y: 0 },
                end: CellPosition { x: 11, y: 0 },
            })
        );
    }

    #[test]
    fn yank_word_object_spans_soft_wrap_and_counted_words() {
        let rows = [row("foo", true), row("bar baz", false)];
        let mode = copy_at(1, 1, 2);
        let read = |y: u64| rows.get(y as usize).cloned();
        assert_eq!(
            mode.word_object_range(1, WordObject::Inner, false, read),
            Some(CellRange {
                start: CellPosition { x: 0, y: 0 },
                end: CellPosition { x: 2, y: 1 },
            })
        );
        assert_eq!(
            mode.word_object_range(2, WordObject::Inner, false, read),
            Some(CellRange {
                start: CellPosition { x: 0, y: 0 },
                end: CellPosition { x: 6, y: 1 },
            })
        );
    }

    #[test]
    fn yank_word_object_uses_cell_columns_for_multibyte_text() {
        let row = TextRow {
            text: "a.界 b".into(),
            columns: vec![0, 1, 2, 2, 2, 4, 5],
            wraps_to_next: false,
        };
        let mode = copy_at(2, 0, 1);
        let range = mode
            .word_object_range(1, WordObject::Around, false, |_| Some(row.clone()))
            .unwrap();
        assert_eq!(range.start, CellPosition { x: 2, y: 0 });
        assert_eq!(range.end, CellPosition { x: 4, y: 0 });
    }

    #[test]
    fn yank_big_word_includes_punctuation() {
        let row = row("foo.bar baz", false);
        let mode = copy_at(4, 0, 1);
        let read = |_| Some(row.clone());
        assert_eq!(
            mode.word_object_range(1, WordObject::Inner, true, read),
            Some(CellRange {
                start: CellPosition { x: 0, y: 0 },
                end: CellPosition { x: 6, y: 0 },
            })
        );
        assert_eq!(
            mode.word_object_range(1, WordObject::Around, true, read),
            Some(CellRange {
                start: CellPosition { x: 0, y: 0 },
                end: CellPosition { x: 7, y: 0 },
            })
        );
    }

    #[test]
    fn yank_quotes_and_nested_brackets_map_to_cells() {
        let rows = [row("prefix (a [界]", true), row(" b) 'one \\' two'", false)];
        let mode = copy_at(10, 0, 2);
        let read = |y: u64| rows.get(y as usize).cloned();
        assert_eq!(
            mode.delimiter_object_range(WordObject::Around, '(', ')', read),
            Some(CellRange {
                start: CellPosition { x: 7, y: 0 },
                end: CellPosition { x: 2, y: 1 },
            })
        );
        let mode = copy_at(9, 1, 2);
        assert_eq!(
            mode.quote_object_range(WordObject::Around, '\'', read),
            Some(CellRange {
                start: CellPosition { x: 4, y: 1 },
                end: CellPosition { x: 15, y: 1 },
            })
        );
    }

    #[test]
    fn yank_word_motions_are_exclusive_and_line_motions_use_logical_lines() {
        let rows = [row("foo bar", false), row("wrap", true), row("end", false)];
        let read = |y: u64| rows.get(y as usize).cloned();
        let mode = copy_at(1, 0, 3);
        assert_eq!(
            mode.yank_motion_range(
                Motion::ForwardWord.result(CellPosition { x: 4, y: 0 }),
                read
            ),
            Some((
                CellRange {
                    start: CellPosition { x: 1, y: 0 },
                    end: CellPosition { x: 3, y: 0 },
                },
                false
            ))
        );
        assert_eq!(
            mode.yank_motion_range(Motion::Down.result(CellPosition { x: 1, y: 2 }), read),
            Some((
                CellRange {
                    start: CellPosition { x: 0, y: 0 },
                    end: CellPosition { x: 19, y: 2 },
                },
                true
            ))
        );
    }

    #[test]
    fn yank_command_objects_use_regex_prompt_boundaries() {
        let rows = [
            row("PS C:\\> first", false),
            row("output", false),
            row("12:34> second", false),
            row("more", false),
        ];
        let mode = copy_at(2, 1, 4);
        let boundaries =
            super::find_prompt_boundaries(rows.len() as u64, |y| rows.get(y as usize).cloned())
                .unwrap();
        assert_eq!(boundaries, [0, 2]);
        assert_eq!(
            mode.command_object_range(WordObject::Inner, 1, &boundaries),
            Some(CellRange {
                start: CellPosition { x: 0, y: 1 },
                end: CellPosition { x: 19, y: 1 },
            })
        );
        assert_eq!(
            mode.command_object_range(WordObject::Around, 2, &boundaries),
            Some(CellRange {
                start: CellPosition { x: 0, y: 0 },
                end: CellPosition { x: 19, y: 3 },
            })
        );
    }

    #[test]
    fn command_navigation_skips_current_prompt_counts_and_stops_at_edges() {
        let mode = copy_at(7, 10, 40);
        let boundaries = [0, 10, 20, 30];
        assert_eq!(
            mode.command_target(false, 1, &boundaries),
            Some(CellPosition { x: 0, y: 0 })
        );
        assert_eq!(
            mode.command_target(true, 1, &boundaries),
            Some(CellPosition { x: 0, y: 20 })
        );
        assert_eq!(
            mode.command_target(true, 2, &boundaries),
            Some(CellPosition { x: 0, y: 30 })
        );
        assert_eq!(mode.command_target(false, 2, &boundaries), None);
        assert_eq!(mode.command_target(true, 3, &boundaries), None);
        assert_eq!(mode.command_target(true, 1, &[]), None);
        let mode = copy_at(7, 15, 40);
        assert_eq!(
            mode.command_target(false, 1, &boundaries),
            Some(CellPosition { x: 0, y: 10 })
        );
    }

    #[test]
    fn prompt_patterns_match_logical_lines_without_osc_markers() {
        let rows = [
            row("ordinary output", false),
            row("› command", true),
            row(" continued input", false),
            row("────────────────────", false),
        ];
        assert_eq!(
            super::find_prompt_boundaries(rows.len() as u64, |y| { rows.get(y as usize).cloned() }),
            Some(vec![1, 3])
        );
    }

    #[test]
    fn yank_line_ranges_and_dollar_count_logical_lines() {
        let rows = [
            row("before", false),
            row("wrapped", true),
            row("end", false),
            row("next", false),
        ];
        let mode = copy_at(1, 1, rows.len() as u64);
        let read = |y: u64| rows.get(y as usize).cloned();
        assert_eq!(
            mode.yank_lines_range(2, read),
            Some(CellRange {
                start: CellPosition { x: 0, y: 1 },
                end: CellPosition { x: 19, y: 3 },
            })
        );
        assert_eq!(
            mode.yank_to_row_end_range(1, read),
            Some(CellRange {
                start: CellPosition { x: 1, y: 1 },
                end: CellPosition { x: 2, y: 2 },
            })
        );
    }

    fn motion_cell(mode: &CopyMode, motion: Motion, count: u32, rows: &[TextRow]) -> CellPosition {
        match mode.evaluate_motion(
            motion,
            count,
            count > 1,
            None,
            |y| rows.get(y as usize).cloned(),
            |cell, _| Some(cell),
        ) {
            MotionEvaluation::Complete(result) | MotionEvaluation::Partial(result) => result.target,
            MotionEvaluation::NoMovement => mode.cursor,
            MotionEvaluation::InvalidSnapshot => panic!("invalid test document"),
        }
    }

    #[test]
    fn logical_horizontal_and_zero_motions_cross_only_soft_wraps() {
        let rows = [row("abcd", true), row("efgh", false), row("ijkl", false)];
        let mut mode = copy_at(0, 1, 3);
        mode.columns = 4;
        assert_eq!(
            motion_cell(&mode, Motion::Left, 1, &rows),
            CellPosition { x: 3, y: 0 }
        );
        assert_eq!(
            motion_cell(&mode, Motion::RowStart, 1, &rows),
            CellPosition { x: 0, y: 0 }
        );
        mode.cursor = CellPosition { x: 3, y: 0 };
        assert_eq!(
            motion_cell(&mode, Motion::Right, 2, &rows),
            CellPosition { x: 1, y: 1 }
        );
        mode.cursor = CellPosition { x: 0, y: 2 };
        assert_eq!(motion_cell(&mode, Motion::Left, 1, &rows), mode.cursor);
        mode.cursor = CellPosition { x: 3, y: 1 };
        assert_eq!(motion_cell(&mode, Motion::Right, 1, &rows), mode.cursor);
    }

    #[test]
    fn logical_horizontal_wraps_preserve_wide_and_combining_graphemes() {
        let rows = [
            TextRow {
                text: "a猫".into(),
                columns: vec![0, 1, 1, 1],
                wraps_to_next: true,
            },
            TextRow {
                text: "e\u{301}z".into(),
                columns: vec![0, 0, 0, 1],
                wraps_to_next: false,
            },
        ];
        let read = |y: u64| rows.get(y as usize).cloned();
        let mut mode = copy_at(1, 0, 2);
        mode.columns = 3;
        let MotionEvaluation::Complete(result) =
            mode.evaluate_motion(Motion::Right, 1, false, None, read, |cell, _| Some(cell))
        else {
            panic!("right wrap failed");
        };
        assert_eq!(result.target, CellPosition { x: 0, y: 1 });
        mode.moved_with_rows(Motion::Right, result.target, read);
        let MotionEvaluation::Complete(result) =
            mode.evaluate_motion(Motion::Left, 1, false, None, read, |mut cell, right| {
                assert!(!right);
                if cell.y == 0 && cell.x == 2 {
                    cell.x = 1;
                }
                Some(cell)
            })
        else {
            panic!("left wrap failed");
        };
        assert_eq!(result.target, CellPosition { x: 1, y: 0 });
        mode.moved_with_rows(Motion::Left, result.target, read);
        let jump = Jump {
            target: "e\u{301}".into(),
            forward: true,
            till: false,
        };
        assert_eq!(
            mode.jump_target(&jump, false, read),
            Some(CellPosition { x: 0, y: 1 })
        );
    }

    #[test]
    fn logical_vertical_motions_preserve_column_through_short_lines() {
        let rows = [
            row("abcd", true),
            row("efgh", false),
            row("ij", false),
            row("klmn", true),
            row("opqr", false),
        ];
        let read = |y: u64| rows.get(y as usize).cloned();
        let mut mode = copy_at(2, 1, 5);
        mode.columns = 4;
        let target = motion_cell(&mode, Motion::Down, 1, &rows);
        assert_eq!(target, CellPosition { x: 1, y: 2 });
        mode.moved_with_rows(Motion::Down, target, read);
        let target = motion_cell(&mode, Motion::Down, 1, &rows);
        assert_eq!(target, CellPosition { x: 2, y: 4 });
        mode.moved_with_rows(Motion::Down, target, read);
        assert_eq!(
            motion_cell(&mode, Motion::Up, 2, &rows),
            CellPosition { x: 2, y: 1 }
        );
        assert_eq!(
            motion_cell(&mode, Motion::Up, 1, &rows),
            CellPosition { x: 1, y: 2 }
        );
        mode.moved(Motion::Left, CellPosition { x: 1, y: 4 });
        assert_eq!(
            motion_cell(&mode, Motion::Up, 2, &rows),
            CellPosition { x: 1, y: 1 }
        );
    }

    #[test]
    fn hard_broken_preview_lines_remain_independent() {
        let rows = [row("abcd", false), row("efgh", false), row("ijkl", false)];
        let mut mode = copy_at(2, 1, 3);
        mode.columns = 4;
        assert_eq!(
            motion_cell(&mode, Motion::Up, 1, &rows),
            CellPosition { x: 2, y: 0 }
        );
        assert_eq!(
            motion_cell(&mode, Motion::Down, 1, &rows),
            CellPosition { x: 2, y: 2 }
        );
        assert_eq!(
            motion_cell(&mode, Motion::RowStart, 1, &rows),
            CellPosition { x: 0, y: 1 }
        );
        mode.cursor.x = 0;
        assert_eq!(motion_cell(&mode, Motion::Left, 1, &rows), mode.cursor);
    }

    #[test]
    fn logical_nonblank_jumps_and_numbered_lines_skip_continuations() {
        let rows = [
            row("  ab", true),
            row("cdef", false),
            row("  gh", true),
            row("ijkl", false),
            row("  mn", false),
        ];
        let read = |y: u64| rows.get(y as usize).cloned();
        let mut mode = copy_at(1, 1, 5);
        mode.columns = 4;
        assert_eq!(
            mode.nonblank_line_target(Motion::NextLineNonblank, 1, read),
            Some(CellPosition { x: 2, y: 2 })
        );
        assert_eq!(
            mode.nonblank_line_target(Motion::NextLineNonblank, 2, read),
            Some(CellPosition { x: 2, y: 4 })
        );
        assert_eq!(
            mode.nonblank_line_target(Motion::PreviousLineNonblank, 1, read),
            Some(CellPosition { x: 2, y: 0 })
        );
        assert_eq!(motion_cell(&mode, Motion::ScrollbackTop, 2, &rows).y, 2);
        assert_eq!(motion_cell(&mode, Motion::ScrollbackBottom, 3, &rows).y, 4);
    }

    #[test]
    fn logical_character_jumps_find_wrapped_graphemes_and_respect_hard_breaks() {
        let rows = [row("abxc", true), row("dxex", false), row("xx", false)];
        let read = |y: u64| rows.get(y as usize).cloned();
        let mut mode = copy_at(3, 0, 3);
        mode.columns = 4;
        let mut jump = Jump {
            target: "x".into(),
            forward: true,
            till: false,
        };
        assert_eq!(
            mode.jump_target(&jump, false, read),
            Some(CellPosition { x: 1, y: 1 })
        );
        jump.till = true;
        let target = mode.jump_target(&jump, false, read).unwrap();
        assert_eq!(target, CellPosition { x: 0, y: 1 });
        mode.moved(Motion::CharacterJump, target);
        assert_eq!(
            mode.jump_target(&jump, true, read),
            Some(CellPosition { x: 2, y: 1 })
        );
        jump.forward = false;
        jump.till = false;
        assert_eq!(
            mode.jump_target(&jump, false, read),
            Some(CellPosition { x: 2, y: 0 })
        );
        mode.cursor = CellPosition { x: 3, y: 1 };
        jump.forward = true;
        assert_eq!(mode.jump_target(&jump, false, read), None);
    }

    #[test]
    fn logical_bracket_lookup_can_start_on_a_continuation_row() {
        let rows = [row("abcd", true), row("(ef)", false), row("(gh)", false)];
        let mut mode = copy_at(2, 0, 3);
        mode.columns = 4;
        assert_eq!(
            mode.text_target(Motion::MatchingBracket, |y| rows.get(y as usize).cloned()),
            Some(CellPosition { x: 3, y: 1 })
        );
    }

    #[test]
    fn line_content_motions_cross_soft_wraps_and_stop_at_hard_breaks() {
        let rows = [
            row("before", false),
            row("  first", true),
            row("middle", true),
            row("last  ", false),
            row("  next", true),
            row("end", false),
        ];
        let read = |y: u64| rows.get(y as usize).cloned();
        for y in 1..=3 {
            let mode = copy_at(1, y, rows.len() as u64);
            assert_eq!(
                mode.text_target(Motion::FirstNonblank, read),
                Some(CellPosition { x: 2, y: 1 })
            );
            assert_eq!(
                mode.text_target(Motion::LastNonblank, read),
                Some(CellPosition { x: 3, y: 3 })
            );
            assert!(
                matches!(mode.evaluate_motion(Motion::LastNonblank, 2, true, None, read, |cell, _| Some(cell)), MotionEvaluation::Complete(result) if result.target == CellPosition { x: 2, y: 5 })
            );
            assert_eq!(
                mode.yank_to_row_end_range(2, read).unwrap().end,
                CellPosition { x: 2, y: 5 }
            );
        }
    }

    #[test]
    fn line_content_motions_skip_blank_wrapped_rows() {
        let rows = [row("   ", true), row("  text  ", true), row("   ", false)];
        let read = |y: u64| rows.get(y as usize).cloned();
        let mode = copy_at(0, 2, 3);
        assert_eq!(
            mode.text_target(Motion::FirstNonblank, read),
            Some(CellPosition { x: 2, y: 1 })
        );
        assert_eq!(
            mode.text_target(Motion::LastNonblank, read),
            Some(CellPosition { x: 5, y: 1 })
        );
        let blank = |y: u64| Some(row("   ", y < 2));
        assert_eq!(
            mode.text_target(Motion::FirstNonblank, blank),
            Some(CellPosition { x: 0, y: 0 })
        );
        assert_eq!(
            mode.text_target(Motion::LastNonblank, blank),
            Some(CellPosition { x: 0, y: 2 })
        );
    }

    #[test]
    fn line_content_motions_use_first_and_last_nonblank_cell() {
        let mode = copy_at(5, 0, 1);
        let read = |_| Some(row("  foo  ", false));
        assert_eq!(mode.text_target(Motion::FirstNonblank, read).unwrap().x, 2);
        assert_eq!(mode.text_target(Motion::LastNonblank, read).unwrap().x, 4);
        let blank = |_| Some(row("    ", false));
        assert_eq!(mode.text_target(Motion::FirstNonblank, blank).unwrap().x, 0);
        assert_eq!(mode.text_target(Motion::LastNonblank, blank).unwrap().x, 0);
    }

    #[test]
    fn word_motions_distinguish_keywords_punctuation_and_whitespace() {
        let mode = copy_at(1, 0, 1);
        let read = |_| Some(row("foo.bar  baz", false));
        assert_eq!(mode.text_target(Motion::ForwardWord, read).unwrap().x, 3);
        assert_eq!(mode.text_target(Motion::ForwardWordEnd, read).unwrap().x, 2);
        let mode = copy_at(9, 0, 1);
        assert_eq!(mode.text_target(Motion::BackwardWord, read).unwrap().x, 4);
    }

    #[test]
    fn word_motions_cross_soft_wrap_without_splitting_word() {
        let rows = [row("foo", true), row("bar baz", false)];
        let read = |y: u64| rows.get(y as usize).cloned();
        let mode = copy_at(1, 0, 2);
        assert_eq!(
            mode.text_target(Motion::ForwardWord, read),
            Some(CellPosition { x: 4, y: 1 })
        );
        assert_eq!(
            mode.text_target(Motion::ForwardWordEnd, read),
            Some(CellPosition { x: 2, y: 1 })
        );
        let mode = copy_at(1, 1, 2);
        assert_eq!(
            mode.text_target(Motion::BackwardWord, read),
            Some(CellPosition { x: 0, y: 0 })
        );
    }

    #[test]
    fn multibyte_graphemes_map_to_ghostty_cell_columns() {
        let row = TextRow {
            text: "a.界 b".into(),
            columns: vec![0, 1, 2, 2, 2, 4, 5],
            wraps_to_next: false,
        };
        let mode = copy_at(0, 0, 1);
        assert_eq!(
            mode.text_target(Motion::ForwardWord, |_| Some(row.clone()))
                .unwrap()
                .x,
            1
        );
        let mode = copy_at(1, 0, 1);
        assert_eq!(
            mode.text_target(Motion::ForwardWord, |_| Some(row.clone()))
                .unwrap()
                .x,
            2
        );
    }

    #[test]
    fn combining_mark_stays_with_its_cell_and_word() {
        let row = TextRow {
            text: "a\u{0301}.b".into(),
            columns: vec![0, 0, 0, 1, 2],
            wraps_to_next: false,
        };
        let mode = copy_at(0, 0, 1);
        assert_eq!(
            mode.text_target(Motion::ForwardWord, |_| Some(row.clone()))
                .unwrap()
                .x,
            1
        );
        assert_eq!(
            mode.text_target(Motion::ForwardWordEnd, |_| Some(row.clone()))
                .unwrap()
                .x,
            1
        );
    }

    #[test]
    fn word_end_skips_current_single_cell_word() {
        let mode = copy_at(0, 0, 1);
        assert_eq!(
            mode.text_target(Motion::ForwardWordEnd, |_| Some(row("a b", false))),
            Some(CellPosition { x: 2, y: 0 })
        );
    }

    #[test]
    fn big_words_join_punctuation_and_words() {
        let read = |_| Some(row("foo.bar baz", false));
        let mode = copy_at(1, 0, 1);
        assert_eq!(mode.text_target(Motion::ForwardBigWord, read).unwrap().x, 8);
        assert_eq!(
            mode.text_target(Motion::ForwardBigWordEnd, read).unwrap().x,
            6
        );
        let mode = copy_at(8, 0, 1);
        assert_eq!(
            mode.text_target(Motion::BackwardBigWord, read).unwrap().x,
            0
        );
    }

    #[test]
    fn counts_keep_zero_as_a_motion_only_without_a_prefix() {
        let mut mode = copy_at(5, 3, 10);
        assert!(!mode.add_count_digit(0));
        assert!(mode.add_count_digit(2));
        assert!(mode.add_count_digit(0));
        assert_eq!(mode.take_count(), 20);
        assert_eq!(mode.take_count(), 1);
        assert_eq!(mode.numbered_row_target(3).unwrap().y, 2);
    }

    #[test]
    fn previous_word_end_distinguishes_punctuation_and_big_words() {
        let mode = copy_at(6, 0, 1);
        let read = |_| Some(row("foo.bar", false));
        assert_eq!(
            mode.text_target(Motion::PreviousWordEnd, read).unwrap().x,
            3
        );
        assert_eq!(mode.text_target(Motion::PreviousBigWordEnd, read), None);
        let rows = [row("foo", true), row("bar baz", false)];
        let mode = copy_at(5, 1, 2);
        assert_eq!(
            mode.text_target(Motion::PreviousWordEnd, |y| rows.get(y as usize).cloned()),
            Some(CellPosition { x: 2, y: 1 })
        );
    }

    #[test]
    fn nonblank_line_motions_use_count_and_clamp() {
        let rows = [
            row("  one", false),
            row("    ", false),
            row(" three", false),
        ];
        let mode = copy_at(3, 0, 3);
        let read = |y: u64| rows.get(y as usize).cloned();
        assert_eq!(
            mode.nonblank_line_target(Motion::NextLineNonblank, 2, read),
            Some(CellPosition { x: 1, y: 2 })
        );
        assert_eq!(
            mode.nonblank_line_target(Motion::LineNonblank, 1, read),
            Some(CellPosition { x: 2, y: 0 })
        );
        let mode = copy_at(3, 2, 3);
        assert_eq!(
            mode.nonblank_line_target(Motion::PreviousLineNonblank, 1, read),
            Some(CellPosition { x: 0, y: 1 })
        );
    }

    #[test]
    fn word_lookup_preserves_search_and_object_punctuation_rules() {
        let rows = [row("foo ... bar", false)];
        let read = |y: u64| rows.get(y as usize).cloned();
        let mode = copy_at(5, 0, 1);
        assert_eq!(mode.word_under_cursor(read).as_deref(), Some("bar"));
        assert_eq!(
            mode.word_object_range(1, WordObject::Inner, false, read),
            Some(CellRange {
                start: CellPosition { x: 4, y: 0 },
                end: CellPosition { x: 6, y: 0 },
            })
        );
        assert_eq!(
            copy_at(3, 0, 1).word_under_cursor(read).as_deref(),
            Some("bar")
        );
        assert_eq!(
            copy_at(10, 0, 1).word_under_cursor(read).as_deref(),
            Some("bar")
        );

        let rows = [row("foo", true), row("...", true), row("bar", false)];
        let read = |y: u64| rows.get(y as usize).cloned();
        // A keyword on another physical row is not a search fallback.
        assert_eq!(copy_at(1, 1, 3).word_under_cursor(read), None);
    }

    #[test]
    fn word_under_cursor_joins_soft_wrapped_keyword() {
        let rows = [row("foo", true), row("bar baz", false)];
        let read = |y: u64| rows.get(y as usize).cloned();
        assert_eq!(
            copy_at(1, 0, 2).word_under_cursor(read).as_deref(),
            Some("foobar")
        );
        assert_eq!(
            copy_at(1, 1, 2).word_under_cursor(read).as_deref(),
            Some("foobar")
        );
        let unicode = TextRow {
            text: "猫語 test".into(),
            columns: vec![0, 0, 0, 1, 1, 1, 2, 3, 4, 5, 6],
            wraps_to_next: false,
        };
        assert_eq!(
            copy_at(1, 0, 1)
                .word_under_cursor(|_| Some(unicode.clone()))
                .as_deref(),
            Some("猫語")
        );
    }

    #[test]
    fn word_search_matches_whole_unicode_words() {
        let matcher = regex::Regex::new(&word_search_pattern("猫")).unwrap();
        assert!(matcher.is_match("a 猫 b"));
        assert!(!matcher.is_match("a 猫語 b"));
        let matcher = regex::Regex::new(&word_search_pattern("foo")).unwrap();
        assert!(matcher.is_match("FOO"));
        assert!(!matcher.is_match("foobar"));
    }

    #[test]
    fn match_navigation_skips_current_and_wraps_in_both_directions() {
        let matches = [9, 4, 0].map(|x| CellRange {
            start: CellPosition { x, y: 0 },
            end: CellPosition { x, y: 0 },
        });
        assert_eq!(
            next_match_cell(&matches, CellPosition { x: 4, y: 0 }, true, 1)
                .unwrap()
                .x,
            9
        );
        assert_eq!(
            next_match_cell(&matches, CellPosition { x: 4, y: 0 }, false, 1)
                .unwrap()
                .x,
            0
        );
        assert_eq!(
            next_match_cell(&matches, CellPosition { x: 9, y: 0 }, true, 2)
                .unwrap()
                .x,
            4
        );
        assert_eq!(
            next_match_cell(&matches, CellPosition { x: 0, y: 0 }, false, 2)
                .unwrap()
                .x,
            4
        );

        // The same ordering is used to publish decorations; matches far
        // apart in scrollback must remain newest-first after search accepts.
        let rows = [900, 500, 100].map(|y| CellRange {
            start: CellPosition { x: 0, y },
            end: CellPosition { x: 3, y },
        });
        assert_eq!(
            next_match_cell(&rows, CellPosition { x: 0, y: 500 }, true, 1)
                .unwrap()
                .y,
            900
        );
        assert_eq!(
            next_match_cell(&rows, CellPosition { x: 0, y: 500 }, false, 1)
                .unwrap()
                .y,
            100
        );
    }

    #[test]
    fn row_and_scrollback_edges_are_clamped() {
        let mode = copy_at(5, 3, 10);
        assert_eq!(
            mode.move_to(Motion::RowStart, None).unwrap().0,
            CellPosition { x: 0, y: 3 }
        );
        assert_eq!(mode.move_to(Motion::ScrollbackTop, None).unwrap().0.y, 0);
        assert_eq!(mode.move_to(Motion::ScrollbackBottom, None).unwrap().0.y, 9);
    }

    #[test]
    fn character_jumps_find_graphemes_and_till_stops_before_target() {
        let row = TextRow {
            text: "a界b界c".into(),
            columns: vec![0, 1, 1, 1, 3, 4, 4, 4, 6],
            wraps_to_next: false,
        };
        let mode = copy_at(0, 0, 1);
        let jump = Jump {
            target: "界".into(),
            forward: true,
            till: false,
        };
        assert_eq!(
            mode.jump_target(&jump, false, |_| Some(row.clone()))
                .unwrap()
                .x,
            1
        );
        let till = Jump {
            till: true,
            ..jump.clone()
        };
        assert_eq!(
            mode.jump_target(&till, false, |_| Some(row.clone()))
                .unwrap()
                .x,
            0
        );
        let mode = copy_at(0, 0, 1);
        assert_eq!(
            mode.jump_target(&till, true, |_| Some(row.clone()))
                .unwrap()
                .x,
            3
        );
        let reverse = Jump {
            target: "界".into(),
            forward: false,
            till: false,
        };
        let mode = copy_at(6, 0, 1);
        assert_eq!(
            mode.jump_target(&reverse, true, |_| Some(row.clone()))
                .unwrap()
                .x,
            4
        );
    }

    #[test]
    fn matching_bracket_handles_nesting_across_rows() {
        let rows = [row("(a(", false), row("b)", false), row(")", false)];
        let read = |y: u64| rows.get(y as usize).cloned();
        let mode = copy_at(0, 0, 3);
        assert_eq!(
            mode.text_target(Motion::MatchingBracket, read),
            Some(CellPosition { x: 0, y: 2 })
        );
        let mode = copy_at(0, 2, 3);
        assert_eq!(
            mode.text_target(Motion::MatchingBracket, read),
            Some(CellPosition { x: 0, y: 0 })
        );
    }

    #[test]
    fn matching_bracket_starts_at_next_bracket_on_row() {
        let mode = copy_at(0, 0, 1);
        assert_eq!(
            mode.text_target(Motion::MatchingBracket, |_| Some(row("hi [x]", false))),
            Some(CellPosition { x: 5, y: 0 })
        );
        assert_eq!(
            mode.text_target(Motion::MatchingBracket, |_| Some(row("plain", false))),
            None
        );
    }

    #[test]
    fn movement_clamps_at_snapshot_edges() {
        let mut mode = CopyMode::default();
        mode.start(
            1,
            View {
                cursor: CellPosition { x: 0, y: 0 },
                columns: 4,
                total_rows: 3,
                ..Default::default()
            },
        );
        assert_eq!(mode.move_to(Motion::Left, None).unwrap().0, mode.cursor);
        assert_eq!(mode.move_to(Motion::Up, None).unwrap().0, mode.cursor);
        mode.moved(Motion::Right, CellPosition { x: 3, y: 2 });
        assert_eq!(mode.move_to(Motion::Right, None).unwrap().0, mode.cursor);
        assert_eq!(mode.move_to(Motion::Down, None).unwrap().0, mode.cursor);
    }

    #[test]
    fn viewport_and_page_motions_use_current_view() {
        let mut mode = CopyMode::default();
        mode.start(
            7,
            View {
                cursor: CellPosition { x: 4, y: 41 },
                columns: 10,
                total_rows: 100,
                ..Default::default()
            },
        );
        let view = View {
            viewport_top: 30,
            viewport_rows: 20,
            total_rows: 100,
            columns: 10,
            ..Default::default()
        };
        let target = |motion| mode.move_to(motion, Some(view)).unwrap().0;
        assert_eq!(target(Motion::ViewportTop), CellPosition { x: 4, y: 30 });
        assert_eq!(target(Motion::ViewportMiddle), CellPosition { x: 4, y: 40 });
        assert_eq!(target(Motion::ViewportBottom), CellPosition { x: 4, y: 49 });
        assert_eq!(target(Motion::PageUp), CellPosition { x: 4, y: 21 });
        assert_eq!(target(Motion::PageDown), CellPosition { x: 4, y: 61 });
        assert_eq!(target(Motion::HalfPageUp), CellPosition { x: 4, y: 31 });
        assert_eq!(target(Motion::HalfPageDown), CellPosition { x: 4, y: 51 });
        assert!(mode.move_to(Motion::PageDown, None).is_none());
    }

    #[test]
    fn page_motions_clamp_at_scrollback_edges() {
        let mut mode = CopyMode::default();
        mode.start(
            1,
            View {
                cursor: CellPosition { x: 2, y: 97 },
                columns: 5,
                total_rows: 100,
                ..Default::default()
            },
        );
        let view = View {
            viewport_top: 80,
            viewport_rows: 20,
            total_rows: 100,
            columns: 5,
            ..Default::default()
        };
        assert_eq!(mode.move_to(Motion::PageDown, Some(view)).unwrap().0.y, 99);
        mode.cursor.y = 2;
        assert_eq!(mode.move_to(Motion::HalfPageUp, Some(view)).unwrap().0.y, 0);
    }
}

pub use crate::document_search::search_regex;
