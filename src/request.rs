#[cfg(windows)]
use crate::picker_snapshot::{ProcessPickerSnapshot, WindowPickerItem};
use crate::PickerItem;
use nfm_picker_sources::delimited::{DelimitedStreamingSnapshot, DelimitedStreamingStore};
pub use nfm_picker_sources::delimited::{DelimitedTextSelector, DelimitedValueSelector};
use nfm_picker_sources::structured::{
    StructuredSchema, StructuredStreamingSnapshot, StructuredStreamingStore,
};
use nfm_search_core::snapshot_store::SnapshotStore;
use nfm_search_core::source::{SearchSource, TypedSearchSource};
use nfm_search_core::store::StreamingItemStore;
use nfm_search_core::store::{FlatSnapshot, ItemsSource, StreamingItemSnapshot};
use std::io::{BufRead, BufReader, Read};
use std::sync::{Arc, Mutex};
use std::thread;

fn picker_source<S>(store: Arc<SnapshotStore<S>>) -> Arc<dyn SearchSource<Item = S::Item>>
where
    S: ItemsSource + Send + Sync + 'static,
    S::Item: PickerItem,
{
    Arc::new(TypedSearchSource::new(store))
}

fn completed_picker_source<S>(snapshot: Arc<S>) -> Arc<dyn SearchSource<Item = S::Item>>
where
    S: ItemsSource + Send + Sync + 'static,
    S::Item: PickerItem,
{
    let store = Arc::new(SnapshotStore::new());
    store.publish(snapshot);
    store.complete();
    picker_source(store)
}

use crate::action::PickerState;
#[cfg(windows)]
use crate::list_processes::ProcessInfo;
#[cfg(windows)]
use crate::list_windows::WindowListItem;
#[cfg(windows)]
use nfm_file_system::walker::{start_scan, PublishedSnapshot, ScanOptions};
#[cfg(windows)]
use nfm_file_system::walker_search_store::FileSystemSearchStore;
#[cfg(windows)]
use std::path::PathBuf;

pub trait PickerRequest {
    type Source: ItemsSource<Item: PickerItem> + Send + Sync + 'static;

    fn search_string(&self) -> Option<&str>;
    fn run(&self) -> Arc<dyn SearchSource<Item = <Self::Source as ItemsSource>::Item>>;
    fn picker_state(&self) -> PickerState;
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

    fn run(&self) -> Arc<dyn SearchSource<Item = <Self::Source as ItemsSource>::Item>> {
        let roots = if self.root_directories.is_empty() {
            vec![std::env::current_dir().expect("cannot get current directory")]
        } else {
            self.root_directories.iter().map(PathBuf::from).collect()
        };
        let shared = Arc::new(FileSystemSearchStore::new());
        start_scan(
            ScanOptions {
                roots,
                max_depth: self.max_depth,
                directories_only: self.directories_only,
                files_only: self.files_only,
            },
            Arc::clone(&shared),
        );
        picker_source(shared)
    }

    fn picker_state(&self) -> PickerState {
        PickerState::Filewalker {
            roots: self.root_directories.clone(),
        }
    }
}

pub struct FlatItemsPickerRequest {
    pub items: Vec<String>,
    pub search_string: Option<String>,
}

#[derive(Clone, Debug)]
pub struct StructuredPickerRow {
    pub cells: Vec<String>,
    pub preview: Option<String>,
}

pub struct StructuredItemsPickerRequest {
    pub columns: Vec<String>,
    pub rows: Vec<StructuredPickerRow>,
    pub search_string: Option<String>,
}

impl PickerRequest for StructuredItemsPickerRequest {
    type Source = StructuredStreamingSnapshot;

    fn search_string(&self) -> Option<&str> {
        self.search_string.as_deref()
    }

