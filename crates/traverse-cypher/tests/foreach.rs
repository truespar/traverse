//! `FOREACH (var IN list | updates)`: runs the update clauses once per list element with `var`
//! bound, without changing the outer row count.

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

fn count(graph: &mut Graph, label: &str) -> i64 {
    let q = format!("MATCH (n:{label}) RETURN count(n) AS c");
    match run(graph, &q).expect("count")[0][0] {
        CypherValue::Integer(i) => i,
        _ => panic!("expected int count"),
    }
}

#[test]
fn foreach_creates_one_node_per_element() {
    let mut g = Graph::new();
    run(&mut g, "FOREACH (x IN [1, 2, 3] | CREATE (:N {v: x}))").expect("foreach");
    assert_eq!(count(&mut g, "N"), 3);
    let mut vals: Vec<i64> = run(&mut g, "MATCH (n:N) RETURN n.v AS v")
        .expect("read")
        .iter()
        .map(|r| match r[0] {
            CypherValue::Integer(i) => i,
            _ => panic!(),
        })
        .collect();
    vals.sort();
    assert_eq!(vals, vec![1, 2, 3]);
}

#[test]
fn foreach_preserves_outer_cardinality() {
    // 2 input rows, each iterating a 2-element list -> 4 nodes, but still 2 rows out (FOREACH
    // doesn't multiply rows)
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "UNWIND [1, 2] AS n FOREACH (x IN [10, 20] | CREATE (:M {v: n * 100 + x})) RETURN n",
    )
    .expect("query");
    assert_eq!(rows.len(), 2);
    assert_eq!(count(&mut g, "M"), 4);
    let mut vals: Vec<i64> = run(&mut g, "MATCH (m:M) RETURN m.v AS v")
        .expect("read")
        .iter()
        .map(|r| match r[0] {
            CypherValue::Integer(i) => i,
            _ => panic!(),
        })
        .collect();
    vals.sort();
    assert_eq!(vals, vec![110, 120, 210, 220]);
}

#[test]
fn foreach_set_on_matched_nodes() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:P {id: 1}), (:P {id: 2}), (:P {id: 3})").expect("seed");
    // SET inside FOREACH on the outer matched `p`
    run(&mut g, "MATCH (p:P) FOREACH (_ IN [1] | SET p.seen = 1)").expect("foreach set");
    let seen =
        match run(&mut g, "MATCH (p:P) WHERE p.seen = 1 RETURN count(p) AS c").expect("q")[0][0] {
            CypherValue::Integer(i) => i,
            _ => panic!(),
        };
    assert_eq!(seen, 3);
}

#[test]
fn foreach_empty_list_is_noop() {
    let mut g = Graph::new();
    run(&mut g, "FOREACH (x IN [] | CREATE (:Empty))").expect("foreach");
    assert_eq!(count(&mut g, "Empty"), 0);
}

#[test]
fn foreach_over_null_is_noop() {
    let mut g = Graph::new();
    run(&mut g, "FOREACH (x IN null | CREATE (:Nope))").expect("foreach");
    assert_eq!(count(&mut g, "Nope"), 0);
}

#[test]
fn foreach_conditional_create_idiom() {
    // the classic "conditional CREATE" trick: iterate a list that's empty or a singleton via CASE
    let mut g = Graph::new();
    run(
        &mut g,
        "FOREACH (x IN CASE WHEN true THEN [1] ELSE [] END | CREATE (:Yes))",
    )
    .expect("foreach");
    run(
        &mut g,
        "FOREACH (x IN CASE WHEN false THEN [1] ELSE [] END | CREATE (:No))",
    )
    .expect("foreach");
    assert_eq!(count(&mut g, "Yes"), 1);
    assert_eq!(count(&mut g, "No"), 0);
}

#[test]
fn foreach_multiple_body_clauses() {
    let mut g = Graph::new();
    run(
        &mut g,
        "FOREACH (x IN [1, 2] | CREATE (:A {v: x}) CREATE (:B {v: x}))",
    )
    .expect("foreach");
    assert_eq!(count(&mut g, "A"), 2);
    assert_eq!(count(&mut g, "B"), 2);
}

#[test]
fn foreach_nested() {
    let mut g = Graph::new();
    run(
        &mut g,
        "FOREACH (x IN [1, 2] | FOREACH (y IN [10, 20] | CREATE (:Pair {s: x + y})))",
    )
    .expect("nested foreach");
    assert_eq!(count(&mut g, "Pair"), 4);
    let mut vals: Vec<i64> = run(&mut g, "MATCH (p:Pair) RETURN p.s AS s")
        .expect("read")
        .iter()
        .map(|r| match r[0] {
            CypherValue::Integer(i) => i,
            _ => panic!(),
        })
        .collect();
    vals.sort();
    assert_eq!(vals, vec![11, 12, 21, 22]);
}

#[test]
fn foreach_rejects_reading_clause_in_body() {
    let mut g = Graph::new();
    let err = run(&mut g, "FOREACH (x IN [1] | RETURN x)").expect_err("RETURN not allowed");
    assert!(
        err.to_lowercase().contains("foreach") || err.to_lowercase().contains("updating"),
        "expected an updating-clauses-only message, got: {err}"
    );
}
