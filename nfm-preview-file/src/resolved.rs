//! Host-selected preview plans. File routing and external-tool policy belong to the host.
use nfm_egui::preview::{PreviewImage, PreviewViewport};
use nfm_egui::{Cancellation, Preview, PreviewProvider, PreviewRequest, Selection};
pub use nfm_preview_command::{CommandSpec, run};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, Default)]
pub enum CommandOutput {
    Text,
    #[default]
    Ansi,
    Image,
}

#[derive(Clone, Debug)]
pub enum PreviewPlan {
    Ansi {
        bytes: Arc<[u8]>,
        center_line: Option<usize>,
        max_bytes: usize,
        max_scrollback_lines: usize,
    },
    Image {
        bytes: Arc<[u8]>,
    },
    Text {
        request: crate::text::TextPreviewRequest,
        options: crate::text::TextPreviewOptions,
    },
    Command {
        command: CommandSpec,
        output: CommandOutput,
        center_line: Option<usize>,
        max_scrollback_lines: usize,
        success_exit_codes: Vec<i32>,
    },
}
impl PreviewPlan {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Ansi {
                bytes,
                center_line,
                max_bytes,
                max_scrollback_lines,
            } => {
                if !(1..=256 * 1024 * 1024).contains(max_bytes)
                    || bytes.len() > *max_bytes
                    || *max_scrollback_lines == 0
                    || *center_line == Some(0)
                {
                    return Err("Invalid ANSI preview limits or line number".into());
                }
            }
            Self::Image { bytes } => {
                if bytes.is_empty() || bytes.len() > 256 * 1024 * 1024 {
                    return Err("Invalid image preview size".into());
                }
            }
            Self::Text { request, options } => {
                options.validate()?;
                if request.center_line == Some(0) || request.highlight_line == Some(0) {
                    return Err("Text preview lines must be positive".into());
                }
                if let Some(syntax) = &request.syntax {
                    crate::text::TextPreviewOptions {
                        syntax: syntax.clone(),
                        ..options.clone()
                    }
                    .validate()?;
                }
            }
            Self::Command {
                command,
                output,
                center_line,
                max_scrollback_lines,
                success_exit_codes,
            } => {
                if command.program.as_os_str().is_empty()
                    || command.arguments.iter().any(|s| s.contains('\0'))
                {
                    return Err(
                        "Preview command requires an executable and arguments without NUL".into(),
                    );
                }
                if !(1..=256 * 1024 * 1024).contains(&command.output_limit)
                    || *max_scrollback_lines == 0
                    || *center_line == Some(0)
                {
                    return Err("Invalid command preview limits or line number".into());
                }
                if success_exit_codes.is_empty()
                    || success_exit_codes.len() > 16
                    || success_exit_codes.iter().any(|c| *c < 0)
                {
                    return Err("Invalid command preview success exit codes".into());
                }
                if center_line.is_some() && !matches!(output, CommandOutput::Ansi) {
                    return Err("center_line requires Ansi command output".into());
                }
            }
        }
        Ok(())
    }
}

type Resolver<T> =
    dyn Fn(&Selection<T>, Cancellation) -> Result<Option<PreviewPlan>, String> + Send + Sync;
type Key = (u64, usize, String);
struct Cached<T> {
    key: Key,
    provider: Arc<dyn PreviewProvider<T>>,
}

