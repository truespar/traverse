using System.Runtime.CompilerServices;
using Traverse.Grpc.Internal;
using Traverse.Grpc.Values;
using Traverse.V1;
using Grpc.Core;

namespace Traverse.Grpc;

/// <summary>
/// Streaming query result. Implements <see cref="IAsyncEnumerable{QueryRow}"/> for row iteration.
/// The result must be disposed after use.
/// </summary>
public sealed class QueryResult : IAsyncEnumerable<QueryRow>, IAsyncDisposable
{
    private readonly AsyncServerStreamingCall<ExecuteQueryResponse> _call;
    private readonly RowBufferPool _pool;
    private string[]? _columns;
    private QuerySummary? _summary;
    private int _enumerated;
    private TvValue[]? _currentBuffer;

    internal QueryResult(AsyncServerStreamingCall<ExecuteQueryResponse> call, RowBufferPool pool)
    {
        _call = call;
        _pool = pool;
    }

    /// <summary>
    /// Column names from the query header. Available after first MoveNext or after ConsumeAsync/ToListAsync.
    /// </summary>
    public IReadOnlyList<string>? Columns => _columns;

    /// <summary>
    /// Query summary from the footer. Available after enumeration completes.
    /// </summary>
    public QuerySummary? Summary => _summary;

    /// <summary>
    /// Consume all rows without materializing them and return the summary.
    /// </summary>
    public async ValueTask<QuerySummary> ConsumeAsync(CancellationToken cancellationToken = default)
    {
        if (Interlocked.Exchange(ref _enumerated, 1) != 0)
            throw new InvalidOperationException("QueryResult can only be enumerated once.");

        try
        {
            await foreach (var _ in ReadStream(cancellationToken).ConfigureAwait(false))
            {
                // discard rows
            }
        }
        catch (RpcException ex)
        {
            throw TraverseException.FromRpcException(ex);
        }
        return _summary!;
    }

    /// <summary>
    /// Materialize all rows into a list. Each row is snapshotted.
    /// </summary>
    public async ValueTask<(List<QueryRow> Rows, QuerySummary Summary)> ToListAsync(CancellationToken cancellationToken = default)
    {
        if (Interlocked.Exchange(ref _enumerated, 1) != 0)
            throw new InvalidOperationException("QueryResult can only be enumerated once.");

        try
        {
            var rows = new List<QueryRow>();
            await foreach (var row in ReadStream(cancellationToken).ConfigureAwait(false))
            {
                rows.Add(row.Snapshot());
            }
            return (rows, _summary!);
        }
        catch (RpcException ex)
        {
            throw TraverseException.FromRpcException(ex);
        }
    }

    public async IAsyncEnumerator<QueryRow> GetAsyncEnumerator(CancellationToken cancellationToken = default)
    {
        if (Interlocked.Exchange(ref _enumerated, 1) != 0)
            throw new InvalidOperationException("QueryResult can only be enumerated once.");

        IAsyncEnumerator<QueryRow> enumerator = ReadStream(cancellationToken).GetAsyncEnumerator(cancellationToken);
        try
        {
            while (true)
            {
                QueryRow current;
                try
                {
                    if (!await enumerator.MoveNextAsync().ConfigureAwait(false))
                        break;
                    current = enumerator.Current;
                }
                catch (RpcException ex)
                {
                    throw TraverseException.FromRpcException(ex);
                }
                yield return current;
            }
        }
        finally
        {
            await enumerator.DisposeAsync().ConfigureAwait(false);
        }
    }

    private async IAsyncEnumerable<QueryRow> ReadStream([EnumeratorCancellation] CancellationToken cancellationToken)
    {
        var stream = _call.ResponseStream;
        int columnCount = 0;

        while (await stream.MoveNext(cancellationToken).ConfigureAwait(false))
        {
            var response = stream.Current;
            switch (response.PayloadCase)
            {
                case ExecuteQueryResponse.PayloadOneofCase.Header:
                    _columns = response.Header.Columns.ToArray();
                    columnCount = _columns.Length;
                    break;

                case ExecuteQueryResponse.PayloadOneofCase.Row:
                    // Return previous buffer
                    if (_currentBuffer is not null)
                    {
                        _pool.Return(_currentBuffer);
                    }

                    var buffer = _pool.Rent(columnCount);
                    var protoValues = response.Row.Values;
                    for (int i = 0; i < columnCount && i < protoValues.Count; i++)
                    {
                        buffer[i] = ValueConverter.FromProto(protoValues[i]);
                    }
                    _currentBuffer = buffer;
                    yield return new QueryRow(_columns!, buffer, columnCount, isSnapshot: false);
                    break;

                case ExecuteQueryResponse.PayloadOneofCase.Footer:
                    _summary = QuerySummary.FromFooter(response.Footer);
                    break;
            }
        }

        // Return the last buffer
        if (_currentBuffer is not null)
        {
            _pool.Return(_currentBuffer);
            _currentBuffer = null;
        }
    }

    public ValueTask DisposeAsync()
    {
        if (_currentBuffer is not null)
        {
            _pool.Return(_currentBuffer);
            _currentBuffer = null;
        }
        _call.Dispose();
        return ValueTask.CompletedTask;
    }
}
