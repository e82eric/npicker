//! Adapter for the same command previews used by the native Skia file picker.
use super::{CommandPreviewTarget, PreviewEvent, PreviewRoutes, PreviewUpdate};
use nfm_egui::{egui, Cancellation, Preview, PreviewCell, PreviewProvider, PreviewRequest};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

/// bat syntax highlighting, ffmpeg image decoding, and quarter-duration video frames.
/// No window or GPU context is created by this provider.
pub fn file_preview_provider() -> Arc<dyn PreviewProvider<String>> {
    file_preview_provider_with_bat_theme(None)
}

pub fn file_preview_provider_with_bat_theme(
    bat_theme: Option<String>,
) -> Arc<dyn PreviewProvider<String>> {
    let cache = Mutex::new(None::<(String, Option<std::time::SystemTime>, u64, Preview)>);
    Arc::new(
        move |request: PreviewRequest<String>, cancel: Cancellation| {
            let metadata = std::fs::metadata(&request.selection.item).map_err(|e| e.to_string())?;
            let modified = metadata.modified().ok();
            {
                let cached = cache.lock().unwrap();
                if let Some((path, time, size, preview)) = cached.as_ref() {
                    if path == &request.selection.item
                        && *time == modified
                        && *size == metadata.len()
                    {
                        return Ok(preview.clone());
                    }
                }
            }
            let result = render_preview_with_bat_theme(
                &request,
                || cancel.is_cancelled(),
                bat_theme.clone(),
            )
            .or_else(|error| {
                let extension = std::path::Path::new(&request.selection.item)
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if matches!(
                    extension.as_str(),
                    "png"
                        | "jpg"
                        | "jpeg"
                        | "gif"
                        | "bmp"
                        | "webp"
                        | "tif"
                        | "tiff"
                        | "mp4"
                        | "mkv"
                        | "mov"
                        | "avi"
                        | "webm"
                        | "m4v"
                        | "wmv"
                ) {
                    return Err(error);
                }
                super::file_fallback::load_preview(
                    std::path::Path::new(&request.selection.item),
                    || cancel.is_cancelled(),
                )
                .map(Preview::Text)
            });
            if !cancel.is_cancelled() {
                if let Ok(preview) = &result {
                    *cache.lock().unwrap() = Some((
                        request.selection.item,
                        modified,
                        metadata.len(),
                        preview.clone(),
                    ));
                }
            }
            result
        },
    )
}

#[cfg(test)]
fn render_preview(
    request: &PreviewRequest<String>,
    cancelled: impl Fn() -> bool,
) -> Result<Preview, String> {
    render_preview_with_bat_theme(request, cancelled, None)
}

