//! `<temporal>.truncate(unit, value [, fields])`.

use std::collections::BTreeMap;

use chrono::{Datelike, NaiveDate, Weekday};

use crate::error::CypherError;
use crate::types::CypherValue;

use super::map::{get_int, parse_offset_or_tz};
use super::parse::{parse_offset_str, resolve_tz_offset};
use super::types::{CypherDate, CypherDateTime, CypherLocalDateTime, CypherLocalTime, CypherTime};
use super::{
    NANOS_PER_HOUR, NANOS_PER_MICRO, NANOS_PER_MILLI, NANOS_PER_MINUTE, NANOS_PER_SECOND, err,
    quarter_of, weekday_from_num,
};

pub fn func_truncate(func_name: &str, args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    if args.is_empty() || args.len() > 3 {
        return Err(err("truncate() expects 2 or 3 arguments"));
    }
    let unit = match &args[0] {
        CypherValue::String(s) => s.to_lowercase(),
        _ => return Err(err("first argument to truncate() must be a string unit")),
    };
    if args.len() < 2 {
        return Err(err("truncate() requires a temporal value"));
    }
    let temporal = &args[1];
    let fill = if args.len() == 3 {
        match &args[2] {
            CypherValue::Map(m) => Some(m),
            _ => None,
        }
    } else {
        None
    };

    // Determine which type to produce based on function name prefix
    let type_prefix = func_name.split('.').next().unwrap_or("");

    match type_prefix {
        "date" => truncate_date(&unit, temporal, fill),
        "localtime" => truncate_localtime(&unit, temporal, fill),
        "time" => truncate_time(&unit, temporal, fill),
        "localdatetime" => truncate_localdatetime(&unit, temporal, fill),
        "datetime" => truncate_datetime(&unit, temporal, fill),
        _ => Err(err(&format!("unknown truncation type: {func_name}"))),
    }
}

fn truncate_date(
    unit: &str,
    temporal: &CypherValue,
    fill: Option<&BTreeMap<String, CypherValue>>,
) -> Result<CypherValue, CypherError> {
    let (base_date, base_cycles) = match temporal {
        CypherValue::Date(d) => (d.date, d.cycles),
        CypherValue::DateTime(dt) => (dt.date, 0),
        CypherValue::LocalDateTime(ldt) => (ldt.date, ldt.cycles),
        _ => return Err(err("truncation requires a date-like temporal value")),
    };

    let truncated = truncate_date_to_unit(&base_date, unit)?;

    // Apply fill overrides
    if let Some(fill_map) = fill {
        let mut date = truncated;
        // Handle dayOfWeek fill for week truncation
        if unit == "week" {
            if let Some(CypherValue::Integer(dow)) = fill_map.get("dayOfWeek")
                && let Some(wd) = weekday_from_num(*dow as u32)
            {
                let current_dow = date.weekday().number_from_monday();
                let target_dow = wd.number_from_monday();
                if target_dow > current_dow {
                    date = date
                        .checked_add_signed(chrono::Duration::days(
                            (target_dow - current_dow) as i64,
                        ))
                        .unwrap_or(date);
                }
            }
        } else {
            let year = get_int(fill_map, "year").unwrap_or(date.year() as i64) as i32;
            let month = get_int(fill_map, "month").unwrap_or(date.month() as i64) as u32;
            let day = get_int(fill_map, "day").unwrap_or(date.day() as i64) as u32;
            date = NaiveDate::from_ymd_opt(year, month, day)
                .ok_or_else(|| err("invalid date after fill"))?;
        }
        return Ok(CypherValue::Date(CypherDate {
            date,
            cycles: base_cycles,
        }));
    }

    Ok(CypherValue::Date(CypherDate {
        date: truncated,
        cycles: base_cycles,
    }))
}

fn truncate_date_to_unit(date: &NaiveDate, unit: &str) -> Result<NaiveDate, CypherError> {
    match unit {
        "millennium" => {
            let y = (date.year() / 1000) * 1000;
            NaiveDate::from_ymd_opt(y, 1, 1).ok_or_else(|| err("invalid truncated date"))
        }
        "century" => {
            let y = (date.year() / 100) * 100;
            NaiveDate::from_ymd_opt(y, 1, 1).ok_or_else(|| err("invalid truncated date"))
        }
        "decade" => {
            let y = (date.year() / 10) * 10;
            NaiveDate::from_ymd_opt(y, 1, 1).ok_or_else(|| err("invalid truncated date"))
        }
        "year" => {
            NaiveDate::from_ymd_opt(date.year(), 1, 1).ok_or_else(|| err("invalid truncated date"))
        }
        "weekyear" => {
            let iso_year = date.iso_week().year();
            NaiveDate::from_isoywd_opt(iso_year, 1, Weekday::Mon)
                .ok_or_else(|| err("invalid truncated date"))
        }
        "quarter" => {
            let q = quarter_of(date);
            let m = (q - 1) * 3 + 1;
            NaiveDate::from_ymd_opt(date.year(), m, 1).ok_or_else(|| err("invalid truncated date"))
        }
        "month" => NaiveDate::from_ymd_opt(date.year(), date.month(), 1)
            .ok_or_else(|| err("invalid truncated date")),
        "week" => {
            let wd = date.weekday().number_from_monday() - 1; // 0=Mon
            date.checked_sub_signed(chrono::Duration::days(wd as i64))
                .ok_or_else(|| err("invalid truncated date"))
        }
        "day" => Ok(*date),
        _ => Err(err(&format!("unsupported date truncation unit: {unit}"))),
    }
}

