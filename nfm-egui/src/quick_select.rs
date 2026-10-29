//! Shared document Quick Select: big-word hints, paging and prefix input.
use crate::copy_mode::{
    CellPosition as SnapshotCell, CellRange as SnapshotCellRange, TextRow, View as SnapshotView,
};

pub struct HintText {
    pub cell: SnapshotCell,
    pub codepoint: u32,
}

use regex::Regex;
use std::sync::OnceLock;

const ALPHABET: &[u8] = b"asdfghjklqwertyuiopzxcvbnm";

/// The letters hint labels are made of, in the order they are handed out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Alphabet(Vec<u8>);

impl Alphabet {
    /// At least two distinct ASCII letters. Labels are typed in lowercase,
    /// since Shift on the last letter is reserved for pasting.
    pub fn new(letters: &str) -> Result<Self, String> {
        let letters = letters.to_ascii_lowercase().into_bytes();
        if let Some(&invalid) = letters.iter().find(|ch| !ch.is_ascii_lowercase()) {
            return Err(format!(
                "alphabet may contain only letters, found {:?}",
                char::from(invalid)
            ));
        }
        for (index, ch) in letters.iter().enumerate() {
            if letters[..index].contains(ch) {
                return Err(format!("alphabet repeats {:?}", char::from(*ch)));
            }
        }
        if letters.len() < 2 {
            return Err("alphabet needs at least two letters".into());
        }
        Ok(Self(letters))
    }
}

impl Default for Alphabet {
    fn default() -> Self {
        Self(ALPHABET.to_vec())
    }
}

/// The big-word pattern the default quick select hints.
pub fn big_words() -> &'static Regex {
    static MATCHER: OnceLock<Regex> = OnceLock::new();
    MATCHER.get_or_init(|| Regex::new(r"\S+").expect("valid big-word regex"))
}

pub struct Hint {
    pub range: SnapshotCellRange,
    pub label: String,
}

#[derive(Clone, Copy, Default)]
pub enum Purpose {
    #[default]
    Navigate,
    YankLines {
        origin: SnapshotCell,
    },
}

pub struct QuickSelect {
    pub generation: u64,
    pub from_copy: bool,
    pub hints: Vec<Hint>,
    pub prefix: String,
    pub view: SnapshotView,
    /// Set through `set_purpose`, which relabels the hints to match.
    purpose: Purpose,
    alphabet: Alphabet,
}

pub enum InputAction {
    None,
    Cancel,
    Page(bool),
    Changed,
    Select { cell: SnapshotCell, shifted: bool },
}

impl QuickSelect {
    pub fn input(&mut self, events: &[egui::Event], index: usize) -> InputAction {
        match &events[index] {
            egui::Event::Key {
                key: egui::Key::Escape,
                pressed: true,
                ..
            } => InputAction::Cancel,
            egui::Event::Key {
                key: key @ (egui::Key::PageUp | egui::Key::PageDown),
                pressed: true,
                modifiers,
                ..
            } if *modifiers == egui::Modifiers::NONE => {
                InputAction::Page(*key == egui::Key::PageDown)
            }
            egui::Event::Key {
                key: egui::Key::Backspace,
                pressed: true,
                ..
            } => {
                self.prefix.pop();
                InputAction::Changed
            }
            egui::Event::Text(text) => match self.type_text(text) {
                Some(cell) => InputAction::Select {
                    cell,
                    shifted: shifted_hint(events, index),
                },
                None => InputAction::Changed,
            },
            _ => InputAction::None,
        }
    }

    pub fn purpose(&self) -> Purpose {
        self.purpose
    }

    /// A linewise yank's hints mark rows, not words. Hints on the origin row
    /// are dropped, since that target is the same as `yy`; hosts mark the row
    /// itself instead (see `origin_row`). Every hint on a row yanks the same
    /// lines, so each row gets one label, repeated at every word so it stays
    /// near the reader's eye, with the shortest labels nearest the origin.
    pub fn set_purpose(&mut self, purpose: Purpose) {
        if let Purpose::YankLines { origin } = purpose {
            self.hints.retain(|hint| hint.range.start.y != origin.y);
            let mut rows: Vec<u64> = self.hints.iter().map(|hint| hint.range.start.y).collect();
            rows.sort_unstable_by_key(|&y| (y.abs_diff(origin.y), y));
            rows.dedup();
            let labels: std::collections::HashMap<_, _> = rows
                .iter()
                .copied()
                .zip(row_labels(&self.alphabet.0, rows.len()))
                .collect();
            for hint in &mut self.hints {
                hint.label = labels[&hint.range.start.y].clone();
            }
        }
        self.purpose = purpose;
    }

