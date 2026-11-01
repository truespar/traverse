using Traverse.Bolt.Internal;

namespace Traverse.Bolt;

/// <summary>
/// An explicit Bolt transaction. Queries run within this transaction share a single
/// server-side transaction scope. Dispose auto-rolls-back if not committed.
/// </summary>
public sealed class BoltTransaction : IAsyncDisposable
{
    private readonly BoltConnection _conn;
    private readonly Action _onCompleted;
    private int _completed; // 0 = active, 1 = completed

    internal BoltTransaction(BoltConnection conn, Action onCompleted)
    {
        _conn = conn;
        _onCompleted = onCompleted;
    }

    /// <summary>Execute a Cypher query and return the full result (columns + records + summary).</summary>
    public async ValueTask<BoltResult> RunAsync(
        string cypher,
        Dictionary<string, object>? parameters = null,
        QueryOptions? options = null,
        CancellationToken ct = default)
    {
        ThrowIfCompleted();

        try
        {
            // Database is null - already set via BEGIN
            await _conn.SendRunPullAsync(cypher, parameters, null, queryOptions: options, ct: ct);

            var (tag, bodyOffset, _) = await _conn.ReadMessageAsync(ct);

            if (tag == MsgTag.FAILURE)
                throw await _conn.HandleRunFailureAsync(bodyOffset, ct);

            if (tag != MsgTag.SUCCESS)
                throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after RUN, got 0x{tag:X2}");

            var runMeta = _conn.ReadSuccessMetadata(bodyOffset);
            var columns = BoltResult.ExtractColumns(runMeta);

            var (rawRecords, summary) = await _conn.ReadPullResponsesAsync(ct);

            var records = new BoltRecord[rawRecords.Count];
            for (int i = 0; i < rawRecords.Count; i++)
                records[i] = new BoltRecord(rawRecords[i], columns);

            return new BoltResult(columns, records, summary);
        }
        catch
        {
            // Any failure triggers RESET (via HandleRunFailureAsync or ReadPullResponsesAsync),
            // which exits TX state - mark the transaction as dead.
            MarkCompleted();
            throw;
        }
    }

    /// <summary>Execute a Cypher query and drain all result records without materializing them.
    /// Returns summary metadata (stats, bookmark) with empty records.</summary>
    public async ValueTask<BoltResult> RunAndConsumeAsync(
        string cypher,
        Dictionary<string, object>? parameters = null,
        QueryOptions? options = null,
        CancellationToken ct = default)
    {
        ThrowIfCompleted();

        try
        {
            await _conn.SendRunPullAsync(cypher, parameters, null, queryOptions: options, ct: ct);

            var (tag, bodyOffset, _) = await _conn.ReadMessageAsync(ct);

            if (tag == MsgTag.FAILURE)
                throw await _conn.HandleRunFailureAsync(bodyOffset, ct);

            if (tag != MsgTag.SUCCESS)
                throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after RUN, got 0x{tag:X2}");

            var runMeta = _conn.ReadSuccessMetadata(bodyOffset);
            var columns = BoltResult.ExtractColumns(runMeta);

            var summary = await _conn.DrainPullResponsesAsync(ct);

            return new BoltResult(columns, Array.Empty<BoltRecord>(), summary);
        }
        catch
        {
            MarkCompleted();
            throw;
        }
    }

    /// <summary>Execute a Cypher query, discarding all result records. Returns columns and summary but no records.</summary>
    public async ValueTask<BoltResult> RunAndDiscardAsync(
        string cypher,
        Dictionary<string, object>? parameters = null,
        QueryOptions? options = null,
        CancellationToken ct = default)
    {
        ThrowIfCompleted();

        try
        {
            await _conn.SendRunDiscardAsync(cypher, parameters, null, queryOptions: options, ct: ct);

            var (tag, bodyOffset, _) = await _conn.ReadMessageAsync(ct);

            if (tag == MsgTag.FAILURE)
                throw await _conn.HandleRunFailureAsync(bodyOffset, ct);

            if (tag != MsgTag.SUCCESS)
                throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after RUN, got 0x{tag:X2}");

            var runMeta = _conn.ReadSuccessMetadata(bodyOffset);
            var columns = BoltResult.ExtractColumns(runMeta);

            var summary = await _conn.ReadDiscardResponseAsync(ct);

            return new BoltResult(columns, Array.Empty<BoltRecord>(), summary);
        }
        catch
        {
            MarkCompleted();
            throw;
        }
    }

