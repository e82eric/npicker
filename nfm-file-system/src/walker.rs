use hashbrown::HashTable;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, Sender};

use nfm_search_core::store::{ChunkedSnapshot, ChunkedStorage, ItemsSource};
use nfm_search_core::timing;

const NODE_CHUNK_SIZE: usize = 64 * 1024;
const NAME_CHUNK_SIZE: usize = 64 * 1024;
const PUBLISH_NODE_INTERVAL: usize = 1_000;
const BYTE_CHUNK_SIZE: usize = 1024 * 1024;

#[cfg(all(windows, feature = "scan-bench"))]
pub mod benchmark;

// Directory enumeration is the only OS-specific part of a scan.
#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
mod win32;
#[cfg(windows)]
use win32 as platform;

use platform::PATH_SEPARATOR;

/// How a relative scan writes its unnamed root entry.
const CURRENT_DIRECTORY: &[u8] = b".";

/// One directory entry reported by the platform enumerator.
struct DirectoryEntry<'a> {
    /// Display name stored in the snapshot.
    name: String,
    /// Native name bytes when they differ from `name`, used to build the
    /// path of a child directory.
    path_name: Option<&'a [u8]>,
    is_dir: bool,
}

pub struct ScanOptions {
    pub roots: Vec<PathBuf>,
    pub max_depth: i32,
    pub directories_only: bool,
    pub files_only: bool,
    /// Write paths relative to their root, like `fd` run from that root. The
    /// root entry itself is written as `.`; `PublishedSnapshot::absolute_path`
    /// still returns full paths. Ambiguous when several roots share names.
    pub relative: bool,
}

impl ScanOptions {
    fn effective_max_depth(&self) -> usize {
        if self.max_depth <= 0 {
            usize::MAX
        } else {
            self.max_depth as usize
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScanStatus {
    Scanning,
    Completed,
    Cancelled,
    Failed,
}

impl ScanStatus {
    fn as_u8(self) -> u8 {
        match self {
            Self::Scanning => 0,
            Self::Completed => 1,
            Self::Cancelled => 2,
            Self::Failed => 3,
        }
    }

    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Completed,
            2 => Self::Cancelled,
            3 => Self::Failed,
            _ => Self::Scanning,
        }
    }
}

pub trait ScanEventSink: Send + Sync + 'static {
    fn snapshot(&self, snapshot: Arc<PublishedSnapshot>);
    fn complete(&self, status: ScanStatus);
}

pub struct FileWalkerScan {
    status: Arc<AtomicU8>,
    cancelled: Arc<AtomicBool>,
}

