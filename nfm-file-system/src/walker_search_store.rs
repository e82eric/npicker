use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicBool, Ordering};
use nfm_search_core::fuzzy_search_session::SearchSnapshotProvider;
use nfm_search_core::store::ItemsSource;
use crate::walker::{PublishedSnapshot, ScanEventSink, ScanStatus};

pub struct FileSystemSearchStore {
    snapshot: RwLock<Option<Arc<PublishedSnapshot>>>,
    done: AtomicBool,
}

impl FileSystemSearchStore {
    pub fn new() -> Self {
        Self {
            snapshot: RwLock::new(None),
            done: AtomicBool::new(false),
        }
    }

    fn snapshot(&self) -> Option<Arc<PublishedSnapshot>> {
        self.snapshot.read().expect("file system search store poisoned").clone()
    }

    fn snapshot_version(&self) -> u64 {
        self.snapshot
            .read()
            .expect("file system search store poisoned")
            .as_ref()
            .map_or(0, |snapshot| snapshot.version())
    }

    fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

impl SearchSnapshotProvider<PublishedSnapshot> for FileSystemSearchStore {
    fn snapshot(&self) -> Option<Arc<PublishedSnapshot>> {
        FileSystemSearchStore::snapshot(self)
    }

    fn snapshot_version(&self) -> u64 {
        FileSystemSearchStore::snapshot_version(self)
    }

    fn is_done(&self) -> bool {
        FileSystemSearchStore::is_done(self)
    }
}

impl ScanEventSink for FileSystemSearchStore {
    fn snapshot(&self, snapshot: Arc<PublishedSnapshot>) {
        *self.snapshot.write().expect("file system search store poisoned") = Some(snapshot);
    }

    fn complete(&self, _status: ScanStatus) {
        self.done.store(true, Ordering::Release);
    }
}
