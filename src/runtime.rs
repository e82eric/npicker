use std::collections::HashMap;
use std::sync::Arc;
use std::thread;

use anyhow::{anyhow, Result};
use crossbeam_channel::{bounded, Receiver, TryRecvError};
use nfm_search_core::store::ItemsSource;

use crate::embedded_interactions;
use crate::file_picker;
use crate::key_binding::{KeyChord, KeyModifiers, KeyName};
use crate::preview::{PreviewConfig, PreviewFactory};
use crate::process_picker;
pub use crate::request::StructuredPickerRow;
use crate::request::{
    FileSystemPickerRequest, FlatItemsPickerRequest, PickerRequest, PickerResponse,
    StructuredItemsPickerRequest,
};
use crate::skia_ui::{self, ViewHandle};
use crate::view_model::{PickerInteractions, ViewModel};
use crate::window_picker;
use crate::{ProcessPickerItem, StructuredPickerItem, WindowPickerItem};

/// Owns a running picker UI and starts typed picker requests against it.
#[derive(Clone)]
pub struct PickerRuntime {
    view_model: Arc<ViewModel>,
}

/// A picker request running on a background thread.
pub struct PickerTask<T> {
    receiver: Receiver<Result<PickerResponse<T>>>,
}

impl<T> PickerTask<T> {
    pub fn recv(self) -> Result<PickerResponse<T>> {
        self.receiver
            .recv()
            .map_err(|_| anyhow!("picker request thread disconnected"))?
    }

    pub fn try_recv(&self) -> Result<Option<PickerResponse<T>>> {
        match self.receiver.try_recv() {
            Ok(response) => response.map(Some),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(anyhow!("picker request thread disconnected")),
        }
    }
}

#[derive(Clone, Debug)]
pub struct FileSystemPickerOptions {
    pub roots: Vec<String>,
    pub max_depth: i32,
    pub directories_only: bool,
    pub files_only: bool,
    pub search_string: Option<String>,
    pub preview_visible: bool,
}

impl Default for FileSystemPickerOptions {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            max_depth: i32::MAX,
            directories_only: false,
            files_only: false,
            search_string: None,
            preview_visible: true,
        }
    }
}

impl PickerRuntime {
    pub fn start() -> Result<Self> {
        let bindings = HashMap::from([(
            KeyChord {
                key: KeyName::Enter,
                modifiers: KeyModifiers::default(),
            },
            "ffi-accept".into(),
        )]);
        let view = Arc::new(ViewHandle::new());
        let view_model = ViewModel::new_with_bindings(bindings, false, view.clone());
        let ui_view_model = Arc::clone(&view_model);
        thread::Builder::new()
            .name("nfm-picker-ui".into())
            .spawn(move || {
                if let Err(error) = skia_ui::run(ui_view_model, view, true, false) {
                    eprintln!("NFM picker UI stopped: {error:#}");
                }
            })?;
        Ok(Self { view_model })
    }

    pub fn set_preferred_center(&self, x: i32, y: i32) {
        skia_ui::set_preferred_center(x, y);
    }

    pub fn hide(&self) {
        self.view_model.cancel();
    }

    pub fn focus(&self) {
        self.view_model.focus();
    }

    pub fn show_items(
        &self,
        items: Vec<String>,
        search_string: Option<String>,
    ) -> Result<PickerTask<String>> {
        self.view_model.set_preview_visible(false);
        let request = FlatItemsPickerRequest {
            items,
            search_string,
        };
        self.spawn_request(
            request,
            embedded_interactions::command(|item: &String| item.clone()),
        )
    }

    pub fn show_programs(&self, directories: Vec<String>) -> Result<PickerTask<String>> {
        self.view_model.set_preview_visible(false);
        let request = FileSystemPickerRequest {
            root_directories: directories,
            max_depth: 5,
            directories_only: false,
            files_only: true,
            search_string: None,
        };
        self.spawn_request(request, file_picker::interactions())
    }

