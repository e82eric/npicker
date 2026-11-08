use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::Range;
use std::sync::{Arc, RwLock};

use crate::cell_format::{CellFormat, CustomFormat};
use crate::column_type::{ColumnType, Key};
use crate::value::day_span;
use nfm_search_core::row::{RowFields, RowFormat, RowTemplate, RowTemplateError, SlotKind};
use nfm_search_core::store::{
    ChunkedSnapshot, ChunkedStorage, FlatSnapshot, ItemsSource, SearchCompletion, SearchPlan,
};
use regex::RegexBuilder;

/// Build and publish a completed source from an in-memory structured table.
/// Validates column names and row widths before returning the source.
pub fn structured_source(
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
) -> Result<Arc<nfm_search_core::snapshot_store::SnapshotStore<StructuredStreamingSnapshot>>, String>
{
    structured_source_with_previews(
        columns,
        rows.into_iter().map(|cells| StructuredSourceRow {
            cells,
            preview: None,
        }),
    )
}

/// A row in an in-memory structured source, with optional preview content.
#[derive(Clone, Debug)]
pub struct StructuredSourceRow {
    pub cells: Vec<String>,
    pub preview: Option<String>,
}

/// Build a completed source, preserving each row's optional preview independently.
pub fn structured_source_with_previews(
    columns: Vec<String>,
    rows: impl IntoIterator<Item = StructuredSourceRow>,
) -> Result<Arc<nfm_search_core::snapshot_store::SnapshotStore<StructuredStreamingSnapshot>>, String>
{
    let mut store = StructuredStreamingStore::new(StructuredSchema::new(columns)?);
    for row in rows {
        store
            .add_record_with_preview(&csv::StringRecord::from(row.cells), row.preview.as_deref())?;
    }
    let source = Arc::new(nfm_search_core::snapshot_store::SnapshotStore::new());
    source.publish(store.snapshot());
    source.complete();
    Ok(source)
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub struct StructuredPickerItem {
    pub value: String,
    pub fields: HashMap<String, String>,
    pub preview: Option<String>,
}

/// Column names, how each column's values are parsed (see
/// [`crate::column_type`]; every column is `auto` unless set), and how a
/// column is shown (see [`crate::cell_format`]).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StructuredSchema {
    pub columns: Vec<String>,
    types: Vec<ColumnType>,
    /// Column formats: a formatted column is shown (in the table and on
    /// cards) and searched formatted, while sorting, filters and the selected
    /// value use the raw text. A typed column without one uses its type's
    /// format ([`CellFormat::default_for`]).
    formats: Vec<Option<CellFormat>>,
    /// The order without fuzzy text or `/!` sorts in the query.
    default_sorts: Vec<Sort>,
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
        let types = vec![ColumnType::Auto; columns.len()];
        let formats = vec![None; columns.len()];
        Ok(Self {
            columns,
            types,
            formats,
            default_sorts: Vec::new(),
        })
    }

    /// Order rows by these sorts, written as in queries
    /// (`/!Descending==CPU /!Descending==PrivateBytes`), until the query has
    /// fuzzy text (which orders by match) or sorts of its own.
    pub fn with_default_sort(mut self, sorts: &str) -> Result<Self, String> {
        let parsed = parse_structured_query(sorts);
        if !parsed.search.is_empty() || !parsed.filters.is_empty() || parsed.sorts.is_empty() {
            return Err(format!(
                "'{sorts}' is not a list of sorts such as /!Descending==Column"
            ));
        }
        self.default_sorts = parsed
            .sorts
            .iter()
            .map(|sort| {
                self.column_index(&sort.column)
                    .map(|column| Sort {
                        column,
                        descending: sort.descending,
                    })
                    .ok_or_else(|| format!("unknown column '{}'", sort.column))
            })
            .collect::<Result<_, _>>()?;
        Ok(self)
    }

    /// Set a column's type, by name ignoring ASCII case.
    pub fn set_type(&mut self, column: &str, column_type: ColumnType) -> Result<(), String> {
        let index = self
            .column_index(column)
            .ok_or_else(|| format!("unknown column '{column}'"))?;
        self.types[index] = column_type;
        Ok(())
    }

    pub fn with_type(mut self, column: &str, column_type: ColumnType) -> Result<Self, String> {
        self.set_type(column, column_type)?;
        Ok(self)
    }

    /// Set types from `NAME:TYPE,NAME:TYPE`, such as `pid:number,size:bytes=KB`.
    pub fn with_types(mut self, spec: &str) -> Result<Self, String> {
        for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let (column, column_type) = entry
                .rsplit_once(':')
                .ok_or_else(|| format!("'{entry}' is not NAME:TYPE"))?;
            let column_type = ColumnType::parse(column_type)
                .map_err(|e| format!("column '{}': {e}", column.trim()))?;
            self.set_type(column.trim(), column_type)?;
        }
        Ok(self)
    }

    /// The type of the column at `column`.
    pub fn column_type(&self, column: usize) -> ColumnType {
        self.types.get(column).copied().unwrap_or_default()
    }

    /// Show a column in a format, such as `bytes` or `number=1` (see
    /// [`crate::cell_format`]), in the table and on cards. Set types first:
    /// the format must suit the column's type. `devicon` makes icon
    /// references, so it is only a card icon slot format.
    pub fn set_format(&mut self, column: &str, format: &str) -> Result<(), String> {
        let index = self
            .column_index(column)
            .ok_or_else(|| format!("unknown column '{column}'"))?;
        let column = &self.columns[index];
        let spec = match format.trim().split_once('=') {
            Some((name, argument)) => RowFormat::with_argument(name.trim(), argument),
            None => RowFormat::new(format.trim()),
        };
        let compiled =
            CellFormat::compile(&spec).map_err(|e| format!("column '{column}': {e}"))?;
        if compiled == CellFormat::Devicon {
            return Err(format!(
                "column '{column}': devicon is only a format for card icon slots"
            ));
        }
        let column_type = self.types[index];
        if !compiled.accepts(column_type) {
            return Err(format!(
                "column '{column}': {} does not apply to a {column_type} column",
                spec.name
            ));
        }
        self.formats[index] = Some(compiled);
        Ok(())
    }

    /// Set formats from `NAME:FORMAT,NAME:FORMAT`, such as
    /// `size:bytes,ratio:percent=1`.
    pub fn with_formats(mut self, spec: &str) -> Result<Self, String> {
        for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let (column, format) = entry
                .split_once(':')
                .ok_or_else(|| format!("'{entry}' is not NAME:FORMAT"))?;
            self.set_format(column.trim(), format)?;
        }
        Ok(self)
    }

    /// Show a column as `format` returns it, such as a timestamp as
    /// "9 minutes ago" (see [`CustomFormat`]). The result is shown and
    /// searched, while the column's type still sorts and filters the raw
    /// value.
    pub fn set_formatter(
        &mut self,
        column: &str,
        format: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Result<(), String> {
        let index = self
            .column_index(column)
            .ok_or_else(|| format!("unknown column '{column}'"))?;
        self.formats[index] = Some(CellFormat::Custom(CustomFormat::new(format)));
        Ok(())
    }

    pub fn with_formatter(
        mut self,
        column: &str,
        format: impl Fn(&str) -> String + Send + Sync + 'static,
    ) -> Result<Self, String> {
        self.set_formatter(column, format)?;
        Ok(self)
    }

    /// A cell as it is shown and searched: in its column's format (or its
    /// type's), or as it is if there is none or the value does not parse.
    fn shown<'a>(&self, column: usize, value: &'a str) -> std::borrow::Cow<'a, str> {
        let column_type = self.column_type(column);
        let format = match self.formats.get(column).and_then(Option::as_ref) {
            Some(format) => std::borrow::Cow::Borrowed(format),
            None => match CellFormat::default_for(column_type) {
                Some(format) => std::borrow::Cow::Owned(format),
                None => return value.into(),
            },
        };
        format
            .apply(value, column_type)
            .map_or(value.into(), Into::into)
    }

    /// Numbers, sizes and durations line up on the right.
    fn right_aligned(&self, column: usize) -> bool {
        matches!(
            self.column_type(column),
            ColumnType::Number | ColumnType::Bytes { .. } | ColumnType::Duration
        )
    }

    /// Column index for a name, ignoring ASCII case.
    pub fn field_index(&self, name: &str) -> Option<usize> {
        self.column_index(name)
    }

    /// Parse a compact card template over this schema's columns. See
    /// [`RowTemplate::parse`]; `leading` is the leading icon slot, such as
    /// `path:devicon`.
    pub fn row_template(
        &self,
        spec: &str,
        leading: Option<&str>,
    ) -> Result<RowTemplate, RowTemplateError> {
        RowTemplate::parse(spec, leading, |name| self.column_index(name))
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
    preview: Option<ByteRange>,
    first_cell: usize,
    cell_count: usize,
}

