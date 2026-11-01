using System.Buffers.Binary;
using System.Text;
using Traverse.Bolt;

namespace Traverse.Bolt.Internal;

/// <summary>
/// Encodes PackStream values into a byte buffer using Span&lt;byte&gt;.
/// All methods advance the position cursor. Zero allocations on the write path.
/// </summary>
internal static class PackStreamWriter
{
    public static void WriteNull(byte[] buf, ref int pos)
    {
        buf[pos++] = Markers.NULL;
    }

    public static void WriteBool(byte[] buf, ref int pos, bool value)
    {
        buf[pos++] = value ? Markers.TRUE : Markers.FALSE;
    }

    public static void WriteInt64(byte[] buf, ref int pos, long value)
    {
        if (value >= -16 && value <= 127)
        {
            // Tiny int: single byte
            buf[pos++] = (byte)value;
        }
        else if (value >= sbyte.MinValue && value <= sbyte.MaxValue)
        {
            buf[pos++] = Markers.INT8;
            buf[pos++] = (byte)(sbyte)value;
        }
        else if (value >= short.MinValue && value <= short.MaxValue)
        {
            buf[pos++] = Markers.INT16;
            BinaryPrimitives.WriteInt16BigEndian(buf.AsSpan(pos), (short)value);
            pos += 2;
        }
        else if (value >= int.MinValue && value <= int.MaxValue)
        {
            buf[pos++] = Markers.INT32;
            BinaryPrimitives.WriteInt32BigEndian(buf.AsSpan(pos), (int)value);
            pos += 4;
        }
        else
        {
            buf[pos++] = Markers.INT64;
            BinaryPrimitives.WriteInt64BigEndian(buf.AsSpan(pos), value);
            pos += 8;
        }
    }

    public static void WriteFloat64(byte[] buf, ref int pos, double value)
    {
        buf[pos++] = Markers.FLOAT64;
        BinaryPrimitives.WriteInt64BigEndian(buf.AsSpan(pos), BitConverter.DoubleToInt64Bits(value));
        pos += 8;
    }

    public static void WriteString(byte[] buf, ref int pos, string value)
    {
        int byteCount = Encoding.UTF8.GetByteCount(value);
        WriteStringHeader(buf, ref pos, byteCount);
        Encoding.UTF8.GetBytes(value, buf.AsSpan(pos));
        pos += byteCount;
    }

    private static void WriteStringHeader(byte[] buf, ref int pos, int byteLength)
    {
        if (byteLength <= 15)
        {
            buf[pos++] = (byte)(Markers.TINY_STRING | byteLength);
        }
        else if (byteLength <= 255)
        {
            buf[pos++] = Markers.STRING8;
            buf[pos++] = (byte)byteLength;
        }
        else if (byteLength <= 65535)
        {
            buf[pos++] = Markers.STRING16;
            BinaryPrimitives.WriteUInt16BigEndian(buf.AsSpan(pos), (ushort)byteLength);
            pos += 2;
        }
        else
        {
            buf[pos++] = Markers.STRING32;
            BinaryPrimitives.WriteInt32BigEndian(buf.AsSpan(pos), byteLength);
            pos += 4;
        }
    }

    public static void WriteMapHeader(byte[] buf, ref int pos, int count)
    {
        if (count <= 15)
        {
            buf[pos++] = (byte)(Markers.TINY_MAP | count);
        }
        else if (count <= 255)
        {
            buf[pos++] = Markers.MAP8;
            buf[pos++] = (byte)count;
        }
        else if (count <= 65535)
        {
            buf[pos++] = Markers.MAP16;
            BinaryPrimitives.WriteUInt16BigEndian(buf.AsSpan(pos), (ushort)count);
            pos += 2;
        }
        else
        {
            buf[pos++] = Markers.MAP32;
            BinaryPrimitives.WriteInt32BigEndian(buf.AsSpan(pos), count);
            pos += 4;
        }
    }

