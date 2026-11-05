//! Pure-Rust ANSI parsing into a `StyledDocument`. `vte` tokenizes escape
//! sequences; a small line-oriented screen applies the subset of terminal
//! behavior that command output relies on: SGR styles, carriage-return
//! overwrite, erase, relative cursor motion, tabs and cursor save/restore.
//!
//! Lines have no fixed width; `StyledDocument::layout` wraps them. A bounded
//! window of recent lines stays editable so cursor-up redraws (multi-line
//! progress output) collapse the way they would in a terminal. Full-screen
//! applications (alternate screen, scroll regions) are out of scope.
use crate::styled_document::{ColumnStyleRun, DocumentBuilder, Style, StyledDocument};
use egui::Color32;
use std::collections::VecDeque;
use std::ops::Range;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Lines that cursor motion can still reach before they are committed.
const WINDOW: usize = 256;
/// Bounds cursor jumps so a hostile CHA/CUF cannot allocate unbounded rows.
const MAX_COLUMNS: usize = 16 * 1024;
const TAB: usize = 8;

const BOLD: u8 = 1;
const ITALIC: u8 = 2;
const STRIKE: u8 = 4;
const INVERSE: u8 = 8;
const HIDDEN: u8 = 16;
const DIM: u8 = 32;

/// Parse terminal output. LF starts a new line at column zero, so bare LF
/// and CRLF are equivalent. Output past `max_lines` is dropped; the head is
/// kept so row numbers stay stable for centering.
pub(crate) fn parse(
    bytes: &[u8],
    max_bytes: usize,
    max_lines: usize,
) -> Result<Arc<StyledDocument>, String> {
    if bytes.len() > max_bytes {
        return Err("ANSI preview exceeds configured byte limit".into());
    }
    let mut screen = Screen::new(max_lines);
    let mut parser = vte::Parser::new();
    for chunk in bytes.chunks(64 * 1024) {
        if screen.done() {
            break;
        }
        parser.advance(&mut screen, chunk);
    }
    screen.finish()
}

/// The 256-color palette Ghostty uses by default, so previews keep the colors
/// they had when Ghostty VT parsed them.
fn palette(index: u16) -> Color32 {
    const BASIC: [(u8, u8, u8); 16] = [
        (0x1D, 0x1F, 0x21),
        (0xCC, 0x66, 0x66),
        (0xB5, 0xBD, 0x68),
        (0xF0, 0xC6, 0x74),
        (0x81, 0xA2, 0xBE),
        (0xB2, 0x94, 0xBB),
        (0x8A, 0xBE, 0xB7),
        (0xC5, 0xC8, 0xC6),
        (0x66, 0x66, 0x66),
        (0xD5, 0x4E, 0x53),
        (0xB9, 0xCA, 0x4A),
        (0xE7, 0xC5, 0x47),
        (0x7A, 0xA6, 0xDA),
        (0xC3, 0x97, 0xD8),
        (0x70, 0xC0, 0xB1),
        (0xEA, 0xEA, 0xEA),
    ];
    let index = index.min(255) as u8;
    let (r, g, b) = match index {
        0..=15 => BASIC[index as usize],
        16..=231 => {
            let index = index - 16;
            let component = |value: u8| if value == 0 { 0 } else { 55 + value * 40 };
            (
                component(index / 36),
                component(index % 36 / 6),
                component(index % 6),
            )
        }
        232..=255 => {
            let value = 8 + (index - 232) * 10;
            (value, value, value)
        }
    };
    Color32::from_rgb(r, g, b)
}

#[derive(Clone)]
struct Cell {
    /// NUL marks an unwritten cell.
    ch: char,
    /// Remaining code points of the grapheme cluster.
    extra: Option<Box<str>>,
    /// Zero marks the trailing half of a wide grapheme.
    width: u8,
    style: Style,
}
impl Cell {
    fn blank() -> Self {
        Self {
            ch: '\0',
            extra: None,
            width: 1,
            style: Style::default(),
        }
    }
    fn spacer(style: Style) -> Self {
        Self {
            ch: '\0',
            extra: None,
            width: 0,
            style,
        }
    }
    fn is_blank(&self) -> bool {
        self.ch == '\0' && self.width != 0
    }
}

