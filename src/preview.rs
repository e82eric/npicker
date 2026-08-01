use std::collections::HashMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, RecvTimeoutError, Sender};
use nfm_search_core::search::SearchResult;

use crate::preview_document::PreviewDocument;
pub use crate::preview_document::PreviewLine;
use crate::source_store::AnyItemSource;
use crate::view_model::ViewModelEvent;

const OUTPUT_LIMIT: usize = 1024 * 1024;
const IMAGE_OUTPUT_LIMIT: usize = 32 * 1024 * 1024;
const RESOLVER_OUTPUT_LIMIT: usize = 64 * 1024;
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
    ImageReady {
        generation: u64,
        encoded: Arc<[u8]>,
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
    resolver: Arc<PreviewResolver>,
    generation: Arc<AtomicU64>,
    events: Sender<ViewModelEvent>,
}

impl CommandPreviewController {
    fn new(resolver: PreviewResolver, events: Sender<ViewModelEvent>) -> Self {
        Self {
            resolver: Arc::new(resolver),
            generation: Arc::new(AtomicU64::new(0)),
            events,
        }
    }

    pub fn request(&self, target: CommandPreviewTarget) -> u64 {
        let generation = self.next_generation();
        let current_generation = Arc::clone(&self.generation);
        let resolver = Arc::clone(&self.resolver);
        let events = self.events.clone();
        std::thread::spawn(move || {
            std::thread::sleep(DEBOUNCE);
            if current_generation.load(Ordering::Acquire) != generation {
                return;
            }
            let job = match resolver.as_ref() {
                PreviewResolver::Fixed(profile) => Some(PreviewJob::Process(profile.clone())),
                PreviewResolver::Process {
                    program,
                    arguments,
                    profiles,
                    default_profile,
                } => match run_resolver(
                    program,
                    arguments,
                    &target,
                    generation,
                    Arc::clone(&current_generation),
                ) {
                    Ok(Some(profile)) => match profiles.get(&profile) {
                        Some(job) => Some(PreviewJob::Process(job.clone())),
                        None => {
                            publish_error(
                                &events,
                                generation,
                                format!("preview resolver returned unknown profile: {profile}"),
                            );
                            return;
                        }
                    },
                    Ok(None) => match default_profile {
                        Some(profile) => match profiles.get(profile) {
                            Some(job) => Some(PreviewJob::Process(job.clone())),
                            None => {
                                publish_error(
                                    &events,
                                    generation,
                                    format!("unknown default preview profile: {profile}"),
                                );
                                return;
                            }
                        },
                        None => None,
                    },
                    Err(message) => {
                        publish_error(&events, generation, message);
                        return;
                    }
                },
                PreviewResolver::Function(resolve) => match resolve(&target) {
                    Ok(job) => job,
                    Err(message) => {
                        publish_error(&events, generation, message);
                        return;
                    }
                },
            };
            let Some(job) = job else {
                return;
            };
            execute_preview_job(job, &target, generation, current_generation, events);
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
        send_preview(
            &self.events,
            PreviewEvent::Command(PreviewUpdate::Clear { generation }),
        );
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewProfile {
    pub program: PathBuf,
    pub arguments: Vec<String>,
    pub working_directory: Option<PathBuf>,
    pub output_type: PreviewOutputType,
}

#[derive(Clone, Debug)]
pub enum PreviewResolver {
    Fixed(PreviewProfile),
    Process {
        program: PathBuf,
        arguments: Vec<String>,
        profiles: HashMap<String, PreviewProfile>,
        default_profile: Option<String>,
    },
    Function(NativePreviewResolver),
}

pub type NativePreviewResolver = fn(&CommandPreviewTarget) -> Result<Option<PreviewJob>, String>;
pub type PreviewFunction =
    fn(&CommandPreviewTarget, &PreviewCancellation) -> Result<Option<PreviewProfile>, String>;

#[derive(Clone)]
pub struct PreviewCancellation {
    generation: u64,
    current_generation: Arc<AtomicU64>,
}

impl PreviewCancellation {
    pub fn is_cancelled(&self) -> bool {
        self.current_generation.load(Ordering::Acquire) != self.generation
    }
}

impl std::fmt::Debug for PreviewCancellation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreviewCancellation")
            .field("generation", &self.generation)
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

#[derive(Clone, Debug)]
pub enum PreviewJob {
    Process(PreviewProfile),
    Function(PreviewFunction),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandPreviewTarget {
    pub item: String,
    pub center_line: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PreviewOutputType {
    #[default]
    Text,
    Image,
}

#[derive(Clone, Debug, Default)]
pub enum PreviewConfig {
    #[default]
    None,
    Command(PreviewResolver),
    NativeWindow,
}

pub struct PreviewService {
    config: PreviewConfig,
}

impl PreviewService {
    pub fn new(config: PreviewConfig) -> Self {
        Self { config }
    }

    pub(crate) fn into_coordinator(self, events: Sender<ViewModelEvent>) -> PreviewCoordinator {
        PreviewCoordinator::new(build_preview_backend(self.config, events))
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
    events: Sender<ViewModelEvent>,
) -> Box<dyn PreviewBackend> {
    match config {
        PreviewConfig::None => Box::new(NoPreviewBackend),
        PreviewConfig::Command(resolver) => Box::new(CommandPreviewBackend {
            controller: CommandPreviewController::new(resolver, events),
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
    events: Sender<ViewModelEvent>,
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
        send_preview(&self.events, PreviewEvent::NativeWindow(window));
    }
}

fn execute_preview_job(
    job: PreviewJob,
    target: &CommandPreviewTarget,
    generation: u64,
    current_generation: Arc<AtomicU64>,
    events: Sender<ViewModelEvent>,
) {
    if current_generation.load(Ordering::Acquire) != generation {
        return;
    }
    let profile = match job {
        PreviewJob::Process(profile) => profile,
        PreviewJob::Function(function) => {
            let cancellation = PreviewCancellation {
                generation,
                current_generation: Arc::clone(&current_generation),
            };
            match function(target, &cancellation) {
                Ok(Some(profile)) => profile,
                Ok(None) => return,
                Err(message) => {
                    publish_error(&events, generation, message);
                    return;
                }
            }
        }
    };
    if current_generation.load(Ordering::Acquire) != generation {
        return;
    }
    run_process(
        &profile.program,
        &profile.arguments,
        profile.working_directory.as_ref(),
        profile.output_type,
        target,
        generation,
        current_generation,
        events,
    );
}

fn run_resolver(
    program: &PathBuf,
    arguments: &[String],
    target: &CommandPreviewTarget,
    generation: u64,
    current_generation: Arc<AtomicU64>,
) -> Result<Option<String>, String> {
    let mut command = Command::new(program);
    command.args(
        arguments
            .iter()
            .map(|argument| expand_preview_argument(argument, target)),
    );
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

    let mut child = command
        .spawn()
        .map_err(|error| format!("preview resolver failed to start: {error}"))?;
    let (output_tx, output_rx) = bounded(OUTPUT_CHANNEL_CAPACITY);
    if let Some(stdout) = child.stdout.take() {
        forward_output(stdout, PreviewStream::Stdout, output_tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        forward_output(stderr, PreviewStream::Stderr, output_tx.clone());
    }
    drop(output_tx);

    let started = Instant::now();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let status = loop {
        if current_generation.load(Ordering::Acquire) != generation {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        if started.elapsed() >= Duration::from_secs(2) {
            let _ = child.kill();
            let _ = child.wait();
            return Err("preview resolver timed out".into());
        }
        match output_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(chunk) => {
                if stdout.len() + stderr.len() + chunk.bytes.len() > RESOLVER_OUTPUT_LIMIT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("preview resolver output exceeded 64 KiB".into());
                }
                let destination = match chunk.stream {
                    PreviewStream::Stdout => &mut stdout,
                    PreviewStream::Stderr => &mut stderr,
                };
                destination.extend_from_slice(&chunk.bytes);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {}
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => return Err(format!("preview resolver process error: {error}")),
        }
    };
    while let Ok(chunk) = output_rx.recv_timeout(Duration::from_millis(100)) {
        if stdout.len() + stderr.len() + chunk.bytes.len() > RESOLVER_OUTPUT_LIMIT {
            return Err("preview resolver output exceeded 64 KiB".into());
        }
        match chunk.stream {
            PreviewStream::Stdout => stdout.extend_from_slice(&chunk.bytes),
            PreviewStream::Stderr => stderr.extend_from_slice(&chunk.bytes),
        }
    }

    if !status.success() {
        let message = String::from_utf8_lossy(&stderr).trim().to_owned();
        return Err(if message.is_empty() {
            format!("preview resolver exited with {status}")
        } else {
            message
        });
    }
    let output = String::from_utf8(stdout)
        .map_err(|_| "preview resolver output was not valid UTF-8".to_string())?;
    let mut lines = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty());
    let profile = lines.next().map(str::to_owned);
    if lines.next().is_some() {
        return Err("preview resolver returned more than one profile".into());
    }
    Ok(profile)
}

fn send_preview(events: &Sender<ViewModelEvent>, event: PreviewEvent) {
    let _ = events.send(ViewModelEvent::Preview(event));
}

fn publish_error(events: &Sender<ViewModelEvent>, generation: u64, message: String) {
    send_preview(
        events,
        PreviewEvent::Command(PreviewUpdate::Error {
            generation,
            message,
        }),
    );
}

fn run_process(
    program: &PathBuf,
    arguments: &[String],
    working_directory: Option<&PathBuf>,
    output_type: PreviewOutputType,
    target: &CommandPreviewTarget,
    generation: u64,
    current_generation: Arc<AtomicU64>,
    events: Sender<ViewModelEvent>,
) {
    let mut command = Command::new(program);
    command.args(
        arguments
            .iter()
            .map(|argument| expand_preview_argument(argument, target)),
    );
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
            send_preview(
                &events,
                PreviewEvent::Command(PreviewUpdate::Error {
                    generation,
                    message: format!("preview failed to start: {error}"),
                }),
            );
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
    let mut image_bytes = Vec::new();
    let mut image_error = PreviewDocument::default();
    let mut bytes_received = 0;
    let mut truncated = false;
    let mut child_finished = false;
    let mut child_succeeded = true;
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
                    let output_limit = match output_type {
                        PreviewOutputType::Text => OUTPUT_LIMIT,
                        PreviewOutputType::Image => IMAGE_OUTPUT_LIMIT,
                    };
                    let remaining = output_limit.saturating_sub(bytes_received);
                    let allowed = remaining.min(chunk.bytes.len());
                    if allowed > 0 {
                        match output_type {
                            PreviewOutputType::Text => {
                                document.push(chunk.stream, &chunk.bytes[..allowed]);
                            }
                            PreviewOutputType::Image => match chunk.stream {
                                PreviewStream::Stdout => {
                                    image_bytes.extend_from_slice(&chunk.bytes[..allowed]);
                                }
                                PreviewStream::Stderr => {
                                    image_error.push(chunk.stream, &chunk.bytes[..allowed]);
                                }
                            },
                        }
                        bytes_received += allowed;
                    }
                    if allowed < chunk.bytes.len()
                        || (output_type == PreviewOutputType::Text && document.line_limit_reached())
                    {
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
                Ok(Some(status)) => {
                    child_succeeded = status.success();
                    child_finished = true;
                }
                Ok(None) => {}
                Err(error) => {
                    send_preview(
                        &events,
                        PreviewEvent::Command(PreviewUpdate::Error {
                            generation,
                            message: format!("preview process error: {error}"),
                        }),
                    );
                    return;
                }
            }
        }

        if child_finished && readers_finished {
            break;
        }
    }

    let update = match output_type {
        PreviewOutputType::Text => PreviewUpdate::Ready {
            generation,
            lines: document.into_lines().into(),
            truncated,
            center_line: target.center_line,
        },
        PreviewOutputType::Image if truncated => PreviewUpdate::Error {
            generation,
            message: "image preview exceeded 32 MiB".into(),
        },
        PreviewOutputType::Image if image_bytes.is_empty() || !child_succeeded => {
            let message = preview_document_text(image_error)
                .filter(|message| !message.is_empty())
                .unwrap_or_else(|| {
                    if image_bytes.is_empty() {
                        "image preview produced no output".into()
                    } else {
                        "image preview process failed".into()
                    }
                });
            PreviewUpdate::Error {
                generation,
                message,
            }
        }
        PreviewOutputType::Image => PreviewUpdate::ImageReady {
            generation,
            encoded: image_bytes.into(),
        },
    };
    send_preview(&events, PreviewEvent::Command(update));
}

fn expand_preview_argument(argument: &str, target: &CommandPreviewTarget) -> String {
    argument.replace("{item}", &target.item).replace(
        "{line}",
        &target
            .center_line
            .map(|line| line.to_string())
            .unwrap_or_default(),
    )
}

fn preview_document_text(document: PreviewDocument) -> Option<String> {
    let text = document
        .into_lines()
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.text)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
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
            PreviewOutputType::Text,
            &CommandPreviewTarget {
                item: String::new(),
                center_line: Some(42),
            },
            generation,
            current_generation,
            events,
        );

        let ViewModelEvent::Preview(PreviewEvent::Command(update)) =
            receiver.recv().expect("preview update")
        else {
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
    fn preview_arguments_expand_item_and_line_placeholders() {
        let target = CommandPreviewTarget {
            item: r"C:\files\a b.png".into(),
            center_line: Some(17),
        };
        assert_eq!(
            expand_preview_argument("--input={item}", &target),
            r"--input=C:\files\a b.png"
        );
        assert_eq!(expand_preview_argument("{line}", &target), "17");
    }

    #[test]
    fn process_resolver_returns_one_profile_name() {
        #[cfg(windows)]
        let (program, arguments) = (
            PathBuf::from("powershell.exe"),
            vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "Write-Output image".into(),
            ],
        );
        #[cfg(not(windows))]
        let (program, arguments) = (
            PathBuf::from("/bin/sh"),
            vec!["-c".into(), "printf 'image\\n'".into()],
        );

        let profile = run_resolver(
            &program,
            &arguments,
            &CommandPreviewTarget {
                item: "sample.png".into(),
                center_line: None,
            },
            3,
            Arc::new(AtomicU64::new(3)),
        )
        .unwrap();

        assert_eq!(profile.as_deref(), Some("image"));
    }

    #[test]
    fn image_process_publishes_binary_stdout() {
        let (events, receiver) = bounded(4);
        let generation = 9;
        let current_generation = Arc::new(AtomicU64::new(generation));
        #[cfg(windows)]
        let (program, arguments) = (
            PathBuf::from("powershell.exe"),
            vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                "[Console]::OpenStandardOutput().Write([byte[]](1,2,3), 0, 3)".into(),
            ],
        );
        #[cfg(not(windows))]
        let (program, arguments) = (
            PathBuf::from("/bin/sh"),
            vec!["-c".into(), "printf '\\001\\002\\003'".into()],
        );

        run_process(
            &program,
            &arguments,
            None,
            PreviewOutputType::Image,
            &CommandPreviewTarget {
                item: String::new(),
                center_line: None,
            },
            generation,
            current_generation,
            events,
        );

        assert!(matches!(
            receiver.recv().unwrap(),
            ViewModelEvent::Preview(PreviewEvent::Command(PreviewUpdate::ImageReady {
                generation: 9,
                encoded
            })) if encoded.as_ref() == [1, 2, 3]
        ));
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
            ViewModelEvent::Preview(PreviewEvent::NativeWindow(Some(NativeWindowId(42))))
        ));
        assert!(matches!(
            receiver.recv().unwrap(),
            ViewModelEvent::Preview(PreviewEvent::NativeWindow(None))
        ));
        assert!(receiver.try_recv().is_err());
    }
}
