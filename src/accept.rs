use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, RecvTimeoutError, Sender};
use serde::Deserialize;

use crate::view_model::ViewModelEvent;

const RESOLVER_OUTPUT_LIMIT: usize = 64 * 1024;
const RESOLVER_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum AcceptConfig {
    #[default]
    Complete,
    Resolver {
        program: PathBuf,
        arguments: Vec<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptTarget {
    pub item: String,
    pub value: String,
    pub center_line: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AcceptResolution {
    Complete,
    FileWalker { roots: Vec<String> },
}

#[derive(Clone, Debug)]
pub struct AcceptEvent {
    pub generation: u64,
    pub target: AcceptTarget,
    pub result: Result<AcceptResolution, String>,
}

pub struct AcceptService {
    config: AcceptConfig,
}

impl AcceptService {
    pub fn new(config: AcceptConfig) -> Self {
        Self { config }
    }

    pub(crate) fn into_controller(self, events: Sender<ViewModelEvent>) -> AcceptController {
        AcceptController::new(self.config, events)
    }
}

impl Default for AcceptService {
    fn default() -> Self {
        Self::new(AcceptConfig::Complete)
    }
}

enum AcceptMode {
    Complete,
    Resolver {
        program: PathBuf,
        arguments: Vec<String>,
    },
}

pub struct AcceptController {
    mode: Arc<AcceptMode>,
    generation: Arc<AtomicU64>,
    events: Sender<ViewModelEvent>,
}

impl AcceptController {
    fn new(config: AcceptConfig, events: Sender<ViewModelEvent>) -> Self {
        let mode = match config {
            AcceptConfig::Complete => AcceptMode::Complete,
            AcceptConfig::Resolver { program, arguments } => {
                AcceptMode::Resolver { program, arguments }
            }
        };
        Self {
            mode: Arc::new(mode),
            generation: Arc::new(AtomicU64::new(0)),
            events,
        }
    }

    pub fn resolves_accept(&self) -> bool {
        matches!(self.mode.as_ref(), AcceptMode::Resolver { .. })
    }

    pub fn reserve(&self) -> u64 {
        self.next_generation()
    }

    pub fn request(&self, generation: u64, target: AcceptTarget) {
        let mode = Arc::clone(&self.mode);
        let current_generation = Arc::clone(&self.generation);
        let events = self.events.clone();
        std::thread::spawn(move || {
            let result = match mode.as_ref() {
                AcceptMode::Complete => Ok(AcceptResolution::Complete),
                AcceptMode::Resolver { program, arguments } => run_resolver(
                    program,
                    arguments,
                    &target,
                    generation,
                    Arc::clone(&current_generation),
                ),
            };
            if current_generation.load(Ordering::Acquire) == generation {
                let _ = events.send(ViewModelEvent::Accept(AcceptEvent {
                    generation,
                    target,
                    result,
                }));
            }
        });
    }

    pub fn cancel(&self) {
        self.next_generation();
    }

    fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
enum ResolverResponse {
    Complete,
    Picker { picker: PickerResponse },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum PickerResponse {
    Filewalker { roots: Vec<String> },
}

fn run_resolver(
    program: &PathBuf,
    arguments: &[String],
    target: &AcceptTarget,
    generation: u64,
    current_generation: Arc<AtomicU64>,
) -> Result<AcceptResolution, String> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .env("NFM_ACCEPT_ITEM", &target.item)
        .env("NFM_ACCEPT_VALUE", &target.value)
        .env(
            "NFM_ACCEPT_LINE",
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
        .map_err(|error| format!("accept resolver failed to start: {error}"))?;
    let (output_tx, output_rx) = bounded(32);
    if let Some(stdout) = child.stdout.take() {
        forward_output(stdout, false, output_tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        forward_output(stderr, true, output_tx.clone());
    }
    drop(output_tx);

    let started = Instant::now();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let status = loop {
        if current_generation.load(Ordering::Acquire) != generation {
            let _ = child.kill();
            let _ = child.wait();
            return Err("accept resolver cancelled".into());
        }
        if started.elapsed() >= RESOLVER_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err("accept resolver timed out".into());
        }
        match output_rx.recv_timeout(Duration::from_millis(20)) {
            Ok(chunk) => {
                if let Err(error) = append_output(chunk, &mut stdout, &mut stderr) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => return Err(format!("accept resolver process error: {error}")),
        }
    };
    while let Ok(chunk) = output_rx.recv_timeout(Duration::from_millis(100)) {
        append_output(chunk, &mut stdout, &mut stderr)?;
    }

    if !status.success() {
        let message = String::from_utf8_lossy(&stderr).trim().to_owned();
        return Err(if message.is_empty() {
            format!("accept resolver exited with {status}")
        } else {
            message
        });
    }
    let response: ResolverResponse = serde_json::from_slice(&stdout)
        .map_err(|error| format!("invalid accept resolver response: {error}"))?;
    match response {
        ResolverResponse::Complete => Ok(AcceptResolution::Complete),
        ResolverResponse::Picker {
            picker: PickerResponse::Filewalker { roots },
        } if roots.is_empty() => Err("filewalker picker requires at least one root".into()),
        ResolverResponse::Picker {
            picker: PickerResponse::Filewalker { roots },
        } => Ok(AcceptResolution::FileWalker {
            roots: roots
                .into_iter()
                .map(|root| {
                    if root == "{item}" {
                        target.item.clone()
                    } else {
                        root
                    }
                })
                .collect(),
        }),
    }
}

struct OutputChunk {
    stderr: bool,
    bytes: Vec<u8>,
}

fn forward_output(
    mut reader: impl Read + Send + 'static,
    stderr: bool,
    output: Sender<OutputChunk>,
) {
    std::thread::spawn(move || {
        let mut buffer = [0; 4096];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    if output
                        .send(OutputChunk {
                            stderr,
                            bytes: buffer[..count].to_vec(),
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

fn append_output(
    chunk: OutputChunk,
    stdout: &mut Vec<u8>,
    stderr: &mut Vec<u8>,
) -> Result<(), String> {
    if stdout.len() + stderr.len() + chunk.bytes.len() > RESOLVER_OUTPUT_LIMIT {
        return Err("accept resolver output exceeded 64 KiB".into());
    }
    if chunk.stderr {
        stderr.extend_from_slice(&chunk.bytes);
    } else {
        stdout.extend_from_slice(&chunk.bytes);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_response(json: &str, target: &AcceptTarget) -> Result<AcceptResolution, String> {
        let response: ResolverResponse =
            serde_json::from_str(json).map_err(|error| error.to_string())?;
        match response {
            ResolverResponse::Complete => Ok(AcceptResolution::Complete),
            ResolverResponse::Picker {
                picker: PickerResponse::Filewalker { roots },
            } => Ok(AcceptResolution::FileWalker {
                roots: roots
                    .into_iter()
                    .map(|root| {
                        if root == "{item}" {
                            target.item.clone()
                        } else {
                            root
                        }
                    })
                    .collect(),
            }),
        }
    }

    #[test]
    fn parses_complete() {
        let target = AcceptTarget {
            item: "C:\\src".into(),
            value: "value".into(),
            center_line: None,
        };
        assert_eq!(
            parse_response(r#"{"action":"complete"}"#, &target).unwrap(),
            AcceptResolution::Complete
        );
    }

    #[test]
    fn expands_only_a_whole_item_placeholder_in_filewalker_roots() {
        let target = AcceptTarget {
            item: "C:\\src".into(),
            value: "value".into(),
            center_line: None,
        };
        assert_eq!(
            parse_response(
                r#"{"action":"picker","picker":{"kind":"filewalker","roots":["{item}","{item}\\child"]}}"#,
                &target,
            )
            .unwrap(),
            AcceptResolution::FileWalker {
                roots: vec!["C:\\src".into(), "{item}\\child".into()]
            }
        );
    }
}
