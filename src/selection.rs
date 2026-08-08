use std::collections::HashMap;

use serde::Serialize;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct SelectedItem {
    pub item: String,
    pub value: String,
    pub line: Option<usize>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub fields: HashMap<String, String>,
}
