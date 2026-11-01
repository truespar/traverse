//! Stopping queries through their `QueryContext`: deadlines reach rayon workers and algorithm
//! kernels, nested time limits (`apoc.cypher.runTimeboxed`) keep the outer one, and a stop never
//! passes for success.

use std::time::{Duration, Instant};

use traverse_core::graph::Graph;
use traverse_cypher::error::CypherError;
use traverse_cypher::execute;
use traverse_cypher::query::{Interrupt, QueryContext, QueryOptions};
use traverse_cypher::types::CypherValue;

fn timed_out(err: &CypherError) -> bool {
    matches!(
        err,
        CypherError::Interrupted {
            reason: Interrupt::TimedOut,
            ..
        }
    )
}

/// Runs `q` under a fresh query with `timeout`.
fn run_with_timeout(
    g: &mut Graph,
    q: &str,
    timeout: Option<Duration>,
) -> (Result<Vec<Vec<CypherValue>>, CypherError>, Duration) {
    let query = QueryContext::new(QueryOptions {
        timeout,
        ..QueryOptions::default()
    });
    let _scope = query.enter();
    let started = Instant::now();
    let result = execute(g, q).map(|r| r.rows);
    (result, started.elapsed())
}

/// A ring with chords: every node reaches every other, so per-source traversals do real work.
fn connected(nodes: i64) -> Graph {
    let mut g = Graph::new();
    execute(
        &mut g,
        &format!(
            "UNWIND range(0, {last}) AS i CREATE (:V {{i: i}})",
            last = nodes - 1
        ),
    )
    .unwrap();
    execute(&mut g, "CREATE INDEX FOR (v:V) ON (v.i)").unwrap();
    execute(
        &mut g,
        &format!(
            "UNWIND range(0, {last}) AS i \
             MATCH (a:V {{i: i}}), (b:V {{i: (i + 1) % {nodes}}}), \
             (c:V {{i: (i * 7 + 3) % {nodes}}}) \
             CREATE (a)-[:E]->(b), (a)-[:E]->(c)",
            last = nodes - 1
        ),
    )
    .unwrap();
    g
}

#[test]
fn a_deadline_stops_a_parallel_algorithm_phase() {
    let mut g = connected(8000);
    let q = "CALL traverse.betweenness.stream({}) YIELD nodeId, score RETURN count(*)";
    let (full, full_time) = run_with_timeout(&mut g, q, None);
    full.unwrap();
    let (stopped, stopped_time) = run_with_timeout(&mut g, q, Some(Duration::from_millis(1)));
    let err = stopped.unwrap_err();
    assert!(timed_out(&err), "{err}");
    // the source-parallel phase checks the token on the workers, so it stops early instead of
    // finishing every source first
    assert!(
        stopped_time < full_time / 2,
        "stopped after {stopped_time:?}, a full run takes {full_time:?}"
    );
}

#[test]
fn a_timebox_keeps_the_outer_time_limit() {
    let mut g = Graph::new();
    execute(&mut g, "UNWIND range(1, 2000) AS i CREATE (:N {i: i})").unwrap();
    // the inner statement finishes inside its own (long) limit; the outer query's 100ms must still
    // apply to the cross product after it
    let (result, elapsed) = run_with_timeout(
        &mut g,
        "CALL apoc.cypher.runTimeboxed('RETURN 1 AS x', {}, 60000) YIELD value \
         MATCH (a:N), (b:N), (c:N) RETURN count(*)",
        Some(Duration::from_millis(100)),
    );
    let err = result.unwrap_err();
    assert!(timed_out(&err), "{err}");
    assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
}

#[test]
fn a_timebox_returns_the_rows_produced_before_its_limit() {
    let mut g = Graph::new();
    execute(&mut g, "UNWIND range(1, 2000) AS i CREATE (:N {i: i})").unwrap();
    let (result, _) = run_with_timeout(
        &mut g,
        "CALL apoc.cypher.runTimeboxed('MATCH (a:N), (b:N), (c:N) RETURN a.i AS i', {}, 50) \
         YIELD value RETURN count(*) AS n",
        None,
    );
    let rows = result.expect("the inner limit isn't an error");
    let CypherValue::Integer(n) = rows[0][0] else {
        panic!("{rows:?}")
    };
    assert!(n < 8_000_000_000, "the inner statement stopped early: {n}");
}

#[test]
fn an_unbounded_expansion_reports_the_stop_instead_of_partial_paths() {
    let mut g = connected(3000);
    let (result, _) = run_with_timeout(
        &mut g,
        "MATCH (s:V {i: 0}) CALL apoc.path.expandConfig(s, {maxLevel: -1, uniqueness: 'NONE'}) \
         YIELD path RETURN count(path)",
        Some(Duration::from_millis(50)),
    );
    match result {
        Err(err) => assert!(timed_out(&err), "{err}"),
        Ok(rows) => panic!("a stopped expansion must not return partial results: {rows:?}"),
    }
}

#[test]
fn a_cancel_stops_a_prepared_lazy_statement() {
    let mut g = Graph::new();
    execute(&mut g, "UNWIND range(1, 2000) AS i CREATE (:N {i: i})").unwrap();
    let q = "MATCH (a:N), (b:N), (c:N) RETURN count(*)";
    let params = std::collections::HashMap::new();
    let prepared = traverse_cypher::prepare_query(&g, q, &params).unwrap();
    let query = QueryContext::new(QueryOptions::default());
    let remote = std::sync::Arc::clone(&query);
    let canceller = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        remote.cancel(Interrupt::Cancelled);
    });
    let started = Instant::now();
    let result = {
        let _scope = query.enter();
        traverse_cypher::execute_prepared_lazy(&mut g, &prepared, &params).map(|_| ())
    };
    canceller.join().unwrap();
    let err = match result {
        Err(e) => e,
        Ok(()) => panic!("a cancelled statement must fail"),
    };
    assert!(
        matches!(
            err,
            CypherError::Interrupted {
                reason: Interrupt::Cancelled,
                ..
            }
        ),
        "{err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_timebox_never_hands_back_a_half_done_write() {
    let mut g = Graph::new();
    execute(&mut g, "UNWIND range(1, 2000) AS i CREATE (:N {i: i})").unwrap();
    // the inner statement writes and hits its own limit: stopping half way would leave part of
    // its writes, so the whole statement fails instead of returning partial rows
    let (result, _) = run_with_timeout(
        &mut g,
        "CALL apoc.cypher.runTimeboxed(\
         'MATCH (a:N), (b:N), (c:N) CREATE (:Out) RETURN 1 AS x', {}, 50) \
         YIELD value RETURN count(*) AS n",
        None,
    );
    let err = result.expect_err("a stopped write is an error");
    assert!(timed_out(&err), "{err}");
}
