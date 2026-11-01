//! `apoc.date.*` and `apoc.temporal.*`: epoch-millisecond dates, parsing and formatting.

use std::collections::BTreeMap;

use crate::error::CypherError;
use crate::types::CypherValue;

use super::{int_arg, str_arg};

/// The pure functions of this module; `None` for other names.
pub(super) fn eval(name: &str, args: &[CypherValue]) -> Option<Result<CypherValue, CypherError>> {
    use CypherValue as V;
    let r = match name {
        "apoc.date.currenttimestamp" => Ok(V::Integer(now_millis())),
        "apoc.date.format" => date_format(args),
        "apoc.date.parse" => date_parse(args),
        "apoc.date.add" => date_add(args),
        "apoc.date.convert" => date_convert(args),
        "apoc.date.field" => date_field(args),
        "apoc.date.fields" => date_fields(args),
        "apoc.date.toiso8601" => date_to_iso8601(args),
        "apoc.date.fromiso8601" => date_from_iso8601(args),
        "apoc.date.systemtimezone" => Ok(V::String("UTC".to_string())),
        "apoc.date.toyears" => date_to_years(args),
        "apoc.date.convertformat" => {
            let (Some(s), Some(from)) = (str_arg(args, 0), str_arg(args, 1)) else {
                return Some(Ok(V::Null));
            };
            let to = str_arg(args, 2).unwrap_or_else(|| "yyyy-MM-dd".to_string());
            match parse_pattern(&s, &from) {
                Some(ms) => Ok(V::String(format_pattern(ms, &to))),
                None => Err(CypherError::semantic(format!(
                    "cannot parse `{s}` with pattern `{from}`"
                ))),
            }
        }
        "apoc.temporal.format" => temporal_format(args),
        "apoc.temporal.formatduration" => temporal_format_duration(args),
        "apoc.temporal.tozonedtemporal" => {
            let (Some(s), fmt) = (
                str_arg(args, 0),
                str_arg(args, 1).unwrap_or_else(|| "yyyy-MM-dd HH:mm:ss".to_string()),
            ) else {
                return Some(Ok(V::Null));
            };
            match parse_pattern(&s, &fmt) {
                Some(ms) => Ok(V::String(format!(
                    "{}Z",
                    format_pattern(ms, "yyyy-MM-ddTHH:mm:ss")
                ))),
                None => Ok(V::Null),
            }
        }
        _ => return None,
    };
    Some(r)
}

fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Unit -> ms multiplier ('ms', 's', 'm', 'h', 'd').
fn unit_millis(unit: &str) -> Option<i64> {
    Some(match unit.to_ascii_lowercase().as_str() {
        "ms" | "milliseconds" | "millis" => 1,
        "s" | "seconds" | "second" => 1_000,
        "m" | "minutes" | "minute" => 60_000,
        "h" | "hours" | "hour" => 3_600_000,
        "d" | "days" | "day" => 86_400_000,
        _ => return None,
    })
}

/// Civil date from days since the Unix epoch (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = if m > 2 { m - 3 } else { m + 9 } as u64;
    let doy = (153 * mp + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

struct Utc {
    year: i64,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
    milli: u32,
}

fn utc_of(ms: i64) -> Utc {
    let days = ms.div_euclid(86_400_000);
    let rem = ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    Utc {
        year,
        month,
        day,
        hour: (rem / 3_600_000) as u32,
        minute: (rem % 3_600_000 / 60_000) as u32,
        second: (rem % 60_000 / 1000) as u32,
        milli: (rem % 1000) as u32,
    }
}

/// Formats epoch ms with a Java SimpleDateFormat subset: yyyy MM dd HH mm ss SSS plus literal
/// separators.
fn format_pattern(ms: i64, pattern: &str) -> String {
    let t = utc_of(ms);
    let mut out = String::new();
    let cs: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        let run = cs[i..].iter().take_while(|x| **x == c).count();
        match (c, run) {
            ('y', _) => {
                out.push_str(&format!("{:04}", t.year));
            }
            ('M', _) => out.push_str(&format!("{:02}", t.month)),
            ('d', _) => out.push_str(&format!("{:02}", t.day)),
            ('H', _) => out.push_str(&format!("{:02}", t.hour)),
            ('m', _) => out.push_str(&format!("{:02}", t.minute)),
            ('s', _) => out.push_str(&format!("{:02}", t.second)),
            ('S', _) => out.push_str(&format!("{:03}", t.milli)),
            _ => {
                for _ in 0..run {
                    out.push(c);
                }
            }
        }
        i += run;
    }
    out
}

