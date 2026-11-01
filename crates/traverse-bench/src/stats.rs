use serde::Serialize;

use crate::profiler::ResourceStats;

/// Result of benchmarking a single query.
#[derive(Debug, Serialize)]
pub struct QueryResult {
    pub query_name: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iterations: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_sec: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub throughput_qps: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<LatencyStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<ResourceStats>,
}

#[derive(Debug, Serialize)]
pub struct LatencyStats {
    pub min: f64,
    pub avg: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}

/// Compute the p-th percentile (0-100) of a sorted slice.
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let k = (sorted.len() - 1) as f64 * p / 100.0;
    let f = k as usize;
    let c = f + 1;
    if c >= sorted.len() {
        sorted[f]
    } else {
        sorted[f] + (k - f as f64) * (sorted[c] - sorted[f])
    }
}

/// Merge worker latency vectors, compute stats.
pub fn merge_latencies(
    mut all_latencies: Vec<f64>,
    total_errors: u64,
    wall_secs: f64,
    concurrency: u32,
    query_name: &str,
    resources: Option<ResourceStats>,
) -> QueryResult {
    let iterations = all_latencies.len() as u64;

    if iterations == 0 {
        return QueryResult {
            query_name: query_name.to_string(),
            status: "error".into(),
            iterations: None,
            errors: Some(total_errors),
            duration_sec: None,
            throughput_qps: None,
            concurrency: Some(concurrency),
            latency_ms: None,
            resources,
        };
    }

    all_latencies.sort_unstable_by(|a, b| a.partial_cmp(b).unwrap());

    let avg = all_latencies.iter().sum::<f64>() / iterations as f64;
    let qps = iterations as f64 / wall_secs;

    QueryResult {
        query_name: query_name.to_string(),
        status: "ok".into(),
        iterations: Some(iterations),
        errors: Some(total_errors),
        duration_sec: Some(round3(wall_secs)),
        throughput_qps: Some(round2(qps)),
        concurrency: Some(concurrency),
        latency_ms: Some(LatencyStats {
            min: round3(all_latencies[0]),
            avg: round3(avg),
            p50: round3(percentile(&all_latencies, 50.0)),
            p95: round3(percentile(&all_latencies, 95.0)),
            p99: round3(percentile(&all_latencies, 99.0)),
            max: round3(*all_latencies.last().unwrap()),
        }),
        resources,
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}
