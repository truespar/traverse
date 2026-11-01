//! Arithmetic: temporal + duration, duration scaling, and `duration.between` / `inMonths` / `inDays` /
//! `inSeconds`.

use chrono::{Datelike, NaiveDate};

use crate::error::CypherError;
use crate::types::CypherValue;

use super::parse::resolve_tz_offset;
use super::types::{
    CypherDate, CypherDateTime, CypherDuration, CypherLocalDateTime, CypherLocalTime, CypherTime,
    wide_epoch_days, wide_year,
};
use super::{
    NANOS_PER_DAY, NANOS_PER_DAY_I, NANOS_PER_SECOND, add_days_to_date, add_days_wide,
    add_months_to_date, add_months_wide, err,
};

// -- Arithmetic -------------------------------------------------------

pub fn temporal_add(
    temporal: &CypherValue,
    dur: &CypherDuration,
) -> Result<CypherValue, CypherError> {
    match temporal {
        CypherValue::Date(d) => {
            let (date, cycles) = add_months_wide(d.date, d.cycles, dur.months)?;
            // For Date (no time component), fold nanos overflow into extra days
            let extra_days = (dur.nanos() / NANOS_PER_DAY as i128) as i64;
            let (date, cycles) = add_days_wide(date, cycles, dur.days + extra_days)?;
            Ok(CypherValue::Date(CypherDate { date, cycles }))
        }
        CypherValue::LocalTime(t) => {
            let new_nanos =
                (t.nanos as i128 + dur.nanos()).rem_euclid(NANOS_PER_DAY as i128) as u64;
            Ok(CypherValue::LocalTime(CypherLocalTime { nanos: new_nanos }))
        }
        CypherValue::Time(t) => {
            let new_nanos =
                (t.nanos as i128 + dur.nanos()).rem_euclid(NANOS_PER_DAY as i128) as u64;
            Ok(CypherValue::Time(CypherTime {
                nanos: new_nanos,
                offset_secs: t.offset_secs,
            }))
        }
        CypherValue::LocalDateTime(ldt) => {
            let (date, cycles) = add_months_wide(ldt.date, ldt.cycles, dur.months)?;
            let (date, cycles) = add_days_wide(date, cycles, dur.days)?;
            let total_nanos = ldt.nanos as i128 + dur.nanos();
            let extra_days = total_nanos.div_euclid(NANOS_PER_DAY as i128) as i64;
            let new_nanos = total_nanos.rem_euclid(NANOS_PER_DAY as i128) as u64;
            let (date, cycles) = add_days_wide(date, cycles, extra_days)?;
            Ok(CypherValue::LocalDateTime(CypherLocalDateTime {
                date,
                nanos: new_nanos,
                cycles,
            }))
        }
        CypherValue::DateTime(dt) => {
            let mut date = add_months_to_date(dt.date, dur.months)?;
            date = add_days_to_date(date, dur.days)?;
            let total_nanos = dt.nanos as i128 + dur.nanos();
            let extra_days = total_nanos.div_euclid(NANOS_PER_DAY as i128) as i64;
            let new_nanos = total_nanos.rem_euclid(NANOS_PER_DAY as i128) as u64;
            date = add_days_to_date(date, extra_days)?;
            Ok(CypherValue::DateTime(CypherDateTime {
                date,
                nanos: new_nanos,
                offset_secs: dt.offset_secs,
                tz_name: dt.tz_name.clone(),
            }))
        }
        CypherValue::Duration(d) => Ok(CypherValue::Duration(CypherDuration::from_nanos(
            d.months + dur.months,
            d.days + dur.days,
            d.nanos() + dur.nanos(),
        ))),
        _ => Err(err(&format!(
            "cannot add duration to {}",
            temporal.type_name()
        ))),
    }
}

pub fn temporal_sub(
    temporal: &CypherValue,
    dur: &CypherDuration,
) -> Result<CypherValue, CypherError> {
    let neg = CypherDuration::from_nanos(-dur.months, -dur.days, -dur.nanos());
    temporal_add(temporal, &neg)
}

