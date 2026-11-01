use std::time::Instant;

use rand::SeedableRng;
use rand::rngs::StdRng;

use crate::bolt::{BoltConnection, BoltError};
use crate::queries::{QueryDef, generate_params};

/// Result from a single worker's measurement loop.
pub struct WorkerResult {
    pub latencies: Vec<f64>,
    pub errors: u64,
}

/// Run a single worker loop until the deadline.
pub async fn run_worker(
    mut conn: BoltConnection,
    query: QueryDef,
    vertex_count: i64,
    deadline: Instant,
) -> WorkerResult {
    let mut rng = StdRng::from_rng(&mut rand::rng());
    let mut latencies = Vec::with_capacity(4096);
    let mut errors: u64 = 0;

    while Instant::now() < deadline {
        let params = generate_params(query.params, vertex_count, &mut rng);
        let param_refs: Vec<(&str, i64)> = params.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        let t0 = Instant::now();

        match conn.run_and_pull(query.cypher, &param_refs).await {
            Ok(()) => {
                let elapsed_ms = t0.elapsed().as_secs_f64() * 1000.0;
                latencies.push(elapsed_ms);
            }
            Err(BoltError::ServerFailure(_)) => {
                errors += 1;
                // Try to recover
                let _ = conn.reset().await;
                if errors > 100 && latencies.is_empty() {
                    break;
                }
            }
            Err(_) => {
                errors += 1;
                if errors > 100 && latencies.is_empty() {
                    break;
                }
            }
        }
    }

    WorkerResult { latencies, errors }
}

/// Run serial warmup iterations on a single connection.
pub async fn run_warmup(
    conn: &mut BoltConnection,
    query: &QueryDef,
    vertex_count: i64,
    iterations: u32,
) {
    let mut rng = StdRng::from_rng(&mut rand::rng());
    let mut ok = 0u32;
    let mut err = 0u32;
    for _ in 0..iterations {
        let params = generate_params(query.params, vertex_count, &mut rng);
        let param_refs: Vec<(&str, i64)> = params.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        match conn.run_and_pull(query.cypher, &param_refs).await {
            Ok(()) => ok += 1,
            Err(_) => {
                err += 1;
                let _ = conn.reset().await;
            }
        }
    }
    if err > 0 {
        eprintln!("    Warmup: {ok}/{iterations} ok");
    }
}

/// Create a Bolt connection.
pub async fn create_connection(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
) -> Result<BoltConnection, BoltError> {
    BoltConnection::connect(host, port, user, password).await
}
