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

Delimited stdin can map separate fields to the searchable text, accepted value,
preview file, and one-based preview center line. Field numbers are one-based,
and the highest selected field consumes the rest of the line:

```powershell
rg --vimgrep TODO |
    nfm-rust-win32host `
        --delimiter ':' `
        --text-field all `
        --value-field 1 `
        --preview-file-field 1 `
        --preview-center-line-field 2 `
        --preview-cwd $PWD `
        --preview pwsh `
        --preview-arg -NoProfile `
        --preview-arg -Command `
        --preview-arg 'bat --color=always --paging=never --highlight-line $env:NFM_PREVIEW_LINE $env:NFM_PREVIEW_ITEM'
```

`--text-field all` searches and displays the original input record; it is the
default in delimiter mode. `--value-field all` returns the original input
record and is also the default. A one-based field number can be used for either
option. The text field consumes the rest of the line when a numbered text field
is used.

`--preview-cwd` sets only the preview child's working directory, so relative
preview filenames remain relative to the producer's directory. The selected
preview file and center line are exposed to that child as `NFM_PREVIEW_ITEM`
and `NFM_PREVIEW_LINE`. Use `--delimiter '\t'` for tab-delimited input.

On Windows, scan the current user's home directory without stdin:

```text
nfm-rust-win32host filewalker
```

Pass one or more roots after `filewalker` to scan different directories.

An accept resolver can decide whether Enter completes NFM or transitions the
same window to a new filewalker picker. The resolver is an executable followed
by repeatable `--accept-resolver-arg` arguments. It receives
`NFM_ACCEPT_ITEM`, `NFM_ACCEPT_VALUE`, and `NFM_ACCEPT_LINE`:

```powershell
nfm-rust-win32host filewalker `
    --accept-resolver pwsh `
    --accept-resolver-arg '-NoProfile' `
    --accept-resolver-arg '-Command' `
    --accept-resolver-arg 'if (Test-Path -LiteralPath $env:NFM_ACCEPT_ITEM -PathType Container) { @{ action = "picker"; picker = @{ kind = "filewalker"; roots = @("{item}") } } | ConvertTo-Json -Compress } else { @{ action = "complete" } | ConvertTo-Json -Compress }'
```

The resolver must write exactly one JSON object to stdout:

```json
{"action":"complete"}
```

or:

```json
{"action":"picker","picker":{"kind":"filewalker","roots":["{item}"]}}
```

For now, NFM expands a root only when its entire value is exactly `{item}`.
Literal roots are also supported. A filewalker transition keeps the original
output request open, and Escape cancels the whole NFM session.

On Windows, list the visible Alt-Tab application windows:

```text
nfm-rust-win32host listwindows
```

Rows contain the window handle, process ID, executable name, and title. The
selected row is written to stdout. Add a live, client-area DWM thumbnail of the
selected window with:

```text
nfm-rust-win32host listwindows --window-preview
```

An explicitly supplied `--preview` command takes precedence over the native
window thumbnail.

Add a non-blocking preview pane above the results with `--preview`. Its value is
an executable, and each repeatable `--preview-arg` supplies one argument. The
selected item is passed only to the preview child process in
`NFM_PREVIEW_ITEM`.

Press Ctrl+P to toggle the preview pane. Use `--preview-visible false` to start
with a configured preview hidden; preview commands continue running while the
pane is hidden so it can be restored immediately.

NFM does not implicitly invoke a shell. Invoke one explicitly when the preview
uses shell expressions or pipelines:

```powershell
nfm-rust-win32host filewalker `
    --preview pwsh `
    --preview-arg -NoProfile `
    --preview-arg -Command `
    --preview-arg 'Get-ChildItem -Force -LiteralPath $env:NFM_PREVIEW_ITEM | Out-String -Width 240'
```

To preview file contents:

```powershell
nfm-rust-win32host filewalker `
    --preview pwsh `
    --preview-arg -NoProfile `
    --preview-arg -Command `
    --preview-arg 'Get-Content -LiteralPath $env:NFM_PREVIEW_ITEM'
```

ANSI SGR colors and styling are rendered in the preview. Because preview output
is captured through a pipe rather than a terminal, commands must be told to
emit colors. For example, use `bat` with color forced and paging disabled:

```powershell
nfm-rust-win32host filewalker `
    --preview pwsh `
    --preview-arg -NoProfile `
    --preview-arg -Command `
    --preview-arg 'Get-Content -LiteralPath $env:NFM_PREVIEW_ITEM | bat --color=always --paging=never'
```

The preview supports standard and bright ANSI colors, 256-color and truecolor
sequences, backgrounds, bold, dim, italic, underline, strikethrough, hidden,
and inverse styling. It does not emulate an interactive terminal or implement
cursor-positioning and alternate-screen controls.

Preview arguments may contain `{item}` and `{line}`. NFM replaces them with the
selected preview item and center line as individual process arguments, so a
shell is not required. To render the first video frame as an image:

```powershell
nfm-rust-win32host filewalker `
    --preview ffmpeg `
    --preview-type image `
    --preview-arg '-loglevel' `
    --preview-arg 'error' `
    --preview-arg '-i' `
    --preview-arg '{item}' `
    --preview-arg '-frames:v' `
    --preview-arg '1' `
    --preview-arg '-f' `
    --preview-arg 'image2pipe' `
    --preview-arg '-vcodec' `
    --preview-arg 'png' `
    --preview-arg 'pipe:1'
```

Image previews expect an encoded image such as PNG or JPEG on stdout. Process
diagnostics remain on stderr. Images are limited to 32 MiB and are scaled to
fit the preview pane while preserving their aspect ratio.

For selection-dependent previews, `--preview-resolver` can classify the current
item by returning one configured profile name on stdout. Profiles are trusted
commands configured with `--preview-command` and related options:

```powershell
nfm-rust-win32host filewalker `
    --preview-resolver classify-preview.exe `
    --preview-resolver-arg '{item}' `
    --preview-command text bat `
    --preview-command-type text text `
    --preview-command-arg text '--color=always' `
    --preview-command-arg text '--paging=never' `
    --preview-command-arg text '{item}' `
    --preview-command image ffmpeg `
    --preview-command-type image image `
    --preview-command-arg image '-loglevel' `
    --preview-command-arg image 'error' `
    --preview-command-arg image '-i' `
    --preview-command-arg image '{item}' `
    --preview-command-arg image '-frames:v' `
    --preview-command-arg image '1' `
    --preview-command-arg image '-f' `
    --preview-command-arg image 'image2pipe' `
    --preview-command-arg image '-vcodec' `
    --preview-command-arg image 'png' `
    --preview-command-arg image 'pipe:1'
```

The resolver receives `NFM_PREVIEW_ITEM` and `NFM_PREVIEW_LINE`, supports the
same `{item}` and `{line}` argument placeholders, and must return zero or one
profile name. Empty output selects `--preview-default`, when supplied. Unknown
profiles, nonzero exit status, output over 64 KiB, multiple names, and resolver
runs longer than two seconds are reported as preview errors.

```sh
some-command-producing-lines |
    nfm-rust-win32host \
        --preview sh \
        --preview-arg -c \
        --preview-arg 'cat "$NFM_PREVIEW_ITEM"'
```

Preview commands are debounced as the selection moves. Older processes are
cancelled and late output is ignored. Output is accumulated and ANSI-parsed
off the UI thread, then displayed when the command finishes. A preview is
truncated and its process stopped after 4,000 lines or 1 MiB of combined
stdout/stderr.
