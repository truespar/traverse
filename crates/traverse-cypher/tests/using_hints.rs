//! `USING INDEX var:Label(prop)` and `USING SCAN var:Label` hints: parsed, validated (the variable
//! must be bound with the hint's label, the index must exist) and advisory to the planner (the
//! index-aware rewrite already prefers indexes when a filter matches).

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

fn seed_with_index() -> Graph {
    let mut g = Graph::new();
    // index plus a small :Person dataset via Cypher
    run(&mut g, "CREATE INDEX ON :Person(age)").expect("create index");
    for i in 0..10 {
        let q = format!("CREATE (:Person {{age: {i}}})");
        run(&mut g, &q).expect("seed node");
    }
    g
}

#[test]
fn using_index_with_existing_index_succeeds() {
    let mut g = seed_with_index();
    let rows = run(
        &mut g,
        "MATCH (p:Person) USING INDEX p:Person(age) WHERE p.age = 5 RETURN p.age AS a",
    )
    .expect("query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::Integer(5));
}

#[test]
fn using_index_missing_index_errors() {
    let mut g = seed_with_index();
    let err = run(
        &mut g,
        "MATCH (p:Person) USING INDEX p:Person(name) WHERE p.name = 'x' RETURN p",
    )
    .expect_err("should error: no index on (Person, name)");
    assert!(
        err.contains("non-existent index") || err.contains("Person(name)"),
        "got: {err}"
    );
}

#[test]
fn using_index_unbound_variable_errors() {
    let mut g = seed_with_index();
    let err = run(
        &mut g,
        "MATCH (p:Person) USING INDEX q:Person(age) WHERE p.age = 1 RETURN p",
    )
    .expect_err("should error: q is not in the pattern");
    assert!(err.contains("unbound variable"), "got: {err}");
}

#[test]
fn using_index_label_mismatch_errors() {
    let mut g = seed_with_index();
    let err = run(
        &mut g,
        "MATCH (p:Person) USING INDEX p:Animal(age) WHERE p.age = 1 RETURN p",
    )
    .expect_err("should error: p is not :Animal");
    assert!(err.contains("not labelled"), "got: {err}");
}

#[test]
fn using_scan_parses_and_runs() {
    let mut g = seed_with_index();
    // `USING SCAN` is always legal: every label has a label scan
    let rows = run(
        &mut g,
        "MATCH (p:Person) USING SCAN p:Person WHERE p.age = 7 RETURN p.age AS a",
    )
    .expect("query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::Integer(7));
}

#[test]
fn multiple_hints_on_same_match() {
    let mut g = seed_with_index();
    // two chained hints both parse and validate
    let rows = run(
        &mut g,
        "MATCH (a:Person), (b:Person) \
         USING INDEX a:Person(age) USING INDEX b:Person(age) \
         WHERE a.age = 1 AND b.age = 2 \
         RETURN a.age AS x, b.age AS y",
    )
    .expect("query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::Integer(1));
    assert_eq!(rows[0][1], CypherValue::Integer(2));
}
