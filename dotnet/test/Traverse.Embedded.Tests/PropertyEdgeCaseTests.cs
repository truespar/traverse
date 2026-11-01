using Traverse.Tests.Helpers;
using Xunit;

namespace Traverse.Tests;

/// <summary>
/// Edge-case tests for property types, unicode, boundary values, and marshalling.
/// </summary>
public sealed class PropertyEdgeCaseTests
{
    [Fact]
    public void Unicode_string_roundtrips()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        tx.Query("CREATE (n:I18n {name: '日本語テスト', emoji: '🚀🔥', arabic: 'مرحبا'})").Dispose();
        using var r = tx.Query("MATCH (n:I18n) RETURN n.name, n.emoji, n.arabic");
        var rows = r.ToList();
        Assert.Single(rows);
        Assert.Equal("日本語テスト", rows[0].GetString(0));
        Assert.Equal("🚀🔥", rows[0].GetString(1));
        Assert.Equal("مرحبا", rows[0].GetString(2));
        tx.Abort();
    }

    [Fact]
    public void Empty_string_property()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        tx.Query("CREATE (n:E {val: ''})").Dispose();
        using var r = tx.Query("MATCH (n:E) RETURN n.val");
        var rows = r.ToList();
        Assert.Single(rows);
        Assert.Equal("", rows[0].GetString(0));
        tx.Abort();
    }

    [Fact]
    public void Large_string_property()
    {
        using var tdb = new TestDb();
        var bigString = new string('x', 100_000);

        using var tx = tdb.Db.BeginTransaction();
        tx.Query($"CREATE (n:Big {{val: '{bigString}'}})").Dispose();
        using var r = tx.Query("MATCH (n:Big) RETURN n.val");
        var rows = r.ToList();
        Assert.Single(rows);
        Assert.Equal(bigString, rows[0].GetString(0));
        tx.Abort();
    }

    [Fact]
    public void Negative_integer()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("RETURN -9999999 AS val");
        var rows = r.ToList();
        Assert.Equal(-9999999L, rows[0].GetInt64(0));
        tx.Abort();
    }

    [Fact]
    public void Large_integer_boundary()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        // Test near i64 max
        using var r = tx.Query("RETURN 9223372036854775807 AS val");
        var rows = r.ToList();
        Assert.Equal(long.MaxValue, rows[0].GetInt64(0));
        tx.Abort();
    }

    [Fact]
    public void Negative_float()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("RETURN -273.15 AS val");
        var rows = r.ToList();
        Assert.Equal(-273.15, rows[0].GetDouble(0), precision: 5);
        tx.Abort();
    }

    [Fact]
    public void Zero_values()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("RETURN 0 AS i, 0.0 AS f, false AS b");
        var rows = r.ToList();
        Assert.Equal(0L, rows[0].GetInt64(0));
        Assert.Equal(0.0, rows[0].GetDouble(1));
        Assert.False(rows[0].GetBool(2));
        tx.Abort();
    }

    [Fact]
    public void Multiple_null_columns()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("RETURN null AS a, null AS b, null AS c");
        var rows = r.ToList();
        Assert.True(rows[0].IsNull(0));
        Assert.True(rows[0].IsNull(1));
        Assert.True(rows[0].IsNull(2));
        tx.Abort();
    }

    [Fact]
    public void Mixed_null_and_values()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("RETURN 42 AS a, null AS b, 'hello' AS c");
        var rows = r.ToList();
        Assert.Equal(42L, rows[0].GetInt64(0));
        Assert.True(rows[0].IsNull(1));
        Assert.Equal("hello", rows[0].GetString(2));
        tx.Abort();
    }

    [Fact]
    public void String_with_special_characters()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        // Backslashes, quotes, newlines
        tx.Query("CREATE (n:Special {val: 'line1\\nline2\\ttab'})").Dispose();
        using var r = tx.Query("MATCH (n:Special) RETURN n.val");
        var rows = r.ToList();
        Assert.Single(rows);
        Assert.Contains("line1", rows[0].GetString(0)!);
        tx.Abort();
    }

    [Fact]
    public void BulkWriter_unicode_properties()
    {
        using var tdb = new TestDb();

        using (var bw = tdb.Db.BeginBulkWrite())
        {
            bw.CreateNode(["I18n"], ["name", "city"], ["Ñoño", "München"]);
            bw.Commit();
        }

        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("MATCH (n:I18n) RETURN n.name, n.city");
        var rows = r.ToList();
        Assert.Equal("Ñoño", rows[0].GetString(0));
        Assert.Equal("München", rows[0].GetString(1));
        tx.Abort();
    }

    [Fact]
    public void BulkWriter_empty_string_becomes_null()
    {
        // BulkWriter auto-infers: empty string -> Null
        using var tdb = new TestDb();

        using (var bw = tdb.Db.BeginBulkWrite())
        {
            bw.CreateNode(["NullTest"], ["val"], [""]);
            bw.Commit();
        }

        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("MATCH (n:NullTest) RETURN n.val");
        var rows = r.ToList();
        Assert.Single(rows);
        Assert.True(rows[0].IsNull(0));
        tx.Abort();
    }

    [Fact]
    public void GetValue_returns_boxed_typed_values()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("RETURN 42 AS i, 3.14 AS f, true AS b, 'text' AS s, null AS n");
        var rows = r.ToList();

        Assert.IsType<long>(rows[0].GetValue(0));
        Assert.Equal(42L, rows[0].GetValue(0));
        Assert.IsType<double>(rows[0].GetValue(1));
        Assert.IsType<bool>(rows[0].GetValue(2));
        Assert.IsType<string>(rows[0].GetValue(3));
        Assert.Null(rows[0].GetValue(4));

        tx.Abort();
    }

    [Fact]
    public void Column_name_lookup_not_found_throws()
    {
        using var tdb = new TestDb();
        using var tx = tdb.Db.BeginTransaction();
        using var r = tx.Query("RETURN 1 AS x");
        var rows = r.ToList();

        Assert.Throws<ArgumentException>(() => rows[0].GetString("nonexistent"));

        tx.Abort();
    }
}
