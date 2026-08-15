use std::sync::Arc;

use crate::action::{ActionDefinition, ActionResolution};
use crate::list_windows::list_windows;
use crate::preview::native_file::preview_factory;
use crate::preview::{CommandPreviewTarget, NativeWindowId};
use crate::request::WindowListPickerRequest;
use crate::view_model::PickerInteractions;
use crate::WindowPickerItem;

pub fn request() -> Result<WindowListPickerRequest, String> {
    Ok(WindowListPickerRequest {
        items: list_windows().map_err(|error| format!("Failed to list windows: {error}"))?,
    })
}

pub fn interactions() -> PickerInteractions<WindowPickerItem> {
    interactions_with(PickerInteractions {
        preview_factory: preview_factory(),
        ..PickerInteractions::default()
    })
}

pub fn interactions_with(
    mut interactions: PickerInteractions<WindowPickerItem>,
) -> PickerInteractions<WindowPickerItem> {
    interactions.actions.insert(
        "ffi-accept".into(),
        ActionDefinition::Native(Arc::new(|_| Ok(ActionResolution::Complete))),
    );
    interactions.preview_routes.native_window = Some(Arc::new(|item: &WindowPickerItem| {
        Some(NativeWindowId(item.native_window))
    }));
    if interactions.preview_routes.command_target.is_none() {
        interactions.preview_routes.command_target = Some(Arc::new(|item: &WindowPickerItem| {
            Some(CommandPreviewTarget {
                item: item.title.clone(),
                center_line: None,
            })
        }));
    }
    interactions
}
