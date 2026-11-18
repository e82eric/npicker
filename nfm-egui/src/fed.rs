//! A picker whose rows are found somewhere else: in another process, or on
//! another machine. The picker says what it wants found, with [`Asked`], and
//! is told what was, with [`Feed::found`]. It keeps what is only on screen:
//! the query as typed, the selected row and where the list is scrolled to.
//!
//! [`Page`] is what was found as plain data, for whoever carries it between
//! the two, and [`Rows`] is a page as a source the picker can draw.

use crate::session::{SearchTimings, WakeCallback};
use egui::Context;
use nfm_search_core::{
    row::{RowFields, RowTemplate},
    search::SearchResult,
    store::ItemsSource,
};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

/// How many rows a fed picker asks for. It shows no more than it is given,
/// whatever the number that matched.
pub const FED_ROWS: usize = 200;

/// What a fed picker wants found. Each has the next revision: what is found
/// says which it is for, and the picker shows only what is for its last.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Asked {
    pub revision: u64,
    pub query: String,
    /// The most rows to answer with, the best first.
    pub rows: usize,
}

/// What was found for one [`Asked`]. There can be several for the same one,
/// each in place of the one before, while the source is still being added to.
pub struct Found<S> {
    /// The revision and the query of the `Asked` this answers.
    pub revision: u64,
    pub query: String,
    /// What the rows are rows of. A result's `node_index` is its row there.
    pub source: Arc<S>,
    /// Which source this is of the ones there have been: one that starts
    /// again has other rows under the same numbers.
    pub epoch: u64,
    /// Whether a row keeps its number and what it says while the source is
    /// added to. The selection then stays on its row from one answer to the
    /// next.
    pub append_only: bool,
    /// The rows to show, the best first.
    pub results: Vec<SearchResult>,
    pub matched: usize,
    pub total: usize,
    /// Source records left out, such as overlong lines.
    pub skipped: usize,
    /// Whether the source has all it will have.
    pub done: bool,
}

/// Where a fed picker is told what was found. From any thread.
pub struct Feed<S> {
    pub(crate) latest: Arc<Mutex<Option<Found<S>>>>,
    pub(crate) skipped: Arc<AtomicUsize>,
    pub(crate) timings: Arc<Mutex<SearchTimings>>,
    pub(crate) wake_callback: Arc<Mutex<Option<WakeCallback>>>,
    pub(crate) ctx: Context,
}
impl<S> Clone for Feed<S> {
    fn clone(&self) -> Self {
        Self {
            latest: self.latest.clone(),
            skipped: self.skipped.clone(),
            timings: self.timings.clone(),
            wake_callback: self.wake_callback.clone(),
            ctx: self.ctx.clone(),
        }
    }
}
impl<S> Feed<S> {
    /// Say what was found. One the picker has not taken yet is replaced: it
    /// shows the last it was told of, and that only if it is for what it
    /// last asked.
    pub fn found(&self, found: Found<S>) {
        self.skipped.store(found.skipped, Ordering::Release);
        let has_matches = found.matched > 0;
        // Release the picker's mailbox before destroying what it replaces.
        let before = self.latest.lock().unwrap().replace(found);
        drop(before);
        let ready = {
            let mut timings = self.timings.lock().unwrap();
            let now = Instant::now();
            timings.first_snapshot_ready.get_or_insert(now);
            timings.first_update_ready.get_or_insert(now);
            if has_matches {
                timings.first_matches_ready.get_or_insert(now);
            }
            timings.clone()
        };
        let callback = self.wake_callback.lock().unwrap().clone();
        if let Some(callback) = callback {
            callback(ready);
        }
        self.ctx.request_repaint();
    }
}

/// One row as it was found.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Row {
    /// The row's number in its source, which it is known by: it is what a
    /// selection of the row says, and what the row keeps from one answer to
    /// the next.
    pub index: usize,
    /// What the row shows as a line, and what its matches are found in.
    pub text: String,
    /// The values of its card, for a source with a template.
    pub fields: Option<Vec<String>>,
}

/// What was found for a query, as plain data.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Page {
    /// The source's version: rows of two pages are the same rows if this is
    /// the same, or if the source is only added to.
    pub version: u64,
    /// The rows to show, the best first.
    pub rows: Vec<Row>,
    /// What of the query the rows' text was matched against, which is what
    /// the matches shown in a row are found with. All of it for a source
    /// with no query language of its own.
    pub matched_against: String,
    /// A line shown over the rows, such as the names of columns.
    pub header: Option<String>,
    /// How a card is laid out, for rows with fields.
    pub template: Option<RowTemplate>,
}
impl Page {
    /// The page as a source, and its rows as results in their order.
    pub fn into_rows(self) -> (Rows, Vec<SearchResult>) {
        let results = self
            .rows
            .iter()
            .map(|row| SearchResult { node_index: row.index, score: 0, path: row.text.clone() })
            .collect();
        let at = self.rows.iter().enumerate().map(|(at, row)| (row.index, at)).collect();
        (Rows { page: self, at }, results)
    }
}

/// A page as a source: the rows a picker was given, and nothing of the
/// source they were found in. Its item is the row's number in that source.
#[derive(Debug, Default)]
pub struct Rows {
    page: Page,
    /// Where in the page a row of the source is.
    at: HashMap<usize, usize>,
}
impl Rows {
    pub fn row(&self, index: usize) -> Option<&Row> {
        self.page.rows.get(*self.at.get(&index)?)
    }
}
impl ItemsSource for Rows {
    type Item = usize;
    fn version(&self) -> u64 {
        self.page.version
    }
    fn len(&self) -> usize {
        self.page.rows.len()
    }
    fn is_empty(&self) -> bool {
        self.page.rows.is_empty()
    }
    fn get_string<'a>(
        &'a self,
        index: usize,
        _stack_buffer: &'a mut [u8],
        _heap_buffer: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        self.row(index).map_or(&[], |row| row.text.as_bytes())
    }
    fn get_string_lossy(&self, node_index: usize, _out: &mut Vec<u8>) -> String {
        self.row(node_index).map(|row| row.text.clone()).unwrap_or_default()
    }
    fn item(&self, node_index: usize) -> Option<usize> {
        self.row(node_index).map(|row| row.index)
    }
    fn header(&self, _query: &str) -> Option<String> {
        self.page.header.clone()
    }
    fn display_text(&self, node_index: usize, _query: &str) -> Option<String> {
        self.row(node_index).map(|row| row.text.clone())
    }
    /// The rows were found for one query, and say what of it they were
    /// matched against whatever is asked here.
    fn effective_query(&self, _input: &str) -> String {
        self.page.matched_against.clone()
    }
    fn row_template(&self) -> Option<&RowTemplate> {
        self.page.template.as_ref()
    }
    fn row_fields(&self, node_index: usize) -> Option<RowFields> {
        let values = self.row(node_index)?.fields.clone()?;
        Some(RowFields { values })
    }
}
