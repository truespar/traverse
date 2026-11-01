using Traverse.Grpc;
using Xunit;

namespace Traverse.Grpc.Tests.Helpers;

/// <summary>
/// Shared fixture that connects to a running Traverse gRPC server.
/// Tests are skipped if the server is unavailable.
/// </summary>
public sealed class ServerFixture : IAsyncLifetime
{
    public const string DefaultAddress = "http://localhost:50051";
    public const string SkipReason = "Traverse gRPC server not available";

    public TraverseGrpcClient? Client { get; private set; }
    public bool IsAvailable => Client is not null;

    public async ValueTask InitializeAsync()
    {
        try
        {
            Client = await TraverseGrpcClient.ConnectAsync(
                DefaultAddress,
                new TraverseClientOptions(),
                new CancellationTokenSource(TimeSpan.FromSeconds(3)).Token);
        }
        catch
        {
            Client = null;
        }
    }

    public async ValueTask DisposeAsync()
    {
        if (Client is not null)
            await Client.DisposeAsync();
    }

    public TraverseGrpcClient RequireClient()
    {
        Assert.SkipWhen(!IsAvailable, SkipReason);
        return Client!;
    }
}
