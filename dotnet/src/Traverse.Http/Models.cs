using System.Text.Json;
using System.Text.Json.Serialization;

namespace Traverse.Http;

// -- Health ----------------------------------------------------------

/// <summary>Server health response from GET /api/health.</summary>
public sealed class HealthResponse
{
    [JsonPropertyName("status")]
    public string Status { get; init; } = "";

    [JsonPropertyName("version")]
    public string Version { get; init; } = "";

    [JsonPropertyName("uptime_secs")]
    public long UptimeSecs { get; init; }

    [JsonPropertyName("memory_bytes")]
    public long MemoryBytes { get; init; }

    [JsonPropertyName("databases")]
    public HealthDatabaseInfo[] Databases { get; init; } = [];

    [JsonPropertyName("allow_drop_database")]
    public bool AllowDropDatabase { get; init; }

    [JsonPropertyName("auth_required")]
    public bool AuthRequired { get; init; }

    /// <summary>
    /// Compile-time server capabilities, e.g. {"gql": true, "ml": false}.
    /// Check <c>Features["gql"]</c> before passing dialect "gql" to
    /// <see cref="TraverseHttpClient.QueryAsync"/>.
    /// </summary>
    [JsonPropertyName("features")]
    public Dictionary<string, bool> Features { get; init; } = new();
}

/// <summary>Per-database info within the health response.</summary>
public sealed class HealthDatabaseInfo
{
    [JsonPropertyName("name")]
    public string Name { get; init; } = "";

    [JsonPropertyName("default")]
    public bool Default { get; init; }

    [JsonPropertyName("nodes")]
    public long Nodes { get; init; }

    [JsonPropertyName("edges")]
    public long Edges { get; init; }

    [JsonPropertyName("memory_bytes")]
    public long MemoryBytes { get; init; }
}

// -- Databases -------------------------------------------------------

/// <summary>Information about a loaded database.</summary>
public sealed class DatabaseInfo
{
    [JsonPropertyName("name")]
    public string Name { get; init; } = "";

    [JsonPropertyName("default")]
    public bool Default { get; init; }

    [JsonPropertyName("readOnly")]
    public bool ReadOnly { get; init; }

    [JsonPropertyName("aliases")]
    public string[] Aliases { get; init; } = [];
}

// -- Files -----------------------------------------------------------

/// <summary>Information about a .tvdb database or .tvmodel trained model on disk.</summary>
public sealed class TvdbFileInfo
{
    [JsonPropertyName("name")]
    public string Name { get; init; } = "";

    [JsonPropertyName("file")]
    public string File { get; init; } = "";

    [JsonPropertyName("bytes")]
    public long Bytes { get; init; }

    [JsonPropertyName("loaded")]
    public bool Loaded { get; init; }

    /// <summary>"database" for .tvdb files, "model" for .tvmodel trained models.</summary>
    [JsonPropertyName("kind")]
    public string Kind { get; init; } = "database";
}

/// <summary>Result of a file upload operation.</summary>
public sealed class UploadResult
{
    [JsonPropertyName("ok")]
    public bool Ok { get; init; }

    [JsonPropertyName("name")]
    public string Name { get; init; } = "";

    [JsonPropertyName("file")]
    public string File { get; init; } = "";

    [JsonPropertyName("bytes")]
    public long Bytes { get; init; }
}

// -- Aliases ---------------------------------------------------------

/// <summary>Information about a database alias.</summary>
public sealed class AliasInfo
{
    [JsonPropertyName("alias")]
    public string Alias { get; init; } = "";

    [JsonPropertyName("database")]
    public string Database { get; init; } = "";
}

// -- Schema ----------------------------------------------------------

/// <summary>Database schema from GET /api/schema.</summary>
public sealed class SchemaResponse
{
    [JsonPropertyName("labels")]
    public string[] Labels { get; init; } = [];