    /// The row a linewise yank starts from, for hosts to highlight.
    pub fn origin_row(&self) -> Option<u64> {
        match self.purpose {
            Purpose::YankLines { origin } => Some(origin.y),
            Purpose::Navigate => None,
        }
    }

    pub fn status_text(&self) -> String {
        if self.hints.is_empty() {
            return "NO MATCHES · Esc".to_owned();
        }
        let prefix = self.prefix.to_ascii_uppercase();
        if let Purpose::YankLines { .. } = self.purpose {
            // Linewise targets are rows, so count rows rather than hints.
            let mut rows: Vec<_> = self
                .visible_hints()
                .map(|hint| hint.range.start.y)
                .collect();
            rows.sort_unstable();
            rows.dedup();
            let unit = if rows.len() == 1 { "line" } else { "lines" };
            return format!("{prefix}  ·  {} {unit}", rows.len());
        }
        format!("{prefix}  ·  {} hints", self.visible_hints().count())
    }

    pub fn paged_view(mut view: SnapshotView, down: bool) -> SnapshotView {
        let height = u64::from(view.viewport_rows);
        let max_top = view.total_rows.saturating_sub(height);
        view.viewport_top = if down {
            view.viewport_top.saturating_add(height).min(max_top)
        } else {
            view.viewport_top.saturating_sub(height).min(max_top)
        };
        view
    }

