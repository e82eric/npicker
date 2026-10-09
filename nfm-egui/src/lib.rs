#![doc = include_str!("../README.md")]
//! Embedded picker UI. Hosts own windows, input, egui/GL contexts and actions.
//! See the crate README for the embedding and lifetime contracts.
mod appearance;
pub mod copy_controller;
pub mod copy_document;
pub mod copy_mode;
pub mod document_search;
mod picker;
mod placement;
pub mod preview;
mod preview_copy;
pub mod quick_select;
#[cfg(feature = "opengl")]
pub mod renderer;
pub mod search_service;
mod session;
pub mod shared;
mod timing;

pub use appearance::{Appearance, FontFace, Palette, Typography};
pub use egui;
pub use nfm_search_core::search::SearchSortMode;
pub use nfm_search_core::{fuzzy_search_session::SearchSnapshotProvider, store::ItemsSource};
pub use picker::{
    Layout, Picker, PickerBinding, PickerConfig, PickerEvent, PickerOutput, PickerTrigger,
    Selection,
};
pub use placement::{Placement, Position};
pub use preview::{Cancellation, Preview, PreviewCell, PreviewProvider, PreviewRequest};
pub use session::SearchTimings;
pub mod byte_preview;
