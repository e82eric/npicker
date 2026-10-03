# nfm-window-egui

Windows picker for a host-owned egui context and native HWND. NFM enumerates
candidates, excludes the host popup, searches, returns selected HWNDs, handles
copying/cancellation, and composites live DWM thumbnails into the preview area.
Thumbnails preserve aspect ratio and use the context's DPI scale. Disabling the
preview or dropping the component unregisters the thumbnail.

Use `WindowPicker::new(&ctx, destination_hwnd)` and call `show` with placement
and appearance. Handle `WindowEvent::Selected(hwnd)` or `Cancelled` in the host.
NFM does not activate selected windows or create a native window/GPU context.
Drop the component before destroying its destination window.

For a shared menu item type, implement `WindowSourceAdapter` and use
`with_picker` with presentation settings and an optional retained picker.
`into_picker` releases the thumbnail and returns the reusable UI control.
Opening constructs a fresh immutable snapshot; existing selection identities
retain their native HWND payload rather than depending on display text.

For cross-workflow reuse without a host adapter, use `with_shared_picker` and
pass the previous workflow's `into_picker()` result. This uses NFM's generic
`SharedPicker`; built-in payload adaptation stays in the component.