    /// <summary>Execute a Cypher query and return a streaming cursor that lazily pulls batches.</summary>
    public async ValueTask<BoltResultCursor> RunStreamAsync(
        string cypher,
        Dictionary<string, object>? parameters = null,
        QueryOptions? options = null,
        long fetchSize = 1000,
        CancellationToken ct = default)
    {
        ThrowIfCompleted();

        try
        {
            await _conn.SendRunOnlyAsync(cypher, parameters, null, queryOptions: options, ct: ct);

            var (tag, bodyOffset, _) = await _conn.ReadMessageAsync(ct);

            if (tag == MsgTag.FAILURE)
            {
                var metadata = _conn.ReadSuccessMetadata(bodyOffset);
                await _conn.ResetAsync(ct);
                throw BoltException.FromMetadata(metadata, _conn.NegotiatedVersion);
            }

            if (tag != MsgTag.SUCCESS)
                throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after RUN, got 0x{tag:X2}");

            var runMeta = _conn.ReadSuccessMetadata(bodyOffset);
            var columns = BoltResult.ExtractColumns(runMeta);

            await _conn.SendPullAsync(fetchSize, ct: ct);

            return new BoltResultCursor(_conn, columns, fetchSize);
        }
        catch
        {
            MarkCompleted();
            throw;
        }
    }

    /// <summary>Commit the transaction.</summary>
    public async ValueTask CommitAsync(CancellationToken ct = default)
    {
        ThrowIfCompleted();
        MarkCompleted();

        await _conn.SendCommitAsync(ct);

        var (tag, bodyOffset, _) = await _conn.ReadMessageAsync(ct);

        if (tag == MsgTag.FAILURE)
        {
            var metadata = _conn.ReadSuccessMetadata(bodyOffset);
            await _conn.ResetAsync(ct);
            throw BoltException.FromMetadata(metadata, _conn.NegotiatedVersion);
        }

        if (tag != MsgTag.SUCCESS)
            throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after COMMIT, got 0x{tag:X2}");
    }

    /// <summary>Roll back the transaction. Idempotent - safe to call multiple times.</summary>
    public async ValueTask RollbackAsync(CancellationToken ct = default)
    {
        if (Interlocked.CompareExchange(ref _completed, 1, 0) != 0)
            return; // Already completed - idempotent

        _onCompleted();

        await _conn.SendRollbackAsync(ct);

        var (tag, bodyOffset, _) = await _conn.ReadMessageAsync(ct);

        if (tag == MsgTag.FAILURE)
        {
            var metadata = _conn.ReadSuccessMetadata(bodyOffset);
            await _conn.ResetAsync(ct);
            throw BoltException.FromMetadata(metadata, _conn.NegotiatedVersion);
        }

        if (tag != MsgTag.SUCCESS)
            throw new BoltException("Bolt.Protocol", $"Expected SUCCESS after ROLLBACK, got 0x{tag:X2}");
    }

    /// <summary>Auto-rollback if not already committed or rolled back.</summary>
    public async ValueTask DisposeAsync()
    {
        if (Interlocked.CompareExchange(ref _completed, 1, 0) != 0)
            return; // Already committed or rolled back

        _onCompleted();

        try
        {
            await _conn.SendRollbackAsync();

            var (tag, _, _) = await _conn.ReadMessageAsync();

            if (tag == MsgTag.FAILURE)
                await _conn.ResetAsync();
        }
        catch
        {
            // Best-effort rollback during dispose
        }
    }

    private void ThrowIfCompleted()
    {
        if (_completed != 0)
            throw new InvalidOperationException("Transaction has already been committed or rolled back.");
    }

    private void MarkCompleted()
    {
        if (Interlocked.Exchange(ref _completed, 1) == 0)
            _onCompleted();
    }
}
