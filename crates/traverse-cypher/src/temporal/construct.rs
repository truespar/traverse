//! The constructor functions (`date()`, `datetime()`, `duration()`, ...) and `format()` with Java
//! DateTimeFormatter patterns.

use std::fmt;

use chrono::NaiveDate;

use crate::error::CypherError;
use crate::types::CypherValue;

use super::clock::{
    func_date_now, func_datetime_now, func_localdatetime_now, func_localtime_now, func_time_now,
};
use super::map::{
    date_from_map, datetime_from_map, duration_from_map, localdatetime_from_map,
    localtime_from_map, time_from_map,
};
use super::parse::{
    parse_date_string, parse_datetime_string, parse_duration_string, parse_localdatetime_string,
    parse_localtime_string, parse_time_string,
};
use super::types::{CypherDate, CypherDateTime, CypherLocalDateTime, CypherLocalTime, CypherTime};
use super::{NANOS_PER_SECOND, err};

// -- Java DateTimeFormatter patterns ------------------------------
// For Cypher 25 `format(temporal, pattern)` (Neo4j 2025.09) and the 2-arg
// constructor form `date('15/01/2024','dd/MM/yyyy')` (Neo4j 2025.10).

/// Java `DateTimeFormatter` pattern -> chrono strftime: common field letters, quoted literals
/// (`'...'`) verbatim, unknown letters dropped.
fn java_pattern_to_chrono(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            i += 1;
            if i < chars.len() && chars[i] == '\'' {
                out.push('\'');
                i += 1;
                continue;
            }
            while i < chars.len() && chars[i] != '\'' {
                if chars[i] == '%' {
                    out.push('%');
                }
                out.push(chars[i]);
                i += 1;
            }
            i += 1; // closing quote
            continue;
        }
        if c.is_ascii_alphabetic() {
            let mut j = i;
            while j < chars.len() && chars[j] == c {
                j += 1;
            }
            out.push_str(map_java_token(c, j - i));
            i = j;
            continue;
        }
        if c == '%' {
            out.push('%');
        }
        out.push(c);
        i += 1;
    }
    out
}

fn map_java_token(c: char, n: usize) -> &'static str {
    match (c, n) {
        ('y', 2) | ('u', 2) => "%y",
        ('y', _) | ('u', _) => "%Y",
        ('M', 1) | ('L', 1) => "%-m",
        ('M', 2) | ('L', 2) => "%m",
        ('M', 3) | ('L', 3) => "%b",
        ('M', _) | ('L', _) => "%B",
        ('d', 1) => "%-d",
        ('d', _) => "%d",
        ('H', 1) => "%-H",
        ('H', _) => "%H",
        ('h', 1) => "%-I",
        ('h', _) => "%I",
        ('m', _) => "%M",
        ('s', _) => "%S",
        ('S', _) => "%3f",
        ('a', _) => "%p",
        ('E', 4) => "%A",
        ('E', _) => "%a",
        ('D', _) => "%j",
        ('z', _) | ('Z', _) | ('X', _) | ('x', _) => "%z",
        _ => "",
    }
}

fn naive_time_from_nanos(nanos: u64) -> chrono::NaiveTime {
    let secs = (nanos / NANOS_PER_SECOND) as u32;
    let frac = (nanos % NANOS_PER_SECOND) as u32;
    chrono::NaiveTime::from_num_seconds_from_midnight_opt(secs, frac).unwrap_or_default()
}

fn nanos_of_naivetime(t: chrono::NaiveTime) -> u64 {
    use chrono::Timelike;
    t.num_seconds_from_midnight() as u64 * NANOS_PER_SECOND + t.nanosecond() as u64
}

fn render_format(df: impl fmt::Display) -> Result<String, CypherError> {
    use std::fmt::Write;
    let mut s = String::new();
    write!(s, "{df}").map_err(|_| err("format() pattern is incompatible with the value"))?;
    Ok(s)
}

