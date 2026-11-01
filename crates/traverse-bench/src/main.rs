mod bolt;
mod download;
mod import;
mod output;
mod profiler;
mod queries;
mod stats;
mod worker;

use std::time::Instant;

use clap::{CommandFactory, Parser};

use output::{BenchmarkOutput, print_summary, write_output};
use profiler::ResourceSampler;
use queries::{resolve_queries, vertex_count};
use stats::merge_latencies;
use worker::{create_connection, run_warmup, run_worker};

#[derive(Parser)]
#[command(
    name = "traverse-bench",
    about = "Traverse benchmark and import tool",
    version = concat!(env!("CARGO_PKG_VERSION"), "\n", "Copyright (c) 2026 The Intelligence Company AB (publ). MIT OR Apache-2.0."),
    before_help = concat!("traverse-bench ", env!("CARGO_PKG_VERSION")),
)]
struct Cli {
    /// Dataset variant: small (10k), medium (100k), large (1.6M vertices)
    #[arg(long, default_value = "small")]
    variant: String,

    /// Measurement duration per query (seconds)
    #[arg(long, default_value_t = 10.0)]
    duration: f64,

    /// Warmup iterations per query
    #[arg(long, default_value_t = 3)]
    warmup: u32,

    /// Number of concurrent workers
    #[arg(long, default_value_t = 1)]
    concurrency: u32,

    /// Comma-separated query groups or names (groups: read, expansion, scan, aggregate,
    /// shortest_path, update, write)
    #[arg(long)]
    groups: Option<String>,

    /// List all available queries and groups, then exit
    #[arg(long)]
    list_queries: bool,

    /// Bolt server hostname
    #[arg(long, default_value = "127.0.0.1")]
    host: String,

    /// Bolt port
    #[arg(long, default_value_t = 7690)]
    port: u16,

    /// Auth as user:password
    #[arg(long)]
    auth: Option<String>,

    /// Server PID for resource profiling (auto-detected if omitted)
    #[arg(long)]
    server_pid: Option<u32>,

    /// Output JSON file path
    #[arg(short, long)]
    output: Option<String>,

    /// Import a .cypher file and exit (no benchmarking)
    #[arg(long)]
    import: Option<String>,

    /// Download pokec dataset and exit (variant: small, medium, large)
    #[arg(long)]
    download: Option<String>,

    /// Download format: cypher (for Traverse/Memgraph Bolt import) or csv (for Neo4j native import)
    #[arg(long, default_value = "cypher")]
    format: String,

    /// Output directory for downloaded datasets [default: benchmarks/data/dataset_cache]
    #[arg(long)]
    download_dir: Option<String>,
}

