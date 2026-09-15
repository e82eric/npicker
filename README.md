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
- A native Win32 popup with a persistent CPU-rasterized Skia surface and GDI presentation.

The crate is structured around the same runtime boundaries as the C# path:

- `request`: streaming stdin, flat-item, and Windows filesystem requests.
- `store`: compact UTF-8 node/name storage and published snapshots.
- `walker`: background file-system scanner.
- `search`: parallel fuzzy search over a published snapshot.
- `preview`: cancellable, generation-tagged preview command worker.
- `view_model`: owns scan/search state and UI events.
- `skia_ui`: native Win32 message loop, Skia raster renderer, GDI presentation, and DWM thumbnails.

Run the picker by piping candidates to stdin. The selected item is printed to stdout:

```text
some-command-producing-lines | nfm-rust-win32host
```

Use `--query "text"` to prefill the search box in any input mode:

```powershell
nfm-rust-win32host filesystem G:\src --query "walker"
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

Structured CSV input streams complete CSV records into a schema-aware store.
By default, the first record supplies the column names:

```powershell
Get-Process |
    Select-Object Name,Id,CPU |
    ConvertTo-Csv -NoTypeInformation |
    nfm-rust-win32host --input-format csv
```

When the stream contains data records only, provide the headers explicitly:

```text
producer | nfm-rust-win32host --input-format csv --csv-columns Name,Id,CPU
```

`--csv-delimiter` selects another single-byte delimiter. The CSV decoder
supports quoted delimiters, escaped quotes, and quoted multiline fields.
Multiline values are escaped for the one-line picker display while the accepted
value remains a valid CSV record.

Structured queries use the same slash syntax as the C# picker. Ordinary query
text continues to use fuzzy matching, while complete structured expressions
are compiled against the CSV schema:

```text
server /:Status==Running /:CPU>=10 /!Descending==CPU
```

Supported filter operators are `==`, `!=`, `=~`, `!~`, `>`, `>=`, `<`, and
`<=`. Multiple values may be separated with commas. Type `/` to open
autocomplete; it suggests actions, column names, operators, sort directions,
and up to 1,000 distinct values observed in the selected column. Use the arrow
keys and Enter to select a suggestion, or Escape to dismiss it.

On Windows, open the filesystem picker without stdin:

```text
nfm-rust-win32host filesystem
```

Pass one or more roots after `filesystem` to scan different directories. With no
roots, the picker starts from the available logical drives.

Limit traversal depth or filter the result type with filesystem-specific options:

```powershell
nfm-rust-win32host filesystem G:\src --max-depth 5 --files-only
nfm-rust-win32host filesystem G:\src --max-depth 2 --directories-only
```

`--files-only` and `--directories-only` cannot be combined.

An accept resolver can decide whether Enter completes NFM or transitions the
same window to a new filesystem picker. The resolver is an executable followed
by repeatable `--accept-resolver-arg` arguments. This is compatibility syntax
for a generated action resolver bound to Enter, and it receives the same
`NFM_ACTION_STATE` JSON document as other actions:

```powershell
nfm-rust-win32host filesystem `
    --accept-resolver pwsh `
    --accept-resolver-arg '-NoProfile' `
    --accept-resolver-arg '-Command' `
    --accept-resolver-arg '$state = $env:NFM_ACTION_STATE | ConvertFrom-Json; if (Test-Path -LiteralPath $state.selection.item -PathType Container) { @{ action = "picker"; picker = @{ kind = "filewalker"; roots = @("{item}") } } | ConvertTo-Json -Compress } else { @{ action = "complete" } | ConvertTo-Json -Compress }'
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
Literal roots are also supported. A filesystem transition keeps the original
output request open, and Escape cancels the whole NFM session.

Named action resolvers can be assigned to key chords. Each invocation receives
one JSON document in `NFM_ACTION_STATE`, containing the current selection,
picker context, and query. This example binds Alt+Up to the parent of the
current filesystem root:

```powershell
nfm-rust-win32host filesystem G:\src `
    --action parent action-resolver `
    --action-program parent pwsh `
    --action-arg parent '-NoProfile' `
    --action-arg parent '-Command' `
    --action-arg parent '$state = $env:NFM_ACTION_STATE | ConvertFrom-Json; $roots = @($state.picker.roots); $parent = if ($roots.Count -eq 1) { Split-Path -Parent $roots[0] }; if ([string]::IsNullOrEmpty($parent)) { @{ action = "none" } | ConvertTo-Json -Compress } else { @{ action = "picker"; picker = @{ kind = "filewalker"; roots = @($parent) } } | ConvertTo-Json -Compress -Depth 4 }' `
    --bind alt+up parent
```

The state has this shape:

```json
{
  "selection": {
    "item": "G:\\src\\project",
    "value": "G:\\src\\project",
    "line": null
  },
  "picker": {
    "kind": "filewalker",
    "roots": ["G:\\src"]
  },
  "query": "project"
}
```

An action resolver currently returns either:

```json
{"action":"none"}
```

to make no change,

```json
{"action":"complete"}
```

to complete with the selection captured when the resolver started, or:

```json
{"action":"picker","picker":{"kind":"filewalker","roots":["G:\\src"]}}
```

`--action`, `--action-program`, and repeatable `--action-arg` options define a
named resolver. Repeatable `--bind <chord> <action>` options reference those
names. Supported chord names include characters, arrows, Enter, Escape, Home,
End, Delete, Backspace, PageUp, and PageDown with Ctrl, Alt, and Shift
modifiers. User bindings override built-in handling for the same chord.

On Windows, list the visible Alt-Tab application windows:

```text
nfm-rust-win32host windows
```

Rows contain the window handle, process ID, executable name, and title. The
selected row is written to stdout. Add a live, client-area DWM thumbnail of the
selected window with:

```text
nfm-rust-win32host windows
```

An explicitly supplied `--preview` command takes precedence over the native
window thumbnail.

On Windows, list running processes in a structured picker:

```text
nfm-rust-win32host processes
```

The picker displays Name, PID, working set, private bytes, and CPU time. It
supports the structured column expressions and autocomplete and writes the
selected row to stdout. The preview pane shows the selected process fields
without launching another process. It starts hidden; press Ctrl+P or pass
`--preview-visible true` to show it. Press Ctrl+K to terminate the selected
process, or Ctrl+R to refresh the process list.

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
nfm-rust-win32host filesystem `
    --preview pwsh `
    --preview-arg -NoProfile `
    --preview-arg -Command `
    --preview-arg 'Get-ChildItem -Force -LiteralPath $env:NFM_PREVIEW_ITEM | Out-String -Width 240'
```

To preview file contents:

```powershell
nfm-rust-win32host filesystem `
    --preview pwsh `
    --preview-arg -NoProfile `
    --preview-arg -Command `
    --preview-arg 'Get-Content -LiteralPath $env:NFM_PREVIEW_ITEM'
```

ANSI SGR colors and styling are rendered in the preview. Because preview output
is captured through a pipe rather than a terminal, commands must be told to
emit colors. For example, use `bat` with color forced and paging disabled:

```powershell
nfm-rust-win32host filesystem `
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
nfm-rust-win32host filesystem `
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
nfm-rust-win32host filesystem `
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
