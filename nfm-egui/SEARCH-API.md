# Public document search API

The API lives in `nfm_egui::search_service` and `nfm_egui::document_search`.
Each public method has mechanics, lifetime/error details, and a runnable example
in its rustdoc. Build the reference with `cargo doc --manifest-path
nfm-egui/Cargo.toml --no-deps`, or read the source docs linked below.

## Responsibilities

| API | Role |
| --- | --- |
| `SearchDocument::visit_text` | Supplies immutable UTF-8 chunks and display-cell mappings; checks cancellation and guards native access. |
| `SearchService` | Runs/reuses a worker, supersedes queued queries, filters stale feedback, and wakes the host. |
| `SearchTicket` | Keeps one query active; dropping it cancels it. |
| `SearchResult` | Correlates query and document revisions with inclusive cell ranges or an error. |
| `LineScanner` | Matches logical lines incrementally across chunks and soft wraps. |
| `SearchSession` | Tracks copy-mode revisions and queues match navigation during pending work. |
| `CopyMode` search methods | Delegate navigation state to `SearchSession`, using the copy cursor. |

## Method reference

See [search_service.rs](src/search_service.rs) for `visit_text`, `cancellation`,
`new`, `set_limits`, `submit`, `cancel`, `cancel_and_wait`, `take_latest_result`,
and synchronous `search_document`.

See [document_search.rs](src/document_search.rs) for scanner `new`, `with_limits`,
`feed`, `finish_line`, `matches`, and `finish`; helpers `search_regex`,
`initial_match_index`, `navigate_match_index`, and `next_match_cell`; and session
`adopt_search`, `begin_search`, `cancel_pending_search`, `search_accepts_result`,
`search_matches`, `finish_search`, and `navigate_search`.

See [copy_mode.rs](src/copy_mode.rs) for the corresponding copy-mode delegates.

## End-to-end host flow

1. Implement `SearchDocument` over immutable data or a guarded native snapshot.
   Supply one display cell for every UTF-8 byte, repeating the cell for multibyte
   glyphs. Mark hard breaks with `line_end`; preserve soft wraps as one line.
2. Create `SearchService` with a short, thread-safe wake callback. The first
   submission starts its worker; subsequent submissions reuse it.
3. Submit the snapshot revision, document, query, and regex flag. Retain the
   returned ticket. Call `SearchSession::begin_search` with its query revision
   if copy-mode navigation should follow the matches.
4. On wake, call `take_latest_result`. Check document identity and query revision,
   handle any error, then pass the newest-first ranges to `finish_search`.
   Resolve the returned target and update your own cursor, viewport, and highlights.
5. A new submission cancels the previous query. Dropping a ticket or calling
   `cancel` cancels without waiting. Before freeing borrowed native resources,
   call `cancel_and_wait`, then close your own lease. Dropping the service alone
   does not join its worker.

The `take_latest_result` rustdoc contains a complete submission/wakeup/result
example with a working document implementation. `search_document` provides the
same matching synchronously for small in-memory previews.

Literal matching ignores case and escapes regex metacharacters. Regex matching
honors case unless inline flags override it. Matches are nonoverlapping,
zero-width matches are omitted, and endpoints are inclusive display cells.
Default limits are 4 MiB per logical line and 200,000 matches per document;
limit failures return errors instead of partial match lists.
