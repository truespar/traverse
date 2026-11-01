namespace Traverse.Grpc.Values;

/// <summary>
/// Represents a graph path (alternating nodes and edges) returned from a query.
/// </summary>
public sealed record TvPath(
    IReadOnlyList<TvNode> Nodes,
    IReadOnlyList<TvEdge> Edges);
