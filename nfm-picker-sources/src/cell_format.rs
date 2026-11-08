//! Column formats: how a cell's value is shown. This is the output side; what
//! a cell's text means, for sorting and filtering, is [`crate::column_type`],
//! and the value parsers both use are in [`crate::value`].
//!
//! | Spec | Value | Shown as |
//! |---|---|---|
//! | `date` | a timestamp | `2026-11-06 14:05`, local time |
//! | `date=FORMAT` | a timestamp | chrono `strftime` FORMAT, local time |
//! | `duration` | a duration | `2h 13m` |
//! | `number`, `number=N` | a number | `1,234,568`; N decimals |
//! | `bytes` | a size | `117.7 MB` |
//! | `percent`, `percent=N` | a fraction | `15.3%`; N decimals (default 1) |
//! | `devicon` | a path or file name | its file-type glyph icon |
//!
//! A column has one format, and its formatted text is what the table and
//! cards show and what fuzzy search reads (stored when the row arrives).
//! Sorting, filters and the selected value use the raw text. A host can also
//! format a column with its own function ([`CustomFormat`]), for example to
//! show a timestamp as "9 minutes ago".
//!
//! Values are read as the column's type reads them (see
//! [`crate::column_type`]): a `bytes` column's plain numbers are in its unit,
//! for example. A timestamp may always be Unix seconds or milliseconds here.
//! A value that does not parse is shown as it is. A format only applies to
//! columns of a type it can read; `auto` and `text` columns take any format.
//!
//! `devicon` is the one card slot format: it turns a path into an icon
//! reference (see `nfm_search_core::icon`) for an icon slot. It always
//! produces one, so a raw value never reaches an icon slot as a reference;
//! an empty value shows no icon.

use crate::column_type::{ColumnType, Key};
use crate::value::{parse_bytes, parse_duration, parse_number, parse_timestamp};
use chrono::Local;
use chrono::format::{Item, StrftimeItems};
use nfm_search_core::row::RowFormat;
use std::sync::Arc;

const DEFAULT_DATE_FORMAT: &str = "%Y-%m-%d %H:%M";
const FORMATS: &str = concat!(
    "date, date=FORMAT, duration, number, number=N, bytes, percent, percent=N ",
    "and devicon"
);

/// A host's own column format: the trimmed raw cell in, the text to show and
/// search out. It runs when a row arrives (for search) and again when the row
/// is shown, so it should be cheap. A picker is not open long enough for a
/// relative time to drift between the two.
#[derive(Clone)]
pub struct CustomFormat(Arc<dyn Fn(&str) -> String + Send + Sync>);

impl CustomFormat {
    pub fn new(format: impl Fn(&str) -> String + Send + Sync + 'static) -> Self {
        Self(Arc::new(format))
    }
}

impl std::fmt::Debug for CustomFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CustomFormat")
    }
}

/// The same function, not an equal one.
impl PartialEq for CustomFormat {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for CustomFormat {}

/// A compiled format.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CellFormat {
    /// A validated strftime format.
    Date(String),
    Duration,
    Number {
        decimals: usize,
    },
    Bytes,
    Percent {
        decimals: usize,
    },
    Devicon,
    Custom(CustomFormat),
}

impl CellFormat {
    pub fn compile(format: &RowFormat) -> Result<Self, String> {
        let argument = format.argument.as_deref();
        let decimals = |name: &str, argument: &str| match argument.parse::<usize>() {
            Ok(decimals) if decimals <= 9 => Ok(decimals),
            _ => Err(format!(
                "{name} takes a number of decimals from 0 to 9, not '{argument}'"
            )),
        };
        let no_argument = |name: &str, format: Self| match argument {
            None => Ok(format),
            Some(_) => Err(format!("{name} takes no argument")),
        };
        match (format.name.as_str(), argument) {
            ("date", None) => Ok(Self::Date(DEFAULT_DATE_FORMAT.into())),
            ("date", Some(pattern)) => {
                // chrono panics when displaying an invalid pattern, so it is
                // checked here, once.
                if pattern.is_empty()
                    || StrftimeItems::new(pattern).any(|item| matches!(item, Item::Error))
                {
                    Err(format!("'{pattern}' is not a valid date format"))
                } else {
                    Ok(Self::Date(pattern.into()))
                }
            }
            ("number", None) => Ok(Self::Number { decimals: 0 }),
            ("number", Some(n)) => Ok(Self::Number {
                decimals: decimals("number", n)?,
            }),
            ("percent", None) => Ok(Self::Percent { decimals: 1 }),
            ("percent", Some(n)) => Ok(Self::Percent {
                decimals: decimals("percent", n)?,
            }),
            ("duration", _) => no_argument("duration", Self::Duration),
            ("bytes", _) => no_argument("bytes", Self::Bytes),
            ("devicon", _) => no_argument("devicon", Self::Devicon),
            (name, _) => Err(format!("unknown format '{name}'; formats are {FORMATS}")),
        }
    }

