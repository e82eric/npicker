use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use anyhow::Result;
use crossbeam_channel::bounded;
use rust_nfm::action::{ActionConfig, ActionDefinition, ActionResolverDefinition, ActionService};
use rust_nfm::key_binding::{parse_key_chord, KeyChord, KeyModifiers, KeyName};
use rust_nfm::preview::{
    PreviewConfig, PreviewOutputType, PreviewProfile, PreviewResolver, PreviewService,
};
use rust_nfm::request::{
    CsvHeaderMode, DelimitedInputOptions, DelimitedStdinRequest, DelimitedTextSelector,
    DelimitedValueSelector, PickerRequest, PickerResponse, StdinRequest, StructuredCsvOptions,
    StructuredCsvStdinRequest,
};
#[cfg(windows)]
use rust_nfm::request::{
    FileSystemPickerRequest, ProcessListPickerRequest, WindowListPickerRequest,
};
use rust_nfm::selection::SelectedItem;
use rust_nfm::skia_ui as picker_ui;
use rust_nfm::view_model::{PickerActionOutcome, PickerInteractions, ViewModel};

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
    let mut options = app_options();
    install_accept_action(&mut options)?;
    validate_action_options(&options)?;
    if options.debug_wait {
        debug_wait();
    }

    nfm_search_core::timing::set_sink(output_timing);
    let is_window_list = matches!(&options.input, InputMode::ListWindows);
    let is_process_list = matches!(&options.input, InputMode::ListProcesses);
    let command_preview =
        options.preview_program.is_some() || options.preview_resolver_program.is_some();
    let native_window_preview =
        resolve_native_window_preview(is_window_list, options.window_preview, command_preview)?;
    let preview_enabled = command_preview || native_window_preview || is_process_list;
    let preview_config = match options.preview_resolver_program {
        Some(program) => PreviewConfig::Command(PreviewResolver::Process {
            program: program.into(),
            arguments: options.preview_resolver_arguments,
            profiles: options
                .preview_profiles
                .into_iter()
                .filter_map(|(name, profile)| {
                    profile.program.map(|program| {
                        (
                            name,
                            rust_nfm::preview::PreviewProfile {
                                program: program.into(),
                                arguments: profile.arguments,
                                working_directory: profile.working_directory,
                                output_type: profile.output_type,
                            },
                        )
                    })
                })
                .collect(),
            default_profile: options.preview_default_profile,
        }),
        None => match options.preview_program {
            Some(program) => PreviewConfig::Command(PreviewResolver::Fixed(PreviewProfile {
                program: program.into(),
                arguments: options.preview_arguments,
                working_directory: options.preview_cwd,
                output_type: options.preview_output_type,
            })),
            None if native_window_preview => PreviewConfig::NativeWindow,
            None if is_process_list => PreviewConfig::Formatted,
            None => PreviewConfig::None,
        },
    };
    let preview_visible = preview_enabled && options.preview_visible.unwrap_or(!is_process_list);
    let action_config = ActionConfig {
        resolvers: options
            .actions
            .into_iter()
            .filter_map(|(name, action)| {
                action.program.map(|program| {
                    (
                        name,
                        ActionDefinition::Process(ActionResolverDefinition {
                            program: program.into(),
                            arguments: action.arguments,
                        }),
                    )
                })
            })
            .collect(),
    };
    let bindings = options.bindings;
    let view_model = ViewModel::new_with_services_and_bindings(
        PreviewService::new(preview_config),
        ActionService::new(action_config),
        bindings,
        preview_visible,
    );
    let (completion_tx, completion_rx) = bounded(1);
    match options.input {
        InputMode::Stdin(None) => run_stdin_request(Arc::clone(&view_model), completion_tx),
        InputMode::Stdin(Some(options)) => {
            run_delimited_request(Arc::clone(&view_model), options, completion_tx)
        }
        InputMode::StructuredCsv(options) => {
            run_structured_csv_request(Arc::clone(&view_model), options, completion_tx)
        }
        InputMode::FileWalker(roots) => {
            run_filewalker_request(Arc::clone(&view_model), roots, completion_tx)?
        }
        InputMode::ListWindows => run_list_windows_request(Arc::clone(&view_model), completion_tx)?,
        InputMode::ListProcesses => {
            run_list_processes_request(Arc::clone(&view_model), completion_tx)?
        }
    }
    let code = picker_ui::run(
        view_model,
        Some(completion_rx),
        preview_enabled,
        preview_visible,
    )?;
    if code != 0 {
        std::process::exit(code);
    }
    Ok(())
}

