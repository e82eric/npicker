use std::collections::HashMap;
use std::sync::Arc;
use std::thread;

use anyhow::{anyhow, Result};
use crossbeam_channel::{bounded, Receiver, TryRecvError};
use nfm_search_core::store::ItemsSource;

use crate::action::{ActionDefinition, ActionResolution};
use crate::ffi::{
    command_interactions, file_system_interactions, logical_drive_roots, process_interactions,
};
use crate::key_binding::{KeyChord, KeyModifiers, KeyName};
use crate::list_processes::list_processes;
use crate::list_windows::list_windows;
use crate::preview::{CommandPreviewTarget, PreviewRoutes};
use crate::request::{
    FileSystemPickerRequest, FlatItemsPickerRequest, PickerRequest, PickerResponse,
    ProcessListPickerRequest, StructuredItemsPickerRequest, WindowListPickerRequest,
};
use crate::skia_ui::{self, ViewHandle};
use crate::view_model::{PickerInteractions, ViewModel};
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
        let bindings = HashMap::from([
            (
                KeyChord {
                    key: KeyName::Enter,
                    modifiers: KeyModifiers::default(),
                },
                "ffi-accept".into(),
            ),
            (
                KeyChord {
                    key: KeyName::Character('u'),
                    modifiers: KeyModifiers {
                        ctrl: true,
                        ..KeyModifiers::default()
                    },
                },
                "ffi-parent".into(),
            ),
        ]);
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
        self.spawn_request(request, command_interactions(|item: &String| item.clone()))
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
        self.spawn_request(request, file_system_interactions())
    }

    pub fn show_file_system(
        &self,
        mut options: FileSystemPickerOptions,
    ) -> Result<PickerTask<String>> {
        self.view_model.set_preview_visible(options.preview_visible);
        if options.roots.is_empty() {
            options.roots = logical_drive_roots();
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
        self.spawn_request(request, file_system_interactions())
    }

    pub fn show_windows(&self) -> Result<PickerTask<WindowPickerItem>> {
        self.view_model.set_preview_visible(true);
        let request = WindowListPickerRequest {
            items: list_windows().map_err(|error| anyhow!(error))?,
        };
        let interactions = PickerInteractions {
            actions: HashMap::from([(
                "ffi-accept".into(),
                ActionDefinition::Native(Arc::new(|_| Ok(ActionResolution::Complete))),
            )]),
            preview_factory: crate::ffi::ffi_preview_factory(),
            preview_routes: PreviewRoutes {
                command_target: Some(Arc::new(|item: &WindowPickerItem| {
                    Some(CommandPreviewTarget {
                        item: item.title.clone(),
                        center_line: None,
                    })
                })),
                native_window: Some(Arc::new(|item: &WindowPickerItem| {
                    Some(crate::preview::NativeWindowId(item.native_window))
                })),
                formatted: None,
            },
            ..PickerInteractions::default()
        };
        self.spawn_request(request, interactions)
    }

    pub fn show_processes(&self) -> Result<PickerTask<ProcessPickerItem>> {
        self.view_model.set_preview_visible(false);
        let request = ProcessListPickerRequest {
            items: list_processes().map_err(|error| anyhow!(error))?,
        };
        self.spawn_request(request, process_interactions())
    }

    pub fn show_structured(
        &self,
        columns: Vec<String>,
        rows: Vec<Vec<String>>,
        search_string: Option<String>,
    ) -> Result<PickerTask<StructuredPickerItem>> {
        if columns.is_empty() {
            return Err(anyhow!("structured picker requires at least one column"));
        }
        if let Some(row) = rows.iter().find(|row| row.len() != columns.len()) {
            return Err(anyhow!(
                "structured picker row has {} cells, expected {}",
                row.len(),
                columns.len()
            ));
        }
        nfm_picker_sources::structured::StructuredSchema::new(columns.clone())
            .map_err(|error| anyhow!(error))?;
        self.view_model.set_preview_visible(false);
        let request = StructuredItemsPickerRequest {
            columns,
            rows,
            search_string,
        };
        self.spawn_request(
            request,
            command_interactions(|item: &StructuredPickerItem| item.value.clone()),
        )
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
