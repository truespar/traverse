//! The temporal value types, their display, equality and ordering, and the wide-year date math
//! (400-year Gregorian cycles) beyond chrono's range.

use std::fmt;

use chrono::{Datelike, NaiveDate};

use super::{NANOS_PER_DAY, NANOS_PER_SECOND, NANOS_PER_SECOND_I};

// -- Type Definitions --------------------------------------------------

#[derive(Debug, Clone)]
pub struct CypherDate {
    pub date: NaiveDate,
    /// Offset in 400-year Gregorian cycles: the calendar year is
    /// `date.year() + 400 * cycles`. 0 for dates chrono can represent (+/-262143),
    /// non-zero only for the wide proleptic range (Neo4j/java.time allow
    /// +/-999,999,999). A cycle is exactly 146097 days = 20871 weeks, so weekday,
    /// leap year, ISO week and month logic on the inner `date` stay correct; only
    /// year outputs and epoch arithmetic add the cycle offset.
    pub cycles: i64,
}

#[derive(Debug, Clone)]
pub struct CypherLocalTime {
    pub nanos: u64,
}

#[derive(Debug, Clone)]
pub struct CypherTime {
    pub nanos: u64,
    pub offset_secs: i32,
}

#[derive(Debug, Clone)]
pub struct CypherLocalDateTime {
    pub date: NaiveDate,
    pub nanos: u64,
    /// 400-year cycle offset for the date part - see [`CypherDate::cycles`].
    pub cycles: i64,
}

#[derive(Debug, Clone)]
pub struct CypherDateTime {
    pub date: NaiveDate,
    pub nanos: u64,
    pub offset_secs: i32,
    pub tz_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CypherDuration {
    pub months: i64,
    pub days: i64,
    /// Whole seconds of the time part. i64 seconds, not i64 nanos: i64 nanos cap at
    /// +/-292 years while the TCK needs spans of +/-999,999,999 years. Seconds + subsec
    /// instead of one i128 keeps CypherValue 8-byte aligned (i128 forces 16 and
    /// grows every value by 8 bytes), and matches the Bolt wire format (months,
    /// days, seconds, nanoseconds).
    pub seconds: i64,
    /// Sub-second nanoseconds, |n| < 1e9, same sign as `seconds` (canonicalized by
    /// [`CypherDuration::from_nanos`]).
    pub subsec_nanos: i32,
}

impl CypherDuration {
    /// From a total-nanoseconds time part, canonicalizing the seconds/subsec split.
    pub fn from_nanos(months: i64, days: i64, nanos: i128) -> Self {
        let seconds = (nanos / NANOS_PER_SECOND_I as i128) as i64;
        let subsec_nanos = (nanos % NANOS_PER_SECOND_I as i128) as i32;
        Self {
            months,
            days,
            seconds,
            subsec_nanos,
        }
    }

