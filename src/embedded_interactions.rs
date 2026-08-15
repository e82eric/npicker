use std::collections::HashMap;
use std::sync::Arc;

use crate::action::{ActionDefinition, ActionResolution};
use crate::preview::native_file::preview_factory;
use crate::preview::{CommandPreviewTarget, PreviewRoutes};
use crate::view_model::PickerInteractions;

pub(crate) fn command<I, F>(target: F) -> PickerInteractions<I>
where
    I: crate::PickerItem,
    F: Fn(&I) -> String + Send + Sync + 'static,
{
    PickerInteractions {
        actions: HashMap::from([(
            "ffi-accept".into(),
            ActionDefinition::Native(Arc::new(|_| Ok(ActionResolution::Complete))),
        )]),
        preview_factory: preview_factory(),
        preview_routes: PreviewRoutes {
            command_target: Some(Arc::new(move |item| {
                Some(CommandPreviewTarget {
                    item: target(item),
                    center_line: None,
                })
            })),
            ..PreviewRoutes::default()
        },
        ..PickerInteractions::default()
    }
}
