use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::Sender;

const OUTPUT_LIMIT: usize = 1024 * 1024;
const DEBOUNCE: Duration = Duration::from_millis(75);

#[derive(Clone, Debug)]
pub enum PreviewUpdate {
    Clear { generation: u64 },
    Output { generation: u64, text: String },
    Error { generation: u64, message: String },
}

pub struct PreviewController {
    command: Arc<str>,
    generation: Arc<AtomicU64>,
    updates: Sender<PreviewUpdate>,
}

impl PreviewController {
    pub fn new(command: String, updates: Sender<PreviewUpdate>) -> Self {
        Self {
            command: Arc::from(command),
            generation: Arc::new(AtomicU64::new(0)),
            updates,
        }
    }

    pub fn request(&self, selected_item: String) -> u64 {
        let generation = self.next_generation();
        let current_generation = Arc::clone(&self.generation);
        let command = Arc::clone(&self.command);
        let updates = self.updates.clone();
        std::thread::spawn(move || {
            std::thread::sleep(DEBOUNCE);
            if current_generation.load(Ordering::Acquire) != generation {
                return;
            }
            run_process(
                &command,
                &selected_item,
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

fn run_process(
    preview_command: &str,
    selected_item: &str,
    generation: u64,
    current_generation: Arc<AtomicU64>,
    updates: Sender<PreviewUpdate>,
) {
    let mut command = shell_command(preview_command);
    command
        .env("NFM_PREVIEW_ITEM", selected_item)
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

    let bytes_sent = Arc::new(AtomicUsize::new(0));
    let output_limit_reached = Arc::new(AtomicBool::new(false));
    if let Some(stdout) = child.stdout.take() {
        forward_output(
            stdout,
            generation,
            Arc::clone(&bytes_sent),
            Arc::clone(&output_limit_reached),
            updates.clone(),
        );
    }
    if let Some(stderr) = child.stderr.take() {
        forward_output(
            stderr,
            generation,
            bytes_sent,
            Arc::clone(&output_limit_reached),
            updates.clone(),
        );
    }

    loop {
        if current_generation.load(Ordering::Acquire) != generation
            || output_limit_reached.load(Ordering::Acquire)
        {
            let _ = child.kill();
            let _ = child.wait();
            break;
        }
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                let _ = updates.send(PreviewUpdate::Error {
                    generation,
                    message: format!("preview process error: {error}"),
                });
                break;
            }
        }
    }
}

fn forward_output(
    reader: impl std::io::Read + Send + 'static,
    generation: u64,
    bytes_sent: Arc<AtomicUsize>,
    output_limit_reached: Arc<AtomicBool>,
    updates: Sender<PreviewUpdate>,
) {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            match reader.read_until(b'\n', &mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let previous = bytes_sent.fetch_add(bytes.len(), Ordering::AcqRel);
                    if previous >= OUTPUT_LIMIT {
                        output_limit_reached.store(true, Ordering::Release);
                        break;
                    }
                    let allowed = (OUTPUT_LIMIT - previous).min(bytes.len());
                    if allowed < bytes.len() {
                        output_limit_reached.store(true, Ordering::Release);
                    }
                    let text = String::from_utf8_lossy(&bytes[..allowed]).into_owned();
                    if updates
                        .send(PreviewUpdate::Output { generation, text })
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

#[cfg(not(windows))]
fn shell_command(preview_command: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command.args(["-c", preview_command]);
    command
}
