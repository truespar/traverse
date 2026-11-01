using System.Buffers.Binary;
using System.Text;
using Traverse.Bolt;

namespace Traverse.Bolt.Internal;

/// <summary>
/// Decodes PackStream values from a byte buffer using ReadOnlySpan&lt;byte&gt;.
/// All methods advance the position cursor.
/// </summary>
internal static class PackStreamReader
{
    /// <summary>Read any PackStream value and return it as a boxed object (or null).</summary>
    public static object? ReadValue(byte[] buf, ref int pos)
    {
        byte marker = buf[pos];

        // Tiny int (+): 0x00..0x7F
        if (marker <= 0x7F)
        {
            pos++;
            return (long)marker;
        }

        // Tiny int (-): 0xF0..0xFF -> -16..-1
        if (marker >= 0xF0)
        {
            pos++;
            return (long)(sbyte)marker;
        }

        // Tiny string: 0x80..0x8F
        if (marker >= 0x80 && marker <= 0x8F)
            return ReadString(buf, ref pos);

        // Tiny list: 0x90..0x9F
        if (marker >= 0x90 && marker <= 0x9F)
        {
            int count = marker & 0x0F;
            pos++;
            var list = new List<object?>(count);
            for (int i = 0; i < count; i++)
                list.Add(ReadValue(buf, ref pos));
            return list;
        }

        // Tiny map: 0xA0..0xAF
        if (marker >= 0xA0 && marker <= 0xAF)
            return ReadMap(buf, ref pos);

        // Tiny struct: 0xB0..0xBF
        if (marker >= 0xB0 && marker <= 0xBF)
            return ReadStruct(buf, ref pos);

        switch (marker)
        {
            case Markers.NULL:
                pos++;
                return null;
            case Markers.TRUE:
                pos++;
                return true;
            case Markers.FALSE:
                pos++;
                return false;
            case Markers.INT8:
                pos++;
                return (long)(sbyte)buf[pos++];
            case Markers.INT16:
                pos++;
                long i16 = BinaryPrimitives.ReadInt16BigEndian(buf.AsSpan(pos));
                pos += 2;
                return i16;
            case Markers.INT32:
                pos++;
                long i32 = BinaryPrimitives.ReadInt32BigEndian(buf.AsSpan(pos));
                pos += 4;
                return i32;
            case Markers.INT64:
                pos++;
                long i64 = BinaryPrimitives.ReadInt64BigEndian(buf.AsSpan(pos));
                pos += 8;
                return i64;
            case Markers.FLOAT64:
                pos++;
                double f = BitConverter.Int64BitsToDouble(
                    BinaryPrimitives.ReadInt64BigEndian(buf.AsSpan(pos)));
                pos += 8;
                return f;
            case Markers.BYTES8:
            case Markers.BYTES16:
            case Markers.BYTES32:
                return ReadBytes(buf, ref pos);
            case Markers.STRING8:
            case Markers.STRING16:
            case Markers.STRING32:
                return ReadString(buf, ref pos);
            case Markers.LIST8:
            case Markers.LIST16:
            case Markers.LIST32:
            {
                int count = ReadListHeader(buf, ref pos);
                var list = new List<object?>(count);
                for (int i = 0; i < count; i++)
                    list.Add(ReadValue(buf, ref pos));
                return list;
            }
            case Markers.MAP8:
            case Markers.MAP16:
            case Markers.MAP32:
                return ReadMap(buf, ref pos);
            default:
                throw new InvalidDataException($"Unknown PackStream marker: 0x{marker:X2} at position {pos}");
        }
    }

    public static long ReadInt64(byte[] buf, ref int pos)
    {
        byte marker = buf[pos];

        // Tiny int (+)
        if (marker <= 0x7F)
        {
            pos++;
            return marker;
        }

        // Tiny int (-)
        if (marker >= 0xF0)
        {
            pos++;
            return (sbyte)marker;
        }

        return marker switch
        {
            Markers.INT8 => (long)(sbyte)buf[(pos += 2) - 1],
            Markers.INT16 => ReadInt16Advance(buf, ref pos),
            Markers.INT32 => ReadInt32Advance(buf, ref pos),
            Markers.INT64 => ReadInt64Advance(buf, ref pos),
            _ => throw new InvalidDataException($"Expected integer, got marker 0x{marker:X2}"),
        };
    }

    private static long ReadInt16Advance(byte[] buf, ref int pos)
    {
        pos++; // skip marker
        long v = BinaryPrimitives.ReadInt16BigEndian(buf.AsSpan(pos));
        pos += 2;
        return v;
    }

