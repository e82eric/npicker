//! Runs a program and shows what it prints in an output page, as it prints
//! it. The host makes an [`OutputWriter`], shows an `nfm_egui::Output` made
//! with it, and calls [`run`] with the same writer. The program is stopped
//! when the page goes.
//!
//! A host that makes the text itself from what the program prints, a line at
//! a time, calls [`run_lines`] and publishes to the writer on its own.
use nfm_egui::OutputWriter;
use nfm_egui::styled_document::StyledDocument;
pub use nfm_preview_command::CommandSpec;
pub use nfm_preview_command::Stream;
use nfm_preview_command::run_streaming;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How often what was printed so far is shown.
const PUBLISH_EVERY: Duration = Duration::from_millis(100);
/// The most that is kept of what the program prints. It is stopped past this.
const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_LINES: usize = 200_000;

/// How the text a program prints is shown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Format {
    /// As a terminal shows it, with ANSI colors.
    #[default]
    Text,
    /// Laid out as markdown.
    Markdown,
}

/// Run `spec` on a thread of its own and publish what it prints to `writer`
/// as `format`: standard output and standard error together in the order
/// they arrive. A program that cannot start, or fails, says so at the end
/// of the text. `writer` is finished when the program is.
pub fn run(spec: CommandSpec, writer: OutputWriter, format: Format) -> std::io::Result<()> {
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    writer.on_close(move || flag.store(true, Ordering::Release));
    std::thread::Builder::new()
        .name("nfm-output".into())
        .spawn(move || {
            let mut printed = Printed::new(writer.clone(), format);
            let result = run_streaming(
                &spec,
                || stopped.load(Ordering::Acquire),
                |stream, bytes| printed.push(stream, bytes),
            );
            if !stopped.load(Ordering::Acquire) {
                match result {
                    Ok(_) if printed.full => printed.note("Stopped: too much output"),
                    Ok(output) if !output.status.success() => {
                        printed.note(&match output.status.code() {
                            Some(code) => format!("Exited with code {code}"),
                            None => "Stopped by a signal".into(),
                        })
                    }
                    Ok(_) => {}
                    Err(error) => printed.note(&error),
                }
                printed.publish();
            }
            writer.finish();
        })
        .map(|_| ())
}

/// What a program run for its lines reports, in the order it happens.
pub enum Report {
    /// A line the program printed, without its line ending.
    Line { stream: Stream, text: String },
    /// Something to say about the run: it could not start, it failed, or it
    /// printed too much and was stopped.
    Note(String),
    /// The program is done. Nothing follows.
    Finished,
}

/// A line longer than this is reported in pieces.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// Run `spec` on a thread of its own and hand what it prints to `report` a
/// line at a time, on that thread. The program is stopped when `writer` is
/// closed, and nothing is reported after that. The host publishes to `writer`
/// and finishes it.
pub fn run_lines(
    spec: CommandSpec,
    writer: &OutputWriter,
    mut report: impl FnMut(Report) + Send + 'static,
) -> std::io::Result<()> {
    let stopped = Arc::new(AtomicBool::new(false));
    let flag = stopped.clone();
    writer.on_close(move || flag.store(true, Ordering::Release));
    std::thread::Builder::new()
        .name("nfm-output".into())
        .spawn(move || {
            let mut lines = Lines::default();
            let result = run_streaming(
                &spec,
                || stopped.load(Ordering::Acquire),
                |stream, bytes| lines.push(stream, bytes, &mut report),
            );
            if stopped.load(Ordering::Acquire) {
                return;
            }
            lines.flush(&mut report);
            match result {
                Ok(_) if lines.full => report(Report::Note("Stopped: too much output".into())),
                Ok(output) if !output.status.success() => {
                    report(Report::Note(match output.status.code() {
                        Some(code) => format!("Exited with code {code}"),
                        None => "Stopped by a signal".into(),
                    }))
                }
                Ok(_) => {}
                Err(error) => report(Report::Note(error)),
            }
            report(Report::Finished);
        })
        .map(|_| ())
}