    [JsonPropertyName("relationship_types")]
    public string[] RelationshipTypes { get; init; } = [];

    [JsonPropertyName("property_keys")]
    public SchemaProperty[] PropertyKeys { get; init; } = [];

    [JsonPropertyName("indexes")]
    public IndexInfo[] Indexes { get; init; } = [];

    [JsonPropertyName("constraints")]
    public ConstraintInfo[] Constraints { get; init; } = [];

    [JsonPropertyName("labels_detail")]
    public LabelDetail[] LabelsDetail { get; init; } = [];

    [JsonPropertyName("relationship_types_detail")]
    public LabelDetail[] RelationshipTypesDetail { get; init; } = [];
}

/// <summary>A property name and type.</summary>
public sealed class SchemaProperty
{
    [JsonPropertyName("name")]
    public string Name { get; init; } = "";

    [JsonPropertyName("type")]
    public string Type { get; init; } = "";
}

/// <summary>Index information.</summary>
public sealed class IndexInfo
{
    [JsonPropertyName("name")]
    public string Name { get; init; } = "";

    [JsonPropertyName("label")]
    public string Label { get; init; } = "";

    [JsonPropertyName("property")]
    public string Property { get; init; } = "";

    [JsonPropertyName("entries")]
    public long Entries { get; init; }
}

/// <summary>Constraint information.</summary>
public sealed class ConstraintInfo
{
    [JsonPropertyName("name")]
    public string Name { get; init; } = "";

    [JsonPropertyName("type")]
    public string Type { get; init; } = "";

    [JsonPropertyName("label")]
    public string Label { get; init; } = "";

    [JsonPropertyName("property")]
    public string Property { get; init; } = "";
}

/// <summary>Label or relationship type with its properties.</summary>
public sealed class LabelDetail
{
    [JsonPropertyName("name")]
    public string Name { get; init; } = "";

    [JsonPropertyName("properties")]
    public SchemaProperty[] Properties { get; init; } = [];
}

// -- Query -----------------------------------------------------------

/// <summary>Query result from POST /api/query.</summary>
public sealed class QueryResponse
{
    [JsonPropertyName("columns")]
    public string[] Columns { get; init; } = [];

    /// <summary>
    /// Rows of query results. Each row is an array of <see cref="JsonElement"/>
    /// values, since Cypher can return any type (string, int, bool, node, edge, path, etc.).
    /// </summary>
    [JsonPropertyName("rows")]
    public JsonElement[][] Rows { get; init; } = [];

    [JsonPropertyName("total_rows")]
    public long TotalRows { get; init; }

    [JsonPropertyName("truncated")]
    public bool Truncated { get; init; }

    [JsonPropertyName("query_type")]
    public string QueryType { get; init; } = "";

    [JsonPropertyName("stats")]
    public QueryStats Stats { get; init; } = new();

    [JsonPropertyName("time_ms")]
    public double TimeMs { get; init; }

    [JsonPropertyName("parsing_time_ms")]
    public double ParsingTimeMs { get; init; }

    [JsonPropertyName("planning_time_ms")]
    public double PlanningTimeMs { get; init; }

    [JsonPropertyName("execution_time_ms")]
    public double ExecutionTimeMs { get; init; }
}

/// <summary>Query execution statistics.</summary>
public sealed class QueryStats
{
    [JsonPropertyName("nodes_created")]
    public long NodesCreated { get; init; }

    [JsonPropertyName("nodes_deleted")]
    public long NodesDeleted { get; init; }

    [JsonPropertyName("relationships_created")]
    public long RelationshipsCreated { get; init; }

    [JsonPropertyName("relationships_deleted")]
    public long RelationshipsDeleted { get; init; }

    [JsonPropertyName("properties_set")]
    public long PropertiesSet { get; init; }

    [JsonPropertyName("labels_added")]
    public long LabelsAdded { get; init; }

    [JsonPropertyName("labels_removed")]
    public long LabelsRemoved { get; init; }
}

