using Traverse.V1;

namespace Traverse.Grpc;

public sealed partial class TraverseGrpcClient
{
    /// <summary>
    /// Create an index on a label/property pair.
    /// </summary>
    public async ValueTask CreateIndexAsync(
        string name,
        string label,
        string property,
        string? transactionId = null,
        CancellationToken cancellationToken = default)
    {
        var request = new CreateIndexRequest { Name = name, Label = label, Property = property };
        if (transactionId is not null)
            request.TransactionId = transactionId;

        await CallAsync(() => _client.CreateIndexAsync(request, cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
    }

    /// <summary>
    /// Drop an index by name.
    /// </summary>
    public async ValueTask DropIndexAsync(
        string name,
        string? transactionId = null,
        CancellationToken cancellationToken = default)
    {
        var request = new DropIndexRequest { Name = name };
        if (transactionId is not null)
            request.TransactionId = transactionId;

        await CallAsync(() => _client.DropIndexAsync(request, cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
    }

    /// <summary>
    /// Get all node labels in the database.
    /// </summary>
    public async ValueTask<IReadOnlyList<string>> GetLabelsAsync(CancellationToken cancellationToken = default)
    {
        var response = await CallAsync(() => _client.GetLabelsAsync(new GetLabelsRequest(), cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
        return response.Labels.ToList().AsReadOnly();
    }

    /// <summary>
    /// Get all relationship types in the database.
    /// </summary>
    public async ValueTask<IReadOnlyList<string>> GetRelationshipTypesAsync(CancellationToken cancellationToken = default)
    {
        var response = await CallAsync(() => _client.GetRelationshipTypesAsync(new GetRelationshipTypesRequest(), cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
        return response.Types_.ToList().AsReadOnly();
    }

    /// <summary>
    /// Get all property keys in the database.
    /// </summary>
    public async ValueTask<IReadOnlyList<string>> GetPropertyKeysAsync(CancellationToken cancellationToken = default)
    {
        var response = await CallAsync(() => _client.GetPropertyKeysAsync(new GetPropertyKeysRequest(), cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
        return response.Keys.ToList().AsReadOnly();
    }

    /// <summary>
    /// Get the total node count.
    /// </summary>
    public async ValueTask<ulong> GetNodeCountAsync(CancellationToken cancellationToken = default)
    {
        var response = await CallAsync(() => _client.GetNodeCountAsync(new GetNodeCountRequest(), cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
        return response.Count;
    }

    /// <summary>
    /// Get the total edge count.
    /// </summary>
    public async ValueTask<ulong> GetEdgeCountAsync(CancellationToken cancellationToken = default)
    {
        var response = await CallAsync(() => _client.GetEdgeCountAsync(new GetEdgeCountRequest(), cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
        return response.Count;
    }
}