/// What each stream printed since its last complete line.
#[derive(Default)]
struct Lines {
    output: Vec<u8>,
    error: Vec<u8>,
    total: usize,
    full: bool,
}
impl Lines {
    /// Take what was printed and report the lines it completes; false stops
    /// the program.
    fn push(&mut self, stream: Stream, bytes: &[u8], report: &mut impl FnMut(Report)) -> bool {
        self.total += bytes.len();
        if self.total > MAX_BYTES {
            self.full = true;
            return false;
        }
        let pending = match stream {
            Stream::Stdout => &mut self.output,
            Stream::Stderr => &mut self.error,
        };
        for &byte in bytes {
            if byte == b'\n' || pending.len() >= MAX_LINE_BYTES {
                report(line(stream, pending));
            }
            if byte != b'\n' {
                pending.push(byte);
            }
        }
        true
    }
    /// Report what was printed after the last line ending.
    fn flush(&mut self, report: &mut impl FnMut(Report)) {
        for (stream, pending) in [
            (Stream::Stdout, &mut self.output),
            (Stream::Stderr, &mut self.error),
        ] {
            if !pending.is_empty() {
                report(line(stream, pending));
            }
        }
    }
}
fn line(stream: Stream, pending: &mut Vec<u8>) -> Report {
    if pending.last() == Some(&b'\r') {
        pending.pop();
    }
    let text = String::from_utf8_lossy(pending).into_owned();
    pending.clear();
    Report::Line { stream, text }
}

/// What the program printed so far, and when it was last shown.
struct Printed {
    writer: OutputWriter,
    bytes: Vec<u8>,
    /// What the page itself says after it.
    notes: Vec<String>,
    format: Format,
    shown: Option<Instant>,
    full: bool,
}
impl Printed {
    fn new(writer: OutputWriter, format: Format) -> Self {
        Self {
            writer,
            bytes: Vec::new(),
            notes: Vec::new(),
            format,
            shown: None,
            full: false,
        }
    }
    /// Take what was printed; false stops the program.
    fn push(&mut self, _stream: Stream, bytes: &[u8]) -> bool {
        let room = MAX_BYTES.saturating_sub(self.bytes.len());
        self.bytes
            .extend_from_slice(&bytes[..bytes.len().min(room)]);
        if bytes.len() > room {
            self.full = true;
            return false;
        }
        if self
            .shown
            .is_none_or(|shown| shown.elapsed() >= PUBLISH_EVERY)
        {
            self.publish();
        }
        true
    }
    /// Add a line of the page's own, dimmed, after what was printed.
    fn note(&mut self, text: &str) {
        self.notes.push(text.into());
    }
    fn publish(&mut self) {
        self.shown = Some(Instant::now());
        self.writer
            .publish(document(self.format, &self.bytes, &self.notes));
    }
}

