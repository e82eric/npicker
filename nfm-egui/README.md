# NFM embedded egui control

Preview selections preserve syntax and ANSI foreground colors and use the warm
gray Gruvbox `bg3` (`#665c54`) `Appearance.palette.preview_selection_background` fill. This is separate
from picker-row `selection_background`/`selection_text` styling. Copy cursors,
active search matches, and yank feedback retain their contrasting overlays.

See [API.md](API.md) for the public API reference, host contracts, and preview lifecycle.
See [Using copy mode](docs/copy-mode/README.md) for preview controls, keybindings, configuration, and host integration.

`nfm-egui` is the shared picker UI for the terminal and SimpleWindowManager.
It owns picker state and widgets, not a native window, event loop, OpenGL context,
clipboard, application actions, or the terminal itself. The existing NFM Skia
host remains available alongside SimpleWindowManager's native Win32 egui host.

The default layout preserves the terminal's bottom query, with rank zero directly
above it and previews above the results. `Layout::QueryTop` provides a top query
and ascending results. The panel uses seven fixed slots by default, rounded
corners, an accent selection marker, Ghostty separators, and a frameless `>`
query/count row. There is no separate title, status, or loading-text row. Bounds and DPI can change each
frame; previews shrink or disappear before consuming the query's available space.

## Embedding in an existing egui host

The host uses the same egui version as this crate (currently 0.36.2), owns the
context, and calls `Picker::show` inside its existing frame. It handles returned
selection and clipboard events. Use a distinct ID for each simultaneous picker.

```rust
use std::sync::Arc;
use nfm_egui::{egui, Appearance, Picker, PickerConfig, PickerEvent, Placement};
use nfm_search_core::{snapshot_store::SnapshotStore, store::FlatSnapshot};

let ctx = egui::Context::default();
let source = Arc::new(SnapshotStore::new());
source.publish(Arc::new(FlatSnapshot::from_items([
    ("First window", 101_u64),
    ("Second window", 202_u64),
])));
source.complete();
let mut picker = Picker::new("windows", &ctx, source, PickerConfig::default());
let appearance = Appearance::default();

// Inside the host frame. Reuse this context and picker on subsequent frames.
let frame = ctx.run_ui(egui::RawInput::default(), |ui| {
    let ctx = ui.ctx();
    let output = picker.show(ctx, Placement::new(ctx.content_rect()), &appearance, false);
    match output.event {
        Some(PickerEvent::Accepted(selection)) => {
            // Host focuses the window identified by selection.item.
            assert!(selection.item == 101 || selection.item == 202);
        }
        Some(PickerEvent::CopyRequested(selection)) => ctx.copy_text(selection.text),
        Some(PickerEvent::Cancelled) | Some(PickerEvent::CustomAction { .. }) | None => {}
    }
});
// A real host paints this output with its existing egui renderer.
frame.drop_without_applying_deltas();
```

Acceptance and cancellation close the control and stop its workers. Drop and
recreate it to open a new session. Copy leaves it open. Handle events inside the
UI callback: egui may run multiple passes, and a terminal event is emitted once.
The host also handles input routing, IME, platform output, repaint wakeups and
buffer presentation. The context's repaint callback wakes the host on background
search/preview updates; `PickerOutput` reports busy state and keyboard ownership.

## Sources and lifetime

Any existing `ItemsSource + Send + Sync` snapshot with a `Send` item payload and
`SearchSnapshotProvider` can be used, including `SnapshotStore`, structured
streaming snapshots, and filesystem stores. A provider can be a concrete type
or an `Arc<dyn SearchSnapshotProvider<S>>`. The control uses the shared NFM search
incremental `FuzzySearchSession` and its `SearchSortMode`; effective queries, display text
and completions are respected.

Publish an empty snapshot for an empty completed source. Snapshots are immutable
once published and must have different versions when contents or indices change.
Scanning/search cancellation belongs to the host's source adapter; closing the
control cancels its own search and preview work, not another consumer's scanner.

Search results pin the exact snapshot searched, rather than resolving indices
against whatever snapshot the provider publishes next. `Selection` contains an
owned item, text, source index/version, and an owned snapshot reference. Preview
requests and accepted events retain that reference. A host adapter can call
`selection.source_snapshot::<MySnapshot>()` to retrieve the exact typed `Arc`.
Release events after handling them to release the snapshot.

For Ghostty, publish scrollback candidates and range payloads through this store
boundary. The snapshot adapter owns the frozen terminal data/handle. A preview
provider reads styled rows from the request's retained snapshot, and Ghostty
interprets accepted range payloads. NFM does not call terminal application FFI or
hold a borrowed terminal host pointer. Avoid reference cycles: snapshots should
not own the picker that retains them.