    /// The format a column of this type gets when it has none.
    pub fn default_for(column: ColumnType) -> Option<Self> {
        match column {
            ColumnType::DateTime => Some(Self::Date(DEFAULT_DATE_FORMAT.into())),
            ColumnType::Duration => Some(Self::Duration),
            ColumnType::Bytes { .. } => Some(Self::Bytes),
            ColumnType::Auto | ColumnType::Text | ColumnType::Number => None,
        }
    }

    /// Whether this format can read a column of this type.
    pub fn accepts(&self, column: ColumnType) -> bool {
        let numeric = matches!(
            column,
            ColumnType::Number | ColumnType::Bytes { .. } | ColumnType::Duration
        );
        matches!(column, ColumnType::Auto | ColumnType::Text)
            || match self {
                Self::Date(_) => matches!(column, ColumnType::Number | ColumnType::DateTime),
                Self::Duration => matches!(column, ColumnType::Number | ColumnType::Duration),
                Self::Bytes => matches!(column, ColumnType::Number | ColumnType::Bytes { .. }),
                Self::Number { .. } | Self::Percent { .. } => numeric,
                Self::Devicon => false,
                Self::Custom(_) => true,
            }
    }

    /// Format `value`, read as `column` reads it, or None if it does not
    /// parse.
    pub fn apply(&self, value: &str, column: ColumnType) -> Option<String> {
        let value = value.trim();
        match self {
            Self::Date(pattern) => Some(
                parse_timestamp(value)?
                    .with_timezone(&Local)
                    .format(pattern)
                    .to_string(),
            ),
            Self::Duration => Some(format_duration(parse_duration(value)?.round() as i64)),
            Self::Number { decimals } => Some(format_number(numeric(value, column)?, *decimals)),
            Self::Bytes => {
                let unit = match column {
                    ColumnType::Bytes { unit } => unit,
                    _ => 1,
                };
                Some(format_bytes(parse_bytes(value, unit)?))
            }
            Self::Percent { decimals } => Some(format!(
                "{}%",
                format_number(numeric(value, column)? * 100.0, *decimals)
            )),
            Self::Devicon => Some(if value.is_empty() {
                String::new()
            } else {
                crate::devicon::lookup(value).reference()
            }),
            Self::Custom(format) => Some((format.0)(value)),
        }
    }
}

/// A number as `column` reads it (bytes in its unit, durations in seconds).
fn numeric(value: &str, column: ColumnType) -> Option<f64> {
    match column.key(value) {
        Key::Number(number) => Some(number),
        _ => parse_number(value),
    }
}

/// The two largest units: `45s`, `2m 13s`, `2h 13m`, `3d 4h`.
fn format_duration(seconds: i64) -> String {
    let sign = if seconds < 0 { "-" } else { "" };
    let seconds = seconds.unsigned_abs();
    let units = [(86_400, "d"), (3_600, "h"), (60, "m"), (1, "s")];
    let Some(first) = units.iter().position(|&(size, _)| seconds >= size) else {
        return "0s".into();
    };
    let (size, unit) = units[first];
    let mut text = format!("{sign}{}{unit}", seconds / size);
    if let Some(&(next_size, next_unit)) = units.get(first + 1) {
        let rest = seconds % size / next_size;
        if rest > 0 {
            text.push_str(&format!(" {rest}{next_unit}"));
        }
    }
    text
}

