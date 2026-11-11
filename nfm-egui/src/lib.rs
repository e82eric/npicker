#![doc = include_str!("../README.md")]
//! Embedded picker UI. Hosts own windows, input, egui/GL contexts and actions.
//! See the crate README for the embedding and lifetime contracts.
mod ansi;
mod appearance;
pub mod copy_controller;
pub mod copy_document;
pub mod copy_mode;
pub mod document_search;
pub mod icons;
mod output;
mod pages;
mod picker;
mod placement;
pub mod preview;
mod preview_copy;
mod prompt;
mod query_box;
pub mod quick_select;
#[cfg(feature = "opengl")]
pub mod renderer;
pub mod search_service;
mod session;
pub mod shared;

pub use appearance::{Appearance, FontFace, Palette, Typography};
pub use egui;
pub use nfm_search_core::search::SearchSortMode;
pub use nfm_search_core::{fuzzy_search_session::SearchSnapshotProvider, store::ItemsSource};
pub use output::{
    OUTPUT_FRAME_HEIGHT, Output, OutputConfig, OutputEvent, OutputWriter,
};
pub use pages::{Outcome, PageStack};
pub use picker::{
    CommandScope, Layout, Picker, PickerAction, PickerBinding, PickerCommand, PickerConfig,
    PickerEvent, PickerOutput, PickerTrigger, PreviewLineNumbers, RowIcon, Selection,
    default_bindings, default_commands, results_height,
};
pub use placement::{Placement, Position};
pub use prompt::{
    PROMPT_FRAME_HEIGHT, Prompt, PromptConfig, PromptEvent, PromptOutput, prompt_height,
};
pub use preview::{Cancellation, GridLines, Preview, PreviewCell, PreviewProvider, PreviewRequest};
pub use session::SearchTimings;
pub mod byte_preview;
pub mod styled_document;
