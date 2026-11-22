//! Builds styled documents a line at a time, and lays markdown out as them.
//! A preview or an output page wraps the lines to its width, and copy mode
//! and quick select work on them.
use nfm_egui::egui::Color32;
use nfm_egui::styled_document::{
    ColumnStyleRun, DocumentBuilder, DocumentRange, LineFormat, Position, Style, StyledDocument,
};
use pulldown_cmark::{
    Alignment, BlockQuoteKind, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd,
};
use std::sync::{Arc, OnceLock};
use two_face::re_exports::syntect::easy::HighlightLines;
use two_face::re_exports::syntect::highlighting::FontStyle;
use two_face::re_exports::syntect::parsing::SyntaxSet;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub const BOLD: u8 = 1;
pub const ITALIC: u8 = 2;
pub const STRIKE: u8 = 4;
pub const FAINT: u8 = 32;

/// Colors for markdown; `None` uses the preview's text color.
#[derive(Clone, Debug)]
pub struct Theme {
    pub text: Option<Color32>,
    /// Headings by level, the last repeating for deeper levels.
    pub headings: Vec<Color32>,
    /// Heading sizes by level, as multiples of the text size. Deeper levels
    /// are the text size; empty leaves every heading at it.
    pub heading_scales: Vec<f32>,
    /// Inline code, and fenced code that isn't highlighted.
    pub code: Color32,
    pub code_background: Option<Color32>,
    /// The bat theme fenced code is highlighted with, such as `gruvbox-dark`; `None`,
    /// an unknown theme, or a language bat doesn't know leaves it in `code`.
    pub code_theme: Option<String>,
    pub link: Color32,
    pub quote: Color32,
    pub marker: Color32,
    /// The bars and titles of alerts: note, tip, important, warning, caution.
    pub alerts: [Color32; 5],
    /// Whether a link's address is shown after its text.
    pub link_destinations: bool,
}
impl Default for Theme {
    fn default() -> Self {
        Self {
            text: None,
            headings: vec![
                Color32::from_rgb(0xfa, 0xbd, 0x2f),
                Color32::from_rgb(0x83, 0xa5, 0x98),
                Color32::from_rgb(0x8e, 0xc0, 0x7c),
            ],
            heading_scales: vec![2.0, 1.5, 1.25],
            code: Color32::from_rgb(0xfe, 0x80, 0x19),
            code_background: Some(Color32::from_rgb(0x32, 0x30, 0x2f)),
            code_theme: Some("gruvbox-dark".into()),
            link: Color32::from_rgb(0x83, 0xa5, 0x98),
            quote: Color32::from_rgb(0xa8, 0x99, 0x84),
            marker: Color32::from_rgb(0xa8, 0x99, 0x84),
            alerts: [
                Color32::from_rgb(0x83, 0xa5, 0x98),
                Color32::from_rgb(0xb8, 0xbb, 0x26),
                Color32::from_rgb(0xd3, 0x86, 0x9b),
                Color32::from_rgb(0xfa, 0xbd, 0x2f),
                Color32::from_rgb(0xfb, 0x49, 0x34),
            ],
            link_destinations: true,
        }
    }
}