fn render_preview_with_bat_theme(
    request: &PreviewRequest<String>,
    cancelled: impl Fn() -> bool,
    bat_theme: Option<String>,
) -> Result<Preview, String> {
    if cancelled() {
        return Ok(Preview::Empty);
    }
    let factory = super::native_file::preview_factory_with_bat_theme(bat_theme);
    let (send, receive) = crossbeam_channel::unbounded();
    let backend = factory.create(
        send.into(),
        PreviewRoutes::<String> {
            command_target: Some(Arc::new(|path| {
                Some(CommandPreviewTarget {
                    item: path.clone(),
                    center_line: None,
                })
            })),
            ..Default::default()
        },
    );
    backend.selection_changed(Some(&request.selection.item));
    let result = loop {
        if cancelled() {
            break Ok(Preview::Empty);
        }
        match receive.recv_timeout(Duration::from_millis(20)) {
            Ok(PreviewEvent::Command(update)) => match update {
                PreviewUpdate::Ready { lines, .. } => {
                    let lines = lines
                        .iter()
                        .map(|line| {
                            line.spans
                                .iter()
                                .map(|span| {
                                    let color = |value: super::document::AnsiColor| {
                                        let (r, g, b) = value.rgb();
                                        egui::Color32::from_rgb(r, g, b)
                                    };
                                    PreviewCell {
                                        text: span.text.clone(),
                                        foreground: span
                                            .style
                                            .foreground
                                            .map(color)
                                            .unwrap_or(request.foreground),
                                        background: span
                                            .style
                                            .background
                                            .map(color)
                                            .unwrap_or(request.background),
                                        underline_color: span
                                            .style
                                            .foreground
                                            .map(color)
                                            .unwrap_or(request.foreground),
                                        bold: span.style.bold,
                                        italic: span.style.italic,
                                        underline: span.style.underline,
                                        strikethrough: span.style.strikethrough,
                                        inverse: span.style.inverse,
                                        invisible: span.style.hidden,
                                        faint: span.style.dim,
                                    }
                                })
                                .collect()
                        })
                        .collect();
                    break Ok(Preview::StyledText(lines));
                }
                PreviewUpdate::ImageReady { encoded, .. } => {
                    let decoded = image::ImageReader::new(std::io::Cursor::new(encoded.as_ref()))
                        .with_guessed_format()
                        .map_err(|e| e.to_string());
                    break decoded.and_then(|mut reader| {
                        let mut limits = image::Limits::default();
                        limits.max_alloc = Some(128 * 1024 * 1024);
                        limits.max_image_width = Some(8192);
                        limits.max_image_height = Some(8192);
                        reader.limits(limits);
                        let rgba = reader.decode().map_err(|e| e.to_string())?.into_rgba8();
                        let image = egui::ColorImage::from_rgba_unmultiplied(
                            [rgba.width() as usize, rgba.height() as usize],
                            rgba.as_raw(),
                        );
                        Ok(Preview::Image(Arc::new(
                            nfm_egui::preview::PreviewImage::new(image),
                        )))
                    });
                }
                PreviewUpdate::Error { message, .. } => break Err(message),
                PreviewUpdate::Clear { .. } => {}
            },
            Ok(_) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                break Err("preview worker stopped".into())
            }
        }
    };
    backend.clear();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(path: &std::path::Path) -> PreviewRequest<String> {
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
            foreground: egui::Color32::WHITE,
            background: egui::Color32::BLACK,
        }
    }
    #[test]
    fn cancelled_preview_never_starts_a_process() {
        assert!(matches!(
            render_preview(&request(std::path::Path::new("missing.mp4")), || true).unwrap(),
            Preview::Empty
        ));
    }
    #[test]
    #[ignore = "requires bat, ffmpeg and ffprobe on PATH"]
    fn native_text_image_video_previews_reach_egui_with_colors_and_pixels() {
        let root = std::env::temp_dir().join(format!("nfm-egui-preview-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let text = root.join("sample.rs");
        std::fs::write(
            &text,
            "fn main() { let answer = 42; println!(\"hello\"); }\n",
        )
        .unwrap();
        let Preview::StyledText(lines) =
            render_preview_with_bat_theme(&request(&text), || false, Some("gruvbox-dark".into()))
                .unwrap()
        else {
            panic!("expected colored text")
        };
        assert!(lines
            .iter()
            .flatten()
            .any(|span| span.foreground != egui::Color32::WHITE));
        assert!(lines
            .iter()
            .flatten()
            .map(|span| span.text.as_str())
            .collect::<String>()
            .contains("answer"));
        let png = root.join("sample.png");
        image::RgbaImage::from_pixel(64, 32, image::Rgba([255, 0, 0, 255]))
            .save(&png)
            .unwrap();
        let video = root.join("sample.mp4");
        use std::os::windows::process::CommandExt;
        let output = std::process::Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=64x32:d=1",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&video)
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        for path in [&png, &video] {
            let Preview::Image(preview) = render_preview(&request(path), || false).unwrap() else {
                panic!("expected image for {}", path.display())
            };
            assert_eq!(preview.image.size, [64, 32]);
            let pixel = preview.image.pixels[0];
            assert!(pixel.r() > 200 && pixel.g() < 30 && pixel.b() < 30);
        }
        for path in [text, png, video] {
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_dir(root).unwrap();
    }
}
