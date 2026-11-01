//! Parsing ISO 8601 strings into temporal values and durations.

use chrono::{NaiveDate, Offset};

use crate::error::CypherError;
use crate::types::CypherValue;

use super::types::{
    CypherDate, CypherDateTime, CypherDuration, CypherLocalDateTime, CypherLocalTime, CypherTime,
    wide_from_ymd,
};
use super::{
    NANOS_PER_DAY, NANOS_PER_HOUR, NANOS_PER_HOUR_I, NANOS_PER_MINUTE, NANOS_PER_MINUTE_I,
    NANOS_PER_SECOND, NANOS_PER_SECOND_I, err, nanos_to_hms, weekday_from_num,
};

/// A temporal string that doesn't parse.
fn format_err(msg: &str) -> CypherError {
    err(msg).with_status(crate::status::GqlStatus::InvalidTemporalFormat)
}

pub fn parse_date_string(s: &str) -> Result<CypherValue, CypherError> {
    let (date, cycles) = parse_date_str(s)?;
    Ok(CypherValue::Date(CypherDate { date, cycles }))
}

/// Date string -> (inner chrono date, 400-year cycle offset); cycles are 0 except extended years
/// beyond chrono's range.
fn parse_date_str(s: &str) -> Result<(NaiveDate, i64), CypherError> {
    let s = s.trim();

    // extended year with +/- prefix (-999999999-01-01, +999999999-12-31)
    if s.starts_with('+') || (s.starts_with('-') && s.len() > 4 && s[1..].contains('-')) {
        let sign: i64 = if s.starts_with('-') { -1 } else { 1 };
        let rest = &s[1..]; // skip +/-
        // Try YYYY...-MM-DD
        if let Some(first_dash) = rest.find('-') {
            let year_str = &rest[..first_dash];
            let remainder = &rest[first_dash + 1..];
            if let Some(second_dash) = remainder.find('-') {
                let month_str = &remainder[..second_dash];
                let day_str = &remainder[second_dash + 1..];
                if let (Ok(year_abs), Ok(month), Ok(day)) = (
                    year_str.parse::<i64>(),
                    month_str.parse::<u32>(),
                    day_str.parse::<u32>(),
                ) {
                    let year = sign * year_abs;
                    return wide_from_ymd(year, month, day)
                        .ok_or_else(|| format_err("invalid date"));
                }
            } else if let (Ok(year_abs), Ok(month)) =
                (year_str.parse::<i64>(), remainder.parse::<u32>())
            {
                let year = sign * year_abs;
                return wide_from_ymd(year, month, 1).ok_or_else(|| format_err("invalid date"));
            }
        }
    }

    let bytes = s.as_bytes();
    let len = bytes.len();

    if len < 4 {
        return Err(format_err("date string too short"));
    }

    // Check for week date (contains 'W')
    if s.contains('W') || s.contains('w') {
        return parse_week_date(s).map(|d| (d, 0));
    }

    // YYYY-MM-DD (len 10)
    if len == 10 && bytes[4] == b'-' && bytes[7] == b'-' {
        let year: i32 = s[0..4].parse().map_err(|_| format_err("invalid year"))?;
        let month: u32 = s[5..7].parse().map_err(|_| format_err("invalid month"))?;
        let day: u32 = s[8..10].parse().map_err(|_| format_err("invalid day"))?;
        return NaiveDate::from_ymd_opt(year, month, day)
            .map(|d| (d, 0))
            .ok_or_else(|| format_err("invalid date"));
    }

    // YYYYMMDD (len 8, no dashes)
    if len == 8 && !s.contains('-') {
        let year: i32 = s[0..4].parse().map_err(|_| format_err("invalid year"))?;
        let month: u32 = s[4..6].parse().map_err(|_| format_err("invalid month"))?;
        let day: u32 = s[6..8].parse().map_err(|_| format_err("invalid day"))?;
        return NaiveDate::from_ymd_opt(year, month, day)
            .map(|d| (d, 0))
            .ok_or_else(|| format_err("invalid date"));
    }

    // YYYY-DDD (ordinal, len 8 with dash at pos 4)
    if len == 8 && bytes[4] == b'-' {
        let year: i32 = s[0..4].parse().map_err(|_| format_err("invalid year"))?;
        let ordinal: u32 = s[5..8].parse().map_err(|_| format_err("invalid ordinal"))?;
        return NaiveDate::from_yo_opt(year, ordinal)
            .map(|d| (d, 0))
            .ok_or_else(|| format_err("invalid ordinal date"));
    }

    // YYYYDDD (ordinal, len 7, no dashes)
    if len == 7 && !s.contains('-') {
        let year: i32 = s[0..4].parse().map_err(|_| format_err("invalid year"))?;
        let ordinal: u32 = s[4..7].parse().map_err(|_| format_err("invalid ordinal"))?;
        return NaiveDate::from_yo_opt(year, ordinal)
            .map(|d| (d, 0))
            .ok_or_else(|| format_err("invalid ordinal date"));
    }

    // YYYY-MM (len 7 with dash at pos 4)
    if len == 7 && bytes[4] == b'-' {
        let year: i32 = s[0..4].parse().map_err(|_| format_err("invalid year"))?;
        let month: u32 = s[5..7].parse().map_err(|_| format_err("invalid month"))?;
        return NaiveDate::from_ymd_opt(year, month, 1)
            .map(|d| (d, 0))
            .ok_or_else(|| format_err("invalid date"));
    }

    // YYYYMM (len 6, no dashes) - compact month format
    if len == 6 && !s.contains('-') {
        let year: i32 = s[0..4].parse().map_err(|_| format_err("invalid year"))?;
        let month: u32 = s[4..6].parse().map_err(|_| format_err("invalid month"))?;
        return NaiveDate::from_ymd_opt(year, month, 1)
            .map(|d| (d, 0))
            .ok_or_else(|| format_err("invalid date"));
    }

    // YYYY (len 4)
    if len == 4 {
        let year: i32 = s.parse().map_err(|_| format_err("invalid year"))?;
        return NaiveDate::from_ymd_opt(year, 1, 1)
            .map(|d| (d, 0))
            .ok_or_else(|| format_err("invalid date"));
    }

    Err(format_err(&format!("unrecognized date format: {s}")))
}

