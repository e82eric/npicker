use crate::{ItemsSource, SearchSnapshotProvider, SearchSortMode};
use egui::Context;
use nfm_search_core::{fuzzy_search_session::FuzzySearchSession, search::SearchOutput};
use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

#[derive(Clone, Default, Debug)]
pub struct SearchTimings {
    pub started: Option<Instant>,
    pub first_snapshot_ready: Option<Instant>,
    pub first_update_ready: Option<Instant>,
    pub first_matches_ready: Option<Instant>,
    pub first_matches_taken: Option<Instant>,
}
type WakeCallback = Arc<dyn Fn(SearchTimings) + Send + Sync>;

pub(crate) struct Update<S> {
    pub revision: u64,
    pub query: String,
    pub snapshot: Arc<S>,
    pub output: SearchOutput,
    pub done: bool,
    pub append_only: bool,
}
struct Provider<S> {
    snapshot: Box<dyn Fn() -> Option<Arc<S>> + Send + Sync>,
    version: Box<dyn Fn() -> u64 + Send + Sync>,
    done: Box<dyn Fn() -> bool + Send + Sync>,
    append_only: bool,
    timings: Arc<Mutex<SearchTimings>>,
    subscribe: Box<dyn Fn(crossbeam_channel::Sender<()>) -> bool + Send + Sync>,
}
impl<S: ItemsSource + Send + Sync + 'static> SearchSnapshotProvider<S> for Provider<S> {
    fn snapshot(&self) -> Option<Arc<S>> {
        let snapshot = (self.snapshot)();
        if snapshot.as_ref().is_some_and(|s| s.len() > 0) {
            self.timings
                .lock()
                .unwrap()
                .first_snapshot_ready
                .get_or_insert_with(Instant::now);
        }
        snapshot
    }
    fn snapshot_version(&self) -> u64 {
        (self.version)()
    }
    fn is_done(&self) -> bool {
        (self.done)()
    }
    fn is_append_only(&self) -> bool {
        self.append_only
    }
    fn subscribe_updates(&self, wake: crossbeam_channel::Sender<()>) -> bool {
        (self.subscribe)(wake)
    }
}

/// Use the same incremental search session as Ghostty, with a coalescing
/// snapshot-aware observer rather than a second full-rescan worker.
pub(crate) struct Session<S: ItemsSource + Send + Sync + 'static> {
    search: FuzzySearchSession<S, Provider<S>>,
    latest: Arc<Mutex<Option<Update<S>>>>,
    revision: u64,
    timings: Arc<Mutex<SearchTimings>>,
    wake_callback: Arc<Mutex<Option<WakeCallback>>>,
}
impl<S: ItemsSource + Send + Sync + 'static> Session<S> {
    pub fn new<P: SearchSnapshotProvider<S> + ?Sized>(
        provider: Arc<P>,
        query: String,
        sort: SearchSortMode,
        ctx: Context,
    ) -> Self {
        let timings = Arc::new(Mutex::new(SearchTimings {
            started: Some(Instant::now()),
            ..Default::default()
        }));
        let wake_callback: Arc<Mutex<Option<WakeCallback>>> = Arc::new(Mutex::new(None));
        let append_only = provider.is_append_only();
        let (snapshot_provider, version_provider, done_provider, notification_provider) = (
            provider.clone(),
            provider.clone(),
            provider.clone(),
            provider,
        );
        let provider = Arc::new(Provider {
            snapshot: Box::new(move || snapshot_provider.snapshot()),
            version: Box::new(move || version_provider.snapshot_version()),
            done: Box::new(move || done_provider.is_done()),
            append_only,
            timings: timings.clone(),
            subscribe: Box::new(move |wake| notification_provider.subscribe_updates(wake)),
        });
        let latest = Arc::new(Mutex::new(None));
        let worker_latest = latest.clone();
        let worker_timings = timings.clone();
        let worker_wake = wake_callback.clone();
        let search = FuzzySearchSession::new_with_snapshot_observer(
            0,
            provider,
            query,
            sort,
            move |update, snapshot| {
                let mut output = SearchOutput::default();
                output.results = update.results;
                output.matched = update.matched;
                output.total = update.total;
                let has_matches = output.matched > 0;
                let ready = {
                    let mut timings = worker_timings.lock().unwrap();
                    timings.first_update_ready.get_or_insert_with(Instant::now);
                    if has_matches {
                        timings.first_matches_ready.get_or_insert_with(Instant::now);
                    }
                    timings.clone()
                };
                let incoming = Update {
                    revision: update.generation,
                    query: update.query,
                    snapshot,
                    output,
                    done: update.source_done,
                    append_only,
                };
                // Release the UI mailbox before destroying superseded results.
                drop(replace_pending(&worker_latest, incoming));
                let callback = worker_wake.lock().unwrap().clone();
                if let Some(callback) = callback {
                    callback(ready);
                }
                ctx.request_repaint();
            },
        );
        search.start();
        Self {
            search,
            latest,
            revision: 0,
            timings,
            wake_callback,
        }
    }
    pub fn set_query(&mut self, query: String) -> u64 {
        self.revision += 1;
        self.search.set_query(query);
        self.revision
    }
    pub fn take(&self) -> Option<Update<S>> {
        let update = self.latest.lock().unwrap().take();
        if update.as_ref().is_some_and(|u| u.output.matched > 0) {
            self.timings
                .lock()
                .unwrap()
                .first_matches_taken
                .get_or_insert_with(Instant::now);
        }
        update
    }
    pub fn timings(&self) -> SearchTimings {
        self.timings.lock().unwrap().clone()
    }
    pub fn set_wake_callback(&self, callback: WakeCallback) {
        *self.wake_callback.lock().unwrap() = Some(callback.clone());
        // A result may have arrived before the host attached its wake callback.
        let timings = self.timings();
        if timings.first_update_ready.is_some() {
            callback(timings);
        }
    }
    pub fn stop(&self) {
        self.search.stop();
    }
}
impl<S: ItemsSource + Send + Sync + 'static> Drop for Session<S> {
    fn drop(&mut self) {
        self.search.stop();
    }
}

// Return ownership after releasing the mailbox guard. The caller must dispose
// of old results outside the lock used by the UI.
fn replace_pending<T>(latest: &Mutex<Option<T>>, incoming: T) -> Option<T> {
    latest.lock().unwrap().replace(incoming)
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    use std::sync::{
        Weak,
        atomic::{AtomicBool, Ordering},
    };
    struct CheckDrop {
        latest: Weak<Mutex<Option<CheckDrop>>>,
        unlocked: Arc<AtomicBool>,
    }
    impl Drop for CheckDrop {
        fn drop(&mut self) {
            if let Some(latest) = self.latest.upgrade() {
                self.unlocked
                    .store(latest.try_lock().is_ok(), Ordering::Release);
            }
        }
    }
    #[test]
    fn superseded_results_are_destroyed_without_holding_the_ui_mailbox() {
        let latest = Arc::new(Mutex::new(None));
        let unlocked = Arc::new(AtomicBool::new(false));
        let item = || CheckDrop {
            latest: Arc::downgrade(&latest),
            unlocked: unlocked.clone(),
        };
        *latest.lock().unwrap() = Some(item());
        drop(replace_pending(&latest, item()));
        assert!(unlocked.load(Ordering::Acquire));
    }
}