/// Writes a document a line at a time.
#[derive(Default)]
pub struct DocumentWriter {
    builder: DocumentBuilder,
    lines: usize,
    current: Vec<(String, Style)>,
    /// The length in bytes of the last line ended.
    last_len: usize,
    /// Whether the last line ended was empty, so blank lines don't repeat.
    blank: bool,
    /// How the lines being written are laid out.
    format: LineFormat,
    /// The lines ended, for tests: nfm lays documents out only for previews.
    #[cfg(test)]
    rows: Vec<Vec<(String, Style)>>,
}
impl DocumentWriter {
    /// The index of the line being written.
    pub fn line(&self) -> usize {
        self.lines
    }
    /// The length in bytes of the last line ended, for a range ending there.
    pub fn last_line_len(&self) -> usize {
        self.last_len
    }
    /// Each line's text so far.
    #[cfg(test)]
    pub(crate) fn texts(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|row| row.iter().map(|(text, _)| text.as_str()).collect())
            .collect()
    }
    /// Writes lines `scale` times the text size from the next one ended, as
    /// a heading is: they are that much taller and wrap that much sooner.
    /// 1 is the text size.
    pub fn set_scale(&mut self, scale: f32) {
        self.format.scale = scale;
    }
    /// Appends `text` to the current line; tabs become spaces and line breaks are
    /// dropped.
    pub fn push(&mut self, text: &str, style: Style) {
        let text: String = text
            .chars()
            .filter(|c| *c != '\r' && *c != '\n')
            .map(|c| if c == '\t' { ' ' } else { c })
            .collect();
        if text.is_empty() {
            return;
        }
        match self.current.last_mut() {
            Some((last, last_style)) if *last_style == style => last.push_str(&text),
            _ => self.current.push((text, style)),
        }
    }
    /// Ends the current line, empty or not.
    pub fn end_line(&mut self) {
        let mut text = String::new();
        let mut columns = Vec::new();
        let mut styles = Vec::new();
        let mut x = 0;
        #[cfg(test)]
        self.rows.push(self.current.clone());
        for (segment, style) in std::mem::take(&mut self.current) {
            let start = x;
            for grapheme in segment.graphemes(true) {
                columns.extend(std::iter::repeat_n(x, grapheme.len()));
                x += UnicodeWidthStr::width(grapheme).max(1) as u32;
            }
            text.push_str(&segment);
            styles.push(ColumnStyleRun {
                columns: start..x,
                style,
            });
        }
        self.blank = text.is_empty();
        self.last_len = text.len();
        // Every row here is well formed, so this cannot fail.
        let _ = self
            .builder
            .push_line(&text, &columns, &styles, &self.format);
        self.lines += 1;
    }
    /// Whether the lines ended from here break between words rather than
    /// wherever the row ends.
    pub fn set_word_wrap(&mut self, word_wrap: bool) {
        self.format.word_wrap = word_wrap;
    }
    /// What the rows after the first start with, of the lines ended from
    /// here: an indent the line's text hangs under.
    pub fn set_hang(&mut self, hang: Vec<(String, Style)>) {
        self.format.hang = hang;
    }
    /// Styles the cells from the end of the lines ended from here to the
    /// pane's edge.
    pub fn set_fill(&mut self, fill: Option<Style>) {
        self.format.fill = fill;
    }
    /// Ends the current line with a rule in `style` from its end to the
    /// pane's edge.
    pub fn rule(&mut self, style: Style) {
        let fill = self.format.fill.replace(Style {
            flags: style.flags | STRIKE,
            ..style
        });
        self.end_line();
        self.format.fill = fill;
        self.blank = false;
    }
    /// Ends the current line if it has text.
    pub fn finish_line(&mut self) {
        if !self.current.is_empty() {
            self.end_line();
        }
    }
    /// Ends the current line and leaves one blank line, unless the document is
    /// empty or already ends with one.
    pub fn blank_line(&mut self) {
        self.finish_line();
        if self.lines > 0 && !self.blank {
            self.end_line();
        }
    }
    /// A whole line of `text` in `style`.
    pub fn line_of(&mut self, text: &str, style: Style) {
        self.finish_line();
        self.push(text, style);
        self.end_line();
    }
    /// Lays out `markdown` from the start of a line.
    pub fn markdown(&mut self, markdown: &str, theme: &Theme) {
        self.finish_line();
        Markdown::new(self, theme).render(markdown);
    }
    pub fn finish(mut self) -> Arc<StyledDocument> {
        self.finish_line();
        self.builder.finish()
    }
}

/// The range covering line `line`, `len` bytes long.
pub fn line_range(line: usize, len: usize) -> DocumentRange {
    DocumentRange {
        start: Position { line, byte: 0 },
        end: Position {
            line,
            byte: len.saturating_sub(1),
        },
    }
}

/// bat's syntaxes, loaded the first time code is highlighted.
fn syntaxes() -> &'static SyntaxSet {
    static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
    SYNTAXES.get_or_init(two_face::syntax::extra_newlines)
}

/// A highlighter for code fenced as `language`, such as `rust`, `rs` or
/// `rust,ignore`, in the bat theme `theme`.
fn highlighter(language: &str, theme: Option<&str>) -> Option<HighlightLines<'static>> {
    let theme = theme?;
    static THEMES: OnceLock<two_face::theme::EmbeddedLazyThemeSet> = OnceLock::new();
    let token = language
        .split(|c: char| c == ',' || c.is_whitespace())
        .next()?;
    let syntax = syntaxes().find_syntax_by_token(token)?;
    let name = two_face::theme::EmbeddedLazyThemeSet::theme_names()
        .iter()
        .find(|candidate| candidate.as_name().eq_ignore_ascii_case(theme))?;
    let theme = THEMES.get_or_init(two_face::theme::extra).get(*name);
    Some(HighlightLines::new(syntax, theme))
}

/// What the lines inside a block start with.
enum Block {
    /// A bar down the side. A plain quote's text is in the quote color; an
    /// alert's is not.
    Quote { bar: Color32, plain: bool },
    /// A list: its next number if it is numbered, and the width of the
    /// current item's marker, which the item's other lines are indented by.
    List { next: Option<u64>, width: usize },
    /// Lines indented under a marker or a term, as a footnote's are.
    Indent { width: usize },
}

/// The bullets of lists by depth, repeating.
const BULLETS: [&str; 3] = ["• ", "◦ ", "▪ "];
/// Fences that say only that the code is no language.
const PLAIN_LANGUAGES: [&str; 4] = ["text", "txt", "plain", "plaintext"];

struct Link {
    destination: String,
    /// The text shown for it so far.
    text: String,
    image: bool,
}

struct Markdown<'a> {
    out: &'a mut DocumentWriter,
    theme: &'a Theme,
    flags: u8,
    heading: Option<HeadingLevel>,
    links: Vec<Link>,
    code_block: bool,
    /// The fenced code block's highlighter, if its language is known.
    highlighter: Option<HighlightLines<'static>>,
    blocks: Vec<Block>,
    /// The marker of a list item or footnote whose first line has not started.
    marker: Option<String>,
    table: Option<Table>,
    /// A blank line is owed before the next line, with the bars of the
    /// quotes that line is in.
    gap: bool,
    /// Inside an HTML comment, which can end in a later piece of HTML.
    comment: bool,
    /// Inside a front matter block.
    metadata: bool,
    /// Inside a definition list's term.
    term: bool,
}
#[derive(Default)]
struct Table {
    alignments: Vec<Alignment>,
    /// Rows of cells of styled text.
    rows: Vec<Vec<Vec<(String, Style)>>>,
    head_rows: usize,
}

