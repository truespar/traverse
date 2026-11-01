using System;
using Xunit;

namespace Traverse.Bolt.Tests.Helpers;

/// <summary>
/// Shared fixture that connects to a running Traverse Bolt server on localhost:7690 (plain)
/// and optionally a TLS endpoint (TRAVERSE_BOLT_TLS_URI, default bolt+ssc://localhost:7692).
/// Tests are skipped if the server is unavailable.
/// </summary>
public sealed class BoltFixture : IAsyncLifetime
{
    public const string PlainUri = "bolt://localhost:7690";
    public static readonly string TlsUri =
        Environment.GetEnvironmentVariable("TRAVERSE_BOLT_TLS_URI") ?? "bolt+ssc://localhost:7692";
    public const string SkipPlain = "Traverse Bolt server not available on localhost:7690";
    public static readonly string SkipTls = $"Traverse Bolt+TLS server not available on {TlsUri}";

    public BoltClient? PlainClient { get; private set; }
    public BoltClient? TlsClient { get; private set; }

    public bool PlainAvailable => PlainClient is not null;
    public bool TlsAvailable => TlsClient is not null;

    public async ValueTask InitializeAsync()
    {
        // Plain Bolt
        try
        {
            using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(3));
            PlainClient = await BoltClient.ConnectAsync(PlainUri, ct: cts.Token);
        }
        catch
        {
            PlainClient = null;
        }

        // Bolt+TLS (self-signed)
        try
        {
            using var cts = new CancellationTokenSource(TimeSpan.FromSeconds(3));
            TlsClient = await BoltClient.ConnectAsync(TlsUri, ct: cts.Token);
        }
        catch
        {
            TlsClient = null;
        }
    }

    public async ValueTask DisposeAsync()
    {
        if (PlainClient is not null)
            await PlainClient.DisposeAsync();
        if (TlsClient is not null)
            await TlsClient.DisposeAsync();
    }

    public BoltClient RequirePlain()
    {
        Assert.SkipWhen(!PlainAvailable, SkipPlain);
        return PlainClient!;
    }

    public BoltClient RequireTls()
    {
        Assert.SkipWhen(!TlsAvailable, SkipTls);
        return TlsClient!;
    }
}
