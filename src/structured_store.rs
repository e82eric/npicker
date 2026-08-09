use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::Range;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, NaiveDate, NaiveDateTime};
use nfm_search_core::store::{
    ChunkedSnapshot, ChunkedStorage, FlatSnapshot, ItemsSource, SearchPlan,
};
use regex::RegexBuilder;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructuredSchema {
    pub columns: Vec<String>,
}

impl StructuredSchema {
    pub fn new(columns: Vec<String>) -> Result<Self, String> {
        if columns.is_empty() {
            return Err("structured input requires at least one column".into());
        }
        let mut seen = HashMap::new();
        for (index, column) in columns.iter().enumerate() {
            if column.is_empty() {
                return Err(format!("column {} has an empty name", index + 1));
            }
            let normalized = column.to_lowercase();
            if seen.insert(normalized, index).is_some() {
                return Err(format!("duplicate column name: {column}"));
            }
        }
        Ok(Self { columns })
    }

    fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|column| column.eq_ignore_ascii_case(name))
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ByteRange {
    offset: usize,
    length: usize,
}

#[derive(Clone, Copy, Debug, Default)]
struct StructuredRow {
    display: ByteRange,
    value: ByteRange,
    first_cell: usize,
    cell_count: usize,
}

pub struct StructuredStreamingStore {
    schema: Arc<StructuredSchema>,
    rows: ChunkedStorage<StructuredRow>,
    cells: ChunkedStorage<ByteRange>,
    bytes: ChunkedStorage<u8>,
    distinct_seen: Vec<HashSet<String>>,
    distinct_values: Arc<RwLock<Vec<Vec<String>>>>,
    column_widths: Vec<usize>,
    version: u64,
}

impl StructuredStreamingStore {
    pub fn new(schema: StructuredSchema) -> Self {
        let column_count = schema.columns.len();
        Self {
            column_widths: schema
                .columns
                .iter()
                .map(|column| column.chars().count())
                .collect(),
            schema: Arc::new(schema),
            rows: ChunkedStorage::new(64 * 1024),
            cells: ChunkedStorage::new(64 * 1024),
            bytes: ChunkedStorage::new(1024 * 1024),
            distinct_seen: (0..column_count).map(|_| HashSet::new()).collect(),
            distinct_values: Arc::new(RwLock::new(vec![Vec::new(); column_count])),
            version: 0,
        }
    }

    pub fn add_record(&mut self, record: &csv::StringRecord) -> Result<(), String> {
        if record.len() != self.schema.columns.len() {
            return Err(
                format!("record has {} fields, expected {}",
                record.len(),
                self.schema.columns.len()
            ))
        }

        let first_cell = self.cells.len();
        for (column, value) in record.iter().enumerate() {
            self.column_widths[column] =
                self.column_widths[column].max(display_cell(value).chars().count());
            let range = ByteRange {
                offset: self.bytes.len(),
                length: value.len(),
            };
            self.bytes.extend_from_slice(value.as_bytes());
            self.cells.push(range);
            if self.distinct_seen[column].len() < 1_000 {
                let normalized = value.to_lowercase();
                if self.distinct_seen[column].insert(normalized) {
                    self.distinct_values
                        .write()
                        .expect("structured suggestions poisoned")[column]
                        .push(value.to_owned());
                }
            }
        }

        let value = encode_csv_record(record.iter());
        let value_range = ByteRange {
            offset: self.bytes.len(),
            length: value.len(),
        };
        self.bytes.extend_from_slice(&value);
        let display_values: Vec<_> = record
            .iter()
            .map(|value| value.replace('\r', "\\r").replace('\n', "\\n"))
            .collect();
        let display = encode_csv_record(display_values.iter().map(String::as_str));
        let display_range = ByteRange {
            offset: self.bytes.len(),
            length: display.len(),
        };
        self.bytes.extend_from_slice(&display);
        self.rows.push(StructuredRow {
            display: display_range,
            value: value_range,
            first_cell,
            cell_count: record.len(),
        });

        Ok(())
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn snapshot(&mut self) -> Arc<StructuredStreamingSnapshot> {
        self.version += 1;
        Arc::new(StructuredStreamingSnapshot {
            schema: Arc::clone(&self.schema),
            rows: self.rows.snapshot(),
            row_count: self.rows.len(),
            cells: self.cells.snapshot(),
            bytes: self.bytes.snapshot(),
            version: self.version,
            distinct_values: Arc::clone(&self.distinct_values),
            column_widths: Arc::from(self.column_widths.clone()),
        })
    }
}

pub struct StructuredStreamingSnapshot {
    schema: Arc<StructuredSchema>,
    rows: ChunkedSnapshot<StructuredRow>,
    row_count: usize,
    cells: ChunkedSnapshot<ByteRange>,
    bytes: ChunkedSnapshot<u8>,
    version: u64,
    distinct_values: Arc<RwLock<Vec<Vec<String>>>>,
    column_widths: Arc<[usize]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletionSuggestion {
    pub text: String,
    pub positions: Vec<usize>,
    pub replacement: String,
    pub replace: Range<usize>,
}

impl StructuredStreamingSnapshot {
    pub fn schema(&self) -> &StructuredSchema {
        &self.schema
    }

