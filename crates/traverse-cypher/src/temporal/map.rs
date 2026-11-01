//! Temporal values from maps (`date({year: 2024, month: 1})`), including from another temporal.

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate, Offset};

use crate::error::CypherError;
use crate::types::CypherValue;

use super::clock::func_datetime_from_epoch_millis;
use super::parse::{
    parse_date_string, parse_datetime_string, parse_localdatetime_string, parse_localtime_string,
    parse_offset_str, parse_time_string, resolve_tz_offset,
};
use super::types::{
    CypherDate, CypherDateTime, CypherDuration, CypherLocalDateTime, CypherLocalTime, CypherTime,
    wide_from_ymd,
};
use super::{
    NANOS_PER_DAY, NANOS_PER_DAY_I, NANOS_PER_HOUR, NANOS_PER_MICRO, NANOS_PER_MILLI,
    NANOS_PER_MINUTE, NANOS_PER_SECOND, NANOS_PER_SECOND_I, add_days_to_date, err, hms_to_nanos,
    nanos_to_hms, weekday_from_num,
};

pub(super) fn get_int(m: &BTreeMap<String, CypherValue>, key: &str) -> Option<i64> {
    match m.get(key) {
        Some(CypherValue::Integer(i)) => Some(*i),
        Some(CypherValue::Float(f)) => Some(*f as i64),
        _ => None,
    }
}

fn get_f64(m: &BTreeMap<String, CypherValue>, key: &str) -> Option<f64> {
    match m.get(key) {
        Some(CypherValue::Integer(i)) => Some(*i as f64),
        Some(CypherValue::Float(f)) => Some(*f),
        _ => None,
    }
}

pub(super) fn date_from_map(m: &BTreeMap<String, CypherValue>) -> Result<CypherValue, CypherError> {
    // Check for string arg
    if let Some(CypherValue::String(s)) = m.get("date") {
        return parse_date_string(s);
    }

    // Check for base temporal value
    let base_date = extract_base_date(m);

    let wide_year_field = get_int(m, "year")
        .or_else(|| base_date.map(|d| d.year() as i64))
        .unwrap_or(0);
    let year = wide_year_field as i32;

    // Week-based construction
    if let Some(week) = get_int(m, "week") {
        let default_dow = base_date
            .map(|d| d.weekday().number_from_monday() as i64)
            .unwrap_or(1);
        let dow = get_int(m, "dayOfWeek").unwrap_or(default_dow) as u32;
        let wd = weekday_from_num(dow).ok_or_else(|| err("dayOfWeek must be 1-7"))?;
        // For ISO week dates, use the ISO week year (not regular year) from base date
        let iso_year = get_int(m, "year")
            .map(|y| y as i32)
            .or_else(|| base_date.map(|d| d.iso_week().year()))
            .unwrap_or(0);
        let date = NaiveDate::from_isoywd_opt(iso_year, week as u32, wd)
            .ok_or_else(|| err("invalid ISO week date"))?;
        return Ok(CypherValue::Date(CypherDate { date, cycles: 0 }));
    }

    // Ordinal-based construction
    if let Some(ordinal) = get_int(m, "ordinalDay") {
        let date = NaiveDate::from_yo_opt(year, ordinal as u32)
            .ok_or_else(|| err("invalid ordinal date"))?;
        return Ok(CypherValue::Date(CypherDate { date, cycles: 0 }));
    }

    // Quarter-based construction
    if let Some(quarter) = get_int(m, "quarter") {
        if let Some(doq) = get_int(m, "dayOfQuarter") {
            let first_month = ((quarter - 1) * 3 + 1) as u32;
            let base = NaiveDate::from_ymd_opt(year, first_month, 1)
                .ok_or_else(|| err("invalid quarter date"))?;
            let date = add_days_to_date(base, doq - 1)?;
            return Ok(CypherValue::Date(CypherDate { date, cycles: 0 }));
        } else if let Some(bd) = base_date {
            // Preserve month-within-quarter and day from base date
            let month_in_q = (bd.month() - 1) % 3; // 0, 1, or 2
            let new_month = ((quarter - 1) * 3 + 1) as u32 + month_in_q;
            let day = bd.day();
            let date = NaiveDate::from_ymd_opt(year, new_month, day)
                .ok_or_else(|| err("invalid quarter date"))?;
            return Ok(CypherValue::Date(CypherDate { date, cycles: 0 }));
        } else {
            let first_month = ((quarter - 1) * 3 + 1) as u32;
            let base = NaiveDate::from_ymd_opt(year, first_month, 1)
                .ok_or_else(|| err("invalid quarter date"))?;
            return Ok(CypherValue::Date(CypherDate {
                date: base,
                cycles: 0,
            }));
        }
    }

    // Standard year-month-day construction
    let month = get_int(m, "month")
        .or_else(|| base_date.map(|d| d.month() as i64))
        .unwrap_or(1) as u32;
    let day = get_int(m, "day")
        .or_else(|| base_date.map(|d| d.day() as i64))
        .unwrap_or(1) as u32;

    let (date, cycles) =
        wide_from_ymd(wide_year_field, month, day).ok_or_else(|| err("invalid date"))?;
    Ok(CypherValue::Date(CypherDate { date, cycles }))
}

