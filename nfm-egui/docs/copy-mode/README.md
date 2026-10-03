# Using NFM copy mode

NFM provides a shared Vim-style copy-mode engine in `nfm_egui::copy_mode`.
The embedded picker uses it for text, ANSI, and host-backed scrollback previews.
Terminal hosts can use the same engine over a frozen terminal snapshot.

This guide covers both using copy mode and integrating it into a host. See
[SEARCH-API.md](../../SEARCH-API.md) for the search service and
[API.md](../../API.md) for picker and preview lifecycle contracts.

## Entering and leaving a preview

Open a picker with a text-capable preview, then press **Ctrl+W** to move focus
from the results/query pane into the preview. Press Ctrl+W again to return.
Focus changes retain the preview's current viewport; changing the selected
result or its document can replace the preview and reset copy state.

Copy mode navigates and copies preview text without accepting a picker result.
Yanking stays in preview focus. Escape first cancels pending input, selections,
or search highlights as applicable; once those are clear, it returns focus to
the results/query pane. Escape while editing a search cancels that search and
restores its original cursor and viewport.

A terminal application's entry binding is supplied by the host. For example,
Ghostty uses Ctrl+Shift+M; that binding is not part of the NFM engine itself.

## Moving through text

Prefix a motion with a count, such as `10j`, `3w`, or `2f:`.

| Keys | Action |
| --- | --- |
| `h`, `j`, `k`, `l`, arrows | Move left, down, up, right. |
| `0`, `^`, `$` | Logical-line start, first nonblank, last nonblank. |
| `w`, `b`, `e` | Next word, previous word, word end. |
| `W`, `B`, `E` | Equivalent motions for whitespace-delimited words. |
| `ge`, `gE` | End of the previous word or whitespace-delimited word. |
| `gg`, `G` | Beginning or end of the document. |
| `3gg`, `3G` | Third logical line, using one-based numbering. |
| `H`, `M`, `L` | Top, middle, bottom visible physical row. |
| PageUp, PageDown | Move by a viewport; Ctrl+U / Ctrl+D move half a viewport. |
| `+`, `-`, `_` | First nonblank on the next, previous, or current/count-selected line. |
| `f` / `F` + character | Jump to a character forward/backward in the logical line. |
| `t` / `T` + character | Jump just before/after that character. |
| `;`, `,` | Repeat or reverse the last character jump. |
| `%` | Matching bracket; `50%` goes halfway through the physical document rows. |
| Ctrl+E, Ctrl+Y | Scroll down/up by physical rows, with an optional count. |
| `zt`, `zz`, `zb` | Place the cursor row at the viewport top, center, or bottom. |

The document stores **physical rows** for rendering and cell coordinates.
Rows marked `wraps_to_next` are joined into one **logical line** for text
motions, vertical `j`/`k` navigation, character jumps, and linewise selections.
A hard line break keeps rows separate. Viewport and scrolling operations use
physical rows. Hosts must supply correct wrap information to obtain this behavior.

The cursor scroll margin defaults to two rows above and below, reduced for
short viewports. A margin of zero disables it. `zz` centers the cursor.

## Selecting and yanking

| Keys | Action |
| --- | --- |
| `v` | Toggle characterwise visual selection. |
| `V` | Toggle complete logical-line selection, including soft wraps. |
| Ctrl+Q | Toggle a rectangular selection over physical rows and columns. |
| `o`, `O` | Swap selection endpoints; `O` switches horizontal corners in a rectangle. |
| `y` in visual mode | Copy the selected text and stay in copy mode. |
| `yy`, `2yy` | Copy one or two logical lines. |
| `y$` | Copy through the last nonblank cell of the logical line. |
| `yw`, `y3w`, `yj`, `ygg` | Yank through a motion, with optional counts. |
| `yf:`, `y;` | Yank through a character jump or its repeat. |
| `yiw`, `yaw`, `yiW`, `yaW` | Yank a word; `a` includes adjacent whitespace. |
| `yi(`, `ya"` | Yank inside delimiters/quotes, or include them. |
| Shift+Y | Select a hint and yank logical lines through that hint. |

