use std::io::Write;
use std::sync::Arc;

use anyhow::Result;
use crossbeam_channel::bounded;
#[cfg(windows)]
use rust_nfm::request::FileSystemPickerRequest;
use rust_nfm::request::{
    DelimitedInputOptions, DelimitedStdinRequest, DelimitedTextSelector, DelimitedValueSelector,
    PickerResponse, StdinRequest,
};
use rust_nfm::skia_ui;
use rust_nfm::view_model::ViewModel;

fn output_timing(line: &str) {
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;

        let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            OutputDebugStringW(PCWSTR(wide.as_ptr()));
        }
    }
    #[cfg(not(windows))]
    eprintln!("{line}");
}

fn main() -> Result<()> {
    let options = app_options();
    if options.debug_wait {
        debug_wait();
    }

    nfm_search_core::timing::set_sink(output_timing);
    let preview_enabled = options.preview_command.is_some();
    let view_model =
        ViewModel::new_with_preview_options(options.preview_command, options.preview_cwd);
    let (completion_tx, completion_rx) = bounded(1);
    match options.input {
        InputMode::Stdin(None) => run_stdin_request(Arc::clone(&view_model), completion_tx),
        InputMode::Stdin(Some(options)) => {
            run_delimited_request(Arc::clone(&view_model), options, completion_tx)
        }
        InputMode::FileWalker(roots) => {
            run_filewalker_request(Arc::clone(&view_model), roots, completion_tx)?
        }
    }
    let code = skia_ui::run(view_model, Some(completion_rx), preview_enabled)?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

fn run_delimited_request(
    view_model: Arc<ViewModel>,
    options: DelimitedInputOptions,
    completion: crossbeam_channel::Sender<i32>,
) {
    std::thread::spawn(move || {
        let request = DelimitedStdinRequest::new(options, None);
        let code = response_exit_code(view_model.run_request(&request));
        let _ = completion.send(code);
    });
}

fn debug_wait() {
    eprintln!("pid: {}", std::process::id());
    eprintln!("Attach debugger now...");
    std::thread::sleep(std::time::Duration::from_secs(20));
}

fn run_stdin_request(view_model: Arc<ViewModel>, completion: crossbeam_channel::Sender<i32>) {
    std::thread::spawn(move || {
        let request = StdinRequest::new(None);
        let code = response_exit_code(view_model.run_request(&request));
        let _ = completion.send(code);
    });
}

#[cfg(windows)]
fn run_filewalker_request(
    view_model: Arc<ViewModel>,
    roots: Vec<String>,
    completion: crossbeam_channel::Sender<i32>,
) -> Result<()> {
    let roots = if roots.is_empty() {
        vec![default_home_directory()?]
    } else {
        roots
    };
    std::thread::spawn(move || {
        let request = FileSystemPickerRequest {
            root_directories: roots,
            max_depth: i32::MAX,
            directories_only: false,
            files_only: false,
            search_string: None,
        };
        let code = response_exit_code(view_model.run_request(&request));
        let _ = completion.send(code);
    });
    Ok(())
}

#[cfg(not(windows))]
fn run_filewalker_request(
    _view_model: Arc<ViewModel>,
    _roots: Vec<String>,
    _completion: crossbeam_channel::Sender<i32>,
) -> Result<()> {
    anyhow::bail!("the filewalker input mode is currently available only on Windows")
}

#[cfg(windows)]
fn default_home_directory() -> Result<String> {
    if let Some(profile) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
        return Ok(profile.to_string_lossy().into_owned());
    }
    if let (Some(drive), Some(path)) = (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH"))
    {
        let mut home = std::path::PathBuf::from(drive);
        home.push(path);
        return Ok(home.to_string_lossy().into_owned());
    }
    Ok(std::env::current_dir()?.to_string_lossy().into_owned())
}

fn response_exit_code(response: Result<PickerResponse>) -> i32 {
    match response {
        Ok(response) if response.status == "selected" => {
            if let Some(item) = response.selected_item {
                let mut stdout = std::io::stdout().lock();
                if writeln!(stdout, "{item}").is_err() || stdout.flush().is_err() {
                    1
                } else {
                    0
                }
            } else {
                0
            }
        }
        Ok(response) if response.status == "cancelled" => 0,
        Ok(response) => {
            if let Some(message) = response.error_message {
                eprintln!("{message}");
            }
            1
        }
        Err(error) => {
            eprintln!("{error:?}");
            1
        }
    }
}

enum InputMode {
    Stdin(Option<DelimitedInputOptions>),
    FileWalker(Vec<String>),
}

struct AppOptions {
    debug_wait: bool,
    input: InputMode,
    preview_command: Option<String>,
    preview_cwd: Option<std::path::PathBuf>,
}

fn app_options() -> AppOptions {
    let mut options = AppOptions {
        debug_wait: false,
        input: InputMode::Stdin(None),
        preview_command: None,
        preview_cwd: None,
    };
    let mut filewalker = false;
    let mut roots = Vec::new();
    let mut delimiter = None;
    let mut text = DelimitedTextSelector::FullLine;
    let mut value = DelimitedValueSelector::FullLine;
    let mut preview_file_field = None;
    let mut preview_center_line_field = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "filewalker" if !filewalker => filewalker = true,
            "--stdin" if !filewalker => {}
            "--debug-wait" => options.debug_wait = true,
            "--preview" => {
                if let Some(command) = args.next() {
                    options.preview_command = Some(command);
                } else {
                    eprintln!("--preview requires a command");
                }
            }
            "--preview-cwd" => {
                if let Some(directory) = args.next() {
                    options.preview_cwd = Some(directory.into());
                } else {
                    eprintln!("--preview-cwd requires a directory");
                }
            }
            "--delimiter" if !filewalker => {
                if let Some(value) = args.next() {
                    delimiter = parse_delimiter(&value);
                    if delimiter.is_none() {
                        eprintln!("--delimiter requires one character or \\\\t");
                    }
                } else {
                    eprintln!("--delimiter requires a value");
                }
            }
            "--text-field" if !filewalker => {
                if let Some(selector) = parse_text_selector(args.next()) {
                    text = selector;
                }
            }
            "--value-field" if !filewalker => {
                if let Some(selector) = parse_value_selector(args.next()) {
                    value = selector;
                }
            }
            "--preview-file-field" if !filewalker => {
                preview_file_field = parse_field(args.next(), "--preview-file-field");
            }
            "--preview-center-line-field" if !filewalker => {
                preview_center_line_field = parse_field(args.next(), "--preview-center-line-field");
            }
            _ if filewalker => roots.push(arg),
            _ => eprintln!("ignoring unsupported argument: {arg}"),
        }
    }
    if filewalker {
        options.input = InputMode::FileWalker(roots);
    } else if let Some(delimiter) = delimiter {
        options.input = InputMode::Stdin(Some(DelimitedInputOptions {
            delimiter,
            text,
            value,
            preview_file_field,
            preview_center_line_field,
        }));
    }
    options
}