fn extract_base_date(m: &BTreeMap<String, CypherValue>) -> Option<NaiveDate> {
    for key in &["date", "datetime", "localdatetime"] {
        match m.get(*key) {
            Some(CypherValue::Date(d)) => return Some(d.date),
            Some(CypherValue::DateTime(dt)) => return Some(dt.date),
            Some(CypherValue::LocalDateTime(ldt)) => return Some(ldt.date),
            _ => {}
        }
    }
    None
}

fn extract_base_time_nanos(m: &BTreeMap<String, CypherValue>) -> Option<u64> {
    for key in &["time", "localtime", "datetime", "localdatetime"] {
        match m.get(*key) {
            Some(CypherValue::LocalTime(t)) => return Some(t.nanos),
            Some(CypherValue::Time(t)) => return Some(t.nanos),
            Some(CypherValue::DateTime(dt)) => return Some(dt.nanos),
            Some(CypherValue::LocalDateTime(ldt)) => return Some(ldt.nanos),
            _ => {}
        }
    }
    None
}

fn extract_base_offset(m: &BTreeMap<String, CypherValue>) -> Option<i32> {
    for key in &["time", "datetime"] {
        match m.get(*key) {
            Some(CypherValue::Time(t)) => return Some(t.offset_secs),
            Some(CypherValue::DateTime(dt)) => return Some(dt.offset_secs),
            _ => {}
        }
    }
    None
}

fn extract_base_tz_name(m: &BTreeMap<String, CypherValue>) -> Option<String> {
    for key in &["time", "datetime", "date", "localdatetime"] {
        if let Some(CypherValue::DateTime(dt)) = m.get(*key) {
            return dt.tz_name.clone();
        }
    }
    None
}

fn time_fields_from_map(m: &BTreeMap<String, CypherValue>, base_nanos: Option<u64>) -> u64 {
    let (bh, bm, bs, bn) = base_nanos.map(nanos_to_hms).unwrap_or((0, 0, 0, 0));
    let hour = get_int(m, "hour").unwrap_or(bh as i64) as u64;
    let minute = get_int(m, "minute").unwrap_or(bm as i64) as u64;
    let second = get_int(m, "second").unwrap_or(bs as i64) as u64;
    let milli = get_int(m, "millisecond").unwrap_or(0) as u64;
    let micro = get_int(m, "microsecond").unwrap_or(0) as u64;
    let nano = get_int(m, "nanosecond").unwrap_or(0) as u64;
    let base_sub = if m.contains_key("millisecond")
        || m.contains_key("microsecond")
        || m.contains_key("nanosecond")
    {
        0
    } else {
        bn
    };
    hms_to_nanos(
        hour,
        minute,
        second,
        base_sub + milli * NANOS_PER_MILLI + micro * NANOS_PER_MICRO + nano,
    )
}

pub(super) fn localtime_from_map(
    m: &BTreeMap<String, CypherValue>,
) -> Result<CypherValue, CypherError> {
    if let Some(CypherValue::String(s)) = m.get("time").or_else(|| m.get("localtime")) {
        return parse_localtime_string(s);
    }
    let base = extract_base_time_nanos(m);
    let nanos = time_fields_from_map(m, base);
    Ok(CypherValue::LocalTime(CypherLocalTime { nanos }))
}