Query changes invalidate acceptance immediately; late search results are ignored.
Previously presented rows remain visible and use the query that produced them
until a current update arrives. The query row displays pending/streaming status.
Search/preview mailboxes coalesce updates instead of accumulating them. Search
uses the same incremental session as Ghostty. For append-only streams, construct
`SnapshotStore::new_append_only()` (or implement the provider's `is_append_only`
contract): every publication must preserve existing indices, searchable text and
payloads. This searches only appended candidates and retains the selected item
and its preview across unrelated source publications. Query extensions can use
the search engine's match cache. Keep `SnapshotStore::new()` for sources that
replace or reorder their items; those updates are fully searched safely.

## Previews

Install an `Arc<dyn PreviewProvider<Item>>` using `set_preview_provider`.
Closures implement the trait too. Requests contain the owned selection, viewport
columns/rows, document scroll offset, and default foreground/background colors.
Return `Preview::Text`, `Preview::Grid`, or `Preview::Empty`, or return an error
string for presentation. Grid cells support Unicode graphemes, backgrounds,
bold/italic, underline, strike, inverse, invisible and faint attributes.

A single preview worker processes the latest pending request. New selection,
viewport, appearance or scrolling requests cancel the previous generation and
ignore its late result. Previously presented preview content and its scroll
position remain visible until the replacement is ready. Its viewport reserves
the same row count when empty or loading. First presentation waits for initial
content for at most 100 ms, and sizing changes resolve before presentation.
The provider must check `Cancellation` during long work.
If it launches a process, it owns cancellation, output limits, and process cleanup.
Workers retain only owned data. Closing or dropping a control cancels them without
joining on the UI thread; a provider that ignores cancellation can continue until
its work returns, retaining its owned snapshot safely.

Use `preview::parse_ansi(bytes, &request)` for command/ANSI
previews. This links only the standalone VT library via `nfm-preview-vt`; see
[the VT build contract](../nfm-preview-vt/README.md). Raw ANSI is not interpreted
by `Preview::Text`. Native window thumbnails and application-specific preview
content are supplied by host adapters, not a second NFM-owned popup window.

## Appearance and placement

Pass `Appearance` each frame. It contains an instance palette and normal/bold
font IDs, logical cell width, and row height. Colors do not come from global
terminal state. Hosts can use distinct themes for distinct controls.

Copy exported font bytes and face indices into `FontFace`, register them into the
host's existing `FontDefinitions`, then call `Context::set_fonts`. Registration
adds a family without removing existing host fonts. Do this when fonts change,
not every frame. Choose font IDs/sizes and provide cell metrics; `with_cell_metrics`
converts physical cell sizes into logical points using the current DPI.

`Placement` fills the supplied bounds horizontally by default; an explicit width
can constrain it. It takes logical bounds, width, margin, top/center/bottom/cursor placement,
and an optional logical anchor point. For terminal cursor placement, pass the
point just below the cursor cell. Update bounds, anchor and physical-to-logical
metrics whenever the host resizes or moves across monitors. The UI clips to the
provided bounds. Extremely small bounds still require enough room for an input.

## Host-owned OpenGL

Hosts with an existing egui painter should reuse it. Do not create a second font
atlas or painter for this control.

Enable `opengl` for `renderer::Renderer` when the host needs an egui painter:

1. Create/make current the host GL context and pass its `Arc<glow::Context>` to
   `Renderer::new`. Keep the context alive through renderer destruction.
2. Run the host egui frame with physical size and pixels-per-point input.
3. Bind the host target framebuffer, then call `Renderer::render` with the full
   framebuffer size in pixels and the complete frame output. Consume every
   texture delta, including frames where the picker is closed.
4. Handle the frame's platform output and viewport commands in the host, then
   swap/present using the host's window integration.
5. Call `Renderer::destroy` exactly once with the context current before context
   destruction. The wrapper does not create or make current a context.

Rendering paints into the currently bound framebuffer without clearing or
swapping. egui_glow changes viewport, scissor, blend, buffers, texture bindings
and program state. Render it last or reestablish the host's GL state afterward.
Graphics resources can be recreated without recreating picker/search state.

## Keyboard and mouse

- Enter accepts; Escape cancels; double-click accepts a result.
- Up/Down navigate in visual order; Ctrl+N advances search rank.
- PageUp/PageDown move a result page; Ctrl+Home/End select first/last search rank.
- Ctrl+C requests copying the selected candidate from the host.
- Ctrl+P (as in Ghostty) or Ctrl+Space toggles the preview; Ctrl+PageUp/PageDown scroll its document.
- Tab applies the first source completion at the query caret. The query displays
  no additional completion row, matching the Ghostty panel. Hosts can also use `completions` and `apply_completion`.

