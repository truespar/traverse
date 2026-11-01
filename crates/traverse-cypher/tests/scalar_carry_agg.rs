//! The `WITH count(...) AS a MATCH ... RETURN a, count(...)` report idiom: the optimizer lifts the
//! grouped aggregate over the one-row cross product back to ungrouped form
//! (lift_scalar_carry_aggregate) and must keep values, column names and order, including when the
//! Aggregate is the plan root (a bare CrossProduct declares no output shape).

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run(g: &mut Graph, q: &str) -> (Vec<String>, Vec<Vec<CypherValue>>) {
    let p: HashMap<String, CypherValue> = HashMap::new();
    let r = execute_with_params(g, q, &p).unwrap();
    (r.columns, r.rows)
}

fn fixture() -> Graph {
    let mut g = Graph::new();
    run(&mut g, "UNWIND range(0, 20) AS i CREATE (:Movie {id: i})");
    run(&mut g, "UNWIND range(0, 50) AS i CREATE (:Person {id: i})");
    run(
        &mut g,
        "MATCH (p:Person), (m:Movie) WHERE p.id % 7 = m.id % 7 CREATE (p)-[:ACTED_IN]->(m)",
    );
    g
}

#[test]
fn three_phase_count_report() {
    let mut g = fixture();
    let (cols, rows) = run(
        &mut g,
        "MATCH (m:Movie) WITH count(m) AS movies \
         MATCH (p:Person) WITH movies, count(p) AS people \
         MATCH ()-[r:ACTED_IN]->() RETURN movies, people, count(r) AS acted",
    );
    assert_eq!(cols, vec!["movies", "people", "acted"]);
    assert_eq!(
        rows,
        vec![vec![
            CypherValue::Integer(21),
            CypherValue::Integer(51),
            CypherValue::Integer(153),
        ]]
    );
}

#[test]
fn two_phase_count_report() {
    let mut g = fixture();
    let (cols, rows) = run(
        &mut g,
        "MATCH (m:Movie) WITH count(m) AS movies \
         MATCH (p:Person) RETURN movies, count(p) AS people",
    );
    assert_eq!(cols, vec!["movies", "people"]);
    assert_eq!(
        rows,
        vec![vec![CypherValue::Integer(21), CypherValue::Integer(51)]]
    );
}

#[test]
fn anchorless_edge_count() {
    let mut g = fixture();
    let (_, rows) = run(&mut g, "MATCH ()-[r:ACTED_IN]->() RETURN count(r) AS c");
    assert_eq!(rows, vec![vec![CypherValue::Integer(153)]]);
    let (_, rows) = run(&mut g, "MATCH ()-[r:ACTED_IN]-() RETURN count(r) AS c");
    assert_eq!(rows, vec![vec![CypherValue::Integer(306)]]);
}

#[test]
fn lift_does_not_fire_on_real_grouping() {
    let mut g = fixture();
    // group key from the RIGHT side: must stay grouped (one row per movie id bucket, not one in
    // total)
    let (_, rows) = run(
        &mut g,
        "MATCH (m:Movie) WITH count(m) AS movies \
         MATCH (p:Person) RETURN movies, p.id % 2 AS bucket, count(p) AS people ORDER BY bucket",
    );
    assert_eq!(rows.len(), 2);
}

#[test]
fn triangle_closure_count() {
    let mut g = Graph::new();
    run(&mut g, "UNWIND range(0, 40) AS i CREATE (:User {uid: i})");
    run(
        &mut g,
        "MATCH (a:User {uid: 0}), (b:User) WHERE b.uid > 0 CREATE (a)-[:Friend]->(b)",
    );
    run(
        &mut g,
        "MATCH (a:User), (b:User) WHERE a.uid > 0 AND b.uid = a.uid + 1 CREATE (a)-[:Friend]->(b)",
    );
    run(
        &mut g,
        "MATCH (b:User), (a:User {uid: 0}) WHERE b.uid % 3 = 0 AND b.uid > 0 CREATE (b)-[:Friend]->(a)",
    );
    // triangles at node 0: 0->b->c->0 needs c % 3 == 0 and c = b + 1, b in 1..=40, c in 2..=40 -> c in
    // {3,6,...,39} -> 13
    let (_, rows) = run(
        &mut g,
        "MATCH (a:User)-[:Friend]->(b)-[:Friend]->(c)-[:Friend]->(a) \
         WHERE a.uid = 0 RETURN count(*) AS triangles",
    );
    assert_eq!(rows, vec![vec![CypherValue::Integer(13)]]);
}
