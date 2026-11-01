using System.Diagnostics;
using Traverse.Tests.Helpers;
using Xunit;

namespace Traverse.Tests;

/// <summary>
/// <see cref="Database.Query(string)"/>: each query a transaction of its own.
/// </summary>
public sealed class AutoCommitTests
{
    private static long Count(Database db, string label)
    {
        using var r = db.Query($"MATCH (n:{label}) RETURN count(n)");
        return r.ToList()[0].GetInt64(0);
    }

    [Fact]
    public void Writes_commit_and_reads_see_them()
    {
        using var tdb = new TestDb();
        tdb.Db.Query("CREATE (:A {v: 1}), (:A {v: 2})").Dispose();
        Assert.Equal(2L, Count(tdb.Db, "A"));
        tdb.Db.QueryGql("INSERT (:G {v: 1})").Dispose();
        using var g = tdb.Db.QueryGql("MATCH (n:G) RETURN n.v AS v");
        Assert.Equal(1L, g.ToList()[0].GetInt64(0));
    }

    [Fact]
    public void A_failed_write_leaves_nothing_behind()
    {
        using var tdb = new TestDb();
        Assert.Throws<TraverseException>(() => tdb.Db.Query("CREATE (:B) WITH 1 AS one RETURN 10 / 0"));
        Assert.Equal(0L, Count(tdb.Db, "B"));
    }

    [Fact]
    public void Call_in_transactions_commits_each_batch()
    {
        using var tdb = new TestDb();
        tdb.Db.Query(
            "UNWIND [1, 0, 2] AS i CALL { WITH i CREATE (:C {v: 10 / i}) } " +
            "IN TRANSACTIONS OF 1 ROW ON ERROR CONTINUE").Dispose();
        Assert.Equal(2L, Count(tdb.Db, "C"));
        // inside a transaction it's refused
        using var tx = tdb.Db.BeginTransaction();
        Assert.Throws<TraverseException>(
            () => tx.Query("UNWIND [1] AS i CALL { WITH i CREATE (:C) } IN TRANSACTIONS"));
    }

    [Fact]
    public void A_procedure_that_writes_runs_as_a_write()
    {
        using var tdb = new TestDb();
        tdb.Db.Query("CALL apoc.create.node(['P'], {v: 1}) YIELD node RETURN node").Dispose();
        Assert.Equal(1L, Count(tdb.Db, "P"));
    }

    [Fact]
    public void Errors_carry_their_gql_status()
    {
        using var tdb = new TestDb();
        Assert.Equal("22012", Assert.Throws<TraverseException>(() => tdb.Db.Query("RETURN 1 / 0")).GqlStatus);
        Assert.Equal("42N62", Assert.Throws<TraverseException>(() => tdb.Db.Query("RETURN x")).GqlStatus);
        using var tx = tdb.Db.BeginReadTransaction();
        Assert.Equal("42N02", Assert.Throws<TraverseException>(() => tx.Query("CREATE ()")).GqlStatus);
    }

    [Fact]
    public void Reads_never_wait_for_the_writer_and_writes_do()
    {
        using var tdb = new TestDb();
        tdb.Db.BusyTimeout = TimeSpan.FromMilliseconds(50);
        using var writer = tdb.Db.BeginTransaction();
        using (var r = tdb.Db.Query("RETURN 1 AS n"))
            Assert.Equal(1L, r.ToList()[0].GetInt64(0));
        var busy = Assert.Throws<TraverseException>(() => tdb.Db.Query("CREATE (:D)"));
        Assert.Equal(TvErrorCode.Busy, busy.ErrorCode);
    }

    [Fact]
    public void A_cancelled_token_stops_a_query_and_a_wait_for_the_writer()
    {
        using var tdb = new TestDb();
        tdb.Db.Query("UNWIND range(1, 2000) AS i CREATE (:Big {i: i})").Dispose();
        using (var cts = new CancellationTokenSource(TimeSpan.FromMilliseconds(200)))
        {
            var watch = Stopwatch.StartNew();
            Assert.ThrowsAny<OperationCanceledException>(
                () => tdb.Db.Query("MATCH (a:Big), (b:Big), (c:Big) RETURN count(*)", cts.Token));
            Assert.True(watch.Elapsed < TimeSpan.FromSeconds(20), $"took {watch.Elapsed}");
        }
        tdb.Db.BusyTimeout = Timeout.InfiniteTimeSpan;
        using var writer = tdb.Db.BeginTransaction();
        using (var cts = new CancellationTokenSource(TimeSpan.FromMilliseconds(200)))
        {
            Assert.ThrowsAny<OperationCanceledException>(() => tdb.Db.Query("CREATE (:W)", cts.Token));
        }
    }
}