    pub fn new(
        generation: u64,
        from_copy: bool,
        view: SnapshotView,
        read: impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<Self> {
        Self::new_visible(generation, from_copy, view, read, |_| true)
    }

    /// Filter document positions against the host's current presentation before
    /// assigning labels. A captured row can be only partly visible after reflow.
    pub fn new_visible(
        generation: u64,
        from_copy: bool,
        view: SnapshotView,
        read: impl FnMut(u64) -> Option<TextRow>,
        visible: impl FnMut(SnapshotCell) -> bool,
    ) -> Option<Self> {
        Self::new_matching(
            generation,
            from_copy,
            view,
            std::slice::from_ref(big_words()),
            &Alphabet::default(),
            read,
            visible,
        )
    }

    /// Hint every match of `patterns` instead of big words. A pattern with a
    /// capture group hints only the first group that participates, so context
    /// can be required without being selected. Empty matches are skipped, and
    /// where matches of different patterns overlap the earliest one wins.
    pub fn new_matching(
        generation: u64,
        from_copy: bool,
        view: SnapshotView,
        patterns: &[Regex],
        alphabet: &Alphabet,
        mut read: impl FnMut(u64) -> Option<TextRow>,
        mut visible: impl FnMut(SnapshotCell) -> bool,
    ) -> Option<Self> {
        let bottom = view
            .viewport_top
            .saturating_add(u64::from(view.viewport_rows))
            .min(view.total_rows);
        let mut row = view.viewport_top;
        // Include the beginning of the first logical line. Matches whose
        // start is above the viewport are excluded rather than mislabeled.
        while row > 0 && read(row - 1)?.wraps_to_next {
            row -= 1;
        }
        let mut text = String::new();
        let mut points = Vec::new();
        let mut ranges = Vec::new();
        while row < view.total_rows {
            let line = read(row)?;
            if line.text.len() != line.columns.len() {
                return None;
            }
            if text.len().saturating_add(line.text.len()) > 4 * 1024 * 1024 {
                return None;
            }
            text.push_str(&line.text);
            points.extend(line.columns.iter().map(|&x| SnapshotCell { x, y: row }));
            row += 1;
            if !line.wraps_to_next || row == view.total_rows {
                for (found_start, found_end) in matches(patterns, &text) {
                    let start = points[found_start];
                    let end = points[found_end - 1];
                    if start.y >= view.viewport_top && start.y < bottom && visible(start) {
                        if ranges.len() >= 200_000 {
                            return None;
                        }
                        ranges.push(SnapshotCellRange { start, end });
                    }
                }
                text.clear();
                points.clear();
                if row >= bottom {
                    break;
                }
            }
        }
        // Prefer the most recent output. Decorations expect newest first.
        ranges.reverse();
        let labels = labels(&alphabet.0, ranges.len());
        Some(Self {
            generation,
            from_copy,
            view,
            prefix: String::new(),
            purpose: Purpose::Navigate,
            alphabet: alphabet.clone(),
            hints: ranges
                .into_iter()
                .zip(labels)
                .map(|(range, label)| Hint { range, label })
                .collect(),
        })
    }

    pub fn type_text(&mut self, text: &str) -> Option<SnapshotCell> {
        for ch in text.chars() {
            if !ch.is_ascii_alphabetic() {
                continue;
            }
            let mut next = self.prefix.clone();
            next.push(ch.to_ascii_lowercase());
            if !self.hints.iter().any(|hint| hint.label.starts_with(&next)) {
                continue;
            }
            self.prefix = next;
            if let Some(hint) = self.hints.iter().find(|hint| hint.label == self.prefix) {
                return Some(hint.range.start);
            }
        }
        None
    }

    pub fn visible_hints(&self) -> impl Iterator<Item = &Hint> {
        self.hints
            .iter()
            .filter(|hint| hint.label.starts_with(&self.prefix))
    }

    pub fn cell_text(&self) -> Vec<HintText> {
        let mut text = Vec::new();
        let visible: Vec<_> = self.visible_hints().collect();
        for (index, hint) in visible.iter().enumerate() {
            let start = hint.range.start;
            // Show the untyped suffix without covering another label.
            let end_x = index
                .checked_sub(1)
                .and_then(|previous| visible.get(previous))
                .filter(|other| other.range.start.y == start.y)
                .map_or(self.view.columns, |other| other.range.start.x);
            let label = &hint.label[self.prefix.len()..];
            for (offset, ch) in label.bytes().enumerate() {
                let x = start.x + offset as u32;
                if x >= end_x || x >= self.view.columns {
                    break;
                }
                text.push(HintText {
                    cell: SnapshotCell { x, y: start.y },
                    codepoint: u32::from(ch.to_ascii_uppercase()),
                });
            }
        }
        text
    }
}

/// Byte ranges of the non-empty, non-overlapping matches in text order.
fn matches(patterns: &[Regex], text: &str) -> Vec<(usize, usize)> {
    let mut found: Vec<(usize, usize)> = Vec::new();
    for pattern in patterns {
        for captures in pattern.captures_iter(text) {
            let target = (1..captures.len())
                .find_map(|group| captures.get(group))
                .or_else(|| captures.get(0))
                .expect("a match has group 0");
            if !target.is_empty() {
                found.push((target.start(), target.end()));
            }
        }
    }
    if patterns.len() > 1 {
        // Earliest first; at the same start, the longer match.
        found.sort_unstable_by_key(|&(start, end)| (start, std::cmp::Reverse(end)));
        let mut end_of_last = 0;
        found.retain(|&(start, end)| {
            let keep = start >= end_of_last;
            if keep {
                end_of_last = end;
            }
            keep
        });
    }
    found
}

fn labels(alphabet: &[u8], count: usize) -> Vec<String> {
    let mut width = 1;
    let mut capacity = alphabet.len();
    while capacity < count {
        width += 1;
        capacity = capacity.saturating_mul(alphabet.len());
    }
    (0..count)
        .map(|mut index| {
            let mut label = vec![alphabet[0]; width];
            for ch in label.iter_mut().rev() {
                *ch = alphabet[index % alphabet.len()];
                index /= alphabet.len();
            }
            String::from_utf8(label).expect("ASCII hint alphabet")
        })
        .collect()
}

/// Prefix-free labels where earlier entries get the shortest labels: as many
/// single letters as possible, the remaining letters prefix two-letter labels.
fn row_labels(alphabet: &[u8], count: usize) -> Vec<String> {
    let base = alphabet.len();
    let letter = |index: usize| char::from(alphabet[index]);
    if count <= base {
        return (0..count).map(|index| letter(index).to_string()).collect();
    }
    if count > base * base {
        // Beyond two letters, fall back to uniform-width labels.
        return labels(alphabet, count);
    }
    // Turning a single letter into a prefix trades one label for `base`.
    let singles = (base * base - count) / (base - 1);
    let mut result: Vec<String> = (0..singles)
        .map(|index| letter(index).to_string())
        .collect();
    'outer: for prefix in singles..base {
        for suffix in 0..base {
            if result.len() == count {
                break 'outer;
            }
            result.push(format!("{}{}", letter(prefix), letter(suffix)));
        }
    }
    result
}

