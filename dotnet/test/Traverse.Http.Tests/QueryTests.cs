using System.Text.Json;
using Traverse.Http.Tests.Helpers;
using Xunit;

namespace Traverse.Http.Tests;

public sealed class QueryTests : IClassFixture<HttpFixture>
{
    private readonly HttpFixture _fixture;

    public QueryTests(HttpFixture fixture) => _fixture = fixture;

    [Fact]
    public async Task Return_Scalar()
    {
        var client = _fixture.RequireServer();
        var result = await client.QueryAsync("RETURN 42 AS n", database: _fixture.TestDatabase, ct: TestContext.Current.CancellationToken);

        Assert.Single(result.Rows);
        Assert.Equal(42, result.Rows[0][0].GetInt32());
    }

    [Fact]
    public async Task Return_String()
    {
        var client = _fixture.RequireServer();
        var result = await client.QueryAsync("RETURN 'hello' AS s", database: _fixture.TestDatabase, ct: TestContext.Current.CancellationToken);

        Assert.Single(result.Rows);
        Assert.Equal("hello", result.Rows[0][0].GetString());
    }

    [Fact]
    public async Task With_Parameters()
    {
        var client = _fixture.RequireServer();
        var result = await client.QueryAsync("RETURN $x AS val", parameters: new Dictionary<string, object?> { ["x"] = 99 }, database: _fixture.TestDatabase, ct: TestContext.Current.CancellationToken);

        Assert.Single(result.Rows);
        Assert.Equal(99, result.Rows[0][0].GetInt32());
    }

    [Fact]
    public async Task Gql_Dialect()
    {
        var client = _fixture.RequireServer();
        var health = await client.HealthAsync(TestContext.Current.CancellationToken);
        Assert.SkipUnless(
            health.Features.TryGetValue("gql", out var gql) && gql,
            "server built without the gql feature");

        // `||` concat and `IS INTEGER` are GQL-only syntax - they
        // parse only when the dialect reaches the server.
        var result = await client.QueryAsync("RETURN 'foo' || 'bar' AS s, 1 IS INTEGER AS i", database: _fixture.TestDatabase, dialect: "gql", ct: TestContext.Current.CancellationToken);

        Assert.Equal("foobar", result.Rows[0][0].GetString());
        Assert.True(result.Rows[0][1].GetBoolean());
    }

    [Fact]
    public async Task Write_Query_Stats()
    {
        var client = _fixture.RequireServer();
        var db = _fixture.TestDatabase;

        var result = await client.QueryAsync("CREATE (n:_HttpTest) RETURN n", database: db, ct: TestContext.Current.CancellationToken);
        try
        {
            Assert.Equal(1, result.Stats.NodesCreated);
        }
        finally
        {
            await client.QueryAsync("MATCH (n:_HttpTest) DELETE n", database: db, ct: TestContext.Current.CancellationToken);
        }
    }

    [Fact]
    public async Task Syntax_Error_Throws()
    {
        var client = _fixture.RequireServer();

        var ex = await Assert.ThrowsAsync<TraverseHttpException>(async () =>
        {
            await client.QueryAsync("THIS IS NOT CYPHER", database: _fixture.TestDatabase, ct: TestContext.Current.CancellationToken);
        });

        Assert.Equal(System.Net.HttpStatusCode.BadRequest, ex.StatusCode);
    }
}