Text objects support words, whitespace-delimited words, single/double quotes,
backticks, parentheses, brackets, braces, and angle brackets. In visual mode,
use `i` or `a` plus the object, such as `viw` or `va(`, then extend with motions
or press `y`. Repeating an object does not expand it exactly as Vim does.

`yic` / `yac` and `vic` / `vac` use command boundaries detected from prompt
patterns. These are useful for terminal output; ordinary previews need not have
meaningful command boundaries. The current shared prompt patterns are fixed in
the engine and text resembling a prompt can create a false boundary.

Linewise copies end with a newline. Rectangular copies produce one line for
each physical row. Unicode glyphs, wide cells, and combining marks follow the
document's cell mapping. Successful yanks flash the copied region for 250 ms;
a new yank replaces and restarts the flash. Full-width painting for linewise
selections and spans crossing rows is a decoration choice, not extra copied text.

## Searching and quick select

Use `/` for forward search or `?` for backward search. Matching and scrolling
update as you type. Literal search ignores case and treats regex punctuation
as text. Ctrl+R switches to case-sensitive regex; `(?i)` enables case-insensitive
matching within a regex. The search prompt hides the copy cursor and highlights
the full active match separately from the other matches.

Ctrl+N / Ctrl+P navigate while the prompt is open. Enter accepts the current
search position; `n` repeats the search direction and `N` reverses it afterward.
`*` / `#` search forward/backward for the word under the cursor. Match navigation
wraps at document boundaries. Search errors are shown rather than accepting
partial results. See the search API guide for cancellation and size limits.

Alt+Space from either results or preview focus enters Quick Select over visible
preview words. Type a hint label to move the cursor into preview copy mode.
PageUp / PageDown page hints; Backspace removes a label prefix. Escape or
Alt+Space cancels and restores the prior focus and viewport. Shift+Y uses the
same hints for a linewise yank and stays in copy mode afterward.

Terminal-specific behaviors such as pasting a hint into the shell, opening a
line-split picker, or entering copy mode from application keybindings belong
to the host. The NFM preview does not automatically implement every host action.

## Using the embedded picker

Attach a text/ANSI `PreviewProvider`, or a guarded `CopyDocument` for scrollback,
and call `Picker::show` inside your existing egui frame. The picker handles copy
input, selections, search, and decorations. Its clipboard output goes through
egui; the host must process platform output to place it on the system clipboard.

```rust
use nfm_egui::{Appearance, ItemsSource, Picker};
use nfm_egui::egui::Color32;

fn configure<S: ItemsSource + Send + Sync + 'static>(
    picker: &mut Picker<S>,
    appearance: &mut Appearance,
) where S::Item: Send {
    picker.set_cursor_scroll_padding(2);
    appearance.palette.copy_cursor_background = Some(Color32::from_rgb(80, 160, 240));
    appearance.palette.search_active_background = Some(Color32::from_rgb(180, 80, 40));
    appearance.palette.yank_background = Some(Color32::from_rgb(100, 150, 60));
}
```

The shared engine does not read Lua. Ghostty maps its Lua
`cursor_scroll_padding` setting to the terminal engine and picker. Other hosts
map their own configuration to the Rust APIs. Cursor, search-match, active-match,
and yank colors are independently configurable through `Appearance::palette`.

## Integrating the engine into a terminal or other host

Use `copy_controller::CopyController` to execute input. Ghostty and the NFM
preview use the same controller for motions, counts, text objects, visual and
operator yanks, search navigation, quick-select completion and viewport padding.
The lower-level `CopyMode` remains available for custom integrations.

1. Capture an immutable document and call `CopyMode::start` with its revision and
   physical viewport. Keep that mode alive for the session.
2. Supply a `CopyDocument`, which automatically implements
   `CopyNavigationDocument`, or a synchronous borrowed navigation adapter.
3. Call `mode.begin_input_batch()` once per batch, then create a short-lived
   controller with the mode, document and current `View` for each event. Call
   `input`; `execute` also accepts a decoded `CopyAction` for binding overrides.
4. Apply `CopyUpdate::viewport_top` together with cursor/selection decorations,
   honor `changed` and `repaint_after`, and process its host effects before the
   next event. `CopyText` already contains extracted text and yank metadata;
   the mode already retains its yank flash. No host motion/yank dispatch needed.