/// Use the physical modifier from the paired key, not uppercase text (Caps Lock).
pub fn shifted_hint(events: &[egui::Event], text_index: usize) -> bool {
    matches!(text_index.checked_sub(1).and_then(|i| events.get(i)),
        Some(egui::Event::Key { pressed: true, modifiers, .. }) if modifiers.shift)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shifted_completion_uses_key_modifier_instead_of_letter_case() {
        for shift in [false, true] {
            let events = vec![
                egui::Event::Key {
                    key: egui::Key::A,
                    physical_key: Some(egui::Key::A),
                    pressed: true,
                    repeat: false,
                    modifiers: egui::Modifiers {
                        shift,
                        ..Default::default()
                    },
                },
                egui::Event::Text("A".into()),
            ];
            assert_eq!(shifted_hint(&events, 1), shift);
        }
        assert!(!shifted_hint(&[egui::Event::Text("A".into())], 0));
    }

    #[test]
    fn paging_clamps_to_scrollback_and_rebuilds_labels_without_prefix() {
        let view = SnapshotView {
            viewport_top: 4,
            viewport_rows: 3,
            total_rows: 8,
            columns: 20,
            ..Default::default()
        };
        assert_eq!(QuickSelect::paged_view(view, true).viewport_top, 5);
        let up = QuickSelect::paged_view(view, false);
        assert_eq!(up.viewport_top, 1);
        assert_eq!(QuickSelect::paged_view(up, false).viewport_top, 0);
        let short = SnapshotView {
            total_rows: 2,
            ..view
        };
        assert_eq!(QuickSelect::paged_view(short, true).viewport_top, 0);
        let read = |row| {
            Some(TextRow {
                text: format!("row{row}"),
                columns: vec![0, 1, 2, 3],
                wraps_to_next: false,
            })
        };
        let mut initial = QuickSelect::new(42, true, view, read).unwrap();
        initial.prefix = "a".into();
        let paged = QuickSelect::new(42, initial.from_copy, up, read).unwrap();
        assert!(paged.prefix.is_empty());
        assert_eq!(paged.generation, 42);
        assert!(paged.from_copy);
        assert_eq!(paged.hints.len(), 3);
        assert_eq!(paged.hints[0].range.start.y, 3);
        assert_eq!(paged.hints[2].range.start.y, 1);
    }
    #[test]
    fn rebuilding_hints_filters_partial_rows_and_exposes_new_words() {
        let view = SnapshotView {
            viewport_rows: 1,
            total_rows: 1,
            columns: 32,
            ..Default::default()
        };
        let read = |_| Some(row("one two three four", false));
        let mut initial =
            QuickSelect::new_visible(42, true, view, read, |cell| cell.x < 8).unwrap();
        assert_eq!(initial.hints.len(), 2);
        initial.prefix = "a".into();
        let enlarged = QuickSelect::new_visible(42, true, view, read, |_| true).unwrap();
        assert_eq!(enlarged.hints.len(), 4);
        assert!(enlarged.prefix.is_empty());
        let mut narrowed =
            QuickSelect::new_visible(42, true, view, read, |cell| cell.x >= 8).unwrap();
        assert_eq!(narrowed.hints.len(), 2);
        assert!(narrowed.hints.iter().all(|hint| hint.range.start.x >= 8));
        let labels: Vec<_> = narrowed
            .hints
            .iter()
            .map(|hint| (hint.label.clone(), hint.range.start))
            .collect();
        for (label, expected) in labels {
            narrowed.prefix.clear();
            assert_eq!(narrowed.type_text(&label), Some(expected));
        }
    }

    #[test]
    fn word_cells_follow_exported_wide_and_combining_character_mapping() {
        let rows = [TextRow {
            text: "e\u{301} 界x".into(),
            columns: vec![0, 0, 0, 1, 2, 2, 2, 4],
            wraps_to_next: false,
        }];
        let view = SnapshotView {
            viewport_rows: 1,
            total_rows: 1,
            columns: 5,
            ..Default::default()
        };
        let state = QuickSelect::new(1, false, view, |y| rows.get(y as usize).cloned()).unwrap();
        assert_eq!(
            state.hints[0].range,
            SnapshotCellRange {
                start: SnapshotCell { x: 2, y: 0 },
                end: SnapshotCell { x: 4, y: 0 },
            }
        );
        assert_eq!(state.hints[1].range.start, state.hints[1].range.end);
        assert!(!state.from_copy);
        assert_eq!(state.generation, 1);
    }

    fn row(text: &str, wraps_to_next: bool) -> TextRow {
        let mut columns = Vec::new();
        for (x, ch) in text.chars().enumerate() {
            columns.extend(std::iter::repeat_n(x as u32, ch.len_utf8()));
        }
        TextRow {
            text: text.into(),
            columns,
            wraps_to_next,
        }
    }

    #[test]
    fn labels_are_unique_and_prefix_free_at_each_size() {
        for count in [0, 1, 26, 27, 676, 677, 2000] {
            let result = labels(ALPHABET, count);
            let unique: std::collections::HashSet<_> = result.iter().collect();
            assert_eq!(unique.len(), count);
            if let Some(first) = result.first() {
                assert!(result.iter().all(|label| label.len() == first.len()));
            }
        }
    }

    #[test]
    fn row_labels_are_prefix_free_and_shortest_first() {
        for count in [1, 26, 27, 37, 200, 676, 677, 1000] {
            let labels = row_labels(ALPHABET, count);
            assert_eq!(labels.len(), count);
            assert!(labels.windows(2).all(|pair| pair[0].len() <= pair[1].len()));
            for (index, label) in labels.iter().enumerate() {
                for (other_index, other) in labels.iter().enumerate() {
                    assert!(index == other_index || !other.starts_with(label.as_str()));
                }
            }
        }
        // 37 rows leave room for 25 single-letter labels.
        assert_eq!(
            row_labels(ALPHABET, 37)
                .iter()
                .filter(|label| label.len() == 1)
                .count(),
            25
        );
    }

    #[test]
    fn yank_lines_labels_rows_nearest_first_and_skips_the_origin() {
        let rows: Vec<_> = (0..6).map(|_| row("one two", false)).collect();
        let view = SnapshotView {
            viewport_rows: 6,
            total_rows: 6,
            columns: 10,
            ..Default::default()
        };
        let mut state = QuickSelect::new(1, true, view, |y| rows.get(y as usize).cloned()).unwrap();
        let origin = SnapshotCell { x: 4, y: 3 };
        state.set_purpose(Purpose::YankLines { origin });
        assert_eq!(state.origin_row(), Some(3));
        assert!(state.hints.iter().all(|hint| hint.range.start.y != 3));
        let label = |y| {
            let mut labels: Vec<_> = state
                .hints
                .iter()
                .filter(|hint| hint.range.start.y == y)
                .map(|hint| hint.label.as_str())
                .collect();
            labels.dedup();
            assert_eq!(labels.len(), 1, "one label per row");
            labels[0].to_owned()
        };
        // Distance 1 above wins the tie with distance 1 below.
        assert_eq!(
            [2, 4, 1, 5, 0].map(label),
            ["a", "s", "d", "f", "g"].map(String::from)
        );
        assert_eq!(state.status_text(), "  ·  5 lines");
        assert_eq!(state.type_text("s").map(|cell| cell.y), Some(4));
    }

    #[test]
    fn big_words_join_soft_wraps_and_map_unicode_to_cells() {
        let rows = [
            row("older", false),
            row("ab", true),
            row("cd 界x", false),
            row("tail", false),
        ];
        let view = SnapshotView {
            viewport_top: 1,
            viewport_rows: 2,
            total_rows: 4,
            columns: 10,
            ..Default::default()
        };
        let mut state = QuickSelect::new(7, true, view, |y| rows.get(y as usize).cloned()).unwrap();
        assert_eq!(state.hints.len(), 2);
        assert_eq!(state.hints[0].range.start, SnapshotCell { x: 3, y: 2 });
        assert_eq!(state.hints[1].range.end, SnapshotCell { x: 1, y: 2 });
        assert_eq!(state.type_text("A"), Some(SnapshotCell { x: 3, y: 2 }));
        let view = SnapshotView {
            viewport_top: 2,
            viewport_rows: 1,
            ..view
        };
        let state = QuickSelect::new(7, true, view, |y| rows.get(y as usize).cloned()).unwrap();
        assert_eq!(state.hints.len(), 1); // exclude the word starting above the viewport
    }

    #[test]
    fn custom_patterns_hint_capture_groups_skip_empty_matches_and_drop_overlaps() {
        let rows = [row("rip=0x7ff6a2 rsp=0x00ab x", false)];
        let view = SnapshotView {
            viewport_rows: 1,
            total_rows: 1,
            columns: 30,
            ..Default::default()
        };
        let read = |y: u64| rows.get(y as usize).cloned();
        let ranges = |state: &QuickSelect| {
            let mut ranges: Vec<_> = state
                .hints
                .iter()
                .map(|hint| (hint.range.start.x, hint.range.end.x))
                .collect();
            ranges.sort_unstable();
            ranges
        };
        let group = [Regex::new(r"r[is]p=(0x[0-9a-f]+)").unwrap()];
        let state =
            QuickSelect::new_matching(1, false, view, &group, &Alphabet::default(), read, |_| true)
                .unwrap();
        assert_eq!(ranges(&state), [(4, 11), (17, 22)]);

        let empty = [Regex::new(r"x*").unwrap()];
        let state =
            QuickSelect::new_matching(1, false, view, &empty, &Alphabet::default(), read, |_| true)
                .unwrap();
        // Only the three x characters; no zero-width hints.
        assert_eq!(ranges(&state), [(5, 5), (18, 18), (24, 24)]);

        let overlapping = [
            Regex::new(r"0x[0-9a-f]+").unwrap(),
            Regex::new(r"[a-z]+=0x[0-9a-f]+").unwrap(),
        ];
        let state = QuickSelect::new_matching(
            1,
            false,
            view,
            &overlapping,
            &Alphabet::default(),
            read,
            |_| true,
        )
        .unwrap();
        assert_eq!(ranges(&state), [(0, 11), (13, 22)]);
    }

    #[test]
    fn custom_alphabet_labels_hints_and_yank_rows() {
        let rows: Vec<_> = (0..3).map(|_| row("a b c", false)).collect();
        let view = SnapshotView {
            viewport_rows: 3,
            total_rows: 3,
            columns: 10,
            ..Default::default()
        };
        let read = |y: u64| rows.get(y as usize).cloned();
        let alphabet = Alphabet::new("JK").unwrap();
        let words = [big_words().clone()];
        let mut state =
            QuickSelect::new_matching(1, true, view, &words, &alphabet, read, |_| true).unwrap();
        // Nine hints from two letters need four-letter labels.
        let labels: Vec<_> = state.hints.iter().map(|hint| hint.label.as_str()).collect();
        assert_eq!(labels[..3], ["jjjj", "jjjk", "jjkj"]);
        assert!(
            state
                .hints
                .iter()
                .all(|hint| hint.label.bytes().all(|ch| b"jk".contains(&ch)))
        );
        state.set_purpose(Purpose::YankLines {
            origin: SnapshotCell { x: 0, y: 1 },
        });
        let mut labels: Vec<_> = state.hints.iter().map(|hint| hint.label.clone()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels, ["j", "k"]);

        for (letters, error) in [("a", "two"), ("aba", "repeats"), ("ab1", "only letters")] {
            assert!(
                Alphabet::new(letters).unwrap_err().contains(error),
                "{letters}"
            );
        }
    }

    #[test]
    fn prefix_filters_and_invalid_input_does_not_destroy_it() {
        let rows = [row(&"x ".repeat(30), false)];
        let view = SnapshotView {
            viewport_rows: 1,
            total_rows: 1,
            columns: 60,
            ..Default::default()
        };
        let mut state =
            QuickSelect::new(1, false, view, |y| rows.get(y as usize).cloned()).unwrap();
        assert_eq!(state.type_text("a"), None);
        assert_eq!(state.visible_hints().count(), 26);
        assert_eq!(state.type_text("!"), None);
        assert_eq!(state.prefix, "a");
        assert!(state.type_text("a").is_some());
        state.prefix.pop();
        assert_eq!(state.prefix, "a");
        let cells = state.cell_text();
        let coords: std::collections::HashSet<_> = cells
            .iter()
            .map(|text| (text.cell.x, text.cell.y))
            .collect();
        assert_eq!(coords.len(), cells.len());
    }
}
