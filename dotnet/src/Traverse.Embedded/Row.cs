using System.Runtime.InteropServices;
using Traverse.Interop;

namespace Traverse;

/// <summary>
/// Represents a single row in a query result.
/// When obtained via enumeration, the row is only valid for the current iteration.
/// Call <see cref="Snapshot"/> to create a copy that lives beyond the cursor advance.
/// </summary>
public sealed class Row
{
    private readonly IntPtr _resultHandle;
    private readonly ColumnInfo[] _columns;
    private readonly bool _isSnapshot;
    private readonly object?[]? _snapshotValues;
    private readonly string?[]? _snapshotTypeNames;

    /// <summary>Number of columns.</summary>
    public int ColumnCount => _columns.Length;

    /// <summary>
    /// Get the underlying native result handle. For internal use by
    /// <see cref="GraphJsonSerializer"/>.
    /// </summary>
    internal IntPtr GetResultHandle() => _resultHandle;

    internal Row(IntPtr resultHandle, ColumnInfo[] columns)
    {
        _resultHandle = resultHandle;
        _columns = columns;
        _isSnapshot = false;
    }

    private Row(ColumnInfo[] columns, object?[] values, string?[] typeNames)
    {
        _resultHandle = IntPtr.Zero;
        _columns = columns;
        _isSnapshot = true;
        _snapshotValues = values;
        _snapshotTypeNames = typeNames;
    }

    private int ResolveColumn(string name)
    {
        for (int i = 0; i < _columns.Length; i++)
        {
            if (string.Equals(_columns[i].Name, name, StringComparison.Ordinal))
                return i;
        }
        throw new ArgumentException($"Column '{name}' not found.", nameof(name));
    }

    /// <summary>
    /// Returns true if the value at <paramref name="col"/> is NULL.
    /// </summary>
    public bool IsNull(int col)
    {
        if (_isSnapshot)
            return _snapshotValues![col] is null;

        return Interop.NativeLibrary.traverse_result_is_null(_resultHandle, col);
    }

    /// <summary>
    /// Get the type name of the value at <paramref name="col"/>.
    /// </summary>
    public string GetTypeName(int col)
    {
        if (_isSnapshot)
            return _snapshotTypeNames![col] ?? "NULL";

        var ptr = Interop.NativeLibrary.traverse_result_get_type(_resultHandle, col);
        if (ptr == IntPtr.Zero)
            return "NULL";
        return Marshal.PtrToStringUTF8(ptr) ?? "NULL";
    }

    /// <summary>
    /// Get the string value at <paramref name="col"/>.
    /// Returns null if the value is NULL.
    /// </summary>
    public string? GetString(int col)
    {
        if (_isSnapshot)
            return _snapshotValues![col]?.ToString();

        var ptr = Interop.NativeLibrary.traverse_result_get_string(_resultHandle, col);
        if (ptr == IntPtr.Zero)
            return null;
        return Marshal.PtrToStringUTF8(ptr);
    }

    /// <summary>
    /// Get the string value for the column with the given <paramref name="name"/>.
    /// </summary>
    public string? GetString(string name) => GetString(ResolveColumn(name));

    /// <summary>
    /// Get the Int64 value at <paramref name="col"/>.
    /// </summary>
    public long GetInt64(int col)
    {
        if (_isSnapshot)
            return _snapshotValues![col] is long l ? l : throw new InvalidCastException($"Column {col} is not INTEGER");

        var val = Interop.NativeLibrary.traverse_result_get_int(_resultHandle, col);
        Interop.NativeLibrary.CheckError();
        return val;
    }

    /// <summary>
    /// Get the Int64 value for the column with the given <paramref name="name"/>.
    /// </summary>
    public long GetInt64(string name) => GetInt64(ResolveColumn(name));

    /// <summary>
    /// Get the double value at <paramref name="col"/>.
    /// </summary>
    public double GetDouble(int col)
    {
        if (_isSnapshot)
            return _snapshotValues![col] is double d ? d : throw new InvalidCastException($"Column {col} is not FLOAT");

        var val = Interop.NativeLibrary.traverse_result_get_double(_resultHandle, col);
        Interop.NativeLibrary.CheckError();
        return val;
    }

    /// <summary>
    /// Get the double value for the column with the given <paramref name="name"/>.
    /// </summary>
    public double GetDouble(string name) => GetDouble(ResolveColumn(name));

    /// <summary>
    /// Get the boolean value at <paramref name="col"/>.
    /// </summary>
    public bool GetBool(int col)
    {
        if (_isSnapshot)
            return _snapshotValues![col] is bool b ? b : throw new InvalidCastException($"Column {col} is not BOOLEAN");

        var val = Interop.NativeLibrary.traverse_result_get_bool(_resultHandle, col);
        Interop.NativeLibrary.CheckError();
        return val;
    }

    /// <summary>
    /// Get the boolean value for the column with the given <paramref name="name"/>.
    /// </summary>
    public bool GetBool(string name) => GetBool(ResolveColumn(name));

    /// <summary>
    /// Get the value at <paramref name="col"/> as a boxed object.
    /// Returns null for NULL, otherwise returns the typed value.
    /// </summary>
    public object? GetValue(int col)
    {
        if (_isSnapshot)
            return _snapshotValues![col];

        if (IsNull(col)) return null;

        var typeName = GetTypeName(col);
        return typeName switch
        {
            "INTEGER" => Interop.NativeLibrary.traverse_result_get_int(_resultHandle, col),
            "FLOAT" => Interop.NativeLibrary.traverse_result_get_double(_resultHandle, col),
            "BOOLEAN" => Interop.NativeLibrary.traverse_result_get_bool(_resultHandle, col),
            _ => GetString(col), // STRING, NODE, EDGE, PATH, temporal types, etc. as string
        };
    }

    /// <summary>
    /// Create a managed copy of this row that is valid beyond the cursor lifetime.
    /// All values are copied into managed memory.
    /// </summary>
    public Row Snapshot()
    {
        if (_isSnapshot)
            return this; // already a snapshot

        var values = new object?[_columns.Length];
        var typeNames = new string?[_columns.Length];
        for (int i = 0; i < _columns.Length; i++)
        {
            typeNames[i] = GetTypeName(i);
            values[i] = GetValue(i);
        }
        return new Row(_columns, values, typeNames);
    }
}
