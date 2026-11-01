use serde::Serialize;

use crate::profiler::ResourceStats;
use crate::stats::QueryResult;

/// Top-level benchmark output.
#[derive(Debug, Serialize)]
pub struct BenchmarkOutput {
    pub benchmark: String,
    pub variant: String,
    pub vertex_count: i64,
    pub duration_per_query: f64,
    pub warmup_iterations: u32,
    pub concurrency: u32,
    pub results: Vec<QueryResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceStats>,
}

/// Write results to a JSON file or stdout.
pub fn write_output(output: &BenchmarkOutput, path: Option<&str>) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(output)?;
    if let Some(p) = path {
        std::fs::write(p, &json)?;
        eprintln!("  Results written to {p}");
    } else {
        println!("{json}");
    }
    Ok(())
}

/// Print a summary table to stderr.
pub fn print_summary(output: &BenchmarkOutput) {
    let has_resources = output.results.iter().any(|r| r.resources.is_some());

    eprintln!("\n-- Summary ---------------------------------------------");
    if has_resources {
        eprintln!(
            "  {:<35} {:>8} {:>8} {:>8} {:>7} {:>7} {:>8}",
            "Query", "QPS", "Avg(ms)", "P99(ms)", "CPU%", "RSS MB", "Status"
        );
        eprintln!("  {}", "-".repeat(91));
    } else {
        eprintln!(
            "  {:<35} {:>8} {:>8} {:>8} {:>8}",
            "Query", "QPS", "Avg(ms)", "P99(ms)", "Status"
        );
        eprintln!("  {}", "-".repeat(75));
    }

    for r in &output.results {
        if r.status == "ok" {
            let lat = r.latency_ms.as_ref().unwrap();
            let qps = r.throughput_qps.unwrap_or(0.0);
            if has_resources {
                if let Some(res) = &r.resources {
                    eprintln!(
                        "  {:<35} {:>8.1} {:>8.1} {:>8.1} {:>7.1} {:>7.1} {:>8}",
                        r.query_name, qps, lat.avg, lat.p99, res.cpu_pct.avg, res.rss_mb.avg, "ok"
                    );
                } else {
                    eprintln!(
                        "  {:<35} {:>8.1} {:>8.1} {:>8.1} {:>7} {:>7} {:>8}",
                        r.query_name, qps, lat.avg, lat.p99, "--", "--", "ok"
                    );
                }
            } else {
                eprintln!(
                    "  {:<35} {:>8.1} {:>8.1} {:>8.1} {:>8}",
                    r.query_name, qps, lat.avg, lat.p99, "ok"
                );
            }
        } else if has_resources {
            eprintln!(
                "  {:<35} {:>8} {:>8} {:>8} {:>7} {:>7} {:>8}",
                r.query_name, "", "", "", "", "", "ERROR"
            );
        } else {
            eprintln!(
                "  {:<35} {:>8} {:>8} {:>8} {:>8}",
                r.query_name, "", "", "", "ERROR"
            );
        }
    }

    if let Some(res) = &output.resources {
        eprintln!(
            "\n  Overall server resources ({} samples, {}s):",
            res.samples, res.duration_sec
        );
        eprintln!(
            "    CPU%: avg={}, p50={}, p95={}, max={}",
            res.cpu_pct.avg, res.cpu_pct.p50, res.cpu_pct.p95, res.cpu_pct.max
        );
        eprintln!(
            "    RSS:  avg={} MB, max={} MB, final={} MB",
            res.rss_mb.avg, res.rss_mb.max, res.rss_mb.final_val
        );
        eprintln!("    Threads: {}-{}", res.threads.min, res.threads.max);
    }
}
