//! Public asynchronous search API for terminal scrollback and NFM previews.
//!
//! Implement [`SearchDocument`] to expose immutable text, then submit it to
//! [`SearchService`]. Keep the returned [`SearchTicket`] alive while the query is
//! relevant. A worker scans using [`crate::document_search::LineScanner`], and
//! the host polls [`SearchService::take_latest_result`] after its wake callback.
//! This module owns scheduling; [`crate::document_search::SearchSession`] owns
//! copy-mode match navigation. Neither module paints or scrolls the host UI.
//!
//! Documents supply display coordinates, not string character indices. Repeat
//! the same cell for all UTF-8 bytes of a glyph; wide glyphs and combining marks
//! must use the host's actual cell mapping. Preserve the snapshot until reads
//! finish, or provide a closable lease which guards every native read.
use crate::copy_mode::{CellPosition, CellRange};
use crate::document_search::{LineScanner, SearchLimits, search_regex};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::thread;

/// Owned or guarded immutable text. A visitor receives one cell per UTF-8 byte;
/// `line_end` distinguishes a hard break from a soft wrap. Hosts may batch rows.
/// Reads must stop promptly when cancelled and serialize invalidation with access.
pub trait SearchDocument: Send + Sync + 'static {
    /// Visit text in document order, from oldest to newest, on the calling thread.
    ///
    /// Invoke `visitor` for each chunk, with exactly one position per byte. A chunk
    /// may split a UTF-8 character or a logical line. Set `line_end` only when the
    /// chunk completes a hard-broken logical line; leave it false for soft wraps.
    /// Embedded newline bytes also terminate lines and still require positions.
    /// At the end of the visit, the scanner flushes any remaining logical line.
    ///
    /// Check `cancelled` before expensive reads and between batches. Propagate a
    /// visitor error immediately; it can indicate cancellation or a search limit.
    /// Do not cache the visitor or call it after this method returns. Implementations
    /// must own data or serialize lease invalidation with native reads. In asynchronous
    /// search this method runs on NFM's worker, never on the UI thread.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nfm_egui::search_service::SearchDocument;
    /// use nfm_egui::copy_mode::CellPosition;
    /// struct Greeting;
    /// impl SearchDocument for Greeting {
    ///     fn visit_text(&self, visitor: &mut dyn FnMut(&[u8], &[CellPosition], bool) -> Result<(), String>, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    ///         if cancelled() { return Err("cancelled".into()); }
    ///         // Both bytes of é occupy the same display cell.
    ///         let cells = [CellPosition { x: 0, y: 4 }; 2];
    ///         visitor("é".as_bytes(), &cells, true)
    ///     }
    /// }
    /// ```
    fn visit_text(
        &self,
        visitor: &mut dyn FnMut(&[u8], &[CellPosition], bool) -> Result<(), String>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), String>;
}

