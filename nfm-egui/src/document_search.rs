//! Logical-line document search shared by terminal adapters and previews.
//! Positions map each UTF-8 byte to its display cell; soft wraps stay in one line.
use crate::copy_mode::{CellPosition, CellRange};
use regex::Regex;
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
/// Memory/work bounds captured per scan. Defaults are 4 MiB per logical line and
/// 200,000 nonempty matches. Exactly the limit is allowed; the next byte/match fails.
/// Setting either field to zero permits none of that resource.
pub struct SearchLimits {
    /// UTF-8 bytes retained for one logical line, including all soft wraps.
    pub line_bytes: usize,
    /// Nonempty matches retained across the entire document.
    pub matches: usize,
}
impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            line_bytes: 4 * 1024 * 1024,
            matches: 200_000,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
/// Typed scanner failures. Discard the scanner after an error: it can contain
/// partially accumulated state. The service converts these errors into feedback
/// strings, while direct scanner users can distinguish the variants.
pub enum SearchError {
    /// The cancellation callback returned true.
    Cancelled,
    /// Position count mismatch or invalid UTF-8 at a logical line boundary.
    InvalidDocument,
    /// Another byte would exceed the logical-line limit.
    LineLimit,
    /// Another nonempty match would exceed the document match limit.
    MatchLimit,
}
impl std::fmt::Display for SearchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Cancelled => "Search cancelled",
            Self::InvalidDocument => "Invalid search document",
            Self::LineLimit => "Search logical line exceeds the byte limit",
            Self::MatchLimit => "Search exceeds the match limit",
        })
    }
}
impl std::error::Error for SearchError {}

