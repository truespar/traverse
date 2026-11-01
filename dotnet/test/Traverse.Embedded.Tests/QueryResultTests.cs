using Traverse.Tests.Helpers;
using Xunit;

namespace Traverse.Tests;

public sealed class QueryResultTests
{
    [Fact]
    public void Columns_returns_correct_names()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("RETURN 1 AS x, 'hello' AS y");

        Assert.Equal(2, r.ColumnCount);
        Assert.Equal("x", r.Columns[0].Name);
        Assert.Equal("y", r.Columns[1].Name);

        tx.Abort();
    }

    [Fact]
    public void RowCount_returns_correct_value()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("UNWIND [1,2,3] AS x RETURN x");

        Assert.Equal(3, r.RowCount);
        tx.Abort();
    }

    [Fact]
    public void Enumeration_yields_all_rows()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("UNWIND [10, 20, 30] AS x RETURN x");

        var values = new List<long>();
        foreach (var row in r)
        {
            values.Add(row.GetInt64(0));
        }

        Assert.Equal([10L, 20L, 30L], values);
        tx.Abort();
    }

    [Fact]
    public void GetString_returns_string_value()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("RETURN 'hello' AS greeting");

        var rows = r.ToList();
        Assert.Single(rows);
        Assert.Equal("hello", rows[0].GetString("greeting"));

        tx.Abort();
    }

    [Fact]
    public void GetInt64_returns_integer_value()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("RETURN 42 AS answer");

        var rows = r.ToList();
        Assert.Equal(42L, rows[0].GetInt64("answer"));

        tx.Abort();
    }

    [Fact]
    public void GetDouble_returns_float_value()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("RETURN 3.14 AS pi");

        var rows = r.ToList();
        Assert.Equal(3.14, rows[0].GetDouble("pi"), precision: 5);

        tx.Abort();
    }

    [Fact]
    public void GetBool_returns_boolean_value()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("RETURN true AS flag");

        var rows = r.ToList();
        Assert.True(rows[0].GetBool("flag"));

        tx.Abort();
    }

    [Fact]
    public void IsNull_detects_null_values()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("RETURN null AS val");

        var rows = r.ToList();
        Assert.True(rows[0].IsNull(0));
        Assert.Null(rows[0].GetString(0));

        tx.Abort();
    }

    [Fact]
    public void GetTypeName_returns_correct_types()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("RETURN 1 AS i, 1.0 AS f, true AS b, 'hi' AS s, null AS n");

        var rows = r.ToList();
        Assert.Equal("INTEGER", rows[0].GetTypeName(0));
        Assert.Equal("FLOAT", rows[0].GetTypeName(1));
        Assert.Equal("BOOLEAN", rows[0].GetTypeName(2));
        Assert.Equal("STRING", rows[0].GetTypeName(3));
        Assert.Equal("NULL", rows[0].GetTypeName(4));

        tx.Abort();
    }

    [Fact]
    public void Snapshot_copies_values_to_managed_memory()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("UNWIND [1, 2] AS x RETURN x");

        Row? firstSnapshot = null;
        foreach (var row in r)
        {
            if (firstSnapshot == null)
                firstSnapshot = row.Snapshot();
        }

        // After enumeration, the native pointers are invalid,
        // but snapshot values should still work.
        Assert.NotNull(firstSnapshot);
        Assert.Equal(1L, firstSnapshot!.GetInt64(0));

        tx.Abort();
    }

    [Fact]
    public void ToList_returns_snapshots()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("UNWIND ['a', 'b', 'c'] AS x RETURN x");

        var list = r.ToList();
        Assert.Equal(3, list.Count);
        Assert.Equal("a", list[0].GetString(0));
        Assert.Equal("b", list[1].GetString(0));
        Assert.Equal("c", list[2].GetString(0));

        tx.Abort();
    }

    [Fact]
    public void Node_type_detected()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        tx.Query("CREATE (n:Person {name: 'Alice'})").Dispose();
        using var r = tx.Query("MATCH (n:Person) RETURN n");

        var rows = r.ToList();
        Assert.Single(rows);
        Assert.Equal("NODE", rows[0].GetTypeName(0));

        tx.Abort();
    }
}
