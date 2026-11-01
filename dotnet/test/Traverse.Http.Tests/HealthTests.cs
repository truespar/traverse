using Traverse.Http.Tests.Helpers;
using Xunit;

namespace Traverse.Http.Tests;

public sealed class HealthTests : IClassFixture<HttpFixture>
{
    private readonly HttpFixture _fixture;

    public HealthTests(HttpFixture fixture) => _fixture = fixture;

    [Fact]
    public async Task Health_Returns_Status_And_Version()
    {
        var client = _fixture.RequireServer();
        var health = await client.HealthAsync(TestContext.Current.CancellationToken);

        Assert.Equal("ok", health.Status);
        Assert.False(string.IsNullOrEmpty(health.Version));
    }

    [Fact]
    public async Task Ready_Returns_True()
    {
        var client = _fixture.RequireServer();
        var ready = await client.ReadyAsync(TestContext.Current.CancellationToken);

        Assert.True(ready);
    }

    [Fact]
    public async Task Health_Reports_Features()
    {
        var client = _fixture.RequireServer();
        var health = await client.HealthAsync(TestContext.Current.CancellationToken);

        // The features map is present on all current servers
        // (gql, grpc, tls, mcp, ml).
        Assert.True(health.Features.ContainsKey("gql"));
    }
}
