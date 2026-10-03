# nfm-preview

Shared preview resolution, bounded command output, cancellation, ANSI documents,
formatted text, native-window identifiers, and Windows file-preview profiles.

The default configuration has no renderer dependency. Enable `egui` for
`nfm_preview::egui::file_preview_provider()` and PNG decoding. This adapter creates
no native window or GPU context. Existing bat/ffmpeg/ffprobe behavior is retained.

`PreviewFactory::create` accepts `PreviewEvents`, which delivers `PreviewEvent`
through a host callback or a crossbeam sender. Callbacks should return promptly;
delivery runs on the producing thread. The legacy host wraps events in its own
view-model envelope without another forwarding thread. SWM uses the egui adapter
directly and does not depend on the legacy host.

The parser's document types are public so both egui and legacy renderers can
present the same output. The `preview` module remains available for compatibility.

Use `egui::file_preview_provider_with_bat_theme(Some("gruvbox-dark".into()))`
or `preview::native_file::preview_factory_with_bat_theme` to explicitly select a
bat theme. `None` preserves bat's config and inherited `BAT_THEME`. Filesystem
pickers expose this as `FileSystemPickerOptions::bat_theme` and retain it during
navigation. Theme names must be available in `bat --list-themes`.

All ANSI output is parsed by the standalone Ghostty VT library through
`nfm-preview-vt`, including command and formatted previews. Build the library with
`zig build -Demit-lib-vt -Doptimize=ReleaseFast -Dvt-features=-kitty-graphics --prefix zig-out/nfm-vt` in your Ghostty checkout, then set
`GHOSTTY_ROOT` to that checkout before building NFM or its consumers. Explicit
`NFM_GHOSTTY_VT_INCLUDE_DIR` and `NFM_GHOSTTY_VT_LIB_DIR` override the derived paths.
The styled-line API remains available to both renderers. Stdout and stderr are
parsed independently and displayed in that order, so terminal state cannot bleed
between streams. Capture is bounded to 20 MiB and 4000 lines; native scrollback
also limits retained physical rows.
