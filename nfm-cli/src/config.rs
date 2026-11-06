//! Defaults from a TOML file and NFM_DEFAULT_OPTS, both turned into the same
//! flags as the command line so one parser validates everything.
//!
//! ```toml
//! layout = "top"            # --layout top
//! rows = 12                 # --rows 12
//! unique = true             # --unique
//! expect = ["ctrl-o"]       # --expect ctrl-o
//!
//! [colors]                  # --color accent=#ff8800
//! accent = "#ff8800"
//!
//! [profiles.code]           # --preview-command code bat ...
//! program = "bat"
//! args = ["--color=always", "{item}"]
//! type = "text"
//!
//! [actions.open]            # --action open action-resolver ...
//! program = "open.exe"
//! args = []
//!
//! [bind]                    # --bind ctrl-o open
//! ctrl-o = "open"
//! ```
use crate::options::{is_switch, Layer};
use std::path::{Path, PathBuf};

pub fn default_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("NFM_CONFIG") {
        return Some(path.into());
    }
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".config")))?;
    Some(base.join("nfm").join("config.toml"))
}

/// The option layers before the command line: the config file (unless
/// `--no-config`), then NFM_DEFAULT_OPTS.
pub fn layers(argv: &[String]) -> Result<Vec<Layer>, String> {
    let mut layers = Vec::new();
    // Arguments after `--` belong to the command's program, not nfm.
    let no_config = no_config_before_separator(argv);
    let explicit = explicit_config(argv)?;
    if !no_config {
        let path = explicit.clone().or_else(default_path);
        if let Some(path) = path {
            match std::fs::read_to_string(&path) {
                Ok(text) => layers.push(Layer {
                    origin: path.display().to_string(),
                    args: tokens(&text).map_err(|e| format!("{}: {e}", path.display()))?,
                    positionals: false,
                }),
                // A missing default config is normal; a missing named one is not.
                Err(error)
                    if explicit.is_none() && error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        }
    }
    if let Ok(text) = std::env::var("NFM_DEFAULT_OPTS")
        && !text.trim().is_empty()
    {
        let args = shlex::split(&text).ok_or("NFM_DEFAULT_OPTS has an unterminated quote")?;
        layers.push(Layer {
            origin: "NFM_DEFAULT_OPTS".into(),
            args,
            positionals: false,
        });
    }
    Ok(layers)
}

fn no_config_before_separator(argv: &[String]) -> bool {
    argv.iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--no-config")
}

fn explicit_config(argv: &[String]) -> Result<Option<PathBuf>, String> {
    let mut found = None;
    let mut args = argv.iter();
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if arg == "--config" {
            found = Some(args.next().ok_or("--config requires a path")?.into());
        } else if let Some(path) = arg.strip_prefix("--config=") {
            found = Some(path.into());
        }
    }
    Ok(found)
}

pub fn tokens(text: &str) -> Result<Vec<String>, String> {
    let table: toml::Table = text.parse().map_err(|e: toml::de::Error| e.to_string())?;
    let mut out = Vec::new();
    for (key, value) in &table {
        match key.as_str() {
            "colors" => {
                for (name, color) in section(key, value)? {
                    out.push("--color".into());
                    out.push(format!("{name}={}", string(&format!("colors.{name}"), color)?));
                }
            }
            "bind" => {
                for (chord, action) in section(key, value)? {
                    out.extend(["--bind".into(), chord.clone()]);
                    out.push(string(&format!("bind.{chord}"), action)?);
                }
            }
            "profiles" => {
                for (name, profile) in section(key, value)? {
                    let at = format!("profiles.{name}");
                    for (field, value) in section(&at, profile)? {
                        let at = format!("{at}.{field}");
                        match field.as_str() {
                            "program" => out.extend(["--preview-command".into(), name.clone(), string(&at, value)?]),
                            "type" => out.extend(["--preview-command-type".into(), name.clone(), string(&at, value)?]),
                            "cwd" => out.extend(["--preview-command-cwd".into(), name.clone(), string(&at, value)?]),
                            "args" => {
                                for arg in strings(&at, value)? {
                                    out.extend(["--preview-command-arg".into(), name.clone(), arg]);
                                }
                            }
                            _ => return Err(format!("unknown key '{at}'; expected program, args, type or cwd")),
                        }
                    }
                }
            }
            "actions" => {
                for (name, action) in section(key, value)? {
                    let at = format!("actions.{name}");
                    out.extend(["--action".into(), name.clone(), "action-resolver".into()]);
                    for (field, value) in section(&at, action)? {
                        let at = format!("{at}.{field}");
                        match field.as_str() {
                            "program" => out.extend(["--action-program".into(), name.clone(), string(&at, value)?]),
                            "args" => {
                                for arg in strings(&at, value)? {
                                    out.extend(["--action-arg".into(), name.clone(), arg]);
                                }
                            }
                            _ => return Err(format!("unknown key '{at}'; expected program or args")),
                        }
                    }
                }
            }
            "config" | "no-config" | "help" | "version" | "debug-wait" => {
                return Err(format!("'{key}' cannot be set in a config file"));
            }
            _ => flag(&mut out, key, value)?,
        }
    }
    Ok(out)
}