    pub fn value(&self, row: usize) -> String {
        let mut stack = [0; 4096];
        let mut heap = Vec::new();
        String::from_utf8_lossy(self.range(self.rows[row].value, &mut stack, &mut heap))
            .into_owned()
    }

    pub fn fields(&self, row: usize) -> HashMap<String, String> {
        self.schema
            .columns
            .iter()
            .enumerate()
            .map(|(column_index, column)| {
                (
                    column.clone(),
                    self.cell_string(row, column_index)
                )
            }).collect()
    }

    pub fn header(&self, query: &str) -> String {
        let columns = selected_display_columns(&self.schema, query);
        format_columns(
            columns
                .iter()
                .map(|&column| self.schema.columns[column].as_str()),
            &columns,
            &self.column_widths,
        )
    }

    pub fn display_row(&self, row: usize, query: &str) -> String {
        let columns = selected_display_columns(&self.schema, query);
        self.format_row(row, &columns)
    }

    fn format_row(&self, row: usize, columns: &[usize]) -> String {
        let values: Vec<_> = columns
            .iter()
            .map(|&column| self.cell_string(row, column))
            .collect();
        format_columns(
            values.iter().map(String::as_str),
            columns,
            &self.column_widths,
        )
    }

    fn range<'a>(
        &'a self,
        range: ByteRange,
        stack: &'a mut [u8],
        heap: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        self.bytes
            .get_range(range.offset, range.length, stack, heap)
    }

    fn cell_string(&self, row: usize, column: usize) -> String {
        let mut stack = [0; 4096];
        let mut heap = Vec::new();
        self
            .cell_bytes(row, column, &mut stack, &mut heap)
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default()
    }

