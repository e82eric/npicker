//! Query commands: host commands typed in the picker's command mode, such as
//! `GrepClaudeSessions fn main`. Ctrl+; switches the query box between search
//! and command mode, as does typing `> ` or `: ` at the start of a search. In
//! command mode the results area lists the registered commands that match the
//! name typed, and the rest of the line is the command's argument, taken
//! literally. Leaving command mode brings back the search before it.

/// A command the host registers with [`crate::Picker::set_query_commands`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryCommand {
    /// Letters, digits and punctuation, without spaces.
    pub name: String,
    pub description: String,
    pub argument: Option<CommandArgument>,
}

/// A command's single argument: everything after the name and one space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandArgument {
    /// Shown as `<name>` in completions and as the placeholder.
    pub name: String,
    pub description: String,
    /// Enter does not run the command while a required argument is empty.
    pub required: bool,
}

/// Typed at the start of a search, either switches to command mode: `>` for
/// the search prompt, `:` for the command prompt.
pub(crate) const SHORTCUTS: [&str; 2] = ["> ", ": "];

/// The command line after a shortcut that starts `query`.
pub(crate) fn strip_shortcut(query: &str) -> Option<&str> {
    SHORTCUTS
        .iter()
        .find_map(|shortcut| query.strip_prefix(shortcut))
}

/// Whether `query` is a shortcut without its space yet.
pub(crate) fn shortcut_started(query: &str) -> bool {
    SHORTCUTS
        .iter()
        .any(|shortcut| shortcut.trim_end() == query)
}

/// A command line: `Name argument`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CommandLine<'a> {
    /// The name typed so far, from the start of the line.
    pub name: &'a str,
    /// Everything after the space that ends the name, when there is one.
    pub argument: Option<&'a str>,
}

pub(crate) fn parse(line: &str) -> CommandLine<'_> {
    match line.find(' ') {
        Some(end) => CommandLine {
            name: &line[..end],
            argument: Some(&line[end + 1..]),
        },
        None => CommandLine {
            name: line,
            argument: None,
        },
    }
}

impl QueryCommand {
    /// The command as completions show it: `GrepClaudeSessions <search text>`
    /// for a required argument, `[search text]` for an optional one.
    pub fn signature(&self) -> String {
        match &self.argument {
            Some(argument) if argument.required => format!("{} <{}>", self.name, argument.name),
            Some(argument) => format!("{} [{}]", self.name, argument.name),
            None => self.name.clone(),
        }
    }
}

/// What `line` runs on Enter, or why it does not.
pub(crate) fn resolve<'a>(
    commands: &'a [QueryCommand],
    line: &'a str,
) -> Result<(&'a QueryCommand, &'a str), String> {
    let line = parse(line);
    if line.name.is_empty() {
        return Err("Type a command name".into());
    }
    let Some(command) = commands.iter().find(|command| command.name == line.name) else {
        return Err(format!("Unknown command {}", line.name));
    };
    let argument = line.argument.unwrap_or("");
    match &command.argument {
        Some(spec) if spec.required && argument.is_empty() => {
            Err(format!("{} is required", spec.name))
        }
        _ => Ok((command, argument)),
    }
}

/// Commands whose names fuzzy-match `typed`, best first; all of them, in
/// registration order, before anything is typed.
pub(crate) fn matching<'a>(commands: &'a [QueryCommand], typed: &str) -> Vec<&'a QueryCommand> {
    if typed.is_empty() {
        return commands.iter().collect();
    }
    let snapshot = std::sync::Arc::new(nfm_search_core::store::FlatSnapshot::from_items(
        commands.iter().map(|command| (command.name.as_str(), ())),
    ));
    nfm_search_core::search::search(snapshot, &typed.to_lowercase(), || false)
        .map(|output| {
            output
                .results
                .into_iter()
                .filter_map(|result| commands.iter().find(|command| command.name == result.path))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grep() -> QueryCommand {
        QueryCommand {
            name: "GrepClaudeSessions".into(),
            description: "Search Claude transcripts".into(),
            argument: Some(CommandArgument {
                name: "search text".into(),
                description: "Literal text".into(),
                required: true,
            }),
        }
    }

    #[test]
    fn the_argument_is_the_rest_of_the_line_verbatim() {
        let line = parse(r#"Grep fn foo("bar")  "#);
        assert_eq!(line.name, "Grep");
        assert_eq!(line.argument, Some(r#"fn foo("bar")  "#));
        assert_eq!(parse("Gr").argument, None);
        assert_eq!(parse("").name, "");
    }

    #[test]
    fn resolving_checks_the_name_and_required_argument() {
        let commands = [grep()];
        assert_eq!(
            resolve(&commands, "GrepClaudeSessions main")
                .map(|(command, argument)| (command.name.as_str(), argument)),
            Ok(("GrepClaudeSessions", "main"))
        );
        assert_eq!(
            resolve(&commands, "GrepClaudeSessions ").err(),
            Some("search text is required".into())
        );
        assert_eq!(
            resolve(&commands, "Nope").err(),
            Some("Unknown command Nope".into())
        );
        assert_eq!(resolve(&commands, "").err(), Some("Type a command name".into()));
    }

    #[test]
    fn names_match_fuzzily_best_first() {
        let reload = QueryCommand {
            name: "Reload".into(),
            description: String::new(),
            argument: None,
        };
        let commands = [grep(), reload];
        let names = |typed| {
            matching(&commands, typed)
                .into_iter()
                .map(|command| command.name.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("gcs"), ["GrepClaudeSessions"]);
        assert_eq!(names(""), ["GrepClaudeSessions", "Reload"]);
        assert_eq!(names("rel")[0], "Reload");
        assert!(names("zz").is_empty());
        assert_eq!(commands[0].signature(), "GrepClaudeSessions <search text>");
    }
}
