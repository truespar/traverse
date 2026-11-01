//! `elementId()`, Neo4j's replacement for the deprecated `id()`: an opaque String handle.

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
fn element_id_of_node_is_string_matching_id() {
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "CREATE (n:X) RETURN elementId(n) AS eid, toString(id(n)) AS sid",
    )
    .expect("query");
    assert_eq!(rows.len(), 1);
    match (&rows[0][0], &rows[0][1]) {
        (CypherValue::String(eid), CypherValue::String(sid)) => assert_eq!(eid, sid),
        other => panic!("expected two strings, got {other:?}"),
    }
}

#[test]
fn element_id_on_matched_noderef() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:X)").expect("seed");
    let rows = run(&mut g, "MATCH (n:X) RETURN elementId(n) AS eid").expect("query");
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], CypherValue::String(_)));
}

#[test]
fn element_id_on_relationship() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:A)-[:R]->(:B)").expect("seed");
    let rows = run(&mut g, "MATCH ()-[r:R]->() RETURN elementId(r) AS eid").expect("query");
    assert_eq!(rows.len(), 1);
    assert!(matches!(rows[0][0], CypherValue::String(_)));
}

#[test]
fn element_id_of_null_is_null() {
    let mut g = Graph::new();
    let rows = run(&mut g, "RETURN elementId(null) AS eid").expect("query");
    assert_eq!(rows[0][0], CypherValue::Null);
}
