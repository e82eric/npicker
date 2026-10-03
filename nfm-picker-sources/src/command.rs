//! Command stdout as incrementally published picker candidates.
use nfm_preview_command::Stream;
pub use nfm_preview_command::{no_match_exit, CommandEvent, CommandObserver, CommandSpec};
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

pub type CommandValues = Arc<Mutex<HashMap<usize, String>>>;
pub struct CommandSourceOptions {
    pub command: CommandSpec,
    pub line_continuation: Option<String>,
    pub unique: bool,
    pub success_exit_codes: Vec<i32>,
    /// Bound one physical line or joined continuation record, not total output.
    pub max_record_bytes: usize,
}
impl CommandSourceOptions {
    pub fn new(command: CommandSpec) -> Self {
        let codes = if no_match_exit(&command.program.to_string_lossy()) {
            vec![0, 1]
        } else {
            vec![0]
        };
        Self {
            command,
            line_continuation: None,
            unique: false,
            success_exit_codes: codes,
            max_record_bytes: 4 * 1024 * 1024,
        }
    }
}
pub struct CommandSink<T: Copy + Default> {
    pub store: Arc<SnapshotStore<StreamingItemSnapshotWithPayload<T>>>,
    pub payload: T,
    /// Changed accepted values are installed before their searchable rows appear.
    pub values: Option<CommandValues>,
    pub error: Arc<Mutex<Option<String>>>,
}
#[derive(Clone)]
pub struct CommandControl {
    pub cancelled: Arc<AtomicBool>,
    pub changed: Arc<dyn Fn() + Send + Sync>,
    pub failed: Arc<dyn Fn(&str) + Send + Sync>,
}
impl Default for CommandControl {
    fn default() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            changed: Arc::new(|| {}),
            failed: Arc::new(|_| {}),
        }
    }
}
struct CommandItem {
    display: String,
    value: String,
}
fn finish_item(lines: &mut Vec<String>, continuation: Option<&str>) -> Option<CommandItem> {
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
    Some(CommandItem { display, value })
}
struct Records<T: Copy + Default> {
    publisher: PublishingStreamingItemStoreWithPayload<T>,
    payload: T,
    values: Option<CommandValues>,
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
    fn new(options: &CommandSourceOptions, sink: &CommandSink<T>) -> Self {
        Self {
            publisher: PublishingStreamingItemStoreWithPayload::new(sink.store.clone()),
            payload: sink.payload,
            values: sink.values.clone(),
            continuation: options.line_continuation.clone().filter(|s| !s.is_empty()),
            seen: options.unique.then(HashSet::new),
            line: Vec::new(),
            lines: Vec::new(),
            record_bytes: 0,
            limit: options.max_record_bytes,
            next_index: 0,
            dirty: false,
        }
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
            return Err("Command picker continuation record exceeds its byte limit".into());
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
    fn push(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut remaining = bytes;
        while !remaining.is_empty() {
            let count = remaining
                .iter()
                .position(|&b| b == b'\n')
                .map_or(remaining.len(), |i| i + 1);
            if self.line.len().saturating_add(count) > self.limit {
                return Err("Command picker line exceeds its byte limit".into());
            }
            self.line.extend_from_slice(&remaining[..count]);
            if remaining[count - 1] == b'\n' {
                self.finish_line()?;
            }
            remaining = &remaining[count..];
        }
        Ok(())
    }
    fn flush(&mut self, changed: &dyn Fn()) {
        if self.dirty {
            self.publisher.publish();
            self.dirty = false;
            changed();
        }
    }
    fn finish(&mut self) -> Result<(), String> {
        if !self.line.is_empty() {
            self.finish_line()?;
        }
        self.publish_item();
        Ok(())
    }
}

/// Starts an owned worker. Signal `control.cancelled` before discarding its
/// handle. Completion, failure and cancellation all complete the source; normal
/// failures retain already-published candidates and report bounded stderr.
/// No UI context or terminal payload type is required by this module.
pub fn start<T: Copy + Default + Send + Sync + 'static>(
    options: CommandSourceOptions,
    sink: CommandSink<T>,
    control: CommandControl,
) -> Result<JoinHandle<()>, String> {
    let mut records = Records::new(&options, &sink);
    let failure_store = sink.store.clone();
    let failure_error = sink.error.clone();
    let failure_control = control.clone();
    let worker = std::thread::Builder::new()
        .name("nfm-command-source".into())
        .spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), String> {
                    if control.cancelled.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    let mut parse_error = None;
                    let output = nfm_preview_command::run_streaming(
                        &options.command,
                        || control.cancelled.load(Ordering::Acquire),
                        |stream, bytes| {
                            if stream == Stream::Stdout {
                                if let Err(error) = records.push(bytes) {
                                    parse_error = Some(error);
                                    return false;
                                }
                                records.flush(control.changed.as_ref());
                            }
                            true
                        },
                    );
                    if control.cancelled.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    if let Some(error) = parse_error {
                        return Err(format!("{}\n{error}", options.command.describe()));
                    }
                    let output = output?;
                    records.finish()?;
                    if !output
                        .status
                        .code()
                        .is_some_and(|code| options.success_exit_codes.contains(&code))
                    {
                        return Err(format!(
                            "{}\nProcess exited with {}\nStderr:\n{}",
                            options.command.describe(),
                            output.status,
                            String::from_utf8_lossy(&output.stderr)
                        ));
                    }
                    Ok(())
                }));
            let result = result.unwrap_or_else(|panic| {
                let detail = panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown panic");
                Err(format!("Command picker worker failed: {detail}"))
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
        let message = format!("Unable to start command source worker: {error}");
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
    use std::time::{Duration, Instant};
    type Items = StreamingItemSnapshotWithPayload<u64>;
    fn new_sink() -> CommandSink<u64> {
        CommandSink {
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
    fn shell(script: &str) -> CommandSpec {
        #[cfg(windows)]
        let (program, args) = ("cmd.exe", vec!["/d".into(), "/c".into(), script.into()]);
        #[cfg(not(windows))]
        let (program, args) = ("sh", vec!["-c".into(), script.into()]);
        let mut spec = CommandSpec::new(program);
        spec.arguments = args;
        spec.output_limit = 16 * 1024;
        spec
    }
    #[test]
    fn chunk_boundaries_unicode_crlf_duplicates_and_final_continuation_preserve_values() {
        let sink = new_sink();
        let mut options = CommandSourceOptions::new(shell(""));
        options.line_continuation = Some("`".into());
        options.unique = true;
        let mut records = Records::new(&options, &sink);
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
        let mut options = CommandSourceOptions::new(shell(""));
        options.max_record_bytes = 8;
        options.line_continuation = Some("`".into());
        let mut records = Records::new(&options, &sink);
        records.push(b"ok\n").unwrap();
        records.flush(&|| {});
        assert!(records.push(b"123456789").is_err());
        assert_eq!(text(&sink.store), ["ok"]);
        let mut records = Records::new(&options, &sink);
        assert!(records.push(b"abc`\nabc`\n").is_err());
    }
    #[cfg(windows)]
    #[test]
    fn process_failures_retain_partial_results_and_stderr_and_success_codes_work() {
        let sink = new_sink();
        let store = sink.store.clone();
        let error = sink.error.clone();
        start(
            CommandSourceOptions::new(shell("echo partial& echo diagnostic 1>&2 & exit /b 1")),
            sink,
            CommandControl::default(),
        )
        .unwrap()
        .join()
        .unwrap();
        assert_eq!(text(&store), ["partial"]);
        let error = error.lock().unwrap().clone().unwrap();
        assert!(
            error.contains("diagnostic") && error.contains("exited") && error.contains("cmd.exe")
        );
        let sink = new_sink();
        let error = sink.error.clone();
        let mut options = CommandSourceOptions::new(shell("exit /b 1"));
        options.success_exit_codes = vec![0, 1];
        start(options, sink, CommandControl::default())
            .unwrap()
            .join()
            .unwrap();
        assert!(error.lock().unwrap().is_none());
    }
    #[test]
    fn child_fixture() {
        if let Ok(gate) = std::env::var("NFM_SOURCE_GATE") {
            use std::io::Write;
            println!("stream-first");
            std::io::stdout().flush().unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            while !std::path::Path::new(&gate).exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            println!("stream-last");
        }
    }
    #[test]
    fn publishes_first_result_before_child_exit_and_cancels_without_errors() {
        let gate = std::env::temp_dir().join(format!("nfm-source-gate-{}", std::process::id()));
        assert!(!gate.exists());
        let mut command = CommandSpec::new(std::env::current_exe().unwrap());
        command.arguments = vec![
            "--exact".into(),
            "command::tests::child_fixture".into(),
            "--nocapture".into(),
        ];
        command
            .environment
            .push(("NFM_SOURCE_GATE".into(), gate.into_os_string()));
        let sink = new_sink();
        let store = sink.store.clone();
        let error = sink.error.clone();
        let control = CommandControl::default();
        let cancelled = control.cancelled.clone();
        let worker = start(CommandSourceOptions::new(command), sink, control).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !text(&store).iter().any(|s| s == "stream-first") {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!store.is_done());
        cancelled.store(true, Ordering::Release);
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            worker.join().unwrap();
            send.send(()).unwrap();
        });
        receive.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(error.lock().unwrap().is_none());
        assert!(store.is_done());
    }
    #[test]
    fn reporting_callback_panics_do_not_leave_source_incomplete() {
        let sink = new_sink();
        let store = sink.store.clone();
        let error = sink.error.clone();
        let control = CommandControl {
            changed: Arc::new(|| {}),
            failed: Arc::new(|_| panic!("reporting callback")),
            ..Default::default()
        };
        start(
            CommandSourceOptions::new(CommandSpec::new("nfm-nonexistent-fixture")),
            sink,
            control,
        )
        .unwrap()
        .join()
        .unwrap();
        assert!(store.is_done());
        assert!(error
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .contains("Unable to start"));
    }
    #[test]
    fn pre_cancelled_source_completes_without_starting_a_process() {
        let sink = new_sink();
        let store = sink.store.clone();
        let error = sink.error.clone();
        let control = CommandControl::default();
        control.cancelled.store(true, Ordering::Release);
        start(
            CommandSourceOptions::new(CommandSpec::new("nfm-nonexistent-fixture")),
            sink,
            control,
        )
        .unwrap()
        .join()
        .unwrap();
        assert!(store.is_done());
        assert!(error.lock().unwrap().is_none());
    }
}
