use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};
use nfm_search_core::search::{RESULT_LIMIT, SearchOutput, SearchResult, search, search_range};
use nfm_search_core::store::ItemsSource;

use crate::source_store::SharedStore;

#[derive(Clone, Debug, Default)]
pub struct SearchCounters {
    pub displayed: usize,
    pub matched: usize,
    pub published: usize,
    pub scanning: bool,
}

#[derive(Clone, Debug)]
pub struct SearchUpdate {
    pub request_id: u64,
    pub results: Vec<SearchResult>,
    pub counters: SearchCounters,
}

#[derive(Clone)]
pub struct FuzzySearchSession {
    inner: Arc<FuzzySearcherInner>,
}

struct FuzzySearcherInner {
    request_id: u64,
    store: Arc<SharedStore>,
    query: Mutex<String>,
    search_version: AtomicU64,
    cancelled: AtomicBool,

    signal_tx: Sender<()>,
    signal_rx: Receiver<()>,
    updates_tx: Sender<SearchUpdate>,
}

#[derive(Default)]
struct SearchCache {
    query: String,
    searched_len: usize,
    results: Vec<SearchResult>,
    matched: usize,
}

impl FuzzySearchSession {
    pub fn new(
        request_id: u64,
        store: Arc<SharedStore>,
        initial_query: String,
        updates_tx: Sender<SearchUpdate>,
    ) -> Self {
        let (signal_tx, signal_rx) = bounded(1);
        Self {
            inner: Arc::new(FuzzySearcherInner {
                request_id,
                store,
                query: Mutex::new(initial_query),
                search_version: AtomicU64::new(0),
                cancelled: AtomicBool::new(false),
                signal_tx,
                signal_rx,
                updates_tx,
            }),
        }
    }

    pub fn start(&self) {
        let inner = Arc::clone(&self.inner);

        thread::spawn(move || {
            inner.run_loop();
        });

        self.signal();
    }

    pub fn stop(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        self.signal();
    }

    pub fn set_query(&self, query: String) {
        {
            let mut current = self.inner.query.lock().expect("search query poisoned");
            if *current == query {
                return;
            }

            *current = query;
        }

        self.inner.search_version.fetch_add(1, Ordering::AcqRel);
        self.signal();
    }

    fn signal(&self) {
        let _ = self.inner.signal_tx.try_send(());
    }
}

impl FuzzySearcherInner {
    fn run_loop(self: &Arc<Self>) {
        let mut last_completed_version = None;
        let mut last_snapshot_version = None;
        let mut cache = SearchCache::default();
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return;
            }

            let (version, snapshot_version, scanning) = {
                (
                    self.search_version.load(Ordering::Acquire),
                    self.store.snapshot_version(),
                    !self.store.is_done(),
                )
            };

            if last_completed_version != Some(version)
                || last_snapshot_version != Some(snapshot_version)
            {
                self.run_search_generation(version, &mut cache);
                last_completed_version = Some(version);
                last_snapshot_version = Some(snapshot_version);
            }

            let timeout = if scanning {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(250)
            };

            let _ = self.signal_rx.recv_timeout(timeout);
        }
    }

    fn run_search_generation(&self, version: u64, cache: &mut SearchCache) {
        let query = self.query.lock().expect("search query poisoned").clone();

        let Some(snapshot) = self.store.snapshot() else {
            return;
        };

        let scanning = !self.store.is_done();
        let total = snapshot.len();

        let should_cancel = || {
            self.cancelled.load(Ordering::Acquire)
                || self.search_version.load(Ordering::Acquire) != version
        };

        let use_incremental = cache.query == query && cache.searched_len <= total;
        let output = if use_incremental {
            let Some(delta) = search_range(
                Arc::clone(&snapshot),
                &query,
                cache.searched_len..total,
                should_cancel,
            ) else {
                return;
            };
            merge_cached_search(cache, &query, delta)
        } else {
            let Some(output) = search(Arc::clone(&snapshot), &query, should_cancel) else {
                return;
            };
            *cache = SearchCache {
                query,
                searched_len: output.total,
                results: output.results.clone(),
                matched: output.matched,
            };
            output
        };
        if should_cancel() {
            return;
        }

        let counters = SearchCounters {
            displayed: output.results.len(),
            matched: output.matched,
            published: output.total,
            scanning,
        };

        let _ = self.updates_tx.send(SearchUpdate {
            request_id: self.request_id,
            results: output.results,
            counters,
        });
    }
}

fn merge_cached_search(cache: &mut SearchCache, query: &str, delta: SearchOutput) -> SearchOutput {
    cache.query = query.to_owned();
    cache.searched_len = delta.total;
    cache.matched += delta.matched;

    if query.is_empty() {
        if cache.results.len() < RESULT_LIMIT {
            let remaining = RESULT_LIMIT - cache.results.len();
            cache
                .results
                .extend(delta.results.into_iter().take(remaining));
        }
    } else {
        cache.results.extend(delta.results);
        cache.results.sort_by(compare_search_results);
        cache.results.truncate(RESULT_LIMIT);
    }

    SearchOutput {
        results: cache.results.clone(),
        matched: cache.matched,
        total: cache.searched_len,
    }
}

fn compare_search_results(left: &SearchResult, right: &SearchResult) -> std::cmp::Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| left.path.len().cmp(&right.path.len()))
        .then_with(|| left.node_index.cmp(&right.node_index))
}