/// Completed feedback for one query, with inclusive cell ranges ordered newest first.
///
/// An error leaves `matches` empty. Query/regex/document/limit errors arrive here;
/// worker startup or transport failures are returned by service methods instead.
/// Compare the document revision with your current snapshot before painting.
#[derive(Debug)]
pub struct SearchResult {
    /// Opaque host-supplied snapshot identity, copied from `submit`.
    pub document_revision: u64,
    /// Service-assigned nonzero query identity; compare with the ticket.
    pub query_revision: u64,
    /// Inclusive display-cell ranges in descending `(start.y, start.x)` order.
    pub matches: Vec<CellRange>,
    /// Regex, document-access, or limit failure; `None` means the scan succeeded.
    pub error: Option<String>,
}
/// Cancellation ownership for a submitted query.
///
/// Dropping this ticket sets its cancellation flag, including after completion:
/// feedback still queued in the service will then be discarded. Store the ticket
/// until you consume the result or abandon the query. Cloned cancellation flags
/// do not keep the query active after the ticket is dropped.
pub struct SearchTicket {
    /// Nonzero revision assigned by the service, with possible gaps.
    pub query_revision: u64,
    cancel: Arc<AtomicBool>,
}
impl SearchTicket {
    /// Clone this query's cooperative cancellation flag without cancelling it.
    ///
    /// Setting it to true cancels only this ticket, not a newer request. The worker
    /// checks it while reading and matching, and the service also filters already
    /// queued results using it. This operation does not wait for document release;
    /// use `SearchService::cancel_and_wait` before freeing borrowed native resources.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nfm_egui::search_service::{SearchDocument, SearchService};
    /// # use nfm_egui::copy_mode::CellPosition;
    /// # use std::sync::Arc;
    /// # struct Text;
    /// # impl SearchDocument for Text {
    /// #     fn visit_text(&self, visitor: &mut dyn FnMut(&[u8], &[CellPosition], bool) -> Result<(), String>, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    /// #         if cancelled() { return Err("cancelled".into()); }
    /// #         visitor(b"cat", &[CellPosition { x: 0, y: 0 }, CellPosition { x: 1, y: 0 }, CellPosition { x: 2, y: 0 }], true)
    /// #     }
    /// # }
    /// # let document: Arc<dyn SearchDocument> = Arc::new(Text);
    /// let mut service = SearchService::new(|| {});
    /// let ticket = service.submit(1, document, "cat".into(), false)?;
    /// let cancellation = ticket.cancellation();
    /// cancellation.store(true, std::sync::atomic::Ordering::Release);
    /// service.cancel_and_wait()?;
    /// # Ok::<(), String>(())
    /// ```
    pub fn cancellation(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }
}
impl Drop for SearchTicket {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}
struct Request {
    document: Arc<dyn SearchDocument>,
    document_revision: u64,
    query_revision: u64,
    query: String,
    regex: bool,
    cancel: Arc<AtomicBool>,
    limits: SearchLimits,
}
enum Command {
    Search(Request),
    Barrier(mpsc::Sender<()>),
    Shutdown,
}

