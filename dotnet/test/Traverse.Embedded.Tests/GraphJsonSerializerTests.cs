using System.Text.Json;
using Traverse.Tests.Helpers;
using Xunit;

namespace Traverse.Tests;

public sealed class GraphJsonSerializerTests
{
    [Fact]
    public void Serialize_nodes_and_edges()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        tx.Query("CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})").Dispose();
        using var r = tx.Query("MATCH (a)-[e]->(b) RETURN a, e, b");

        var json = GraphJsonSerializer.Serialize(r);

        using var doc = JsonDocument.Parse(json);
        var root = doc.RootElement;

        Assert.True(root.TryGetProperty("nodes", out var nodes));
        Assert.True(root.TryGetProperty("edges", out var edges));

        Assert.Equal(2, nodes.GetArrayLength());
        Assert.Equal(1, edges.GetArrayLength());

        // Check that edges have source/target referencing node keys.
        var edge = edges[0];
        Assert.True(edge.TryGetProperty("source", out _));
        Assert.True(edge.TryGetProperty("target", out _));
        Assert.True(edge.TryGetProperty("attributes", out var attrs));
        Assert.Equal("KNOWS", attrs.GetProperty("label").GetString());

        tx.Abort();
    }

    [Fact]
    public void Serialize_deduplicates_nodes()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        tx.Query("CREATE (a:X {v: 1})-[:R]->(b:X {v: 2}), (a)-[:R]->(b)").Dispose();
        using var r = tx.Query("MATCH (a)-[e]->(b) RETURN a, e, b");

        var json = GraphJsonSerializer.Serialize(r);

        using var doc = JsonDocument.Parse(json);
        var nodes = doc.RootElement.GetProperty("nodes");

        // Two rows returned but only 2 unique nodes
        Assert.Equal(2, nodes.GetArrayLength());

        tx.Abort();
    }

    [Fact]
    public void Serialize_empty_result_produces_empty_arrays()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("MATCH (n:Nothing) RETURN n");

        var json = GraphJsonSerializer.Serialize(r);

        using var doc = JsonDocument.Parse(json);
        Assert.Equal(0, doc.RootElement.GetProperty("nodes").GetArrayLength());
        Assert.Equal(0, doc.RootElement.GetProperty("edges").GetArrayLength());

        tx.Abort();
    }

    [Fact]
    public void Serialize_with_scalar_columns_ignores_non_graph()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        tx.Query("CREATE (a:Foo {val: 42})").Dispose();
        using var r = tx.Query("MATCH (n:Foo) RETURN n, n.val AS v");

        var json = GraphJsonSerializer.Serialize(r);

        using var doc = JsonDocument.Parse(json);
        // Only the node is serialized, the scalar is ignored
        Assert.Equal(1, doc.RootElement.GetProperty("nodes").GetArrayLength());
        Assert.Equal(0, doc.RootElement.GetProperty("edges").GetArrayLength());

        tx.Abort();
    }

    [Fact]
    public void Serialize_node_attributes_include_properties()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        tx.Query("CREATE (a:Person {name: 'Eve', age: 30})").Dispose();
        using var r = tx.Query("MATCH (n:Person {name: 'Eve'}) RETURN n");

        var json = GraphJsonSerializer.Serialize(r);

        using var doc = JsonDocument.Parse(json);
        var nodeAttrs = doc.RootElement.GetProperty("nodes")[0].GetProperty("attributes");

        Assert.Equal("Person", nodeAttrs.GetProperty("label").GetString());
        Assert.Equal("Eve", nodeAttrs.GetProperty("name").GetString());

        tx.Abort();
    }

    [Fact]
    public void Serialize_path_result_extracts_nodes_and_edges()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        tx.Query("CREATE (a:P {n: 'A'})-[:R]->(b:P {n: 'B'})-[:R]->(c:P {n: 'C'})").Dispose();
        using var r = tx.Query("MATCH p = (a:P {n: 'A'})-[:R*]->(c:P {n: 'C'}) RETURN p");

        var json = GraphJsonSerializer.Serialize(r);

        using var doc = JsonDocument.Parse(json);
        var root = doc.RootElement;
        var nodes = root.GetProperty("nodes");
        var edges = root.GetProperty("edges");

        // Path A->B->C should yield 3 nodes and 2 edges
        Assert.Equal(3, nodes.GetArrayLength());
        Assert.Equal(2, edges.GetArrayLength());

        tx.Abort();
    }

    [Fact]
    public void Serialize_edge_attributes_include_type_and_properties()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        tx.Query("CREATE (a:X)-[:LINK {weight: 5}]->(b:X)").Dispose();
        using var r = tx.Query("MATCH (a)-[e]->(b) RETURN a, e, b");

        var json = GraphJsonSerializer.Serialize(r);

        using var doc = JsonDocument.Parse(json);
        var edge = doc.RootElement.GetProperty("edges")[0];
        var attrs = edge.GetProperty("attributes");

        Assert.Equal("LINK", attrs.GetProperty("label").GetString());

        tx.Abort();
    }
}
