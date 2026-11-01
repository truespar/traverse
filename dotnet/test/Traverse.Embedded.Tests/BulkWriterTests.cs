using Traverse.Tests.Helpers;
using Xunit;

namespace Traverse.Tests;

public sealed class BulkWriterTests
{
    [Fact]
    public void CreateNode_and_commit_persists_data()
    {
        using var tdb = new TestDb();

        using (var bw = tdb.Db.BeginBulkWrite())
        {
            bw.CreateNode(["Person"], ["name"], ["Alice"]);
            bw.Commit();
        }

        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("MATCH (n:Person) RETURN n.name");
        var rows = r.ToList();
        Assert.Single(rows);
        Assert.Equal("Alice", rows[0].GetString(0));
        tx.Abort();
    }

    [Fact]
    public void CreateNode_with_multiple_labels()
    {
        using var tdb = new TestDb();

        using (var bw = tdb.Db.BeginBulkWrite())
        {
            bw.CreateNode(["Person", "Employee"], ["name"], ["Bob"]);
            bw.Commit();
        }

        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("MATCH (n:Person:Employee) RETURN n.name");
        var rows = r.ToList();
        Assert.Single(rows);
        Assert.Equal("Bob", rows[0].GetString(0));
        tx.Abort();
    }

    [Fact]
    public void CreateNode_with_typed_properties()
    {
        using var tdb = new TestDb();

        using (var bw = tdb.Db.BeginBulkWrite())
        {
            // BulkWriter auto-infers types: "42" -> Int, "3.14" -> Float, "true" -> Bool
            bw.CreateNode(["Item"],
                ["count", "weight", "active", "label"],
                ["42", "3.14", "true", "widget"]);
            bw.Commit();
        }

        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("MATCH (n:Item) RETURN n.count, n.weight, n.active, n.label");
        var rows = r.ToList();
        Assert.Single(rows);
        Assert.Equal(42L, rows[0].GetInt64(0));
        Assert.Equal(3.14, rows[0].GetDouble(1), precision: 5);
        Assert.True(rows[0].GetBool(2));
        Assert.Equal("widget", rows[0].GetString(3));
        tx.Abort();
    }

    [Fact]
    public void CreateNode_no_properties()
    {
        using var tdb = new TestDb();

        using (var bw = tdb.Db.BeginBulkWrite())
        {
            bw.CreateNode(["Empty"]);
            bw.Commit();
        }

        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("MATCH (n:Empty) RETURN count(n)");
        var rows = r.ToList();
        Assert.Equal(1L, rows[0].GetInt64(0));
        tx.Abort();
    }

    [Fact]
    public void CreateEdge_links_nodes()
    {
        using var tdb = new TestDb();

        using (var bw = tdb.Db.BeginBulkWrite())
        {
            var alice = bw.CreateNode(["Person"], ["name"], ["Alice"]);
            var bob = bw.CreateNode(["Person"], ["name"], ["Bob"]);
            bw.CreateEdge(alice, bob, "KNOWS", ["since"], ["2020"]);
            bw.Commit();
        }

        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query(
            "MATCH (a:Person)-[e:KNOWS]->(b:Person) RETURN a.name, b.name, e.since");
        var rows = r.ToList();
        Assert.Single(rows);
        Assert.Equal("Alice", rows[0].GetString(0));
        Assert.Equal("Bob", rows[0].GetString(1));
        Assert.Equal(2020L, rows[0].GetInt64(2));
        tx.Abort();
    }

    [Fact]
    public void CreateEdge_no_properties()
    {
        using var tdb = new TestDb();

        using (var bw = tdb.Db.BeginBulkWrite())
        {
            var a = bw.CreateNode(["X"]);
            var b = bw.CreateNode(["X"]);
            bw.CreateEdge(a, b, "LINKS");
            bw.Commit();
        }

        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("MATCH ()-[e:LINKS]->() RETURN count(e)");
        var rows = r.ToList();
        Assert.Equal(1L, rows[0].GetInt64(0));
        tx.Abort();
    }

    [Fact]
    public void Abort_rolls_the_load_back()
    {
        using var tdb = new TestDb();

        using (var bw = tdb.Db.BeginBulkWrite())
        {
            bw.CreateNode(["Ghost"], ["name"], ["Casper"]);
            bw.Abort();
        }

        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("MATCH (n:Ghost) RETURN count(n)");
        Assert.Equal(0L, r.ToList()[0].GetInt64(0));
        tx.Abort();
    }

    [Fact]
    public void Dispose_without_commit_rolls_the_load_back()
    {
        using var tdb = new TestDb();

        using (var bw = tdb.Db.BeginBulkWrite())
        {
            bw.CreateNode(["Phantom"]);
            // no commit or abort: dispose rolls back
        }

        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("MATCH (n:Phantom) RETURN count(n)");
        Assert.Equal(0L, r.ToList()[0].GetInt64(0));
        tx.Abort();
    }

