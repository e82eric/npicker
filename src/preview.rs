use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{bounded, RecvTimeoutError, Sender};

use crate::preview_document::PreviewDocument;
pub use crate::preview_document::PreviewLine;

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
    },
    Error {
        generation: u64,
        message: String,
    },
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

pub struct PreviewController {
    command: Arc<str>,
    working_directory: Option<Arc<PathBuf>>,
    generation: Arc<AtomicU64>,
    updates: Sender<PreviewUpdate>,
}

impl PreviewController {
    pub fn new(
        command: String,
        working_directory: Option<PathBuf>,
        updates: Sender<PreviewUpdate>,
    ) -> Self {
        Self {
            command: Arc::from(command),
            working_directory: working_directory.map(Arc::new),
            generation: Arc::new(AtomicU64::new(0)),
            updates,
        }
    }

    pub fn request(&self, target: PreviewTarget) -> u64 {
        let generation = self.next_generation();
        let current_generation = Arc::clone(&self.generation);
        let command = Arc::clone(&self.command);
        let working_directory = self.working_directory.clone();
        let updates = self.updates.clone();
        std::thread::spawn(move || {
            std::thread::sleep(DEBOUNCE);
            if current_generation.load(Ordering::Acquire) != generation {
                return;
            }
            run_process(
                &command,
                working_directory.as_deref(),
                &target,
                generation,
                current_generation,
                updates,
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreviewTarget {
    pub item: String,
    pub center_line: Option<usize>,
}

fn run_process(
    preview_command: &str,
    working_directory: Option<&PathBuf>,
    target: &PreviewTarget,
    generation: u64,
    current_generation: Arc<AtomicU64>,
    updates: Sender<PreviewUpdate>,
) {
    let mut command = shell_command(preview_command);
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
            let _ = updates.send(PreviewUpdate::Error {
                generation,
                message: format!("preview failed to start: {error}"),
            });
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
                    let _ = updates.send(PreviewUpdate::Error {
                        generation,
                        message: format!("preview process error: {error}"),
                    });
                    return;
                }
            }
        }

        if child_finished && readers_finished {
            break;
        }
    }

    let _ = updates.send(PreviewUpdate::Ready {
        generation,
        lines: document.into_lines().into(),
        truncated,
    });
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

#[cfg(windows)]
fn shell_command(preview_command: &str) -> Command {
    let mut command = Command::new("powershell.exe");
    let script = format!(
        "[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false); \
         $OutputEncoding = [Console]::OutputEncoding; {preview_command}"
    );
    command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"]);
    command.arg(script);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_publishes_one_completed_document() {
        let (updates, receiver) = bounded(4);
        let generation = 7;
        let current_generation = Arc::new(AtomicU64::new(generation));
        #[cfg(windows)]
        let command =
            "Write-Output 'first'; Write-Output 'second'; Write-Output $env:NFM_PREVIEW_LINE";
        #[cfg(not(windows))]
        let command = "printf 'first\\nsecond\\n%s\\n' \"$NFM_PREVIEW_LINE\"";

        run_process(
            command,
            None,
            &PreviewTarget {
                item: String::new(),
                center_line: Some(42),
            },
            generation,
            current_generation,
            updates,
        );

        let update = receiver.recv().expect("preview update");
        let PreviewUpdate::Ready {
            generation: result_generation,
            lines,
            truncated,
        } = update
        else {
            panic!("expected completed preview");
        };
        assert_eq!(result_generation, generation);
        assert!(!truncated);
        let text: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|span| span.text.as_str()).collect())
            .collect();
        assert_eq!(&text[..3], ["first", "second", "42"]);
    }
}

#[cfg(not(windows))]
fn shell_command(preview_command: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", preview_command]);
    command
}
