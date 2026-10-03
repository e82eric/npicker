use std::sync::Arc;

use crate::action::{ActionDefinition, ActionResolution};
use crate::key_binding::{KeyChord, KeyModifiers, KeyName};
use crate::list_processes::{list_processes, terminate_process};
use crate::preview::{PreviewConfig, PreviewFactory};
use crate::request::{PickerRequest, ProcessListPickerRequest};
use crate::view_model::{PickerActionOutcome, PickerBinding, PickerInteractions};
use crate::ProcessPickerItem;

pub fn request() -> Result<ProcessListPickerRequest, String> {
    Ok(ProcessListPickerRequest {
        search_string: None,
        items: list_processes().map_err(|error| format!("Failed to list processes: {error}"))?,
    })
}

pub fn interactions() -> PickerInteractions<ProcessPickerItem> {
    interactions_with(PickerInteractions {
        preview_factory: Arc::new(PreviewFactory::new(PreviewConfig::Formatted)),
        ..PickerInteractions::default()
    })
}

pub fn interactions_with(
    mut interactions: PickerInteractions<ProcessPickerItem>,
) -> PickerInteractions<ProcessPickerItem> {
    let kill_chord = KeyChord {
        key: KeyName::Character('k'),
        modifiers: KeyModifiers {
            ctrl: true,
            ..KeyModifiers::default()
        },
    };
    let refresh_chord = KeyChord {
        key: KeyName::Character('r'),
        modifiers: KeyModifiers {
            ctrl: true,
            ..KeyModifiers::default()
        },
    };
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
    let preview = Arc::new(|item: &ProcessPickerItem| Ok(nfm_win32::format_process_preview(item)));
    interactions.actions.insert(
        "ffi-accept".into(),
        ActionDefinition::Native(Arc::new(|_| Ok(ActionResolution::Complete))),
    );
    interactions.bindings.extend([
        (
            kill_chord,
            PickerBinding {
                action: kill as _,
                help: "Kill selected process".to_owned(),
            },
        ),
        (
            refresh_chord,
            PickerBinding {
                action: refresh_action as _,
                help: "Refresh current picker".to_owned(),
            },
        ),
    ]);
    interactions.refresh = Some(refresh);
    interactions.preview_routes.formatted = Some(preview);
    interactions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_shortcuts_have_specific_help_text() {
        let interactions = interactions();
        let binding_for = |key| {
            interactions.bindings.get(&KeyChord {
                key: KeyName::Character(key),
                modifiers: KeyModifiers {
                    ctrl: true,
                    ..KeyModifiers::default()
                },
            })
        };

        assert_eq!(
            binding_for('k').map(|binding| binding.help.as_str()),
            Some("Kill selected process")
        );
        assert_eq!(
            binding_for('r').map(|binding| binding.help.as_str()),
            Some("Refresh current picker")
        );
    }
}