pub fn duration_mul(dur: &CypherDuration, factor: f64) -> CypherValue {
    // Multiply each component and cascade fractional remainders
    let months_f = dur.months as f64 * factor;
    let months = months_f.trunc() as i64;
    let month_remainder_days = months_f.fract() * (365.2425 / 12.0); // Gregorian average

    let days_f = dur.days as f64 * factor + month_remainder_days;
    let days = days_f.trunc() as i64;
    let day_remainder_nanos = days_f.fract() * (NANOS_PER_DAY as f64);

    let nanos_f = dur.nanos() as f64 * factor + day_remainder_nanos;
    let nanos = nanos_f as i128;

    CypherValue::Duration(CypherDuration::from_nanos(months, days, nanos))
}

pub fn duration_div(dur: &CypherDuration, divisor: f64) -> Result<CypherValue, CypherError> {
    if divisor == 0.0 {
        return Err(err("division by zero"));
    }
    Ok(duration_mul(dur, 1.0 / divisor))
}

// -- Duration Between -------------------------------------------------

pub fn func_duration_between(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    if args.len() != 2 {
        return Err(err("duration.between() expects 2 arguments"));
    }
    if args[0].is_null() || args[1].is_null() {
        return Ok(CypherValue::Null);
    }

    let is_time_only =
        |v: &CypherValue| matches!(v, CypherValue::LocalTime(_) | CypherValue::Time(_));
    let either_time_only = is_time_only(&args[0]) || is_time_only(&args[1]);

    // Determine if we should compare in UTC (both have offset info)
    let has_offset = |v: &CypherValue| matches!(v, CypherValue::Time(_) | CypherValue::DateTime(_));
    let both_have_offset = has_offset(&args[0]) && has_offset(&args[1]);

    let (d1, c1, n1) = temporal_to_date_nanos(&args[0])?;
    let (d2, c2, n2) = temporal_to_date_nanos(&args[1])?;

    if either_time_only {
        // When either input is time-only, only compute time difference
        let effective_n1 = if both_have_offset {
            n1 as i64 - temporal_offset_secs(&args[0]) as i64 * NANOS_PER_SECOND as i64
        } else {
            n1 as i64
        };
        let effective_n2 = if both_have_offset {
            n2 as i64 - temporal_offset_secs(&args[1]) as i64 * NANOS_PER_SECOND as i64
        } else {
            n2 as i64
        };
        let nanos = effective_n2 - effective_n1;
        return Ok(CypherValue::Duration(CypherDuration::from_nanos(
            0,
            0,
            nanos as i128,
        )));
    }

    // Compute months difference (whole months only, not overshooting)
    let mut months =
        (wide_year(&d2, c2) - wide_year(&d1, c1)) * 12 + (d2.month() as i64 - d1.month() as i64);

    // Adjust months so we don't overshoot: after adding months to d1,
    // the adjusted date should be between d1 and d2 (inclusive).
    let d2_days = wide_epoch_days(&d2, c2);
    let (mut adjusted, mut adj_cycles) = add_months_wide(d1, c1, months)?;
    if months > 0 && wide_epoch_days(&adjusted, adj_cycles) > d2_days {
        months -= 1;
        (adjusted, adj_cycles) = add_months_wide(d1, c1, months)?;
    } else if months < 0 && wide_epoch_days(&adjusted, adj_cycles) < d2_days {
        months += 1;
        (adjusted, adj_cycles) = add_months_wide(d1, c1, months)?;
    }

    let mut days = d2_days - wide_epoch_days(&adjusted, adj_cycles);

    // If both have offsets, convert nanos to UTC for accurate difference
    let effective_n1 = if both_have_offset {
        n1 as i64 - temporal_offset_secs(&args[0]) as i64 * NANOS_PER_SECOND as i64
    } else {
        n1 as i64
    };
    let effective_n2 = if both_have_offset {
        n2 as i64 - temporal_offset_secs(&args[1]) as i64 * NANOS_PER_SECOND as i64
    } else {
        n2 as i64
    };
    let mut nanos = (effective_n2 - effective_n1) as i128;

    // Normalize: if nanos and days have different signs, borrow
    if days > 0 && nanos < 0 {
        days -= 1;
        nanos += NANOS_PER_DAY_I as i128;
    } else if days < 0 && nanos > 0 {
        days += 1;
        nanos -= NANOS_PER_DAY_I as i128;
    }

    Ok(CypherValue::Duration(CypherDuration::from_nanos(
        months, days, nanos,
    )))
}

