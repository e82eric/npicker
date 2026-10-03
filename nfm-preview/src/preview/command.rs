use super::PreviewEvents;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::{
    send_preview, PreviewDocument, PreviewEvent, PreviewStream, PreviewUpdate, SelectionPreview,
};

const OUTPUT_LIMIT: usize = 1024 * 1024;
const IMAGE_OUTPUT_LIMIT: usize = 32 * 1024 * 1024;
const RESOLVER_OUTPUT_LIMIT: usize = 64 * 1024;
const DEBOUNCE: Duration = Duration::from_millis(75);

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
    NativeFile {
        bat_theme: Option<String>,
    },
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

pub(super) struct CommandPreviewBackend<I> {
    controller: CommandPreviewController,
    selected: Mutex<Option<CommandPreviewTarget>>,
    target: Option<Arc<dyn Fn(&I) -> Option<CommandPreviewTarget> + Send + Sync>>,
}

impl<I> CommandPreviewBackend<I> {
    pub(super) fn new(
        resolver: PreviewResolver,
        events: PreviewEvents,
        generation: Arc<AtomicU64>,
        target: Option<Arc<dyn Fn(&I) -> Option<CommandPreviewTarget> + Send + Sync>>,
    ) -> Self {
        Self {
            controller: CommandPreviewController::with_generation(resolver, events, generation),
            selected: Mutex::new(None),
            target,
        }
    }

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

impl<I> SelectionPreview<I> for CommandPreviewBackend<I> {
    fn selection_changed(&self, item: Option<&I>) {
        let target = self
            .target
            .as_ref()
            .and_then(|target| item.and_then(|item| target(item)));
        self.set_target(target);
    }

    fn clear(&self) {
        self.set_target(None);
    }
}

pub(super) struct CommandPreviewController {
    resolver: Arc<PreviewResolver>,
    generation: Arc<AtomicU64>,
    events: PreviewEvents,
}

impl CommandPreviewController {
    pub(super) fn with_generation(
        resolver: PreviewResolver,
        events: PreviewEvents,
        generation: Arc<AtomicU64>,
    ) -> Self {
        Self {
            resolver: Arc::new(resolver),
            generation,
            events,
        }
    }

    pub(super) fn request(&self, target: CommandPreviewTarget) -> u64 {
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
                PreviewResolver::NativeFile { bat_theme } => {
                    match super::native_file::resolve_native_file_preview_with_theme(
                        &target,
                        bat_theme.as_deref(),
                    ) {
                        Ok(job) => job,
                        Err(message) => {
                            publish_error(&events, generation, message);
                            return;
                        }
                    }
                }
            };
            let Some(job) = job else {
                return;
            };
            execute_preview_job(job, &target, generation, current_generation, events);
        });
        generation
    }

    pub(super) fn cancel(&self) -> u64 {
        self.next_generation()
    }

    fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }

    pub(super) fn publish_clear(&self, generation: u64) {
        send_preview(
            &self.events,
            PreviewEvent::Command(PreviewUpdate::Clear { generation }),
        );
    }
}

