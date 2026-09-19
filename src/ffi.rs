use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::OnceLock;

use crate::embedded_interactions;
use crate::file_picker;
use crate::key_binding::{KeyChord, KeyModifiers, KeyName};
use crate::process_picker;
use crate::request::{FileSystemPickerRequest, FlatItemsPickerRequest, PickerResponse};
use crate::skia_ui as picker_ui;
use crate::view_model::ViewModel;
use crate::window_picker;
use std::sync::Arc;
use std::thread;

type NativeItemsAction = unsafe extern "C" fn(*mut c_void) -> *mut *mut c_char;
type OnSelect = unsafe extern "C" fn(*mut c_char, *mut c_void);
type OnWindowSelect = unsafe extern "C" fn(isize, *mut c_void);
type OnProcessSelect = unsafe extern "C" fn(u32, *mut c_void);
type OnClosed = unsafe extern "C" fn();

static VIEW_MODEL: OnceLock<Arc<ViewModel>> = OnceLock::new();

#[repr(C)]
pub struct RustNfmFileSystemOptions {
    pub struct_size: usize,
    pub max_depth: i32,
    pub preview_visible: c_int,
}

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
        nfm_search_core::timing::set_sink_from_env(output_debug_string);
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
        view_model.set_preview_visible(false);
        let root_directories = unsafe { copy_directories(directories, directory_count) };
        let state = state as usize;

        thread::spawn(move || {
            let request = FileSystemPickerRequest {
                root_directories,
                max_depth: 5,
                directories_only: false,
                files_only: true,
                search_string: None,
            };

            let interactions = file_picker::interactions();
            match view_model.run_request_with_interactions(&request, interactions) {
                Ok(PickerResponse::Selected(item)) => {
                    if let Some(on_select) = on_select {
                        if let Ok(selected) = CString::new(item) {
                            unsafe {
                                on_select(selected.as_ptr() as *mut c_char, state as *mut c_void);
                            }
                        }
                    }
                }
                _ => {
                    if let Some(on_closed) = on_closed {
                        unsafe { on_closed() };
                    }
                }
            }
        });
    }));
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn RustNfmShowFileSystem(
    options: *const RustNfmFileSystemOptions,
    on_select: Option<OnSelect>,
    on_closed: Option<OnClosed>,
    state: *mut c_void,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        let Some(options) = (unsafe { options.as_ref() }) else {
            return 0;
        };
        if options.struct_size < std::mem::size_of::<RustNfmFileSystemOptions>() {
            return 0;
        }
        let roots = file_picker::logical_drive_roots();
        if roots.is_empty() {
            return 0;
        }
        let view_model = ensure_initialized();
        view_model.set_preview_visible(options.preview_visible != 0);
        let max_depth = options.max_depth;
        let state = state as usize;
        thread::spawn(move || {
            let request = FileSystemPickerRequest {
                root_directories: roots,
                max_depth,
                directories_only: false,
                files_only: false,
                search_string: None,
            };
            let interactions = file_picker::interactions();
            match view_model.run_request_with_interactions(&request, interactions) {
                Ok(PickerResponse::Selected(item)) => {
                    if let Some(on_select) = on_select {
                        if let Ok(selected) = CString::new(item) {
                            unsafe {
                                on_select(selected.as_ptr() as *mut c_char, state as *mut c_void)
                            };
                        }
                    }
                }
                _ => {
                    if let Some(on_closed) = on_closed {
                        unsafe { on_closed() };
                    }
                }
            }
        });
        1
    }))
    .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn RustNfmShowWindows(
    on_select: Option<OnWindowSelect>,
    on_closed: Option<OnClosed>,
    state: *mut c_void,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        let request = match window_picker::request() {
            Ok(request) => request,
            Err(_) => return 0,
        };
        let view_model = ensure_initialized();
        view_model.set_preview_visible(true);
        let state = state as usize;
        thread::spawn(move || {
            match view_model.run_request_with_interactions(&request, window_picker::interactions())
            {
                Ok(PickerResponse::Selected(item)) => match on_select {
                    Some(on_select) => unsafe {
                        on_select(item.native_window, state as *mut c_void)
                    },
                    _ => {
                        if let Some(on_closed) = on_closed {
                            unsafe { on_closed() };
                        }
                    }
                },
                _ => {
                    if let Some(on_closed) = on_closed {
                        unsafe { on_closed() };
                    }
                }
            }
        });
        1
    }))
    .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn RustNfmShowProcesses(
    on_select: Option<OnProcessSelect>,
    on_closed: Option<OnClosed>,
    state: *mut c_void,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        let request = match process_picker::request() {
            Ok(request) => request,
            Err(_) => return 0,
        };
        let view_model = ensure_initialized();
        view_model.set_preview_visible(false);
        let state = state as usize;
        thread::spawn(move || {
            match view_model.run_request_with_interactions(&request, process_picker::interactions())
            {
                Ok(PickerResponse::Selected(item)) => match on_select {
                    Some(on_select) => unsafe { on_select(item.pid, state as *mut c_void) },
                    _ => {
                        if let Some(on_closed) = on_closed {
                            unsafe { on_closed() };
                        }
                    }
                },
                _ => {
                    if let Some(on_closed) = on_closed {
                        unsafe { on_closed() };
                    }
                }
            }
        });
        1
    }))
    .unwrap_or(0)
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
        view_model.set_preview_visible(false);
        let items = collect_items(native_items_action, state);
        let state = state as usize;

        std::thread::spawn(move || {
            let request = FlatItemsPickerRequest {
                items,
                search_string: None,
            };

            let interactions = embedded_interactions::command(|item: &String| item.clone());
            match view_model.run_request_with_interactions(&request, interactions) {
                Ok(PickerResponse::Selected(item)) => {
                    if let Some(on_select) = on_select {
                        if let Ok(selected) = CString::new(item) {
                            unsafe {
                                on_select(selected.as_ptr() as *mut c_char, state as *mut c_void);
                            }
                        }
                    }
                }
                _ => {
                    if let Some(on_closed) = on_closed {
                        unsafe { on_closed() };
                    }
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
pub extern "C" fn RustNfmFocus() {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if let Some(view_model) = VIEW_MODEL.get() {
            view_model.focus();
        }
    }));
}

#[unsafe(no_mangle)]
pub extern "C" fn RustNfmSetMenuLocation(x: i32, y: i32) {
    picker_ui::set_preferred_center(x, y);
}

fn ensure_initialized() -> Arc<ViewModel> {
    Arc::clone(VIEW_MODEL.get_or_init(|| {
        let mut bindings = HashMap::new();
        bindings.insert(
            KeyChord {
                key: KeyName::Enter,
                modifiers: KeyModifiers::default(),
            },
            "ffi-accept".into(),
        );
        bindings.insert(
            KeyChord {
                key: KeyName::Character('u'),
                modifiers: KeyModifiers {
                    ctrl: true,
                    ..KeyModifiers::default()
                },
            },
            "ffi-parent".into(),
        );
        let view = Arc::new(picker_ui::ViewHandle::new());
        let view_model = ViewModel::new_with_bindings(bindings, false, view.clone());
        let ui_view_model = Arc::clone(&view_model);
        std::thread::spawn(move || {
            if let Err(error) = picker_ui::run(ui_view_model, view, true, false) {
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
