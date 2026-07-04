use nfm_file_system::walker::PublishedSnapshot;
use nfm_search_core::store::{FlatSnapshot, ItemsSource, StreamingItemSnapshot};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

pub enum AnyItemSource {
    FileSystem(Arc<PublishedSnapshot>),
    Flat(Arc<FlatSnapshot>),
    Streaming(Arc<StreamingItemSnapshot>),
}

impl ItemsSource for AnyItemSource {
    fn version(&self) -> u64 {
        match self {
            AnyItemSource::FileSystem(source) => source.version(),
            AnyItemSource::Flat(source) => source.version(),
            AnyItemSource::Streaming(source) => source.version(),
        }
    }

    fn len(&self) -> usize {
        match self {
            AnyItemSource::FileSystem(source) => source.len(),
            AnyItemSource::Flat(source) => source.len(),
            AnyItemSource::Streaming(source) => source.len(),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            AnyItemSource::FileSystem(source) => source.is_empty(),
            AnyItemSource::Flat(source) => source.is_empty(),
            AnyItemSource::Streaming(source) => source.is_empty(),
        }
    }

    fn get_string<'a>(
        &'a self,
        index: usize,
        stack_buffer: &'a mut [u8],
        heap_buffer: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        match self {
            AnyItemSource::FileSystem(source) => {
                source.get_string(index, stack_buffer, heap_buffer)
            }
            AnyItemSource::Flat(source) => source.get_string(index, stack_buffer, heap_buffer),
            AnyItemSource::Streaming(source) => source.get_string(index, stack_buffer, heap_buffer),
        }
    }

    fn get_string_lossy(&self, node_index: usize, out: &mut Vec<u8>) -> String {
        match self {
            AnyItemSource::FileSystem(source) => source.get_string_lossy(node_index, out),
            AnyItemSource::Flat(source) => source.get_string_lossy(node_index, out),
            AnyItemSource::Streaming(source) => source.get_string_lossy(node_index, out),
        }
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
