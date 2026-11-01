using Traverse.Grpc.Tests.Helpers;
using Traverse.Grpc.Values;
using Xunit;

namespace Traverse.Grpc.Tests;

public sealed class QueryTests : IClassFixture<ServerFixture>
{
    private readonly ServerFixture _fixture;

    public QueryTests(ServerFixture fixture) => _fixture = fixture;

    [Fact]
    public async Task QueryAsync_ReturnLiteral_ReturnsValue()
    {
        var client = _fixture.RequireClient();

        await using var result = await client.QueryAsync("RETURN 42 AS n", cancellationToken: TestContext.Current.CancellationToken);
        var rows = new List<QueryRow>();
        await foreach (var row in result)
            rows.Add(row.Snapshot());

        Assert.Single(rows);
        Assert.Equal(42L, rows[0].GetInt64("n"));
        Assert.NotNull(result.Summary);
    }

    [Fact]
    public async Task QueryAsync_WithParameters_ReturnsParameterValue()
    {
        var client = _fixture.RequireClient();

        var parameters = new Dictionary<string, TvValue> { ["x"] = 123L };
        await using var result = await client.QueryAsync("RETURN $x AS val", parameters, cancellationToken: TestContext.Current.CancellationToken);
        var (rows, summary) = await result.ToListAsync(TestContext.Current.CancellationToken);

        Assert.Single(rows);
        Assert.Equal(123L, rows[0].GetInt64("val"));
    }

    [Fact]
    public async Task ConsumeAsync_WriteQuery_ReturnsSummary()
    {
        var client = _fixture.RequireClient();

        var summary = await (await client.QueryAsync("CREATE (n:_TestConsume) DELETE n", cancellationToken: TestContext.Current.CancellationToken)).ConsumeAsync(TestContext.Current.CancellationToken);
        Assert.NotNull(summary);
    }

    [Fact]
    public async Task QueryResult_DoubleEnumeration_Throws()
    {
        var client = _fixture.RequireClient();

        await using var result = await client.QueryAsync("RETURN 1 AS n", cancellationToken: TestContext.Current.CancellationToken);
        await foreach (var _ in result) { }

        await Assert.ThrowsAsync<InvalidOperationException>(async () =>
        {
            await foreach (var _ in result) { }
        });
    }

    [Fact]
    public async Task QueryAsync_SyntaxError_ThrowsTraverseException()
    {
        var client = _fixture.RequireClient();

        var ex = await Assert.ThrowsAsync<TraverseException>(async () =>
        {
            await (await client.QueryAsync("INVALID CYPHER", cancellationToken: TestContext.Current.CancellationToken)).ConsumeAsync(TestContext.Current.CancellationToken);
        });

        Assert.Equal(TraverseErrorCode.SyntaxError, ex.ErrorCode);
    }
}
