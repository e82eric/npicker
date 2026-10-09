# nfm-preview-file

Host-selected Text, Image and Command preview plans for NFM's egui picker.
Hosts supply text snapshots or worker readers; this crate does not read files,
list directories or classify binary files. File plans are no longer supported.

`resolved::ResolvedPreviewProvider` validates and dispatches plans, caches the
selected provider, forwards paging and exposes its copy document.
`nfm-preview-text` handles syntax highlighting, cell layout and text copy/search.
Commands use cancellable, bounded execution and produce Text, Ansi or Image output.
Images decode on the worker, preserve aspect ratio and have no text copy document.
Image dimensions are bounded to 8192 pixels and decoded RGBA data to 128 MiB.
Paging and repainting reuse the cached result rather than rerunning commands.
