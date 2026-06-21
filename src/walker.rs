use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded, unbounded};
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FIND_FIRST_EX_LARGE_FETCH, FINDEX_SEARCH_OPS, FindClose,
    FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW, WIN32_FIND_DATAW,
};
use windows::core::PCWSTR;

use crate::store::{AnyItemSource, CompactUtf8FileStore, ItemsSource};
use crate::timing;

pub struct ScanOptions {
    pub roots: Vec<PathBuf>,
    pub max_depth: usize,
    pub directories_only: bool,
    pub files_only: bool,
}

pub struct SharedStore {
    published: RwLock<Arc<AnyItemSource>>,
    done: AtomicBool,
    stats: ScanStats,
}

impl SharedStore {
    pub fn new(initial: Arc<AnyItemSource>) -> Self {
        Self {
            published: RwLock::new(initial),
            done: AtomicBool::new(false),
            stats: ScanStats::default(),
        }
    }

    pub fn completed(source: Arc<AnyItemSource>) -> Self {
        Self {
            published: RwLock::new(source),
            done: AtomicBool::new(true),
            stats: ScanStats::default(),
        }
    }

    pub fn snapshot(&self) -> Arc<AnyItemSource> {
        Arc::clone(&self.published.read().expect("store poisoned"))
    }

    pub fn publish(&self, source: Arc<AnyItemSource>){
        *self.published.write().expect("store poisoned") = source;
    }

    pub fn snapshot_version(&self) -> u64 {
        self.published.read().expect("store poisoned").version()
    }

    pub fn complete(&self) {
        self.done.store(true, Ordering::Release);
    }

    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
}