/// `1,234,567.89`: thousands grouped with `,`, `decimals` places after `.`.
fn format_number(value: f64, decimals: usize) -> String {
    let text = format!("{:.*}", decimals, value.abs());
    let (int, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let mut out = String::new();
    // Rounding can leave nothing but zeros, which take no sign.
    if value < 0.0 && text.bytes().any(|b| matches!(b, b'1'..=b'9')) {
        out.push('-');
    }
    for (index, digit) in int.chars().enumerate() {
        if index > 0 && (int.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
    if !fraction.is_empty() {
        out.push('.');
        out.push_str(fraction);
    }
    out
}

/// `512 B`, `117.7 MB`: binary units with one decimal.
fn format_bytes(bytes: f64) -> String {
    if bytes.abs() < 1024.0 {
        return format!("{} B", format_number(bytes, 0));
    }
    let units = ["KB", "MB", "GB", "TB", "PB"];
    let mut scaled = bytes / 1024.0;
    let mut unit = 0;
    // Step up when one decimal would round to 1024 (1023.97 KB is 1.0 MB).
    while unit + 1 < units.len() && (scaled.abs() * 10.0).round() >= 10_240.0 {
        scaled /= 1024.0;
        unit += 1;
    }
    format!("{} {}", format_number(scaled, 1), units[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile(name: &str, argument: Option<&str>) -> Result<CellFormat, String> {
        CellFormat::compile(&match argument {
            Some(argument) => RowFormat::with_argument(name, argument),
            None => RowFormat::new(name),
        })
    }

    /// Apply `format` to `value` in an `auto` column.
    fn auto(format: &CellFormat, value: &str) -> Option<String> {
        format.apply(value, ColumnType::Auto)
    }

    #[test]
    fn devicon_turns_paths_into_glyph_references() {
        let devicon = compile("devicon", None).unwrap();
        assert_eq!(
            auto(&devicon, r"C:\src\main.rs"),
            Some(crate::devicon::lookup("main.rs").reference())
        );
        assert_eq!(
            auto(&devicon, "src/"),
            Some(crate::devicon::FOLDER.reference())
        );
        assert_eq!(auto(&devicon, "").as_deref(), Some(""));
        assert!(compile("devicon", Some("x")).is_err());
    }

    #[test]
    fn compiles_known_formats_and_rejects_others() {
        assert_eq!(
            compile("date", None),
            Ok(CellFormat::Date(DEFAULT_DATE_FORMAT.into()))
        );
        assert_eq!(
            compile("number", Some("2")),
            Ok(CellFormat::Number { decimals: 2 })
        );
        assert_eq!(
            compile("percent", None),
            Ok(CellFormat::Percent { decimals: 1 })
        );
        assert!(compile("date", Some("%Y-%")).is_err());
        // Relative times are a host's own format, not a built-in one.
        assert!(compile("time", None).is_err());
        assert!(compile("duration", Some("x")).is_err());
        assert!(compile("bytes", Some("kb")).is_err());
        assert!(compile("number", Some("12")).is_err());
        assert!(compile("number", Some("x")).is_err());
        let error = compile("mutd", None).unwrap_err();
        assert!(error.contains("unknown format 'mutd'"), "{error}");
    }

    #[test]
    fn formats_apply_to_the_column_types_they_can_read() {
        let bytes = ColumnType::Bytes { unit: 1 };
        for format in [
            "date", "duration", "number", "bytes", "percent", "devicon",
        ] {
            let format = compile(format, None).unwrap();
            assert!(format.accepts(ColumnType::Auto));
            assert!(format.accepts(ColumnType::Text));
        }
        assert!(compile("date", None).unwrap().accepts(ColumnType::DateTime));
        assert!(compile("date", None).unwrap().accepts(ColumnType::Number));
        assert!(!compile("date", None).unwrap().accepts(bytes));
        assert!(
            !compile("bytes", None)
                .unwrap()
                .accepts(ColumnType::DateTime)
        );
        assert!(
            !compile("bytes", None)
                .unwrap()
                .accepts(ColumnType::Duration)
        );
        assert!(compile("number", None).unwrap().accepts(bytes));
        assert!(
            !compile("number", None)
                .unwrap()
                .accepts(ColumnType::DateTime)
        );
        assert!(
            !compile("devicon", None)
                .unwrap()
                .accepts(ColumnType::Number)
        );
        assert_eq!(
            CellFormat::default_for(ColumnType::DateTime),
            Some(CellFormat::Date(DEFAULT_DATE_FORMAT.into()))
        );
        assert_eq!(CellFormat::default_for(bytes), Some(CellFormat::Bytes));
        assert_eq!(CellFormat::default_for(ColumnType::Number), None);
    }

    #[test]
    fn time_formats_apply_and_unparseable_values_are_none() {
        let date = CellFormat::Date("%Y".into());
        assert_eq!(auto(&date, "2026-06-15 12:00").as_deref(), Some("2026"));
        assert_eq!(auto(&date, "sometime"), None);
        let duration = |value| auto(&CellFormat::Duration, value);
        assert_eq!(duration("7980").as_deref(), Some("2h 13m"));
        assert_eq!(duration("1.02:00:00").as_deref(), Some("1d 2h"));
        assert_eq!(duration("00:02:13.4").as_deref(), Some("2m 13s"));
        assert_eq!(duration("45").as_deref(), Some("45s"));
        assert_eq!(duration("0").as_deref(), Some("0s"));
        assert_eq!(duration("120").as_deref(), Some("2m"));
        assert_eq!(duration("soon"), None);
    }

    #[test]
    fn custom_formats_take_any_column_and_always_produce_text() {
        let upper = CellFormat::Custom(CustomFormat::new(|value| value.to_uppercase()));
        assert!(upper.accepts(ColumnType::DateTime));
        assert!(upper.accepts(ColumnType::Bytes { unit: 1 }));
        assert_eq!(auto(&upper, " done ").as_deref(), Some("DONE"));
        // Equal when they are the same function.
        assert_eq!(upper, upper.clone());
        assert_ne!(
            upper,
            CellFormat::Custom(CustomFormat::new(|value| value.to_uppercase()))
        );
    }

    #[test]
    fn numbers_group_thousands() {
        assert_eq!(format_number(1_234_567.891, 0), "1,234,568");
        assert_eq!(format_number(1_234.5, 2), "1,234.50");
        assert_eq!(format_number(-1_000.0, 0), "-1,000");
        assert_eq!(format_number(999.0, 0), "999");
        assert_eq!(format_number(-0.4, 0), "0");
        let number = CellFormat::Number { decimals: 1 };
        assert_eq!(auto(&number, "1234.56").as_deref(), Some("1,234.6"));
        assert_eq!(auto(&number, "many"), None);
        // A bytes column's number is its byte count.
        let kb = ColumnType::Bytes { unit: 1024 };
        assert_eq!(
            CellFormat::Number { decimals: 0 }
                .apply("2", kb)
                .as_deref(),
            Some("2,048")
        );
        let percent = CellFormat::Percent { decimals: 1 };
        assert_eq!(auto(&percent, "0.153").as_deref(), Some("15.3%"));
    }

    #[test]
    fn bytes_scale_by_1024_in_the_columns_unit() {
        assert_eq!(format_bytes(512.0), "512 B");
        assert_eq!(format_bytes(1024.0), "1.0 KB");
        assert_eq!(format_bytes(123_456_789.0), "117.7 MB");
        assert_eq!(format_bytes(1023.97 * 1024.0), "1.0 MB");
        assert_eq!(format_bytes(-2048.0), "-2.0 KB");
        let bytes = CellFormat::Bytes;
        assert_eq!(auto(&bytes, "1.5GB").as_deref(), Some("1.5 GB"));
        assert_eq!(
            bytes
                .apply("123456", ColumnType::Bytes { unit: 1024 })
                .as_deref(),
            Some("120.6 MB")
        );
        assert_eq!(auto(&bytes, "lots"), None);
    }
}
