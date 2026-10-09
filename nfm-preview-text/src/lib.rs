//! Source-independent text highlighting, cell layout, paging and copy documents.
//! Hosts supply immutable text or a worker reader; this crate performs no filesystem IO.
use nfm_egui::copy_document::{CopyDocument, DocumentMetadata, DocumentRow};
use nfm_egui::copy_mode::{CellPosition, CopySelection, TextRow};
use nfm_egui::egui::Color32;
use nfm_egui::preview::PreviewViewport;
use nfm_egui::{
    Appearance, Cancellation, Preview, PreviewCell, PreviewProvider, PreviewRequest, Selection,
};
use std::sync::{Arc, Mutex, OnceLock};
use two_face::re_exports::syntect::{
    easy::HighlightLines, highlighting::FontStyle, parsing::SyntaxSet,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, Default)]
pub enum SyntaxChoice {
    #[default]
    Auto,
    PlainText,
    Named(String),
}
#[derive(Clone, Debug)]
pub struct TextPreviewOptions {
    pub syntax: SyntaxChoice,
    pub theme: String,
    pub max_bytes: usize,
}
impl Default for TextPreviewOptions {
    fn default() -> Self {
        Self {
            syntax: SyntaxChoice::Auto,
            theme: "gruvbox-dark".into(),
            max_bytes: 10 * 1024 * 1024,
        }
    }
}
/// One immutable UTF-8 snapshot. A source can force plain text for generated
/// listings or metadata; otherwise request/options select the syntax.
#[derive(Clone, Debug)]
pub struct TextContent {
    pub text: String,
    pub syntax: Option<SyntaxChoice>,
}
impl TextContent {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            syntax: None,
        }
    }
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            syntax: Some(SyntaxChoice::PlainText),
        }
    }
}
/// Called only on NFM's worker. Return a consistent, complete snapshot so
/// multiline highlighting and copy/search share the same document revision.
pub trait TextSource: Send + Sync + 'static {
    fn read(&self, max_bytes: usize, cancelled: &dyn Fn() -> bool) -> Result<TextContent, String>;
}
impl<F> TextSource for F
where
    F: Fn(usize, &dyn Fn() -> bool) -> Result<TextContent, String> + Send + Sync + 'static,
{
    fn read(&self, max_bytes: usize, cancelled: &dyn Fn() -> bool) -> Result<TextContent, String> {
        self(max_bytes, cancelled)
    }
}
#[derive(Clone, Debug)]
pub struct SnapshotSource(pub TextContent);
impl TextSource for SnapshotSource {
    fn read(&self, max_bytes: usize, cancelled: &dyn Fn() -> bool) -> Result<TextContent, String> {
        if cancelled() {
            return Err("Text preview cancelled".into());
        }
        if self.0.text.len() > max_bytes {
            return Err("Text preview exceeds max_bytes".into());
        }
        Ok(self.0.clone())
    }
}
#[derive(Clone)]
pub struct TextPreviewRequest {
    pub source: Arc<dyn TextSource>,
    /// Optional filename hint, used only for Auto syntax selection. Never opened.
    pub name: Option<String>,
    /// Explicit extension hint for Auto syntax selection; accepts an optional dot.
    pub extension: Option<String>,
    pub center_line: Option<usize>,
    pub highlight_line: Option<usize>,
    pub syntax: Option<SyntaxChoice>,
}
impl std::fmt::Debug for TextPreviewRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextPreviewRequest")
            .field("name", &self.name)
            .field("extension", &self.extension)
            .field("center_line", &self.center_line)
            .field("highlight_line", &self.highlight_line)
            .field("syntax", &self.syntax)
            .finish_non_exhaustive()
    }
}
impl TextPreviewRequest {
    pub fn new(source: Arc<dyn TextSource>) -> Self {
        Self {
            source,
            name: None,
            extension: None,
            center_line: None,
            highlight_line: None,
            syntax: None,
        }
    }
    pub fn snapshot(text: impl Into<String>) -> Self {
        Self::new(Arc::new(SnapshotSource(TextContent::new(text))))
    }
}
static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
fn syntaxes() -> &'static SyntaxSet {
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}
fn theme(
    name: &str,
) -> Result<&'static two_face::re_exports::syntect::highlighting::Theme, String> {
    static THEMES: OnceLock<two_face::theme::EmbeddedLazyThemeSet> = OnceLock::new();
    let themes = THEMES.get_or_init(two_face::theme::extra);
    two_face::theme::EmbeddedLazyThemeSet::theme_names()
        .iter()
        .find(|candidate| candidate.as_name().eq_ignore_ascii_case(name))
        .map(|candidate| themes.get(*candidate))
        .ok_or_else(|| format!("Unknown file preview theme: {name}"))
}
impl TextPreviewOptions {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=256 * 1024 * 1024).contains(&self.max_bytes) {
            return Err("Text preview max_bytes must be between 1 and 256 MiB".into());
        }
        theme(&self.theme)?;
        if let SyntaxChoice::Named(name) = &self.syntax {
            if syntaxes()
                .find_syntax_by_name(name)
                .or_else(|| syntaxes().find_syntax_by_extension(name))
                .is_none()
            {
                return Err(format!("Unknown file preview syntax: {name}"));
            }
        }
        Ok(())
    }
}
struct Row {
    text: String,
    cells: Vec<PreviewCell>,
    mapping: Vec<u32>,
    widths: Vec<u8>,
}
pub struct TextDocument {
    rows: Vec<Row>,
    columns: usize,
    highlight_line: Option<usize>,
}
impl TextDocument {
    fn grid(&self, top: usize, rows: usize, columns: usize) -> Preview {
        let columns = columns.max(1);
        let mut cells = Vec::new();
        for (index, row) in self.rows.iter().enumerate().skip(top).take(rows) {
            let mut line = Vec::new();
            line.extend(row.cells.iter().cloned());
            line.truncate(columns);
            if self.highlight_line == Some(index + 1) {
                for cell in &mut line {
                    cell.background = Color32::from_rgb(75, 65, 40);
                }
            }
            // Padding width differs between worker pages and CopyDocument pages.
            // Highlight only actual line cells so switching pages cannot flash.
            line.resize(columns, PreviewCell::default());
            cells.extend(line);
        }
        Preview::Grid {
            columns,
            cells,
            total_rows: self.rows.len(),
        }
    }
    fn selected_text(&self, selection: &CopySelection) -> Result<String, String> {
        let (mut start, mut end) = (selection.range.start, selection.range.end);
        if (start.y, start.x) > (end.y, end.x) {
            std::mem::swap(&mut start, &mut end);
        }
        if end.y >= self.rows.len() as u64 {
            return Err("Text selection outside document".into());
        }
        let mut result = String::new();
        for y in start.y..=end.y {
            let row = &self.rows[y as usize];
            let (left, right) = if selection.linewise {
                (0, u32::MAX)
            } else if selection.rectangle {
                (start.x.min(end.x), start.x.max(end.x))
            } else {
                (
                    if y == start.y { start.x } else { 0 },
                    if y == end.y { end.x } else { u32::MAX },
                )
            };
            let mut text = String::new();
            for (byte, grapheme) in row.text.grapheme_indices(true) {
                let x = row.mapping[byte];
                let next_byte = byte + grapheme.len();
                let next_x = row
                    .mapping
                    .get(next_byte)
                    .copied()
                    .unwrap_or(row.widths.len() as u32);
                let width = next_x.saturating_sub(x).max(1);
                if x <= right && x.saturating_add(width) > left {
                    text.push_str(grapheme);
                }
            }
            if y > start.y {
                result.push('\n');
            }
            result.push_str(if selection.trim {
                text.trim_end()
            } else {
                &text
            });
        }
        if selection.linewise {
            result.push('\n');
        }
        Ok(result)
    }
}
impl CopyDocument for TextDocument {
    fn metadata(&self) -> Result<DocumentMetadata, String> {
        Ok(DocumentMetadata {
            revision: 1,
            total_rows: self.rows.len() as u64,
            columns: self.columns.max(1) as u32,
        })
    }
    fn text_row(&self, row: u64) -> Result<Option<DocumentRow>, String> {
        Ok(self.rows.get(row as usize).map(|row| {
            // Navigation and painting use individual spaces for expanded tabs.
            // Selection extraction still uses the original text and byte mapping.
            let mut text = String::new();
            let mut columns = Vec::new();
            for (byte, grapheme) in row.text.grapheme_indices(true) {
                let x = row.mapping[byte];
                if grapheme == "\t" {
                    let end = row
                        .mapping
                        .get(byte + 1)
                        .copied()
                        .unwrap_or(row.widths.len() as u32);
                    for column in x..end {
                        text.push(' ');
                        columns.push(column);
                    }
                } else {
                    text.push_str(grapheme);
                    columns.extend(std::iter::repeat_n(x, grapheme.len()));
                }
            }
            DocumentRow {
                text: TextRow {
                    text,
                    columns,
                    wraps_to_next: false,
                },
                widths: row.widths.clone(),
            }
        }))
    }
    fn resolve_cell(&self, mut cell: CellPosition, right: bool) -> Result<CellPosition, String> {
        let row = self
            .rows
            .get(cell.y as usize)
            .ok_or("Text row outside document")?;
        cell.x = cell.x.min(row.widths.len().saturating_sub(1) as u32);
        while cell.x > 0 && row.widths.get(cell.x as usize) == Some(&0) {
            cell.x -= 1;
        }
        if right {
            cell.x +=
                u32::from(row.widths.get(cell.x as usize).copied().unwrap_or(1)).saturating_sub(1);
        }
        Ok(cell)
    }
    fn selection_text(&self, selection: &CopySelection) -> Result<String, String> {
        self.selected_text(selection)
    }
    fn viewport(&self, top: u64, rows: usize, _: &Appearance) -> Result<Preview, String> {
        Ok(self.grid(top as usize, rows, self.columns))
    }
}
fn cancelled(cancel: &dyn Fn() -> bool) -> Result<(), String> {
    if cancel() {
        Err("Text preview cancelled".into())
    } else {
        Ok(())
    }
}
pub fn read_document(
    request: &TextPreviewRequest,
    options: &TextPreviewOptions,
    cancel: &Cancellation,
) -> Result<Arc<TextDocument>, String> {
    render_document(request, options, &|| cancel.is_cancelled())
}
/// Build a document from a host source on a worker. No filesystem access occurs here.
pub fn render_document(
    request: &TextPreviewRequest,
    options: &TextPreviewOptions,
    cancel: &dyn Fn() -> bool,
) -> Result<Arc<TextDocument>, String> {
    options.validate()?;
    cancelled(cancel)?;
    if request.center_line == Some(0) || request.highlight_line == Some(0) {
        return Err("Text preview lines are one-based".into());
    }
    let content = request.source.read(options.max_bytes, cancel)?;
    cancelled(cancel)?;
    if content.text.len() > options.max_bytes {
        return Err("Text preview exceeds max_bytes".into());
    }
    let text = content.text;
    let ss = syntaxes();
    let syntax = match content
        .syntax
        .as_ref()
        .or(request.syntax.as_ref())
        .unwrap_or(&options.syntax)
    {
        SyntaxChoice::PlainText => ss.find_syntax_plain_text(),
        SyntaxChoice::Auto => {
            let name = request
                .name
                .as_deref()
                .unwrap_or("")
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or("");
            let extension = request.extension.as_deref().unwrap_or("");
            let extension = extension.strip_prefix('.').unwrap_or(extension);
            ss.find_syntax_by_extension(name)
                .or_else(|| ss.find_syntax_by_extension(extension))
                .or_else(|| ss.find_syntax_by_extension(&extension.to_lowercase()))
                .or_else(|| {
                    name.rsplit_once('.')
                        .and_then(|(_, extension)| ss.find_syntax_by_extension(extension))
                })
                .or_else(|| {
                    text.lines()
                        .next()
                        .and_then(|line| ss.find_syntax_by_first_line(line))
                })
                .unwrap_or_else(|| ss.find_syntax_plain_text())
        }
        SyntaxChoice::Named(name) => ss
            .find_syntax_by_name(name)
            .or_else(|| ss.find_syntax_by_extension(name))
            .ok_or_else(|| format!("Unknown text preview syntax: {name}"))?,
    };
    let mut highlighter = HighlightLines::new(syntax, theme(&options.theme)?);
    let mut rows = Vec::new();
    let mut columns = 1;
    let mut cell_count = 0usize;
    let warning = "[Preview truncated: display limit reached (16,384 columns, 1,000,000 cells, or 100,000 rows).]";
    let mut truncated = false;
    for line in text.split_inclusive('\n') {
        cancelled(cancel)?;
        if rows.len() >= 99_999 {
            truncated = true;
            break;
        }
        let content = line.trim_end_matches('\n').trim_end_matches('\r');
        let spans = highlighter
            .highlight_line(line, ss)
            .map_err(|e| e.to_string())?;
        let mut row = Row {
            text: content.into(),
            cells: Vec::new(),
            mapping: Vec::new(),
            widths: Vec::new(),
        };
        let mut offset = 0;
        let mut styles = Vec::new();
        for (style, span) in spans {
            styles.push((offset, style));
            offset += span.len();
        }
        for (byte, grapheme) in content.grapheme_indices(true) {
            let style = styles
                .get(
                    styles
                        .partition_point(|(offset, _)| *offset <= byte)
                        .saturating_sub(1),
                )
                .map(|(_, style)| *style)
                .ok_or("Missing syntax style")?;
            let x = row.cells.len();
            let width = if grapheme == "\t" {
                4 - x % 4
            } else {
                grapheme.width().max(1).min(2)
            };
            if x + width > 16_384 || cell_count + width + warning.len() > 1_000_000 {
                row.text.truncate(byte);
                truncated = true;
                break;
            }
            cell_count += width;
            row.mapping.resize(byte + grapheme.len(), x as u32);
            let base = PreviewCell {
                foreground: Color32::from_rgb(
                    style.foreground.r,
                    style.foreground.g,
                    style.foreground.b,
                ),
                bold: style.font_style.contains(FontStyle::BOLD),
                italic: style.font_style.contains(FontStyle::ITALIC),
                underline: style.font_style.contains(FontStyle::UNDERLINE),
                ..Default::default()
            };
            // Display control bytes literally as replacement glyphs, never interpret them.
            row.cells.push(PreviewCell {
                text: if grapheme == "\t" {
                    " ".into()
                } else if grapheme.chars().any(char::is_control) {
                    "�".into()
                } else {
                    grapheme.into()
                },
                ..base.clone()
            });
            row.widths
                .push(if grapheme == "\t" { 1 } else { width as u8 });
            for _ in 1..width {
                row.cells.push(PreviewCell {
                    text: if grapheme == "\t" {
                        " ".into()
                    } else {
                        String::new()
                    },
                    ..base.clone()
                });
                row.widths.push(if grapheme == "\t" { 1 } else { 0 });
            }
        }
        columns = columns.max(row.cells.len());
        rows.push(row);
        if truncated {
            break;
        }
    }
    if truncated {
        columns = columns.max(warning.len());
        rows.push(Row {
            text: warning.into(),
            cells: warning
                .chars()
                .map(|ch| PreviewCell {
                    text: ch.to_string(),
                    foreground: Color32::YELLOW,
                    ..Default::default()
                })
                .collect(),
            mapping: (0..warning.len() as u32).collect(),
            widths: vec![1; warning.len()],
        });
    }
    if rows.is_empty() {
        rows.push(Row {
            text: String::new(),
            cells: vec![],
            mapping: vec![],
            widths: vec![],
        });
    }
    Ok(Arc::new(TextDocument {
        rows,
        columns,
        highlight_line: request.highlight_line,
    }))
}
type Resolver<T> =
    dyn Fn(&Selection<T>) -> Result<Option<TextPreviewRequest>, String> + Send + Sync;
