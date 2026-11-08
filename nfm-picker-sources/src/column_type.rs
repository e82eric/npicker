//! Column types for structured sources: what a cell's text means, for sorting and filtering.
//!
//! | Type | Values | Sorted and compared as |
//! |---|---|---|
//! | `auto` (default) | a number, else a date and time, else text | numbers, then times, then text |
//! | `text` | anything | text, ignoring case |
//! | `number` | `42`, `-1.5`, `1e5` | numbers |
//! | `bytes`, `bytes=UNIT` | `1234`, `100MB`, `1.5 GiB`; plain numbers are in UNIT (default bytes) | bytes |
//! | `duration` | seconds, `[d.]hh:mm[:ss[.fff]]` or `2h 13m` | seconds |
//! | `datetime` | RFC 3339, a date and time without an offset (local time), a date, or Unix seconds or milliseconds | instants |
//!
//! Each cell is parsed once, when its row is added, into a [`Key`]. A value
//! its column's type cannot parse (including an empty one) is text, which
//! sorts after every parsed value in either direction. `NaN` and infinities
//! are not numbers.
//!
//! In `auto` columns a plain number is always a number, never a Unix time,
//! and clock-like values such as `12:30` are text; declare the column
//! `datetime` or `duration` to read them as such.
//!
//! This is the input side. How a cell is shown is [`crate::cell_format`];
//! the value parsers both use are in [`crate::value`].

use crate::value::{
    BYTE_UNITS, DateTimeValue, byte_unit, parse_bytes, parse_duration, parse_number,
    parse_timestamp, seconds,
};
use std::cmp::Ordering;
use std::fmt;

const TYPES: &str = "auto, text, number, bytes, bytes=UNIT, duration and datetime";

/// How a column's values are parsed, sorted and filtered.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Hash)]
pub enum ColumnType {
    #[default]
    Auto,
    Text,
    Number,
    /// Sizes. A plain number is `unit` bytes; a value may give its own unit.
    Bytes {
        unit: u64,
    },
    Duration,
    DateTime,
}

impl ColumnType {
    /// `auto`, `text`, `number`, `bytes`, `bytes=KB`, `duration` or
    /// `datetime`, ignoring case.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        let (name, argument) = match spec.split_once('=') {
            Some((name, argument)) => (name.trim(), Some(argument.trim())),
            None => (spec, None),
        };
        let name = name.to_ascii_lowercase();
        let plain = match name.as_str() {
            "auto" => Self::Auto,
            "text" => Self::Text,
            "number" => Self::Number,
            "duration" => Self::Duration,
            "datetime" => Self::DateTime,
            "bytes" => {
                return match argument {
                    None => Ok(Self::Bytes { unit: 1 }),
                    Some(unit) => byte_unit(unit)
                        .map(|unit| Self::Bytes { unit })
                        .ok_or_else(|| format!("'{unit}' is not a byte unit such as B, KB or MB")),
                };
            }
            _ => return Err(format!("unknown column type '{spec}'; types are {TYPES}")),
        };
        match argument {
            None => Ok(plain),
            Some(_) => Err(format!("{name} takes no argument")),
        }
    }

    /// Parse a cell (or a filter value) into its sortable key.
    pub fn key(self, value: &str) -> Key {
        let value = value.trim();
        let key = match self {
            Self::Auto => parse_number(value).map(Key::Number).or_else(|| {
                DateTimeValue::parse(value).map(|time| Key::Time(seconds(time.instant())))
            }),
            Self::Text => None,
            Self::Number => parse_number(value).map(Key::Number),
            Self::Bytes { unit } => parse_bytes(value, unit).map(Key::Number),
            Self::Duration => parse_duration(value).map(Key::Number),
            Self::DateTime => parse_timestamp(value).map(|time| Key::Time(seconds(time))),
        };
        key.unwrap_or(Key::Text)
    }

    /// Whether values are dates and times, so a filter value that is only a
    /// date means the whole day.
    pub fn has_dates(self) -> bool {
        matches!(self, Self::Auto | Self::DateTime)
    }
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Text => f.write_str("text"),
            Self::Number => f.write_str("number"),
            Self::Bytes { unit: 1 } => f.write_str("bytes"),
            Self::Bytes { unit } => match BYTE_UNITS.iter().find(|(_, size)| size == unit) {
                Some((name, _)) => write!(f, "bytes={name}"),
                None => write!(f, "bytes={unit}"),
            },
            Self::Duration => f.write_str("duration"),
            Self::DateTime => f.write_str("datetime"),
        }
    }
}

