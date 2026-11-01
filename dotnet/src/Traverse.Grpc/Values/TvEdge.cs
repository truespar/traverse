namespace Traverse.Grpc.Values;

/// <summary>
/// Represents a graph edge (relationship) returned from a query.
/// </summary>
public sealed record TvEdge(
    ulong Id,
    string Type,
    ulong Source,
    ulong Target,
    IReadOnlyDictionary<string, TvValue> Properties);
