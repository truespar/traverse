using Traverse.Grpc.Values;
using Traverse.V1;
using Grpc.Core;

namespace Traverse.Grpc;

/// <summary>
/// An explicit transaction handle. Auto-rolls back on DisposeAsync if not committed.
/// </summary>
public sealed class TraverseTransaction : IAsyncDisposable
{
    private readonly TraverseService.TraverseServiceClient _client;
    private readonly TraverseGrpcClient _owner;
    private int _completed; // 0 = active, 1 = completed

    internal TraverseTransaction(
        string transactionId,
        TraverseService.TraverseServiceClient client,
        TraverseGrpcClient owner)
    {
        TransactionId = transactionId;
        _client = client;
        _owner = owner;
    }

    /// <summary>
    /// The server-assigned transaction ID.
    /// </summary>
    public string TransactionId { get; }

    /// <summary>
    /// Execute a query within this transaction.
    /// </summary>
    public ValueTask<QueryResult> QueryAsync(
        string query,
        IReadOnlyDictionary<string, TvValue>? parameters = null,
        CancellationToken cancellationToken = default)
    {
        ThrowIfCompleted();
        return _owner.QueryAsync(query, parameters, new QueryOptions { TransactionId = TransactionId }, cancellationToken);
    }

    /// <summary>
    /// Commit this transaction.
    /// </summary>
    public async ValueTask<string?> CommitAsync(CancellationToken cancellationToken = default)
    {
        ThrowIfCompleted();
        Interlocked.Exchange(ref _completed, 1);

        try
        {
            var response = await _client.CommitTransactionAsync(
                new CommitTransactionRequest { TransactionId = TransactionId },
                cancellationToken: cancellationToken).ResponseAsync.ConfigureAwait(false);
            return string.IsNullOrEmpty(response.Bookmark) ? null : response.Bookmark;
        }
        catch (RpcException ex)
        {
            throw TraverseException.FromRpcException(ex);
        }
    }

    /// <summary>
    /// Rollback this transaction explicitly.
    /// </summary>
    public async ValueTask RollbackAsync(CancellationToken cancellationToken = default)
    {
        if (Interlocked.Exchange(ref _completed, 1) != 0)
            return; // already completed

        try
        {
            await _client.RollbackTransactionAsync(
                new RollbackTransactionRequest { TransactionId = TransactionId },
                cancellationToken: cancellationToken).ResponseAsync.ConfigureAwait(false);
        }
        catch (RpcException ex)
        {
            throw TraverseException.FromRpcException(ex);
        }
    }

    public async ValueTask DisposeAsync()
    {
        if (Interlocked.Exchange(ref _completed, 1) != 0)
            return;

        try
        {
            await _client.RollbackTransactionAsync(
                new RollbackTransactionRequest { TransactionId = TransactionId })
                .ResponseAsync.ConfigureAwait(false);
        }
        catch
        {
            // Best-effort rollback on dispose - swallow errors
        }
    }

    private void ThrowIfCompleted()
    {
        if (_completed != 0)
            throw new InvalidOperationException("Transaction has already been committed or rolled back.");
    }
}