    fn run(&self) -> Arc<dyn SearchSource<Item = <Self::Source as ItemsSource>::Item>> {
        let schema = StructuredSchema::new(self.columns.clone())
            .expect("StructuredItemsPickerRequest must have a valid schema");
        let mut store = StructuredStreamingStore::new(schema);
        for row in &self.rows {
            store
                .add_record_with_preview(
                    &csv::StringRecord::from(row.cells.clone()),
                    row.preview.as_deref(),
                )
                .expect("StructuredItemsPickerRequest rows must match the schema");
        }
        completed_picker_source(store.snapshot())
    }

    fn picker_state(&self) -> PickerState {
        PickerState::StructuredStdin
    }
}

impl PickerRequest for FlatItemsPickerRequest {
    type Source = StreamingItemSnapshot;

    fn search_string(&self) -> Option<&str> {
        self.search_string.as_deref()
    }

    fn run(&self) -> Arc<dyn SearchSource<Item = <Self::Source as ItemsSource>::Item>> {
        let mut store = StreamingItemStore::new();
        for item in &self.items {
            store.add_item(item.as_bytes());
        }
        store.complete_adding();
        completed_picker_source(store.snapshot())
    }

    fn picker_state(&self) -> PickerState {
        PickerState::Stdin
    }
}

#[cfg(windows)]
pub struct WindowListPickerRequest {
    pub items: Vec<WindowListItem>,
}

#[cfg(windows)]
impl PickerRequest for WindowListPickerRequest {
    type Source = FlatSnapshot<WindowPickerItem>;

    fn search_string(&self) -> Option<&str> {
        None
    }

    fn run(&self) -> Arc<dyn SearchSource<Item = <Self::Source as ItemsSource>::Item>> {
        let snapshot = Arc::new(FlatSnapshot::from_items(self.items.iter().map(|item| {
            (
                item.text.as_str(),
                WindowPickerItem {
                    title: item.text.clone(),
                    native_window: item.hwnd,
                },
            )
        })));
        completed_picker_source(snapshot)
    }

    fn picker_state(&self) -> PickerState {
        PickerState::Stdin
    }
}

#[cfg(windows)]
pub struct ProcessListPickerRequest {
    pub items: Vec<ProcessInfo>,
}

#[cfg(windows)]
impl PickerRequest for ProcessListPickerRequest {
    type Source = ProcessPickerSnapshot;

    fn search_string(&self) -> Option<&str> {
        None
    }

    fn run(&self) -> Arc<dyn SearchSource<Item = <Self::Source as ItemsSource>::Item>> {
        completed_picker_source(Arc::new(ProcessPickerSnapshot::from_items(&self.items)))
    }

    fn picker_state(&self) -> PickerState {
        PickerState::StructuredStdin
    }
}

pub struct StdinRequest {
    search_string: Option<String>,
    shared_store: Arc<SnapshotStore<StreamingItemSnapshot>>,
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
    shared_store: Arc<SnapshotStore<DelimitedStreamingSnapshot>>,
    reader: Mutex<Option<Box<dyn Read + Send>>>,
}

#[derive(Clone, Debug)]
pub enum CsvHeaderMode {
    FirstRecord,
    Explicit(Vec<String>),
}

#[derive(Clone, Debug)]
pub struct StructuredCsvOptions {
    pub headers: CsvHeaderMode,
    pub delimiter: u8,
}

pub struct StructuredCsvStdinRequest {
    options: StructuredCsvOptions,
    search_string: Option<String>,
    shared_store: Arc<SnapshotStore<StructuredStreamingSnapshot>>,
    reader: Mutex<Option<Box<dyn Read + Send>>>,
}

