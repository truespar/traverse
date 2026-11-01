namespace Traverse.Bolt;

/// <summary>Exception thrown when the Bolt server returns a FAILURE message.</summary>
public sealed class BoltException : Exception
{
    /// <summary>The error code from the server (e.g. "Neo.ClientError.Statement.SyntaxError").</summary>
    public string Code { get; }

    /// <summary>True if the error is transient and the operation may succeed on retry.</summary>
    public bool IsTransient => Code.Contains("TransientError", StringComparison.Ordinal);

    /// <summary>GQL status code (e.g. "02N42"). Null if server didn't provide it.</summary>
    public string? GqlStatus { get; init; }

    /// <summary>GQL status description. Null if server didn't provide it.</summary>
    public string? GqlStatusDescription { get; init; }

    /// <summary>GQL diagnostic record with extra error context. Null if not provided.</summary>
    public Dictionary<string, object>? DiagnosticRecord { get; init; }

    /// <summary>Chained GQL error cause. Null if no nested cause.</summary>
    public BoltException? Cause { get; init; }

    public BoltException(string code, string message) : base(message)
    {
        Code = code;
    }

    public override string ToString() => $"BoltException({Code}): {Message}";

    /// <summary>Parse a FAILURE metadata map into a BoltException with GQL fields.</summary>
    internal static BoltException FromMetadata(Dictionary<string, object?> metadata, int boltVersion)
    {
        // Neo4j uses "neo4j_code" in Bolt 5+, falling back to "code"
        string code;
        if (metadata.TryGetValue("neo4j_code", out var nc) && nc is string ncs)
            code = ncs;
        else if (metadata.TryGetValue("code", out var c) && c is string cs)
            code = cs;
        else
            code = "Unknown";

        // Remap error codes for Bolt < 5 compatibility
        if (boltVersion < 500)
        {
            code = code switch
            {
                "Neo.TransientError.Transaction.Terminated" => "Neo.ClientError.Transaction.Terminated",
                "Neo.TransientError.Transaction.LockClientStopped" => "Neo.ClientError.Transaction.LockClientStopped",
                _ => code
            };
        }

        string message = metadata.TryGetValue("message", out var m) && m is string ms ? ms : "Unknown error";

        var ex = new BoltException(code, message)
        {
            GqlStatus = metadata.TryGetValue("gql_status", out var gs) && gs is string gss ? gss : null,
            GqlStatusDescription = metadata.TryGetValue("description", out var d) && d is string ds ? ds : null,
            DiagnosticRecord = metadata.TryGetValue("diagnostic_record", out var dr) && dr is Dictionary<string, object> drm
                ? drm : null,
            Cause = metadata.TryGetValue("cause", out var cause) && cause is Dictionary<string, object?> causeMap
                ? FromMetadata(causeMap, boltVersion) : null,
        };

        return ex;
    }
}
