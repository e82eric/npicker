use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use crate::action::{
    ActionConfig, ActionDefinition, ActionResolution, ActionService, ActionState, PickerState,
};
use crate::key_binding::{KeyChord, KeyModifiers, KeyName};
use crate::preview::{
    CommandPreviewTarget, PreviewCancellation, PreviewConfig, PreviewJob, PreviewOutputType,
    PreviewProfile, PreviewResolver, PreviewService,
};
use crate::request::{FileSystemPickerRequest, FlatItemsPickerRequest};
use crate::skia_ui as picker_ui;
use crate::view_model::ViewModel;
use std::sync::Arc;
use std::thread;

type NativeItemsAction = unsafe extern "C" fn(*mut c_void) -> *mut *mut c_char;
type OnSelect = unsafe extern "C" fn(*mut c_char, *mut c_void);
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

            match view_model.run_request(&request) {
                Ok(response) if response.status == "selected" => {
                    if let (Some(on_select), Some(selected)) = (on_select, response.selected_item) {
                        if let Ok(selected) = CString::new(selected) {
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
        let roots = logical_drive_roots();
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
            match view_model.run_request(&request) {
                Ok(response) if response.status == "selected" => {
                    if let (Some(on_select), Some(selected)) = (on_select, response.selected_item) {
                        if let Ok(selected) = CString::new(selected) {
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

fn logical_drive_roots() -> Vec<String> {
    use windows::Win32::Storage::FileSystem::GetLogicalDrives;
    let mask = unsafe { GetLogicalDrives() };
    (0..26)
        .filter(|index| mask & (1 << index) != 0)
        .map(|index| format!("{}:\\", (b'A' + index as u8) as char))
        .collect()
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

            match view_model.run_request(&request) {
                Ok(response) if response.status == "selected" => {
                    if let (Some(on_select), Some(selected)) = (on_select, response.selected_item) {
                        if let Ok(selected) = CString::new(selected) {
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
pub extern "C" fn RustNfmSetMenuLocation(x: i32, y: i32) {
    picker_ui::set_preferred_center(x, y);
}

fn ensure_initialized() -> Arc<ViewModel> {
    Arc::clone(VIEW_MODEL.get_or_init(|| {
        let mut resolvers = HashMap::new();
        resolvers.insert(
            "ffi-accept".into(),
            ActionDefinition::Native(resolve_file_system_accept),
        );
        resolvers.insert(
            "ffi-parent".into(),
            ActionDefinition::Native(resolve_file_system_parent),
        );
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
        let view_model = ViewModel::new_with_services_and_bindings(
            PreviewService::new(PreviewConfig::Command(PreviewResolver::Function(
                resolve_native_file_preview,
            ))),
            ActionService::new(ActionConfig { resolvers }),
            bindings,
            false,
        );
        let ui_view_model = Arc::clone(&view_model);
        std::thread::spawn(move || {
            if let Err(error) = picker_ui::run(ui_view_model, None, true, false) {
                eprintln!("RustNfm UI stopped: {error:?}");
            }
        });
        view_model
    }))
}

fn resolve_native_file_preview(
    target: &CommandPreviewTarget,
) -> Result<Option<PreviewJob>, String> {
    let path = std::path::Path::new(&target.item);
    if path.is_dir() {
        return Ok(Some(PreviewJob::Process(PreviewProfile {
            program: "cmd.exe".into(),
            arguments: vec!["/d".into(), "/c".into(), "dir".into(), "{item}".into()],
            working_directory: None,
            output_type: PreviewOutputType::Text,
        })));
    }
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        extension.as_str(),
        "mp4" | "mkv" | "mov" | "avi" | "webm" | "m4v" | "wmv"
    ) {
        return Ok(Some(PreviewJob::Function(resolve_video_frame_preview)));
    }
    if matches!(
        extension.as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "tif" | "tiff"
    ) {
        return Ok(Some(PreviewJob::Process(PreviewProfile {
            program: "ffmpeg".into(),
            arguments: vec![
                "-loglevel".into(),
                "error".into(),
                "-i".into(),
                "{item}".into(),
                "-frames:v".into(),
                "1".into(),
                "-f".into(),
                "image2pipe".into(),
                "-vcodec".into(),
                "png".into(),
                "pipe:1".into(),
            ],
            working_directory: None,
            output_type: PreviewOutputType::Image,
        })));
    }
    if matches!(
        extension.as_str(),
        "txt"
            | "md"
            | "json"
            | "xml"
            | "yaml"
            | "yml"
            | "toml"
            | "ini"
            | "rs"
            | "c"
            | "h"
            | "cpp"
            | "hpp"
            | "cs"
            | "js"
            | "ts"
            | "css"
            | "html"
            | "ps1"
            | "cmd"
            | "bat"
            | "sh"
            | "py"
            | "rb"
            | "go"
            | "java"
            | "log"
    ) {
        return Ok(Some(PreviewJob::Process(PreviewProfile {
            program: "bat".into(),
            arguments: vec![
                "--color=always".into(),
                "--style=plain".into(),
                "--paging=never".into(),
                "{item}".into(),
            ],
            working_directory: None,
            output_type: PreviewOutputType::Text,
        })));
    }
    Ok(Some(PreviewJob::Process(PreviewProfile {
        program: "cmd.exe".into(),
        arguments: vec!["/d".into(), "/c".into(), "dir".into(), "{item}".into()],
        working_directory: None,
        output_type: PreviewOutputType::Text,
    })))
}

fn resolve_video_frame_preview(
    target: &CommandPreviewTarget,
    cancellation: &PreviewCancellation,
) -> Result<Option<PreviewProfile>, String> {
    let mut probe = Command::new("ffprobe");
    probe.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
    ]);
    probe.arg(&target.item).stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        probe.creation_flags(CREATE_NO_WINDOW);
    }
    let output = probe
        .output()
        .map_err(|error| format!("ffprobe failed to start: {error}"))?;
    if cancellation.is_cancelled() {
        return Ok(None);
    }
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if message.is_empty() {
            format!("ffprobe exited with {}", output.status)
        } else {
            message
        });
    }
    let duration = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<f64>()
        .map_err(|_| "ffprobe returned an invalid duration".to_string())?;
    if !duration.is_finite() || duration < 0.0 {
        return Err("ffprobe returned an invalid duration".into());
    }
    Ok(Some(PreviewProfile {
        program: "ffmpeg".into(),
        arguments: vec![
            "-loglevel".into(),
            "error".into(),
            "-ss".into(),
            format!("{:.3}", duration * 0.25),
            "-i".into(),
            "{item}".into(),
            "-frames:v".into(),
            "1".into(),
            "-f".into(),
            "image2pipe".into(),
            "-vcodec".into(),
            "png".into(),
            "pipe:1".into(),
        ],
        working_directory: None,
        output_type: PreviewOutputType::Image,
    }))
}

fn resolve_file_system_accept(state: &ActionState) -> Result<ActionResolution, String> {
    let Some(selection) = &state.selection else {
        return Ok(ActionResolution::None);
    };
    if std::path::Path::new(&selection.item).is_dir() {
        Ok(ActionResolution::FileWalker {
            roots: vec![selection.item.clone()],
        })
    } else {
        Ok(ActionResolution::Complete)
    }
}

fn resolve_file_system_parent(state: &ActionState) -> Result<ActionResolution, String> {
    let PickerState::Filewalker { roots } = &state.picker else {
        return Ok(ActionResolution::None);
    };
    let roots = file_system_parent_roots(roots, logical_drive_roots());
    if roots.is_empty() {
        return Ok(ActionResolution::None);
    }
    Ok(ActionResolution::FileWalker { roots })
}

fn file_system_parent_roots(roots: &[String], drive_roots: Vec<String>) -> Vec<String> {
    if roots.len() == 1 {
        if let Some(parent) = std::path::Path::new(&roots[0])
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            return vec![parent.to_string_lossy().into_owned()];
        }
    }
    drive_roots
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

    #[test]
    fn parent_action_uses_the_single_roots_parent() {
        assert_eq!(
            file_system_parent_roots(
                &[r"C:\work\child".into()],
                vec![r"C:\".into(), r"D:\".into()],
            ),
            vec![r"C:\work".to_owned()]
        );
    }

    #[test]
    fn parent_action_shows_drives_at_a_file_system_root() {
        let drives = vec![r"C:\".to_owned(), r"D:\".to_owned()];
        assert_eq!(
            file_system_parent_roots(&[r"C:\".into()], drives.clone()),
            drives
        );
    }

    #[test]
    fn parent_action_keeps_the_drive_list_idempotent() {
        let drives = vec![r"C:\".to_owned(), r"D:\".to_owned()];
        assert_eq!(file_system_parent_roots(&drives, drives.clone()), drives);
    }
}