5. `BeginSearch` opens the host's search prompt. Suppress its trigger text when
   egui emits both a key and text event. `Search` submits a word-search request;
   pass accepted feedback to `finish_search`. An optional anchor supports live
   incremental searches without briefly resetting the displayed cursor.
6. `QuickSelect` opens hints; pass a completed hint to `select_quick` with its
   purpose. `OpenLineSplit` is an optional host picker action. `ClearSearch`
   cancels pending work when requested. `Exit` releases copy focus/snapshot.
7. Treat controller errors as failed operations. Stop using expired native
   documents. Cancel and wait for asynchronous reads before invalidating leases.

```rust
use nfm_egui::copy_controller::{CopyController, CopyUpdate};
use nfm_egui::copy_document::CopyDocument;
use nfm_egui::copy_mode::{CopyMode, View};
use nfm_egui::egui::Event;

fn handle_event(
    mode: &mut CopyMode,
    document: &dyn CopyDocument,
    view: View,
    event: &Event,
) -> Result<CopyUpdate, String> {
    CopyController::new(mode, document, view).input(event)
}
```

The controller borrows state and the document for synchronous execution. It
does not send borrowed native pointers to workers, access the system clipboard,
or draw UI. Hosts retain their search prompt widgets, native snapshot lifecycle,
quick-select rendering and application actions; matching and worker execution
remain in the shared `SearchService`.

### Using the lower-level engine

This minimal example evaluates a word motion against an ASCII document:

```rust
use nfm_egui::copy_mode::{
    CellPosition, CopyMode, Motion, MotionEvaluation, TextRow, View,
};

let read = |y| (y == 0).then(|| TextRow {
    text: "one two".into(),
    columns: (0..7).collect(), // One display column per byte for this ASCII row.
    wraps_to_next: false,
});
let mut mode = CopyMode::default();
let view = View {
    cursor: CellPosition { x: 0, y: 0 },
    columns: 7,
    total_rows: 1,
    viewport_top: 0,
    viewport_rows: 1,
};
mode.start(1, view);
mode.set_scroll_padding(2);

let evaluated = mode.evaluate_motion(
    Motion::ForwardWord, 1, false, Some(view), read,
    |cell, _snap_right| Some(cell), // Real hosts validate bounds and wide cells.
);
if let MotionEvaluation::Complete(result) = evaluated {
    mode.moved_with_rows(Motion::ForwardWord, result.target, read);
}
assert_eq!(mode.cursor, CellPosition { x: 4, y: 0 });

// A host implementing yy extracts this range as linewise text and copies it.
let range = mode.yank_lines_range(1, read).unwrap();
let now = std::time::Instant::now();
mode.highlight_yank(range, now);
assert_eq!(mode.yank_highlight_range(), Some(range));
```

`MotionEvaluation::Partial` reports progress made before a counted motion could
finish. Ordinary movement can keep that progress; yank operators can reject a
partial target. `InvalidSnapshot` means the host must stop using that document.
`NoMovement` means no target was reached. Use `evaluate_motion` for logical-line
behavior rather than treating low-level `move_to` as a complete text-motion API.

`TextRow::columns` needs one display column per UTF-8 byte, not per character.
Use actual physical cell coordinates and repeat them for multibyte/combining
glyph bytes. `wraps_to_next` must describe the captured layout, not inferred
word wrapping. Freeze metadata and coordinates during a copy session; if the
host changes the document layout, invalidate or restart that session.

For host-backed picker previews, `CopyDocument` supplies fixed metadata, owned
text rows, cell resolution, selection text, and viewports. Own the data or use
a lease which serializes invalidation with reads. An `Arc` around an unguarded
native pointer is insufficient. See [copy_document.rs](../../src/copy_document.rs).

For asynchronous search, submit a `SearchDocument` to `SearchService`, retain
its ticket, validate document/query revisions, and feed matches into the copy
engine's search methods. NFM owns matching and worker scheduling; the host
owns cursor movement and painting. See [SEARCH-API.md](../../SEARCH-API.md).