const ACCEPT_ACTION_NAME: &str = "__nfm_accept";

fn install_accept_action(options: &mut AppOptions) -> Result<()> {
    let Some(program) = options.accept_resolver_program.take() else {
        return Ok(());
    };
    if options.actions.contains_key(ACCEPT_ACTION_NAME) {
        anyhow::bail!("action name '{ACCEPT_ACTION_NAME}' is reserved");
    }
    options.actions.insert(
        ACCEPT_ACTION_NAME.into(),
        ActionResolverOptions {
            declared: true,
            program: Some(program),
            arguments: std::mem::take(&mut options.accept_resolver_arguments),
        },
    );
    options
        .bindings
        .entry(KeyChord {
            key: KeyName::Enter,
            modifiers: KeyModifiers::default(),
        })
        .or_insert_with(|| ACCEPT_ACTION_NAME.into());
    Ok(())
}

fn validate_action_options(options: &AppOptions) -> Result<()> {
    for (name, action) in &options.actions {
        if !action.declared {
            anyhow::bail!("action '{name}' is missing --action {name} action-resolver");
        }
        if action.program.is_none() {
            anyhow::bail!("action '{name}' is missing --action-program");
        }
    }
    for action in options.bindings.values() {
        if !options.actions.contains_key(action) {
            anyhow::bail!("key binding references undefined action: {action}");
        }
    }
    Ok(())
}

