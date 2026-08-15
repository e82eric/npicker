pub mod action;
#[cfg(windows)]
mod clipboard;
#[cfg(windows)]
pub(crate) mod embedded_interactions;
#[cfg(windows)]
pub mod ffi;
#[cfg(windows)]
pub mod file_picker;
pub mod key_binding;
#[cfg(windows)]
pub mod list_processes;
#[cfg(windows)]
pub mod list_windows;
mod picker_snapshot;
#[cfg(windows)]
pub mod process_picker;
#[cfg(windows)]
pub mod window_picker;
pub use nfm_picker_sources::structured::StructuredPickerItem;
#[cfg(windows)]
pub use picker_snapshot::{ProcessPickerItem, WindowPickerItem};
pub mod preview;
pub mod request;
#[cfg(windows)]
pub mod runtime;
pub mod skia_ui;
pub mod view_model;

use serde::Serialize;

pub trait PickerItem: Clone + Serialize + Send + Sync + 'static {
    fn value(&self) -> &str;
}

impl PickerItem for String {
    fn value(&self) -> &str {
        self
    }
}

impl PickerItem for nfm_picker_sources::structured::StructuredPickerItem {
    fn value(&self) -> &str {
        &self.value
    }
}

impl PickerItem for nfm_picker_sources::delimited::DelimitedPickerItem {
    fn value(&self) -> &str {
        &self.value
    }
}