## Demo and checks

The separate `nfm-egui-demo` package is the example host: eframe owns its window, input and GL context. The shared `nfm-egui` crate has no eframe or winit dependency, including optional features.
It shows 5000 candidates with owned payloads, previews, placement changes,
accept/cancel/copy events and adjustable row height.

```powershell
cargo run -p nfm-egui-demo
Get-ChildItem G:\src -Recurse -File | ForEach-Object FullName |
    cargo run -p nfm-egui-demo -- --stdin
cargo test -p nfm-egui
cargo check -p nfm-egui --features opengl
```

With the standalone VT environment variables configured:

```powershell
cargo run -p nfm-egui-demo 
cargo test -p nfm-egui -p nfm-preview-vt --all-features
```

The default control needs no native VT build. Demo window/input dependencies live
only in the separate demo package and do not enter shared-control builds.
Existing Skia host behavior remains unchanged.

For an OpenGL screenshot after the stream settles, pass `--screenshot target/picker.png`.
The demo captures its framebuffer and closes automatically.

Styled text providers may return `Preview::StyledText`, preserving foreground,
background and ANSI text decorations while the control handles line scrolling.
`Preview::Image` takes an `Arc<preview::PreviewImage>` containing a decoded egui
ColorImage; the control fits it to the preview area and uploads its texture once.
The image document should be used with the egui context that paints it.

On Windows, the parent `nfm-rust-win32host` package's optional `egui-previews`
feature exposes `preview::egui::file_preview_provider()`. This adapts the same
bat/ffmpeg/ffprobe resolver and cancellable command backend used by the Skia file
menu. Hosts do not need to duplicate media detection or ANSI parsing.
Structured queries use the same parser and source completions as the Skia picker:
`/:Column==value` filters (also `!=`, `=~`, `!~`, `>`, `>=`, `<`, `<=`),
`/!Ascending==Column` or `/!Descending==Column` sorts, and
`/#Display==Column,OtherColumn` selects/reorders displayed columns. Quoted values,
comma-separated alternatives, repeated expressions and fuzzy text can be combined.
These expressions are available on structured sources, such as processes.

Typing `/` opens the Skia-style six-row completion popup at the active token.
Up/Down selects a suggestion, Enter or Tab applies it, and Escape dismisses the
popup before closing the picker. Suggestions include actions, columns, operators
and values, with match highlighting and mouse selection. Completion retains the
surrounding query and cursor position. Headers and row display use the source's
selected columns; header text is yellow with a gold separator.
Keyboard parity: Ctrl+C requests copying the selected item; Preview copying requires Ctrl+W preview focus; Ctrl+Shift+C then copies the complete document. Results-pane copy commands apply to the selected item. Ctrl+Shift+/ opens a keyboard help overlay; Up/Down, Page Up/Down, Home/End scroll it, and Escape closes help without cancelling the picker. Hosts can add their action descriptions with `set_keybinding_help` and check `keybinding_help_visible` before consuming custom keys.

Hosts can call `set_external_preview(true)` to reserve preview space without a
preview worker. `PickerOutput.preview_rect` supplies the clipped bounds in egui
points, and is absent when preview is hidden, help is open, or the picker closes.
The host owns native preview handles and must hide/release them when absent.

`show(ctx, placement, appearance, disabled)` lets the host mark an inactive pane
as disabled. Pass `true` to paint without taking focus or consuming input; pass
`false` for an interactive picker. Disabled controls use the middle layer and
disabled result widgets, preserving the previous background rendering behavior. Hosts with their own preview workers can call
`set_external_preview_document(document, loading)` each frame to use NFM’s shared
preview painter and copy action while retaining snapshot ownership and paging.


### Complete bytes and paged host snapshots

Install `byte_preview::BytePreviewProvider` with
`set_preview_provider`. Its resolver receives an owned `Selection` and a
`Cancellation` token on the preview worker. It returns a `ByteDocument`:

- `ByteDocument::complete(bytes)` retains the entire ANSI document in an
  `Arc<[u8]>`; NFM parses requested pages without calling the resolver again.
- `ByteSource::Paged { columns, total_rows, read }` retains immutable snapshot
  metadata and an owned callback. NFM calls `read(PageRequest, Cancellation)`
  for only the requested physical rows. Return `BytePage { bytes, first_row }`;
  the actual origin may differ from the request. Each page must establish its
  own ANSI styles and preserve the source's physical row boundaries.