    private static long ReadInt32Advance(byte[] buf, ref int pos)
    {
        pos++; // skip marker
        long v = BinaryPrimitives.ReadInt32BigEndian(buf.AsSpan(pos));
        pos += 4;
        return v;
    }

    private static long ReadInt64Advance(byte[] buf, ref int pos)
    {
        pos++; // skip marker
        long v = BinaryPrimitives.ReadInt64BigEndian(buf.AsSpan(pos));
        pos += 8;
        return v;
    }

    public static double ReadFloat64(byte[] buf, ref int pos)
    {
        if (buf[pos] != Markers.FLOAT64)
            throw new InvalidDataException($"Expected FLOAT64 marker, got 0x{buf[pos]:X2}");
        pos++;
        double v = BitConverter.Int64BitsToDouble(
            BinaryPrimitives.ReadInt64BigEndian(buf.AsSpan(pos)));
        pos += 8;
        return v;
    }

    public static string ReadString(byte[] buf, ref int pos)
    {
        byte marker = buf[pos];
        int length;

        if (marker >= 0x80 && marker <= 0x8F)
        {
            length = marker & 0x0F;
            pos++;
        }
        else
        {
            switch (marker)
            {
                case Markers.STRING8:
                    pos++;
                    length = buf[pos++];
                    break;
                case Markers.STRING16:
                    pos++;
                    length = BinaryPrimitives.ReadUInt16BigEndian(buf.AsSpan(pos));
                    pos += 2;
                    break;
                case Markers.STRING32:
                    pos++;
                    length = BinaryPrimitives.ReadInt32BigEndian(buf.AsSpan(pos));
                    pos += 4;
                    break;
                default:
                    throw new InvalidDataException($"Expected string marker, got 0x{marker:X2}");
            }
        }

        string result = Encoding.UTF8.GetString(buf, pos, length);
        pos += length;
        return result;
    }

    public static byte[] ReadBytes(byte[] buf, ref int pos)
    {
        byte marker = buf[pos];
        int length;

        switch (marker)
        {
            case Markers.BYTES8:
                pos++;
                length = buf[pos++];
                break;
            case Markers.BYTES16:
                pos++;
                length = BinaryPrimitives.ReadUInt16BigEndian(buf.AsSpan(pos));
                pos += 2;
                break;
            case Markers.BYTES32:
                pos++;
                length = BinaryPrimitives.ReadInt32BigEndian(buf.AsSpan(pos));
                pos += 4;
                break;
            default:
                throw new InvalidDataException($"Expected bytes marker, got 0x{marker:X2}");
        }

        var result = new byte[length];
        Buffer.BlockCopy(buf, pos, result, 0, length);
        pos += length;
        return result;
    }

    public static int ReadMapHeader(byte[] buf, ref int pos)
    {
        byte marker = buf[pos];

        if (marker >= 0xA0 && marker <= 0xAF)
        {
            pos++;
            return marker & 0x0F;
        }

        return marker switch
        {
            Markers.MAP8 => (int)buf[(pos += 2) - 1],
            Markers.MAP16 => ReadUInt16Advance(buf, ref pos),
            Markers.MAP32 => ReadInt32AdvanceRaw(buf, ref pos),
            _ => throw new InvalidDataException($"Expected map marker, got 0x{marker:X2}"),
        };
    }

    public static int ReadListHeader(byte[] buf, ref int pos)
    {
        byte marker = buf[pos];

        if (marker >= 0x90 && marker <= 0x9F)
        {
            pos++;
            return marker & 0x0F;
        }

        return marker switch
        {
            Markers.LIST8 => (int)buf[(pos += 2) - 1],
            Markers.LIST16 => ReadUInt16Advance(buf, ref pos),
            Markers.LIST32 => ReadInt32AdvanceRaw(buf, ref pos),
            _ => throw new InvalidDataException($"Expected list marker, got 0x{marker:X2}"),
        };
    }

    private static int ReadUInt16Advance(byte[] buf, ref int pos)
    {
        pos++; // skip marker
        int v = BinaryPrimitives.ReadUInt16BigEndian(buf.AsSpan(pos));
        pos += 2;
        return v;
    }

    private static int ReadInt32AdvanceRaw(byte[] buf, ref int pos)
    {
        pos++; // skip marker
        int v = BinaryPrimitives.ReadInt32BigEndian(buf.AsSpan(pos));
        pos += 4;
        return v;
    }

