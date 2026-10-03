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
    pub purpose: Purpose,
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

    pub fn status_text(&self) -> String {
        if self.hints.is_empty() {
            "NO MATCHES · Esc".to_owned()
        } else {
            format!(
                "{}  ·  {} hints",
                self.prefix.to_ascii_uppercase(),
                self.visible_hints().count()
            )
        }
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
        mut read: impl FnMut(u64) -> Option<TextRow>,
    ) -> Option<Self> {
        static MATCHER: OnceLock<Regex> = OnceLock::new();
        let matcher = MATCHER.get_or_init(|| Regex::new(r"\S+").expect("valid big-word regex"));
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
                for found in matcher.find_iter(&text) {
                    let start = points[found.start()];
                    let end = points[found.end() - 1];
                    if start.y >= view.viewport_top && start.y < bottom {
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
        let labels = labels(ranges.len());
        Some(Self {
            generation,
            from_copy,
            view,
            prefix: String::new(),
            purpose: Purpose::Navigate,
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

fn labels(count: usize) -> Vec<String> {
    let mut width = 1;
    let mut capacity = ALPHABET.len();
    while capacity < count {
        width += 1;
        capacity = capacity.saturating_mul(ALPHABET.len());
    }
    (0..count)
        .map(|mut index| {
            let mut label = vec![ALPHABET[0]; width];
            for ch in label.iter_mut().rev() {
                *ch = ALPHABET[index % ALPHABET.len()];
                index /= ALPHABET.len();
            }
            String::from_utf8(label).expect("ASCII hint alphabet")
        })
        .collect()
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
            let result = labels(count);
            let unique: std::collections::HashSet<_> = result.iter().collect();
            assert_eq!(unique.len(), count);
            if let Some(first) = result.first() {
                assert!(result.iter().all(|label| label.len() == first.len()));
            }
        }
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