/// `format(temporal, pattern)` - Cypher 25 (Neo4j 2025.09).
pub fn func_format(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    if args.len() != 2 {
        return Err(err("format() expects (temporal, pattern)"));
    }
    if args[0].is_null() || args[1].is_null() {
        return Ok(CypherValue::Null);
    }
    let pattern = match &args[1] {
        CypherValue::String(s) => s,
        _ => return Err(err("format() pattern must be a string")),
    };
    let fmt = java_pattern_to_chrono(pattern);
    use chrono::format::StrftimeItems;
    let out = match &args[0] {
        CypherValue::Date(d) => render_format(d.date.format_with_items(StrftimeItems::new(&fmt)))?,
        CypherValue::LocalDateTime(x) => render_format(
            x.date
                .and_time(naive_time_from_nanos(x.nanos))
                .format_with_items(StrftimeItems::new(&fmt)),
        )?,
        CypherValue::DateTime(x) => render_format(
            x.date
                .and_time(naive_time_from_nanos(x.nanos))
                .format_with_items(StrftimeItems::new(&fmt)),
        )?,
        CypherValue::LocalTime(x) => render_format(
            naive_time_from_nanos(x.nanos).format_with_items(StrftimeItems::new(&fmt)),
        )?,
        CypherValue::Time(x) => render_format(
            naive_time_from_nanos(x.nanos).format_with_items(StrftimeItems::new(&fmt)),
        )?,
        other => {
            return Err(err(&format!(
                "format() requires a temporal value, got {}",
                other.type_name()
            )));
        }
    };
    Ok(CypherValue::String(out))
}

fn parse_date_with_pattern(s: &str, pattern: &str) -> Result<NaiveDate, CypherError> {
    let fmt = java_pattern_to_chrono(pattern);
    NaiveDate::parse_from_str(s, &fmt).map_err(|e| {
        err(&format!(
            "date(): cannot parse '{s}' with pattern '{pattern}': {e}"
        ))
    })
}

fn parse_naivedatetime_with_pattern(
    s: &str,
    pattern: &str,
) -> Result<chrono::NaiveDateTime, CypherError> {
    let fmt = java_pattern_to_chrono(pattern);
    chrono::NaiveDateTime::parse_from_str(s, &fmt)
        .map_err(|e| err(&format!("cannot parse '{s}' with pattern '{pattern}': {e}")))
}

pub fn func_date(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    if args.is_empty() {
        return func_date_now(args);
    }
    if args.len() == 2 {
        return match (&args[0], &args[1]) {
            (CypherValue::Null, _) | (_, CypherValue::Null) => Ok(CypherValue::Null),
            (CypherValue::String(s), CypherValue::String(p)) => Ok(CypherValue::Date(CypherDate {
                date: parse_date_with_pattern(s, p)?,
                cycles: 0,
            })),
            _ => Err(err("date(string, pattern) requires string arguments")),
        };
    }
    if args.len() != 1 {
        return Err(err("date() expects 0, 1, or 2 arguments"));
    }
    match &args[0] {
        CypherValue::Null => Ok(CypherValue::Null),
        CypherValue::String(s) => parse_date_string(s),
        CypherValue::Map(m) => date_from_map(m),
        CypherValue::Date(_) => Ok(args[0].clone()),
        CypherValue::DateTime(dt) => Ok(CypherValue::Date(CypherDate {
            date: dt.date,
            cycles: 0,
        })),
        CypherValue::LocalDateTime(ldt) => Ok(CypherValue::Date(CypherDate {
            date: ldt.date,
            cycles: ldt.cycles,
        })),
        _ => Err(err(&format!(
            "date() cannot convert {} to date",
            args[0].type_name()
        ))),
    }
}

pub fn func_localtime(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    if args.is_empty() {
        return func_localtime_now(args);
    }
    if args.len() != 1 {
        return Err(err("localtime() expects 0 or 1 argument"));
    }
    match &args[0] {
        CypherValue::Null => Ok(CypherValue::Null),
        CypherValue::String(s) => parse_localtime_string(s),
        CypherValue::Map(m) => localtime_from_map(m),
        CypherValue::LocalTime(_) => Ok(args[0].clone()),
        CypherValue::Time(t) => Ok(CypherValue::LocalTime(CypherLocalTime { nanos: t.nanos })),
        CypherValue::LocalDateTime(ldt) => {
            Ok(CypherValue::LocalTime(CypherLocalTime { nanos: ldt.nanos }))
        }
        CypherValue::DateTime(dt) => {
            Ok(CypherValue::LocalTime(CypherLocalTime { nanos: dt.nanos }))
        }
        _ => Err(err(&format!(
            "localtime() cannot convert {} to localtime",
            args[0].type_name()
        ))),
    }
}

