use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::OnceLock;

use crate::request::{FileSystemPickerRequest, FlatItemsPickerRequest};
use crate::skia_ui as picker_ui;
use crate::view_model::ViewModel;
use std::sync::Arc;
use std::thread;

type NativeItemsAction = unsafe extern "C" fn(*mut c_void) -> *mut *mut c_char;
type OnSelect = unsafe extern "C" fn(*mut c_char, *mut c_void);
type OnClosed = unsafe extern "C" fn();

static VIEW_MODEL: OnceLock<Arc<ViewModel>> = OnceLock::new();

fn output_debug_string(line: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;

    let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        OutputDebugStringW(PCWSTR(wide.as_ptr()));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn RustNfmInitialize() {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        nfm_search_core::timing::set_sink(output_debug_string);
        ensure_initialized();
    }));
}

#[unsafe(no_mangle)]
pub extern "C" fn RustNfmShowProgramsList(
    directories: *const *const c_char,
    directory_count: c_int,
    on_select: Option<OnSelect>,
    on_closed: Option<OnClosed>,
    state: *mut c_void,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let view_model = ensure_initialized();
        let root_directories = unsafe { copy_directories(directories, directory_count) };
        let state = state as usize;

        thread::spawn(move || {
            let request = FileSystemPickerRequest {
                root_directories,
                max_depth: 5,
                directories_only: false,
                files_only: false,
                search_string: None,
            };

            if let Ok(response) = view_model.run_request(&request) {
                if response.status == "selected" {
                    if let (Some(on_select), Some(selected)) = (on_select, response.selected_item) {
                        if let Ok(selected) = CString::new(selected) {
                            unsafe {
                                on_select(selected.as_ptr() as *mut c_char, state as *mut c_void);
                            }
                        }
                    }
                }
            }

            if let Some(on_closed) = on_closed {
                unsafe {
                    on_closed();
                }
            }
        });
    }));
}

unsafe fn copy_directories(
    directories: *const *const c_char,
    directory_count: c_int,
) -> Vec<String> {
    if directories.is_null() || directory_count <= 0 {
        return Vec::new();
    }

    let mut result = Vec::with_capacity(directory_count as usize);

    for i in 0..directory_count as usize {
        let ptr = unsafe { *directories.add(i) };

        if ptr.is_null() {
            continue;
        }

        let value = unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();

        result.push(value);
    }

    result
}

#[unsafe(no_mangle)]
pub extern "C" fn RustNfmShowItemsList(
    _header: *mut c_char,
    native_items_action: Option<NativeItemsAction>,
    on_select: Option<OnSelect>,
    on_closed: Option<OnClosed>,
    state: *mut c_void,
) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let view_model = ensure_initialized();
        let items = collect_items(native_items_action, state);
        let state = state as usize;

        std::thread::spawn(move || {
            let request = FlatItemsPickerRequest {
                items,
                search_string: None,
            };

            if let Ok(response) = view_model.run_request(&request) {
                if response.status == "selected" {
                    if let (Some(on_select), Some(selected)) = (on_select, response.selected_item) {
                        if let Ok(selected) = CString::new(selected) {
                            unsafe {
                                on_select(selected.as_ptr() as *mut c_char, state as *mut c_void);
                            }
                        }
                    }
                }
            }

            if let Some(on_closed) = on_closed {
                unsafe {
                    on_closed();
                }
            }
        });
    }));
}

#[unsafe(no_mangle)]
pub extern "C" fn RustNfmHide() {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(view_model) = VIEW_MODEL.get() {
            view_model.cancel();
        }
    }));
}

#[unsafe(no_mangle)]
pub extern "C" fn RustNfmSetMenuLocation(x: i32, y: i32) {
    picker_ui::set_preferred_center(x, y);
}

fn ensure_initialized() -> Arc<ViewModel> {
    Arc::clone(VIEW_MODEL.get_or_init(|| {
        let view_model = ViewModel::new(crate::preview::PreviewService::default());
        let ui_view_model = Arc::clone(&view_model);
        std::thread::spawn(move || {
            if let Err(error) =
                picker_ui::run(ui_view_model, None, false, false, Default::default())
            {
                eprintln!("RustNfm UI stopped: {error:?}");
            }
        });
        view_model
    }))
}

fn collect_items(
    native_items_action: Option<NativeItemsAction>,
    state: *mut c_void,
) -> Vec<String> {
    let Some(native_items_action) = native_items_action else {
        return Vec::new();
    };

    let items = unsafe { native_items_action(state) };
    copy_null_terminated_items(items as *const *const c_char)
}

fn copy_null_terminated_items(items: *const *const c_char) -> Vec<String> {
    if items.is_null() {
        return Vec::new();
    }

    let mut copied = Vec::new();
    let mut cursor = items;
    loop {
        let item = unsafe { *cursor };
        if item.is_null() {
            break;
        }

        let item = unsafe { CStr::from_ptr(item) };
        copied.push(item.to_string_lossy().into_owned());
        cursor = unsafe { cursor.add(1) };
    }

    copied
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    #[test]
    fn null_items_copy_to_empty_vec() {
        assert!(copy_null_terminated_items(ptr::null()).is_empty());
    }

    #[test]
    fn copies_null_terminated_items() {
        let alpha = CString::new("alpha").unwrap();
        let beta = CString::new("beta\\gamma").unwrap();
        let items = [alpha.as_ptr(), beta.as_ptr(), ptr::null()];

        assert_eq!(
            copy_null_terminated_items(items.as_ptr()),
            vec!["alpha".to_owned(), "beta\\gamma".to_owned()]
        );
    }
}
