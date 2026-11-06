//! File-type routing for path previews: images and video frames through
//! ffmpeg/ffprobe, everything else through the built-in syntax highlighter.
//! `resolver` only chooses a plan; `nfm-preview-file` executes and renders it.
use nfm_egui::{Cancellation, PreviewProvider};
pub use nfm_preview_file::resolved::{
    CommandOutput, CommandSpec, PreviewPlan, ResolvedPreviewProvider,
};
use nfm_preview_file::text::{SyntaxChoice, TextContent, TextPreviewOptions, TextPreviewRequest};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

pub type FilePreviewResolver =
    Arc<dyn Fn(&Path, Cancellation) -> Result<Option<PreviewPlan>, String> + Send + Sync>;

const VIDEO: &[&str] = &["mp4", "mkv", "mov", "avi", "webm", "m4v", "wmv"];
const IMAGE: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp", "tif", "tiff"];

/// Tool paths, the video frame position and text options. Hosts that need
/// different routing can supply their own `FilePreviewResolver` instead.
#[derive(Clone, Debug)]
pub struct MediaPreviewSettings {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// Fraction of the video's duration, from 0 to 1.
    pub video_position: f64,
    /// Text highlighting options; `max_bytes` also bounds ffmpeg's output.
    pub files: TextPreviewOptions,
}
impl Default for MediaPreviewSettings {
    fn default() -> Self {
        Self {
            ffmpeg: "ffmpeg.exe".into(),
            ffprobe: "ffprobe.exe".into(),
            video_position: 0.25,
            files: TextPreviewOptions::default(),
        }
    }
}

/// A path preview provider with the default settings.
pub fn file_preview_provider() -> Arc<dyn PreviewProvider<String>> {
    path_preview_provider(resolver(MediaPreviewSettings::default()))
}

/// Preview `String` paths with any resolver.
pub fn path_preview_provider(resolve: FilePreviewResolver) -> Arc<dyn PreviewProvider<String>> {
    Arc::new(ResolvedPreviewProvider::new(
        move |selection: &nfm_egui::Selection<String>, cancel| {
            resolve(Path::new(&selection.item), cancel)
        },
    ))
}

/// Images and videos become ffmpeg commands (videos probe their duration with
/// ffprobe first, so this runs on the preview worker). Directories and all
/// other files are read and highlighted in process.
pub fn resolver(settings: MediaPreviewSettings) -> FilePreviewResolver {
    Arc::new(move |path, cancel| {
        if cancel.is_cancelled() {
            return Err("preview cancelled".into());
        }
        settings.files.validate()?;
        let metadata = fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let extension = path
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let video = VIDEO.contains(&extension.as_str());
        let image = IMAGE.contains(&extension.as_str());
        if metadata.is_dir() || !(image || video) {
            let owned = path.to_owned();
            let mut request = TextPreviewRequest::new(Arc::new(
                move |max_bytes: usize, cancel: &dyn Fn() -> bool| {
                    read_content(&owned, max_bytes, cancel)
                },
            ));
            request.name = Some(path.to_string_lossy().into_owned());
            return Ok(Some(PreviewPlan::Text {
                request,
                options: settings.files.clone(),
            }));
        }
        let mut command = CommandSpec::new(&settings.ffmpeg);
        command.arguments = vec!["-loglevel".into(), "error".into()];
        if video {
            if !settings.video_position.is_finite()
                || !(0.0..=1.0).contains(&settings.video_position)
            {
                return Err("video_position must be between 0 and 1".into());
            }
            let seconds = probe_duration(&settings.ffprobe, path, &cancel)?;
            command.arguments.extend([
                "-ss".into(),
                format!("{:.3}", seconds * settings.video_position),
            ]);
        }
        command.arguments.extend(
            [
                "-i",
                &path.to_string_lossy(),
                "-frames:v",
                "1",
                "-vf",
                // Shrink large frames to fit 1280x1280; never enlarge small ones.
                r"scale=min(1280\,iw):min(1280\,ih):force_original_aspect_ratio=decrease",
                "-f",
                "image2pipe",
                "-vcodec",
                "png",
                "pipe:1",
            ]
            .map(String::from),
        );
        command.output_limit = settings.files.max_bytes;
        Ok(Some(PreviewPlan::Command {
            command,
            output: CommandOutput::Image,
            center_line: None,
            max_scrollback_lines: 8192,
            success_exit_codes: vec![0],
        }))
    })
}

fn probe_duration(ffprobe: &Path, path: &Path, cancel: &Cancellation) -> Result<f64, String> {
    let mut probe = CommandSpec::new(ffprobe);
    probe.arguments = [
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
        &path.to_string_lossy(),
    ]
    .map(String::from)
    .to_vec();
    probe.output_limit = 64 * 1024;
    let result = nfm_preview_file::resolved::run(&probe, || cancel.is_cancelled(), |_, _| true)?;
    if !result.status.success() || result.truncated {
        return Err(format!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }
    String::from_utf8_lossy(&result.stdout)
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|duration| duration.is_finite() && *duration >= 0.0)
        .ok_or_else(|| "ffprobe returned an invalid duration".into())
}

