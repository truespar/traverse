//! The statement clock (one `now` per query unless asked otherwise) and the epoch / now functions.

use chrono::NaiveDate;
use chrono::Timelike;

use crate::error::CypherError;
use crate::types::CypherValue;

use super::types::{CypherDate, CypherDateTime, CypherLocalDateTime, CypherLocalTime, CypherTime};
use super::{
    NANOS_PER_DAY_I, NANOS_PER_MILLI_I, NANOS_PER_SECOND_I, add_days_to_date, err, hms_to_nanos,
};

/// Cypher clock source (ISO section 15.4 / Neo4j):
///  - `Statement`: wall clock frozen for the statement; the default for bare
///    `now()`-style calls and `*.statement()`.
///  - `Realtime`: sampled at every call.
///
/// `*.transaction()` maps to `Statement`: the executor has no transaction-scoped clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClockMode {
    /// Statement-scoped: cached at first call.
    Statement,
    /// Wall clock per call.
    Realtime,
}

/// "Now" for a clock mode: statement / transaction read the running query's statement clock (the
/// same on every thread working on it), realtime samples `chrono::Local::now()`; outside a query
/// every call samples.
fn clock_now(mode: ClockMode) -> chrono::DateTime<chrono::Local> {
    if mode == ClockMode::Realtime {
        return chrono::Local::now();
    }
    match crate::query::with_current(|c| c.map(|ctx| ctx.statement_now())) {
        Some((nanos, offset)) => {
            // the offset came from a valid local time
            let fixed = chrono::FixedOffset::east_opt(offset).unwrap();
            chrono::DateTime::from_timestamp_nanos(nanos)
                .with_timezone(&fixed)
                .with_timezone(&chrono::Local)
        }
        None => chrono::Local::now(),
    }
}

// -- Misc Functions ---------------------------------------------------

pub fn func_datetime_from_epoch(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    if args.len() != 2 {
        return Err(err("datetime.fromepoch() expects 2 arguments"));
    }
    let seconds = match &args[0] {
        CypherValue::Integer(i) => *i,
        CypherValue::Float(f) => *f as i64,
        _ => return Err(err("datetime.fromepoch() expects integer seconds")),
    };
    let nanos_arg = match &args[1] {
        CypherValue::Integer(i) => *i,
        CypherValue::Float(f) => *f as i64,
        _ => return Err(err("datetime.fromepoch() expects integer nanoseconds")),
    };

    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let total_nanos = seconds * NANOS_PER_SECOND_I + nanos_arg;
    let days = total_nanos.div_euclid(NANOS_PER_DAY_I);
    let day_nanos = total_nanos.rem_euclid(NANOS_PER_DAY_I) as u64;
    let date = add_days_to_date(epoch, days)?;

    Ok(CypherValue::DateTime(CypherDateTime {
        date,
        nanos: day_nanos,
        offset_secs: 0,
        tz_name: None,
    }))
}

pub fn func_datetime_from_epoch_millis(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    if args.len() != 1 {
        return Err(err("datetime.fromepochmillis() expects 1 argument"));
    }
    let millis = match &args[0] {
        CypherValue::Integer(i) => *i,
        CypherValue::Float(f) => *f as i64,
        _ => {
            return Err(err(
                "datetime.fromepochmillis() expects integer milliseconds",
            ));
        }
    };

    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    let total_nanos = millis * NANOS_PER_MILLI_I;
    let days = total_nanos.div_euclid(NANOS_PER_DAY_I);
    let day_nanos = total_nanos.rem_euclid(NANOS_PER_DAY_I) as u64;
    let date = add_days_to_date(epoch, days)?;

    Ok(CypherValue::DateTime(CypherDateTime {
        date,
        nanos: day_nanos,
        offset_secs: 0,
        tz_name: None,
    }))
}

/// `timestamp()`: ms since the Unix epoch (UTC) as INTEGER from the statement clock, like
/// `datetime().epochMillis` (Neo4j: frozen for the statement).
pub fn func_timestamp(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    if !args.is_empty() {
        return Err(err("timestamp() expects no arguments"));
    }
    Ok(CypherValue::Integer(
        clock_now(ClockMode::Statement).timestamp_millis(),
    ))
}

