use crate::fuzzy_search_session::SearchSnapshotProvider;
use crate::snapshot_store::SnapshotStore;
use std::cmp::Ordering as CmpOrdering;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

pub trait SearchPlan: Send + Sync {
    fn fuzzy_query(&self) -> &str;

    fn filters_items(&self) -> bool {
        false
    }

    fn includes(&self, _index: usize) -> bool {
        true
    }

    fn compare(&self, _left: usize, _right: usize) -> CmpOrdering {
        CmpOrdering::Equal
    }

    fn has_custom_sort(&self) -> bool {
        false
    }

    fn display_text(&self, _index: usize) -> Option<String> {
        None
    }
}

struct PlainSearchPlan {
    query: String,
}

impl SearchPlan for PlainSearchPlan {
    fn fuzzy_query(&self) -> &str {
        &self.query
    }
}

const ITEM_CHUNK_SIZE: usize = 64 * 1024;
const BYTE_CHUNK_SIZE: usize = 1024 * 1024;
const PUBLISH_ITEM_INTERVAL: usize = 1_000;

pub trait ItemsSource {
    fn version(&self) -> u64;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool;
    fn get_string<'a>(
        &'a self,
        index: usize,
        stack_buffer: &'a mut [u8],
        heap_buffer: &'a mut Vec<u8>,
    ) -> &'a [u8];

    fn get_string_lossy(&self, node_index: usize, out: &mut Vec<u8>) -> String;

    fn create_search_plan(&self, query: &str) -> Box<dyn SearchPlan + '_> {
        Box::new(PlainSearchPlan {
            query: query.to_owned(),
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct FlatItem {
    offset: usize,
    len: usize,
}

impl Default for FlatItem {
    fn default() -> Self {
        Self { offset: 0, len: 0 }
    }
}

pub struct FlatSnapshot<T> {
    items: Vec<FlatItem>,
    payloads: Vec<T>,
    bytes: Vec<u8>,
    version: u64,
}

impl<T> FlatSnapshot<T> {
    pub fn from_items<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = (S, T)>,
        S: AsRef<str>,
    {
        let mut flat_items = Vec::new();
        let mut bytes = Vec::new();
        let mut payloads = Vec::new();

        for (item, payload) in items {
            let item = item.as_ref();
            let offset = bytes.len();
            bytes.extend_from_slice(item.as_bytes());
            flat_items.push(FlatItem {
                offset,
                len: item.len(),
            });

            payloads.push(payload);
        }

        Self {
            items: flat_items,
            payloads,
            bytes,
            version: 1,
        }
    }

    pub fn payload(&self, index: usize) -> &T {
        &self.payloads[index]
    }

    fn item_bytes(&self, index: usize) -> &[u8] {
        let item = self.items[index];
        let start = item.offset;
        let end = start + item.len;
        &self.bytes[start..end]
    }
}

pub struct StreamingItemStore {
    items: ChunkedStorage<FlatItem>,
    bytes: ChunkedStorage<u8>,
    published: Arc<StreamingItemSnapshot>,
    version: AtomicU64,
}

impl StreamingItemStore {
    pub fn new() -> Self {
        Self {
            items: ChunkedStorage::new(ITEM_CHUNK_SIZE),
            bytes: ChunkedStorage::new(BYTE_CHUNK_SIZE),
            published: Arc::new(StreamingItemSnapshot::empty()),
            version: AtomicU64::new(0),
        }
    }

    pub fn add_item(&mut self, item: &[u8]) -> u32 {
        let flat_item = FlatItem {
            offset: self.bytes.len(),
            len: item.len(),
        };

        let item_index = self.items.len() as u32;
        self.bytes.extend_from_slice(item);
        self.items.push(flat_item);

        if self.items.len() % PUBLISH_ITEM_INTERVAL == 0 {
            self.publish();
        }

        item_index
    }

    pub fn publish(&mut self) {
        let version = self.version.fetch_add(1, Ordering::Relaxed) + 1;
        self.published = Arc::new(StreamingItemSnapshot {
            items: self.items.snapshot(),
            items_count: self.items.len(),
            bytes: self.bytes.snapshot(),
            version,
        });
    }

    pub fn snapshot(&self) -> Arc<StreamingItemSnapshot> {
        Arc::clone(&self.published)
    }

    pub fn complete_adding(&mut self) {
        self.publish();
    }
}

pub struct StreamingItemSnapshot {
    items: ChunkedSnapshot<FlatItem>,
    items_count: usize,
    bytes: ChunkedSnapshot<u8>,
    version: u64,
}

impl StreamingItemSnapshot {
    fn empty() -> Self {
        Self {
            items: ChunkedSnapshot::empty(),
            items_count: 0,
            bytes: ChunkedSnapshot::empty(),
            version: 0,
        }
    }
}

impl ItemsSource for StreamingItemSnapshot {
    fn version(&self) -> u64 {
        self.version
    }

    fn len(&self) -> usize {
        self.items_count
    }

    fn is_empty(&self) -> bool {
        self.items_count == 0
    }

    fn get_string<'a>(
        &'a self,
        index: usize,
        stack_buffer: &'a mut [u8],
        heap_buffer: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        debug_assert!(index < self.items_count);
        let item = self.items[index];
        self.bytes
            .get_range(item.offset, item.len, stack_buffer, heap_buffer)
    }

    fn get_string_lossy(&self, node_index: usize, out: &mut Vec<u8>) -> String {
        let mut stack_buffer = [0u8; 4096];
        let result = self.get_string(node_index, &mut stack_buffer, out);
        String::from_utf8_lossy(result).into_owned()
    }
}

impl<T: Copy> ItemsSource for StreamingItemSnapshotWithPayload<T> {
    fn version(&self) -> u64 {
        self.version
    }

    fn len(&self) -> usize {
        self.items_count
    }

    fn is_empty(&self) -> bool {
        self.items_count == 0
    }

    fn get_string<'a>(
        &'a self,
        index: usize,
        stack_buffer: &'a mut [u8],
        heap_buffer: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        debug_assert!(index < self.items_count);
        let item = self.items[index];
        self.bytes
            .get_range(item.offset, item.len, stack_buffer, heap_buffer)
    }

    fn get_string_lossy(&self, node_index: usize, out: &mut Vec<u8>) -> String {
        let mut stack_buffer = [0u8; 4096];
        let result = self.get_string(node_index, &mut stack_buffer, out);
        String::from_utf8_lossy(result).into_owned()
    }
}

