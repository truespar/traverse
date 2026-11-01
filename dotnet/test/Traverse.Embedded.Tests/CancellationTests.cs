using System.Diagnostics;
using Traverse.Tests.Helpers;
using Xunit;

namespace Traverse.Tests;

/// <summary>
/// Stopping statements: cancellation tokens, <see cref="Transaction.Cancel"/>,
/// <see cref="Database.StatementTimeout"/>, TERMINATE TRANSACTIONS; and a statement that fails
/// after writing failing its transaction.
/// </summary>
public sealed class CancellationTests
{
    // a three-way cross product over 2,000 nodes: minutes of work unless stopped
    private const string Long = "MATCH (a:Big), (b:Big), (c:Big) RETURN count(*)";

    private static void Seed(Database db)
    {
        using var tx = db.BeginTransaction();
        tx.Query("UNWIND range(1, 2000) AS i CREATE (:Big {i: i})").Dispose();
        tx.Commit();
    }

    [Fact]
    public void A_cancelled_token_stops_the_query()
    {
        using var tdb = new TestDb();
        Seed(tdb.Db);
        using var tx = tdb.Db.BeginReadTransaction();
        using var cts = new CancellationTokenSource(TimeSpan.FromMilliseconds(200));
        var watch = Stopwatch.StartNew();
        Assert.ThrowsAny<OperationCanceledException>(() => tx.Query(Long, cts.Token));
        Assert.True(watch.Elapsed < TimeSpan.FromSeconds(20), $"took {watch.Elapsed}");
        // the transaction carries on: a read changed nothing
        using var r = tx.Query("RETURN 1 AS n");
        Assert.Equal(1L, r.ToList()[0].GetInt64(0));
    }

    [Fact]
    public async Task Cancel_from_another_thread_stops_the_query()
    {
        using var tdb = new TestDb();
        Seed(tdb.Db);
        using var tx = tdb.Db.BeginReadTransaction();
        var canceller = Task.Run(async () =>
        {
            await Task.Delay(200);
            tx.Cancel();
        });
        var ex = Assert.Throws<TraverseException>(() => tx.Query(Long));
        Assert.Equal(TvErrorCode.Interrupted, ex.ErrorCode);
        await canceller;
    }

    [Fact]
    public void A_statement_timeout_stops_the_query()
    {
        using var tdb = new TestDb();
        Seed(tdb.Db);
        tdb.Db.StatementTimeout = TimeSpan.FromMilliseconds(200);
        Assert.Equal(TimeSpan.FromMilliseconds(200), tdb.Db.StatementTimeout);
        using var tx = tdb.Db.BeginReadTransaction();
        var ex = Assert.Throws<TraverseException>(() => tx.Query(Long));
        Assert.Equal(TvErrorCode.Interrupted, ex.ErrorCode);
        Assert.Contains("timed out", ex.Message);
    }

    [Fact]
    public void Terminate_transactions_ends_another_transaction()
    {
        // a database of its own: tests run in parallel against one process-wide registry, and
        // SHOW TRANSACTIONS lists a database by its file name
        var dir = System.IO.Path.Combine(System.IO.Path.GetTempPath(), "traverse_tests", Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(dir);
        var name = "terminate_" + Guid.NewGuid().ToString("N");
        try
        {
            using var db = Database.Open(System.IO.Path.Combine(dir, name + ".tvdb"));
            using var victim = db.BeginTransaction();
            victim.Query("CREATE (:Pending)").Dispose();
            using (var admin = db.BeginReadTransaction())
            {
                using var r = admin.Query(
                    "SHOW TRANSACTIONS YIELD transactionId AS id, database, protocol, currentQuery " +
                    $"WHERE database = '{name}' AND protocol = 'embedded' AND currentQuery = '' " +
                    "TERMINATE TRANSACTIONS id YIELD message RETURN message");
                var rows = r.ToList();
                Assert.Single(rows);
                Assert.Equal("Transaction terminated.", rows[0].GetString(0));
            }
            var ex = Assert.Throws<TraverseException>(() => victim.Query("RETURN 1"));
            Assert.Equal(TvErrorCode.Interrupted, ex.ErrorCode);
            Assert.Throws<TraverseException>(() => victim.Commit());
            using var check = db.BeginReadTransaction();
            using var count = check.Query("MATCH (n:Pending) RETURN count(n)");
            Assert.Equal(0L, count.ToList()[0].GetInt64(0));
        }
        finally
        {
            try { Directory.Delete(dir, recursive: true); } catch { /* best effort */ }
        }
    }

    [Fact]
    public void A_statement_that_fails_after_writing_fails_the_transaction()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        // fails before writing: the transaction carries on
        Assert.Throws<TraverseException>(() => tx.Query("CREATE (:Kept"));
        tx.Query("CREATE (:Kept)").Dispose();
        // writes, then fails
        Assert.Throws<TraverseException>(() => tx.Query("CREATE (:Partial) WITH 1 AS one RETURN 10 / 0"));
        var later = Assert.Throws<TraverseException>(() => tx.Query("RETURN 1"));
        Assert.Equal(TvErrorCode.Transaction, later.ErrorCode);
        var commit = Assert.Throws<TraverseException>(() => tx.Commit());
        Assert.Equal(TvErrorCode.Transaction, commit.ErrorCode);
        using var check = tdb.Db.BeginReadTransaction();
        using var count = check.Query("MATCH (n) RETURN count(n)");
        Assert.Equal(0L, count.ToList()[0].GetInt64(0));
    }
}