pub fn func_date_now(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    func_date_now_with_clock(args, ClockMode::Statement)
}

pub fn func_date_now_with_clock(
    args: &[CypherValue],
    mode: ClockMode,
) -> Result<CypherValue, CypherError> {
    if args.first().is_some_and(|a| a.is_null()) {
        return Ok(CypherValue::Null);
    }
    let now = clock_now(mode);
    let date = now.date_naive();
    Ok(CypherValue::Date(CypherDate { date, cycles: 0 }))
}

pub fn func_localtime_now(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    func_localtime_now_with_clock(args, ClockMode::Statement)
}

pub fn func_localtime_now_with_clock(
    args: &[CypherValue],
    mode: ClockMode,
) -> Result<CypherValue, CypherError> {
    if args.first().is_some_and(|a| a.is_null()) {
        return Ok(CypherValue::Null);
    }
    let now = clock_now(mode);
    let t = now.time();
    let nanos = hms_to_nanos(
        t.hour() as u64,
        t.minute() as u64,
        t.second() as u64,
        t.nanosecond() as u64,
    );
    Ok(CypherValue::LocalTime(CypherLocalTime { nanos }))
}

pub fn func_time_now(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    func_time_now_with_clock(args, ClockMode::Statement)
}

pub fn func_time_now_with_clock(
    args: &[CypherValue],
    mode: ClockMode,
) -> Result<CypherValue, CypherError> {
    if args.first().is_some_and(|a| a.is_null()) {
        return Ok(CypherValue::Null);
    }
    let now = clock_now(mode);
    let t = now.time();
    let nanos = hms_to_nanos(
        t.hour() as u64,
        t.minute() as u64,
        t.second() as u64,
        t.nanosecond() as u64,
    );
    let offset_secs = now.offset().local_minus_utc();
    Ok(CypherValue::Time(CypherTime { nanos, offset_secs }))
}

pub fn func_localdatetime_now(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    func_localdatetime_now_with_clock(args, ClockMode::Statement)
}

pub fn func_localdatetime_now_with_clock(
    args: &[CypherValue],
    mode: ClockMode,
) -> Result<CypherValue, CypherError> {
    if args.first().is_some_and(|a| a.is_null()) {
        return Ok(CypherValue::Null);
    }
    let now = clock_now(mode);
    let date = now.date_naive();
    let t = now.time();
    let nanos = hms_to_nanos(
        t.hour() as u64,
        t.minute() as u64,
        t.second() as u64,
        t.nanosecond() as u64,
    );
    Ok(CypherValue::LocalDateTime(CypherLocalDateTime {
        date,
        nanos,
        cycles: 0,
    }))
}

pub fn func_datetime_now(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    func_datetime_now_with_clock(args, ClockMode::Statement)
}

pub fn func_datetime_now_with_clock(
    args: &[CypherValue],
    mode: ClockMode,
) -> Result<CypherValue, CypherError> {
    if args.first().is_some_and(|a| a.is_null()) {
        return Ok(CypherValue::Null);
    }
    let now = clock_now(mode);
    let date = now.date_naive();
    let t = now.time();
    let nanos = hms_to_nanos(
        t.hour() as u64,
        t.minute() as u64,
        t.second() as u64,
        t.nanosecond() as u64,
    );
    let offset_secs = now.offset().local_minus_utc();
    Ok(CypherValue::DateTime(CypherDateTime {
        date,
        nanos,
        offset_secs,
        tz_name: None,
    }))
}

pub fn temporal_to_string(val: &CypherValue) -> String {
    match val {
        CypherValue::Date(d) => d.to_string(),
        CypherValue::LocalTime(t) => t.to_string(),
        CypherValue::Time(t) => t.to_string(),
        CypherValue::LocalDateTime(ldt) => ldt.to_string(),
        CypherValue::DateTime(dt) => dt.to_string(),
        CypherValue::Duration(d) => d.to_string(),
        _ => String::new(),
    }
}
