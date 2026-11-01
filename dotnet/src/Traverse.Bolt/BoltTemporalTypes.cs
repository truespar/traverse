namespace Traverse.Bolt;

/// <summary>
/// A time value with UTC offset. Represents Bolt Time type (0x54).
/// .NET has no built-in "time with offset" type - TimeOnly lacks offset, DateTimeOffset includes a date.
/// </summary>
public sealed record BoltOffsetTime(TimeOnly Time, TimeSpan Offset);

/// <summary>
/// A date-time value with an IANA time zone identifier. Represents Bolt DateTimeZoneId type (0x69).
/// DateTimeOffset alone loses the zone name (needed for DST-aware round-tripping).
/// </summary>
public sealed record BoltZonedDateTime(DateTimeOffset DateTime, string TimeZone);

/// <summary>
/// A duration with month, day, second, and nanosecond components. Represents Bolt Duration type (0x45).
/// TimeSpan cannot represent months (variable-length), so a dedicated type is needed.
/// </summary>
public sealed record BoltDuration(long Months, long Days, long Seconds, int Nanoseconds);
