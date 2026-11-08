#[cfg(windows)]
use nfm_picker_sources::structured::{
    StructuredSchema, StructuredStreamingSnapshot, StructuredStreamingStore,
};
use nfm_search_core::store::{ItemsSource, SearchPlan};
use std::sync::Arc;

#[cfg(windows)]
use crate::list_processes::ProcessInfo;
#[cfg(windows)]
#[cfg(windows)]
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WindowPickerItem {
    pub title: String,
    pub native_window: isize,
}

#[cfg(windows)]
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProcessPickerItem {
    pub value: String,
    pub name: String,
    pub pid: u32,
    pub working_set_kb: u64,
    pub private_bytes_kb: u64,
    pub cpu_seconds: u64,
}

#[cfg(windows)]
pub struct ProcessPickerSnapshot {
    version: u64,
    structured: Arc<StructuredStreamingSnapshot>,
    processes: Arc<[ProcessInfo]>,
}

#[cfg(windows)]
impl ProcessPickerSnapshot {
    pub fn from_items(items: &[ProcessInfo]) -> Self {
        let schema = StructuredSchema::new(vec![
            "Name".into(),
            "PID".into(),
            "WorkingSet".into(),
            "PrivateBytes".into(),
            "CPU".into(),
        ])
        .expect("process schema is valid")
        // Sizes are in KB; CPU is seconds.
        .with_types(concat!(
            "Name:text,PID:number,WorkingSet:bytes=KB,",
            "PrivateBytes:bytes=KB,CPU:duration"
        ))
        .expect("process types are valid")
        // The table shows `117.7 MB` and `1h 2m`; sorting and filters use the
        // raw KB and seconds.
        .with_formats("WorkingSet:bytes,PrivateBytes:bytes,CPU:duration")
        .expect("process formats are valid")
        // Busiest first until the query has text or sorts of its own.
        .with_default_sort("/!Descending==CPU /!Descending==PrivateBytes")
        .expect("process sorts are valid");
        let mut store = StructuredStreamingStore::new(schema);
        for process in items {
            store
                .add_record(&csv::StringRecord::from(vec![
                    process.name.clone(),
                    process.pid.to_string(),
                    process.working_set_kb.to_string(),
                    process.private_bytes_kb.to_string(),
                    process.cpu_seconds.to_string(),
                ]))
                .expect("process record matches schema");
        }
        Self {
            version: {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            },
            structured: store.snapshot(),
            processes: Arc::from(items.to_vec()),
        }
    }
}

#[cfg(windows)]
impl ItemsSource for ProcessPickerSnapshot {
    type Item = ProcessPickerItem;

    fn version(&self) -> u64 {
        self.version
    }

    fn len(&self) -> usize {
        self.structured.len()
    }

    fn is_empty(&self) -> bool {
        self.structured.is_empty()
    }

    fn get_string<'a>(
        &'a self,
        index: usize,
        stack_buffer: &'a mut [u8],
        heap_buffer: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        self.structured.get_string(index, stack_buffer, heap_buffer)
    }

    fn get_string_lossy(&self, node_index: usize, out: &mut Vec<u8>) -> String {
        self.structured.get_string_lossy(node_index, out)
    }

    fn create_search_plan(&self, query: &str) -> Box<dyn SearchPlan + '_> {
        self.structured.create_search_plan(query)
    }

    fn item(&self, node_index: usize) -> Option<Self::Item> {
        let process = self.processes.get(node_index)?;
        Some(ProcessPickerItem {
            value: self
                .structured
                .get_string_lossy(node_index, &mut Vec::new()),
            name: process.name.clone(),
            pid: process.pid,
            working_set_kb: process.working_set_kb,
            private_bytes_kb: process.private_bytes_kb,
            cpu_seconds: process.cpu_seconds,
        })
    }

    fn header(&self, query: &str) -> Option<String> {
        Some(self.structured.header(query))
    }

    fn display_text(&self, node_index: usize, query: &str) -> Option<String> {
        self.structured.display_text(node_index, query)
    }

    fn completions(
        &self,
        input: &str,
        cursor: usize,
    ) -> Vec<nfm_search_core::store::SearchCompletion> {
        self.structured.completions(input, cursor)
    }

    fn effective_query(&self, input: &str) -> String {
        self.structured.effective_query(input)
    }
}

/// Top-level windows as a table: `HWND`, `PID`, `Process` and `Title`
/// columns under a header, with column filters (`/:Process==Code.exe`) and
/// sorting (`/!Descending==PID`) like the process picker.
#[cfg(windows)]
pub struct WindowPickerSnapshot {
    version: u64,
    structured: Arc<StructuredStreamingSnapshot>,
    windows: Arc<[WindowPickerItem]>,
}

#[cfg(windows)]
impl WindowPickerSnapshot {
    pub fn from_items(items: &[crate::list_windows::WindowListItem]) -> Self {
        let schema = StructuredSchema::new(vec![
            "HWND".into(),
            "PID".into(),
            "Process".into(),
            "Title".into(),
        ])
        .expect("window schema is valid")
        // Handles are hex, so they are text, not numbers.
        .with_types("HWND:text,PID:number,Process:text,Title:text")
        .expect("window types are valid");
        let mut store = StructuredStreamingStore::new(schema);
        for window in items {
            store
                .add_record(&csv::StringRecord::from(vec![
                    format!("{:08X}", window.hwnd as usize),
                    window.process_id.to_string(),
                    window.process_name.clone(),
                    window.title.clone(),
                ]))
                .expect("window record matches schema");
        }
        Self {
            version: {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            },
            structured: store.snapshot(),
            windows: items
                .iter()
                .map(|window| WindowPickerItem {
                    title: window.text.clone(),
                    native_window: window.hwnd,
                })
                .collect(),
        }
    }
}

#[cfg(windows)]
impl ItemsSource for WindowPickerSnapshot {
    type Item = WindowPickerItem;

    fn version(&self) -> u64 {
        self.version
    }

    fn len(&self) -> usize {
        self.structured.len()
    }

    fn is_empty(&self) -> bool {
        self.structured.is_empty()
    }

    fn get_string<'a>(
        &'a self,
        index: usize,
        stack_buffer: &'a mut [u8],
        heap_buffer: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        self.structured.get_string(index, stack_buffer, heap_buffer)
    }

    fn get_string_lossy(&self, node_index: usize, out: &mut Vec<u8>) -> String {
        self.structured.get_string_lossy(node_index, out)
    }

    fn create_search_plan(&self, query: &str) -> Box<dyn SearchPlan + '_> {
        self.structured.create_search_plan(query)
    }

    fn item(&self, node_index: usize) -> Option<Self::Item> {
        self.windows.get(node_index).cloned()
    }

    fn header(&self, query: &str) -> Option<String> {
        Some(self.structured.header(query))
    }

    fn display_text(&self, node_index: usize, query: &str) -> Option<String> {
        self.structured.display_text(node_index, query)
    }

    fn completions(
        &self,
        input: &str,
        cursor: usize,
    ) -> Vec<nfm_search_core::store::SearchCompletion> {
        self.structured.completions(input, cursor)
    }

    fn effective_query(&self, input: &str) -> String {
        self.structured.effective_query(input)
    }
}