fn parse_text_selector(value: Option<String>) -> Option<DelimitedTextSelector> {
    let Some(value) = value else {
        eprintln!("--text-field requires 'all' or a one-based field number");
        return None;
    };
    if value.eq_ignore_ascii_case("all") {
        return Some(DelimitedTextSelector::FullLine);
    }
    match value.parse::<usize>() {
        Ok(field) if field > 0 => Some(DelimitedTextSelector::Field(field - 1)),
        _ => {
            eprintln!("--text-field requires 'all' or a positive field number");
            None
        }
    }
}

fn parse_value_selector(value: Option<String>) -> Option<DelimitedValueSelector> {
    let Some(value) = value else {
        eprintln!("--value-field requires 'all' or a one-based field number");
        return None;
    };
    if value.eq_ignore_ascii_case("all") {
        return Some(DelimitedValueSelector::FullLine);
    }
    match value.parse::<usize>() {
        Ok(field) if field > 0 => Some(DelimitedValueSelector::Field(field - 1)),
        _ => {
            eprintln!("--value-field requires 'all' or a positive field number");
            None
        }
    }
}

fn parse_delimiter(value: &str) -> Option<char> {
    if value == "\\t" {
        return Some('\t');
    }
    let mut chars = value.chars();
    let delimiter = chars.next()?;
    chars.next().is_none().then_some(delimiter)
}

fn parse_field(value: Option<String>, option: &str) -> Option<usize> {
    let Some(value) = value else {
        eprintln!("{option} requires a one-based field number");
        return None;
    };
    match value.parse::<usize>() {
        Ok(field) if field > 0 => Some(field - 1),
        _ => {
            eprintln!("{option} requires a positive field number");
            None
        }
    }
}