fn flag(out: &mut Vec<String>, key: &str, value: &toml::Value) -> Result<(), String> {
    let name = format!("--{key}");
    match value {
        toml::Value::Boolean(enabled) if is_switch(key) => {
            if *enabled {
                out.push(name);
            }
        }
        toml::Value::Array(items) => {
            for item in items {
                flag(out, key, item)?;
            }
        }
        toml::Value::Table(_) => return Err(format!("'{key}' must be a value, not a table")),
        _ if is_switch(key) => return Err(format!("'{key}' must be true or false")),
        toml::Value::String(s) => out.extend([name, s.clone()]),
        toml::Value::Integer(n) => out.extend([name, n.to_string()]),
        toml::Value::Float(n) => out.extend([name, n.to_string()]),
        toml::Value::Boolean(b) => out.extend([name, b.to_string()]),
        toml::Value::Datetime(d) => out.extend([name, d.to_string()]),
    }
    Ok(())
}

fn section<'a>(key: &str, value: &'a toml::Value) -> Result<&'a toml::Table, String> {
    value
        .as_table()
        .ok_or_else(|| format!("'{key}' must be a table"))
}
fn string(key: &str, value: &toml::Value) -> Result<String, String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("'{key}' must be a string"))
}
fn strings(key: &str, value: &toml::Value) -> Result<Vec<String>, String> {
    value
        .as_array()
        .ok_or_else(|| format!("'{key}' must be an array of strings"))?
        .iter()
        .map(|item| string(key, item))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::options::{parse, Parsed};

    #[test]
    fn no_config_after_separator_belongs_to_the_program() {
        let argv = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(no_config_before_separator(&argv(&["--no-config", "file", "x"])));
        assert!(!no_config_before_separator(&argv(&["command", "--", "tool", "--no-config"])));
    }

    #[test]
    fn config_becomes_flags() {
        let args = tokens(
            r##"
layout = "top"
rows = 12
unique = true
reverse = false
preview-visible = false
expect = ["ctrl-o", "alt-enter"]

[colors]
accent = "#ff8800"

[profiles.code]
program = "bat"
args = ["--color=always", "{item}"]

[actions.open]
program = "open.exe"

[bind]
ctrl-o = "open"
"##,
        )
        .unwrap();
        let joined = args.join(" ");
        for expected in [
            "--layout top",
            "--rows 12",
            "--unique",
            "--preview-visible false",
            "--expect ctrl-o --expect alt-enter",
            "--color accent=#ff8800",
            "--preview-command code bat",
            "--preview-command-arg code --color=always --preview-command-arg code {item}",
            "--action open action-resolver --action-program open open.exe",
            "--bind ctrl-o open",
        ] {
            assert!(joined.contains(expected), "{expected} missing from {joined}");
        }
        assert!(!joined.contains("--reverse"));
        let Parsed::Run(options) = parse(&[Layer {
            origin: "config".into(),
            args,
            positionals: false,
        }])
        .unwrap() else {
            panic!()
        };
        assert_eq!(options.rows, Some(12));
        assert_eq!(options.bindings[0].1, "open");
    }

    #[test]
    fn config_errors_name_the_key() {
        assert!(tokens("unique = \"yes\"").unwrap_err().contains("unique"));
        assert!(tokens("[profiles.x]\nbogus = 1").unwrap_err().contains("profiles.x.bogus"));
        assert!(tokens("config = \"x\"").is_err());
        assert!(tokens("rows = ").is_err());
    }

    #[test]
    fn explicit_config_is_found_before_the_separator() {
        let argv = |args: &[&str]| args.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            explicit_config(&argv(&["--config", "a.toml"])).unwrap(),
            Some("a.toml".into())
        );
        assert_eq!(explicit_config(&argv(&["--config=b.toml"])).unwrap(), Some("b.toml".into()));
        assert_eq!(explicit_config(&argv(&["command", "--", "--config", "x"])).unwrap(), None);
    }
}
