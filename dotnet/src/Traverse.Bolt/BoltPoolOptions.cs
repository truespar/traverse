namespace Traverse.Bolt;

/// <summary>Configuration for <see cref="BoltPool"/> connection lifecycle management.</summary>
public sealed class BoltPoolOptions
{
    /// <summary>Maximum number of connections in the pool. Default: 100.</summary>
    public int MaxSize { get; init; } = 100;

    /// <summary>Maximum time to wait for a connection from the pool. Default: 1 minute.</summary>
    public TimeSpan ConnectionAcquisitionTimeout { get; init; } = TimeSpan.FromMinutes(1);

    /// <summary>Maximum lifetime of a connection before it is retired. Default: 1 hour.</summary>
    public TimeSpan MaxConnectionLifetime { get; init; } = TimeSpan.FromHours(1);

    /// <summary>Maximum idle time before a connection is evicted. Null = no eviction.</summary>
    public TimeSpan? ConnectionIdleTimeout { get; init; }

    /// <summary>
    /// If a connection has been idle longer than this, send RESET to verify liveness before reuse.
    /// Null = no liveness probe.
    /// </summary>
    public TimeSpan? ConnectionLivenessThreshold { get; init; }

    /// <summary>
    /// Interval for background maintenance that proactively evicts expired and idle connections.
    /// Null = no background maintenance (cleanup happens lazily on acquire). Default: null.
    /// </summary>
    public TimeSpan? IdleMaintenanceInterval { get; init; }
}