    public static (byte tag, int fieldCount) ReadStructHeader(byte[] buf, ref int pos)
    {
        byte marker = buf[pos];
        if (marker < 0xB0 || marker > 0xBF)
            throw new InvalidDataException($"Expected struct marker, got 0x{marker:X2}");
        int fields = marker & 0x0F;
        pos++;
        byte tag = buf[pos++];
        return (tag, fields);
    }

    /// <summary>Read a struct, dispatching to typed graph objects for known tags.</summary>
    private static object? ReadStruct(byte[] buf, ref int pos)
    {
        var (tag, fieldCount) = ReadStructHeader(buf, ref pos);

        return tag switch
        {
            GraphTag.NODE => ReadNode(buf, ref pos, fieldCount),
            GraphTag.RELATIONSHIP => ReadRelationship(buf, ref pos, fieldCount),
            GraphTag.UNBOUND_REL => ReadUnboundRelationship(buf, ref pos, fieldCount),
            GraphTag.PATH => ReadPath(buf, ref pos),
            TypeTag.DATE => ReadDate(buf, ref pos),
            TypeTag.LOCAL_TIME => ReadLocalTime(buf, ref pos),
            TypeTag.TIME => ReadTime(buf, ref pos),
            TypeTag.LOCAL_DT => ReadLocalDateTime(buf, ref pos),
            TypeTag.DT_OFFSET => ReadDateTimeOffset(buf, ref pos),
            TypeTag.DT_ZONE => ReadDateTimeZone(buf, ref pos),
            TypeTag.DURATION => ReadDuration(buf, ref pos),
            TypeTag.POINT2D => ReadPoint2D(buf, ref pos),
            TypeTag.POINT3D => ReadPoint3D(buf, ref pos),
            _ => ReadGenericStruct(buf, ref pos, fieldCount),
        };
    }

    /// <summary>Peek whether the next marker is a list (used to detect Bolt 6.0 format).</summary>
    private static bool IsListMarker(byte marker) =>
        (marker >= 0x90 && marker <= 0x9F) ||
        marker == Markers.LIST8 || marker == Markers.LIST16 || marker == Markers.LIST32;

    /// <summary>Peek whether the next marker is a string (used to detect Bolt 6.0 format).</summary>
    private static bool IsStringMarker(byte marker) =>
        (marker >= 0x80 && marker <= 0x8F) ||
        marker == Markers.STRING8 || marker == Markers.STRING16 || marker == Markers.STRING32;

    /// <summary>Parse numeric id from an element id string like "4:123".</summary>
    private static long ParseIdFromElementId(string? elementId)
    {
        if (elementId == null) return 0;
        var colonIdx = elementId.IndexOf(':');
        if (colonIdx >= 0 && long.TryParse(elementId.AsSpan(colonIdx + 1), out var id))
            return id;
        return 0;
    }

    private static BoltNode ReadNode(byte[] buf, ref int pos, int fieldCount)
    {
        // Bolt 6.0+: 3 fields (labels, props, elementId) - no integer id
        // Bolt 5.x:  4 fields (id, labels, props, elementId)
        // Bolt 4.x:  3 fields (id, labels, props)
        // Detect by peeking: list marker = Bolt 6.0 (labels first), integer = Bolt 4.x/5.x (id first)
        bool isBolt6 = IsListMarker(buf[pos]);

        long id = isBolt6 ? 0 : ReadInt64(buf, ref pos);

        int labelCount = ReadListHeader(buf, ref pos);
        var labels = new string[labelCount];
        for (int i = 0; i < labelCount; i++)
            labels[i] = ReadString(buf, ref pos);

        var props = ReadMap(buf, ref pos);

        string? elementId = (isBolt6 || fieldCount >= 4) ? ReadString(buf, ref pos) : null;

        if (isBolt6)
            id = ParseIdFromElementId(elementId);

        return new BoltNode(id, labels, props, elementId);
    }

