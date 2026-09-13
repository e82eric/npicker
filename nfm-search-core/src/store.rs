use crate::fuzzy_search_session::SearchSnapshotProvider;
use crate::snapshot_store::SnapshotStore;
use std::cmp::Ordering as CmpOrdering;
use std::ops::Range;
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchCompletion {
    pub text: String,
    pub positions: Vec<usize>,
    pub replacement: String,
    pub replace: Range<usize>,
}

pub trait ItemsSource {
    type Item: Clone;

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

    fn item(&self, node_index: usize) -> Option<Self::Item>;

    fn create_search_plan(&self, query: &str) -> Box<dyn SearchPlan + '_> {
        Box::new(PlainSearchPlan {
            query: query.to_owned(),
        })
    }

    fn header(&self, _query: &str) -> Option<String> {
        None
    }

    fn display_text(&self, _node_index: usize, _query: &str) -> Option<String> {
        None
    }

    fn completions(&self, _input: &str, _cursor: usize) -> Vec<SearchCompletion> {
        Vec::new()
    }

    fn effective_query(&self, input: &str) -> String {
        input.to_owned()
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
    type Item = String;

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

    fn item(&self, node_index: usize) -> Option<Self::Item> {
        (node_index < self.len()).then(|| self.get_string_lossy(node_index, &mut Vec::new()))
    }
}

impl<T: Copy> ItemsSource for StreamingItemSnapshotWithPayload<T> {
    type Item = T;

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

    fn item(&self, node_index: usize) -> Option<Self::Item> {
        (node_index < self.len()).then(|| *self.payload(node_index))
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

impl<T: Clone> ItemsSource for FlatSnapshot<T> {
    type Item = T;

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

    fn item(&self, node_index: usize) -> Option<Self::Item> {
        self.payloads.get(node_index).cloned()
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
        assert!(chunk_size > 0, "chunk size must be nonzero");
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

    pub fn extend_from_slice(&mut self, mut items: &[T]) {
        while !items.is_empty() {
            let count = items.len().min(self.chunk_size - self.current.len());
            self.current.extend_from_slice(&items[..count]);
            self.len += count;
            items = &items[count..];

            if self.current.len() == self.chunk_size {
                self.seal_current();
            }
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
            chunks.push(Arc::<[T]>::from(self.current.as_slice()));
        }

        ChunkedSnapshot {
            chunks: Arc::new(chunks),
            chunk_size: self.chunk_size,
            len: self.len,
        }
    }

    pub fn eq_slice(&self, mut offset: usize, mut items: &[T]) -> bool
    where
        T: Eq,
    {
        if offset > self.len || items.len() > self.len - offset {
            return false;
        }

        while !items.is_empty() {
            if offset >= self.sealed_len {
                let start = offset - self.sealed_len;
                return &self.current[start..start + items.len()] == items;
            }

            let chunk_index = offset / self.chunk_size;
            let chunk_offset = offset % self.chunk_size;
            let chunk = &self.chunks[chunk_index];
            let count = items.len().min(chunk.len() - chunk_offset);
            if chunk[chunk_offset..chunk_offset + count] != items[..count] {
                return false;
            }
            offset += count;
            items = &items[count..];
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

    /// Copies `target.len()` bytes starting at `offset`, across chunk boundaries.
    /// Panics if the requested range lies outside this snapshot.
    #[inline]
    pub fn copy_range_to(&self, mut offset: usize, mut target: &mut [u8]) {
        assert!(offset <= self.len && target.len() <= self.len - offset);
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
    fn bulk_appends_match_pushes_and_preserve_snapshots() {
        let input: Vec<u8> = (0..37).collect();
        for chunk_size in [1, 2, 4, 8, 64] {
            for split in 0..=input.len() {
                let mut bulk = ChunkedStorage::new(chunk_size);
                let mut reference = ChunkedStorage::new(chunk_size);
                for &byte in &input[..split] {
                    bulk.push(byte);
                    reference.push(byte);
                }
                let earlier = bulk.snapshot();
                bulk.extend_from_slice(&[]);
                bulk.extend_from_slice(&input[split..]);
                for &byte in &input[split..] {
                    reference.push(byte);
                }
                assert_eq!(bulk.len(), reference.len());
                assert_eq!(bulk.sealed_len, reference.sealed_len);
                assert_eq!(bulk.current, reference.current);
                assert_eq!(bulk.chunks, reference.chunks);
                assert_eq!(earlier.len, split);
                for index in 0..split {
                    assert_eq!(earlier[index], input[index]);
                }
                // A subsequent scalar push must still work after bulk sealing.
                bulk.push(99);
                assert_eq!(bulk[input.len()], 99);
            }
        }
    }

    #[test]
    fn bulk_appends_support_zero_sized_elements() {
        let mut storage = ChunkedStorage::new(2);
        storage.extend_from_slice(&[(); 5]);
        assert_eq!(storage.len(), 5);
        assert_eq!(storage.sealed_len, 4);
        assert_eq!(storage.current.len(), 1);
    }

    #[test]
    #[should_panic(expected = "chunk size must be nonzero")]
    fn rejects_zero_chunk_size() {
        let _ = ChunkedStorage::<u8>::new(0);
    }

    #[test]
    fn chunked_equality_matches_contiguous_storage() {
        let bytes = b"abcdefghij";
        for chunk_size in [1, 2, 4, 16] {
            let mut storage = ChunkedStorage::new(chunk_size);
            storage.extend_from_slice(bytes);
            // Every valid range, including empty ranges and chunk boundaries.
            for start in 0..=bytes.len() {
                for end in start..=bytes.len() {
                    assert!(storage.eq_slice(start, &bytes[start..end]));
                    let mut different = bytes[start..end].to_vec();
                    for index in 0..different.len() {
                        different[index] ^= 0xff;
                        assert!(!storage.eq_slice(start, &different));
                        different[index] ^= 0xff;
                    }
                }
            }
            assert!(!storage.eq_slice(bytes.len(), b"x"));
            assert!(!storage.eq_slice(bytes.len() + 1, b""));
            assert!(!storage.eq_slice(usize::MAX, b"xx"));
        }
        let empty = ChunkedStorage::<u8>::new(4);
        assert!(empty.eq_slice(0, b""));
        assert!(!empty.eq_slice(0, b"x"));
    }

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
