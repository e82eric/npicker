//! Command stdout as incrementally published picker candidates.
use crate::records::{spawn, Records, StreamControl, StreamSink};
use nfm_preview_command::Stream;
pub use nfm_preview_command::{no_match_exit, CommandEvent, CommandObserver, CommandSpec};
use std::{sync::atomic::Ordering, thread::JoinHandle};

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

/// Starts an owned worker. Signal `control.cancelled` before discarding its
/// handle. Completion, failure and cancellation all complete the source; normal
/// failures retain already-published candidates and report bounded stderr.
/// No UI context or terminal payload type is required by this module.
pub fn start<T: Copy + Default + Send + Sync + 'static>(
    options: CommandSourceOptions,
    sink: StreamSink<T>,
    control: StreamControl,
) -> Result<JoinHandle<()>, String> {
    let records = Records::with(
        options.line_continuation.clone(),
        options.unique,
        options.max_record_bytes,
        &sink,
    );
    spawn(
        "command",
        records,
        sink,
        control,
        move |records, control| {
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
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nfm_search_core::{
        snapshot_store::SnapshotStore,
        store::{ItemsSource, StreamingItemSnapshotWithPayload},
    };
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };
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
    #[cfg(windows)]
    #[test]
    fn process_failures_retain_partial_results_and_stderr_and_success_codes_work() {
        let sink = new_sink();
        let store = sink.store.clone();
        let error = sink.error.clone();
        start(
            CommandSourceOptions::new(shell("echo partial& echo diagnostic 1>&2 & exit /b 1")),
            sink,
            StreamControl::default(),
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
        start(options, sink, StreamControl::default())
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
        let control = StreamControl::default();
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
        let control = StreamControl {
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
        let control = StreamControl::default();
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