fn parse_week_date(s: &str) -> Result<NaiveDate, CypherError> {
    // Formats: YYYY-Www-D, YYYYWwwD, YYYY-Www, YYYYWww
    let s_upper = s.to_uppercase();
    let w_pos = s_upper
        .find('W')
        .ok_or_else(|| format_err("expected W in week date"))?;
    let year: i32 = s[..w_pos]
        .trim_end_matches('-')
        .parse()
        .map_err(|_| format_err("invalid year"))?;
    let rest = &s_upper[w_pos + 1..];

    let (week_str, day_str) = if rest.contains('-') {
        let parts: Vec<&str> = rest.split('-').collect();
        (parts[0], if parts.len() > 1 { parts[1] } else { "1" })
    } else if rest.len() >= 3 {
        (&rest[..2], &rest[2..])
    } else {
        (rest, "1")
    };

    let week: u32 = week_str.parse().map_err(|_| format_err("invalid week"))?;
    let day: u32 = if day_str.is_empty() {
        1
    } else {
        day_str
            .parse()
            .map_err(|_| format_err("invalid day of week"))?
    };
    let wd = weekday_from_num(day).ok_or_else(|| format_err("dayOfWeek must be 1-7"))?;
    NaiveDate::from_isoywd_opt(year, week, wd).ok_or_else(|| format_err("invalid ISO week date"))
}

fn parse_time_str(s: &str) -> Result<u64, CypherError> {
    let bytes = s.as_bytes();
    let len = bytes.len();
    if len < 2 {
        return Err(format_err("time string too short"));
    }

    let hours: u64 = s[0..2].parse().map_err(|_| format_err("invalid hours"))?;
    if hours > 23 {
        return Err(format_err("hours must be 0-23"));
    }
    if len == 2 {
        return Ok(hours * NANOS_PER_HOUR);
    }

    let rest = if bytes[2] == b':' { &s[3..] } else { &s[2..] };
    let rest_bytes = rest.as_bytes();
    if rest.len() < 2 {
        return Err(format_err("invalid time format"));
    }

    let minutes: u64 = rest[0..2]
        .parse()
        .map_err(|_| format_err("invalid minutes"))?;
    if minutes > 59 {
        return Err(format_err("minutes must be 0-59"));
    }
    if rest.len() == 2 {
        return Ok(hours * NANOS_PER_HOUR + minutes * NANOS_PER_MINUTE);
    }

    let rest2 = if rest_bytes[2] == b':' {
        &rest[3..]
    } else {
        &rest[2..]
    };
    if rest2.is_empty() {
        return Ok(hours * NANOS_PER_HOUR + minutes * NANOS_PER_MINUTE);
    }

    // Parse seconds + optional fractional
    let (sec_str, frac_nanos) = if let Some(dot_pos) = rest2.find('.') {
        let sec_s = &rest2[..dot_pos];
        let frac_s = &rest2[dot_pos + 1..];
        let frac = parse_fractional_nanos(frac_s)?;
        (sec_s, frac)
    } else {
        (rest2, 0u64)
    };

    let seconds: u64 = sec_str.parse().map_err(|_| format_err("invalid seconds"))?;
    if seconds > 59 {
        return Err(format_err("seconds must be 0-59"));
    }

    Ok(hours * NANOS_PER_HOUR
        + minutes * NANOS_PER_MINUTE
        + seconds * NANOS_PER_SECOND
        + frac_nanos)
}

