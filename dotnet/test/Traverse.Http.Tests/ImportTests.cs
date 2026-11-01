using Traverse.Http.Tests.Helpers;
using Xunit;

namespace Traverse.Http.Tests;

public sealed class ImportTests : IClassFixture<HttpFixture>
{
    private readonly HttpFixture _fixture;

    public ImportTests(HttpFixture fixture) => _fixture = fixture;

    [Fact]
    public async Task ImportCypher_Creates_Nodes()
    {
        var client = _fixture.RequireServer();
        var db = _fixture.TestDatabase;

        var cypher = "CREATE (:_ImportTest {id: 1});\nCREATE (:_ImportTest {id: 2});\nCREATE (:_ImportTest {id: 3});";

        var result = await client.ImportCypherAsync(cypher, database: db, ct: TestContext.Current.CancellationToken);
        try
        {
            Assert.Equal(3, result.Executed);
            Assert.Empty(result.Errors);
        }
        finally
        {
            await client.QueryAsync("MATCH (n:_ImportTest) DELETE n", database: db, ct: TestContext.Current.CancellationToken);
        }
    }

    [Fact]
    public async Task ImportCsv_Creates_Nodes()
    {
        var client = _fixture.RequireServer();
        var db = _fixture.TestDatabase;

        var csv = "name,age\nAlice,30\nBob,25";
        var columns = new CsvColumnMapping[]
        {
            new() { Name = "name", Property = "name", Type = "string" },
            new() { Name = "age", Property = "age", Type = "integer" },
        };

        var result = await client.ImportCsvAsync(csv, "_CsvTestNode", columns, database: db, ct: TestContext.Current.CancellationToken);
        try
        {
            Assert.Equal(2, result.NodesCreated);
            Assert.Empty(result.Errors);
        }
        finally
        {
            await client.QueryAsync("MATCH (n:_CsvTestNode) DELETE n", database: db, ct: TestContext.Current.CancellationToken);
        }
    }

    [Fact]
    public async Task ImportCsvEdges_Creates_Edges()
    {
        var client = _fixture.RequireServer();
        var db = _fixture.TestDatabase;

        // Create source and target nodes first
        await client.QueryAsync("CREATE (:_CsvEdgeSrc {key: 'a'}), (:_CsvEdgeDst {key: 'b'})", database: db, ct: TestContext.Current.CancellationToken);

        try
        {
            var csv = "src,dst\na,b";
            var source = new CsvEdgeEndpoint { Label = "_CsvEdgeSrc", Column = "src", Property = "key" };
            var target = new CsvEdgeEndpoint { Label = "_CsvEdgeDst", Column = "dst", Property = "key" };

            var result = await client.ImportCsvEdgesAsync(csv, "_CSV_TEST_REL", source, target, database: db, ct: TestContext.Current.CancellationToken);

            Assert.Equal(1, result.EdgesCreated);
            Assert.Empty(result.Errors);
        }
        finally
        {
            await client.QueryAsync("MATCH (n:_CsvEdgeSrc) DETACH DELETE n", database: db, ct: TestContext.Current.CancellationToken);
            await client.QueryAsync("MATCH (n:_CsvEdgeDst) DETACH DELETE n", database: db, ct: TestContext.Current.CancellationToken);
        }
    }
}
