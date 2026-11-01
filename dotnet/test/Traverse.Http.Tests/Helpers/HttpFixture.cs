using Xunit;

namespace Traverse.Http.Tests.Helpers;

/// <summary>
/// Shared fixture that connects to a running Traverse HTTP server on localhost:7691.
/// Creates an isolated test database per fixture instance and drops it on dispose.
/// Tests are skipped if the server is unavailable.
/// </summary>
public sealed class HttpFixture : IAsyncLifetime
{
    public static readonly string BaseUrl = Environment.GetEnvironmentVariable("TRAVERSE_BASE_URL") ?? "http://localhost:7691";
    public static readonly string SkipMessage = $"Traverse HTTP server not available on {BaseUrl}";

    public TraverseHttpClient? Client { get; private set; }
    public bool Available => Client is not null;

    /// <summary>API key used by the client (null if auth is not required).</summary>
    public string? ApiKey { get; private set; }

    /// <summary>Name of the isolated test database created for this fixture.</summary>
    public string TestDatabase { get; } = $"_httptest_{Guid.NewGuid().ToString("N")[..8]}";

    public async ValueTask InitializeAsync()
    {
        try
        {
            // First probe health without auth (the health endpoint is always open)
            using var probe = new TraverseHttpClient(BaseUrl, new TraverseHttpOptions
            {
                Timeout = TimeSpan.FromSeconds(3),
            });
            var health = await probe.HealthAsync();
            if (health.Status != "ok") return;

            // Management endpoints require X-API-Key even when user
            // auth is disabled - always forward the env key when set.
            ApiKey = Environment.GetEnvironmentVariable("TRAVERSE_API_KEY");
            var client = new TraverseHttpClient(BaseUrl, new TraverseHttpOptions
            {
                ApiKey = ApiKey,
                Timeout = TimeSpan.FromSeconds(10),
            });

            // Create an isolated test database
            await client.CreateDatabaseAsync(TestDatabase);

            Client = client;
        }
        catch
        {
            Client = null;
        }
    }

    public async ValueTask DisposeAsync()
    {
        if (Client is null) return;

        try
        {
            await Client.DropDatabaseAsync(TestDatabase);
        }
        catch { /* best effort cleanup */ }

        Client.Dispose();
    }

    public TraverseHttpClient RequireServer()
    {
        Assert.SkipWhen(!Available, SkipMessage);
        return Client!;
    }
}
