//! Action resolvers: programs that receive the picker state as JSON in
//! NFM_ACTION_STATE and answer on stdout with what the picker does next.
use crate::options::Program;
use nfm_preview::CommandSpec;
use serde::{Deserialize, Serialize};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const OUTPUT_LIMIT: usize = 64 * 1024;
const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Serialize)]
pub struct ActionState {
    pub selection: Option<serde_json::Value>,
    pub picker: PickerState,
    pub query: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PickerState {
    Filewalker { roots: Vec<String> },
    Stdin,
    DelimitedStdin,
    StructuredStdin,
    Processes,
    Windows,
    /// The `argument` input: the selection is the text typed.
    Argument,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Resolution {
    None,
    Complete,
    /// Switch to the filesystem picker at these roots.
    Filewalker(Vec<String>),
    /// Show another page on top of this one: an nfm command line.
    Push(Vec<String>),
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "lowercase")]
enum Response {
    None,
    Complete,
    Picker { picker: PickerResponse },
    Push { args: Vec<String> },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum PickerResponse {
    Filewalker { roots: Vec<String> },
}

/// Runs on a worker thread; `cancel` stops the program early.
pub fn run(
    program: &Program,
    state: &ActionState,
    selected_value: Option<&str>,
    cancel: &Arc<AtomicBool>,
) -> Result<Resolution, String> {
    let json = serde_json::to_string(state)
        .map_err(|e| format!("failed to serialize action state: {e}"))?;
    let mut spec = CommandSpec::new(&program.program);
    spec.arguments = program.args.clone();
    spec.working_directory = program.cwd.clone();
    spec.environment = vec![("NFM_ACTION_STATE".into(), json.into())];
    spec.output_limit = OUTPUT_LIMIT;
    let started = Instant::now();
    let output = nfm_preview_file::resolved::run(
        &spec,
        || cancel.load(Ordering::Acquire) || started.elapsed() > TIMEOUT,
        |_, _| true,
    )
    .map_err(|error| {
        if cancel.load(Ordering::Acquire) {
            "action resolver cancelled".to_owned()
        } else if started.elapsed() > TIMEOUT {
            "action resolver timed out".to_owned()
        } else {
            format!("action resolver failed: {error}")
        }
    })?;
    if output.truncated {
        return Err("action resolver output is too large".into());
    }
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if message.is_empty() {
            format!("action resolver exited with {}", output.status)
        } else {
            message
        });
    }
    parse_response(&output.stdout, selected_value)
}

fn parse_response(stdout: &[u8], selected_value: Option<&str>) -> Result<Resolution, String> {
    let response: Response = serde_json::from_slice(stdout)
        .map_err(|e| format!("invalid action resolver response: {e}"))?;
    match response {
        Response::None => Ok(Resolution::None),
        Response::Complete => Ok(Resolution::Complete),
        Response::Push { args } => Ok(Resolution::Push(args)),
        Response::Picker {
            picker: PickerResponse::Filewalker { roots },
        } => {
            if roots.is_empty() {
                return Err("filewalker picker requires at least one root".into());
            }
            roots
                .into_iter()
                .map(|root| {
                    if root == "{item}" {
                        selected_value
                            .map(str::to_owned)
                            .ok_or_else(|| "{item} root requires a selection".to_owned())
                    } else {
                        Ok(root)
                    }
                })
                .collect::<Result<_, _>>()
                .map(Resolution::Filewalker)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses() {
        assert_eq!(parse_response(br#"{"action":"none"}"#, None), Ok(Resolution::None));
        assert_eq!(parse_response(br#"{"action":"complete"}"#, None), Ok(Resolution::Complete));
        assert_eq!(
            parse_response(
                br#"{"action":"picker","picker":{"kind":"filewalker","roots":["{item}","D:\\"]}}"#,
                Some("C:\\src")
            ),
            Ok(Resolution::Filewalker(vec!["C:\\src".into(), "D:\\".into()]))
        );
        assert!(parse_response(br#"{"action":"picker","picker":{"kind":"filewalker","roots":[]}}"#, None).is_err());
        assert!(parse_response(br#"{"action":"picker","picker":{"kind":"filewalker","roots":["{item}"]}}"#, None).is_err());
        assert_eq!(
            parse_response(br#"{"action":"push","args":["command","--","rg","a b"]}"#, None),
            Ok(Resolution::Push(vec!["command".into(), "--".into(), "rg".into(), "a b".into()]))
        );
        assert!(parse_response(br#"{"action":"push"}"#, None).is_err());
        assert!(parse_response(b"nope", None).is_err());
    }

    #[test]
    fn state_json_matches_the_protocol() {
        let state = ActionState {
            selection: Some("a.txt".into()),
            picker: PickerState::Filewalker {
                roots: vec!["C:\\".into()],
            },
            query: "a".into(),
        };
        assert_eq!(
            serde_json::to_string(&state).unwrap(),
            r#"{"selection":"a.txt","picker":{"kind":"filewalker","roots":["C:\\"]},"query":"a"}"#
        );
        let state = ActionState {
            selection: None,
            picker: PickerState::DelimitedStdin,
            query: String::new(),
        };
        assert!(serde_json::to_string(&state).unwrap().contains(r#""kind":"delimitedstdin""#));
    }

    #[cfg(windows)]
    #[test]
    fn runs_a_resolver_with_the_state() {
        let script = std::env::temp_dir().join(format!("nfm-action-{}.cmd", std::process::id()));
        std::fs::write(
            &script,
            "@echo off\r\necho %NFM_ACTION_STATE% | findstr /C:needle >nul && echo {\"action\":\"complete\"}\r\n",
        )
        .unwrap();
        let program = Program {
            program: script.to_string_lossy().into_owned(),
            args: Vec::new(),
            cwd: None,
        };
        let state = ActionState {
            selection: Some("needle".into()),
            picker: PickerState::Stdin,
            query: String::new(),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let result = run(&program, &state, Some("needle"), &cancel);
        std::fs::remove_file(&script).unwrap();
        assert_eq!(result, Ok(Resolution::Complete));
        let failing = Program {
            program: "cmd.exe".into(),
            args: vec!["/C".into(), "echo nope 1>&2 & exit /b 2".into()],
            cwd: None,
        };
        assert_eq!(run(&failing, &state, None, &cancel), Err("nope".into()));
    }
}
