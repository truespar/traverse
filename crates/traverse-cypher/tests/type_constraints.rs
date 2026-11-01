//! Property type constraints:
//!   CREATE CONSTRAINT name FOR (n:Label) REQUIRE n.prop IS :: <TYPE>
//!
//! Enforced on writes via `check_all_node_constraints` (same gate as UNIQUE and
//! NOT NULL). NULL always satisfies a type constraint; combine with
//! `IS NOT NULL` for non-null typed properties. Existing data is checked when
//! the constraint is created.

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
fn create_constraint_then_typed_insert_succeeds() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT ct1 FOR (n:Person) REQUIRE n.age IS :: INTEGER",
    )
    .expect("create constraint");
    run(&mut g, "CREATE (:Person {age: 42})").expect("typed insert");
    let rows = run(&mut g, "MATCH (n:Person) RETURN n.age AS a").expect("read");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::Integer(42));
}

#[test]
fn create_constraint_rejects_wrong_type_on_insert() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT ct1 FOR (n:Person) REQUIRE n.age IS :: INTEGER",
    )
    .expect("create constraint");
    let err =
        run(&mut g, "CREATE (:Person {age: 'forty-two'})").expect_err("string is not INTEGER");
    assert!(err.contains("type"), "got: {err}");
}

#[test]
fn null_property_always_allowed() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT ct1 FOR (n:Person) REQUIRE n.age IS :: INTEGER",
    )
    .expect("create constraint");
    // no `age` = NULL = allowed
    run(&mut g, "CREATE (:Person {name: 'Alice'})").expect("null prop ok");
    let rows = run(&mut g, "MATCH (n:Person) RETURN n.name AS n").expect("read");
    assert_eq!(rows.len(), 1);
}

#[test]
fn create_constraint_rejects_when_existing_data_violates() {
    let mut g = Graph::new();
    // bad data first
    run(&mut g, "CREATE (:Person {age: 'not-an-int'})").expect("seed");
    // then constraint creation must fail
    let err = run(
        &mut g,
        "CREATE CONSTRAINT ct1 FOR (n:Person) REQUIRE n.age IS :: INTEGER",
    )
    .expect_err("existing string violates INTEGER constraint");
    assert!(
        err.contains("type") || err.contains("incompatible"),
        "got: {err}"
    );
}

#[test]
fn string_constraint_accepts_strings_rejects_integers() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT ct_s FOR (n:Doc) REQUIRE n.title IS :: STRING",
    )
    .expect("create constraint");
    run(&mut g, "CREATE (:Doc {title: 'Hello'})").expect("string ok");
    let err = run(&mut g, "CREATE (:Doc {title: 42})").expect_err("integer is not STRING");
    assert!(err.contains("type"), "got: {err}");
}

#[test]
fn boolean_constraint_accepts_bool_only() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT ct_b FOR (n:Flag) REQUIRE n.value IS :: BOOLEAN",
    )
    .expect("create constraint");
    run(&mut g, "CREATE (:Flag {value: true})").expect("bool ok");
    let err = run(&mut g, "CREATE (:Flag {value: 1})").expect_err("integer is not BOOLEAN");
    assert!(err.contains("type"), "got: {err}");
}

#[test]
fn list_constraint_accepts_lists() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT ct_l FOR (n:Tags) REQUIRE n.items IS :: LIST",
    )
    .expect("create constraint");
    run(&mut g, "CREATE (:Tags {items: [1, 2, 3]})").expect("list ok");
    let err = run(&mut g, "CREATE (:Tags {items: 'not a list'})").expect_err("string is not LIST");
    assert!(err.contains("type"), "got: {err}");
}

#[test]
fn unknown_type_name_errors_at_parse() {
    let mut g = Graph::new();
    let err = run(
        &mut g,
        "CREATE CONSTRAINT bad FOR (n:T) REQUIRE n.p IS :: ALIEN",
    )
    .expect_err("unknown type name");
    assert!(err.contains("unknown property type"), "got: {err}");
}