pub struct StreamingItemSnapshotWithPayload<T: Copy> {
    items: ChunkedSnapshot<FlatItem>,
    items_count: usize,
    payloads: ChunkedSnapshot<T>,
    bytes: ChunkedSnapshot<u8>,
    version: u64,
}

impl<T: Copy> StreamingItemSnapshotWithPayload<T> {
    fn empty() -> Self {
        Self {
            items: ChunkedSnapshot::empty(),
            items_count: 0,
            payloads: ChunkedSnapshot::empty(),
            bytes: ChunkedSnapshot::empty(),
            version: 0,
        }
    }

    pub fn payload(&self, index: usize) -> &T {
        debug_assert!(index < self.items_count);
        &self.payloads[index]
    }
}

pub struct StreamingItemStoreWithPayload<T: Copy + Default> {
    items: ChunkedStorage<FlatItem>,
    bytes: ChunkedStorage<u8>,
    payloads: ChunkedStorage<T>,
    published: Arc<StreamingItemSnapshotWithPayload<T>>,
    version: AtomicU64,
    done: AtomicBool,
}

pub struct AddItemResult {
    pub item_index: u32,
    pub published: bool,
}

impl<T: Copy + Default> StreamingItemStoreWithPayload<T> {
    pub fn new() -> Self {
        Self {
            items: ChunkedStorage::new(ITEM_CHUNK_SIZE),
            bytes: ChunkedStorage::new(BYTE_CHUNK_SIZE),
            payloads: ChunkedStorage::new(ITEM_CHUNK_SIZE),
            published: Arc::new(StreamingItemSnapshotWithPayload::empty()),
            version: AtomicU64::new(0),
            done: AtomicBool::new(false),
        }
    }

    pub fn add_item(&mut self, item: &[u8], payload: T) -> AddItemResult {
        let flat_item = FlatItem {
            offset: self.bytes.len(),
            len: item.len(),
        };

        let item_index = self.items.len() as u32;
        self.bytes.extend_from_slice(item);
        self.items.push(flat_item);
        self.payloads.push(payload);

        debug_assert_eq!(self.items.len(), self.payloads.len());

        let published = if self.items.len() % PUBLISH_ITEM_INTERVAL == 0 {
            self.publish();
            true
        } else {
            false
        };

        AddItemResult {
            item_index,
            published,
        }
    }

