use std::collections::HashMap;
use std::sync::Arc;

use crate::action::{ActionDefinition, ActionResolution};
use crate::key_binding::{KeyChord, KeyModifiers, KeyName};
use crate::list_processes::{list_processes, terminate_process};
use crate::preview::{PreviewConfig, PreviewFactory, PreviewRoutes};
use crate::request::{PickerRequest, ProcessListPickerRequest};
use crate::view_model::{PickerActionOutcome, PickerInteractions};
use crate::ProcessPickerItem;

pub(crate) fn request() -> Result<ProcessListPickerRequest, String> {
    Ok(ProcessListPickerRequest {
        items: list_processes().map_err(|error| format!("Failed to list processes: {error}"))?,
    })
}

pub(crate) fn interactions() -> PickerInteractions<ProcessPickerItem> {
    let refresh = Arc::new(|| Ok(request()?.run()));
    let kill = Arc::new(|selected: Option<&ProcessPickerItem>| {
        let pid = selected
            .map(|item| item.pid)
            .ok_or_else(|| "No process selected".to_owned())?;
        terminate_process(pid)
            .map_err(|error| format!("Failed to terminate process {pid}: {error}"))?;
        Ok(PickerActionOutcome::RefreshWithToast(format!(
            "Terminated process {pid}"
        )))
    });
    let refresh_action = Arc::new(|_: Option<&ProcessPickerItem>| Ok(PickerActionOutcome::Refresh));
    let preview = Arc::new(|item: &ProcessPickerItem| {
        Ok(format!(
            "Name: {}\nPID: {}\nWorkingSet: {}\nPrivateBytes: {}\nCPU: {}",
            item.name, item.pid, item.working_set_kb, item.private_bytes_kb, item.cpu_seconds,
        ))
    });
    PickerInteractions {
        actions: HashMap::from([(
            "ffi-accept".into(),
            ActionDefinition::Native(Arc::new(|_| Ok(ActionResolution::Complete))),
        )]),
        bindings: HashMap::from([
            (
                KeyChord {
                    key: KeyName::Character('k'),
                    modifiers: KeyModifiers {
                        ctrl: true,
                        ..KeyModifiers::default()
                    },
                },
                kill as _,
            ),
            (
                KeyChord {
                    key: KeyName::Character('r'),
                    modifiers: KeyModifiers {
                        ctrl: true,
                        ..KeyModifiers::default()
                    },
                },
                refresh_action as _,
            ),
        ]),
        refresh: Some(refresh),
        preview_factory: Arc::new(PreviewFactory::new(PreviewConfig::Formatted)),
        preview_routes: PreviewRoutes {
            formatted: Some(preview),
            ..PreviewRoutes::default()
        },
        ..PickerInteractions::default()
    }
}
