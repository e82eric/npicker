use super::PreviewEvents;
use std::sync::{Arc, Mutex};

use super::{send_preview, PreviewEvent, SelectionPreview};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeWindowId(pub isize);

pub(super) struct NativeWindowPreviewBackend<I> {
    pub(super) events: PreviewEvents,
    pub(super) selected: Mutex<Option<NativeWindowId>>,
    pub(super) target: Option<Arc<dyn Fn(&I) -> Option<NativeWindowId> + Send + Sync>>,
}

impl<I> SelectionPreview<I> for NativeWindowPreviewBackend<I> {
    fn selection_changed(&self, item: Option<&I>) {
        self.set_window(self.target(item));
    }

    fn clear(&self) {
        self.set_window(None);
    }
}

impl<I> NativeWindowPreviewBackend<I> {
    pub(super) fn target(&self, item: Option<&I>) -> Option<NativeWindowId> {
        (self.target.as_ref()?)(item?)
    }

    pub(super) fn set_window(&self, window: Option<NativeWindowId>) {
        let mut selected = self.selected.lock().expect("preview backend poisoned");
        if *selected == window {
            return;
        }
        *selected = window;
        drop(selected);
        send_preview(&self.events, PreviewEvent::NativeWindow(window));
    }
}