    public static void WriteListHeader(byte[] buf, ref int pos, int count)
    {
        if (count <= 15)
        {
            buf[pos++] = (byte)(Markers.TINY_LIST | count);
        }
        else if (count <= 255)
        {
            buf[pos++] = Markers.LIST8;
            buf[pos++] = (byte)count;
        }
        else if (count <= 65535)
        {
            buf[pos++] = Markers.LIST16;
            BinaryPrimitives.WriteUInt16BigEndian(buf.AsSpan(pos), (ushort)count);
            pos += 2;
        }
        else
        {
            buf[pos++] = Markers.LIST32;
            BinaryPrimitives.WriteInt32BigEndian(buf.AsSpan(pos), count);
            pos += 4;
        }
    }

    public static void WriteBytes(byte[] buf, ref int pos, byte[] value)
    {
        int length = value.Length;
        if (length <= 255)
        {
            buf[pos++] = Markers.BYTES8;
            buf[pos++] = (byte)length;
        }
        else if (length <= 65535)
        {
            buf[pos++] = Markers.BYTES16;
            BinaryPrimitives.WriteUInt16BigEndian(buf.AsSpan(pos), (ushort)length);
            pos += 2;
        }
        else
        {
            buf[pos++] = Markers.BYTES32;
            BinaryPrimitives.WriteInt32BigEndian(buf.AsSpan(pos), length);
            pos += 4;
        }
        Buffer.BlockCopy(value, 0, buf, pos, length);
        pos += length;
    }

    public static void WriteStructHeader(byte[] buf, ref int pos, byte tag, int fieldCount)
    {
        buf[pos++] = (byte)(Markers.TINY_STRUCT | fieldCount);
        buf[pos++] = tag;
    }

    // -- Temporal types ------------------------------------------------------

    private static readonly DateOnly UnixEpochDate = new(1970, 1, 1);

    public static void WriteDate(byte[] buf, ref int pos, DateOnly value)
    {
        WriteStructHeader(buf, ref pos, TypeTag.DATE, 1);
        WriteInt64(buf, ref pos, value.DayNumber - UnixEpochDate.DayNumber);
    }

    public static void WriteLocalTime(byte[] buf, ref int pos, TimeOnly value)
    {
        WriteStructHeader(buf, ref pos, TypeTag.LOCAL_TIME, 1);
        WriteInt64(buf, ref pos, value.Ticks * 100);
    }

    public static void WriteTime(byte[] buf, ref int pos, BoltOffsetTime value)
    {
        WriteStructHeader(buf, ref pos, TypeTag.TIME, 2);
        WriteInt64(buf, ref pos, value.Time.Ticks * 100);
        WriteInt64(buf, ref pos, (long)value.Offset.TotalSeconds);
    }

    public static void WriteLocalDateTime(byte[] buf, ref int pos, DateTime value)
    {
        WriteStructHeader(buf, ref pos, TypeTag.LOCAL_DT, 2);
        long totalTicks = value.Ticks - DateTimeOffset.UnixEpoch.Ticks;
        WriteInt64(buf, ref pos, totalTicks / TimeSpan.TicksPerSecond);
        WriteInt64(buf, ref pos, (totalTicks % TimeSpan.TicksPerSecond) * 100);
    }

    public static void WriteDateTimeOffset(byte[] buf, ref int pos, DateTimeOffset value)
    {
        WriteStructHeader(buf, ref pos, TypeTag.DT_OFFSET, 3);
        long utcTicks = value.UtcTicks - DateTimeOffset.UnixEpoch.Ticks;
        WriteInt64(buf, ref pos, utcTicks / TimeSpan.TicksPerSecond);
        WriteInt64(buf, ref pos, (utcTicks % TimeSpan.TicksPerSecond) * 100);
        WriteInt64(buf, ref pos, (long)value.Offset.TotalSeconds);
    }

