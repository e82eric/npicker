use crate::fuzzy_search_session::SearchSnapshotProvider;
use crate::store::ItemsSource;
use crossbeam_channel::{Sender, TrySendError};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

pub struct SnapshotStore<S: ItemsSource> {
    snapshot: RwLock<Option<Arc<S>>>,
    done: AtomicBool,
    append_only: bool,
    subscribers: Mutex<Vec<Sender<()>>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{fuzzy_search_session::FuzzySearchSession, store::FlatSnapshot};
    #[test]
    fn publications_and_completion_coalesce_and_prune_closed_subscribers() {
        let store = SnapshotStore::new();
        let (wake, receive) = crossbeam_channel::bounded(1);
        assert!(store.subscribe_updates(wake));
        store.publish(Arc::new(FlatSnapshot::from_items([("first", 1u64)])));
        store.complete();
        assert_eq!(receive.len(), 1);
        receive.try_recv().unwrap();
        store.publish(Arc::new(FlatSnapshot::from_items([("second", 2u64)])));
        receive.try_recv().unwrap();
        drop(receive);
        store.complete();
        assert!(store.subscribers.lock().unwrap().is_empty());
    }
    #[test]
    fn subscribed_search_observes_publication_and_completion_without_polling() {
        let store = Arc::new(SnapshotStore::<FlatSnapshot<u64>>::new());
        let (send, receive) = crossbeam_channel::unbounded();
        let session = FuzzySearchSession::new(1, store.clone(), String::new(), send);
        session.start();
        store.publish(Arc::new(FlatSnapshot::from_items([("first", 1u64)])));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let update = receive
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap();
            if update.total == 1 {
                break;
            }
        }
        store.complete();
        loop {
            let update = receive
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .unwrap();
            if update.source_done {
                break;
            }
        }
        session.stop();
    }
}

impl<S: ItemsSource> SnapshotStore<S> {
    pub fn new() -> Self {
        Self {
            snapshot: RwLock::new(None),
            done: AtomicBool::new(false),
            append_only: false,
            subscribers: Mutex::new(Vec::new()),
        }
    }

    /// Use only for immutable append-only streams: existing indices, searchable
    /// text and payloads must retain their meaning across every publication.
    pub fn new_append_only() -> Self {
        Self {
            append_only: true,
            ..Self::new()
        }
    }
    pub fn snapshot(&self) -> Option<Arc<S>> {
        self.snapshot
            .read()
            .expect("snapshot store poisoned")
            .clone()
    }

    pub fn publish(&self, snapshot: Arc<S>) {
        *self.snapshot.write().expect("snapshot store poisoned") = Some(snapshot);
        self.notify();
    }

    pub fn complete(&self) {
        self.done.store(true, Ordering::Release);
        self.notify();
    }
    fn notify(&self) {
        self.subscribers
            .lock()
            .expect("snapshot subscribers poisoned")
            .retain(|wake| !matches!(wake.try_send(()), Err(TrySendError::Disconnected(_))));
    }

    pub fn snapshot_version(&self) -> u64 {
        self.snapshot
            .read()
            .expect("snapshot store poisoned")
            .as_ref()
            .map_or(0, |snapshot| snapshot.version())
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}
impl<S> SearchSnapshotProvider<S> for SnapshotStore<S>
where
    S: ItemsSource + Send + Sync + 'static,
{
    fn snapshot(&self) -> Option<Arc<S>> {
        SnapshotStore::snapshot(self)
    }

    fn snapshot_version(&self) -> u64 {
        SnapshotStore::snapshot_version(self)
    }

    fn is_append_only(&self) -> bool {
        self.append_only
    }
    fn is_done(&self) -> bool {
        SnapshotStore::is_done(self)
    }
    fn subscribe_updates(&self, wake: Sender<()>) -> bool {
        self.subscribers
            .lock()
            .expect("snapshot subscribers poisoned")
            .push(wake);
        true
    }
}
