//! Neo4j relationship-property index syntax: `CREATE INDEX name FOR ()-[r:TYPE]-() ON (r.prop)`
//! (plus the RANGE/TEXT/POINT typed forms), routed to the edge-index backend.

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

// SHOW INDEXES columns: name(0), type(1), entityType(2), labelsOrTypes(3), properties(4).
fn find_index<'a>(rows: &'a [Vec<CypherValue>], name: &str) -> &'a Vec<CypherValue> {
    rows.iter()
        .find(|r| matches!(&r[0], CypherValue::String(s) if s == name))
        .unwrap_or_else(|| panic!("index {name} not found in SHOW INDEXES"))
}

fn s(v: &CypherValue) -> &str {
    match v {
        CypherValue::String(s) => s,
        _ => panic!("expected string, got {v:?}"),
    }
}

#[test]
fn create_rel_property_index_undirected() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE INDEX relIdx FOR ()-[r:KNOWS]-() ON (r.since)",
    )
    .expect("create rel index");
    let rows = run(&mut g, "SHOW INDEXES").expect("show");
    let row = find_index(&rows, "relIdx");
    assert_eq!(s(&row[2]), "RELATIONSHIP"); // entityType
    assert_eq!(s(&row[3]), "KNOWS"); // labelsOrTypes
    assert_eq!(s(&row[4]), "since"); // properties
}

#[test]
fn create_rel_property_index_directed_both_ways() {
    let mut g = Graph::new();
    run(&mut g, "CREATE INDEX ri FOR ()-[r:LIKES]->() ON (r.weight)").expect("directed ->");
    run(
        &mut g,
        "CREATE INDEX ri2 FOR ()<-[r:HATES]-() ON (r.weight)",
    )
    .expect("directed <-");
    let rows = run(&mut g, "SHOW INDEXES").expect("show");
    assert_eq!(s(&find_index(&rows, "ri")[2]), "RELATIONSHIP");
    assert_eq!(s(&find_index(&rows, "ri2")[2]), "RELATIONSHIP");
}

#[test]
fn create_range_rel_property_index() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE RANGE INDEX rr FOR ()-[r:RATED]-() ON (r.stars)",
    )
    .expect("range rel index");
    let rows = run(&mut g, "SHOW INDEXES").expect("show");
    assert_eq!(s(&find_index(&rows, "rr")[2]), "RELATIONSHIP");
}

#[test]
fn rel_index_if_not_exists() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE INDEX ei IF NOT EXISTS FOR ()-[r:KNOWS]-() ON (r.since)",
    )
    .expect("if not exists");
    let rows = run(&mut g, "SHOW INDEXES").expect("show");
    assert_eq!(s(&find_index(&rows, "ei")[2]), "RELATIONSHIP");
}

#[test]
fn node_index_still_works() {
    // the node form and its typed variants still produce NODE indexes
    let mut g = Graph::new();
    run(&mut g, "CREATE INDEX ni FOR (p:Person) ON (p.age)").expect("node index");
    run(&mut g, "CREATE RANGE INDEX nr FOR (p:Person) ON (p.name)").expect("range node index");
    let rows = run(&mut g, "SHOW INDEXES").expect("show");
    assert_eq!(s(&find_index(&rows, "ni")[2]), "NODE");
    assert_eq!(s(&find_index(&rows, "nr")[2]), "NODE");
}

#[test]
fn rel_index_backs_edge_property_query() {
    let mut g = Graph::new();
    run(&mut g, "CREATE INDEX ei FOR ()-[r:KNOWS]-() ON (r.since)").expect("index");
    run(&mut g, "CREATE (:P)-[:KNOWS {since: 2020}]->(:P)").expect("edge");
    run(&mut g, "CREATE (:P)-[:KNOWS {since: 1999}]->(:P)").expect("edge2");
    let rows = run(
        &mut g,
        "MATCH ()-[r:KNOWS]->() WHERE r.since = 2020 RETURN r.since AS s",
    )
    .expect("query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::Integer(2020));
}
