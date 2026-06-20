use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const NODE_CHUNK_SIZE: usize = 64 * 1024;
const NAME_CHUNK_SIZE: usize = 64 * 1024;
const BYTE_CHUNK_SIZE: usize = 1024 * 1024;
const PUBLISH_NODE_INTERVAL: usize = 1_000;

#[derive(Clone, Copy, Debug)]
pub struct Node {
    pub parent: i32,
    pub name: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Name {
    pub offset: u32,
    pub len: u32,
}

pub struct CompactUtf8FileStore {
    nodes: ChunkedStorage<Node>,
    names: ChunkedStorage<Name>,
    name_bytes: ChunkedStorage<u8>,
    interned_names: Option<HashMap<u64, Vec<u32>>>,
    published: Arc<PublishedSnapshot>,
    snapshot_version: AtomicU64,
}

impl CompactUtf8FileStore {
    pub fn new() -> Self {
        Self {
            nodes: ChunkedStorage::new(NODE_CHUNK_SIZE),
            names: ChunkedStorage::new(NAME_CHUNK_SIZE),
            name_bytes: ChunkedStorage::new(BYTE_CHUNK_SIZE),
            interned_names: Some(HashMap::new()),
            published: Arc::new(PublishedSnapshot::empty()),
            snapshot_version: AtomicU64::new(0),
        }
    }

    pub fn add_node(&mut self, parent: i32, name: &str) -> u32 {
        let name_index = self.get_or_add_name(name.as_bytes());
        let node_index = self.nodes.len();
        self.nodes.push(Node {
            parent,
            name: name_index,
        });

        if self.nodes.len() == 15 || self.nodes.len() % PUBLISH_NODE_INTERVAL == 0 {
            self.publish();
        }

        node_index as u32
    }

    pub fn complete_adding(&mut self) {
        self.interned_names = None;
        self.publish();
    }

    pub fn snapshot(&self) -> Arc<PublishedSnapshot> {
        Arc::clone(&self.published)
    }

    fn publish(&mut self) {
        let version = self.snapshot_version.fetch_add(1, Ordering::Relaxed) + 1;
        self.published = Arc::new(PublishedSnapshot {
            nodes: self.nodes.snapshot(),
            node_count: self.nodes.len(),
            name_count: self.names.len(),
            names: self.names.snapshot(),
            byte_count: self.name_bytes.len(),
            name_bytes: self.name_bytes.snapshot(),
            version,
        });
    }

    fn get_or_add_name(&mut self, bytes: &[u8]) -> u32 {
        let Some(interned) = self.interned_names.as_mut() else {
            return self.add_name(bytes);
        };

        let hash = hash_bytes(bytes);
        if let Some(candidates) = interned.get(&hash) {
            for &candidate in candidates {
                let name = self.names[candidate as usize];
                if name.len as usize == bytes.len()
                    && self.name_bytes.eq_slice(name.offset as usize, bytes)
                {
                    return candidate;
                }
            }
        }

        let name_index = self.add_name(bytes);
        self.interned_names
            .as_mut()
            .expect("interning is still enabled")
            .entry(hash)
            .or_default()
            .push(name_index);
        name_index
    }

    fn add_name(&mut self, bytes: &[u8]) -> u32 {
        let offset = self.name_bytes.len();
        self.name_bytes.extend_from_slice(bytes);
        let index = self.names.len();
        self.names.push(Name {
            offset: offset as u32,
            len: bytes.len() as u32,
        });
        index as u32
    }
}

pub trait ItemsSource {
    fn version(&self) -> u64;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool;
    fn get_string<'a>(
        &self,
        index: usize,
        stack_buffer: &'a mut [u8],
        heap_buffer: &'a mut Vec<u8>,
    ) -> &'a [u8];

    fn get_string_lossy(&self, node_index: usize, out: &mut Vec<u8>) -> String;
}

pub enum AnyItemSource {
    FileSystem(Arc<PublishedSnapshot>),
}

impl ItemsSource for AnyItemSource {
    fn version(&self) -> u64 {
        match self {
            AnyItemSource::FileSystem(source) => source.version(),
        }
    }

    fn len(&self) -> usize {
        match self {
            AnyItemSource::FileSystem(source) => source.len(),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            AnyItemSource::FileSystem(source) => source.is_empty(),
        }
    }

    fn get_string<'a>(
        &self,
        index: usize,
        stack_buffer: &'a mut [u8],
        heap_buffer: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        match self {
            AnyItemSource::FileSystem(source) => {
                source.get_string(index, stack_buffer, heap_buffer)
            }
        }
    }

    fn get_string_lossy(&self, node_index: usize, out: &mut Vec<u8>) -> String {
        match self {
            AnyItemSource::FileSystem(source) => source.get_string_lossy(node_index, out),
        }
    }
}

pub struct PublishedSnapshot {
    nodes: ChunkedSnapshot<Node>,
    node_count: usize,
    names: ChunkedSnapshot<Name>,
    name_count: usize,
    name_bytes: ChunkedSnapshot<u8>,
    byte_count: usize,
    version: u64,
}

impl ItemsSource for PublishedSnapshot {
    fn version(&self) -> u64 {
        self.version
    }
    fn len(&self) -> usize {
        self.node_count
    }
    fn is_empty(&self) -> bool {
        self.node_count == 0
    }
    fn get_string<'a>(
        &self,
        index: usize,
        stack_buffer: &'a mut [u8],
        heap_buffer: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        if let Some(path) = self.path_utf8_stack(index, stack_buffer) {
            path
        } else {
            self.path_utf8(index, heap_buffer);
            heap_buffer.as_slice()
        }
    }
    fn get_string_lossy(&self, node_index: usize, out: &mut Vec<u8>) -> String {
        self.path_utf8(node_index, out);
        String::from_utf8_lossy(out).into_owned()
    }
}

