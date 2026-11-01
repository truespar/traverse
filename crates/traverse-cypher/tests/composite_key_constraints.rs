//! Composite (multi-property) NODE KEY / UNIQUE constraints: `CREATE CONSTRAINT ... REQUIRE (n.a,
//! n.b) IS NODE KEY`. Per Neo4j, uniqueness applies to the value TUPLE (not per property) and NODE
//! KEY also requires every key present. Relationship keys (`IS RELATIONSHIP KEY`) work the same.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run(graph: &mut Graph, q: &str) -> Result<Vec<Vec<CypherValue>>, String> {
    execute_with_params(graph, q, &HashMap::new())
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

fn ok(graph: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    run(graph, q).unwrap_or_else(|e| panic!("query failed: {q}\n{e}"))
}

#[test]
fn composite_node_key_creates_and_shows() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE CONSTRAINT person_key FOR (p:Person) REQUIRE (p.first, p.last) IS NODE KEY",
    );
    let rows = ok(
        &mut g,
        "SHOW CONSTRAINTS YIELD name, type, properties WHERE name = 'person_key' \
         RETURN type, properties",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("NODE KEY".into()));
    assert_eq!(rows[0][1], CypherValue::String("first, last".into()));
}

#[test]
fn tuple_uniqueness_not_per_property() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE CONSTRAINT k FOR (p:Person) REQUIRE (p.first, p.last) IS NODE KEY",
    );
    ok(&mut g, "CREATE (:Person {first: 'Ada', last: 'Lovelace'})");
    // same first, different last: the TUPLE differs, so legal (per-property uniqueness would reject
    // it)
    ok(&mut g, "CREATE (:Person {first: 'Ada', last: 'Byron'})");
    // same last, different first: also legal
    ok(&mut g, "CREATE (:Person {first: 'Alan', last: 'Lovelace'})");
    // exact tuple duplicate: rejected (on the embedded raw-Graph path the violation surfaces
    // post-write without rollback, as for single-property constraints, so no count assertion)
    let err = run(&mut g, "CREATE (:Person {first: 'Ada', last: 'Lovelace'})").unwrap_err();
    assert!(
        err.contains("tuple") || err.contains("already exists"),
        "got: {err}"
    );
}

#[test]
fn node_key_requires_every_property() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE CONSTRAINT k FOR (p:Person) REQUIRE (p.first, p.last) IS NODE KEY",
    );
    // one key component missing: rejected
    let err = run(&mut g, "CREATE (:Person {first: 'OnlyFirst'})").unwrap_err();
    assert!(err.contains("must have property"), "got: {err}");
    let err2 = run(&mut g, "CREATE (:Person {last: 'OnlyLast'})").unwrap_err();
    assert!(err2.contains("must have property"), "got: {err2}");
}

#[test]
fn composite_unique_allows_partial_nulls() {
    // composite UNIQUE (unlike KEY) exempts rows with a missing component and requires nothing to
    // exist
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE CONSTRAINT u FOR (p:Person) REQUIRE (p.first, p.last) IS UNIQUE",
    );
    ok(&mut g, "CREATE (:Person {first: 'OnlyFirst'})");
    ok(&mut g, "CREATE (:Person {first: 'OnlyFirst'})"); // partial duplicate: fine
    ok(&mut g, "CREATE (:Person {first: 'Ada', last: 'Lovelace'})");
    let err = run(&mut g, "CREATE (:Person {first: 'Ada', last: 'Lovelace'})").unwrap_err();
    assert!(
        err.contains("tuple") || err.contains("already exists"),
        "got: {err}"
    );
}

