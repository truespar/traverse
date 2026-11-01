//! Per-query memory accounting: a query buffering more than its budget fails with a clear error,
//! the process and later queries are unaffected, and large-output shapes run in memory proportional
//! to their inputs, not their outputs.

use std::collections::HashMap;
use std::sync::Mutex;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::memory;
use traverse_cypher::types::CypherValue;

/// The budget is process-global: a test takes this lock before building its graph, or a small
/// budget another test set fails the setup.
static LIMIT_LOCK: Mutex<()> = Mutex::new(());

struct Budget {
    saved: usize,
    _guard: std::sync::MutexGuard<'static, ()>,
}

fn budget(bytes: usize) -> Budget {
    let guard = LIMIT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let saved = memory::query_limit();
    memory::set_query_limit(bytes);
    Budget {
        saved,
        _guard: guard,
    }
}

impl Budget {
    /// Changes the budget, still holding the lock: set up unlimited, then test under a small one.
    fn set(&self, bytes: usize) {
        memory::set_query_limit(bytes);
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        memory::set_query_limit(self.saved);
    }
}

fn run(graph: &mut Graph, q: &str) -> Result<Vec<Vec<CypherValue>>, String> {
    execute_with_params(graph, q, &HashMap::new())
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

fn ok(graph: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    run(graph, q).unwrap_or_else(|e| panic!("query failed: {q}\n{e}"))
}

fn int(v: &CypherValue) -> i64 {
    match v {
        CypherValue::Integer(i) => *i,
        other => panic!("expected integer, got {other:?}"),
    }
}

fn two_sets(n: i64) -> Graph {
    let mut g = Graph::new();
    ok(
        &mut g,
        &format!("UNWIND range(1, {n}) AS i CREATE (:A {{x: i}})"),
    );
    ok(
        &mut g,
        &format!("UNWIND range(1, {n}) AS i CREATE (:B {{y: i}})"),
    );
    g
}

const MIB: usize = 1 << 20;

#[test]
fn materialized_cross_product_over_budget_fails_cleanly() {
    let _b = budget(4 * MIB);
    let mut g = two_sets(700);
    // 490 000 result rows, far over 4 MiB
    let err = run(&mut g, "MATCH (a:A) MATCH (b:B) RETURN a.x, b.y").unwrap_err();
    assert!(err.contains("memory limit"), "{err}");
    assert!(err.contains("TRAVERSE_QUERY_MEMORY_LIMIT"), "{err}");
    // nothing leaks into the next query, which is small and must succeed
    assert_eq!(memory::used(), 0);
    let rows = ok(&mut g, "MATCH (a:A) RETURN count(a)");
    assert_eq!(int(&rows[0][0]), 700);
}

#[test]
fn cross_product_under_a_write_streams_within_a_small_budget() {
    let _b = budget(4 * MIB);
    let mut g = two_sets(700);
    // 244 650 relationships from a real (non-equi) cross product: the pairs stream through CREATE,
    // nothing buffered
    ok(
        &mut g,
        "MATCH (a:A) MATCH (b:B) WHERE a.x < b.y CREATE (a)-[:R]->(b)",
    );
    let rows = ok(&mut g, "MATCH ()-[r:R]->() RETURN count(r)");
    assert_eq!(int(&rows[0][0]), 700 * 699 / 2);
}

#[test]
fn aggregates_over_a_cross_product_stream() {
    let _b = budget(4 * MIB);
    let mut g = two_sets(700);
    let rows = ok(
        &mut g,
        "MATCH (a:A) MATCH (b:B) RETURN count(*), sum(a.x), min(b.y), max(b.y), avg(a.x)",
    );
    assert_eq!(int(&rows[0][0]), 490_000);
    assert_eq!(int(&rows[0][1]), 700 * (700 * 701 / 2));
    assert_eq!(int(&rows[0][2]), 1);
    assert_eq!(int(&rows[0][3]), 700);
    assert_eq!(rows[0][4], CypherValue::Float(350.5));
    // grouped: memory is the group table (10 groups), not the 490 000 rows
    let rows = ok(
        &mut g,
        "MATCH (a:A) MATCH (b:B) RETURN a.x % 10 AS k, count(*) AS c ORDER BY k",
    );
    assert_eq!(rows.len(), 10);
    assert_eq!(int(&rows[0][1]), 49_000);
}

#[test]
fn streaming_aggregate_matches_the_materializing_one() {
    let mut g = Graph::new();
    ok(&mut g, "UNWIND [1, 2, 2, null, 3] AS v CREATE (:N {v: v})");
    ok(&mut g, "UNWIND [1.5, 2, null] AS v CREATE (:M {v: v})");
    // over a join (streaming path) vs over a scan (pushdown path): same answers
    let a = ok(
        &mut g,
        "MATCH (n:N) MATCH (m:M {v: 2}) RETURN count(*), count(n.v), sum(n.v), avg(n.v), min(n.v), max(n.v), collect(n.v)",
    );
    let b = ok(
        &mut g,
        "MATCH (n:N) RETURN count(*), count(n.v), sum(n.v), avg(n.v), min(n.v), max(n.v), collect(n.v)",
    );
    assert_eq!(a, b);
    assert_eq!(int(&a[0][0]), 5);
    assert_eq!(int(&a[0][1]), 4);
    assert_eq!(int(&a[0][2]), 8);
    assert_eq!(a[0][3], CypherValue::Float(2.0));
    // any float makes sum a float; all-null makes min/max/avg null
    let c = ok(
        &mut g,
        "MATCH (m:M) MATCH (n:N {v: 1}) RETURN sum(m.v), avg(m.v), min(m.v)",
    );
    assert_eq!(c[0][0], CypherValue::Float(3.5));
    assert_eq!(c[0][1], CypherValue::Float(1.75));
    let d = ok(
        &mut g,
        "MATCH (n:N) MATCH (m:M) WHERE n.v IS NULL RETURN sum(n.v), avg(n.v), min(n.v), max(n.v), collect(n.v)",
    );
    assert_eq!(d[0][0], CypherValue::Integer(0));
    assert_eq!(d[0][1], CypherValue::Null);
    assert_eq!(d[0][2], CypherValue::Null);
    assert_eq!(d[0][3], CypherValue::Null);
    assert_eq!(d[0][4], CypherValue::List(vec![]));
    // empty input without keys gives one row
    let e = ok(
        &mut g,
        "MATCH (n:Nope) MATCH (m:M) RETURN count(*), sum(n.v), collect(n.v)",
    );
    assert_eq!(e.len(), 1);
    assert_eq!(int(&e[0][0]), 0);
}

#[test]
fn range_over_budget_fails_before_allocating() {
    let _b = budget(16 * MIB);
    let mut g = Graph::new();
    let err = run(&mut g, "UNWIND range(1, 100000000) AS i RETURN count(i)").unwrap_err();
    assert!(err.contains("memory limit"), "{err}");
    // within budget it still works
    let rows = ok(&mut g, "UNWIND range(1, 100000) AS i RETURN count(i)");
    assert_eq!(int(&rows[0][0]), 100_000);
}

#[test]
fn hash_join_build_side_is_charged() {
    let b = budget(0);
    let mut g = two_sets(20_000);
    b.set(MIB);
    // the build side (20k frames) exceeds 1 MiB: clean error
    let err = run(
        &mut g,
        "MATCH (a:A) MATCH (b:B) WHERE b.y = a.x RETURN count(*)",
    )
    .unwrap_err();
    assert!(err.contains("memory limit"), "{err}");
    assert_eq!(memory::used(), 0);
}

#[test]
fn sort_and_distinct_buffers_are_charged() {
    let b = budget(0);
    let mut g = two_sets(20_000);
    b.set(MIB);
    let err = run(&mut g, "MATCH (a:A) RETURN a.x ORDER BY a.x DESC").unwrap_err();
    assert!(err.contains("memory limit"), "{err}");
    let err = run(&mut g, "MATCH (a:A) MATCH (b:B) RETURN DISTINCT a.x + b.y").unwrap_err();
    assert!(err.contains("memory limit"), "{err}");
    assert_eq!(memory::used(), 0);
}

#[test]
fn variable_length_expansion_is_charged() {
    let _b = budget(2 * MIB);
    let mut g = Graph::new();
    // dense clique: paths up to length 6 explode combinatorially
    ok(&mut g, "UNWIND range(1, 12) AS i CREATE (:C {id: i})");
    ok(
        &mut g,
        "MATCH (a:C) MATCH (b:C) WHERE a.id <> b.id CREATE (a)-[:E]->(b)",
    );
    let err = run(&mut g, "MATCH p = (a:C)-[:E*1..6]->(b:C) RETURN count(p)").unwrap_err();
    assert!(err.contains("memory limit"), "{err}");
    assert_eq!(memory::used(), 0);
    // bounded depth fits; trails may return to the start node: 12*11 one-hop + 12*11*11 two-hop
    let rows = ok(&mut g, "MATCH p = (a:C)-[:E*1..2]->(b:C) RETURN count(p)");
    assert_eq!(int(&rows[0][0]), 12 * 11 + 12 * 11 * 11);
}

#[test]
fn eager_barrier_is_charged() {
    let b = budget(0);
    let mut g = two_sets(20_000);
    b.set(MIB);
    // MATCH (n:A) CREATE (:A) needs an Eager over 20k rows: over 1 MiB
    let err = run(&mut g, "MATCH (n:A) CREATE (:A)").unwrap_err();
    assert!(err.contains("memory limit"), "{err}");
    // the non-conflicting write over the same rows streams and succeeds
    ok(&mut g, "MATCH (n:A) CREATE (:Z)");
    let rows = ok(&mut g, "MATCH (z:Z) RETURN count(z)");
    assert_eq!(int(&rows[0][0]), 20_000);
}

#[test]
fn peak_is_reported_and_released() {
    // charges are batched (256 KiB) before reaching the shared counter, so the sort buffer must be
    // well above that
    let _b = budget(0);
    let mut g = two_sets(20_000);
    let _scope = traverse_cypher::query::enter();
    ok(&mut g, "MATCH (a:A) RETURN a.x ORDER BY a.x");
    assert!(
        memory::peak() > memory::CHARGE_BATCH_BYTES,
        "peak {}",
        memory::peak()
    );
    assert_eq!(memory::used(), 0);
}

#[test]
fn unlimited_budget_runs_large_results() {
    let _b = budget(0);
    let mut g = two_sets(300);
    let rows = ok(&mut g, "MATCH (a:A) MATCH (b:B) RETURN a.x, b.y");
    assert_eq!(rows.len(), 90_000);
}

#[test]
fn parses_and_formats_budgets() {
    assert_eq!(memory::parse_byte_size("2GB"), Some(2 << 30));
    assert_eq!(memory::parse_byte_size("unlimited"), Some(0));
    assert_eq!(memory::format_bytes(3 * MIB), "3.0 MB");
}
