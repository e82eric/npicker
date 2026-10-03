//! Ranked line fragments with host-provided terminal cell coordinates.
use std::{collections::HashSet, ops::Range};

/// One displayed grapheme. Columns are half-open; width is supplied by the host.
#[derive(Clone, Debug)]
pub struct Cell {
    pub text: String,
    pub columns: Range<usize>,
}
#[derive(Clone, Debug)]
pub struct Line {
    pub row: u64,
    pub cells: Vec<Cell>,
    pub wraps_to_next: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    TableField,
    Delimited,
    Value,
    Fragment,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub text: String,
    pub row: u64,
    /// Half-open terminal columns, not UTF-8 byte or character offsets.
    pub columns: Range<usize>,
    pub kind: Kind,
}
fn whitespace(cell: &Cell) -> bool {
    cell.text.chars().all(char::is_whitespace)
}
fn symbol(cell: &Cell) -> Option<char> {
    let mut chars = cell.text.chars();
    let ch = chars.next()?;
    chars.next().is_none().then_some(ch)
}
fn bounds(line: &Line) -> Option<Range<usize>> {
    let start = line.cells.iter().position(|c| !whitespace(c))?;
    let end = line.cells.iter().rposition(|c| !whitespace(c))? + 1;
    Some(start..end)
}
fn blank_at(line: &Line, x: usize) -> Option<bool> {
    line.cells
        .iter()
        .find(|cell| cell.columns.contains(&x))
        .map(whitespace)
}