impl StructuredCsvStdinRequest {
    pub fn new(options: StructuredCsvOptions, search_string: Option<String>) -> Self {
        Self {
            options,
            search_string,
            shared_store: Arc::new(SnapshotStore::new()),
            reader: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn with_reader(options: StructuredCsvOptions, reader: impl Read + Send + 'static) -> Self {
        Self {
            options,
            search_string: None,
            shared_store: Arc::new(SnapshotStore::new()),
            reader: Mutex::new(Some(Box::new(reader))),
        }
    }

    fn spawn_reader(&self) {
        let options = self.options.clone();
        let shared = Arc::clone(&self.shared_store);
        let reader = self
            .reader
            .lock()
            .expect("stdin reader poisoned")
            .take()
            .unwrap_or_else(|| Box::new(std::io::stdin()));
        thread::spawn(move || {
            let mut csv = csv::ReaderBuilder::new()
                .has_headers(false)
                .flexible(true)
                .delimiter(options.delimiter)
                .from_reader(reader);
            let mut records = csv.records();
            let schema = match options.headers {
                CsvHeaderMode::FirstRecord => match records.next() {
                    Some(Ok(record)) => {
                        StructuredSchema::new(record.iter().map(str::to_owned).collect())
                    }
                    Some(Err(error)) => Err(format!("failed to read CSV header: {error}")),
                    None => Err("CSV input did not contain a header record".into()),
                },
                CsvHeaderMode::Explicit(columns) => StructuredSchema::new(columns),
            };
            let schema = match schema {
                Ok(schema) => schema,
                Err(error) => {
                    eprintln!("{error}");
                    shared.complete();
                    return;
                }
            };
            let mut store = StructuredStreamingStore::new(schema);
            shared.publish(store.snapshot());
            for record in records {
                let record = match record {
                    Ok(record) => record,
                    Err(error) => {
                        eprintln!("failed to read csv record: {error}");
                        break;
                    }
                };

                if let Err(error) = store.add_record(&record) {
                    eprintln!("ignoring CSV record: {error}");
                    continue;
                }

                if store.len().is_multiple_of(1_000) {
                    shared.publish(store.snapshot());
                }
            }
            shared.publish(store.snapshot());
            shared.complete();
        });
    }
}

impl PickerRequest for StructuredCsvStdinRequest {
    type Source = StructuredStreamingSnapshot;
    fn search_string(&self) -> Option<&str> {
        self.search_string.as_deref()
    }
    fn run(&self) -> Arc<dyn SearchSource<Item = <Self::Source as ItemsSource>::Item>> {
        self.spawn_reader();
        picker_source(Arc::clone(&self.shared_store))
    }
    fn picker_state(&self) -> PickerState {
        PickerState::StructuredStdin
    }
}

impl DelimitedStdinRequest {
    pub fn new(options: DelimitedInputOptions, search_string: Option<String>) -> Self {
        Self {
            options,
            search_string,
            shared_store: Arc::new(SnapshotStore::new()),
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
            shared_store: Arc::new(SnapshotStore::new()),
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
                    shared_store.publish(store.snapshot());
                }
            }
            shared_store.publish(store.snapshot());
            shared_store.complete();
        });
    }
}

impl PickerRequest for DelimitedStdinRequest {
    type Source = DelimitedStreamingSnapshot;

    fn search_string(&self) -> Option<&str> {
        self.search_string.as_deref()
    }

    fn run(&self) -> Arc<dyn SearchSource<Item = <Self::Source as ItemsSource>::Item>> {
        self.spawn_reader();
        picker_source(Arc::clone(&self.shared_store))
    }

    fn picker_state(&self) -> PickerState {
        PickerState::DelimitedStdin
    }
}

impl StdinRequest {
    pub fn new(search_string: Option<String>) -> Self {
        Self {
            shared_store: Arc::new(SnapshotStore::new()),
            search_string,
            reader: Mutex::new(None),
        }
    }

    #[cfg(test)]
    fn with_reader(search_string: Option<String>, reader: impl Read + Send + 'static) -> Self {
        Self {
            shared_store: Arc::new(SnapshotStore::new()),
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
                    shared_store.publish(store.snapshot());
                }
            }
            store.complete_adding();
            shared_store.publish(store.snapshot());
            shared_store.complete();
        });
    }
}

impl PickerRequest for StdinRequest {
    type Source = StreamingItemSnapshot;

    fn search_string(&self) -> Option<&str> {
        self.search_string.as_deref()
    }

