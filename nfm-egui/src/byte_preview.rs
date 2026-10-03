//! Byte-backed previews. Resolve one owned document per selected item. NFM
//! parses complete documents locally or requests bounded pages from the host.
use crate::preview::{PreviewViewport, parse_ansi};
use crate::{Cancellation, Preview, PreviewProvider, PreviewRequest, Selection};
use egui::Color32;
pub use nfm_preview_vt::{AnsiDocument, Selection as DocumentSelection, SelectionKind, TextRow};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRequest {
    pub first_row: usize,
    pub rows: u16,
    /// Original document width, independent of the displayed preview width.
    pub columns: u16,
}
pub struct BytePage {
    /// ANSI bytes for physical rows; each page must establish its own styles.
    pub bytes: Vec<u8>,
    pub first_row: usize,
}
pub type PageReader = dyn Fn(PageRequest, Cancellation) -> Result<BytePage, String> + Send + Sync;

pub enum ByteSource {
    Complete(Arc<[u8]>),
    /// Immutable snapshot metadata. The callback owns its data or an explicitly
    /// closable host lease; cancellation alone does not extend pointer lifetime.
    Paged {
        columns: u16,
        total_rows: usize,
        read: Arc<PageReader>,
    },
}
#[derive(Clone, Copy, Debug)]
pub struct CellPosition {
    pub row: usize,
    pub column: usize,
}
#[derive(Clone, Copy, Debug)]
pub struct Highlight {
    /// Inclusive document coordinates, before clipping to the preview width.
    pub start: CellPosition,
    pub end: CellPosition,
    pub background: Color32,
}
pub struct ByteDocument {
    pub source: ByteSource,
    pub center_row: Option<usize>,
    pub highlight: Option<Highlight>,
}
impl ByteDocument {
    pub fn complete(bytes: impl Into<Arc<[u8]>>) -> Self {
        Self {
            source: ByteSource::Complete(bytes.into()),
            center_row: None,
            highlight: None,
        }
    }
}
type Resolver<T> =
    dyn Fn(&Selection<T>, Cancellation) -> Result<ByteDocument, String> + Send + Sync;
type Key = (u64, usize, String);
struct Cached {
    key: Key,
    document: ByteDocument,
    initialized: bool,
    ansi: Option<Arc<Mutex<nfm_preview_vt::AnsiDocument>>>,
    frozen_columns: Option<u16>,
}

/// Install with `Picker::set_preview_provider`. The resolver runs on the NFM
/// worker, once per selection/version; scrolling and resizing reuse its bytes
/// or page reader. Replace the provider to invalidate a changed document.
pub struct BytePreviewProvider<T> {
    resolve: Arc<Resolver<T>>,
    cache: Mutex<Option<Cached>>,
    max_bytes: usize,
    max_scrollback_lines: usize,
}
impl<T> BytePreviewProvider<T> {
    /// Configure input and retained scrollback bounds before installing the provider.
    pub fn with_limits(mut self, max_bytes: usize, max_scrollback_lines: usize) -> Self {
        self.max_bytes = max_bytes;
        self.max_scrollback_lines = max_scrollback_lines;
        self
    }

    /// Read the currently cached complete ANSI document. The callback runs
    /// under the provider lock: do not call this provider recursively. Check
    /// both document version and item index to avoid reading a stale selection.
    /// Paged sources expose no retained document through this API.
    pub fn with_ansi_document<R>(
        &self,
        document_version: u64,
        selection_index: usize,
        read: impl FnOnce(&mut nfm_preview_vt::AnsiDocument) -> R,
    ) -> Result<Option<R>, String> {
        let mut cache = self.cache.lock().map_err(|_| "Preview cache poisoned")?;
        let Some(cached) = cache
            .as_mut()
            .filter(|c| c.key.0 == document_version && c.key.1 == selection_index)
        else {
            return Ok(None);
        };
        cached
            .ansi
            .as_ref()
            .map(|doc| {
                let mut doc = doc
                    .lock()
                    .map_err(|_| "ANSI document poisoned".to_owned())?;
                Ok(read(&mut doc))
            })
            .transpose()
    }

