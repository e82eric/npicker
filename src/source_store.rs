#[cfg(windows)]
use nfm_file_system::walker::{PublishedSnapshot, ScanEventSink, ScanStatus};
use nfm_search_core::fuzzy_search_session::SearchSnapshotProvider;
use nfm_search_core::store::{FlatSnapshot, ItemsSource, StreamingItemSnapshot};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

use crate::delimited_store::{DelimitedItemMetadata, DelimitedStreamingSnapshot};

pub enum AnyItemSource {
    #[cfg(windows)]
    FileSystem(Arc<PublishedSnapshot>),
    Flat(Arc<FlatSnapshot<()>>),
    Streaming(Arc<StreamingItemSnapshot>),
    Delimited(Arc<DelimitedStreamingSnapshot>),
}

impl ItemsSource for AnyItemSource {
    fn version(&self) -> u64 {
        match self {
            #[cfg(windows)]
            AnyItemSource::FileSystem(source) => source.version(),
            AnyItemSource::Flat(source) => source.version(),
            AnyItemSource::Streaming(source) => source.version(),
            AnyItemSource::Delimited(source) => source.version(),
        }
    }

    fn len(&self) -> usize {
        match self {
            #[cfg(windows)]
            AnyItemSource::FileSystem(source) => source.len(),
            AnyItemSource::Flat(source) => source.len(),
            AnyItemSource::Streaming(source) => source.len(),
            AnyItemSource::Delimited(source) => source.len(),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            #[cfg(windows)]
            AnyItemSource::FileSystem(source) => source.is_empty(),
            AnyItemSource::Flat(source) => source.is_empty(),
            AnyItemSource::Streaming(source) => source.is_empty(),
            AnyItemSource::Delimited(source) => source.is_empty(),
        }
    }

    fn get_string<'a>(
        &'a self,
        index: usize,
        stack_buffer: &'a mut [u8],
        heap_buffer: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        match self {
            #[cfg(windows)]
            AnyItemSource::FileSystem(source) => {
                source.get_string(index, stack_buffer, heap_buffer)
            }
            AnyItemSource::Flat(source) => source.get_string(index, stack_buffer, heap_buffer),
            AnyItemSource::Streaming(source) => source.get_string(index, stack_buffer, heap_buffer),
            AnyItemSource::Delimited(source) => source.get_string(index, stack_buffer, heap_buffer),
        }
    }

    fn get_string_lossy(&self, node_index: usize, out: &mut Vec<u8>) -> String {
        match self {
            #[cfg(windows)]
            AnyItemSource::FileSystem(source) => source.get_string_lossy(node_index, out),
            AnyItemSource::Flat(source) => source.get_string_lossy(node_index, out),
            AnyItemSource::Streaming(source) => source.get_string_lossy(node_index, out),
            AnyItemSource::Delimited(source) => source.get_string_lossy(node_index, out),
        }
    }
}

impl AnyItemSource {
    pub fn delimited_metadata(&self, node_index: usize) -> Option<DelimitedItemMetadata> {
        let Self::Delimited(source) = self else {
            return None;
        };
        Some(source.metadata(node_index))
    }
}

pub struct SharedStore {
    published: RwLock<Option<Arc<AnyItemSource>>>,
    done: AtomicBool,
}

impl SharedStore {
    pub fn new() -> Self {
        Self {
            published: RwLock::new(None),
            done: AtomicBool::new(false),
        }
    }

    pub fn completed(source: Arc<AnyItemSource>) -> Self {
        Self {
            published: RwLock::new(Some(source)),
            done: AtomicBool::new(true),
        }
    }

    pub fn snapshot(&self) -> Option<Arc<AnyItemSource>> {
        self.published.read().expect("store poisoned").clone()
    }

    pub fn publish(&self, source: Arc<AnyItemSource>) {
        *self.published.write().expect("store poisoned") = Some(source);
    }

    pub fn snapshot_version(&self) -> u64 {
        self.published
            .read()
            .expect("store poisoned")
            .as_ref()
            .map_or(0, |snapshot| snapshot.version())
    }

    pub fn complete(&self) {
        self.done.store(true, Ordering::Release);
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

impl SearchSnapshotProvider<AnyItemSource> for SharedStore {
    fn snapshot(&self) -> Option<Arc<AnyItemSource>> {
        SharedStore::snapshot(self)
    }

    fn snapshot_version(&self) -> u64 {
        SharedStore::snapshot_version(self)
    }

    fn is_done(&self) -> bool {
        SharedStore::is_done(self)
    }
}

#[cfg(windows)]
impl ScanEventSink for SharedStore {
    fn snapshot(&self, snapshot: Arc<PublishedSnapshot>) {
        self.publish(Arc::new(AnyItemSource::FileSystem(snapshot)));
    }

    fn complete(&self, _status: ScanStatus) {
        SharedStore::complete(self);
    }
}
