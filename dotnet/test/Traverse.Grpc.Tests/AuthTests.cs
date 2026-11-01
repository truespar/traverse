using Traverse.Grpc.Tests.Helpers;
using Xunit;

namespace Traverse.Grpc.Tests;

public sealed class AuthTests : IClassFixture<ServerFixture>
{
    private readonly ServerFixture _fixture;

    public AuthTests(ServerFixture fixture) => _fixture = fixture;

    [Fact]
    public void Credentials_StoresUsername()
    {
        var creds = new TraverseCredentials("admin", "secret");
        Assert.Equal("admin", creds.Username);
    }

    [Fact]
    public async Task ConnectWithCredentials_CanQuery()
    {
        Assert.SkipWhen(!_fixture.IsAvailable, ServerFixture.SkipReason);

        await using var client = await TraverseGrpcClient.ConnectAsync(ServerFixture.DefaultAddress, new TraverseClientOptions
            {
                Credentials = new TraverseCredentials("admin", "admin"),
            }, TestContext.Current.CancellationToken);

        await using var result = await client.QueryAsync("RETURN 1 AS n", cancellationToken: TestContext.Current.CancellationToken);
        var (rows, _) = await result.ToListAsync(TestContext.Current.CancellationToken);
        Assert.Single(rows);
    }
}
