use anstyle_parse::{Params, Parser, Perform};

use crate::preview::PreviewStream;

pub(crate) const MAX_PREVIEW_LINES: usize = 4000;
const MAX_SPANS: usize = 32_768;
const TAB_WIDTH: usize = 4;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AnsiColor {
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl AnsiColor {
    pub(crate) fn rgb(self) -> (u8, u8, u8) {
        match self {
            Self::Rgb(red, green, blue) => (red, green, blue),
            Self::Indexed(index) => indexed_color(index),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct PreviewStyle {
    pub(crate) foreground: Option<AnsiColor>,
    pub(crate) background: Option<AnsiColor>,
    pub(crate) bold: bool,
    pub(crate) dim: bool,
    pub(crate) italic: bool,
    pub(crate) underline: bool,
    pub(crate) strikethrough: bool,
    pub(crate) hidden: bool,
    pub(crate) inverse: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreviewSpan {
    pub(crate) text: String,
    pub(crate) style: PreviewStyle,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PreviewLine {
    pub(crate) spans: Vec<PreviewSpan>,
}

#[derive(Default)]
struct StreamState {
    parser: Parser,
    style: PreviewStyle,
    pending_carriage_return: bool,
}

pub(crate) struct PreviewDocument {
    lines: Vec<PreviewLine>,
    span_count: usize,
    line_limit_reached: bool,
    stdout: StreamState,
    stderr: StreamState,
}

impl Default for PreviewDocument {
    fn default() -> Self {
        Self {
            lines: vec![PreviewLine::default()],
            span_count: 0,
            line_limit_reached: false,
            stdout: StreamState::default(),
            stderr: StreamState::default(),
        }
    }
}

impl PreviewDocument {
    pub(crate) fn push(&mut self, stream: PreviewStream, bytes: &[u8]) {
        let Self {
            lines,
            span_count,
            line_limit_reached,
            stdout,
            stderr,
        } = self;
        let stream = match stream {
            PreviewStream::Stdout => stdout,
            PreviewStream::Stderr => stderr,
        };
        let mut performer = PreviewPerformer {
            lines,
            span_count,
            line_limit_reached,
            style: &mut stream.style,
            pending_carriage_return: &mut stream.pending_carriage_return,
        };
        for &byte in bytes {
            stream.parser.advance(&mut performer, byte);
            if *performer.line_limit_reached {
                break;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn lines(&self) -> &[PreviewLine] {
        &self.lines
    }

    pub(crate) fn line_limit_reached(&self) -> bool {
        self.line_limit_reached
    }

    pub(crate) fn into_lines(self) -> Vec<PreviewLine> {
        self.lines
    }
}

struct PreviewPerformer<'a> {
    lines: &'a mut Vec<PreviewLine>,
    span_count: &'a mut usize,
    line_limit_reached: &'a mut bool,
    style: &'a mut PreviewStyle,
    pending_carriage_return: &'a mut bool,
}

impl PreviewPerformer<'_> {
    fn prepare_for_output(&mut self) {
        if std::mem::take(self.pending_carriage_return) {
            if let Some(line) = self.lines.last_mut() {
                *self.span_count = self.span_count.saturating_sub(line.spans.len());
                line.spans.clear();
            }
        }
    }

    fn append_char(&mut self, ch: char) {
        self.prepare_for_output();
        let line = self.lines.last_mut().expect("preview always has a line");
        if let Some(span) = line
            .spans
            .last_mut()
            .filter(|span| span.style == *self.style)
        {
            span.text.push(ch);
        } else if *self.span_count < MAX_SPANS {
            line.spans.push(PreviewSpan {
                text: ch.to_string(),
                style: self.style.clone(),
            });
            *self.span_count += 1;
        } else if let Some(span) = line.spans.last_mut() {
            span.text.push(ch);
        }
    }

    fn newline(&mut self) {
        *self.pending_carriage_return = false;
        if self.lines.len() < MAX_PREVIEW_LINES {
            self.lines.push(PreviewLine::default());
        } else {
            *self.line_limit_reached = true;
        }
    }

    fn backspace(&mut self) {
        self.prepare_for_output();
        let Some(line) = self.lines.last_mut() else {
            return;
        };
        let Some(span) = line.spans.last_mut() else {
            return;
        };
        span.text.pop();
        if span.text.is_empty() {
            line.spans.pop();
            *self.span_count = self.span_count.saturating_sub(1);
        }
    }

    fn tab(&mut self) {
        self.prepare_for_output();
        let column: usize = self
            .lines
            .last()
            .into_iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.text.chars().count())
            .sum();
        let spaces = TAB_WIDTH - column % TAB_WIDTH;
        for _ in 0..spaces {
            self.append_char(' ');
        }
    }

    fn apply_sgr(&mut self, params: &Params) {
        if params.is_empty() {
            *self.style = PreviewStyle::default();
            return;
        }
        let parameters: Vec<&[u16]> = params.iter().collect();
        let mut index = 0;
        while index < parameters.len() {
            let parameter = parameters[index];
            if parameter.len() > 1 {
                let code = parameter[0];
                if matches!(code, 38 | 48) {
                    if let Some(color) = colon_color(&parameter[1..]) {
                        self.set_extended_color(code, color);
                    }
                } else {
                    self.apply_sgr_code(code);
                }
                index += 1;
                continue;
            }
            let code = parameter.first().copied().unwrap_or(0);
            if matches!(code, 38 | 48) {
                if let Some((color, consumed)) = semicolon_color(&parameters[index + 1..]) {
                    self.set_extended_color(code, color);
                    index += consumed;
                }
            } else {
                self.apply_sgr_code(code);
            }
            index += 1;
        }
    }

    fn set_extended_color(&mut self, code: u16, color: AnsiColor) {
        if code == 38 {
            self.style.foreground = Some(color);
        } else {
            self.style.background = Some(color);
        }
    }

    fn apply_sgr_code(&mut self, code: u16) {
        match code {
            0 => *self.style = PreviewStyle::default(),
            1 => self.style.bold = true,
            2 => self.style.dim = true,
            3 => self.style.italic = true,
            4 => self.style.underline = true,
            7 => self.style.inverse = true,
            8 => self.style.hidden = true,
            9 => self.style.strikethrough = true,
            22 => {
                self.style.bold = false;
                self.style.dim = false;
            }
            23 => self.style.italic = false,
            24 => self.style.underline = false,
            27 => self.style.inverse = false,
            28 => self.style.hidden = false,
            29 => self.style.strikethrough = false,
            30..=37 => self.style.foreground = Some(AnsiColor::Indexed((code - 30) as u8)),
            39 => self.style.foreground = None,
            40..=47 => self.style.background = Some(AnsiColor::Indexed((code - 40) as u8)),
            49 => self.style.background = None,
            90..=97 => self.style.foreground = Some(AnsiColor::Indexed((code - 90 + 8) as u8)),
            100..=107 => self.style.background = Some(AnsiColor::Indexed((code - 100 + 8) as u8)),
            _ => {}
        }
    }
}

impl Perform for PreviewPerformer<'_> {
    fn print(&mut self, ch: char) {
        self.append_char(ch);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => *self.pending_carriage_return = true,
            b'\x08' => self.backspace(),
            b'\t' => self.tab(),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: u8) {
        if !ignore && intermediates.is_empty() && action == b'm' {
            self.apply_sgr(params);
        }
    }
}

fn semicolon_color(parameters: &[&[u16]]) -> Option<(AnsiColor, usize)> {
    let value = |index: usize| parameters.get(index)?.first().copied();
    match value(0)? {
        5 => Some((AnsiColor::Indexed(value(1)?.min(255) as u8), 2)),
        2 => Some((
            AnsiColor::Rgb(
                value(1)?.min(255) as u8,
                value(2)?.min(255) as u8,
                value(3)?.min(255) as u8,
            ),
            4,
        )),
        _ => None,
    }
}

fn colon_color(values: &[u16]) -> Option<AnsiColor> {
    match values {
        [5, index, ..] => Some(AnsiColor::Indexed((*index).min(255) as u8)),
        [2, 0, red, green, blue, ..] | [2, red, green, blue, ..] => Some(AnsiColor::Rgb(
            (*red).min(255) as u8,
            (*green).min(255) as u8,
            (*blue).min(255) as u8,
        )),
        _ => None,
    }
}

fn indexed_color(index: u8) -> (u8, u8, u8) {
    const BASIC: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    match index {
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &PreviewLine) -> String {
        line.spans.iter().map(|span| span.text.as_str()).collect()
    }

    #[test]
    fn parses_split_truecolor_and_reset_sequences() {
        let mut document = PreviewDocument::default();
        document.push(PreviewStream::Stdout, b"plain \x1b[38;2;12;");
        document.push(PreviewStream::Stdout, b"34;56mcolor\x1b[0m end");

        let line = &document.lines()[0];
        assert_eq!(line_text(line), "plain color end");
        assert_eq!(line.spans.len(), 3);
        assert_eq!(
            line.spans[1].style.foreground,
            Some(AnsiColor::Rgb(12, 34, 56))
        );
        assert_eq!(line.spans[2].style, PreviewStyle::default());
    }

    #[test]
    fn parses_indexed_colors_and_font_attributes() {
        let mut document = PreviewDocument::default();
        document.push(
            PreviewStream::Stdout,
            b"\x1b[1;3;4;9;48;5;200mstyled\x1b[22;23;24;29;49mplain",
        );

        let line = &document.lines()[0];
        let styled = &line.spans[0].style;
        assert!(styled.bold && styled.italic && styled.underline && styled.strikethrough);
        assert_eq!(styled.background, Some(AnsiColor::Indexed(200)));
        assert_eq!(line.spans[1].style, PreviewStyle::default());
    }

    #[test]
    fn distinguishes_semicolon_and_colon_truecolor_parameters() {
        let mut document = PreviewDocument::default();
        document.push(
            PreviewStream::Stdout,
            b"\x1b[38;2;0;1;2;4msemi\x1b[0m \x1b[38:2::3:4:5mcolon",
        );

        let line = &document.lines()[0];
        assert_eq!(
            line.spans[0].style.foreground,
            Some(AnsiColor::Rgb(0, 1, 2))
        );
        assert!(line.spans[0].style.underline);
        assert_eq!(
            line.spans[2].style.foreground,
            Some(AnsiColor::Rgb(3, 4, 5))
        );
    }

    #[test]
    fn keeps_stream_style_state_independent() {
        let mut document = PreviewDocument::default();
        document.push(PreviewStream::Stdout, b"\x1b[31mout");
        document.push(PreviewStream::Stderr, b" error");
        document.push(PreviewStream::Stdout, b" red");

        let line = &document.lines()[0];
        assert_eq!(line.spans[0].style.foreground, Some(AnsiColor::Indexed(1)));
        assert_eq!(line.spans[1].style.foreground, None);
        assert_eq!(line.spans[2].style.foreground, Some(AnsiColor::Indexed(1)));
    }

    #[test]
    fn handles_carriage_return_backspace_tab_and_unsupported_sequences() {
        let mut document = PreviewDocument::default();
        document.push(PreviewStream::Stdout, b"old\rnewx\x08\tvalue\x1b[2J");
        assert_eq!(line_text(&document.lines()[0]), "new value");
    }

    #[test]
    fn stops_accumulating_at_four_thousand_lines() {
        let mut document = PreviewDocument::default();
        let output = "line\n".repeat(MAX_PREVIEW_LINES + 1);
        document.push(PreviewStream::Stdout, output.as_bytes());

        assert_eq!(document.lines().len(), MAX_PREVIEW_LINES);
        assert!(document.line_limit_reached());
    }
}
