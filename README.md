# nfm Rust Picker Prototype

This is a Rust port of the picker host. It is intentionally
separate from the C# projects so the storage/search/UI design can evolve without
disturbing the current implementation.

Goals:

- Cross-platform Windows/Linux CLI consuming newline-delimited items from stdin.
- A Windows-only file-system walker retained for native FFI callers.
- Compact UTF-8 file store with reverse-linked nodes, name interning, chunked
  backing storage, and published snapshots for streaming search.
- Parallel top-K fuzzy search over published snapshots.
- `nucleo-matcher` fuzzy scoring.
- A winit window with CPU-rasterized Skia and softbuffer presentation.

The crate is structured around the same runtime boundaries as the C# path:

- `request`: streaming stdin, flat-item, and Windows filesystem requests.
- `store`: compact UTF-8 node/name storage and published snapshots.
- `walker`: background file-system scanner.
- `search`: parallel fuzzy search over a published snapshot.
- `preview`: cancellable, generation-tagged preview command worker.
- `view_model`: owns scan/search state and UI events.
- `skia_ui`: winit/Skia picker window.

Run the picker by piping candidates to stdin. The selected item is printed to stdout:

```text
some-command-producing-lines | nfm-rust-win32host
```

On Windows, scan the current user's home directory without stdin:

```text
nfm-rust-win32host filewalker
```

Pass one or more roots after `filewalker` to scan different directories.

Add a non-blocking preview pane above the results with `--preview`. The selected
item is passed only to the preview child process in `NFM_PREVIEW_ITEM`.

On Windows, preview commands run in a non-interactive PowerShell process:

```powershell
nfm-rust-win32host filewalker --preview 'Get-ChildItem -Force -LiteralPath $env:NFM_PREVIEW_ITEM | Out-String -Width 240'
```

To preview file contents:

```powershell
nfm-rust-win32host filewalker --preview 'Get-Content -LiteralPath $env:NFM_PREVIEW_ITEM'
```

ANSI SGR colors and styling are rendered in the preview. Because preview output
is captured through a pipe rather than a terminal, commands must be told to
emit colors. For example, use `bat` with color forced and paging disabled:

```powershell
nfm-rust-win32host filewalker --preview 'Get-Content -LiteralPath $env:NFM_PREVIEW_ITEM | bat --color=always --paging=never'
```

The preview supports standard and bright ANSI colors, 256-color and truecolor
sequences, backgrounds, bold, dim, italic, underline, strikethrough, hidden,
and inverse styling. It does not emulate an interactive terminal or implement
cursor-positioning and alternate-screen controls.

On other platforms, preview commands run through `/bin/sh`:

```sh
some-command-producing-lines | nfm-rust-win32host --preview 'cat "$NFM_PREVIEW_ITEM"'
```

Preview commands are debounced as the selection moves. Older processes are
cancelled and late output is ignored. Output is accumulated and ANSI-parsed
off the UI thread, then displayed when the command finishes. A preview is
truncated and its process stopped after 4,000 lines or 1 MiB of combined
stdout/stderr.