fn truncate_localtime(
    unit: &str,
    temporal: &CypherValue,
    fill: Option<&BTreeMap<String, CypherValue>>,
) -> Result<CypherValue, CypherError> {
    let nanos = match temporal {
        CypherValue::LocalTime(t) => t.nanos,
        CypherValue::LocalDateTime(ldt) => ldt.nanos,
        CypherValue::Time(t) => t.nanos,
        CypherValue::DateTime(dt) => dt.nanos,
        _ => return Err(err("truncation requires a time-like temporal value")),
    };
    let mut truncated = truncate_nanos_to_unit(nanos, unit)?;
    if let Some(m) = fill {
        truncated = apply_time_fill(truncated, m, unit);
    }
    Ok(CypherValue::LocalTime(CypherLocalTime { nanos: truncated }))
}

fn truncate_time(
    unit: &str,
    temporal: &CypherValue,
    fill: Option<&BTreeMap<String, CypherValue>>,
) -> Result<CypherValue, CypherError> {
    let (nanos, offset) = match temporal {
        CypherValue::Time(t) => (t.nanos, t.offset_secs),
        CypherValue::DateTime(dt) => (dt.nanos, dt.offset_secs),
        CypherValue::LocalDateTime(ldt) => (ldt.nanos, 0),
        CypherValue::LocalTime(t) => (t.nanos, 0),
        CypherValue::Date(_) => (0u64, 0),
        _ => return Err(err("truncation requires a time-like temporal value")),
    };
    let mut truncated = truncate_nanos_to_unit(nanos, unit)?;
    let mut final_offset = offset;
    if let Some(m) = fill {
        truncated = apply_time_fill(truncated, m, unit);
        if let Some(CypherValue::String(tz)) = m.get("timezone") {
            final_offset = resolve_timezone(tz).unwrap_or(offset);
        }
    }
    Ok(CypherValue::Time(CypherTime {
        nanos: truncated,
        offset_secs: final_offset,
    }))
}

fn truncate_nanos_to_unit(nanos: u64, unit: &str) -> Result<u64, CypherError> {
    match unit {
        "day" => Ok(0),
        "hour" => Ok((nanos / NANOS_PER_HOUR) * NANOS_PER_HOUR),
        "minute" => Ok((nanos / NANOS_PER_MINUTE) * NANOS_PER_MINUTE),
        "second" => Ok((nanos / NANOS_PER_SECOND) * NANOS_PER_SECOND),
        "millisecond" => Ok((nanos / NANOS_PER_MILLI) * NANOS_PER_MILLI),
        "microsecond" => Ok((nanos / NANOS_PER_MICRO) * NANOS_PER_MICRO),
        _ => Err(err(&format!("unsupported time truncation unit: {unit}"))),
    }
}

fn resolve_timezone(tz: &str) -> Option<i32> {
    parse_offset_or_tz(tz).ok()
}

fn apply_time_fill(mut nanos: u64, m: &BTreeMap<String, CypherValue>, unit: &str) -> u64 {
    // Apply fill components for sub-units below the truncation point
    let units_order = [
        "hour",
        "minute",
        "second",
        "millisecond",
        "microsecond",
        "nanosecond",
    ];
    let unit_idx = units_order.iter().position(|u| *u == unit).unwrap_or(0);
    // Fill sub-units below truncation point
    for &fill_unit in &units_order[unit_idx + 1..] {
        if let Some(CypherValue::Integer(v)) = m.get(fill_unit) {
            let v = *v as u64;
            match fill_unit {
                "hour" => nanos += v * NANOS_PER_HOUR,
                "minute" => nanos += v * NANOS_PER_MINUTE,
                "second" => nanos += v * NANOS_PER_SECOND,
                "millisecond" => nanos += v * NANOS_PER_MILLI,
                "microsecond" => nanos += v * NANOS_PER_MICRO,
                "nanosecond" => nanos += v,
                _ => {}
            }
        }
    }
    nanos
}

