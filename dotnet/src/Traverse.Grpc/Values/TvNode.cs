namespace Traverse.Grpc.Values;

/// <summary>
/// Represents a graph node returned from a query.
/// </summary>
public sealed record TvNode(
    ulong Id,
    IReadOnlyList<string> Labels,
    IReadOnlyDictionary<string, TvValue> Properties);