    private static BoltRelationship ReadRelationship(byte[] buf, ref int pos, int fieldCount)
    {
        // Bolt 6.0+: 5 fields (type, props, elementId, startElementId, endElementId) - no integer ids
        // Bolt 5.x:  8 fields (id, startId, endId, type, props, elementId, startElementId, endElementId)
        // Bolt 4.x:  5 fields (id, startId, endId, type, props)
        // Detect by peeking: string marker = Bolt 6.0 (type first), integer = Bolt 4.x/5.x (id first)
        bool isBolt6 = IsStringMarker(buf[pos]);

        long id = 0, startNodeId = 0, endNodeId = 0;
        if (!isBolt6)
        {
            id = ReadInt64(buf, ref pos);
            startNodeId = ReadInt64(buf, ref pos);
            endNodeId = ReadInt64(buf, ref pos);
        }

        string type = ReadString(buf, ref pos);
        var props = ReadMap(buf, ref pos);

        string? elementId = null, startNodeElementId = null, endNodeElementId = null;
        if (isBolt6 || fieldCount >= 8)
        {
            elementId = ReadString(buf, ref pos);
            startNodeElementId = ReadString(buf, ref pos);
            endNodeElementId = ReadString(buf, ref pos);
        }

        if (isBolt6)
        {
            id = ParseIdFromElementId(elementId);
            startNodeId = ParseIdFromElementId(startNodeElementId);
            endNodeId = ParseIdFromElementId(endNodeElementId);
        }

        return new BoltRelationship(id, startNodeId, endNodeId, type, props,
            elementId, startNodeElementId, endNodeElementId);
    }

    private static BoltUnboundRelationship ReadUnboundRelationship(byte[] buf, ref int pos, int fieldCount)
    {
        // Bolt 6.0+: 3 fields (type, props, elementId) - no integer id
        // Bolt 5.x:  4 fields (id, type, props, elementId)
        // Bolt 4.x:  3 fields (id, type, props)
        // Detect by peeking: string marker = Bolt 6.0 (type first), integer = Bolt 4.x/5.x (id first)
        bool isBolt6 = IsStringMarker(buf[pos]);

        long id = isBolt6 ? 0 : ReadInt64(buf, ref pos);
        string type = ReadString(buf, ref pos);
        var props = ReadMap(buf, ref pos);

        string? elementId = (isBolt6 || fieldCount >= 4) ? ReadString(buf, ref pos) : null;

        if (isBolt6)
            id = ParseIdFromElementId(elementId);

        return new BoltUnboundRelationship(id, type, props, elementId);
    }

    private static BoltPath ReadPath(byte[] buf, ref int pos)
    {
        // Nodes list
        int nodeCount = ReadListHeader(buf, ref pos);
        var nodes = new BoltNode[nodeCount];
        for (int i = 0; i < nodeCount; i++)
        {
            var (_, fc) = ReadStructHeader(buf, ref pos);
            nodes[i] = ReadNode(buf, ref pos, fc);
        }

        // Unbound relationships list
        int relCount = ReadListHeader(buf, ref pos);
        var unboundRels = new BoltUnboundRelationship[relCount];
        for (int i = 0; i < relCount; i++)
        {
            var (_, fc) = ReadStructHeader(buf, ref pos);
            unboundRels[i] = ReadUnboundRelationship(buf, ref pos, fc);
        }

        // Indices list - alternating rel_index, node_index
        int indexCount = ReadListHeader(buf, ref pos);
        var indices = new long[indexCount];
        for (int i = 0; i < indexCount; i++)
            indices[i] = ReadInt64(buf, ref pos);

        // Resolve into bound relationships
        int pathRelCount = indexCount / 2;
        var relationships = new BoltRelationship[pathRelCount];
        int prevNodeIdx = 0;

        for (int i = 0; i < pathRelCount; i++)
        {
            long relIdx = indices[i * 2];
            int nextNodeIdx = (int)indices[i * 2 + 1];

            var urel = unboundRels[(int)Math.Abs(relIdx) - 1];

            long startId, endId;
            string? startElemId, endElemId;

            if (relIdx > 0)
            {
                // Forward: current node -> next node
                startId = nodes[prevNodeIdx].Id;
                endId = nodes[nextNodeIdx].Id;
                startElemId = nodes[prevNodeIdx].ElementId;
                endElemId = nodes[nextNodeIdx].ElementId;
            }
            else
            {
                // Backward: next node -> current node
                startId = nodes[nextNodeIdx].Id;
                endId = nodes[prevNodeIdx].Id;
                startElemId = nodes[nextNodeIdx].ElementId;
                endElemId = nodes[prevNodeIdx].ElementId;
            }

            relationships[i] = new BoltRelationship(
                urel.Id, startId, endId, urel.Type, urel.Properties,
                urel.ElementId, startElemId, endElemId);

            prevNodeIdx = nextNodeIdx;
        }

        return new BoltPath(nodes, relationships);
    }

    // -- Temporal types ------------------------------------------------------

    private static readonly DateOnly UnixEpochDate = new(1970, 1, 1);