    pub fn publish(&mut self) {
        debug_assert_eq!(self.items.len(), self.payloads.len());

        let version = self.version.fetch_add(1, Ordering::Relaxed) + 1;
        self.published = Arc::new(StreamingItemSnapshotWithPayload {
            items: self.items.snapshot(),
            items_count: self.items.len(),
            payloads: self.payloads.snapshot(),
            bytes: self.bytes.snapshot(),
            version,
        });
    }

    pub fn snapshot(&self) -> Arc<StreamingItemSnapshotWithPayload<T>> {
        Arc::clone(&self.published)
    }

    pub fn complete_adding(&mut self) {
        self.publish();
        self.done.store(true, Ordering::Release);
    }
}

pub struct PublishingStreamingItemStoreWithPayload<T: Copy + Default> {
    store: StreamingItemStoreWithPayload<T>,
    publisher: Arc<SnapshotStore<StreamingItemSnapshotWithPayload<T>>>,
}

impl<T: Copy + Default> PublishingStreamingItemStoreWithPayload<T> {
    pub fn new(publisher: Arc<SnapshotStore<StreamingItemSnapshotWithPayload<T>>>) -> Self {
        Self {
            store: StreamingItemStoreWithPayload::new(),
            publisher,
        }
    }

    pub fn add_item(&mut self, item: &[u8], payload: T) -> u32 {
        let result = self.store.add_item(item, payload);

        if result.published {
            self.publisher.publish(self.store.snapshot());
        }

        result.item_index
    }

    pub fn publish(&mut self) {
        self.store.publish();
        self.publisher.publish(self.store.snapshot());
    }

    pub fn complete(&mut self) {
        self.store.complete_adding();
        self.publisher.publish(self.store.snapshot());
        self.publisher.complete();
    }
}

impl<T: Copy + Default + Send + Sync + 'static>
    SearchSnapshotProvider<StreamingItemSnapshotWithPayload<T>>
    for StreamingItemStoreWithPayload<T>
{
    fn snapshot(&self) -> Option<Arc<StreamingItemSnapshotWithPayload<T>>> {
        Some(StreamingItemStoreWithPayload::snapshot(self))
    }

    fn snapshot_version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

impl<T> ItemsSource for FlatSnapshot<T> {
    fn version(&self) -> u64 {
        self.version
    }

    fn len(&self) -> usize {
        self.items.len()
    }

    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn get_string<'a>(
        &'a self,
        index: usize,
        _stack_buffer: &'a mut [u8],
        _heap_buffer: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        self.item_bytes(index)
    }

    fn get_string_lossy(&self, node_index: usize, _out: &mut Vec<u8>) -> String {
        String::from_utf8_lossy(self.item_bytes(node_index)).into_owned()
    }
}

pub struct ChunkedStorage<T: Copy> {
    chunk_size: usize,
    len: usize,
    sealed_len: usize,
    chunks: Vec<Arc<[T]>>,
    current: Vec<T>,
}

