namespace Traverse.Bolt;

/// <summary>Connection options for <see cref="BoltClient"/>.</summary>
public sealed class BoltClientOptions
{
    /// <summary>Username for LOGON authentication. Null to skip authentication.</summary>
    public string? Username { get; init; }

    /// <summary>Password for LOGON authentication.</summary>
    public string? Password { get; init; }

    /// <summary>Target database name (sent in RUN extras).</summary>
    public string? Database { get; init; }

    /// <summary>Enable TLS encryption for the connection.</summary>
    public bool UseTls { get; init; }

    /// <summary>Accept untrusted/self-signed server certificates (for development).</summary>
    public bool AllowUntrustedCertificate { get; init; }

    /// <summary>TCP connection timeout. Default: 30 seconds.</summary>
    public TimeSpan ConnectionTimeout { get; init; } = TimeSpan.FromSeconds(30);

    /// <summary>Notification filtering sent in HELLO, RUN, and BEGIN metadata.</summary>
    public NotificationsConfig? Notifications { get; init; }

    /// <summary>PULL batch size. -1 (default) fetches all records at once; positive values enable batched streaming.</summary>
    public long FetchSize { get; init; } = -1;

    /// <summary>Auth token for LOGON. When set, overrides Username/Password.</summary>
    public BoltAuthToken? AuthToken { get; init; }

    /// <summary>Disable telemetry messages. Default: false.</summary>
    public bool TelemetryDisabled { get; init; }

    /// <summary>Routing context sent in HELLO for cluster-aware connections. Null = no routing.</summary>
    public Dictionary<string, string>? RoutingContext { get; init; }
}