/// Incremental UTF-8-to-display-cell matcher independent of host and threads.
///
/// Feed chronological chunks, flush hard breaks, then consume with `finish`.
/// Chunks can split UTF-8 and soft wraps; match endpoints are inclusive cells.
/// Regex matching is nonoverlapping and confined to logical lines.
pub struct LineScanner {
    matcher: Regex,
    limits: SearchLimits,
    line: Vec<u8>,
    cells: Vec<CellPosition>,
    matches: Vec<CellRange>,
}
impl LineScanner {
    /// Create an empty scanner using default search limits. The supplied compiled
    /// regex controls literal/regex and case semantics; use `search_regex` to follow
    /// NFM defaults. Compilation happens before construction, not during each feed.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nfm_egui::document_search::{LineScanner, search_regex};
    /// let scanner = LineScanner::new(search_regex("cat", false)?);
    /// assert!(scanner.matches().is_empty());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn new(matcher: Regex) -> Self {
        Self::with_limits(matcher, SearchLimits::default())
    }
    /// Create an empty scanner with explicit per-line and total-match limits.
    /// Limits apply incrementally and produce typed errors; no partial result is
    /// returned by `finish` after failure.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nfm_egui::document_search::{LineScanner, SearchLimits, search_regex};
    /// let scanner = LineScanner::with_limits(search_regex("cat", false)?, SearchLimits { line_bytes: 128, matches: 10 });
    /// assert!(scanner.matches().is_empty());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn with_limits(matcher: Regex, limits: SearchLimits) -> Self {
        Self {
            matcher,
            limits,
            line: Vec::new(),
            cells: Vec::new(),
            matches: Vec::new(),
        }
    }
    /// A chunk may end inside UTF-8 or a soft-wrapped logical line. Newlines
    /// terminate logical lines; hosts may also call finish_line explicitly.
    /// Append bytes and their display positions, matching only at hard line boundaries.
    ///
    /// Require exactly one cell per byte, including newline bytes. Newlines flush the
    /// current logical line and are not included in it. Other bytes are buffered; UTF-8
    /// need not be complete until `finish_line` or `finish`. Checks cancellation at the
    /// start of each nonempty chunk and every 4,096 bytes, plus during line matching.
    /// Enforces the byte limit before each append. Cancellation is cooperative; a
    /// single regex operation can run until it returns. Discard the scanner on error.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nfm_egui::document_search::{LineScanner, search_regex};
    /// use nfm_egui::copy_mode::CellPosition;
    /// let mut scanner = LineScanner::new(search_regex("é", false)?);
    /// let cell = CellPosition { x: 3, y: 2 };
    /// scanner.feed(&[0xc3], [cell], &|| false)?;
    /// scanner.feed(&[0xa9], [cell], &|| false)?;
    /// assert_eq!(scanner.finish(&|| false)?[0].start, cell);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn feed(
        &mut self,
        bytes: &[u8],
        positions: impl IntoIterator<Item = CellPosition>,
        cancelled: &impl Fn() -> bool,
    ) -> Result<(), SearchError> {
        let mut positions = positions.into_iter();
        for (i, &byte) in bytes.iter().enumerate() {
            if i & 0xfff == 0 && cancelled() {
                return Err(SearchError::Cancelled);
            }
            let position = positions.next().ok_or(SearchError::InvalidDocument)?;
            if byte == b'\n' {
                self.finish_line(cancelled)?;
                continue;
            }
            if self.line.len() >= self.limits.line_bytes {
                return Err(SearchError::LineLimit);
            }
            self.line.push(byte);
            self.cells.push(position);
        }
        if positions.next().is_some() {
            return Err(SearchError::InvalidDocument);
        }
        Ok(())
    }
    /// Flush one hard-broken logical line without ending the document.
    ///
    /// Validates buffered UTF-8, finds nonoverlapping matches, skips zero-width matches,
    /// and maps the first/last matched bytes to inclusive cell endpoints. Checks
    /// cancellation before matching and for each nonempty match, enforcing the total
    /// match limit. Clears line buffers on success; accumulated matches remain.
    /// Calling on an empty line is valid. Do not flush between soft-wrapped rows.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nfm_egui::document_search::{LineScanner, search_regex};
    /// use nfm_egui::copy_mode::CellPosition;
    /// let mut scanner = LineScanner::new(search_regex("a.*b", true)?);
    /// scanner.feed(b"a", [CellPosition { x: 0, y: 0 }], &|| false)?;
    /// scanner.finish_line(&|| false)?; // A hard break prevents a match across rows.
    /// scanner.feed(b"b", [CellPosition { x: 0, y: 1 }], &|| false)?;
    /// assert!(scanner.finish(&|| false)?.is_empty());
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn finish_line(&mut self, cancelled: &impl Fn() -> bool) -> Result<(), SearchError> {
        if cancelled() {
            return Err(SearchError::Cancelled);
        }
        let line = std::str::from_utf8(&self.line).map_err(|_| SearchError::InvalidDocument)?;
        for found in self
            .matcher
            .find_iter(line)
            .filter(|found| !found.is_empty())
        {
            if cancelled() {
                return Err(SearchError::Cancelled);
            }
            if self.matches.len() >= self.limits.matches {
                return Err(SearchError::MatchLimit);
            }
            self.matches.push(CellRange {
                start: self.cells[found.start()],
                end: self.cells[found.end() - 1],
            });
        }
        self.line.clear();
        self.cells.clear();
        Ok(())
    }
    /// Borrow matches from lines already flushed. The unfinished logical line is
    /// not searched yet. These are in discovery order, not the final newest-first
    /// order; use `finish` for results suitable for navigation and decorations.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nfm_egui::document_search::{LineScanner, search_regex};
    /// use nfm_egui::copy_mode::CellPosition;
    /// let mut scanner = LineScanner::new(search_regex("a", false)?);
    /// scanner.feed(b"a", [CellPosition { x: 0, y: 0 }], &|| false)?;
    /// assert!(scanner.matches().is_empty());
    /// scanner.finish_line(&|| false)?;
    /// assert_eq!(scanner.matches().len(), 1);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn matches(&self) -> &[CellRange] {
        &self.matches
    }
    /// Consume the scanner, flush the final logical line, and sort ranges in
    /// descending `(start.y, start.x)` order. Returns owned inclusive ranges, or an
    /// error with no partial output. Cancellation is checked while flushing; sorting
    /// itself has no cancellation checkpoints.
    ///
    /// # Example
    ///
    /// ```rust
    /// use nfm_egui::document_search::{LineScanner, search_regex};
    /// use nfm_egui::copy_mode::CellPosition;
    /// let mut scanner = LineScanner::new(search_regex("a", false)?);
    /// for y in [0, 2] {
    ///     scanner.feed(b"a", [CellPosition { x: 0, y }], &|| false)?;
    ///     scanner.finish_line(&|| false)?;
    /// }
    /// let matches = scanner.finish(&|| false)?;
    /// assert_eq!(matches[0].start.y, 2);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn finish(mut self, cancelled: &impl Fn() -> bool) -> Result<Vec<CellRange>, SearchError> {
        self.finish_line(cancelled)?;
        self.matches
            .sort_by_key(|range| std::cmp::Reverse((range.start.y, range.start.x)));
        Ok(self.matches)
    }
}

