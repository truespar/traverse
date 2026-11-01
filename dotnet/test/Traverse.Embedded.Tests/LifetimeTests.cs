using System.Runtime.CompilerServices;
using Traverse.Tests.Helpers;
using Xunit;

namespace Traverse.Tests;

/// <summary>
/// Handle lifetimes and threading: disposal order, read transactions, busy timeout,
/// cross-thread use.
/// </summary>
[Collection("DatabaseOpen")]
public sealed class LifetimeTests
{
    private static long Count(Transaction tx, string query)
    {
        using var r = tx.Query(query);
        return r.ToList()[0].GetInt64(0);
    }

    [Fact]
    public void Disposing_the_database_rolls_back_open_transactions()
    {
        using var tdb = new TestDb();
        var tx = tdb.Db.BeginTransaction();
        tx.Query("CREATE (:Lost)").Dispose();
        var reader = tdb.Db.BeginReadTransaction();

        tdb.Db.Dispose();

        var ex = Assert.Throws<TraverseException>(() => tx.Query("RETURN 1"));
        Assert.Equal(TvErrorCode.Transaction, ex.ErrorCode);
        Assert.Contains("closed", ex.Message);
        Assert.Throws<TraverseException>(() => tx.Commit());
        reader.Dispose();
        tx.Dispose();

        // the file was released and holds nothing
        using var db = Database.Open(tdb.Path);
        using var check = db.BeginReadTransaction();
        Assert.Equal(0L, Count(check, "MATCH (n) RETURN count(n)"));
    }

    [Fact]
    public void Read_transactions_run_alongside_the_writer_on_their_snapshot()
    {
        using var tdb = new TestDb();
        using var writer = tdb.Db.BeginTransaction();
        writer.Query("CREATE (:N)").Dispose();

        using var reader = tdb.Db.BeginReadTransaction();
        Assert.True(reader.IsReadOnly);
        Assert.Equal(0L, Count(reader, "MATCH (n) RETURN count(n)"));
        Assert.Throws<TraverseException>(() => reader.Query("CREATE (:M)"));

        writer.Commit();
        Assert.Equal(0L, Count(reader, "MATCH (n) RETURN count(n)"));
        reader.Commit();

        using var after = tdb.Db.BeginReadTransaction();
        Assert.Equal(1L, Count(after, "MATCH (n) RETURN count(n)"));
    }

    [Fact]
    public void A_second_writer_throws_busy_after_the_timeout()
    {
        using var tdb = new TestDb();
        tdb.Db.BusyTimeout = TimeSpan.FromMilliseconds(20);
        Assert.Equal(TimeSpan.FromMilliseconds(20), tdb.Db.BusyTimeout);

        using var writer = tdb.Db.BeginTransaction();
        var ex = Assert.Throws<TraverseException>(() => tdb.Db.BeginTransaction());
        Assert.Equal(TvErrorCode.Busy, ex.ErrorCode);
        Assert.Equal(TvErrorCode.Busy,
            Assert.Throws<TraverseException>(() => tdb.Db.BeginBulkWrite()).ErrorCode);
        Assert.Equal(TvErrorCode.Busy,
            Assert.Throws<TraverseException>(() => tdb.Db.Flush()).ErrorCode);

        writer.Abort();
        using var next = tdb.Db.BeginTransaction();
        next.Commit();
        Assert.Throws<ArgumentOutOfRangeException>(() => tdb.Db.BusyTimeout = TimeSpan.FromSeconds(-2));
        tdb.Db.BusyTimeout = Timeout.InfiniteTimeSpan;
    }

    [Fact]
    public async Task A_transaction_finishes_on_another_thread()
    {
        using var tdb = new TestDb();
        var tx = tdb.Db.BeginTransaction();
        tx.Query("CREATE (:N)").Dispose();
        await Task.Run(() => tx.Commit());

        using var reader = tdb.Db.BeginReadTransaction();
        Assert.Equal(1L, Count(reader, "MATCH (n) RETURN count(n)"));
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    private static void AbandonWriter(Database db)
    {
        var tx = db.BeginTransaction();
        tx.Query("CREATE (:Abandoned)").Dispose();
        // never committed or disposed
    }

    [Fact]
    public void An_abandoned_writer_is_rolled_back_by_the_finalizer()
    {
        using var tdb = new TestDb();
        tdb.Db.BusyTimeout = TimeSpan.FromSeconds(10);
        AbandonWriter(tdb.Db);
        GC.Collect();
        GC.WaitForPendingFinalizers();

        // the finalizer thread released the writer lock and rolled the writes back
        using var tx = tdb.Db.BeginTransaction();
        Assert.Equal(0L, Count(tx, "MATCH (n:Abandoned) RETURN count(n)"));
        tx.Abort();
    }

    [Fact]
    public async Task Concurrent_readers_and_writers_stay_consistent()
    {
        using var tdb = new TestDb();
        tdb.Db.BusyTimeout = Timeout.InfiniteTimeSpan;
        var writers = Enumerable.Range(0, 4).Select(_ => Task.Run(() =>
        {
            for (int i = 0; i < 25; i++)
            {
                using var tx = tdb.Db.BeginTransaction();
                tx.Query("CREATE (:N)").Dispose();
                tx.Commit();
            }
        }));
        var readers = Enumerable.Range(0, 4).Select(_ => Task.Run(() =>
        {
            for (int i = 0; i < 25; i++)
            {
                using var tx = tdb.Db.BeginReadTransaction();
                var a = Count(tx, "MATCH (n) RETURN count(n)");
                var b = Count(tx, "MATCH (n) RETURN count(n)");
                Assert.Equal(a, b); // a snapshot doesn't move
            }
        }));
        await Task.WhenAll(writers.Concat(readers));

        using var check = tdb.Db.BeginReadTransaction();
        Assert.Equal(100L, Count(check, "MATCH (n) RETURN count(n)"));
    }
}