    fn run(&self) -> Arc<dyn SearchSource<Item = <Self::Source as ItemsSource>::Item>> {
        self.spawn_reader();
        picker_source(Arc::clone(&self.shared_store))
    }

    fn picker_state(&self) -> PickerState {
        PickerState::Stdin
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum PickerResponse<I> {
    Selected(I),
    Cancelled,
    Error(String),
}

impl<I> PickerResponse<I> {
    pub fn cancelled() -> Self {
        Self::Cancelled
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self::Error(message.into())
    }

    pub fn status_label(&self) -> &'static str {
        match self {
            Self::Selected(_) => "selected",
            Self::Cancelled => "cancelled",
            Self::Error(_) => "error",
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
        assert_eq!(snapshot.item(0).expect("first item"), "one");
        assert_eq!(snapshot.item(1).expect("second item"), "three");
    }

    #[test]
    fn structured_csv_streams_headers_quotes_and_multiline_records() {
        let request = StructuredCsvStdinRequest::with_reader(
            StructuredCsvOptions {
                headers: CsvHeaderMode::FirstRecord,
                delimiter: b',',
            },
            b"Name,Description\nalpha,plain\n\"beta\",\"two\nlines\"\n".as_slice(),
        );
        let store = request.run();
        for _ in 0..100 {
            if store.is_done() {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
        let snapshot = store.snapshot().expect("snapshot");
        let item = snapshot.item(1).expect("item");
        assert_eq!(item.value, "beta,\"two\nlines\"");
        assert_eq!(item.fields.get("Name").map(String::as_str), Some("beta"));
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
        let item = snapshot.item(0).expect("item");
        assert_eq!(item.preview_item.as_deref(), Some("tmp\\result.txt"));
        assert_eq!(item.value, "tmp\\result.txt");
        assert_eq!(item.preview_center_line, Some(639));
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
        let item = snapshot.item(0).expect("item");
        assert_eq!(item.value, "tmp\\result.txt");
        assert_eq!(item.preview_center_line, Some(639));
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
                .item(0)
                .expect("item")
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
        assert_eq!(snapshot.item(0).expect("item").value, "text");
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
        assert_eq!(snapshot.item(0).expect("item").native_window, 4660);
        assert_eq!(
            snapshot.item(0).expect("item").title,
            "00001234      100 app.exe Window title"
        );
    }

    #[cfg(windows)]
    #[test]
    fn process_list_request_retains_structured_fields() {
        let request = ProcessListPickerRequest {
            items: vec![ProcessInfo {
                name: "example.exe".into(),
                pid: 1234,
                working_set_kb: 2048,
                private_bytes_kb: 4096,
                cpu_seconds: 1,
            }],
        };
        assert_eq!(request.search_string(), None);
        let store = request.run();
        let snapshot = store.snapshot().expect("snapshot");
        let item = snapshot.item(0).expect("item");
        assert_eq!(item.name, "example.exe");
        assert_eq!(item.pid, 1234);
    }

    #[cfg(windows)]
    #[test]
    fn process_list_request_retains_structured_search_and_completions() {
        let request = ProcessListPickerRequest {
            items: vec![
                ProcessInfo {
                    name: "small.exe".into(),
                    pid: 100,
                    working_set_kb: 100,
                    private_bytes_kb: 200,
                    cpu_seconds: 1,
                },
                ProcessInfo {
                    name: "large.exe".into(),
                    pid: 2_000,
                    working_set_kb: 300,
                    private_bytes_kb: 400,
                    cpu_seconds: 2,
                },
            ],
        };
        let store = request.run();
        let snapshot = store.snapshot().expect("snapshot");
        let plan = snapshot.create_search_plan("/:PID>1000");
        assert!(!plan.includes(0));
        assert!(plan.includes(1));
        assert!(snapshot
            .completions("/:PI", 4)
            .iter()
            .any(|completion| completion.replacement.starts_with("/:PID")));
    }
}