pub struct StructuredStreamingStore {
    schema: Arc<StructuredSchema>,
    rows: ChunkedStorage<StructuredRow>,
    cells: ChunkedStorage<ByteRange>,
    /// Each cell parsed by its column's type, parallel to `cells`.
    keys: ChunkedStorage<Key>,
    bytes: ChunkedStorage<u8>,
    distinct_seen: Vec<HashSet<String>>,
    distinct_values: Arc<RwLock<Vec<Vec<String>>>>,
    column_widths: Vec<usize>,
    version: u64,
    template: Option<Arc<CardTemplate>>,
}

/// A card template. Text slots show their column as the table does; a
/// `devicon` icon slot reads a field past the columns: `devicons[k]` is the
/// column whose path icon is field `columns + k`.
struct CardTemplate {
    template: RowTemplate,
    devicons: Vec<usize>,
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
            keys: ChunkedStorage::new(64 * 1024),
            bytes: ChunkedStorage::new(1024 * 1024),
            distinct_seen: (0..column_count).map(|_| HashSet::new()).collect(),
            distinct_values: Arc::new(RwLock::new(vec![Vec::new(); column_count])),
            version: 0,
            template: None,
        }
    }

    /// Paint results as cards with `template`. Its fields are column indexes.
    /// Text slots show their column's formatted text, the same text the
    /// table shows and fuzzy search reads, so they take no format of their
    /// own; the one slot format is `devicon`, on icon slots.
    pub fn set_row_template(&mut self, template: RowTemplate) -> Result<(), String> {
        let columns = self.schema.columns.len();
        if template.max_field().is_some_and(|field| field >= columns) {
            return Err(format!(
                "row template refers to a column past the {columns} columns"
            ));
        }
        let mut template = template;
        let mut devicons = Vec::new();
        for slot in template.slots_mut() {
            let Some(format) = &slot.format else {
                continue;
            };
            let column = &self.schema.columns[slot.field];
            let name = &format.name;
            let compiled =
                CellFormat::compile(format).map_err(|e| format!("column '{column}': {e}"))?;
            match (slot.kind, compiled) {
                (SlotKind::Icon, CellFormat::Devicon) => {}
                (SlotKind::Icon, _) => {
                    return Err(format!(
                        "column '{column}': icon slots take devicon, not {name}"
                    ));
                }
                (SlotKind::Text, _) => {
                    return Err(format!(
                        "column '{column}': {name} is a column format; set it for the \
                         column (with_formats, --csv-formats), not on a card slot"
                    ));
                }
            }
            devicons.push(slot.field);
            slot.field = columns + devicons.len() - 1;
        }
        self.template = Some(Arc::new(CardTemplate {
            template,
            devicons,
        }));
        Ok(())
    }

    pub fn with_row_template(mut self, template: RowTemplate) -> Result<Self, String> {
        self.set_row_template(template)?;
        Ok(self)
    }

    pub fn add_record(&mut self, record: &csv::StringRecord) -> Result<(), String> {
        self.add_record_with_preview(record, None)
    }

    pub fn add_record_with_preview(
        &mut self,
        record: &csv::StringRecord,
        preview: Option<&str>,
    ) -> Result<(), String> {
        if record.len() != self.schema.columns.len() {
            return Err(format!(
                "record has {} fields, expected {}",
                record.len(),
                self.schema.columns.len()
            ));
        }

        let first_cell = self.cells.len();
        // The table's text: formatted columns formatted, control characters
        // escaped. It sets the column widths and is what fuzzy search reads.
        let display_values: Vec<_> = record
            .iter()
            .enumerate()
            .map(|(column, value)| display_cell(&self.schema.shown(column, value)))
            .collect();
        for (column, value) in record.iter().enumerate() {
            self.column_widths[column] =
                self.column_widths[column].max(display_values[column].chars().count());
            let range = ByteRange {
                offset: self.bytes.len(),
                length: value.len(),
            };
            self.bytes.extend_from_slice(value.as_bytes());
            self.cells.push(range);
            self.keys.push(self.schema.column_type(column).key(value));
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
        let display = encode_csv_record(display_values.iter().map(String::as_str));
        let display_range = ByteRange {
            offset: self.bytes.len(),
            length: display.len(),
        };
        self.bytes.extend_from_slice(&display);
        let preview = preview.map(|preview| {
            let range = ByteRange {
                offset: self.bytes.len(),
                length: preview.len(),
            };
            self.bytes.extend_from_slice(preview.as_bytes());
            range
        });
        self.rows.push(StructuredRow {
            display: display_range,
            value: value_range,
            preview,
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
            keys: self.keys.snapshot(),
            bytes: self.bytes.snapshot(),
            version: self.version,
            distinct_values: Arc::clone(&self.distinct_values),
            column_widths: Arc::from(self.column_widths.clone()),
            template: self.template.clone(),
        })
    }
}