// -- Import ----------------------------------------------------------

/// <summary>Result of a Cypher import operation (POST /api/import).</summary>
public sealed class ImportResult
{
    [JsonPropertyName("total_statements")]
    public long TotalStatements { get; init; }

    [JsonPropertyName("executed")]
    public long Executed { get; init; }

    [JsonPropertyName("errors")]
    public ImportError[] Errors { get; init; } = [];

    [JsonPropertyName("stats")]
    public QueryStats Stats { get; init; } = new();

    [JsonPropertyName("time_ms")]
    public double TimeMs { get; init; }
}

/// <summary>A per-statement import error.</summary>
public sealed class ImportError
{
    [JsonPropertyName("statement")]
    public string Statement { get; init; } = "";

    [JsonPropertyName("error")]
    public string Error { get; init; } = "";
}

/// <summary>Result of a CSV node import (POST /api/import/csv).</summary>
public sealed class CsvImportResult
{
    [JsonPropertyName("nodes_created")]
    public long NodesCreated { get; init; }

    [JsonPropertyName("properties_set")]
    public long PropertiesSet { get; init; }

    [JsonPropertyName("labels_added")]
    public long LabelsAdded { get; init; }

    [JsonPropertyName("errors")]
    public CsvImportError[] Errors { get; init; } = [];

    [JsonPropertyName("time_ms")]
    public double TimeMs { get; init; }
}

/// <summary>Result of a CSV edge import (POST /api/import/csv/edges).</summary>
public sealed class CsvEdgeImportResult
{
    [JsonPropertyName("edges_created")]
    public long EdgesCreated { get; init; }

    [JsonPropertyName("properties_set")]
    public long PropertiesSet { get; init; }

    [JsonPropertyName("errors")]
    public CsvImportError[] Errors { get; init; } = [];

    [JsonPropertyName("time_ms")]
    public double TimeMs { get; init; }
}

/// <summary>A per-row CSV import error.</summary>
public sealed class CsvImportError
{
    [JsonPropertyName("row")]
    public long Row { get; init; }

    [JsonPropertyName("error")]
    public string Error { get; init; } = "";
}

/// <summary>Column mapping for CSV import.</summary>
public sealed class CsvColumnMapping
{
    [JsonPropertyName("name")]
    public required string Name { get; init; }

    [JsonPropertyName("property")]
    public string? Property { get; init; }

    /// <summary>Data type: "string" (default), "integer", "float", "boolean".</summary>
    [JsonPropertyName("type")]
    public string? Type { get; init; }
}

/// <summary>Source or target endpoint mapping for CSV edge import.</summary>
public sealed class CsvEdgeEndpoint
{
    [JsonPropertyName("label")]
    public required string Label { get; init; }

    [JsonPropertyName("column")]
    public required string Column { get; init; }

    [JsonPropertyName("property")]
    public required string Property { get; init; }
}

// -- Tasks -------------------------------------------------------

/// <summary>Information about an async server task from GET /api/tasks/{id}.</summary>
public sealed class TaskInfo
{
    [JsonPropertyName("id")]
    public string Id { get; init; } = "";

    [JsonPropertyName("operation")]
    public string Operation { get; init; } = "";

    [JsonPropertyName("status")]
    public string Status { get; init; } = "";

    [JsonPropertyName("result")]
    public JsonElement? Result { get; init; }

    [JsonPropertyName("error")]
    public string? Error { get; init; }

    [JsonPropertyName("createdAt")]
    public string CreatedAt { get; init; } = "";

    [JsonPropertyName("finishedAt")]
    public string? FinishedAt { get; init; }
}

// -- Users -----------------------------------------------------------

/// <summary>User account information.</summary>
public sealed class UserInfo
{
    [JsonPropertyName("username")]
    public string Username { get; init; } = "";

    [JsonPropertyName("role")]
    public string Role { get; init; } = "";
}
