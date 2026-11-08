//! Reading cell text as values: numbers, byte sizes, durations and dates.
//!
//! Shared by both directions: [`crate::column_type`] parses cells into sort
//! and filter keys (input), and [`crate::cell_format`] parses them again to
//! display them (output). Nothing here knows about columns or formats.
//!
//! Byte units are binary: `KB` and `KiB` are both 1024 bytes. A timestamp
//! without an offset is local time.

use chrono::{DateTime, FixedOffset, Local, NaiveDate, NaiveDateTime, TimeDelta, TimeZone};

/// A finite number. Rust's float syntax, without `NaN` or infinities.
pub fn parse_number(value: &str) -> Option<f64> {
    let number: f64 = value.trim().parse().ok()?;
    number.is_finite().then_some(number)
}

pub(crate) const BYTE_UNITS: [(&str, u64); 6] = [
    ("B", 1),
    ("KB", 1 << 10),
    ("MB", 1 << 20),
    ("GB", 1 << 30),
    ("TB", 1 << 40),
    ("PB", 1 << 50),
];

/// `B`, `K`, `KB`, `KiB`, ... `PB`, ignoring case; all binary.
pub(crate) fn byte_unit(text: &str) -> Option<u64> {
    let text = text.trim().to_ascii_uppercase();
    if text.is_empty() || text == "IB" {
        return None;
    }
    // `KiB`, `KB` and `K` all have the prefix `K`; `B` has none.
    let prefix = text
        .strip_suffix("IB")
        .or_else(|| text.strip_suffix('B'))
        .unwrap_or(&text);
    let power = ["", "K", "M", "G", "T", "P"]
        .iter()
        .position(|unit| *unit == prefix)?;
    Some(1 << (10 * power))
}

/// Bytes: a number of `unit`s, or a number and its own unit (`100MB`,
/// `1.5 GiB`).
pub fn parse_bytes(value: &str, unit: u64) -> Option<f64> {
    let value = value.trim();
    if let Some(number) = parse_number(value) {
        return Some(number * unit as f64);
    }
    let split = value.find(|c: char| c.is_ascii_alphabetic())?;
    let number = parse_number(&value[..split])?;
    Some(number * byte_unit(&value[split..])? as f64)
}

/// Seconds: a number, `[-][d.]hh:mm[:ss[.fff]]` (as .NET writes a
/// `TimeSpan`), or units such as `2h 13m`, `1d2h` or `1.5h`.
pub fn parse_duration(value: &str) -> Option<f64> {
    let value = value.trim();
    if let Some(seconds) = parse_number(value) {
        return Some(seconds);
    }
    let (sign, body) = match value.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, value),
    };
    let seconds = if body.contains(':') {
        parse_clock(body)?
    } else {
        parse_units(body)?
    };
    Some(sign * seconds)
}

fn parse_clock(body: &str) -> Option<f64> {
    let fields: Vec<_> = body.split(':').collect();
    if !(2..=3).contains(&fields.len()) {
        return None;
    }
    let (days, hours) = match fields[0].split_once('.') {
        Some((days, hours)) => {
            let hours = whole(hours)?;
            if hours >= 24.0 {
                return None;
            }
            (whole(days)?, hours)
        }
        None => (0.0, whole(fields[0])?),
    };
    let minutes = whole(fields[1])?;
    let seconds = fields.get(2).map_or(Some(0.0), |field| decimal(field))?;
    if minutes >= 60.0 || seconds >= 60.0 {
        return None;
    }
    Some(((days * 24.0 + hours) * 60.0 + minutes) * 60.0 + seconds)
}

fn parse_units(body: &str) -> Option<f64> {
    let mut rest = body.trim();
    if rest.is_empty() {
        return None;
    }
    let mut total = 0.0;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let number = decimal(&rest[..digits])?;
        rest = rest[digits..].trim_start();
        let letters = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let scale = match rest[..letters].to_ascii_lowercase().as_str() {
            "d" => 86_400.0,
            "h" => 3_600.0,
            "m" | "min" => 60.0,
            "s" => 1.0,
            "ms" => 0.001,
            _ => return None,
        };
        total += number * scale;
        rest = rest[letters..].trim_start();
    }
    Some(total)
}

