using System.Net.Sockets;

namespace Traverse.Bolt;

/// <summary>
/// Wrapper around a pooled BoltClient. Disposing returns the connection to the pool
/// rather than closing it. Transport errors mark the connection as broken for discard.
/// </summary>
public sealed class PooledBoltClient : IAsyncDisposable
{
    private readonly BoltClient _inner;
    private readonly BoltPool _pool;
    private bool _broken;
    private bool _returned;

    internal PooledBoltClient(BoltClient inner, BoltPool pool)
    {
        _inner = inner;
        _pool = pool;
    }

    /// <summary>Execute a Cypher query and return the full result.</summary>
    public async ValueTask<BoltResult> RunAsync(
        string cypher,
        Dictionary<string, object>? parameters = null,
        IReadOnlyList<string>? bookmarks = null,
        QueryOptions? options = null,
        CancellationToken ct = default)
    {
        try
        {
            return await _inner.RunAsync(cypher, parameters, bookmarks, options, ct);
        }
        catch (Exception ex) when (IsTransportError(ex))
        {
            _broken = true;
            throw;
        }
    }

    /// <summary>Execute a Cypher query and drain all result records.</summary>
    public async ValueTask<BoltResult> RunAndConsumeAsync(
        string cypher,
        Dictionary<string, object>? parameters = null,
        IReadOnlyList<string>? bookmarks = null,
        QueryOptions? options = null,
        CancellationToken ct = default)
    {
        try
        {
            return await _inner.RunAndConsumeAsync(cypher, parameters, bookmarks, options, ct);
        }
        catch (Exception ex) when (IsTransportError(ex))
        {
            _broken = true;
            throw;
        }
    }

    /// <summary>Execute a Cypher query, discarding result records.</summary>
    public async ValueTask<BoltResult> RunAndDiscardAsync(
        string cypher,
        Dictionary<string, object>? parameters = null,
        IReadOnlyList<string>? bookmarks = null,
        QueryOptions? options = null,
        CancellationToken ct = default)
    {
        try
        {
            return await _inner.RunAndDiscardAsync(cypher, parameters, bookmarks, options, ct);
        }
        catch (Exception ex) when (IsTransportError(ex))
        {
            _broken = true;
            throw;
        }
    }

    /// <summary>Execute a Cypher query and return a streaming cursor.</summary>
    public async ValueTask<BoltResultCursor> RunStreamAsync(
        string cypher,
        Dictionary<string, object>? parameters = null,
        IReadOnlyList<string>? bookmarks = null,
        QueryOptions? options = null,
        CancellationToken ct = default)
    {
        try
        {
            return await _inner.RunStreamAsync(cypher, parameters, bookmarks, options, ct);
        }
        catch (Exception ex) when (IsTransportError(ex))
        {
            _broken = true;
            throw;
        }
    }

    /// <summary>Begin an explicit transaction.</summary>
    public async ValueTask<BoltTransaction> BeginTransactionAsync(
        IReadOnlyList<string>? bookmarks = null,
        QueryOptions? options = null,
        CancellationToken ct = default)
    {
        try
        {
            return await _inner.BeginTransactionAsync(bookmarks, options, ct);
        }
        catch (Exception ex) when (IsTransportError(ex))
        {
            _broken = true;
            throw;
        }
    }

    /// <summary>Returns the connection to the pool (or discards if broken).</summary>
    public ValueTask DisposeAsync()
    {
        if (_returned) return ValueTask.CompletedTask;
        _returned = true;
        _pool.Return(_inner, _broken);
        return ValueTask.CompletedTask;
    }

    /// <summary>
    /// Transport errors indicate a dead connection that cannot be reused.
    /// BoltException (server errors like syntax, constraints) are recoverable
    /// after the internal RESET that already happened.
    /// </summary>
    private static bool IsTransportError(Exception ex) =>
        ex is SocketException or EndOfStreamException or IOException;
}
