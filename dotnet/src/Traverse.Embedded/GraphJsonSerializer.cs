using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using Traverse.Interop;

namespace Traverse;

/// <summary>
/// Serializes a <see cref="QueryResult"/> to graphology-compatible JSON
/// (<c>{"nodes":[...],"edges":[...]}</c>) for use with Sigma.js and similar
/// graph visualization libraries.
/// </summary>
public static class GraphJsonSerializer
{
    /// <summary>
    /// Serialize all Node and Edge values from the result into graphology JSON.
    /// Paths are decomposed into their constituent nodes and edges.
    /// Duplicate nodes/edges (by id) are deduplicated.
    /// </summary>
    public static string Serialize(QueryResult result)
    {
        ArgumentNullException.ThrowIfNull(result);

        var nodes = new Dictionary<long, JsonNode>();
        var edges = new Dictionary<long, JsonEdge>();

        foreach (var row in result)
        {
            for (int col = 0; col < result.ColumnCount; col++)
            {
                CollectFromCell(row, col, nodes, edges);
            }
        }

        return WriteJson(nodes.Values, edges.Values);
    }

    private static void CollectFromCell(
        Row row,
        int col,
        Dictionary<long, JsonNode> nodes,
        Dictionary<long, JsonEdge> edges)
    {
        if (row.IsNull(col)) return;

        var typeName = row.GetTypeName(col);

        switch (typeName)
        {
            case "NODE":
                CollectNode(row, col, nodes);
                break;
            case "RELATIONSHIP":
                CollectEdge(row, col, edges);
                break;
            case "PATH":
                CollectPath(row, col, nodes, edges);
                break;
        }
    }

    private static void CollectNode(Row row, int col, Dictionary<long, JsonNode> nodes)
    {
        var raw = GetRawHandle(row);
        long id = Interop.NativeLibrary.traverse_result_get_node_id(raw, col);
        if (nodes.ContainsKey(id)) return;

        int labelCount = Interop.NativeLibrary.traverse_result_get_node_label_count(raw, col);
        var labels = new List<string>(labelCount);
        for (int i = 0; i < labelCount; i++)
        {
            var ptr = Interop.NativeLibrary.traverse_result_get_node_label(raw, col, i);
            if (ptr != IntPtr.Zero)
                labels.Add(Marshal.PtrToStringUTF8(ptr)!);
        }

        int propCount = Interop.NativeLibrary.traverse_result_get_node_prop_count(raw, col);
        var props = new Dictionary<string, string>(propCount);
        for (int i = 0; i < propCount; i++)
        {
            var kPtr = Interop.NativeLibrary.traverse_result_get_node_prop_key(raw, col, i);
            var vPtr = Interop.NativeLibrary.traverse_result_get_node_prop_value(raw, col, i);
            if (kPtr != IntPtr.Zero && vPtr != IntPtr.Zero)
            {
                var key = Marshal.PtrToStringUTF8(kPtr)!;
                var val = Marshal.PtrToStringUTF8(vPtr)!;
                props[key] = val;
            }
        }

        string? label = labels.Count > 0 ? labels[0] : null;
        nodes[id] = new JsonNode(id, labels, props, label);
    }

    private static void CollectEdge(Row row, int col, Dictionary<long, JsonEdge> edges)
    {
        var raw = GetRawHandle(row);
        long id = Interop.NativeLibrary.traverse_result_get_edge_id(raw, col);
        if (edges.ContainsKey(id)) return;

        var typePtr = Interop.NativeLibrary.traverse_result_get_edge_type_name(raw, col);
        var edgeType = typePtr != IntPtr.Zero ? Marshal.PtrToStringUTF8(typePtr)! : "";

        long source = Interop.NativeLibrary.traverse_result_get_edge_source(raw, col);
        long target = Interop.NativeLibrary.traverse_result_get_edge_target(raw, col);

        int propCount = Interop.NativeLibrary.traverse_result_get_node_prop_count(raw, col);
        var props = new Dictionary<string, string>(propCount);
        for (int i = 0; i < propCount; i++)
        {
            var kPtr = Interop.NativeLibrary.traverse_result_get_node_prop_key(raw, col, i);
            var vPtr = Interop.NativeLibrary.traverse_result_get_node_prop_value(raw, col, i);
            if (kPtr != IntPtr.Zero && vPtr != IntPtr.Zero)
            {
                var key = Marshal.PtrToStringUTF8(kPtr)!;
                var val = Marshal.PtrToStringUTF8(vPtr)!;
                props[key] = val;
            }
        }

        edges[id] = new JsonEdge(id, source, target, edgeType, props);
    }