    pub fn show_file_system(
        &self,
        mut options: FileSystemPickerOptions,
    ) -> Result<PickerTask<String>> {
        self.view_model.set_preview_visible(options.preview_visible);
        if options.roots.is_empty() {
            options.roots = file_picker::logical_drive_roots();
        }
        if options.roots.is_empty() {
            return Err(anyhow!("no file-system roots are available"));
        }
        let request = FileSystemPickerRequest {
            root_directories: options.roots,
            max_depth: options.max_depth,
            directories_only: options.directories_only,
            files_only: options.files_only,
            search_string: options.search_string,
        };
        self.spawn_request(request, file_picker::interactions())
    }

    pub fn show_windows(&self) -> Result<PickerTask<WindowPickerItem>> {
        self.view_model.set_preview_visible(true);
        self.spawn_request(
            window_picker::request().map_err(anyhow::Error::msg)?,
            window_picker::interactions(),
        )
    }

    pub fn show_processes(&self) -> Result<PickerTask<ProcessPickerItem>> {
        self.view_model.set_preview_visible(false);
        self.spawn_request(
            process_picker::request().map_err(anyhow::Error::msg)?,
            process_picker::interactions(),
        )
    }

    pub fn show_structured(
        &self,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        search_string: Option<String>,
    ) -> Result<PickerTask<StructuredPickerItem>> {
        self.show_structured_rows(
            columns,
            rows.into_iter()
                .map(|cells| StructuredPickerRow {
                    cells,
                    preview: None,
                })
                .collect(),
            search_string,
            false,
        )
    }

    pub fn show_structured_with_previews(
        &self,
        columns: Vec<String>,
        rows: Vec<StructuredPickerRow>,
        search_string: Option<String>,
    ) -> Result<PickerTask<StructuredPickerItem>> {
        self.show_structured_rows(columns, rows, search_string, true)
    }

    fn show_structured_rows(
        &self,
        columns: Vec<String>,
        rows: Vec<StructuredPickerRow>,
        search_string: Option<String>,
        preview_visible: bool,
    ) -> Result<PickerTask<StructuredPickerItem>> {
        if columns.is_empty() {
            return Err(anyhow!("structured picker requires at least one column"));
        }
        if let Some(row) = rows.iter().find(|row| row.cells.len() != columns.len()) {
            return Err(anyhow!(
                "structured picker row has {} cells, expected {}",
                row.cells.len(),
                columns.len()
            ));
        }
        nfm_picker_sources::structured::StructuredSchema::new(columns.clone())
            .map_err(|error| anyhow!(error))?;
        self.view_model.set_preview_visible(preview_visible);
        let request = StructuredItemsPickerRequest {
            columns,
            rows,
            search_string,
        };
        let mut interactions =
            embedded_interactions::command(|item: &StructuredPickerItem| item.value.clone());
        if preview_visible {
            interactions.preview_factory = Arc::new(PreviewFactory::new(PreviewConfig::Formatted));
            interactions.preview_routes.formatted =
                Some(Arc::new(|item: &StructuredPickerItem| {
                    Ok(item.preview.clone().unwrap_or_default())
                }));
        }
        self.spawn_request(request, interactions)
    }

    fn spawn_request<R>(
        &self,
        request: R,
        interactions: PickerInteractions<<<R as PickerRequest>::Source as ItemsSource>::Item>,
    ) -> Result<PickerTask<<<R as PickerRequest>::Source as ItemsSource>::Item>>
    where
        R: PickerRequest + Send + 'static,
    {
        let view_model = Arc::clone(&self.view_model);
        let (sender, receiver) = bounded(1);
        thread::Builder::new()
            .name("nfm-picker-request".into())
            .spawn(move || {
                let response = view_model.run_request_with_interactions(&request, interactions);
                let _ = sender.send(response);
            })?;
        Ok(PickerTask { receiver })
    }
}
