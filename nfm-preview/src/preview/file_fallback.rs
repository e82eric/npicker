//! Bounded fallback previews without external tools.
use std::{fs, io::Read, path::Path};
const PREVIEW_BYTES: usize = 256 * 1024;
const DIRECTORY_ENTRIES: usize = 200;
/// Read a bounded preview with cancellation checks between reads.
pub fn load_preview(path: &Path, cancelled: impl Fn() -> bool) -> Result<String, String> {
    if cancelled() {
        return Ok(String::new());
    }
    let metadata = fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let mut text = format!("{}\n", path.display());
    if metadata.is_dir() {
        let mut entries = Vec::new();
        let mut truncated = false;
        for entry in fs::read_dir(path).map_err(|error| error.to_string())? {
            if cancelled() {
                return Ok(String::new());
            }
            if entries.len() == DIRECTORY_ENTRIES {
                truncated = true;
                break;
            }
            let entry = entry.map_err(|error| error.to_string())?;
            let directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
            entries.push((
                !directory,
                format!(
                    "{}{}",
                    entry.file_name().to_string_lossy(),
                    if directory { "\\" } else { "" }
                ),
            ));
        }
        entries.sort();
        text.push_str("Directory\n\n");
        for (_, name) in entries {
            text.push_str(&name);
            text.push('\n');
        }
        if truncated {
            text.push_str("… directory preview limited to 200 entries\n");
        }
        return Ok(text);
    }
    if !metadata.is_file() {
        text.push_str("Not a regular file\n");
        return Ok(text);
    }
    text.push_str(&format!("{} bytes\n\n", metadata.len()));
    let mut file = fs::File::open(path).map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    while bytes.len() < PREVIEW_BYTES {
        if cancelled() {
            return Ok(String::new());
        }
        let capacity = (PREVIEW_BYTES - bytes.len()).min(chunk.len());
        let read = file
            .read(&mut chunk[..capacity])
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
    }
    if cancelled() {
        return Ok(String::new());
    }
    let decoded = if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        let little = bytes[0] == 0xff;
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|pair| {
                if little {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                }
            })
            .collect();
        Some(String::from_utf16_lossy(&units))
    } else if bytes.contains(&0) {
        None
    } else {
        let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&bytes);
        Some(String::from_utf8_lossy(bytes).into_owned())
    };
    match decoded {
        Some(contents) => text.push_str(&contents.replace('\0', "�")),
        None => text.push_str("Binary file — text preview unavailable\n"),
    }
    if metadata.len() > PREVIEW_BYTES as u64 {
        text.push_str("\n… preview limited to 256 KiB\n");
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    fn fixture(name: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("swm-preview-{}-{name}", std::process::id()));
        fs::write(&path, bytes).unwrap();
        path
    }
    #[test]
    fn previews_unicode_text_and_utf16_and_binary_without_unbounded_reads() {
        let utf8 = fixture("utf8", "hello 猫\nnext".as_bytes());
        assert!(load_preview(&utf8, || false).unwrap().contains("hello 猫"));
        let utf16 = fixture("utf16", &[0xff, 0xfe, b'h', 0, b'i', 0]);
        assert!(load_preview(&utf16, || false).unwrap().ends_with("hi"));
        let binary = fixture("binary", &[0, 1, 2, 3]);
        assert!(load_preview(&binary, || false)
            .unwrap()
            .contains("Binary file"));
        let large = fixture("large", &vec![b'a'; PREVIEW_BYTES * 2]);
        let preview = load_preview(&large, || false).unwrap();
        assert!(preview.contains("limited to 256 KiB"));
        assert!(preview.len() < PREVIEW_BYTES + 1024);
        for path in [utf8, utf16, binary, large] {
            fs::remove_file(path).unwrap();
        }
    }
    #[test]
    fn cancellation_prevents_even_opening_a_missing_file() {
        assert_eq!(
            load_preview(Path::new("does-not-exist"), || true).unwrap(),
            ""
        );
    }
}