    fn cell_bytes<'a>(
        &'a self,
        row_index: usize,
        column: usize,
        stack: &'a mut [u8],
        heap: &'a mut Vec<u8>,
    ) -> Option<&'a [u8]> {
        let row = self.rows[row_index];
        if column >= row.cell_count {
            return None;
        }
        Some(self.range(self.cells[row.first_cell + column], stack, heap))
    }

    pub fn completions(&self, input: &str, cursor: usize) -> Vec<CompletionSuggestion> {
        if cursor > input.len() || !input.is_char_boundary(cursor) {
            return Vec::new();
        }
        let start = input[..cursor]
            .char_indices()
            .rev()
            .find(|(_, ch)| ch.is_whitespace())
            .map_or(0, |(index, ch)| index + ch.len_utf8());
        let token = &input[start..cursor];
        if !token.starts_with('/') {
            return Vec::new();
        }
        if token == "/" {
            return [("/:", "Filter"), ("/!", "Sort"), ("/#", "Display columns")]
                .into_iter()
                .map(|(replacement, description)| CompletionSuggestion {
                    text: format!("{replacement}   {description}"),
                    positions: Vec::new(),
                    replacement: if replacement == "/#" {
                        "/#Display==".into()
                    } else {
                        replacement.into()
                    },
                    replace: start..cursor,
                })
                .collect();
        }
        let Some(action) = token.as_bytes().get(1).copied() else {
            return Vec::new();
        };
        if !matches!(action, b':' | b'!' | b'#') {
            return Vec::new();
        }
        let body = &token[2..];
        let operator = find_operator_or_prefix(body);
        match operator {
            OperatorPosition::None => {
                if action == b'#' {
                    return suggestions_for(
                        self.schema.columns.clone(),
                        body,
                        start..cursor,
                        |value| format!("/#Display=={},", quote_if_needed(value)),
                    );
                }
                let values: Vec<String> = if action == b'!' {
                    vec!["Ascending".into(), "Descending".into()]
                } else {
                    self.schema.columns.clone()
                };
                suggestions_for(values, body, start..cursor, |value| {
                    let postfix = if action == b'!' { "==" } else { "=" };
                    format!("/{}{}{}", action as char, quote_if_needed(value), postfix)
                })
            }
            OperatorPosition::Partial(position) => {
                let before = &body[..position];
                suggestions_for(
                    ["==", "!=", "=~", "!~", ">=", "<=", ">", "<"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                    "",
                    start..cursor,
                    |value| format!("/{}{}{}", action as char, before, value),
                )
            }
            OperatorPosition::Complete(position, operator) => {
                let column_or_direction = unquote(&body[..position]);
                let value_prefix = &body[position + operator.len()..];
                if action == b'#' {
                    let focused_start =
                        last_unquoted_comma(value_prefix).map_or(0, |position| position + 1);
                    let existing = &value_prefix[..focused_start];
                    let selected: Vec<_> = split_expression_values(existing.trim_end_matches(','))
                        .into_iter()
                        .map(|value| unquote(&value))
                        .filter(|value| !value.is_empty())
                        .collect();
                    let available = self
                        .schema
                        .columns
                        .iter()
                        .filter(|column| {
                            !selected
                                .iter()
                                .any(|value| value.eq_ignore_ascii_case(column))
                        })
                        .cloned()
                        .collect();
                    return suggestions_for(
                        available,
                        &value_prefix[focused_start..],
                        start..cursor,
                        |value| {
                            format!(
                                "/#{}{}{}{},",
                                quote_if_needed(&column_or_direction),
                                operator,
                                existing,
                                quote_if_needed(value)
                            )
                        },
                    );
                }
                let values = if action == b':' {
                    self.schema
                        .column_index(&column_or_direction)
                        .map(|column| self.distinct_values(column, 1_000))
                        .unwrap_or_default()
                } else {
                    self.schema.columns.clone()
                };
                suggestions_for(values, value_prefix, start..cursor, |value| {
                    let postfix = if action == b'#' { "," } else { " " };
                    format!(
                        "/{}{}{}{}{}",
                        action as char,
                        quote_if_needed(&column_or_direction),
                        operator,
                        quote_if_needed(value),
                        postfix,
                    )
                })
            }
        }
    }

    fn distinct_values(&self, column: usize, limit: usize) -> Vec<String> {
        let distinct = self
            .distinct_values
            .read()
            .expect("structured suggestions poisoned");
        let mut values: Vec<_> = distinct
            .get(column)
            .into_iter()
            .flatten()
            .filter(|value| !value.is_empty())
            .take(limit)
            .cloned()
            .collect();
        values.sort_by_key(|value| value.to_lowercase());
        values
    }
}

fn encode_csv_record<'a>(values: impl IntoIterator<Item = &'a str>) -> Vec<u8> {
    let mut writer = csv::WriterBuilder::new()
        .has_headers(false)
        .terminator(csv::Terminator::Any(b'\n'))
        .from_writer(Vec::new());
    writer
        .write_record(values)
        .expect("writing CSV to memory failed");
    let mut bytes = writer.into_inner().expect("flushing CSV to memory failed");
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    bytes
}

fn display_cell(value: &str) -> String {
    value.replace('\r', "\\r").replace('\n', "\\n")
}

