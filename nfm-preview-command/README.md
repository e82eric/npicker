# NFM preview command executor

This crate has no UI, native-window, Skia, VT, or external crate dependencies.
It runs an owned `CommandSpec` on the caller's worker and delivers stdout/stderr
through a bounded streaming mailbox. Arguments are passed directly, without a
shell; callers may explicitly select a shell executable. Working directory and
environment overrides are supported. Windows children use CREATE_NO_WINDOW.

`run(spec, cancelled, output)` returns captured bytes, the actual exit status,
and a truncation flag. Each stream is bounded by `spec.output_limit`; returning
false from the streaming callback also stops output as truncated. Cancellation,
read errors and early returns kill and reap the directly spawned child. Readers
own pipes only and stop when the mailbox closes; cancellation does not wait for
pipes inherited by descendants. Process-tree termination is not provided.

NFM's native command preview backend uses the streaming callback to maintain its
text/image limits and document styles. `nfm-egui::byte_preview::command_document`
uses captured stdout as a complete byte document, with optional initial center
row and accepted exit codes. Its BytePreviewProvider caches output across page
and size changes. Host Lua callbacks must resolve to owned specifications on
their owning thread before installation; Lua handles never enter the worker.


`run_streaming` shares the same execution and cancellation implementation but
does not retain stdout or impose an aggregate stdout byte limit. It retains
only `output_limit` stderr bytes while draining both streams completely. The
consumer receives every chunk and must bound records and its own storage.
`nfm-picker-sources::command` uses this path to publish searchable candidates
before a command completes, without applying preview output truncation.
