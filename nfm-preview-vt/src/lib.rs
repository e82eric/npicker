//! Standalone Ghostty VT preview parsing; no terminal host, PTY or GPU dependency.
use std::{error::Error, fmt};

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Cell {
    pub codepoints: [u32; 8],
    pub foreground: u32,
    pub background: u32,
    pub underline_color: u32,
    pub length: u8,
    /// bold=1, italic=2, strike=4, inverse=8, invisible=16, faint=32.
    pub flags: u8,
    pub underline: u8,
    reserved: u8,
}
#[derive(Clone, Debug)]
pub struct Grid {
    pub columns: u16,
    pub rows: u16,
    pub total_rows: usize,
    pub cells: Vec<Cell>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    InvalidDimensions,
    InputTooLarge,
    NativeFailure,
    StaleRevision,
    InvalidSelection,
}
impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidDimensions => "preview dimensions must contain 1..=1048576 cells",
            Self::InputTooLarge => "ANSI preview exceeds configured byte limit",
            Self::NativeFailure => "Ghostty VT failed to parse preview",
            Self::StaleRevision => "selection belongs to a different document revision",
            Self::InvalidSelection => "selection is outside the document",
        })
    }
}
impl Error for ParseError {}
unsafe extern "C" {
    fn nfm_preview_vt_new(
        bytes: *const u8,
        length: usize,
        columns: u16,
        rows: u16,
        max_scrollback_lines: usize,
        total_rows: *mut usize,
    ) -> *mut std::ffi::c_void;
    fn nfm_preview_vt_free(terminal: *mut std::ffi::c_void);
    fn nfm_preview_vt_viewport(
        terminal: *mut std::ffi::c_void,
        columns: u16,
        rows: u16,
        first_row: usize,
        foreground: u32,
        background: u32,
        cells: *mut Cell,
    ) -> bool;
    fn nfm_preview_vt_text_row(
        terminal: *mut std::ffi::c_void,
        row: usize,
        columns: u16,
        codepoints: *mut u32,
        capacity: usize,
        offsets: *mut usize,
        widths: *mut u8,
        required: *mut usize,
        wraps: *mut bool,
    ) -> bool;
}

