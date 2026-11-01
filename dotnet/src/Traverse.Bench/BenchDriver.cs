using Traverse.Bolt;
using Traverse.Grpc;
using Traverse.Grpc.Values;
using Neo4j.Driver;

namespace Traverse.Bench;

/// <summary>Abstraction over query execution backends.</summary>
internal interface IBenchDriver : IAsyncDisposable
{
    /// <summary>Execute a query and fully drain the result stream.</summary>
    ValueTask RunAndDrainAsync(string cypher, Dictionary<string, object>? parameters);
}

/// <summary>Driver using Traverse gRPC client.</summary>
internal sealed class TraverseGrpcDriver : IBenchDriver
{
    private readonly TraverseGrpcClient _client;

    private TraverseGrpcDriver(TraverseGrpcClient client) => _client = client;

    public static async ValueTask<TraverseGrpcDriver> ConnectAsync(string host, int port, string? auth)
    {
        TraverseClientOptions? options = null;
        if (auth is not null)
        {
            var parts = auth.Split(':', 2);
            options = new TraverseClientOptions
            {
                Credentials = new TraverseCredentials(parts[0], parts.Length > 1 ? parts[1] : ""),
                EnableCompression = false, // compression OFF for fair comparison
            };
        }
        else
        {
            options = new TraverseClientOptions { EnableCompression = false };
        }

        var client = await TraverseGrpcClient.ConnectAsync($"http://{host}:{port}", options);
        return new TraverseGrpcDriver(client);
    }

    public async ValueTask RunAndDrainAsync(string cypher, Dictionary<string, object>? parameters)
    {
        Dictionary<string, TvValue>? fgParams = null;
        if (parameters is { Count: > 0 })
        {
            fgParams = new(parameters.Count);
            foreach (var (k, v) in parameters)
                fgParams[k] = v is long l ? (TvValue)l : (TvValue)(long)(int)v;
        }

        await using var result = await _client.QueryAsync(cypher, fgParams);
        await result.ConsumeAsync();
    }

    public ValueTask DisposeAsync() => _client.DisposeAsync();
}

/// <summary>Driver using Traverse gRPC bidirectional query channel.</summary>
internal sealed class TraverseGrpcChannelDriver : IBenchDriver
{
    private readonly TraverseGrpcClient _client;
    private readonly QueryChannel _channel;

    private TraverseGrpcChannelDriver(TraverseGrpcClient client, QueryChannel channel)
    {
        _client = client;
        _channel = channel;
    }

    public static async ValueTask<TraverseGrpcChannelDriver> ConnectAsync(string host, int port, string? auth)
    {
        TraverseClientOptions? options = null;
        if (auth is not null)
        {
            var parts = auth.Split(':', 2);
            options = new TraverseClientOptions
            {
                Credentials = new TraverseCredentials(parts[0], parts.Length > 1 ? parts[1] : ""),
                EnableCompression = false,
            };
        }
        else
        {
            options = new TraverseClientOptions { EnableCompression = false };
        }

        var client = await TraverseGrpcClient.ConnectAsync($"http://{host}:{port}", options);
        var channel = client.OpenQueryChannel();
        return new TraverseGrpcChannelDriver(client, channel);
    }

    public async ValueTask RunAndDrainAsync(string cypher, Dictionary<string, object>? parameters)
    {
        Dictionary<string, TvValue>? fgParams = null;
        if (parameters is { Count: > 0 })
        {
            fgParams = new(parameters.Count);
            foreach (var (k, v) in parameters)
                fgParams[k] = v is long l ? (TvValue)l : (TvValue)(long)(int)v;
        }

        await _channel.RunAndConsumeAsync(cypher, fgParams);
    }

    public async ValueTask DisposeAsync()
    {
        await _channel.DisposeAsync();
        await _client.DisposeAsync();
    }
}

/// <summary>Driver using native Traverse.Bolt client (zero-dependency, pipelined RUN+PULL).</summary>
internal sealed class TraverseNativeBoltDriver : IBenchDriver
{
    private readonly BoltClient _client;

    private TraverseNativeBoltDriver(BoltClient client) => _client = client;

    public static async ValueTask<TraverseNativeBoltDriver> ConnectAsync(string host, int port, string? auth)
    {
        BoltClientOptions? options = null;
        if (auth is not null)
        {
            var parts = auth.Split(':', 2);
            options = new BoltClientOptions
            {
                Username = parts[0],
                Password = parts.Length > 1 ? parts[1] : "",
            };
        }

        var client = await BoltClient.ConnectAsync(host, port, options);
        return new TraverseNativeBoltDriver(client);
    }

    public async ValueTask RunAndDrainAsync(string cypher, Dictionary<string, object>? parameters)
        => await _client.RunAndConsumeAsync(cypher, parameters);

    public ValueTask DisposeAsync() => _client.DisposeAsync();
}

/// <summary>Driver using Neo4j.Driver Bolt protocol (works for both Traverse Bolt and Neo4j).</summary>
internal sealed class BoltDriver : IBenchDriver
{
    private readonly IDriver _driver;

    private BoltDriver(IDriver driver) => _driver = driver;

    public static ValueTask<BoltDriver> ConnectAsync(string host, int port, string? auth)
    {
        var uri = $"bolt://{host}:{port}";
        IAuthToken authToken;
        if (auth is not null)
        {
            var parts = auth.Split(':', 2);
            authToken = AuthTokens.Basic(parts[0], parts.Length > 1 ? parts[1] : "");
        }
        else
        {
            authToken = AuthTokens.None;
        }

        var driver = GraphDatabase.Driver(uri, authToken);
        return ValueTask.FromResult(new BoltDriver(driver));
    }

    public async ValueTask RunAndDrainAsync(string cypher, Dictionary<string, object>? parameters)
    {
        await using var session = _driver.AsyncSession();
        var cursor = await session.RunAsync(cypher, parameters);
        await cursor.ConsumeAsync();
    }

    public async ValueTask DisposeAsync()
    {
        await _driver.DisposeAsync();
    }
}
