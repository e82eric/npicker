# nfm-preview-text

`TextPreviewRequest::extension` supplies an optional Auto syntax hint (for example
`cs` or `.cs`). Filename-specific rules, extension hints, filename extensions, and
first-line detection select the syntax in that order. Explicit syntax choices override
these hints. The renderer never reads the original file to infer its type.

Documents that exceed the rendering limits retain the renderable prefix and
end with a copyable truncation warning. Limits are 16,384 cells per row,
1,000,000 cells per document, and 100,000 rows, including the warning.

Source-independent syntax highlighting, cell layout, paging, and copy documents
for NFM's egui picker. This crate never opens a path or classifies a file.

Hosts supply `TextPreviewRequest` with a `TextSource`, syntax, optional filename
hint, initial center/highlight lines, and `TextPreviewOptions` with theme and byte limit. A source returns a consistent, complete UTF-8 snapshot
on NFM's worker. Rendering reads from the beginning to preserve multiline syntax
state; scrolling/search/copy then reuse the immutable document.

```rust
use nfm_preview_text::{TextContent, TextPreviewOptions, TextPreviewProvider,
    TextPreviewRequest, SyntaxChoice};
use std::sync::Arc;

let mut request = TextPreviewRequest::snapshot("fn main() {}\n");
request.syntax = Some(SyntaxChoice::Named("Rust".into()));
let provider = TextPreviewProvider::new(TextPreviewOptions {
}, move |_: &nfm_egui::Selection<String>| Ok(Some(request.clone())))?;
```

For content held by another component, use `TextPreviewRequest::new(Arc::new(
reader))`. The reader signature is
`Fn(usize, &dyn Fn() -> bool) -> Result<TextContent, String>`; the arguments are
the byte limit and cancellation check. It must bound its own reads, check
cancellation, and return a stable snapshot. Reader errors reach the preview.
`TextContent::plain` forces plain rendering for generated listings/metadata.

Tabs display as independently navigable spaces while clipboard extraction
preserves tabs. Unicode graphemes and wide cells retain byte-to-cell mappings.
CRLF normalizes to LF.

File reading, directory listing and binary detection belong to hosts.
Hosts supply snapshots or their own TextSource worker readers.
