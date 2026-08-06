#![cfg(windows)]

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use windows::core::{BOOL, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetAncestor, GetClassNameW, GetDesktopWindow, GetLastActivePopup, GetParent,
    GetShellWindow, GetWindowLongW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    GA_ROOTOWNER, GWL_EXSTYLE, GWL_STYLE, HWND_DESKTOP, WS_CHILD, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_VISIBLE,
};

pub struct WindowListItem {
    pub text: String,
    pub hwnd: isize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WindowPayload {
    pub hwnd: isize,
}

struct WindowInfo {
    hwnd: HWND,
    process_id: u32,
    process_name: String,
    class_name: String,
    title: String,
}

pub fn list_windows() -> Result<Vec<WindowListItem>> {
    let mut windows = Vec::<WindowInfo>::new();
    unsafe {
        EnumWindows(
            Some(enum_window),
            LPARAM((&mut windows as *mut Vec<WindowInfo>) as isize),
        )?;
    }
    remove_app_frame_duplicates(&mut windows);

    let process_width = windows
        .iter()
        .map(|window| window.process_name.len())
        .max()
        .unwrap_or(0);
    Ok(windows
        .into_iter()
        .map(|window| WindowListItem {
            text: format!(
                "{:08X} {:8} {:process_width$} {}",
                window.hwnd.0 as usize, window.process_id, window.process_name, window.title,
            ),
            hwnd: window.hwnd.0 as isize,
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
    // A cloaked top-level window can represent an inactive workspace in an
    // external window manager. Cloaking does not make it ineligible for the
    // window picker; the remaining Alt-Tab-style checks still reject shell,
    // tool, child, and no-activate windows.
    if style & WS_VISIBLE.0 == 0 || extended_style & WS_EX_TOOLWINDOW.0 != 0 {
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
        class_name,
        title,
    })
}

fn remove_app_frame_duplicates(windows: &mut Vec<WindowInfo>) {
    let hosted_titles: HashSet<_> = windows
        .iter()
        .filter(|window| {
            window.class_name == "ApplicationFrameWindow"
                && window
                    .process_name
                    .eq_ignore_ascii_case("ApplicationFrameHost.exe")
        })
        .map(|window| window.title.clone())
        .collect();
    if hosted_titles.is_empty() {
        return;
    }
    windows.retain(|window| {
        !hosted_titles.contains(&window.title)
            || window
                .process_name
                .eq_ignore_ascii_case("ApplicationFrameHost.exe")
    });
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

#[cfg(test)]
mod tests {
    use super::*;

    fn window(hwnd: isize, process_name: &str, class_name: &str, title: &str) -> WindowInfo {
        WindowInfo {
            hwnd: HWND(hwnd as *mut _),
            process_id: hwnd as u32,
            process_name: process_name.into(),
            class_name: class_name.into(),
            title: title.into(),
        }
    }

    #[test]
    fn application_frame_host_replaces_duplicate_app_window() {
        let mut windows = vec![
            window(
                1,
                "ApplicationFrameHost.exe",
                "ApplicationFrameWindow",
                "Media Player",
            ),
            window(
                2,
                "Microsoft.Media.Player.exe",
                "SomeAppWindow",
                "Media Player",
            ),
        ];

        remove_app_frame_duplicates(&mut windows);

        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].hwnd, HWND(1 as *mut _));
    }

    #[test]
    fn equal_titles_without_an_application_frame_are_preserved() {
        let mut windows = vec![
            window(1, "explorer.exe", "CabinetWClass", "Downloads"),
            window(2, "explorer.exe", "CabinetWClass", "Downloads"),
        ];

        remove_app_frame_duplicates(&mut windows);

        assert_eq!(windows.len(), 2);
    }
}
