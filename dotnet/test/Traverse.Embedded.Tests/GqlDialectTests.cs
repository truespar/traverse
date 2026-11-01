using Traverse.Tests.Helpers;
using Xunit;

namespace Traverse.Tests;

/// <summary>
/// Embedded ISO/IEC 39075 GQL execution via
/// <see cref="Transaction.QueryGql"/>. The queries use GQL-only
/// syntax (<c>||</c> concat, <c>IS INTEGER</c>,
/// <c>INSERT</c>/<c>FILTER</c>) that the openCypher parser rejects,
/// so passing proves the GQL front-end handled them.
/// </summary>
public sealed class GqlDialectTests
{
    [Fact]
    public void Gql_only_syntax_executes()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();

        using var r = tx.QueryGql("RETURN 'foo' || 'bar' AS s, 1 IS INTEGER AS i");
        var rows = r.ToList();
        Assert.Single(rows);
        Assert.Equal("foobar", rows[0].GetString(0));
        Assert.True(rows[0].GetBool(1));
        tx.Abort();
    }

    [Fact]
    public void Gql_insert_and_filter_round_trip()
    {
        using var tdb = new TestDb();
        using (var tx = tdb.Db.BeginTransaction())
        {
            using var r = tx.QueryGql("INSERT (:Person {name: 'Ada', age: 36})");
            tx.Commit();
        }

        using (var tx = tdb.Db.BeginTransaction())
        {
            using var r = tx.QueryGql(
                "MATCH (p:Person) FILTER p.age > 10 RETURN p.name AS name");
            var rows = r.ToList();
            Assert.Single(rows);
            Assert.Equal("Ada", rows[0].GetString(0));
            tx.Abort();
        }
    }

    [Fact]
    public void Cypher_rejects_gql_syntax()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();

        Assert.Throws<TraverseException>(
            () => tx.Query("RETURN 1 IS INTEGER AS i"));
        tx.Abort();
    }
}
