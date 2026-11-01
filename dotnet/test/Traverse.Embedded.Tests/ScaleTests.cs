using Traverse.Tests.Helpers;
using Xunit;

namespace Traverse.Tests;

/// <summary>
/// Scale tests - verify the .NET wrapper handles realistic data volumes
/// without marshalling issues or memory problems.
/// </summary>
public sealed class ScaleTests
{
    [Fact]
    public void BulkInsert_10k_nodes_with_properties()
    {
        using var tdb = new TestDb();
        const int count = 10_000;

        using (var bw = tdb.Db.BeginBulkWrite())
        {
            for (int i = 0; i < count; i++)
            {
                bw.CreateNode(
                    ["User"],
                    ["id", "name", "age", "active"],
                    [i.ToString(), $"user_{i}", (20 + i % 50).ToString(), (i % 2 == 0).ToString()]);
            }
            bw.Commit();
        }

        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("MATCH (n:User) RETURN count(n)");
        Assert.Equal((long)count, r.ToList()[0].GetInt64(0));
        tx.Abort();
    }

    [Fact]
    public void BulkInsert_10k_nodes_and_edges()
    {
        using var tdb = new TestDb();
        const int nodeCount = 10_000;
        const int edgeCount = 20_000;

        long[] ids;
        using (var bw = tdb.Db.BeginBulkWrite())
        {
            ids = new long[nodeCount];
            for (int i = 0; i < nodeCount; i++)
            {
                ids[i] = bw.CreateNode(["N"], ["idx"], [i.ToString()]);
            }

            // Create random-ish edges (deterministic pattern)
            var rng = new Random(42);
            for (int i = 0; i < edgeCount; i++)
            {
                var src = ids[rng.Next(nodeCount)];
                var tgt = ids[rng.Next(nodeCount)];
                bw.CreateEdge(src, tgt, "LINKS");
            }
            bw.Commit();
        }

        using var tx = tdb.Db.BeginTransaction();

        using var rn = tx.Query("MATCH (n:N) RETURN count(n)");
        Assert.Equal((long)nodeCount, rn.ToList()[0].GetInt64(0));

        using var re = tx.Query("MATCH ()-[e:LINKS]->() RETURN count(e)");
        Assert.Equal((long)edgeCount, re.ToList()[0].GetInt64(0));

        tx.Abort();
    }

    [Fact]
    public void Transaction_insert_1k_nodes_with_index()
    {
        using var tdb = new TestDb();
        const int count = 1_000;

        // Create index first
        using (var tx = tdb.Db.BeginTransaction())
        {
            tx.Query("CREATE INDEX idx_item_id FOR (n:Item) ON (n.id)").Dispose();
            tx.Commit();
        }

        // Insert via transactions
        using (var tx = tdb.Db.BeginTransaction())
        {
            for (int i = 0; i < count; i++)
                tx.Query($"CREATE (n:Item {{id: {i}, data: 'payload_{i}'}})").Dispose();
            tx.Commit();
        }

        // Verify count
        using (var tx = tdb.Db.BeginTransaction())
        {
            using var r = tx.Query("MATCH (n:Item) RETURN count(n)");
            Assert.Equal((long)count, r.ToList()[0].GetInt64(0));
            tx.Abort();
        }

        // Verify index lookup works for arbitrary element
        using (var tx = tdb.Db.BeginTransaction())
        {
            using var r = tx.Query("MATCH (n:Item {id: 777}) RETURN n.data");
            var rows = r.ToList();
            Assert.Single(rows);
            Assert.Equal("payload_777", rows[0].GetString(0));
            tx.Abort();
        }
    }

    [Fact]
    public void Large_result_set_enumeration()
    {
        using var tdb = new TestDb();
        const int count = 5_000;

        using (var bw = tdb.Db.BeginBulkWrite())
        {
            for (int i = 0; i < count; i++)
                bw.CreateNode(["Row"], ["val"], [i.ToString()]);
            bw.Commit();
        }

        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("MATCH (n:Row) RETURN n.val ORDER BY n.val");

        // Enumerate all rows - tests cursor + marshalling under volume
        long rowCount = 0;
        foreach (var row in r)
        {
            _ = row.GetString(0);
            rowCount++;
        }
        Assert.Equal(count, rowCount);

        tx.Abort();
    }

    [Fact]
    public void BulkInsert_complex_graph_with_multiple_labels_and_edge_types()
    {
        using var tdb = new TestDb();

        using (var bw = tdb.Db.BeginBulkWrite())
        {
            // Create a small social network
            var users = new long[100];
            var posts = new long[200];

            for (int i = 0; i < 100; i++)
                users[i] = bw.CreateNode(
                    ["User", "Active"],
                    ["name", "age"],
                    [$"user_{i}", (18 + i % 60).ToString()]);

            for (int i = 0; i < 200; i++)
                posts[i] = bw.CreateNode(
                    ["Post"],
                    ["title", "likes"],
                    [$"Post title {i}", (i * 3).ToString()]);

            // FOLLOWS edges (user -> user)
            var rng = new Random(123);
            for (int i = 0; i < 300; i++)
                bw.CreateEdge(users[rng.Next(100)], users[rng.Next(100)], "FOLLOWS");

            // CREATED edges (user -> post)
            for (int i = 0; i < 200; i++)
                bw.CreateEdge(users[i % 100], posts[i], "CREATED");

            // LIKED edges (user -> post)
            for (int i = 0; i < 500; i++)
                bw.CreateEdge(users[rng.Next(100)], posts[rng.Next(200)], "LIKED");

            bw.Commit();
        }

        using var tx = tdb.Db.BeginTransaction();

        // Verify structure
        using var ru = tx.Query("MATCH (n:User) RETURN count(n)");
        Assert.Equal(100L, ru.ToList()[0].GetInt64(0));

        using var rp = tx.Query("MATCH (n:Post) RETURN count(n)");
        Assert.Equal(200L, rp.ToList()[0].GetInt64(0));

        using var rf = tx.Query("MATCH ()-[e:FOLLOWS]->() RETURN count(e)");
        Assert.Equal(300L, rf.ToList()[0].GetInt64(0));

        using var rc = tx.Query("MATCH ()-[e:CREATED]->() RETURN count(e)");
        Assert.Equal(200L, rc.ToList()[0].GetInt64(0));

        using var rl = tx.Query("MATCH ()-[e:LIKED]->() RETURN count(e)");
        Assert.Equal(500L, rl.ToList()[0].GetInt64(0));

        // Multi-hop query: users who follow someone who created a post
        using var rq = tx.Query(
            "MATCH (u:User)-[:FOLLOWS]->(f:User)-[:CREATED]->(p:Post) " +
            "RETURN count(DISTINCT u)");
        var followers = rq.ToList()[0].GetInt64(0);
        Assert.True(followers > 0, "Expected at least one user in follow->create chain");

        tx.Abort();
    }
}
