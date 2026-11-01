using Traverse.Tests.Helpers;
using Xunit;

namespace Traverse.Tests;

/// <summary>
/// Tests that verify correct TvErrorCode propagation for various error scenarios.
/// </summary>
[Collection("DatabaseOpen")]
public sealed class ErrorCodeTests
{
    [Fact]
    public void Syntax_error_returns_Syntax_code()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();

        var ex = Assert.Throws<TraverseException>(() => tx.Query("NOT VALID CYPHER"));
        Assert.Equal(TvErrorCode.Syntax, ex.ErrorCode);

        tx.Abort();
    }

    [Fact]
    public void Unknown_function_returns_error()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();

        // Unknown functions are caught at compile time
        var ex = Assert.Throws<TraverseException>(
            () => tx.Query("RETURN no_such_function(1)"));
        Assert.Equal(TvErrorCode.Syntax, ex.ErrorCode);

        tx.Abort();
    }

    [Fact]
    public void Open_empty_path_returns_error()
    {
        var ex = Assert.Throws<TraverseException>(() => Database.Open(""));
        Assert.NotEqual(TvErrorCode.Ok, ex.ErrorCode);
    }

    [Fact]
    public void Type_error_returns_Type_code()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();

        // sum() on non-numeric values triggers TypeError at runtime
        tx.Query("CREATE (n:T {v: 'text'})").Dispose();
        var ex = Assert.Throws<TraverseException>(
            () => tx.Query("MATCH (n:T) RETURN sum(n.v)"));
        Assert.Equal(TvErrorCode.Type, ex.ErrorCode);

        tx.Abort();
    }

    [Fact]
    public void Transaction_already_finished_returns_Transaction_code()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        tx.Query("RETURN 1").Dispose();
        tx.Commit();

        var ex = Assert.Throws<TraverseException>(() => tx.Commit());
        Assert.Equal(TvErrorCode.Transaction, ex.ErrorCode);
    }

    [Fact]
    public void Abort_after_commit_returns_Transaction_code()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        tx.Query("RETURN 1").Dispose();
        tx.Commit();

        var ex = Assert.Throws<TraverseException>(() => tx.Abort());
        Assert.Equal(TvErrorCode.Transaction, ex.ErrorCode);
    }

    [Fact]
    public void BulkWriter_finished_returns_InvalidArg_code()
    {
        using var tdb = new TestDb();
        using var bw = tdb.Db.BeginBulkWrite();
        bw.Commit();

        var ex = Assert.Throws<TraverseException>(() => bw.CreateNode(["X"]));
        Assert.Equal(TvErrorCode.InvalidArg, ex.ErrorCode);
    }

    [Fact]
    public void Locked_database_returns_Locked_code()
    {
        var dir = Path.Combine(
            Path.GetTempPath(), "traverse_tests", Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(dir);
        var path = Path.Combine(dir, "locked.tvdb");

        try
        {
            using var db1 = Database.Open(path);

            // Second open on the same file should fail with Locked
            var ex = Assert.Throws<TraverseException>(() => Database.Open(path));
            Assert.Equal(TvErrorCode.Locked, ex.ErrorCode);
        }
        finally
        {
            try { Directory.Delete(dir, recursive: true); } catch { }
        }
    }

    [Fact]
    public void Query_after_dispose_throws_ObjectDisposedException()
    {
        var tdb = new TestDb();
        var db = tdb.Db;
        tdb.Dispose();

        Assert.Throws<ObjectDisposedException>(() => db.BeginTransaction());
    }
}