pub fn func_duration_in_months(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    if args.len() != 2 {
        return Err(err("duration.inMonths() expects 2 arguments"));
    }
    if args[0].is_null() || args[1].is_null() {
        return Ok(CypherValue::Null);
    }
    let is_time_only =
        |v: &CypherValue| matches!(v, CypherValue::LocalTime(_) | CypherValue::Time(_));
    if is_time_only(&args[0]) || is_time_only(&args[1]) {
        return Ok(CypherValue::Duration(CypherDuration::from_nanos(0, 0, 0)));
    }
    let (d1, c1, n1) = temporal_to_date_nanos(&args[0])?;
    let (d2, c2, n2) = temporal_to_date_nanos(&args[1])?;

    let has_offset = |v: &CypherValue| matches!(v, CypherValue::Time(_) | CypherValue::DateTime(_));
    let both_have_offset = has_offset(&args[0]) && has_offset(&args[1]);

    // Use UTC-adjusted nanos when both have offsets
    let effective_n1 = if both_have_offset {
        n1 as i64 - temporal_offset_secs(&args[0]) as i64 * NANOS_PER_SECOND as i64
    } else {
        n1 as i64
    };
    let effective_n2 = if both_have_offset {
        n2 as i64 - temporal_offset_secs(&args[1]) as i64 * NANOS_PER_SECOND as i64
    } else {
        n2 as i64
    };

    let mut months =
        (wide_year(&d2, c2) - wide_year(&d1, c1)) * 12 + (d2.month() as i64 - d1.month() as i64);
    let (adjusted, adj_cycles) = add_months_wide(d1, c1, months)?;
    let (adj_days, d2_days) = (
        wide_epoch_days(&adjusted, adj_cycles),
        wide_epoch_days(&d2, c2),
    );
    if months > 0 && adj_days > d2_days {
        months -= 1;
    } else if months < 0 && adj_days < d2_days {
        months += 1;
    } else if adj_days == d2_days {
        // Same date after month adjustment - check time component for overshoot
        if months > 0 && effective_n2 < effective_n1 {
            months -= 1;
        } else if months < 0 && effective_n2 > effective_n1 {
            months += 1;
        }
    }

    Ok(CypherValue::Duration(CypherDuration::from_nanos(
        months, 0, 0,
    )))
}

pub fn func_duration_in_days(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    if args.len() != 2 {
        return Err(err("duration.inDays() expects 2 arguments"));
    }
    if args[0].is_null() || args[1].is_null() {
        return Ok(CypherValue::Null);
    }
    let is_time_only =
        |v: &CypherValue| matches!(v, CypherValue::LocalTime(_) | CypherValue::Time(_));
    if is_time_only(&args[0]) || is_time_only(&args[1]) {
        return Ok(CypherValue::Duration(CypherDuration::from_nanos(0, 0, 0)));
    }
    let (d1, c1, n1) = temporal_to_date_nanos(&args[0])?;
    let (d2, c2, n2) = temporal_to_date_nanos(&args[1])?;

    let has_offset = |v: &CypherValue| matches!(v, CypherValue::Time(_) | CypherValue::DateTime(_));
    let both_have_offset = has_offset(&args[0]) && has_offset(&args[1]);

    let effective_n1 = if both_have_offset {
        n1 as i64 - temporal_offset_secs(&args[0]) as i64 * NANOS_PER_SECOND as i64
    } else {
        n1 as i64
    };
    let effective_n2 = if both_have_offset {
        n2 as i64 - temporal_offset_secs(&args[1]) as i64 * NANOS_PER_SECOND as i64
    } else {
        n2 as i64
    };

    let mut days = wide_epoch_days(&d2, c2) - wide_epoch_days(&d1, c1);
    // Adjust for time-of-day: if destination time is earlier, one fewer day has elapsed
    if days > 0 && effective_n2 < effective_n1 {
        days -= 1;
    } else if days < 0 && effective_n2 > effective_n1 {
        days += 1;
    }
    Ok(CypherValue::Duration(CypherDuration::from_nanos(
        0, days, 0,
    )))
}