#[tokio::main]
async fn main() {
    // No arguments -> show help (same as --help) and exit
    if std::env::args().len() <= 1 {
        Cli::command().print_help().ok();
        return;
    }

    let cli = Cli::parse();

    // -- List queries mode ---------------------------------------------
    if cli.list_queries {
        eprintln!("Available query groups and queries:\n");
        for &group in queries::GROUP_ORDER {
            eprintln!("  {group}:");
            for q in queries::QUERIES {
                if q.group == group {
                    eprintln!("    {:<40} {}", q.name, q.cypher);
                }
            }
            eprintln!();
        }
        return;
    }

    // -- Download mode ------------------------------------------------
    if let Some(ref variant) = cli.download {
        let dir = cli
            .download_dir
            .as_deref()
            .unwrap_or("benchmarks/data/dataset_cache");

        eprintln!("============================================================");
        eprintln!("Traverse Bench v{} - Download", env!("CARGO_PKG_VERSION"));
        eprintln!("============================================================");
        eprintln!("  Variant:  {variant}");
        eprintln!("  Format:   {}", cli.format);

        let result = match cli.format.as_str() {
            "cypher" => download::download_dataset(variant, dir),
            "csv" => download::download_csv(variant, dir),
            other => {
                eprintln!("Unknown format '{other}'. Valid formats: cypher, csv");
                std::process::exit(1);
            }
        };
        if let Err(e) = result {
            eprintln!("Download failed: {e}");
            std::process::exit(1);
        }
        return;
    }

    let (user, password) = parse_auth(cli.auth.as_deref());
    let user = user.unwrap_or("");
    let password = password.unwrap_or("");

    // -- Import mode --------------------------------------------------
    if let Some(ref import_arg) = cli.import {
        // If the argument is a variant name (small/medium/large), auto-download
        let import_path = match import_arg.as_str() {
            "small" | "medium" | "large" => {
                let dir = cli
                    .download_dir
                    .as_deref()
                    .unwrap_or("benchmarks/data/dataset_cache");
                match download::download_dataset(import_arg, dir) {
                    Ok(path) => path,
                    Err(e) => {
                        eprintln!("Dataset download failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            _ => import_arg.clone(),
        };

        eprintln!("============================================================");
        eprintln!("Traverse Bench v{} - Import", env!("CARGO_PKG_VERSION"));
        eprintln!("============================================================");
        eprintln!("  Server:   {}:{}", cli.host, cli.port);
        eprintln!("  File:     {import_path}");

        let mut conn = create_connection(&cli.host, cli.port, user, password)
            .await
            .expect("failed to connect for import");

        eprintln!("\n-- Index Creation --------------------------------------");
        import::create_indexes(&mut conn).await;

        eprintln!("\n-- Data Import -----------------------------------------");
        match import::run_import(&mut conn, &import_path).await {
            Ok(stats) => {
                eprintln!(
                    "  {} statements in {:.1}s",
                    stats.statements, stats.elapsed_secs
                );
                if stats.errors > 0 {
                    eprintln!(
                        "  Warning: {}/{} statements had errors",
                        stats.errors, stats.statements
                    );
                }
            }
            Err(e) => {
                eprintln!("Import failed: {e}");
                std::process::exit(1);
            }
        }

        return;
    }

    // -- Benchmark mode -----------------------------------------------
    let vc = vertex_count(&cli.variant);
    if vc.is_none() {
        eprintln!(
            "Error: unknown variant '{}'. Valid variants: small (10k), medium (100k), large (1.6M)",
            cli.variant
        );
        std::process::exit(1);
    }
    let vc = vc.unwrap();

    let selected = resolve_queries(cli.groups.as_deref());

    if selected.is_empty() {
        eprintln!("No queries selected. Use --list-queries to see available queries.");
        std::process::exit(1);
    }

    eprintln!("============================================================");
    eprintln!("Traverse Bench v{}", env!("CARGO_PKG_VERSION"));
    eprintln!("============================================================");
    eprintln!("  Variant:      {} (~{vc} vertices)", cli.variant);
    eprintln!("  Duration:     {}s per query", cli.duration);
    eprintln!("  Warmup:       {} iterations", cli.warmup);
    eprintln!("  Concurrency:  {}", cli.concurrency);
    eprintln!("  Queries:      {}", selected.len());
    eprintln!("  Server:       {}:{}", cli.host, cli.port);

    // -- Resource profiling setup --
    let server_pid = cli
        .server_pid
        .or_else(|| profiler::find_server_pid(cli.port));
    let sampler = server_pid.map(|pid| {
        eprintln!("\n  Resource profiling: pid={pid}");
        let mut s = ResourceSampler::new(pid);
        s.start();
        s
    });
    if sampler.is_none() {
        eprintln!("\n  Resource profiling: server PID not found, skipping");
    }

    // -- Create warmup connection --
    let mut warmup_conn = create_connection(&cli.host, cli.port, user, password)
        .await
        .expect("failed to connect for warmup");

    // -- Ensure indexes exist (idempotent) --
    import::ensure_indexes(&mut warmup_conn).await;

    eprintln!("\n-- Benchmark -------------------------------------------");

    let mut results = Vec::new();

    for query in &selected {
        eprintln!("\n  [{}]", query.name);
        let cypher_preview = if query.cypher.len() > 80 {
            format!("{}...", &query.cypher[..80])
        } else {
            query.cypher.to_string()
        };
        eprintln!("    Query: {cypher_preview}");

        // Warmup (serial, single connection)
        run_warmup(&mut warmup_conn, query, vc, cli.warmup).await;

        // Mark query start for profiling
        if let Some(ref s) = sampler {
            s.mark_query_start();
        }

        // Create W connections in parallel
        let mut conn_handles = Vec::new();
        for _ in 0..cli.concurrency {
            let h = cli.host.clone();
            let p = cli.port;
            let u = user.to_string();
            let pw = password.to_string();
            conn_handles.push(tokio::spawn(async move {
                create_connection(&h, p, &u, &pw).await
            }));
        }
        let mut connections = Vec::with_capacity(cli.concurrency as usize);
        for h in conn_handles {
            connections.push(
                h.await
                    .unwrap()
                    .expect("failed to create worker connection"),
            );
        }

        // Start measurement
        let wall_start = Instant::now();
        let deadline = wall_start + std::time::Duration::from_secs_f64(cli.duration);

        // Spawn workers
        let mut handles = Vec::new();
        for conn in connections {
            let q = (*query).clone();
            handles.push(tokio::spawn(async move {
                run_worker(conn, q, vc, deadline).await
            }));
        }

        // Collect results
        let mut all_latencies = Vec::new();
        let mut total_errors: u64 = 0;
        for handle in handles {
            let wr = handle.await.expect("worker task panicked");
            total_errors += wr.errors;
            all_latencies.extend(wr.latencies);
        }

        let wall_secs = wall_start.elapsed().as_secs_f64();

        // Resource stats for this query
        let resources = sampler.as_ref().and_then(|s| s.mark_query_end());

        let result = merge_latencies(
            all_latencies,
            total_errors,
            wall_secs,
            cli.concurrency,
            query.name,
            resources,
        );

        // Print per-query summary
        if result.status == "ok" {
            let lat = result.latency_ms.as_ref().unwrap();
            eprintln!(
                "    Iterations: {} | Errors: {} | Throughput: {:.1} qps",
                result.iterations.unwrap_or(0),
                result.errors.unwrap_or(0),
                result.throughput_qps.unwrap_or(0.0)
            );
            eprintln!(
                "    Latency (ms): avg={}, p50={}, p95={}, p99={}, max={}",
                lat.avg, lat.p50, lat.p95, lat.p99, lat.max
            );
        } else {
            eprintln!("    SKIPPED: {} errors", result.errors.unwrap_or(0));
        }

        results.push(result);
    }

    // -- Overall resource stats --
    let overall_resources = sampler.as_ref().and_then(|s| s.get_overall_stats());
    if let Some(ref s) = sampler {
        s.stop();
    }

    // -- Output --
    let bench_output = BenchmarkOutput {
        benchmark: "pokec".into(),
        variant: cli.variant.clone(),
        vertex_count: vc,
        duration_per_query: cli.duration,
        warmup_iterations: cli.warmup,
        concurrency: cli.concurrency,
        results,
        resources: overall_resources,
    };

    print_summary(&bench_output);

    if let Err(e) = write_output(&bench_output, cli.output.as_deref()) {
        eprintln!("Error writing output: {e}");
        std::process::exit(1);
    }
}

fn parse_auth(auth: Option<&str>) -> (Option<&str>, Option<&str>) {
    match auth {
        Some(s) => {
            let mut parts = s.splitn(2, ':');
            let user = parts.next().unwrap();
            let pass = parts.next().unwrap_or("");
            (Some(user), Some(pass))
        }
        None => (None, None),
    }
}