/// ASCII digits only.
fn whole(text: &str) -> Option<f64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// ASCII digits with an optional fraction.
fn decimal(text: &str) -> Option<f64> {
    let (int, fraction) = text.split_once('.').unwrap_or((text, ""));
    whole(int)?;
    if !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// A date and time as written: with an offset, or without one (local time).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DateTimeValue {
    Offset(DateTime<FixedOffset>),
    Naive(NaiveDateTime),
}

const DATE_FORMATS: [&str; 2] = ["%Y-%m-%d", "%m/%d/%Y"];

impl DateTimeValue {
    /// RFC 3339, a date and time without an offset, or a date (midnight).
    pub fn parse(value: &str) -> Option<Self> {
        // Every accepted form starts with a digit; most text does not.
        if !value.as_bytes().first().is_some_and(u8::is_ascii_digit) {
            return None;
        }
        if let Ok(value) = DateTime::parse_from_rfc3339(value) {
            return Some(Self::Offset(value));
        }
        for format in [
            "%Y-%m-%d %H:%M:%S%.f",
            "%Y-%m-%dT%H:%M:%S%.f",
            "%Y-%m-%d %H:%M",
            "%m/%d/%Y %I:%M:%S %p",
            "%m/%d/%Y %I:%M %p",
            "%m/%d/%Y %H:%M:%S",
            "%m/%d/%Y %H:%M",
        ] {
            if let Ok(naive) = NaiveDateTime::parse_from_str(value, format) {
                return Some(Self::Naive(naive));
            }
        }
        parse_date(value).map(|date| Self::Naive(date.and_time(chrono::NaiveTime::MIN)))
    }

    /// The instant, reading a time without an offset as local time.
    pub fn instant(&self) -> DateTime<FixedOffset> {
        match *self {
            Self::Offset(time) => time,
            Self::Naive(naive) => match Local.from_local_datetime(&naive).earliest() {
                Some(time) => time.fixed_offset(),
                // A local time skipped by a daylight saving change: use the
                // offset in effect around it.
                None => {
                    let offset = Local.offset_from_utc_datetime(&naive);
                    let utc = naive - TimeDelta::seconds(offset.local_minus_utc().into());
                    offset.from_utc_datetime(&utc)
                }
            },
        }
    }
}

fn parse_date(value: &str) -> Option<NaiveDate> {
    DATE_FORMATS
        .iter()
        .find_map(|format| NaiveDate::parse_from_str(value, format).ok())
}

/// For a value that is only a date: the local day it covers, as Unix
/// seconds `[start, end)`.
pub fn day_span(value: &str) -> Option<(f64, f64)> {
    let date = parse_date(value.trim())?;
    let start = |date: NaiveDate| {
        seconds(DateTimeValue::Naive(date.and_time(chrono::NaiveTime::MIN)).instant())
    };
    Some((start(date), start(date.succ_opt()?)))
}

/// Unix seconds or milliseconds, or a [`DateTimeValue`].
pub fn parse_timestamp(value: &str) -> Option<DateTime<FixedOffset>> {
    let value = value.trim();
    parse_unix(value).or_else(|| DateTimeValue::parse(value).map(|value| value.instant()))
}

/// Unix seconds, or milliseconds when the number is too large to be seconds
/// (beyond the year 5138).
fn parse_unix(value: &str) -> Option<DateTime<FixedOffset>> {
    let digits = value.strip_prefix('-').unwrap_or(value);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let number: i64 = value.parse().ok()?;
    let time = if number.abs() >= 100_000_000_000 {
        DateTime::from_timestamp_millis(number)?
    } else {
        DateTime::from_timestamp(number, 0)?
    };
    Some(time.fixed_offset())
}