/// Parses a timestamp with the same pattern subset -> epoch ms.
fn parse_pattern(s: &str, pattern: &str) -> Option<i64> {
    let (mut y, mut mo, mut d, mut h, mut mi, mut sec, mut ms) =
        (1970i64, 1u32, 1u32, 0u32, 0u32, 0u32, 0u32);
    let pc: Vec<char> = pattern.chars().collect();
    let sc: Vec<char> = s.chars().collect();
    let (mut i, mut j) = (0usize, 0usize);
    while i < pc.len() {
        let c = pc[i];
        let run = pc[i..].iter().take_while(|x| **x == c).count();
        if "yMdHmsS".contains(c) {
            let want = match c {
                'y' => 4.min(run.max(2)),
                'S' => 3,
                _ => 2,
            };
            let digits: String = sc.get(j..)?.iter().take(want).collect();
            let n: i64 = digits.parse().ok()?;
            j += digits.len();
            match c {
                'y' => y = n,
                'M' => mo = n as u32,
                'd' => d = n as u32,
                'H' => h = n as u32,
                'm' => mi = n as u32,
                's' => sec = n as u32,
                'S' => ms = n as u32,
                _ => {}
            }
        } else {
            j += run;
        }
        i += run;
    }
    Some(
        days_from_civil(y, mo, d) * 86_400_000
            + h as i64 * 3_600_000
            + mi as i64 * 60_000
            + sec as i64 * 1000
            + ms as i64,
    )
}

fn date_format(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(t) = int_arg(args, 0) else {
        return Ok(CypherValue::Null);
    };
    let unit = str_arg(args, 1).unwrap_or_else(|| "ms".to_string());
    let pattern = str_arg(args, 2).unwrap_or_else(|| "yyyy-MM-dd HH:mm:ss".to_string());
    let Some(mult) = unit_millis(&unit) else {
        return Err(CypherError::semantic(format!("unknown time unit `{unit}`")));
    };
    Ok(CypherValue::String(format_pattern(t * mult, &pattern)))
}

fn date_parse(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(s) = str_arg(args, 0) else {
        return Ok(CypherValue::Null);
    };
    let unit = str_arg(args, 1).unwrap_or_else(|| "ms".to_string());
    let pattern = str_arg(args, 2).unwrap_or_else(|| "yyyy-MM-dd HH:mm:ss".to_string());
    let Some(mult) = unit_millis(&unit) else {
        return Err(CypherError::semantic(format!("unknown time unit `{unit}`")));
    };
    match parse_pattern(&s, &pattern) {
        Some(ms) => Ok(CypherValue::Integer(ms / mult)),
        None => Err(CypherError::semantic(format!(
            "cannot parse `{s}` with pattern `{pattern}`"
        ))),
    }
}

fn date_add(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(t), Some(unit), Some(delta), Some(delta_unit)) = (
        int_arg(args, 0),
        str_arg(args, 1),
        int_arg(args, 2),
        str_arg(args, 3),
    ) else {
        return Ok(CypherValue::Null);
    };
    let (Some(m1), Some(m2)) = (unit_millis(&unit), unit_millis(&delta_unit)) else {
        return Err(CypherError::semantic("unknown time unit".to_string()));
    };
    Ok(CypherValue::Integer((t * m1 + delta * m2) / m1))
}

fn date_convert(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let (Some(t), Some(from), Some(to)) = (int_arg(args, 0), str_arg(args, 1), str_arg(args, 2))
    else {
        return Ok(CypherValue::Null);
    };
    let (Some(m1), Some(m2)) = (unit_millis(&from), unit_millis(&to)) else {
        return Err(CypherError::semantic("unknown time unit".to_string()));
    };
    Ok(CypherValue::Integer(t * m1 / m2))
}

fn date_field(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(t) = int_arg(args, 0) else {
        return Ok(CypherValue::Null);
    };
    let unit = str_arg(args, 1).unwrap_or_else(|| "d".to_string());
    let u = utc_of(t);
    Ok(CypherValue::Integer(
        match unit.to_ascii_lowercase().as_str() {
            "years" | "year" => u.year,
            "months" | "month" => u.month as i64,
            "days" | "day" | "d" => u.day as i64,
            "hours" | "hour" | "h" => u.hour as i64,
            "minutes" | "minute" | "m" => u.minute as i64,
            "seconds" | "second" | "s" => u.second as i64,
            "ms" | "millis" | "milliseconds" => u.milli as i64,
            other => {
                return Err(CypherError::semantic(format!(
                    "unknown time unit `{other}`"
                )));
            }
        },
    ))
}