impl PublishedSnapshot {
    fn empty() -> Self {
        Self {
            nodes: ChunkedSnapshot::empty(),
            node_count: 0,
            names: ChunkedSnapshot::empty(),
            name_count: 0,
            name_bytes: ChunkedSnapshot::empty(),
            byte_count: 0,
            version: 0,
        }
    }

    fn path_utf8(&self, node_index: usize, out: &mut Vec<u8>) {
        let full_len = self.path_utf8_full_len(node_index);
        let len = full_len.saturating_sub(1);
        out.resize(full_len, 0);
        self.write_path_utf8_backwards(node_index, out);
        out.truncate(len);
    }

    fn path_utf8_stack<'a>(&self, node_index: usize, out: &'a mut [u8]) -> Option<&'a [u8]> {
        let full_len = self.path_utf8_full_len(node_index);
        let len = full_len.saturating_sub(1);
        if full_len > out.len() {
            return None;
        }

        self.write_path_utf8_backwards(node_index, &mut out[..full_len]);
        Some(&out[..len])
    }

    fn node(&self, index: usize) -> Node {
        assert!(index < self.node_count);
        self.nodes[index]
    }

    fn name(&self, index: usize) -> Name {
        assert!(index < self.name_count);
        self.names[index]
    }

    fn copy_name_to_slice(&self, name: Name, out: &mut [u8]) {
        let mut offset = name.offset as usize;
        let mut remaining = name.len as usize;
        let mut written = 0usize;
        assert!(offset + remaining <= self.byte_count);
        assert!(remaining <= out.len());

        while remaining > 0 {
            let (chunk_index, chunk_offset) = self.name_bytes.locate_direct(offset);
            let chunk = &self.name_bytes.chunks[chunk_index];
            let readable = remaining.min(chunk.len() - chunk_offset);
            out[written..written + readable]
                .copy_from_slice(&chunk[chunk_offset..chunk_offset + readable]);
            offset += readable;
            written += readable;
            remaining -= readable;
        }
    }

    fn path_utf8_full_len(&self, node_index: usize) -> usize {
        let mut len = 0usize;
        let mut current = node_index as i32;

        while current >= 0 {
            let node = self.node(current as usize);
            let name = self.name(node.name as usize);
            len += name.len as usize;
            if name.len > 0 && self.byte_at(name.offset as usize + name.len as usize - 1) != b'\\' {
                len += 1;
            }
            current = node.parent;
        }

        len
    }

    fn write_path_utf8_backwards(&self, node_index: usize, out: &mut [u8]) {
        if out.is_empty() {
            return;
        }

        let mut position = out.len() - 1;
        let mut current = node_index as i32;

        while current >= 0 {
            let node = self.node(current as usize);
            let name = self.name(node.name as usize);
            if name.len > 0 && self.byte_at(name.offset as usize + name.len as usize - 1) != b'\\' {
                out[position] = b'\\';
                position = position.saturating_sub(1);
            }

            let name_len = name.len as usize;
            let start = position + 1 - name_len;
            self.copy_name_to_slice(name, &mut out[start..start + name_len]);
            position = start.saturating_sub(1);
            current = node.parent;
        }
    }

    fn byte_at(&self, index: usize) -> u8 {
        assert!(index < self.byte_count);
        self.name_bytes[index]
    }
}

struct ChunkedStorage<T: Copy> {
    chunk_size: usize,
    len: usize,
    sealed_len: usize,
    chunks: Vec<Arc<[T]>>,
    current: Vec<T>,
}

impl<T: Copy + Default> ChunkedStorage<T> {
    fn new(chunk_size: usize) -> Self {
        Self {
            chunk_size,
            len: 0,
            sealed_len: 0,
            chunks: Vec::new(),
            current: Vec::with_capacity(chunk_size),
        }
    }

    fn len(&self) -> usize {
        self.len
    }

    fn push(&mut self, item: T) {
        self.current.push(item);
        self.len += 1;

        if self.current.len() == self.chunk_size {
            self.seal_current();
        }
    }

    fn extend_from_slice(&mut self, items: &[T]) {
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

    fn snapshot(&self) -> ChunkedSnapshot<T> {
        let mut chunks = self.chunks.clone();
        if !self.current.is_empty() {
            chunks.push(Arc::from(self.current.clone().into_boxed_slice()));
        }

        ChunkedSnapshot {
            chunks: Arc::new(chunks),
            chunk_size: self.chunk_size,
        }
    }

    fn eq_slice(&self, offset: usize, items: &[T]) -> bool
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

struct ChunkedSnapshot<T: Copy> {
    chunks: Arc<Vec<Arc<[T]>>>,
    chunk_size: usize,
}

impl<T: Copy> ChunkedSnapshot<T> {
    fn empty() -> Self {
        Self {
            chunks: Arc::new(Vec::new()),
            chunk_size: 1,
        }
    }

    fn locate_direct(&self, index: usize) -> (usize, usize) {
        (index / self.chunk_size, index % self.chunk_size)
    }
}

impl<T: Copy> std::ops::Index<usize> for ChunkedSnapshot<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        let (chunk_index, offset) = self.locate_direct(index);
        &self.chunks[chunk_index][offset]
    }
}

impl Default for Node {
    fn default() -> Self {
        Self {
            parent: -1,
            name: 0,
        }
    }
}

impl Default for Name {
    fn default() -> Self {
        Self { offset: 0, len: 0 }
    }
}

impl<T: Copy> std::ops::Index<usize> for ChunkedStorage<T> {
    type Output = T;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index)
    }
}

fn hash_bytes(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;

    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}
