//! Shared plumbing for streaming text sources: lines grouped into records and
//! published as picker candidates, and the worker that owns a source.
use nfm_search_core::{
    snapshot_store::SnapshotStore,
    store::{PublishingStreamingItemStoreWithPayload, StreamingItemSnapshotWithPayload},
};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
};

pub type StreamValues = Arc<Mutex<HashMap<usize, String>>>;
pub struct StreamSink<T: Copy + Default> {
    pub store: Arc<SnapshotStore<StreamingItemSnapshotWithPayload<T>>>,
    pub payload: T,
    /// Changed accepted values are installed before their searchable rows appear.
    pub values: Option<StreamValues>,
    pub error: Arc<Mutex<Option<String>>>,
}
#[derive(Clone)]
pub struct StreamControl {
    pub cancelled: Arc<AtomicBool>,
    pub changed: Arc<dyn Fn() + Send + Sync>,
    pub failed: Arc<dyn Fn(&str) + Send + Sync>,
}
impl Default for StreamControl {
    fn default() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            changed: Arc::new(|| {}),
            failed: Arc::new(|_| {}),
        }
    }
}
struct Record {
    display: String,
    value: String,
}
fn finish_item(lines: &mut Vec<String>, continuation: Option<&str>) -> Option<Record> {
    if lines.is_empty() {
        return None;
    }
    let display = lines.join(" ");
    let mut value = String::new();
    for line in lines.drain(..) {
        if let Some(stripped) = continuation.and_then(|suffix| line.strip_suffix(suffix)) {
            value.push_str(stripped);
            value.push('\n');
        } else {
            value.push_str(&line);
        }
    }
    Some(Record { display, value })
}
/// Groups lines into records, joining continuations, and publishes them. Shared
/// by the command and file sources so both treat lines identically.
pub(crate) struct Records<T: Copy + Default> {
    publisher: PublishingStreamingItemStoreWithPayload<T>,
    payload: T,
    values: Option<StreamValues>,
    continuation: Option<String>,
    seen: Option<HashSet<String>>,
    line: Vec<u8>,
    lines: Vec<String>,
    record_bytes: usize,
    limit: usize,
    next_index: usize,
    dirty: bool,
}
impl<T: Copy + Default> Records<T> {
    pub(crate) fn with(
        line_continuation: Option<String>,
        unique: bool,
        max_record_bytes: usize,
        sink: &StreamSink<T>,
    ) -> Self {
        Self {
            publisher: PublishingStreamingItemStoreWithPayload::new(sink.store.clone()),
            payload: sink.payload,
            values: sink.values.clone(),
            continuation: line_continuation.filter(|s| !s.is_empty()),
            seen: unique.then(HashSet::new),
            line: Vec::new(),
            lines: Vec::new(),
            record_bytes: 0,
            limit: max_record_bytes,
            next_index: 0,
            dirty: false,
        }
    }
    pub(crate) fn continuation(&self) -> Option<&str> {
        self.continuation.as_deref()
    }
    pub(crate) fn limit(&self) -> usize {
        self.limit
    }
    fn publish_item(&mut self) {
        let Some(item) = finish_item(&mut self.lines, self.continuation.as_deref()) else {
            return;
        };
        self.record_bytes = 0;
        if self
            .seen
            .as_mut()
            .is_some_and(|seen| !seen.insert(item.value.clone()))
        {
            return;
        }
        if item.value != item.display {
            if let Some(values) = &self.values {
                values.lock().unwrap().insert(self.next_index, item.value);
            }
        }
        let index = self
            .publisher
            .add_item(item.display.as_bytes(), self.payload);
        debug_assert_eq!(index as usize, self.next_index);
        self.next_index += 1;
        self.dirty = true;
    }
    fn finish_line(&mut self) -> Result<(), String> {
        let raw = std::mem::take(&mut self.line);
        let text = String::from_utf8_lossy(&raw);
        let text = text.trim_end_matches(['\r', '\n']);
        if self.lines.is_empty() && text.is_empty() {
            return Ok(());
        }
        self.record_bytes = self
            .record_bytes
            .saturating_add(text.len())
            .saturating_add(1);
        if self.record_bytes > self.limit {
            return Err("Picker continuation record exceeds its byte limit".into());
        }
        let continued = self
            .continuation
            .as_ref()
            .is_some_and(|suffix| text.ends_with(suffix));
        self.lines.push(text.to_owned());
        if !continued {
            self.publish_item();
        }
        Ok(())
    }
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let count = remaining
                .iter()
                .position(|&b| b == b'\n')
                .map_or(remaining.len(), |i| i + 1);
            if self.line.len().saturating_add(count) > self.limit {
                return Err("Picker line exceeds its byte limit".into());
            }
            self.line.extend_from_slice(&remaining[..count]);
            if remaining[count - 1] == b'\n' {
                self.finish_line()?;
            }
            remaining = &remaining[count..];
        }
        Ok(())
    }
    pub(crate) fn flush(&mut self, changed: &dyn Fn()) {
        if self.dirty {
            self.publisher.publish();
            self.dirty = false;
            changed();
        }
    }
    pub(crate) fn finish(&mut self) -> Result<(), String> {
        if !self.line.is_empty() {
            self.finish_line()?;
        }
        self.publish_item();
        Ok(())
    }
}

