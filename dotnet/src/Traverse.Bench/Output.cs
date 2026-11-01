using System.Text.Json;
using System.Text.Json.Serialization;

namespace Traverse.Bench;

/// <summary>Top-level benchmark output (matches Rust BenchmarkOutput).</summary>
internal sealed class BenchmarkOutput
{
    public string Benchmark { get; init; } = "pokec";
    public string Engine { get; init; } = "";
    public string Variant { get; init; } = "";
    public long VertexCount { get; init; }
    public double DurationPerQuery { get; init; }
    public int WarmupIterations { get; init; }
    public List<QueryResult> Results { get; init; } = [];
}

/// <summary>Per-query result (matches Rust QueryResult + optional memory block).</summary>
internal sealed class QueryResult
{
    public string QueryName { get; init; } = "";
    public string Status { get; init; } = "";
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public long? Iterations { get; init; }
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public long? Errors { get; init; }
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public double? DurationSec { get; init; }
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public double? ThroughputQps { get; init; }
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public LatencyStats? LatencyMs { get; init; }
    [JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]
    public MemoryStats? Memory { get; init; }
}

internal sealed class LatencyStats
{
    public double Min { get; init; }
    public double Avg { get; init; }
    public double P50 { get; init; }
    public double P95 { get; init; }
    public double P99 { get; init; }
    public double Max { get; init; }
}

internal static class Output
{
    private static readonly JsonSerializerOptions s_jsonOptions = new()
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        WriteIndented = true,
        DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull,
    };

    public static void Write(BenchmarkOutput output, string? path)
    {
        string json = JsonSerializer.Serialize(output, s_jsonOptions);
        if (path is not null)
        {
            File.WriteAllText(path, json);
            Console.Error.WriteLine($"  Results written to {path}");
        }
        else
        {
            Console.WriteLine(json);
        }
    }

    public static void PrintSummary(BenchmarkOutput output)
    {
        bool hasMem = output.Results.Any(r => r.Memory is not null);

        Console.Error.WriteLine();
        Console.Error.WriteLine("-- Summary ---------------------------------------------");
        if (hasMem)
        {
            Console.Error.WriteLine($"  {"Query",-35} {"QPS",8} {"Avg(ms)",8} {"P99(ms)",8} {"GC MB",7} {"Status",8}");
            Console.Error.WriteLine($"  {new string('-', 82)}");
        }
        else
        {
            Console.Error.WriteLine($"  {"Query",-35} {"QPS",8} {"Avg(ms)",8} {"P99(ms)",8} {"Status",8}");
            Console.Error.WriteLine($"  {new string('-', 75)}");
        }

        foreach (var r in output.Results)
        {
            if (r.Status == "ok")
            {
                var lat = r.LatencyMs!;
                double qps = r.ThroughputQps ?? 0;
                if (hasMem)
                {
                    string gcMb = r.Memory is not null
                        ? $"{r.Memory.GcAllocatedBytes / 1_048_576.0:F1}"
                        : "--";
                    Console.Error.WriteLine($"  {r.QueryName,-35} {qps,8:F1} {lat.Avg,8:F1} {lat.P99,8:F1} {gcMb,7} {"ok",8}");
                }
                else
                {
                    Console.Error.WriteLine($"  {r.QueryName,-35} {qps,8:F1} {lat.Avg,8:F1} {lat.P99,8:F1} {"ok",8}");
                }
            }
            else
            {
                if (hasMem)
                    Console.Error.WriteLine($"  {r.QueryName,-35} {"",8} {"",8} {"",8} {"",7} {"ERROR",8}");
                else
                    Console.Error.WriteLine($"  {r.QueryName,-35} {"",8} {"",8} {"",8} {"ERROR",8}");
            }
        }
    }
}