    private static DateOnly ReadDate(byte[] buf, ref int pos)
    {
        long epochDays = ReadInt64(buf, ref pos);
        return UnixEpochDate.AddDays((int)epochDays);
    }

    private static TimeOnly ReadLocalTime(byte[] buf, ref int pos)
    {
        long nanoOfDay = ReadInt64(buf, ref pos);
        return new TimeOnly(nanoOfDay / 100);
    }

    private static BoltOffsetTime ReadTime(byte[] buf, ref int pos)
    {
        long nanoOfDay = ReadInt64(buf, ref pos);
        long offsetSeconds = ReadInt64(buf, ref pos);
        return new BoltOffsetTime(
            new TimeOnly(nanoOfDay / 100),
            TimeSpan.FromSeconds(offsetSeconds));
    }

    private static DateTime ReadLocalDateTime(byte[] buf, ref int pos)
    {
        long epochSecond = ReadInt64(buf, ref pos);
        long nano = ReadInt64(buf, ref pos);
        long ticks = DateTimeOffset.UnixEpoch.Ticks + epochSecond * TimeSpan.TicksPerSecond + nano / 100;
        return new DateTime(ticks, DateTimeKind.Unspecified);
    }

    private static DateTimeOffset ReadDateTimeOffset(byte[] buf, ref int pos)
    {
        long epochSecond = ReadInt64(buf, ref pos);
        long nano = ReadInt64(buf, ref pos);
        long offsetSeconds = ReadInt64(buf, ref pos);
        var offset = TimeSpan.FromSeconds(offsetSeconds);
        long utcTicks = DateTimeOffset.UnixEpoch.Ticks + epochSecond * TimeSpan.TicksPerSecond + nano / 100;
        return new DateTimeOffset(utcTicks + offset.Ticks, offset);
    }

    private static BoltZonedDateTime ReadDateTimeZone(byte[] buf, ref int pos)
    {
        long epochSecond = ReadInt64(buf, ref pos);
        long nano = ReadInt64(buf, ref pos);
        string tzId = ReadString(buf, ref pos);
        var utcDt = DateTimeOffset.UnixEpoch.AddSeconds(epochSecond).AddTicks(nano / 100);
        try
        {
            var tz = TimeZoneInfo.FindSystemTimeZoneById(tzId);
            var offset = tz.GetUtcOffset(utcDt);
            return new BoltZonedDateTime(utcDt.ToOffset(offset), tzId);
        }
        catch (TimeZoneNotFoundException)
        {
            // Unknown zone - return as UTC with zone name preserved
            return new BoltZonedDateTime(utcDt, tzId);
        }
    }

    private static BoltDuration ReadDuration(byte[] buf, ref int pos)
    {
        long months = ReadInt64(buf, ref pos);
        long days = ReadInt64(buf, ref pos);
        long seconds = ReadInt64(buf, ref pos);
        long nanoseconds = ReadInt64(buf, ref pos);
        return new BoltDuration(months, days, seconds, (int)nanoseconds);
    }

    // -- Spatial types ------------------------------------------------------

    private static BoltPoint2D ReadPoint2D(byte[] buf, ref int pos)
    {
        long srid = ReadInt64(buf, ref pos);
        double x = ReadFloat64(buf, ref pos);
        double y = ReadFloat64(buf, ref pos);
        return new BoltPoint2D((int)srid, x, y);
    }

    private static BoltPoint3D ReadPoint3D(byte[] buf, ref int pos)
    {
        long srid = ReadInt64(buf, ref pos);
        double x = ReadFloat64(buf, ref pos);
        double y = ReadFloat64(buf, ref pos);
        double z = ReadFloat64(buf, ref pos);
        return new BoltPoint3D((int)srid, x, y, z);
    }

    private static List<object?> ReadGenericStruct(byte[] buf, ref int pos, int fieldCount)
    {
        var fields = new List<object?>(fieldCount);
        for (int i = 0; i < fieldCount; i++)
            fields.Add(ReadValue(buf, ref pos));
        return fields;
    }

    /// <summary>Read a map into a Dictionary&lt;string, object?&gt;.</summary>
    public static Dictionary<string, object?> ReadMap(byte[] buf, ref int pos)
    {
        int count = ReadMapHeader(buf, ref pos);
        var map = new Dictionary<string, object?>(count);
        for (int i = 0; i < count; i++)
        {
            string key = ReadString(buf, ref pos);
            object? value = ReadValue(buf, ref pos);
            map[key] = value;
        }
        return map;
    }

