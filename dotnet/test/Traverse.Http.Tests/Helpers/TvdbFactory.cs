namespace Traverse.Http.Tests.Helpers;

/// <summary>
/// Creates .tvdb files with pre-baked data using the embedded Traverse engine.
/// Files are written to a temp directory and cleaned up on dispose.
/// </summary>
internal sealed class TvdbFactory : IDisposable
{
    private readonly string _dir;

    public TvdbFactory()
    {
        _dir = Path.Combine(Path.GetTempPath(), "traverse_http_tests", Guid.NewGuid().ToString("N"));
        Directory.CreateDirectory(_dir);
    }

    /// <summary>
    /// Creates a .tvdb file containing the given Cypher statements.
    /// Returns the full file path.
    /// </summary>
    public string Create(string name, params string[] cypherStatements)
    {
        var path = Path.Combine(_dir, $"{name}.tvdb");
        using var db = Database.Open(path);
        using var tx = db.BeginTransaction();
        foreach (var cypher in cypherStatements)
            tx.Query(cypher);
        tx.Commit();
        db.Flush();
        return path;
    }

    /// <summary>Returns the bytes of a .tvdb file created by <see cref="Create"/>.</summary>
    public static byte[] ReadBytes(string path) => File.ReadAllBytes(path);

    public void Dispose()
    {
        try { Directory.Delete(_dir, recursive: true); }
        catch { /* best effort */ }
    }
}
