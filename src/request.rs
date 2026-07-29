use crate::delimited_store::{DelimitedStreamingSnapshot, DelimitedStreamingStore};
pub use crate::delimited_store::{DelimitedTextSelector, DelimitedValueSelector};
use crate::source_store::{AnyItemSource, SharedStore};
use nfm_search_core::store::{
    FlatSnapshot, ItemsSource, StreamingItemSnapshot, StreamingItemStore,
};
use std::io::{BufRead, BufReader, Read};
use std::sync::{Arc, Mutex};
use std::thread;

#[cfg(windows)]
use crate::list_windows::{WindowListItem, WindowPayload};
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

#[cfg(windows)]
pub struct WindowListPickerRequest {
    pub items: Vec<WindowListItem>,
}

#[cfg(windows)]
impl PickerRequest for WindowListPickerRequest {
    type Source = FlatSnapshot<WindowPayload>;

    fn search_string(&self) -> Option<&str> {
        None
    }

    fn run(&self) -> Arc<SharedStore> {
        let snapshot =
            Arc::new(FlatSnapshot::from_items(self.items.iter().map(|item| {
                (item.text.as_str(), WindowPayload { hwnd: item.hwnd })
            })));
        Arc::new(SharedStore::completed(Arc::new(AnyItemSource::Windows(
            snapshot,
        ))))
    }
}

pub struct StdinRequest {
    search_string: Option<String>,
    shared_store: Arc<SharedStore>,
    reader: Mutex<Option<Box<dyn Read + Send>>>,
}

#[derive(Clone, Debug)]
pub struct DelimitedInputOptions {
    pub delimiter: char,
    pub text: DelimitedTextSelector,
    pub value: DelimitedValueSelector,
    pub preview_file_field: Option<usize>,
    pub preview_center_line_field: Option<usize>,
}

pub struct DelimitedStdinRequest {
    options: DelimitedInputOptions,
    search_string: Option<String>,
    shared_store: Arc<SharedStore>,
    reader: Mutex<Option<Box<dyn Read + Send>>>,
}