pub fn func_duration_in_seconds(args: &[CypherValue]) -> Result<CypherValue, CypherError> {
    if args.len() != 2 {
        return Err(err("duration.inSeconds() expects 2 arguments"));
    }
    if args[0].is_null() || args[1].is_null() {
        return Ok(CypherValue::Null);
    }
    let is_time_only =
        |v: &CypherValue| matches!(v, CypherValue::LocalTime(_) | CypherValue::Time(_));
    let has_offset = |v: &CypherValue| matches!(v, CypherValue::Time(_) | CypherValue::DateTime(_));
    let both_have_offset = has_offset(&args[0]) && has_offset(&args[1]);

    // Check if either operand has a named timezone (for DST-aware computation)
    let tz_name = temporal_tz_name(&args[0]).or_else(|| temporal_tz_name(&args[1]));
    let use_tz_resolution = tz_name.is_some();

    let (d1, c1, n1) = temporal_to_date_nanos(&args[0])?;
    let (d2, c2, n2) = temporal_to_date_nanos(&args[1])?;

    let day_diff = if is_time_only(&args[0]) || is_time_only(&args[1]) {
        0
    } else {
        wide_epoch_days(&d2, c2) - wide_epoch_days(&d1, c1)
    };

    let (effective_n1, effective_n2) = if use_tz_resolution {
        // Named timezone: resolve each operand's effective offset in that timezone.
        // For time-only types, use the other operand's date for DST resolution.
        let ctx_d1 = if is_time_only(&args[0]) { &d2 } else { &d1 };
        let ctx_d2 = if is_time_only(&args[1]) { &d1 } else { &d2 };
        let off1 = effective_offset_secs(&args[0], ctx_d1, n1, tz_name.as_deref())?;
        let off2 = effective_offset_secs(&args[1], ctx_d2, n2, tz_name.as_deref())?;
        (
            n1 as i64 - off1 as i64 * NANOS_PER_SECOND as i64,
            n2 as i64 - off2 as i64 * NANOS_PER_SECOND as i64,
        )
    } else if both_have_offset {
        // Both have fixed offsets: normalize to UTC
        (
            n1 as i64 - temporal_offset_secs(&args[0]) as i64 * NANOS_PER_SECOND as i64,
            n2 as i64 - temporal_offset_secs(&args[1]) as i64 * NANOS_PER_SECOND as i64,
        )
    } else {
        // Local types: compare raw nanos
        (n1 as i64, n2 as i64)
    };

    let total_nanos =
        day_diff as i128 * NANOS_PER_DAY_I as i128 + (effective_n2 - effective_n1) as i128;
    Ok(CypherValue::Duration(CypherDuration::from_nanos(
        0,
        0,
        total_nanos,
    )))
}

fn temporal_offset_secs(val: &CypherValue) -> i32 {
    match val {
        CypherValue::Time(t) => t.offset_secs,
        CypherValue::DateTime(dt) => dt.offset_secs,
        _ => 0,
    }
}

fn temporal_tz_name(val: &CypherValue) -> Option<String> {
    match val {
        CypherValue::DateTime(dt) => dt.tz_name.clone(),
        _ => None,
    }
}

/// Effective UTC offset of a temporal value: its own for offset-aware types; for local types
/// resolved in `tz_name` (from the other operand) at its local date/time, if given.
fn effective_offset_secs(
    val: &CypherValue,
    date: &NaiveDate,
    nanos: u64,
    tz_name: Option<&str>,
) -> Result<i32, CypherError> {
    match val {
        CypherValue::Time(t) => Ok(t.offset_secs),
        CypherValue::DateTime(dt) => Ok(dt.offset_secs),
        // Local types: if a timezone is provided from the other operand, resolve offset
        CypherValue::LocalTime(_) | CypherValue::LocalDateTime(_) | CypherValue::Date(_) => {
            if let Some(tz) = tz_name {
                resolve_tz_offset(tz, date, nanos).or(Ok(0))
            } else {
                Ok(0)
            }
        }
        _ => Ok(0),
    }
}

fn temporal_to_date_nanos(val: &CypherValue) -> Result<(NaiveDate, i64, u64), CypherError> {
    match val {
        CypherValue::Date(d) => Ok((d.date, d.cycles, 0)),
        CypherValue::LocalTime(t) => Ok((NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(), 0, t.nanos)),
        CypherValue::Time(t) => Ok((NaiveDate::from_ymd_opt(1970, 1, 1).unwrap(), 0, t.nanos)),
        CypherValue::LocalDateTime(ldt) => Ok((ldt.date, ldt.cycles, ldt.nanos)),
        CypherValue::DateTime(dt) => Ok((dt.date, 0, dt.nanos)),
        _ => Err(err(&format!(
            "cannot compute duration between {} values",
            val.type_name()
        ))),
    }
}
