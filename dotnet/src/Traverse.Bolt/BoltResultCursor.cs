using Traverse.Bolt.Internal;

namespace Traverse.Bolt;

/// <summary>
/// Streaming query result cursor. Lazily pulls batches of records from the server
/// as an <see cref="IAsyncEnumerable{BoltRecord}"/>. Use for large result sets
/// where materializing all records would be memory-prohibitive.
/// </summary>
public sealed class BoltResultCursor : IAsyncEnumerable<BoltRecord>, IAsyncDisposable
{
    private readonly BoltConnection _conn;
    private readonly long _fetchSize;
    private bool _exhausted;
    private bool _disposed;

    /// <summary>Column names from the RUN SUCCESS "fields" metadata.</summary>
    public string[] Columns { get; }

    /// <summary>Summary metadata - available only after the cursor is fully consumed.</summary>
    public Dictionary<string, object?> Summary { get; private set; } = new();

    internal BoltResultCursor(BoltConnection conn, string[] columns, long fetchSize)
    {
        _conn = conn;
        Columns = columns;
        _fetchSize = fetchSize;
    }

    public async IAsyncEnumerator<BoltRecord> GetAsyncEnumerator(
        CancellationToken cancellationToken = default)
    {
        while (!_exhausted)
        {
            var (records, hasMore, summary) = await _conn.ReadPullBatchAsync(cancellationToken);

            foreach (var values in records)
                yield return new BoltRecord(values, Columns);

            if (!hasMore)
            {
                _exhausted = true;
                Summary = summary;
            }
            else
            {
                await _conn.SendPullAsync(_fetchSize, ct: cancellationToken);
            }
        }
    }

    /// <summary>Drain remaining records without materializing. Returns summary metadata.</summary>
    public async ValueTask<Dictionary<string, object?>> ConsumeAsync(CancellationToken ct = default)
    {
        if (_exhausted) return Summary;

        while (true)
        {
            var (_, hasMore, summary) = await _conn.ReadPullBatchAsync(ct);
            if (!hasMore)
            {
                _exhausted = true;
                Summary = summary;
                return summary;
            }
            await _conn.SendPullAsync(_fetchSize, ct: ct);
        }
    }

    public ValueTask DisposeAsync()
    {
        if (_disposed) return ValueTask.CompletedTask;
        _disposed = true;
        // If not exhausted, the connection is in a dirty state - caller should RESET
        return ValueTask.CompletedTask;
    }
}
