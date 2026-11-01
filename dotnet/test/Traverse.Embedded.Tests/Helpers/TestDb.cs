namespace Traverse.Tests.Helpers;

/// <summary>
/// Creates a temporary Traverse database for testing.
/// Automatically cleans up the database file(s) on dispose.
/// </summary>
internal sealed class TestDb : IDisposable
{
    private readonly string _dir;

    public Database Db { get; }
    public string Path { get; }

    public TestDb()
    {
        _dir = System.IO.Path.Combine(
            System.IO.Path.GetTempPath(),
            "traverse_tests",
            Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(_dir);
        Path = System.IO.Path.Combine(_dir, "test.tvdb");
        Db = Database.Open(Path);
    }

    public void Dispose()
    {
        Db.Dispose();
        try { Directory.Delete(_dir, recursive: true); }
        catch { /* best effort */ }
    }
}
