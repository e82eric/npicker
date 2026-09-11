use std::collections::HashMap;
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
    FILE_ATTRIBUTE_DIRECTORY, FINDEX_SEARCH_OPS, FIND_FIRST_EX_LARGE_FETCH, WIN32_FIND_DATAW,
};

use nfm_search_core::store::{ChunkedSnapshot, ChunkedStorage, ItemsSource};
use nfm_search_core::timing;

const NODE_CHUNK_SIZE: usize = 64 * 1024;
const NAME_CHUNK_SIZE: usize = 64 * 1024;
const PUBLISH_NODE_INTERVAL: usize = 1_000;
const BYTE_CHUNK_SIZE: usize = 1024 * 1024;

pub struct ScanOptions {
    pub roots: Vec<PathBuf>,
    pub max_depth: i32,
    pub directories_only: bool,
    pub files_only: bool,
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

impl FileWalkerScan {
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
        let offset = name.offset as usize;
        let len = name.len as usize;
        assert!(offset <= self.byte_count && len <= self.byte_count - offset);
        assert!(len <= out.len());
        self.name_bytes.copy_range_to(offset, &mut out[..len]);
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

pub struct CompactUtf8FileStore {
    nodes: ChunkedStorage<Node>,
    names: ChunkedStorage<Name>,
    name_bytes: ChunkedStorage<u8>,
    interned_names: Option<HashMap<u64, Vec<u32>>>,
    published: Arc<PublishedSnapshot>,
    snapshot_version: AtomicU64,
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

struct DirectoryWork {
    depth: usize,
    node_index: i32,
    path: Vec<u8>,
}

enum WriterCommand {
    Add {
        parent: i32,
        name: String,
        response: Option<Sender<u32>>,
    },
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
    timing::write(format!(
        "source_start mode=rust_win32 workers={worker_count} roots={} max_depth={} directories_only={} files_only={}",
        options.roots.len(),
        options.max_depth,
        options.directories_only,
        options.files_only
    ));

    let writer_status = Arc::clone(&status);
    let writer_cancelled = Arc::clone(&cancelled);
    let writer = std::thread::spawn(move || {
        store_writer_loop(writer_rx, sink, writer_status, writer_cancelled)
    });

    for root in &options.roots {
        if cancelled.load(Ordering::Acquire) {
            break;
        }

        let root_text = root.to_string_lossy().into_owned();
        let root_index = add_node_sync(&writer_tx, -1, root_text.clone());

        if !options.files_only {
            // Root entries are published through the store; no separate item callback exists in Rust.
        }

        pending.fetch_add(1, Ordering::AcqRel);
        if tx
            .send(DirectoryWork {
                depth: 0,
                node_index: root_index as i32,
                path: root_text.into_bytes(),
            })
            .is_err()
        {
            pending.fetch_sub(1, Ordering::AcqRel);
        }
    }

    let options = Arc::new(options);
    let mut workers = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        let worker_options = Arc::clone(&options);
        let worker_pending = Arc::clone(&pending);
        let worker_writer_tx = writer_tx.clone();
        let worker_tx = tx.clone();
        let worker_rx = rx.clone();
        let worker_cancelled = Arc::clone(&cancelled);
        workers.push(std::thread::spawn(move || {
            worker_loop(
                worker_options,
                worker_pending,
                worker_writer_tx,
                worker_tx,
                worker_rx,
                worker_cancelled,
            );
        }));
    }

    drop(tx);
    drop(writer_tx);

    for worker in workers {
        let _ = worker.join();
    }

