//! Native Windows data and actions; no picker runtime or renderer.
#![cfg(windows)]
pub mod list_processes;
pub mod list_windows;
mod picker_snapshot;
pub use picker_snapshot::{ProcessPickerItem, ProcessPickerSnapshot, WindowPickerItem};

/// Format process details for any presentation backend.
pub fn format_process_preview(item: &ProcessPickerItem) -> String {
    format!(
        "Name: {}\nPID: {}\nWorkingSet: {}\nPrivateBytes: {}\nCPU: {}",
        item.name, item.pid, item.working_set_kb, item.private_bytes_kb, item.cpu_seconds
    )
}

pub fn logical_drive_roots() -> Vec<String> {
    use windows::Win32::Storage::FileSystem::GetLogicalDrives;
    let mask = unsafe { GetLogicalDrives() };
    (0..26)
        .filter(|index| mask & (1 << index) != 0)
        .map(|index| format!("{}:\\", (b'A' + index as u8) as char))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use nfm_search_core::store::ItemsSource;

    #[test]
    fn same_count_refresh_keeps_payload_and_preview_in_sync() {
        let mut process = list_processes::ProcessInfo {
            name: "fixture.exe".into(),
            pid: 1234,
            working_set_kb: 64,
            private_bytes_kb: 32,
            cpu_seconds: 1,
        };
        let first = ProcessPickerSnapshot::from_items(&[process.clone()]);
        process.cpu_seconds = 2;
        let refreshed = ProcessPickerSnapshot::from_items(&[process]);
        assert_ne!(first.version(), refreshed.version());
        let first_item = first.item(0).unwrap();
        let refreshed_item = refreshed.item(0).unwrap();
        assert_eq!(first_item.cpu_seconds, 1);
        assert_eq!(refreshed_item.cpu_seconds, 2);
        assert_eq!(
            format_process_preview(&refreshed_item),
            "Name: fixture.exe\nPID: 1234\nWorkingSet: 64\nPrivateBytes: 32\nCPU: 2"
        );
        assert!(refreshed.header("").unwrap().contains("PID"));
        assert!(refreshed.item(1).is_none());
    }
}
