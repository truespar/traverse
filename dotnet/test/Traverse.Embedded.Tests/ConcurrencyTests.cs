using Traverse.Tests.Helpers;
using Xunit;

namespace Traverse.Tests;

/// <summary>
/// Tests for concurrent operations and MVCC behavior.
/// </summary>
[Collection("DatabaseOpen")]
public sealed class ConcurrencyTests
{
    [Fact]
    public void Multiple_sequential_transactions()
    {
        using var tdb = new TestDb();

        // Write in separate transactions
        for (int i = 0; i < 10; i++)
        {
            using var tx = tdb.Db.BeginTransaction();
            tx.Query($"CREATE (n:Seq {{idx: {i}}})").Dispose();
            tx.Commit();
        }

        // Verify all committed
        using var rtx = tdb.Db.BeginTransaction();
        using var r = rtx.Query("MATCH (n:Seq) RETURN count(n)");
        Assert.Equal(10L, r.ToList()[0].GetInt64(0));
        rtx.Abort();
    }

    [Fact]
    public void Transaction_after_bulk_write()
    {
        using var tdb = new TestDb();

        // Bulk write first
        using (var bw = tdb.Db.BeginBulkWrite())
        {
            for (int i = 0; i < 100; i++)
                bw.CreateNode(["Bulk"], ["idx"], [i.ToString()]);
            bw.Commit();
        }

        // Then transactional write
        using (var tx = tdb.Db.BeginTransaction())
        {
            tx.Query("CREATE (n:Tx {val: 'after_bulk'})").Dispose();
            tx.Commit();
        }

        // Verify both are visible
        using (var tx = tdb.Db.BeginTransaction())
        {
            using var r1 = tx.Query("MATCH (n:Bulk) RETURN count(n)");
            Assert.Equal(100L, r1.ToList()[0].GetInt64(0));

            using var r2 = tx.Query("MATCH (n:Tx) RETURN n.val");
            Assert.Equal("after_bulk", r2.ToList()[0].GetString(0));
            tx.Abort();
        }
    }

    [Fact]
    public void Bulk_write_after_transaction()
    {
        using var tdb = new TestDb();

        // Transactional write first
        using (var tx = tdb.Db.BeginTransaction())
        {
            tx.Query("CREATE (n:Pre {val: 'before_bulk'})").Dispose();
            tx.Commit();
        }

        // Then bulk write
        using (var bw = tdb.Db.BeginBulkWrite())
        {
            bw.CreateNode(["Post"], ["val"], ["after_tx"]);
            bw.Commit();
        }

        // Both visible
        using (var tx = tdb.Db.BeginTransaction())
        {
            using var r1 = tx.Query("MATCH (n:Pre) RETURN n.val");
            Assert.Equal("before_bulk", r1.ToList()[0].GetString(0));

            using var r2 = tx.Query("MATCH (n:Post) RETURN n.val");
            Assert.Equal("after_tx", r2.ToList()[0].GetString(0));
            tx.Abort();
        }
    }

    [Fact]
    public void Multiple_queries_in_single_transaction()
    {
        using var tdb = new TestDb();

        using var tx = tdb.Db.BeginTransaction();

        tx.Query("CREATE (a:Person {name: 'Alice'})").Dispose();
        tx.Query("CREATE (b:Person {name: 'Bob'})").Dispose();
        tx.Query("MATCH (a:Person {name: 'Alice'}), (b:Person {name: 'Bob'}) CREATE (a)-[:KNOWS]->(b)").Dispose();

        using var r = tx.Query("MATCH (a)-[:KNOWS]->(b) RETURN a.name, b.name");
        var rows = r.ToList();
        Assert.Single(rows);
        Assert.Equal("Alice", rows[0].GetString(0));
        Assert.Equal("Bob", rows[0].GetString(1));

        tx.Commit();
    }

    [Fact]
    public void Flush_persists_data_across_reopen()
    {
        var dir = Path.Combine(
            Path.GetTempPath(), "traverse_tests", Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(dir);
        var path = Path.Combine(dir, "persist.tvdb");

        try
        {
            using (var db = Database.Open(path))
            {
                using (var tx = db.BeginTransaction())
                {
                    tx.Query("CREATE (n:Persist {val: 'survived'})").Dispose();
                    tx.Commit();
                }
                db.Flush();
            }

            using (var db2 = Database.Open(path))
            {
                using var tx = db2.BeginTransaction();
                using var r = tx.Query("MATCH (n:Persist) RETURN n.val");
                var rows = r.ToList();
                Assert.Single(rows);
                Assert.Equal("survived", rows[0].GetString(0));
                tx.Abort();
            }
        }
        finally
        {
            try { Directory.Delete(dir, recursive: true); } catch { }
        }
    }
}