fn cancelled(cancel: &dyn Fn() -> bool) -> Result<(), String> {
    if cancel() {
        Err("File preview cancelled".into())
    } else {
        Ok(())
    }
}

/// A sorted directory listing, a binary-file notice, or the file's UTF-8 text.
fn read_content(
    path: &Path,
    max_bytes: usize,
    cancel: &dyn Fn() -> bool,
) -> Result<TextContent, String> {
    cancelled(cancel)?;
    let metadata = fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if metadata.is_dir() {
        let mut entries = Vec::new();
        let mut bytes = 0usize;
        for entry in fs::read_dir(path).map_err(|e| e.to_string())? {
            cancelled(cancel)?;
            let entry = entry.map_err(|e| e.to_string())?;
            let slash = if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
                "/"
            } else {
                ""
            };
            let name = format!("{}{slash}", entry.file_name().to_string_lossy());
            bytes += name.len() + 1;
            if bytes > max_bytes {
                return Err("Directory preview exceeds max_bytes".into());
            }
            entries.push(name);
        }
        entries.sort();
        return Ok(TextContent::plain(entries.join("\n")));
    }
    if !metadata.is_file() {
        return Err("File preview requires a regular file or directory".into());
    }
    let too_large = || {
        format!(
            "the file is larger than the {} preview limit",
            format_size(max_bytes as u64)
        )
    };
    let unreadable = |error: std::io::Error| {
        Ok(fallback(path, metadata.len(), &format!("it could not be read: {error}")))
    };
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) => return unreadable(error),
    };
    // Sniff the start for a NUL byte, as Git does, so a binary file of any
    // size is reported as binary after one small read.
    let mut bytes = Vec::new();
    if let Err(error) = file.by_ref().take(SNIFF_BYTES).read_to_end(&mut bytes) {
        return unreadable(error);
    }
    cancelled(cancel)?;
    if bytes.contains(&0) {
        return Ok(file_info("Binary file", path, metadata.len()));
    }
    if metadata.len() > max_bytes as u64 {
        return Ok(fallback(path, metadata.len(), &too_large()));
    }
    let rest = (max_bytes as u64 + 1).saturating_sub(bytes.len() as u64);
    if let Err(error) = file.take(rest).read_to_end(&mut bytes) {
        return unreadable(error);
    }
    cancelled(cancel)?;
    // The file grew after its size was read.
    if bytes.len() > max_bytes {
        return Ok(fallback(path, metadata.len(), &too_large()));
    }
    match String::from_utf8(bytes) {
        Ok(text) if !text.contains('\0') => Ok(TextContent {
            text: text.trim_start_matches('\u{feff}').to_owned(),
            syntax: None::<SyntaxChoice>,
        }),
        _ => Ok(file_info("Binary file", path, metadata.len())),
    }
}

/// Bytes read from the start of a file to tell binary files from text.
const SNIFF_BYTES: u64 = 8 * 1024;

/// A file's path and size under a heading, shown instead of its contents.
fn file_info(heading: &str, path: &Path, size: u64) -> TextContent {
    TextContent::plain(format!("{heading}\n{}\n{size} bytes", path.display()))
}

/// File info for a file whose contents cannot be previewed, saying why. The
/// reason is also logged, since the preview would otherwise hide it.
fn fallback(path: &Path, size: u64, reason: &str) -> TextContent {
    tracing::warn!(
        target: "nfm_preview",
        path = %path.display(),
        size,
        "file preview fell back to file info: {reason}"
    );
    file_info(
        &format!("Showing file info: {reason}"),
        path,
        size,
    )
}