fn resolve_native_window_preview(
    is_window_list: bool,
    requested: bool,
    has_command_preview: bool,
) -> Result<bool> {
    if requested && !is_window_list {
        anyhow::bail!("--window-preview is only valid with listwindows");
    }
    Ok(requested && !has_command_preview)
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

fn run_structured_csv_request(
    view_model: Arc<ViewModel>,
    options: StructuredCsvOptions,
    completion: crossbeam_channel::Sender<i32>,
) {
    std::thread::spawn(move || {
        let request = StructuredCsvStdinRequest::new(options, None);
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
fn run_list_windows_request(
    view_model: Arc<ViewModel>,
    completion: crossbeam_channel::Sender<i32>,
) -> Result<()> {
    let items = rust_nfm::list_windows::list_windows()?;
    std::thread::spawn(move || {
        let request = WindowListPickerRequest { items };
        let code = response_exit_code(view_model.run_request(&request));
        let _ = completion.send(code);
    });
    Ok(())
}

#[cfg(windows)]
fn run_list_processes_request(
    view_model: Arc<ViewModel>,
    completion: crossbeam_channel::Sender<i32>,
) -> Result<()> {
    let items = rust_nfm::list_processes::list_processes()?;
    std::thread::spawn(move || {
        let request = ProcessListPickerRequest { items };
        let code = response_exit_code(
            view_model.run_request_with_interactions(&request, process_interactions()),
        );
        let _ = completion.send(code);
    });
    Ok(())
}

#[cfg(windows)]
fn process_interactions() -> PickerInteractions {
    let refresh = Arc::new(|| {
        let items = rust_nfm::list_processes::list_processes()
            .map_err(|error| format!("Failed to refresh processes: {error}"))?;
        Ok(ProcessListPickerRequest { items }.run())
    });
    let kill = Arc::new(|selected: Option<&SelectedItem>| {
        let pid = selected
            .and_then(|item| item.fields.get("PID"))
            .ok_or_else(|| "No process selected".to_owned())?
            .parse::<u32>()
            .map_err(|error| format!("Selected item has an invalid PID: {error}"))?;
        rust_nfm::list_processes::terminate_process(pid)
            .map_err(|error| format!("Failed to terminate process {pid}: {error}"))?;
        Ok(PickerActionOutcome::RefreshWithToast(format!(
            "Terminated process {pid}"
        )))
    });
    let refresh_action = Arc::new(|_: Option<&SelectedItem>| Ok(PickerActionOutcome::Refresh));
    let preview = Arc::new(|item: &SelectedItem| {
        Ok(["Name", "PID", "WorkingSet", "PrivateBytes", "CPU"]
            .into_iter()
            .filter_map(|name| {
                item.fields
                    .get(name)
                    .map(|value| format!("{name}: {value}"))
            })
            .collect::<Vec<_>>()
            .join("\n"))
    });
    PickerInteractions {
        bindings: HashMap::from([
            (
                KeyChord {
                    key: KeyName::Character('k'),
                    modifiers: KeyModifiers {
                        ctrl: true,
                        ..KeyModifiers::default()
                    },
                },
                kill as _,
            ),
            (
                KeyChord {
                    key: KeyName::Character('r'),
                    modifiers: KeyModifiers {
                        ctrl: true,
                        ..KeyModifiers::default()
                    },
                },
                refresh_action as _,
            ),
        ]),
        refresh: Some(refresh),
        preview: Some(preview),
    }
}

#[cfg(not(windows))]
fn run_list_processes_request(
    _view_model: Arc<ViewModel>,
    _completion: crossbeam_channel::Sender<i32>,
) -> Result<()> {
    anyhow::bail!("the listprocesses command is currently available only on Windows")
}

#[cfg(not(windows))]
fn run_list_windows_request(
    _view_model: Arc<ViewModel>,
    _completion: crossbeam_channel::Sender<i32>,
) -> Result<()> {
    anyhow::bail!("the listwindows command is currently available only on Windows")
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
        Ok(PickerResponse::Selected(selection)) => {
            let mut stdout = std::io::stdout().lock();
            if writeln!(stdout, "{}", selection.value).is_err() || stdout.flush().is_err() {
                1
            } else {
                0
            }
        }
        Ok(PickerResponse::Cancelled) => 0,
        Ok(PickerResponse::Error(message)) => {
            eprintln!("{message}");
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
    StructuredCsv(StructuredCsvOptions),
    FileWalker(Vec<String>),
    ListWindows,
    ListProcesses,
}

struct AppOptions {
    debug_wait: bool,
    input: InputMode,
    preview_program: Option<String>,
    preview_arguments: Vec<String>,
    preview_cwd: Option<std::path::PathBuf>,
    preview_output_type: PreviewOutputType,
    preview_resolver_program: Option<String>,
    preview_resolver_arguments: Vec<String>,
    preview_profiles: HashMap<String, PreviewProfileOptions>,
    preview_default_profile: Option<String>,
    preview_visible: Option<bool>,
    window_preview: bool,
    accept_resolver_program: Option<String>,
    accept_resolver_arguments: Vec<String>,
    actions: HashMap<String, ActionResolverOptions>,
    bindings: HashMap<KeyChord, String>,
}

#[derive(Default)]
struct PreviewProfileOptions {
    program: Option<String>,
    arguments: Vec<String>,
    working_directory: Option<std::path::PathBuf>,
    output_type: PreviewOutputType,
}

#[derive(Default)]
struct ActionResolverOptions {
    declared: bool,
    program: Option<String>,
    arguments: Vec<String>,
}

fn app_options() -> AppOptions {
    let mut options = AppOptions {
        debug_wait: false,
        input: InputMode::Stdin(None),
        preview_program: None,
        preview_arguments: Vec::new(),
        preview_cwd: None,
        preview_output_type: PreviewOutputType::Text,
        preview_resolver_program: None,
        preview_resolver_arguments: Vec::new(),
        preview_profiles: HashMap::new(),
        preview_default_profile: None,
        preview_visible: None,
        window_preview: false,
        accept_resolver_program: None,
        accept_resolver_arguments: Vec::new(),
        actions: HashMap::new(),
        bindings: HashMap::new(),
    };
    let mut filewalker = false;
    let mut list_windows = false;
    let mut list_processes = false;
    let mut roots = Vec::new();
    let mut delimiter = None;
    let mut csv_input = false;
    let mut csv_columns = None;
    let mut csv_delimiter = b',';
    let mut text = DelimitedTextSelector::FullLine;
    let mut value = DelimitedValueSelector::FullLine;
    let mut preview_file_field = None;
    let mut preview_center_line_field = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "filewalker" if !filewalker => filewalker = true,
            "listwindows" | "ListWindows" if !filewalker => list_windows = true,
            "listprocesses" | "ListProcesses" if !filewalker => list_processes = true,
            "--stdin" if !filewalker => {}
            "--input-format" if !filewalker => match args.next().as_deref() {
                Some("csv") => csv_input = true,
                Some(value) => eprintln!("unsupported input format: {value}"),
                None => eprintln!("--input-format requires a value"),
            },
            "--csv-columns" if !filewalker => {
                csv_columns = args
                    .next()
                    .map(|value| value.split(',').map(str::to_owned).collect());
                if csv_columns.is_none() {
                    eprintln!("--csv-columns requires comma-separated names");
                }
            }
            "--csv-delimiter" if !filewalker => {
                match args
                    .next()
                    .as_deref()
                    .and_then(parse_delimiter)
                    .filter(|ch| ch.is_ascii())
                {
                    Some(delimiter) => csv_delimiter = delimiter as u8,
                    None => eprintln!("--csv-delimiter requires one ASCII character or \\t"),
                }
            }
            "--debug-wait" => options.debug_wait = true,
            "--preview" => {
                if let Some(program) = args.next() {
                    options.preview_program = Some(program);
                } else {
                    eprintln!("--preview requires an executable");
                }
            }
            "--preview-arg" => {
                if let Some(argument) = args.next() {
                    options.preview_arguments.push(argument);
                } else {
                    eprintln!("--preview-arg requires a value");
                }
            }
            "--preview-type" => match args.next().as_deref() {
                Some("text") => options.preview_output_type = PreviewOutputType::Text,
                Some("image") => options.preview_output_type = PreviewOutputType::Image,
                Some(value) => eprintln!("unsupported preview type: {value}"),
                None => eprintln!("--preview-type requires 'text' or 'image'"),
            },
            "--preview-resolver" => {
                options.preview_resolver_program = args.next();
                if options.preview_resolver_program.is_none() {
                    eprintln!("--preview-resolver requires an executable");
                }
            }
            "--preview-resolver-arg" => {
                if let Some(argument) = args.next() {
                    options.preview_resolver_arguments.push(argument);
                } else {
                    eprintln!("--preview-resolver-arg requires a value");
                }
            }
            "--preview-command" => {
                if let (Some(profile), Some(program)) = (args.next(), args.next()) {
                    options.preview_profiles.entry(profile).or_default().program = Some(program);
                } else {
                    eprintln!("--preview-command requires a profile and executable");
                }
            }
            "--preview-command-arg" => {
                if let (Some(profile), Some(argument)) = (args.next(), args.next()) {
                    options
                        .preview_profiles
                        .entry(profile)
                        .or_default()
                        .arguments
                        .push(argument);
                } else {
                    eprintln!("--preview-command-arg requires a profile and argument");
                }
            }
            "--preview-command-type" => {
                if let (Some(profile), Some(output_type)) = (args.next(), args.next()) {
                    let output_type = match output_type.as_str() {
                        "text" => Some(PreviewOutputType::Text),
                        "image" => Some(PreviewOutputType::Image),
                        _ => None,
                    };
                    if let Some(output_type) = output_type {
                        options
                            .preview_profiles
                            .entry(profile)
                            .or_default()
                            .output_type = output_type;
                    } else {
                        eprintln!("--preview-command-type requires 'text' or 'image'");
                    }
                } else {
                    eprintln!("--preview-command-type requires a profile and type");
                }
            }
            "--preview-command-cwd" => {
                if let (Some(profile), Some(directory)) = (args.next(), args.next()) {
                    options
                        .preview_profiles
                        .entry(profile)
                        .or_default()
                        .working_directory = Some(directory.into());
                } else {
                    eprintln!("--preview-command-cwd requires a profile and directory");
                }
            }
            "--preview-default" => {
                options.preview_default_profile = args.next();
                if options.preview_default_profile.is_none() {
                    eprintln!("--preview-default requires a profile");
                }
            }
            "--preview-visible" => match args.next().as_deref() {
                Some("true") => options.preview_visible = Some(true),
                Some("false") => options.preview_visible = Some(false),
                Some(value) => eprintln!("--preview-visible requires true or false, got: {value}"),
                None => eprintln!("--preview-visible requires true or false"),
            },
            "--preview-cwd" => {
                if let Some(directory) = args.next() {
                    options.preview_cwd = Some(directory.into());
                } else {
                    eprintln!("--preview-cwd requires a directory");
                }
            }
            "--window-preview" => options.window_preview = true,
            "--accept-resolver" => {
                options.accept_resolver_program = args.next();
                if options.accept_resolver_program.is_none() {
                    eprintln!("--accept-resolver requires an executable");
                }
            }
            "--accept-resolver-arg" => {
                if let Some(argument) = args.next() {
                    options.accept_resolver_arguments.push(argument);
                } else {
                    eprintln!("--accept-resolver-arg requires a value");
                }
            }
            "--action" => {
                if let (Some(name), Some(kind)) = (args.next(), args.next()) {
                    if kind == "action-resolver" {
                        options.actions.entry(name).or_default().declared = true;
                    } else {
                        eprintln!("unsupported action type: {kind}");
                    }
                } else {
                    eprintln!("--action requires a name and action-resolver");
                }
            }
            "--action-program" => {
                if let (Some(name), Some(program)) = (args.next(), args.next()) {
                    options.actions.entry(name).or_default().program = Some(program);
                } else {
                    eprintln!("--action-program requires an action name and executable");
                }
            }
            "--action-arg" => {
                if let (Some(name), Some(argument)) = (args.next(), args.next()) {
                    options
                        .actions
                        .entry(name)
                        .or_default()
                        .arguments
                        .push(argument);
                } else {
                    eprintln!("--action-arg requires an action name and argument");
                }
            }
            "--bind" => {
                if let (Some(chord), Some(action)) = (args.next(), args.next()) {
                    if let Some(chord) = parse_key_chord(&chord) {
                        options.bindings.insert(chord, action);
                    } else {
                        eprintln!("invalid key binding: {chord}");
                    }
                } else {
                    eprintln!("--bind requires a key chord and action name");
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
    if list_windows {
        options.input = InputMode::ListWindows;
    } else if list_processes {
        options.input = InputMode::ListProcesses;
    } else if filewalker {
        options.input = InputMode::FileWalker(roots);
    } else if csv_input {
        options.input = InputMode::StructuredCsv(StructuredCsvOptions {
            headers: csv_columns.map_or(CsvHeaderMode::FirstRecord, CsvHeaderMode::Explicit),
            delimiter: csv_delimiter,
        });
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

#[cfg(test)]
mod tests {
    use super::resolve_native_window_preview;

    #[test]
    fn command_preview_overrides_native_window_preview() {
        assert!(!resolve_native_window_preview(true, true, true).unwrap());
        assert!(resolve_native_window_preview(true, true, false).unwrap());
    }

    #[test]
    fn native_window_preview_is_rejected_for_other_inputs() {
        assert!(resolve_native_window_preview(false, true, false).is_err());
    }
}