    [Fact]
    public void Load_is_invisible_to_readers_until_commit()
    {
        using var tdb = new TestDb();

        using var bw = tdb.Db.BeginBulkWrite();
        for (int i = 0; i < 100; i++)
            bw.CreateNode(["Item"], ["i"], [i.ToString()]);

        using (var reader = tdb.Db.BeginReadTransaction())
        using (var r = reader.Query("MATCH (n:Item) RETURN count(n)"))
            Assert.Equal(0L, r.ToList()[0].GetInt64(0));

        bw.Commit();

        using (var reader = tdb.Db.BeginReadTransaction())
        using (var r = reader.Query("MATCH (n:Item) RETURN count(n)"))
            Assert.Equal(100L, r.ToList()[0].GetInt64(0));
    }

    [Fact]
    public void Abort_releases_write_lock_for_next_writer()
    {
        // After abort, a new bulk writer or transaction should succeed.
        using var tdb = new TestDb();

        using (var bw = tdb.Db.BeginBulkWrite())
        {
            bw.CreateNode(["First"]);
            bw.Abort();
        }

        // Second bulk write should succeed (lock released)
        using (var bw2 = tdb.Db.BeginBulkWrite())
        {
            bw2.CreateNode(["Second"]);
            bw2.Commit();
        }

        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("MATCH (n:Second) RETURN count(n)");
        Assert.Equal(1L, r.ToList()[0].GetInt64(0));
        tx.Abort();
    }

    [Fact]
    public void Commit_after_commit_throws()
    {
        using var tdb = new TestDb();
        using var bw = tdb.Db.BeginBulkWrite();
        bw.CreateNode(["X"]);
        bw.Commit();

        var ex = Assert.Throws<TraverseException>(() => bw.Commit());
        Assert.Equal(TvErrorCode.InvalidArg, ex.ErrorCode);
    }

    [Fact]
    public void CreateNode_after_commit_throws()
    {
        using var tdb = new TestDb();
        using var bw = tdb.Db.BeginBulkWrite();
        bw.Commit();

        var ex = Assert.Throws<TraverseException>(() => bw.CreateNode(["X"]));
        Assert.Equal(TvErrorCode.InvalidArg, ex.ErrorCode);
    }

    [Fact]
    public void CreateEdge_invalid_source_throws()
    {
        using var tdb = new TestDb();
        using var bw = tdb.Db.BeginBulkWrite();
        var a = bw.CreateNode(["X"]);

        Assert.Throws<TraverseException>(() => bw.CreateEdge(a, 999999, "BAD"));

        bw.Abort();
    }

    [Fact]
    public void CreateNode_mismatched_keys_values_throws()
    {
        using var tdb = new TestDb();
        using var bw = tdb.Db.BeginBulkWrite();

        Assert.Throws<ArgumentException>(
            () => bw.CreateNode(["X"], ["key1", "key2"], ["value1"]));

        bw.Abort();
    }

    [Fact]
    public void Bulk_insert_many_nodes()
    {
        using var tdb = new TestDb();
        const int count = 1000;

        using (var bw = tdb.Db.BeginBulkWrite())
        {
            for (int i = 0; i < count; i++)
            {
                bw.CreateNode(["Batch"], ["idx"], [i.ToString()]);
            }
            bw.Commit();
        }

        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("MATCH (n:Batch) RETURN count(n)");
        var rows = r.ToList();
        Assert.Equal((long)count, rows[0].GetInt64(0));
        tx.Abort();
    }

    [Fact]
    public void Bulk_insert_nodes_and_edges_graph_structure()
    {
        using var tdb = new TestDb();

        // Build a small chain: n0 -> n1 -> n2 -> n3 -> n4
        long[] ids;
        using (var bw = tdb.Db.BeginBulkWrite())
        {
            ids = new long[5];
            for (int i = 0; i < 5; i++)
                ids[i] = bw.CreateNode(["Chain"], ["pos"], [i.ToString()]);

            for (int i = 0; i < 4; i++)
                bw.CreateEdge(ids[i], ids[i + 1], "NEXT");

            bw.Commit();
        }

        using var tx = tdb.Db.BeginTransaction();

        // Verify node count
        using var rn = tx.Query("MATCH (n:Chain) RETURN count(n)");
        Assert.Equal(5L, rn.ToList()[0].GetInt64(0));

        // Verify edge count
        using var re = tx.Query("MATCH ()-[e:NEXT]->() RETURN count(e)");
        Assert.Equal(4L, re.ToList()[0].GetInt64(0));

        // Verify chain traversal from pos=0 to pos=4
        using var rp = tx.Query(
            "MATCH p = (start:Chain {pos: 0})-[:NEXT*]->(end:Chain) " +
            "WHERE NOT (end)-[:NEXT]->() " +
            "RETURN end.pos");
        var path = rp.ToList();
        Assert.Single(path);
        Assert.Equal(4L, path[0].GetInt64(0));

        tx.Abort();
    }
}