/// Starts a reusable worker on demand. New requests supersede previous ones.
/// Dropping the service cancels without joining the UI thread; hosts with native
/// resources call cancel_and_wait before closing their lease or freeing data.
pub struct SearchService {
    commands: mpsc::Sender<Command>,
    results: mpsc::Receiver<SearchResult>,
    latest: Arc<AtomicU64>,
    revision: u64,
    thread: Option<thread::JoinHandle<()>>,
    fault: Arc<Mutex<Option<String>>>,
    wake: Arc<dyn Fn() + Send + Sync>,
    active: Option<Arc<AtomicBool>>,
    limits: SearchLimits,
}
impl SearchService {
    /// Create an idle service with default limits and no worker thread yet.
    ///
    /// The first successful `submit` starts a reusable worker. `wake` is called from
    /// that worker after publishing feedback or recording a panic. Keep the callback
    /// short, thread-safe, and non-panicking; enqueue a UI event or request repaint,
    /// then poll results on the owning thread. A wake is a notification, not a result,
    /// and does not guarantee feedback remains current when the host receives it.
    ///
    /// Dropping the service cancels and requests shutdown without joining the worker.
    /// It therefore does not guarantee a borrowed native resource is safe to free.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nfm_egui::search_service::SearchService;
    /// let context = nfm_egui::egui::Context::default();
    /// let mut service = SearchService::new(move || context.request_repaint());
    /// assert!(service.take_latest_result()?.is_none());
    /// # Ok::<(), String>(())
    /// ```
    pub fn new(wake: impl Fn() + Send + Sync + 'static) -> Self {
        let (commands, _) = mpsc::channel();
        let (_, results) = mpsc::channel();
        Self {
            commands,
            results,
            latest: Arc::new(AtomicU64::new(0)),
            revision: 0,
            thread: None,
            fault: Arc::new(Mutex::new(None)),
            wake: Arc::new(wake),
            active: None,
            limits: SearchLimits::default(),
        }
    }
    /// Set limits used by future submissions.
    ///
    /// Each queued request captures its own copy of the limits; this does not restart
    /// or change an in-progress search. `line_bytes` bounds one logical line across
    /// soft wraps, and `matches` bounds all nonempty matches in the document. Errors
    /// discard the result rather than returning a partial match list.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nfm_egui::{search_service::SearchService, document_search::SearchLimits};
    /// let mut service = SearchService::new(|| {});
    /// service.set_limits(SearchLimits { line_bytes: 1024 * 1024, matches: 10_000 });
    /// ```
    pub fn set_limits(&mut self, limits: SearchLimits) {
        self.limits = limits;
    }
    fn advance(&mut self) -> u64 {
        self.revision = self.revision.wrapping_add(1).max(1);
        self.latest.store(self.revision, Ordering::Release);
        self.revision
    }
    fn ensure_started(&mut self) -> Result<(), String> {
        if let Some(error) = self.fault.lock().unwrap_or_else(|e| e.into_inner()).take() {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            return Err(format!("Search worker failed: {error}"));
        }
        if self.thread.as_ref().is_some_and(|t| !t.is_finished()) {
            return Ok(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let (tx, rx) = mpsc::channel();
        let (results_tx, results_rx) = mpsc::channel();
        let latest = self.latest.clone();
        let wake = self.wake.clone();
        let fault = self.fault.clone();
        let worker = thread::Builder::new()
            .name("nfm-document-search".into())
            .spawn(move || {
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    worker_loop(rx, results_tx, latest, &wake)
                })) {
                    let error = panic
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "Search worker panicked".into());
                    *fault.lock().unwrap_or_else(|e| e.into_inner()) = Some(error);
                    wake();
                }
            })
            .map_err(|e| format!("Unable to start search worker: {e}"))?;
        self.commands = tx;
        self.results = results_rx;
        self.thread = Some(worker);
        Ok(())
    }
    /// Submit a query without waiting for its scan to finish.
    ///
    /// Starts the worker if needed, cancels the previous query, advances the revision,
    /// and queues an owned `Arc` to the immutable document. Queued edits are collapsed
    /// to the newest request, and an in-progress scan exits cooperatively when superseded.
    /// The host's `document_revision` is an opaque correlation tag, not a validation:
    /// the document implementation must enforce its own snapshot lifetime.
    ///
    /// `regex = false` uses escaped, case-insensitive literal matching; true uses
    /// case-sensitive regex unless inline flags override it. Invalid regex produces
    /// asynchronous error feedback. Empty queries succeed without reading the document.
    /// Keep the returned ticket alive. Revisions are nonzero with gaps and may wrap;
    /// use equality to correlate results rather than assuming consecutive numbers.
    ///
    /// Returns an error if worker startup fails, an earlier panic is reported, or the
    /// request channel has disconnected. After handling a worker failure, a later
    /// submission can start a replacement. No failed request is automatically replayed.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nfm_egui::search_service::{SearchDocument, SearchService};
    /// # use nfm_egui::copy_mode::CellPosition;
    /// # use std::sync::Arc;
    /// # struct Text;
    /// # impl SearchDocument for Text {
    /// #     fn visit_text(&self, visitor: &mut dyn FnMut(&[u8], &[CellPosition], bool) -> Result<(), String>, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    /// #         if cancelled() { return Err("cancelled".into()); }
    /// #         visitor(b"cat", &[CellPosition { x: 0, y: 0 }, CellPosition { x: 1, y: 0 }, CellPosition { x: 2, y: 0 }], true)
    /// #     }
    /// # }
    /// # let document: Arc<dyn SearchDocument> = Arc::new(Text);
    /// let mut service = SearchService::new(|| {});
    /// let first = service.submit(42, document.clone(), "ca".into(), false)?;
    /// let current = service.submit(42, document, "cat".into(), false)?;
    /// assert!(first.cancellation().load(std::sync::atomic::Ordering::Acquire));
    /// // Retain `current` while awaiting feedback for current.query_revision.
    /// service.cancel_and_wait()?;
    /// # Ok::<(), String>(())
    /// ```
    pub fn submit(
        &mut self,
        document_revision: u64,
        document: Arc<dyn SearchDocument>,
        query: String,
        regex: bool,
    ) -> Result<SearchTicket, String> {
        self.ensure_started()?;
        self.cancel();
        let query_revision = self.advance();
        let cancel = Arc::new(AtomicBool::new(false));
        self.commands
            .send(Command::Search(Request {
                document,
                document_revision,
                query_revision,
                query,
                regex,
                cancel: cancel.clone(),
                limits: self.limits,
            }))
            .map_err(|_| "Search worker stopped before receiving the request".to_string())?;
        self.active = Some(cancel.clone());
        Ok(SearchTicket {
            query_revision,
            cancel,
        })
    }
    /// Cancel current work and invalidate queued feedback without waiting.
    ///
    /// Sets the active request's flag and advances the latest revision. The worker
    /// stops when it next checks cancellation; document access may still be running
    /// when this returns. The service stays available for subsequent submissions.
    /// This also works before the worker starts. Use `cancel_and_wait` for teardown.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nfm_egui::search_service::{SearchDocument, SearchService};
    /// # use nfm_egui::copy_mode::CellPosition;
    /// # use std::sync::Arc;
    /// # struct Text;
    /// # impl SearchDocument for Text {
    /// #     fn visit_text(&self, visitor: &mut dyn FnMut(&[u8], &[CellPosition], bool) -> Result<(), String>, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    /// #         if cancelled() { return Err("cancelled".into()); }
    /// #         visitor(b"cat", &[CellPosition { x: 0, y: 0 }, CellPosition { x: 1, y: 0 }, CellPosition { x: 2, y: 0 }], true)
    /// #     }
    /// # }
    /// # let document: Arc<dyn SearchDocument> = Arc::new(Text);
    /// let mut service = SearchService::new(|| {});
    /// let ticket = service.submit(1, document, "cat".into(), false)?;
    /// service.cancel();
    /// assert!(ticket.cancellation().load(std::sync::atomic::Ordering::Acquire));
    /// assert!(service.take_latest_result()?.is_none());
    /// service.cancel_and_wait()?;
    /// # Ok::<(), String>(())
    /// ```
    pub fn cancel(&mut self) {
        if let Some(active) = self.active.take() {
            active.store(true, Ordering::Release);
        }
        self.advance();
    }
    /// Acknowledgement occurs only after all earlier document handles are dropped.
    /// Cancel and block until earlier worker document handles have been released.
    ///
    /// Advances the revision, sends a barrier behind prior requests, and waits for its
    /// acknowledgement. The worker drops an active or queued request before acknowledging.
    /// If the channel fails, this joins the stopped worker and returns an error instead.
    /// Before startup it returns immediately. The worker remains reusable afterward.
    ///
    /// This is the lifetime boundary for a host-owned snapshot: after handling the
    /// return value, invalidate your own lease and free the native resource. It releases
    /// the worker's handles, not additional `Arc`s held by the host. It can block as
    /// long as the document's read/cancellation implementation takes; do not call it
    /// from the worker's wake callback or while holding a lock that document reads need.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nfm_egui::search_service::{SearchDocument, SearchService};
    /// # use nfm_egui::copy_mode::CellPosition;
    /// # use std::sync::Arc;
    /// # struct Text;
    /// # impl SearchDocument for Text {
    /// #     fn visit_text(&self, visitor: &mut dyn FnMut(&[u8], &[CellPosition], bool) -> Result<(), String>, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    /// #         if cancelled() { return Err("cancelled".into()); }
    /// #         visitor(b"cat", &[CellPosition { x: 0, y: 0 }, CellPosition { x: 1, y: 0 }, CellPosition { x: 2, y: 0 }], true)
    /// #     }
    /// # }
    /// # let document: Arc<dyn SearchDocument> = Arc::new(Text);
    /// let mut service = SearchService::new(|| {});
    /// let ticket = service.submit(7, document.clone(), "cat".into(), false)?;
    /// service.cancel_and_wait()?;
    /// drop(ticket);
    /// drop(document); // Now close the host's lease or destroy its native snapshot.
    /// # Ok::<(), String>(())
    /// ```
    pub fn cancel_and_wait(&mut self) -> Result<(), String> {
        self.cancel();
        if self.thread.is_none() {
            return Ok(());
        }
        let (tx, rx) = mpsc::channel();
        if self.commands.send(Command::Barrier(tx)).is_err() || rx.recv().is_err() {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            return Err("Search worker stopped while releasing document".into());
        }
        Ok(())
    }
    /// Drain available feedback without waiting for a scan.
    ///
    /// Returns the newest queued result whose query revision still matches the service
    /// and whose cancellation flag remains false. Superseded/cancelled feedback is
    /// discarded. `Ok(None)` means no current feedback is ready, including before startup.
    /// It does not distinguish pending, cancelled, and already-consumed requests; keep
    /// that UI state yourself. A returned result does not drop your ticket or scroll UI.
    ///
    /// `Err` reports a worker panic/disconnection, separate from `SearchResult::error`.
    /// Error handling may join a stopped worker. A later submission can retry with a
    /// replacement worker. The service filters query identity, but hosts must also
    /// compare `document_revision` against their current snapshot before applying it.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nfm_egui::search_service::{SearchDocument, SearchService};
    /// # use nfm_egui::copy_mode::CellPosition;
    /// # use std::sync::Arc;
    /// # struct Text;
    /// # impl SearchDocument for Text {
    /// #     fn visit_text(&self, visitor: &mut dyn FnMut(&[u8], &[CellPosition], bool) -> Result<(), String>, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
    /// #         if cancelled() { return Err("cancelled".into()); }
    /// #         visitor(b"cat", &[CellPosition { x: 0, y: 0 }, CellPosition { x: 1, y: 0 }, CellPosition { x: 2, y: 0 }], true)
    /// #     }
    /// # }
    /// # let document: Arc<dyn SearchDocument> = Arc::new(Text);
    /// let (wake_tx, wake_rx) = std::sync::mpsc::channel();
    /// let mut service = SearchService::new(move || { let _ = wake_tx.send(()); });
    /// let ticket = service.submit(12, document, "cat".into(), false)?;
    /// wake_rx.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
    /// if let Some(result) = service.take_latest_result()? {
    ///     assert_eq!(result.document_revision, 12);
    ///     assert_eq!(result.query_revision, ticket.query_revision);
    ///     assert!(result.error.is_none());
    ///     assert_eq!(result.matches.len(), 1);
    ///     // Apply result.matches to host decorations and viewport here.
    /// }
    /// service.cancel_and_wait()?;
    /// # Ok::<(), String>(())
    /// ```
    pub fn take_latest_result(&mut self) -> Result<Option<SearchResult>, String> {
        if let Some(error) = self.fault.lock().unwrap_or_else(|e| e.into_inner()).take() {
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
            return Err(format!("Search worker failed: {error}"));
        }
        if self.thread.is_none() {
            return Ok(None);
        }
        let mut newest = None;
        loop {
            match self.results.try_recv() {
                Ok(result) => {
                    if result.query_revision == self.latest.load(Ordering::Acquire)
                        && self
                            .active
                            .as_ref()
                            .is_some_and(|a| !a.load(Ordering::Acquire))
                    {
                        newest = Some(result);
                    }
                }
                Err(mpsc::TryRecvError::Empty) => return Ok(newest),
                Err(mpsc::TryRecvError::Disconnected) => {
                    if let Some(thread) = self.thread.take() {
                        let _ = thread.join();
                    }
                    return Err("Search worker stopped unexpectedly; retry the search".into());
                }
            }
        }
    }
}
impl Drop for SearchService {
    fn drop(&mut self) {
        self.cancel();
        let _ = self.commands.send(Command::Shutdown);
    }
}

