//! Command-line options. Each source of options (config file, NFM_DEFAULT_OPTS,
//! the command line) is a list of the same flags, applied in that order, so a
//! later scalar flag overrides an earlier one and repeated flags accumulate.
use crate::keys;
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    Stdin,
    File(PathBuf),
    /// Program and arguments, everything after `--`.
    Run(Vec<String>),
    FileSystem(Vec<String>),
    Processes,
    Windows,
}
impl Command {
    pub fn is_line_input(&self) -> bool {
        matches!(self, Self::Stdin | Self::File(_) | Self::Run(_))
    }
    fn name(&self) -> &'static str {
        match self {
            Self::Stdin => "stdin",
            Self::File(_) => "file",
            Self::Run(_) => "command",
            Self::FileSystem(_) => "filesystem",
            Self::Processes => "processes",
            Self::Windows => "windows",
        }
    }
}

/// A delimited field. Fields are numbered from 1 on the command line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    All,
    Index(usize),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputFormat {
    #[default]
    Lines,
    Csv,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Plain,
    Json,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WindowPosition {
    #[default]
    Center,
    Top,
    Mouse,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PreviewKind {
    /// Program output with ANSI colors.
    #[default]
    Text,
    Image,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Program {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PreviewCommand {
    pub program: Program,
    pub kind: PreviewKind,
}

/// Bind targets every picker understands; anything else names an `--action`.
pub const BUILTIN_ACTIONS: &[&str] = &["accept", "cancel", "parent"];

#[derive(Clone, Debug, PartialEq)]
pub struct Options {
    pub command: Command,
    pub query: String,
    pub title: Option<String>,
    // Input
    pub delimiter: Option<char>,
    pub text_field: Field,
    pub value_field: Field,
    pub input_format: InputFormat,
    pub csv_columns: Option<Vec<String>>,
    /// Column types, `NAME:TYPE,...`; see `StructuredSchema::with_types`.
    pub csv_types: Option<String>,
    /// Table formats, `NAME:FORMAT,...`; see `StructuredSchema::with_formats`.
    pub csv_formats: Option<String>,
    pub csv_delimiter: u8,
    /// Card layout for csv rows; see `RowTemplate::parse`.
    pub row_template: Option<String>,
    /// Slot painted as an icon beside each card title, such as `path:devicon`.
    pub row_icon: Option<String>,
    pub preview_file_field: Option<usize>,
    pub preview_center_line_field: Option<usize>,
    pub unique: bool,
    pub line_continuation: Option<String>,
    pub reverse: bool,
    pub max_depth: Option<usize>,
    pub files_only: bool,
    pub directories_only: bool,
    // Preview
    pub preview: Option<PreviewCommand>,
    pub preview_resolver: Option<Program>,
    pub preview_profiles: BTreeMap<String, PreviewCommand>,
    pub preview_default: Option<String>,
    /// None chooses: native file preview when `--preview-file-field` is set.
    pub preview_native: Option<bool>,
    pub preview_visible: Option<bool>,
    /// Wrap plain-text previews at the pane width (default true).
    pub preview_wrap: bool,
    pub window_preview: bool,
    // Actions
    pub accept_resolver: Option<Program>,
    pub actions: BTreeMap<String, Program>,
    pub bindings: Vec<(keys::Chord, String)>,
    pub expect: Vec<keys::Chord>,
    // Presentation
    pub layout_top: bool,
    pub sort_input: bool,
    pub rows: Option<usize>,
    pub preview_rows: Option<usize>,
    pub scroll_padding: Option<u32>,
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub position: WindowPosition,
    pub font: Option<PathBuf>,
    pub font_size: Option<f32>,
    pub colors: Vec<(String, String)>,
    // Output
    pub print_query: bool,
    pub output: OutputFormat,
    pub select_1: bool,
    pub exit_0: bool,
    pub filter: Option<String>,
    pub activate: bool,
    pub debug_wait: bool,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            command: Command::Stdin,
            query: String::new(),
            title: None,
            delimiter: None,
            text_field: Field::All,
            value_field: Field::All,
            input_format: InputFormat::Lines,
            csv_columns: None,
            csv_types: None,
            csv_formats: None,
            csv_delimiter: b',',
            row_template: None,
            row_icon: None,
            preview_file_field: None,
            preview_center_line_field: None,
            unique: false,
            line_continuation: None,
            reverse: false,
            max_depth: None,
            files_only: false,
            directories_only: false,
            preview: None,
            preview_resolver: None,
            preview_profiles: BTreeMap::new(),
            preview_default: None,
            preview_native: None,
            preview_visible: None,
            preview_wrap: true,
            window_preview: false,
            accept_resolver: None,
            actions: BTreeMap::new(),
            bindings: Vec::new(),
            expect: Vec::new(),
            layout_top: false,
            sort_input: false,
            rows: None,
            preview_rows: None,
            scroll_padding: None,
            width: None,
            height: None,
            position: WindowPosition::Center,
            font: None,
            font_size: None,
            colors: Vec::new(),
            print_query: false,
            output: OutputFormat::Plain,
            select_1: false,
            exit_0: false,
            filter: None,
            activate: false,
            debug_wait: false,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Parsed {
    Help,
    Version,
    Run(Box<Options>),
}

/// One list of flags and where it came from, for error messages.
pub struct Layer {
    pub origin: String,
    pub args: Vec<String>,
    /// Only the command line may name a command or its operands.
    pub positionals: bool,
}

/// Flags that take no value; every other flag takes at least one.
const SWITCHES: &[&str] = &[
    "help",
    "version",
    "stdin",
    "files-only",
    "directories-only",
    "window-preview",
    "unique",
    "reverse",
    "print-query",
    "select-1",
    "exit-0",
    "activate",
    "debug-wait",
    "no-config",
];
pub fn is_switch(name: &str) -> bool {
    SWITCHES.contains(&name)
}

/// Gathered while parsing so that flags may come in any order.
#[derive(Default)]
struct Draft {
    options: Options,
    help: bool,
    version: bool,
    command: Option<String>,
    operands: Vec<String>,
    preview_program: Option<String>,
    preview_args: Vec<String>,
    preview_kind: Option<PreviewKind>,
    preview_cwd: Option<PathBuf>,
    preview_resolver: Option<String>,
    preview_resolver_args: Vec<String>,
    profiles: BTreeMap<String, ProfileDraft>,
    accept_resolver: Option<String>,
    accept_resolver_args: Vec<String>,
    actions: BTreeMap<String, ActionDraft>,
    bindings: Vec<(String, String)>,
}
#[derive(Default)]
struct ProfileDraft {
    program: Option<String>,
    args: Vec<String>,
    kind: Option<PreviewKind>,
    cwd: Option<PathBuf>,
}
#[derive(Default)]
struct ActionDraft {
    declared: bool,
    program: Option<String>,
    args: Vec<String>,
}

pub fn parse(layers: &[Layer]) -> Result<Parsed, String> {
    let mut draft = Draft::default();
    for layer in layers {
        parse_layer(&mut draft, layer).map_err(|error| {
            if layer.origin == "command line" {
                error
            } else {
                format!("{}: {error}", layer.origin)
            }
        })?;
    }
    if draft.help {
        return Ok(Parsed::Help);
    }
    if draft.version {
        return Ok(Parsed::Version);
    }
    finish(draft).map(|options| Parsed::Run(Box::new(options)))
}

fn parse_layer(draft: &mut Draft, layer: &Layer) -> Result<(), String> {
    let mut args = layer.args.iter().cloned();
    let mut raw = false;
    while let Some(arg) = args.next() {
        if raw || !arg.starts_with('-') || arg == "-" {
            if !layer.positionals {
                return Err(format!("unexpected argument '{arg}': only options are allowed here"));
            }
            if draft.command.is_none() && !raw {
                draft.command = Some(arg);
            } else {
                draft.operands.push(arg);
            }
            continue;
        }
        if arg == "--" {
            if !layer.positionals {
                return Err("'--' is only allowed on the command line".into());
            }
            raw = true;
            continue;
        }
        if arg == "-h" {
            draft.help = true;
            continue;
        }
        if arg == "-q" || arg == "-f" {
            let value = args.next().ok_or_else(|| format!("{arg} requires a value"))?;
            if arg == "-q" {
                draft.options.query = value;
            } else {
                draft.options.filter = Some(value);
            }
            continue;
        }
        let Some(flag) = arg.strip_prefix("--") else {
            return Err(format!("unknown option '{arg}'"));
        };
        let (name, inline) = match flag.split_once('=') {
            Some((name, value)) => (name.to_owned(), Some(value.to_owned())),
            None => (flag.to_owned(), None),
        };
        if is_switch(&name) {
            if inline.is_some() {
                return Err(format!("--{name} does not take a value"));
            }
            apply_switch(draft, &name)?;
            continue;
        }
        let mut inline = inline;
        let mut value = |what: &str| -> Result<String, String> {
            inline
                .take()
                .or_else(|| args.next())
                .ok_or_else(|| format!("--{name} requires {what}"))
        };
        apply_value(draft, &name, &mut value)?;
    }
    Ok(())
}

fn apply_switch(draft: &mut Draft, name: &str) -> Result<(), String> {
    let o = &mut draft.options;
    match name {
        "help" => draft.help = true,
        "version" => draft.version = true,
        // The default input; accepted for compatibility.
        "stdin" => {}
        "files-only" => o.files_only = true,
        "directories-only" => o.directories_only = true,
        "window-preview" => o.window_preview = true,
        "unique" => o.unique = true,
        "reverse" => o.reverse = true,
        "print-query" => o.print_query = true,
        "select-1" => o.select_1 = true,
        "exit-0" => o.exit_0 = true,
        "activate" => o.activate = true,
        "debug-wait" => o.debug_wait = true,
        // Read before the config file is loaded.
        "no-config" => {}
        _ => unreachable!("unhandled switch {name}"),
    }
    Ok(())
}

fn apply_value(
    draft: &mut Draft,
    name: &str,
    value: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Result<(), String> {
    let flag = format!("--{name}");
    let o = &mut draft.options;
    match name {
        "query" => o.query = value("a value")?,
        "title" => o.title = Some(value("a value")?),
        "config" => {
            // Read before the config file is loaded.
            value("a path")?;
        }
        // Input
        "delimiter" => o.delimiter = Some(parse_delimiter(&flag, &value("a character")?)?),
        "text-field" => o.text_field = parse_field(&flag, &value("all or a field number")?)?,
        "value-field" => o.value_field = parse_field(&flag, &value("all or a field number")?)?,
        "input-format" => {
            o.input_format = match value("lines or csv")?.as_str() {
                "lines" => InputFormat::Lines,
                "csv" => InputFormat::Csv,
                other => return Err(format!("{flag} must be lines or csv, not '{other}'")),
            }
        }
        "csv-columns" => {
            let columns: Vec<String> = value("column names")?
                .split(',')
                .map(|c| c.trim().to_owned())
                .collect();
            if columns.iter().any(String::is_empty) {
                return Err(format!("{flag} requires comma-separated, non-empty column names"));
            }
            o.csv_columns = Some(columns);
        }
        "csv-types" => o.csv_types = Some(value("NAME:TYPE pairs")?),
        "csv-formats" => o.csv_formats = Some(value("NAME:FORMAT pairs")?),
        "row-template" => o.row_template = Some(value("a row template")?),
        "row-icon" => o.row_icon = Some(value("an icon slot")?),
        "csv-delimiter" => {
            let delimiter = parse_delimiter(&flag, &value("a character")?)?;
            o.csv_delimiter = u8::try_from(delimiter)
                .ok()
                .filter(u8::is_ascii)
                .ok_or_else(|| format!("{flag} must be a single ASCII character"))?;
        }
        "preview-file-field" => {
            o.preview_file_field = Some(parse_field_number(&flag, &value("a field number")?)?)
        }
        "preview-center-line-field" => {
            o.preview_center_line_field =
                Some(parse_field_number(&flag, &value("a field number")?)?)
        }
        "line-continuation" => {
            let marker = value("a marker")?;
            if marker.is_empty() {
                return Err(format!("{flag} requires a non-empty marker"));
            }
            o.line_continuation = Some(marker);
        }
        "max-depth" => o.max_depth = Some(parse_number(&flag, &value("a number")?)?),
        // Preview
        "preview" => draft.preview_program = Some(value("a program")?),
        "preview-arg" => draft.preview_args.push(value("an argument")?),
        "preview-type" => draft.preview_kind = Some(parse_kind(&flag, &value("text or image")?)?),
        "preview-cwd" => draft.preview_cwd = Some(value("a directory")?.into()),
        "preview-visible" => o.preview_visible = Some(parse_bool(&flag, &value("true or false")?)?),
        "preview-wrap" => o.preview_wrap = parse_bool(&flag, &value("true or false")?)?,
        "preview-native" => {
            o.preview_native = parse_auto_bool(&flag, &value("auto, true or false")?)?
        }
        "preview-resolver" => draft.preview_resolver = Some(value("a program")?),
        "preview-resolver-arg" => draft.preview_resolver_args.push(value("an argument")?),
        "preview-command" => {
            let name = profile_name(&flag, value("a profile name")?)?;
            let program = value("a profile name and a program")?;
            draft.profiles.entry(name).or_default().program = Some(program);
        }
        "preview-command-arg" => {
            let name = profile_name(&flag, value("a profile name")?)?;
            let arg = value("a profile name and an argument")?;
            draft.profiles.entry(name).or_default().args.push(arg);
        }
        "preview-command-type" => {
            let name = profile_name(&flag, value("a profile name")?)?;
            let kind = parse_kind(&flag, &value("a profile name and text or image")?)?;
            draft.profiles.entry(name).or_default().kind = Some(kind);
        }
        "preview-command-cwd" => {
            let name = profile_name(&flag, value("a profile name")?)?;
            let cwd = value("a profile name and a directory")?;
            draft.profiles.entry(name).or_default().cwd = Some(cwd.into());
        }
        "preview-default" => o.preview_default = Some(value("a profile name")?),
        // Actions
        "accept-resolver" => draft.accept_resolver = Some(value("a program")?),
        "accept-resolver-arg" => draft.accept_resolver_args.push(value("an argument")?),
        "action" => {
            let name = action_name(&flag, value("an action name")?)?;
            let kind = value("an action name and a type")?;
            if kind != "action-resolver" {
                return Err(format!("{flag} {name}: the only action type is action-resolver, not '{kind}'"));
            }
            draft.actions.entry(name).or_default().declared = true;
        }
        "action-program" => {
            let name = action_name(&flag, value("an action name")?)?;
            let program = value("an action name and a program")?;
            draft.actions.entry(name).or_default().program = Some(program);
        }
        "action-arg" => {
            let name = action_name(&flag, value("an action name")?)?;
            let arg = value("an action name and an argument")?;
            draft.actions.entry(name).or_default().args.push(arg);
        }
        "bind" => {
            let chord = value("a key and an action")?;
            let action = value("a key and an action")?;
            draft.bindings.push((chord, action));
        }
        "expect" => {
            for chord in value("comma-separated keys")?.split(',') {
                let chord = keys::parse(chord.trim()).map_err(|e| format!("{flag}: {e}"))?;
                if !o.expect.contains(&chord) {
                    o.expect.push(chord);
                }
            }
        }
        // Presentation
        "layout" => {
            o.layout_top = match value("top or bottom")?.as_str() {
                "top" => true,
                "bottom" => false,
                other => return Err(format!("{flag} must be top or bottom, not '{other}'")),
            }
        }
        "sort" => {
            o.sort_input = match value("score or input")?.as_str() {
                "score" => false,
                "input" => true,
                other => return Err(format!("{flag} must be score or input, not '{other}'")),
            }
        }
        "rows" => o.rows = Some(parse_count(&flag, &value("a number")?)?),
        "preview-rows" => o.preview_rows = Some(parse_count(&flag, &value("a number")?)?),
        "scroll-padding" => o.scroll_padding = Some(parse_number(&flag, &value("a number")?)? as u32),
        "width" => o.width = Some(parse_size(&flag, &value("a size in points")?)?),
        "height" => o.height = Some(parse_size(&flag, &value("a size in points")?)?),
        "position" => {
            o.position = match value("center, top or mouse")?.as_str() {
                "center" => WindowPosition::Center,
                "top" => WindowPosition::Top,
                "mouse" | "cursor" => WindowPosition::Mouse,
                other => return Err(format!("{flag} must be center, top or mouse, not '{other}'")),
            }
        }
        "font" => o.font = Some(value("a font file")?.into()),
        "font-size" => {
            let size = parse_size(&flag, &value("a size in points")?)?;
            if !(4.0..=96.0).contains(&size) {
                return Err(format!("{flag} must be between 4 and 96"));
            }
            o.font_size = Some(size);
        }
        "color" => {
            for entry in value("NAME=#RRGGBB pairs")?.split(',') {
                let (name, color) = entry
                    .split_once('=')
                    .ok_or_else(|| format!("{flag}: expected NAME=#RRGGBB, not '{entry}'"))?;
                let (name, color) = (name.trim().to_owned(), color.trim().to_owned());
                crate::theme::check(&name, &color).map_err(|e| format!("{flag}: {e}"))?;
                o.colors.push((name, color));
            }
        }
        // Output
        "output" => {
            o.output = match value("plain or json")?.as_str() {
                "plain" => OutputFormat::Plain,
                "json" => OutputFormat::Json,
                other => return Err(format!("{flag} must be plain or json, not '{other}'")),
            }
        }
        "filter" => o.filter = Some(value("a query")?),
        _ => return Err(format!("unknown option '--{name}'")),
    }
    Ok(())
}

fn finish(draft: Draft) -> Result<Options, String> {
    let Draft {
        mut options,
        command,
        operands,
        preview_program,
        preview_args,
        preview_kind,
        preview_cwd,
        preview_resolver,
        preview_resolver_args,
        profiles,
        accept_resolver,
        accept_resolver_args,
        actions,
        bindings,
        ..
    } = draft;
    options.command = match command.as_deref() {
        None => Command::Stdin,
        Some("file") => match operands.as_slice() {
            [path] => Command::File(path.into()),
            _ => return Err("file requires exactly one path".into()),
        },
        Some("command") => {
            if operands.is_empty() {
                return Err("command requires a program: nfm command [OPTIONS] -- PROGRAM [ARGS...]".into());
            }
            Command::Run(operands.clone())
        }
        Some("filesystem") => Command::FileSystem(operands.clone()),
        Some("processes") => Command::Processes,
        Some("windows") => Command::Windows,
        Some(other) => {
            return Err(format!(
                "unknown command '{other}'; expected file, command, filesystem, processes or windows"
            ));
        }
    };
    if matches!(options.command, Command::Processes | Command::Windows | Command::Stdin)
        && !operands.is_empty()
    {
        return Err(format!(
            "{} does not take arguments: '{}'",
            options.command.name(),
            operands[0]
        ));
    }
    let o = &mut options;
    let command = o.command.clone();
    let only = |enabled: bool, flag: &str, allowed: &str| -> Result<(), String> {
        if enabled {
            Err(format!("{flag} is only valid with {allowed}"))
        } else {
            Ok(())
        }
    };
    let filesystem = matches!(command, Command::FileSystem(_));
    only(!filesystem && o.max_depth.is_some(), "--max-depth", "filesystem")?;
    only(!filesystem && o.files_only, "--files-only", "filesystem")?;
    only(!filesystem && o.directories_only, "--directories-only", "filesystem")?;
    if o.files_only && o.directories_only {
        return Err("--files-only and --directories-only cannot be combined".into());
    }
    let windows = command == Command::Windows;
    only(!windows && o.window_preview, "--window-preview", "windows")?;
    only(!windows && o.activate, "--activate", "windows")?;
    let lines = command.is_line_input();
    let line_inputs = "stdin, file or command input";
    only(!lines && o.delimiter.is_some(), "--delimiter", line_inputs)?;
    only(!lines && o.input_format == InputFormat::Csv, "--input-format csv", line_inputs)?;
    only(!lines && o.unique, "--unique", line_inputs)?;
    only(!lines && o.reverse, "--reverse", line_inputs)?;
    only(!lines && o.line_continuation.is_some(), "--line-continuation", line_inputs)?;
    only(!lines && o.filter.is_some(), "--filter", line_inputs)?;
    only(!lines && o.select_1, "--select-1", line_inputs)?;
    only(!lines && o.exit_0, "--exit-0", line_inputs)?;
    let delimited = o.delimiter.is_some();
    let csv = o.input_format == InputFormat::Csv;
    if csv && delimited {
        return Err("--input-format csv uses --csv-delimiter, not --delimiter".into());
    }
    only(!delimited && o.text_field != Field::All, "--text-field", "--delimiter")?;
    only(!delimited && o.value_field != Field::All, "--value-field", "--delimiter")?;
    only(!delimited && o.preview_file_field.is_some(), "--preview-file-field", "--delimiter")?;
    only(
        !delimited && o.preview_center_line_field.is_some(),
        "--preview-center-line-field",
        "--delimiter",
    )?;
    only(
        o.preview_center_line_field.is_some() && o.preview_file_field.is_none(),
        "--preview-center-line-field",
        "--preview-file-field",
    )?;
    only(!csv && o.csv_columns.is_some(), "--csv-columns", "--input-format csv")?;
    only(!csv && o.csv_types.is_some(), "--csv-types", "--input-format csv")?;
    only(!csv && o.csv_formats.is_some(), "--csv-formats", "--input-format csv")?;
    only(!csv && o.csv_delimiter != b',', "--csv-delimiter", "--input-format csv")?;
    only(!csv && o.row_template.is_some(), "--row-template", "--input-format csv")?;
    only(o.row_icon.is_some() && o.row_template.is_none(), "--row-icon", "--row-template")?;
    if (delimited || csv) && (o.unique || o.reverse || o.line_continuation.is_some()) {
        return Err("--unique, --reverse and --line-continuation apply to plain lines, not --delimiter or csv input".into());
    }
    if o.preview_native == Some(true) && o.preview_file_field.is_none() && !filesystem {
        return Err("--preview-native true requires --preview-file-field (or the filesystem command)".into());
    }

    // Previews
    let fixed = preview_program.is_some();
    if !fixed && (!preview_args.is_empty() || preview_kind.is_some() || preview_cwd.is_some()) {
        return Err("--preview-arg, --preview-type and --preview-cwd require --preview".into());
    }
    if let Some(program) = preview_program {
        o.preview = Some(PreviewCommand {
            program: Program {
                program,
                args: preview_args,
                cwd: preview_cwd,
            },
            kind: preview_kind.unwrap_or_default(),
        });
    }
    if preview_resolver.is_none() && !preview_resolver_args.is_empty() {
        return Err("--preview-resolver-arg requires --preview-resolver".into());
    }
    for (name, profile) in profiles {
        let program = profile
            .program
            .ok_or_else(|| format!("preview profile '{name}' has no program; add --preview-command {name} PROGRAM"))?;
        o.preview_profiles.insert(
            name,
            PreviewCommand {
                program: Program {
                    program,
                    args: profile.args,
                    cwd: profile.cwd,
                },
                kind: profile.kind.unwrap_or_default(),
            },
        );
    }
    if let Some(name) = &o.preview_default
        && !o.preview_profiles.contains_key(name)
    {
        return Err(format!("--preview-default names an undefined preview profile '{name}'"));
    }
    if let Some(program) = preview_resolver {
        if fixed {
            return Err("--preview and --preview-resolver cannot be combined".into());
        }
        if o.preview_profiles.is_empty() {
            return Err("--preview-resolver requires at least one --preview-command profile".into());
        }
        o.preview_resolver = Some(Program {
            program,
            args: preview_resolver_args,
            cwd: None,
        });
    } else if !o.preview_profiles.is_empty() && !fixed {
        // Profiles without a resolver: the default (or the only) profile is the preview.
        let name = match &o.preview_default {
            Some(name) => name.clone(),
            None if o.preview_profiles.len() == 1 => o.preview_profiles.keys().next().unwrap().clone(),
            None => {
                return Err("several preview profiles need --preview-resolver or --preview-default".into());
            }
        };
        o.preview = o.preview_profiles.get(&name).cloned();
    }
    if windows && fixed && o.window_preview {
        return Err("--window-preview and --preview cannot be combined".into());
    }

    // Actions
    if accept_resolver.is_none() && !accept_resolver_args.is_empty() {
        return Err("--accept-resolver-arg requires --accept-resolver".into());
    }
    o.accept_resolver = accept_resolver.map(|program| Program {
        program,
        args: accept_resolver_args,
        cwd: None,
    });
    for (name, action) in actions {
        if !action.declared {
            return Err(format!(
                "action '{name}' is not declared; add --action {name} action-resolver"
            ));
        }
        let program = action
            .program
            .ok_or_else(|| format!("action '{name}' has no program; add --action-program {name} PROGRAM"))?;
        o.actions.insert(
            name,
            Program {
                program,
                args: action.args,
                cwd: None,
            },
        );
    }
    for (chord, action) in bindings {
        let parsed = keys::parse(&chord).map_err(|e| format!("--bind {chord}: {e}"))?;
        if !BUILTIN_ACTIONS.contains(&action.as_str()) && !o.actions.contains_key(&action) {
            return Err(format!("--bind {chord} names an undefined action '{action}'"));
        }
        if action == "parent" && !filesystem {
            return Err("the parent action is only valid with filesystem".into());
        }
        // A later binding for the same key replaces an earlier one.
        o.bindings.retain(|(existing, _)| existing != &parsed);
        o.bindings.push((parsed, action));
    }
    if o.filter.is_some() && (o.select_1 || o.exit_0) {
        return Err("--filter does not open a window; drop --select-1 and --exit-0".into());
    }
    Ok(options)
}

fn profile_name(flag: &str, name: String) -> Result<String, String> {
    if name.is_empty() || name.starts_with('-') {
        return Err(format!("{flag} requires a profile name before its value"));
    }
    Ok(name)
}
fn action_name(flag: &str, name: String) -> Result<String, String> {
    if name.is_empty() || name.starts_with('-') {
        return Err(format!("{flag} requires an action name before its value"));
    }
    if BUILTIN_ACTIONS.contains(&name.as_str()) || name.starts_with("__") {
        return Err(format!("{flag}: '{name}' is reserved"));
    }
    Ok(name)
}
pub fn parse_delimiter(flag: &str, value: &str) -> Result<char, String> {
    match value {
        "\\t" | "tab" => Ok('\t'),
        _ => {
            let mut chars = value.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) if c != '\n' && c != '\r' => Ok(c),
                _ => Err(format!("{flag} must be a single character or \\t, not '{value}'")),
            }
        }
    }
}
fn parse_field(flag: &str, value: &str) -> Result<Field, String> {
    if value == "all" {
        return Ok(Field::All);
    }
    parse_field_number(flag, value).map(Field::Index)
}
/// 1-based on the command line, 0-based inside.
fn parse_field_number(flag: &str, value: &str) -> Result<usize, String> {
    match value.parse::<usize>() {
        Ok(n) if n > 0 => Ok(n - 1),
        _ => Err(format!("{flag} requires a field number starting at 1, not '{value}'")),
    }
}
fn parse_number(flag: &str, value: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| format!("{flag} requires a non-negative integer, not '{value}'"))
}
fn parse_count(flag: &str, value: &str) -> Result<usize, String> {
    match parse_number(flag, value)? {
        n @ 1..=256 => Ok(n),
        _ => Err(format!("{flag} must be between 1 and 256")),
    }
}
fn parse_size(flag: &str, value: &str) -> Result<f32, String> {
    match value.parse::<f32>() {
        Ok(n) if n.is_finite() && n > 0.0 => Ok(n),
        _ => Err(format!("{flag} requires a positive number, not '{value}'")),
    }
}
fn parse_bool(flag: &str, value: &str) -> Result<bool, String> {
    match value {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(format!("{flag} must be true or false, not '{value}'")),
    }
}
fn parse_auto_bool(flag: &str, value: &str) -> Result<Option<bool>, String> {
    if value == "auto" {
        Ok(None)
    } else {
        parse_bool(flag, value).map(Some)
    }
}
fn parse_kind(flag: &str, value: &str) -> Result<PreviewKind, String> {
    match value {
        "text" | "ansi" => Ok(PreviewKind::Text),
        "image" => Ok(PreviewKind::Image),
        _ => Err(format!("{flag} must be text or image, not '{value}'")),
    }
}

pub const HELP: &str = "\
nfm - fuzzy picker

Usage:
  nfm [OPTIONS]                              Pick a line from standard input
  nfm file <PATH> [OPTIONS]                  Pick a line from a file
  nfm command [OPTIONS] -- PROGRAM [ARGS]    Pick a line of a program's output
  nfm filesystem [ROOT...] [OPTIONS]         Pick a file; Enter opens directories, Ctrl+U goes up
  nfm processes [OPTIONS]                    Pick a process; Ctrl+K kills, Ctrl+R refreshes
  nfm windows [OPTIONS]                      Pick a desktop window with a live preview

The selected value is printed to standard output.
Exit status: 0 selected, 1 no match, 2 error, 130 cancelled.

Search:
  -q, --query <TEXT>                  Initial query
      --sort <score|input>            Rank by match score (default) or keep input order
      --filter <QUERY>                Print the matches for QUERY without opening a window
      --select-1                      Print the only match without opening a window
      --exit-0                        Exit with status 1 without opening a window when nothing matches

Line input (stdin, file, command):
      --unique                        Drop repeated lines, keeping the first (the last with --reverse)
      --reverse                       Newest (last) line first
      --line-continuation <MARKER>    Join lines ending in MARKER into one candidate
      --delimiter <CHAR>              Split lines into fields (use \\t for tab)
      --text-field <all|N>            Field to display and search (1-based)
      --value-field <all|N>           Field to print when selected
      --preview-file-field <N>        Field holding a file path to preview
      --preview-center-line-field <N> Field holding the line to center the file preview on
      --input-format <lines|csv>      Read CSV records as a table; filter with /:size==2, sort with /!Descending==size
      --csv-columns <A,B,...>         Column names when the input has no header record
      --csv-delimiter <CHAR>          CSV field separator (default ,)
      --csv-types <NAME:TYPE,...>     Column types for sorting, filters and default formats: auto,
                                      text, number, bytes[=UNIT], duration or datetime
      --csv-formats <NAME:FORMAT,...> Show and search columns formatted, e.g. size:bytes,took:duration:
                                      date[=STRFTIME], duration, number[=N], bytes or percent[=N];
                                      typed columns default to their type's format. Sorting,
                                      filters and output keep the raw values
      --row-template <SPEC>           Show csv rows as cards: lines split by ;, slots by |,
                                      slots are COLUMN[:title|muted|accent|right|nohl], shown as
                                      the table shows them; COLUMN:icon[:devicon] paints an icon
      --row-icon <SLOT>               Icon beside each card title, e.g. path:devicon or a column
                                      of glyph:TEXT[@COLOR] references

Filesystem:
      --max-depth <N>                 Limit how deep the walk goes
      --files-only                    Only list files
      --directories-only              Only list directories

Preview:
      --preview <PROGRAM>             Run PROGRAM for the selected item
      --preview-arg <ARG>             Argument for --preview; {item} and {line} are replaced
      --preview-type <text|image>     text shows ANSI output (default); image decodes stdout
      --preview-cwd <DIR>             Working directory for --preview
      --preview-command <NAME> <PROGRAM>      Define a named preview profile
      --preview-command-arg <NAME> <ARG>      Argument for a profile
      --preview-command-type <NAME> <TYPE>    text or image
      --preview-command-cwd <NAME> <DIR>      Working directory for a profile
      --preview-default <NAME>        Profile used when the resolver prints nothing
      --preview-resolver <PROGRAM>    Prints the profile NAME to use for each item
      --preview-resolver-arg <ARG>    Argument for the resolver
      --preview-native <auto|true|false>  Built-in file preview for --preview-file-field (default auto)
      --preview-visible <true|false>  Show the preview initially (Ctrl+P toggles)
      --preview-wrap <true|false>     Wrap long text preview lines (default true; Alt+Z toggles)
      --preview-rows <N>              Preview height in rows when --height is not given
      --window-preview                Live window thumbnail (the default for windows)
  Preview programs also receive NFM_PREVIEW_ITEM and NFM_PREVIEW_LINE.

Keys and actions:
      --bind <KEY> <ACTION>           Run ACTION on KEY, e.g. --bind ctrl-o open
                                      Built-in actions: accept, cancel, parent (filesystem)
      --action <NAME> action-resolver Declare an action run by a program
      --action-program <NAME> <PROGRAM>
      --action-arg <NAME> <ARG>
      --accept-resolver <PROGRAM>     Resolve Enter through a program
      --accept-resolver-arg <ARG>
      --expect <KEY,KEY...>           Accept with these keys too and print the key first
  Action programs receive NFM_ACTION_STATE (JSON with selection, picker and query) and
  print {\"action\":\"none\"}, {\"action\":\"complete\"} or
  {\"action\":\"picker\",\"picker\":{\"kind\":\"filewalker\",\"roots\":[\"{item}\"]}}.
  Keys look like ctrl-o, alt-enter, ctrl-shift-k or f5.

Window:
      --layout <bottom|top>           Query below (default) or above the results
      --rows <N>                      Result rows (default 10)
      --scroll-padding <N>            Preview cursor scroll margin in rows
      --width <POINTS>                Window width (default 1000)
      --height <POINTS>               Window height (default from rows)
      --position <center|top|mouse>   Where the window opens, on the monitor under the mouse
      --title <TEXT>                  Window title
      --font <FILE>                   Monospace font file (.ttf/.otf)
      --font-size <POINTS>            Font size (default 14)
      --color <NAME=#RRGGBB,...>      Override colors: background, text, muted, selection,
                                      selection-text, preview-selection, match, divider,
                                      error, accent, success

Output:
      --print-query                   Print the query before the selection
      --output <plain|json>           Print the value, or a JSON object with the item
      --activate                      Switch to the selected window (windows)

Configuration:
      --config <FILE>                 Read defaults from FILE instead of the default config
      --no-config                     Do not read a config file
  Defaults come from %APPDATA%\\nfm\\config.toml (or NFM_CONFIG), then NFM_DEFAULT_OPTS,
  then the command line.

  -h, --help                          Show this help
      --version                       Show the version
      --debug-wait                    Print the process id and wait 20 seconds for a debugger
";

#[cfg(test)]
mod tests {
    use super::*;

    fn run(args: &[&str]) -> Result<Options, String> {
        match parse(&[Layer {
            origin: "command line".into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            positionals: true,
        }])? {
            Parsed::Run(options) => Ok(*options),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn commands_and_operands() {
        assert_eq!(run(&[]).unwrap().command, Command::Stdin);
        assert_eq!(run(&["file", "a.txt"]).unwrap().command, Command::File("a.txt".into()));
        assert_eq!(
            run(&["command", "--unique", "--", "rg", "--files"]).unwrap().command,
            Command::Run(vec!["rg".into(), "--files".into()])
        );
        assert_eq!(
            run(&["filesystem", "C:\\", "D:\\", "--max-depth", "2"]).unwrap().command,
            Command::FileSystem(vec!["C:\\".into(), "D:\\".into()])
        );
        assert!(run(&["processes", "x"]).is_err());
        assert!(run(&["file"]).is_err());
        assert!(run(&["command"]).is_err());
        assert!(run(&["bogus"]).is_err());
    }

    #[test]
    fn values_inline_and_separate() {
        let o = run(&["--query=abc", "--rows", "5", "-q", "x", "--delimiter", "\\t"]).unwrap();
        assert_eq!(o.query, "x");
        assert_eq!(o.rows, Some(5));
        assert_eq!(o.delimiter, Some('\t'));
        assert!(run(&["--rows"]).is_err());
        assert!(run(&["--unique=true"]).is_err());
        assert!(run(&["--nope"]).is_err());
    }

    #[test]
    fn preview_wrap_defaults_on() {
        assert!(run(&[]).unwrap().preview_wrap);
        assert!(!run(&["--preview-wrap", "false"]).unwrap().preview_wrap);
        assert!(run(&["--preview-wrap=true"]).unwrap().preview_wrap);
    }

    #[test]
    fn delimited_fields_are_one_based() {
        let o = run(&["--delimiter", ":", "--text-field", "2", "--value-field", "all"]).unwrap();
        assert_eq!(o.text_field, Field::Index(1));
        assert_eq!(o.value_field, Field::All);
        assert!(run(&["--delimiter", ":", "--text-field", "0"]).is_err());
        assert!(run(&["--text-field", "2"]).is_err());
        assert!(run(&["--delimiter", "ab"]).is_err());
    }

    #[test]
    fn mode_specific_flags_are_rejected_elsewhere() {
        assert!(run(&["--max-depth", "2"]).is_err());
        assert!(run(&["filesystem", "--files-only", "--directories-only"]).is_err());
        assert!(run(&["--window-preview"]).is_err());
        assert!(run(&["windows", "--window-preview"]).is_ok());
        assert!(run(&["processes", "--unique"]).is_err());
        assert!(run(&["--input-format", "csv", "--delimiter", ","]).is_err());
        assert!(run(&["--csv-columns", "a,b"]).is_err());
        assert!(run(&["--input-format", "csv", "--csv-columns", "a,b"]).is_ok());
        assert!(run(&["--csv-types", "a:number"]).is_err());
        assert!(run(&["--input-format", "csv", "--csv-types", "a:number"]).is_ok());
        assert!(run(&["--csv-formats", "a:bytes"]).is_err());
        assert!(run(&["--input-format", "csv", "--csv-formats", "a:bytes"]).is_ok());
        assert!(run(&["--row-template", "a"]).is_err());
        assert!(run(&["--input-format", "csv", "--row-icon", "a:devicon"]).is_err());
        assert!(
            run(&["--input-format", "csv", "--row-template", "a;b:muted", "--row-icon", "a:devicon"])
                .is_ok()
        );
    }

    #[test]
    fn preview_profiles_resolve_in_any_order() {
        let o = run(&[
            "--preview-command-arg", "code", "{item}",
            "--preview-command", "code", "bat",
            "--preview-command", "img", "magick",
            "--preview-command-type", "img", "image",
            "--preview-resolver", "pick.exe",
            "--preview-default", "code",
        ])
        .unwrap();
        assert_eq!(o.preview_profiles["code"].program.args, ["{item}"]);
        assert_eq!(o.preview_profiles["img"].kind, PreviewKind::Image);
        assert_eq!(o.preview_resolver.unwrap().program, "pick.exe");
        assert!(o.preview.is_none());
        assert!(run(&["--preview-command-arg", "x", "y"]).is_err());
        assert!(run(&["--preview-default", "missing"]).is_err());
        assert!(run(&["--preview-resolver", "r"]).is_err());
        assert!(run(&["--preview-arg", "x"]).is_err());
        // A lone profile is the preview.
        let o = run(&["--preview-command", "only", "cat"]).unwrap();
        assert_eq!(o.preview.unwrap().program.program, "cat");
    }

    #[test]
    fn actions_and_bindings_are_validated() {
        let o = run(&[
            "--bind", "ctrl-o", "open",
            "--action", "open", "action-resolver",
            "--action-program", "open", "open.exe",
            "--action-arg", "open", "--flag",
            "--bind", "ctrl-o", "accept",
        ])
        .unwrap();
        assert_eq!(o.actions["open"].args, ["--flag"]);
        assert_eq!(o.bindings.len(), 1);
        assert_eq!(o.bindings[0].1, "accept");
        assert!(run(&["--bind", "ctrl-o", "open"]).is_err());
        assert!(run(&["--action", "open", "action-resolver"]).is_err());
        assert!(run(&["--action-program", "open", "x"]).is_err());
        assert!(run(&["--action", "open", "script"]).is_err());
        assert!(run(&["--bind", "ctrl-u", "parent"]).is_err());
        assert!(run(&["filesystem", "--bind", "ctrl-u", "parent"]).is_ok());
        assert!(run(&["--bind", "hyper-x", "accept"]).is_err());
    }

    #[test]
    fn later_layers_override_earlier_ones() {
        let parsed = parse(&[
            Layer {
                origin: "config".into(),
                args: vec!["--rows".into(), "4".into(), "--layout".into(), "top".into()],
                positionals: false,
            },
            Layer {
                origin: "command line".into(),
                args: vec!["--rows".into(), "9".into()],
                positionals: true,
            },
        ])
        .unwrap();
        let Parsed::Run(o) = parsed else { panic!() };
        assert_eq!(o.rows, Some(9));
        assert!(o.layout_top);
        let error = parse(&[Layer {
            origin: "NFM_DEFAULT_OPTS".into(),
            args: vec!["windows".into()],
            positionals: false,
        }])
        .unwrap_err();
        assert!(error.starts_with("NFM_DEFAULT_OPTS:"), "{error}");
    }

    #[test]
    fn help_and_version() {
        let layer = |args: &[&str]| Layer {
            origin: "command line".into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            positionals: true,
        };
        assert_eq!(parse(&[layer(&["-h"])]).unwrap(), Parsed::Help);
        assert_eq!(parse(&[layer(&["--rows", "2", "--help"])]).unwrap(), Parsed::Help);
        assert_eq!(parse(&[layer(&["--version"])]).unwrap(), Parsed::Version);
    }
}
