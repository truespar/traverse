using Traverse.Grpc.Tests.Helpers;
using Xunit;

namespace Traverse.Grpc.Tests;

public sealed class ConnectionTests : IClassFixture<ServerFixture>
{
    private readonly ServerFixture _fixture;

    public ConnectionTests(ServerFixture fixture) => _fixture = fixture;

    [Fact]
    public void Client_IsConnected()
    {
        var client = _fixture.RequireClient();
        Assert.NotNull(client);
    }

    [Fact]
    public async Task ConnectAsync_InvalidAddress_Throws()
    {
        await Assert.ThrowsAnyAsync<Exception>(async () =>
        {
            await TraverseGrpcClient.ConnectAsync(
                "http://localhost:1",
                cancellationToken: new CancellationTokenSource(TimeSpan.FromSeconds(2)).Token);
        });
    }
}
