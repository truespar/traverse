using Traverse.Tests.Helpers;
using Xunit;

namespace Traverse.Tests;

/// <summary>
/// Tests for index and constraint operations via Cypher.
/// </summary>
[Collection("DatabaseOpen")]
public sealed class IndexTests
{
    [Fact]
    public void Create_index_and_use_in_lookup()
    {
        using var tdb = new TestDb();

        // Seed data
        using (var tx = tdb.Db.BeginTransaction())
        {
            for (int i = 0; i < 100; i++)
                tx.Query($"CREATE (n:User {{id: {i}, name: 'user{i}'}})").Dispose();
            tx.Commit();
        }

        // Create index
        using (var tx = tdb.Db.BeginTransaction())
        {
            tx.Query("CREATE INDEX idx_user_id FOR (n:User) ON (n.id)").Dispose();
            tx.Commit();
        }

        // Query using indexed property
        using (var tx = tdb.Db.BeginTransaction())
        {
            using var r = tx.Query("MATCH (n:User {id: 42}) RETURN n.name");
            var rows = r.ToList();
            Assert.Single(rows);
            Assert.Equal("user42", rows[0].GetString(0));
            tx.Abort();
        }
    }

    [Fact]
    public void Drop_index()
    {
        using var tdb = new TestDb();

        using (var tx = tdb.Db.BeginTransaction())
        {
            tx.Query("CREATE (n:Item {code: 'A'})").Dispose();
            tx.Query("CREATE INDEX idx_item_code FOR (n:Item) ON (n.code)").Dispose();
            tx.Commit();
        }

        // Drop the index - should not affect data
        using (var tx = tdb.Db.BeginTransaction())
        {
            tx.Query("DROP INDEX idx_item_code").Dispose();
            tx.Commit();
        }

        // Data still queryable (full scan)
        using (var tx = tdb.Db.BeginTransaction())
        {
            using var r = tx.Query("MATCH (n:Item) RETURN n.code");
            var rows = r.ToList();
            Assert.Single(rows);
            Assert.Equal("A", rows[0].GetString(0));
            tx.Abort();
        }
    }

    [Fact]
    public void Create_unique_constraint()
    {
        using var tdb = new TestDb();

        using (var tx = tdb.Db.BeginTransaction())
        {
            tx.Query("CREATE CONSTRAINT uniq_email FOR (n:User) REQUIRE n.email IS UNIQUE").Dispose();
            tx.Query("CREATE (n:User {email: 'alice@test.com'})").Dispose();
            tx.Commit();
        }

        // Inserting a duplicate should fail
        using (var tx = tdb.Db.BeginTransaction())
        {
            var ex = Assert.Throws<TraverseException>(
                () => tx.Query("CREATE (n:User {email: 'alice@test.com'})"));
            tx.Abort();
        }
    }

    [Fact]
    public void Index_survives_flush_and_requery()
    {
        var dir = Path.Combine(
            Path.GetTempPath(), "traverse_tests", Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(dir);
        var path = Path.Combine(dir, "idx_test.tvdb");

        try
        {
            using (var db = Database.Open(path))
            {
                using (var tx = db.BeginTransaction())
                {
                    tx.Query("CREATE INDEX idx_name FOR (n:Person) ON (n.name)").Dispose();
                    for (int i = 0; i < 50; i++)
                        tx.Query($"CREATE (n:Person {{name: 'person{i}'}})").Dispose();
                    tx.Commit();
                }
                db.Flush();
            }

            // Reopen - index should be persisted
            using (var db2 = Database.Open(path))
            {
                using var tx = db2.BeginTransaction();
                using var r = tx.Query("MATCH (n:Person {name: 'person25'}) RETURN n.name");
                var rows = r.ToList();
                Assert.Single(rows);
                Assert.Equal("person25", rows[0].GetString(0));
                tx.Abort();
            }
        }
        finally
        {
            try { Directory.Delete(dir, recursive: true); } catch { }
        }
    }
}