#[derive(Default)]
struct Row {
    cells: Vec<Cell>,
    /// Background an erase left at the end of the line; layouts extend it to
    /// the right edge.
    fill: Option<Style>,
}
impl Row {
    fn ensure(&mut self, len: usize) {
        if self.cells.len() < len {
            self.cells.resize_with(len, Cell::blank);
        }
    }
    /// Blank any wide grapheme that `start..end` covers only partially.
    fn clear_overlap(&mut self, start: usize, end: usize) {
        let cells = &mut self.cells;
        if start < cells.len() && cells[start].width == 0 {
            let mut owner = start;
            while owner > 0 && cells[owner].width == 0 {
                owner -= 1;
            }
            cells[owner..start].fill_with(Cell::blank);
        }
        let mut x = end;
        while x < cells.len() && cells[x].width == 0 {
            cells[x] = Cell::blank();
            x += 1;
        }
    }
    fn write(&mut self, x: usize, cell: Cell) {
        let width = usize::from(cell.width.max(1));
        self.ensure(x + width);
        self.clear_overlap(x, x + width);
        let style = cell.style;
        self.cells[x] = cell;
        self.cells[x + 1..x + width].fill_with(|| Cell::spacer(style));
    }
    fn blank(&mut self, range: Range<usize>) {
        let end = range.end.min(self.cells.len());
        if range.start < end {
            self.clear_overlap(range.start, end);
            self.cells[range.start..end].fill_with(Cell::blank);
        }
    }
}

/// Reused buffers for committing rows. Unwritten interior cells become
/// spaces; unwritten trailing cells are omitted.
#[derive(Default)]
struct Output {
    builder: DocumentBuilder,
    lines: usize,
    text: String,
    columns: Vec<u32>,
    runs: Vec<ColumnStyleRun>,
    error: Option<String>,
}
impl Output {
    fn push(&mut self, row: &Row) {
        let end = row
            .cells
            .iter()
            .rposition(|cell| !cell.is_blank())
            .map_or(0, |x| x + 1);
        self.text.clear();
        self.columns.clear();
        self.runs.clear();
        for (x, cell) in row.cells[..end].iter().enumerate() {
            if cell.width == 0 {
                continue;
            }
            if cell.is_blank() {
                self.text.push(' ');
            } else {
                self.text.push(cell.ch);
                if let Some(extra) = &cell.extra {
                    self.text.push_str(extra);
                }
            }
            self.columns.resize(self.text.len(), x as u32);
            if cell.style != Style::default() {
                let span = x as u32..(x + usize::from(cell.width)) as u32;
                match self.runs.last_mut() {
                    Some(run) if run.style == cell.style && run.columns.end == span.start => {
                        run.columns.end = span.end;
                    }
                    _ => self.runs.push(ColumnStyleRun {
                        columns: span,
                        style: cell.style,
                    }),
                }
            }
        }
        let pushed =
            self.builder
                .push_row_filled(&self.text, &self.columns, false, &self.runs, row.fill);
        if let Err(error) = pushed {
            self.error.get_or_insert(error);
        }
        self.lines += 1;
    }
}

struct Screen {
    rows: VecDeque<Row>,
    out: Output,
    /// Absolute line number of `rows[0]`.
    base: usize,
    row: usize,
    col: usize,
    style: Style,
    /// Absolute row and column for DECSC/DECRC and CSI s/u.
    saved: (usize, usize),
    /// Last printed cell; a following code point may extend its grapheme.
    last: Option<(usize, usize)>,
    max_lines: usize,
}