#[derive(Clone)]
struct Cached {
    key: (u64, usize, String),
    document: Option<Arc<TextDocument>>,
    request: Option<TextPreviewRequest>,
}
pub struct TextPreviewProvider<T> {
    options: TextPreviewOptions,
    resolve: Arc<Resolver<T>>,
    cache: Mutex<Option<Cached>>,
}
impl<T> TextPreviewProvider<T> {
    pub fn new(
        options: TextPreviewOptions,
        resolve: impl Fn(&Selection<T>) -> Result<Option<TextPreviewRequest>, String>
        + Send
        + Sync
        + 'static,
    ) -> Result<Self, String> {
        options.validate()?;
        Ok(Self {
            options,
            resolve: Arc::new(resolve),
            cache: Mutex::new(None),
        })
    }
}
impl<T: 'static> PreviewProvider<T> for TextPreviewProvider<T> {
    fn preview(&self, request: PreviewRequest<T>, cancel: Cancellation) -> Result<Preview, String> {
        self.preview_viewport(request, cancel).map(|p| p.document)
    }
    fn preview_viewport(
        &self,
        request: PreviewRequest<T>,
        cancel: Cancellation,
    ) -> Result<PreviewViewport, String> {
        let key = (
            request.document_version,
            request.selection.index,
            request.selection.text.clone(),
        );
        let cached = self
            .cache
            .lock()
            .map_err(|_| "Text preview cache poisoned")?
            .as_ref()
            .filter(|c| c.key == key)
            .cloned();
        let cached = if let Some(cached) = cached {
            cached
        } else {
            // Do not block UI copy-document access while a host reader performs IO.
            let text = (self.resolve)(&request.selection)?;
            let document = text
                .as_ref()
                .map(|text| read_document(text, &self.options, &cancel))
                .transpose()?;
            cancelled(&|| cancel.is_cancelled())?;
            let cached = Cached {
                key,
                document,
                request: text,
            };
            *self
                .cache
                .lock()
                .map_err(|_| "Text preview cache poisoned")? = Some(cached.clone());
            cached
        };
        let Some(document) = &cached.document else {
            return Ok(PreviewViewport {
                document: Preview::Empty,
                first_row: 0,
            });
        };
        let max_top = document.rows.len().saturating_sub(request.rows as usize);
        let first_row = if request.initial_page {
            cached
                .request
                .as_ref()
                .and_then(|r| r.center_line)
                .map(|line| {
                    line.saturating_sub(1)
                        .saturating_sub(request.rows as usize / 2)
                })
                .unwrap_or(request.scroll_offset)
        } else {
            request.scroll_offset
        }
        .min(max_top);
        Ok(PreviewViewport {
            document: document.grid(first_row, request.rows as usize, request.columns as usize),
            first_row,
        })
    }
    fn copy_document(
        &self,
        version: u64,
        index: usize,
    ) -> Result<Option<Arc<dyn CopyDocument>>, String> {
        let cache = self
            .cache
            .lock()
            .map_err(|_| "Text preview cache poisoned")?;
        Ok(cache
            .as_ref()
            .filter(|c| c.key.0 == version && c.key.1 == index)
            .and_then(|c| c.document.clone())
            .map(|doc| doc as Arc<dyn CopyDocument>))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn highlighted_rows_do_not_color_viewport_padding() {
        let mut request = TextPreviewRequest::snapshot("猫\ttext\nlonger line in the document");
        request.highlight_line = Some(1);
        {
            let options = TextPreviewOptions {
                ..Default::default()
            };
            let document = render_document(&request, &options, &|| false).unwrap();
            let actual_columns = document.rows[0].cells.len();
            for columns in [actual_columns - 1, document.columns, 120] {
                let Preview::Grid { cells, .. } = document.grid(0, 1, columns) else {
                    panic!()
                };
                for (index, cell) in cells.iter().enumerate() {
                    assert_eq!(
                        cell.background,
                        if index < actual_columns {
                            Color32::from_rgb(75, 65, 40)
                        } else {
                            PreviewCell::default().background
                        }
                    );
                }
            }
        }
    }
    #[test]
    fn explicit_extensions_choose_syntax_and_named_syntax_still_overrides() {
        let text = "public class Example { int value = 42; }";
        let options = TextPreviewOptions {
            ..Default::default()
        };
        let mut reference = TextPreviewRequest::snapshot(text);
        reference.syntax = Some(SyntaxChoice::Named("C#".into()));
        let expected = render_document(&reference, &options, &|| false).unwrap();
        for extension in ["cs", ".CS"] {
            let mut request = TextPreviewRequest::snapshot(text);
            request.name = Some("wrong.rs".into());
            request.extension = Some(extension.into());
            let document = render_document(&request, &options, &|| false).unwrap();
            assert_eq!(
                document.rows[0]
                    .cells
                    .iter()
                    .map(|cell| cell.foreground)
                    .collect::<Vec<_>>(),
                expected.rows[0]
                    .cells
                    .iter()
                    .map(|cell| cell.foreground)
                    .collect::<Vec<_>>()
            );
            request.syntax = Some(SyntaxChoice::PlainText);
            let document = render_document(&request, &options, &|| false).unwrap();
            assert!(
                document.rows[0]
                    .cells
                    .iter()
                    .all(|cell| cell.foreground == document.rows[0].cells[0].foreground)
            );
        }
        let mut unknown = TextPreviewRequest::snapshot(text);
        unknown.extension = Some("unknown-extension".into());
        render_document(&unknown, &options, &|| false).unwrap();
    }
    #[test]
    fn display_limits_truncate_documents_with_a_copyable_warning() {
        for text in [
            "猫".repeat(9000),
            ("a".repeat(10_000) + "\n").repeat(101),
            "line\n".repeat(100_001),
        ] {
            let request = TextPreviewRequest::snapshot(text);
            let options = TextPreviewOptions {
                syntax: SyntaxChoice::PlainText,
                ..Default::default()
            };
            let document = render_document(&request, &options, &|| false).unwrap();
            assert!(document.rows.len() <= 100_000);
            assert!(
                document
                    .rows
                    .iter()
                    .map(|row| row.cells.len())
                    .sum::<usize>()
                    <= 1_000_000
            );
            assert!(document.rows.iter().all(|row| row.cells.len() <= 16_384));
            let last = document.rows.len() - 1;
            assert!(document.rows[last].text.starts_with("[Preview truncated:"));
            for row in &document.rows {
                assert_eq!(row.mapping.len(), row.text.len());
            }
            let selected = document
                .selection_text(&CopySelection {
                    range: nfm_egui::copy_mode::CellRange {
                        start: CellPosition {
                            x: 0,
                            y: last as u64,
                        },
                        end: CellPosition {
                            x: 200,
                            y: last as u64,
                        },
                    },
                    linewise: false,
                    trim: false,
                    rectangle: false,
                })
                .unwrap();
            assert_eq!(selected, document.rows[last].text);
        }
    }
    #[test]
    fn host_reader_preserves_multiline_syntax_tabs_and_original_copy() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let read_calls = calls.clone();
        let source = Arc::new(move |limit: usize, cancelled: &dyn Fn() -> bool| {
            assert_eq!(limit, 4096);
            assert!(!cancelled());
            read_calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(TextContent::new(
                "/* comment\n\tstill comment\n*/\nlet value = 1;",
            ))
        });
        let mut request = TextPreviewRequest::new(source);
        request.syntax = Some(SyntaxChoice::Named("Rust".into()));
        request.highlight_line = Some(2);
        let options = TextPreviewOptions {
            max_bytes: 4096,
            ..Default::default()
        };
        let document = render_document(&request, &options, &|| false).unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        // The second line inherits the comment style established by the first.
        assert_eq!(
            document.rows[0].cells[0].foreground,
            document.rows[1].cells[4].foreground
        );
        assert_eq!(&document.text_row(1).unwrap().unwrap().widths[..4], &[1; 4]);
        let Preview::Grid { cells, .. } = document.viewport(1, 1, &Appearance::default()).unwrap()
        else {
            panic!()
        };
        assert_eq!(cells[0].background, Color32::from_rgb(75, 65, 40));
        assert_eq!(
            document
                .selection_text(&CopySelection {
                    range: nfm_egui::copy_mode::CellRange {
                        start: CellPosition { x: 0, y: 1 },
                        end: CellPosition { x: 16, y: 1 }
                    },
                    rectangle: false,
                    linewise: true,
                    trim: false
                })
                .unwrap(),
            "\tstill comment\n"
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "viewport and copy must not reread the host source"
        );
    }
    #[test]
    fn source_errors_cancellation_and_byte_limits_propagate() {
        let options = TextPreviewOptions {
            max_bytes: 2,
            ..Default::default()
        };
        let request = TextPreviewRequest::snapshot("too long");
        assert!(
            render_document(&request, &options, &|| false)
                .err()
                .unwrap()
                .contains("max_bytes")
        );
        assert!(
            render_document(&request, &options, &|| true)
                .err()
                .unwrap()
                .contains("cancelled")
        );
        let source =
            Arc::new(|_: usize, _: &dyn Fn() -> bool| Err("Host snapshot expired".to_owned()));
        assert_eq!(
            render_document(&TextPreviewRequest::new(source), &options, &|| false)
                .err()
                .unwrap(),
            "Host snapshot expired"
        );
        // A custom reader cannot bypass the renderer's byte limit.
        let source = Arc::new(|_: usize, _: &dyn Fn() -> bool| Ok(TextContent::new("too long")));
        assert!(
            render_document(&TextPreviewRequest::new(source), &options, &|| false)
                .err()
                .unwrap()
                .contains("max_bytes")
        );
    }
}
