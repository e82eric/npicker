use std::sync::Arc;
use crossbeam_channel::Sender;
use nfm_search_core::fuzzy_search_session::{FuzzySearchSession, FuzzySearchUpdate};
use crate::walker::{start_scan, FileWalkerScan, PublishedSnapshot, ScanOptions};
use crate::walker_search_store::FileSystemSearchStore;

pub struct FileSystemSearch {
    search: FuzzySearchSession<PublishedSnapshot, FileSystemSearchStore>,
    scan: FileWalkerScan,
    store: Arc<FileSystemSearchStore>,
}

impl FileSystemSearch {
    pub fn start(
        session_id: u64,
        options: ScanOptions,
        initial_query: String,
        updates_tx: Sender<FuzzySearchUpdate>,
    ) -> Self {
        let store = Arc::new(FileSystemSearchStore::new());

        let scan = start_scan(options, Arc::clone(&store));

        let search = FuzzySearchSession::new(
            session_id,
            Arc::clone(&store),
            initial_query,
            updates_tx,
        );

        search.start();

        Self {
            search,
            scan,
            store
        }
    }

    pub fn set_query(&self, query: String) {
        self.search.set_query(query);
    }

    pub fn stop(&self) {
        self.scan.cancel();
        self.search.stop();
    }
}

impl Drop for FileSystemSearch {
    fn drop(&mut self) {
        self.stop();
    }
}