pub(crate) fn seconds(time: DateTime<FixedOffset>) -> f64 {
    time.timestamp() as f64 + f64::from(time.timestamp_subsec_nanos()) / 1e9
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_finite() {
        assert_eq!(parse_number(" 1e3 "), Some(1000.0));
        assert_eq!(parse_number("NaN"), None);
        assert_eq!(parse_number("inf"), None);
    }

    #[test]
    fn bytes_take_their_own_unit_or_the_columns() {
        assert_eq!(parse_bytes("512", 1), Some(512.0));
        assert_eq!(parse_bytes("512", 1024), Some(512.0 * 1024.0));
        assert_eq!(parse_bytes("100MB", 1024), Some(100.0 * 1048576.0));
        assert_eq!(parse_bytes("1.5 GiB", 1), Some(1.5 * 1073741824.0));
        assert_eq!(parse_bytes("2k", 1), Some(2048.0));
        assert_eq!(parse_bytes("12 apples", 1), None);
        assert_eq!(parse_bytes("MB", 1), None);
    }

    #[test]
    fn durations_are_seconds_clocks_or_units() {
        assert_eq!(parse_duration("90"), Some(90.0));
        assert_eq!(parse_duration("12:30"), Some(45_000.0));
        assert_eq!(parse_duration("1.02:00:00"), Some(93_600.0));
        // .NET TimeSpans carry a fraction of a second.
        assert_eq!(parse_duration("00:00:12.5"), Some(12.5));
        assert_eq!(parse_duration("-00:01:00"), Some(-60.0));
        assert_eq!(parse_duration("2h 13m"), Some(7_980.0));
        assert_eq!(parse_duration("1d2h"), Some(93_600.0));
        assert_eq!(parse_duration("1.5h"), Some(5_400.0));
        // Not durations: an unparsed day count is no longer read as zero.
        assert_eq!(parse_duration("abc.02:00"), None);
        assert_eq!(parse_duration("1:75"), None);
        assert_eq!(parse_duration("1.25:00:00"), None);
        assert_eq!(parse_duration("2 hours"), None);
        assert_eq!(parse_duration(""), None);
    }

    #[test]
    fn timestamps_without_an_offset_are_local_time() {
        let naive = NaiveDateTime::parse_from_str("2026-11-06 14:00", "%Y-%m-%d %H:%M").unwrap();
        let expected = Local.from_local_datetime(&naive).earliest().unwrap();
        let parsed = parse_timestamp("2026-11-06 14:00").unwrap();
        assert_eq!(parsed.timestamp(), expected.timestamp());
        // An offset is kept as given.
        let rfc = parse_timestamp("2026-11-06T14:00:00-08:00").unwrap();
        assert_eq!(rfc.offset().local_minus_utc(), -8 * 3600);
        assert_eq!(
            rfc.timestamp(),
            parse_timestamp("2026-11-06T22:00:00Z").unwrap().timestamp()
        );
        assert_eq!(
            parse_timestamp("2026-11-06 14:00:00.250")
                .unwrap()
                .timestamp_subsec_millis(),
            250
        );
    }

    #[test]
    fn unix_seconds_and_milliseconds_parse() {
        assert_eq!(
            parse_timestamp("1700000000").unwrap().timestamp(),
            1_700_000_000
        );
        assert_eq!(
            parse_timestamp("1700000000123").unwrap().timestamp(),
            1_700_000_000
        );
        assert!(parse_timestamp("12ab").is_none());
    }

    #[test]
    fn a_date_spans_its_local_day() {
        let (start, end) = day_span("2026-11-06").unwrap();
        assert_eq!(start, seconds(parse_timestamp("2026-11-06 00:00").unwrap()));
        assert_eq!(end, seconds(parse_timestamp("2026-11-07 00:00").unwrap()));
        assert_eq!(day_span("2026-11-06 12:00"), None);
    }
}
