#![cfg(windows)]

use std::path::Path;

use anyhow::Result;
use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetClassNameW, GetDesktopWindow, GetLastActivePopup, GetParent,
    GetShellWindow, GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    GA_ROOTOWNER, GWL_EXSTYLE, GWL_STYLE, HWND_DESKTOP, WS_CHILD, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_VISIBLE,
};

struct WindowInfo {
    hwnd: HWND,
    process_id: u32,
    process_name: String,
    title: String,
}

pub fn list_windows() -> Result<Vec<String>> {
    let mut windows = Vec::<WindowInfo>::new();
    unsafe {
        EnumWindows(
            Some(enum_window),
            LPARAM((&mut windows as *mut Vec<WindowInfo>) as isize),
        )?;
    }

    let process_width = windows
        .iter()
        .map(|window| window.process_name.len())
        .max()
        .unwrap_or(0);
    Ok(windows
        .into_iter()
        .map(|window| {
            format!(
                "{:08X} {:8} {:process_width$} {}",
                window.hwnd.0 as usize, window.process_id, window.process_name, window.title,
            )
        })
        .collect())
}

unsafe extern "system" fn enum_window(hwnd: HWND, state: LPARAM) -> BOOL {
    let windows = unsafe { &mut *(state.0 as *mut Vec<WindowInfo>) };
    if let Some(window) = unsafe { window_info(hwnd) } {
        windows.push(window);
    }
    true.into()
}

unsafe fn window_info(hwnd: HWND) -> Option<WindowInfo> {
    if hwnd == unsafe { GetShellWindow() }
        || hwnd == unsafe { GetDesktopWindow() }
        || hwnd == HWND_DESKTOP
        || !unsafe { IsWindowVisible(hwnd) }.as_bool()
    {
        return None;
    }

    let style = unsafe { GetWindowLongW(hwnd, GWL_STYLE) } as u32;
    let extended_style = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
    if style & WS_VISIBLE.0 == 0 || extended_style & WS_EX_TOOLWINDOW.0 != 0 || is_cloaked(hwnd) {
        return None;
    }

    let class_name = window_class(hwnd);
    let title = window_title(hwnd);
    if class_name.contains("Progman") || title.contains("ApplicationFrameWindow") {
        return None;
    }
    let application_frame = class_name.contains("ApplicationFrameWindow");
    if application_frame && title.is_empty() {
        return None;
    }
    if !application_frame
        && (style & WS_CHILD.0 != 0
            || extended_style & WS_EX_NOACTIVATE.0 != 0
            || unsafe { GetParent(hwnd) }.is_ok()
            || !is_alt_tab_window(hwnd))
    {
        return None;
    }

    let mut process_id = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    }
    Some(WindowInfo {
        hwnd,
        process_id,
        process_name: process_name(process_id),
        title,
    })
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
    }
}

fn is_alt_tab_window(hwnd: HWND) -> bool {
    let mut walk = unsafe { GetAncestor(hwnd, GA_ROOTOWNER) };
    loop {
        let candidate = unsafe { GetLastActivePopup(walk) };
        if candidate == walk {
            break;
        }
        if unsafe { IsWindowVisible(candidate) }.as_bool() {
            break;
        }
        walk = candidate;
    }
    walk == hwnd
}

fn window_title(hwnd: HWND) -> String {
    let mut buffer = [0u16; 1024];
    let length = unsafe { GetWindowTextW(hwnd, &mut buffer) }.max(0) as usize;
    String::from_utf16_lossy(&buffer[..length])
}

fn window_class(hwnd: HWND) -> String {
    let mut buffer = [0u16; 256];
    let length = unsafe { GetClassNameW(hwnd, &mut buffer) }.max(0) as usize;
    String::from_utf16_lossy(&buffer[..length])
}

fn process_name(process_id: u32) -> String {
    let Ok(process) =
        (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) })
    else {
        return String::new();
    };
    let mut buffer = [0u16; 1024];
    let mut length = buffer.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    let _ = unsafe { CloseHandle(process) };
    if result.is_err() {
        return String::new();
    }
    Path::new(&String::from_utf16_lossy(&buffer[..length as usize]))
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}
