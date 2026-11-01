//! Property access on temporal values (`d.year`, `dt.epochSeconds`, ...).

use chrono::Datelike;

use crate::types::CypherValue;

use super::types::{
    CypherDate, CypherDateTime, CypherDuration, CypherLocalDateTime, CypherLocalTime, CypherTime,
    YEARS_PER_CYCLE, format_offset, wide_epoch_days, wide_year,
};
use super::{
    NANOS_PER_HOUR_I, NANOS_PER_MICRO, NANOS_PER_MICRO_I, NANOS_PER_MILLI, NANOS_PER_MILLI_I,
    NANOS_PER_MINUTE_I, NANOS_PER_SECOND, NANOS_PER_SECOND_I, day_of_quarter, nanos_to_hms,
    quarter_of, weekday_to_num,
};

pub fn get_temporal_property(val: &CypherValue, key: &str) -> Option<CypherValue> {
    match val {
        CypherValue::Date(d) => get_date_property(d, key),
        CypherValue::LocalTime(t) => get_localtime_property(t, key),
        CypherValue::Time(t) => get_time_property(t, key),
        CypherValue::LocalDateTime(ldt) => get_date_property(
            &CypherDate {
                date: ldt.date,
                cycles: ldt.cycles,
            },
            key,
        )
        .or_else(|| get_localtime_property(&CypherLocalTime { nanos: ldt.nanos }, key))
        .or_else(|| get_localdatetime_extra(ldt, key)),
        CypherValue::DateTime(dt) => {
            get_date_property(
                &CypherDate {
                    date: dt.date,
                    cycles: 0,
                },
                key,
            )
            // DateTime-specific first (timezone uses tz_name, epoch*)
            .or_else(|| get_datetime_extra(dt, key))
            .or_else(|| {
                get_time_property(
                    &CypherTime {
                        nanos: dt.nanos,
                        offset_secs: dt.offset_secs,
                    },
                    key,
                )
            })
        }
        CypherValue::Duration(d) => get_duration_property(d, key),
        _ => None,
    }
}

fn get_date_property(d: &CypherDate, key: &str) -> Option<CypherValue> {
    match key {
        "year" => Some(CypherValue::Integer(wide_year(&d.date, d.cycles))),
        "month" => Some(CypherValue::Integer(d.date.month() as i64)),
        "day" => Some(CypherValue::Integer(d.date.day() as i64)),
        "quarter" => Some(CypherValue::Integer(quarter_of(&d.date) as i64)),
        "week" => Some(CypherValue::Integer(d.date.iso_week().week() as i64)),
        // ISO week structure repeats every 400 years, so inner week-year + cycle offset is exact
        "weekYear" => Some(CypherValue::Integer(
            d.date.iso_week().year() as i64 + YEARS_PER_CYCLE * d.cycles,
        )),
        "ordinalDay" => Some(CypherValue::Integer(d.date.ordinal() as i64)),
        "weekDay" | "dayOfWeek" => {
            Some(CypherValue::Integer(weekday_to_num(d.date.weekday()) as i64))
        }
        "dayOfQuarter" => Some(CypherValue::Integer(day_of_quarter(&d.date) as i64)),
        _ => None,
    }
}

fn get_localtime_property(t: &CypherLocalTime, key: &str) -> Option<CypherValue> {
    let (h, m, s, ns) = nanos_to_hms(t.nanos);
    match key {
        "hour" => Some(CypherValue::Integer(h as i64)),
        "minute" => Some(CypherValue::Integer(m as i64)),
        "second" => Some(CypherValue::Integer(s as i64)),
        "millisecond" => Some(CypherValue::Integer((ns / NANOS_PER_MILLI) as i64)),
        "microsecond" => Some(CypherValue::Integer((ns / NANOS_PER_MICRO) as i64)),
        "nanosecond" => Some(CypherValue::Integer(ns as i64)),
        _ => None,
    }
}

fn get_time_property(t: &CypherTime, key: &str) -> Option<CypherValue> {
    let (h, m, s, ns) = nanos_to_hms(t.nanos);
    match key {
        "hour" => Some(CypherValue::Integer(h as i64)),
        "minute" => Some(CypherValue::Integer(m as i64)),
        "second" => Some(CypherValue::Integer(s as i64)),
        "millisecond" => Some(CypherValue::Integer((ns / NANOS_PER_MILLI) as i64)),
        "microsecond" => Some(CypherValue::Integer((ns / NANOS_PER_MICRO) as i64)),
        "nanosecond" => Some(CypherValue::Integer(ns as i64)),
        "timezone" | "offset" => Some(CypherValue::String(format_offset(t.offset_secs))),
        "offsetMinutes" => Some(CypherValue::Integer((t.offset_secs / 60) as i64)),
        "offsetSeconds" => Some(CypherValue::Integer(t.offset_secs as i64)),
        _ => None,
    }
}

