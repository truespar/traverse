using System.Diagnostics.CodeAnalysis;
using System.Globalization;
using System.Runtime.CompilerServices;

namespace Traverse.Grpc.Values;

[SuppressMessage("Naming", "CA1720", Justification = "named after the wire types")]
public enum TvValueKind : byte
{
    Null,
    Bool,
    Int64,
    Double,
    String,
    Bytes,
    List,
    Map,
    Node,
    Edge,
    Path,
    Temporal,
}

/// <summary>
/// A discriminated union value from a Traverse query result.
/// Primitives (bool, int64, double) are stored inline with zero boxing.
/// </summary>
public readonly record struct TvValue
{
    private readonly long _intStorage;
    private readonly object? _refStorage;

    public TvValueKind Kind { get; }

    private TvValue(TvValueKind kind, long intStorage = 0, object? refStorage = null)
    {
        Kind = kind;
        _intStorage = intStorage;
        _refStorage = refStorage;
    }

    public static readonly TvValue Null = new(TvValueKind.Null);

    public bool IsNull => Kind == TvValueKind.Null;

    // -- Implicit conversions ------------------------------------------

    public static implicit operator TvValue(bool value) => new(TvValueKind.Bool, value ? 1L : 0L);
    public static implicit operator TvValue(long value) => new(TvValueKind.Int64, value);
    public static implicit operator TvValue(int value) => new(TvValueKind.Int64, value);
    public static implicit operator TvValue(double value) => new(TvValueKind.Double, BitConverter.DoubleToInt64Bits(value));
    public static implicit operator TvValue(string value) => new(TvValueKind.String, refStorage: value);

    // -- Factory methods -----------------------------------------------

    public static TvValue FromBool(bool value) => value;
    public static TvValue FromInt64(long value) => value;
    public static TvValue FromDouble(double value) => value;
    public static TvValue FromString(string value) => value;
    public static TvValue FromBytes(byte[] value) => new(TvValueKind.Bytes, refStorage: value);
    public static TvValue FromList(IReadOnlyList<TvValue> value) => new(TvValueKind.List, refStorage: value);
    public static TvValue FromMap(IReadOnlyDictionary<string, TvValue> value) => new(TvValueKind.Map, refStorage: value);
    public static TvValue FromNode(TvNode value) => new(TvValueKind.Node, refStorage: value);
    public static TvValue FromEdge(TvEdge value) => new(TvValueKind.Edge, refStorage: value);
    public static TvValue FromPath(TvPath value) => new(TvValueKind.Path, refStorage: value);
    public static TvValue FromTemporal(string value) => new(TvValueKind.Temporal, refStorage: value);

    // -- Typed accessors -----------------------------------------------

    public bool AsBool() => Kind == TvValueKind.Bool
        ? _intStorage != 0
        : throw new InvalidCastException($"Cannot convert {Kind} to Bool");

    public long AsInt64() => Kind == TvValueKind.Int64
        ? _intStorage
        : throw new InvalidCastException($"Cannot convert {Kind} to Int64");

    public double AsDouble() => Kind == TvValueKind.Double
        ? BitConverter.Int64BitsToDouble(_intStorage)
        : throw new InvalidCastException($"Cannot convert {Kind} to Double");

    public string AsString() => Kind == TvValueKind.String
        ? (string)_refStorage!
        : throw new InvalidCastException($"Cannot convert {Kind} to String");

    public byte[] AsBytes() => Kind == TvValueKind.Bytes
        ? (byte[])_refStorage!
        : throw new InvalidCastException($"Cannot convert {Kind} to Bytes");

    public IReadOnlyList<TvValue> AsList() => Kind == TvValueKind.List
        ? (IReadOnlyList<TvValue>)_refStorage!
        : throw new InvalidCastException($"Cannot convert {Kind} to List");

    public IReadOnlyDictionary<string, TvValue> AsMap() => Kind == TvValueKind.Map
        ? (IReadOnlyDictionary<string, TvValue>)_refStorage!
        : throw new InvalidCastException($"Cannot convert {Kind} to Map");

    public TvNode AsNode() => Kind == TvValueKind.Node
        ? (TvNode)_refStorage!
        : throw new InvalidCastException($"Cannot convert {Kind} to Node");

    public TvEdge AsEdge() => Kind == TvValueKind.Edge
        ? (TvEdge)_refStorage!
        : throw new InvalidCastException($"Cannot convert {Kind} to Edge");

    public TvPath AsPath() => Kind == TvValueKind.Path
        ? (TvPath)_refStorage!
        : throw new InvalidCastException($"Cannot convert {Kind} to Path");

    public string AsTemporal() => Kind == TvValueKind.Temporal
        ? (string)_refStorage!
        : throw new InvalidCastException($"Cannot convert {Kind} to Temporal");

    // -- Object representation -----------------------------------------

    public object? AsObject() => Kind switch
    {
        TvValueKind.Null => null,
        TvValueKind.Bool => _intStorage != 0,
        TvValueKind.Int64 => _intStorage,
        TvValueKind.Double => BitConverter.Int64BitsToDouble(_intStorage),
        _ => _refStorage,
    };

    public override string ToString() => Kind switch
    {
        TvValueKind.Null => "null",
        TvValueKind.Bool => AsBool() ? "true" : "false",
        TvValueKind.Int64 => _intStorage.ToString(CultureInfo.InvariantCulture),
        TvValueKind.Double => AsDouble().ToString(CultureInfo.InvariantCulture),
        TvValueKind.String => AsString(),
        TvValueKind.Temporal => AsTemporal(),
        _ => $"{Kind}({_refStorage})",
    };
}
