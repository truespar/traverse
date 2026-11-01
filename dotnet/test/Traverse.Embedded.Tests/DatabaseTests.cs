using Traverse.Tests.Helpers;
using Xunit;

namespace Traverse.Tests;

[Collection("DatabaseOpen")]
public sealed class DatabaseTests
{
    [Fact]
    public void Open_creates_and_reopens_database()
    {
        var dir = Path.Combine(
            Path.GetTempPath(), "traverse_tests", Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(dir);
        var path = Path.Combine(dir, "test.tvdb");

        try
        {
            using (var db1 = Database.Open(path))
            {
                db1.Flush();
            }

            // Re-open the same file after closing.
            using (var db2 = Database.Open(path))
            {
                db2.Flush();
            }
        }
        finally
        {
            try { Directory.Delete(dir, recursive: true); } catch { }
        }
    }

    [Fact]
    public void Open_invalid_path_throws()
    {
        // NUL bytes in path are not valid UTF-8 for C strings.
        Assert.Throws<TraverseException>(() => Database.Open(""));
    }

    [Fact]
    public void Dispose_is_idempotent()
    {
        using var tdb = new TestDb();
        tdb.Db.Dispose();
        tdb.Db.Dispose(); // second dispose should not throw
    }

    [Fact]
    public void Flush_after_dispose_throws()
    {
        var tdb = new TestDb();
        tdb.Db.Dispose();

        Assert.Throws<ObjectDisposedException>(() => tdb.Db.Flush());

        try { Directory.Delete(Path.GetDirectoryName(tdb.Path)!, true); } catch { }
    }

    [Fact]
    public void Open_rejects_file_exceeding_MaxFileSizeBytes()
    {
        var dir = Path.Combine(
            Path.GetTempPath(), "traverse_tests", Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(dir);
        var path = Path.Combine(dir, "huge.tvdb");

        try
        {
            // Create a small file, then set a very low limit
            File.WriteAllBytes(path, new byte[1024]);

            var original = Database.MaxFileSizeBytes;
            try
            {
                Database.MaxFileSizeBytes = 512; // 512 bytes limit

                var ex = Assert.Throws<TraverseException>(() => Database.Open(path));
                Assert.Equal(TvErrorCode.ResourceExhausted, ex.ErrorCode);
                Assert.Contains("exceeds the safety limit", ex.Message);
            }
            finally
            {
                Database.MaxFileSizeBytes = original;
            }
        }
        finally
        {
            try { Directory.Delete(dir, recursive: true); } catch { }
        }
    }

    [Fact]
    public void Open_allows_new_database_regardless_of_limit()
    {
        var dir = Path.Combine(
            Path.GetTempPath(), "traverse_tests", Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(dir);
        var path = Path.Combine(dir, "new.tvdb");

        var original = Database.MaxFileSizeBytes;
        try
        {
            Database.MaxFileSizeBytes = 1; // Tiny limit - but file doesn't exist yet

            using var db = Database.Open(path);
            db.Flush(); // Should work fine - new database
        }
        finally
        {
            Database.MaxFileSizeBytes = original;
            try { Directory.Delete(dir, recursive: true); } catch { }
        }
    }

    [Fact]
    public void Open_skips_check_when_MaxFileSizeBytes_is_zero()
    {
        using var tdb = new TestDb();
        tdb.Db.Flush();
        tdb.Db.Dispose();

        var original = Database.MaxFileSizeBytes;
        try
        {
            Database.MaxFileSizeBytes = 0; // Disabled

            using var db = Database.Open(tdb.Path);
            db.Flush();
        }
        finally
        {
            Database.MaxFileSizeBytes = original;
            try { Directory.Delete(Path.GetDirectoryName(tdb.Path)!, true); } catch { }
        }
    }
}
