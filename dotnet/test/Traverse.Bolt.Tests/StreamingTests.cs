using Traverse.Bolt.Tests.Helpers;
using Xunit;

namespace Traverse.Bolt.Tests;

public sealed class StreamingTests : IClassFixture<BoltFixture>
{
    private readonly BoltFixture _fixture;

    public StreamingTests(BoltFixture fixture) => _fixture = fixture;

    [Fact]
    public async Task RunStreamAsync_ReturnsAllRows()
    {
        var client = _fixture.RequirePlain();

        await using var cursor = await client.RunStreamAsync("UNWIND range(1, 100) AS i RETURN i", ct: TestContext.Current.CancellationToken);

        var values = new List<long>();
        await foreach (var record in cursor)
            values.Add((long)record["i"]!);

        Assert.Equal(100, values.Count);
        Assert.Equal(1L, values[0]);
        Assert.Equal(100L, values[99]);
    }

    [Fact]
    public async Task RunStreamAsync_SmallBatch()
    {
        var client = _fixture.RequirePlain();

        await using var cursor = await client.RunStreamAsync("UNWIND range(1, 5) AS i RETURN i", ct: TestContext.Current.CancellationToken);

        var values = new List<long>();
        await foreach (var record in cursor)
            values.Add((long)record["i"]!);

        Assert.Equal(5, values.Count);
    }

    [Fact]
    public async Task Tls_RunStreamAsync_ReturnsAllRows()
    {
        var client = _fixture.RequireTls();

        await using var cursor = await client.RunStreamAsync("UNWIND range(1, 50) AS i RETURN i", ct: TestContext.Current.CancellationToken);

        var values = new List<long>();
        await foreach (var record in cursor)
            values.Add((long)record["i"]!);

        Assert.Equal(50, values.Count);
    }
}
