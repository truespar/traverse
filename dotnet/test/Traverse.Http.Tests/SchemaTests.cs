using Traverse.Http.Tests.Helpers;
using Xunit;

namespace Traverse.Http.Tests;

public sealed class SchemaTests : IClassFixture<HttpFixture>
{
    private readonly HttpFixture _fixture;

    public SchemaTests(HttpFixture fixture) => _fixture = fixture;

    [Fact]
    public async Task GetSchema_Returns_Labels()
    {
        var client = _fixture.RequireServer();
        var db = _fixture.TestDatabase;

        // Create a node and edge so the schema has something to report
        await client.QueryAsync("CREATE (a:_SchemaTestNode {name: 'a'})-[:_SCHEMA_TEST_REL]->(b:_SchemaTestNode {name: 'b'})", database: db, ct: TestContext.Current.CancellationToken);

        try
        {
            var schema = await client.GetSchemaAsync(db, TestContext.Current.CancellationToken);

            Assert.Contains("_SchemaTestNode", schema.Labels);
            Assert.Contains("_SCHEMA_TEST_REL", schema.RelationshipTypes);
        }
        finally
        {
            await client.QueryAsync("MATCH (n:_SchemaTestNode) DETACH DELETE n", database: db, ct: TestContext.Current.CancellationToken);
        }
    }
}