    /// Total nanoseconds of the time part.
    pub fn nanos(&self) -> i128 {
        self.seconds as i128 * NANOS_PER_SECOND_I as i128 + self.subsec_nanos as i128
    }
}

// -- Wide-date helpers (400-year Gregorian cycles) --------------------

/// Days in one 400-year Gregorian cycle (= 20871 weeks exactly).
pub const DAYS_PER_CYCLE: i64 = 146_097;
/// Years per cycle.
pub const YEARS_PER_CYCLE: i64 = 400;
/// Years with an absolute value below this always get 0 cycles; well inside chrono's +/-262143, so
/// every date representable today keeps its exact representation.
const NARROW_YEAR_BOUND: i64 = 260_000;

/// Wide calendar year -> (inner chrono year, cycle offset); years inside the narrow bound keep 0
/// cycles.
pub(crate) fn split_wide_year(year: i64) -> (i32, i64) {
    if year.abs() <= NARROW_YEAR_BOUND {
        (year as i32, 0)
    } else {
        let inner = year.rem_euclid(YEARS_PER_CYCLE); // 0..=399
        ((inner as i32), (year - inner) / YEARS_PER_CYCLE)
    }
}

/// Wide date from any i64 year + month/day. Validity (month lengths, leap years) is
/// cycle-invariant, so chrono validates the inner date.
pub(crate) fn wide_from_ymd(year: i64, month: u32, day: u32) -> Option<(NaiveDate, i64)> {
    if year.abs() > 999_999_999 {
        return None;
    }
    let (inner, cycles) = split_wide_year(year);
    NaiveDate::from_ymd_opt(inner, month, day).map(|d| (d, cycles))
}

/// Calendar year of a wide date.
pub(crate) fn wide_year(date: &NaiveDate, cycles: i64) -> i64 {
    date.year() as i64 + YEARS_PER_CYCLE * cycles
}

/// Days since 1970-01-01 of a wide date (exact; +/-999,999,999 years ~ +/-3.65e11 days fits i64).
pub(crate) fn wide_epoch_days(date: &NaiveDate, cycles: i64) -> i64 {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    date.signed_duration_since(epoch).num_days() + DAYS_PER_CYCLE * cycles
}

/// Folds cycles back into the inner date when the year fits the narrow bound, so results
/// compare/display like dates that never went wide.
pub(crate) fn normalize_wide(date: NaiveDate, cycles: i64) -> (NaiveDate, i64) {
    if cycles == 0 {
        return (date, 0);
    }
    let year = wide_year(&date, cycles);
    if year.abs() <= NARROW_YEAR_BOUND {
        // same month/day stays valid in the folded year: whole-cycle shifts keep leap status
        if let Some(d) = NaiveDate::from_ymd_opt(year as i32, date.month(), date.day()) {
            return (d, 0);
        }
    }
    (date, cycles)
}

/// Wide date from days since 1970-01-01. Public: the Bolt layer decodes Date/LocalDateTime through
/// it.
pub fn wide_date_from_epoch_days(days: i64) -> Option<(NaiveDate, i64)> {
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
    // fast path when representable; try_days because Duration::days PANICS past ~1.06e11 days,
    // which wide dates exceed
    if let Some(d) = chrono::Duration::try_days(days).and_then(|d| epoch.checked_add_signed(d)) {
        return Some((d, 0));
    }
    let inner_days = days.rem_euclid(DAYS_PER_CYCLE); // 0..DAYS_PER_CYCLE
    let cycles = (days - inner_days) / DAYS_PER_CYCLE;
    let d = epoch.checked_add_signed(chrono::Duration::days(inner_days))?;
    Some(normalize_wide(d, cycles))
}

/// Date part of a wide temporal value (ISO 8601, explicit sign for years outside 0..=9999, like
/// java.time).
fn write_wide_date(f: &mut fmt::Formatter<'_>, date: &NaiveDate, cycles: i64) -> fmt::Result {
    if cycles == 0 {
        return write!(f, "{}", date.format("%Y-%m-%d"));
    }
    let year = wide_year(date, cycles);
    if year < 0 {
        write!(f, "-{:04}-{:02}-{:02}", -year, date.month(), date.day())
    } else {
        write!(f, "+{:04}-{:02}-{:02}", year, date.month(), date.day())
    }
}

// -- Display Implementations ------------------------------------------

fn write_time_fields(f: &mut fmt::Formatter<'_>, nanos: u64) -> fmt::Result {
    let total_secs = nanos / NANOS_PER_SECOND;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    let nano_part = nanos % NANOS_PER_SECOND;
    write!(f, "{hours:02}:{minutes:02}")?;
    if seconds != 0 || nano_part != 0 {
        write!(f, ":{seconds:02}")?;
        write_fractional(f, nano_part)?;
    }
    Ok(())
}

fn write_fractional(f: &mut fmt::Formatter<'_>, nanos: u64) -> fmt::Result {
    if nanos == 0 {
        return Ok(());
    }
    let s = format!("{nanos:09}");
    let trimmed = s.trim_end_matches('0');
    write!(f, ".{trimmed}")
}

pub(super) fn format_offset(offset_secs: i32) -> String {
    if offset_secs == 0 {
        return "Z".to_string();
    }
    let sign = if offset_secs >= 0 { '+' } else { '-' };
    let abs = offset_secs.unsigned_abs();
    let hours = abs / 3600;
    let minutes = (abs % 3600) / 60;
    let seconds = abs % 60;
    if seconds != 0 {
        format!("{sign}{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{sign}{hours:02}:{minutes:02}")
    }
}

impl fmt::Display for CypherDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_wide_date(f, &self.date, self.cycles)
    }
}

impl fmt::Display for CypherLocalTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_time_fields(f, self.nanos)
    }
}

impl fmt::Display for CypherTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_time_fields(f, self.nanos)?;
        write!(f, "{}", format_offset(self.offset_secs))
    }
}

impl fmt::Display for CypherLocalDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_wide_date(f, &self.date, self.cycles)?;
        write!(f, "T")?;
        write_time_fields(f, self.nanos)
    }
}

impl fmt::Display for CypherDateTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}T", self.date.format("%Y-%m-%d"))?;
        write_time_fields(f, self.nanos)?;
        write!(f, "{}", format_offset(self.offset_secs))?;
        if let Some(ref tz) = self.tz_name {
            write!(f, "[{tz}]")?;
        }
        Ok(())
    }
}