#[test]
fn set_into_violation_is_rejected() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE CONSTRAINT k FOR (p:Person) REQUIRE (p.first, p.last) IS NODE KEY",
    );
    ok(&mut g, "CREATE (:Person {first: 'Ada', last: 'Lovelace'})");
    ok(&mut g, "CREATE (:Person {first: 'Alan', last: 'Turing'})");
    // mutating Alan into Ada's tuple must fail
    let err = run(
        &mut g,
        "MATCH (p:Person {first: 'Alan'}) SET p.first = 'Ada', p.last = 'Lovelace' RETURN p",
    )
    .unwrap_err();
    assert!(
        err.contains("tuple") || err.contains("already exists"),
        "got: {err}"
    );
}

#[test]
fn create_time_validation_rejects_existing_duplicates() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:Person {first: 'Ada', last: 'Lovelace'})");
    ok(&mut g, "CREATE (:Person {first: 'Ada', last: 'Lovelace'})");
    let err = run(
        &mut g,
        "CREATE CONSTRAINT k FOR (p:Person) REQUIRE (p.first, p.last) IS NODE KEY",
    )
    .unwrap_err();
    assert!(err.contains("existing data violates"), "got: {err}");
    // missing keys also block creation
    let mut g2 = Graph::new();
    ok(&mut g2, "CREATE (:Person {first: 'OnlyFirst'})");
    let err2 = run(
        &mut g2,
        "CREATE CONSTRAINT k FOR (p:Person) REQUIRE (p.first, p.last) IS NODE KEY",
    )
    .unwrap_err();
    assert!(err2.contains("existing data violates"), "got: {err2}");
}

#[test]
fn composite_relationship_key() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE CONSTRAINT rk FOR ()-[r:SHIPMENT]-() REQUIRE (r.batch, r.seq) IS RELATIONSHIP KEY",
    );
    ok(
        &mut g,
        "CREATE (:A)-[:SHIPMENT {batch: 'b1', seq: 1}]->(:B)",
    );
    ok(
        &mut g,
        "CREATE (:A)-[:SHIPMENT {batch: 'b1', seq: 2}]->(:B)",
    );
    let err = run(
        &mut g,
        "CREATE (:A)-[:SHIPMENT {batch: 'b1', seq: 1}]->(:B)",
    )
    .unwrap_err();
    assert!(
        err.contains("tuple") || err.contains("already exists"),
        "got: {err}"
    );
    let err2 = run(&mut g, "CREATE (:A)-[:SHIPMENT {batch: 'b2'}]->(:B)").unwrap_err();
    assert!(err2.contains("must have property"), "got: {err2}");
}

#[test]
fn composite_not_null_still_rejected() {
    // Neo4j: existence / type constraints are single-property only
    let mut g = Graph::new();
    let err = run(
        &mut g,
        "CREATE CONSTRAINT c FOR (p:Person) REQUIRE (p.a, p.b) IS NOT NULL",
    )
    .unwrap_err();
    assert!(err.contains("only IS UNIQUE"), "got: {err}");
}

#[test]
fn drop_composite_constraint_lifts_enforcement() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE CONSTRAINT k FOR (p:Person) REQUIRE (p.first, p.last) IS NODE KEY",
    );
    ok(&mut g, "CREATE (:Person {first: 'Ada', last: 'Lovelace'})");
    ok(&mut g, "DROP CONSTRAINT k");
    // duplicate tuple now allowed
    ok(&mut g, "CREATE (:Person {first: 'Ada', last: 'Lovelace'})");
    let rows = ok(&mut g, "MATCH (p:Person) RETURN count(p)");
    assert_eq!(rows[0][0], CypherValue::Integer(2));
}

#[test]
fn single_property_key_unchanged() {
    // the single-property form still works
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE CONSTRAINT k FOR (p:Person) REQUIRE p.id IS NODE KEY",
    );
    ok(&mut g, "CREATE (:Person {id: 1})");
    let err = run(&mut g, "CREATE (:Person {id: 1})").unwrap_err();
    assert!(err.contains("already exists"), "got: {err}");
    let err2 = run(&mut g, "CREATE (:Person {name: 'x'})").unwrap_err();
    assert!(err2.contains("must have property"), "got: {err2}");
}