fn cell_width(cell: &[(String, Style)]) -> usize {
    cell.iter()
        .map(|(text, _)| UnicodeWidthStr::width(text.as_str()))
        .sum()
}

impl<'a> Markdown<'a> {
    fn new(out: &'a mut DocumentWriter, theme: &'a Theme) -> Self {
        Self {
            out,
            theme,
            flags: 0,
            heading: None,
            links: Vec::new(),
            code_block: false,
            highlighter: None,
            blocks: Vec::new(),
            marker: None,
            table: None,
            gap: false,
            comment: false,
            metadata: false,
            term: false,
        }
    }

    fn render(mut self, markdown: &str) {
        let options = Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_FOOTNOTES
            | Options::ENABLE_GFM
            | Options::ENABLE_MATH
            | Options::ENABLE_DEFINITION_LIST
            | Options::ENABLE_HEADING_ATTRIBUTES
            | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS;
        for event in Parser::new_ext(markdown, options) {
            self.event(event);
        }
        self.out.finish_line();
        self.out.set_hang(Vec::new());
        self.out.set_word_wrap(false);
        self.out.set_fill(None);
        if self.gap {
            self.out.blank_line();
        }
    }

    fn style(&self) -> Style {
        let mut style = Style {
            foreground: self.theme.text,
            flags: self.flags,
            ..Default::default()
        };
        if let Some(level) = self.heading {
            let index = level as usize - 1;
            style.foreground = self
                .theme
                .headings
                .get(index)
                .or(self.theme.headings.last())
                .copied();
            // The levels past the sizes and colors are told apart by weight.
            style.flags |= match level {
                HeadingLevel::H5 => BOLD | ITALIC,
                HeadingLevel::H6 => BOLD | FAINT,
                _ => BOLD,
            };
            style.underline = level == HeadingLevel::H1;
        } else if !self.links.is_empty() {
            style.foreground = Some(self.theme.link);
            style.underline = true;
        } else if self
            .blocks
            .iter()
            .any(|block| matches!(block, Block::Quote { plain: true, .. }))
        {
            style.foreground = Some(self.theme.quote);
        }
        if self.term {
            style.flags |= BOLD;
        }
        style
    }

    fn marker_style(&self) -> Style {
        Style {
            foreground: Some(self.theme.marker),
            ..Default::default()
        }
    }

    fn code_style(&self) -> Style {
        Style {
            foreground: Some(self.theme.code),
            background: self.theme.code_background,
            ..Default::default()
        }
    }

    /// The background of a code block's rows, out to the pane's edge.
    fn code_fill(&self) -> Option<Style> {
        self.theme.code_background.map(|background| Style {
            background: Some(background),
            ..Default::default()
        })
    }

    /// What a line inside the open blocks starts with when it is not the
    /// first of a list item: the quotes' bars and the lists' indents. A
    /// line's later rows hang under it.
    fn indent(&self) -> Vec<(String, Style)> {
        self.blocks
            .iter()
            .map(|block| match block {
                Block::Quote { bar, .. } => (
                    "│ ".to_string(),
                    Style {
                        foreground: Some(*bar),
                        ..Default::default()
                    },
                ),
                Block::List { width, .. } | Block::Indent { width } => {
                    (" ".repeat(*width), Style::default())
                }
            })
            .collect()
    }

    /// Starts a line with the quote bars and list indentation it needs,
    /// after the blank line owed before it.
    fn begin_line(&mut self) {
        if !self.out.current.is_empty() {
            return;
        }
        let mut indent = self.indent();
        // The gap is plain, whatever the block before it was.
        self.out.set_hang(Vec::new());
        self.out.set_word_wrap(true);
        self.out.set_fill(None);
        if std::mem::take(&mut self.gap) && self.out.lines > 0 {
            let quoted = self
                .blocks
                .iter()
                .any(|block| matches!(block, Block::Quote { .. }));
            if quoted {
                // The quote's bar runs through the gap.
                for (text, style) in &indent {
                    self.out.push(text, *style);
                }
                self.out.end_line();
            } else {
                self.out.blank_line();
            }
        }
        self.out.set_hang(indent.clone());
        // The innermost list or footnote takes the marker.
        if let Some(marker) = self.marker.take() {
            let marked = self
                .blocks
                .iter()
                .rposition(|block| !matches!(block, Block::Quote { .. }));
            if let Some(index) = marked {
                indent[index] = (marker, self.marker_style());
            }
        }
        for (text, style) in &indent {
            self.out.push(text, *style);
        }
    }

    /// Text in the line being written, or in the table cell being read.
    fn inline(&mut self, text: &str, style: Style) {
        if let Some(table) = &mut self.table {
            if let Some(cell) = table.rows.last_mut().and_then(|row| row.last_mut()) {
                match cell.last_mut() {
                    Some((last, last_style)) if *last_style == style => last.push_str(text),
                    _ => cell.push((text.to_string(), style)),
                }
            }
            return;
        }
        self.begin_line();
        self.out.push(text, style);
    }

