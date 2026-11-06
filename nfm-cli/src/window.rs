//! Native window placement on the monitor under the mouse, and activating the
//! window chosen with `nfm windows --activate`.
#![cfg(windows)]
use crate::options::WindowPosition;
use windows_sys::Win32::{
    Foundation::{POINT, RECT},
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint},
    UI::WindowsAndMessaging::{
        GetCursorPos, GetWindowRect, IsIconic, SW_RESTORE, SWP_NOACTIVATE, SWP_NOSIZE,
        SWP_NOZORDER, SetForegroundWindow, SetWindowPos, ShowWindow,
    },
};

/// Move the (not yet shown) window. Coordinates are physical pixels, so this
/// avoids converting between monitor scale factors.
pub fn place(hwnd: isize, position: WindowPosition) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        let mut cursor = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut cursor) == 0 {
            return;
        }
        let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        let mut window: RECT = std::mem::zeroed();
        if GetMonitorInfoW(monitor, &mut info) == 0 || GetWindowRect(hwnd as _, &mut window) == 0 {
            return;
        }
        let work = info.rcWork;
        let (width, height) = (window.right - window.left, window.bottom - window.top);
        let (x, y) = origin(
            position,
            (work.left, work.top, work.right, work.bottom),
            (width, height),
            (cursor.x, cursor.y),
        );
        SetWindowPos(
            hwnd as _,
            std::ptr::null_mut(),
            x,
            y,
            0,
            0,
            SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

fn origin(
    position: WindowPosition,
    (left, top, right, bottom): (i32, i32, i32, i32),
    (width, height): (i32, i32),
    (cursor_x, cursor_y): (i32, i32),
) -> (i32, i32) {
    let centered_x = left + (right - left - width) / 2;
    let (x, y) = match position {
        WindowPosition::Center => (centered_x, top + (bottom - top - height) / 2),
        WindowPosition::Top => (centered_x, top + (bottom - top) / 8),
        WindowPosition::Mouse => (cursor_x - width / 2, cursor_y - height / 3),
    };
    (
        x.clamp(left, (right - width).max(left)),
        y.clamp(top, (bottom - height).max(top)),
    )
}

/// Make the window a tool window before it is first shown: no taskbar button,
/// and tiling window managers that tile `WS_EX_APPWINDOW` windows float it.
pub fn make_popup(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, SetWindowLongPtrW, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW,
    };
    if hwnd == 0 {
        return;
    }
    unsafe {
        let style = GetWindowLongPtrW(hwnd as _, GWL_EXSTYLE);
        let popup = (style & !(WS_EX_APPWINDOW as isize)) | WS_EX_TOOLWINDOW as isize;
        SetWindowLongPtrW(hwnd as _, GWL_EXSTYLE, popup);
    }
}

pub fn activate(hwnd: isize) {
    unsafe {
        if IsIconic(hwnd as _) != 0 {
            ShowWindow(hwnd as _, SW_RESTORE);
        }
        SetForegroundWindow(hwnd as _);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_stay_on_the_work_area() {
        let work = (0, 0, 1920, 1040);
        assert_eq!(origin(WindowPosition::Center, work, (1000, 500), (0, 0)), (460, 270));
        assert_eq!(origin(WindowPosition::Top, work, (1000, 500), (0, 0)), (460, 130));
        assert_eq!(origin(WindowPosition::Mouse, work, (1000, 500), (1900, 1000)), (920, 540));
        assert_eq!(origin(WindowPosition::Mouse, work, (1000, 500), (10, 10)), (0, 0));
        // A second monitor to the left.
        let left = (-1920, 0, 0, 1080);
        assert_eq!(origin(WindowPosition::Center, left, (1000, 500), (-5, 5)), (-1460, 290));
    }
}