pub(super) fn time_from_map(m: &BTreeMap<String, CypherValue>) -> Result<CypherValue, CypherError> {
    if let Some(CypherValue::String(s)) = m.get("time") {
        return parse_time_string(s);
    }
    let base_nanos = extract_base_time_nanos(m);
    let base_offset = extract_base_offset(m);
    let nanos = time_fields_from_map(m, base_nanos);
    let offset_secs = if let Some(CypherValue::String(tz)) = m.get("timezone") {
        parse_offset_or_tz(tz)?
    } else {
        base_offset.unwrap_or(0)
    };
    // If timezone changed, convert wall-clock time (overrides are applied in source tz first)
    let nanos = if let (Some(src_offset), Some(CypherValue::String(_))) =
        (base_offset, m.get("timezone"))
    {
        if src_offset != offset_secs {
            let diff_secs = (offset_secs - src_offset) as i64;
            let converted = nanos as i64 + diff_secs * NANOS_PER_SECOND_I;
            let day_nanos = 24 * 3600 * NANOS_PER_SECOND_I;
            ((converted % day_nanos + day_nanos) % day_nanos) as u64
        } else {
            nanos
        }
    } else {
        nanos
    };
    Ok(CypherValue::Time(CypherTime { nanos, offset_secs }))
}

pub(super) fn parse_offset_or_tz(s: &str) -> Result<i32, CypherError> {
    // Try as offset first (+02:00, Z, etc.)
    if s.starts_with('+') || s.starts_with('-') || s == "Z" || s == "z" {
        return parse_offset_str(s);
    }
    // Try as named timezone - resolve to current offset
    let tz: chrono_tz::Tz = s
        .parse()
        .map_err(|_| err(&format!("unknown timezone: {s}")))?;
    let now = chrono::Utc::now().with_timezone(&tz);
    Ok(now.offset().fix().local_minus_utc())
}

pub(super) fn localdatetime_from_map(
    m: &BTreeMap<String, CypherValue>,
) -> Result<CypherValue, CypherError> {
    if let Some(CypherValue::String(s)) = m.get("localdatetime") {
        return parse_localdatetime_string(s);
    }
    let base_date = extract_base_date(m);
    let year = get_int(m, "year")
        .or_else(|| base_date.map(|d| d.year() as i64))
        .unwrap_or(0) as i32;

    // Week-based construction
    if let Some(week) = get_int(m, "week") {
        let default_dow = base_date
            .map(|d| d.weekday().number_from_monday() as i64)
            .unwrap_or(1);
        let dow = get_int(m, "dayOfWeek").unwrap_or(default_dow) as u32;
        let wd = weekday_from_num(dow).ok_or_else(|| err("dayOfWeek must be 1-7"))?;
        let iso_year = get_int(m, "year")
            .map(|y| y as i32)
            .or_else(|| base_date.map(|d| d.iso_week().year()))
            .unwrap_or(0);
        let date = NaiveDate::from_isoywd_opt(iso_year, week as u32, wd)
            .ok_or_else(|| err("invalid ISO week date"))?;
        let base_nanos = extract_base_time_nanos(m);
        let nanos = time_fields_from_map(m, base_nanos);
        return Ok(CypherValue::LocalDateTime(CypherLocalDateTime {
            date,
            nanos,
            cycles: 0,
        }));
    }

    // Ordinal-based construction
    if let Some(ordinal) = get_int(m, "ordinalDay") {
        let date = NaiveDate::from_yo_opt(year, ordinal as u32)
            .ok_or_else(|| err("invalid ordinal date"))?;
        let base_nanos = extract_base_time_nanos(m);
        let nanos = time_fields_from_map(m, base_nanos);
        return Ok(CypherValue::LocalDateTime(CypherLocalDateTime {
            date,
            nanos,
            cycles: 0,
        }));
    }

    // Quarter-based construction
    if let Some(quarter) = get_int(m, "quarter") {
        let date = if let Some(doq) = get_int(m, "dayOfQuarter") {
            let first_month = ((quarter - 1) * 3 + 1) as u32;
            let base =
                NaiveDate::from_ymd_opt(year, first_month, 1).ok_or_else(|| err("invalid date"))?;
            chrono::Duration::try_days(doq - 1)
                .and_then(|d| base.checked_add_signed(d))
                .ok_or_else(|| err("invalid quarter date"))?
        } else if let Some(bd) = base_date {
            let month_in_q = (bd.month() - 1) % 3;
            let new_month = ((quarter - 1) * 3 + 1) as u32 + month_in_q;
            let day = bd.day();
            NaiveDate::from_ymd_opt(year, new_month, day)
                .ok_or_else(|| err("invalid quarter date"))?
        } else {
            let first_month = ((quarter - 1) * 3 + 1) as u32;
            NaiveDate::from_ymd_opt(year, first_month, 1).ok_or_else(|| err("invalid date"))?
        };
        let base_nanos = extract_base_time_nanos(m);
        let nanos = time_fields_from_map(m, base_nanos);
        return Ok(CypherValue::LocalDateTime(CypherLocalDateTime {
            date,
            nanos,
            cycles: 0,
        }));
    }

    let month = get_int(m, "month")
        .or_else(|| base_date.map(|d| d.month() as i64))
        .unwrap_or(1) as u32;
    let day = get_int(m, "day")
        .or_else(|| base_date.map(|d| d.day() as i64))
        .unwrap_or(1) as u32;
    let wide_y = get_int(m, "year")
        .or_else(|| base_date.map(|d| d.year() as i64))
        .unwrap_or(0);
    let (date, cycles) = wide_from_ymd(wide_y, month, day).ok_or_else(|| err("invalid date"))?;
    let base_nanos = extract_base_time_nanos(m);
    let nanos = time_fields_from_map(m, base_nanos);
    Ok(CypherValue::LocalDateTime(CypherLocalDateTime {
        date,
        nanos,
        cycles,
    }))
}

