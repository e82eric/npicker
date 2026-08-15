use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;

use super::{
    send_preview, PreviewDocument, PreviewEvent, PreviewStream, PreviewUpdate, SelectionPreview,
};
use crate::view_model::ViewModelEvent;
pub type PickerPreviewFormatter<I> = Arc<dyn Fn(&I) -> Result<String, String> + Send + Sync>;

pub(super) struct FormattedPreviewBackend<I> {
    pub(super) events: Sender<ViewModelEvent>,
    pub(super) formatter: Option<PickerPreviewFormatter<I>>,
    pub(super) selected: Mutex<Option<Result<String, String>>>,
    pub(super) generation: Arc<AtomicU64>,
}

impl<I> SelectionPreview<I> for FormattedPreviewBackend<I> {
    fn selection_changed(&self, item: Option<&I>) {
        self.set_preview(match (self.formatter.as_ref(), item) {
            (Some(formatter), Some(item)) => Some(formatter(item)),
            _ => None,
        });
    }

    fn clear(&self) {
        self.set_preview(None);
    }
}

impl<I> FormattedPreviewBackend<I> {
    pub(super) fn is_configured(&self) -> bool {
        self.formatter.is_some()
    }

    pub(super) fn set_preview(&self, preview: Option<Result<String, String>>) {
        {
            let mut selected = self.selected.lock().expect("preview backend poisoned");
            if *selected == preview {
                return;
            }
            *selected = preview.clone();
        }
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        send_preview(
            &self.events,
            PreviewEvent::Command(PreviewUpdate::Clear { generation }),
        );
        let Some(preview) = preview else {
            return;
        };
        let text = match preview {
            Ok(text) => text,
            Err(message) => {
                send_preview(
                    &self.events,
                    PreviewEvent::Command(PreviewUpdate::Error {
                        generation,
                        message,
                    }),
                );
                return;
            }
        };
        let mut document = PreviewDocument::default();
        document.push(PreviewStream::Stdout, text.as_bytes());
        send_preview(
            &self.events,
            PreviewEvent::Command(PreviewUpdate::Ready {
                generation,
                lines: Arc::from(document.into_lines()),
                truncated: false,
                center_line: None,
            }),
        );
    }
}
