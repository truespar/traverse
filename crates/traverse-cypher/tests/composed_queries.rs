//! Cypher 25 composed queries: sequential composition (`NEXT`) and conditional queries (`WHEN ...
//! THEN ... ELSE ...`). NEXT fuses segments (RETURN -> WITH); WHEN desugars to guarded UNION ALL arms.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run(graph: &mut Graph, q: &str) -> Result<Vec<Vec<CypherValue>>, String> {
    execute_with_params(graph, q, &HashMap::new())
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

fn run_p(
    graph: &mut Graph,
    q: &str,
    params: &HashMap<String, CypherValue>,
) -> Vec<Vec<CypherValue>> {
    execute_with_params(graph, q, params)
        .unwrap_or_else(|e| panic!("query failed: {q}\n{e}"))
        .rows
}

fn ok(graph: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    run(graph, q).unwrap_or_else(|e| panic!("query failed: {q}\n{e}"))
}

fn shop() -> Graph {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:Customer {name: 'ann'}), (b:Customer {name: 'bob'}), \
                (p1:Product {sku: 'x'}), (p2:Product {sku: 'y'}), (p3:Product {sku: 'z'}) \
         CREATE (a)-[:BUYS]->(p1), (a)-[:BUYS]->(p2), (a)-[:BUYS]->(p3), (b)-[:BUYS]->(p1)",
    );
    g
}

// -- NEXT ----------------------------------------------------------

#[test]
fn next_pipes_aggregated_columns() {
    let mut g = shop();
    let rows = ok(
        &mut g,
        "MATCH (c:Customer)-[:BUYS]->(p:Product) \
         RETURN c AS customer, count(p) AS total \
         NEXT \
         FILTER total > 2 \
         RETURN customer.name AS name",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("ann".into()));
}

#[test]
fn next_chains_multiple_segments() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "UNWIND [1, 2, 3, 4] AS x RETURN x, x * x AS sq \
         NEXT FILTER sq > 4 RETURN x, sq \
         NEXT RETURN sum(sq) AS total",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::Integer(25), "9 + 16");
}

#[test]
fn next_return_star_and_order_limit() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "UNWIND [3, 1, 2] AS x RETURN x ORDER BY x DESC LIMIT 2 \
         NEXT RETURN collect(x) AS xs",
    );
    assert_eq!(
        rows[0][0],
        CypherValue::List(vec![CypherValue::Integer(3), CypherValue::Integer(2)])
    );
}

#[test]
fn next_scope_is_only_returned_columns() {
    let mut g = shop();
    let e = run(
        &mut g,
        "MATCH (c:Customer) RETURN c.name AS name NEXT RETURN c.name",
    )
    .unwrap_err();
    assert!(!e.is_empty(), "c must be out of scope after NEXT");
}

#[test]
fn next_requires_return() {
    let mut g = Graph::new();
    let e = run(&mut g, "MATCH (n) NEXT RETURN 1").unwrap_err();
    assert!(e.contains("end with RETURN"), "got: {e}");
}

// -- WHEN / THEN / ELSE --------------------------------------------

#[test]
fn conditional_first_true_branch_wins() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "WHEN false THEN RETURN 1 AS x WHEN true THEN RETURN 2 AS x ELSE RETURN 3 AS x",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::Integer(2));
}

#[test]
fn conditional_else_taken_when_all_false_or_null() {
    let mut g = Graph::new();
    let rows = ok(&mut g, "WHEN false THEN RETURN 1 AS x ELSE RETURN 3 AS x");
    assert_eq!(rows[0][0], CypherValue::Integer(3));
    // a NULL predicate counts as false (Neo4j semantics): ELSE fires
    let rows2 = ok(&mut g, "WHEN null THEN RETURN 1 AS x ELSE RETURN 3 AS x");
    assert_eq!(rows2[0][0], CypherValue::Integer(3));
}

#[test]
fn conditional_no_matching_branch_no_rows() {
    let mut g = Graph::new();
    let rows = ok(&mut g, "WHEN false THEN RETURN 1 AS x");
    assert!(rows.is_empty());
}

#[test]
fn conditional_parameter_driven_with_matches() {
    let mut g = shop();
    let q = "WHEN $mode = 'customers' THEN MATCH (c:Customer) RETURN c.name AS v ORDER BY v \
             ELSE MATCH (p:Product) RETURN p.sku AS v ORDER BY v";
    let mut params = HashMap::new();
    params.insert("mode".to_string(), CypherValue::String("customers".into()));
    let rows = run_p(&mut g, q, &params);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], CypherValue::String("ann".into()));
    params.insert("mode".to_string(), CypherValue::String("products".into()));
    let rows2 = run_p(&mut g, q, &params);
    assert_eq!(rows2.len(), 3);
    assert_eq!(rows2[0][0], CypherValue::String("x".into()));
}

#[test]
fn conditional_braced_branches() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "WHEN true THEN { UNWIND [1, 2] AS x RETURN sum(x) AS s } ELSE { RETURN 0 AS s }",
    );
    assert_eq!(rows[0][0], CypherValue::Integer(3));
}

#[test]
fn conditional_side_effects_only_taken_branch() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "WHEN true THEN CREATE (:Made {via: 'when'}) ELSE CREATE (:Made {via: 'else'})",
    );
    let rows = ok(&mut g, "MATCH (m:Made) RETURN m.via");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("when".into()));
}

#[test]
fn conditional_after_next_works() {
    // WHEN after NEXT: the fused prefix is duplicated into the guarded arms
    let mut g = Graph::new();
    let rows = ok(&mut g, "RETURN 1 AS x NEXT WHEN x > 0 THEN RETURN x");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::Integer(1));
}
