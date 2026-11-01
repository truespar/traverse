namespace Traverse.Bolt;

/// <summary>A node returned from a Cypher query.</summary>
public sealed record BoltNode(
    long Id, string[] Labels, Dictionary<string, object?> Properties, string? ElementId = null);

/// <summary>A relationship (with start/end node IDs) returned from a Cypher query.</summary>
public sealed record BoltRelationship(
    long Id, long StartNodeId, long EndNodeId, string Type,
    Dictionary<string, object?> Properties, string? ElementId = null,
    string? StartNodeElementId = null, string? EndNodeElementId = null);

/// <summary>An unbound relationship (no start/end node IDs) used within path structures.</summary>
public sealed record BoltUnboundRelationship(
    long Id, string Type, Dictionary<string, object?> Properties, string? ElementId = null);

/// <summary>A path consisting of alternating nodes and relationships.</summary>
public sealed record BoltPath(BoltNode[] Nodes, BoltRelationship[] Relationships);