impl fmt::Display for CypherDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.months == 0 && self.days == 0 && self.nanos() == 0 {
            return write!(f, "PT0S");
        }
        write!(f, "P")?;
        let years = self.months / 12;
        let rem_months = self.months % 12;
        if years != 0 {
            write!(f, "{years}Y")?;
        }
        if rem_months != 0 {
            write!(f, "{rem_months}M")?;
        }
        if self.days != 0 {
            write!(f, "{}D", self.days)?;
        }
        if self.nanos() != 0 {
            write!(f, "T")?;
            let abs_nanos = self.nanos().unsigned_abs();
            let neg = self.nanos() < 0;
            let total_secs = abs_nanos / NANOS_PER_SECOND as u128;
            let hours = total_secs / 3600;
            let minutes = (total_secs % 3600) / 60;
            let seconds = total_secs % 60;
            let nano_part = abs_nanos % NANOS_PER_SECOND as u128;
            if hours != 0 {
                if neg {
                    write!(f, "-")?
                }
                write!(f, "{hours}H")?;
            }
            if minutes != 0 {
                if neg {
                    write!(f, "-")?
                }
                write!(f, "{minutes}M")?;
            }
            if seconds != 0 || nano_part != 0 {
                if neg {
                    write!(f, "-")?
                }
                write!(f, "{seconds}")?;
                if nano_part != 0 {
                    let s = format!("{nano_part:09}");
                    let trimmed = s.trim_end_matches('0');
                    write!(f, ".{trimmed}")?;
                }
                write!(f, "S")?;
            }
        }
        Ok(())
    }
}

// -- PartialEq --------------------------------------------------------

impl PartialEq for CypherDate {
    fn eq(&self, other: &Self) -> bool {
        wide_epoch_days(&self.date, self.cycles) == wide_epoch_days(&other.date, other.cycles)
    }
}

impl PartialEq for CypherLocalTime {
    fn eq(&self, other: &Self) -> bool {
        self.nanos == other.nanos
    }
}

impl PartialEq for CypherTime {
    fn eq(&self, other: &Self) -> bool {
        // compare by UTC instant
        let a = self.nanos as i64 - self.offset_secs as i64 * NANOS_PER_SECOND_I;
        let b = other.nanos as i64 - other.offset_secs as i64 * NANOS_PER_SECOND_I;
        a == b
    }
}

impl PartialEq for CypherLocalDateTime {
    fn eq(&self, other: &Self) -> bool {
        wide_epoch_days(&self.date, self.cycles) == wide_epoch_days(&other.date, other.cycles)
            && self.nanos == other.nanos
    }
}

impl PartialEq for CypherDateTime {
    fn eq(&self, other: &Self) -> bool {
        // compare by UTC instant
        self.to_epoch_nanos() == other.to_epoch_nanos()
    }
}

impl PartialEq for CypherDuration {
    fn eq(&self, other: &Self) -> bool {
        self.months == other.months && self.days == other.days && self.nanos() == other.nanos()
    }
}

// -- PartialOrd -------------------------------------------------------

impl PartialOrd for CypherDate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        wide_epoch_days(&self.date, self.cycles)
            .partial_cmp(&wide_epoch_days(&other.date, other.cycles))
    }
}

impl PartialOrd for CypherLocalTime {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.nanos.partial_cmp(&other.nanos)
    }
}

impl PartialOrd for CypherTime {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        let a = self.nanos as i64 - self.offset_secs as i64 * NANOS_PER_SECOND_I;
        let b = other.nanos as i64 - other.offset_secs as i64 * NANOS_PER_SECOND_I;
        a.partial_cmp(&b)
    }
}

impl PartialOrd for CypherLocalDateTime {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        let a = wide_epoch_days(&self.date, self.cycles);
        let b = wide_epoch_days(&other.date, other.cycles);
        match a.cmp(&b) {
            std::cmp::Ordering::Equal => self.nanos.partial_cmp(&other.nanos),
            ord => Some(ord),
        }
    }
}

impl PartialOrd for CypherDateTime {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.to_epoch_nanos().partial_cmp(&other.to_epoch_nanos())
    }
}

impl CypherDateTime {
    pub(super) fn to_epoch_nanos(&self) -> i128 {
        let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
        let days = self.date.signed_duration_since(epoch).num_days();
        let local_nanos = days as i128 * NANOS_PER_DAY as i128 + self.nanos as i128;
        local_nanos - self.offset_secs as i128 * NANOS_PER_SECOND as i128
    }
}
