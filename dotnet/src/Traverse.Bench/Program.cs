using System.Diagnostics;
using System.Globalization;
using Traverse.Bench;

// -- CLI parsing ----------------------------------------------------------

string? engine = null, host = null, variant = "small", groups = null, output = null, auth = null;
int? port = null;
double duration = 10.0, warmup = 3;
bool listQueries = false;

for (int i = 0; i < args.Length; i++)
{
    switch (args[i])
    {
        case "--engine":   engine = args[++i]; break;
        case "--host":     host = args[++i]; break;
        case "--port":     port = int.Parse(args[++i], CultureInfo.InvariantCulture); break;
        case "--variant":  variant = args[++i]; break;
        case "--duration": duration = double.Parse(args[++i], CultureInfo.InvariantCulture); break;
        case "--warmup":   warmup = double.Parse(args[++i], CultureInfo.InvariantCulture); break;
        case "--groups":   groups = args[++i]; break;
        case "--output":   output = args[++i]; break;
        case "--auth":     auth = args[++i]; break;
        case "--list-queries": listQueries = true; break;
        default:
            Console.Error.WriteLine($"Unknown argument: {args[i]}");
            Environment.Exit(1);
            break;
    }
}

// -- List queries ---------------------------------------------------------

if (listQueries)
{
    Console.Error.WriteLine("Available query groups and queries:\n");
    foreach (var g in Queries.GroupOrder)
    {
        Console.Error.WriteLine($"  {g}:");
        foreach (var q in Queries.All)
            if (q.Group == g)
                Console.Error.WriteLine($"    {q.Name,-40} {q.Cypher}");
        Console.Error.WriteLine();
    }
    return;
}

// -- Validate engine ------------------------------------------------------

if (engine is null)
{
    Console.Error.WriteLine("Error: --engine is required. Choose: traverse-grpc, traverse-grpc-channel, traverse-bolt, traverse-native-bolt");
    Environment.Exit(1);
    return;
}

// Apply defaults per engine
host ??= engine switch
{
    "traverse-grpc" or "traverse-grpc-channel" => "localhost",
    _ => "127.0.0.1", // traverse-bolt, traverse-native-bolt
};

port ??= engine switch
{
    "traverse-grpc" or "traverse-grpc-channel" => 50051,
    "traverse-bolt" or "traverse-native-bolt" => 7690,
    _ => 7690,
};

string engineLabel = engine switch
{
    "traverse-grpc" => "dotnet_grpc",
    "traverse-grpc-channel" => "dotnet_grpc_channel",
    "traverse-bolt" => "dotnet_bolt",
    "traverse-native-bolt" => "dotnet_native_bolt",
    _ => engine,
};

// -- Validate variant -----------------------------------------------------

var vertexCount = Queries.VertexCount(variant);
if (vertexCount is null)
{
    Console.Error.WriteLine($"Error: unknown variant '{variant}'. Valid: small (10k), medium (100k), large (1.6M)");
    Environment.Exit(1);
    return;
}

long vc = vertexCount.Value;
int warmupIterations = (int)warmup;

// -- Resolve queries ------------------------------------------------------

var selected = Queries.Resolve(groups);
if (selected.Count == 0)
{
    Console.Error.WriteLine("No queries selected. Use --list-queries to see available queries.");
    Environment.Exit(1);
    return;
}

// -- Banner ---------------------------------------------------------------

Console.Error.WriteLine("============================================================");
Console.Error.WriteLine("Traverse.Bench (.NET)");
Console.Error.WriteLine("============================================================");
Console.Error.WriteLine($"  Engine:       {engine} ({engineLabel})");
Console.Error.WriteLine($"  Variant:      {variant} (~{vc} vertices)");
Console.Error.WriteLine($"  Duration:     {duration}s per query");
Console.Error.WriteLine($"  Warmup:       {warmupIterations} iterations");
Console.Error.WriteLine($"  Queries:      {selected.Count}");
Console.Error.WriteLine($"  Server:       {host}:{port}");

// -- Create driver factory ------------------------------------------------

