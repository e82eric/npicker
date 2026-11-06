//! Preview plans for the selected item: a fixed command, a command profile
//! chosen by a resolver program, or the built-in file preview.
use crate::options::{Options, PreviewCommand, PreviewKind, Program};
use nfm_egui::{Cancellation, PreviewProvider, Selection};
use nfm_preview::{CommandOutput, CommandSpec, PreviewPlan, ResolvedPreviewProvider};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

const RESOLVER_TIMEOUT: Duration = Duration::from_secs(2);
const RESOLVER_OUTPUT: usize = 64 * 1024;

/// What a preview is about: `{item}` and `{line}` in arguments, and a file for
/// the built-in preview.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Target {
    pub item: String,
    pub line: Option<usize>,
    pub file: Option<String>,
}

#[derive(Clone)]
pub struct PreviewSpec {
    fixed: Option<PreviewCommand>,
    resolver: Option<Program>,
    profiles: BTreeMap<String, PreviewCommand>,
    default: Option<String>,
    native: bool,
}
impl PreviewSpec {
    pub fn new(options: &Options) -> Self {
        Self {
            fixed: options.preview.clone(),
            resolver: options.preview_resolver.clone(),
            profiles: options.preview_profiles.clone(),
            default: options.preview_default.clone(),
            native: options
                .preview_native
                .unwrap_or(options.preview_file_field.is_some()),
        }
    }
    /// A command replaces a picker's own preview.
    pub fn has_command(&self) -> bool {
        self.fixed.is_some() || self.resolver.is_some()
    }
    pub fn is_empty(&self) -> bool {
        !self.has_command() && !self.native
    }

    pub fn provider<T: Clone + Send + Sync + 'static>(
        &self,
        target: impl Fn(&Selection<T>) -> Option<Target> + Send + Sync + 'static,
    ) -> Arc<dyn PreviewProvider<T>> {
        let spec = self.clone();
        let files = nfm_preview::resolver(nfm_preview::MediaPreviewSettings::default());
        Arc::new(ResolvedPreviewProvider::new(
            move |selection: &Selection<T>, cancel: Cancellation| {
                let Some(target) = target(selection) else {
                    return Ok(None);
                };
                spec.plan(&target, &cancel, |path, cancel| files(path, cancel))
            },
        ))
    }

    fn plan(
        &self,
        target: &Target,
        cancel: &Cancellation,
        files: impl FnOnce(&Path, Cancellation) -> Result<Option<PreviewPlan>, String>,
    ) -> Result<Option<PreviewPlan>, String> {
        let command = match &self.resolver {
            Some(resolver) => self.resolve_profile(resolver, target, cancel)?,
            None => self.fixed.as_ref(),
        };
        if let Some(command) = command {
            return Ok(Some(command_plan(command, target)));
        }
        match (&target.file, self.native) {
            (Some(file), true) => Ok(files(Path::new(file), cancel.clone())?.map(|plan| match plan {
                PreviewPlan::Text {
                    mut request,
                    options,
                } => {
                    let line = target.line.filter(|&line| line > 0);
                    request.center_line = line;
                    request.highlight_line = line;
                    PreviewPlan::Text { request, options }
                }
                plan => plan,
            })),
            _ => Ok(None),
        }
    }

    /// Run the resolver; its first non-empty output line names a profile.
    fn resolve_profile(
        &self,
        resolver: &Program,
        target: &Target,
        cancel: &Cancellation,
    ) -> Result<Option<&PreviewCommand>, String> {
        let mut spec = command_spec(resolver, target);
        spec.output_limit = RESOLVER_OUTPUT;
        let started = Instant::now();
        let output = nfm_preview_file::resolved::run(
            &spec,
            || cancel.is_cancelled() || started.elapsed() > RESOLVER_TIMEOUT,
            |_, _| true,
        )
        .map_err(|error| {
            if started.elapsed() > RESOLVER_TIMEOUT {
                "preview resolver timed out".to_owned()
            } else {
                error
            }
        })?;
        if output.truncated {
            return Err("preview resolver output is too large".into());
        }
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(if message.is_empty() {
                format!("preview resolver exited with {}", output.status)
            } else {
                message
            });
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        let name = stdout
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned)
            .or_else(|| self.default.clone());
        match name {
            None => Ok(None),
            Some(name) => self
                .profiles
                .get(&name)
                .map(Some)
                .ok_or_else(|| format!("preview resolver chose an undefined profile '{name}'")),
        }
    }
}

