using Traverse.Grpc.Values;

namespace Traverse.Grpc;

/// <summary>
/// A single row in a streaming query result.
/// During enumeration, the row's buffer may be recycled - call <see cref="Snapshot"/>
/// to create a copy that lives beyond the current iteration.
/// </summary>
public sealed class QueryRow
{
    private readonly string[] _columns;
    private readonly TvValue[] _values;
    private readonly int _columnCount;
    private readonly bool _isSnapshot;

    internal QueryRow(string[] columns, TvValue[] values, int columnCount, bool isSnapshot)
    {
        _columns = columns;
        _values = values;
        _columnCount = columnCount;
        _isSnapshot = isSnapshot;
    }

    public int ColumnCount => _columnCount;
    public IReadOnlyList<string> Columns => _columns;

    private int ResolveColumn(string name)
    {
        for (int i = 0; i < _columnCount; i++)
        {
            if (string.Equals(_columns[i], name, StringComparison.Ordinal))
                return i;
        }
        throw new ArgumentException($"Column '{name}' not found.", nameof(name));
    }

    public TvValue this[int col] => _values[col];
    public TvValue this[string name] => _values[ResolveColumn(name)];

    public bool IsNull(int col) => _values[col].IsNull;
    public bool IsNull(string name) => _values[ResolveColumn(name)].IsNull;

    public bool GetBool(int col) => _values[col].AsBool();
    public bool GetBool(string name) => _values[ResolveColumn(name)].AsBool();

    public long GetInt64(int col) => _values[col].AsInt64();
    public long GetInt64(string name) => _values[ResolveColumn(name)].AsInt64();

    public double GetDouble(int col) => _values[col].AsDouble();
    public double GetDouble(string name) => _values[ResolveColumn(name)].AsDouble();

    public string GetString(int col) => _values[col].AsString();
    public string GetString(string name) => _values[ResolveColumn(name)].AsString();

    public TvNode GetNode(int col) => _values[col].AsNode();
    public TvNode GetNode(string name) => _values[ResolveColumn(name)].AsNode();

    public TvEdge GetEdge(int col) => _values[col].AsEdge();
    public TvEdge GetEdge(string name) => _values[ResolveColumn(name)].AsEdge();

    public TvPath GetPath(int col) => _values[col].AsPath();
    public TvPath GetPath(string name) => _values[ResolveColumn(name)].AsPath();

    /// <summary>
    /// Creates a copy of this row that is valid beyond the streaming cursor lifetime.
    /// </summary>
    public QueryRow Snapshot()
    {
        if (_isSnapshot)
            return this;
        var copy = new TvValue[_columnCount];
        Array.Copy(_values, copy, _columnCount);
        return new QueryRow(_columns, copy, _columnCount, isSnapshot: true);
    }
}
