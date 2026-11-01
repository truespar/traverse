using Traverse.Tests.Helpers;
using Xunit;

namespace Traverse.Tests;

public sealed class TransactionTests
{
    [Fact]
    public void Commit_persists_data()
    {
        using var tdb = new TestDb();

        using (var tx = tdb.Db.BeginTransaction())
        {
            using var r = tx.Query("CREATE (n:Person {name: 'Alice'}) RETURN n.name");
            tx.Commit();
        }

        using (var tx = tdb.Db.BeginTransaction())
        {
            using var r = tx.Query("MATCH (n:Person) RETURN n.name");
            var rows = r.ToList();
            Assert.Single(rows);
            Assert.Equal("Alice", rows[0].GetString(0));
            tx.Abort();
        }
    }

    [Fact]
    public void Abort_discards_changes()
    {
        using var tdb = new TestDb();

        using (var tx = tdb.Db.BeginTransaction())
        {
            using var r = tx.Query("CREATE (n:Person {name: 'Bob'})");
            tx.Abort();
        }

        using (var tx = tdb.Db.BeginTransaction())
        {
            using var r = tx.Query("MATCH (n:Person) RETURN n.name");
            Assert.Equal(0, r.RowCount);
            tx.Abort();
        }
    }

    [Fact]
    public void Dispose_auto_aborts_uncommitted_transaction()
    {
        using var tdb = new TestDb();

        using (var tx = tdb.Db.BeginTransaction())
        {
            using var r = tx.Query("CREATE (n:Temp {val: 1})");
            // No commit - dispose should auto-abort.
        }

        using (var tx = tdb.Db.BeginTransaction())
        {
            using var r = tx.Query("MATCH (n:Temp) RETURN n.val");
            Assert.Equal(0, r.RowCount);
            tx.Abort();
        }
    }

    [Fact]
    public void Commit_after_commit_throws()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("RETURN 1");
        tx.Commit();

        var ex = Assert.Throws<TraverseException>(() => tx.Commit());
        Assert.Equal(TvErrorCode.Transaction, ex.ErrorCode);
    }

    [Fact]
    public void Query_with_syntax_error_throws()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();

        var ex = Assert.Throws<TraverseException>(() => tx.Query("NOT VALID CYPHER"));
        Assert.Equal(TvErrorCode.Syntax, ex.ErrorCode);

        tx.Abort();
    }
}
