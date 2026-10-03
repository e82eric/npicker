//! Bounded capture of preview streams; Ghostty VT supplies all ANSI parsing.
use crate::preview::PreviewStream;
pub use nfm_preview_vt::styled::{AnsiColor, PreviewLine, PreviewSpan, PreviewStyle};
pub const MAX_PREVIEW_LINES: usize = 4000;
const MAX_BYTES: usize = 20 * 1024 * 1024;
#[derive(Default)]
pub struct PreviewDocument {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    newlines: usize,
    truncated: bool,
}
impl PreviewDocument {
    pub fn push(&mut self, stream: PreviewStream, bytes: &[u8]) {
        let remaining = MAX_BYTES.saturating_sub(self.stdout.len() + self.stderr.len());
        let target = match stream {
            PreviewStream::Stdout => &mut self.stdout,
            PreviewStream::Stderr => &mut self.stderr,
        };
        for &byte in bytes.iter().take(remaining) {
            if self.truncated {
                break;
            }
            if byte == b'\n' {
                self.newlines += 1;
                if self.newlines >= MAX_PREVIEW_LINES {
                    self.truncated = true;
                    break;
                }
            }
            target.push(byte);
        }
        self.truncated |= bytes.len() > remaining;
    }
    pub fn line_limit_reached(&self) -> bool {
        self.truncated
    }
    /// Parse stdout and stderr independently so ANSI state cannot leak between them.
    /// Completed stdout is followed by stderr; read chunk boundaries do not affect parsing.
    pub fn into_lines(self) -> Result<Vec<PreviewLine>, String> {
        let mut lines = if self.stdout.is_empty() && !self.stderr.is_empty() {
            Vec::new()
        } else {
            nfm_preview_vt::styled::parse_lines(&self.stdout, MAX_PREVIEW_LINES)
                .map_err(|e| e.to_string())?
        };
        if !self.stderr.is_empty() && lines.len() < MAX_PREVIEW_LINES {
            lines.extend(
                nfm_preview_vt::styled::parse_lines(&self.stderr, MAX_PREVIEW_LINES - lines.len())
                    .map_err(|e| e.to_string())?,
            );
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn chunked_sequences_and_independent_streams() {
        let mut doc = PreviewDocument::default();
        doc.push(PreviewStream::Stdout, b"plain \x1b[38;2;12;");
        doc.push(PreviewStream::Stderr, b"error");
        doc.push(PreviewStream::Stdout, b"34;56mcolor\x1b[0m end");
        let lines = doc.into_lines().unwrap();
        assert_eq!(lines[0].plain_text(), "plain color end");
        assert_eq!(
            lines[0].spans[1].style.foreground,
            Some(AnsiColor::Rgb(12, 34, 56))
        );
        assert_eq!(lines[1].plain_text(), "error");
        assert_eq!(lines[1].spans[0].style, PreviewStyle::default());
    }
    #[test]
    fn bounded_line_capture() {
        let mut doc = PreviewDocument::default();
        doc.push(
            PreviewStream::Stdout,
            "line\n".repeat(MAX_PREVIEW_LINES + 1).as_bytes(),
        );
        assert!(doc.line_limit_reached());
        assert_eq!(doc.into_lines().unwrap().len(), MAX_PREVIEW_LINES);
    }
}