    /// Freeze parsed width during copy mode. Releasing the lock allows the
    /// next request to reparse at its display width with a fresh revision.
    pub fn freeze_ansi_width(
        &self,
        document_version: u64,
        selection_index: usize,
        frozen: bool,
    ) -> Result<bool, String> {
        let mut cache = self.cache.lock().map_err(|_| "Preview cache poisoned")?;
        let Some(cached) = cache
            .as_mut()
            .filter(|c| c.key.0 == document_version && c.key.1 == selection_index)
        else {
            return Ok(false);
        };
        let Some(ansi) = cached.ansi.as_ref() else {
            return Ok(false);
        };
        cached.frozen_columns =
            frozen.then_some(ansi.lock().map_err(|_| "ANSI document poisoned")?.columns());
        Ok(true)
    }

    pub fn new(
        resolve: impl Fn(&Selection<T>, Cancellation) -> Result<ByteDocument, String>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            resolve: Arc::new(resolve),
            cache: Mutex::new(None),
            max_bytes: 10 * 1024 * 1024,
            max_scrollback_lines: 8192,
        }
    }
}
fn normalize(bytes: &[u8], max_bytes: usize) -> Result<Vec<u8>, String> {
    if bytes.len() > max_bytes {
        return Err("ANSI preview exceeds configured byte limit".into());
    }
    let mut result = Vec::with_capacity(bytes.len());
    for (i, &byte) in bytes.iter().enumerate() {
        if byte == b'\n' && (i == 0 || bytes[i - 1] != b'\r') {
            result.push(b'\r');
        }
        result.push(byte);
    }
    Ok(result)
}
impl<T: Clone + Send + 'static> PreviewProvider<T> for BytePreviewProvider<T> {
    fn copy_document(
        &self,
        version: u64,
        index: usize,
    ) -> Result<Option<Arc<dyn crate::copy_document::CopyDocument>>, String> {
        let cache = self.cache.lock().map_err(|_| "Preview cache poisoned")?;
        Ok(cache
            .as_ref()
            .filter(|c| c.key.0 == version && c.key.1 == index)
            .and_then(|c| c.ansi.clone())
            .map(crate::preview_copy::ansi_copy_document)
            .transpose()?)
    }

    fn page_step(&self, rows: usize) -> usize {
        (rows / 2).max(1)
    }
    fn preview(&self, request: PreviewRequest<T>, cancel: Cancellation) -> Result<Preview, String> {
        self.preview_viewport(request, cancel).map(|p| p.document)
    }
    fn preview_viewport(
        &self,
        mut request: PreviewRequest<T>,
        cancel: Cancellation,
    ) -> Result<PreviewViewport, String> {
        if cancel.is_cancelled() {
            return Err("preview cancelled".into());
        }
        if request.columns == 0 || request.rows == 0 {
            return Err("Invalid preview dimensions".into());
        }
        let key = (
            request.document_version,
            request.selection.index,
            request.selection.text.clone(),
        );
        let mut cache = self.cache.lock().map_err(|_| "Preview cache poisoned")?;
        if cache.as_ref().is_none_or(|c| c.key != key) {
            let document = (self.resolve)(&request.selection, cancel.clone())?;
            if cancel.is_cancelled() {
                return Err("preview cancelled".into());
            }
            *cache = Some(Cached {
                key,
                document,
                initialized: false,
                ansi: None,
                frozen_columns: None,
            });
        }
        let cached = cache.as_mut().unwrap();
        let rows = usize::from(request.rows);
        let center = if cached.initialized && !request.initial_page {
            None
        } else {
            cached.document.center_row
        };
        let requested = center.map_or(request.scroll_offset, |row| row.saturating_sub(rows / 2));
        let display_columns = request.columns;
        let (mut document, first_row) = match &cached.document.source {
            ByteSource::Complete(bytes) => {
                let columns = cached.frozen_columns.unwrap_or(request.columns);
                if cached
                    .ansi
                    .as_ref()
                    .is_none_or(|doc| doc.lock().map_or(true, |doc| doc.columns() != columns))
                {
                    cached.ansi = Some(Arc::new(Mutex::new(
                        nfm_preview_vt::AnsiDocument::parse_with_limits(
                            &normalize(bytes, self.max_bytes)?,
                            columns,
                            request.rows,
                            self.max_bytes.saturating_mul(2),
                            self.max_scrollback_lines,
                        )
                        .map_err(|error| error.to_string())?,
                    )));
                }
                let mut ansi = cached
                    .ansi
                    .as_ref()
                    .unwrap()
                    .lock()
                    .map_err(|_| "ANSI document poisoned")?;
                let first = ansi.first_row(requested, request.rows);
                let rgb =
                    |c: Color32| ((c.r() as u32) << 16) | ((c.g() as u32) << 8) | c.b() as u32;
                let grid = ansi
                    .viewport(
                        first,
                        request.rows,
                        rgb(request.foreground),
                        rgb(request.background),
                    )
                    .map_err(|error| error.to_string())?;
                let document = crate::preview::preview_from_grid(grid);
                (document, first)
            }
            ByteSource::Paged {
                columns,
                total_rows,
                read,
            } => {
                if *columns == 0 {
                    return Err("Invalid source width".into());
                }
                let first = requested.min(total_rows.saturating_sub(rows));
                if *total_rows == 0 {
                    return Ok(PreviewViewport {
                        document: Preview::Empty,
                        first_row: 0,
                    });
                }
                let count = rows.min(total_rows.saturating_sub(first)) as u16;
                let page = read(
                    PageRequest {
                        first_row: first,
                        rows: count,
                        columns: *columns,
                    },
                    cancel.clone(),
                )?;
                if cancel.is_cancelled() {
                    return Err("preview cancelled".into());
                }
                if page.first_row >= *total_rows {
                    return Err("Preview page starts outside the document".into());
                }
                request.columns = *columns;
                request.scroll_offset = 0;
                let mut document = parse_ansi(&normalize(&page.bytes, self.max_bytes)?, &request)?;
                if let Preview::Grid {
                    total_rows: total, ..
                } = &mut document
                {
                    *total = *total_rows;
                }
                (document, page.first_row)
            }
        };
        if cancel.is_cancelled() {
            return Err("preview cancelled".into());
        }
        if let Preview::Grid { columns, cells, .. } = &mut document {
            let source_width = *columns;
            let width = source_width.min(usize::from(display_columns));
            let highlight = cached.document.highlight;
            *cells = cells
                .chunks(source_width)
                .enumerate()
                .flat_map(|(y, row)| {
                    row.iter().take(width).enumerate().map(move |(x, cell)| {
                        let mut cell = cell.clone();
                        if cell.inverse {
                            std::mem::swap(&mut cell.foreground, &mut cell.background);
                            cell.inverse = false;
                        }
                        let coordinate = (first_row.saturating_add(y), x);
                        if let Some(h) = highlight.filter(|h| {
                            coordinate >= (h.start.row, h.start.column)
                                && coordinate <= (h.end.row, h.end.column)
                        }) {
                            cell.background = h.background;
                        } else if cell.background == request.background {
                            cell.background = Color32::TRANSPARENT;
                        }
                        cell
                    })
                })
                .collect();
            *columns = width;
        }
        cached.initialized = true;
        Ok(PreviewViewport {
            document,
            first_row,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[test]
    fn normalization_uses_configured_input_limit_and_allows_crlf_expansion() {
        assert_eq!(normalize(b"a\nb\n", 4).unwrap(), b"a\r\nb\r\n");
        assert!(normalize(b"a\nb\n", 3).is_err());
        let bytes = vec![b'x'; 10 * 1024 * 1024 + 1];
        assert!(normalize(&bytes, bytes.len()).is_ok());
        assert!(normalize(&bytes, 10 * 1024 * 1024).is_err());
    }

    #[test]
    fn retained_document_pages_freezes_width_and_rejects_stale_items() {
        let provider = BytePreviewProvider::new(|_: &Selection<()>, _| {
            Ok(ByteDocument::complete(&b"abcdefghi\none\ntwo\nthree"[..]))
        });
        provider
            .preview(request(0, 0, 6, 2), Cancellation::test())
            .unwrap();
        let revision = provider
            .with_ansi_document(1, 0, |doc| {
                assert!(doc.text_row(0).unwrap().unwrap().wraps_to_next);
                doc.revision()
            })
            .unwrap()
            .unwrap();
        provider
            .preview(request(0, 2, 6, 3), Cancellation::test())
            .unwrap();
        assert_eq!(
            provider
                .with_ansi_document(1, 0, |doc| doc.revision())
                .unwrap(),
            Some(revision)
        );
        assert!(provider.freeze_ansi_width(1, 0, true).unwrap());
        provider
            .preview(request(0, 0, 12, 2), Cancellation::test())
            .unwrap();
        assert_eq!(
            provider
                .with_ansi_document(1, 0, |doc| (doc.columns(), doc.revision()))
                .unwrap(),
            Some((6, revision))
        );
        assert!(provider.with_ansi_document(2, 0, |_| ()).unwrap().is_none());
        assert!(provider.with_ansi_document(1, 1, |_| ()).unwrap().is_none());
        assert!(provider.freeze_ansi_width(1, 0, false).unwrap());
        provider
            .preview(request(0, 0, 12, 2), Cancellation::test())
            .unwrap();
        let next = provider
            .with_ansi_document(1, 0, |doc| {
                assert_eq!(doc.columns(), 12);
                doc.revision()
            })
            .unwrap()
            .unwrap();
        assert_ne!(next, revision);
        provider
            .preview(request(1, 0, 12, 2), Cancellation::test())
            .unwrap();
        assert!(provider.with_ansi_document(1, 0, |_| ()).unwrap().is_none());
    }
    fn request(index: usize, offset: usize, columns: u16, rows: u16) -> PreviewRequest<()> {
        PreviewRequest {
            document_version: 1,
            initial_page: false,
            selection: Selection {
                item: (),
                index,
                text: index.to_string(),
                source_version: 1,
                snapshot: Arc::new(()),
            },
            columns,
            rows,
            scroll_offset: offset,
            foreground: Color32::WHITE,
            background: Color32::BLACK,
        }
    }
    fn first_line(document: &Preview) -> String {
        let Preview::Grid { columns, cells, .. } = document else {
            panic!("not a grid")
        };
        cells
            .iter()
            .take(*columns)
            .map(|cell| cell.text.as_str())
            .collect::<String>()
            .trim_end()
            .into()
    }

    #[cfg(windows)]
    #[test]
    fn command_document_centers_and_pages_without_rerunning_the_command() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let provider = BytePreviewProvider::new(move |_: &Selection<()>, cancel| {
            count.fetch_add(1, Ordering::SeqCst);
            let mut spec = CommandSpec::new("cmd.exe");
            spec.arguments = vec![
                "/d".into(),
                "/c".into(),
                "echo A&& echo B&& echo C&& echo D&& echo E&& echo F&& <nul set /p =G & exit /b 0"
                    .into(),
            ];
            command_document(&spec, Some(4), &[0], cancel)
        });
        let page = provider
            .preview_viewport(request(0, 0, 8, 4), Cancellation::test())
            .unwrap();
        assert_eq!(page.first_row, 2);
        assert_eq!(first_line(&page.document), "C");
        let page = provider
            .preview_viewport(request(0, 0, 8, 4), Cancellation::test())
            .unwrap();
        assert_eq!(page.first_row, 0);
        assert_eq!(first_line(&page.document), "A");
        provider
            .preview(request(0, 3, 12, 4), Cancellation::test())
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    #[cfg(windows)]
    #[test]
    fn command_document_preserves_diagnostics_and_configured_success_codes() {
        let mut spec = CommandSpec::new("cmd.exe");
        spec.arguments = vec![
            "/d".into(),
            "/c".into(),
            "echo preview diagnostic 1>&2 & exit /b 2".into(),
        ];
        let error = command_document(&spec, None, &[0], Cancellation::test())
            .err()
            .unwrap();
        assert!(
            error.contains("preview diagnostic")
                && error.contains("cmd.exe")
                && error.contains("exited")
        );
        spec.arguments[2] = "exit /b 1".into();
        assert!(command_document(&spec, None, &[0, 1], Cancellation::test()).is_ok());
    }
    #[test]
    fn unpublished_initial_page_is_centered_again_after_a_superseding_resize() {
        let provider = BytePreviewProvider::new(|_: &Selection<()>, _| {
            Ok(ByteDocument {
                source: ByteSource::Paged {
                    columns: 80,
                    total_rows: 1000,
                    read: Arc::new(|page, _| {
                        Ok(BytePage {
                            bytes: b"page".to_vec(),
                            first_row: page.first_row,
                        })
                    }),
                },
                center_row: Some(800),
                highlight: None,
            })
        });
        let mut initial = request(0, 0, 40, 12);
        initial.initial_page = true;
        assert_eq!(
            provider
                .preview_viewport(initial, Cancellation::test())
                .unwrap()
                .first_row,
            794
        );
        // The worker completed, but its result was superseded before presentation.
        let mut resized = request(0, 0, 30, 10);
        resized.initial_page = true;
        assert_eq!(
            provider
                .preview_viewport(resized, Cancellation::test())
                .unwrap()
                .first_row,
            795
        );
        // Once presented, explicitly scrolling to row zero must stay at zero.
        assert_eq!(
            provider
                .preview_viewport(request(0, 0, 30, 10), Cancellation::test())
                .unwrap()
                .first_row,
            0
        );
    }
    #[test]
    fn complete_bytes_are_resolved_once_and_reused_for_pages_and_resize() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let provider = BytePreviewProvider::new(move |_: &Selection<()>, _| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(ByteDocument::complete(Vec::from(
                &b"zero\none\ntwo\nthree\nfour"[..],
            )))
        });
        let page = provider
            .preview_viewport(request(0, 0, 12, 2), Cancellation::test())
            .unwrap();
        assert_eq!(first_line(&page.document), "zero");
        let page = provider
            .preview_viewport(request(0, 2, 10, 2), Cancellation::test())
            .unwrap();
        assert_eq!(first_line(&page.document), "two");
        let page = provider
            .preview_viewport(request(0, usize::MAX, 10, 2), Cancellation::test())
            .unwrap();
        assert_eq!(page.first_row, 3);
        assert_eq!(first_line(&page.document), "three");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        provider
            .preview(request(1, 0, 10, 2), Cancellation::test())
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
    #[test]
    fn paged_source_centers_clamps_and_preserves_original_cell_coordinates() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let provider = BytePreviewProvider::new(move |_: &Selection<()>, _| {
            let seen = seen.clone();
            Ok(ByteDocument {
                source: ByteSource::Paged {
                    columns: 8,
                    total_rows: 1000,
                    read: Arc::new(move |page, _| {
                        seen.lock().unwrap().push(page);
                        Ok(BytePage {
                            bytes: b"\x1b[31mabcdef\r\nghijkl\r\nmnopqr\r\nstuvwx".to_vec(),
                            first_row: page.first_row,
                        })
                    }),
                },
                center_row: Some(800),
                highlight: Some(Highlight {
                    start: CellPosition {
                        row: 800,
                        column: 1,
                    },
                    end: CellPosition {
                        row: 800,
                        column: 2,
                    },
                    background: Color32::YELLOW,
                }),
            })
        });
        let page = provider
            .preview_viewport(request(0, 0, 3, 4), Cancellation::test())
            .unwrap();
        assert_eq!(page.first_row, 798);
        let Preview::Grid {
            columns,
            cells,
            total_rows,
        } = page.document
        else {
            panic!()
        };
        assert_eq!((columns, total_rows), (3, 1000));
        assert_eq!(cells[7].background, Color32::YELLOW);
        assert_eq!(cells[8].background, Color32::YELLOW);
        assert_eq!(cells[6].text, "m");
        let page = provider
            .preview_viewport(request(0, 9999, 3, 4), Cancellation::test())
            .unwrap();
        assert_eq!(page.first_row, 996);
        assert_eq!(
            *calls.lock().unwrap(),
            [
                PageRequest {
                    first_row: 798,
                    rows: 4,
                    columns: 8
                },
                PageRequest {
                    first_row: 996,
                    rows: 4,
                    columns: 8
                }
            ]
        );
        assert_eq!(provider.page_step(12), 6);
    }
    #[test]
    fn actual_page_origin_is_used_and_invalid_origins_are_rejected() {
        let provider = BytePreviewProvider::new(|_: &Selection<()>, _| {
            Ok(ByteDocument {
                source: ByteSource::Paged {
                    columns: 8,
                    total_rows: 10,
                    read: Arc::new(|_, _| {
                        Ok(BytePage {
                            bytes: b"offset".to_vec(),
                            first_row: 3,
                        })
                    }),
                },
                center_row: None,
                highlight: None,
            })
        });
        assert_eq!(
            provider
                .preview_viewport(request(0, 5, 8, 2), Cancellation::test())
                .unwrap()
                .first_row,
            3
        );
        let bad = BytePreviewProvider::new(|_: &Selection<()>, _| {
            Ok(ByteDocument {
                source: ByteSource::Paged {
                    columns: 8,
                    total_rows: 10,
                    read: Arc::new(|_, _| {
                        Ok(BytePage {
                            bytes: Vec::new(),
                            first_row: 10,
                        })
                    }),
                },
                center_row: None,
                highlight: None,
            })
        });
        assert!(
            bad.preview(request(0, 0, 8, 2), Cancellation::test())
                .is_err()
        );
    }
    #[test]
    fn cancellation_during_callback_does_not_publish_or_cache_a_partial_page() {
        let count = Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let provider = BytePreviewProvider::new(move |_: &Selection<()>, _| {
            let calls = calls.clone();
            Ok(ByteDocument {
                source: ByteSource::Paged {
                    columns: 8,
                    total_rows: 10,
                    read: Arc::new(move |page, cancel| {
                        if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                            cancel.cancel_for_test();
                        }
                        Ok(BytePage {
                            bytes: b"page".to_vec(),
                            first_row: page.first_row,
                        })
                    }),
                },
                center_row: Some(6),
                highlight: None,
            })
        });
        assert!(
            provider
                .preview(request(0, 0, 8, 2), Cancellation::test())
                .is_err()
        );
        let page = provider
            .preview_viewport(request(0, 0, 8, 2), Cancellation::test())
            .unwrap();
        assert_eq!(page.first_row, 5);
    }
    #[test]
    fn empty_sources_and_bad_dimensions_are_handled_without_host_reads() {
        let provider = BytePreviewProvider::new(|_: &Selection<()>, _| {
            Ok(ByteDocument {
                source: ByteSource::Paged {
                    columns: 8,
                    total_rows: 0,
                    read: Arc::new(|_, _| panic!("empty snapshot read")),
                },
                center_row: None,
                highlight: None,
            })
        });
        assert!(matches!(
            provider
                .preview(request(0, 0, 8, 2), Cancellation::test())
                .unwrap(),
            Preview::Empty
        ));
        assert!(
            provider
                .preview(request(0, 0, 0, 2), Cancellation::test())
                .is_err()
        );
        assert!(
            provider
                .preview(request(0, 0, 8, 0), Cancellation::test())
                .is_err()
        );
    }
    #[test]
    fn picker_uses_actual_center_for_keyboard_paging_and_preserves_it_on_resize() {
        use crate::{Appearance, Picker, PickerConfig, Placement};
        use nfm_search_core::{snapshot_store::SnapshotStore, store::FlatSnapshot};
        use std::time::{Duration, Instant};
        let ctx = egui::Context::default();
        let store = Arc::new(SnapshotStore::new());
        store.publish(Arc::new(FlatSnapshot::from_items([("item", ())])));
        store.complete();
        let reads = Arc::new(Mutex::new(Vec::new()));
        let seen = reads.clone();
        let provider = BytePreviewProvider::new(move |_: &Selection<()>, _| {
            let seen = seen.clone();
            Ok(ByteDocument {
                source: ByteSource::Paged {
                    columns: 80,
                    total_rows: 1000,
                    read: Arc::new(move |page, _| {
                        seen.lock().unwrap().push(page.first_row);
                        Ok(BytePage {
                            first_row: page.first_row,
                            bytes: b"preview".to_vec(),
                        })
                    }),
                },
                center_row: Some(800),
                highlight: None,
            })
        });
        let mut picker = Picker::new(
            "paged-test",
            &ctx,
            store,
            PickerConfig {
                preview_rows: 4,
                ..Default::default()
            },
        );
        picker.set_preview_provider(&ctx, Arc::new(provider));
        let mut run = |event: Option<egui::Event>, width: f32| {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut events: Vec<_> = event.into_iter().collect();
            loop {
                let mut busy = true;
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(width, 800.0),
                        )),
                        events: std::mem::take(&mut events),
                        ..Default::default()
                    },
                    |ui| {
                        busy = picker
                            .show(
                                ui.ctx(),
                                Placement::new(ui.ctx().content_rect()),
                                &Appearance::default(),
                                false,
                            )
                            .busy;
                    },
                );
                output.drop_without_applying_deltas();
                if !busy {
                    break;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        run(None, 800.0);
        assert_eq!(*reads.lock().unwrap(), [798]);
        let key = |key| {
            Some(egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::CTRL,
            })
        };
        run(key(egui::Key::PageDown), 800.0);
        assert_eq!(reads.lock().unwrap().last(), Some(&800));
        run(None, 600.0);
        assert_eq!(reads.lock().unwrap().last(), Some(&800));
        run(key(egui::Key::PageUp), 600.0);
        assert_eq!(reads.lock().unwrap().last(), Some(&798));
    }
}

pub use nfm_preview_command::{CommandEvent, CommandObserver, CommandSpec};

/// Execute an owned command on the existing NFM preview worker, then retain
/// stdout as a complete document for local parsing and paging.
pub fn command_document(
    command: &CommandSpec,
    center_row: Option<usize>,
    success_exit_codes: &[i32],
    cancel: Cancellation,
) -> Result<ByteDocument, String> {
    let output = nfm_preview_command::run(command, || cancel.is_cancelled(), |_, _| true)?;
    if !output.truncated
        && !output.status.success()
        && !output
            .status
            .code()
            .is_some_and(|c| success_exit_codes.contains(&c))
    {
        return Err(format!(
            "{}\nProcess exited with {}\nStderr:\n{}",
            command.describe(),
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let mut document = ByteDocument::complete(output.stdout);
    document.center_row = center_row;
    Ok(document)
}
