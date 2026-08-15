use std::collections::HashMap;
use std::sync::Arc;

use crate::action::{ActionDefinition, ActionResolution};
use crate::list_windows::list_windows;
use crate::preview::native_file::preview_factory;
use crate::preview::{CommandPreviewTarget, NativeWindowId, PreviewRoutes};
use crate::request::WindowListPickerRequest;
use crate::view_model::PickerInteractions;
use crate::WindowPickerItem;

pub(crate) fn request() -> Result<WindowListPickerRequest, String> {
    Ok(WindowListPickerRequest {
        items: list_windows().map_err(|error| format!("Failed to list windows: {error}"))?,
    })
}

pub(crate) fn interactions() -> PickerInteractions<WindowPickerItem> {
    PickerInteractions {
        actions: HashMap::from([(
            "ffi-accept".into(),
            ActionDefinition::Native(Arc::new(|_| Ok(ActionResolution::Complete))),
        )]),
        preview_factory: preview_factory(),
        preview_routes: PreviewRoutes {
            command_target: Some(Arc::new(|item: &WindowPickerItem| {
                Some(CommandPreviewTarget {
                    item: item.title.clone(),
                    center_line: None,
                })
            })),
            native_window: Some(Arc::new(|item: &WindowPickerItem| {
                Some(NativeWindowId(item.native_window))
            })),
            formatted: None,
        },
        ..PickerInteractions::default()
    }
}
