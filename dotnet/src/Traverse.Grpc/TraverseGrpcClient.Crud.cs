using Traverse.Grpc.Values;
using Traverse.V1;

namespace Traverse.Grpc;

public sealed partial class TraverseGrpcClient
{
    /// <summary>
    /// Create a node with the given labels and properties.
    /// </summary>
    public async ValueTask<TvNode> CreateNodeAsync(
        IEnumerable<string> labels,
        IReadOnlyDictionary<string, TvValue>? properties = null,
        string? transactionId = null,
        CancellationToken cancellationToken = default)
    {
        var request = new CreateNodeRequest();
        request.Labels.AddRange(labels);
        if (properties is not null)
        {
            foreach (var (k, v) in properties)
                request.Properties.Add(k, ValueConverter.ToProto(v));
        }
        if (transactionId is not null)
            request.TransactionId = transactionId;

        var response = await CallAsync(() => _client.CreateNodeAsync(request, cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
        return ValueConverter.ConvertNode(response.Node);
    }

    /// <summary>
    /// Get a node by its ID.
    /// </summary>
    public async ValueTask<TvNode> GetNodeAsync(
        ulong id,
        string? transactionId = null,
        CancellationToken cancellationToken = default)
    {
        var request = new GetNodeRequest { Id = id };
        if (transactionId is not null)
            request.TransactionId = transactionId;

        var response = await CallAsync(() => _client.GetNodeAsync(request, cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
        return ValueConverter.ConvertNode(response.Node);
    }

    /// <summary>
    /// Delete a node by its ID.
    /// </summary>
    public async ValueTask DeleteNodeAsync(
        ulong id,
        bool detach = false,
        string? transactionId = null,
        CancellationToken cancellationToken = default)
    {
        var request = new DeleteNodeRequest { Id = id, Detach = detach };
        if (transactionId is not null)
            request.TransactionId = transactionId;

        await CallAsync(() => _client.DeleteNodeAsync(request, cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
    }

    /// <summary>
    /// Create an edge between two nodes.
    /// </summary>
    public async ValueTask<TvEdge> CreateEdgeAsync(
        ulong source,
        ulong target,
        string edgeType,
        IReadOnlyDictionary<string, TvValue>? properties = null,
        string? transactionId = null,
        CancellationToken cancellationToken = default)
    {
        var request = new CreateEdgeRequest
        {
            Source = source,
            Target = target,
            EdgeType = edgeType,
        };
        if (properties is not null)
        {
            foreach (var (k, v) in properties)
                request.Properties.Add(k, ValueConverter.ToProto(v));
        }
        if (transactionId is not null)
            request.TransactionId = transactionId;

        var response = await CallAsync(() => _client.CreateEdgeAsync(request, cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
        return ValueConverter.ConvertEdge(response.Edge);
    }

    /// <summary>
    /// Get an edge by its ID.
    /// </summary>
    public async ValueTask<TvEdge> GetEdgeAsync(
        ulong id,
        string? transactionId = null,
        CancellationToken cancellationToken = default)
    {
        var request = new GetEdgeRequest { Id = id };
        if (transactionId is not null)
            request.TransactionId = transactionId;

        var response = await CallAsync(() => _client.GetEdgeAsync(request, cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
        return ValueConverter.ConvertEdge(response.Edge);
    }

    /// <summary>
    /// Delete an edge by its ID.
    /// </summary>
    public async ValueTask DeleteEdgeAsync(
        ulong id,
        string? transactionId = null,
        CancellationToken cancellationToken = default)
    {
        var request = new DeleteEdgeRequest { Id = id };
        if (transactionId is not null)
            request.TransactionId = transactionId;

        await CallAsync(() => _client.DeleteEdgeAsync(request, cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
    }

    /// <summary>
    /// Update properties on a node.
    /// </summary>
    public async ValueTask UpdateNodePropertiesAsync(
        ulong nodeId,
        IReadOnlyDictionary<string, TvValue> properties,
        string? transactionId = null,
        CancellationToken cancellationToken = default)
    {
        var request = new UpdatePropertiesRequest { NodeId = nodeId };
        foreach (var (k, v) in properties)
            request.Properties.Add(k, ValueConverter.ToProto(v));
        if (transactionId is not null)
            request.TransactionId = transactionId;

        await CallAsync(() => _client.UpdatePropertiesAsync(request, cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
    }

    /// <summary>
    /// Update properties on an edge.
    /// </summary>
    public async ValueTask UpdateEdgePropertiesAsync(
        ulong edgeId,
        IReadOnlyDictionary<string, TvValue> properties,
        string? transactionId = null,
        CancellationToken cancellationToken = default)
    {
        var request = new UpdatePropertiesRequest { EdgeId = edgeId };
        foreach (var (k, v) in properties)
            request.Properties.Add(k, ValueConverter.ToProto(v));
        if (transactionId is not null)
            request.TransactionId = transactionId;

        await CallAsync(() => _client.UpdatePropertiesAsync(request, cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
    }
}
