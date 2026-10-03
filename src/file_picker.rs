use std::sync::Arc;

use crate::action::{ActionDefinition, ActionResolution, ActionState, PickerState};
use crate::key_binding::{KeyChord, KeyModifiers, KeyName};
use crate::preview::native_file::preview_factory;
use crate::preview::{CommandPreviewTarget, PreviewFactory, PreviewRoutes};
use crate::request::{FileSystemPickerRequest, PickerRequest};
use crate::view_model::{PickerInteractions, SourceResolver, SourceTransition};

pub const ACCEPT_ACTION: &str = "ffi-accept";
pub const PARENT_ACTION: &str = "ffi-parent";

pub fn default_preview_factory() -> Arc<PreviewFactory> {
    preview_factory()
}

pub(crate) fn interactions() -> PickerInteractions<String> {
    interactions_with(PickerInteractions {
        preview_factory: default_preview_factory(),
        preview_routes: PreviewRoutes {
            command_target: Some(Arc::new(|item: &String| {
                Some(CommandPreviewTarget {
                    item: item.clone(),
                    center_line: None,
                })
            })),
            ..PreviewRoutes::default()
        },
        ..PickerInteractions::default()
    })
}

pub fn interactions_with(
    mut interactions: PickerInteractions<String>,
) -> PickerInteractions<String> {
    interactions.action_bindings.insert(
        KeyChord {
            key: KeyName::Character('u'),
            modifiers: KeyModifiers {
                ctrl: true,
                ..KeyModifiers::default()
            },
        },
        PARENT_ACTION.into(),
    );
    interactions.actions.insert(
        ACCEPT_ACTION.into(),
        ActionDefinition::Native(Arc::new(resolve_accept)),
    );
    interactions.actions.insert(
        PARENT_ACTION.into(),
        ActionDefinition::Native(Arc::new(resolve_parent)),
    );
    let transition_interactions = interactions.clone();
    interactions.source_resolver = Some(source_resolver(transition_interactions));
    interactions
}

pub use nfm_win32::logical_drive_roots;

fn resolve_accept(state: &ActionState<String>) -> Result<ActionResolution, String> {
    let Some(selection) = state.selection.as_ref() else {
        return Ok(ActionResolution::None);
    };
    if std::path::Path::new(selection).is_dir() {
        Ok(ActionResolution::Picker(PickerState::Filewalker {
            roots: vec![selection.clone()],
        }))
    } else {
        Ok(ActionResolution::Complete)
    }
}

fn resolve_parent(state: &ActionState<String>) -> Result<ActionResolution, String> {
    let PickerState::Filewalker { roots } = &state.picker else {
        return Ok(ActionResolution::None);
    };
    let roots = parent_roots(roots, logical_drive_roots());
    if roots.is_empty() {
        return Ok(ActionResolution::None);
    }
    Ok(ActionResolution::Picker(PickerState::Filewalker { roots }))
}

fn source_resolver(transition_interactions: PickerInteractions<String>) -> SourceResolver<String> {
    Arc::new(move |picker| match picker {
        PickerState::Filewalker { roots } => {
            let request = FileSystemPickerRequest {
                root_directories: roots,
                max_depth: i32::MAX,
                directories_only: false,
                files_only: false,
                search_string: None,
            };
            Ok(SourceTransition::new(
                request.run(),
                request.picker_state(),
                interactions_with(transition_interactions.clone()),
                true,
            ))
        }
        _ => Err("unsupported picker source transition".into()),
    })
}

use nfm_file_system::parent_roots;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_action_uses_the_single_roots_parent() {
        assert_eq!(
            parent_roots(
                &[r"C:\work\child".into()],
                vec![r"C:\".into(), r"D:\".into()],
            ),
            vec![r"C:\work".to_owned()]
        );
    }

    #[test]
    fn parent_action_shows_drives_at_a_file_system_root() {
        let drives = vec![r"C:\".to_owned(), r"D:\".to_owned()];
        assert_eq!(parent_roots(&[r"C:\".into()], drives.clone()), drives);
    }

    #[test]
    fn parent_action_keeps_the_drive_list_idempotent() {
        let drives = vec![r"C:\".to_owned(), r"D:\".to_owned()];
        assert_eq!(parent_roots(&drives, drives.clone()), drives);
    }

    #[test]
    fn accept_completes_for_a_non_directory() {
        let state = ActionState {
            selection: Some(r"Z:\nfm-nonexistent-file".into()),
            picker: PickerState::Stdin,
            query: String::new(),
        };
        assert!(matches!(
            resolve_accept(&state),
            Ok(ActionResolution::Complete)
        ));
    }
}
