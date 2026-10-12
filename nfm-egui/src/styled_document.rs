//! Compact immutable text and style runs. Layouts retain row boundaries rather
//! than another terminal or a grid of styled cells.
use crate::copy_document::{CopyDocument, DocumentMetadata, DocumentRow};
use crate::copy_mode::{CellPosition, CopySelection, TextRow};
use crate::{Appearance, Cancellation, Preview, PreviewCell, PreviewProvider, PreviewRequest};
use nfm_search_core::store::{ChunkedSnapshot, ChunkedStorage};
use std::ops::Range;
use std::sync::{Arc, Mutex};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    /// None uses the preview's default color.
    pub foreground: Option<egui::Color32>,
    pub background: Option<egui::Color32>,
    pub underline_color: Option<egui::Color32>,
    /// bold=1, italic=2, strike=4, inverse=8, invisible=16, faint=32.
    pub flags: u8,
    pub underline: bool,
}
#[derive(Clone, Debug)]
pub struct ColumnStyleRun {
    pub columns: Range<u32>,
    pub style: Style,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub line: usize,
    pub byte: usize,
}
#[derive(Clone, Copy, Debug)]
pub struct DocumentRange {
    pub start: Position,
    pub end: Position,
}
struct Run {
    end: usize,
    style: Style,
}
struct SourceRow {
    line: usize,
    start: usize,
    end: usize,
    maps: Range<usize>,
    wraps: bool,
}
struct ColumnMap {
    start: usize,
    end: usize,
    column: u32,
    step: u32,
}
pub struct DocumentBuilder {
    text: ChunkedStorage<u8>,
    lines: Vec<Range<usize>>,
    runs: Vec<Run>,
    source_rows: Vec<SourceRow>,
    column_maps: Vec<ColumnMap>,
    line_start: usize,
    source_origin: usize,
}
impl Default for DocumentBuilder {
    fn default() -> Self {
        Self {
            text: ChunkedStorage::new(1024 * 1024),
            lines: Vec::new(),
            runs: Vec::new(),
            source_rows: Vec::new(),
            column_maps: Vec::new(),
            line_start: 0,
            source_origin: 0,
        }
    }
}
impl DocumentBuilder {
    pub fn with_source_origin(mut self, row: usize) -> Self {
        self.source_origin = row;
        self
    }
    /// Source rows arrive in order. Soft boundaries join into logical lines.
    pub fn push_row(
        &mut self,
        text: &str,
        columns: &[u32],
        wraps: bool,
        styles: &[ColumnStyleRun],
    ) -> Result<(), String> {
        if columns.len() != text.len() {
            return Err("Invalid document column map".into());
        }
        if columns.windows(2).any(|pair| pair[0] > pair[1])
            || styles
                .windows(2)
                .any(|pair| pair[0].columns.end > pair[1].columns.start)
        {
            return Err("Unsorted document row".into());
        }
        let start = self.text.len();
        let mut end_column = 0;
        let mut style_index = 0;
        for (byte, grapheme) in text.grapheme_indices(true) {
            let x = columns[byte];
            self.append(grapheme, style_at(styles, x, &mut style_index));
            end_column = x + UnicodeWidthStr::width(grapheme).max(1) as u32;
        }
        let source_end = self.text.len();
        // Preserve backgrounds on otherwise empty trailing cells. They are
        // presentation content, excluded from the original searchable row.
        if !wraps {
            let styled_end = styles
                .iter()
                .filter(|r| r.style.background.is_some() || r.style.flags & 8 != 0)
                .map(|r| r.columns.end)
                .max()
                .unwrap_or(end_column);
            while end_column < styled_end {
                self.append(" ", style_at(styles, end_column, &mut style_index));
                end_column += 1;
            }
        }
        let first_map = self.column_maps.len();
        let mut byte = 0;
        while byte < columns.len() {
            let step = columns
                .get(byte + 1)
                .copied()
                .unwrap_or(columns[byte])
                .saturating_sub(columns[byte])
                .min(1);
            let mut end = byte + 1;
            while end < columns.len() && columns[end] == columns[byte] + (end - byte) as u32 * step
            {
                end += 1;
            }
            self.column_maps.push(ColumnMap {
                start: byte,
                end,
                column: columns[byte],
                step,
            });
            byte = end;
        }
        self.source_rows.push(SourceRow {
            line: self.lines.len(),
            start,
            end: source_end,
            maps: first_map..self.column_maps.len(),
            wraps,
        });
        if !wraps {
            self.lines.push(self.line_start..self.text.len());
            self.line_start = self.text.len();
        }
        Ok(())
    }
    fn append(&mut self, text: &str, style: Style) {
        let start = self.text.len();
        self.text.extend_from_slice(text.as_bytes());
        if let Some(run) = self.runs.last_mut() {
            if run.end == start && run.style == style {
                run.end = self.text.len();
                return;
            }
        }
        self.runs.push(Run {
            end: self.text.len(),
            style,
        });
    }
    pub fn finish(mut self) -> Arc<StyledDocument> {
        if self.source_rows.last().is_some_and(|r| r.wraps) {
            self.lines.push(self.line_start..self.text.len());
        }
        Arc::new(StyledDocument {
            text: self.text.snapshot(),
            lines: self.lines,
            runs: self.runs,
            source_rows: self.source_rows,
            column_maps: self.column_maps,
            source_origin: self.source_origin,
        })
    }
}
fn style_at(runs: &[ColumnStyleRun], column: u32, index: &mut usize) -> Style {
    while *index < runs.len() && runs[*index].columns.end <= column {
        *index += 1;
    }
    runs.get(*index)
        .filter(|r| r.columns.contains(&column))
        .map_or(Style::default(), |r| r.style)
}
pub struct StyledDocument {
    text: ChunkedSnapshot<u8>,
    lines: Vec<Range<usize>>,
    runs: Vec<Run>,
    source_rows: Vec<SourceRow>,
    column_maps: Vec<ColumnMap>,
    source_origin: usize,
}
impl StyledDocument {
    pub fn source_byte_offset(&self, row: usize, column: u32) -> Option<usize> {
        let position = self.source_position(row, column)?;
        Some(self.lines[position.line].start + position.byte)
    }
    fn text(&self, range: Range<usize>) -> String {
        let mut bytes = vec![0; range.len()];
        self.text.copy_range_to(range.start, &mut bytes);
        String::from_utf8(bytes).expect("document contains validated UTF-8")
    }
    pub fn source_row(&self, row: usize) -> Option<TextRow> {
        let source = self.source_rows.get(row.checked_sub(self.source_origin)?)?;
        let text = self.text(source.start..source.end);
        let mut columns = Vec::with_capacity(text.len());
        for run in &self.column_maps[source.maps.clone()] {
            columns.extend(
                (run.start..run.end).map(|byte| run.column + (byte - run.start) as u32 * run.step),
            );
        }
        Some(TextRow {
            text,
            columns,
            wraps_to_next: source.wraps,
        })
    }
    pub fn source_position(&self, row: usize, column: u32) -> Option<Position> {
        let source = self.source_rows.get(row.checked_sub(self.source_origin)?)?;
        let text = self.text(source.start..source.end);
        let maps = &self.column_maps[source.maps.clone()];
        let mut byte = text.len();
        for (offset, grapheme) in text.grapheme_indices(true) {
            let map = &maps[maps.partition_point(|run| run.end <= offset)];
            let x = map.column + (offset - map.start) as u32 * map.step;
            let width = UnicodeWidthStr::width(grapheme).max(1) as u32;
            if column < x + width {
                byte = offset;
                break;
            }
        }
        Some(Position {
            line: source.line,
            byte: source.start + byte - self.lines[source.line].start,
        })
    }
    pub fn layout(
        self: &Arc<Self>,
        columns: usize,
        cancel: &Cancellation,
    ) -> Result<Arc<DocumentLayout>, String> {
        if columns == 0 {
            return Err("Invalid document width".into());
        }
        let mut rows = Vec::new();
        let mut line_rows = Vec::with_capacity(self.lines.len());
        for range in &self.lines {
            if cancel.is_cancelled() {
                return Err("preview cancelled".into());
            }
            line_rows.push(rows.len());
            let text = self.text(range.clone());
            let mut start = range.start;
            let mut width = 0;
            for (byte, grapheme) in text.grapheme_indices(true) {
                if byte % 4096 == 0 && cancel.is_cancelled() {
                    return Err("preview cancelled".into());
                }
                let w = UnicodeWidthStr::width(grapheme).max(1).min(columns);
                if width + w > columns {
                    rows.push(LayoutRow {
                        start,
                        end: range.start + byte,
                        width,
                        wraps: true,
                    });
                    start = range.start + byte;
                    width = 0;
                }
                width += w;
            }
            rows.push(LayoutRow {
                start,
                end: range.end,
                width,
                wraps: false,
            });
        }
        static REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Ok(Arc::new(DocumentLayout {
            document: self.clone(),
            columns,
            rows,
            line_rows,
            revision: REVISION.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }))
    }
}