fn selected_display_columns(schema: &StructuredSchema, query: &str) -> Vec<usize> {
    let parsed = parse_structured_query(query);
    let selected: Vec<_> = parsed
        .display_columns
        .into_iter()
        .filter_map(|column| schema.column_index(&column))
        .collect();
    if selected.is_empty() {
        (0..schema.columns.len()).collect()
    } else {
        selected
    }
}

fn format_columns<'a>(
    values: impl IntoIterator<Item = &'a str>,
    columns: &[usize],
    widths: &[usize],
) -> String {
    let values: Vec<_> = values.into_iter().collect();
    let mut output = String::new();
    for (position, (&column, value)) in columns.iter().zip(values).enumerate() {
        let value = display_cell(value);
        output.push_str(&value);
        if position + 1 < columns.len() {
            output.extend(std::iter::repeat_n(
                ' ',
                widths[column].saturating_sub(value.chars().count()) + 2,
            ));
        }
    }
    output
}

enum OperatorPosition<'a> {
    None,
    Partial(usize),
    Complete(usize, &'a str),
}

fn find_operator_or_prefix(body: &str) -> OperatorPosition<'_> {
    for (index, ch) in body.char_indices() {
        if matches!(ch, '=' | '!' | '<' | '>') {
            let suffix = &body[index..];
            for operator in ["==", "!=", "=~", "!~", ">=", "<=", ">", "<"] {
                if suffix.starts_with(operator) {
                    return OperatorPosition::Complete(index, operator);
                }
            }
            return OperatorPosition::Partial(index);
        }
    }
    OperatorPosition::None
}

fn suggestions_for(
    values: Vec<String>,
    prefix: &str,
    replace: Range<usize>,
    replacement: impl Fn(&str) -> String,
) -> Vec<CompletionSuggestion> {
    let prefix = unquote(prefix).to_lowercase();
    let values = if prefix.is_empty() {
        values
    } else {
        let snapshot = Arc::new(FlatSnapshot::from_items(
            values.iter().map(|value| (value.as_str(), ())),
        ));
        nfm_search_core::search::search(snapshot, &prefix, || false)
            .map(|output| {
                output
                    .results
                    .into_iter()
                    .map(|result| result.path)
                    .collect()
            })
            .unwrap_or_default()
    };
    values
        .into_iter()
        .take(50)
        .map(|text| CompletionSuggestion {
            positions: if prefix.is_empty() {
                Vec::new()
            } else {
                nfm_search_core::search::resolve_match_positions(&prefix, &text)
            },
            replacement: replacement(&text),
            text,
            replace: replace.clone(),
        })
        .collect()
}

fn quote_if_needed(value: &str) -> String {
    if value
        .chars()
        .any(|ch| ch.is_whitespace() || matches!(ch, ',' | '"'))
    {
        format!("\"{}\"", value.replace('"', "\\\""))
    } else {
        value.to_owned()
    }
}

impl ItemsSource for StructuredStreamingSnapshot {
    fn version(&self) -> u64 {
        self.version
    }
    fn len(&self) -> usize {
        self.row_count
    }
    fn is_empty(&self) -> bool {
        self.row_count == 0
    }

    fn get_string<'a>(
        &'a self,
        index: usize,
        stack: &'a mut [u8],
        heap: &'a mut Vec<u8>,
    ) -> &'a [u8] {
        self.range(self.rows[index].display, stack, heap)
    }

    fn get_string_lossy(&self, index: usize, out: &mut Vec<u8>) -> String {
        let mut stack = [0; 4096];
        String::from_utf8_lossy(self.get_string(index, &mut stack, out)).into_owned()
    }

    fn create_search_plan(&self, query: &str) -> Box<dyn SearchPlan + '_> {
        Box::new(StructuredSearchPlan::compile(self, query))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Operator {
    Equals,
    NotEquals,
    Regex,
    NotRegex,
    Greater,
    GreaterEqual,
    Less,
    LessEqual,
}

