# npicker

Rust crates for NFM's embedded egui picker. Hosts such as SimpleWindowManager
and nterm own the window, egui context, renderer and actions; these crates
supply the stores, search, pickers and previews.

| Crate | Purpose |
|---|---|
| [`nfm-egui`](nfm-egui/README.md) | The embedded picker control: search UI, previews, copy mode and ANSI parsing |
| `nfm-search-core` | Compact UTF-8 item stores, published snapshots and parallel fuzzy search |
| `nfm-file-system` | Background filesystem walker and its search store |
| [`nfm-picker-sources`](nfm-picker-sources/README.md) | Shared picker item sources and command sources |
| [`nfm-file-system-egui`](nfm-file-system-egui/README.md) | Filesystem picker with navigation and previews |
| [`nfm-process-egui`](nfm-process-egui/README.md) | Process picker with background refresh and actions |
| [`nfm-window-egui`](nfm-window-egui/README.md) | Window picker with live DWM previews |
| [`nfm-win32`](nfm-win32/README.md) | Windows enumeration, process actions and picker snapshots |
| [`nfm-preview`](nfm-preview/README.md) | Path previews: ffmpeg and ffprobe for media, highlighted text for everything else |
| [`nfm-preview-file`](nfm-preview-file/README.md) | Host-selected text, image and command preview plans |
| [`nfm-preview-text`](nfm-preview-text/README.md) | Syntax-highlighted text previews |
| [`nfm-preview-command`](nfm-preview-command/README.md) | Bounded, cancellable command execution for previews |
| `nfm-egui-demo` | A standalone window hosting the picker |

ANSI previews are parsed in pure Rust (`vte` plus a small line model in
`nfm-egui`), so the workspace needs no native terminal library, Zig toolchain or
build-time environment.

```text
cargo build --workspace
cargo test --workspace --all-features
```
