# nfm-win32

Windows enumeration, process actions, drive roots, typed window/process payloads,
process snapshots, and process-detail formatting. This crate owns no windows,
UI thread, renderer, or legacy picker interaction machinery.

SWM consumes this crate directly. The legacy host re-exports its types and helpers
under existing paths; it keeps legacy interaction adapters in its own package.