/// `text` as a document in `format`, followed by `notes`: lines of the
/// page's own, dimmed, such as how the program ended.
pub fn document(format: Format, text: &[u8], notes: &[String]) -> Arc<StyledDocument> {
    match format {
        Format::Text => {
            let mut bytes = text.to_vec();
            for note in notes {
                if !bytes.is_empty() && !bytes.ends_with(b"\n") {
                    bytes.push(b'\n');
                }
                bytes.extend_from_slice(b"\x1b[0m\x1b[2m");
                bytes.extend_from_slice(note.as_bytes());
                bytes.extend_from_slice(b"\x1b[0m\n");
            }
            StyledDocument::from_ansi(&bytes, usize::MAX, MAX_LINES)
                .unwrap_or_else(|error| StyledDocument::from_text(&error))
        }
        Format::Markdown => {
            let mut writer = nfm_markdown::DocumentWriter::default();
            writer.markdown(
                &String::from_utf8_lossy(text),
                &nfm_markdown::Theme::default(),
            );
            let dim = nfm_egui::styled_document::Style {
                flags: nfm_markdown::FAINT,
                ..Default::default()
            };
            for note in notes {
                writer.line_of(note, dim);
            }
            writer.finish()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(writer: &OutputWriter) -> String {
        let document = writer.document().expect("something was published");
        let layout = document
            .layout(200, &nfm_egui::Cancellation::never())
            .unwrap();
        use nfm_egui::copy_document::CopyDocument;
        (0..layout.total_rows())
            .map(|row| layout.text_row(row as u64).unwrap().unwrap().text.text)
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn wait(writer: &OutputWriter) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !writer.is_finished() {
            assert!(Instant::now() < deadline, "the program did not finish");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    fn shell(line: &str) -> CommandSpec {
        let mut spec = CommandSpec::new(if cfg!(windows) { "cmd.exe" } else { "sh" });
        spec.arguments = vec![if cfg!(windows) { "/c" } else { "-c" }.into(), line.into()];
        spec
    }

    #[test]
    fn what_a_program_prints_is_published_and_the_writer_finished() {
        let writer = OutputWriter::new();
        run(shell("echo one&& echo two"), writer.clone(), Format::Text).unwrap();
        wait(&writer);
        let text = text(&writer);
        assert!(text.contains("one") && text.contains("two"), "{text:?}");
        assert!(!text.contains("Exited"), "{text:?}");
    }

    #[test]
    fn a_failure_is_said_after_what_was_printed() {
        let writer = OutputWriter::new();
        run(shell("echo before&& exit 3"), writer.clone(), Format::Text).unwrap();
        wait(&writer);
        let text = text(&writer);
        assert!(text.contains("before"), "{text:?}");
        assert!(text.contains("Exited with code 3"), "{text:?}");

        let writer = OutputWriter::new();
        run(
            CommandSpec::new("nfm-no-such-program"),
            writer.clone(),
            Format::Text,
        )
        .unwrap();
        wait(&writer);
        assert!(!self::text(&writer).is_empty());
    }

    #[test]
    fn markdown_is_laid_out_and_a_failure_still_said_after_it() {
        let writer = OutputWriter::new();
        run(
            shell("echo # Title&& echo.&& echo - one&& exit 3"),
            writer.clone(),
            Format::Markdown,
        )
        .unwrap();
        wait(&writer);
        let text = text(&writer);
        let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        assert_eq!(
            lines,
            ["Title", "", "\u{2022} one", "", "Exited with code 3"]
        );
    }

    #[test]
    fn a_program_run_for_its_lines_reports_each_and_then_that_it_is_done() {
        let writer = OutputWriter::new();
        let (send, receive) = std::sync::mpsc::channel();
        run_lines(
            shell("echo one&& echo two 1>&2&& exit 2"),
            &writer,
            move |report| {
                send.send(report).ok();
            },
        )
        .unwrap();
        let mut seen = Vec::new();
        loop {
            match receive.recv_timeout(Duration::from_secs(20)).unwrap() {
                Report::Line { stream, text } => seen.push(format!(
                    "{}:{}",
                    if matches!(stream, Stream::Stdout) {
                        "out"
                    } else {
                        "err"
                    },
                    text.trim_end()
                )),
                Report::Note(note) => seen.push(format!("note:{note}")),
                Report::Finished => break,
            }
        }
        seen.sort();
        assert_eq!(seen, ["err:two", "note:Exited with code 2", "out:one"]);
        // The host finishes the writer, not the run.
        assert!(!writer.is_finished());
    }

    #[test]
    fn closing_the_writer_stops_the_program() {
        let writer = OutputWriter::new();
        run(
            shell(if cfg!(windows) {
                "ping -n 30 127.0.0.1"
            } else {
                "sleep 30"
            }),
            writer.clone(),
            Format::Text,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let started = Instant::now();
        writer.close();
        wait(&writer);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