    public static void WriteZonedDateTime(byte[] buf, ref int pos, BoltZonedDateTime value)
    {
        WriteStructHeader(buf, ref pos, TypeTag.DT_ZONE, 3);
        long utcTicks = value.DateTime.UtcTicks - DateTimeOffset.UnixEpoch.Ticks;
        WriteInt64(buf, ref pos, utcTicks / TimeSpan.TicksPerSecond);
        WriteInt64(buf, ref pos, (utcTicks % TimeSpan.TicksPerSecond) * 100);
        WriteString(buf, ref pos, value.TimeZone);
    }

    public static void WriteDuration(byte[] buf, ref int pos, BoltDuration value)
    {
        WriteStructHeader(buf, ref pos, TypeTag.DURATION, 4);
        WriteInt64(buf, ref pos, value.Months);
        WriteInt64(buf, ref pos, value.Days);
        WriteInt64(buf, ref pos, value.Seconds);
        WriteInt64(buf, ref pos, value.Nanoseconds);
    }

    // -- Spatial types ------------------------------------------------------

    public static void WritePoint2D(byte[] buf, ref int pos, BoltPoint2D value)
    {
        WriteStructHeader(buf, ref pos, TypeTag.POINT2D, 3);
        WriteInt64(buf, ref pos, value.Srid);
        WriteFloat64(buf, ref pos, value.X);
        WriteFloat64(buf, ref pos, value.Y);
    }

    public static void WritePoint3D(byte[] buf, ref int pos, BoltPoint3D value)
    {
        WriteStructHeader(buf, ref pos, TypeTag.POINT3D, 4);
        WriteInt64(buf, ref pos, value.Srid);
        WriteFloat64(buf, ref pos, value.X);
        WriteFloat64(buf, ref pos, value.Y);
        WriteFloat64(buf, ref pos, value.Z);
    }

    /// <summary>Write a value of unknown type (for parameter maps).</summary>
    public static void WriteValue(byte[] buf, ref int pos, object? value)
    {
        switch (value)
        {
            case null:
                WriteNull(buf, ref pos);
                break;
            case bool b:
                WriteBool(buf, ref pos, b);
                break;
            case int i:
                WriteInt64(buf, ref pos, i);
                break;
            case long l:
                WriteInt64(buf, ref pos, l);
                break;
            case double d:
                WriteFloat64(buf, ref pos, d);
                break;
            case float f:
                WriteFloat64(buf, ref pos, f);
                break;
            case string s:
                WriteString(buf, ref pos, s);
                break;
            case byte[] bytes:
                WriteBytes(buf, ref pos, bytes);
                break;
            case DateOnly date:
                WriteDate(buf, ref pos, date);
                break;
            case TimeOnly time:
                WriteLocalTime(buf, ref pos, time);
                break;
            case DateTime dt:
                WriteLocalDateTime(buf, ref pos, dt);
                break;
            case DateTimeOffset dto:
                WriteDateTimeOffset(buf, ref pos, dto);
                break;
            case BoltOffsetTime ot:
                WriteTime(buf, ref pos, ot);
                break;
            case BoltZonedDateTime zdt:
                WriteZonedDateTime(buf, ref pos, zdt);
                break;
            case BoltDuration dur:
                WriteDuration(buf, ref pos, dur);
                break;
            case BoltPoint2D p2:
                WritePoint2D(buf, ref pos, p2);
                break;
            case BoltPoint3D p3:
                WritePoint3D(buf, ref pos, p3);
                break;
            case Dictionary<string, object> map:
                WriteMapHeader(buf, ref pos, map.Count);
                foreach (var (k, v) in map)
                {
                    WriteString(buf, ref pos, k);
                    WriteValue(buf, ref pos, v);
                }
                break;
            case IList<object> list:
                WriteListHeader(buf, ref pos, list.Count);
                foreach (var item in list)
                    WriteValue(buf, ref pos, item);
                break;
            default:
                throw new NotSupportedException($"PackStream encoding not supported for type {value.GetType().Name}");
        }
    }
}