    /// Text a reader sees, which a link around it is shown for.
    fn shown(&mut self, text: &str, style: Style) {
        for link in &mut self.links {
            link.text.push_str(text);
        }
        self.inline(text, style);
    }

    fn text(&mut self, text: &str) {
        let style = self.style();
        self.shown(text, style);
    }

    /// Ends the block being written, leaving a blank line before the next.
    fn end_block(&mut self) {
        self.out.finish_line();
        self.gap = true;
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) if self.code_block => self.code_lines(&text),
            Event::Text(text) if self.metadata => self.metadata_lines(&text),
            Event::Text(text) => self.text(&text),
            Event::Code(code) | Event::InlineMath(code) | Event::DisplayMath(code) => {
                let style = self.code_style();
                self.shown(&code, style);
            }
            Event::Html(html) => self.html(&html, true),
            Event::InlineHtml(html) => self.html(&html, false),
            Event::SoftBreak => self.text(" "),
            Event::HardBreak if self.table.is_some() => self.text(" "),
            Event::HardBreak => {
                self.begin_line();
                self.out.end_line();
            }
            Event::Rule => {
                self.out.finish_line();
                self.begin_line();
                let style = self.marker_style();
                self.out.rule(style);
                self.gap = true;
            }
            Event::TaskListMarker(done) => {
                let box_ = if done { "[x] " } else { "[ ] " };
                if self.marker.is_some() {
                    // The box stands in for the item's bullet.
                    self.marker = Some(box_.into());
                    if let Some(Block::List { width, .. }) = self
                        .blocks
                        .iter_mut()
                        .rev()
                        .find(|block| matches!(block, Block::List { .. }))
                    {
                        *width = box_.len();
                    }
                } else {
                    let style = self.marker_style();
                    self.inline(box_, style);
                }
            }
            Event::FootnoteReference(name) => {
                let style = Style {
                    foreground: Some(self.theme.link),
                    ..Default::default()
                };
                self.shown(&format!("[{name}]"), style);
            }
        }
    }

    fn code_lines(&mut self, text: &str) {
        let style = self.code_style();
        for line in text.split_inclusive('\n') {
            self.begin_code_line();
            let pieces = self
                .highlighter
                .as_mut()
                .and_then(|highlighter| highlighter.highlight_line(line, syntaxes()).ok());
            match pieces {
                Some(pieces) => {
                    for (highlight, piece) in pieces {
                        let font = highlight.font_style;
                        let color = highlight.foreground;
                        let style = Style {
                            foreground: Some(Color32::from_rgb(color.r, color.g, color.b)),
                            flags: if font.contains(FontStyle::BOLD) {
                                BOLD
                            } else {
                                0
                            } | if font.contains(FontStyle::ITALIC) {
                                ITALIC
                            } else {
                                0
                            },
                            underline: font.contains(FontStyle::UNDERLINE),
                            ..style
                        };
                        self.out.push(piece, style);
                    }
                }
                None => self.out.push(line, style),
            }
            if line.ends_with('\n') {
                self.out.end_line();
            }
        }
    }

    /// Starts a line of a code block, unless it has started: code keeps its
    /// columns, so it breaks where the row ends, and its background runs to
    /// the pane's edge.
    fn begin_code_line(&mut self) {
        if !self.out.current.is_empty() {
            return;
        }
        self.begin_line();
        let style = self.code_style();
        let mut hang = self.indent();
        hang.push(("  ".into(), style));
        self.out.set_hang(hang);
        self.out.set_word_wrap(false);
        let fill = self.code_fill();
        self.out.set_fill(fill);
        self.out.push("  ", style);
    }

    /// Front matter, shown as it is written.
    fn metadata_lines(&mut self, text: &str) {
        let style = Style {
            flags: FAINT,
            ..self.marker_style()
        };
        for line in text.lines() {
            self.begin_line();
            self.out.set_word_wrap(false);
            self.out.push(line, style);
            self.out.end_line();
        }
    }

    /// `html` without the comments in it; one can end in a later piece.
    fn without_comments(&mut self, html: &str) -> String {
        let mut shown = String::new();
        let mut rest = html;
        loop {
            let (mark, keep) = if self.comment {
                ("-->", false)
            } else {
                ("<!--", true)
            };
            let Some(at) = rest.find(mark) else {
                if keep {
                    shown.push_str(rest);
                }
                return shown;
            };
            if keep {
                shown.push_str(&rest[..at]);
            }
            rest = &rest[at + mark.len()..];
            self.comment = !self.comment;
        }
    }

    /// HTML is shown as it is written, faint, but for comments, which are
    /// left out, and line breaks, which break the line.
    fn html(&mut self, html: &str, block: bool) {
        let shown = self.without_comments(html);
        let style = Style {
            flags: FAINT,
            ..self.style()
        };
        if block {
            for line in shown.lines().filter(|line| !line.trim().is_empty()) {
                self.begin_line();
                self.out.push(line, style);
                self.out.end_line();
            }
            return;
        }
        let tag: String = shown
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_ascii_lowercase();
        if tag == "<br>" || tag == "<br/>" {
            if self.table.is_some() {
                self.inline(" ", style);
            } else {
                self.begin_line();
                self.out.end_line();
            }
        } else if !shown.is_empty() {
            self.inline(&shown, style);
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { level, .. } => {
                // A heading stands apart from what is above it.
                self.end_block();
                self.heading = Some(level);
                let scale = self.theme.heading_scales.get(level as usize - 1).copied();
                self.begin_line();
                self.out.set_scale(scale.unwrap_or(1.0));
            }
            Tag::BlockQuote(kind) => {
                self.out.finish_line();
                let alert = kind.map(|kind| match kind {
                    BlockQuoteKind::Note => ("Note", 0),
                    BlockQuoteKind::Tip => ("Tip", 1),
                    BlockQuoteKind::Important => ("Important", 2),
                    BlockQuoteKind::Warning => ("Warning", 3),
                    BlockQuoteKind::Caution => ("Caution", 4),
                });
                let Some((title, index)) = alert else {
                    self.blocks.push(Block::Quote {
                        bar: self.theme.marker,
                        plain: true,
                    });
                    return;
                };
                let color = self.theme.alerts[index];
                self.blocks.push(Block::Quote {
                    bar: color,
                    plain: false,
                });
                self.begin_line();
                let style = Style {
                    foreground: Some(color),
                    flags: BOLD,
                    ..Default::default()
                };
                self.out.push(title, style);
                self.out.end_line();
            }
            Tag::CodeBlock(kind) => {
                self.out.finish_line();
                if let CodeBlockKind::Fenced(language) = kind
                    && !language.is_empty()
                {
                    if !PLAIN_LANGUAGES.contains(&language.as_ref()) {
                        self.begin_line();
                        let style = Style {
                            flags: FAINT,
                            background: self.theme.code_background,
                            ..self.marker_style()
                        };
                        let fill = self.code_fill();
                        self.out.set_fill(fill);
                        self.out.push(&language, style);
                        self.out.end_line();
                    }
                    self.highlighter = highlighter(&language, self.theme.code_theme.as_deref());
                }
                self.code_block = true;
            }
            Tag::List(start) => {
                self.out.finish_line();
                self.blocks.push(Block::List {
                    next: start,
                    width: 2,
                });
            }
            Tag::Item => {
                self.out.finish_line();
                let depth = self
                    .blocks
                    .iter()
                    .filter(|block| matches!(block, Block::List { .. }))
                    .count();
                if let Some(Block::List { next, width }) = self
                    .blocks
                    .iter_mut()
                    .rev()
                    .find(|block| matches!(block, Block::List { .. }))
                {
                    let marker = match next {
                        Some(number) => {
                            *number += 1;
                            format!("{}. ", *number - 1)
                        }
                        None => BULLETS[(depth - 1) % BULLETS.len()].into(),
                    };
                    *width = UnicodeWidthStr::width(marker.as_str());
                    self.marker = Some(marker);
                }
            }
            Tag::FootnoteDefinition(name) => {
                self.out.finish_line();
                let marker = format!("[{name}] ");
                self.blocks.push(Block::Indent {
                    width: UnicodeWidthStr::width(marker.as_str()),
                });
                self.marker = Some(marker);
            }
            Tag::DefinitionListTitle => {
                self.out.finish_line();
                self.term = true;
            }
            Tag::DefinitionListDefinition => {
                self.out.finish_line();
                self.blocks.push(Block::Indent { width: 2 });
            }
            Tag::MetadataBlock(_) => {
                self.out.finish_line();
                self.metadata = true;
            }
            Tag::Emphasis => self.flags |= ITALIC,
            Tag::Strong => self.flags |= BOLD,
            Tag::Strikethrough => self.flags |= STRIKE,
            Tag::Link { dest_url, .. } => self.links.push(Link {
                destination: dest_url.to_string(),
                text: String::new(),
                image: false,
            }),
            Tag::Image { dest_url, .. } => {
                let style = self.marker_style();
                self.inline("[image: ", style);
                self.links.push(Link {
                    destination: dest_url.to_string(),
                    text: String::new(),
                    image: true,
                });
            }
            Tag::Table(alignments) => {
                self.out.finish_line();
                self.table = Some(Table {
                    alignments,
                    ..Table::default()
                });
            }
            Tag::TableHead | Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(row) = self.table.as_mut().and_then(|t| t.rows.last_mut()) {
                    row.push(Vec::new());
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => self.end_block(),
            TagEnd::Heading(_) => {
                self.heading = None;
                self.end_block();
                self.out.set_scale(1.0);
            }
            TagEnd::BlockQuote(_) => {
                self.out.finish_line();
                self.blocks.pop();
                self.gap = true;
            }
            TagEnd::CodeBlock => {
                self.code_block = false;
                self.highlighter = None;
                self.end_block();
            }
            TagEnd::List(_) => {
                self.out.finish_line();
                self.blocks.pop();
                // A list inside another's item ends without a gap.
                self.gap = !self
                    .blocks
                    .iter()
                    .any(|block| matches!(block, Block::List { .. }));
            }
            TagEnd::Item => {
                // An item with no text still shows its marker.
                if self.marker.is_some() {
                    self.begin_line();
                }
                self.out.finish_line();
            }
            TagEnd::FootnoteDefinition => {
                if self.marker.is_some() {
                    self.begin_line();
                }
                self.out.finish_line();
                self.blocks.pop();
                self.gap = true;
            }
            TagEnd::DefinitionListTitle => {
                self.out.finish_line();
                self.term = false;
                self.gap = false;
            }
            TagEnd::DefinitionListDefinition => {
                self.out.finish_line();
                self.blocks.pop();
                self.gap = false;
            }
            TagEnd::DefinitionList => self.end_block(),
            TagEnd::MetadataBlock(_) => {
                self.metadata = false;
                self.end_block();
            }
            TagEnd::Emphasis => self.flags &= !ITALIC,
            TagEnd::Strong => self.flags &= !BOLD,
            TagEnd::Strikethrough => self.flags &= !STRIKE,
            TagEnd::Link | TagEnd::Image => self.end_link(),
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.head_rows = table.rows.len();
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    self.table_lines(table);
                }
                self.end_block();
            }
            _ => {}
        }
    }

    /// After a link's text, where it goes, unless the text says so. An
    /// image is its description, or its address where it has none.
    fn end_link(&mut self) {
        let Some(link) = self.links.pop() else {
            return;
        };
        let style = Style {
            flags: FAINT,
            ..self.marker_style()
        };
        if link.image {
            if link.text.is_empty() {
                let address = self.style();
                self.inline(&link.destination, address);
            }
            let style = self.marker_style();
            self.inline("]", style);
            return;
        }
        let destination = link.destination.as_str();
        let said = destination == link.text
            || destination.strip_prefix("mailto:") == Some(link.text.as_str());
        // A link to a place in the document goes nowhere a reader can follow.
        if self.theme.link_destinations
            && !destination.is_empty()
            && !destination.starts_with('#')
            && !said
            && self.heading.is_none()
        {
            self.inline(&format!(" ({destination})"), style);
        }
    }

    /// Cells padded to their column's widest, between bars, each column
    /// aligned as the table says.
    fn table_lines(&mut self, table: Table) {
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        let widths: Vec<usize> = (0..columns)
            .map(|column| {
                table
                    .rows
                    .iter()
                    .filter_map(|row| row.get(column))
                    .map(|cell| cell_width(cell))
                    .max()
                    .unwrap_or(0)
            })
            .collect();
        let bar = self.marker_style();
        let plain = self.style();
        for (index, row) in table.rows.iter().enumerate() {
            self.begin_line();
            // A row keeps its columns, so it breaks where the pane ends.
            self.out.set_word_wrap(false);
            for (column, width) in widths.iter().enumerate() {
                if column > 0 {
                    self.out.push(" │ ", bar);
                }
                let cell = row.get(column).map_or(&[][..], Vec::as_slice);
                let pad = width - cell_width(cell);
                let (before, after) = match table.alignments.get(column) {
                    Some(Alignment::Right) => (pad, 0),
                    Some(Alignment::Center) => (pad / 2, pad - pad / 2),
                    _ => (0, pad),
                };
                self.out.push(&" ".repeat(before), plain);
                for (text, style) in cell {
                    let style = Style {
                        flags: style.flags | if index < table.head_rows { BOLD } else { 0 },
                        ..*style
                    };
                    self.out.push(text, style);
                }
                if column + 1 < widths.len() {
                    self.out.push(&" ".repeat(after), plain);
                }
            }
            self.out.end_line();
            if index + 1 == table.head_rows {
                self.begin_line();
                self.out.set_word_wrap(false);
                let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
                self.out.push(&rule.join("─┼─"), bar);
                self.out.end_line();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(markdown: &str) -> DocumentWriter {
        let mut writer = DocumentWriter::default();
        writer.markdown(markdown, &Theme::default());
        writer.finish_line();
        writer
    }
    /// Each line's text.
    fn lines(markdown: &str) -> Vec<String> {
        written(markdown).texts()
    }

    #[test]
    fn blocks_are_separated_and_inline_text_joins() {
        assert_eq!(
            lines("# Title\n\nSome *text*\nwrapped `code`.\n\nNext."),
            ["Title", "", "Some text wrapped code.", "", "Next.", ""]
        );
    }

    #[test]
    fn lists_nest_with_markers_and_continuations() {
        assert_eq!(
            lines("- one\n- two\n  - inner\n3. three\n4. four"),
            ["• one", "• two", "  ◦ inner", "", "3. three", "4. four", ""]
        );
    }

    #[test]
    fn code_blocks_keep_their_lines_and_language() {
        assert_eq!(
            lines("```rust\nfn main() {\n    run();\n}\n```\nAfter"),
            [
                "rust",
                "  fn main() {",
                "      run();",
                "  }",
                "",
                "After",
                ""
            ]
        );
    }

    #[test]
    fn quotes_and_tables_are_drawn() {
        assert_eq!(lines("> quoted\n> more"), ["│ quoted more", ""]);
        assert_eq!(
            lines("| a | bee |\n|---|---|\n| cc | d |"),
            ["a  │ bee", "───┼────", "cc │ d", ""]
        );
    }

    #[test]
    fn styles_follow_emphasis_headings_and_code() {
        let theme = Theme::default();
        let rows = written("## Head\n\n**bold** `x` [link](u)").rows;
        let (heading, style) = &rows[0][0];
        assert_eq!(heading, "Head");
        assert_eq!(style.foreground, Some(theme.headings[1]));
        assert_eq!(style.flags, BOLD);
        let body: Vec<_> = rows[2]
            .iter()
            .map(|(text, style)| (text.as_str(), style.flags, style.foreground))
            .collect();
        assert_eq!(
            body,
            [
                ("bold", BOLD, None),
                (" ", 0, None),
                ("x", 0, Some(theme.code)),
                (" ", 0, None),
                ("link", 0, Some(theme.link)),
                (" (u)", FAINT, Some(theme.marker)),
            ]
        );
    }

    #[test]
    fn headings_are_larger_by_level_and_the_text_after_them_is_not() {
        use nfm_egui::copy_document::CopyDocument;
        let scales = |theme: &Theme| -> Vec<f32> {
            let mut writer = DocumentWriter::default();
            writer.markdown("# One\n\n## Two\n\n### Three\n\n#### Four\n\ntext", theme);
            let layout = writer
                .finish()
                .layout(80, &nfm_egui::Cancellation::never())
                .unwrap();
            // Rows: a heading and a blank line for each level, then the text.
            (0..9).map(|row| layout.row_scale(row)).collect()
        };
        assert_eq!(
            scales(&Theme::default()),
            [2.0, 1.0, 1.5, 1.0, 1.25, 1.0, 1.0, 1.0, 1.0]
        );
        let flat = Theme {
            heading_scales: Vec::new(),
            ..Theme::default()
        };
        assert_eq!(scales(&flat), [1.0; 9]);
    }

    /// The distinct colors on the code line `fn main() {}` fenced as `fence`.
    fn code_colors(fence: &str, theme: Theme) -> Vec<Option<Color32>> {
        let mut writer = DocumentWriter::default();
        writer.markdown(&format!("```{fence}\nfn main() {{}}\n```"), &theme);
        // A fence without a language has no label line above the code.
        let code = writer
            .rows
            .iter()
            .find(|row| row.iter().any(|(text, _)| text.contains("main")));
        let mut colors: Vec<_> = code.unwrap().iter().map(|(_, s)| s.foreground).collect();
        colors.dedup();
        colors
    }

    #[test]
    fn known_languages_are_highlighted_in_the_theme() {
        let theme = Theme::default();
        let plain = Some(theme.code);
        assert!(code_colors("rust", theme.clone()).len() > 2);
        assert!(code_colors("rs,ignore", theme.clone()).len() > 2);
        assert_eq!(code_colors("not-a-language", theme.clone()), [plain]);
        assert_eq!(code_colors("", theme.clone()), [plain]);
        let without = Theme {
            code_theme: None,
            ..theme.clone()
        };
        assert_eq!(code_colors("rust", without), [plain]);
        let unknown = Theme {
            code_theme: Some("no-such-theme".into()),
            ..theme
        };
        assert_eq!(code_colors("rust", unknown), [plain]);
    }

    #[test]
    fn highlighted_code_keeps_the_block_background_and_text() {
        let theme = Theme::default();
        let mut writer = DocumentWriter::default();
        writer.markdown("```rust\nlet x = 1;\n```", &theme);
        let line = &writer.rows[1];
        let text: String = line.iter().map(|(text, _)| text.as_str()).collect();
        assert_eq!(text, "  let x = 1;");
        assert!(
            line.iter()
                .all(|(_, s)| s.background == theme.code_background)
        );
    }

    /// The rows `markdown` takes in a pane `columns` cells wide: where each
    /// row's text starts, and the text.
    fn laid_out(markdown: &str, columns: usize) -> Vec<(u32, String)> {
        use nfm_egui::copy_document::CopyDocument;
        let mut writer = DocumentWriter::default();
        writer.markdown(markdown, &Theme::default());
        let layout = writer
            .finish()
            .layout(columns, &nfm_egui::Cancellation::never())
            .unwrap();
        (0..layout.total_rows() as u64)
            .map(|y| {
                let row = layout.text_row(y).unwrap().unwrap().text;
                (row.columns[0], row.text.trim_end().to_string())
            })
            .collect()
    }
    fn row(start: u32, text: &str) -> (u32, String) {
        (start, text.to_string())
    }

    #[test]
    fn long_lines_break_between_words_and_hang_under_their_block() {
        assert_eq!(
            laid_out("- one two three four\n  - five six seven eight", 12),
            [
                row(0, "• one two"),
                row(2, "three four"),
                // A row keeps a cell for the space its last word ends with.
                row(0, "  ◦ five"),
                row(4, "six"),
                row(4, "seven"),
                row(4, "eight"),
                row(0, ""),
            ]
        );
        assert_eq!(
            laid_out("> quoted words go on\n\n1. numbered item text", 15),
            [
                row(0, "│ quoted words"),
                row(2, "go on"),
                row(0, ""),
                row(0, "1. numbered"),
                row(3, "item text"),
                row(0, ""),
            ]
        );
        // Code and tables keep their columns: they break where the row ends.
        assert_eq!(
            laid_out("```\nlet value = compute();\n```", 14),
            [row(0, "  let value ="), row(2, "compute();"), row(0, "")]
        );
    }

    #[test]
    fn blocks_inside_quotes_and_lists_keep_their_gaps() {
        assert_eq!(
            lines("> one\n>\n> two\n\nafter"),
            ["│ one", "│ ", "│ two", "", "after", ""]
        );
        assert_eq!(
            lines("- one\n\n  more\n\n- two"),
            ["• one", "", "  more", "", "• two", ""]
        );
        assert_eq!(
            lines("- [x] done\n- [ ] open\n- plain"),
            ["[x] done", "[ ] open", "• plain", ""]
        );
        assert_eq!(
            lines("- a\n  - b\n    - c\n      - d"),
            ["• a", "  ◦ b", "    ▪ c", "      • d", ""]
        );
    }

    #[test]
    fn table_cells_keep_their_styles_and_columns_their_alignment() {
        let theme = Theme::default();
        let writer = written("| name | n | mid |\n|:--|--:|:-:|\n| `a` | 1 | x |\n| bb | 22 | yyy |");
        assert_eq!(
            writer.texts(),
            [
                "name │  n │ mid",
                "─────┼────┼────",
                "a    │  1 │  x",
                "bb   │ 22 │ yyy",
                ""
            ]
        );
        let head = &writer.rows[0][0];
        assert_eq!((head.0.as_str(), head.1.flags), ("name", BOLD));
        let code = &writer.rows[2][0];
        assert_eq!((code.0.as_str(), code.1.foreground), ("a", Some(theme.code)));
    }

    #[test]
    fn links_say_where_they_go_and_images_what_they_are() {
        assert_eq!(
            lines("[site](https://x.test) <https://y.test> [top](#top)"),
            ["site (https://x.test) https://y.test top", ""]
        );
        assert_eq!(
            lines("![a chart](c.png) ![](d.png)"),
            ["[image: a chart] [image: d.png]", ""]
        );
        let quiet = Theme {
            link_destinations: false,
            ..Theme::default()
        };
        let mut writer = DocumentWriter::default();
        writer.markdown("[site](https://x.test)", &quiet);
        assert_eq!(writer.texts(), ["site", ""]);
    }

    #[test]
    fn footnotes_alerts_terms_and_front_matter_are_laid_out() {
        assert_eq!(
            lines("Text[^1].\n\n[^1]: The note."),
            ["Text[1].", "", "[1] The note.", ""]
        );
        let theme = Theme::default();
        let alert = written("> [!WARNING]\n> Careful.");
        assert_eq!(alert.texts(), ["│ Warning", "│ Careful.", ""]);
        assert_eq!(alert.rows[0][0].1.foreground, Some(theme.alerts[3]));
        assert_eq!(alert.rows[0][1].1.flags, BOLD);
        // An alert's text is the text color, not a quote's.
        assert_eq!(alert.rows[1][1].1.foreground, None);
        assert_eq!(
            lines("Term\n: What it means.\n\nAfter"),
            ["Term", "  What it means.", "", "After", ""]
        );
        assert_eq!(
            lines("---\ntitle: T\n---\n\n# Head {#id}"),
            ["title: T", "", "Head", ""]
        );
        assert_eq!(lines("Cost $x^2$ here"), ["Cost x^2 here", ""]);
    }

    #[test]
    fn html_comments_are_left_out_and_breaks_break() {
        assert_eq!(
            lines("<!-- hidden\nover lines -->\n\none<br>two <!-- no --> three\n\n<div>\nkept\n</div>"),
            ["one", "two  three", "", "<div>", "kept", "</div>", ""]
        );
    }

    #[test]
    fn code_and_rules_run_to_the_edge_and_plain_fences_have_no_label() {
        use nfm_egui::copy_document::CopyDocument;
        use nfm_egui::egui::Color32;
        assert_eq!(lines("```text\nplain\n```"), ["  plain", ""]);
        let theme = Theme::default();
        let mut writer = DocumentWriter::default();
        writer.markdown("```\ncode\n```\n\n---\n\nend", &theme);
        assert_eq!(writer.texts(), ["  code", "", "", "", "end", ""]);
        let layout = writer
            .finish()
            .layout(10, &nfm_egui::Cancellation::never())
            .unwrap();
        let appearance = nfm_egui::Appearance::default();
        let nfm_egui::Preview::Grid { cells, .. } = layout.viewport(0, 3, &appearance).unwrap()
        else {
            panic!("layouts page as grids");
        };
        // The code's background, and the rule, reach the last cell.
        assert_eq!(cells[9].background, theme.code_background.unwrap());
        assert_ne!(cells[19].background, theme.code_background.unwrap());
        assert!(cells[20].strikethrough && cells[29].strikethrough);
        assert_ne!(cells[29].foreground, Color32::TRANSPARENT);
    }

    #[test]
    fn deeper_headings_differ_by_weight() {
        let flags = |markdown: &str| written(markdown).rows[0][0].1.flags;
        assert_eq!(flags("#### Four"), BOLD);
        assert_eq!(flags("##### Five"), BOLD | ITALIC);
        assert_eq!(flags("###### Six"), BOLD | FAINT);
    }

    #[test]
    fn line_ranges_cover_their_line() {
        let range = line_range(3, 10);
        assert_eq!((range.start.line, range.start.byte), (3, 0));
        assert_eq!((range.end.line, range.end.byte), (3, 9));
    }
}