fn date_fields(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(s) = str_arg(args, 0) else {
        return Ok(CypherValue::Null);
    };
    let pattern = str_arg(args, 1).unwrap_or_else(|| "yyyy-MM-dd HH:mm:ss".to_string());
    let Some(ms) = parse_pattern(&s, &pattern) else {
        return Err(CypherError::semantic(format!(
            "cannot parse `{s}` with pattern `{pattern}`"
        )));
    };
    let u = utc_of(ms);
    // ISO day of week (Mon=1 ... Sun=7); epoch day 0 was a Thursday (4)
    let weekday = (ms.div_euclid(86_400_000).rem_euclid(7) + 3) % 7 + 1;
    let mut m = BTreeMap::new();
    m.insert("years".to_string(), CypherValue::Integer(u.year));
    m.insert("months".to_string(), CypherValue::Integer(u.month as i64));
    m.insert("days".to_string(), CypherValue::Integer(u.day as i64));
    m.insert("weekdays".to_string(), CypherValue::Integer(weekday));
    m.insert("hours".to_string(), CypherValue::Integer(u.hour as i64));
    m.insert("minutes".to_string(), CypherValue::Integer(u.minute as i64));
    m.insert("seconds".to_string(), CypherValue::Integer(u.second as i64));
    m.insert("zoneid".to_string(), CypherValue::String("UTC".to_string()));
    Ok(CypherValue::Map(m))
}

fn date_to_iso8601(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(t) = int_arg(args, 0) else {
        return Ok(CypherValue::Null);
    };
    let unit = str_arg(args, 1).unwrap_or_else(|| "ms".to_string());
    let Some(mult) = unit_millis(&unit) else {
        return Err(CypherError::semantic(format!("unknown time unit `{unit}`")));
    };
    Ok(CypherValue::String(format!(
        "{}Z",
        format_pattern(t * mult, "yyyy-MM-ddTHH:mm:ss.SSS")
    )))
}

fn date_from_iso8601(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let Some(s) = str_arg(args, 0) else {
        return Ok(CypherValue::Null);
    };
    let trimmed = s.trim_end_matches('Z');
    let ms = parse_pattern(trimmed, "yyyy-MM-ddTHH:mm:ss.SSS")
        .or_else(|| parse_pattern(trimmed, "yyyy-MM-ddTHH:mm:ss"));
    match ms {
        Some(v) => Ok(CypherValue::Integer(v)),
        None => Err(CypherError::semantic(format!(
            "cannot parse ISO-8601 `{s}`"
        ))),
    }
}

fn date_to_years(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    match args.first() {
        Some(CypherValue::Integer(ms)) => {
            Ok(CypherValue::Float(*ms as f64 / (365.25 * 86_400_000.0)))
        }
        Some(CypherValue::String(s)) => {
            let fmt = str_arg(args, 1).unwrap_or_else(|| "yyyy-MM-dd HH:mm:ss".to_string());
            match parse_pattern(s, &fmt) {
                Some(ms) => Ok(CypherValue::Float(
                    1970.0 + ms as f64 / (365.25 * 86_400_000.0),
                )),
                None => Ok(CypherValue::Null),
            }
        }
        _ => Ok(CypherValue::Null),
    }
}

fn temporal_format(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    let fmt = str_arg(args, 1).unwrap_or_else(|| "yyyy-MM-dd".to_string());
    // temporal values formatted via their canonical string forms + epoch math where possible
    match args.first() {
        Some(CypherValue::Integer(ms)) => Ok(CypherValue::String(format_pattern(*ms, &fmt))),
        Some(other) => {
            // try an ISO parse of the value's string form
            let s = other.to_string();
            let trimmed = s.trim_end_matches('Z');
            let ms = parse_pattern(trimmed, "yyyy-MM-ddTHH:mm:ss")
                .or_else(|| parse_pattern(trimmed, "yyyy-MM-dd"));
            match ms {
                Some(ms) => Ok(CypherValue::String(format_pattern(ms, &fmt))),
                None => Ok(CypherValue::String(s)),
            }
        }
        None => Ok(CypherValue::Null),
    }
}

fn temporal_format_duration(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    // best effort: total seconds as HH:mm:ss-style patterns
    let secs = match args.first() {
        Some(CypherValue::Integer(s)) => *s,
        Some(CypherValue::Float(f)) => *f as i64,
        Some(other) => {
            let s = other.to_string();
            return Ok(CypherValue::String(s));
        }
        None => return Ok(CypherValue::Null),
    };
    let fmt = str_arg(args, 1).unwrap_or_else(|| "HH:mm:ss".to_string());
    Ok(CypherValue::String(format_pattern(secs * 1000, &fmt)))
}