/// `bytes` in the largest whole binary unit, such as `10 MiB`.
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["bytes", "KiB", "MiB", "GiB"];
    let mut size = bytes;
    let mut unit = 0;
    while unit + 1 < UNITS.len() && size >= 1024 && size % 1024 == 0 {
        size /= 1024;
        unit += 1;
    }
    format!("{size} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;
    use nfm_egui::{Preview, PreviewRequest};

    fn request(path: &Path) -> PreviewRequest<String> {
        let item = path.to_string_lossy().into_owned();
        PreviewRequest {
            document_version: 0,
            initial_page: false,
            selection: nfm_egui::Selection {
                text: item.clone(),
                item,
                index: 0,
                source_version: 0,
                snapshot: Arc::new(()),
            },
            columns: 80,
            rows: 10,
            scroll_offset: 0,
            foreground: nfm_egui::egui::Color32::WHITE,
            background: nfm_egui::egui::Color32::BLACK,
            wrap_text: true,
        }
    }

    fn temp_dir(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("nfm-preview-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn media_routes_to_ffmpeg_and_everything_else_to_text() {
        let root = temp_dir("routing");
        let image = root.join("a.PNG");
        let source = root.join("a.rs");
        fs::write(&image, b"not really a png").unwrap();
        fs::write(&source, "fn main() {}\n").unwrap();
        let resolve = resolver(MediaPreviewSettings::default());
        let Some(PreviewPlan::Command {
            command, output, ..
        }) = resolve(&image, Cancellation::never()).unwrap()
        else {
            panic!("image should run ffmpeg")
        };
        assert!(matches!(output, CommandOutput::Image));
        assert_eq!(command.program, PathBuf::from("ffmpeg.exe"));
        assert!(!command.arguments.contains(&"-ss".into()));
        assert!(
            command
                .arguments
                .contains(&image.to_string_lossy().into_owned())
        );
        for path in [&source, &root] {
            assert!(matches!(
                resolve(path, Cancellation::never()).unwrap(),
                Some(PreviewPlan::Text { .. })
            ));
        }
        assert!(resolve(&root.join("missing.mp4"), Cancellation::never()).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn content_lists_directories_and_flags_binary_files() {
        let root = temp_dir("content");
        fs::create_dir(root.join("child")).unwrap();
        fs::write(root.join("b.bin"), [0u8, 1, 2]).unwrap();
        fs::write(root.join("a.txt"), "\u{feff}hello").unwrap();
        let never = &|| false;
        assert_eq!(
            read_content(&root, 1024, never).unwrap().text,
            "a.txt\nb.bin\nchild/"
        );
        assert_eq!(
            read_content(&root.join("a.txt"), 1024, never).unwrap().text,
            "hello"
        );
        assert!(
            read_content(&root.join("b.bin"), 1024, never)
                .unwrap()
                .text
                .starts_with("Binary file\n")
        );
        // Too large to preview: file info, saying why, instead of an error.
        let large = read_content(&root.join("a.txt"), 2, never).unwrap();
        assert_eq!(large.text.lines().count(), 3);
        assert_eq!(
            large.text.lines().next(),
            Some("Showing file info: the file is larger than the 2 bytes preview limit")
        );
        assert!(large.text.ends_with("\n8 bytes"));
        assert!(matches!(large.syntax, Some(SyntaxChoice::PlainText)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn binary_files_are_sniffed_before_the_size_limit() {
        let root = temp_dir("sniff");
        let never = &|| false;
        // A binary over the limit is still reported as binary, not too large.
        let mut object = vec![b'x'; 100];
        object[10] = 0;
        fs::write(root.join("big.o"), &object).unwrap();
        assert_eq!(
            read_content(&root.join("big.o"), 50, never).unwrap().text,
            format!("Binary file\n{}\n100 bytes", root.join("big.o").display())
        );
        // Text longer than the sniffed prefix is read whole.
        let text = "line of text\n".repeat(2000);
        fs::write(root.join("long.txt"), &text).unwrap();
        assert_eq!(read_content(&root.join("long.txt"), 1 << 20, never).unwrap().text, text);
        // A NUL after the prefix is still caught once the file is read.
        let mut late = vec![b'y'; SNIFF_BYTES as usize + 10];
        late[SNIFF_BYTES as usize + 5] = 0;
        fs::write(root.join("late.bin"), &late).unwrap();
        assert!(
            read_content(&root.join("late.bin"), 1 << 20, never)
                .unwrap()
                .text
                .starts_with("Binary file\n")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sizes_use_the_largest_whole_unit() {
        assert_eq!(format_size(10 * 1024 * 1024), "10 MiB");
        assert_eq!(format_size(1536), "1536 bytes");
        assert_eq!(format_size(4096), "4 KiB");
    }

    #[test]
    fn text_files_render_highlighted_with_a_copy_document() {
        let root = temp_dir("text");
        let source = root.join("sample.rs");
        fs::write(&source, "fn main() { let answer = 42; }\n").unwrap();
        let provider = file_preview_provider();
        let preview = provider
            .preview(request(&source), Cancellation::never())
            .unwrap();
        let Preview::Grid { cells, .. } = preview else {
            panic!("expected a highlighted grid")
        };
        assert!(
            cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
                .contains("answer")
        );
        assert!(
            cells
                .iter()
                .any(|cell| cell.foreground != nfm_egui::egui::Color32::WHITE)
        );
        assert!(provider.copy_document(0, 0).unwrap().is_some());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires ffmpeg and ffprobe on PATH"]
    fn images_and_video_frames_decode_to_pixels() {
        let root = temp_dir("media");
        let png = root.join("sample.png");
        image::RgbaImage::from_pixel(64, 32, image::Rgba([255, 0, 0, 255]))
            .save(&png)
            .unwrap();
        let video = root.join("sample.mp4");
        let output = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
            .args(["-i", "color=c=red:s=64x32:d=1", "-pix_fmt", "yuv420p"])
            .arg(&video)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        for path in [&png, &video] {
            let Preview::Image(preview) = file_preview_provider()
                .preview(request(path), Cancellation::never())
                .unwrap()
            else {
                panic!("expected image for {}", path.display())
            };
            assert_eq!(preview.image.size, [64, 32]);
            let pixel = preview.image.pixels[0];
            assert!(pixel.r() > 200 && pixel.g() < 30 && pixel.b() < 30);
        }
        fs::remove_dir_all(root).unwrap();
    }
}