    let _ = writer.join();
}

fn worker_loop(
    options: Arc<ScanOptions>,
    pending: Arc<AtomicUsize>,
    writer_tx: Sender<WriterCommand>,
    tx: Sender<DirectoryWork>,
    rx: Receiver<DirectoryWork>,
    cancelled: Arc<AtomicBool>,
) {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return;
        }

        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(work) => {
                scan_directory(&options, &pending, &writer_tx, &tx, &cancelled, work);
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
    tx: &Sender<DirectoryWork>,
    cancelled: &AtomicBool,
    work: DirectoryWork,
) {
    if cancelled.load(Ordering::Acquire) {
        return;
    }

    let search_path = make_search_path(&work.path);
    let effective_max_depth = options.effective_max_depth();

    unsafe {
        let mut find_data = WIN32_FIND_DATAW::default();
        let handle = FindFirstFileExW(
            PCWSTR(search_path.as_ptr()),
            FindExInfoBasic,
            &mut find_data as *mut WIN32_FIND_DATAW as *mut c_void,
            FINDEX_SEARCH_OPS(FindExSearchNameMatch.0),
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
        .unwrap_or(INVALID_HANDLE_VALUE);

        if handle == INVALID_HANDLE_VALUE {
            return;
        }

        let find_handle = FindHandle(handle);
        loop {
            if cancelled.load(Ordering::Acquire) {
                break;
            }

            if let Some(name) = file_name(&find_data) {
                if name != "." && name != ".." {
                    let is_dir = (find_data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0) != 0;
                    let will_recurse = is_dir && work.depth + 1 < effective_max_depth;

                    if (!options.directories_only || is_dir) && (!options.files_only || !is_dir) {
                        let child_index = if is_dir {
                            add_node_sync(writer_tx, work.node_index, name.clone())
                        } else {
                            add_node_async(writer_tx, work.node_index, name.clone());
                            u32::MAX
                        };

                        if will_recurse {
                            let child_path = make_child_path(&work.path, name.as_bytes());
                            pending.fetch_add(1, Ordering::AcqRel);
                            if tx
                                .send(DirectoryWork {
                                    depth: work.depth + 1,
                                    node_index: child_index as i32,
                                    path: child_path,
                                })
                                .is_err()
                            {
                                pending.fetch_sub(1, Ordering::AcqRel);
                            }
                        }
                    } else if will_recurse {
                        let child_index = add_node_sync(writer_tx, work.node_index, name.clone());
                        let child_path = make_child_path(&work.path, name.as_bytes());
                        pending.fetch_add(1, Ordering::AcqRel);
                        if tx
                            .send(DirectoryWork {
                                depth: work.depth + 1,
                                node_index: child_index as i32,
                                path: child_path,
                            })
                            .is_err()
                        {
                            pending.fetch_sub(1, Ordering::AcqRel);
                        }
                    }
                }
            } else {
            }

            let next = FindNextFileW(find_handle.0, &mut find_data);
            if next.is_err() {
                break;
            }
        }
    }
}

fn store_writer_loop<S>(
    rx: Receiver<WriterCommand>,
    sink: Arc<S>,
    status: Arc<AtomicU8>,
    cancelled: Arc<AtomicBool>,
) where
    S: ScanEventSink,
{
    let mut store = CompactUtf8FileStore::new();
    while let Ok(command) = rx.recv() {
        match command {
            WriterCommand::Add {
                parent,
                name,
                response,
            } => {
                let node_index = store.add_node(parent, &name);
                if node_index == 0 || (node_index + 1) % 1_000 == 0 {
                    sink.snapshot(store.snapshot());
                }
                if let Some(response) = response {
                    let _ = response.send(node_index);
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

fn add_node_sync(writer_tx: &Sender<WriterCommand>, parent: i32, name: String) -> u32 {
    let (response_tx, response_rx) = bounded(1);
    let sent = writer_tx.send(WriterCommand::Add {
        parent,
        name,
        response: Some(response_tx),
    });
    let node_index = if sent.is_ok() {
        response_rx.recv().unwrap_or(u32::MAX)
    } else {
        u32::MAX
    };
    node_index
}

fn add_node_async(writer_tx: &Sender<WriterCommand>, parent: i32, name: String) {
    let _ = writer_tx.send(WriterCommand::Add {
        parent,
        name,
        response: None,
    });
}

struct FindHandle(HANDLE);

impl Drop for FindHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = FindClose(self.0);
        }
    }
}

fn file_name(find_data: &WIN32_FIND_DATAW) -> Option<String> {
    let len = find_data
        .cFileName
        .iter()
        .position(|ch| *ch == 0)
        .unwrap_or(find_data.cFileName.len());
    if len == 0 {
        None
    } else {
        Some(String::from_utf16_lossy(&find_data.cFileName[..len]))
    }
}

fn make_search_path(path: &[u8]) -> Vec<u16> {
    let path = String::from_utf8_lossy(path);
    let mut value: Vec<u16> = path.encode_utf16().collect();
    if !path.ends_with('\\') {
        value.push('\\' as u16);
    }
    value.push('*' as u16);
    value.push(0);
    value
}

fn make_child_path(parent: &[u8], name: &[u8]) -> Vec<u8> {
    let mut path = Vec::with_capacity(parent.len() + name.len() + 1);
    path.extend_from_slice(parent);
    if !path.is_empty() && !path.ends_with(b"\\") {
        path.push(b'\\');
    }
    path.extend_from_slice(name);
    path
}
