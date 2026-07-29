use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender};
use nfm_search_core::search::SearchResult;

use crate::preview_document::PreviewDocument;
pub use crate::preview_document::PreviewLine;
use crate::source_store::AnyItemSource;

const OUTPUT_LIMIT: usize = 1024 * 1024;
const OUTPUT_CHANNEL_CAPACITY: usize = 32;
const DEBOUNCE: Duration = Duration::from_millis(75);

#[derive(Clone, Debug)]
pub enum PreviewUpdate {
    Clear {
        generation: u64,
    },
    Ready {
        generation: u64,
        lines: Arc<[PreviewLine]>,
        truncated: bool,
        center_line: Option<usize>,
    },
    Error {
        generation: u64,
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeWindowId(pub isize);

#[derive(Clone, Debug)]
pub enum PreviewEvent {
    Command(PreviewUpdate),
    NativeWindow(Option<NativeWindowId>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PreviewStream {
    Stdout,
    Stderr,
}

struct OutputChunk {
    stream: PreviewStream,
    bytes: Vec<u8>,
}

pub struct CommandPreviewController {
    program: Arc<PathBuf>,
    arguments: Arc<[String]>,
    working_directory: Option<Arc<PathBuf>>,
    generation: Arc<AtomicU64>,
    events: Sender<PreviewEvent>,
}

impl CommandPreviewController {
    pub fn new(
        program: PathBuf,
        arguments: Vec<String>,
        working_directory: Option<PathBuf>,
        events: Sender<PreviewEvent>,
    ) -> Self {
        Self {
            program: Arc::new(program),
            arguments: arguments.into(),
            working_directory: working_directory.map(Arc::new),
            generation: Arc::new(AtomicU64::new(0)),
            events,
        }
    }

    pub fn request(&self, target: CommandPreviewTarget) -> u64 {
        let generation = self.next_generation();
        let current_generation = Arc::clone(&self.generation);
        let program = Arc::clone(&self.program);
        let arguments = Arc::clone(&self.arguments);
        let working_directory = self.working_directory.clone();
        let events = self.events.clone();
        std::thread::spawn(move || {
            std::thread::sleep(DEBOUNCE);
            if current_generation.load(Ordering::Acquire) != generation {
                return;
            }
            run_process(
                &program,
                &arguments,
                working_directory.as_deref(),
                &target,
                generation,
                current_generation,
                events,
            );
        });
        generation
    }

    pub fn cancel(&self) -> u64 {
        self.next_generation()
    }

    fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn publish_clear(&self, generation: u64) {
        let _ = self
            .events
            .send(PreviewEvent::Command(PreviewUpdate::Clear { generation }));
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandPreviewTarget {
    pub item: String,
    pub center_line: Option<usize>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum PreviewConfig {
    #[default]
    None,
    Command {
        program: PathBuf,
        arguments: Vec<String>,
        working_directory: Option<PathBuf>,
    },
    NativeWindow,
}

pub struct PreviewService {
    coordinator: PreviewCoordinator,
    events: Receiver<PreviewEvent>,
}

impl PreviewService {
    pub fn new(config: PreviewConfig) -> Self {
        let (events_tx, events) = unbounded();
        let backend = build_preview_backend(config, events_tx);
        Self {
            coordinator: PreviewCoordinator::new(backend),
            events,
        }
    }

    pub fn into_parts(self) -> (PreviewCoordinator, Receiver<PreviewEvent>) {
        (self.coordinator, self.events)
    }
}

impl Default for PreviewService {
    fn default() -> Self {
        Self::new(PreviewConfig::None)
    }
}

trait PreviewBackend: Send + Sync {
    fn selected_result_changed(
        &self,
        result: Option<&SearchResult>,
        source: Option<&AnyItemSource>,
    );
    fn clear(&self);
}

pub struct PreviewCoordinator {
    backend: Box<dyn PreviewBackend>,
}

impl PreviewCoordinator {
    fn new(backend: Box<dyn PreviewBackend>) -> Self {
        Self { backend }
    }

    pub fn selected_result_changed(
        &self,
        result: Option<&SearchResult>,
        source: Option<&AnyItemSource>,
    ) {
        self.backend.selected_result_changed(result, source);
    }

    pub fn clear(&self) {
        self.backend.clear();
    }
}

fn build_preview_backend(
    config: PreviewConfig,
    events: Sender<PreviewEvent>,
) -> Box<dyn PreviewBackend> {
    match config {
        PreviewConfig::None => Box::new(NoPreviewBackend),
        PreviewConfig::Command {
            program,
            arguments,
            working_directory,
        } => Box::new(CommandPreviewBackend {
            controller: CommandPreviewController::new(
                program,
                arguments,
                working_directory,
                events,
            ),
            selected: Mutex::new(None),
        }),
        PreviewConfig::NativeWindow => Box::new(NativeWindowPreviewBackend {
            events,
            selected: Mutex::new(None),
        }),
    }
}

struct NoPreviewBackend;

impl PreviewBackend for NoPreviewBackend {
    fn selected_result_changed(
        &self,
        _result: Option<&SearchResult>,
        _source: Option<&AnyItemSource>,
    ) {
    }

    fn clear(&self) {}
}

struct CommandPreviewBackend {
    controller: CommandPreviewController,
    selected: Mutex<Option<CommandPreviewTarget>>,
}

impl PreviewBackend for CommandPreviewBackend {
    fn selected_result_changed(
        &self,
        result: Option<&SearchResult>,
        source: Option<&AnyItemSource>,
    ) {
        let target = result.map(|result| {
            source
                .and_then(|source| source.delimited_metadata(result.node_index))
                .map_or_else(
                    || CommandPreviewTarget {
                        item: result.path.clone(),
                        center_line: None,
                    },
                    |metadata| CommandPreviewTarget {
                        item: metadata.preview_item.unwrap_or(metadata.value),
                        center_line: metadata.preview_center_line,
                    },
                )
        });
        self.set_target(target);
    }

    fn clear(&self) {
        self.set_target(None);
    }
}

impl CommandPreviewBackend {
    fn set_target(&self, target: Option<CommandPreviewTarget>) {
        let mut selected = self.selected.lock().expect("preview backend poisoned");
        if *selected == target {
            return;
        }
        *selected = target.clone();
        drop(selected);

        let generation = match target {
            Some(target) => self.controller.request(target),
            None => self.controller.cancel(),
        };
        self.controller.publish_clear(generation);
    }
}

struct NativeWindowPreviewBackend {
    events: Sender<PreviewEvent>,
    selected: Mutex<Option<NativeWindowId>>,
}

impl PreviewBackend for NativeWindowPreviewBackend {
    fn selected_result_changed(
        &self,
        result: Option<&SearchResult>,
        source: Option<&AnyItemSource>,
    ) {
        let window = result
            .and_then(|result| source?.native_window(result.node_index))
            .map(NativeWindowId);
        self.set_window(window);
    }

    fn clear(&self) {
        self.set_window(None);
    }
}

impl NativeWindowPreviewBackend {
    fn set_window(&self, window: Option<NativeWindowId>) {
        let mut selected = self.selected.lock().expect("preview backend poisoned");
        if *selected == window {
            return;
        }
        *selected = window;
        drop(selected);
        let _ = self.events.send(PreviewEvent::NativeWindow(window));
    }
}

fn run_process(
    program: &PathBuf,
    arguments: &[String],
    working_directory: Option<&PathBuf>,
    target: &CommandPreviewTarget,
    generation: u64,
    current_generation: Arc<AtomicU64>,
    events: Sender<PreviewEvent>,
) {
    let mut command = Command::new(program);
    command.args(arguments);
    if let Some(working_directory) = working_directory {
        command.current_dir(working_directory);
    }
    command
        .env("NFM_PREVIEW_ITEM", &target.item)
        .env(
            "NFM_PREVIEW_LINE",
            target
                .center_line
                .map(|line| line.to_string())
                .unwrap_or_default(),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = events.send(PreviewEvent::Command(PreviewUpdate::Error {
                generation,
                message: format!("preview failed to start: {error}"),
            }));
            return;
        }
    };

    let (output_tx, output_rx) = bounded(OUTPUT_CHANNEL_CAPACITY);
    if let Some(stdout) = child.stdout.take() {
        forward_output(stdout, PreviewStream::Stdout, output_tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        forward_output(stderr, PreviewStream::Stderr, output_tx.clone());
    }
    drop(output_tx);

    let mut document = PreviewDocument::default();
    let mut bytes_received = 0;
    let mut truncated = false;
    let mut child_finished = false;
    let mut readers_finished = false;
    loop {
        if current_generation.load(Ordering::Acquire) != generation {
            let _ = child.kill();
            let _ = child.wait();
            return;
        }

        if !readers_finished {
            match output_rx.recv_timeout(Duration::from_millis(20)) {
                Ok(chunk) => {
                    let remaining = OUTPUT_LIMIT.saturating_sub(bytes_received);
                    let allowed = remaining.min(chunk.bytes.len());
                    if allowed > 0 {
                        document.push(chunk.stream, &chunk.bytes[..allowed]);
                        bytes_received += allowed;
                    }
                    if allowed < chunk.bytes.len() || document.line_limit_reached() {
                        truncated = true;
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => readers_finished = true,
            }
        }

        if !child_finished {
            match child.try_wait() {
                Ok(Some(_)) => child_finished = true,
                Ok(None) => {}
                Err(error) => {
                    let _ = events.send(PreviewEvent::Command(PreviewUpdate::Error {
                        generation,
                        message: format!("preview process error: {error}"),
                    }));
                    return;
                }
            }
        }

        if child_finished && readers_finished {
            break;
        }
    }

    let _ = events.send(PreviewEvent::Command(PreviewUpdate::Ready {
        generation,
        lines: document.into_lines().into(),
        truncated,
        center_line: target.center_line,
    }));
}

fn forward_output(
    reader: impl std::io::Read + Send + 'static,
    stream: PreviewStream,
    output: Sender<OutputChunk>,
) {
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if output
                        .send(OutputChunk {
                            stream,
                            bytes: buffer[..read].to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_publishes_one_completed_document() {
        let (events, receiver) = bounded(4);
        let generation = 7;
        let current_generation = Arc::new(AtomicU64::new(generation));
        #[cfg(windows)]
        let (program, arguments) = (
            PathBuf::from("powershell.exe"),
            vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "Write-Output 'first'; Write-Output 'second'; Write-Output $env:NFM_PREVIEW_LINE"
                    .into(),
            ],
        );
        #[cfg(not(windows))]
        let (program, arguments) = (
            PathBuf::from("/bin/sh"),
            vec![
                "-c".into(),
                "printf 'first\\nsecond\\n%s\\n' \"$NFM_PREVIEW_LINE\"".into(),
            ],
        );

        run_process(
            &program,
            &arguments,
            None,
            &CommandPreviewTarget {
                item: String::new(),
                center_line: Some(42),
            },
            generation,
            current_generation,
            events,
        );

        let PreviewEvent::Command(update) = receiver.recv().expect("preview update") else {
            panic!("expected command preview event");
        };
        let PreviewUpdate::Ready {
            generation: result_generation,
            lines,
            truncated,
            center_line,
        } = update
        else {
            panic!("expected completed preview");
        };
        assert_eq!(result_generation, generation);
        assert!(!truncated);
        assert_eq!(center_line, Some(42));
        let text: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
            .collect();
        assert_eq!(&text[..3], ["first", "second", "42"]);
    }

    #[test]
    fn native_window_backend_deduplicates_and_clears_selection() {
        let (events, receiver) = bounded(4);
        let backend = NativeWindowPreviewBackend {
            events,
            selected: Mutex::new(None),
        };

        backend.set_window(Some(NativeWindowId(42)));
        backend.set_window(Some(NativeWindowId(42)));
        backend.clear();

        assert!(matches!(
            receiver.recv().unwrap(),
            PreviewEvent::NativeWindow(Some(NativeWindowId(42)))
        ));
        assert!(matches!(
            receiver.recv().unwrap(),
            PreviewEvent::NativeWindow(None)
        ));
        assert!(receiver.try_recv().is_err());
    }
}