impl Screen {
    fn new(max_lines: usize) -> Self {
        Self {
            rows: VecDeque::from([Row::default()]),
            out: Output::default(),
            base: 0,
            row: 0,
            col: 0,
            style: Style::default(),
            saved: (0, 0),
            last: None,
            max_lines,
        }
    }
    fn done(&self) -> bool {
        self.out.lines >= self.max_lines
    }
    fn finish(mut self) -> Result<Arc<StyledDocument>, String> {
        while let Some(row) = self.rows.pop_front() {
            if self.done() {
                break;
            }
            self.out.push(&row);
        }
        match self.out.error {
            Some(error) => Err(error),
            None => Ok(self.out.builder.finish()),
        }
    }
    fn current(&mut self) -> &mut Row {
        &mut self.rows[self.row]
    }
    fn commit(&mut self) {
        while self.rows.len() > WINDOW {
            let row = self.rows.pop_front().unwrap();
            self.base += 1;
            self.row = self.row.saturating_sub(1);
            if !self.done() {
                self.out.push(&row);
            }
        }
    }
    fn line_feed(&mut self) {
        self.row += 1;
        self.col = 0;
        if self.row == self.rows.len() {
            self.rows.push_back(Row::default());
            self.commit();
        }
    }
    fn set_row(&mut self, row: usize) {
        while self.rows.len() <= row && self.rows.len() < WINDOW {
            self.rows.push_back(Row::default());
        }
        self.row = row.min(self.rows.len() - 1);
    }
    fn set_col(&mut self, col: usize) {
        self.col = col.min(MAX_COLUMNS);
    }
    fn restore(&mut self) {
        let (row, col) = self.saved;
        self.row = row.saturating_sub(self.base).min(self.rows.len() - 1);
        self.col = col;
    }
    fn fill(&self) -> Option<Style> {
        self.style.background.map(|background| Style {
            background: Some(background),
            ..Style::default()
        })
    }
    fn extend_grapheme(&mut self, row: usize, x: usize, c: char) -> bool {
        let Some(cell) = self.rows.get_mut(row).and_then(|r| r.cells.get_mut(x)) else {
            return false;
        };
        if cell.width == 0 || cell.ch == '\0' {
            return false;
        }
        let mut text = String::from(cell.ch);
        if let Some(extra) = &cell.extra {
            text.push_str(extra);
        }
        text.push(c);
        if text.graphemes(true).nth(1).is_some() {
            return false;
        }
        cell.extra = Some(text[cell.ch.len_utf8()..].into());
        let old = usize::from(cell.width);
        let width = UnicodeWidthStr::width(text.as_str()).clamp(1, 2);
        if width > old {
            cell.width = width as u8;
            let style = cell.style;
            let row = &mut self.rows[row];
            row.ensure(x + width);
            row.clear_overlap(x + old, x + width);
            row.cells[x + old..x + width].fill_with(|| Cell::spacer(style));
            self.col = x + width;
        }
        true
    }
    fn erase_display(&mut self, mode: u16) {
        match mode {
            0 => {
                self.erase_line(0);
                for row in self.rows.range_mut(self.row + 1..) {
                    *row = Row::default();
                }
            }
            1 => {
                for row in self.rows.range_mut(..self.row) {
                    *row = Row::default();
                }
                self.erase_line(1);
            }
            2 | 3 => {
                for row in &mut self.rows {
                    *row = Row::default();
                }
            }
            _ => {}
        }
    }
    fn erase_line(&mut self, mode: u16) {
        let (col, fill) = (self.col, self.fill());
        let row = self.current();
        match mode {
            0 => {
                if col < row.cells.len() {
                    row.clear_overlap(col, row.cells.len());
                    row.cells.truncate(col);
                }
                row.fill = fill;
            }
            1 => row.blank(0..col + 1),
            2 => {
                row.cells.clear();
                row.fill = fill;
            }
            _ => {}
        }
    }
    fn sgr(&mut self, params: &vte::Params) {
        let style = &mut self.style;
        let mut iter = params.iter();
        while let Some(param) = iter.next() {
            let Some(&code) = param.first() else {
                continue;
            };
            let mut set = |flag: u8, on: bool| {
                if on {
                    style.flags |= flag;
                } else {
                    style.flags &= !flag;
                }
            };
            match code {
                0 => *style = Style::default(),
                1 => set(BOLD, true),
                2 => set(DIM, true),
                3 => set(ITALIC, true),
                4 => style.underline = param.get(1).is_none_or(|&kind| kind != 0),
                7 => set(INVERSE, true),
                8 => set(HIDDEN, true),
                9 => set(STRIKE, true),
                21 => style.underline = true,
                22 => set(BOLD | DIM, false),
                23 => set(ITALIC, false),
                24 => style.underline = false,
                27 => set(INVERSE, false),
                28 => set(HIDDEN, false),
                29 => set(STRIKE, false),
                30..=37 => style.foreground = Some(palette(code - 30)),
                38 => {
                    if let Some(color) = extended_color(param, &mut iter) {
                        style.foreground = Some(color);
                    }
                }
                39 => style.foreground = None,
                40..=47 => style.background = Some(palette(code - 40)),
                48 => {
                    if let Some(color) = extended_color(param, &mut iter) {
                        style.background = Some(color);
                    }
                }
                49 => style.background = None,
                // Underline color: consume its arguments; previews draw
                // underlines in the foreground color.
                58 => {
                    extended_color(param, &mut iter);
                }
                90..=97 => style.foreground = Some(palette(code - 90 + 8)),
                100..=107 => style.background = Some(palette(code - 100 + 8)),
                _ => {}
            }
        }
    }
}