/// A scan owner that requests cancellation when dropped.
pub struct OwnedFileWalkerScan(FileWalkerScan);
impl std::ops::Deref for OwnedFileWalkerScan {
    type Target = FileWalkerScan;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Drop for OwnedFileWalkerScan {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
impl FileWalkerScan {
    /// Tie cancellation to the lifetime of the returned owner.
    pub fn cancel_on_drop(self) -> OwnedFileWalkerScan {
        OwnedFileWalkerScan(self)
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn status(&self) -> ScanStatus {
        ScanStatus::from_u8(self.status.load(Ordering::Acquire))
    }

    pub fn is_done(&self) -> bool {
        self.status() != ScanStatus::Scanning
    }
}

/// Set in `Node::name` when the node is a directory. Names are counted in the
/// remaining bits, so the flag costs no memory.
const DIRECTORY_BIT: u32 = 1 << 31;

#[derive(Clone, Copy, Debug)]
pub struct Node {
    pub parent: i32,
    /// The index of the node's name, with `DIRECTORY_BIT` set for a
    /// directory. Read it with `name_index` and `is_dir`.
    pub name: u32,
}

impl Node {
    /// The index of the node's name.
    pub fn name_index(self) -> usize {
        (self.name & !DIRECTORY_BIT) as usize
    }

    /// Whether the node was added as a directory.
    pub fn is_dir(self) -> bool {
        self.name & DIRECTORY_BIT != 0
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Name {
    pub offset: u32,
    pub len: u32,
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

pub struct PublishedSnapshot {
    nodes: ChunkedSnapshot<Node>,
    node_count: usize,
    names: ChunkedSnapshot<Name>,
    name_count: usize,
    name_bytes: ChunkedSnapshot<u8>,
    byte_count: usize,
    version: u64,
    /// Root paths by root node index when roots are stored with empty names.
    relative_roots: Option<Arc<[PathBuf]>>,
    /// The directories that `Node::parent` indexes when a scan keeps them
    /// only as the parents of its entries (`files_only`), so they are not
    /// items. When this is `None`, parents are in `nodes` and every directory
    /// is an item.
    parents: Option<ChunkedSnapshot<Node>>,
    parent_count: usize,
}

impl ItemsSource for PublishedSnapshot {
    type Item = String;

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
        &'a self,
        index: usize,
        stack_buffer: &'a mut [u8],
        heap_buffer: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        let full_len = self.path_utf8_full_len(index);
        let len = full_len.saturating_sub(1);
        if full_len == 0 {
            return CURRENT_DIRECTORY;
        }
        if full_len <= stack_buffer.len() {
            self.write_path_utf8_backwards(index, &mut stack_buffer[..full_len]);
            &stack_buffer[..len]
        } else {
            heap_buffer.resize(full_len, 0);
            self.write_path_utf8_backwards(index, heap_buffer);
            heap_buffer.truncate(len);
            heap_buffer.as_slice()
        }
    }
    fn get_string_lossy(&self, node_index: usize, out: &mut Vec<u8>) -> String {
        self.path_utf8(node_index, out);
        String::from_utf8_lossy(out).into_owned()
    }

    fn item(&self, node_index: usize) -> Option<Self::Item> {
        (node_index < self.node_count).then(|| self.get_string_lossy(node_index, &mut Vec::new()))
    }
}

impl PublishedSnapshot {
    pub fn empty() -> Self {
        Self {
            nodes: ChunkedSnapshot::empty(),
            node_count: 0,
            names: ChunkedSnapshot::empty(),
            name_count: 0,
            name_bytes: ChunkedSnapshot::empty(),
            byte_count: 0,
            version: 0,
            relative_roots: None,
            parents: None,
            parent_count: 0,
        }
    }

    /// The nodes that `Node::parent` indexes, and how many there are.
    fn parents(&self) -> (&ChunkedSnapshot<Node>, usize) {
        match &self.parents {
            Some(parents) => (parents, self.parent_count),
            None => (&self.nodes, self.node_count),
        }
    }

    /// Whether an entry is a directory, as the scan saw it. An index past the
    /// end is not a directory.
    pub fn is_dir(&self, node_index: usize) -> bool {
        node_index < self.node_count && self.nodes[node_index].is_dir()
    }

    /// The full path of an entry, including its root in relative scans.
    pub fn absolute_path(&self, node_index: usize) -> Option<PathBuf> {
        if node_index >= self.node_count {
            return None;
        }
        let path = self.get_string_lossy(node_index, &mut Vec::new());
        let Some(roots) = &self.relative_roots else {
            return Some(PathBuf::from(path));
        };
        let (parents, parent_count) = self.parents();
        let mut root = node_index;
        let mut node = self.node(node_index);
        while node.parent >= 0 {
            root = node.parent as usize;
            assert!(root < parent_count);
            node = parents[root];
        }
        // Roots are inserted before any of their entries, in scan order.
        let root = roots.get(root)?;
        Some(if self.path_utf8_full_len(node_index) == 0 {
            root.clone()
        } else {
            root.join(path)
        })
    }

    fn path_utf8(&self, node_index: usize, out: &mut Vec<u8>) {
        let full_len = self.path_utf8_full_len(node_index);
        let len = full_len.saturating_sub(1);
        if full_len == 0 {
            out.clear();
            out.extend_from_slice(CURRENT_DIRECTORY);
            return;
        }
        out.resize(full_len, 0);
        self.write_path_utf8_backwards(node_index, out);
        out.truncate(len);
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
        let offset = name.offset as usize;
        let len = name.len as usize;
        assert!(offset <= self.byte_count && len <= self.byte_count - offset);
        assert!(len <= out.len());
        self.name_bytes.copy_range_to(offset, &mut out[..len]);
    }

    fn path_utf8_full_len(&self, node_index: usize) -> usize {
        let (parents, parent_count) = self.parents();
        let mut len = 0usize;
        let mut node = self.node(node_index);

        loop {
            let name = self.name(node.name_index());
            len += name.len as usize;
            if name.len > 0
                && self.byte_at(name.offset as usize + name.len as usize - 1) != PATH_SEPARATOR
            {
                len += 1;
            }
            if node.parent < 0 {
                return len;
            }
            assert!((node.parent as usize) < parent_count);
            node = parents[node.parent as usize];
        }
    }

    fn write_path_utf8_backwards(&self, node_index: usize, out: &mut [u8]) {
        if out.is_empty() {
            return;
        }

        let (parents, parent_count) = self.parents();
        let mut position = out.len() - 1;
        let mut node = self.node(node_index);

        loop {
            let name = self.name(node.name_index());
            if name.len > 0
                && self.byte_at(name.offset as usize + name.len as usize - 1) != PATH_SEPARATOR
            {
                out[position] = PATH_SEPARATOR;
                position = position.saturating_sub(1);
            }

            let name_len = name.len as usize;
            let start = position + 1 - name_len;
            self.copy_name_to_slice(name, &mut out[start..start + name_len]);
            position = start.saturating_sub(1);
            if node.parent < 0 {
                return;
            }
            assert!((node.parent as usize) < parent_count);
            node = parents[node.parent as usize];
        }
    }

    fn byte_at(&self, index: usize) -> u8 {
        assert!(index < self.byte_count);
        self.name_bytes[index]
    }
}

struct InternEntry {
    hash: u64,
    name_index: u32,
}

pub struct CompactUtf8FileStore {
    nodes: ChunkedStorage<Node>,
    names: ChunkedStorage<Name>,
    name_bytes: ChunkedStorage<u8>,
    interned_names: Option<HashTable<InternEntry>>,
    published: Arc<PublishedSnapshot>,
    snapshot_version: AtomicU64,
    relative_roots: Option<Arc<[PathBuf]>>,
    /// Directories kept apart from the items; see
    /// `PublishedSnapshot::parents`.
    parents: Option<ChunkedStorage<Node>>,
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

impl CompactUtf8FileStore {
    pub fn new() -> Self {
        Self {
            nodes: ChunkedStorage::new(NODE_CHUNK_SIZE),
            names: ChunkedStorage::new(NAME_CHUNK_SIZE),
            name_bytes: ChunkedStorage::new(BYTE_CHUNK_SIZE),
            interned_names: Some(HashTable::new()),
            published: Arc::new(PublishedSnapshot::empty()),
            snapshot_version: AtomicU64::new(0),
            relative_roots: None,
            parents: None,
        }
    }

    /// Store roots with empty names, keeping their paths for `absolute_path`.
    pub fn with_relative_roots(roots: Arc<[PathBuf]>) -> Self {
        Self {
            relative_roots: Some(roots),
            ..Self::new()
        }
    }

    /// Keep nodes added with `add_parent` apart from the items, so
    /// directories that are only the parents of other nodes are not listed.
    /// Every `parent` then indexes the nodes added with `add_parent`.
    pub fn with_separate_parents(self) -> Self {
        Self {
            parents: Some(ChunkedStorage::new(NODE_CHUNK_SIZE)),
            ..self
        }
    }

    /// Add a directory that is only the parent of other nodes, returning the
    /// index its children use as `parent`. It is an item too, as
    /// `add_directory` makes it, unless the store was made with
    /// `with_separate_parents`.
    pub fn add_parent(&mut self, parent: i32, name: &str) -> u32 {
        if self.parents.is_none() {
            return self.add_directory(parent, name);
        }
        let name = self.get_or_add_name(name.as_bytes());
        let parents = self.parents.as_mut().expect("checked above");
        let index = parents.len();
        parents.push(Node { parent, name });
        index as u32
    }

    /// Add a file as an item, returning its index.
    pub fn add_node(&mut self, parent: i32, name: &str) -> u32 {
        self.add_item(parent, name, 0)
    }

    /// Add a directory as an item, returning its index.
    pub fn add_directory(&mut self, parent: i32, name: &str) -> u32 {
        self.add_item(parent, name, DIRECTORY_BIT)
    }

    fn add_item(&mut self, parent: i32, name: &str, kind: u32) -> u32 {
        let name_index = self.get_or_add_name(name.as_bytes());
        let node_index = self.nodes.len();
        self.nodes.push(Node {
            parent,
            name: name_index | kind,
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
            relative_roots: self.relative_roots.clone(),
            parents: self.parents.as_ref().map(ChunkedStorage::snapshot),
            parent_count: self.parents.as_ref().map_or(0, ChunkedStorage::len),
        });
    }

    fn get_or_add_name(&mut self, bytes: &[u8]) -> u32 {
        let hash = hash_bytes(bytes);
        self.get_or_add_name_hashed(bytes, hash)
    }

    // Callers must consistently supply the same hash for equal byte strings.
    fn get_or_add_name_hashed(&mut self, bytes: &[u8], hash: u64) -> u32 {
        let Some(interned) = self.interned_names.as_ref() else {
            return self.add_name(bytes);
        };

        if let Some(entry) = interned.find(hash, |entry| {
            if entry.hash != hash {
                return false;
            }
            let name = self.names[entry.name_index as usize];
            name.len as usize == bytes.len()
                && self.name_bytes.eq_slice(name.offset as usize, bytes)
        }) {
            return entry.name_index;
        }

        let name_index = self.add_name(bytes);
        self.interned_names
            .as_mut()
            .expect("interning is still enabled")
            // Cache full hashes so table growth never re-reads name bytes.
            .insert_unique(hash, InternEntry { hash, name_index }, |entry| entry.hash);
        name_index
    }

    fn add_name(&mut self, bytes: &[u8]) -> u32 {
        let offset = self.name_bytes.len();
        self.name_bytes.extend_from_slice(bytes);
        let index = self.names.len();
        // The top bit of a node's name index marks a directory.
        assert!(index < DIRECTORY_BIT as usize, "too many distinct names");
        self.names.push(Name {
            offset: offset as u32,
            len: bytes.len() as u32,
        });
        index as u32
    }
}

#[cfg(test)]
mod interning_tests {
    use super::*;

    #[test]
    fn collisions_and_growth_preserve_distinct_names() {
        let mut store = CompactUtf8FileStore::new();
        // Exercise names spanning sealed chunks and the unfinished chunk.
        store.name_bytes = ChunkedStorage::new(4);
        let inputs = [
            b"alpha".as_slice(),
            b"bravo",
            b"",
            b"longer-name",
            b"alpha!",
        ];
        let indexes: Vec<_> = inputs
            .iter()
            .map(|bytes| store.get_or_add_name_hashed(bytes, 7))
            .collect();
        assert_eq!(indexes, vec![0, 1, 2, 3, 4]);
        for index in 0..1000 {
            store.get_or_add_name(format!("unique-{index}").as_bytes());
        }
        let name_count = store.names.len();
        let byte_count = store.name_bytes.len();
        for (bytes, expected) in inputs.iter().zip(indexes) {
            assert_eq!(store.get_or_add_name_hashed(bytes, 7), expected);
        }
        assert_eq!(store.names.len(), name_count);
        assert_eq!(store.name_bytes.len(), byte_count);
        for index in 0..1000 {
            assert_eq!(
                store.get_or_add_name(format!("unique-{index}").as_bytes()),
                5 + index
            );
        }
    }

    #[test]
    fn repeated_node_names_share_storage_and_publish() {
        let mut store = CompactUtf8FileStore::new();
        let first = store.add_node(-1, "same");
        let second = store.add_node(-1, "same");
        assert_eq!(
            store.nodes[first as usize].name,
            store.nodes[second as usize].name
        );
        assert_eq!(store.names.len(), 1);
        store.complete_adding();
        assert!(store.interned_names.is_none());
        let snapshot = store.snapshot();
        assert_eq!(snapshot.node_count, 2);
        assert_eq!(snapshot.name_count, 1);
        assert_eq!(snapshot.byte_count, 4);
    }

    #[test]
    fn separate_parents_are_not_items_and_still_build_paths() {
        let sep = PATH_SEPARATOR as char;
        let mut store = CompactUtf8FileStore::new().with_separate_parents();
        let root = store.add_parent(-1, "root");
        let sub = store.add_parent(root as i32, "sub");
        assert_eq!((root, sub), (0, 1));
        // Items are numbered apart from the parents they point at.
        assert_eq!(store.add_node(root as i32, "a.txt"), 0);
        assert_eq!(store.add_node(sub as i32, "b.txt"), 1);
        store.complete_adding();
        let snapshot = store.snapshot();
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot.item(0).unwrap(), format!("root{sep}a.txt"));
        assert_eq!(snapshot.item(1).unwrap(), format!("root{sep}sub{sep}b.txt"));
        assert!(snapshot.item(2).is_none());

        assert!(!snapshot.is_dir(0) && !snapshot.is_dir(1));

        // Without separate parents a parent is an item like any other node.
        let mut store = CompactUtf8FileStore::new();
        let root = store.add_parent(-1, "root");
        assert_eq!(store.add_node(root as i32, "a.txt"), 1);
        store.complete_adding();
        assert_eq!(store.snapshot().len(), 2);
    }

    #[test]
    fn directories_are_flagged_without_changing_names_or_paths() {
        let sep = PATH_SEPARATOR as char;
        let mut store = CompactUtf8FileStore::new();
        // A directory and a file with the same name share one stored name.
        let root = store.add_directory(-1, "same");
        let file = store.add_node(root as i32, "same");
        let sub = store.add_parent(root as i32, "sub");
        assert_eq!(store.names.len(), 2);
        assert_eq!(
            store.nodes[root as usize].name_index(),
            store.nodes[file as usize].name_index()
        );
        store.complete_adding();
        let snapshot = store.snapshot();
        assert_eq!(
            [root, file, sub].map(|index| snapshot.is_dir(index as usize)),
            [true, false, true]
        );
        assert!(!snapshot.is_dir(snapshot.len()));
        assert_eq!(snapshot.item(0).unwrap(), "same");
        assert_eq!(snapshot.item(1).unwrap(), format!("same{sep}same"));
        assert_eq!(snapshot.item(2).unwrap(), format!("same{sep}sub"));
    }
}

struct DirectoryWork {
    depth: usize,
    node_index: i32,
    path: Vec<u8>,
}

enum WriterCommand {
    Add {
        parent: i32,
        name: String,
        directory: Option<(usize, Vec<u8>)>,
        kind: EntryKind,
    },
}

/// What a scanned entry is in the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryKind {
    /// A file, listed as an item.
    File,
    /// A directory, listed as an item.
    Directory,
    /// A directory kept only as the parent of its entries.
    Parent,
}

pub fn start_scan<S>(options: ScanOptions, sink: Arc<S>) -> FileWalkerScan
where
    S: ScanEventSink,
{
    let status = Arc::new(AtomicU8::new(ScanStatus::Scanning.as_u8()));
    let cancelled = Arc::new(AtomicBool::new(false));
    let scan_status = Arc::clone(&status);
    let scan_cancelled = Arc::clone(&cancelled);

    std::thread::spawn(move || {
        scan(options, sink, scan_status, scan_cancelled);
    });

    FileWalkerScan { status, cancelled }
}

fn scan<S>(options: ScanOptions, sink: Arc<S>, status: Arc<AtomicU8>, cancelled: Arc<AtomicBool>)
where
    S: ScanEventSink,
{
    let (tx, rx) = unbounded();
    let (writer_tx, writer_rx) = unbounded();
    let pending = Arc::new(AtomicUsize::new(0));
    let worker_count = std::thread::available_parallelism()
        .map(|count| count.get())
        .unwrap_or(1)
        .saturating_sub(2)
        .max(1);
    timing::write_lazy(|| {
        format!(
        "source_start mode={} workers={worker_count} roots={} max_depth={} directories_only={} files_only={}",
        platform::SCAN_MODE,
        options.roots.len(),
        options.max_depth,
        options.directories_only,
        options.files_only
    )
    });

    let writer_status = Arc::clone(&status);
    let writer_cancelled = Arc::clone(&cancelled);
    let writer_pending = Arc::clone(&pending);
    let directory_tx = tx.clone();
    let store = if options.relative {
        CompactUtf8FileStore::with_relative_roots(options.roots.clone().into())
    } else {
        CompactUtf8FileStore::new()
    };
    // Scanned directories stay as the parents of their entries, but only
    // files are listed.
    let store = if options.files_only {
        store.with_separate_parents()
    } else {
        store
    };
    let writer = std::thread::spawn(move || {
        store_writer_loop(
            store,
            writer_rx,
            directory_tx,
            writer_pending,
            sink,
            writer_status,
            writer_cancelled,
        )
    });

    for root in &options.roots {
        if cancelled.load(Ordering::Acquire) {
            break;
        }

        let name = if options.relative {
            String::new()
        } else {
            root.to_string_lossy().into_owned()
        };
        add_entry(
            &writer_tx,
            &pending,
            -1,
            name,
            Some((0, platform::path_bytes(root))),
            if options.files_only {
                EntryKind::Parent
            } else {
                EntryKind::Directory
            },
        );
    }

    let options = Arc::new(options);
    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let worker_options = Arc::clone(&options);
        let worker_pending = Arc::clone(&pending);
        let worker_writer_tx = writer_tx.clone();
        let worker_rx = rx.clone();
        let worker_cancelled = Arc::clone(&cancelled);
        workers.push(std::thread::spawn(move || {
            worker_loop(
                worker_options,
                worker_pending,
                worker_writer_tx,
                worker_rx,
                worker_cancelled,
            );
        }));
    }

    drop(tx);
    drop(rx);
    drop(writer_tx);

    for worker in workers {
        let _ = worker.join();
    }

    let _ = writer.join();
}

#[cfg(test)]
mod scheduling_tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Sink(Mutex<Option<Arc<PublishedSnapshot>>>);

    impl ScanEventSink for Sink {
        fn snapshot(&self, snapshot: Arc<PublishedSnapshot>) {
            *self.0.lock().unwrap() = Some(snapshot);
        }
        fn complete(&self, _: ScanStatus) {}
    }

    #[test]
    fn writer_schedules_inserted_directories_and_preserves_parent_indexes() {
        let (writer_tx, writer_rx) = unbounded();
        let (directory_tx, directory_rx) = unbounded();
        let pending = Arc::new(AtomicUsize::new(0));
        let sink = Arc::new(Sink::default());
        let status = Arc::new(AtomicU8::new(ScanStatus::Scanning.as_u8()));
        // Enqueue before starting the writer: insertion must not wait for a reply.
        add_node(
            &writer_tx,
            &pending,
            -1,
            "root".into(),
            Some((0, b"root".to_vec())),
        );
        assert_eq!(pending.load(Ordering::Acquire), 1);
        let writer = {
            let pending = Arc::clone(&pending);
            let sink = Arc::clone(&sink);
            let status = Arc::clone(&status);
            std::thread::spawn(move || {
                store_writer_loop(
                    CompactUtf8FileStore::new(),
                    writer_rx,
                    directory_tx,
                    pending,
                    sink,
                    status,
                    Arc::new(AtomicBool::new(false)),
                )
            })
        };
        let root = directory_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(root.node_index, 0);
        let child_path = make_child_path(b"root", b"child");
        add_node(
            &writer_tx,
            &pending,
            root.node_index,
            "child".into(),
            Some((1, child_path.clone())),
        );
        pending.fetch_sub(1, Ordering::AcqRel);
        let child = directory_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(child.node_index, 1);
        assert_eq!(child.depth, 1);
        assert_eq!(child.path, child_path);
        add_node(&writer_tx, &pending, child.node_index, "file".into(), None);
        pending.fetch_sub(1, Ordering::AcqRel);
        drop(writer_tx);
        writer.join().unwrap();
        assert_eq!(pending.load(Ordering::Acquire), 0);
        assert_eq!(
            ScanStatus::from_u8(status.load(Ordering::Acquire)),
            ScanStatus::Completed
        );
        assert_eq!(
            sink.0.lock().unwrap().as_ref().unwrap().item(2).unwrap(),
            format!("root{0}child{0}file", PATH_SEPARATOR as char)
        );
        assert!(directory_rx.try_recv().is_err());
    }

    #[test]
    fn failed_writer_send_releases_pending_work() {
        let (tx, rx) = unbounded();
        drop(rx);
        let pending = AtomicUsize::new(0);
        add_node(&tx, &pending, -1, "root".into(), Some((0, Vec::new())));
        assert_eq!(pending.load(Ordering::Acquire), 0);
    }

    #[test]
    fn cancelled_or_disconnected_scheduling_releases_pending_work() {
        for cancelled in [false, true] {
            let (tx, rx) = unbounded();
            let (directory_tx, directory_rx) = unbounded();
            let pending = Arc::new(AtomicUsize::new(0));
            add_node(&tx, &pending, -1, "root".into(), Some((0, Vec::new())));
            drop(tx);
            if !cancelled {
                drop(directory_rx);
            }
            let status = Arc::new(AtomicU8::new(ScanStatus::Scanning.as_u8()));
            store_writer_loop(
                CompactUtf8FileStore::new(),
                rx,
                directory_tx,
                Arc::clone(&pending),
                Arc::new(Sink::default()),
                Arc::clone(&status),
                Arc::new(AtomicBool::new(cancelled)),
            );
            assert_eq!(pending.load(Ordering::Acquire), 0);
            assert_eq!(
                ScanStatus::from_u8(status.load(Ordering::Acquire)),
                if cancelled {
                    ScanStatus::Cancelled
                } else {
                    ScanStatus::Completed
                }
            );
        }
    }
}

fn worker_loop(
    options: Arc<ScanOptions>,
    pending: Arc<AtomicUsize>,
    writer_tx: Sender<WriterCommand>,
    rx: Receiver<DirectoryWork>,
    cancelled: Arc<AtomicBool>,
) {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return;
        }

        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(work) => {
                scan_directory(&options, &pending, &writer_tx, &cancelled, work);
                pending.fetch_sub(1, Ordering::AcqRel);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if pending.load(Ordering::Acquire) == 0 {
                    return;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn scan_directory(
    options: &ScanOptions,
    pending: &AtomicUsize,
    writer_tx: &Sender<WriterCommand>,
    cancelled: &AtomicBool,
    work: DirectoryWork,
) {
    if cancelled.load(Ordering::Acquire) {
        return;
    }

    let effective_max_depth = options.effective_max_depth();
    platform::read_directory(&work.path, cancelled, |entry| {
        let directory = (entry.is_dir && work.depth + 1 < effective_max_depth)
            .then(|| {
                let name = entry.path_name.unwrap_or(entry.name.as_bytes());
                make_child_path(&work.path, name)
            })
            .filter(|path| platform::should_descend(path))
            .map(|path| (work.depth + 1, path));

        let included = if entry.is_dir {
            !options.files_only
        } else {
            !options.directories_only
        };
        if included || directory.is_some() {
            let kind = match (entry.is_dir, included) {
                (false, _) => EntryKind::File,
                (true, true) => EntryKind::Directory,
                (true, false) => EntryKind::Parent,
            };
            add_entry(
                writer_tx,
                pending,
                work.node_index,
                entry.name,
                directory,
                kind,
            );
        }
    });
}

fn store_writer_loop<S>(
    mut store: CompactUtf8FileStore,
    rx: Receiver<WriterCommand>,
    directory_tx: Sender<DirectoryWork>,
    pending: Arc<AtomicUsize>,
    sink: Arc<S>,
    status: Arc<AtomicU8>,
    cancelled: Arc<AtomicBool>,
) where
    S: ScanEventSink,
{
    while let Ok(command) = rx.recv() {
        match command {
            WriterCommand::Add {
                parent,
                name,
                directory,
                kind,
            } => {
                let node_index = if kind == EntryKind::Parent {
                    store.add_parent(parent, &name)
                } else {
                    let node_index = if kind == EntryKind::Directory {
                        store.add_directory(parent, &name)
                    } else {
                        store.add_node(parent, &name)
                    };
                    if node_index == 0 || (node_index + 1) % 1_000 == 0 {
                        sink.snapshot(store.snapshot());
                    }
                    node_index
                };
                if let Some((depth, path)) = directory {
                    if cancelled.load(Ordering::Acquire)
                        || directory_tx
                            .send(DirectoryWork {
                                depth,
                                node_index: node_index as i32,
                                path,
                            })
                            .is_err()
                    {
                        pending.fetch_sub(1, Ordering::AcqRel);
                    }
                }
            }
        }
    }

    store.complete_adding();
    sink.snapshot(store.snapshot());

    let final_status = if cancelled.load(Ordering::Acquire) {
        ScanStatus::Cancelled
    } else {
        ScanStatus::Completed
    };
    status.store(final_status.as_u8(), Ordering::Release);
    sink.complete(final_status);
}

#[cfg(test)]
fn add_node(
    writer_tx: &Sender<WriterCommand>,
    pending: &AtomicUsize,
    parent: i32,
    name: String,
    directory: Option<(usize, Vec<u8>)>,
) {
    let kind = if directory.is_some() {
        EntryKind::Directory
    } else {
        EntryKind::File
    };
    add_entry(writer_tx, pending, parent, name, directory, kind);
}

/// Queue an entry for the writer, and count a directory it schedules as
/// pending work.
fn add_entry(
    writer_tx: &Sender<WriterCommand>,
    pending: &AtomicUsize,
    parent: i32,
    name: String,
    directory: Option<(usize, Vec<u8>)>,
    kind: EntryKind,
) {
    let schedules_directory = directory.is_some();
    // Count work before enqueueing so workers stay alive while the writer
    // still has directories waiting to be inserted and scheduled.
    if schedules_directory {
        pending.fetch_add(1, Ordering::AcqRel);
    }
    if writer_tx
        .send(WriterCommand::Add {
            parent,
            name,
            directory,
            kind,
        })
        .is_err()
        && schedules_directory
    {
        pending.fetch_sub(1, Ordering::AcqRel);
    }
}
fn make_child_path(parent: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = Vec::with_capacity(parent.len() + name.len() + 1);
    path.extend_from_slice(parent);
    if !path.is_empty() && !path.ends_with(&[PATH_SEPARATOR]) {
        path.push(PATH_SEPARATOR);
    }
    path.extend_from_slice(name);
    path
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    #[test]
    fn dropping_owned_handle_cancels_scan() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let scan = FileWalkerScan {
            status: Arc::new(AtomicU8::new(ScanStatus::Scanning.as_u8())),
            cancelled: cancelled.clone(),
        };
        let owner = scan.cancel_on_drop();
        assert!(!cancelled.load(Ordering::Acquire));
        drop(owner);
        assert!(cancelled.load(Ordering::Acquire));
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Sink(Mutex<Option<Arc<PublishedSnapshot>>>);

    impl ScanEventSink for Sink {
        fn snapshot(&self, snapshot: Arc<PublishedSnapshot>) {
            *self.0.lock().unwrap() = Some(snapshot);
        }
        fn complete(&self, _: ScanStatus) {}
    }

    fn scan_paths(
        root: &Path,
        max_depth: i32,
        directories_only: bool,
        files_only: bool,
    ) -> BTreeSet<String> {
        let snapshot = scan_snapshot(root, max_depth, directories_only, files_only, false);
        (0..snapshot.len())
            .map(|index| snapshot.item(index).unwrap())
            .collect()
    }

    fn scan_snapshot(
        root: &Path,
        max_depth: i32,
        directories_only: bool,
        files_only: bool,
        relative: bool,
    ) -> Arc<PublishedSnapshot> {
        let sink = Arc::new(Sink::default());
        let scan = start_scan(
            ScanOptions {
                roots: vec![root.to_path_buf()],
                max_depth,
                directories_only,
                files_only,
                relative,
            },
            Arc::clone(&sink),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !scan.is_done() {
            assert!(std::time::Instant::now() < deadline, "scan did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(scan.status(), ScanStatus::Completed);
        let snapshot = sink.0.lock().unwrap().clone();
        snapshot.unwrap()
    }

    #[test]
    fn relative_scan_omits_root_and_resolves_absolute_paths() {
        let root = std::env::temp_dir().join(format!("nfm-scan-relative-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.txt"), "").unwrap();
        std::fs::write(root.join("sub").join("b.txt"), "").unwrap();

        let snapshot = scan_snapshot(&root, i32::MAX, false, false, true);
        let mut rows: Vec<_> = (0..snapshot.len())
            .map(|index| {
                let mut stack = [0u8; 64];
                let mut heap = Vec::new();
                let matched = snapshot.get_string(index, &mut stack, &mut heap).to_vec();
                let item = snapshot.item(index).unwrap();
                // Matching and accepted values use the same relative text.
                assert_eq!(matched, item.as_bytes());
                (item, snapshot.absolute_path(index).unwrap())
            })
            .collect();
        rows.sort();
        assert_eq!(
            rows,
            vec![
                (".".to_owned(), root.clone()),
                ("a.txt".to_owned(), root.join("a.txt")),
                ("sub".to_owned(), root.join("sub")),
                (join(&["sub", "b.txt"]), root.join("sub").join("b.txt")),
            ]
        );
        assert_eq!(snapshot.absolute_path(snapshot.len()), None);

        // Files only: the root and `sub` are parents but not items, and
        // absolute paths still find the root.
        let files = scan_snapshot(&root, i32::MAX, false, true, true);
        let mut rows: Vec<_> = (0..files.len())
            .map(|index| {
                (
                    files.item(index).unwrap(),
                    files.absolute_path(index).unwrap(),
                )
            })
            .collect();
        rows.sort();
        assert_eq!(
            rows,
            vec![
                ("a.txt".to_owned(), root.join("a.txt")),
                (join(&["sub", "b.txt"]), root.join("sub").join("b.txt")),
            ]
        );
        assert_eq!(files.absolute_path(files.len()), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn join(parts: &[&str]) -> String {
        parts.join(&(PATH_SEPARATOR as char).to_string())
    }

    #[test]
    fn scans_nested_directories_with_native_separators_and_filters() {
        let root = std::env::temp_dir().join(format!("nfm-scan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub").join("deeper")).unwrap();
        std::fs::write(root.join("a.txt"), "").unwrap();
        std::fs::write(root.join("sub").join("b.txt"), "").unwrap();
        std::fs::write(root.join("sub").join("deeper").join("c.txt"), "").unwrap();
        let r = root.to_string_lossy().into_owned();
        let r = r.as_str();

        let all = [
            r.to_owned(),
            join(&[r, "a.txt"]),
            join(&[r, "sub"]),
            join(&[r, "sub", "b.txt"]),
            join(&[r, "sub", "deeper"]),
            join(&[r, "sub", "deeper", "c.txt"]),
        ];
        assert_eq!(
            scan_paths(&root, i32::MAX, false, false),
            all.iter().cloned().collect()
        );
        // The root node is always present; filters apply to scanned entries.
        assert_eq!(
            scan_paths(&root, i32::MAX, true, false),
            [&all[0], &all[2], &all[4]].into_iter().cloned().collect()
        );
        // Scanned directories stay as parents of their entries but are not
        // listed, the roots included.
        assert_eq!(
            scan_paths(&root, i32::MAX, false, true),
            [&all[1], &all[3], &all[5]].into_iter().cloned().collect()
        );
        assert_eq!(
            scan_paths(&root, 1, false, true),
            [&all[1]].into_iter().cloned().collect()
        );
        assert_eq!(
            scan_paths(&root, 1, false, false),
            [&all[0], &all[1], &all[2]].into_iter().cloned().collect()
        );
        // Every listed entry knows whether it is a directory, including one
        // the depth limit keeps the scan out of (`sub` at depth 1).
        for (max_depth, directories_only, files_only) in [
            (i32::MAX, false, false),
            (1, false, false),
            (i32::MAX, true, false),
            (i32::MAX, false, true),
        ] {
            let snapshot = scan_snapshot(&root, max_depth, directories_only, files_only, false);
            assert!(!snapshot.is_empty());
            for index in 0..snapshot.len() {
                let path = snapshot.item(index).unwrap();
                assert_eq!(
                    snapshot.is_dir(index),
                    Path::new(&path).is_dir(),
                    "{path} (max_depth={max_depth})"
                );
            }
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn unix_scan_skips_symlinks_and_opens_non_utf8_directories() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let root = std::env::temp_dir().join(format!("nfm-scan-unix-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let odd = root.join(OsStr::from_bytes(b"odd\xff"));
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(&odd).unwrap();
        std::fs::write(root.join("target").join("t.txt"), "").unwrap();
        std::fs::write(odd.join("inner.txt"), "").unwrap();
        std::os::unix::fs::symlink(root.join("target"), root.join("link")).unwrap();
        // A link back to the root would loop forever if links were followed.
        std::os::unix::fs::symlink(&root, root.join("loop")).unwrap();
        let r = root.to_string_lossy().into_owned();

        let paths = scan_paths(&root, i32::MAX, false, false);
        let expected: BTreeSet<String> = [
            r.clone(),
            format!("{r}/target"),
            format!("{r}/target/t.txt"),
            format!("{r}/link"),
            format!("{r}/loop"),
            format!("{r}/odd\u{FFFD}"),
            format!("{r}/odd\u{FFFD}/inner.txt"),
        ]
        .into_iter()
        .collect();
        assert_eq!(paths, expected);
        assert!(should_skip_virtual());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    fn should_skip_virtual() -> bool {
        !platform::should_descend(b"/proc")
            && !platform::should_descend(b"/sys")
            && platform::should_descend(b"/home/proc")
    }
}
