using Traverse.Bolt.Tests.Helpers;
using Xunit;

namespace Traverse.Bolt.Tests;

/// <summary>Tests run against both plain and TLS connections where available.</summary>
public sealed class QueryTests : IClassFixture<BoltFixture>
{
    private readonly BoltFixture _fixture;

    public QueryTests(BoltFixture fixture) => _fixture = fixture;

    // -- Return types ------------------------------------------------

    [Fact]
    public async Task Return_Integer()
    {
        var client = _fixture.RequirePlain();
        var result = await client.RunAsync("RETURN 42 AS n", ct: TestContext.Current.CancellationToken);

        Assert.Single(result.Records);
        Assert.Equal(42L, result.Records[0]["n"]);
    }

    [Fact]
    public async Task Return_String()
    {
        var client = _fixture.RequirePlain();
        var result = await client.RunAsync("RETURN 'hello' AS s", ct: TestContext.Current.CancellationToken);

        Assert.Single(result.Records);
        Assert.Equal("hello", result.Records[0]["s"]);
    }

    [Fact]
    public async Task Return_Float()
    {
        var client = _fixture.RequirePlain();
        var result = await client.RunAsync("RETURN 3.14 AS f", ct: TestContext.Current.CancellationToken);

        Assert.Single(result.Records);
        Assert.Equal(3.14, result.Records[0]["f"]);
    }

    [Fact]
    public async Task Return_Boolean()
    {
        var client = _fixture.RequirePlain();
        var result = await client.RunAsync("RETURN true AS b", ct: TestContext.Current.CancellationToken);

        Assert.Single(result.Records);
        Assert.Equal(true, result.Records[0]["b"]);
    }

    [Fact]
    public async Task Return_Null()
    {
        var client = _fixture.RequirePlain();
        var result = await client.RunAsync("RETURN null AS n", ct: TestContext.Current.CancellationToken);

        Assert.Single(result.Records);
        Assert.Null(result.Records[0]["n"]);
    }

    [Fact]
    public async Task Return_List()
    {
        var client = _fixture.RequirePlain();
        var result = await client.RunAsync("RETURN [1, 2, 3] AS list", ct: TestContext.Current.CancellationToken);

        Assert.Single(result.Records);
        var list = Assert.IsType<List<object?>>(result.Records[0]["list"]);
        Assert.Equal(3, list.Count);
    }

    [Fact]
    public async Task Return_Map()
    {
        var client = _fixture.RequirePlain();
        var result = await client.RunAsync("RETURN {a: 1, b: 'two'} AS map", ct: TestContext.Current.CancellationToken);

        Assert.Single(result.Records);
        var map = Assert.IsType<Dictionary<string, object?>>(result.Records[0]["map"]);
        Assert.Equal(1L, map["a"]);
        Assert.Equal("two", map["b"]);
    }

    // -- Multiple columns --------------------------------------------

    [Fact]
    public async Task Return_MultipleColumns()
    {
        var client = _fixture.RequirePlain();
        var result = await client.RunAsync("RETURN 1 AS a, 'x' AS b, true AS c", ct: TestContext.Current.CancellationToken);

        Assert.Equal(["a", "b", "c"], result.Columns);
        Assert.Single(result.Records);
        Assert.Equal(1L, result.Records[0]["a"]);
        Assert.Equal("x", result.Records[0]["b"]);
        Assert.Equal(true, result.Records[0]["c"]);
    }

    // -- Multiple rows -----------------------------------------------

    [Fact]
    public async Task Unwind_MultipleRows()
    {
        var client = _fixture.RequirePlain();
        var result = await client.RunAsync("UNWIND range(1, 5) AS i RETURN i", ct: TestContext.Current.CancellationToken);

        Assert.Equal(5, result.Records.Count);
        for (int i = 0; i < 5; i++)
            Assert.Equal((long)(i + 1), result.Records[i]["i"]);
    }

    // -- Parameters --------------------------------------------------

    [Fact]
    public async Task Parameters_Integer()
    {
        var client = _fixture.RequirePlain();
        var result = await client.RunAsync("RETURN $x AS val", new Dictionary<string, object> { ["x"] = 99L }, ct: TestContext.Current.CancellationToken);

        Assert.Single(result.Records);
        Assert.Equal(99L, result.Records[0]["val"]);
    }

    [Fact]
    public async Task Parameters_String()
    {
        var client = _fixture.RequirePlain();
        var result = await client.RunAsync("RETURN $name AS val", new Dictionary<string, object> { ["name"] = "Alice" }, ct: TestContext.Current.CancellationToken);

        Assert.Single(result.Records);
        Assert.Equal("Alice", result.Records[0]["val"]);
    }

    // -- Syntax errors -----------------------------------------------

    [Fact]
    public async Task SyntaxError_ThrowsBoltException()
    {
        var client = _fixture.RequirePlain();

        var ex = await Assert.ThrowsAsync<BoltException>(async () =>
        {
            await client.RunAsync("THIS IS NOT CYPHER", ct: TestContext.Current.CancellationToken);
        });

        Assert.Contains("SyntaxError", ex.Code);
    }

    // -- RunAndConsumeAsync ------------------------------------------

    [Fact]
    public async Task RunAndConsume_ReturnsEmptyRecords()
    {
        var client = _fixture.RequirePlain();
        var result = await client.RunAndConsumeAsync("UNWIND range(1, 100) AS i RETURN i", ct: TestContext.Current.CancellationToken);

        Assert.Empty(result.Records);
        Assert.NotNull(result.Summary);
    }

    // -- RunAndDiscardAsync ------------------------------------------

    [Fact]
    public async Task RunAndDiscard_ReturnsEmptyRecords()
    {
        var client = _fixture.RequirePlain();
        var result = await client.RunAndDiscardAsync("UNWIND range(1, 100) AS i RETURN i", ct: TestContext.Current.CancellationToken);

        Assert.Empty(result.Records);
        Assert.NotNull(result.Summary);
    }

    // -- TLS variants ------------------------------------------------

    [Fact]
    public async Task Tls_Return_Integer()
    {
        var client = _fixture.RequireTls();
        var result = await client.RunAsync("RETURN 42 AS n", ct: TestContext.Current.CancellationToken);

        Assert.Single(result.Records);
        Assert.Equal(42L, result.Records[0]["n"]);
    }

    [Fact]
    public async Task Tls_Return_String()
    {
        var client = _fixture.RequireTls();
        var result = await client.RunAsync("RETURN 'hello' AS s", ct: TestContext.Current.CancellationToken);

        Assert.Single(result.Records);
        Assert.Equal("hello", result.Records[0]["s"]);
    }

    [Fact]
    public async Task Tls_Parameters()
    {
        var client = _fixture.RequireTls();
        var result = await client.RunAsync("RETURN $x + $y AS sum", new Dictionary<string, object> { ["x"] = 10L, ["y"] = 20L }, ct: TestContext.Current.CancellationToken);

        Assert.Single(result.Records);
        Assert.Equal(30L, result.Records[0]["sum"]);
    }

    [Fact]
    public async Task Tls_MultipleRows()
    {
        var client = _fixture.RequireTls();
        var result = await client.RunAsync("UNWIND range(1, 10) AS i RETURN i", ct: TestContext.Current.CancellationToken);

        Assert.Equal(10, result.Records.Count);
    }
}
