use std::process::{Command, Stdio};
use std::sync::Arc;

use super::{
    CommandPreviewTarget, PreviewCancellation, PreviewConfig, PreviewFactory, PreviewJob,
    PreviewOutputType, PreviewProfile, PreviewResolver,
};

pub(crate) fn preview_factory() -> Arc<PreviewFactory> {
    Arc::new(PreviewFactory::new(PreviewConfig::CommandOrNativeWindow(
        PreviewResolver::Function(resolve_native_file_preview),
    )))
}

fn resolve_native_file_preview(
    target: &CommandPreviewTarget,
) -> Result<Option<PreviewJob>, String> {
    let path = std::path::Path::new(&target.item);
    if path.is_dir() {
        return Ok(Some(PreviewJob::Process(PreviewProfile {
            program: "cmd.exe".into(),
            arguments: vec!["/d".into(), "/c".into(), "dir".into(), "{item}".into()],
            working_directory: None,
            output_type: PreviewOutputType::Text,
        })));
    }
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        extension.as_str(),
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" | "wmv"
    ) {
        return Ok(Some(PreviewJob::Function(resolve_video_frame_preview)));
    }
    if matches!(
        extension.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tif" | "tiff"
    ) {
        return Ok(Some(PreviewJob::Process(PreviewProfile {
            program: "ffmpeg".into(),
            arguments: vec![
                "-loglevel".into(),
                "error".into(),
                "-i".into(),
                "{item}".into(),
                "-frames:v".into(),
                "1".into(),
                "-f".into(),
                "image2pipe".into(),
                "-vcodec".into(),
                "png".into(),
                "pipe:1".into(),
            ],
            working_directory: None,
            output_type: PreviewOutputType::Image,
        })));
    }
    if matches!(
        extension.as_str(),
        "txt"
            | "md"
            | "json"
            | "xml"
            | "yaml"
            | "yml"
            | "toml"
            | "ini"
            | "rs"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "cs"
            | "js"
            | "ts"
            | "css"
            | "html"
            | "ps1"
            | "cmd"
            | "bat"
            | "sh"
            | "py"
            | "rb"
            | "go"
            | "java"
            | "log"
    ) {
        return Ok(Some(PreviewJob::Process(PreviewProfile {
            program: "bat".into(),
            arguments: vec![
                "--color=always".into(),
                "--style=plain".into(),
                "--paging=never".into(),
                "{item}".into(),
            ],
            working_directory: None,
            output_type: PreviewOutputType::Text,
        })));
    }
    Ok(Some(PreviewJob::Process(PreviewProfile {
        program: "cmd.exe".into(),
        arguments: vec!["/d".into(), "/c".into(), "dir".into(), "{item}".into()],
        working_directory: None,
        output_type: PreviewOutputType::Text,
    })))
}

fn resolve_video_frame_preview(
    target: &CommandPreviewTarget,
    cancellation: &PreviewCancellation,
) -> Result<Option<PreviewProfile>, String> {
    let mut probe = Command::new("ffprobe");
    probe.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
    ]);
    probe.arg(&target.item).stdin(Stdio::null());
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    probe.creation_flags(CREATE_NO_WINDOW);
    let output = probe
        .output()
        .map_err(|error| format!("ffprobe failed to start: {error}"))?;
    if cancellation.is_cancelled() {
        return Ok(None);
    }
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if message.is_empty() {
            format!("ffprobe exited with {}", output.status)
        } else {
            message
        });
    }
    let duration = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|_| "ffprobe returned an invalid duration".to_string())?;
    if !duration.is_finite() || duration < 0.0 {
        return Err("ffprobe returned an invalid duration".into());
    }
    Ok(Some(PreviewProfile {
        program: "ffmpeg".into(),
        arguments: vec![
            "-loglevel".into(),
            "error".into(),
            "-ss".into(),
            format!("{:.3}", duration * 0.25),
            "-i".into(),
            "{item}".into(),
            "-frames:v".into(),
            "1".into(),
            "-f".into(),
            "image2pipe".into(),
            "-vcodec".into(),
            "png".into(),
            "pipe:1".into(),
        ],
        working_directory: None,
        output_type: PreviewOutputType::Image,
    }))
}