struct Filter {
    column: usize,
    operator: Operator,
    values: Vec<String>,
    regexes: Vec<Option<regex::Regex>>,
}

#[derive(Clone, Copy)]
struct Sort {
    column: usize,
    descending: bool,
}

struct StructuredSearchPlan<'a> {
    snapshot: &'a StructuredStreamingSnapshot,
    fuzzy_query: String,
    filters: Vec<Filter>,
    sorts: Vec<Sort>,
    invalid_filter: bool,
    display_columns: Vec<usize>,
}

impl<'a> StructuredSearchPlan<'a> {
    fn compile(snapshot: &'a StructuredStreamingSnapshot, query: &str) -> Self {
        let parsed = parse_structured_query(query);
        let mut invalid_filter = false;
        let filters = parsed
            .filters
            .into_iter()
            .filter_map(|filter| {
                let Some(column) = snapshot.schema.column_index(&filter.column) else {
                    invalid_filter = true;
                    return None;
                };
                let regexes = filter
                    .values
                    .iter()
                    .map(|value| {
                        matches!(filter.operator, Operator::Regex | Operator::NotRegex)
                            .then(|| RegexBuilder::new(value).case_insensitive(true).build().ok())
                            .flatten()
                    })
                    .collect();
                Some(Filter {
                    column,
                    operator: filter.operator,
                    values: filter.values,
                    regexes,
                })
            })
            .collect();
        let sorts = parsed
            .sorts
            .into_iter()
            .filter_map(|sort| {
                snapshot
                    .schema
                    .column_index(&sort.column)
                    .map(|column| Sort {
                        column,
                        descending: sort.descending,
                    })
            })
            .collect();
        let display_columns = if parsed.display_columns.is_empty() {
            (0..snapshot.schema.columns.len()).collect()
        } else {
            parsed
                .display_columns
                .into_iter()
                .filter_map(|column| snapshot.schema.column_index(&column))
                .collect()
        };
        Self {
            snapshot,
            fuzzy_query: parsed.search,
            filters,
            sorts,
            invalid_filter,
            display_columns,
        }
    }
}

impl SearchPlan for StructuredSearchPlan<'_> {
    fn fuzzy_query(&self) -> &str {
        &self.fuzzy_query
    }

    fn filters_items(&self) -> bool {
        self.invalid_filter || !self.filters.is_empty()
    }

    fn includes(&self, index: usize) -> bool {
        if self.invalid_filter {
            return false;
        }
        self.filters.iter().all(|filter| {
            let mut stack = [0; 4096];
            let mut heap = Vec::new();
            let cell = self
                .snapshot
                .cell_bytes(index, filter.column, &mut stack, &mut heap)
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .unwrap_or_default();
            filter
                .values
                .iter()
                .enumerate()
                .any(|(value_index, value)| match filter.operator {
                    Operator::Equals => cell.eq_ignore_ascii_case(value),
                    Operator::NotEquals => !cell.eq_ignore_ascii_case(value),
                    Operator::Regex => filter.regexes[value_index].as_ref().map_or_else(
                        || cell.eq_ignore_ascii_case(value),
                        |regex| regex.is_match(cell),
                    ),
                    Operator::NotRegex => filter.regexes[value_index].as_ref().map_or_else(
                        || !cell.eq_ignore_ascii_case(value),
                        |regex| !regex.is_match(cell),
                    ),
                    Operator::Greater => compare_values(cell, value).is_gt(),
                    Operator::GreaterEqual => !compare_values(cell, value).is_lt(),
                    Operator::Less => compare_values(cell, value).is_lt(),
                    Operator::LessEqual => !compare_values(cell, value).is_gt(),
                })
        })
    }

    fn compare(&self, left: usize, right: usize) -> Ordering {
        let mut left_stack = [0; 4096];
        let mut right_stack = [0; 4096];
        let mut left_heap = Vec::new();
        let mut right_heap = Vec::new();

        for sort in &self.sorts {
            let left = self
                .snapshot
                .cell_bytes(left, sort.column, &mut left_stack, &mut left_heap)
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .unwrap_or_default();
            let right = self
                .snapshot
                .cell_bytes(right, sort.column, &mut right_stack, &mut right_heap)
                .and_then(|bytes| std::str::from_utf8(bytes).ok())
                .unwrap_or_default();
            let ordering = compare_values(left, right);
            let ordering = if sort.descending {
                ordering.reverse()
            } else {
                ordering
            };
            if !ordering.is_eq() {
                return ordering;
            }
        }
        Ordering::Equal
    }

    fn has_custom_sort(&self) -> bool {
        !self.sorts.is_empty()
    }

    fn display_text(&self, index: usize) -> Option<String> {
        Some(self.snapshot.format_row(index, &self.display_columns))
    }
}

