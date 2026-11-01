namespace Traverse.Bench;

internal static class Stats
{
    /// <summary>Compute the p-th percentile (0-100) of a sorted span. Matches stats.rs algorithm.</summary>
    public static double Percentile(ReadOnlySpan<double> sorted, double p)
    {
        if (sorted.IsEmpty) return 0.0;
        double k = (sorted.Length - 1) * p / 100.0;
        int f = (int)k;
        int c = f + 1;
        if (c >= sorted.Length) return sorted[f];
        return sorted[f] + (k - f) * (sorted[c] - sorted[f]);
    }

    /// <summary>Merge latency samples into a QueryResult. Matches stats.rs merge_latencies.</summary>
    public static QueryResult MergeLatencies(
        List<double> latencies,
        long totalErrors,
        double wallSecs,
        string queryName,
        MemoryStats? memory)
    {
        long iterations = latencies.Count;

        if (iterations == 0)
        {
            return new QueryResult
            {
                QueryName = queryName,
                Status = "error",
                Errors = totalErrors,
            };
        }

        latencies.Sort();
        var span = System.Runtime.InteropServices.CollectionsMarshal.AsSpan(latencies);

        double avg = 0;
        foreach (double v in span) avg += v;
        avg /= iterations;
        double qps = iterations / wallSecs;

        return new QueryResult
        {
            QueryName = queryName,
            Status = "ok",
            Iterations = iterations,
            Errors = totalErrors,
            DurationSec = Round3(wallSecs),
            ThroughputQps = Round2(qps),
            LatencyMs = new LatencyStats
            {
                Min = Round3(span[0]),
                Avg = Round3(avg),
                P50 = Round3(Percentile(span, 50.0)),
                P95 = Round3(Percentile(span, 95.0)),
                P99 = Round3(Percentile(span, 99.0)),
                Max = Round3(span[^1]),
            },
            Memory = memory,
        };
    }

    private static double Round2(double v) => Math.Round(v * 100.0) / 100.0;
    private static double Round3(double v) => Math.Round(v * 1000.0) / 1000.0;
}

/// <summary>.NET-specific memory stats captured per query.</summary>
internal sealed class MemoryStats
{
    public long GcAllocatedBytes { get; init; }
    public long RssDeltaBytes { get; init; }
    public int GcGen0 { get; init; }
    public int GcGen1 { get; init; }
    public int GcGen2 { get; init; }
}
