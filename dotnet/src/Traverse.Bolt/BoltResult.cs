namespace Traverse.Bolt;

/// <summary>Complete result of a Cypher query: columns, materialized records, and summary metadata.</summary>
public sealed class BoltResult
{
    /// <summary>Column names from the RUN SUCCESS "fields" metadata.</summary>
    public string[] Columns { get; }

    /// <summary>All materialized RECORD messages.</summary>
    public IReadOnlyList<BoltRecord> Records { get; }

    /// <summary>Summary metadata from the PULL SUCCESS (stats, bookmark, etc.).</summary>
    public Dictionary<string, object?> Summary { get; }

    internal BoltResult(string[] columns, IReadOnlyList<BoltRecord> records, Dictionary<string, object?> summary)
    {
        Columns = columns;
        Records = records;
        Summary = summary;
    }

    internal static string[] ExtractColumns(Dictionary<string, object?> metadata)
    {
        if (metadata.TryGetValue("fields", out var f) && f is List<object?> fields)
        {
            var columns = new string[fields.Count];
            for (int i = 0; i < fields.Count; i++)
                columns[i] = (string)fields[i]!;
            return columns;
        }
        return [];
    }
}
