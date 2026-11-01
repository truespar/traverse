using System.Collections;
using System.Runtime.InteropServices;
using Traverse.Interop;

namespace Traverse;

/// <summary>
/// Represents the result of a Cypher query.
/// Implements <see cref="IEnumerable{Row}"/> for forward-only row iteration
/// and <see cref="IDisposable"/> to free native memory.
/// </summary>
public sealed class QueryResult : IEnumerable<Row>, IDisposable
{
    private readonly ResultSafeHandle _handle;
    private readonly ColumnInfo[] _columns;
    private bool _disposed;

    internal QueryResult(ResultSafeHandle handle)
    {
        _handle = handle;

        var raw = handle.DangerousGetHandle();
        int colCount = Interop.NativeLibrary.traverse_result_column_count(raw);
        _columns = new ColumnInfo[colCount];
        for (int i = 0; i < colCount; i++)
        {
            var namePtr = Interop.NativeLibrary.traverse_result_column_name(raw, i);
            var name = namePtr != IntPtr.Zero
                ? Marshal.PtrToStringUTF8(namePtr) ?? $"col{i}"
                : $"col{i}";
            _columns[i] = new ColumnInfo(i, name);
        }
    }

    /// <summary>
    /// Column metadata for the result set.
    /// </summary>
    public ReadOnlySpan<ColumnInfo> Columns => _columns;

    /// <summary>
    /// Number of columns in the result set.
    /// </summary>
    public int ColumnCount => _columns.Length;

    /// <summary>
    /// Total number of rows in the result set.
    /// </summary>
    public long RowCount
    {
        get
        {
            ObjectDisposedException.ThrowIf(_disposed, this);
            return Interop.NativeLibrary.traverse_result_row_count(_handle.DangerousGetHandle());
        }
    }

    /// <summary>
    /// Enumerate all rows and return snapshots in a list.
    /// This consumes the cursor but each row is safe to use after enumeration.
    /// </summary>
    public List<Row> ToList()
    {
        var list = new List<Row>();
        foreach (var row in this)
        {
            list.Add(row.Snapshot());
        }
        return list;
    }

    /// <inheritdoc/>
    public IEnumerator<Row> GetEnumerator()
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        return new RowEnumerator(_handle, _columns);
    }

    IEnumerator IEnumerable.GetEnumerator() => GetEnumerator();

    /// <inheritdoc/>
    public void Dispose()
    {
        if (_disposed) return;
        _disposed = true;
        _handle.Dispose();
    }

    private sealed class RowEnumerator : IEnumerator<Row>
    {
        private readonly ResultSafeHandle _handle;
        private readonly ColumnInfo[] _columns;
        private Row? _current;
        private bool _refAdded;

        public RowEnumerator(ResultSafeHandle handle, ColumnInfo[] columns)
        {
            _handle = handle;
            _columns = columns;
            _handle.DangerousAddRef(ref _refAdded);
        }

        public Row Current => _current ?? throw new InvalidOperationException("No current row.");

        object IEnumerator.Current => Current;

        public bool MoveNext()
        {
            ObjectDisposedException.ThrowIf(!_refAdded, this);

            var raw = _handle.DangerousGetHandle();
            if (!Interop.NativeLibrary.traverse_result_next(raw))
            {
                _current = null;
                return false;
            }
            _current = new Row(raw, _columns);
            return true;
        }

        public void Reset() => throw new NotSupportedException("Forward-only cursor.");

        public void Dispose()
        {
            if (_refAdded)
            {
                _refAdded = false;
                _handle.DangerousRelease();
            }
        }
    }
}
