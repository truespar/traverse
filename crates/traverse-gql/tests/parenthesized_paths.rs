//! ISO section 16.7 parenthesized path patterns with optional inner WHERE (optional features G050 / G051).

mod common;
use common::{GqlCatalog, StmtOutcome, run_program};
use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

fn run_with_graph(preamble: &[&str], query: &str) -> StmtOutcome {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    for p in preamble {
        let out = run_program(&mut g, &mut cat, p);
        assert!(out.is_ok(), "preamble failed: {p}");
    }
    run_program(&mut g, &mut cat, query)
}

// -- parenthesized pattern without WHERE ---------------------------

#[test]
fn parenthesised_path_pattern_without_where() {
    let out = run_with_graph(
        &["CREATE (:N {n: 'A'})-[:R]->(:N {n: 'B'})"],
        "MATCH ((a:N)-[:R]->(b:N)) RETURN a.n AS x, b.n AS y",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], CypherValue::String("A".into()));
            assert_eq!(rows[0][1], CypherValue::String("B".into()));
        }
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        _ => panic!("non-Ran"),
    }
}

// -- G050: parenthesized pattern with inner WHERE -----------------

#[test]
fn parenthesised_inner_where_filters_paths() {
    // two edges, A->B (w=10) and A->C (w=2); inner WHERE keeps the heavy one
    let out = run_with_graph(
        &[
            "CREATE (a:N {n: 'A'})-[:R {w: 10}]->(:N {n: 'B'})",
            "MATCH (a:N {n: 'A'}) CREATE (a)-[:R {w: 2}]->(:N {n: 'C'})",
        ],
        "MATCH ((a:N)-[r:R]->(b:N) WHERE r.w > 5) RETURN b.n AS x",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], CypherValue::String("B".into()));
        }
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        _ => panic!("non-Ran"),
    }
}

// -- G051: non-local inner predicate over both endpoints ----------

#[test]
fn parenthesised_inner_where_references_both_endpoints() {
    let out = run_with_graph(
        &[
            "CREATE (a:N {n: 'A', age: 30})-[:R]->(:N {n: 'B', age: 25})",
            "MATCH (a:N {n: 'A'}) CREATE (a)-[:R]->(:N {n: 'C', age: 40})",
        ],
        "MATCH ((a:N)-[:R]->(b:N) WHERE b.age > a.age) RETURN b.n AS x",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], CypherValue::String("C".into()));
        }
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        _ => panic!("non-Ran"),
    }
}

// -- inner WHERE + outer WHERE both apply -------------------------

#[test]
fn inner_and_outer_where_are_anded() {
    let out = run_with_graph(
        &[
            "CREATE (:N {n: 'A', age: 30})-[:R {w: 10}]->(:N {n: 'B', age: 25})",
            "MATCH (a:N {n: 'A'}) CREATE (a)-[:R {w: 1}]->(:N {n: 'C', age: 25})",
            "MATCH (a:N {n: 'A'}) CREATE (a)-[:R {w: 10}]->(:N {n: 'D', age: 50})",
        ],
        // inner WHERE w > 5 drops C, outer WHERE b.age < 40 drops D: only B left
        "MATCH ((a:N {n: 'A'})-[r:R]->(b:N) WHERE r.w > 5) \
         WHERE b.age < 40 \
         RETURN b.n AS x",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0][0], CypherValue::String("B".into()));
        }
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        _ => panic!("non-Ran"),
    }
}