fn apply_datetime_fill(
    mut date: NaiveDate,
    mut nanos: u64,
    m: &BTreeMap<String, CypherValue>,
    unit: &str,
) -> (NaiveDate, u64) {
    // Date sub-unit fill: day, dayOfWeek, month, etc. - only fill below the truncation unit
    let date_units = [
        "millennium",
        "century",
        "decade",
        "year",
        "weekyear",
        "quarter",
        "month",
        "week",
        "day",
    ];
    let time_units = [
        "hour",
        "minute",
        "second",
        "millisecond",
        "microsecond",
        "nanosecond",
    ];

    let unit_in_date = date_units.iter().position(|u| *u == unit);
    let unit_in_time = time_units.iter().position(|u| *u == unit);

    if let Some(idx) = unit_in_date {
        // Fill date sub-components below truncation point
        // Special handling for dayOfWeek when truncating to week
        if unit == "week"
            && let Some(CypherValue::Integer(dow)) = m.get("dayOfWeek")
            && let Some(wd) = weekday_from_num(*dow as u32)
        {
            let current_dow = date.weekday().number_from_monday();
            let target_dow = wd.number_from_monday();
            if target_dow > current_dow {
                date = date
                    .checked_add_signed(chrono::Duration::days((target_dow - current_dow) as i64))
                    .unwrap_or(date);
            }
        }
        for &fill_unit in &date_units[idx + 1..] {
            match fill_unit {
                "day" => {
                    if let Some(CypherValue::Integer(d)) = m.get("day") {
                        date = NaiveDate::from_ymd_opt(date.year(), date.month(), *d as u32)
                            .unwrap_or(date);
                    }
                }
                "month" => {
                    if let Some(CypherValue::Integer(mo)) = m.get("month") {
                        date = NaiveDate::from_ymd_opt(date.year(), *mo as u32, date.day())
                            .unwrap_or(date);
                    }
                }
                _ => {}
            }
        }
        // Also fill time sub-components
        nanos = apply_time_fill(nanos, m, "day");
    } else if let Some(idx) = unit_in_time {
        // Fill time sub-components
        nanos = apply_time_fill(nanos, m, time_units[idx]);
    }

    (date, nanos)
}

fn truncate_localdatetime(
    unit: &str,
    temporal: &CypherValue,
    fill: Option<&BTreeMap<String, CypherValue>>,
) -> Result<CypherValue, CypherError> {
    let (date, nanos, base_cycles) = match temporal {
        CypherValue::LocalDateTime(ldt) => (ldt.date, ldt.nanos, ldt.cycles),
        CypherValue::DateTime(dt) => (dt.date, dt.nanos, 0),
        CypherValue::Date(d) => (d.date, 0u64, d.cycles),
        _ => return Err(err("truncation requires a datetime-like temporal value")),
    };

    let is_time_unit = matches!(
        unit,
        "hour" | "minute" | "second" | "millisecond" | "microsecond"
    );

    let truncated_date = if is_time_unit {
        date
    } else {
        truncate_date_to_unit(&date, unit)?
    };
    let truncated_nanos = if is_time_unit {
        truncate_nanos_to_unit(nanos, unit)?
    } else {
        0
    };

    let (final_date, final_nanos) = if let Some(fill_map) = fill {
        apply_datetime_fill(truncated_date, truncated_nanos, fill_map, unit)
    } else {
        (truncated_date, truncated_nanos)
    };

    Ok(CypherValue::LocalDateTime(CypherLocalDateTime {
        date: final_date,
        nanos: final_nanos,
        cycles: base_cycles,
    }))
}

fn truncate_datetime(
    unit: &str,
    temporal: &CypherValue,
    fill: Option<&BTreeMap<String, CypherValue>>,
) -> Result<CypherValue, CypherError> {
    let (date, nanos, offset, tz) = match temporal {
        CypherValue::DateTime(dt) => (dt.date, dt.nanos, dt.offset_secs, dt.tz_name.clone()),
        CypherValue::LocalDateTime(ldt) => (ldt.date, ldt.nanos, 0, None),
        CypherValue::Date(d) => (d.date, 0u64, 0, None),
        _ => return Err(err("truncation requires a datetime-like temporal value")),
    };

    let is_time_unit = matches!(
        unit,
        "hour" | "minute" | "second" | "millisecond" | "microsecond"
    );

    let truncated_date = if is_time_unit {
        date
    } else {
        truncate_date_to_unit(&date, unit)?
    };
    let truncated_nanos = if is_time_unit {
        truncate_nanos_to_unit(nanos, unit)?
    } else {
        0
    };

    let (final_date, final_nanos) = if let Some(fill_map) = fill {
        apply_datetime_fill(truncated_date, truncated_nanos, fill_map, unit)
    } else {
        (truncated_date, truncated_nanos)
    };

    // time zone override in the fill map; named zones depend on the local date (DST), so resolve
    // against `final_date`, not today
    let (final_offset, final_tz) = if let Some(fill_map) = fill {
        if let Some(CypherValue::String(tz_str)) = fill_map.get("timezone") {
            if tz_str.starts_with('+') || tz_str.starts_with('-') || tz_str == "Z" || tz_str == "z"
            {
                let off = parse_offset_str(tz_str).unwrap_or(offset);
                (off, None)
            } else {
                let off = resolve_tz_offset(tz_str, &final_date, final_nanos).unwrap_or(offset);
                (off, Some(tz_str.clone()))
            }
        } else {
            (offset, tz)
        }
    } else {
        (offset, tz)
    };

    Ok(CypherValue::DateTime(CypherDateTime {
        date: final_date,
        nanos: final_nanos,
        offset_secs: final_offset,
        tz_name: final_tz,
    }))
}