#[derive(Clone, Copy, Default)]
struct Item<T: Copy> {
    offset: usize,
    len: usize,
    payload: T,
}
/// Search candidates reference the same text chunks as the preview document.
pub struct DocumentItemStore<T: Copy> {
    document: Arc<StyledDocument>,
    items: ChunkedStorage<Item<T>>,
    version: u64,
}
pub struct DocumentItems<T: Copy> {
    document: Arc<StyledDocument>,
    items: ChunkedSnapshot<Item<T>>,
    len: usize,
    version: u64,
}
impl<T: Copy + Default> DocumentItemStore<T> {
    pub fn new(document: Arc<StyledDocument>) -> Self {
        Self {
            document,
            items: ChunkedStorage::new(64 * 1024),
            version: 0,
        }
    }
    pub fn add(&mut self, offset: usize, len: usize, payload: T) -> Result<(), String> {
        let total = self.document.lines.last().map_or(0, |line| line.end);
        if offset > total || len > total - offset {
            return Err("Invalid document item range".into());
        }
        self.items.push(Item {
            offset,
            len,
            payload,
        });
        Ok(())
    }
    pub fn snapshot(&mut self) -> Arc<DocumentItems<T>> {
        self.version += 1;
        Arc::new(DocumentItems {
            document: self.document.clone(),
            items: self.items.snapshot(),
            len: self.items.len(),
            version: self.version,
        })
    }
}
impl<T: Copy> DocumentItems<T> {
    pub fn payload(&self, index: usize) -> &T {
        &self.items[index].payload
    }
}
impl<T: Copy> crate::ItemsSource for DocumentItems<T> {
    type Item = T;
    fn version(&self) -> u64 {
        self.version
    }
    fn len(&self) -> usize {
        self.len
    }
    fn is_empty(&self) -> bool {
        self.len == 0
    }
    fn get_string<'a>(
        &'a self,
        index: usize,
        stack: &'a mut [u8],
        heap: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        let item = &self.items[index];
        self.document
            .text
            .get_range(item.offset, item.len, stack, heap)
    }
    fn get_string_lossy(&self, index: usize, heap: &mut Vec<u8>) -> String {
        let mut stack = [0; 4096];
        String::from_utf8_lossy(self.get_string(index, &mut stack, heap)).into_owned()
    }
    fn item(&self, index: usize) -> Option<T> {
        (index < self.len).then(|| *self.payload(index))
    }
}
struct LayoutRow {
    start: usize,
    end: usize,
    width: usize,
    wraps: bool,
}
pub struct DocumentLayout {
    document: Arc<StyledDocument>,
    columns: usize,
    rows: Vec<LayoutRow>,
    line_rows: Vec<usize>,
    revision: u64,
}
impl DocumentLayout {
    pub fn cell(&self, position: Position) -> Option<CellPosition> {
        let line = self.document.lines.get(position.line)?;
        let absolute = line.start + position.byte.min(line.len());
        let first = self.line_rows[position.line];
        let end = self
            .line_rows
            .get(position.line + 1)
            .copied()
            .unwrap_or(self.rows.len());
        let y = first
            + self.rows[first..end]
                .partition_point(|r| r.start <= absolute)
                .saturating_sub(1);
        let row = &self.rows[y];
        let text = self.document.text(row.start..row.end);
        let mut boundary = (absolute - row.start).min(text.len());
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        let width = UnicodeWidthStr::width(&text[..boundary]);
        Some(CellPosition {
            x: width.min(self.columns - 1) as u32,
            y: y as u64,
        })
    }
    fn row(&self, y: usize) -> Option<DocumentRow> {
        let row = self.rows.get(y)?;
        let mut text = self.document.text(row.start..row.end);
        let mut offsets = Vec::new();
        let mut widths = vec![0; self.columns];
        let mut x = 0;
        for grapheme in text.graphemes(true) {
            let width = UnicodeWidthStr::width(grapheme).max(1).min(self.columns);
            offsets.extend(std::iter::repeat_n(x as u32, grapheme.len()));
            widths[x] = width as u8;
            x += width;
        }
        while x < self.columns {
            text.push(' ');
            offsets.push(x as u32);
            widths[x] = 1;
            x += 1;
        }
        Some(DocumentRow {
            text: TextRow {
                text,
                columns: offsets,
                wraps_to_next: row.wraps,
            },
            widths,
        })
    }
    fn grid(
        &self,
        first: usize,
        count: usize,
        foreground: egui::Color32,
        background: egui::Color32,
        highlight: Option<(CellPosition, CellPosition, egui::Color32)>,
    ) -> Preview {
        let mut cells = Vec::with_capacity(count.saturating_mul(self.columns));
        for y in first..first.saturating_add(count).min(self.rows.len()) {
            let row = &self.rows[y];
            let text = self.document.text(row.start..row.end);
            let mut run_index = self.document.runs.partition_point(|r| r.end <= row.start);
            let mut x = 0;
            for (byte, grapheme) in text.grapheme_indices(true) {
                let absolute = row.start + byte;
                while run_index < self.document.runs.len()
                    && self.document.runs[run_index].end <= absolute
                {
                    run_index += 1;
                }
                let style = self
                    .document
                    .runs
                    .get(run_index)
                    .map_or(Style::default(), |r| r.style);
                let width = UnicodeWidthStr::width(grapheme).max(1).min(self.columns);
                for spacer in 0..width {
                    let mut cell = styled_cell(
                        style,
                        if spacer == 0 { grapheme } else { "" },
                        foreground,
                        background,
                    );
                    apply_highlight(&mut cell, x + spacer, y, highlight);
                    cells.push(cell);
                }
                x += width;
            }
            while x < self.columns {
                let mut cell = styled_cell(Style::default(), " ", foreground, background);
                apply_highlight(&mut cell, x, y, highlight);
                cells.push(cell);
                x += 1;
            }
        }
        Preview::Grid {
            columns: self.columns,
            cells,
            total_rows: self.rows.len(),
        }
    }
}
fn styled_cell(style: Style, text: &str, fg: egui::Color32, bg: egui::Color32) -> PreviewCell {
    PreviewCell {
        text: text.into(),
        foreground: style.foreground.unwrap_or(fg),
        background: style.background.unwrap_or(bg),
        underline_color: style.underline_color.or(style.foreground).unwrap_or(fg),
        bold: style.flags & 1 != 0,
        italic: style.flags & 2 != 0,
        strikethrough: style.flags & 4 != 0,
        inverse: style.flags & 8 != 0,
        invisible: style.flags & 16 != 0,
        faint: style.flags & 32 != 0,
        underline: style.underline,
    }
}
fn apply_highlight(
    cell: &mut PreviewCell,
    x: usize,
    y: usize,
    highlight: Option<(CellPosition, CellPosition, egui::Color32)>,
) {
    if let Some((start, end, color)) = highlight {
        let point = (y as u64, x as u32);
        if point >= (start.y, start.x) && point <= (end.y, end.x) {
            if cell.inverse {
                cell.foreground = color;
            } else {
                cell.background = color;
            }
        }
    }
}
impl CopyDocument for DocumentLayout {
    fn metadata(&self) -> Result<DocumentMetadata, String> {
        Ok(DocumentMetadata {
            revision: self.revision,
            total_rows: self.rows.len() as u64,
            columns: self.columns as u32,
        })
    }
    fn text_row(&self, row: u64) -> Result<Option<DocumentRow>, String> {
        Ok(self.row(row as usize))
    }
    fn resolve_cell(
        &self,
        mut cell: CellPosition,
        snap_right: bool,
    ) -> Result<CellPosition, String> {
        let row = self
            .row(cell.y as usize)
            .ok_or("Document row unavailable")?;
        let mut x = (cell.x as usize).min(self.columns - 1);
        while x > 0 && row.widths[x] == 0 {
            x -= 1;
        }
        cell.x = (x + if snap_right {
            row.widths[x].saturating_sub(1) as usize
        } else {
            0
        }) as u32;
        Ok(cell)
    }
    fn selection_text(&self, selection: &CopySelection) -> Result<String, String> {
        let (a, b) = (selection.range.start, selection.range.end);
        let (start, end) = if (a.y, a.x) <= (b.y, b.x) {
            (a, b)
        } else {
            (b, a)
        };
        if end.y as usize >= self.rows.len()
            || start.x as usize >= self.columns
            || end.x as usize >= self.columns
        {
            return Err("Invalid document selection".into());
        }
        let mut result = String::new();
        for y in start.y..=end.y {
            let layout_row = &self.rows[y as usize];
            let row = self.row(y as usize).unwrap();
            let left = if selection.linewise {
                0
            } else if selection.rectangle {
                a.x.min(b.x)
            } else if y == start.y {
                start.x
            } else {
                0
            };
            let right = if selection.linewise {
                self.columns as u32 - 1
            } else if selection.rectangle {
                a.x.max(b.x)
            } else if y == end.y {
                end.x
            } else {
                self.columns as u32 - 1
            };
            let mut part = String::new();
            for (byte, grapheme) in row.text.text.grapheme_indices(true) {
                let x = row.text.columns[byte];
                let width = row.widths[x as usize] as u32;
                if x <= right
                    && x + width > left
                    && (selection.rectangle || !layout_row.wraps || (x as usize) < layout_row.width)
                {
                    part.push_str(grapheme);
                }
            }
            if selection.trim && (!layout_row.wraps || y == end.y || selection.rectangle) {
                part.truncate(part.trim_end_matches(' ').len());
            }
            result.push_str(&part);
            if y < end.y && (selection.rectangle || !layout_row.wraps) {
                result.push('\n');
            }
        }
        if selection.linewise {
            result.push('\n');
        }
        Ok(result)
    }
    fn viewport(
        &self,
        first_row: u64,
        rows: usize,
        appearance: &Appearance,
    ) -> Result<Preview, String> {
        Ok(self.grid(
            first_row as usize,
            rows,
            appearance.palette.text,
            appearance.palette.background,
            None,
        ))
    }
}
struct SelectedDocument {
    layout: Arc<DocumentLayout>,
    highlight: Option<(CellPosition, CellPosition, egui::Color32)>,
}
impl CopyDocument for SelectedDocument {
    fn metadata(&self) -> Result<DocumentMetadata, String> {
        self.layout.metadata()
    }
    fn text_row(&self, row: u64) -> Result<Option<DocumentRow>, String> {
        self.layout.text_row(row)
    }
    fn resolve_cell(&self, cell: CellPosition, right: bool) -> Result<CellPosition, String> {
        self.layout.resolve_cell(cell, right)
    }
    fn selection_text(&self, selection: &CopySelection) -> Result<String, String> {
        self.layout.selection_text(selection)
    }
    fn viewport(
        &self,
        first_row: u64,
        rows: usize,
        appearance: &Appearance,
    ) -> Result<Preview, String> {
        Ok(self.layout.grid(
            first_row as usize,
            rows,
            appearance.palette.text,
            appearance.palette.background,
            self.highlight,
        ))
    }
}
type Resolver<T> = dyn Fn(&crate::Selection<T>) -> Result<(Arc<StyledDocument>, Option<DocumentRange>), String>
    + Send
    + Sync;
