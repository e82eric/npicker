# nfm-process-egui

Embedded Windows process picker for host-owned egui contexts. NFM handles
background enumeration, structured filters and columns, formatted previews,
Ctrl+R refresh, Ctrl+K termination followed by refresh, copying, and status/errors.
Refresh preserves the query and live preview toggle. Help blocks process actions.

Use `ProcessPicker::new(&ctx)`, then `show` with placement and appearance.
`show` returns true when accepted or cancelled so the host can dismiss its window.
The preview starts hidden and can be toggled with the normal picker shortcuts.

Hosts retaining a shared picker can implement `ProcessSourceAdapter` and use
`with_picker` with their presentation configuration. `into_picker` returns the
UI control for reuse. Workers publish only into their original store, so a
completed refresh cannot overwrite another menu. A requested termination runs
to completion even if its menu closes; closing does not undo that action.

No native window, GPU context, or legacy NFM picker runtime is created.

For cross-workflow reuse without a host adapter, use `with_shared_picker` and
pass the previous workflow's `into_picker()` result. This uses NFM's generic
`SharedPicker`; built-in payload adaptation stays in the component.
