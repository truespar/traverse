using Traverse.Grpc.Values;
using Traverse.V1;
using Grpc.Core;

namespace Traverse.Grpc;

public sealed partial class TraverseGrpcClient
{
    /// <summary>
    /// Execute a Cypher query and return a streaming result.
    /// </summary>
    public ValueTask<QueryResult> QueryAsync(
        string query,
        IReadOnlyDictionary<string, TvValue>? parameters = null,
        QueryOptions? options = null,
        CancellationToken cancellationToken = default)
    {
        var request = new ExecuteQueryRequest
        {
            Query = query,
            Database = options?.Database ?? _options.Database,
        };

        if (parameters is not null)
        {
            foreach (var (k, v) in parameters)
                request.Parameters.Add(k, ValueConverter.ToProto(v));
        }

        if (options is not null)
        {
            if (options.TransactionId is not null)
                request.TransactionId = options.TransactionId;
            if (options.Bookmarks is not null)
                request.Bookmarks.AddRange(options.Bookmarks);
            if (options.AccessMode is not null)
                request.AccessMode = options.AccessMode;
            if (options.Timeout is { } timeout)
                request.TimeoutMs = (long)timeout.TotalMilliseconds;
            if (options.NotificationMinimumSeverity is not null)
                request.NotificationMinimumSeverity = options.NotificationMinimumSeverity;
            if (options.NotificationDisabledCategories is not null)
                request.NotificationDisabledCategories.AddRange(options.NotificationDisabledCategories);
        }

        try
        {
            var call = _client.ExecuteQuery(request, cancellationToken: cancellationToken);
            return ValueTask.FromResult(new QueryResult(call, _pool));
        }
        catch (RpcException ex)
        {
            throw TraverseException.FromRpcException(ex);
        }
    }
}
