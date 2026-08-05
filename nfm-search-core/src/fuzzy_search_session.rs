use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::search::{
    search_range_with_match_collection, search_range_with_sort, search_with_match_cache,
    search_with_sort, MatchBitmap, SearchOutput, SearchResult, SearchSortMode, RESULT_LIMIT,
};
use crate::store::ItemsSource;
use crossbeam_channel::{bounded, Receiver, Sender};

pub trait SearchSnapshotProvider<S>: Send + Sync + 'static
where
    S: ItemsSource + Send + Sync + 'static,
{
    fn snapshot(&self) -> Option<Arc<S>>;
    fn snapshot_version(&self) -> u64;
    fn is_done(&self) -> bool;
}

#[derive(Clone, Debug)]
pub struct FuzzySearchUpdate {
    pub session_id: u64,
    pub generation: u64,
    pub query: String,
    pub results: Vec<SearchResult>,
    pub matched: usize,
    pub searched: usize,
    pub total: usize,
    pub source_version: u64,
    pub source_done: bool,
}

pub struct FuzzySearchSession<S, P>
where
    S: ItemsSource + Send + Sync + 'static,
    P: SearchSnapshotProvider<S>,
{
    inner: Arc<FuzzySearcherInner<S, P>>,
}

struct FuzzySearcherInner<S, P>
where
    S: ItemsSource + Send + Sync + 'static,
    P: SearchSnapshotProvider<S>,
{
    session_id: u64,
    provider: Arc<P>,
    sort_mode: SearchSortMode,
    query: Mutex<String>,
    search_version: AtomicU64,
    cancelled: AtomicBool,

    signal_tx: Sender<()>,
    signal_rx: Receiver<()>,
    updates_tx: Sender<FuzzySearchUpdate>,
    _snapshot: PhantomData<S>,
}

#[derive(Default)]
struct SearchCache {
    query: String,
    searched_len: usize,
    results: Vec<SearchResult>,
    matched: usize,
    match_bitmap: Option<MatchBitmap>,
}

