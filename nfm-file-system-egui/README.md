# nfm-file-system-egui

Windows filesystem picker using a host-owned egui context. NFM owns streaming
scans, Enter-to-enter-directory, Ctrl+U parent/drive navigation, query and filter
resets, path copying, previews, and cancellation when closed or replaced.
Navigation preserves the live preview toggle and keeps the picker UI during
source transitions. Help blocks navigation; repeated egui layout passes cannot
accept the previous directory again.

Create `FilePicker::new(&ctx, options)` and call `show` with placement and
appearance. It returns `FileEvent::Selected(path)` or `FileEvent::Cancelled`;
directory selections are handled internally. The host owns windows and rendering.

For a shared picker with a custom item type, implement `FileSourceAdapter` to
map snapshot providers and extract paths, then use `FilePicker::with_picker`.
`into_picker` returns the UI control for another menu and cancels its scan.
The crate depends on the shared NFM libraries, without the legacy picker runtime.

For cross-workflow reuse without a host adapter, use `with_shared_picker` and
pass the previous workflow's `into_picker()` result. This uses NFM's generic
`SharedPicker`; built-in payload adaptation stays in the component.
