//! A text file as incrementally published picker candidates, optionally read
//! from the end so the newest lines of a log or history file arrive first.
use crate::records::{spawn, Records, StreamControl, StreamSink};
use std::{
    collections::VecDeque,
    io::{Read, Seek, SeekFrom},
    path::PathBuf,
    sync::atomic::Ordering,
    thread::JoinHandle,
};

pub(crate) const CHUNK_BYTES: usize = 64 * 1024;
const BOM: &[u8] = b"\xEF\xBB\xBF";

pub struct FileSourceOptions {
    pub path: PathBuf,
    /// Publish records last line first. Continuation groups keep their line
    /// order, and `unique` keeps the newest copy.
    pub reverse: bool,
    pub line_continuation: Option<String>,
    pub unique: bool,
    /// Bound one physical line or joined continuation record, not the file.
    pub max_record_bytes: usize,
}
impl FileSourceOptions {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            reverse: false,
            line_continuation: None,
            unique: false,
            max_record_bytes: crate::records::DEFAULT_MAX_RECORD_BYTES,
        }
    }
}

/// Starts an owned worker with the same completion, failure and cancellation
/// contract as [`crate::command::start`]. The file length is read once, so
/// lines appended while it is read are not included.
pub fn start<T: Copy + Default + Send + Sync + 'static>(
    options: FileSourceOptions,
    sink: StreamSink<T>,
    control: StreamControl,
) -> Result<JoinHandle<()>, String> {
    let records = Records::with(
        options.line_continuation.clone(),
        options.unique,
        options.max_record_bytes,
        &sink,
    );
    spawn("file", records, sink, control, move |records, control| {
        let describe = |error: String| format!("{}\n{error}", options.path.display());
        let mut file = std::fs::File::open(&options.path).map_err(|e| describe(e.to_string()))?;
        if options.reverse {
            read_reverse(&mut file, CHUNK_BYTES, records, control)
        } else {
            read_forward(&mut file, CHUNK_BYTES, records, control)
        }
        .map_err(describe)
    })
}

fn cancelled(control: &StreamControl) -> bool {
    control.cancelled.load(Ordering::Acquire)
}

pub(crate) fn read_forward<T: Copy + Default>(
    file: &mut impl Read,
    chunk: usize,
    records: &mut Records<T>,
    control: &StreamControl,
) -> Result<(), String> {
    let mut buffer = vec![0; chunk];
    let mut first = true;
    loop {
        if cancelled(control) {
            return Ok(());
        }
        let count = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.to_string()),
        };
        let mut bytes = &buffer[..count];
        if std::mem::take(&mut first) {
            bytes = bytes.strip_prefix(BOM).unwrap_or(bytes);
        }
        records.push(bytes)?;
        records.flush(control.changed.as_ref());
    }
    records.finish()?;
    records.flush(control.changed.as_ref());
    Ok(())
}

/// Reads fixed chunks from the end and splits them into lines, newest first.
/// Lines are gathered into a group until the line before it does not end with
/// the continuation marker; the group then goes through `records` in file
/// order, so values match a forward read exactly.
pub(crate) fn read_reverse<T: Copy + Default>(
    file: &mut (impl Read + Seek),
    chunk: usize,
    records: &mut Records<T>,
    control: &StreamControl,
) -> Result<(), String> {
    let mut grouper = Grouper {
        marker: records.continuation().map(|s| s.as_bytes().to_vec()),
        limit: records.limit(),
        group: VecDeque::new(),
        group_bytes: 0,
        overflowed: false,
    };
    let mut position = file.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
    // The bytes of the line that continues into the previously read chunk.
    let mut carry = Vec::new();
    let mut at_end = true;
    // The line in `carry` grew past the limit; its bytes are being dropped
    // until its start is found.
    let mut oversized = false;
    while position > 0 {
        if cancelled(control) {
            return Ok(());
        }
        let count = (chunk as u64).min(position) as usize;
        position -= count as u64;
        let mut data = vec![0; count + carry.len()];
        file.seek(SeekFrom::Start(position))
            .and_then(|_| file.read_exact(&mut data[..count]))
            .map_err(|e| e.to_string())?;
        data[count..].copy_from_slice(&carry);
        let mut end = data.len();
        while let Some(newline) = data[..end].iter().rposition(|&b| b == b'\n') {
            let line = &data[newline + 1..end];
            if std::mem::take(&mut oversized) {
                // The start of an overlong line: leave the whole line out.
                grouper.skip_line(records)?;
            } else if !(std::mem::take(&mut at_end) && line.is_empty()) {
                // A file that ends with a newline has no line after it.
                grouper.line(line, records)?;
            }
            end = newline;
        }
        if oversized || end > grouper.limit {
            oversized = true;
            at_end = false;
            data.clear();
        } else {
            data.truncate(end);
        }
        carry = data;
        records.flush(control.changed.as_ref());
    }
    let first = carry.strip_prefix(BOM).unwrap_or(&carry);
    if oversized {
        grouper.skip_line(records)?;
    } else if !(at_end && first.is_empty()) {
        grouper.line(first, records)?;
    }
    grouper.emit(records)?;
    records.flush(control.changed.as_ref());
    Ok(())
}