impl<S, P> Clone for FuzzySearchSession<S, P>
where
    S: ItemsSource + Send + Sync + 'static,
    P: SearchSnapshotProvider<S>,
{
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<S, P> FuzzySearchSession<S, P>
where
    S: ItemsSource + Send + Sync + 'static,
    P: SearchSnapshotProvider<S>,
{
    pub fn new(
        session_id: u64,
        provider: Arc<P>,
        initial_query: String,
        updates_tx: Sender<FuzzySearchUpdate>,
    ) -> Self {
        Self::new_with_sort(
            session_id,
            provider,
            initial_query,
            updates_tx,
            SearchSortMode::Score,
        )
    }

    pub fn new_with_sort(
        session_id: u64,
        provider: Arc<P>,
        initial_query: String,
        updates_tx: Sender<FuzzySearchUpdate>,
        sort_mode: SearchSortMode,
    ) -> Self {
        let (signal_tx, signal_rx) = bounded(1);
        Self {
            inner: Arc::new(FuzzySearcherInner {
                session_id,
                provider,
                sort_mode,
                query: Mutex::new(initial_query),
                search_version: AtomicU64::new(0),
                cancelled: AtomicBool::new(false),
                signal_tx,
                signal_rx,
                updates_tx,
                _snapshot: PhantomData,
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

impl<S, P> FuzzySearcherInner<S, P>
where
    S: ItemsSource + Send + Sync + 'static,
    P: SearchSnapshotProvider<S>,
{
    fn run_loop(self: &Arc<Self>) {
        let mut last_completed_version = None;
        let mut last_snapshot_version = None;
        let mut last_source_done = false;
        let mut cache = SearchCache::default();
        loop {
            if self.cancelled.load(Ordering::Acquire) {
                return;
            }

            let (version, snapshot_version, scanning, source_done) = {
                (
                    self.search_version.load(Ordering::Acquire),
                    self.provider.snapshot_version(),
                    !self.provider.is_done(),
                    self.provider.is_done(),
                )
            };

            if last_completed_version != Some(version)
                || last_snapshot_version != Some(snapshot_version)
                || last_source_done != source_done
            {
                self.run_search_generation(version, snapshot_version, &mut cache);
                last_completed_version = Some(version);
                last_snapshot_version = Some(snapshot_version);
                last_source_done = source_done;
            }

            let timeout = if scanning {
                Duration::from_millis(50)
            } else {
                Duration::from_millis(250)
            };

            let _ = self.signal_rx.recv_timeout(timeout);
        }
    }

    fn run_search_generation(&self, version: u64, snapshot_version: u64, cache: &mut SearchCache) {
        let query = self.query.lock().expect("search query poisoned").clone();

        let Some(snapshot) = self.provider.snapshot() else {
            return;
        };

        let source_done = self.provider.is_done();
        let total = snapshot.len();

        let should_cancel = || {
            self.cancelled.load(Ordering::Acquire)
                || self.search_version.load(Ordering::Acquire) != version
        };

        let cacheable = is_plain_fuzzy_term(&query);
        let use_incremental = cache.query == query && cache.searched_len <= total;
        let use_extension_cache = cacheable
            && is_compatible_query_extension(&cache.query, &query)
            && cache.searched_len <= total
            && cache.match_bitmap.is_some();
        let output = if use_incremental {
            let delta = if cacheable {
                search_range_with_match_collection(
                    Arc::clone(&snapshot),
                    &query,
                    cache.searched_len..total,
                    self.sort_mode,
                    should_cancel,
                )
            } else {
                search_range_with_sort(
                    Arc::clone(&snapshot),
                    &query,
                    cache.searched_len..total,
                    self.sort_mode,
                    should_cancel,
                )
            };
            let Some(delta) = delta else {
                return;
            };
            merge_cached_search(cache, &query, delta, self.sort_mode)
        } else if use_extension_cache {
            let Some(output) = search_with_match_cache(
                Arc::clone(&snapshot),
                &query,
                self.sort_mode,
                should_cancel,
                cache.match_bitmap.as_ref(),
            ) else {
                return;
            };
            *cache = SearchCache {
                query,
                searched_len: output.total,
                results: output.results.clone(),
                matched: output.matched,
                match_bitmap: output.match_bitmap.clone(),
            };
            output
        } else {
            let output = if cacheable {
                search_with_match_cache(
                    Arc::clone(&snapshot),
                    &query,
                    self.sort_mode,
                    should_cancel,
                    None,
                )
            } else {
                search_with_sort(Arc::clone(&snapshot), &query, self.sort_mode, should_cancel)
            };
            let Some(output) = output else {
                return;
            };
            *cache = SearchCache {
                query,
                searched_len: output.total,
                results: output.results.clone(),
                matched: output.matched,
                match_bitmap: output.match_bitmap.clone(),
            };
            output
        };
        if should_cancel() {
            return;
        }

        let _ = self.updates_tx.send(FuzzySearchUpdate {
            session_id: self.session_id,
            generation: version,
            query: cache.query.clone(),
            results: output.results,
            matched: output.matched,
            searched: output.total,
            total,
            source_version: snapshot_version,
            source_done,
        });
    }
}

fn merge_cached_search(
    cache: &mut SearchCache,
    query: &str,
    delta: SearchOutput,
    sort_mode: SearchSortMode,
) -> SearchOutput {
    cache.query = query.to_owned();
    cache.searched_len = delta.total;
    cache.matched += delta.matched;
    if let Some(delta_bitmap) = &delta.match_bitmap {
        if let Some(bitmap) = &mut cache.match_bitmap {
            bitmap.merge(delta_bitmap);
        } else {
            cache.match_bitmap = Some(delta_bitmap.clone());
        }
    }

    if query.is_empty() {
        if cache.results.len() < RESULT_LIMIT {
            let remaining = RESULT_LIMIT - cache.results.len();
            cache
                .results
                .extend(delta.results.into_iter().take(remaining));
        }
    } else {
        cache.results.extend(delta.results);
        match sort_mode {
            SearchSortMode::Score => cache.results.sort_by(compare_search_results),
            SearchSortMode::SourceOrder => cache.results.sort_by_key(|result| result.node_index),
        }
        cache.results.truncate(RESULT_LIMIT);
    }

    SearchOutput {
        results: cache.results.clone(),
        matched: cache.matched,
        total: cache.searched_len,
        match_bitmap: cache.match_bitmap.clone(),
    }
}

fn is_plain_fuzzy_term(query: &str) -> bool {
    !query.is_empty()
        && !query.chars().any(|character| {
            character.is_whitespace() || matches!(character, '!' | '\'' | '^' | '$' | '|')
        })
}

fn is_compatible_query_extension(previous: &str, next: &str) -> bool {
    is_plain_fuzzy_term(previous)
        && is_plain_fuzzy_term(next)
        && next.len() > previous.len()
        && next.starts_with(previous)
}

fn compare_search_results(left: &SearchResult, right: &SearchResult) -> std::cmp::Ordering {
    right
        .score
        .cmp(&left.score)
        .then_with(|| left.path.len().cmp(&right.path.len()))
        .then_with(|| left.node_index.cmp(&right.node_index))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_appended_plain_fuzzy_terms_are_cache_compatible() {
        assert!(is_compatible_query_extension("ru", "rust"));
        assert!(is_compatible_query_extension("日", "日本"));
        assert!(!is_compatible_query_extension("rust", "rus"));
        assert!(!is_compatible_query_extension("foo", "foo bar"));
        assert!(!is_compatible_query_extension("^foo", "^foobar"));
        assert!(!is_compatible_query_extension("", "f"));
    }
}