/// Select a match relative to an optional copy cursor, or the newest match.
/// Choose a starting index in newest-first matches. Without a cursor origin,
/// selects index zero regardless of direction. With an origin, selects the next
/// strictly later/earlier cell for forward/backward search, wrapping at document
/// edges. Empty input returns `None`.
///
/// # Example
///
/// ```rust
/// # use nfm_egui::copy_mode::{CellPosition, CellRange};
/// # let matches: Vec<_> = [8, 4, 0].into_iter().map(|x| CellRange { start: CellPosition { x, y: 0 }, end: CellPosition { x, y: 0 } }).collect();
/// use nfm_egui::document_search::initial_match_index;
/// assert_eq!(initial_match_index(&matches, Some(CellPosition { x: 4, y: 0 }), true), Some(0));
/// assert_eq!(initial_match_index(&matches, None, false), Some(0));
/// ```
pub fn initial_match_index(
    matches: &[CellRange],
    origin: Option<CellPosition>,
    forward: bool,
) -> Option<usize> {
    if let Some(origin) = origin {
        let target = next_match_cell(matches, origin, forward, 1)?;
        matches.iter().position(|range| range.start == target)
    } else {
        (!matches.is_empty()).then_some(0)
    }
}
/// Move one index with wrapping. `previous` decrements the displayed result
/// index; false increments it. A missing selection starts at the last/first
/// index respectively, and out-of-range indices are reduced modulo count.
/// This follows result-list order, not logical forward/backward cursor motion.
///
/// # Example
///
/// ```rust
/// use nfm_egui::document_search::navigate_match_index;
/// assert_eq!(navigate_match_index(Some(0), 3, true), Some(2));
/// assert_eq!(navigate_match_index(Some(2), 3, false), Some(0));
/// assert_eq!(navigate_match_index(None, 0, false), None);
/// ```
pub fn navigate_match_index(
    selected: Option<usize>,
    count: usize,
    previous: bool,
) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(match selected {
        Some(index) if previous => (index % count + count - 1) % count,
        Some(index) => (index % count + 1) % count,
        None if previous => count - 1,
        None => 0,
    })
}

/// Matches are sorted by their start cell, newest to oldest, as required by
/// Ghostty's snapshot decoration viewport filter.
/// Resolve counted cursor navigation in newest-first ranges using binary
/// partitioning. Forward moves toward increasing `(y, x)`; backward decreases it.
/// The match at exactly the cursor is skipped before counting. Counts wrap through
/// all matches; zero is treated as one. Empty input returns `None`. The input must
/// be sorted descending by its start cell, as returned by `LineScanner::finish`.
///
/// # Example
///
/// ```rust
/// # use nfm_egui::copy_mode::{CellPosition, CellRange};
/// # let matches: Vec<_> = [8, 4, 0].into_iter().map(|x| CellRange { start: CellPosition { x, y: 0 }, end: CellPosition { x, y: 0 } }).collect();
/// use nfm_egui::document_search::next_match_cell;
/// let cursor = CellPosition { x: 4, y: 0 };
/// assert_eq!(next_match_cell(&matches, cursor, true, 1), Some(CellPosition { x: 8, y: 0 }));
/// assert_eq!(next_match_cell(&matches, cursor, true, 2), Some(CellPosition { x: 0, y: 0 }));
/// ```
pub fn next_match_cell(
    matches: &[CellRange],
    cursor: CellPosition,
    forward: bool,
    count: u32,
) -> Option<CellPosition> {
    if matches.is_empty() {
        return None;
    }
    let at = (cursor.y, cursor.x);
    let index = if forward {
        let newer = matches.partition_point(|range| (range.start.y, range.start.x) > at);
        (newer + matches.len() - 1) % matches.len()
    } else {
        matches.partition_point(|range| (range.start.y, range.start.x) >= at) % matches.len()
    };
    let steps = ((count.max(1) - 1) as usize) % matches.len();
    let index = if forward {
        (index + matches.len() - steps) % matches.len()
    } else {
        (index + steps) % matches.len()
    };
    Some(matches[index].start)
}