pub fn func_time(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    if args.is_empty() {
        return func_time_now(args);
    }
    if args.len() != 1 {
        return Err(err("time() expects 0 or 1 argument"));
    }
    match &args[0] {
        CypherValue::Null => Ok(CypherValue::Null),
        CypherValue::String(s) => parse_time_string(s),
        CypherValue::Map(m) => time_from_map(m),
        CypherValue::Time(_) => Ok(args[0].clone()),
        CypherValue::LocalTime(lt) => Ok(CypherValue::Time(CypherTime {
            nanos: lt.nanos,
            offset_secs: 0,
        })),
        CypherValue::DateTime(dt) => Ok(CypherValue::Time(CypherTime {
            nanos: dt.nanos,
            offset_secs: dt.offset_secs,
        })),
        CypherValue::LocalDateTime(ldt) => Ok(CypherValue::Time(CypherTime {
            nanos: ldt.nanos,
            offset_secs: 0,
        })),
        _ => Err(err(&format!(
            "time() cannot convert {} to time",
            args[0].type_name()
        ))),
    }
}

pub fn func_localdatetime(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    if args.is_empty() {
        return func_localdatetime_now(args);
    }
    if args.len() == 2 {
        return match (&args[0], &args[1]) {
            (CypherValue::Null, _) | (_, CypherValue::Null) => Ok(CypherValue::Null),
            (CypherValue::String(s), CypherValue::String(p)) => {
                let ndt = parse_naivedatetime_with_pattern(s, p)?;
                Ok(CypherValue::LocalDateTime(CypherLocalDateTime {
                    date: ndt.date(),
                    nanos: nanos_of_naivetime(ndt.time()),
                    cycles: 0,
                }))
            }
            _ => Err(err(
                "localdatetime(string, pattern) requires string arguments",
            )),
        };
    }
    if args.len() != 1 {
        return Err(err("localdatetime() expects 0, 1, or 2 arguments"));
    }
    match &args[0] {
        CypherValue::Null => Ok(CypherValue::Null),
        CypherValue::String(s) => parse_localdatetime_string(s),
        CypherValue::Map(m) => localdatetime_from_map(m),
        CypherValue::LocalDateTime(_) => Ok(args[0].clone()),
        CypherValue::DateTime(dt) => Ok(CypherValue::LocalDateTime(CypherLocalDateTime {
            date: dt.date,
            nanos: dt.nanos,
            cycles: 0,
        })),
        CypherValue::Date(d) => Ok(CypherValue::LocalDateTime(CypherLocalDateTime {
            date: d.date,
            nanos: 0,
            cycles: d.cycles,
        })),
        _ => Err(err(&format!(
            "localdatetime() cannot convert {} to localdatetime",
            args[0].type_name()
        ))),
    }
}

pub fn func_datetime(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    if args.is_empty() {
        return func_datetime_now(args);
    }
    if args.len() == 2 {
        return match (&args[0], &args[1]) {
            (CypherValue::Null, _) | (_, CypherValue::Null) => Ok(CypherValue::Null),
            (CypherValue::String(s), CypherValue::String(p)) => {
                let ndt = parse_naivedatetime_with_pattern(s, p)?;
                Ok(CypherValue::DateTime(CypherDateTime {
                    date: ndt.date(),
                    nanos: nanos_of_naivetime(ndt.time()),
                    offset_secs: 0,
                    tz_name: None,
                }))
            }
            _ => Err(err("datetime(string, pattern) requires string arguments")),
        };
    }
    if args.len() != 1 {
        return Err(err("datetime() expects 0, 1, or 2 arguments"));
    }
    match &args[0] {
        CypherValue::Null => Ok(CypherValue::Null),
        CypherValue::String(s) => parse_datetime_string(s),
        CypherValue::Map(m) => datetime_from_map(m),
        CypherValue::DateTime(_) => Ok(args[0].clone()),
        CypherValue::Date(d) => Ok(CypherValue::DateTime(CypherDateTime {
            date: d.date,
            nanos: 0,
            offset_secs: 0,
            tz_name: None,
        })),
        CypherValue::LocalDateTime(ldt) => Ok(CypherValue::DateTime(CypherDateTime {
            date: ldt.date,
            nanos: ldt.nanos,
            offset_secs: 0,
            tz_name: None,
        })),
        _ => Err(err(&format!(
            "datetime() cannot convert {} to datetime",
            args[0].type_name()
        ))),
    }
}

pub fn func_duration(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    if args.is_empty() {
        return Err(err("duration() requires an argument"));
    }
    if args.len() != 1 {
        return Err(err("duration() expects 1 argument"));
    }
    match &args[0] {
        CypherValue::Null => Ok(CypherValue::Null),
        CypherValue::String(s) => parse_duration_string(s),
        CypherValue::Map(m) => duration_from_map(m),
        CypherValue::Duration(_) => Ok(args[0].clone()),
        _ => Err(err(&format!(
            "duration() cannot convert {} to duration",
            args[0].type_name()
        ))),
    }
}