fn parse_fractional_nanos(s: &str) -> Result<u64, CypherError> {
    if s.is_empty() {
        return Ok(0);
    }
    // Pad or truncate to 9 digits
    let padded = if s.len() >= 9 {
        s[..9].to_string()
    } else {
        format!("{s:0<9}")
    };
    padded
        .parse()
        .map_err(|_| format_err("invalid fractional seconds"))
}

pub(super) fn parse_offset_str(s: &str) -> Result<i32, CypherError> {
    if s.is_empty() {
        return Ok(0);
    }
    if s == "Z" || s == "z" {
        return Ok(0);
    }
    let sign: i32 = match s.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return Err(format_err("offset must start with +, -, or Z")),
    };
    let rest = &s[1..];
    let (hours, minutes, seconds) = if rest.contains(':') {
        let parts: Vec<&str> = rest.split(':').collect();
        let h: i32 = parts[0]
            .parse()
            .map_err(|_| format_err("invalid offset hours"))?;
        let m: i32 = parts
            .get(1)
            .unwrap_or(&"0")
            .parse()
            .map_err(|_| format_err("invalid offset minutes"))?;
        let s: i32 = parts
            .get(2)
            .unwrap_or(&"0")
            .parse()
            .map_err(|_| format_err("invalid offset seconds"))?;
        (h, m, s)
    } else {
        match rest.len() {
            2 => {
                let h: i32 = rest
                    .parse()
                    .map_err(|_| format_err("invalid offset hours"))?;
                (h, 0, 0)
            }
            4 => {
                let h: i32 = rest[0..2]
                    .parse()
                    .map_err(|_| format_err("invalid offset hours"))?;
                let m: i32 = rest[2..4]
                    .parse()
                    .map_err(|_| format_err("invalid offset minutes"))?;
                (h, m, 0)
            }
            _ => return Err(format_err("invalid offset format")),
        }
    };
    Ok(sign * (hours * 3600 + minutes * 60 + seconds))
}

/// Split a time string into (time_part, offset_part).
fn split_time_offset(s: &str) -> (&str, &str) {
    if s.ends_with('Z') || s.ends_with('z') {
        return (&s[..s.len() - 1], &s[s.len() - 1..]);
    }
    let bytes = s.as_bytes();
    for i in (1..bytes.len()).rev() {
        if bytes[i] == b'+' || bytes[i] == b'-' {
            let rest = &s[i + 1..];
            if !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b':') {
                return (&s[..i], &s[i..]);
            }
        }
    }
    (s, "")
}

pub fn parse_localtime_string(s: &str) -> Result<CypherValue, CypherError> {
    let nanos = parse_time_str(s.trim())?;
    Ok(CypherValue::LocalTime(CypherLocalTime { nanos }))
}

pub fn parse_time_string(s: &str) -> Result<CypherValue, CypherError> {
    let s = s.trim();
    let (time_part, offset_part) = split_time_offset(s);
    let nanos = parse_time_str(time_part)?;
    let offset_secs = parse_offset_str(offset_part)?;
    Ok(CypherValue::Time(CypherTime { nanos, offset_secs }))
}

pub fn parse_localdatetime_string(s: &str) -> Result<CypherValue, CypherError> {
    let s = s.trim();
    if let Some(t_pos) = s.find(['T', 't']) {
        let (date, cycles) = parse_date_str(&s[..t_pos])?;
        let nanos = parse_time_str(&s[t_pos + 1..])?;
        Ok(CypherValue::LocalDateTime(CypherLocalDateTime {
            date,
            nanos,
            cycles,
        }))
    } else {
        // No T separator - treat as date at midnight
        let (date, cycles) = parse_date_str(s)?;
        Ok(CypherValue::LocalDateTime(CypherLocalDateTime {
            date,
            nanos: 0,
            cycles,
        }))
    }
}

