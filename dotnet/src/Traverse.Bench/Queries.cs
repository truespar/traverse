using System.Threading;

namespace Traverse.Bench;

/// <summary>Parameter kind for a benchmark query.</summary>
internal enum ParamKind
{
    None,
    Vertex,
    VertexPair,
    Create,
}

/// <summary>A benchmark query definition.</summary>
internal sealed class QueryDef
{
    public required string Name { get; init; }
    public required string Cypher { get; init; }
    public required ParamKind Params { get; init; }
    public required string Group { get; init; }
}

internal static class Queries
{
    /// <summary>All 26 Pokec benchmark queries (mirrors queries.rs).</summary>
    public static readonly QueryDef[] All =
    [
        // -- Read --
        new() { Name = "single_vertex_read",    Cypher = "MATCH (n:User {id: $id}) RETURN n",           Params = ParamKind.Vertex,     Group = "read" },
        new() { Name = "single_vertex_property", Cypher = "MATCH (n:User {id: $id}) RETURN n.age, n.name", Params = ParamKind.Vertex, Group = "read" },
        new() { Name = "pattern_short",          Cypher = "MATCH (n:User {id: $id})-[e]->(m) RETURN m LIMIT 1", Params = ParamKind.Vertex, Group = "read" },
        new() { Name = "pattern_cycle",          Cypher = "MATCH (n:User {id: $id})-[e1]->(m)-[e2]->(n) RETURN e1, m, e2", Params = ParamKind.Vertex, Group = "read" },
        new() { Name = "pattern_long",           Cypher = "MATCH (n1:User {id: $id})-[e1]->(n2)-[e2]->(n3)-[e3]->(n4)<-[e4]-(n5) RETURN n5 LIMIT 1", Params = ParamKind.Vertex, Group = "read" },

        // -- Expansion --
        new() { Name = "expansion_1",             Cypher = "MATCH (s:User {id: $id})-->(n:User) RETURN n.id",           Params = ParamKind.Vertex, Group = "expansion" },
        new() { Name = "expansion_1_with_filter",  Cypher = "MATCH (s:User {id: $id})-->(n:User) WHERE n.age >= 18 RETURN n.id", Params = ParamKind.Vertex, Group = "expansion" },
        new() { Name = "expansion_2",             Cypher = "MATCH (s:User {id: $id})-->()-->(n:User) RETURN DISTINCT n.id", Params = ParamKind.Vertex, Group = "expansion" },
        new() { Name = "expansion_2_with_filter",  Cypher = "MATCH (s:User {id: $id})-->()-->(n:User) WHERE n.age >= 18 RETURN DISTINCT n.id", Params = ParamKind.Vertex, Group = "expansion" },
        new() { Name = "expansion_3",             Cypher = "MATCH (s:User {id: $id})-->()-->()-->(n:User) RETURN DISTINCT n.id", Params = ParamKind.Vertex, Group = "expansion" },
        new() { Name = "expansion_3_with_filter",  Cypher = "MATCH (s:User {id: $id})-->()-->()-->(n:User) WHERE n.age >= 18 RETURN DISTINCT n.id", Params = ParamKind.Vertex, Group = "expansion" },
        new() { Name = "expansion_4",             Cypher = "MATCH (s:User {id: $id})-->()-->()-->()-->(n:User) RETURN DISTINCT n.id", Params = ParamKind.Vertex, Group = "expansion" },
        new() { Name = "expansion_4_with_filter",  Cypher = "MATCH (s:User {id: $id})-->()-->()-->()-->(n:User) WHERE n.age >= 18 RETURN DISTINCT n.id", Params = ParamKind.Vertex, Group = "expansion" },
        new() { Name = "neighbours_2",            Cypher = "MATCH (s:User {id: $id})-[*1..2]->(n:User) RETURN DISTINCT n.id", Params = ParamKind.Vertex, Group = "expansion" },
        new() { Name = "neighbours_2_with_filter", Cypher = "MATCH (s:User {id: $id})-[*1..2]->(n:User) WHERE n.age >= 18 RETURN DISTINCT n.id", Params = ParamKind.Vertex, Group = "expansion" },
        new() { Name = "neighbours_2_with_data",   Cypher = "MATCH (s:User {id: $id})-[*1..2]->(n:User) RETURN DISTINCT n.id, n.age, n.name", Params = ParamKind.Vertex, Group = "expansion" },
        new() { Name = "neighbours_2_with_data_and_filter", Cypher = "MATCH (s:User {id: $id})-[*1..2]->(n:User) WHERE n.age >= 18 RETURN DISTINCT n.id, n", Params = ParamKind.Vertex, Group = "expansion" },

        // -- Scan --
        new() { Name = "scan_expand",            Cypher = "MATCH (n:User)-[:KNOWS]->(m:User) RETURN n.id, m.id",          Params = ParamKind.None, Group = "scan" },
        new() { Name = "scan_expand_filter",     Cypher = "MATCH (n:User)-[:KNOWS]->(m:User) WHERE m.age > 50 RETURN n.id, m.id", Params = ParamKind.None, Group = "scan" },
        new() { Name = "scan_expand_2hop",       Cypher = "MATCH (n:User)-[:KNOWS]->()-[:KNOWS]->(m:User) RETURN DISTINCT m.id", Params = ParamKind.None, Group = "scan" },
        new() { Name = "scan_expand_2hop_filter", Cypher = "MATCH (n:User)-[:KNOWS]->()-[:KNOWS]->(m:User) WHERE m.age > 50 RETURN DISTINCT m.id", Params = ParamKind.None, Group = "scan" },
        new() { Name = "scan_project",           Cypher = "MATCH (n:User) RETURN n.age, n.name",                           Params = ParamKind.None, Group = "scan" },
        new() { Name = "scan_agg_global",        Cypher = "MATCH (n:User)-[:KNOWS]->()-[:KNOWS]->(m:User) RETURN count(m)", Params = ParamKind.None, Group = "scan" },
        new() { Name = "scan_agg_grouped",       Cypher = "MATCH (n:User)-[:KNOWS]->()-[:KNOWS]->(m:User) RETURN m.age, count(*)", Params = ParamKind.None, Group = "scan" },

        // -- Shortest path --
        new() { Name = "shortest_path",       Cypher = "MATCH (n:User {id: $from}), (m:User {id: $to}) WITH n, m MATCH p = shortestPath((n)-[*..15]->(m)) RETURN [node IN nodes(p) | node.id] AS path", Params = ParamKind.VertexPair, Group = "shortest_path" },
        new() { Name = "all_shortest_paths",  Cypher = "MATCH (n:User {id: $from}), (m:User {id: $to}) WITH n, m MATCH p = allShortestPaths((n)-[*..2]->(m)) RETURN [node IN nodes(p) | node.id] AS path", Params = ParamKind.VertexPair, Group = "shortest_path" },

        // -- Write --
        new() { Name = "vertex_create", Cypher = "CREATE (n:UserTemp {id: $id}) RETURN n",                                 Params = ParamKind.Create, Group = "write" },
        new() { Name = "edge_create",   Cypher = "MATCH (a:User {id: $from}), (b:User {id: $to}) CREATE (a)-[:TempEdge]->(b)", Params = ParamKind.VertexPair, Group = "write" },

        // -- Update --
        new() { Name = "vertex_update", Cypher = "MATCH (n:User {id: $id}) SET n.property = -1", Params = ParamKind.Vertex, Group = "update" },

        // -- Aggregate --
        new() { Name = "count",                 Cypher = "MATCH (n) RETURN count(n), count(n.age)",                    Params = ParamKind.None, Group = "aggregate" },
        new() { Name = "min_max_avg",           Cypher = "MATCH (n) RETURN min(n.age), max(n.age), avg(n.age)",        Params = ParamKind.None, Group = "aggregate" },
        new() { Name = "group_by_age",          Cypher = "MATCH (n:User) RETURN n.age, count(*)",                      Params = ParamKind.None, Group = "aggregate" },
        new() { Name = "aggregate_with_filter", Cypher = "MATCH (n:User) WHERE n.age >= 18 RETURN n.age, count(*)",    Params = ParamKind.None, Group = "aggregate" },
    ];

