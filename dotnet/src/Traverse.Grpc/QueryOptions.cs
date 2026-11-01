namespace Traverse.Grpc;

/// <summary>
/// Per-query options for <see cref="TraverseGrpcClient.QueryAsync"/>.
/// </summary>
public sealed class QueryOptions
{
    /// <summary>
    /// Target database name. Null uses the client default.
    /// </summary>
    public string? Database { get; init; }

    /// <summary>
    /// Causal consistency bookmarks.
    /// </summary>
    public IReadOnlyList<string>? Bookmarks { get; init; }

    /// <summary>
    /// Access mode: "r" for read-only, "w" for read-write. Default "w".
    /// </summary>
    public string? AccessMode { get; init; }

    /// <summary>
    /// Query timeout. Zero or null means no timeout.
    /// </summary>
    public TimeSpan? Timeout { get; init; }

    /// <summary>
    /// Transaction ID for executing within an explicit transaction.
    /// </summary>
    public string? TransactionId { get; init; }

    /// <summary>
    /// Minimum notification severity: "WARNING", "INFORMATION", or "OFF".
    /// </summary>
    public string? NotificationMinimumSeverity { get; init; }

    /// <summary>
    /// Notification categories to disable.
    /// </summary>
    public IReadOnlyList<string>? NotificationDisabledCategories { get; init; }
}