    private static void CollectPath(
        Row row,
        int col,
        Dictionary<long, JsonNode> nodes,
        Dictionary<long, JsonEdge> edges)
    {
        // Paths interleave nodes and edges, and the graph accessors work on the outer
        // cell, not the inner nodes/edges, so the path accessors give IDs only (for
        // full node/edge data, UNWIND or return nodes/edges separately). Path node and
        // edge IDs are recorded so they show up in the graph: with labels/properties if
        // the entity was collected elsewhere, else as a stub.
        var raw = GetRawHandle(row);
        int pathLen = Interop.NativeLibrary.traverse_result_get_path_length(raw, col);

        for (int i = 0; i <= pathLen; i++)
        {
            long nodeId = Interop.NativeLibrary.traverse_result_get_path_node_id(raw, col, i);
            nodes.TryAdd(nodeId, new JsonNode(nodeId, [], new Dictionary<string, string>(), null));
        }

        for (int i = 0; i < pathLen; i++)
        {
            long edgeId = Interop.NativeLibrary.traverse_result_get_path_edge_id(raw, col, i);
            if (!edges.ContainsKey(edgeId))
            {
                // Stubs - we only know the edge exists, not its endpoints from this path accessor alone.
                // The path nodes at i and i+1 are the endpoints.
                long src = Interop.NativeLibrary.traverse_result_get_path_node_id(raw, col, i);
                long tgt = Interop.NativeLibrary.traverse_result_get_path_node_id(raw, col, i + 1);
                edges[edgeId] = new JsonEdge(edgeId, src, tgt, "", new Dictionary<string, string>());
            }
        }
    }

    private static IntPtr GetRawHandle(Row row)
    {
        // Access the internal result handle via reflection-free field.
        // Row stores the raw IntPtr as its first field.
        return row.GetResultHandle();
    }

    private static string WriteJson(IEnumerable<JsonNode> nodes, IEnumerable<JsonEdge> edges)
    {
        using var stream = new MemoryStream();
        using var writer = new Utf8JsonWriter(stream, new JsonWriterOptions { Indented = false });

        writer.WriteStartObject();

        writer.WriteStartArray("nodes");
        foreach (var n in nodes)
        {
            writer.WriteStartObject();
            writer.WriteString("key", $"n{n.Id}");

            writer.WriteStartObject("attributes");
            if (n.Label is not null)
                writer.WriteString("label", n.Label);
            foreach (var (key, val) in n.Properties)
                writer.WriteString(key, val);
            if (n.Labels.Count > 0)
            {
                writer.WriteStartArray("labels");
                foreach (var lbl in n.Labels)
                    writer.WriteStringValue(lbl);
                writer.WriteEndArray();
            }
            writer.WriteEndObject(); // attributes

            writer.WriteEndObject();
        }
        writer.WriteEndArray(); // nodes

        writer.WriteStartArray("edges");
        foreach (var e in edges)
        {
            writer.WriteStartObject();
            writer.WriteString("key", $"e{e.Id}");
            writer.WriteString("source", $"n{e.Source}");
            writer.WriteString("target", $"n{e.Target}");

            writer.WriteStartObject("attributes");
            if (!string.IsNullOrEmpty(e.EdgeType))
                writer.WriteString("label", e.EdgeType);
            foreach (var (key, val) in e.Properties)
                writer.WriteString(key, val);
            writer.WriteEndObject(); // attributes

            writer.WriteEndObject();
        }
        writer.WriteEndArray(); // edges

        writer.WriteEndObject();
        writer.Flush();

        return Encoding.UTF8.GetString(stream.ToArray());
    }

    private sealed record JsonNode(
        long Id,
        List<string> Labels,
        Dictionary<string, string> Properties,
        string? Label);

    private sealed record JsonEdge(
        long Id,
        long Source,
        long Target,
        string EdgeType,
        Dictionary<string, string> Properties);
}
