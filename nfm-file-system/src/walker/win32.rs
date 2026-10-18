use std::ffi::c_void;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
    FILE_ATTRIBUTE_DIRECTORY, FINDEX_SEARCH_OPS, FIND_FIRST_EX_LARGE_FETCH, WIN32_FIND_DATAW,
};

use super::DirectoryEntry;

pub(super) const PATH_SEPARATOR: u8 = b'\\';
pub(super) const SCAN_MODE: &str = "rust_win32";

/// Scan paths are UTF-8 text; names are converted from UTF-16 lossily.
pub(super) fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

pub(super) fn should_descend(_path: &[u8]) -> bool {
    true
}

/// Report each entry of `path`, using basic info and large fetches.
pub(super) fn read_directory(
    path: &[u8],
    cancelled: &AtomicBool,
    mut visit: impl FnMut(DirectoryEntry<'_>),
) {
    let search_path = make_search_path(path);

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
                    visit(DirectoryEntry {
                        name,
                        path_name: None,
                        is_dir,
                    });
                }
            }

            let next = FindNextFileW(find_handle.0, &mut find_data);
            if next.is_err() {
                break;
            }
        }
    }
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