`ByteDocument.center_row` centers the initial page on a selected row.
`highlight` uses inclusive document row/column coordinates. NFM owns half-page
keyboard scrolling, clamping, parsing, highlight application and clipping.
Paged previews parse at the original source width before clipping to the UI;
resizing cannot reflow terminal coordinates. LF-only input is normalized to
CRLF. Input and parser allocation limits remain those of `nfm-preview-vt`.
Complete documents can reflow at the requested UI width.

The resolver is cached by selected source index, text and version. Append-only
sources use version zero for previews because existing payloads cannot change.
Replace the provider to invalidate a changed document. Pending work retains the
last presented page; cancellation and generation checks prevent stale results
from replacing it. Existing `PreviewProvider` implementations retain their
scrolling behavior; byte providers use half-page steps.

Callbacks must own their data or an explicitly closable host lease. Cancelling
does not join a thread or extend a native pointer's lifetime. Before releasing
a native snapshot, close its lease: block new reads and wait for active reads
to finish. Subsequent parsing uses owned bytes and requires no host handle.
Ghostty's `SnapshotPreview` implements this contract with a mutex around reads
and closure, so a detached NFM worker cannot access a released terminal host.


Command previews can return `command_document(&CommandSpec, center_row,
success_exit_codes, cancel)` from a `BytePreviewProvider` resolver. Execution
uses the shared `nfm-preview-command` crate on the existing preview worker;
output is retained as a complete document, so paging and resizing do not spawn
the command again. Errors preserve the executable, working directory, exit
status and bounded stderr. Truncated output is shown as a bounded preview.
When a host replaces a resolved command provider, `set_preview_provider`
cancels the old worker and retains its displayed document until the new one is
ready, resetting the new document's requested offset. Resolve host-thread-only
callbacks before installing the provider, and reject requests for a selection
that has not yet had its callback resolved.
## Preview copy mode

Document motions use shared logical-line traversal over `TextRow.wraps_to_next`.
Only soft-wrapped physical rows are joined. `h/l` cross soft wraps, `j/k` count
logical lines and preserve the logical column through short lines, and `0/^/$`,
`+/-/Enter`, character jumps, and counted `gg/G` use the same line boundaries.
Page/viewport motions remain based on screen rows. Plain-text previews report
hard breaks, so ordinary preview lines stay independent. Hosts evaluate motions
with `CopyMode::evaluate_motion` and commit them with `moved_with_rows`; the
coordinate-only `move_to` primitive does not supply document semantics.


Press **Ctrl+W** to switch between the picker query and a text preview. While
the preview has focus, input goes to the copy-mode cursor and leaves the query
and selected result unchanged. Ctrl+W returns to the query. Escape cancels a
pending command, visual selection, or search first, then returns query focus.

The preview and Ghostty terminal share the engine exported as
`nfm_egui::copy_mode`.

- `h/j/k/l` or arrow keys move the cursor; counts, word motions, `gg`, `G`,
  page motions, `f/t`, bracket matching, and `z` viewport positioning work.
- `v`, Shift+V, and Ctrl+Q select characters, lines, and rectangles.
- `y` copies a visual selection; `yy`, `yw`, `yiw`, and other yank motions
  or text objects copy without exiting preview focus. Ctrl+C copies the
  selection or current logical line; Ctrl+Shift+C copies the document.
- `/` and `?` enter a forward/backward regex search that highlights and scrolls
  as you type, paste, or delete text. Enter accepts the current search without
  advancing again; Escape restores the previous cursor, viewport, and search.
  Empty or invalid patterns clear live matches and restore the starting position.
  `n` and
  Shift+N navigate matches. Word searches also use the copied engine bindings.
- Ctrl+P hides the preview and returns to the query.

Ghostty VT is a required dependency. Plain and styled text previews and complete
ANSI byte previews use a retained document adapter exposed by
`PreviewProvider::copy_document`, preserving full graphemes, wide-cell widths,
and soft wraps. Layout stays fixed during preview focus, including resize;
returning to the query allows the next preview request to reparse at the new
width. `Picker::preview_focused()` lets hosts inspect focus.

Images and cell-only/paged previews without a copy document handle do not take
copy-mode focus. Ghostty's host-specific line-split and hint-picker actions
remain host integration points in the engine rather than preview shortcuts.

### Host-backed copy documents

`PreviewProvider::copy_document(version, index)` returns
`Result<Option<Arc<dyn copy_document::CopyDocument>>, String>` with no feature flags. The handle must match the displayed item and version.
`CopyDocument` supplies fixed revision/row/column metadata, physical text rows
with byte-to-cell mappings and widths, cell resolution, selection extraction,
and an exact styled viewport. No full-history byte export is required.

