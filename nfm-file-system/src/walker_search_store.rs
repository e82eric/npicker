use crate::walker::{PublishedSnapshot, ScanEventSink, ScanStatus};
use nfm_search_core::snapshot_store::SnapshotStore;
use std::sync::Arc;

pub type FileSystemSearchStore = SnapshotStore<PublishedSnapshot>;

impl ScanEventSink for FileSystemSearchStore {
    fn snapshot(&self, snapshot: Arc<PublishedSnapshot>) {
        self.publish(snapshot);
    }

    fn complete(&self, _status: ScanStatus) {
        SnapshotStore::complete(self);
    }
}