async ValueTask<IBenchDriver> CreateDriver() => engine switch
{
    "traverse-grpc" => await TraverseGrpcDriver.ConnectAsync(host, port.Value, auth),
    "traverse-grpc-channel" => await TraverseGrpcChannelDriver.ConnectAsync(host, port.Value, auth),
    "traverse-bolt" => await BoltDriver.ConnectAsync(host, port.Value, auth),
    "traverse-native-bolt" => await TraverseNativeBoltDriver.ConnectAsync(host, port.Value, auth),
    _ => throw new ArgumentException($"Unknown engine: {engine}"),
};

// -- Benchmark loop -------------------------------------------------------

Console.Error.WriteLine("\n-- Benchmark -------------------------------------------");

var results = new List<QueryResult>();
var rng = new Random(42);

foreach (var query in selected)
{
    Console.Error.WriteLine($"\n  [{query.Name}]");
    string cypherPreview = query.Cypher.Length > 80 ? query.Cypher[..80] + "..." : query.Cypher;
    Console.Error.WriteLine($"    Query: {cypherPreview}");

    // -- Warmup --
    await using (var warmupDriver = await CreateDriver())
    {
        for (int w = 0; w < warmupIterations; w++)
        {
            try
            {
                var p = Queries.GenerateParams(query.Params, vc, rng);
                await warmupDriver.RunAndDrainAsync(query.Cypher, p);
            }
            catch
            {
                // swallow warmup errors
            }
        }
    }

    // -- Memory baseline --
    long gcBefore = GC.GetAllocatedBytesForCurrentThread();
    long rssBefore = Environment.WorkingSet;
    int gen0Before = GC.CollectionCount(0);
    int gen1Before = GC.CollectionCount(1);
    int gen2Before = GC.CollectionCount(2);

    // -- Measurement --
    var latencies = new List<double>();
    long errors = 0;

    await using var driver = await CreateDriver();
    var deadline = Stopwatch.GetTimestamp() + (long)(duration * Stopwatch.Frequency);
    var wallStart = Stopwatch.GetTimestamp();

    while (Stopwatch.GetTimestamp() < deadline)
    {
        var p = Queries.GenerateParams(query.Params, vc, rng);
        long t0 = Stopwatch.GetTimestamp();
        try
        {
            await driver.RunAndDrainAsync(query.Cypher, p);
            long t1 = Stopwatch.GetTimestamp();
            double ms = (t1 - t0) * 1000.0 / Stopwatch.Frequency;
            latencies.Add(ms);
        }
        catch
        {
            errors++;
        }
    }

    double wallSecs = (Stopwatch.GetTimestamp() - wallStart) * 1.0 / Stopwatch.Frequency;

    // -- Memory delta --
    long gcAfter = GC.GetAllocatedBytesForCurrentThread();
    long rssAfter = Environment.WorkingSet;
    var memory = new MemoryStats
    {
        GcAllocatedBytes = gcAfter - gcBefore,
        RssDeltaBytes = rssAfter - rssBefore,
        GcGen0 = GC.CollectionCount(0) - gen0Before,
        GcGen1 = GC.CollectionCount(1) - gen1Before,
        GcGen2 = GC.CollectionCount(2) - gen2Before,
    };

    var result = Stats.MergeLatencies(latencies, errors, wallSecs, query.Name, memory);

    // -- Per-query summary --
    if (result.Status == "ok")
    {
        var lat = result.LatencyMs!;
        Console.Error.WriteLine($"    Iterations: {result.Iterations} | Errors: {result.Errors} | Throughput: {result.ThroughputQps:F1} qps");
        Console.Error.WriteLine($"    Latency (ms): avg={lat.Avg}, p50={lat.P50}, p95={lat.P95}, p99={lat.P99}, max={lat.Max}");
        Console.Error.WriteLine($"    Memory: GC={memory.GcAllocatedBytes / 1_048_576.0:F1} MB, RSS delta={memory.RssDeltaBytes / 1_048_576.0:F1} MB, Gen0={memory.GcGen0} Gen1={memory.GcGen1} Gen2={memory.GcGen2}");
    }
    else
    {
        Console.Error.WriteLine($"    SKIPPED: {result.Errors} errors");
    }

    results.Add(result);
}

// -- Output ---------------------------------------------------------------

var benchOutput = new BenchmarkOutput
{
    Benchmark = "pokec",
    Engine = engineLabel,
    Variant = variant,
    VertexCount = vc,
    DurationPerQuery = duration,
    WarmupIterations = warmupIterations,
    Results = results,
};

Output.PrintSummary(benchOutput);
Output.Write(benchOutput, output);
