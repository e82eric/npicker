//! Styled lines parsed exclusively by Ghostty VT for any renderer.
use crate::{AnsiDocument, ParseError};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnsiColor {
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl AnsiColor {
    pub fn rgb(self) -> (u8, u8, u8) {
        match self {
            Self::Rgb(red, green, blue) => (red, green, blue),
            Self::Indexed(index) => indexed_color(index),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PreviewStyle {
    pub foreground: Option<AnsiColor>,
    pub background: Option<AnsiColor>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub hidden: bool,
    pub inverse: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewSpan {
    pub text: String,
    pub style: PreviewStyle,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PreviewLine {
    pub spans: Vec<PreviewSpan>,
}

impl PreviewLine {
    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
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

/// Parse line-oriented ANSI: normalize bare LF and join soft wraps.
/// Ghostty handles cursor movement, erase commands, styles and graphemes.
pub fn parse_lines(bytes: &[u8], max_lines: usize) -> Result<Vec<PreviewLine>, ParseError> {
    if bytes.len() > 20 * 1024 * 1024 {
        return Err(ParseError::InputTooLarge);
    }
    if max_lines == 0 {
        return Ok(Vec::new());
    }
    let mut normalized = Vec::with_capacity(bytes.len());
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' && (index == 0 || bytes[index - 1] != b'\r') {
            normalized.push(b'\r');
        }
        normalized.push(byte);
    }
    let mut document =
        AnsiDocument::parse_with_limits(&normalized, 512, 1, 40 * 1024 * 1024, 8192)?;
    const FG: u32 = 0x01000000;
    const BG: u32 = 0x02000000;
    let color = |v: u32, default: u32| {
        (v != default).then_some(AnsiColor::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8))
    };
    let mut lines = vec![PreviewLine::default()];
    for y in 0..document.total_rows() {
        let row = document.text_row(y)?.ok_or(ParseError::NativeFailure)?;
        let grid = document.viewport(y, 1, FG, BG)?;
        let text = if row.wraps_to_next {
            row.text.as_str()
        } else {
            let end = row
                .text
                .char_indices()
                .rev()
                .find_map(|(byte, ch)| {
                    (grid.cells[row.columns[byte] as usize].length != 0)
                        .then_some(byte + ch.len_utf8())
                })
                .unwrap_or(0);
            &row.text[..end]
        };
        for (byte, ch) in text.char_indices() {
            let cell = grid.cells[row.columns[byte] as usize];
            let style = PreviewStyle {
                foreground: color(cell.foreground, FG),
                background: color(cell.background, BG),
                bold: cell.flags & 1 != 0,
                italic: cell.flags & 2 != 0,
                strikethrough: cell.flags & 4 != 0,
                inverse: cell.flags & 8 != 0,
                hidden: cell.flags & 16 != 0,
                dim: cell.flags & 32 != 0,
                underline: cell.underline != 0,
            };
            let line = lines.last_mut().unwrap();
            if let Some(span) = line.spans.last_mut().filter(|span| span.style == style) {
                span.text.push(ch);
            } else {
                line.spans.push(PreviewSpan {
                    text: ch.to_string(),
                    style,
                });
            }
        }
        if !row.wraps_to_next && y + 1 < document.total_rows() {
            if lines.len() == max_lines {
                break;
            }
            lines.push(PreviewLine::default());
        }
    }
    Ok(lines)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sgr_unicode_and_newlines() {
        let lines = parse_lines(
            "plain \x1b[1;3;4;9;38;2;12;34;56m\u{732b}e\u{301}\x1b[0m end\nsecond\n".as_bytes(),
            4000,
        )
        .unwrap();
        assert_eq!(
            lines
                .iter()
                .map(PreviewLine::plain_text)
                .collect::<Vec<_>>(),
            ["plain \u{732b}e\u{301} end", "second", ""]
        );
        let style = &lines[0].spans[1].style;
        assert_eq!(style.foreground, Some(AnsiColor::Rgb(12, 34, 56)));
        assert!(style.bold && style.italic && style.underline && style.strikethrough);
        assert_eq!(lines[0].spans[2].style, PreviewStyle::default());
    }
    #[test]
    fn cursor_erase_and_soft_wraps() {
        assert_eq!(
            parse_lines(b"spaces  \n", 4000).unwrap()[0].plain_text(),
            "spaces  "
        );
        assert_eq!(
            parse_lines(b"old\rnewx\x08!\x1b[K", 4000).unwrap()[0].plain_text(),
            "new!"
        );
        assert_eq!(
            parse_lines(b"discard\r\x1b[2Kkept", 4000).unwrap()[0].plain_text(),
            "kept"
        );
        let text = "x".repeat(1200);
        let lines = parse_lines(text.as_bytes(), 4000).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].plain_text(), text);
    }
    #[test]
    fn indexed_colon_colors_and_bounds() {
        let lines =
            parse_lines(b"\x1b[48;5;200mindexed\x1b[0m \x1b[38:2::3:4:5mcolon", 4000).unwrap();
        assert_eq!(
            lines[0].spans[0].style.background,
            Some(AnsiColor::Rgb(255, 0, 215))
        );
        assert_eq!(
            lines[0].spans[2].style.foreground,
            Some(AnsiColor::Rgb(3, 4, 5))
        );
        assert_eq!(parse_lines(b"a\nb\nc", 2).unwrap().len(), 2);
        assert!(parse_lines(&vec![0; 20 * 1024 * 1024 + 1], 4000).is_err());
    }
}
