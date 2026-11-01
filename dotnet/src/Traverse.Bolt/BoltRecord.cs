namespace Traverse.Bolt;

/// <summary>A single row in a query result. Supports indexed and column-name access.</summary>
public sealed class BoltRecord
{
    private readonly object?[] _values;
    private readonly string[] _columns;

    internal BoltRecord(object?[] values, string[] columns)
    {
        _values = values;
        _columns = columns;
    }

    public int Count => _values.Length;

    public object? this[int index] => _values[index];

    public object? this[string column]
    {
        get
        {
            // Linear scan - faster than dict for typical 1-10 column result sets
            for (int i = 0; i < _columns.Length; i++)
            {
                if (_columns[i] == column)
                    return _values[i];
            }
            throw new KeyNotFoundException($"Column '{column}' not found");
        }
    }

    public long GetInt64(int index) => (long)_values[index]!;
    public string GetString(int index) => (string)_values[index]!;
    public double GetDouble(int index) => (double)_values[index]!;
    public bool GetBool(int index) => (bool)_values[index]!;
    public byte[] GetBytes(int index) => (byte[])_values[index]!;
    public bool IsNull(int index) => _values[index] is null;
    public BoltNode GetNode(int index) => (BoltNode)_values[index]!;
    public BoltRelationship GetRelationship(int index) => (BoltRelationship)_values[index]!;
    public BoltPath GetPath(int index) => (BoltPath)_values[index]!;
    public DateOnly GetDate(int index) => (DateOnly)_values[index]!;
    public TimeOnly GetTimeOnly(int index) => (TimeOnly)_values[index]!;
    public BoltOffsetTime GetOffsetTime(int index) => (BoltOffsetTime)_values[index]!;
    public DateTime GetDateTime(int index) => (DateTime)_values[index]!;
    public DateTimeOffset GetDateTimeOffset(int index) => (DateTimeOffset)_values[index]!;
    public BoltZonedDateTime GetZonedDateTime(int index) => (BoltZonedDateTime)_values[index]!;
    public BoltDuration GetDuration(int index) => (BoltDuration)_values[index]!;
    public BoltPoint2D GetPoint2D(int index) => (BoltPoint2D)_values[index]!;
    public BoltPoint3D GetPoint3D(int index) => (BoltPoint3D)_values[index]!;
}