    /// <summary>Group ordering (matches Rust bench default).</summary>
    public static readonly string[] GroupOrder =
        ["read", "expansion", "scan", "aggregate", "shortest_path", "update", "write"];

    /// <summary>Resolve comma-separated groups/query-names to query list.</summary>
    public static List<QueryDef> Resolve(string? groups)
    {
        if (string.IsNullOrEmpty(groups) || groups == "all")
        {
            var result = new List<QueryDef>();
            foreach (var g in GroupOrder)
                foreach (var q in All)
                    if (q.Group == g) result.Add(q);
            return result;
        }

        var list = new List<QueryDef>();
        foreach (var token in groups.Split(',', StringSplitOptions.TrimEntries | StringSplitOptions.RemoveEmptyEntries))
        {
            if (GroupOrder.Contains(token))
            {
                foreach (var q in All)
                    if (q.Group == token) list.Add(q);
            }
            else
            {
                var match = All.FirstOrDefault(q => q.Name == token);
                if (match is not null)
                    list.Add(match);
                else
                    Console.Error.WriteLine($"Warning: unknown group/query '{token}', skipping");
            }
        }
        return list;
    }

    /// <summary>Vertex counts per dataset variant.</summary>
    public static long? VertexCount(string variant) => variant switch
    {
        "small"  => 10_000,
        "medium" => 100_000,
        "large"  => 1_632_803,
        _        => null,
    };

    private static long _createCounter = 1_000_000;

    /// <summary>Generate query parameters based on param kind.</summary>
    public static Dictionary<string, object>? GenerateParams(ParamKind kind, long vertexCount, Random rng)
    {
        switch (kind)
        {
            case ParamKind.Vertex:
                return new() { ["id"] = rng.NextInt64(1, vertexCount + 1) };
            case ParamKind.VertexPair:
            {
                long a = rng.NextInt64(1, vertexCount + 1);
                long b;
                do { b = rng.NextInt64(1, vertexCount + 1); } while (b == a);
                return new() { ["from"] = a, ["to"] = b };
            }
            case ParamKind.Create:
            {
                long id = Interlocked.Increment(ref _createCounter);
                return new() { ["id"] = id };
            }
            default:
                return null;
        }
    }
}