impl DelimitedStdinRequest {
    pub fn new(options: DelimitedInputOptions, search_string: Option<String>) -> Self {
        Self {
            options,
            search_string,
            shared_store: Arc::new(SharedStore::new()),
            reader: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn with_reader(
        options: DelimitedInputOptions,
        search_string: Option<String>,
        reader: impl Read + Send + 'static,
    ) -> Self {
        Self {
            options,
            search_string,
            shared_store: Arc::new(SharedStore::new()),
            reader: Mutex::new(Some(Box::new(reader))),
        }
    }

    fn spawn_reader(&self) {
        let options = self.options.clone();
        let shared_store = Arc::clone(&self.shared_store);
        let reader: Box<dyn Read + Send> = self
            .reader
            .lock()
            .expect("stdin reader poisoned")
            .take()
            .unwrap_or_else(|| Box::new(std::io::stdin()));

        thread::spawn(move || {
            let mut store = DelimitedStreamingStore::new(
                options.delimiter,
                options.text,
                options.value,
                options.preview_file_field,
                options.preview_center_line_field,
            );
            for line in BufReader::new(reader).lines() {
                let Ok(line) = line else {
                    break;
                };
                let text = match options.text {
                    DelimitedTextSelector::FullLine => line.as_str(),
                    DelimitedTextSelector::Field(field) => {
                        let Some(text) = line
                            .splitn(field + 1, options.delimiter)
                            .nth(field)
                            .filter(|text| !text.is_empty())
                        else {
                            continue;
                        };
                        text
                    }
                };
                if text.is_empty() {
                    continue;
                }
                let search_offset = text.as_ptr() as usize - line.as_ptr() as usize;
                store.add_item(line.as_bytes(), search_offset, text.len());
                if store.len().is_multiple_of(1_000) {
                    shared_store.publish(Arc::new(AnyItemSource::Delimited(store.snapshot())));
                }
            }
            shared_store.publish(Arc::new(AnyItemSource::Delimited(store.snapshot())));
            shared_store.complete();
        });
    }
}

impl PickerRequest for DelimitedStdinRequest {
    type Source = DelimitedStreamingSnapshot;

    fn search_string(&self) -> Option<&str> {
        self.search_string.as_deref()
    }

    fn run(&self) -> Arc<SharedStore> {
        self.spawn_reader();
        Arc::clone(&self.shared_store)
    }
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
    use crate::delimited_store::DelimitedItemMetadata;
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

    #[test]
    fn delimited_request_preserves_the_last_field_and_metadata() {
        let request = DelimitedStdinRequest::with_reader(
            DelimitedInputOptions {
                delimiter: ':',
                text: DelimitedTextSelector::Field(3),
                value: DelimitedValueSelector::Field(0),
                preview_file_field: Some(0),
                preview_center_line_field: Some(1),
            },
            None,
            b"tmp\\result.txt:639:26:g:\\src\\project\\TODO\n".as_slice(),
        );
        let store = request.run();
        for _ in 0..100 {
            if store.is_done() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }

        let snapshot = store.snapshot().expect("snapshot");
        let mut stack = [0; 128];
        let mut heap = Vec::new();
        assert_eq!(
            snapshot.get_string(0, &mut stack, &mut heap),
            b"g:\\src\\project\\TODO"
        );
        assert_eq!(
            snapshot.delimited_metadata(0),
            Some(DelimitedItemMetadata {
                value: "tmp\\result.txt".into(),
                preview_item: Some("tmp\\result.txt".into()),
                preview_center_line: Some(639),
            })
        );
    }

    #[test]
    fn delimited_request_can_search_the_full_input_line() {
        let input = b"tmp\\result.txt:639:26:g:\\src\\project\\TODO\n";
        let request = DelimitedStdinRequest::with_reader(
            DelimitedInputOptions {
                delimiter: ':',
                text: DelimitedTextSelector::FullLine,
                value: DelimitedValueSelector::Field(0),
                preview_file_field: Some(0),
                preview_center_line_field: Some(1),
            },
            None,
            input.as_slice(),
        );
        let store = request.run();
        for _ in 0..100 {
            if store.is_done() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let snapshot = store.snapshot().expect("snapshot");
        let mut stack = [0; 128];
        let mut heap = Vec::new();
        assert_eq!(
            snapshot.get_string(0, &mut stack, &mut heap),
            &input[..input.len() - 1]
        );
        assert_eq!(
            snapshot.delimited_metadata(0),
            Some(DelimitedItemMetadata {
                value: "tmp\\result.txt".into(),
                preview_item: Some("tmp\\result.txt".into()),
                preview_center_line: Some(639),
            })
        );
    }

    #[test]
    fn delimited_request_defaults_value_to_the_full_line() {
        let input = b"file.txt:42:7:matching text\n";
        let request = DelimitedStdinRequest::with_reader(
            DelimitedInputOptions {
                delimiter: ':',
                text: DelimitedTextSelector::Field(3),
                value: DelimitedValueSelector::FullLine,
                preview_file_field: Some(0),
                preview_center_line_field: Some(1),
            },
            None,
            input.as_slice(),
        );
        let store = request.run();
        for _ in 0..100 {
            if store.is_done() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            store
                .snapshot()
                .expect("snapshot")
                .delimited_metadata(0)
                .expect("metadata")
                .value,
            "file.txt:42:7:matching text"
        );
    }

    #[test]
    fn search_field_keeps_delimiters_when_another_selector_is_later() {
        let request = DelimitedStdinRequest::with_reader(
            DelimitedInputOptions {
                delimiter: ':',
                text: DelimitedTextSelector::Field(3),
                value: DelimitedValueSelector::Field(4),
                preview_file_field: Some(0),
                preview_center_line_field: Some(1),
            },
            None,
            b"file.txt:42:7:matching:text\n".as_slice(),
        );
        let store = request.run();
        for _ in 0..100 {
            if store.is_done() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let snapshot = store.snapshot().expect("snapshot");
        let mut stack = [0; 128];
        let mut heap = Vec::new();
        assert_eq!(
            snapshot.get_string(0, &mut stack, &mut heap),
            b"matching:text"
        );
        assert_eq!(
            snapshot.delimited_metadata(0).expect("metadata").value,
            "text"
        );
    }

    #[cfg(windows)]
    #[test]
    fn window_list_request_retains_native_window_payload() {
        let request = WindowListPickerRequest {
            items: vec![WindowListItem {
                text: "00001234      100 app.exe Window title".into(),
                hwnd: 0x1234,
            }],
        };
        let store = request.run();
        let snapshot = store.snapshot().expect("snapshot");
        assert_eq!(snapshot.native_window(0), Some(0x1234));
        let mut stack = [0; 64];
        let mut heap = Vec::new();
        assert_eq!(
            snapshot.get_string(0, &mut stack, &mut heap),
            b"00001234      100 app.exe Window title"
        );
    }
}
