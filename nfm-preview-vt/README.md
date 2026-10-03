# NFM Ghostty VT previews

This crate links the standalone Ghostty VT library, not the Ghostty terminal
application or renderer. It provides owned terminal documents and viewport grids.
Default parsing limits input to 20 MiB and output to 1,048,576 cells; scrollback uses an
8192-line limit, subject to native page pruning; the terminal byte limit is disabled. Providers should bound their input and check cancellation
before and after parsing. The native parse call itself is not interruptible.

Build the standalone VT library from a compatible Ghostty checkout using
`zig build -Demit-lib-vt -Doptimize=ReleaseFast -Dvt-features=-kitty-graphics --prefix zig-out/nfm-vt`, then provide matching headers
and static library through these variables (paths below are placeholders):

Alternatively, set `GHOSTTY_ROOT` to the checkout; headers come from `include`.
Libraries come from `zig-out/nfm-vt/lib` when present, otherwise `zig-out/lib`.
The explicit directory variables override these. The recommended build disables
Kitty graphics to avoid Wuffs image-codec symbol collisions with Skia hosts.

```powershell
$env:NFM_GHOSTTY_VT_INCLUDE_DIR = 'C:\path\to\ghostty\include'
$env:NFM_GHOSTTY_VT_LIB_DIR = 'C:\path\to\ghostty\zig-out\nfm-vt\lib'
$env:NFM_GHOSTTY_VT_LIB_NAME = 'ghostty-vt-static'
cargo test -p nfm-preview-vt
cargo test -p nfm-egui
```

`NFM_GHOSTTY_VT_LIB_NAME` defaults to `ghostty-vt-static` (omit file extension,
`lib` prefix, and directory). Adjust for the artifact produced on your platform.
The host must use the same compatible headers/library and avoid linking a
second incompatible copy. This crate does not invoke the terminal build.

`nfm-egui` requires the standalone VT library in every build. Use `nfm_egui::preview::parse_ansi` for ANSI previews.
Source acquisition/pinning or distributing prebuilt VT artifacts is a later
packaging task; no machine-specific path is embedded in these manifests.

## Retained ANSI documents

`AnsiDocument::parse(bytes, columns, rows)` owns an immutable standalone terminal.
`viewport` reads styled cells; `text_row` reads any retained absolute row with
full graphemes, UTF-8 byte-to-cell mapping, cell widths, and soft-wrap metadata.
`selection_text` extracts characterwise, linewise, or rectangular plain text and
rejects selections from another document revision. Endpoints are inclusive.
Selecting either half of a wide glyph includes the full glyph.

Raw ANSI input retains VT newline semantics. `BytePreviewProvider` normalizes
bare LF to CRLF for line-oriented documents. The existing `parse` function is a
single-viewport compatibility wrapper. Parsing remains bounded to 20 MiB input
and 1,048,576 initial screen cells, with the existing 8192-line scrollback policy
and native page/byte limits; `total_rows` describes retained rows, not necessarily
every line in the original input.

Documents are movable between threads but not concurrently readable: row and
viewport operations require mutable access. No native row references escape.
The provider caches a complete parsed document across scrolling and changes in
viewport height. Width changes reparse with a new revision. Its
`freeze_ansi_width(version, item_index, true)` pins layout during copy mode;
unfreezing permits reparsing on the next preview request. Use
`with_ansi_document(version, item_index, callback)` for row access; do not call
the provider recursively from this callback. Paged sources retain their current
page-reader contract and do not yet expose complete document semantics.

`nfm-egui::byte_preview` reexports `AnsiDocument`, `TextRow`, `SelectionKind`,
and `DocumentSelection` for hosts. The copy-mode state machine and preview input
bindings are a separate integration step; this change supplies their document
interface and extraction semantics.

Use `AnsiDocument::parse_with_limits` to supply an input byte limit and a
physical scrollback line limit. `parse` retains the default limits. NFM command
providers allow twice their captured output limit for CRLF normalization.
