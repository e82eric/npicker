//! What the picker prints, and the exit status.
use crate::options::{Options, OutputFormat};
use std::io::Write;

pub const EXIT_SELECTED: i32 = 0;
pub const EXIT_NO_MATCH: i32 = 1;
pub const EXIT_ERROR: i32 = 2;
pub const EXIT_CANCELLED: i32 = 130;

/// A selected candidate in every output form.
#[derive(Clone, Debug, PartialEq)]
pub struct Chosen {
    /// What is printed: the value field, the joined record, the path...
    pub value: String,
    /// What was displayed and searched.
    pub text: String,
    pub index: usize,
    /// The item as JSON, also sent to action resolvers as the selection.
    pub item: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Selected {
        chosen: Option<Chosen>,
        /// The `--expect` key that accepted, if any.
        key: Option<String>,
        query: String,
    },
    Cancelled,
}

/// Print an outcome and return the exit status.
pub fn finish(outcome: &Outcome, options: &Options) -> i32 {
    let mut stdout = std::io::stdout().lock();
    let (text, code) = render(outcome, options);
    if stdout
        .write_all(text.as_bytes())
        .and_then(|_| stdout.flush())
        .is_err()
    {
        return EXIT_ERROR;
    }
    code
}

fn render(outcome: &Outcome, options: &Options) -> (String, i32) {
    match outcome {
        Outcome::Cancelled => (String::new(), EXIT_CANCELLED),
        Outcome::Selected { chosen, key, query } => {
            let code = if chosen.is_some() {
                EXIT_SELECTED
            } else {
                EXIT_NO_MATCH
            };
            let text = match options.output {
                OutputFormat::Json => {
                    let mut object = serde_json::Map::new();
                    object.insert("query".into(), query.clone().into());
                    object.insert("key".into(), key.clone().into());
                    if let Some(chosen) = chosen {
                        insert_chosen(&mut object, chosen);
                    }
                    format!("{}\n", serde_json::Value::Object(object))
                }
                OutputFormat::Plain => {
                    let mut text = String::new();
                    if options.print_query {
                        text.push_str(query);
                        text.push('\n');
                    }
                    if !options.expect.is_empty() {
                        text.push_str(key.as_deref().unwrap_or_default());
                        text.push('\n');
                    }
                    if let Some(chosen) = chosen {
                        text.push_str(&chosen.value);
                        text.push('\n');
                    }
                    text
                }
            };
            (text, code)
        }
    }
}

fn insert_chosen(object: &mut serde_json::Map<String, serde_json::Value>, chosen: &Chosen) {
    object.insert("value".into(), chosen.value.clone().into());
    object.insert("text".into(), chosen.text.clone().into());
    object.insert("index".into(), chosen.index.into());
    object.insert("item".into(), chosen.item.clone());
}

/// `--filter`: every match, one per line (one JSON object per line with
/// `--output json`).
pub fn print_matches(matches: &[Chosen], query: &str, options: &Options) -> i32 {
    let mut text = String::new();
    if options.print_query {
        text.push_str(query);
        text.push('\n');
    }
    for chosen in matches {
        match options.output {
            OutputFormat::Plain => text.push_str(&chosen.value),
            OutputFormat::Json => {
                let mut object = serde_json::Map::new();
                insert_chosen(&mut object, chosen);
                text.push_str(&serde_json::Value::Object(object).to_string());
            }
        }
        text.push('\n');
    }
    let mut stdout = std::io::stdout().lock();
    if stdout
        .write_all(text.as_bytes())
        .and_then(|_| stdout.flush())
        .is_err()
    {
        return EXIT_ERROR;
    }
    if matches.is_empty() {
        EXIT_NO_MATCH
    } else {
        EXIT_SELECTED
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys;

    fn chosen() -> Chosen {
        Chosen {
            value: "value".into(),
            text: "text".into(),
            index: 3,
            item: "value".into(),
        }
    }

    #[test]
    fn plain_output_with_query_and_key() {
        let mut options = Options::default();
        let selected = Outcome::Selected {
            chosen: Some(chosen()),
            key: None,
            query: "q".into(),
        };
        assert_eq!(render(&selected, &options), ("value\n".into(), 0));
        options.print_query = true;
        options.expect = vec![keys::parse("ctrl-o").unwrap()];
        assert_eq!(render(&selected, &options).0, "q\n\nvalue\n");
        let expected = Outcome::Selected {
            chosen: None,
            key: Some("ctrl-o".into()),
            query: "q".into(),
        };
        assert_eq!(render(&expected, &options), ("q\nctrl-o\n".into(), 1));
        assert_eq!(render(&Outcome::Cancelled, &options), (String::new(), 130));
    }

    #[test]
    fn json_output() {
        let options = Options {
            output: OutputFormat::Json,
            ..Options::default()
        };
        let (text, code) = render(
            &Outcome::Selected {
                chosen: Some(chosen()),
                key: Some("ctrl-o".into()),
                query: "q".into(),
            },
            &options,
        );
        assert_eq!(code, 0);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["value"], "value");
        assert_eq!(value["index"], 3);
        assert_eq!(value["key"], "ctrl-o");
        assert_eq!(value["query"], "q");
    }
}