fn get_localdatetime_extra(ldt: &CypherLocalDateTime, key: &str) -> Option<CypherValue> {
    match key {
        "epochSeconds" => {
            let days = wide_epoch_days(&ldt.date, ldt.cycles);
            let total_secs = days * 86400 + (ldt.nanos / NANOS_PER_SECOND) as i64;
            Some(CypherValue::Integer(total_secs))
        }
        "epochMillis" => {
            let days = wide_epoch_days(&ldt.date, ldt.cycles);
            let total_ms = days * 86_400_000 + (ldt.nanos / NANOS_PER_MILLI) as i64;
            Some(CypherValue::Integer(total_ms))
        }
        _ => None,
    }
}

fn get_datetime_extra(dt: &CypherDateTime, key: &str) -> Option<CypherValue> {
    match key {
        "epochSeconds" => {
            let epoch_nanos = dt.to_epoch_nanos();
            Some(CypherValue::Integer(
                (epoch_nanos / NANOS_PER_SECOND as i128) as i64,
            ))
        }
        "epochMillis" => {
            let epoch_nanos = dt.to_epoch_nanos();
            Some(CypherValue::Integer(
                (epoch_nanos / NANOS_PER_MILLI as i128) as i64,
            ))
        }
        "timezone" => {
            if let Some(ref tz) = dt.tz_name {
                Some(CypherValue::String(tz.clone()))
            } else {
                Some(CypherValue::String(format_offset(dt.offset_secs)))
            }
        }
        "offset" => Some(CypherValue::String(format_offset(dt.offset_secs))),
        "offsetMinutes" => Some(CypherValue::Integer((dt.offset_secs / 60) as i64)),
        "offsetSeconds" => Some(CypherValue::Integer(dt.offset_secs as i64)),
        _ => None,
    }
}

fn get_duration_property(d: &CypherDuration, key: &str) -> Option<CypherValue> {
    match key {
        "years" => Some(CypherValue::Integer(d.months / 12)),
        "quarters" => Some(CypherValue::Integer(d.months / 3)),
        "months" => Some(CypherValue::Integer(d.months)),
        "weeks" => Some(CypherValue::Integer(d.days / 7)),
        "days" => Some(CypherValue::Integer(d.days)),
        "hours" => Some(CypherValue::Integer(
            d.nanos().div_euclid(NANOS_PER_HOUR_I as i128) as i64,
        )),
        "minutes" => Some(CypherValue::Integer(
            d.nanos().div_euclid(NANOS_PER_MINUTE_I as i128) as i64,
        )),
        "seconds" => Some(CypherValue::Integer(
            d.nanos().div_euclid(NANOS_PER_SECOND_I as i128) as i64,
        )),
        "milliseconds" => Some(CypherValue::Integer(
            d.nanos().div_euclid(NANOS_PER_MILLI_I as i128) as i64,
        )),
        "microseconds" => Some(CypherValue::Integer(
            d.nanos().div_euclid(NANOS_PER_MICRO_I as i128) as i64,
        )),
        "nanoseconds" => Some(CypherValue::Integer(d.nanos() as i64)),
        // "of" variants
        "monthsOfYear" => Some(CypherValue::Integer(d.months.rem_euclid(12))),
        "monthsOfQuarter" => Some(CypherValue::Integer(d.months.rem_euclid(3))),
        "quartersOfYear" => Some(CypherValue::Integer((d.months.rem_euclid(12)) / 3)),
        "daysOfWeek" => Some(CypherValue::Integer(d.days.rem_euclid(7))),
        "minutesOfHour" => Some(CypherValue::Integer(
            ((d.nanos() / NANOS_PER_MINUTE_I as i128).rem_euclid(60)) as i64,
        )),
        "secondsOfMinute" => Some(CypherValue::Integer(
            ((d.nanos() / NANOS_PER_SECOND_I as i128).rem_euclid(60)) as i64,
        )),
        "millisecondsOfSecond" => Some(CypherValue::Integer(
            ((d.nanos() / NANOS_PER_MILLI_I as i128).rem_euclid(1000)) as i64,
        )),
        "microsecondsOfSecond" => Some(CypherValue::Integer(
            ((d.nanos() / NANOS_PER_MICRO_I as i128).rem_euclid(1_000_000)) as i64,
        )),
        "nanosecondsOfSecond" => Some(CypherValue::Integer(
            d.nanos().rem_euclid(NANOS_PER_SECOND_I as i128) as i64,
        )),
        _ => None,
    }
}
