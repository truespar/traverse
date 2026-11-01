//! Bound-entity provenance: a projection like `RETURN t.amount` drops the matched node/edge
//! bindings from the rows, so `CypherResult::bound` carries the distinct bindings the rows' frames
//! held but didn't return, as unmaterialized NodeRef/EdgeRef values. Graph visualizations use it to
//! highlight what matched.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::executor::CypherResult;
use traverse_cypher::types::CypherValue;

fn run(graph: &mut Graph, q: &str) -> CypherResult {
    let params: HashMap<String, CypherValue> = HashMap::new();
    execute_with_params(graph, q, &params).expect("query")
}

fn seed(g: &mut Graph) {
    run(
        g,
        "CREATE (a:Account {iban: 'SE1'})-[:TRANSFERRED {amount: 950000}]->(b:Account {iban: 'SE2'})",
    );
    run(g, "CREATE (:Account {iban: 'SE3'})");
}

fn bound_counts(res: &CypherResult) -> (usize, usize) {
    let nodes = res
        .bound
        .iter()
        .filter(|v| matches!(v, CypherValue::NodeRef(_)))
        .count();
    let edges = res
        .bound
        .iter()
        .filter(|v| matches!(v, CypherValue::EdgeRef { .. } | CypherValue::Edge { .. }))
        .count();
    (nodes, edges)
}

#[test]
fn projection_keeps_its_bindings_as_provenance() {
    let mut g = Graph::new();
    seed(&mut g);
    // the natural "biggest transfer" query: every returned value is a property, yet each row bound
    // two nodes and an edge
    let res = run(
        &mut g,
        "MATCH (a:Account)-[t:TRANSFERRED]->(b:Account) \
         RETURN t.amount AS amount, a.iban AS from_iban, b.iban AS to_iban",
    );
    assert_eq!(res.rows.len(), 1);
    let (nodes, edges) = bound_counts(&res);
    assert_eq!(nodes, 2, "both endpoint bindings survive: {:?}", res.bound);
    assert_eq!(edges, 1, "the edge binding survives: {:?}", res.bound);
}

#[test]
fn bound_is_a_superset_including_returned_entities() {
    let mut g = Graph::new();
    seed(&mut g);
    // `bound` is ALL pattern bindings, returned ones included: consumers dedupe by id against the
    // returned entities, and a superset gives the same answer at every capture site (project op,
    // fused scan, pass-through), where "returned-only" would differ per path
    let res = run(
        &mut g,
        "MATCH (a:Account)-[t:TRANSFERRED]->(b:Account) RETURN a",
    );
    let (nodes, edges) = bound_counts(&res);
    assert_eq!(nodes, 2, "{:?}", res.bound);
    assert_eq!(edges, 1, "{:?}", res.bound);
}

#[test]
fn bound_is_distinct_across_rows() {
    let mut g = Graph::new();
    seed(&mut g);
    // SE1 appears once per outgoing transfer; another edge makes the same node bind in two rows
    run(
        &mut g,
        "MATCH (a:Account {iban: 'SE1'}), (c:Account {iban: 'SE3'}) CREATE (a)-[:TRANSFERRED {amount: 5}]->(c)",
    );
    let res = run(
        &mut g,
        "MATCH (a:Account {iban: 'SE1'})-[t:TRANSFERRED]->(b:Account) RETURN t.amount",
    );
    assert_eq!(res.rows.len(), 2);
    let (nodes, edges) = bound_counts(&res);
    // a once, b two distinct (SE2, SE3) = 3 nodes; 2 distinct edges
    assert_eq!(nodes, 3, "{:?}", res.bound);
    assert_eq!(edges, 2, "{:?}", res.bound);
}

#[test]
fn aggregates_lose_row_bindings_gracefully() {
    let mut g = Graph::new();
    seed(&mut g);
    // aggregation collapses frames; whatever the aggregate keeps is fine, but no error and within
    // the cap
    let res = run(
        &mut g,
        "MATCH (a:Account)-[t:TRANSFERRED]->() RETURN count(t) AS n",
    );
    assert_eq!(res.rows.len(), 1);
    assert!(res.bound.len() <= 1024);
}
