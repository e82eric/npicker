# nfm-preview

File-type routing for path previews. `resolver(MediaPreviewSettings)` returns a
`FilePreviewResolver` that picks a `nfm_preview_file` plan for each path:

- Images: the first frame from `ffmpeg`, as PNG.
- Videos: a frame at `video_position` (default a quarter of the way through),
  located with `ffprobe`.
- Directories: a sorted entry list, with `/` after subdirectories.
- Everything else: the file's UTF-8 text, syntax-highlighted in process with
  `nfm-preview-text`, or a "Binary file" notice.

Frames larger than 1280×1280 are scaled down to fit. `MediaPreviewSettings::files`
sets the highlighting theme and syntax, and `max_bytes` bounds both text and
ffmpeg output. The resolver runs on the preview worker, so the ffprobe call never
blocks the UI. No window or GPU context is created.

`file_preview_provider()` wraps the default resolver as a
`PreviewProvider<String>`. `path_preview_provider(resolver)` does the same for
any resolver, including a host's own. Filesystem pickers install the default
provider, and hosts can replace it with `set_preview_provider`.

```text
cargo test -p nfm-preview -- --include-ignored   # needs ffmpeg and ffprobe
```