/// Scan synchronously using the same engine as `SearchService`.
///
/// Checks cancellation, returns no matches for an empty query without visiting the
/// document, compiles the pattern, then feeds visited chunks into a bounded logical-
/// line scanner. Hard line endings flush matches; soft wraps preserve them. Final
/// ranges are inclusive display cells, sorted newest to oldest. Zero-width regex
/// matches are omitted. No threads, wake callbacks, or revision filtering are used.
///
/// Cancellation, invalid regex, document/visitor errors, and limits return an error
/// string and discard partial results. For large host documents use the worker;
/// this call executes every read on the caller's thread.
///
/// # Example
///
/// ```rust
/// # use nfm_egui::search_service::{SearchDocument, SearchService};
/// # use nfm_egui::copy_mode::CellPosition;
/// # use std::sync::Arc;
/// # struct Text;
/// # impl SearchDocument for Text {
/// #     fn visit_text(&self, visitor: &mut dyn FnMut(&[u8], &[CellPosition], bool) -> Result<(), String>, cancelled: &dyn Fn() -> bool) -> Result<(), String> {
/// #         if cancelled() { return Err("cancelled".into()); }
/// #         visitor(b"cat", &[CellPosition { x: 0, y: 0 }, CellPosition { x: 1, y: 0 }, CellPosition { x: 2, y: 0 }], true)
/// #     }
/// # }
/// # let document: Arc<dyn SearchDocument> = Arc::new(Text);
/// use nfm_egui::search_service::search_document;
/// let matches = search_document(document.as_ref(), "CAT", false, Default::default(), &|| false)?;
/// assert_eq!(matches[0].start, CellPosition { x: 0, y: 0 });
/// assert_eq!(matches[0].end, CellPosition { x: 2, y: 0 });
/// # Ok::<(), String>(())
/// ```
pub fn search_document(
    document: &dyn SearchDocument,
    query: &str,
    regex: bool,
    limits: SearchLimits,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<CellRange>, String> {
    if cancelled() {
        return Err("Search cancelled".into());
    }
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let matcher = search_regex(query, regex).map_err(|e| e.to_string())?;
    let mut scanner = LineScanner::with_limits(matcher, limits);
    document.visit_text(
        &mut |text, cells, line_end| {
            scanner
                .feed(text, cells.iter().copied(), &cancelled)
                .map_err(|e| e.to_string())?;
            if line_end {
                scanner.finish_line(&cancelled).map_err(|e| e.to_string())?;
            }
            Ok(())
        },
        cancelled,
    )?;
    scanner.finish(&cancelled).map_err(|e| e.to_string())
}
fn worker_loop(
    commands: mpsc::Receiver<Command>,
    results: mpsc::Sender<SearchResult>,
    latest: Arc<AtomicU64>,
    wake: &Arc<dyn Fn() + Send + Sync>,
) {
    'requests: while let Ok(command) = commands.recv() {
        let mut request = match command {
            Command::Search(request) => request,
            Command::Barrier(done) => {
                let _ = done.send(());
                continue;
            }
            Command::Shutdown => return,
        };
        loop {
            match commands.try_recv() {
                Ok(Command::Search(newer)) => request = newer,
                Ok(Command::Barrier(done)) => {
                    drop(request); // Native lifetime guarantee: release BEFORE acknowledgement.
                    let _ = done.send(());
                    continue 'requests;
                }
                Ok(Command::Shutdown) => return,
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }
        let cancelled = || {
            request.cancel.load(Ordering::Acquire)
                || latest.load(Ordering::Acquire) != request.query_revision
        };
        if cancelled() {
            continue;
        }
        let outcome = search_document(
            request.document.as_ref(),
            &request.query,
            request.regex,
            request.limits,
            &cancelled,
        );
        if cancelled() {
            continue;
        }
        let (matches, error) = match outcome {
            Ok(matches) => (matches, None),
            Err(error) => (Vec::new(), Some(error)),
        };
        let result = SearchResult {
            document_revision: request.document_revision,
            query_revision: request.query_revision,
            matches,
            error,
        };
        drop(request);
        if results.send(result).is_err() {
            return;
        }
        wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    struct Text(&'static str);
    impl SearchDocument for Text {
        fn visit_text(
            &self,
            visitor: &mut dyn FnMut(&[u8], &[CellPosition], bool) -> Result<(), String>,
            _: &dyn Fn() -> bool,
        ) -> Result<(), String> {
            let cells: Vec<_> = self
                .0
                .bytes()
                .enumerate()
                .map(|(x, _)| CellPosition { x: x as u32, y: 7 })
                .collect();
            visitor(self.0.as_bytes(), &cells, true)
        }
    }
    fn poll(service: &mut SearchService) -> SearchResult {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(result) = service.take_latest_result().unwrap() {
                return result;
            }
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
    }
    struct Waiting {
        started: mpsc::Sender<()>,
        dropped: Arc<AtomicBool>,
    }
    impl SearchDocument for Waiting {
        fn visit_text(
            &self,
            _: &mut dyn FnMut(&[u8], &[CellPosition], bool) -> Result<(), String>,
            cancelled: &dyn Fn() -> bool,
        ) -> Result<(), String> {
            self.started.send(()).unwrap();
            while !cancelled() {
                thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        }
    }
    impl Drop for Waiting {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }
    fn waiting() -> (Arc<Waiting>, mpsc::Receiver<()>, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        (
            Arc::new(Waiting {
                started: tx,
                dropped: dropped.clone(),
            }),
            rx,
            dropped,
        )
    }
    #[test]
    fn latest_request_wins_and_worker_is_reused() {
        let mut service = SearchService::new(|| {});
        let (doc, started, dropped) = waiting();
        let old = service.submit(1, doc, "old".into(), false).unwrap();
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        let thread = service.thread.as_ref().unwrap().thread().id();
        let ticket = service
            .submit(2, Arc::new(Text("new NEW")), "new".into(), false)
            .unwrap();
        let result = poll(&mut service);
        assert!(old.cancel.load(Ordering::Acquire));
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(result.query_revision, ticket.query_revision);
        assert_eq!(result.document_revision, 2);
        assert_eq!(result.matches.len(), 2);
        assert_eq!(service.thread.as_ref().unwrap().thread().id(), thread);
        service.cancel_and_wait().unwrap();
    }
    #[test]
    fn cancellation_barrier_releases_document_before_returning() {
        let mut service = SearchService::new(|| {});
        let (doc, started, dropped) = waiting();
        let weak = Arc::downgrade(&doc);
        let ticket = service.submit(9, doc, "query".into(), false).unwrap();
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        service.cancel_and_wait().unwrap();
        assert!(ticket.cancel.load(Ordering::Acquire));
        assert!(dropped.load(Ordering::Acquire));
        assert!(weak.upgrade().is_none());
        assert!(service.take_latest_result().unwrap().is_none());
    }
    #[test]
    fn cancelled_ticket_discards_already_completed_feedback() {
        let (tx, rx) = mpsc::channel();
        let mut service = SearchService::new(move || {
            let _ = tx.send(());
        });
        let ticket = service
            .submit(1, Arc::new(Text("one")), "one".into(), false)
            .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(ticket);
        assert!(service.take_latest_result().unwrap().is_none());
        service.cancel_and_wait().unwrap();
    }
    #[test]
    fn service_drop_cancels_and_releases_in_background() {
        let mut service = SearchService::new(|| {});
        let (doc, started, dropped) = waiting();
        let ticket = service.submit(1, doc, "one".into(), false).unwrap();
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(service);
        assert!(ticket.cancel.load(Ordering::Acquire));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !dropped.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
    }
    #[test]
    fn errors_have_correlated_feedback_and_next_request_recovers() {
        let mut service = SearchService::new(|| {});
        let invalid = service
            .submit(3, Arc::new(Text("one")), "[".into(), true)
            .unwrap();
        let result = poll(&mut service);
        assert_eq!(result.query_revision, invalid.query_revision);
        assert!(result.error.is_some());
        service.set_limits(SearchLimits {
            line_bytes: 1,
            matches: 1,
        });
        let limited = service
            .submit(4, Arc::new(Text("one")), "one".into(), false)
            .unwrap();
        let result = poll(&mut service);
        assert_eq!(result.query_revision, limited.query_revision);
        assert!(result.error.unwrap().contains("byte limit"));
        service.set_limits(Default::default());
        let _ticket = service
            .submit(5, Arc::new(Text("one")), "one".into(), false)
            .unwrap();
        assert_eq!(poll(&mut service).matches.len(), 1);
    }
    #[test]
    fn disconnected_or_panicked_worker_restarts_on_retry() {
        let mut service = SearchService::new(|| {});
        let first = service
            .submit(1, Arc::new(Text("one")), "one".into(), false)
            .unwrap();
        service.commands.send(Command::Shutdown).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !service.thread.as_ref().unwrap().is_finished() {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        assert!(service.take_latest_result().is_err());
        struct PanicDocument;
        impl SearchDocument for PanicDocument {
            fn visit_text(
                &self,
                _: &mut dyn FnMut(&[u8], &[CellPosition], bool) -> Result<(), String>,
                _: &dyn Fn() -> bool,
            ) -> Result<(), String> {
                panic!("test failure");
            }
        }
        let (tx, rx) = mpsc::channel();
        service.wake = Arc::new(move || {
            let _ = tx.send(());
        });
        let failed = service
            .submit(2, Arc::new(PanicDocument), "query".into(), false)
            .unwrap();
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            service
                .take_latest_result()
                .unwrap_err()
                .contains("test failure")
        );
        let recovered = service
            .submit(3, Arc::new(Text("one")), "one".into(), false)
            .unwrap();
        assert!(
            recovered.query_revision > first.query_revision
                && recovered.query_revision > failed.query_revision
        );
        assert_eq!(poll(&mut service).document_revision, 3);
    }
}
