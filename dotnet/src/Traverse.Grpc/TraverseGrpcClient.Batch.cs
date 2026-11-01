using Traverse.Grpc.Values;
using Traverse.V1;

namespace Traverse.Grpc;

public sealed partial class TraverseGrpcClient
{
    /// <summary>
    /// Batch-create multiple nodes in a single RPC call.
    /// Returns the IDs of the created nodes.
    /// </summary>
    public async ValueTask<IReadOnlyList<ulong>> BatchCreateNodesAsync(
        IEnumerable<(IEnumerable<string> Labels, IReadOnlyDictionary<string, TvValue>? Properties)> nodes,
        string? transactionId = null,
        CancellationToken cancellationToken = default)
    {
        var request = new BatchCreateNodesRequest();
        if (transactionId is not null)
            request.TransactionId = transactionId;

        foreach (var (labels, props) in nodes)
        {
            var nodeReq = new CreateNodeRequest();
            nodeReq.Labels.AddRange(labels);
            if (props is not null)
            {
                foreach (var (k, v) in props)
                    nodeReq.Properties.Add(k, ValueConverter.ToProto(v));
            }
            request.Nodes.Add(nodeReq);
        }

        var response = await CallAsync(() => _client.BatchCreateNodesAsync(request, cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
        return response.NodeIds.ToList().AsReadOnly();
    }

    /// <summary>
    /// Batch-create multiple edges in a single RPC call.
    /// Returns the IDs of the created edges.
    /// </summary>
    public async ValueTask<IReadOnlyList<ulong>> BatchCreateEdgesAsync(
        IEnumerable<(ulong Source, ulong Target, string EdgeType, IReadOnlyDictionary<string, TvValue>? Properties)> edges,
        string? transactionId = null,
        CancellationToken cancellationToken = default)
    {
        var request = new BatchCreateEdgesRequest();
        if (transactionId is not null)
            request.TransactionId = transactionId;

        foreach (var (source, target, edgeType, props) in edges)
        {
            var edgeReq = new CreateEdgeRequest
            {
                Source = source,
                Target = target,
                EdgeType = edgeType,
            };
            if (props is not null)
            {
                foreach (var (k, v) in props)
                    edgeReq.Properties.Add(k, ValueConverter.ToProto(v));
            }
            request.Edges.Add(edgeReq);
        }

        var response = await CallAsync(() => _client.BatchCreateEdgesAsync(request, cancellationToken: cancellationToken).ResponseAsync).ConfigureAwait(false);
        return response.EdgeIds.ToList().AsReadOnly();
    }
}