pub fn parse_datetime_string(s: &str) -> Result<CypherValue, CypherError> {
    let s = s.trim();
    let t_pos = s
        .find(['T', 't'])
        .ok_or_else(|| format_err("datetime requires 'T' separator"))?;
    let date_str = &s[..t_pos];

    // Check for timezone name in brackets
    let (time_offset_str, tz_name) = if let Some(bracket_start) = s.find('[') {
        let bracket_end = s.find(']').ok_or_else(|| format_err("unclosed bracket"))?;
        let tz = &s[bracket_start + 1..bracket_end];
        (&s[t_pos + 1..bracket_start], Some(tz.to_string()))
    } else {
        (&s[t_pos + 1..], None)
    };

    let (date, cycles) = parse_date_str(date_str)?;
    if cycles != 0 {
        // zoned datetimes stay chrono-backed: time zone rules mean nothing at +/-1e9 years
        return Err(format_err("datetime year out of supported range"));
    }
    let (time_part, offset_part) = split_time_offset(time_offset_str);
    let nanos = parse_time_str(time_part)?;

    // time zone name without explicit offset: resolve the offset from the zone
    let offset_secs = if !offset_part.is_empty() {
        parse_offset_str(offset_part)?
    } else if let Some(ref tz) = tz_name {
        resolve_tz_offset(tz, &date, nanos)?
    } else {
        0
    };

    Ok(CypherValue::DateTime(CypherDateTime {
        date,
        nanos,
        offset_secs,
        tz_name,
    }))
}

pub(super) fn resolve_tz_offset(
    tz_name: &str,
    date: &NaiveDate,
    nanos: u64,
) -> Result<i32, CypherError> {
    use chrono::TimeZone;
    let tz: chrono_tz::Tz = tz_name
        .parse()
        .map_err(|_| format_err(&format!("unknown timezone: {tz_name}")))?;
    let (h, m, s, ns) = nanos_to_hms(nanos);
    let naive_time = chrono::NaiveTime::from_hms_nano_opt(h, m, s, ns as u32)
        .ok_or_else(|| format_err("invalid time for tz resolution"))?;
    let naive_dt = chrono::NaiveDateTime::new(*date, naive_time);
    let dt = tz
        .from_local_datetime(&naive_dt)
        .single()
        .ok_or_else(|| format_err("ambiguous or invalid datetime in timezone"))?;
    Ok(dt.offset().fix().local_minus_utc())
}

pub fn parse_duration_string(s: &str) -> Result<CypherValue, CypherError> {
    let s = s.trim();
    if !s.starts_with('P') && !s.starts_with('p') {
        return Err(format_err("duration must start with 'P'"));
    }
    let rest = &s[1..];

    // date-style duration P2012-02-02T14:37:21.545 (digits before the first dash, no letters)
    if let Some(dash_pos) = rest.find('-') {
        let before_dash = &rest[..dash_pos];
        if !before_dash.is_empty() && before_dash.chars().all(|c| c.is_ascii_digit()) {
            return parse_duration_date_style(rest);
        }
    }

    let mut months: i64 = 0;
    let mut days: i64 = 0;
    let mut nanos: i128 = 0;

    let (date_part, time_part) = if let Some(t_pos) = rest.find(['T', 't']) {
        (&rest[..t_pos], &rest[t_pos + 1..])
    } else {
        (rest, "")
    };

    // Parse date components: nY, nM, nW, nD (with fraction cascading)
    parse_duration_date_components(date_part, &mut months, &mut days, &mut nanos)?;

    // Parse time components: nH, nM, n.fS
    if !time_part.is_empty() {
        parse_duration_time_components(time_part, &mut nanos)?;
    }

    Ok(CypherValue::Duration(CypherDuration::from_nanos(
        months, days, nanos,
    )))
}

fn parse_duration_date_components(
    s: &str,
    months: &mut i64,
    days: &mut i64,
    nanos: &mut i128,
) -> Result<(), CypherError> {
    let mut buf = String::new();
    let mut frac_days: f64 = 0.0;
    for c in s.chars() {
        match c {
            'Y' | 'y' => {
                let n: f64 = buf
                    .parse()
                    .map_err(|_| format_err("invalid duration year"))?;
                let total_months = n * 12.0;
                *months += total_months.trunc() as i64;
                frac_days += total_months.fract() * (365.2425 / 12.0);
                buf.clear();
            }
            'M' | 'm' => {
                let n: f64 = buf
                    .parse()
                    .map_err(|_| format_err("invalid duration month"))?;
                *months += n.trunc() as i64;
                frac_days += n.fract() * (365.2425 / 12.0);
                buf.clear();
            }
            'W' | 'w' => {
                let n: f64 = buf
                    .parse()
                    .map_err(|_| format_err("invalid duration week"))?;
                let total_days = n * 7.0;
                *days += total_days.trunc() as i64;
                frac_days += total_days.fract();
                buf.clear();
            }
            'D' | 'd' => {
                let n: f64 = buf
                    .parse()
                    .map_err(|_| format_err("invalid duration day"))?;
                *days += n.trunc() as i64;
                frac_days += n.fract();
                buf.clear();
            }
            _ => buf.push(c),
        }
    }
    // Cascade fractional days into whole days + nanos
    *days += frac_days.trunc() as i64;
    let frac_day_nanos = frac_days.fract() * NANOS_PER_DAY as f64;
    *nanos += frac_day_nanos.round() as i128;
    Ok(())
}