/// The resolver runs on NFM's worker once per selected item/document version.
/// Lua hosts can resolve on a separate Lua owner and supply owned plans here.
pub struct ResolvedPreviewProvider<T> {
    resolve: Arc<Resolver<T>>,
    cached: Mutex<Option<Cached<T>>>,
}
impl<T> ResolvedPreviewProvider<T> {
    pub fn new(
        resolve: impl Fn(&Selection<T>, Cancellation) -> Result<Option<PreviewPlan>, String>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            resolve: Arc::new(resolve),
            cached: Mutex::new(None),
        }
    }
}
fn decode_image(bytes: &[u8]) -> Result<Preview, String> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(128 * 1024 * 1024);
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|e| format!("Unable to decode preview image: {e}"))?;
    if u64::from(decoded.width()) * u64::from(decoded.height()) > 32 * 1024 * 1024 {
        return Err("Decoded preview image exceeds 128 MiB".into());
    }
    let rgba = decoded.into_rgba8();
    Ok(Preview::Image(Arc::new(PreviewImage::new(
        nfm_egui::egui::ColorImage::from_rgba_unmultiplied(
            [rgba.width() as usize, rgba.height() as usize],
            rgba.as_raw(),
        ),
    ))))
}
fn make_provider<T: Clone + Send + 'static>(
    plan: Option<PreviewPlan>,
    cancel: Cancellation,
) -> Result<Arc<dyn PreviewProvider<T>>, String> {
    if cancel.is_cancelled() {
        return Err("preview cancelled".into());
    }
    let Some(plan) = plan else {
        return Ok(Arc::new(|_: PreviewRequest<T>, _| Ok(Preview::Empty)));
    };
    plan.validate()?;
    match plan {
        PreviewPlan::Ansi {
            bytes,
            center_line,
            max_bytes,
            max_scrollback_lines,
        } => Ok(Arc::new(
            nfm_egui::byte_preview::BytePreviewProvider::new(move |_, _| {
                let mut document = nfm_egui::byte_preview::ByteDocument::complete(bytes.clone());
                document.center_row = center_line;
                Ok(document)
            })
            .with_limits(max_bytes, max_scrollback_lines),
        )),
        PreviewPlan::Image { bytes } => {
            let preview = decode_image(&bytes)?;
            if cancel.is_cancelled() {
                return Err("preview cancelled".into());
            }
            Ok(Arc::new(move |_: PreviewRequest<T>, _| Ok(preview.clone())))
        }
        PreviewPlan::Text { request, options } => Ok(Arc::new(
            crate::text::TextPreviewProvider::new(options, move |_| Ok(Some(request.clone())))?,
        )),
        PreviewPlan::Command {
            command,
            output,
            center_line,
            max_scrollback_lines,
            success_exit_codes,
        } => {
            if matches!(output, CommandOutput::Ansi) {
                let max_bytes = command.output_limit;
                return Ok(Arc::new(
                    nfm_egui::byte_preview::BytePreviewProvider::new(move |_, cancel| {
                        nfm_egui::byte_preview::command_document(
                            &command,
                            center_line,
                            &success_exit_codes,
                            cancel,
                        )
                    })
                    .with_limits(max_bytes, max_scrollback_lines),
                ));
            }
            let result = run(&command, || cancel.is_cancelled(), |_, _| true)?;
            if cancel.is_cancelled() {
                return Err("preview cancelled".into());
            }
            if result.truncated {
                return Err("Preview command exceeds max_bytes".into());
            }
            if !result
                .status
                .code()
                .is_some_and(|c| success_exit_codes.contains(&c))
            {
                return Err(format!(
                    "Preview command exited with {}: {}",
                    result.status,
                    String::from_utf8_lossy(&result.stderr).trim()
                ));
            }
            let preview = match output {
                CommandOutput::Image => decode_image(&result.stdout)?,
                CommandOutput::Text => {
                    let text = String::from_utf8_lossy(&result.stdout).into_owned();
                    if text.lines().count() > max_scrollback_lines {
                        return Err("Preview text exceeds max_scrollback_lines".into());
                    }
                    Preview::Text(text)
                }
                CommandOutput::Ansi => unreachable!(),
            };
            if cancel.is_cancelled() {
                return Err("preview cancelled".into());
            }
            Ok(Arc::new(move |_: PreviewRequest<T>, _| Ok(preview.clone())))
        }
    }
}
impl<T: Clone + Send + 'static> PreviewProvider<T> for ResolvedPreviewProvider<T> {
    fn preview(&self, request: PreviewRequest<T>, cancel: Cancellation) -> Result<Preview, String> {
        self.preview_viewport(request, cancel).map(|p| p.document)
    }
    fn preview_viewport(
        &self,
        request: PreviewRequest<T>,
        cancel: Cancellation,
    ) -> Result<PreviewViewport, String> {
        let key = (
            request.document_version,
            request.selection.index,
            request.selection.text.clone(),
        );
        let provider = self
            .cached
            .lock()
            .map_err(|_| "Resolved preview cache poisoned")?
            .as_ref()
            .filter(|c| c.key == key)
            .map(|c| c.provider.clone());
        if let Some(provider) = provider {
            return provider.preview_viewport(request, cancel);
        }
        // Never hold the cache lock while resolving, running processes or reading
        // files: the UI may ask for the previous document during replacement.
        let plan = (self.resolve)(&request.selection, cancel.clone())?;
        let provider = make_provider(plan, cancel.clone())?;
        let page = provider.preview_viewport(request, cancel.clone())?;
        if cancel.is_cancelled() {
            return Err("preview cancelled".into());
        }
        *self
            .cached
            .lock()
            .map_err(|_| "Resolved preview cache poisoned")? = Some(Cached { key, provider });
        Ok(page)
    }
    fn copy_document(
        &self,
        version: u64,
        index: usize,
    ) -> Result<Option<Arc<dyn nfm_egui::copy_document::CopyDocument>>, String> {
        let cached = self
            .cached
            .lock()
            .map_err(|_| "Resolved preview cache poisoned")?;
        match cached
            .as_ref()
            .filter(|c| (c.key.0, c.key.1) == (version, index))
        {
            Some(c) => c.provider.copy_document(version, index),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn image_output_decodes_pixels_and_rejects_invalid_data() {
        let pixels = image::RgbaImage::from_pixel(2, 1, image::Rgba([20, 30, 40, 255]));
        let mut encoded = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(pixels)
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        let Preview::Image(preview) = decode_image(encoded.get_ref()).unwrap() else {
            panic!("Image expected")
        };
        assert_eq!(preview.image.size, [2, 1]);
        assert_eq!(
            preview.image.pixels[0],
            nfm_egui::egui::Color32::from_rgb(20, 30, 40)
        );
        assert!(decode_image(b"not an image").is_err());
    }
}
