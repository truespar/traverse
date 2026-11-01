using System.IO.Compression;

namespace Traverse.Grpc;

/// <summary>
/// Connection configuration for <see cref="TraverseGrpcClient"/>.
/// </summary>
public sealed class TraverseClientOptions
{
    /// <summary>
    /// Optional credentials for authentication.
    /// </summary>
    public TraverseCredentials? Credentials { get; init; }

    /// <summary>
    /// Default database name for queries. Empty string uses server default.
    /// </summary>
    public string Database { get; init; } = "";

    /// <summary>
    /// HTTP/2 keepalive ping interval. Default matches server (10 seconds).
    /// </summary>
    public TimeSpan KeepAliveInterval { get; init; } = TimeSpan.FromSeconds(10);

    /// <summary>
    /// HTTP/2 keepalive ping timeout. Default matches server (20 seconds).
    /// </summary>
    public TimeSpan KeepAliveTimeout { get; init; } = TimeSpan.FromSeconds(20);

    /// <summary>
    /// Maximum send message size in bytes. Default 128 MB (matches server).
    /// </summary>
    public int MaxSendMessageSize { get; init; } = 128 * 1024 * 1024;

    /// <summary>
    /// Maximum receive message size in bytes. Default 128 MB (matches server).
    /// </summary>
    public int MaxReceiveMessageSize { get; init; } = 128 * 1024 * 1024;

    /// <summary>
    /// Enable gzip compression for requests. Default true.
    /// </summary>
    public bool EnableCompression { get; init; } = true;
}
