//! `exists((a)-[:R]->(b))`, the legacy spelling of a pattern predicate, and
//! patterns whose left-hand node isn't an expression: `()-[:R]->(b)`,
//! `(:L)-[:R]->(b)`.
//!
//! `exists(pattern)` must be a pattern predicate, not the null-testing
//! `exists()` over an evaluated boolean (which would answer true whether or not
//! the pattern matched, and make `NOT exists(pattern)` return nothing).

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

fn names(rows: &[Vec<CypherValue>]) -> Vec<String> {
    let mut v: Vec<String> = rows
        .iter()
        .map(|r| match &r[0] {
            CypherValue::String(s) => s.clone(),
            o => panic!("expected string, got {o:?}"),
        })
        .collect();
    v.sort();
    v
}

/// Andy knows Timothy and Peter; nobody knows Andy.
fn people() -> Graph {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:Person {name: 'Andy', age: 36}), (t:Person {name: 'Timothy', age: 25}), \
                (p:Person {name: 'Peter', age: 35}) \
         CREATE (a)-[:KNOWS {since: 1999}]->(t), (a)-[:KNOWS {since: 2012}]->(p)",
    );
    g
}

// -- exists(pattern) -----------------------------------------------

#[test]
fn exists_pattern_agrees_with_bare_pattern() {
    let mut g = people();
    let call = ok(
        &mut g,
        "MATCH (a:Person) WHERE exists((a)-[:KNOWS]->()) RETURN a.name",
    );
    let bare = ok(
        &mut g,
        "MATCH (a:Person) WHERE (a)-[:KNOWS]->() RETURN a.name",
    );
    assert_eq!(names(&call), vec!["Andy"]);
    assert_eq!(names(&call), names(&bare));
}

#[test]
fn not_exists_pattern_is_not_empty() {
    // NOT exists(pattern) keeps the rows without a match (a null test would see Bool(false) as "not
    // null" and drop every row)
    let mut g = people();
    let rows = ok(
        &mut g,
        "MATCH (a:Person) WHERE NOT exists((a)-[:KNOWS]->()) RETURN a.name",
    );
    assert_eq!(names(&rows), vec!["Peter", "Timothy"]);
}

#[test]
fn not_exists_pattern_agrees_with_the_other_two_spellings() {
    let mut g = people();
    let call = ok(
        &mut g,
        "MATCH (a:Person) WHERE NOT exists((a)-[:KNOWS]->()) RETURN a.name",
    );
    let block = ok(
        &mut g,
        "MATCH (a:Person) WHERE NOT EXISTS { (a)-[:KNOWS]->() } RETURN a.name",
    );
    let bare = ok(
        &mut g,
        "MATCH (a:Person) WHERE NOT (a)-[:KNOWS]->() RETURN a.name",
    );
    assert_eq!(names(&call), names(&block));
    assert_eq!(names(&call), names(&bare));
}

#[test]
fn exists_pattern_with_a_labelled_target() {
    let mut g = people();
    let rows = ok(
        &mut g,
        "MATCH (a:Person) WHERE exists((a)-[:KNOWS]->(:Person)) RETURN a.name",
    );
    assert_eq!(names(&rows), vec!["Andy"]);
}

#[test]
fn exists_pattern_uppercase_spelling() {
    let mut g = people();
    let rows = ok(
        &mut g,
        "MATCH (a:Person) WHERE NOT EXISTS((a)-[:KNOWS]->()) RETURN a.name",
    );
    assert_eq!(names(&rows), vec!["Peter", "Timothy"]);
}

// -- exists(property) still tests for null ------------------------

#[test]
fn exists_property_still_tests_for_null() {
    let mut g = people();
    let present = ok(&mut g, "MATCH (p:Person) WHERE exists(p.age) RETURN p.name");
    assert_eq!(names(&present), vec!["Andy", "Peter", "Timothy"]);

    let absent = ok(
        &mut g,
        "MATCH (p:Person) WHERE exists(p.nickname) RETURN p.name",
    );
    assert!(absent.is_empty(), "expected no rows, got {absent:?}");

    let negated = ok(
        &mut g,
        "MATCH (p:Person) WHERE NOT exists(p.nickname) RETURN p.name",
    );
    assert_eq!(names(&negated), vec!["Andy", "Peter", "Timothy"]);
}

// -- patterns starting with a node that isn't an expression -------

#[test]
fn anonymous_left_node_as_predicate() {
    let mut g = people();
    let rows = ok(
        &mut g,
        "MATCH (b:Person) WHERE ()-[:KNOWS]->(b) RETURN b.name",
    );
    assert_eq!(names(&rows), vec!["Peter", "Timothy"]);
}

#[test]
fn negated_anonymous_left_node_as_predicate() {
    let mut g = people();
    let rows = ok(
        &mut g,
        "MATCH (b:Person) WHERE NOT ()-[:KNOWS]->(b) RETURN b.name",
    );
    assert_eq!(names(&rows), vec!["Andy"]);
}

#[test]
fn labelled_anonymous_left_node_as_predicate() {
    let mut g = people();
    let rows = ok(
        &mut g,
        "MATCH (b:Person) WHERE (:Person)-[:KNOWS]->(b) RETURN b.name",
    );
    assert_eq!(names(&rows), vec!["Peter", "Timothy"]);
}

#[test]
fn parenthesized_expression_is_still_an_expression() {
    // pattern parsing must not swallow ordinary parentheses
    let mut g = people();
    let arithmetic = ok(
        &mut g,
        "MATCH (p:Person) WHERE (p.age - 1) > 34 RETURN p.name",
    );
    assert_eq!(names(&arithmetic), vec!["Andy"]);

    let grouped = ok(&mut g, "MATCH (p:Person) WHERE (p.age) < 36 RETURN p.name");
    assert_eq!(names(&grouped), vec!["Peter", "Timothy"]);
}