fn compare_values(left: &str, right: &str) -> Ordering {
    let left = left.trim();
    let right = right.trim();
    if let (Ok(left), Ok(right)) = (left.parse::<f64>(), right.parse::<f64>()) {
        return left.partial_cmp(&right).unwrap_or(Ordering::Equal);
    }
    if let (Some(left), Some(right)) = (parse_datetime(left), parse_datetime(right)) {
        return left.cmp(&right);
    }
    if let (Some(left), Some(right)) = (parse_duration(left), parse_duration(right)) {
        return left.cmp(&right);
    }
    compare_case_insensitive(left, right)
}

fn compare_case_insensitive(left: &str, right: &str) -> Ordering {
    left.chars()
        .flat_map(char::to_lowercase)
        .cmp(right.chars().flat_map(char::to_lowercase))
}

fn parse_datetime(value: &str) -> Option<i64> {
    if let Ok(value) = DateTime::parse_from_rfc3339(value) {
        return Some(value.timestamp());
    }
    for format in [
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%d %H:%M",
        "%m/%d/%Y %I:%M:%S %p",
        "%m/%d/%Y %I:%M %p",
        "%m/%d/%Y %H:%M:%S",
        "%m/%d/%Y %H:%M",
    ] {
        if let Ok(value) = NaiveDateTime::parse_from_str(value, format) {
            return Some(value.and_utc().timestamp());
        }
    }
    for format in ["%Y-%m-%d", "%m/%d/%Y"] {
        if let Ok(value) = NaiveDate::parse_from_str(value, format) {
            return value
                .and_hms_opt(0, 0, 0)
                .map(|value| value.and_utc().timestamp());
        }
    }
    None
}

fn parse_duration(value: &str) -> Option<i128> {
    let (days, clock) = value.rsplit_once('.').map_or((0, value), |(days, clock)| {
        (days.parse::<i128>().unwrap_or(0), clock)
    });
    let fields: Vec<_> = clock.split(':').collect();
    if fields.len() < 2 || fields.len() > 3 {
        return None;
    }
    let hours = fields[0].parse::<i128>().ok()?;
    let minutes = fields[1].parse::<i128>().ok()?;
    let seconds = fields
        .get(2)
        .map_or(Some(0), |value| value.parse::<i128>().ok())?;
    Some((((days * 24 + hours) * 60 + minutes) * 60) + seconds)
}

struct ParsedFilter {
    column: String,
    operator: Operator,
    values: Vec<String>,
}
struct ParsedSort {
    column: String,
    descending: bool,
}
struct ParsedQuery {
    search: String,
    filters: Vec<ParsedFilter>,
    sorts: Vec<ParsedSort>,
    display_columns: Vec<String>,
}

fn parse_structured_query(input: &str) -> ParsedQuery {
    let mut search_parts = Vec::new();
    let mut filters = Vec::new();
    let mut sorts = Vec::new();
    let mut display_columns = Vec::new();
    for part in split_query_parts(input) {
        if let Some(token) = part.strip_prefix("/:") {
            if let Some((column, operator, values)) = parse_predicate(token) {
                filters.push(ParsedFilter {
                    column,
                    operator,
                    values,
                });
            }
            continue;
        } else if let Some(token) = part.strip_prefix("/!") {
            if let Some((direction, _, values)) = parse_predicate(token) {
                if let Some(column) = values.first() {
                    sorts.push(ParsedSort {
                        column: column.clone(),
                        descending: direction.eq_ignore_ascii_case("Descending"),
                    });
                }
            }
            continue;
        } else if let Some(token) = part.strip_prefix("/#") {
            if let Some((_, _, values)) = parse_predicate(token) {
                display_columns = values;
            }
            continue;
        } else if part == "/" {
            continue;
        }
        search_parts.push(part);
    }
    ParsedQuery {
        search: search_parts.join(" "),
        filters,
        sorts,
        display_columns,
    }
}

