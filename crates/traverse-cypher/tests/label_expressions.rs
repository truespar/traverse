//! Full Cypher 5+ label / relationship-type expressions in the Bolt dialect (`&`, `|`, `!`, `%`,
//! grouping) in node patterns, relationship patterns and WHERE-position label checks. Complex
//! shapes lower at parse time onto the pattern-internal WHERE machinery; plain conjunctions /
//! disjunctions keep the index-friendly fast paths.

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

/// alice: Person+Manager, bob: Person, carol: Manager, dave: Person+Admin, eve: unlabeled.
fn org() -> Graph {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (:Person:Manager {name: 'alice'}), (:Person {name: 'bob'}), \
                (:Manager {name: 'carol'}), (:Person:Admin {name: 'dave'}), \
                ({name: 'eve'})",
    );
    g
}

// -- node patterns -------------------------------------------------

#[test]
fn conjunction_amp() {
    let mut g = org();
    let rows = ok(&mut g, "MATCH (n:Person&Manager) RETURN n.name");
    assert_eq!(names(&rows), vec!["alice"]);
}

#[test]
fn amp_equals_colon_conjunction() {
    let mut g = org();
    let a = ok(&mut g, "MATCH (n:Person&Manager) RETURN n.name");
    let b = ok(&mut g, "MATCH (n:Person:Manager) RETURN n.name");
    assert_eq!(names(&a), names(&b));
}

#[test]
fn disjunction_pipe() {
    let mut g = org();
    let rows = ok(&mut g, "MATCH (n:Person|Manager) RETURN n.name");
    assert_eq!(names(&rows), vec!["alice", "bob", "carol", "dave"]);
}

#[test]
fn negation_bang() {
    let mut g = org();
    let rows = ok(&mut g, "MATCH (n:Person&!Manager) RETURN n.name");
    assert_eq!(names(&rows), vec!["bob", "dave"]);
}

#[test]
fn wildcard_percent() {
    let mut g = org();
    let rows = ok(&mut g, "MATCH (n:%) RETURN n.name");
    assert_eq!(
        names(&rows),
        vec!["alice", "bob", "carol", "dave"],
        "eve has no label"
    );
    let none = ok(&mut g, "MATCH (n:!%) RETURN n.name");
    assert_eq!(names(&none), vec!["eve"], "!% matches only unlabeled nodes");
}

#[test]
fn grouping_parentheses() {
    let mut g = org();
    let rows = ok(&mut g, "MATCH (n:(Person|Admin)&!Manager) RETURN n.name");
    assert_eq!(names(&rows), vec!["bob", "dave"]);
}

#[test]
fn anonymous_node_with_complex_expression() {
    let mut g = org();
    ok(
        &mut g,
        "MATCH (a {name: 'alice'}), (b {name: 'bob'}) CREATE (a)-[:KNOWS]->(b)",
    );
    let rows = ok(&mut g, "MATCH (:Person&!Admin)-[:KNOWS]->(m) RETURN m.name");
    assert_eq!(names(&rows), vec!["bob"]);
}

#[test]
fn complex_expression_combines_with_props_and_where() {
    let mut g = org();
    let rows = ok(
        &mut g,
        "MATCH (n:(Person|Manager)&!Admin {name: 'alice'} WHERE n.name STARTS WITH 'a') \
         RETURN n.name",
    );
    assert_eq!(names(&rows), vec!["alice"]);
}

// -- WHERE-position label checks -----------------------------------

#[test]
fn where_position_full_expressions() {
    let mut g = org();
    let rows = ok(&mut g, "MATCH (n) WHERE n:Person|Manager RETURN n.name");
    assert_eq!(names(&rows), vec!["alice", "bob", "carol", "dave"]);
    let rows2 = ok(&mut g, "MATCH (n) WHERE n:Person&!Manager RETURN n.name");
    assert_eq!(names(&rows2), vec!["bob", "dave"]);
    let rows3 = ok(&mut g, "MATCH (n) WHERE NOT n:% RETURN n.name");
    assert_eq!(names(&rows3), vec!["eve"]);
    // legacy conjunction unchanged
    let rows4 = ok(&mut g, "MATCH (n) WHERE n:Person:Manager RETURN n.name");
    assert_eq!(names(&rows4), vec!["alice"]);
}

#[test]
fn where_position_in_return_projection() {
    let mut g = org();
    let rows = ok(
        &mut g,
        "MATCH (n {name: 'dave'}) RETURN n:Person&Admin AS both, n:Manager AS mgr",
    );
    assert_eq!(rows[0][0], CypherValue::Bool(true));
    assert_eq!(rows[0][1], CypherValue::Bool(false));
}

// -- relationship-type expressions ---------------------------------

fn wires() -> Graph {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:N {name: 'a'}), (b:N {name: 'b'}), (c:N {name: 'c'}), (d:N {name: 'd'}) \
         CREATE (a)-[:LIKES]->(b), (a)-[:HATES]->(c), (a)-[:KNOWS]->(d)",
    );
    g
}

#[test]
fn rel_type_negation() {
    let mut g = wires();
    let rows = ok(
        &mut g,
        "MATCH (:N {name: 'a'})-[:!HATES]->(m) RETURN m.name",
    );
    assert_eq!(names(&rows), vec!["b", "d"]);
}

#[test]
fn rel_type_wildcard_and_combination() {
    let mut g = wires();
    let rows = ok(&mut g, "MATCH (:N {name: 'a'})-[:%]->(m) RETURN m.name");
    assert_eq!(names(&rows), vec!["b", "c", "d"]);
    let rows2 = ok(
        &mut g,
        "MATCH (:N {name: 'a'})-[:%&!KNOWS]->(m) RETURN m.name",
    );
    assert_eq!(names(&rows2), vec!["b", "c"]);
}

#[test]
fn rel_type_disjunction_unchanged() {
    let mut g = wires();
    let rows = ok(
        &mut g,
        "MATCH (:N {name: 'a'})-[:LIKES|KNOWS]->(m) RETURN m.name",
    );
    assert_eq!(names(&rows), vec!["b", "d"]);
}

#[test]
fn rel_type_negation_var_length_per_element() {
    // a -LIKES-> b -HATES-> c: with :!HATES* only the first hop survives
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:N {name: 'a'})-[:LIKES]->(b:N {name: 'b'})-[:HATES]->(c:N {name: 'c'})",
    );
    let rows = ok(
        &mut g,
        "MATCH (:N {name: 'a'})-[:!HATES*1..2]->(m) RETURN m.name",
    );
    assert_eq!(names(&rows), vec!["b"]);
}

// -- CREATE / MERGE semantics --------------------------------------

#[test]
fn create_accepts_amp_conjunction() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (n:Person&Manager {name: 'x'})");
    let rows = ok(&mut g, "MATCH (n:Person:Manager) RETURN n.name");
    assert_eq!(names(&rows), vec!["x"]);
}

#[test]
fn create_rejects_disjunction_negation() {
    let mut g = Graph::new();
    assert!(run(&mut g, "CREATE (n:A|B)").is_err());
    assert!(run(&mut g, "CREATE (n:!A)").is_err());
    assert!(run(&mut g, "MERGE (n:A|B)").is_err());
}
