use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, RecvTimeoutError, Sender};
use serde::{Deserialize, Serialize};

use crate::PickerItem;

const OUTPUT_LIMIT: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActionResolverDefinition {
    pub program: PathBuf,
    pub arguments: Vec<String>,
}

pub type NativeActionResolver<I> =
    Arc<dyn Fn(&ActionState<I>) -> Result<ActionResolution, String> + Send + Sync>;

pub enum ActionDefinition<I> {
    Process(ActionResolverDefinition),
    Native(NativeActionResolver<I>),
}

impl<I> Clone for ActionDefinition<I> {
    fn clone(&self) -> Self {
        match self {
            Self::Process(definition) => Self::Process(definition.clone()),
            Self::Native(resolve) => Self::Native(Arc::clone(resolve)),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ActionState<I> {
    pub selection: Option<I>,
    pub picker: PickerState,
    pub query: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PickerState {
    Filewalker { roots: Vec<String> },
    Stdin,
    DelimitedStdin,
    StructuredStdin,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ActionResolution {
    None,
    Complete,
    Picker(PickerState),
}

#[derive(Clone, Debug)]
pub struct ActionEvent<I> {
    pub generation: u64,
    pub state: ActionState<I>,
    pub result: Result<ActionResolution, String>,
}

pub struct ActionController {
    generation: Arc<AtomicU64>,
}

pub struct PreparedAction {
    run: Box<dyn FnOnce(u64, Arc<AtomicU64>) + Send>,
}

impl PreparedAction {
    pub fn new<I, F>(definition: ActionDefinition<I>, state: ActionState<I>, completed: F) -> Self
    where
        I: PickerItem,
        F: FnOnce(ActionEvent<I>) + Send + 'static,
    {
        Self {
            run: Box::new(move |generation, current_generation| {
                std::thread::spawn(move || {
                    let result = match definition {
                        ActionDefinition::Process(definition) => {
                            run_resolver(&definition, &state, generation, &current_generation)
                        }
                        ActionDefinition::Native(resolve) => resolve(&state),
                    };
                    if current_generation.load(Ordering::Acquire) == generation {
                        completed(ActionEvent {
                            generation,
                            state,
                            result,
                        });
                    }
                });
            }),
        }
    }

    pub fn from_task<T, F, C>(task: F, completed: C) -> Self
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, String> + Send + 'static,
        C: FnOnce(u64, Result<T, String>) + Send + 'static,
    {
        Self {
            run: Box::new(move |generation, current_generation| {
                std::thread::spawn(move || {
                    let result = task();
                    if current_generation.load(Ordering::Acquire) == generation {
                        completed(generation, result);
                    }
                });
            }),
        }
    }
}

impl ActionController {
    pub(crate) fn new() -> Self {
        ActionController {
            generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn request(&self, action: PreparedAction) -> u64 {
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        let current_generation = Arc::clone(&self.generation);
        (action.run)(generation, current_generation);
        generation
    }

    pub fn cancel(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
enum ResolverResponse {
    None,
    Complete,
    Picker { picker: PickerResponse },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum PickerResponse {
    Filewalker { roots: Vec<String> },
}

fn run_resolver<I: PickerItem>(
    definition: &ActionResolverDefinition,
    state: &ActionState<I>,
    generation: u64,
    current_generation: &Arc<AtomicU64>,
) -> Result<ActionResolution, String> {
    let state_json = serde_json::to_string(state)
        .map_err(|error| format!("failed to serialize action state: {error}"))?;
    let mut command = Command::new(&definition.program);
    command
        .args(&definition.arguments)
        .env("NFM_ACTION_STATE", state_json)
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
        .map_err(|error| format!("action resolver failed to start: {error}"))?;
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
            return Err("action resolver cancelled".into());
        }
        if started.elapsed() >= TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err("action resolver timed out".into());
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
            Err(error) => return Err(format!("action resolver process error: {error}")),
        }
    };
    while let Ok(chunk) = output_rx.recv_timeout(Duration::from_millis(100)) {
        append_output(chunk, &mut stdout, &mut stderr)?;
    }
    if !status.success() {
        let message = String::from_utf8_lossy(&stderr).trim().to_owned();
        return Err(if message.is_empty() {
            format!("action resolver exited with {status}")
        } else {
            message
        });
    }

    let response: ResolverResponse = serde_json::from_slice(&stdout)
        .map_err(|error| format!("invalid action resolver response: {error}"))?;
    match response {
        ResolverResponse::None => Ok(ActionResolution::None),
        ResolverResponse::Complete => Ok(ActionResolution::Complete),
        ResolverResponse::Picker {
            picker: PickerResponse::Filewalker { roots },
        } if roots.is_empty() => Err("filewalker picker requires at least one root".into()),
        ResolverResponse::Picker {
            picker: PickerResponse::Filewalker { roots },
        } => Ok(ActionResolution::Picker(PickerState::Filewalker {
            roots: roots
                .into_iter()
                .map(|root| {
                    if root == "{item}" {
                        state
                            .selection
                            .as_ref()
                            .map(|item| item.value().to_owned())
                            .unwrap_or(root)
                    } else {
                        root
                    }
                })
                .collect(),
        })),
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
    if stdout.len() + stderr.len() + chunk.bytes.len() > OUTPUT_LIMIT {
        return Err("action resolver output exceeded 64 KiB".into());
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

    #[test]
    fn action_state_serializes_as_one_json_document() {
        let state = ActionState {
            selection: Some(serde_json::json!({
                "item": r"G:\src\file.rs",
                "value": "value",
                "line": 42,
            })),
            picker: PickerState::Filewalker {
                roots: vec![r"G:\src".into()],
            },
            query: "file".into(),
        };
        let json = serde_json::to_value(state).unwrap();
        assert_eq!(json["picker"]["kind"], "filewalker");
        assert_eq!(json["picker"]["roots"][0], r"G:\src");
        assert_eq!(json["selection"]["line"], 42);
    }

    #[test]
    fn parses_complete_resolution() {
        let response: ResolverResponse = serde_json::from_str(r#"{"action":"complete"}"#).unwrap();
        assert!(matches!(response, ResolverResponse::Complete));
    }
}