impl<T: Copy + Default> ChunkedStorage<T> {
    pub fn new(chunk_size: usize) -> Self {
        Self {
            chunk_size,
            len: 0,
            sealed_len: 0,
            chunks: Vec::new(),
            current: Vec::with_capacity(chunk_size),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn push(&mut self, item: T) {
        self.current.push(item);
        self.len += 1;

        if self.current.len() == self.chunk_size {
            self.seal_current();
        }
    }

    pub fn extend_from_slice(&mut self, items: &[T]) {
        for &item in items {
            self.push(item);
        }
    }

    fn seal_current(&mut self) {
        if self.current.is_empty() {
            return;
        }

        debug_assert_eq!(self.current.len(), self.chunk_size);
        let sealed = std::mem::replace(&mut self.current, Vec::with_capacity(self.chunk_size));
        self.sealed_len += sealed.len();
        self.chunks.push(Arc::from(sealed.into_boxed_slice()));
    }

    pub fn snapshot(&self) -> ChunkedSnapshot<T> {
        let mut chunks = self.chunks.clone();
        if !self.current.is_empty() {
            chunks.push(Arc::from(self.current.clone().into_boxed_slice()));
        }

        ChunkedSnapshot {
            chunks: Arc::new(chunks),
            chunk_size: self.chunk_size,
            len: self.len,
        }
    }

    pub fn eq_slice(&self, offset: usize, items: &[T]) -> bool
    where
        T: Eq,
    {
        if offset + items.len() > self.len {
            return false;
        }

        for (index, item) in items.iter().enumerate() {
            if *self.get(offset + index) != *item {
                return false;
            }
        }

        true
    }
}

impl<T: Copy> ChunkedStorage<T> {
    fn get(&self, index: usize) -> &T {
        assert!(index < self.len);
        if index >= self.sealed_len {
            return &self.current[index - self.sealed_len];
        }

        let chunk_index = index / self.chunk_size;
        let offset = index % self.chunk_size;
        &self.chunks[chunk_index][offset]
    }
}

pub struct ChunkedSnapshot<T: Copy> {
    pub chunks: Arc<Vec<Arc<[T]>>>,
    chunk_size: usize,
    len: usize,
}

impl<T: Copy> ChunkedSnapshot<T> {
    pub fn empty() -> Self {
        Self {
            chunks: Arc::new(Vec::new()),
            chunk_size: 1,
            len: 0,
        }
    }

    pub fn locate_direct(&self, index: usize) -> (usize, usize) {
        (index / self.chunk_size, index % self.chunk_size)
    }
}

impl ChunkedSnapshot<u8> {
    pub fn get_range<'a>(
        &'a self,
        offset: usize,
        length: usize,
        stack_buffer: &'a mut [u8],
        heap_buffer: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        assert!(offset <= self.len && length <= self.len - offset);
        if length == 0 {
            return &stack_buffer[..0];
        }

        let (chunk_index, chunk_offset) = self.locate_direct(offset);
        let chunk = &self.chunks[chunk_index];
        if chunk_offset + length <= chunk.len() {
            return &chunk[chunk_offset..chunk_offset + length];
        }

        if length > stack_buffer.len() {
            heap_buffer.resize(length, 0);
            self.copy_range_to(offset, heap_buffer);
            return heap_buffer;
        }

        self.copy_range_to(offset, &mut stack_buffer[..length]);
        &stack_buffer[..length]
    }

    fn copy_range_to(&self, mut offset: usize, mut target: &mut [u8]) {
        while !target.is_empty() {
            let (chunk_index, chunk_offset) = self.locate_direct(offset);
            let chunk = &self.chunks[chunk_index];
            let readable = target.len().min(chunk.len() - chunk_offset);
            target[..readable].copy_from_slice(&chunk[chunk_offset..chunk_offset + readable]);
            offset += readable;
            target = &mut target[readable..];
        }
    }
}

impl<T: Copy> std::ops::Index<usize> for ChunkedSnapshot<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        let (chunk_index, offset) = self.locate_direct(index);
        &self.chunks[chunk_index][offset]
    }
}

impl<T: Copy> std::ops::Index<usize> for ChunkedStorage<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_snapshot_returns_direct_item_bytes() {
        let snapshot = FlatSnapshot::from_items([("alpha", ()), ("beta\\gamma", ())]);
        let mut stack = [0u8; 16];
        let mut heap = Vec::new();

        assert_eq!(snapshot.get_string(0, &mut stack, &mut heap), b"alpha");
        assert_eq!(
            snapshot.get_string(1, &mut stack, &mut heap),
            b"beta\\gamma"
        );
        assert!(heap.is_empty());
    }

    #[test]
    fn flat_snapshot_materializes_lossy_strings() {
        let snapshot = FlatSnapshot::from_items([("one", ()), ("two", ())]);
        let mut out = Vec::new();

        assert_eq!(snapshot.get_string_lossy(1, &mut out), "two");
    }

    #[test]
    fn chunked_byte_snapshot_reads_contiguous_and_cross_chunk_ranges() {
        let mut storage = ChunkedStorage::new(4);
        storage.extend_from_slice(b"abcdefghij");
        let snapshot = storage.snapshot();
        let mut stack = [0; 8];
        let mut heap = Vec::new();

        assert_eq!(snapshot.get_range(0, 3, &mut stack, &mut heap), b"abc");
        assert_eq!(snapshot.get_range(3, 5, &mut stack, &mut heap), b"defgh");

        let mut small_stack = [0; 2];
        assert_eq!(
            snapshot.get_range(2, 7, &mut small_stack, &mut heap),
            b"cdefghi"
        );
        assert_eq!(heap, b"cdefghi");
    }
}
