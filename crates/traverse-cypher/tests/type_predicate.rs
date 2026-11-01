//! `x IS :: TYPE` / `IS TYPED TYPE` value predicate in the openCypher (Bolt) dialect. Neo4j
//! semantics: null matches any declared type unless `NOT NULL` is given; a non-null value must
//! match strictly.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn b(q: &str) -> CypherValue {
    let mut g = Graph::new();
    let params: HashMap<String, CypherValue> = HashMap::new();
    execute_with_params(&mut g, &format!("RETURN {q} AS r"), &params)
        .expect("query")
        .rows[0][0]
        .clone()
}

fn t() -> CypherValue {
    CypherValue::Bool(true)
}
fn f() -> CypherValue {
    CypherValue::Bool(false)
}

#[test]
fn scalar_type_predicates() {
    assert_eq!(b("5 IS :: INTEGER"), t());
    assert_eq!(b("5 IS :: FLOAT"), f()); // strict: int isn't float
    assert_eq!(b("1.5 IS :: FLOAT"), t());
    assert_eq!(b("'x' IS :: STRING"), t());
    assert_eq!(b("true IS :: BOOLEAN"), t());
    assert_eq!(b("5 IS :: STRING"), f());
}

#[test]
fn nullability_semantics() {
    // null matches any declared type by default...
    assert_eq!(b("null IS :: INTEGER"), t());
    assert_eq!(b("null IS :: ANY"), t());
    // ...unless NOT NULL is given
    assert_eq!(b("null IS :: INTEGER NOT NULL"), f());
    assert_eq!(b("5 IS :: INTEGER NOT NULL"), t());
}

#[test]
fn any_and_negation() {
    assert_eq!(b("5 IS :: ANY"), t());
    assert_eq!(b("5 IS NOT :: STRING"), t());
    assert_eq!(b("5 IS NOT :: INTEGER"), f());
}

#[test]
fn typed_keyword_form() {
    assert_eq!(b("5 IS TYPED INTEGER"), t());
    assert_eq!(b("5 IS NOT TYPED STRING"), t());
}

#[test]
fn list_type_predicates() {
    assert_eq!(b("[1,2,3] IS :: LIST<INTEGER>"), t());
    assert_eq!(b("[1,'a'] IS :: LIST<INTEGER>"), f()); // heterogeneous
    assert_eq!(b("[] IS :: LIST<INTEGER>"), t()); // empty matches vacuously
    assert_eq!(b("[1,2] IS :: LIST"), t()); // bare LIST
    assert_eq!(b("5 IS :: LIST<INTEGER>"), f()); // a scalar isn't a list
}

#[test]
fn temporal_and_map_predicates() {
    assert_eq!(b("date('2024-01-15') IS :: DATE"), t());
    assert_eq!(b("duration('P1D') IS :: DURATION"), t());
    assert_eq!(b("{a: 1} IS :: MAP"), t());
    assert_eq!(b("{a: 1} IS :: INTEGER"), f());
}

#[test]
fn structural_predicates() {
    let mut g = Graph::new();
    let params: HashMap<String, CypherValue> = HashMap::new();
    execute_with_params(&mut g, "CREATE (:A)-[:R]->(:B)", &params).expect("seed");
    let rows = execute_with_params(
        &mut g,
        "MATCH (n:A)-[r]->(m) RETURN n IS :: NODE AS a, r IS :: RELATIONSHIP AS b, n IS :: RELATIONSHIP AS c",
        &params,
    )
    .expect("query")
    .rows;
    assert_eq!(rows[0][0], t());
    assert_eq!(rows[0][1], t());
    assert_eq!(rows[0][2], f());
}

#[test]
fn type_predicate_in_where() {
    let mut g = Graph::new();
    let params: HashMap<String, CypherValue> = HashMap::new();
    execute_with_params(
        &mut g,
        "CREATE (:N {v: 1}), (:N {v: 'x'}), (:N {v: 2})",
        &params,
    )
    .expect("seed");
    let rows = execute_with_params(
        &mut g,
        "MATCH (n:N) WHERE n.v IS :: INTEGER RETURN count(n) AS c",
        &params,
    )
    .expect("query")
    .rows;
    assert_eq!(rows[0][0], CypherValue::Integer(2));
}