pub(super) fn datetime_from_map(
    m: &BTreeMap<String, CypherValue>,
) -> Result<CypherValue, CypherError> {
    if let Some(CypherValue::String(s)) = m.get("datetime") {
        return parse_datetime_string(s);
    }

    // epoch construction datetime({epochMillis: ms}) / datetime({epochSeconds: s [, nanosecond:
    // n]}), UTC. Neo4j temporal map keys are case-insensitive (generated queries often write
    // `epochmillis`)
    let get_int_ci = |m: &BTreeMap<String, CypherValue>, key: &str| -> Option<i64> {
        get_int(m, key).or_else(|| {
            m.iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .and_then(|(_, v)| match v {
                    CypherValue::Integer(i) => Some(*i),
                    _ => None,
                })
        })
    };
    if let Some(ms) = get_int_ci(m, "epochMillis") {
        return func_datetime_from_epoch_millis(&[CypherValue::Integer(ms)]);
    }
    if let Some(secs) = get_int_ci(m, "epochSeconds") {
        let extra_nanos = get_int(m, "nanosecond").unwrap_or(0);
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let total_nanos = secs * NANOS_PER_SECOND_I + extra_nanos;
        let days = total_nanos.div_euclid(NANOS_PER_DAY_I);
        let day_nanos = total_nanos.rem_euclid(NANOS_PER_DAY_I) as u64;
        let date = add_days_to_date(epoch, days)?;
        return Ok(CypherValue::DateTime(CypherDateTime {
            date,
            nanos: day_nanos,
            offset_secs: 0,
            tz_name: None,
        }));
    }

    let base_date = extract_base_date(m);
    let year = get_int(m, "year")
        .or_else(|| base_date.map(|d| d.year() as i64))
        .unwrap_or(0) as i32;

    // Week-based
    if let Some(week) = get_int(m, "week") {
        let default_dow = base_date
            .map(|d| d.weekday().number_from_monday() as i64)
            .unwrap_or(1);
        let dow = get_int(m, "dayOfWeek").unwrap_or(default_dow) as u32;
        let wd = weekday_from_num(dow).ok_or_else(|| err("dayOfWeek must be 1-7"))?;
        let iso_year = get_int(m, "year")
            .map(|y| y as i32)
            .or_else(|| base_date.map(|d| d.iso_week().year()))
            .unwrap_or(0);
        let date = NaiveDate::from_isoywd_opt(iso_year, week as u32, wd)
            .ok_or_else(|| err("invalid ISO week date"))?;
        let base_nanos = extract_base_time_nanos(m);
        let nanos = time_fields_from_map(m, base_nanos);
        let (offset_secs, tz_name) = resolve_map_timezone(m)?;
        let offset_secs = if let Some(ref tz) = tz_name {
            resolve_tz_offset(tz, &date, nanos)?
        } else {
            offset_secs
        };
        return Ok(CypherValue::DateTime(CypherDateTime {
            date,
            nanos,
            offset_secs,
            tz_name,
        }));
    }

    // Ordinal-based
    if let Some(ordinal) = get_int(m, "ordinalDay") {
        let date = NaiveDate::from_yo_opt(year, ordinal as u32)
            .ok_or_else(|| err("invalid ordinal date"))?;
        let base_nanos = extract_base_time_nanos(m);
        let nanos = time_fields_from_map(m, base_nanos);
        let (offset_secs, tz_name) = resolve_map_timezone(m)?;
        let offset_secs = if let Some(ref tz) = tz_name {
            resolve_tz_offset(tz, &date, nanos)?
        } else {
            offset_secs
        };
        return Ok(CypherValue::DateTime(CypherDateTime {
            date,
            nanos,
            offset_secs,
            tz_name,
        }));
    }

    // Quarter-based
    if let Some(quarter) = get_int(m, "quarter") {
        let date = if let Some(doq) = get_int(m, "dayOfQuarter") {
            let first_month = ((quarter - 1) * 3 + 1) as u32;
            let base =
                NaiveDate::from_ymd_opt(year, first_month, 1).ok_or_else(|| err("invalid date"))?;
            chrono::Duration::try_days(doq - 1)
                .and_then(|d| base.checked_add_signed(d))
                .ok_or_else(|| err("invalid quarter date"))?
        } else if let Some(bd) = base_date {
            let month_in_q = (bd.month() - 1) % 3;
            let new_month = ((quarter - 1) * 3 + 1) as u32 + month_in_q;
            let day = bd.day();
            NaiveDate::from_ymd_opt(year, new_month, day)
                .ok_or_else(|| err("invalid quarter date"))?
        } else {
            let first_month = ((quarter - 1) * 3 + 1) as u32;
            NaiveDate::from_ymd_opt(year, first_month, 1).ok_or_else(|| err("invalid date"))?
        };
        let base_nanos = extract_base_time_nanos(m);
        let nanos = time_fields_from_map(m, base_nanos);
        let (offset_secs, tz_name) = resolve_map_timezone(m)?;
        let offset_secs = if let Some(ref tz) = tz_name {
            resolve_tz_offset(tz, &date, nanos)?
        } else {
            offset_secs
        };
        return Ok(CypherValue::DateTime(CypherDateTime {
            date,
            nanos,
            offset_secs,
            tz_name,
        }));
    }

    let month = get_int(m, "month")
        .or_else(|| base_date.map(|d| d.month() as i64))
        .unwrap_or(1) as u32;
    let day = get_int(m, "day")
        .or_else(|| base_date.map(|d| d.day() as i64))
        .unwrap_or(1) as u32;
    let date = NaiveDate::from_ymd_opt(year, month, day).ok_or_else(|| err("invalid date"))?;
    let base_nanos = extract_base_time_nanos(m);
    let nanos = time_fields_from_map(m, base_nanos);
    let base_offset = extract_base_offset(m);
    let (offset_secs, tz_name) = resolve_map_timezone(m)?;

    // If timezone is named, re-resolve offset for the specific date/time
    let offset_secs = if let Some(ref tz) = tz_name {
        resolve_tz_offset(tz, &date, nanos)?
    } else {
        offset_secs
    };

    // time zone changed and no explicit time components: convert the wall-clock time (overrides
    // apply in the source zone first), re-resolving the source offset for the NEW date when the
    // base has a named zone
    let src_offset_resolved = if let Some(base_off) = base_offset {
        let src_tz_name = extract_base_tz_name(m);
        if let Some(ref stz) = src_tz_name {
            resolve_tz_offset(stz, &date, nanos).unwrap_or(base_off)
        } else {
            base_off
        }
    } else {
        0
    };
    let (date, nanos) = if base_offset.is_some() {
        let src_offset = src_offset_resolved;
        if m.contains_key("timezone") && src_offset != offset_secs {
            let diff_secs = (offset_secs - src_offset) as i64;
            let converted = nanos as i64 + diff_secs * NANOS_PER_SECOND_I;
            let day_nanos = 24 * 3600 * NANOS_PER_SECOND_I;
            if converted < 0 {
                let prev = date.pred_opt().unwrap_or(date);
                (
                    prev,
                    ((converted % day_nanos + day_nanos) % day_nanos) as u64,
                )
            } else if converted >= day_nanos {
                let next = date.succ_opt().unwrap_or(date);
                (next, (converted % day_nanos) as u64)
            } else {
                (date, converted as u64)
            }
        } else {
            (date, nanos)
        }
    } else {
        (date, nanos)
    };

    Ok(CypherValue::DateTime(CypherDateTime {
        date,
        nanos,
        offset_secs,
        tz_name,
    }))
}

