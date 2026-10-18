use std::borrow::Cow;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use super::DirectoryEntry;

pub(super) const PATH_SEPARATOR: u8 = b'/';
pub(super) const SCAN_MODE: &str = "rust_unix";

// Kernel virtual filesystems: large, always changing, and not files a picker
// is useful for. They are still listed, and scanned when chosen as a root.
const SKIPPED_DIRECTORIES: [&[u8]; 2] = [b"/proc", b"/sys"];

/// Unix names are bytes; scan paths keep them exact so directories whose
/// names are not UTF-8 can still be opened. Only displayed names are lossy.
pub(super) fn path_bytes(path: &Path) -> Vec<u8> {
    path.as_os_str().as_bytes().to_vec()
}

pub(super) fn should_descend(path: &[u8]) -> bool {
    !SKIPPED_DIRECTORIES.contains(&path)
}

/// Report each entry of `path`. `read_dir` batches entries with getdents64
/// and takes the type from d_type, so files are never stat'ed. Symbolic links
/// are reported as non-directories and not followed, which avoids cycles.
pub(super) fn read_directory(
    path: &[u8],
    cancelled: &AtomicBool,
    mut visit: impl FnMut(DirectoryEntry<'_>),
) {
    let Ok(entries) = std::fs::read_dir(OsStr::from_bytes(path)) else {
        return;
    };

    for entry in entries {
        if cancelled.load(Ordering::Acquire) {
            break;
        }

        let Ok(entry) = entry else {
            break;
        };
        let name = entry.file_name();
        let display = name.to_string_lossy();
        // Only names that are not UTF-8 need their exact bytes kept.
        let path_name = matches!(display, Cow::Owned(_)).then(|| name.as_bytes());
        visit(DirectoryEntry {
            name: display.into_owned(),
            path_name,
            is_dir: entry.file_type().is_ok_and(|kind| kind.is_dir()),
        });
    }
}
