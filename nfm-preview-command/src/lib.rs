//! Host-independent, bounded command execution for NFM previews.
use std::{
    ffi::OsString,
    io::Read,
    path::PathBuf,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, RecvTimeoutError, SyncSender},
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: PathBuf,
    pub arguments: Vec<String>,
    pub working_directory: Option<PathBuf>,
    pub environment: Vec<(OsString, OsString)>,
    /// Maximum retained bytes per stream. Output beyond this cancels the child.
    pub output_limit: usize,
    /// Optional timing-only observer; callbacks receive no command or output data.
    pub observer: Option<CommandObserver>,
}
impl CommandSpec {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            working_directory: None,
            environment: Vec::new(),
            output_limit: 1024 * 1024,
            observer: None,
        }
    }
    pub fn describe(&self) -> String {
        format!(
            "Command: {:?} {:?}\nWorking directory: {}",
            self.program,
            self.arguments,
            self.working_directory
                .as_ref()
                .map_or_else(|| "inherited".into(), |p| p.display().to_string())
        )
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandEvent {
    SpawnStarted,
    Spawned,
    FirstStdout,
    Finished,
}
#[derive(Clone)]
pub struct CommandObserver(std::sync::Arc<dyn Fn(CommandEvent) + Send + Sync>);
impl CommandObserver {
    pub fn new(callback: impl Fn(CommandEvent) + Send + Sync + 'static) -> Self {
        Self(std::sync::Arc::new(callback))
    }
    fn notify(&self, event: CommandEvent) {
        (self.0)(event);
    }
}
impl std::fmt::Debug for CommandObserver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CommandObserver(..)")
    }
}
struct ObservedRun<'a>(Option<&'a CommandObserver>);
impl Drop for ObservedRun<'_> {
    fn drop(&mut self) {
        if let Some(observer) = self.0 {
            observer.notify(CommandEvent::Finished);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}
#[derive(Debug)]
pub struct CommandOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: ExitStatus,
    pub truncated: bool,
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}
enum Message {
    Bytes(Stream, Vec<u8>),
    Error(String),
}
fn reader(
    mut input: impl Read + Send + 'static,
    stream: Stream,
    sender: SyncSender<Message>,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name(format!("nfm-preview-{stream:?}"))
        .spawn(move || {
            let mut buffer = [0; 8192];
            loop {
                match input.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if sender
                            .send(Message::Bytes(stream, buffer[..count].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => {
                        let _ =
                            sender.send(Message::Error(format!("Unable to read {stream:?}: {e}")));
                        break;
                    }
                }
            }
        })
        .map(|_| ())
        .map_err(|e| format!("Unable to start preview reader: {e}"))
}
/// Runs on the caller's worker. Streams are drained concurrently through a
/// bounded mailbox. Returning false from `output` stops the child as truncated.
/// Readers own pipes only; cancellation never waits on inherited child pipes.
pub fn run(
    spec: &CommandSpec,
    cancelled: impl Fn() -> bool,
    output: impl FnMut(Stream, &[u8]) -> bool,
) -> Result<CommandOutput, String> {
    run_inner(spec, cancelled, output, true)
}

/// Stream an unbounded number of stdout bytes without retaining them. Stderr is
/// drained fully but only `output_limit` bytes are retained for diagnostics.
/// The consumer must bound individual records and its own stored data.
pub fn run_streaming(
    spec: &CommandSpec,
    cancelled: impl Fn() -> bool,
    output: impl FnMut(Stream, &[u8]) -> bool,
) -> Result<CommandOutput, String> {
    run_inner(spec, cancelled, output, false)
}

fn run_inner(
    spec: &CommandSpec,
    cancelled: impl Fn() -> bool,
    mut output: impl FnMut(Stream, &[u8]) -> bool,
    capture: bool,
) -> Result<CommandOutput, String> {
    if cancelled() {
        return Err("preview cancelled".into());
    }
    if spec.program.as_os_str().is_empty() {
        return Err("preview command requires an executable".into());
    }
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.arguments)
        .envs(spec.environment.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &spec.working_directory {
        command.current_dir(cwd);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    let context = spec.describe();
    let _observed = ObservedRun(spec.observer.as_ref());
    if let Some(observer) = &spec.observer {
        observer.notify(CommandEvent::SpawnStarted);
    }
    let mut child = ChildGuard(
        command
            .spawn()
            .map_err(|e| format!("{context}\nUnable to start preview: {e}"))?,
    );
    if let Some(observer) = &spec.observer {
        observer.notify(CommandEvent::Spawned);
    }
    let mut first_stdout = true;
    let (send, receive) = mpsc::sync_channel(32);
    reader(
        child.0.stdout.take().ok_or("Preview has no stdout")?,
        Stream::Stdout,
        send.clone(),
    )?;
    reader(
        child.0.stderr.take().ok_or("Preview has no stderr")?,
        Stream::Stderr,
        send.clone(),
    )?;
    drop(send);
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut status = None;
    let mut finished = false;
    let mut truncated = false;
    loop {
        if cancelled() {
            return Err("preview cancelled".into());
        }
        if !finished {
            match receive.recv_timeout(Duration::from_millis(20)) {
                Ok(Message::Bytes(stream, bytes)) => {
                    if stream == Stream::Stdout && first_stdout && !bytes.is_empty() {
                        first_stdout = false;
                        if let Some(observer) = &spec.observer {
                            observer.notify(CommandEvent::FirstStdout);
                        }
                    }
                    let retained = match stream {
                        Stream::Stdout => &mut stdout,
                        Stream::Stderr => &mut stderr,
                    };
                    let count = bytes
                        .len()
                        .min(spec.output_limit.saturating_sub(retained.len()));
                    if capture || stream == Stream::Stderr {
                        retained.extend_from_slice(&bytes[..count]);
                    }
                    let stopped = if capture {
                        (count > 0 && !output(stream, &bytes[..count])) || count < bytes.len()
                    } else {
                        !output(stream, &bytes)
                    };
                    if stopped {
                        truncated = true;
                        child.0.kill().ok();
                        status = Some(
                            child
                                .0
                                .wait()
                                .map_err(|e| format!("{context}\nUnable to stop preview: {e}"))?,
                        );
                        break;
                    }
                }
                Ok(Message::Error(error)) => return Err(format!("{context}\n{error}")),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => finished = true,
            }
        } else if status.is_none() {
            std::thread::sleep(Duration::from_millis(20));
        }
        if status.is_none() {
            status = child
                .0
                .try_wait()
                .map_err(|e| format!("{context}\nUnable to wait for preview: {e}"))?;
        }
        if status.is_some() && finished {
            break;
        }
    }
    if cancelled() {
        return Err("preview cancelled".into());
    }
    Ok(CommandOutput {
        stdout,
        stderr,
        status: status.unwrap(),
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(windows)]
    fn shell(script: &str) -> CommandSpec {
        let mut s = CommandSpec::new("cmd.exe");
        s.arguments = vec!["/d".into(), "/c".into(), script.into()];
        s
    }
    #[cfg(not(windows))]
    fn shell(script: &str) -> CommandSpec {
        let mut s = CommandSpec::new("sh");
        s.arguments = vec!["-c".into(), script.into()];
        s
    }
    #[test]
    fn observer_reports_spawn_and_first_stdout_once_and_finishes_on_failure() {
        for streaming in [false, true] {
            let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorded = events.clone();
            let mut spec = shell("echo timing");
            spec.observer = Some(CommandObserver::new(move |event| {
                recorded.lock().unwrap().push(event)
            }));
            if streaming {
                run_streaming(&spec, || false, |_, _| true).unwrap();
            } else {
                run(&spec, || false, |_, _| true).unwrap();
            }
            assert_eq!(
                *events.lock().unwrap(),
                vec![
                    CommandEvent::SpawnStarted,
                    CommandEvent::Spawned,
                    CommandEvent::FirstStdout,
                    CommandEvent::Finished
                ]
            );
            events.lock().unwrap().clear();
            spec.program = "nfm-nonexistent-timing-command".into();
            assert!(run(&spec, || false, |_, _| true).is_err());
            assert_eq!(
                *events.lock().unwrap(),
                vec![CommandEvent::SpawnStarted, CommandEvent::Finished]
            );
            events.lock().unwrap().clear();
            assert!(run(&spec, || true, |_, _| true).is_err());
            assert!(events.lock().unwrap().is_empty());
        }
    }
    #[test]
    fn drains_both_streams_and_preserves_exit_status() {
        let spec = shell("echo out & echo diagnostic 1>&2 & exit /b 2");
        #[cfg(not(windows))]
        let spec = shell("echo out; echo diagnostic >&2; exit 2");
        let mut streams = Vec::new();
        let result = run(
            &spec,
            || false,
            |stream, _| {
                streams.push(stream);
                true
            },
        )
        .unwrap();
        assert_eq!(result.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&result.stdout).contains("out"));
        assert!(String::from_utf8_lossy(&result.stderr).contains("diagnostic"));
        assert!(streams.contains(&Stream::Stdout) && streams.contains(&Stream::Stderr));
    }
    #[test]
    fn cancellation_prevents_spawning_and_stops_an_active_child() {
        assert!(
            run(&CommandSpec::new("missing-command"), || true, |_, _| true)
                .unwrap_err()
                .contains("cancelled")
        );
        let mut spec = CommandSpec::new(std::env::current_exe().unwrap());
        spec.arguments = vec![
            "--exact".into(),
            "tests::child_fixture".into(),
            "--nocapture".into(),
        ];
        spec.environment
            .push(("NFM_CHILD_FIXTURE".into(), "wait".into()));
        let cancelled = std::cell::Cell::new(false);
        let start = std::time::Instant::now();
        assert!(
            run(
                &spec,
                || cancelled.get(),
                |_, _| {
                    cancelled.set(true);
                    true
                }
            )
            .unwrap_err()
            .contains("cancelled")
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }
    #[test]
    fn streaming_does_not_capture_or_truncate_total_stdout_and_drains_large_stderr() {
        let mut spec = CommandSpec::new(std::env::current_exe().unwrap());
        spec.arguments = vec![
            "--exact".into(),
            "tests::child_fixture".into(),
            "--nocapture".into(),
        ];
        spec.environment
            .push(("NFM_CHILD_FIXTURE".into(), "bulk".into()));
        spec.output_limit = 16;
        let mut stdout_count = 0;
        let mut stderr_count = 0;
        let result = run_streaming(
            &spec,
            || false,
            |stream, bytes| {
                match stream {
                    Stream::Stdout => stdout_count += bytes.len(),
                    Stream::Stderr => stderr_count += bytes.len(),
                }
                true
            },
        )
        .unwrap();
        assert!(result.status.success() && !result.truncated);
        assert!(result.stdout.is_empty());
        assert_eq!(result.stderr.len(), 16);
        assert!(stdout_count >= 128 * 8192 && stderr_count >= 128 * 8192);
    }
    #[test]
    fn child_fixture() {
        if std::env::var("NFM_CHILD_FIXTURE").as_deref() == Ok("bulk") {
            use std::io::Write;
            for _ in 0..128 {
                std::io::stdout().write_all(&[b'x'; 8192]).unwrap();
                std::io::stderr().write_all(&[b'e'; 8192]).unwrap();
            }
        }
        if std::env::var("NFM_CHILD_FIXTURE").as_deref() == Ok("wait") {
            use std::io::Write;
            println!("ready");
            std::io::stdout().flush().unwrap();
            std::thread::sleep(Duration::from_secs(30));
        }
    }
    #[test]
    fn output_limit_and_consumer_stop_bound_results() {
        let mut spec = shell("echo abcdefghijklmnop");
        spec.output_limit = 3;
        let result = run(&spec, || false, |_, _| true).unwrap();
        assert!(result.truncated);
        assert_eq!(result.stdout, b"abc");
        let result = run(&shell("echo hello"), || false, |_, _| false).unwrap();
        assert!(result.truncated);
    }
    #[test]
    fn working_directory_environment_and_start_errors_are_preserved() {
        let mut spec = shell("echo %NFM_TEST_VALUE% & cd");
        #[cfg(not(windows))]
        {
            spec = shell("echo $NFM_TEST_VALUE; pwd");
        }
        spec.environment
            .push(("NFM_TEST_VALUE".into(), "space value".into()));
        spec.working_directory = Some(std::env::temp_dir());
        let result = run(&spec, || false, |_, _| true).unwrap();
        let text = String::from_utf8_lossy(&result.stdout);
        assert!(text.contains("space value"));
        assert!(
            text.to_lowercase().contains(
                &std::env::temp_dir()
                    .display()
                    .to_string()
                    .trim_end_matches(['/', '\\'])
                    .to_lowercase()
            )
        );
        assert!(
            run(
                &CommandSpec::new("nfm-missing-executable-fixture"),
                || false,
                |_, _| true
            )
            .unwrap_err()
            .contains("Unable to start")
        );
    }
}

/// ripgrep uses exit code one for a successful search with no matches.
pub fn no_match_exit(program: &str) -> bool {
    matches!(
        program
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(program)
            .to_ascii_lowercase()
            .as_str(),
        "rg" | "rg.exe" | "ripgrep" | "ripgrep.exe"
    )
}
