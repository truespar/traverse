namespace Traverse.Bolt;

/// <summary>Access mode hint for query routing (read replicas vs leader).</summary>
public enum AccessMode { Write, Read }

/// <summary>Per-query options sent in the RUN/BEGIN extras map.</summary>
public sealed class QueryOptions
{
    /// <summary>Server-side transaction timeout. Sent as milliseconds in "tx_timeout".</summary>
    public TimeSpan? TxTimeout { get; init; }

    /// <summary>Arbitrary metadata attached to the transaction, visible in query logs.</summary>
    public Dictionary<string, object>? TxMetadata { get; init; }

    /// <summary>Access mode hint. Read sends "r"; Write (default) is omitted.</summary>
    public AccessMode Mode { get; init; } = AccessMode.Write;

    /// <summary>Run the query as a different user (requires server support).</summary>
    public string? ImpersonatedUser { get; init; }
}