/// A cell's value for sorting and filtering.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Key {
    /// Not parsed by the column's type; compared as text.
    #[default]
    Text,
    /// A number, byte count or duration in seconds.
    Number(f64),
    /// Unix seconds.
    Time(f64),
}

impl Key {
    /// Order two keys: parsed values by value (reversed when `descending`),
    /// then text, which always sorts after values. `text` compares two text
    /// keys; it is only called for them.
    pub fn order(self, other: Self, descending: bool, text: impl FnOnce() -> Ordering) -> Ordering {
        let direction = |ordering: Ordering| {
            if descending {
                ordering.reverse()
            } else {
                ordering
            }
        };
        match (self, other) {
            (Self::Number(left), Self::Number(right)) | (Self::Time(left), Self::Time(right)) => {
                direction(left.total_cmp(&right))
            }
            (Self::Text, Self::Text) => direction(text()),
            _ => self.rank().cmp(&other.rank()),
        }
    }

    /// The value of a parsed key, if `other` is the same kind.
    pub fn same_kind(self, other: Self) -> Option<(f64, f64)> {
        match (self, other) {
            (Self::Number(left), Self::Number(right)) | (Self::Time(left), Self::Time(right)) => {
                Some((left, right))
            }
            _ => None,
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Number(_) => 0,
            Self::Time(_) => 1,
            Self::Text => 2,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_parse_and_display() {
        assert_eq!(ColumnType::parse("Number"), Ok(ColumnType::Number));
        assert_eq!(
            ColumnType::parse("bytes"),
            Ok(ColumnType::Bytes { unit: 1 })
        );
        assert_eq!(
            ColumnType::parse("bytes=KiB"),
            Ok(ColumnType::Bytes { unit: 1024 })
        );
        assert!(ColumnType::parse("bytes=furlongs").is_err());
        assert!(ColumnType::parse("number=2").is_err());
        let error = ColumnType::parse("money").unwrap_err();
        assert!(error.contains("unknown column type 'money'"), "{error}");
        for spec in [
            "auto", "text", "number", "bytes", "bytes=KB", "duration", "datetime",
        ] {
            assert_eq!(ColumnType::parse(spec).unwrap().to_string(), spec);
        }
    }

    #[test]
    fn non_finite_numbers_are_text() {
        assert_eq!(ColumnType::Number.key("nan"), Key::Text);
    }

    #[test]
    fn auto_reads_numbers_then_times_and_never_unix_or_durations() {
        assert_eq!(ColumnType::Auto.key("1700000000"), Key::Number(1.7e9));
        assert!(matches!(ColumnType::Auto.key("2026-11-06"), Key::Time(_)));
        assert_eq!(ColumnType::Auto.key("12:30"), Key::Text);
        assert_eq!(ColumnType::Auto.key("notepad.exe"), Key::Text);
        assert!(matches!(ColumnType::DateTime.key("1700000000"), Key::Time(t) if t == 1.7e9));
        assert_eq!(ColumnType::Text.key("42"), Key::Text);
    }

    #[test]
    fn keys_order_values_before_text_in_both_directions() {
        let text = || Ordering::Equal;
        let (one, two) = (Key::Number(1.0), Key::Number(2.0));
        assert_eq!(one.order(two, false, text), Ordering::Less);
        assert_eq!(one.order(two, true, text), Ordering::Greater);
        assert_eq!(one.order(Key::Text, false, text), Ordering::Less);
        assert_eq!(one.order(Key::Text, true, text), Ordering::Less);
        assert_eq!(Key::Time(0.0).order(one, false, text), Ordering::Greater);
        assert_eq!(
            Key::Text.order(Key::Text, true, || Ordering::Less),
            Ordering::Greater
        );
    }

}