fn execute_preview_job(
    job: PreviewJob,
    target: &CommandPreviewTarget,
    generation: u64,
    current_generation: Arc<AtomicU64>,
    events: PreviewEvents,
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

pub(super) fn run_resolver(
    program: &PathBuf,
    arguments: &[String],
    target: &CommandPreviewTarget,
    generation: u64,
    current_generation: Arc<AtomicU64>,
) -> Result<Option<String>, String> {
    let mut spec = nfm_preview_command::CommandSpec::new(program.clone());
    spec.arguments = arguments
        .iter()
        .map(|arg| expand_preview_argument(arg, target))
        .collect();
    spec.environment = vec![
        ("NFM_PREVIEW_ITEM".into(), target.item.clone().into()),
        (
            "NFM_PREVIEW_LINE".into(),
            target
                .center_line
                .map(|l| l.to_string())
                .unwrap_or_default()
                .into(),
        ),
    ];
    spec.output_limit = RESOLVER_OUTPUT_LIMIT;
    let started = Instant::now();
    let mut received: usize = 0;
    let result = nfm_preview_command::run(
        &spec,
        || {
            current_generation.load(Ordering::Acquire) != generation
                || started.elapsed() >= Duration::from_secs(2)
        },
        |_, bytes| {
            received += bytes.len();
            received <= RESOLVER_OUTPUT_LIMIT
        },
    );
    if current_generation.load(Ordering::Acquire) != generation {
        return Ok(None);
    }
    if started.elapsed() >= Duration::from_secs(2) {
        return Err("preview resolver timed out".into());
    }
    let result = result?;
    if result.truncated {
        return Err("preview resolver output exceeded 64 KiB".into());
    }
    let status = result.status;
    let stdout = result.stdout;
    let stderr = result.stderr;

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

fn publish_error(events: &PreviewEvents, generation: u64, message: String) {
    send_preview(
        events,
        PreviewEvent::Command(PreviewUpdate::Error {
            generation,
            message,
        }),
    );
}

pub(super) fn run_process(
    program: &PathBuf,
    arguments: &[String],
    working_directory: Option<&PathBuf>,
    output_type: PreviewOutputType,
    target: &CommandPreviewTarget,
    generation: u64,
    current_generation: Arc<AtomicU64>,
    events: PreviewEvents,
) {
    let mut spec = nfm_preview_command::CommandSpec::new(program.clone());
    spec.arguments = arguments
        .iter()
        .map(|arg| expand_preview_argument(arg, target))
        .collect();
    spec.working_directory = working_directory.cloned();
    spec.environment = vec![
        ("NFM_PREVIEW_ITEM".into(), target.item.clone().into()),
        (
            "NFM_PREVIEW_LINE".into(),
            target
                .center_line
                .map(|l| l.to_string())
                .unwrap_or_default()
                .into(),
        ),
    ];
    spec.output_limit = match output_type {
        PreviewOutputType::Text => OUTPUT_LIMIT,
        PreviewOutputType::Image => IMAGE_OUTPUT_LIMIT,
    };
    let mut document = PreviewDocument::default();
    let mut image_bytes = Vec::new();
    let mut image_error = PreviewDocument::default();
    let mut bytes_received: usize = 0;
    let result = nfm_preview_command::run(
        &spec,
        || current_generation.load(Ordering::Acquire) != generation,
        |stream, bytes| {
            let stream = match stream {
                nfm_preview_command::Stream::Stdout => PreviewStream::Stdout,
                nfm_preview_command::Stream::Stderr => PreviewStream::Stderr,
            };
            let count = bytes
                .len()
                .min(spec.output_limit.saturating_sub(bytes_received));
            bytes_received += count;
            match output_type {
                PreviewOutputType::Text => document.push(stream, &bytes[..count]),
                PreviewOutputType::Image => match stream {
                    PreviewStream::Stdout => image_bytes.extend_from_slice(&bytes[..count]),
                    PreviewStream::Stderr => image_error.push(stream, &bytes[..count]),
                },
            }
            count == bytes.len()
                && !(output_type == PreviewOutputType::Text && document.line_limit_reached())
        },
    );
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            if current_generation.load(Ordering::Acquire) == generation {
                send_preview(
                    &events,
                    PreviewEvent::Command(PreviewUpdate::Error {
                        generation,
                        message: error,
                    }),
                );
            }
            return;
        }
    };
    let truncated = result.truncated;
    let child_succeeded = result.status.success();

    let update = match output_type {
        PreviewOutputType::Text => match document.into_lines() {
            Ok(lines) => PreviewUpdate::Ready {
                generation,
                lines: lines.into(),
                truncated,
                center_line: target.center_line,
            },
            Err(message) => PreviewUpdate::Error {
                generation,
                message,
            },
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

pub(super) fn expand_preview_argument(argument: &str, target: &CommandPreviewTarget) -> String {
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
        .ok()?
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