/// Shared terminal/preview search semantics: literal ignores case; regex honors
/// case and supports explicit inline flags such as (?i).
/// Compile shared pattern semantics once. Literal mode escapes metacharacters
/// and enables Unicode case-insensitive matching. Regex mode honors case and
/// accepts inline flags such as `(?i)`. Invalid regex returns the compiler error.
/// The scanner excludes zero-width matches and prevents cross-logical-line matches.
///
/// # Example
///
/// ```rust
/// use nfm_egui::document_search::search_regex;
/// assert!(search_regex("a.b", false)?.is_match("A.B"));
/// assert!(!search_regex("a.b", false)?.is_match("axb"));
/// assert!(!search_regex("cat", true)?.is_match("CAT"));
/// assert!(search_regex("(?i)cat", true)?.is_match("CAT"));
/// assert!(search_regex("[", true).is_err());
/// # Ok::<(), regex::Error>(())
/// ```
pub fn search_regex(query: &str, regex: bool) -> Result<regex::Regex, regex::Error> {
    let pattern = if regex {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(!regex)
        .build()
}

/// Copy-mode match navigation state, separate from worker scheduling and UI.
///
/// Keeps a query revision, immutable shared ranges, original direction, and motions
/// queued while results are pending. Default has revision zero and no search.
/// Inputs to result/adoption methods must use newest-first match order. Clone
/// shares existing match storage but copies pending navigation state.
#[derive(Clone, Default)]
pub struct SearchSession {
    pub(crate) query_revision: u64,
    pub(crate) matches: Arc<[CellRange]>,
    pub(crate) forward: bool,
    pub(crate) waiting: bool,
    pub(crate) pending_moves: Vec<(bool, u32)>,
}

impl SearchSession {
    /// Replace all session state with existing newest-first results. Sets the
    /// revision and shares owned ranges through an internal `Arc`; clears waiting and
    /// queued moves. No cursor move is produced. Direction resets to backward, so
    /// normal `navigate_search(..., false, ...)` moves backward; revision zero disables
    /// navigation. Use `begin_search`/`finish_search` to preserve an explicit direction.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nfm_egui::copy_mode::{CellPosition, CellRange};
    /// # let matches: Vec<_> = [8, 4, 0].into_iter().map(|x| CellRange { start: CellPosition { x, y: 0 }, end: CellPosition { x, y: 0 } }).collect();
    /// # use nfm_egui::document_search::SearchSession;
    /// # let mut session = SearchSession::default();
    /// session.adopt_search(5, matches);
    /// assert_eq!(session.search_matches().len(), 3);
    /// assert!(!session.search_accepts_result(5));
    /// ```
    pub fn adopt_search(&mut self, query_revision: u64, matches: Vec<CellRange>) {
        *self = SearchSession {
            query_revision,
            matches: matches.into(),
            ..Default::default()
        };
    }

    /// Replace the previous search with a pending revision/direction and an initial
    /// counted motion. Clears previous matches, marks waiting, and queues the initial
    /// move; no worker is started. Subsequent navigation is queued until `finish_search`.
    /// Use a nonzero revision matching the service ticket. Hosts that keep old highlights
    /// visible while typing must retain those decorations separately.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nfm_egui::document_search::SearchSession;
    /// # let mut session = SearchSession::default();
    /// session.begin_search(9, true, 1);
    /// assert!(session.search_accepts_result(9));
    /// assert!(session.search_matches().is_empty());
    /// ```
    pub fn begin_search(&mut self, query_revision: u64, forward: bool, count: u32) {
        *self = SearchSession {
            query_revision,
            forward,
            waiting: true,
            pending_moves: vec![(forward, count)],
            ..Default::default()
        };
    }

    /// Clear waiting and queued moves while retaining the revision, direction, and
    /// any existing matches. Later results for that pending search are rejected. This
    /// is only navigation-state cancellation: cancel the service or drop its ticket
    /// separately to stop worker reads.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nfm_egui::document_search::SearchSession;
    /// # let mut session = SearchSession::default();
    /// session.begin_search(9, true, 1);
    /// session.cancel_pending_search();
    /// assert!(!session.search_accepts_result(9));
    /// ```
    pub fn cancel_pending_search(&mut self) {
        self.waiting = false;
        self.pending_moves.clear();
    }

    /// Return true only while waiting for exactly this revision. This check does
    /// not mutate state or verify document identity. The host must validate its document
    /// revision separately. A result is rejected after acceptance or cancellation.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nfm_egui::document_search::SearchSession;
    /// # let mut session = SearchSession::default();
    /// session.begin_search(9, true, 1);
    /// assert!(session.search_accepts_result(9));
    /// assert!(!session.search_accepts_result(8));
    /// ```
    pub fn search_accepts_result(&self, query_revision: u64) -> bool {
        self.waiting && self.query_revision == query_revision
    }

    /// Borrow the adopted or completed newest-first ranges without copying them.
    /// A newly begun search has no ranges until completion. The borrow remains valid
    /// until the session is mutably changed; it contains inclusive display coordinates.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nfm_egui::copy_mode::{CellPosition, CellRange};
    /// # let matches: Vec<_> = [8, 4, 0].into_iter().map(|x| CellRange { start: CellPosition { x, y: 0 }, end: CellPosition { x, y: 0 } }).collect();
    /// # use nfm_egui::document_search::SearchSession;
    /// # let mut session = SearchSession::default();
    /// session.adopt_search(2, matches);
    /// assert_eq!(session.search_matches()[0].start.x, 8);
    /// ```
    pub fn search_matches(&self) -> &[CellRange] {
        &self.matches
    }

    /// Accept only the pending revision, install newest-first ranges, clear waiting,
    /// and replay the initial motion plus queued navigation against the supplied cursor.
    /// Each replay starts at the previous target and wraps through the matches. Returns
    /// the target without moving a host cursor. Empty results return the supplied cursor.
    /// A stale/cancelled/already-applied result returns `None` and leaves state unchanged.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nfm_egui::copy_mode::{CellPosition, CellRange};
    /// # let matches: Vec<_> = [8, 4, 0].into_iter().map(|x| CellRange { start: CellPosition { x, y: 0 }, end: CellPosition { x, y: 0 } }).collect();
    /// # use nfm_egui::document_search::SearchSession;
    /// # let mut session = SearchSession::default();
    /// let cursor = CellPosition { x: 4, y: 0 };
    /// session.begin_search(9, true, 1);
    /// assert!(session.finish_search(8, matches.clone(), cursor).is_none());
    /// assert_eq!(session.finish_search(9, matches, cursor), Some(CellPosition { x: 8, y: 0 }));
    /// ```
    pub fn finish_search(
        &mut self,
        query_revision: u64,
        matches: Vec<CellRange>,
        cursor: CellPosition,
    ) -> Option<CellPosition> {
        if !self.search_accepts_result(query_revision) {
            return None;
        }
        self.waiting = false;
        self.matches = matches.into();
        let mut target = cursor;
        for (forward, count) in std::mem::take(&mut self.pending_moves) {
            if let Some(next) = next_match_cell(&self.matches, target, forward, count) {
                target = next;
            }
        }
        Some(target)
    }

    /// Navigate immediately after completion, or queue a counted move while waiting.
    /// `reverse` flips the original search direction, rather than selecting an absolute
    /// backward direction. Pending calls return `None`; `finish_search` later replays
    /// them. Completed calls return the next start cell but do not update the host
    /// cursor. Revision zero or no completed matches returns `None`.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use nfm_egui::copy_mode::{CellPosition, CellRange};
    /// # let matches: Vec<_> = [8, 4, 0].into_iter().map(|x| CellRange { start: CellPosition { x, y: 0 }, end: CellPosition { x, y: 0 } }).collect();
    /// # use nfm_egui::document_search::SearchSession;
    /// # let mut session = SearchSession::default();
    /// let cursor = CellPosition { x: 4, y: 0 };
    /// session.begin_search(9, true, 1);
    /// assert!(session.navigate_search(cursor, false, 1).is_none());
    /// // Initial forward step reaches 8; queued forward step wraps to 0.
    /// assert_eq!(session.finish_search(9, matches, cursor), Some(CellPosition { x: 0, y: 0 }));
    /// ```
    pub fn navigate_search(
        &mut self,
        cursor: CellPosition,
        reverse: bool,
        count: u32,
    ) -> Option<CellPosition> {
        if self.query_revision == 0 {
            return None;
        }
        let forward = self.forward != reverse;
        if self.waiting {
            self.pending_moves.push((forward, count));
            None
        } else {
            next_match_cell(&self.matches, cursor, forward, count)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cells(bytes: &[u8], y: u64) -> Vec<CellPosition> {
        bytes
            .iter()
            .enumerate()
            .map(|(x, _)| CellPosition { x: x as u32, y })
            .collect()
    }
    #[test]
    fn chunks_utf8_soft_wraps_and_hard_lines_share_one_matcher() {
        let mut scanner = LineScanner::new(search_regex("界x", false).unwrap());
        let bytes = "界x\n界x".as_bytes();
        scanner
            .feed(&bytes[..1], cells(&bytes[..1], 2), &|| false)
            .unwrap();
        scanner
            .feed(&bytes[1..], cells(&bytes[1..], 3), &|| false)
            .unwrap();
        let matches = scanner.finish(&|| false).unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[1].start.y, 2);
        assert_eq!(matches[1].end.y, 3);
        let mut scanner = LineScanner::new(search_regex("x.*y", true).unwrap());
        scanner.feed(b"x\ny", cells(b"x\ny", 0), &|| false).unwrap();
        assert!(scanner.finish(&|| false).unwrap().is_empty());
    }
    #[test]
    fn literal_regex_and_zero_width_semantics() {
        for (pattern, regex, count) in [
            ("A.B", false, 2),
            ("A.B", true, 1),
            ("(?i)a.b", true, 3),
            ("^|a", true, 1),
        ] {
            let mut scanner = LineScanner::new(search_regex(pattern, regex).unwrap());
            scanner
                .feed(b"a.b A.B axb", cells(b"a.b A.B axb", 0), &|| false)
                .unwrap();
            assert_eq!(scanner.finish(&|| false).unwrap().len(), count);
        }
        assert!(search_regex("[", true).is_err());
    }
    #[test]
    fn cancellation_limits_and_malformed_positions_are_explicit() {
        let limits = SearchLimits {
            line_bytes: 3,
            matches: 1,
        };
        let mut scanner = LineScanner::with_limits(search_regex("a", false).unwrap(), limits);
        assert_eq!(
            scanner.feed(b"abcd", cells(b"abcd", 0), &|| false),
            Err(SearchError::LineLimit)
        );
        let mut scanner = LineScanner::with_limits(search_regex("a", false).unwrap(), limits);
        scanner.feed(b"aa", cells(b"aa", 0), &|| false).unwrap();
        assert_eq!(scanner.finish(&|| false), Err(SearchError::MatchLimit));
        let mut scanner = LineScanner::new(search_regex("a", false).unwrap());
        assert_eq!(
            scanner.feed(b"a", [], &|| false),
            Err(SearchError::InvalidDocument)
        );
        assert_eq!(
            scanner.feed(b"a", cells(b"a", 0), &|| true),
            Err(SearchError::Cancelled)
        );
    }
    #[test]
    fn revisions_pending_moves_and_navigation_wrap() {
        let matches: Vec<_> = [8, 4, 0]
            .into_iter()
            .map(|x| CellRange {
                start: CellPosition { x, y: 0 },
                end: CellPosition { x, y: 0 },
            })
            .collect();
        let cursor = CellPosition { x: 4, y: 0 };
        assert_eq!(initial_match_index(&matches, Some(cursor), true), Some(0));
        assert_eq!(initial_match_index(&matches, Some(cursor), false), Some(2));
        assert_eq!(navigate_match_index(Some(0), 3, true), Some(2));
        assert_eq!(navigate_match_index(Some(2), 3, false), Some(0));
        assert_eq!(navigate_match_index(None, 0, false), None);
        let mut session = SearchSession::default();
        session.begin_search(2, true, 1);
        assert!(session.finish_search(1, matches.clone(), cursor).is_none());
        assert!(session.navigate_search(cursor, false, 1).is_none());
        assert_eq!(
            session.finish_search(2, matches, cursor),
            Some(CellPosition { x: 0, y: 0 })
        );
        assert!(!session.search_accepts_result(2));
        session.begin_search(3, false, 1);
        session.cancel_pending_search();
        assert!(!session.search_accepts_result(3));
    }
}
