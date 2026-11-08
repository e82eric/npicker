//! Native Windows data and actions; no picker runtime or renderer.
#![cfg(windows)]
pub mod list_processes;
pub mod list_windows;
mod picker_snapshot;
pub use picker_snapshot::{
    ProcessPickerItem, ProcessPickerSnapshot, WindowPickerItem, WindowPickerSnapshot,
};

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

    #[test]
    fn the_process_table_shows_sizes_formatted_and_numbers_aligned() {
        let process = |name: &str, pid, working_set_kb, private_bytes_kb, cpu_seconds| {
            list_processes::ProcessInfo {
                name: name.into(),
                pid,
                working_set_kb,
                private_bytes_kb,
                cpu_seconds,
            }
        };
        let snapshot = ProcessPickerSnapshot::from_items(&[
            process("small.exe", 12345, 512, 64, 0),
            process("big.exe", 4, 1_572_864, 120_000, 3_725),
            process("idle.exe", 77, 2_048, 900_000, 0),
        ]);
        let snapshot = std::sync::Arc::new(snapshot);
        let shown = |query: &str| {
            let output =
                nfm_search_core::search::search(snapshot.clone(), query, || false).unwrap();
            let mut lines = vec![snapshot.header(query).unwrap()];
            lines.extend(output.results.into_iter().map(|result| result.path));
            lines
        };
        // Most CPU first, then most private bytes.
        assert_eq!(
            shown(""),
            [
                "Name         PID  WorkingSet  PrivateBytes    CPU",
                "big.exe        4      1.5 GB      117.2 MB  1h 2m",
                "idle.exe      77      2.0 MB      878.9 MB     0s",
                "small.exe  12345    512.0 KB       64.0 KB     0s",
            ]
        );
        // Sorts in the query replace the default order.
        assert_eq!(shown("/!Ascending==PID")[1..].len(), 3);
        assert!(shown("/!Ascending==PID")[1].starts_with("big.exe"));
        assert!(shown("/!Descending==PID")[1].starts_with("small.exe"));
        // The selected item keeps the raw KB.
        assert_eq!(snapshot.item(1).unwrap().working_set_kb, 1_572_864);
    }
}
