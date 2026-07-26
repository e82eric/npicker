use crate::fuzzy_search_session::SearchSnapshotProvider;
use crate::store::ItemsSource;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

pub struct SnapshotStore<S: ItemsSource> {
    snapshot: RwLock<Option<Arc<S>>>,
    done: AtomicBool,
}

impl<S: ItemsSource> SnapshotStore<S> {
    pub fn new() -> Self {
        Self {
            snapshot: RwLock::new(None),
            done: AtomicBool::new(false),
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
    }

    pub fn complete(&self) {
        self.done.store(true, Ordering::Release);
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

    fn is_done(&self) -> bool {
        SnapshotStore::is_done(self)
    }
}