struct Grouper {
    marker: Option<Vec<u8>>,
    limit: usize,
    /// Physical lines of the record being gathered, in file order.
    group: VecDeque<Vec<u8>>,
    group_bytes: usize,
    /// The record being gathered grew past the limit; its earlier lines are
    /// dropped as they arrive, and it is counted as skipped once complete.
    overflowed: bool,
}
impl Grouper {
    fn gathering(&self) -> bool {
        !self.group.is_empty() || self.overflowed
    }
    /// Leave out an overlong physical line as a record of its own. Its end,
    /// which would say whether it continues into the record after it, is
    /// not kept, so that record is finished first.
    fn skip_line<T: Copy + Default>(&mut self, records: &mut Records<T>) -> Result<(), String> {
        if self.gathering() {
            self.emit(records)?;
        }
        records.skip_record();
        Ok(())
    }
    fn continues(&self, line: &[u8]) -> bool {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        self.marker
            .as_deref()
            .is_some_and(|marker| line.ends_with(marker))
    }
    fn line<T: Copy + Default>(
        &mut self,
        line: &[u8],
        records: &mut Records<T>,
    ) -> Result<(), String> {
        if self.gathering() && !self.continues(line) {
            self.emit(records)?;
        }
        if self.overflowed {
            // This line continues into the overlong record being dropped.
            return Ok(());
        }
        self.group_bytes = self.group_bytes.saturating_add(line.len() + 1);
        if self.group_bytes > self.limit {
            self.group.clear();
            self.group_bytes = 0;
            self.overflowed = true;
            return Ok(());
        }
        self.group.push_front(line.to_vec());
        Ok(())
    }
    fn emit<T: Copy + Default>(&mut self, records: &mut Records<T>) -> Result<(), String> {
        if std::mem::take(&mut self.overflowed) {
            self.group.clear();
            self.group_bytes = 0;
            records.skip_record();
            return Ok(());
        }
        for mut line in self.group.drain(..) {
            line.push(b'\n');
            records.push(&line)?;
        }
        self.group_bytes = 0;
        records.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nfm_search_core::{
        snapshot_store::SnapshotStore,
        store::{ItemsSource, StreamingItemSnapshotWithPayload},
    };
    use std::{
        io::Cursor,
        sync::{Arc, Mutex},
    };
    type Items = StreamingItemSnapshotWithPayload<u64>;
    fn new_sink() -> StreamSink<u64> {
        StreamSink {
            store: Arc::new(SnapshotStore::new_append_only()),
            payload: 7,
            values: Some(Arc::default()),
            error: Arc::new(Mutex::new(None)),
        }
    }
    fn text(store: &SnapshotStore<Items>) -> Vec<String> {
        store.snapshot().map_or_else(Vec::new, |s| {
            (0..s.len())
                .map(|i| s.get_string_lossy(i, &mut Vec::new()))
                .collect()
        })
    }
    /// Returns (display, value) pairs; a value equal to its display has no entry.
    fn read(
        input: &str,
        reverse: bool,
        chunk: usize,
        continuation: Option<&str>,
        unique: bool,
    ) -> Vec<(String, String)> {
        let sink = new_sink();
        let mut records = Records::with(continuation.map(Into::into), unique, 1 << 20, &sink);
        let control = StreamControl::default();
        let mut file = Cursor::new(input.as_bytes().to_vec());
        if reverse {
            read_reverse(&mut file, chunk, &mut records, &control).unwrap();
        } else {
            read_forward(&mut file, chunk, &mut records, &control).unwrap();
        }
        let values = sink.values.as_ref().unwrap().lock().unwrap();
        text(&sink.store)
            .into_iter()
            .enumerate()
            .map(|(i, display)| {
                let value = values.get(&i).cloned().unwrap_or_else(|| display.clone());
                (display, value)
            })
            .collect()
    }
    fn reversed(mut items: Vec<(String, String)>) -> Vec<(String, String)> {
        items.reverse();
        items
    }

    #[test]
    fn reverse_matches_forward_for_every_chunk_size() {
        let inputs = [
            "",
            "\n",
            "one",
            "one\n",
            "\u{FEFF}one\r\ntwo\r\n",
            "a`\nb`\nc\nd\n\ne`\n",
            "猫`\r\nsecond\r\n\nlast`",
            "x`\n\ny\n",
            "first\nmulti`\nline`\nend\nlast",
        ];
        for input in inputs {
            let forward = read(input, false, 3, Some("`"), false);
            for chunk in 1..=input.len().max(1) + 1 {
                assert_eq!(
                    read(input, true, chunk, Some("`"), false),
                    reversed(forward.clone()),
                    "input {input:?} chunk {chunk}"
                );
            }
        }
    }
    #[test]
    fn continuation_groups_keep_line_order_and_values() {
        let items = read("ls`\n  -Force\nGet-Item x\n", true, 4, Some("`"), false);
        assert_eq!(
            items,
            [
                ("Get-Item x".to_string(), "Get-Item x".to_string()),
                ("ls`   -Force".to_string(), "ls\n  -Force".to_string()),
            ]
        );
    }
    #[test]
    fn unique_keeps_the_newest_copy() {
        let items = read("a\nb\na\nc\nb\n", true, 2, None, true);
        let displays: Vec<_> = items.into_iter().map(|(d, _)| d).collect();
        assert_eq!(displays, ["b", "c", "a"]);
    }
    #[test]
    fn overlong_records_are_skipped_the_same_in_both_directions() {
        // (display, skipped) for an 8-byte limit.
        let run = |input: &str, reverse: bool, chunk: usize| {
            let sink = new_sink();
            let control = StreamControl::default();
            let mut records = Records::with(Some("`".into()), false, 8, &sink);
            let mut file = Cursor::new(input.as_bytes().to_vec());
            if reverse {
                read_reverse(&mut file, chunk, &mut records, &control).unwrap();
            } else {
                read_forward(&mut file, chunk, &mut records, &control).unwrap();
            }
            (text(&sink.store), sink.store.skipped())
        };
        let inputs = [
            ("ok\n123456789012\nnext\n", vec!["ok", "next"], 1),
            ("abc`\nabc`\nabc\nlast\n", vec!["last"], 1),
            ("0123456789\nok", vec!["ok"], 1),
            ("ok\n0123456789", vec!["ok"], 1),
            ("a\n0123456789\nb\nabcdefghijk\nc\n", vec!["a", "b", "c"], 2),
        ];
        for (input, expected, skipped) in inputs {
            let expected: Vec<String> = expected.into_iter().map(String::from).collect();
            assert_eq!(run(input, false, 3), (expected.clone(), skipped), "forward {input:?}");
            let mut reversed = expected.clone();
            reversed.reverse();
            for chunk in 1..=input.len() + 1 {
                assert_eq!(
                    run(input, true, chunk),
                    (reversed.clone(), skipped),
                    "reverse {input:?} chunk {chunk}"
                );
            }
        }
    }
    #[test]
    fn worker_reads_a_file_and_reports_a_missing_one() {
        let path = std::env::temp_dir().join(format!("nfm-file-source-{}", std::process::id()));
        std::fs::write(&path, "old\nnew\n").unwrap();
        let sink = new_sink();
        let store = sink.store.clone();
        let mut options = FileSourceOptions::new(&path);
        options.reverse = true;
        start(options, sink, StreamControl::default())
            .unwrap()
            .join()
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(store.is_done());
        assert_eq!(text(&store), ["new", "old"]);

        let sink = new_sink();
        let store = sink.store.clone();
        let error = sink.error.clone();
        start(
            FileSourceOptions::new(&path),
            sink,
            StreamControl::default(),
        )
        .unwrap()
        .join()
        .unwrap();
        assert!(store.is_done());
        assert!(error.lock().unwrap().is_some());
    }
}
