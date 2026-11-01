namespace Traverse;

/// <summary>
/// Metadata about a single column in a query result.
/// </summary>
/// <param name="Index">Zero-based column ordinal.</param>
/// <param name="Name">Column name from the Cypher query.</param>
public readonly record struct ColumnInfo(int Index, string Name);
