use std::ffi::c_void;
use std::io::{BufRead, BufReader, Write};
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_MODE,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::core::PCWSTR;
use crate::store::{ItemsSource, PublishedSnapshot};
use crate::view_model::ViewModel;
use crate::walker::{start_scan, ScanOptions, SharedStore};

const PIPE_NAME: &str = r"\\.\pipe\nfm.win32.picker.v1";

pub trait PickerRequest {
    type Source : ItemsSource;
    fn search_string(&self) -> Option<&str>;
    fn run(&self) -> Arc<SharedStore>;
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemPickerRequest {
    #[serde(default = "default_command")]
    pub command: String,
    #[serde(default)]
    pub root_directories: Vec<String>,
    #[serde(default = "default_max_depth")]
    pub max_depth: i32,
    #[serde(default)]
    pub directories_only: bool,
    #[serde(default)]
    pub files_only: bool,
    #[serde(default)]
    pub search_string: Option<String>,
}

impl PickerRequest for FileSystemPickerRequest {
    type Source = PublishedSnapshot;

    fn search_string(&self) -> Option<&str> {
        self.search_string.as_deref()
    }

    fn run(&self) -> Arc<SharedStore> {
        let roots = if self.root_directories.is_empty() {
            vec![std::env::current_dir().expect("Cannot get current directory")]
        } else {
            self.root_directories.iter().map(PathBuf::from).collect()
        };

        let store = start_scan(ScanOptions {
            roots,
            max_depth: self.max_depth.max(0) as usize,
            directories_only: self.directories_only,
            files_only: self.files_only,
        });

        store
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PickerResponse {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_item: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

impl PickerResponse {
    pub fn selected(path: String) -> Self {
        Self {
            status: "selected".to_string(),
            selected_item: Some(path.clone()),
            selected_path: Some(path),
            error_message: None,
        }
    }

    pub fn cancelled() -> Self {
        Self {
            status: "cancelled".to_string(),
            selected_item: None,
            selected_path: None,
            error_message: None,
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            status: "error".to_string(),
            selected_item: None,
            selected_path: None,
            error_message: Some(message.into()),
        }
    }
}

fn default_command() -> String {
    "filesystem".to_string()
}

fn default_max_depth() -> i32 {
    i32::MAX
}

pub fn run_pipe_server(view_model: Arc<ViewModel>) -> Result<()> {
    loop {
        let handle = create_server_pipe()?;
        unsafe {
            if let Err(error) = ConnectNamedPipe(handle, None) {
                let last_error = GetLastError();
                if last_error.0 != 535 {
                    CloseHandle(handle)?;
                    return Err(anyhow!("ConnectNamedPipe failed: {error:?}"));
                }
            }
        }

        let raw_handle = handle.0 as usize;
        let connection_view_model = Arc::clone(&view_model);
        std::thread::spawn(move || {
            let handle = HANDLE(raw_handle as *mut c_void);
            if let Err(error) = handle_connection(handle, connection_view_model) {
                eprintln!("pipe connection failed: {error:?}");
            }
        });
    }
}

fn create_server_pipe() -> Result<HANDLE> {
    let name = wide_null(PIPE_NAME);
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            PIPE_UNLIMITED_INSTANCES,
            64 * 1024,
            64 * 1024,
            0,
            None,
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        Err(anyhow!("CreateNamedPipeW failed"))
    } else {
        Ok(handle)
    }
}

fn handle_connection(handle: HANDLE, view_model: Arc<ViewModel>) -> Result<()> {
    let owned = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    let reader_handle = owned
        .try_clone()
        .context("failed to clone pipe handle for reader")?;
    let mut reader = BufReader::new(std::fs::File::from(reader_handle));
    let mut writer = std::fs::File::from(owned);

    let mut line = String::new();
    reader.read_line(&mut line)?;

    let response = match serde_json::from_str::<FileSystemPickerRequest>(&line) {
        Ok(request) if !request.command.eq_ignore_ascii_case("filesystem") => {
            PickerResponse::error(format!("Unsupported command: {}", request.command))
        }
        Ok(request) => match view_model.run_request(&request) {
            Ok(response) => response,
            Err(error) => PickerResponse::error(error.to_string()),
        },
        Err(error) => PickerResponse::error(format!("Invalid request: {error}")),
    };

    serde_json::to_writer(&mut writer, &response)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

#[allow(dead_code)]
pub fn send_request(request: &FileSystemPickerRequest) -> Result<PickerResponse> {
    let name = wide_null(PIPE_NAME);
    let handle = unsafe {
        CreateFileW(
            PCWSTR(name.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }?;

    let owned = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    let reader_handle = owned.try_clone()?;
    let mut reader = BufReader::new(std::fs::File::from(reader_handle));
    let mut writer = std::fs::File::from(owned);

    serde_json::to_writer(&mut writer, request)?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    let mut line = String::new();
    reader.read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