fn substitute(arg: &str, target: &Target) -> String {
    let line = target.line.map(|l| l.to_string()).unwrap_or_default();
    arg.replace("{item}", &target.item).replace("{line}", &line)
}

fn command_spec(program: &Program, target: &Target) -> CommandSpec {
    let mut spec = CommandSpec::new(&program.program);
    spec.arguments = program
        .args
        .iter()
        .map(|arg| substitute(arg, target))
        .collect();
    spec.working_directory = program.cwd.clone();
    spec.environment = vec![
        ("NFM_PREVIEW_ITEM".into(), target.item.clone().into()),
        (
            "NFM_PREVIEW_LINE".into(),
            target.line.map(|l| l.to_string()).unwrap_or_default().into(),
        ),
    ];
    spec
}

fn command_plan(command: &PreviewCommand, target: &Target) -> PreviewPlan {
    let mut spec = command_spec(&command.program, target);
    let (output, center_line) = match command.kind {
        PreviewKind::Text => (CommandOutput::Ansi, target.line.filter(|&l| l > 0)),
        PreviewKind::Image => {
            spec.output_limit = 64 * 1024 * 1024;
            (CommandOutput::Image, None)
        }
    };
    let success_exit_codes = if nfm_picker_sources::command::no_match_exit(&command.program.program) {
        vec![0, 1]
    } else {
        vec![0]
    };
    PreviewPlan::Command {
        command: spec,
        output,
        center_line,
        max_scrollback_lines: 8192,
        success_exit_codes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> Target {
        Target {
            item: "src/main.rs".into(),
            line: Some(12),
            file: Some("src/main.rs".into()),
        }
    }

    #[test]
    fn placeholders_and_environment() {
        let program = Program {
            program: "bat".into(),
            args: vec!["--highlight-line={line}".into(), "{item}".into()],
            cwd: None,
        };
        let spec = command_spec(&program, &target());
        assert_eq!(spec.arguments, ["--highlight-line=12", "src/main.rs"]);
        assert!(spec
            .environment
            .contains(&("NFM_PREVIEW_LINE".into(), "12".into())));
        let plan = command_plan(
            &PreviewCommand {
                program,
                kind: PreviewKind::Text,
            },
            &target(),
        );
        assert!(plan.validate().is_ok());
        assert!(matches!(plan, PreviewPlan::Command { center_line: Some(12), .. }));
    }

    #[test]
    fn fixed_commands_win_over_native_previews() {
        let options = Options {
            preview: Some(PreviewCommand {
                program: Program {
                    program: "type".into(),
                    ..Default::default()
                },
                kind: PreviewKind::Image,
            }),
            preview_native: Some(true),
            ..Options::default()
        };
        let spec = PreviewSpec::new(&options);
        let plan = spec
            .plan(&target(), &Cancellation::never(), |_, _| panic!("native preview"))
            .unwrap();
        assert!(matches!(plan, Some(PreviewPlan::Command { output: CommandOutput::Image, .. })));

        let spec = PreviewSpec::new(&Options {
            preview_native: Some(true),
            ..Options::default()
        });
        let mut called = false;
        let plan = spec
            .plan(&target(), &Cancellation::never(), |path, _| {
                assert_eq!(path, Path::new("src/main.rs"));
                called = true;
                Ok(None)
            });
        assert!(plan.unwrap().is_none());
        assert!(called);
    }

    #[cfg(windows)]
    #[test]
    fn resolver_chooses_a_profile_or_the_default() {
        let profile = |program: &str| PreviewCommand {
            program: Program {
                program: program.into(),
                ..Default::default()
            },
            kind: PreviewKind::Text,
        };
        let spec = |script: &str| PreviewSpec {
            fixed: None,
            resolver: Some(Program {
                program: "cmd.exe".into(),
                args: vec!["/C".into(), script.into()],
                cwd: None,
            }),
            profiles: [("code".to_owned(), profile("bat")), ("other".to_owned(), profile("cat"))]
                .into(),
            default: Some("other".into()),
            native: false,
        };
        let cancel = Cancellation::never();
        let chosen = |script: &str| {
            spec(script)
                .resolve_profile(spec(script).resolver.as_ref().unwrap(), &target(), &cancel)
                .map(|p| p.map(|p| p.program.program.clone()))
        };
        assert_eq!(chosen("echo code").unwrap().as_deref(), Some("bat"));
        assert_eq!(chosen("echo.").unwrap().as_deref(), Some("cat"));
        assert!(chosen("echo nope").unwrap_err().contains("nope"));
        assert!(chosen("echo boom 1>&2 & exit /b 3").unwrap_err().contains("boom"));
    }
}
