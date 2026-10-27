# NFM picker sources


## Streaming command sources

`command::start(CommandSourceOptions, StreamSink<T>, StreamControl)` starts a
host-independent worker using NFM's shared command executor. Pass an owned
`CommandSpec`, an existing `SnapshotStore<StreamingItemSnapshotWithPayload<T>>`,
and a copyable payload. Completed stdout records publish immediately while the
child is still running; total stdout is streamed, not buffered or capped at the
preview document limit. Use an append-only store when indices and payloads are
stable. UI input, config parameter substitution and application actions stay in
the host.

The source handles newline/CRLF and UTF-8 across pipe chunks, final unterminated
records, optional continuation suffixes, and optional deduplication by accepted
value. Continuation records keep a single-line searchable display but restore
newlines in the accepted value. Supply `StreamSink.values` when using this
feature: changed values are stored by source index before rows publish, so
selection and preview adapters can safely resolve the accepted value.

`StreamControl.changed` wakes the host after publications and completion;
`failed` reports errors. Set its cancellation flag before dropping the returned
worker handle. Workers own only their command, store and callbacks, and can be
cancelled without joining a host frame. Cancellation and failures both complete
the source. Failures preserve published results and retain bounded stderr for
diagnostics. Explicit success codes override defaults: zero normally, zero/one
for ripgrep. Individual physical lines and joined records default to a 4 MiB
limit; this does not restrict the total number of candidates. `CommandSpec`
output_limit controls retained stderr in streaming mode.

`StreamSink`, `StreamControl` and `StreamValues` live in `records`, which also
holds the line grouping shared by the command and file sources.

## Streaming file sources

`file::start(FileSourceOptions, StreamSink<T>, StreamControl)` streams the lines
of a text file with the same sink, control, continuation, deduplication and
record limits as a command source. Set `reverse` to read from the end, so the
newest lines of a history or log file publish first. Continuation groups keep
their line order and `unique` keeps the newest copy, so accepted values match a
forward read. The file length is read once when the worker starts.
