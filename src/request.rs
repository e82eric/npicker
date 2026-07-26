use crate::source_store::{AnyItemSource, SharedStore};
use nfm_search_core::store::{
    FlatSnapshot, ItemsSource, StreamingItemSnapshot, StreamingItemStore,
};
use std::io::{BufRead, BufReader, Read};
use std::sync::{Arc, Mutex};
use std::thread;

#[cfg(windows)]
use nfm_file_system::walker::{start_scan, PublishedSnapshot, ScanOptions};
#[cfg(windows)]
use std::path::PathBuf;

pub trait PickerRequest {
    type Source: ItemsSource;

    fn search_string(&self) -> Option<&str>;
    fn run(&self) -> Arc<SharedStore>;
}

#[cfg(windows)]
pub struct FileSystemPickerRequest {
    pub root_directories: Vec<String>,
    pub max_depth: i32,
    pub directories_only: bool,
    pub files_only: bool,
    pub search_string: Option<String>,
}

#[cfg(windows)]
impl PickerRequest for FileSystemPickerRequest {
    type Source = PublishedSnapshot;

    fn search_string(&self) -> Option<&str> {
        self.search_string.as_deref()
    }

    fn run(&self) -> Arc<SharedStore> {
        let roots = if self.root_directories.is_empty() {
            vec![std::env::current_dir().expect("cannot get current directory")]
        } else {
            self.root_directories.iter().map(PathBuf::from).collect()
        };
        let shared = Arc::new(SharedStore::new());
        start_scan(
            ScanOptions {
                roots,
                max_depth: self.max_depth,
                directories_only: self.directories_only,
                files_only: self.files_only,
            },
            Arc::clone(&shared),
        );
        shared
    }
}

pub struct FlatItemsPickerRequest {
    pub items: Vec<String>,
    pub search_string: Option<String>,
}

impl PickerRequest for FlatItemsPickerRequest {
    type Source = FlatSnapshot<()>;

    fn search_string(&self) -> Option<&str> {
        self.search_string.as_deref()
    }

    fn run(&self) -> Arc<SharedStore> {
        let snapshot = Arc::new(FlatSnapshot::from_items(
            self.items.iter().map(|item| (item, ())),
        ));
        Arc::new(SharedStore::completed(Arc::new(AnyItemSource::Flat(
            snapshot,
        ))))
    }
}

pub struct StdinRequest {
    search_string: Option<String>,
    shared_store: Arc<SharedStore>,
    reader: Mutex<Option<Box<dyn Read + Send>>>,
}

impl StdinRequest {
    pub fn new(search_string: Option<String>) -> Self {
        Self {
            shared_store: Arc::new(SharedStore::new()),
            search_string,
            reader: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn with_reader(search_string: Option<String>, reader: impl Read + Send + 'static) -> Self {
        Self {
            shared_store: Arc::new(SharedStore::new()),
            search_string,
            reader: Mutex::new(Some(Box::new(reader))),
        }
    }

    fn spawn_reader(&self) {
        let shared_store = Arc::clone(&self.shared_store);
        let reader: Box<dyn Read + Send> = self
            .reader
            .lock()
            .expect("stdin reader poisoned")
            .take()
            .unwrap_or_else(|| Box::new(std::io::stdin()));

        thread::spawn(move || {
            let mut store = StreamingItemStore::new();
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else {
                    break;
                };
                if line.is_empty() {
                    continue;
                }
                let node_index = store.add_item(line.as_bytes());
                if (node_index + 1).is_multiple_of(1_000) {
                    shared_store.publish(Arc::new(AnyItemSource::Streaming(store.snapshot())));
                }
            }
            store.complete_adding();
            shared_store.publish(Arc::new(AnyItemSource::Streaming(store.snapshot())));
            shared_store.complete();
        });
    }
}

impl PickerRequest for StdinRequest {
    type Source = StreamingItemSnapshot;

    fn search_string(&self) -> Option<&str> {
        self.search_string.as_deref()
    }

    fn run(&self) -> Arc<SharedStore> {
        self.spawn_reader();
        Arc::clone(&self.shared_store)
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PickerResponse {
    pub status: String,
    pub selected_item: Option<String>,
    pub selected_path: Option<String>,
    pub error_message: Option<String>,
}

impl PickerResponse {
    pub fn selected(path: String) -> Self {
        Self {
            status: "selected".to_string(),
            selected_item: Some(path.clone()),
            selected_path: Some(path),
            error_message: None,
        }
    }

    pub fn cancelled() -> Self {
        Self {
            status: "cancelled".to_string(),
            selected_item: None,
            selected_path: None,
            error_message: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            status: "error".to_string(),
            selected_item: None,
            selected_path: None,
            error_message: Some(message.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn stdin_request_streams_non_empty_lines() {
        let request = StdinRequest::with_reader(None, b"one\n\nthree\n".as_slice());
        let store = request.run();
        for _ in 0..100 {
            if store.is_done() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let snapshot = store.snapshot().expect("snapshot");
        assert_eq!(snapshot.len(), 2);
    }
}