fn resolve_map_timezone(
    m: &BTreeMap<String, CypherValue>,
) -> Result<(i32, Option<String>), CypherError> {
    if let Some(CypherValue::String(tz)) = m.get("timezone") {
        if tz.starts_with('+') || tz.starts_with('-') || tz == "Z" || tz == "z" {
            let offset = parse_offset_str(tz)?;
            return Ok((offset, None));
        }
        // Named timezone
        let offset = parse_offset_or_tz(tz)?;
        return Ok((offset, Some(tz.clone())));
    }
    let offset = extract_base_offset(m).unwrap_or(0);
    // Check if base has tz_name - prefer "time" source over "date" source
    for key in &["time", "datetime", "date", "localdatetime"] {
        if let Some(CypherValue::DateTime(dt)) = m.get(*key)
            && dt.tz_name.is_some()
        {
            return Ok((offset, dt.tz_name.clone()));
        }
    }
    Ok((offset, None))
}

pub(super) fn duration_from_map(
    m: &BTreeMap<String, CypherValue>,
) -> Result<CypherValue, CypherError> {
    let mut months: f64 = 0.0;
    let mut days: f64 = 0.0;
    let mut nanos: f64 = 0.0;

    if let Some(v) = get_f64(m, "years") {
        months += v * 12.0;
    }
    if let Some(v) = get_f64(m, "months") {
        months += v;
    }
    if let Some(v) = get_f64(m, "weeks") {
        days += v * 7.0;
    }
    if let Some(v) = get_f64(m, "days") {
        days += v;
    }
    if let Some(v) = get_f64(m, "hours") {
        nanos += v * NANOS_PER_HOUR as f64;
    }
    if let Some(v) = get_f64(m, "minutes") {
        nanos += v * NANOS_PER_MINUTE as f64;
    }
    if let Some(v) = get_f64(m, "seconds") {
        nanos += v * NANOS_PER_SECOND as f64;
    }
    if let Some(v) = get_f64(m, "milliseconds") {
        nanos += v * NANOS_PER_MILLI as f64;
    }
    if let Some(v) = get_f64(m, "microseconds") {
        nanos += v * NANOS_PER_MICRO as f64;
    }
    if let Some(v) = get_f64(m, "nanoseconds") {
        nanos += v;
    }

    // Handle cascading: fractional months -> days, fractional days -> nanos
    let months_i = months.trunc() as i64;
    let frac_months = months - months.trunc();
    days += frac_months * (365.2425 / 12.0); // Gregorian average days per month

    let days_i = days.trunc() as i64;
    let frac_days = days - days.trunc();
    nanos += frac_days * NANOS_PER_DAY as f64;

    Ok(CypherValue::Duration(CypherDuration::from_nanos(
        months_i,
        days_i,
        nanos as i128,
    )))
}