fn parse_duration_time_components(s: &str, nanos: &mut i128) -> Result<(), CypherError> {
    // integer components take the exact path: f64 loses precision above 2^53 nanos, which wide
    // durations exceed
    fn add_component(buf: &str, unit: u64, nanos: &mut i128) -> Result<(), CypherError> {
        if buf.contains('.') {
            let n: f64 = buf
                .parse()
                .map_err(|_| format_err("invalid duration component"))?;
            *nanos += (n * unit as f64) as i128;
        } else {
            let n: i128 = buf
                .parse()
                .map_err(|_| format_err("invalid duration component"))?;
            *nanos += n * unit as i128;
        }
        Ok(())
    }
    let mut buf = String::new();
    for c in s.chars() {
        match c {
            'H' | 'h' => {
                add_component(&buf, NANOS_PER_HOUR, nanos)
                    .map_err(|_| format_err("invalid duration hour"))?;
                buf.clear();
            }
            'M' | 'm' => {
                add_component(&buf, NANOS_PER_MINUTE, nanos)
                    .map_err(|_| format_err("invalid duration minute"))?;
                buf.clear();
            }
            'S' | 's' => {
                add_component(&buf, NANOS_PER_SECOND, nanos)
                    .map_err(|_| format_err("invalid duration second"))?;
                buf.clear();
            }
            _ => buf.push(c),
        }
    }
    Ok(())
}

fn parse_duration_date_style(s: &str) -> Result<CypherValue, CypherError> {
    // Format: YYYY-MM-DDThh:mm:ss.fff
    let (date_part, time_part) = if let Some(t_pos) = s.find(['T', 't']) {
        (&s[..t_pos], &s[t_pos + 1..])
    } else {
        (s, "")
    };

    let date_parts: Vec<&str> = date_part.split('-').collect();
    let years: i64 = date_parts
        .first()
        .unwrap_or(&"0")
        .parse()
        .map_err(|_| format_err("invalid duration years"))?;
    let mons: i64 = date_parts
        .get(1)
        .unwrap_or(&"0")
        .parse()
        .map_err(|_| format_err("invalid duration months"))?;
    let d: i64 = date_parts
        .get(2)
        .unwrap_or(&"0")
        .parse()
        .map_err(|_| format_err("invalid duration days"))?;

    let months = years * 12 + mons;
    let days = d;

    let mut nanos: i128 = 0;
    if !time_part.is_empty() {
        let time_parts: Vec<&str> = time_part.split(':').collect();
        let h: i64 = time_parts
            .first()
            .unwrap_or(&"0")
            .parse()
            .map_err(|_| format_err("invalid duration hours"))?;
        let m: i64 = time_parts
            .get(1)
            .unwrap_or(&"0")
            .parse()
            .map_err(|_| format_err("invalid duration minutes"))?;
        let sec_str = time_parts.get(2).unwrap_or(&"0");
        let (sec_int, sec_frac) = if let Some(dot) = sec_str.find('.') {
            let si: i64 = sec_str[..dot]
                .parse()
                .map_err(|_| format_err("invalid duration seconds"))?;
            let frac = parse_fractional_nanos(&sec_str[dot + 1..])? as i64;
            (si, frac)
        } else {
            let si: i64 = sec_str
                .parse()
                .map_err(|_| format_err("invalid duration seconds"))?;
            (si, 0i64)
        };
        nanos = h as i128 * NANOS_PER_HOUR_I as i128
            + m as i128 * NANOS_PER_MINUTE_I as i128
            + sec_int as i128 * NANOS_PER_SECOND_I as i128
            + sec_frac as i128;
    }

    Ok(CypherValue::Duration(CypherDuration::from_nanos(
        months, days, nanos,
    )))
}
