//! Single-property NODE KEY constraints. NODE KEY on `(n:L)`'s property `p` equals `IS UNIQUE` plus
//! `IS NOT NULL`: every `L` node must have a non-null `p`, unique across `L` nodes.

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
fn node_key_accepts_unique_non_null() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT pk_user_id FOR (u:User) REQUIRE u.id IS NODE KEY",
    )
    .expect("create constraint");
    run(&mut g, "CREATE (:User {id: 1})").expect("insert 1");
    run(&mut g, "CREATE (:User {id: 2})").expect("insert 2");
    let rows = run(&mut g, "MATCH (u:User) RETURN count(u) AS c").expect("read");
    assert_eq!(rows[0][0], CypherValue::Integer(2));
}

#[test]
fn node_key_rejects_duplicate() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT pk_user_id FOR (u:User) REQUIRE u.id IS NODE KEY",
    )
    .expect("create constraint");
    run(&mut g, "CREATE (:User {id: 1})").expect("insert 1");
    let err =
        run(&mut g, "CREATE (:User {id: 1})").expect_err("duplicate violates NODE KEY uniqueness");
    assert!(
        err.contains("already exists")
            || err.contains("UNIQUE")
            || err.to_lowercase().contains("constraint"),
        "got: {err}"
    );
}

#[test]
fn node_key_rejects_missing_property() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT pk_user_id FOR (u:User) REQUIRE u.id IS NODE KEY",
    )
    .expect("create constraint");
    let err = run(&mut g, "CREATE (:User {name: 'no-id'})")
        .expect_err("missing id violates NODE KEY existence");
    assert!(
        err.to_lowercase().contains("must have") || err.to_lowercase().contains("constraint"),
        "got: {err}"
    );
}

#[test]
fn node_key_parenthesised_single_property_ok() {
    // `REQUIRE (n.p) IS NODE KEY` parses the same as `REQUIRE n.p`
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT pk_user_id FOR (u:User) REQUIRE (u.id) IS NODE KEY",
    )
    .expect("paren single-property NODE KEY should parse");
    run(&mut g, "CREATE (:User {id: 7})").expect("insert");
}

#[test]
fn node_key_composite_now_supported() {
    // composite NODE KEY is enforced too (tuple uniqueness + per-key existence); full coverage in
    // tests/composite_key_constraints.rs
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE CONSTRAINT pk_user_full FOR (u:User) REQUIRE (u.first, u.last) IS NODE KEY",
    )
    .expect("composite NODE KEY is now supported");
    run(&mut g, "CREATE (:User {first: 'a', last: 'b'})").expect("insert");
    let err = run(&mut g, "CREATE (:User {first: 'a', last: 'b'})")
        .expect_err("duplicate tuple rejected");
    assert!(
        err.contains("tuple") || err.contains("already exists"),
        "got: {err}"
    );
}

#[test]
fn create_constraint_rejects_existing_duplicate() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:User {id: 1})").expect("seed 1");
    run(&mut g, "CREATE (:User {id: 1})").expect("seed 2");
    let err = run(
        &mut g,
        "CREATE CONSTRAINT pk_user_id FOR (u:User) REQUIRE u.id IS NODE KEY",
    )
    .expect_err("existing duplicates should block constraint create");
    assert!(
        err.to_lowercase().contains("violates")
            || err.to_lowercase().contains("share")
            || err.to_lowercase().contains("node key"),
        "got: {err}"
    );
}
