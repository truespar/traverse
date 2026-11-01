namespace Traverse.Http;

/// <summary>
/// Configuration options for <see cref="TraverseHttpClient"/>.
/// </summary>
public sealed class TraverseHttpOptions
{
    /// <summary>
    /// API key for authentication (sent as <c>Authorization: Bearer {key}</c>).
    /// Null to skip authentication.
    /// </summary>
    public string? ApiKey { get; init; }

    /// <summary>
    /// Allow connections to servers with untrusted TLS certificates
    /// (self-signed, expired, etc.). Default: false.
    /// </summary>
    public bool AllowUntrustedCertificate { get; init; }

    /// <summary>
    /// HTTP request timeout. Default: 100 seconds (HttpClient default).
    /// Set to <see cref="System.Threading.Timeout.InfiniteTimeSpan"/> for
    /// no timeout (useful for large file uploads/downloads).
    /// </summary>
    public TimeSpan? Timeout { get; init; }

    /// <summary>
    /// Timeout for file upload/download operations.
    /// Default: <see cref="System.Threading.Timeout.InfiniteTimeSpan"/> (no timeout).
    /// The regular <see cref="Timeout"/> applies to all other API calls.
    /// </summary>
    public TimeSpan? FileTransferTimeout { get; init; }
}
