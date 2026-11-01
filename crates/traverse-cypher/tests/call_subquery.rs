//! Inline `CALL { ... }` subqueries in the openCypher dialect (dotted names still go to the procedure
//! path), plus `timestamp()`.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run(graph: &mut Graph, q: &str) -> Result<Vec<Vec<CypherValue>>, String> {
    let params: HashMap<String, CypherValue> = HashMap::new();
    match execute_with_params(graph, q, &params) {
        Ok(res) => Ok(res.rows),
        Err(e) => Err(e.to_string()),
    }
}

#[test]
fn standalone_call_subquery() {
    let mut g = Graph::new();
    let rows = run(&mut g, "CALL { RETURN 1 AS x } RETURN x").expect("query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::Integer(1));
}

#[test]
fn call_subquery_expands_rows() {
    let mut g = Graph::new();
    let rows = run(&mut g, "CALL { UNWIND [1, 2, 3] AS x RETURN x } RETURN x").expect("query");
    let got: Vec<i64> = rows
        .iter()
        .map(|r| match r[0] {
            CypherValue::Integer(i) => i,
            _ => panic!("expected int"),
        })
        .collect();
    assert_eq!(got, vec![1, 2, 3]);
}

#[test]
fn call_subquery_per_row_transparent_scope() {
    let mut g = Graph::new();
    // body references the outer `n` without an explicit import (transparent scope)
    let rows = run(
        &mut g,
        "UNWIND [10, 20] AS n CALL { WITH n RETURN n * 2 AS d } RETURN n, d",
    )
    .expect("query");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0],
        vec![CypherValue::Integer(10), CypherValue::Integer(20)]
    );
    assert_eq!(
        rows[1],
        vec![CypherValue::Integer(20), CypherValue::Integer(40)]
    );
}

#[test]
fn call_subquery_explicit_imports() {
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "UNWIND [10, 20] AS n CALL (n) { RETURN n * 2 AS d } RETURN n, d",
    )
    .expect("query");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0],
        vec![CypherValue::Integer(10), CypherValue::Integer(20)]
    );
    assert_eq!(
        rows[1],
        vec![CypherValue::Integer(20), CypherValue::Integer(40)]
    );
}

#[test]
fn call_subquery_with_writes_in_body_errors_clearly() {
    // writes inside a plain CALL subquery body aren't supported (the shared read-only
    // `exec_subquery` interpreter runs no write operators); they must fail with a *clear* message,
    // not silently do nothing or give a confusing parse error
    let mut g = Graph::new();
    let err = run(
        &mut g,
        "UNWIND [1, 2, 3] AS x CALL { CREATE (:Widget {v: x}) } RETURN x",
    )
    .expect_err("writes in CALL body not yet supported");
    assert!(
        err.contains("not yet supported"),
        "expected a clear not-yet-supported message, got: {err}"
    );
    // nothing written
    let rows = run(&mut g, "MATCH (w:Widget) RETURN count(w) AS c").expect("count");
    assert_eq!(rows[0][0], CypherValue::Integer(0));
}

#[test]
fn call_subquery_in_transactions_writes() {
    // IN TRANSACTIONS runs the body per row with writes (full coverage in
    // tests/call_in_transactions.rs)
    let mut g = Graph::new();
    run(
        &mut g,
        "UNWIND range(1, 3) AS i CALL { WITH i CREATE (:X {id: i}) } IN TRANSACTIONS",
    )
    .expect("in transactions writes");
    let rows = run(&mut g, "MATCH (x:X) RETURN count(x) AS c").expect("count");
    assert_eq!(rows[0][0], CypherValue::Integer(3));
}

#[test]
fn call_procedure_still_parses() {
    // a dotted-name CALL must still route to the procedure path, not the subquery path
    let mut g = Graph::new();
    // unknown procedure: a procedure-not-found error, NOT a parse error about `{`, so it took the
    // procedure branch
    let err = run(&mut g, "CALL db.nope.missing()").expect_err("unknown proc");
    assert!(
        !err.to_lowercase().contains("unexpected"),
        "should reach procedure resolution, got: {err}"
    );
}

#[test]
fn timestamp_returns_epoch_millis() {
    let mut g = Graph::new();
    let rows = run(&mut g, "RETURN timestamp() AS t").expect("timestamp");
    match rows[0][0] {
        CypherValue::Integer(ms) => assert!(ms > 1_600_000_000_000, "epoch millis, got {ms}"),
        ref other => panic!("expected Integer epoch millis, got {other:?}"),
    }
}

#[test]
fn timestamp_is_stable_within_statement() {
    let mut g = Graph::new();
    // statement clock: two calls in one statement return the same value
    let rows = run(&mut g, "RETURN timestamp() AS a, timestamp() AS b").expect("q");
    assert_eq!(rows[0][0], rows[0][1]);
}
