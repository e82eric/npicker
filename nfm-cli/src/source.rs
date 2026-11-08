//! Candidate sources for line input: plain lines through the shared
//! `nfm-picker-sources` workers, and delimited or CSV records read here.
use crate::options::{Command, Field, InputFormat, Options};
use nfm_picker_sources::{
    command::{CommandSourceOptions, CommandSpec},
    delimited::{
        DelimitedStreamingSnapshot, DelimitedStreamingStore, DelimitedTextSelector,
        DelimitedValueSelector,
    },
    file::FileSourceOptions,
    reader::ReaderSourceOptions,
    records::{StreamControl, StreamSink, StreamValues},
    structured::{StructuredSchema, StructuredStreamingSnapshot, StructuredStreamingStore},
};
use nfm_search_core::{
    snapshot_store::SnapshotStore,
    store::{ItemsSource, StreamingItemSnapshotWithPayload},
};
use std::{
    io::{BufRead, BufReader, IsTerminal, Read},
    process::{Child, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

pub type LineSnapshot = StreamingItemSnapshotWithPayload<u64>;
pub type Repaint = Arc<dyn Fn() + Send + Sync>;

/// A running source. Dropping it stops the worker and any child process.
pub struct Feed<S: ItemsSource> {
    pub store: Arc<SnapshotStore<S>>,
    pub error: Arc<Mutex<Option<String>>>,
    cancel: Arc<AtomicBool>,
    child: Option<Arc<Mutex<Child>>>,
}
impl<S: ItemsSource> Feed<S> {
    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    /// Block until every candidate is published.
    pub fn wait(&self) {
        while !self.store.is_done() {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl<S: ItemsSource> Drop for Feed<S> {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(child) = &self.child {
            let mut child = child.lock().unwrap_or_else(|e| e.into_inner());
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

pub enum Source {
    /// Lines and the values of joined continuation records, by item index.
    Lines(Feed<LineSnapshot>, StreamValues),
    Delimited(Feed<DelimitedStreamingSnapshot>),
    Csv(Feed<StructuredStreamingSnapshot>),
}

pub fn start(options: &Options, repaint: Repaint) -> Result<Source, String> {
    if options.delimiter.is_some() {
        let (reader, child) = open(&options.command)?;
        return Ok(Source::Delimited(delimited(options, reader, child, repaint)));
    }
    if options.input_format == InputFormat::Csv {
        let (reader, child) = open(&options.command)?;
        return Ok(Source::Csv(csv(options, reader, child, repaint)));
    }
    lines(options, repaint)
}

fn lines(options: &Options, repaint: Repaint) -> Result<Source, String> {
    let cancel = Arc::new(AtomicBool::new(false));
    let values = StreamValues::default();
    let sink = StreamSink {
        store: Arc::new(SnapshotStore::new_append_only()),
        payload: 0u64,
        values: Some(values.clone()),
        error: Arc::new(Mutex::new(None)),
    };
    let feed = Feed {
        store: sink.store.clone(),
        error: sink.error.clone(),
        cancel: cancel.clone(),
        child: None,
    };
    let control = StreamControl {
        cancelled: cancel,
        changed: repaint.clone(),
        failed: Arc::new(move |_| repaint()),
    };
    const MAX_RECORD: usize = nfm_picker_sources::records::DEFAULT_MAX_RECORD_BYTES;
    match &options.command {
        Command::Stdin => {
            let stdin = std::io::stdin();
            if stdin.is_terminal() {
                return Err(no_input());
            }
            nfm_picker_sources::reader::start(
                "stdin",
                stdin,
                ReaderSourceOptions {
                    reverse: options.reverse,
                    line_continuation: options.line_continuation.clone(),
                    unique: options.unique,
                    max_record_bytes: MAX_RECORD,
                },
                sink,
                control,
            )?;
        }
        Command::File(path) => {
            let mut file = FileSourceOptions::new(path);
            file.reverse = options.reverse;
            file.line_continuation = options.line_continuation.clone();
            file.unique = options.unique;
            nfm_picker_sources::file::start(file, sink, control)?;
        }
        Command::Run(argv) => {
            let mut spec = CommandSpec::new(&argv[0]);
            spec.arguments = argv[1..].to_vec();
            let mut command = CommandSourceOptions::new(spec);
            command.line_continuation = options.line_continuation.clone();
            command.unique = options.unique;
            command.reverse = options.reverse;
            nfm_picker_sources::command::start(command, sink, control)?;
        }
        _ => unreachable!("only line inputs have a candidate source"),
    }
    Ok(Source::Lines(feed, values))
}

fn no_input() -> String {
    "no input: pipe lines into nfm, or use `nfm file PATH` or `nfm command -- PROGRAM` (see --help)"
        .into()
}

type Input = (Box<dyn Read + Send>, Option<Arc<Mutex<Child>>>);
fn open(command: &Command) -> Result<Input, String> {
    match command {
        Command::Stdin => {
            let stdin = std::io::stdin();
            if stdin.is_terminal() {
                return Err(no_input());
            }
            Ok((Box::new(stdin), None))
        }
        Command::File(path) => {
            let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
            Ok((Box::new(file), None))
        }
        Command::Run(argv) => {
            let mut command = std::process::Command::new(&argv[0]);
            command
                .args(&argv[1..])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                command.creation_flags(CREATE_NO_WINDOW);
            }
            let mut child = command
                .spawn()
                .map_err(|e| format!("{}: {e}", argv[0]))?;
            let stdout = child.stdout.take().expect("piped stdout");
            Ok((Box::new(stdout), Some(Arc::new(Mutex::new(child)))))
        }
        _ => unreachable!("only line inputs are read"),
    }
}

/// Publishes at least every `INTERVAL`, so slow streams still appear promptly.
const INTERVAL: Duration = Duration::from_millis(50);
const BATCH: usize = 1_000;

fn spawn_reader<S: ItemsSource + Send + Sync + 'static>(
    child: Option<Arc<Mutex<Child>>>,
    work: impl FnOnce(&AtomicBool, &SnapshotStore<S>, &Mutex<Option<String>>) + Send + 'static,
    repaint: Repaint,
) -> Feed<S> {
    let store = Arc::new(SnapshotStore::new());
    let error = Arc::new(Mutex::new(None));
    let cancel = Arc::new(AtomicBool::new(false));
    let (worker_store, worker_error, worker_cancel) = (store.clone(), error.clone(), cancel.clone());
    std::thread::Builder::new()
        .name("nfm-cli-reader".into())
        .spawn(move || {
            // Complete the store even if the worker panics, so `Feed::wait`
            // and the picker never wait on a dead thread.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                work(&worker_cancel, &worker_store, &worker_error)
            }));
            if let Err(panic) = result {
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "unknown panic".into());
                *worker_error.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(format!("input reader panicked: {message}"));
            }
            worker_store.complete();
            repaint();
        })
        .expect("spawn reader thread");
    Feed {
        store,
        error,
        cancel,
        child,
    }
}

fn delimited(
    options: &Options,
    reader: Box<dyn Read + Send>,
    child: Option<Arc<Mutex<Child>>>,
    repaint: Repaint,
) -> Feed<DelimitedStreamingSnapshot> {
    let delimiter = options.delimiter.expect("delimited input");
    let text = match options.text_field {
        Field::All => DelimitedTextSelector::FullLine,
        Field::Index(i) => DelimitedTextSelector::Field(i),
    };
    let value = match options.value_field {
        Field::All => DelimitedValueSelector::FullLine,
        Field::Index(i) => DelimitedValueSelector::Field(i),
    };
    let (file_field, line_field) = (options.preview_file_field, options.preview_center_line_field);
    let notify = repaint.clone();
    spawn_reader(
        child,
        move |cancel, shared, error| {
            let mut store = DelimitedStreamingStore::new(delimiter, text, value, file_field, line_field);
            shared.publish(store.snapshot());
            let mut reader = BufReader::with_capacity(64 * 1024, reader);
            let mut bytes = Vec::new();
            let mut published = Instant::now();
            let mut first = true;
            loop {
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                match read_line_bounded(&mut reader, &mut bytes, MAX_RECORD_BYTES) {
                    Ok(None) => break,
                    Ok(Some(Line::Kept)) => {}
                    Ok(Some(Line::TooLong)) => {
                        first = false;
                        shared.add_skipped(1);
                        continue;
                    }
                    Err(e) => {
                        *error.lock().unwrap_or_else(|e| e.into_inner()) = Some(e.to_string());
                        break;
                    }
                }
                let mut line = bytes.as_slice();
                if std::mem::take(&mut first) {
                    line = line.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(line);
                }
                let line = line.strip_suffix(b"\n").unwrap_or(line);
                let line = line.strip_suffix(b"\r").unwrap_or(line);
                let line = String::from_utf8_lossy(line);
                if let Some((offset, length)) = search_range(&line, delimiter, text) {
                    store.add_item(line.as_bytes(), offset, length);
                }
                if store.len().is_multiple_of(BATCH) || published.elapsed() >= INTERVAL {
                    shared.publish(store.snapshot());
                    published = Instant::now();
                    notify();
                }
            }
            shared.publish(store.snapshot());
        },
        repaint,
    )
}

/// The same cap as plain line input: longer lines or CSV records are skipped
/// and counted on the store, which the picker shows beside its totals.
const MAX_RECORD_BYTES: usize = nfm_picker_sources::records::DEFAULT_MAX_RECORD_BYTES;

enum Line {
    Kept,
    /// Longer than the limit; its bytes were dropped up to the newline.
    TooLong,
}

/// Read one line, newline included, into `line`. A line longer than `limit`
/// is not kept, so one huge line never has to fit in memory. None at the end
/// of input.
fn read_line_bounded(
    reader: &mut impl BufRead,
    line: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<Option<Line>> {
    line.clear();
    let mut read_any = false;
    let mut too_long = false;
    loop {
        let (used, ended) = {
            let available = match reader.fill_buf() {
                Ok(available) => available,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if available.is_empty() {
                break;
            }
            read_any = true;
            let (used, ended) = match available.iter().position(|&b| b == b'\n') {
                Some(newline) => (newline + 1, true),
                None => (available.len(), false),
            };
            if !too_long {
                if line.len() + used > limit {
                    too_long = true;
                    line.clear();
                } else {
                    line.extend_from_slice(&available[..used]);
                }
            }
            (used, ended)
        };
        reader.consume(used);
        if ended {
            break;
        }
    }
    Ok(read_any.then_some(if too_long { Line::TooLong } else { Line::Kept }))
}

/// The byte range searched and displayed, or None to skip a line whose text
/// field is missing or empty.
fn search_range(line: &str, delimiter: char, text: DelimitedTextSelector) -> Option<(usize, usize)> {
    let text = match text {
        DelimitedTextSelector::FullLine => line,
        DelimitedTextSelector::Field(field) => line.splitn(field + 1, delimiter).nth(field)?,
    };
    if text.is_empty() {
        return None;
    }
    Some((text.as_ptr() as usize - line.as_ptr() as usize, text.len()))
}

fn csv(
    options: &Options,
    reader: Box<dyn Read + Send>,
    child: Option<Arc<Mutex<Child>>>,
    repaint: Repaint,
) -> Feed<StructuredStreamingSnapshot> {
    let columns = options.csv_columns.clone();
    let types = options.csv_types.clone();
    let formats = options.csv_formats.clone();
    let delimiter = options.csv_delimiter;
    let row_template = options.row_template.clone();
    let row_icon = options.row_icon.clone();
    let notify = repaint.clone();
    spawn_reader(
        child,
        move |cancel, shared, error| {
            let fail = |message: String| {
                *error.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
            };
            let mut csv = csv::ReaderBuilder::new()
                .has_headers(false)
                .flexible(true)
                .delimiter(delimiter)
                .from_reader(reader);
            let mut records = csv.records();
            let schema = match columns {
                Some(columns) => StructuredSchema::new(columns),
                None => match records.next() {
                    Some(Ok(record)) => StructuredSchema::new(
                        record
                            .iter()
                            .map(|c| c.trim_start_matches('\u{FEFF}').to_owned())
                            .collect(),
                    ),
                    Some(Err(e)) => Err(format!("failed to read the CSV header: {e}")),
                    None => Err("CSV input has no header record".into()),
                },
            };
            // Types name columns, so they are applied once the header is known.
            let schema = schema.and_then(|schema| match types.as_deref() {
                Some(types) => schema
                    .with_types(types)
                    .map_err(|e| format!("--csv-types: {e}")),
                None => Ok(schema),
            });
            // Formats are checked against the types, so they come after them.
            let schema = schema.and_then(|schema| match formats.as_deref() {
                Some(formats) => schema
                    .with_formats(formats)
                    .map_err(|e| format!("--csv-formats: {e}")),
                None => Ok(schema),
            });
            let schema = match schema {
                Ok(schema) => schema,
                Err(message) => return fail(message),
            };
            // The template names columns, so it is checked once the header is known.
            let template = match row_template
                .as_deref()
                .map(|spec| schema.row_template(spec, row_icon.as_deref()))
                .transpose()
            {
                Ok(template) => template,
                Err(e) => return fail(format!("--row-template: {e}")),
            };
            let mut store = StructuredStreamingStore::new(schema);
            if let Some(template) = template
                && let Err(message) = store.set_row_template(template)
            {
                return fail(message);
            }
            shared.publish(store.snapshot());
            let mut skipped = 0usize;
            let mut published = Instant::now();
            for record in records {
                if cancel.load(Ordering::Acquire) {
                    return;
                }
                let record = match record {
                    Ok(record) => record,
                    Err(e) => {
                        fail(format!("failed to read a CSV record: {e}"));
                        break;
                    }
                };
                // The csv crate reads a whole record before it can be measured,
                // so this bounds what is kept, not what is read.
                if record.as_slice().len() > MAX_RECORD_BYTES {
                    shared.add_skipped(1);
                } else if store.add_record(&record).is_err() {
                    skipped += 1;
                    shared.add_skipped(1);
                }
                if store.len().is_multiple_of(BATCH) || published.elapsed() >= INTERVAL {
                    shared.publish(store.snapshot());
                    published = Instant::now();
                    notify();
                }
            }
            shared.publish(store.snapshot());
            if skipped > 0 {
                eprintln!("nfm: skipped {skipped} CSV records with the wrong number of fields");
            }
        },
        repaint,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delimited_search_ranges() {
        let line = "a:b:c";
        assert_eq!(search_range(line, ':', DelimitedTextSelector::FullLine), Some((0, 5)));
        // The selected field keeps the rest of the line, so `path:12:a:b`
        // searches `a:b` with --text-field 3.
        assert_eq!(search_range(line, ':', DelimitedTextSelector::Field(1)), Some((2, 3)));
        assert_eq!(search_range(line, ':', DelimitedTextSelector::Field(2)), Some((4, 1)));
        assert_eq!(search_range("a:", ':', DelimitedTextSelector::Field(1)), None);
        assert_eq!(search_range("a", ':', DelimitedTextSelector::Field(3)), None);
    }

    #[test]
    fn delimited_and_csv_readers_publish_everything() {
        use nfm_search_core::store::ItemsSource;
        let mut options = Options {
            delimiter: Some(':'),
            text_field: Field::Index(1),
            value_field: Field::Index(0),
            ..Options::default()
        };
        let input = "v1:one\nv2:\r\nv3:three\n";
        let feed = delimited(&options, Box::new(std::io::Cursor::new(input)), None, Arc::new(|| {}));
        feed.wait();
        let snapshot = feed.store.snapshot().unwrap();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot.item(1).unwrap().value, "v3");

        options.delimiter = None;
        options.input_format = InputFormat::Csv;
        let feed = csv(
            &options,
            Box::new(std::io::Cursor::new("name,size\na,1\nb\nc,3\n")),
            None,
            Arc::new(|| {}),
        );
        feed.wait();
        let snapshot = feed.store.snapshot().unwrap();
        assert_eq!(snapshot.len(), 2);
        assert!(feed.error().is_none());
        // The record with the wrong number of fields is counted as skipped.
        assert_eq!(feed.store.skipped(), 1);
    }

    #[test]
    fn bounded_lines_drop_overlong_ones_and_keep_reading() {
        let input = "short\nthis line is too long\n\nlast";
        // A tiny buffer makes lines span several fills.
        let mut reader = BufReader::with_capacity(4, input.as_bytes());
        let mut line = Vec::new();
        let mut lines = Vec::new();
        while let Some(kind) = read_line_bounded(&mut reader, &mut line, 8).unwrap() {
            lines.push(match kind {
                Line::Kept => String::from_utf8(line.clone()).unwrap(),
                Line::TooLong => "<too long>".into(),
            });
        }
        assert_eq!(lines, ["short\n", "<too long>", "\n", "last"]);
    }

    #[test]
    fn delimited_input_skips_lines_over_the_cap() {
        let options = Options {
            delimiter: Some(':'),
            ..Options::default()
        };
        let long = format!("a:{}\n", "x".repeat(MAX_RECORD_BYTES));
        let input = format!("v1:one\n{long}v3:three\n");
        let feed = delimited(&options, Box::new(std::io::Cursor::new(input)), None, Arc::new(|| {}));
        feed.wait();
        assert_eq!(feed.store.snapshot().unwrap().len(), 2);
        assert_eq!(feed.store.skipped(), 1);
    }
}
