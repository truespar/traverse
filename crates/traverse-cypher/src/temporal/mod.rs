//! Cypher temporal values and functions: dates, times, datetimes and durations, their parsing,
//! construction, arithmetic, truncation and formatting.

#![allow(missing_docs)]

mod access;
mod arith;
mod clock;
mod construct;
mod map;
mod parse;
mod truncate;
mod types;

use chrono::{Datelike, Months, NaiveDate, Weekday};

use crate::error::CypherError;

pub use access::get_temporal_property;
pub use arith::{
    duration_div, duration_mul, func_duration_between, func_duration_in_days,
    func_duration_in_months, func_duration_in_seconds, temporal_add, temporal_sub,
};
pub use clock::{
    ClockMode, func_date_now, func_date_now_with_clock, func_datetime_from_epoch,
    func_datetime_from_epoch_millis, func_datetime_now, func_datetime_now_with_clock,
    func_localdatetime_now, func_localdatetime_now_with_clock, func_localtime_now,
    func_localtime_now_with_clock, func_time_now, func_time_now_with_clock, func_timestamp,
    temporal_to_string,
};
pub use construct::{
    func_date, func_datetime, func_duration, func_format, func_localdatetime, func_localtime,
    func_time,
};
pub use parse::{
    parse_date_string, parse_datetime_string, parse_duration_string, parse_localdatetime_string,
    parse_localtime_string, parse_time_string,
};
pub use truncate::func_truncate;
pub use types::{
    CypherDate, CypherDateTime, CypherDuration, CypherLocalDateTime, CypherLocalTime, CypherTime,
    DAYS_PER_CYCLE, YEARS_PER_CYCLE, wide_date_from_epoch_days,
};
pub(crate) use types::{normalize_wide, wide_epoch_days, wide_from_ymd, wide_year};

// -- Constants ---------------------------------------------------------

const NANOS_PER_SECOND: u64 = 1_000_000_000;
const NANOS_PER_MILLI: u64 = 1_000_000;
const NANOS_PER_MICRO: u64 = 1_000;
const NANOS_PER_MINUTE: u64 = 60 * NANOS_PER_SECOND;
const NANOS_PER_HOUR: u64 = 3600 * NANOS_PER_SECOND;
const NANOS_PER_DAY: u64 = 86400 * NANOS_PER_SECOND;

const NANOS_PER_SECOND_I: i64 = 1_000_000_000;
const NANOS_PER_MILLI_I: i64 = 1_000_000;
const NANOS_PER_MICRO_I: i64 = 1_000;
const NANOS_PER_MINUTE_I: i64 = 60 * NANOS_PER_SECOND_I;
const NANOS_PER_HOUR_I: i64 = 3600 * NANOS_PER_SECOND_I;
const NANOS_PER_DAY_I: i64 = 86400 * NANOS_PER_SECOND_I;

// -- Helpers ----------------------------------------------------------

fn err(msg: &str) -> CypherError {
    CypherError::type_error(msg.to_string())
}

fn quarter_of(date: &NaiveDate) -> u32 {
    (date.month() - 1) / 3 + 1
}

fn day_of_quarter(date: &NaiveDate) -> u32 {
    let q = quarter_of(date);
    let first_month = (q - 1) * 3 + 1;
    let start = NaiveDate::from_ymd_opt(date.year(), first_month, 1).unwrap();
    (date.signed_duration_since(start).num_days() + 1) as u32
}

fn add_months_to_date(date: NaiveDate, months: i64) -> Result<NaiveDate, CypherError> {
    if months >= 0 {
        date.checked_add_months(Months::new(months as u32))
            .ok_or_else(|| err("date overflow adding months"))
    } else {
        date.checked_sub_months(Months::new((-months) as u32))
            .ok_or_else(|| err("date overflow subtracting months"))
    }
}

fn add_days_to_date(date: NaiveDate, days: i64) -> Result<NaiveDate, CypherError> {
    chrono::Duration::try_days(days)
        .and_then(|d| date.checked_add_signed(d))
        .ok_or_else(|| err("date overflow adding days"))
}

/// Month addition with chrono's `checked_add_months` semantics (day clamped to the target month),
/// over the full +/-999,999,999-year range via cycles.
fn add_months_wide(
    date: NaiveDate,
    cycles: i64,
    months: i64,
) -> Result<(NaiveDate, i64), CypherError> {
    if cycles == 0 {
        // fast path keeps the usual behavior exactly; wide arithmetic only if chrono overflows
        if let Ok(d) = add_months_to_date(date, months) {
            return Ok((d, 0));
        }
    }
    let total = wide_year(&date, cycles) * 12 + (date.month() as i64 - 1) + months;
    let new_year = total.div_euclid(12);
    let new_month = (total.rem_euclid(12) + 1) as u32;
    let day = date.day();
    wide_from_ymd(new_year, new_month, day)
        .or_else(|| {
            // clamp to the last day of the target month
            for d in [30, 29, 28] {
                if d < day
                    && let Some(r) = wide_from_ymd(new_year, new_month, d)
                {
                    return Some(r);
                }
            }
            None
        })
        .map(|(d, c)| normalize_wide(d, c))
        .ok_or_else(|| err("date overflow adding months"))
}

/// Day addition via epoch days (wide-aware).
fn add_days_wide(date: NaiveDate, cycles: i64, days: i64) -> Result<(NaiveDate, i64), CypherError> {
    if cycles == 0
        && let Ok(d) = add_days_to_date(date, days)
    {
        return Ok((d, 0));
    }
    let total = wide_epoch_days(&date, cycles)
        .checked_add(days)
        .ok_or_else(|| err("date overflow adding days"))?;
    let (d, c) =
        wide_date_from_epoch_days(total).ok_or_else(|| err("date overflow adding days"))?;
    if wide_year(&d, c).abs() > 999_999_999 {
        return Err(err("date overflow adding days"));
    }
    Ok((d, c))
}

fn weekday_from_num(n: u32) -> Option<Weekday> {
    match n {
        1 => Some(Weekday::Mon),
        2 => Some(Weekday::Tue),
        3 => Some(Weekday::Wed),
        4 => Some(Weekday::Thu),
        5 => Some(Weekday::Fri),
        6 => Some(Weekday::Sat),
        7 => Some(Weekday::Sun),
        _ => None,
    }
}

fn weekday_to_num(wd: Weekday) -> u32 {
    wd.number_from_monday()
}

fn nanos_to_hms(nanos: u64) -> (u32, u32, u32, u64) {
    let total_secs = nanos / NANOS_PER_SECOND;
    let hours = (total_secs / 3600) as u32;
    let minutes = ((total_secs % 3600) / 60) as u32;
    let seconds = (total_secs % 60) as u32;
    let nano_part = nanos % NANOS_PER_SECOND;
    (hours, minutes, seconds, nano_part)
}

fn hms_to_nanos(h: u64, m: u64, s: u64, nano: u64) -> u64 {
    h * NANOS_PER_HOUR + m * NANOS_PER_MINUTE + s * NANOS_PER_SECOND + nano
}