struct Published {
    key: (u64, usize),
    document: Arc<SelectedDocument>,
}
/// Resolve shared content and stable byte coordinates. One width-specific
/// layout is reused across selections. Copy documents retain that layout.
pub struct DocumentPreviewProvider<T> {
    resolve: Arc<Resolver<T>>,
    layout: Mutex<Option<Arc<DocumentLayout>>>,
    published: Mutex<Option<Published>>,
    highlight: egui::Color32,
}
impl<T> DocumentPreviewProvider<T> {
    pub fn new(
        resolve: impl Fn(
            &crate::Selection<T>,
        ) -> Result<(Arc<StyledDocument>, Option<DocumentRange>), String>
        + Send
        + Sync
        + 'static,
        highlight: egui::Color32,
    ) -> Self {
        Self {
            resolve: Arc::new(resolve),
            layout: Mutex::new(None),
            published: Mutex::new(None),
            highlight,
        }
    }
}
impl<T: Clone + Send + 'static> PreviewProvider<T> for DocumentPreviewProvider<T> {
    fn preview(&self, request: PreviewRequest<T>, cancel: Cancellation) -> Result<Preview, String> {
        self.preview_viewport(request, cancel).map(|p| p.document)
    }
    fn preview_viewport(
        &self,
        request: PreviewRequest<T>,
        cancel: Cancellation,
    ) -> Result<crate::preview::PreviewViewport, String> {
        let (document, range) = (self.resolve)(&request.selection)?;
        let cached = self
            .layout
            .lock()
            .map_err(|_| "Document layout poisoned")?
            .clone();
        let layout = if let Some(layout) = cached.filter(|l| {
            l.columns == request.columns as usize && Arc::ptr_eq(&l.document, &document)
        }) {
            layout
        } else {
            let layout = document.layout(request.columns as usize, &cancel)?;
            *self.layout.lock().map_err(|_| "Document layout poisoned")? = Some(layout.clone());
            layout
        };
        let highlight = range.and_then(|r| {
            Some((
                layout.cell(r.start)?,
                layout.resolve_cell(layout.cell(r.end)?, true).ok()?,
                self.highlight,
            ))
        });
        let first = if request.initial_page {
            highlight.map_or(0, |(start, _, _)| {
                (start.y as usize).saturating_sub(request.rows as usize / 2)
            })
        } else {
            request.scroll_offset
        };
        let first = first.min(layout.rows.len().saturating_sub(request.rows as usize));
        if cancel.is_cancelled() {
            return Err("preview cancelled".into());
        }
        let preview = layout.grid(
            first,
            request.rows as usize,
            request.foreground,
            request.background,
            highlight,
        );
        *self
            .published
            .lock()
            .map_err(|_| "Document publication poisoned")? = Some(Published {
            key: (request.document_version, request.selection.index),
            document: Arc::new(SelectedDocument { layout, highlight }),
        });
        Ok(crate::preview::PreviewViewport {
            document: preview,
            first_row: first,
        })
    }
    fn copy_document(
        &self,
        version: u64,
        index: usize,
    ) -> Result<Option<Arc<dyn CopyDocument>>, String> {
        // Layout work never holds this short publication lock.
        Ok(self
            .published
            .lock()
            .map_err(|_| "Document publication poisoned")?
            .as_ref()
            .filter(|p| p.key == (version, index))
            .map(|p| p.document.clone() as Arc<dyn CopyDocument>))
    }
    fn page_step(&self, rows: usize) -> usize {
        (rows / 2).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn push(builder: &mut DocumentBuilder, text: &str, wraps: bool, style: Style) {
        let mut columns = Vec::new();
        let mut x = 0;
        for grapheme in text.graphemes(true) {
            columns.extend(std::iter::repeat_n(x, grapheme.len()));
            x += UnicodeWidthStr::width(grapheme).max(1) as u32;
        }
        builder
            .push_row(
                text,
                &columns,
                wraps,
                &[ColumnStyleRun {
                    columns: 0..x,
                    style,
                }],
            )
            .unwrap();
    }
    fn request(index: usize, columns: u16) -> PreviewRequest<usize> {
        PreviewRequest {
            selection: crate::Selection {
                item: index,
                text: index.to_string(),
                index,
                source_version: 1,
                snapshot: Arc::new(()),
            },
            document_version: 0,
            initial_page: true,
            columns,
            rows: 3,
            scroll_offset: 0,
            foreground: egui::Color32::WHITE,
            background: egui::Color32::BLACK,
        }
    }
    #[test]
    fn compact_document_preserves_unicode_styles_and_soft_boundaries() {
        let red = Style {
            foreground: Some(egui::Color32::RED),
            flags: 1 | 2,
            underline: true,
            ..Default::default()
        };
        let mut builder = DocumentBuilder::default();
        push(&mut builder, "ab界", true, red);
        push(&mut builder, "e\u{301}f  ", false, red);
        push(&mut builder, "next", false, Style::default());
        let document = builder.finish();
        assert_eq!(document.lines.len(), 2);
        assert_eq!(document.runs.len(), 2);
        assert_eq!(document.source_row(0).unwrap().columns, vec![0, 1, 2, 2, 2]);
        let stable = document.source_position(1, 1).unwrap();
        for columns in [3, 8, 20] {
            let layout = document.layout(columns, &Cancellation::test()).unwrap();
            let start = layout.cell(Position { line: 0, byte: 0 }).unwrap();
            let end = layout
                .cell(Position {
                    line: 0,
                    byte: "ab界e\u{301}f".len() - 1,
                })
                .unwrap();
            assert_eq!(
                layout
                    .selection_text(&CopySelection {
                        range: crate::copy_mode::CellRange { start, end },
                        linewise: false,
                        rectangle: false,
                        trim: true
                    })
                    .unwrap(),
                "ab界e\u{301}f"
            );
            let cell = layout.cell(stable).unwrap();
            assert!(
                layout
                    .text_row(cell.y)
                    .unwrap()
                    .unwrap()
                    .text
                    .text
                    .contains('f')
            );
            let Preview::Grid { cells, .. } =
                layout.grid(0, 2, egui::Color32::WHITE, egui::Color32::BLACK, None)
            else {
                panic!()
            };
            assert_eq!(cells[0].foreground, egui::Color32::RED);
            assert!(cells[0].bold && cells[0].italic && cells[0].underline);
        }
    }
    #[test]
    fn copy_preserves_wide_glyphs_rectangles_spaces_and_linewise_newlines() {
        let mut builder = DocumentBuilder::default();
        push(&mut builder, "a界b ", false, Style::default());
        push(&mut builder, "12345", false, Style::default());
        let document = builder.finish();
        let layout = document.layout(8, &Cancellation::test()).unwrap();
        assert_eq!(
            layout
                .resolve_cell(CellPosition { x: 2, y: 0 }, false)
                .unwrap()
                .x,
            1
        );
        let mut selection = CopySelection {
            range: crate::copy_mode::CellRange {
                start: CellPosition { x: 2, y: 0 },
                end: CellPosition { x: 2, y: 1 },
            },
            linewise: false,
            rectangle: true,
            trim: false,
        };
        assert_eq!(layout.selection_text(&selection).unwrap(), "界\n3");
        selection.range = crate::copy_mode::CellRange {
            start: CellPosition { x: 0, y: 0 },
            end: CellPosition { x: 4, y: 0 },
        };
        selection.rectangle = false;
        assert_eq!(layout.selection_text(&selection).unwrap(), "a界b ");
        selection.linewise = true;
        selection.trim = true;
        assert_eq!(layout.selection_text(&selection).unwrap(), "a界b\n");
    }
    #[test]
    fn provider_reuses_large_layout_and_retains_copy_document_and_highlights() {
        let mut builder = DocumentBuilder::default();
        for _ in 0..70000 {
            push(&mut builder, "abcdefgh", false, Style::default());
        }
        let document = builder.finish();
        let shared = document.clone();
        let provider = DocumentPreviewProvider::new(
            move |selection: &crate::Selection<usize>| {
                Ok((
                    shared.clone(),
                    Some(DocumentRange {
                        start: Position {
                            line: selection.item,
                            byte: 2,
                        },
                        end: Position {
                            line: selection.item,
                            byte: 4,
                        },
                    }),
                ))
            },
            egui::Color32::YELLOW,
        );
        provider
            .preview_viewport(request(60000, 4), Cancellation::test())
            .unwrap();
        let first_layout = provider.layout.lock().unwrap().clone().unwrap();
        let copy = provider.copy_document(0, 60000).unwrap().unwrap();
        provider
            .preview_viewport(request(60001, 4), Cancellation::test())
            .unwrap();
        assert!(Arc::ptr_eq(
            &first_layout,
            provider.layout.lock().unwrap().as_ref().unwrap()
        ));
        assert!(provider.copy_document(0, 60000).unwrap().is_none());
        assert_eq!(copy.metadata().unwrap().columns, 4);
        let Preview::Grid { cells, .. } = copy.viewport(120000, 2, &Appearance::default()).unwrap()
        else {
            panic!()
        };
        assert_eq!(cells[2].background, egui::Color32::YELLOW);
        assert_eq!(cells[4].background, egui::Color32::YELLOW);
        provider
            .preview_viewport(request(60001, 12), Cancellation::test())
            .unwrap();
        assert_eq!(copy.metadata().unwrap().columns, 4);
        assert_eq!(
            provider
                .copy_document(0, 60001)
                .unwrap()
                .unwrap()
                .metadata()
                .unwrap()
                .columns,
            12
        );
    }
    #[test]
    fn cancelled_layout_does_not_publish_partial_content() {
        let mut builder = DocumentBuilder::default();
        push(&mut builder, "hello", false, Style::default());
        let document = builder.finish();
        let cancel = Cancellation::test();
        cancel.cancel_for_test();
        assert!(document.layout(4, &cancel).is_err());
    }
    #[test]
    fn search_items_share_text_chunks_and_keep_source_column_gaps() {
        use crate::ItemsSource;
        let mut builder = DocumentBuilder::default().with_source_origin(40);
        builder.push_row("abc", &[0, 1, 5], false, &[]).unwrap();
        for _ in 0..140000 {
            push(&mut builder, "abcdefgh", false, Style::default());
        }
        let document = builder.finish();
        assert_eq!(document.source_row(40).unwrap().columns, vec![0, 1, 5]);
        assert_eq!(document.source_position(40, 5).unwrap().byte, 2);
        let mut items = DocumentItemStore::new(document.clone());
        items.add(0, 3, 42u32).unwrap();
        let first = items.snapshot();
        let offset = document.source_byte_offset(131112, 0).unwrap();
        items.add(offset, 8, 43).unwrap();
        let second = items.snapshot();
        assert!(Arc::ptr_eq(&document, &first.document));
        assert!(Arc::ptr_eq(&first.document, &second.document));
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 2);
        let mut bytes = Vec::new();
        assert_eq!(second.get_string_lossy(1, &mut bytes), "abcdefgh");
        assert_eq!(second.item(1), Some(43));
    }
    #[test]
    fn default_background_padding_does_not_wrap_into_blank_rows() {
        let mut builder = DocumentBuilder::default();
        for text in ["abc  ", "def"] {
            builder
                .push_row(
                    text,
                    &(0..text.len() as u32).collect::<Vec<_>>(),
                    false,
                    &[ColumnStyleRun {
                        columns: 0..80,
                        style: Style::default(),
                    }],
                )
                .unwrap();
        }
        let document = builder.finish();
        // Actual spaces in the captured text remain; unused terminal cells do not.
        assert_eq!(document.source_row(0).unwrap().text, "abc  ");
        let layout = document.layout(10, &Cancellation::test()).unwrap();
        assert_eq!(layout.rows.len(), 2);
    }
    #[test]
    fn trailing_background_is_rendered_without_changing_searchable_text() {
        let mut builder = DocumentBuilder::default();
        let style = Style {
            background: Some(egui::Color32::RED),
            ..Default::default()
        };
        builder
            .push_row(
                "abc",
                &[0, 1, 2],
                false,
                &[ColumnStyleRun {
                    columns: 3..8,
                    style,
                }],
            )
            .unwrap();
        let document = builder.finish();
        assert_eq!(document.source_row(0).unwrap().text, "abc");
        let layout = document.layout(8, &Cancellation::test()).unwrap();
        let Preview::Grid { cells, .. } =
            layout.grid(0, 1, egui::Color32::WHITE, egui::Color32::BLACK, None)
        else {
            panic!()
        };
        assert_eq!(cells[7].background, egui::Color32::RED);
    }
    #[test]
    fn ui_copy_metadata_does_not_wait_for_worker_resolution() {
        let mut builder = DocumentBuilder::default();
        push(&mut builder, "hello", false, Style::default());
        let document = builder.finish();
        let (started, entered) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let released = Mutex::new(released);
        let provider = Arc::new(DocumentPreviewProvider::new(
            move |selection: &crate::Selection<usize>| {
                if selection.index == 1 {
                    started.send(()).unwrap();
                    released.lock().unwrap().recv().unwrap();
                }
                Ok((document.clone(), None))
            },
            egui::Color32::YELLOW,
        ));
        provider
            .preview_viewport(request(0, 8), Cancellation::test())
            .unwrap();
        let worker_provider = provider.clone();
        let worker = std::thread::spawn(move || {
            worker_provider.preview_viewport(request(1, 8), Cancellation::test())
        });
        entered
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let ui_provider = provider.clone();
        let (done, finished) = std::sync::mpsc::channel();
        let ui = std::thread::spawn(move || {
            done.send(ui_provider.copy_document(0, 0).unwrap().is_some())
                .unwrap()
        });
        let returned = finished.recv_timeout(std::time::Duration::from_secs(2));
        release.send(()).unwrap();
        worker.join().unwrap().unwrap();
        ui.join().unwrap();
        assert_eq!(returned.unwrap(), true);
    }
}
