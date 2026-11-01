using Traverse.Bolt.Tests.Helpers;
using Xunit;

namespace Traverse.Bolt.Tests;

public sealed class ConnectionTests : IClassFixture<BoltFixture>
{
    private readonly BoltFixture _fixture;

    public ConnectionTests(BoltFixture fixture) => _fixture = fixture;

    // -- Plain Bolt ---------------------------------------------------

    [Fact]
    public void PlainBolt_IsConnected()
    {
        var client = _fixture.RequirePlain();
        Assert.NotNull(client);
    }

    [Fact]
    public async Task PlainBolt_ReturnLiteral()
    {
        var client = _fixture.RequirePlain();
        var result = await client.RunAsync("RETURN 1 AS x", ct: TestContext.Current.CancellationToken);

        Assert.Equal(["x"], result.Columns);
        Assert.Single(result.Records);
        Assert.Equal(1L, result.Records[0]["x"]);
    }

    // -- Bolt+TLS ----------------------------------------------------

    [Fact]
    public void BoltTls_IsConnected()
    {
        var client = _fixture.RequireTls();
        Assert.NotNull(client);
    }

    [Fact]
    public async Task BoltTls_ReturnLiteral()
    {
        var client = _fixture.RequireTls();
        var result = await client.RunAsync("RETURN 1 AS x", ct: TestContext.Current.CancellationToken);

        Assert.Equal(["x"], result.Columns);
        Assert.Single(result.Records);
        Assert.Equal(1L, result.Records[0]["x"]);
    }

    // -- Connection errors -------------------------------------------

    [Fact]
    public async Task ConnectAsync_InvalidAddress_Throws()
    {
        var opts = new BoltClientOptions { ConnectionTimeout = TimeSpan.FromSeconds(2) };
        await Assert.ThrowsAnyAsync<Exception>(async () =>
        {
            await BoltClient.ConnectAsync("localhost", 1, opts, TestContext.Current.CancellationToken);
        });
    }

    [Fact]
    public async Task ConnectAsync_InvalidUri_Throws()
    {
        await Assert.ThrowsAnyAsync<Exception>(async () =>
        {
            var opts = new BoltClientOptions { ConnectionTimeout = TimeSpan.FromSeconds(2) };
            await BoltClient.ConnectAsync("bolt://localhost:1", opts, TestContext.Current.CancellationToken);
        });
    }
}
