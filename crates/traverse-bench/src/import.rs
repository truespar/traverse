//! Cypher file import via Bolt with pipelined batching.
//!
//! Reads a `.cypher` file and sends statements in pipelined batches
//! (many RUN+PULL pairs per TCP flush), eliminating per-statement
//! round-trip latency.

use std::io::BufRead;
use std::time::Instant;

use crate::bolt::BoltConnection;

/// Pipeline batch size - number of statements sent before flushing
/// and draining responses.  512 statements ~ 100-250 KB of Bolt
/// messages, well within typical TCP buffer sizes.
const BATCH_SIZE: usize = 512;

pub struct ImportStats {
    pub statements: u64,
    pub errors: u64,
    pub elapsed_secs: f64,
}

/// Import a `.cypher` file using pipelined Bolt batches.
pub async fn run_import(
    conn: &mut BoltConnection,
    cypher_path: &str,
) -> Result<ImportStats, String> {
    // Count lines for progress reporting
    let file = std::fs::File::open(cypher_path)
        .map_err(|e| format!("failed to open {cypher_path}: {e}"))?;
    let total_lines = std::io::BufReader::new(file).lines().count() as u64;

    eprintln!("  Importing {total_lines} statements from {cypher_path} ...");
    eprintln!("  Pipeline batch size: {BATCH_SIZE}");

    let file = std::fs::File::open(cypher_path)
        .map_err(|e| format!("failed to open {cypher_path}: {e}"))?;
    let reader = std::io::BufReader::new(file);

    let mut imported: u64 = 0;
    let mut errors: u64 = 0;
    let mut batch: Vec<String> = Vec::with_capacity(BATCH_SIZE);
    let t0 = Instant::now();

    for line in reader.lines() {
        let line = line.map_err(|e| format!("read error at line {}: {e}", imported + 1))?;
        let stmt = line.trim().to_string();
        if stmt.is_empty() || stmt.starts_with("//") {
            imported += 1;
            continue;
        }

        batch.push(stmt);

        if batch.len() >= BATCH_SIZE {
            let (ok, errs) = flush_batch(conn, &batch).await?;
            imported += batch.len() as u64;
            errors += errs as u64;
            let _ = ok; // counted implicitly
            batch.clear();

            if imported % 10_000 < BATCH_SIZE as u64 {
                let elapsed = t0.elapsed().as_secs_f64();
                let rate = imported as f64 / elapsed;
                eprint!("\r  {imported}/{total_lines} ({rate:.0} stmt/s)");
            }
        }
    }

    // Flush remaining
    if !batch.is_empty() {
        let (_ok, errs) = flush_batch(conn, &batch).await?;
        imported += batch.len() as u64;
        errors += errs as u64;
    }

    let elapsed = t0.elapsed().as_secs_f64();
    let rate = imported as f64 / elapsed;
    eprintln!(
        "\n  Import complete: {imported} statements, {errors} errors, {elapsed:.1}s ({rate:.0} stmt/s)"
    );

    Ok(ImportStats {
        statements: imported,
        errors,
        elapsed_secs: elapsed,
    })
}

/// Enqueue a batch of statements, flush, and drain all responses.
async fn flush_batch(
    conn: &mut BoltConnection,
    batch: &[String],
) -> Result<(usize, usize), String> {
    // Enqueue all RUN+PULL pairs without flushing
    for stmt in batch {
        conn.enqueue_run_pull_no_params(stmt)
            .await
            .map_err(|e| format!("enqueue failed: {e}"))?;
    }

    // Single flush for the entire batch
    conn.flush()
        .await
        .map_err(|e| format!("flush failed: {e}"))?;

    // Drain all responses
    conn.drain_import_batch(batch.len())
        .await
        .map_err(|e| format!("drain failed: {e}"))
}

/// Create indexes for the Pokec dataset.
///
/// Tries Traverse/Neo4j syntax first, falls back to Memgraph syntax.
pub async fn create_indexes(conn: &mut BoltConnection) {
    match conn
        .run_and_pull(
            "CREATE INDEX user_id IF NOT EXISTS FOR (n:User) ON (n.id)",
            &[],
        )
        .await
    {
        Ok(()) => {
            eprintln!("  Index created: user_id on User(id)");
            // Also create the age index for range queries (Q19)
            match conn
                .run_and_pull(
                    "CREATE INDEX user_age IF NOT EXISTS FOR (n:User) ON (n.age)",
                    &[],
                )
                .await
            {
                Ok(()) => {
                    eprintln!("  Index created: user_age on User(age)");
                }
                Err(e) => {
                    eprintln!("  Index User(age) skipped ({e})");
                    let _ = conn.reset().await;
                }
            }
            return;
        }
        Err(_) => {
            let _ = conn.reset().await;
        }
    }
    // Fallback: Memgraph syntax
    match conn.run_and_pull("CREATE INDEX ON :User(id)", &[]).await {
        Ok(()) => {
            eprintln!("  Index created (Memgraph syntax): User(id)");
        }
        Err(e) => {
            eprintln!("  Index skipped ({e})");
            let _ = conn.reset().await;
        }
    }
    match conn.run_and_pull("CREATE INDEX ON :User(age)", &[]).await {
        Ok(()) => {
            eprintln!("  Index created (Memgraph syntax): User(age)");
        }
        Err(e) => {
            eprintln!("  Index User(age) skipped ({e})");
            let _ = conn.reset().await;
        }
    }
}

/// Ensure all required indexes exist on a pre-built database.
/// Idempotent: skips indexes that already exist.
pub async fn ensure_indexes(conn: &mut BoltConnection) {
    // User(id) index
    match conn
        .run_and_pull(
            "CREATE INDEX user_id IF NOT EXISTS FOR (n:User) ON (n.id)",
            &[],
        )
        .await
    {
        Ok(()) => eprintln!("  Index ensured: user_id on User(id)"),
        Err(e) => {
            eprintln!("  Index User(id) skipped ({e})");
            let _ = conn.reset().await;
        }
    }
    // User(age) index for range queries (Q19)
    match conn
        .run_and_pull(
            "CREATE INDEX user_age IF NOT EXISTS FOR (n:User) ON (n.age)",
            &[],
        )
        .await
    {
        Ok(()) => eprintln!("  Index ensured: user_age on User(age)"),
        Err(e) => {
            eprintln!("  Index User(age) skipped ({e})");
            let _ = conn.reset().await;
        }
    }
}