/// Runs `work` on a named worker thread with the shared completion contract: a
/// pre-cancelled source does no work, errors and panics are recorded unless
/// cancelled, and the store always completes.
pub(crate) fn spawn<T, F>(
    kind: &'static str,
    mut records: Records<T>,
    sink: StreamSink<T>,
    control: StreamControl,
    work: F,
) -> Result<JoinHandle<()>, String>
where
    T: Copy + Default + Send + Sync + 'static,
    F: FnOnce(&mut Records<T>, &StreamControl) -> Result<(), String> + Send + 'static,
{
    let failure_store = sink.store.clone();
    let failure_error = sink.error.clone();
    let failure_control = control.clone();
    let worker = std::thread::Builder::new()
        .name(format!("nfm-{kind}-source"))
        .spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), String> {
                    if control.cancelled.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    work(&mut records, &control)
                }));
            let result = result.unwrap_or_else(|panic| {
                let detail = panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown panic");
                Err(format!("{kind} picker worker failed: {detail}"))
            });
            if let Err(error) = result {
                if !control.cancelled.load(Ordering::Acquire) {
                    *sink.error.lock().unwrap_or_else(|e| e.into_inner()) = Some(error.clone());
                    // A host reporting callback cannot prevent source completion.
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        (control.failed)(&error)
                    }));
                }
            }
            records.publisher.complete();
            (control.changed)();
        });
    worker.map_err(|error| {
        let message = format!("Unable to start {kind} source worker: {error}");
        *failure_error.lock().unwrap_or_else(|e| e.into_inner()) = Some(message.clone());
        PublishingStreamingItemStoreWithPayload::new(failure_store).complete();
        (failure_control.changed)();
        message
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use nfm_search_core::store::ItemsSource;
    type Items = StreamingItemSnapshotWithPayload<u64>;
    fn new_sink() -> StreamSink<u64> {
        StreamSink {
            store: Arc::new(SnapshotStore::new_append_only()),
            payload: 77,
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
    #[test]
    fn chunk_boundaries_unicode_crlf_duplicates_and_final_continuation_preserve_values() {
        let sink = new_sink();
        let mut records = Records::with(Some("`".into()), true, 4 * 1024 * 1024, &sink);
        let bytes = "猫`\r\nsecond\r\n猫`\nsecond\n\nlast`".as_bytes();
        for byte in bytes {
            records.push(&[*byte]).unwrap();
            records.flush(&|| {});
        }
        records.finish().unwrap();
        records.publisher.complete();
        assert_eq!(text(&sink.store), ["猫` second", "last`"]);
        assert_eq!(
            sink.values
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .get(&0)
                .unwrap(),
            "猫\nsecond"
        );
        assert_eq!(
            sink.values
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .get(&1)
                .unwrap(),
            "last\n"
        );
        assert_eq!(*sink.store.snapshot().unwrap().payload(0), 77);
    }
    #[test]
    fn record_limit_stops_a_long_line_or_continuation_without_losing_old_items() {
        let sink = new_sink();
        let mut records = Records::with(Some("`".into()), false, 8, &sink);
        records.push(b"ok\n").unwrap();
        records.flush(&|| {});
        assert!(records.push(b"123456789").is_err());
        assert_eq!(text(&sink.store), ["ok"]);
        let mut records = Records::with(Some("`".into()), false, 8, &sink);
        assert!(records.push(b"abc`\nabc`\n").is_err());
    }
}