    /// <summary>
    /// Skip past any PackStream value without allocating strings, lists, or maps.
    /// Critical for benchmark performance - walks type markers to advance past values.
    /// </summary>
    public static void SkipValue(byte[] buf, ref int pos)
    {
        byte marker = buf[pos];

        // Tiny int (+): 0x00..0x7F
        if (marker <= 0x7F) { pos++; return; }
        // Tiny int (-): 0xF0..0xFF
        if (marker >= 0xF0) { pos++; return; }

        // Tiny string: 0x80..0x8F
        if (marker >= 0x80 && marker <= 0x8F)
        {
            int len = marker & 0x0F;
            pos += 1 + len;
            return;
        }

        // Tiny list: 0x90..0x9F
        if (marker >= 0x90 && marker <= 0x9F)
        {
            int count = marker & 0x0F;
            pos++;
            for (int i = 0; i < count; i++)
                SkipValue(buf, ref pos);
            return;
        }

        // Tiny map: 0xA0..0xAF
        if (marker >= 0xA0 && marker <= 0xAF)
        {
            int count = marker & 0x0F;
            pos++;
            for (int i = 0; i < count * 2; i++)
                SkipValue(buf, ref pos);
            return;
        }

        // Tiny struct: 0xB0..0xBF
        if (marker >= 0xB0 && marker <= 0xBF)
        {
            int fields = marker & 0x0F;
            pos += 2; // marker + tag
            for (int i = 0; i < fields; i++)
                SkipValue(buf, ref pos);
            return;
        }

        switch (marker)
        {
            case Markers.NULL:
            case Markers.TRUE:
            case Markers.FALSE:
                pos++;
                return;
            case Markers.INT8:
                pos += 2;
                return;
            case Markers.INT16:
                pos += 3;
                return;
            case Markers.INT32:
                pos += 5;
                return;
            case Markers.INT64:
            case Markers.FLOAT64:
                pos += 9;
                return;
            case Markers.STRING8:
                pos++;
                pos += 1 + buf[pos];
                return;
            case Markers.STRING16:
            {
                pos++;
                int len = BinaryPrimitives.ReadUInt16BigEndian(buf.AsSpan(pos));
                pos += 2 + len;
                return;
            }
            case Markers.STRING32:
            {
                pos++;
                int len = BinaryPrimitives.ReadInt32BigEndian(buf.AsSpan(pos));
                pos += 4 + len;
                return;
            }
            case Markers.BYTES8:
                pos++;
                pos += 1 + buf[pos];
                return;
            case Markers.BYTES16:
            {
                pos++;
                int len = BinaryPrimitives.ReadUInt16BigEndian(buf.AsSpan(pos));
                pos += 2 + len;
                return;
            }
            case Markers.BYTES32:
            {
                pos++;
                int len = BinaryPrimitives.ReadInt32BigEndian(buf.AsSpan(pos));
                pos += 4 + len;
                return;
            }
            case Markers.LIST8:
            {
                pos++;
                int count = buf[pos++];
                for (int i = 0; i < count; i++)
                    SkipValue(buf, ref pos);
                return;
            }
            case Markers.LIST16:
            {
                pos++;
                int count = BinaryPrimitives.ReadUInt16BigEndian(buf.AsSpan(pos));
                pos += 2;
                for (int i = 0; i < count; i++)
                    SkipValue(buf, ref pos);
                return;
            }
            case Markers.LIST32:
            {
                pos++;
                int count = BinaryPrimitives.ReadInt32BigEndian(buf.AsSpan(pos));
                pos += 4;
                for (int i = 0; i < count; i++)
                    SkipValue(buf, ref pos);
                return;
            }
            case Markers.MAP8:
            {
                pos++;
                int count = buf[pos++];
                for (int i = 0; i < count * 2; i++)
                    SkipValue(buf, ref pos);
                return;
            }
            case Markers.MAP16:
            {
                pos++;
                int count = BinaryPrimitives.ReadUInt16BigEndian(buf.AsSpan(pos));
                pos += 2;
                for (int i = 0; i < count * 2; i++)
                    SkipValue(buf, ref pos);
                return;
            }
            case Markers.MAP32:
            {
                pos++;
                int count = BinaryPrimitives.ReadInt32BigEndian(buf.AsSpan(pos));
                pos += 4;
                for (int i = 0; i < count * 2; i++)
                    SkipValue(buf, ref pos);
                return;
            }
            default:
                throw new InvalidDataException($"Cannot skip unknown PackStream marker: 0x{marker:X2} at position {pos}");
        }
    }
}