#[derive(Default)]
struct ScanStats {
    items: AtomicUsize,
    directories: AtomicUsize,
    path_us: AtomicU64,
    find_us: AtomicU64,
    name_us: AtomicU64,
    add_us: AtomicU64,
    writer_us: AtomicU64,
    queue_us: AtomicU64,
    first_item_logged: AtomicBool,
    first_visible_batch_logged: AtomicBool,
    first_complete_chunk_logged: AtomicBool,
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

pub fn start_scan(options: ScanOptions) -> Arc<SharedStore> {
    let file_store = CompactUtf8FileStore::new();
    let initial = Arc::new(AnyItemSource::FileSystem(file_store.snapshot()));
    let store = Arc::new(SharedStore::new(initial));
    let scan_store = Arc::clone(&store);

    std::thread::spawn(move || {
        scan(scan_store, options);
    });

    store
}

fn scan(shared: Arc<SharedStore>, options: ScanOptions) {
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

    let writer_shared = Arc::clone(&shared);
    let writer = std::thread::spawn(move || store_writer_loop(writer_shared, writer_rx));

    for root in &options.roots {
        let root_text = root.to_string_lossy().into_owned();
        let root_index = add_node_sync(&shared, &writer_tx, -1, root_text.clone());

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
        let worker_shared = Arc::clone(&shared);
        let worker_options = Arc::clone(&options);
        let worker_pending = Arc::clone(&pending);
        let worker_writer_tx = writer_tx.clone();
        let worker_tx = tx.clone();
        let worker_rx = rx.clone();
        workers.push(std::thread::spawn(move || {
            worker_loop(
                worker_shared,
                worker_options,
                worker_pending,
                worker_writer_tx,
                worker_tx,
                worker_rx,
            );
        }));
    }

    drop(tx);
    drop(writer_tx);

    for worker in workers {
        let _ = worker.join();
    }

    let _ = writer.join();
    timing::write(format!(
        "source_done items={} dirs={} path_us={} find_us={} name_us={} add_us={} writer_us={} queue_us={}",
        shared.stats.items.load(Ordering::Relaxed),
        shared.stats.directories.load(Ordering::Relaxed),
        shared.stats.path_us.load(Ordering::Relaxed),
        shared.stats.find_us.load(Ordering::Relaxed),
        shared.stats.name_us.load(Ordering::Relaxed),
        shared.stats.add_us.load(Ordering::Relaxed),
        shared.stats.writer_us.load(Ordering::Relaxed),
        shared.stats.queue_us.load(Ordering::Relaxed),
    ));
}

fn worker_loop(
    shared: Arc<SharedStore>,
    options: Arc<ScanOptions>,
    pending: Arc<AtomicUsize>,
    writer_tx: Sender<WriterCommand>,
    tx: Sender<DirectoryWork>,
    rx: Receiver<DirectoryWork>,
) {
    loop {
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(work) => {
                scan_directory(&shared, &options, &pending, &writer_tx, &tx, work);
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
    shared: &SharedStore,
    options: &ScanOptions,
    pending: &AtomicUsize,
    writer_tx: &Sender<WriterCommand>,
    tx: &Sender<DirectoryWork>,
    work: DirectoryWork,
) {
    if work.depth >= options.max_depth {
        return;
    }

    shared.stats.directories.fetch_add(1, Ordering::Relaxed);
    let path_start = Instant::now();
    let search_path = make_search_path(&work.path);
    shared
        .stats
        .path_us
        .fetch_add(timing::elapsed_us(path_start) as u64, Ordering::Relaxed);

    unsafe {
        let mut find_data = WIN32_FIND_DATAW::default();
        let find_start = Instant::now();
        let handle = FindFirstFileExW(
            PCWSTR(search_path.as_ptr()),
            FindExInfoBasic,
            &mut find_data as *mut WIN32_FIND_DATAW as *mut c_void,
            FINDEX_SEARCH_OPS(FindExSearchNameMatch.0),
            None,
            FIND_FIRST_EX_LARGE_FETCH,
        )
        .unwrap_or(INVALID_HANDLE_VALUE);
        shared
            .stats
            .find_us
            .fetch_add(timing::elapsed_us(find_start) as u64, Ordering::Relaxed);

        if handle == INVALID_HANDLE_VALUE {
            return;
        }

        let find_handle = FindHandle(handle);
        loop {
            let name_start = Instant::now();
            if let Some(name) = file_name(&find_data) {
                shared
                    .stats
                    .name_us
                    .fetch_add(timing::elapsed_us(name_start) as u64, Ordering::Relaxed);
                if name != "." && name != ".." {
                    let is_dir = (find_data.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY.0) != 0;

                    if (!options.directories_only || is_dir) && (!options.files_only || !is_dir) {
                        let child_index = if is_dir {
                            add_node_sync(shared, writer_tx, work.node_index, name.clone())
                        } else {
                            add_node_async(shared, writer_tx, work.node_index, name.clone());
                            u32::MAX
                        };

                        if is_dir {
                            let child_path = make_child_path(&work.path, name.as_bytes());
                            pending.fetch_add(1, Ordering::AcqRel);
                            let queue_start = Instant::now();
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
                            shared.stats.queue_us.fetch_add(
                                timing::elapsed_us(queue_start) as u64,
                                Ordering::Relaxed,
                            );
                        }
                    } else if is_dir {
                        let child_index =
                            add_node_sync(shared, writer_tx, work.node_index, name.clone());
                        let child_path = make_child_path(&work.path, name.as_bytes());
                        pending.fetch_add(1, Ordering::AcqRel);
                        let queue_start = Instant::now();
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
                        shared
                            .stats
                            .queue_us
                            .fetch_add(timing::elapsed_us(queue_start) as u64, Ordering::Relaxed);
                    }
                }
            } else {
                shared
                    .stats
                    .name_us
                    .fetch_add(timing::elapsed_us(name_start) as u64, Ordering::Relaxed);
            }

            let find_next_start = Instant::now();
            let next = FindNextFileW(find_handle.0, &mut find_data);
            shared.stats.find_us.fetch_add(
                timing::elapsed_us(find_next_start) as u64,
                Ordering::Relaxed,
            );
            if next.is_err() {
                break;
            }
        }
    }
}

fn record_item(shared: &SharedStore) {
    let items = shared.stats.items.fetch_add(1, Ordering::Relaxed) + 1;
    if shared
        .stats
        .first_item_logged
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        timing::write("source_first_item");
    }

    if items >= 1_000
        && shared
            .stats
            .first_visible_batch_logged
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        timing::write(format!(
            "source_first_visible_batch_search_signal items={items}"
        ));
    }

    if items >= 10_000
        && shared
            .stats
            .first_complete_chunk_logged
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    {
        timing::write(format!("source_first_complete_chunk items={items}"));
    }

    if items % 10_000 == 0 {
        timing::write(format!(
            "scan_progress items={} dirs={} path_us={} find_us={} name_us={} add_us={} writer_us={} queue_us={}",
            items,
            shared.stats.directories.load(Ordering::Relaxed),
            shared.stats.path_us.load(Ordering::Relaxed),
            shared.stats.find_us.load(Ordering::Relaxed),
            shared.stats.name_us.load(Ordering::Relaxed),
            shared.stats.add_us.load(Ordering::Relaxed),
            shared.stats.writer_us.load(Ordering::Relaxed),
            shared.stats.queue_us.load(Ordering::Relaxed),
        ));
    }
}

fn store_writer_loop(shared: Arc<SharedStore>, rx: Receiver<WriterCommand>) {
    let mut store = CompactUtf8FileStore::new();
    while let Ok(command) = rx.recv() {
        match command {
            WriterCommand::Add {
                parent,
                name,
                response,
            } => {
                let add_start = Instant::now();
                let node_index = store.add_node(parent, &name);
                shared
                    .stats
                    .writer_us
                    .fetch_add(timing::elapsed_us(add_start) as u64, Ordering::Relaxed);
                if node_index == 0 || (node_index + 1) % 1_000 == 0 {
                    publish_snapshot(&shared, &store);
                }
                record_item(&shared);
                if let Some(response) = response {
                    let _ = response.send(node_index);
                }
            }
        }
    }

    let add_start = Instant::now();
    store.complete_adding();
    shared
        .stats
        .writer_us
        .fetch_add(timing::elapsed_us(add_start) as u64, Ordering::Relaxed);
    publish_snapshot(&shared, &store);
    shared.done.store(true, Ordering::Release);
}

fn publish_snapshot(shared: &SharedStore, store: &CompactUtf8FileStore) {
    *shared.published.write().expect("store poisoned") =
        Arc::new(AnyItemSource::FileSystem(store.snapshot()));
}

fn add_node_sync(
    shared: &SharedStore,
    writer_tx: &Sender<WriterCommand>,
    parent: i32,
    name: String,
) -> u32 {
    let add_start = Instant::now();
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
    shared
        .stats
        .add_us
        .fetch_add(timing::elapsed_us(add_start) as u64, Ordering::Relaxed);
    node_index
}

fn add_node_async(
    shared: &SharedStore,
    writer_tx: &Sender<WriterCommand>,
    parent: i32,
    name: String,
) {
    let add_start = Instant::now();
    let _ = writer_tx.send(WriterCommand::Add {
        parent,
        name,
        response: None,
    });
    shared
        .stats
        .add_us
        .fetch_add(timing::elapsed_us(add_start) as u64, Ordering::Relaxed);
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