fn split_query_parts(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in input.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            current.push(ch);
            escaped = true;
            continue;
        }
        if ch == '"' {
            quoted = !quoted;
            current.push(ch);
            continue;
        }
        if ch.is_whitespace() && !quoted {
            if !current.is_empty() {
                parts.push(std::mem::take(&mut current));
            }
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

fn parse_predicate(token: &str) -> Option<(String, Operator, Vec<String>)> {
    const OPERATORS: [(&str, Operator); 8] = [
        ("==", Operator::Equals),
        ("!=", Operator::NotEquals),
        ("=~", Operator::Regex),
        ("!~", Operator::NotRegex),
        (">=", Operator::GreaterEqual),
        ("<=", Operator::LessEqual),
        (">", Operator::Greater),
        ("<", Operator::Less),
    ];
    let (position, text, operator) = OPERATORS
        .iter()
        .filter_map(|(text, operator)| {
            token
                .find(text)
                .map(|position| (position, *text, *operator))
        })
        .min_by_key(|entry| entry.0)?;
    let column = unquote(&token[..position]);
    let values = split_expression_values(&token[position + text.len()..])
        .into_iter()
        .map(|value| unquote(&value))
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    (!column.is_empty() && !values.is_empty()).then_some((column, operator, values))
}

fn split_expression_values(input: &str) -> Vec<String> {
    let mut values = Vec::new();
    let mut value = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for ch in input.chars() {
        if escaped {
            value.push(ch);
            escaped = false;
        } else if ch == '\\' {
            value.push(ch);
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
            value.push(ch);
        } else if ch == ',' && !quoted {
            values.push(std::mem::take(&mut value));
        } else {
            value.push(ch);
        }
    }
    values.push(value);
    values
}

fn last_unquoted_comma(input: &str) -> Option<usize> {
    let mut quoted = false;
    let mut escaped = false;
    let mut last = None;
    for (index, ch) in input.char_indices() {
        if escaped {
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '"' {
            quoted = !quoted;
        } else if ch == ',' && !quoted {
            last = Some(index);
        }
    }
    last
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    let value = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or(value);
    value.replace("\\\"", "\"").replace("\\\\", "\\")
}

pub fn fuzzy_query(input: &str) -> String {
    parse_structured_query(input).search
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_removes_structured_tokens_from_fuzzy_query() {
        let parsed = parse_structured_query("needle /:Name==alpha /!Descending==Length");
        assert_eq!(parsed.search, "needle");
        assert_eq!(parsed.filters.len(), 1);
        assert_eq!(parsed.sorts.len(), 1);
        assert!(parsed.sorts[0].descending);
    }

    #[test]
    fn structured_search_filters_and_sorts_rows() {
        let mut store = StructuredStreamingStore::new(
            StructuredSchema::new(vec!["Name".into(), "Length".into()]).unwrap(),
        );
        store.add_record(&csv::StringRecord::from(vec!["beta", "2"])).unwrap();
        store.add_record(&csv::StringRecord::from(vec!["alpha", "10"])).unwrap();
        store.add_record(&csv::StringRecord::from(vec!["gamma", "1"])).unwrap();
        let snapshot = store.snapshot();
        let output = nfm_search_core::search::search(
            Arc::clone(&snapshot),
            "/:Name!=gamma /!Descending==Length /#Display==Name",
            || false,
        )
        .unwrap();
        assert_eq!(output.matched, 2);
        assert_eq!(
            output
                .results
                .iter()
                .map(|result| result.path.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        assert_eq!(snapshot.value(output.results[0].node_index), "alpha,10");
    }

    #[test]
    fn structured_rows_and_header_share_aligned_column_widths() {
        let mut store = StructuredStreamingStore::new(
            StructuredSchema::new(vec!["Name".into(), "Length".into()]).unwrap(),
        );
        store.add_record(&csv::StringRecord::from(vec!["alpha", "10"])).unwrap();
        store.add_record(&csv::StringRecord::from(vec!["beta", "2"])).unwrap();
        let snapshot = store.snapshot();

        assert_eq!(snapshot.header(""), "Name   Length");
        assert_eq!(snapshot.header("/#Display==Length"), "Length");

        let output = nfm_search_core::search::search(Arc::clone(&snapshot), "", || false).unwrap();
        assert_eq!(output.results[0].path, "alpha  10");
        assert_eq!(output.results[1].path, "beta   2");
        assert_eq!(snapshot.value(output.results[0].node_index), "alpha,10");
    }

    #[test]
    fn autocomplete_uses_schema_and_observed_values() {
        let mut store = StructuredStreamingStore::new(
            StructuredSchema::new(vec!["Name".into(), "Status".into()]).unwrap(),
        );
        store.add_record(&csv::StringRecord::from(vec!["alpha", "Running"])).unwrap();
        let snapshot = store.snapshot();

        let sort = snapshot.completions("/!De", 4);
        assert_eq!(sort[0].replacement, "/!Descending==");
        assert_eq!(sort[0].positions, [0, 1]);
        let columns = snapshot.completions("/!Descending==Na", "/!Descending==Na".len());
        assert_eq!(columns[0].replacement, "/!Descending==Name ");
        assert!(snapshot
            .completions("/!Descending==Name ", "/!Descending==Name ".len())
            .is_empty());
        let values = snapshot.completions("/:Status==Run", 13);
        assert_eq!(values[0].replacement, "/:Status==Running ");
    }

    #[test]
    fn selecting_a_column_advances_completion_to_the_operator() {
        let mut store = StructuredStreamingStore::new(
            StructuredSchema::new(vec!["Name".into(), "Status".into()]).unwrap(),
        );
        store.add_record(&csv::StringRecord::from(vec!["alpha", "Running"])).unwrap();
        let snapshot = store.snapshot();

        let columns = snapshot.completions("/:Na", 4);
        assert_eq!(columns[0].replacement, "/:Name=");

        let operators = snapshot.completions("/:Name=", 7);
        assert!(operators
            .iter()
            .any(|suggestion| suggestion.replacement == "/:Name=="));
        assert!(operators
            .iter()
            .any(|suggestion| suggestion.replacement == "/:Name=~"));
    }

    #[test]
    fn display_completion_inserts_the_fixed_expression_prefix() {
        let mut store = StructuredStreamingStore::new(
            StructuredSchema::new(vec!["Name".into(), "Status".into()]).unwrap(),
        );
        store.add_record(&csv::StringRecord::from(vec!["alpha", "Running"])).unwrap();
        let snapshot = store.snapshot();

        let actions = snapshot.completions("/", 1);
        assert_eq!(actions[2].replacement, "/#Display==");

        let columns = snapshot.completions("/#", 2);
        assert_eq!(columns[0].replacement, "/#Display==Name,");

        let remaining = snapshot.completions("/#Display==Name,", "/#Display==Name,".len());
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].replacement, "/#Display==Name,Status,");
    }

    #[test]
    fn quoted_filter_values_may_contain_commas() {
        let parsed = parse_structured_query("/:Name==\"alpha,beta\",gamma");
        assert_eq!(parsed.filters[0].values, ["alpha,beta", "gamma"]);
    }

    #[test]
    fn text_comparison_is_case_insensitive_without_normalizing_strings() {
        assert_eq!(compare_case_insensitive("Alpha", "alpha"), Ordering::Equal);
        assert_eq!(compare_case_insensitive("alpha", "BETA"), Ordering::Less);
        assert_eq!(compare_case_insensitive("GAMMA", "beta"), Ordering::Greater);
    }
}