Implementations must own their data or hold a closable lease whose closure
serializes with active reads. Reads return owned data and fail after closure;
metadata changes invalidate preview focus. Ghostty's scrollback provider uses
its native snapshot APIs through such a lease. Its ANSI page parsing is only
for display; copy motions and extraction use original snapshot coordinates,
graphemes, and soft-wrap information.

Host-backed regex search runs on a cancellable worker and wakes the egui host
when ready. Leaving focus or replacing a search cancels it without joining the
UI thread. Search bounds each logical line to 4 MiB and matches to 200,000;
exceeding either bound reports a search error. Preview painting reads only
the visible physical rows.

Preview search opens with `/` or `?` and defaults to case-insensitive literal
matching. `Ctrl+R` toggles case-sensitive regex matching; inline `(?i)` enables
case-insensitive regex. `Ctrl+N` / `Ctrl+P` navigate matches while the prompt is
open; `n` / `N` navigate after accepting. Terminal and preview search share
the same `document_search::LineScanner`, pattern compiler, and match navigation. The scanner accepts UTF-8 chunks with a cell position per byte, preserves soft wraps, and returns ranges in newest-first order. `SearchSession` owns revision checking and navigation queued while results are pending; `CopyMode` delegates to it. Hosts retain document access, worker scheduling, and UI integration. Invalid regex is indicated in the preview prompt.

Quick Select uses the shared `quick_select` engine. `Alt+Space` from results
or preview focus labels visible preview words. Type a label to move the copy
cursor and stay in preview focus. `Escape` or `Alt+Space` cancels and restores
the previous focus and viewport. `PageUp` / `PageDown` page the hints, and
`Backspace` removes a typed prefix. `Shift+Y` in preview copy mode uses the
same hints to yank logical lines through the selected word. Labels respect
Unicode cell mappings and soft wraps. Ghostty retains terminal paste and native
decoration adapters; hint discovery and input are shared with NFM.

The cursor scroll margin defaults to two rows. Hosts can set it with
`Picker::set_cursor_scroll_padding(rows)`; zero disables the margin. The same
setting on `CopyMode` controls viewport positioning and scrolling. Ghostty
applies its top-level Lua `cursor_scroll_padding` value to both terminal copy
mode and NFM previews, including after config reload.

Host-backed preview search and terminal search use `search_service::SearchService`.
Hosts implement `SearchDocument::visit_text`, supplying UTF-8 chunks with a cell
position for each byte and a flag for hard line endings. This permits efficient
native batch reads as well as row-backed preview documents. `submit` receives the
document revision, document handle, query, and literal/regex mode; it returns a
`SearchTicket` which must stay alive until the result is accepted or cancelled.
`take_latest_result` returns correlated document/query revisions, cell ranges,
and any error. Dropping the ticket cancels its query; newer submissions supersede
older ones. The wake callback signals completion or worker failure to the host.

The worker starts on demand and is reused for subsequent host-document searches.
Small in-memory previews use the same `search_document` operation synchronously.
SearchService owns scheduling, cancellation, stale-result filtering, and recovery
from worker failure. Dropping it cancels without joining the UI thread. Native
hosts must call `cancel_and_wait` before destroying a borrowed snapshot resource;
its acknowledgement arrives only after all earlier worker document handles have
been released. Documents must own their data or guard their native lease, and
serialize invalidation with reads. A cancelled or closed document cannot publish
stale search results.

For the public search API, method mechanics, and runnable examples, see
[SEARCH-API.md](SEARCH-API.md) and the `search_service` / `document_search` rustdoc.

## Reusing a picker across source types

`nfm_egui::shared` provides `SharedPicker`, `provider`, and `replace` for hosts
that switch between different typed sources while retaining the same UI control.
Pass a typed snapshot provider to `shared::replace(ctx, source, config, previous)`;
no host item enum or mapping functions are required. Item payloads are owned by
`SharedItem` and can be read with `downcast_ref::<T>()` at the selection boundary.
Search plans, display/header semantics, completions, versions, append-only status,
and update subscriptions are delegated to the original source. Retained rows
remain visible during loading, while old selections cannot trigger actions.
Replacement clears the previous preview provider, external preview mode, help,
and bindings. Hosts set the new preview provider after replacement.

The NFM file, process, and window egui components expose `with_shared_picker`
constructors. Pass `into_picker()` from the previous workflow to switch types;
the specialized components supply their own shared adapters, previews, and actions.
Native windows, event loops, focus policy, and selection actions remain host-owned.
The generic sharing layer adds no native-enumeration or specialized-picker dependency.