/// A physical terminal row. `columns` maps every UTF-8 byte to its starting
/// cell; `widths` records terminal cell widths (zero for wide-cell spacers).
/// Blank cells become spaces. Wide-cell spacers do not become extra text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TextRow {
    pub text: String,
    pub columns: Vec<u32>,
    pub widths: Vec<u8>,
    pub wraps_to_next: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CellPosition {
    pub row: usize,
    pub column: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionKind {
    Characterwise,
    Linewise,
    Rectangle,
}

/// Inclusive cell endpoints tied to one immutable document layout.
pub struct Selection {
    pub revision: u64,
    pub start: CellPosition,
    pub end: CellPosition,
    pub kind: SelectionKind,
    pub trim: bool,
}

/// Immutable ANSI terminal document with bounded scrollback (by default 8192 lines plus
/// the active screen, subject to Ghostty's page pruning and byte limits).
/// Coordinates are absolute physical rows in the retained terminal screen.
/// Each parse receives a distinct revision. Reparse to change terminal width;
/// keep this document alive to freeze layout while copying.
pub struct AnsiDocument {
    terminal: std::ptr::NonNull<std::ffi::c_void>,
    columns: u16,
    total_rows: usize,
    revision: u64,
}

// The terminal has no host callbacks or thread-affine GPU/PTY resources.
// Moving ownership is safe. Access requires &mut self, so native operations
// never run concurrently. The type intentionally does not implement Sync.
unsafe impl Send for AnsiDocument {}

impl Drop for AnsiDocument {
    fn drop(&mut self) {
        unsafe { nfm_preview_vt_free(self.terminal.as_ptr()) }
    }
}

impl AnsiDocument {
    /// Extract plain text without ANSI escapes. Soft wraps join in ordinary
    /// selections; rectangle selections retain physical row boundaries.
    /// Selecting either half of a wide character copies its full grapheme.
    pub fn selection_text(&mut self, selection: &Selection) -> Result<String, ParseError> {
        if selection.revision != self.revision {
            return Err(ParseError::StaleRevision);
        }
        let (start, end) = if selection.start <= selection.end {
            (selection.start, selection.end)
        } else {
            (selection.end, selection.start)
        };
        if end.row >= self.total_rows
            || start.column >= u32::from(self.columns)
            || end.column >= u32::from(self.columns)
        {
            return Err(ParseError::InvalidSelection);
        }
        let mut result = String::new();
        for y in start.row..=end.row {
            let row = self.text_row(y)?.ok_or(ParseError::InvalidSelection)?;
            let (left, right) = match selection.kind {
                SelectionKind::Linewise => (0, u32::from(self.columns) - 1),
                SelectionKind::Rectangle => {
                    (start.column.min(end.column), start.column.max(end.column))
                }
                SelectionKind::Characterwise => (
                    if y == start.row { start.column } else { 0 },
                    if y == end.row {
                        end.column
                    } else {
                        u32::from(self.columns) - 1
                    },
                ),
            };
            let mut line = String::new();
            for (byte, ch) in row.text.char_indices() {
                let x = row.columns[byte];
                let width = u32::from(row.widths[x as usize]);
                if x <= right && x + width > left {
                    line.push(ch);
                }
            }
            // Padding at a soft wrap belongs to the logical line, so preserve
            // it unless this is the final row or a rectangular selection.
            if selection.trim
                && (!row.wraps_to_next
                    || y == end.row
                    || selection.kind == SelectionKind::Rectangle)
            {
                line.truncate(line.trim_end_matches(' ').len());
            }
            result.push_str(&line);
            if y != end.row && (!row.wraps_to_next || selection.kind == SelectionKind::Rectangle) {
                result.push('\n');
            }
        }
        if selection.kind == SelectionKind::Linewise {
            result.push('\n');
        }
        Ok(result)
    }

    /// Parse raw terminal bytes; bare LF retains terminal LF semantics.
    /// `rows` fixes the original active-screen height independently of later
    /// viewport requests. Use CRLF for ordinary line-oriented preview text.
    pub fn parse(bytes: &[u8], columns: u16, rows: u16) -> Result<Self, ParseError> {
        Self::parse_with_limits(bytes, columns, rows, 20 * 1024 * 1024, 8192)
    }

    /// Parse with an input byte bound and a retained physical scrollback line bound.
    pub fn parse_with_limits(
        bytes: &[u8],
        columns: u16,
        rows: u16,
        max_bytes: usize,
        max_scrollback_lines: usize,
    ) -> Result<Self, ParseError> {
        validate_dimensions(columns, rows)?;
        if bytes.len() > max_bytes {
            return Err(ParseError::InputTooLarge);
        }
        let mut total_rows = 0;
        let terminal = std::ptr::NonNull::new(unsafe {
            nfm_preview_vt_new(
                bytes.as_ptr(),
                bytes.len(),
                columns,
                rows,
                max_scrollback_lines,
                &mut total_rows,
            )
        })
        .ok_or(ParseError::NativeFailure)?;
        static REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Ok(Self {
            terminal,
            columns,
            total_rows,
            revision: REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        })
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn columns(&self) -> u16 {
        self.columns
    }
    pub fn total_rows(&self) -> usize {
        self.total_rows
    }
    pub fn first_row(&self, requested: usize, rows: u16) -> usize {
        requested.min(self.total_rows.saturating_sub(usize::from(rows)))
    }

    /// Read styled cells without changing the document's width or revision.
    /// Requests clamp to the last full viewport, as the legacy parse API did.
    pub fn viewport(
        &mut self,
        first_row: usize,
        rows: u16,
        foreground: u32,
        background: u32,
    ) -> Result<Grid, ParseError> {
        let count = validate_dimensions(self.columns, rows)?;
        let mut cells = vec![Cell::default(); count];
        if !unsafe {
            nfm_preview_vt_viewport(
                self.terminal.as_ptr(),
                self.columns,
                rows,
                self.first_row(first_row, rows),
                foreground,
                background,
                cells.as_mut_ptr(),
            )
        } {
            return Err(ParseError::NativeFailure);
        }
        Ok(Grid {
            columns: self.columns,
            rows,
            total_rows: self.total_rows,
            cells,
        })
    }

    /// Read arbitrary rows, including outside the displayed viewport.
    /// Returns None outside the document; preserves full combining clusters.
    pub fn text_row(&mut self, row: usize) -> Result<Option<TextRow>, ParseError> {
        if row >= self.total_rows {
            return Ok(None);
        }
        let mut offsets = vec![0; usize::from(self.columns) + 1];
        let mut widths = vec![0; usize::from(self.columns)];
        let mut required = 0;
        let mut wraps = false;
        if !unsafe {
            nfm_preview_vt_text_row(
                self.terminal.as_ptr(),
                row,
                self.columns,
                std::ptr::null_mut(),
                0,
                offsets.as_mut_ptr(),
                widths.as_mut_ptr(),
                &mut required,
                &mut wraps,
            )
        } {
            return Err(ParseError::NativeFailure);
        }
        let mut codepoints = vec![0; required];
        if !unsafe {
            nfm_preview_vt_text_row(
                self.terminal.as_ptr(),
                row,
                self.columns,
                codepoints.as_mut_ptr(),
                codepoints.len(),
                offsets.as_mut_ptr(),
                widths.as_mut_ptr(),
                &mut required,
                &mut wraps,
            )
        } {
            return Err(ParseError::NativeFailure);
        }
        let mut text = String::new();
        let mut columns = Vec::new();
        for x in 0..usize::from(self.columns) {
            if widths[x] == 0 {
                continue;
            }
            let before = text.len();
            for &cp in &codepoints[offsets[x]..offsets[x + 1]] {
                text.push(char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER));
            }
            if text.len() == before {
                text.push(' ');
            }
            columns.resize(text.len(), x as u32);
        }
        Ok(Some(TextRow {
            text,
            columns,
            widths,
            wraps_to_next: wraps,
        }))
    }
}

fn validate_dimensions(columns: u16, rows: u16) -> Result<usize, ParseError> {
    let count = usize::from(columns) * usize::from(rows);
    if count == 0 || count > 1_048_576 {
        return Err(ParseError::InvalidDimensions);
    }
    Ok(count)
}

/// Compatibility wrapper for callers needing only a single viewport.
pub fn parse(
    bytes: &[u8],
    columns: u16,
    rows: u16,
    scroll_offset: usize,
    foreground: u32,
    background: u32,
) -> Result<Grid, ParseError> {
    AnsiDocument::parse(bytes, columns, rows)?.viewport(scroll_offset, rows, foreground, background)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn configured_byte_and_scrollback_limits_are_honored() {
        assert_eq!(
            AnsiDocument::parse_with_limits(b"abc", 80, 4, 2, 100).err(),
            Some(ParseError::InputTooLarge)
        );
        let text: String = (0..1200).map(|row| format!("row{row:04}\r\n")).collect();
        let doc =
            AnsiDocument::parse_with_limits(text.as_bytes(), 160, 12, text.len(), 20).unwrap();
        assert!(doc.total_rows() < 1200);
    }

    #[test]
    fn long_preview_retains_output_relative_rows_for_centering() {
        let text: String = (0..1200).map(|row| format!("row{row:04}\r\n")).collect();
        let mut doc = AnsiDocument::parse(text.as_bytes(), 160, 12).unwrap();
        assert!(doc.total_rows() >= 1200);
        assert_eq!(doc.text_row(0).unwrap().unwrap().text.trim_end(), "row0000");
        assert_eq!(
            doc.text_row(288).unwrap().unwrap().text.trim_end(),
            "row0288"
        );
    }

    #[test]
    fn retained_rows_wrap_mapping_selection_and_revision() {
        let mut doc = AnsiDocument::parse("ab猫e\u{301}fg\r\nlast".as_bytes(), 6, 2).unwrap();
        let row = doc.text_row(0).unwrap().unwrap();
        assert!(row.wraps_to_next);
        assert!(row.text.contains("猫"));
        assert!(row.text.contains("e\u{301}"));
        assert_eq!(row.columns.len(), row.text.len());
        assert_eq!(row.widths[2], 2);
        assert_eq!(row.widths[3], 0);
        assert_eq!(row.columns[row.text.find('猫').unwrap()], 2);
        let revision = doc.revision();
        let selection = Selection {
            revision,
            start: CellPosition { row: 0, column: 3 },
            end: CellPosition { row: 0, column: 3 },
            kind: SelectionKind::Characterwise,
            trim: true,
        };
        assert_eq!(doc.selection_text(&selection).unwrap(), "猫");
        let selection = Selection {
            revision,
            start: CellPosition { row: 0, column: 0 },
            end: CellPosition { row: 1, column: 0 },
            kind: SelectionKind::Characterwise,
            trim: true,
        };
        assert_eq!(doc.selection_text(&selection).unwrap(), "ab猫e\u{301}fg");
        doc.viewport(1, 1, 0xffffff, 0).unwrap();
        assert_eq!(doc.revision(), revision);
        assert_eq!(doc.text_row(0).unwrap().unwrap(), row);
        assert!(doc.text_row(doc.total_rows()).unwrap().is_none());
        let mut other = AnsiDocument::parse(b"other", 6, 2).unwrap();
        assert_ne!(other.revision(), revision);
        assert_eq!(
            other.selection_text(&selection),
            Err(ParseError::StaleRevision)
        );
    }

    #[test]
    fn full_graphemes_rectangles_and_linewise_text() {
        let long = format!("a{}", "\u{301}".repeat(12));
        let mut doc = AnsiDocument::parse(format!("{long}\r\nxyz").as_bytes(), 6, 2).unwrap();
        assert!(doc.text_row(0).unwrap().unwrap().text.starts_with(&long));
        let mut selection = Selection {
            revision: doc.revision(),
            start: CellPosition { row: 0, column: 0 },
            end: CellPosition { row: 1, column: 1 },
            kind: SelectionKind::Rectangle,
            trim: true,
        };
        assert_eq!(
            doc.selection_text(&selection).unwrap(),
            format!("{long}\nxy")
        );
        selection.kind = SelectionKind::Linewise;
        assert_eq!(
            doc.selection_text(&selection).unwrap(),
            format!("{long}\nxyz\n")
        );
    }
    #[test]
    fn ansi_colors_styles_unicode_and_scroll() {
        let grid = parse(
            "\x1b[31;1m猫\x1b[0m\r\nnext\r\nlast".as_bytes(),
            12,
            2,
            0,
            0xffffff,
            0x282828,
        )
        .unwrap();
        assert_eq!(grid.cells[0].codepoints[0], '猫' as u32);
        assert_ne!(grid.cells[0].foreground, 0xffffff);
        assert_ne!(grid.cells[0].flags & 1, 0);
        assert_eq!(grid.cells[12].codepoints[0], 'n' as u32);
        let scrolled = parse(b"one\r\ntwo\r\nthree\r\nfour", 12, 2, 2, 0xffffff, 0).unwrap();
        assert_eq!(scrolled.cells[0].codepoints[0], 't' as u32);
        assert_eq!(scrolled.cells[12].codepoints[0], 'f' as u32);
    }
    #[test]
    fn rejects_unbounded_allocations() {
        assert_eq!(
            parse(b"", 0, 1, 0, 0, 0).unwrap_err(),
            ParseError::InvalidDimensions
        );
        assert_eq!(
            parse(b"", u16::MAX, u16::MAX, 0, 0, 0).unwrap_err(),
            ParseError::InvalidDimensions
        );
    }
}

pub mod styled;
