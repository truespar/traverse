//! Relationship property constraints: `CREATE CONSTRAINT ... FOR ()-[r:TYPE]-() REQUIRE r.p IS
//! [UNIQUE|NOT NULL|:: TYPE|KEY]`. Create-time validation, write-time enforcement (CREATE / MERGE /
//! SET), node/relationship token isolation, SHOW, and a serialization round-trip through
//! GraphParts.

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

fn is_violation(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("constraint")
        || e.contains("already exists")
        || e.contains("must have")
        || e.contains("existing data")
        || e.contains("type")
}

#[test]
fn rel_existence_enforced() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT rex FOR ()-[r:RATED]-() REQUIRE r.stars IS NOT NULL",
    )
    .expect("create");
    run(&mut g, "CREATE (:U)-[:RATED {stars: 5}]->(:M)").expect("with prop ok");
    let err = run(&mut g, "CREATE (:U)-[:RATED]->(:M)").expect_err("missing prop");
    assert!(is_violation(&err), "got: {err}");
}

#[test]
fn rel_uniqueness_enforced() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT ru FOR ()-[r:CODE]-() REQUIRE r.val IS UNIQUE",
    )
    .expect("create");
    run(&mut g, "CREATE (:A)-[:CODE {val: 1}]->(:B)").expect("first ok");
    let err = run(&mut g, "CREATE (:A)-[:CODE {val: 1}]->(:B)").expect_err("dup");
    assert!(is_violation(&err), "got: {err}");
    run(&mut g, "CREATE (:A)-[:CODE {val: 2}]->(:B)").expect("distinct ok");
}

#[test]
fn rel_type_enforced() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT rt FOR ()-[r:W]-() REQUIRE r.weight IS :: INTEGER",
    )
    .expect("create");
    run(&mut g, "CREATE (:A)-[:W {weight: 5}]->(:B)").expect("int ok");
    let err = run(&mut g, "CREATE (:A)-[:W {weight: 'heavy'}]->(:B)").expect_err("string not int");
    assert!(is_violation(&err), "got: {err}");
}

#[test]
fn rel_key_enforces_both_uniqueness_and_existence() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT rk FOR ()-[r:LINK]-() REQUIRE r.id IS KEY",
    )
    .expect("create");
    run(&mut g, "CREATE (:A)-[:LINK {id: 1}]->(:B)").expect("first ok");
    let missing = run(&mut g, "CREATE (:A)-[:LINK]->(:B)").expect_err("missing -> existence");
    assert!(is_violation(&missing), "got: {missing}");
    let dup = run(&mut g, "CREATE (:A)-[:LINK {id: 1}]->(:B)").expect_err("dup -> uniqueness");
    assert!(is_violation(&dup), "got: {dup}");
}

#[test]
fn rel_constraint_rejects_violating_existing_data() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:A)-[:DUP {val: 7}]->(:B)").expect("seed");
    run(&mut g, "CREATE (:A)-[:DUP {val: 7}]->(:B)").expect("seed dup");
    let err = run(
        &mut g,
        "CREATE CONSTRAINT dc FOR ()-[r:DUP]-() REQUIRE r.val IS UNIQUE",
    )
    .expect_err("existing data violates");
    assert!(is_violation(&err), "got: {err}");
}

#[test]
fn node_and_rel_constraint_on_same_token_isolated() {
    // `Foo` is both a node label and a relationship type; the entity discriminator keeps the two
    // constraints apart
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT nc FOR (n:Foo) REQUIRE n.p IS NOT NULL",
    )
    .expect("node");
    run(
        &mut g,
        "CREATE CONSTRAINT rc FOR ()-[r:Foo]-() REQUIRE r.p IS UNIQUE",
    )
    .expect("rel");
    // node missing p: node existence fires
    let en = run(&mut g, "CREATE (:Foo)").expect_err("node missing p");
    assert!(is_violation(&en), "got: {en}");
    // relationship of type Foo: the node constraint must NOT fire (endpoints aren't :Foo), rel
    // uniqueness must
    run(&mut g, "CREATE (:A)-[:Foo {p: 1}]->(:B)").expect("rel ok");
    let er = run(&mut g, "CREATE (:A)-[:Foo {p: 1}]->(:B)").expect_err("rel dup");
    assert!(is_violation(&er), "got: {er}");
}

#[test]
fn set_edge_property_enforces_uniqueness() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT su FOR ()-[r:S]-() REQUIRE r.k IS UNIQUE",
    )
    .expect("create");
    run(&mut g, "CREATE (:A)-[:S {k: 1}]->(:B)").expect("e1");
    run(&mut g, "CREATE (:A)-[:S {k: 2}]->(:B)").expect("e2");
    let err = run(&mut g, "MATCH ()-[r:S]->() WHERE r.k = 2 SET r.k = 1").expect_err("set dup");
    assert!(is_violation(&err), "got: {err}");
}

#[test]
fn merge_edge_enforces_existence() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT me FOR ()-[r:ME]-() REQUIRE r.tag IS NOT NULL",
    )
    .expect("create");
    run(&mut g, "CREATE (:A {id: 1}), (:B {id: 2})").expect("seed nodes");
    // MERGE an edge without a tag: existence violation
    let err = run(
        &mut g,
        "MATCH (a:A {id: 1}), (b:B {id: 2}) MERGE (a)-[:ME]->(b)",
    )
    .expect_err("merge missing tag");
    assert!(is_violation(&err), "got: {err}");
}

#[test]
fn show_constraints_surfaces_relationship_entity() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT rc FOR ()-[r:X]-() REQUIRE r.p IS UNIQUE",
    )
    .expect("create");
    let rows = run(&mut g, "SHOW CONSTRAINTS").expect("show");
    // at least one row reports the RELATIONSHIP entity type
    let has_rel = rows
        .iter()
        .flatten()
        .any(|v| matches!(v, CypherValue::String(s) if s == "RELATIONSHIP"));
    assert!(has_rel, "expected a RELATIONSHIP row: {rows:?}");
}

#[test]
fn rel_constraint_survives_parts_round_trip() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT rp FOR ()-[r:PAY]-() REQUIRE r.amt IS UNIQUE",
    )
    .expect("create");
    run(&mut g, "CREATE (:A)-[:PAY {amt: 100}]->(:B)").expect("seed");
    // serialize -> reload through GraphParts (the tvdb round-trip in miniature)
    let parts = g.into_parts();
    let mut g2 = Graph::from_parts(parts);
    let err = run(&mut g2, "CREATE (:A)-[:PAY {amt: 100}]->(:B)").expect_err("dup after reload");
    assert!(
        is_violation(&err),
        "constraint lost across round-trip; got: {err}"
    );
    run(&mut g2, "CREATE (:A)-[:PAY {amt: 200}]->(:B)").expect("distinct after reload ok");
}