/// Extract a target physical row using up to ten contiguous neighbors each side.
/// Prompt ranges are half-open columns on the target row. No parsing changes
/// candidate text: quoted/escaped values retain their original interior bytes.
pub fn split(lines: &[Line], target: usize, prompts: &[Range<usize>]) -> Vec<Candidate> {
    let Some(line) = lines.get(target) else {
        return vec![];
    };
    let Some(content) = bounds(line) else {
        return vec![];
    };
    let mut spans: Vec<(Range<usize>, Kind)> = vec![];
    if !line.wraps_to_next {
        let mut neighbors = vec![line];
        for indices in [
            (target.saturating_sub(10)..target)
                .rev()
                .collect::<Vec<_>>(),
            (target + 1..lines.len().min(target + 11)).collect::<Vec<_>>(),
        ] {
            for index in indices {
                let neighbor = &lines[index];
                let Some(b) = bounds(neighbor) else { break };
                if neighbor.wraps_to_next
                    || neighbor.cells[b.start]
                        .columns
                        .start
                        .abs_diff(line.cells[content.start].columns.start)
                        > 2
                {
                    break;
                }
                neighbors.push(neighbor);
            }
        }
        // Infer separators from agreement across rows, not padding width.
        // A one-cell gutter also requires a consistent start of the next field.
        let gutter = |cell: &Cell| {
            whitespace(cell)
                && cell.columns.clone().all(|x| {
                    let votes: Vec<_> = neighbors
                        .iter()
                        .filter_map(|row| blank_at(row, x))
                        .collect();
                    votes.len() >= 3
                        && votes.iter().filter(|&&blank| blank).count() * 100 >= votes.len() * 85
                })
        };
        let mut start = content.start;
        let mut i = start;
        let mut fields = vec![];
        let mut coarse_fields = vec![];
        let mut coarse_start = start;
        while i < content.end {
            if !gutter(&line.cells[i]) {
                i += 1;
                continue;
            }
            let gap = i;
            while i < content.end && gutter(&line.cells[i]) {
                i += 1
            }
            let width = line.cells[i - 1].columns.end - line.cells[gap].columns.start;
            let right = line.cells.get(i).map(|cell| cell.columns.start);
            let aligned = right.is_some_and(|x| {
                // An internal space in equal-length phrases can coincide across
                // rows. Require evidence that this boundary stays fixed while
                // the preceding value's occupied width changes.
                let starts_left: HashSet<_> = neighbors
                    .iter()
                    .filter_map(|row| {
                        let mut left = x;
                        while left > 0 && blank_at(row, left - 1) == Some(true) {
                            left -= 1;
                        }
                        let end = left;
                        while left > 0 && blank_at(row, left - 1) == Some(false) {
                            left -= 1;
                        }
                        (left < end).then_some(left)
                    })
                    .collect();
                let eligible = neighbors
                    .iter()
                    .filter(|row| blank_at(row, x).is_some())
                    .count();
                let starts = neighbors
                    .iter()
                    .filter(|row| {
                        blank_at(row, x) == Some(false)
                            && x > 0
                            && blank_at(row, x - 1) == Some(true)
                    })
                    .count();
                eligible >= 3
                    && starts >= 3
                    && starts * 100 >= eligible * 85
                    && starts_left.len() >= 2
            });
            if width >= 2 && gap > coarse_start {
                coarse_fields.push(coarse_start..gap);
                coarse_start = i;
            }
            if (width >= 2 || aligned) && gap > start {
                fields.push(start..gap);
                start = i;
            }
        }
        if start < content.end {
            fields.push(start..content.end)
        }
        if coarse_start < content.end {
            coarse_fields.push(coarse_start..content.end);
        }
        if fields.len() >= 2 {
            // Retain broader values (e.g. a date plus time) as alternatives,
            // while confirmed individual fields rank ahead of them.
            if coarse_fields.len() >= 2 {
                spans.extend(coarse_fields.into_iter().map(|range| (range, Kind::Value)));
            }
            spans.extend(fields.into_iter().map(|range| (range, Kind::TableField)))
        }
    }
    // Quotes shield their contents from bracket parsing. Backslash only escapes
    // quotes/backslash, so Windows path separators are preserved.
    let mut stack: Vec<(char, usize)> = vec![];
    let mut quote: Option<(char, usize)> = None;
    let mut i = content.start;
    while i < content.end {
        let ch = symbol(&line.cells[i]);
        if let Some((q, start)) = quote {
            if ch == Some('\\')
                && i + 1 < content.end
                && matches!(symbol(&line.cells[i + 1]), Some(c) if c == q || c == '\\')
            {
                i += 2;
                continue;
            }
            if ch == Some(q) {
                // SQL/PowerShell doubled quotes are escaped interior quotes.
                if i + 1 < content.end && symbol(&line.cells[i + 1]) == Some(q) {
                    i += 2;
                    continue;
                }
                spans.push((start + 1..i, Kind::Delimited));
                quote = None;
            }
        } else if matches!(ch, Some('\'' | '"')) {
            let embedded_apostrophe = ch == Some('\'')
                && i > content.start
                && i + 1 < content.end
                && !whitespace(&line.cells[i - 1])
                && !whitespace(&line.cells[i + 1]);
            if !embedded_apostrophe {
                quote = Some((ch.unwrap(), i))
            }
        } else if let Some(ch) = ch {
            match ch {
                '(' => stack.push((')', i)),
                '[' => stack.push((']', i)),
                '{' => stack.push(('}', i)),
                '<' => stack.push(('>', i)),
                ')' | ']' | '}' | '>' => {
                    if stack.last().is_some_and(|&(close, _)| close == ch) {
                        let (_, start) = stack.pop().unwrap();
                        spans.push((start + 1..i, Kind::Delimited));
                    } else {
                        stack.clear()
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    let mut i = content.start;
    while i < content.end {
        if whitespace(&line.cells[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < content.end && !whitespace(&line.cells[i]) {
            i += 1
        }
        spans.push((start..i, Kind::Value));
    }
    let mut candidates = vec![];
    let mut add = |mut range: Range<usize>, kind| {
        while range.start < range.end && whitespace(&line.cells[range.start]) {
            range.start += 1
        }
        while range.start < range.end && whitespace(&line.cells[range.end - 1]) {
            range.end -= 1
        }
        if range.is_empty() {
            return;
        }
        let columns = line.cells[range.start].columns.start..line.cells[range.end - 1].columns.end;
        if prompts
            .iter()
            .any(|prompt| columns.start < prompt.end && prompt.start < columns.end)
        {
            return;
        }
        let text: String = line.cells[range]
            .iter()
            .map(|cell| cell.text.as_str())
            .collect();
        if kind != Kind::TableField && text.chars().filter(|c| !c.is_whitespace()).count() < 3 {
            return;
        }
        candidates.push(Candidate {
            text,
            row: line.row,
            columns,
            kind,
        });
    };
    for (range, kind) in spans {
        add(range.clone(), kind);
        let mut clean = range.clone();
        while clean.start < clean.end
            && matches!(
                symbol(&line.cells[clean.start]),
                Some('"' | '\'' | '(' | '[' | '{' | '<')
            )
        {
            clean.start += 1
        }
        while clean.start < clean.end
            && matches!(
                symbol(&line.cells[clean.end - 1]),
                Some('"' | '\'' | ')' | ']' | '}' | '>' | ',' | ';' | ':' | '.' | '!' | '?')
            )
        {
            clean.end -= 1
        }
        add(clean.clone(), kind);
        // Slash-separated path/URL components and key=value arguments supplement
        // complete values; periods remain intact so filenames stay searchable.
        if line.cells[clean.clone()]
            .iter()
            .any(|c| matches!(symbol(c), Some('/' | '\\' | '=')))
        {
            let mut start = clean.start;
            for index in clean.clone() {
                if matches!(
                    symbol(&line.cells[index]),
                    Some('/' | '\\' | '=' | '?' | '&' | '#')
                ) {
                    add(start..index, Kind::Fragment);
                    start = index + 1;
                }
            }
            add(start..clean.end, Kind::Fragment);
        }
    }
    candidates.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then(a.columns.start.cmp(&b.columns.start))
            .then_with(|| b.columns.len().cmp(&a.columns.len()))
    });
    let mut seen = HashSet::new();
    candidates.retain(|candidate| seen.insert(candidate.text.clone()));
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    fn line(text: &str, row: u64) -> Line {
        Line {
            row,
            cells: text
                .chars()
                .enumerate()
                .map(|(i, c)| Cell {
                    text: c.to_string(),
                    columns: i..i + 1,
                })
                .collect(),
            wraps_to_next: false,
        }
    }
    #[test]
    fn table_fields_preserve_spaces_and_rank_before_tokens() {
        let rows = [
            line("alpha   5/6/2026 7:00 PM   first file.txt", 0),
            line("bravo   6/7/2026 8:00 AM   other file.txt", 1),
            line("delta   7/8/2026 9:00 PM   third file.txt", 2),
        ];
        let result = split(&rows, 1, &[]);
        let fields: Vec<_> = result
            .iter()
            .filter(|c| c.kind == Kind::TableField)
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(fields, ["bravo", "6/7/2026 8:00 AM", "other file.txt"]);
        assert!(result.iter().any(|c| c.text == "6/7/2026 8:00 AM"));
        assert_eq!(result[0].kind, Kind::TableField);
    }
    #[test]
    fn single_cell_gutters_split_right_aligned_size_from_filename_with_spaces() {
        let rows = [
            line(
                &format!("-a---   {:>12} {}", "61504556", "TE.ProcessHost.exe.dmp"),
                0,
            ),
            line(&format!("-a---   {:>12} {}", "0", "Test file tmp.txt"), 1),
            line(
                &format!("-a---   {:>12} {}", "5345887", "vt-render-test.txt"),
                2,
            ),
            line(
                &format!(
                    "-a---   {:>12} {}",
                    "10237255694", "WindowsTerminal.exe.dmp"
                ),
                3,
            ),
        ];
        let result = split(&rows, 1, &[]);
        let fields: Vec<_> = result
            .iter()
            .filter(|c| c.kind == Kind::TableField)
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(fields, ["-a---", "0", "Test file tmp.txt"]);
        let size = result.iter().find(|c| c.text == "0").unwrap();
        let filename = result
            .iter()
            .find(|c| c.text == "Test file tmp.txt")
            .unwrap();
        assert_eq!(size.columns, 19..20);
        assert_eq!(filename.columns, 21..38);
        assert!(
            result.iter().position(|c| c == filename).unwrap()
                < result
                    .iter()
                    .position(|c| c.text == "0 Test file tmp.txt")
                    .unwrap()
        );
    }

    #[test]
    fn nested_quotes_escapes_and_prompt_exclusion() {
        let rows = [line(
            r#"PS> run (outer [inner value]) "say \"hello\"" 'it''s fine'"#,
            12,
        )];
        let result = split(&rows, 0, &[0..4]);
        for value in [
            "outer [inner value]",
            "inner value",
            r#"say \"hello\""#,
            "it''s fine",
        ] {
            assert!(
                result.iter().any(|c| c.text == value),
                "{value}: {result:?}"
            );
        }
        assert!(!result.iter().any(|c| c.text == "PS>"));
        assert!(result.iter().all(|c| c.row == 12));
    }
    #[test]
    fn paths_urls_trimmed_ranges_and_duplicate_priority() {
        let rows = [line(
            r#""C:\src\project\file.rs" https://host.test/path/file.txt, file.rs"#,
            2,
        )];
        let result = split(&rows, 0, &[]);
        for value in [
            r"C:\src\project\file.rs",
            "project",
            "file.rs",
            "https://host.test/path/file.txt",
        ] {
            assert!(result.iter().any(|c| c.text == value), "{value}");
        }
        let file = result.iter().find(|c| c.text == "file.rs").unwrap();
        assert_eq!(file.kind, Kind::Value); // standalone value outranks path fragment
        let url = result
            .iter()
            .find(|c| c.text == "https://host.test/path/file.txt")
            .unwrap();
        let text: String = rows[0].cells[url.columns.clone()]
            .iter()
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(text, url.text);
    }
    #[test]
    fn graphemes_and_wide_cells_keep_exact_host_ranges() {
        let row = Line {
            row: 7,
            wraps_to_next: false,
            cells: vec![
                Cell {
                    text: "界".into(),
                    columns: 0..2,
                },
                Cell {
                    text: "e\u{301}".into(),
                    columns: 2..3,
                },
                Cell {
                    text: "x".into(),
                    columns: 3..4,
                },
            ],
        };
        let result = split(&[row], 0, &[]);
        assert_eq!(result[0].text, "界e\u{301}x");
        assert_eq!(result[0].columns, 0..4);
        assert!(split(&[], 0, &[]).is_empty());
    }
}