/// `38;5;n`, `38;2;r;g;b` and the colon forms `38:5:n`, `38:2:r:g:b` and
/// `38:2:colorspace:r:g:b`.
fn extended_color<'a>(
    param: &[u16],
    rest: &mut impl Iterator<Item = &'a [u16]>,
) -> Option<Color32> {
    let byte = |value: u16| value.min(255) as u8;
    if param.len() > 1 {
        return match param[1] {
            5 => param.get(2).map(|&index| palette(index)),
            2 => {
                let rgb = if param.len() >= 6 {
                    &param[3..6]
                } else {
                    param.get(2..5)?
                };
                Some(Color32::from_rgb(byte(rgb[0]), byte(rgb[1]), byte(rgb[2])))
            }
            _ => None,
        };
    }
    let mut next = || rest.next().and_then(|value| value.first().copied());
    match next()? {
        5 => Some(palette(next()?)),
        2 => Some(Color32::from_rgb(
            byte(next()?),
            byte(next()?),
            byte(next()?),
        )),
        _ => None,
    }
}

fn arg(params: &vte::Params, index: usize, default: usize) -> usize {
    params
        .iter()
        .nth(index)
        .and_then(|param| param.first().copied())
        .filter(|&value| value != 0)
        .map_or(default, usize::from)
}

impl vte::Perform for Screen {
    fn print(&mut self, c: char) {
        if self.done() {
            return;
        }
        if let Some((row, x)) = self.last {
            if row == self.row && self.extend_grapheme(row, x, c) {
                return;
            }
        }
        let width = UnicodeWidthChar::width(c).unwrap_or(0).clamp(1, 2);
        let x = self.col.min(MAX_COLUMNS);
        let style = self.style;
        self.current().write(
            x,
            Cell {
                ch: c,
                extra: None,
                width: width as u8,
                style,
            },
        );
        self.col = x + width;
        self.last = Some((self.row, x));
    }