pub struct StructuredStreamingSnapshot {
    schema: Arc<StructuredSchema>,
    rows: ChunkedSnapshot<StructuredRow>,
    row_count: usize,
    cells: ChunkedSnapshot<ByteRange>,
    keys: ChunkedSnapshot<Key>,
    bytes: ChunkedSnapshot<u8>,
    version: u64,
    distinct_values: Arc<RwLock<Vec<Vec<String>>>>,
    column_widths: Arc<[usize]>,
    template: Option<Arc<CardTemplate>>,
}

impl StructuredStreamingSnapshot {
    /// Build a validated immutable structured source for embedded controls.
    pub fn from_rows(columns: Vec<String>, rows: Vec<Vec<String>>) -> Result<Arc<Self>, String> {
        let mut store = StructuredStreamingStore::new(StructuredSchema::new(columns)?);
        for row in rows {
            store.add_record(&csv::StringRecord::from(row))?;
        }
        Ok(store.snapshot())
    }

    /// Card field values: every column as the table shows it (formatted,
    /// display-escaped; the text fuzzy search reads), then each `devicon`
    /// slot's icon reference.
    pub fn card_fields(&self, row: usize) -> RowFields {
        let columns = self.schema.columns.len();
        let mut values: Vec<String> = (0..columns)
            .map(|column| {
                display_cell(&self.schema.shown(column, &self.cell_string(row, column)))
            })
            .collect();
        if let Some(card) = &self.template {
            for &column in &card.devicons {
                let raw = self.cell_string(row, column);
                values.push(
                    CellFormat::Devicon
                        .apply(&raw, self.schema.column_type(column))
                        .unwrap_or_default(),
                );
            }
        }
        RowFields { values }
    }

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
            .map(|(column_index, column)| (column.clone(), self.cell_string(row, column_index)))
            .collect()
    }

    pub fn preview(&self, row: usize) -> Option<String> {
        let range = self.rows[row].preview?;
        let mut stack = [0; 4096];
        let mut heap = Vec::new();
        Some(String::from_utf8_lossy(self.range(range, &mut stack, &mut heap)).into_owned())
    }

    pub fn header(&self, query: &str) -> String {
        let columns = selected_display_columns(&self.schema, query);
        format_columns(
            &self.schema,
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
            .map(|&column| {
                let raw = self.cell_string(row, column);
                self.schema.shown(column, &raw).into_owned()
            })
            .collect();
        format_columns(
            &self.schema,
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
        self.cell_bytes(row, column, &mut stack, &mut heap)
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

    fn cell_key(&self, row_index: usize, column: usize) -> Key {
        let row = self.rows[row_index];
        if column >= row.cell_count {
            return Key::Text;
        }
        self.keys[row.first_cell + column]
    }

    pub fn completions(&self, input: &str, cursor: usize) -> Vec<SearchCompletion> {
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
                .map(|(replacement, description)| SearchCompletion {
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

/// Columns padded to their widths, two spaces apart; numeric columns are
/// right-aligned (see [`StructuredSchema::right_aligned`]).
fn format_columns<'a>(
    schema: &StructuredSchema,
    values: impl IntoIterator<Item = &'a str>,
    columns: &[usize],
    widths: &[usize],
) -> String {
    let values: Vec<_> = values.into_iter().collect();
    let mut output = String::new();
    for (position, (&column, value)) in columns.iter().zip(values).enumerate() {
        let value = display_cell(value);
        let padding = widths[column].saturating_sub(value.chars().count());
        let last = position + 1 == columns.len();
        if schema.right_aligned(column) {
            output.extend(std::iter::repeat_n(' ', padding));
            output.push_str(&value);
        } else {
            output.push_str(&value);
            if !last {
                output.extend(std::iter::repeat_n(' ', padding));
            }
        }
        if !last {
            output.push_str("  ");
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
) -> Vec<SearchCompletion> {
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
        .map(|text| SearchCompletion {
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
    type Item = StructuredPickerItem;

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

    fn item(&self, node_index: usize) -> Option<Self::Item> {
        if node_index >= self.row_count {
            return None;
        }
        Some(StructuredPickerItem {
            value: self.value(node_index),
            fields: self.fields(node_index),
            preview: self.preview(node_index),
        })
    }

    fn header(&self, query: &str) -> Option<String> {
        if self.template.is_some() {
            return None;
        }
        Some(StructuredStreamingSnapshot::header(self, query))
    }

    fn display_text(&self, node_index: usize, query: &str) -> Option<String> {
        (node_index < self.row_count).then(|| self.display_row(node_index, query))
    }

    fn completions(&self, input: &str, cursor: usize) -> Vec<SearchCompletion> {
        StructuredStreamingSnapshot::completions(self, input, cursor)
    }

    fn effective_query(&self, input: &str) -> String {
        fuzzy_query(input)
    }

    fn row_template(&self) -> Option<&RowTemplate> {
        self.template.as_ref().map(|t| &t.template)
    }

    fn row_fields(&self, node_index: usize) -> Option<RowFields> {
        (self.template.is_some() && node_index < self.row_count)
            .then(|| self.card_fields(node_index))
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
    /// Each value parsed by the column's type.
    keys: Vec<FilterKey>,
    regexes: Vec<Option<regex::Regex>>,
}

/// A filter value, parsed as its column parses cells.
#[derive(Clone, Copy)]
enum FilterKey {
    Value(Key),
    /// A date alone, in a column with dates: the local day, as Unix seconds
    /// `[start, end)`, so `==` means "on that day".
    Day(f64, f64),
}

impl FilterKey {
    fn parse(column_type: ColumnType, value: &str) -> Self {
        column_type
            .has_dates()
            .then(|| day_span(value))
            .flatten()
            .map_or_else(
                || Self::Value(column_type.key(value)),
                |(start, end)| Self::Day(start, end),
            )
    }

    /// How a cell compares with this value, or None when one is a value its
    /// type parsed and the other is not (a number and a word, say).
    fn order(self, cell: &str, cell_key: Key, value: &str) -> Option<Ordering> {
        match self {
            Self::Day(start, end) => match cell_key {
                Key::Time(time) if time < start => Some(Ordering::Less),
                Key::Time(time) if time >= end => Some(Ordering::Greater),
                Key::Time(_) => Some(Ordering::Equal),
                _ => None,
            },
            Self::Value(Key::Text) => {
                (cell_key == Key::Text).then(|| compare_case_insensitive(cell, value))
            }
            Self::Value(key) => cell_key
                .same_kind(key)
                .map(|(cell, value)| cell.partial_cmp(&value).unwrap_or(Ordering::Equal)),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
                let column_type = snapshot.schema.column_type(column);
                let keys = filter
                    .values
                    .iter()
                    .map(|value| FilterKey::parse(column_type, value))
                    .collect();
                Some(Filter {
                    column,
                    operator: filter.operator,
                    values: filter.values,
                    keys,
                    regexes,
                })
            })
            .collect();
        let sorts = if parsed.sorts.is_empty() && parsed.search.is_empty() {
            snapshot.schema.default_sorts.clone()
        } else {
            parsed
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
                .collect()
        };
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
            let cell_key = self.snapshot.cell_key(index, filter.column);
            let order = |value_index: usize, value: &str| {
                filter.keys[value_index].order(cell, cell_key, value)
            };
            // Equal values, or the same text when their types differ.
            let equals = |value_index: usize, value: &str| match order(value_index, value) {
                Some(ordering) => ordering.is_eq(),
                None => cell.eq_ignore_ascii_case(value),
            };
            filter
                .values
                .iter()
                .enumerate()
                .any(|(value_index, value)| match filter.operator {
                    Operator::Equals => equals(value_index, value),
                    Operator::NotEquals => !equals(value_index, value),
                    Operator::Regex => filter.regexes[value_index].as_ref().map_or_else(
                        || cell.eq_ignore_ascii_case(value),
                        |regex| regex.is_match(cell),
                    ),
                    Operator::NotRegex => filter.regexes[value_index].as_ref().map_or_else(
                        || !cell.eq_ignore_ascii_case(value),
                        |regex| !regex.is_match(cell),
                    ),
                    Operator::Greater => order(value_index, value).is_some_and(Ordering::is_gt),
                    Operator::GreaterEqual => {
                        order(value_index, value).is_some_and(Ordering::is_ge)
                    }
                    Operator::Less => order(value_index, value).is_some_and(Ordering::is_lt),
                    Operator::LessEqual => order(value_index, value).is_some_and(Ordering::is_le),
                })
        })
    }

    fn compare(&self, left: usize, right: usize) -> Ordering {
        let mut left_stack = [0; 4096];
        let mut right_stack = [0; 4096];
        let mut left_heap = Vec::new();
        let mut right_heap = Vec::new();

        for sort in &self.sorts {
            let left_key = self.snapshot.cell_key(left, sort.column);
            let right_key = self.snapshot.cell_key(right, sort.column);
            // Text is only read when both cells are text.
            let ordering = left_key.order(right_key, sort.descending, || {
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
                compare_case_insensitive(left.trim(), right.trim())
            });
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

fn compare_case_insensitive(left: &str, right: &str) -> Ordering {
    left.chars()
        .flat_map(char::to_lowercase)
        .cmp(right.chars().flat_map(char::to_lowercase))
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
    fn preview_metadata_is_not_part_of_display_or_fields() {
        let mut store = StructuredStreamingStore::new(
            StructuredSchema::new(vec!["Name".into(), "State".into()]).unwrap(),
        );
        store
            .add_record_with_preview(
                &csv::StringRecord::from(vec!["service", "failed"]),
                Some("Stack trace:\nframe one\nframe two"),
            )
            .unwrap();
        let snapshot = store.snapshot();

        assert_eq!(
            snapshot.preview(0).as_deref(),
            Some("Stack trace:\nframe one\nframe two")
        );
        assert!(!snapshot.display_row(0, "").contains("frame one"));
        assert!(!snapshot
            .fields(0)
            .values()
            .any(|value| value.contains("frame one")));
    }

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
        store
            .add_record(&csv::StringRecord::from(vec!["beta", "2"]))
            .unwrap();
        store
            .add_record(&csv::StringRecord::from(vec!["alpha", "10"]))
            .unwrap();
        store
            .add_record(&csv::StringRecord::from(vec!["gamma", "1"]))
            .unwrap();
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

    /// The `Name` of every row `query` matches, in result order.
    fn names(schema: StructuredSchema, rows: &[[&str; 2]], query: &str) -> Vec<String> {
        let mut store = StructuredStreamingStore::new(schema);
        for row in rows {
            store
                .add_record(&csv::StringRecord::from(row.to_vec()))
                .unwrap();
        }
        let query = format!("{query} /#Display==Name");
        nfm_search_core::search::search(store.snapshot(), &query, || false)
            .unwrap()
            .results
            .into_iter()
            .map(|result| result.path)
            .collect()
    }

    fn schema(value_type: &str) -> StructuredSchema {
        StructuredSchema::new(vec!["Name".into(), "Value".into()])
            .unwrap()
            .with_types(&format!("Name:text, value:{value_type}"))
            .unwrap()
    }

    #[test]
    fn schemas_take_types_by_column_name() {
        let schema = schema("bytes=KB");
        assert_eq!(schema.column_type(0), ColumnType::Text);
        assert_eq!(schema.column_type(1), ColumnType::Bytes { unit: 1024 });
        let plain = StructuredSchema::new(vec!["a".into()]).unwrap();
        assert_eq!(plain.column_type(0), ColumnType::Auto);
        assert!(plain.clone().with_types("b:number").is_err());
        assert!(plain.clone().with_types("a").is_err());
        let error = plain.with_types("a:money").unwrap_err();
        assert!(error.starts_with("column 'a': unknown column type"), "{error}");
    }

    #[test]
    fn auto_columns_sort_numbers_then_times_then_text_without_cycles() {
        // Text compared with numbers used to be compared as text, which
        // ordered these in a cycle.
        let rows = [
            ["word", "n/a"],
            ["date", "2026-11-06"],
            ["five", "5"],
            ["unix", "1700000000"],
            ["empty", ""],
        ];
        let ascending = names(schema("auto"), &rows, "/!Ascending==Value");
        assert_eq!(ascending, ["five", "unix", "date", "empty", "word"]);
        // Values reverse; text still comes last.
        let descending = names(schema("auto"), &rows, "/!Descending==Value");
        assert_eq!(descending, ["unix", "five", "date", "word", "empty"]);
        // NaN is text, not a number equal to every other number.
        let rows = [["nan", "NaN"], ["two", "2"], ["one", "1"]];
        assert_eq!(
            names(schema("number"), &rows, "/!Ascending==Value"),
            ["one", "two", "nan"]
        );
    }

    #[test]
    fn typed_columns_sort_and_filter_by_their_values() {
        let rows = [["small", "512"], ["large", "2MB"], ["medium", "300"]];
        // A plain number is in the column's unit: 512 KB, 300 KB.
        let kb = schema("bytes=KB");
        assert_eq!(
            names(kb.clone(), &rows, "/!Descending==Value"),
            ["large", "small", "medium"]
        );
        assert_eq!(names(kb.clone(), &rows, "/:Value>400KB"), ["small", "large"]);
        assert_eq!(names(kb, &rows, "/:Value==0.5MB"), ["small"]);

        let rows = [["long", "1.02:00:00"], ["short", "00:00:12.5"], ["mid", "2h 13m"]];
        assert_eq!(
            names(schema("duration"), &rows, "/!Ascending==Value"),
            ["short", "mid", "long"]
        );
        assert_eq!(names(schema("duration"), &rows, "/:Value>=1h"), ["long", "mid"]);
        // Durations are not guessed in auto columns: they sort as text.
        assert_eq!(
            names(schema("auto"), &rows, "/!Ascending==Value"),
            ["short", "long", "mid"]
        );

        // Numbers compare as numbers, so 1 and 1.0 are equal.
        let rows = [["a", "1.0"], ["b", "10"]];
        assert_eq!(names(schema("number"), &rows, "/:Value==1"), ["a"]);
        // A number and a word never compare.
        assert_eq!(names(schema("number"), &rows, "/:Value>abc").len(), 0);
    }

    #[test]
    fn a_date_filter_value_means_its_whole_day() {
        let rows = [
            ["before", "2026-11-05 23:59"],
            ["morning", "2026-11-06 08:00"],
            ["night", "2026-11-06 23:30"],
            ["after", "2026-11-07"],
        ];
        for value_type in ["auto", "datetime"] {
            let schema = || schema(value_type);
            assert_eq!(names(schema(), &rows, "/:Value==2026-11-06"), ["morning", "night"]);
            assert_eq!(names(schema(), &rows, "/:Value>2026-11-06"), ["after"]);
            assert_eq!(names(schema(), &rows, "/:Value<2026-11-06"), ["before"]);
            assert_eq!(names(schema(), &rows, "/:Value<=2026-11-06").len(), 3);
            assert_eq!(names(schema(), &rows, "/:Value>=2026-11-06").len(), 3);
        }
        // Unix times are times in a datetime column, and numbers otherwise.
        let unix = parse_timestamp_seconds("2026-11-06 12:00").to_string();
        let rows = [["unix", unix.as_str()]];
        assert_eq!(names(schema("datetime"), &rows, "/:Value==2026-11-06"), ["unix"]);
        assert_eq!(names(schema("auto"), &rows, "/:Value==2026-11-06").len(), 0);
    }

    fn parse_timestamp_seconds(value: &str) -> i64 {
        crate::value::parse_timestamp(value)
            .unwrap()
            .timestamp()
    }

    #[test]
    fn table_formats_show_formatted_cells_and_keep_raw_values_for_the_rest() {
        let schema = StructuredSchema::new(vec!["Name".into(), "Size".into(), "Note".into()])
            .unwrap()
            .with_types("size:bytes=KB")
            .unwrap()
            .with_formats("size:bytes")
            .unwrap();
        let mut store = StructuredStreamingStore::new(schema.clone());
        for row in [["big", "123456", "x"], ["small", "512", "y"], ["odd", "n/a", "z"]] {
            store
                .add_record(&csv::StringRecord::from(row.to_vec()))
                .unwrap();
        }
        let snapshot = store.snapshot();
        // Sizes are formatted and right-aligned under a right-aligned header.
        assert_eq!(snapshot.header(""), "Name       Size  Note");
        let rows: Vec<_> = (0..3).map(|row| snapshot.display_row(row, "")).collect();
        assert_eq!(rows, ["big    120.6 MB  x", "small  512.0 KB  y", "odd         n/a  z"]);
        // Fuzzy search reads the formatted text; the value keeps the raw cell.
        let output = nfm_search_core::search::search(snapshot.clone(), "MB", || false).unwrap();
        assert_eq!(output.matched, 1);
        assert_eq!(snapshot.value(output.results[0].node_index), "big,123456,x");
        // Sorting and filters still read the KB.
        let query = "/:Size>1MB /#Display==Name";
        let output = nfm_search_core::search::search(snapshot, query, || false).unwrap();
        assert_eq!(output.results[0].path, "big");

        let mut schema = schema;
        assert!(schema.set_format("size", "time").is_err());
        assert!(schema.set_format("note", "devicon").is_err());
        let error = schema.set_format("size", "date").unwrap_err();
        assert_eq!(error, "column 'Size': date does not apply to a bytes=KB column");
        assert!(schema.set_format("nope", "bytes").is_err());
    }

    #[test]
    fn default_sorts_apply_until_the_query_has_text_or_sorts() {
        let schema = schema("number")
            .with_default_sort("/!Descending==Value /!Ascending==Name")
            .unwrap();
        let rows = [["b", "1"], ["c", "5"], ["a", "5"], ["d", "2"]];
        assert_eq!(names(schema.clone(), &rows, ""), ["a", "c", "d", "b"]);
        // Filters keep the default order.
        assert_eq!(names(schema.clone(), &rows, "/:Value<5"), ["d", "b"]);
        // The query's own sorts replace it.
        assert_eq!(
            names(schema.clone(), &rows, "/!Ascending==Value"),
            ["b", "d", "c", "a"]
        );
        // Fuzzy text orders by match instead.
        assert_eq!(names(schema.clone(), &rows, "c"), ["c"]);

        assert!(schema.clone().with_default_sort("/!Descending==Nope").is_err());
        assert!(schema.clone().with_default_sort("text").is_err());
        assert!(schema.clone().with_default_sort("/:Value==1").is_err());
        assert!(schema.with_default_sort("").is_err());
    }

    #[test]
    fn sorts_without_fuzzy_text_order_every_row_not_only_the_first_shown() {
        let mut store = StructuredStreamingStore::new(schema("number"));
        let rows = nfm_search_core::search::RESULT_LIMIT + 500;
        for value in 0..rows {
            store
                .add_record(&csv::StringRecord::from(vec![
                    format!("row{value}"),
                    value.to_string(),
                ]))
                .unwrap();
        }
        let query = "/!Descending==Value /#Display==Name";
        let output = nfm_search_core::search::search(store.snapshot(), query, || false).unwrap();
        assert_eq!(output.matched, rows);
        assert_eq!(output.results.len(), nfm_search_core::search::RESULT_LIMIT);
        assert_eq!(output.results[0].path, format!("row{}", rows - 1));
        assert_eq!(output.results[1].path, format!("row{}", rows - 2));
    }

    #[test]
    fn typed_columns_are_shown_and_searched_in_their_format() {
        let schema = StructuredSchema::new(vec!["Name".into(), "Size".into(), "Took".into()])
            .unwrap()
            .with_types("size:bytes=KB,took:duration")
            .unwrap();
        let template = schema.row_template("name|size|took", None).unwrap();
        let mut store = StructuredStreamingStore::new(schema.clone())
            .with_row_template(template)
            .unwrap();
        store
            .add_record(&csv::StringRecord::from(vec!["a", "123456", "7980"]))
            .unwrap();
        let snapshot = store.snapshot();
        // Cards show the column as the table does, highlighted.
        let template = snapshot.row_template().unwrap();
        let slots: Vec<_> = template.slots().collect();
        assert_eq!(slots.iter().map(|s| s.field).collect::<Vec<_>>(), [0, 1, 2]);
        assert!(slots.iter().all(|slot| slot.highlight));
        let row = snapshot.row_fields(0).unwrap();
        assert_eq!((row.get(1), row.get(2)), ("120.6 MB", "2h 13m"));
        assert_eq!(snapshot.display_row(0, ""), "a     120.6 MB  2h 13m");
        // Fuzzy search reads the same text.
        let output = nfm_search_core::search::search(snapshot, "2h 13m", || false).unwrap();
        assert_eq!(output.matched, 1);
    }

    #[test]
    fn structured_rows_and_header_share_aligned_column_widths() {
        let mut store = StructuredStreamingStore::new(
            StructuredSchema::new(vec!["Name".into(), "Length".into()]).unwrap(),
        );
        store
            .add_record(&csv::StringRecord::from(vec!["alpha", "10"]))
            .unwrap();
        store
            .add_record(&csv::StringRecord::from(vec!["beta", "2"]))
            .unwrap();
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
        store
            .add_record(&csv::StringRecord::from(vec!["alpha", "Running"]))
            .unwrap();
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
        store
            .add_record(&csv::StringRecord::from(vec!["alpha", "Running"]))
            .unwrap();
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
        store
            .add_record(&csv::StringRecord::from(vec!["alpha", "Running"]))
            .unwrap();
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

#[cfg(test)]
mod completed_source_tests {
    use super::*;
    #[test]
    fn leading_icons_are_formatted_like_other_slots() {
        let schema = StructuredSchema::new(vec!["name".into(), "path".into()]).unwrap();
        let template = schema.row_template("name", Some("path:devicon")).unwrap();
        let mut store = StructuredStreamingStore::new(schema)
            .with_row_template(template)
            .unwrap();
        for row in [["a", "src/main.rs"], ["b", ""]] {
            store
                .add_record(&csv::StringRecord::from(row.to_vec()))
                .unwrap();
        }
        let snapshot = store.snapshot();
        let leading = snapshot.row_template().unwrap().leading.clone().unwrap();
        assert_eq!(leading.field, 2);
        assert_eq!(
            snapshot.row_fields(0).unwrap().get(2),
            crate::devicon::lookup("main.rs").reference()
        );
        assert_eq!(snapshot.row_fields(1).unwrap().get(2), "");
    }

    #[test]
    fn a_host_formatter_is_shown_and_searched_while_the_type_sorts_and_filters() {
        // The host words the time; the column stays a datetime underneath.
        let schema = StructuredSchema::new(vec!["name".into(), "completed".into()])
            .unwrap()
            .with_types("completed:datetime")
            .unwrap()
            .with_formatter("completed", |raw| match raw {
                "2026-11-06T10:00:00Z" => "9 minutes ago".into(),
                "2026-11-05T10:00:00Z" => "1 day ago".into(),
                other => other.into(),
            })
            .unwrap();
        let template = schema.row_template("name|completed:right", None).unwrap();
        let mut store = StructuredStreamingStore::new(schema)
            .with_row_template(template)
            .unwrap();
        for row in [
            ["older", "2026-11-05T10:00:00Z"],
            ["newer", "2026-11-06T10:00:00Z"],
        ] {
            store
                .add_record(&csv::StringRecord::from(row.to_vec()))
                .unwrap();
        }
        let snapshot = store.snapshot();
        // Cards show the formatted text, highlighted like any other text.
        let template = snapshot.row_template().unwrap();
        assert!(template.slots().all(|slot| slot.highlight));
        assert_eq!(snapshot.row_fields(1).unwrap().get(1), "9 minutes ago");
        // Fuzzy search reads it.
        let search = |query: &str| {
            let output =
                nfm_search_core::search::search(snapshot.clone(), query, || false).unwrap();
            output
                .results
                .iter()
                .map(|result| snapshot.value(result.node_index))
                .collect::<Vec<_>>()
        };
        assert_eq!(search("minutes ago"), ["newer,2026-11-06T10:00:00Z"]);
        // Sorts and filters read the timestamp, and the value is raw.
        assert_eq!(
            search("/!Descending==completed"),
            ["newer,2026-11-06T10:00:00Z", "older,2026-11-05T10:00:00Z"]
        );
        assert_eq!(
            search("/:completed<2026-11-06"),
            ["older,2026-11-05T10:00:00Z"]
        );
    }

    #[test]
    fn card_slots_take_no_text_formats() {
        let schema = StructuredSchema::new(vec!["a".into(), "path".into()]).unwrap();
        let template = |spec| {
            StructuredStreamingStore::new(schema.clone())
                .with_row_template(schema.row_template(spec, None).unwrap())
                .err()
                .unwrap()
        };
        let error = template("a:bytes");
        assert!(error.contains("bytes is a column format"), "{error}");
        let error = template("path:icon:date");
        assert!(error.contains("icon slots take devicon, not date"), "{error}");
        let error = template("a:mutd");
        assert!(error.contains("column 'a': unknown format 'mutd'"), "{error}");
        assert!(
            StructuredStreamingStore::new(schema.clone())
                .with_row_template(schema.row_template("a|path:icon:devicon", None).unwrap())
                .is_ok()
        );
    }

    #[test]
    fn completed_tables_validate_schema_and_row_widths() {
        let source = structured_source(vec!["Name".into()], vec![vec!["entry".into()]]).unwrap();
        assert!(source.is_done());
        assert_eq!(
            source.snapshot().unwrap().item(0).unwrap().fields["Name"],
            "entry"
        );
        let empty = structured_source(vec!["Name".into()], Vec::new()).unwrap();
        assert!(empty.is_done());
        assert!(empty.snapshot().unwrap().is_empty());
        assert!(structured_source(Vec::new(), Vec::new()).is_err());
        assert!(structured_source(vec!["Name".into(), "name".into()], Vec::new()).is_err());
        assert!(structured_source(vec!["Name".into()], vec![Vec::new()]).is_err());
        assert!(
            structured_source(vec!["Name".into()], vec![vec!["a".into(), "b".into()]]).is_err()
        );
    }
}
