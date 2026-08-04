use anyhow::{anyhow, Context, Result};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

const CF_UNICODETEXT: u32 = 13;

struct ClipboardGuard;

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

struct GlobalMemory(HGLOBAL);

impl Drop for GlobalMemory {
    fn drop(&mut self) {
        unsafe {
            let _ = GlobalFree(Some(self.0));
        }
    }
}

pub fn copy_text(text: &str) -> Result<()> {
    let utf16: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let byte_count = utf16.len() * size_of::<u16>();

    unsafe {
        OpenClipboard(None).context("failed to open the clipboard")?;
        let _clipboard = ClipboardGuard;
        EmptyClipboard().context("failed to empty the clipboard")?;

        let memory = GlobalMemory(
            GlobalAlloc(GMEM_MOVEABLE, byte_count).context("failed to allocate clipboard data")?,
        );
        let destination = GlobalLock(memory.0).cast::<u16>();
        if destination.is_null() {
            return Err(anyhow!("failed to lock clipboard data"));
        }
        std::ptr::copy_nonoverlapping(utf16.as_ptr(), destination, utf16.len());
        let _ = GlobalUnlock(memory.0);

        SetClipboardData(CF_UNICODETEXT, Some(HANDLE(memory.0 .0)))
            .context("failed to set clipboard text")?;
        std::mem::forget(memory);
    }
    Ok(())
}