    fn execute(&mut self, byte: u8) {
        self.last = None;
        match byte {
            b'\n' | 0x0b | 0x0c => self.line_feed(),
            b'\r' => self.col = 0,
            0x08 => self.col = self.col.saturating_sub(1),
            b'\t' => self.set_col((self.col / TAB + 1) * TAB),
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        // Private modes (`CSI ? ...`) and malformed sequences change nothing
        // a preview can show.
        if ignore || !intermediates.is_empty() {
            return;
        }
        self.last = None;
        let n = arg(params, 0, 1);
        let mode = params
            .iter()
            .next()
            .and_then(|param| param.first().copied())
            .unwrap_or(0);
        match action {
            'm' => self.sgr(params),
            'A' => self.row = self.row.saturating_sub(n),
            'B' | 'e' => self.row = (self.row + n).min(self.rows.len() - 1),
            'C' | 'a' => self.set_col(self.col.saturating_add(n)),
            'D' => self.col = self.col.saturating_sub(n),
            'E' => {
                self.row = (self.row + n).min(self.rows.len() - 1);
                self.col = 0;
            }
            'F' => {
                self.row = self.row.saturating_sub(n);
                self.col = 0;
            }
            'G' | '`' => self.set_col(n - 1),
            'd' => self.set_row(n - 1),
            'H' | 'f' => {
                self.set_row(n - 1);
                self.set_col(arg(params, 1, 1) - 1);
            }
            'J' => self.erase_display(mode),
            'K' => self.erase_line(mode),
            'X' => {
                let col = self.col;
                self.current().blank(col..col.saturating_add(n));
            }
            'P' => {
                let col = self.col;
                let row = self.current();
                if col < row.cells.len() {
                    let end = col.saturating_add(n).min(row.cells.len());
                    row.clear_overlap(col, end);
                    row.cells.drain(col..end);
                }
            }
            '@' => {
                let col = self.col;
                let row = self.current();
                if col < row.cells.len() {
                    row.clear_overlap(col, col);
                    let count = n.min(MAX_COLUMNS);
                    row.cells
                        .splice(col..col, std::iter::repeat_with(Cell::blank).take(count));
                }
            }
            's' => self.saved = (self.base + self.row, self.col),
            'u' => self.restore(),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if ignore || !intermediates.is_empty() {
            return;
        }
        self.last = None;
        match byte {
            b'7' => self.saved = (self.base + self.row, self.col),
            b'8' => self.restore(),
            b'D' => {
                let col = self.col;
                self.line_feed();
                self.col = col;
            }
            b'E' => self.line_feed(),
            b'M' => self.row = self.row.saturating_sub(1),
            b'c' => self.style = Style::default(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::copy_mode::TextRow;
    use crate::{Cancellation, Preview, PreviewCell};

    fn rows(bytes: &[u8]) -> Vec<TextRow> {
        let document = parse(bytes, usize::MAX, usize::MAX).unwrap();
        (0..).map_while(|row| document.source_row(row)).collect()
    }
    fn lines(bytes: &[u8]) -> Vec<String> {
        rows(bytes).into_iter().map(|row| row.text).collect()
    }
    /// The first row of an 80-column layout.
    fn cells(bytes: &[u8]) -> Vec<PreviewCell> {
        let document = parse(bytes, usize::MAX, usize::MAX).unwrap();
        let layout = document.layout(80, &Cancellation::test()).unwrap();
        let Preview::Grid { cells, .. } = layout.grid(0, 1, Color32::WHITE, Color32::BLACK, None)
        else {
            panic!("grid expected")
        };
        cells
    }

    #[test]
    fn carriage_return_overwrites_and_erases() {
        assert_eq!(lines(b"old\rnewx\x08!\x1b[K"), ["new!"]);
        assert_eq!(lines(b"discard\r\x1b[2Kkept"), ["kept"]);
        assert_eq!(lines(b"spaces  \n"), ["spaces  ", ""]);
        assert_eq!(lines(b"a\x1b[3Cb"), ["a   b"]);
        assert_eq!(lines(b"abcdef\x1b[3G\x1b[2P"), ["abef"]);
        assert_eq!(lines(b"abc\x1b[2G\x1b[2@X"), ["aX bc"]);
        assert_eq!(lines(b"abcdef\x1b[2G\x1b[2X"), ["a  def"]);
        assert_eq!(lines(b"a\tb"), ["a       b"]);
        let long = "x".repeat(1200);
        assert_eq!(lines(long.as_bytes()), [long]);
    }

    #[test]
    fn cursor_up_redraws_progress_lines() {
        let bytes = b"one 10%\ntwo 10%\n\x1b[2A\rone 99%\x1b[K\n\rtwo 99%\x1b[K\ndone";
        assert_eq!(lines(bytes), ["one 99%", "two 99%", "done"]);
        assert_eq!(lines(b"a\x1b7\nb\x1b8c"), ["ac", "b"]);
        assert_eq!(
            lines(b"\x1b[2J\x1b[Hfirst\x1b[3;1Hthird"),
            ["first", "", "third"]
        );
    }

    #[test]
    fn wide_graphemes_keep_cells_and_clusters() {
        let row = &rows("ab猫e\u{301}f".as_bytes())[0];
        assert_eq!(row.text, "ab猫e\u{301}f");
        assert_eq!(row.columns[row.text.find('猫').unwrap()], 2);
        assert_eq!(row.columns[row.text.find('e').unwrap()], 4);
        assert_eq!(*row.columns.last().unwrap(), 5);
        // Overwriting half of a wide glyph blanks the other half.
        assert_eq!(lines("猫猫\r\x1b[1Cx".as_bytes()), [" x猫"]);
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let row = &rows(format!("{family}!🇺🇸?").as_bytes())[0];
        assert_eq!(row.text, format!("{family}!🇺🇸?"));
        assert_eq!(row.columns[family.len()], 2);
        assert_eq!(*row.columns.last().unwrap(), 5);
        // VS16 widens a text-presentation emoji to two cells.
        let row = &rows("\u{2764}\u{FE0F}x".as_bytes())[0];
        assert_eq!(*row.columns.last().unwrap(), 2);
    }

    #[test]
    fn sgr_attributes_and_resets() {
        let cells = cells(b"plain \x1b[1;3;4;9;7;38;2;12;34;56mX\x1b[22;23;24;29;27mY\x1b[0mZ");
        assert_eq!(cells[0].foreground, Color32::WHITE);
        let x = &cells[6];
        assert!(x.bold && x.italic && x.underline && x.strikethrough && x.inverse);
        assert_eq!(x.foreground, Color32::from_rgb(12, 34, 56));
        let y = &cells[7];
        assert!(!(y.bold || y.italic || y.underline || y.strikethrough || y.inverse));
        assert_eq!(y.foreground, Color32::from_rgb(12, 34, 56));
        assert_eq!(cells[8].foreground, Color32::WHITE);
        let cells = super::tests::cells(b"\x1b[2;8mh\x1b[4:0mu");
        assert!(cells[0].faint && cells[0].invisible && !cells[1].underline);
    }

    #[test]
    fn palette_truecolor_and_colon_forms() {
        let cells = cells(
            b"\x1b[48;5;200mi\x1b[0m\x1b[38:2::3:4:5mc\x1b[38:2:6:7:8ms\x1b[31mr\x1b[94mb\x1b[38;5;244mg",
        );
        assert_eq!(cells[0].background, Color32::from_rgb(255, 0, 215));
        assert_eq!(cells[1].foreground, Color32::from_rgb(3, 4, 5));
        assert_eq!(cells[2].foreground, Color32::from_rgb(6, 7, 8));
        assert_eq!(cells[3].foreground, Color32::from_rgb(0xCC, 0x66, 0x66));
        assert_eq!(cells[4].foreground, Color32::from_rgb(0x7A, 0xA6, 0xDA));
        assert_eq!(cells[5].foreground, Color32::from_rgb(128, 128, 128));
        // Truncated extended colors leave the current color unchanged.
        let cells = super::tests::cells(b"\x1b[31m\x1b[38;2;1mx\x1b[38;5my");
        assert_eq!(cells[0].foreground, Color32::from_rgb(0xCC, 0x66, 0x66));
        assert_eq!(cells[1].foreground, Color32::from_rgb(0xCC, 0x66, 0x66));
    }

    #[test]
    fn erase_with_background_fills_to_the_edge() {
        let cells = cells(b"\x1b[31;1mred\x1b[0m plain \x1b[44mbg\x1b[K");
        let blue = Color32::from_rgb(0x81, 0xA2, 0xBE);
        assert!(cells[0].bold && !cells[4].bold);
        assert_eq!(cells[4].background, Color32::BLACK);
        assert_eq!(cells[79].background, blue);
    }

    #[test]
    fn limits_keep_the_head_and_reject_large_input() {
        assert!(parse(b"abc", 2, 10).is_err());
        let text: String = (0..1200).map(|row| format!("row{row:04}\r\n")).collect();
        let document = parse(text.as_bytes(), usize::MAX, 20).unwrap();
        assert_eq!(document.source_row(0).unwrap().text, "row0000");
        assert!(document.source_row(19).is_some() && document.source_row(20).is_none());
        let all = lines(text.as_bytes());
        assert_eq!(all[288], "row0288");
        assert_eq!(all.len(), 1201);
        assert!(parse(b"\x1b[99999999C\x1b[9999999@x", usize::MAX, 4).is_ok());
    }
}
