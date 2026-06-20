# nfm Rust Win32 Host Prototype

This is a filewalker-only Rust port of the Win32 picker host. It is intentionally
separate from the C# projects so the storage/search/UI design can evolve without
disturbing the current implementation.

Goals:

- `nfm.win32.picker.v1` named pipe compatibility.
- File-system picker requests only.
- Compact UTF-8 file store with reverse-linked nodes, name interning, chunked
  backing storage, and published snapshots for streaming search.
- Parallel top-K fuzzy search over published snapshots.
- `nucleo-matcher` fuzzy scoring.
- Direct2D/DirectWrite list UI with no preview pane.

The crate is structured around the same runtime boundaries as the C# path:

- `ipc`: line-delimited JSON named pipe server.
- `store`: compact UTF-8 node/name storage and published snapshots.
- `walker`: background file-system scanner.
- `search`: parallel fuzzy search over a published snapshot.
- `view_model`: owns scan/search state and UI events.
- `d2d_ui`: minimal D2D picker window.

