//! Pattern-internal `WHERE` (`(n:L WHERE pred)` / `[r:T WHERE pred]`, Cypher 5+) and Cypher 25 `||`
//! list concatenation.

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

// -- node-internal WHERE -------------------------------------------

#[test]
fn node_where_basic() {
    let mut g = people();
    let rows = ok(&mut g, "MATCH (p:Person WHERE p.age > 30) RETURN p.name");
    assert_eq!(names(&rows), vec!["Andy", "Peter"]);
}

#[test]
fn node_where_equivalent_to_clause_where() {
    let mut g = people();
    let a = ok(&mut g, "MATCH (p:Person WHERE p.age > 30) RETURN p.name");
    let b = ok(&mut g, "MATCH (p:Person) WHERE p.age > 30 RETURN p.name");
    assert_eq!(names(&a), names(&b));
}

#[test]
fn node_where_combines_with_props_and_clause_where() {
    let mut g = people();
    let rows = ok(
        &mut g,
        "MATCH (p:Person {age: 36} WHERE p.name STARTS WITH 'A') WHERE p.age < 40 \
         RETURN p.name",
    );
    assert_eq!(names(&rows), vec!["Andy"]);
}

#[test]
fn node_where_references_later_variable() {
    // inline predicates may reference variables bound LATER in the same pattern
    let mut g = people();
    let rows = ok(
        &mut g,
        "MATCH (a WHERE a.age > t.age)-[:KNOWS]->(t) RETURN a.name, t.name",
    );
    assert_eq!(rows.len(), 2, "Andy is older than both people he knows");
}

#[test]
fn node_where_on_chain_node() {
    let mut g = people();
    let rows = ok(
        &mut g,
        "MATCH (a:Person {name: 'Andy'})-[:KNOWS]->(f WHERE f.age < 30) RETURN f.name",
    );
    assert_eq!(names(&rows), vec!["Timothy"]);
}

#[test]
fn node_where_in_optional_match() {
    let mut g = people();
    let rows = ok(
        &mut g,
        "MATCH (a:Person {name: 'Timothy'}) \
         OPTIONAL MATCH (a)-[:KNOWS]->(f WHERE f.age > 100) \
         RETURN a.name, f",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], CypherValue::Null);
}

#[test]
fn node_where_referencing_outer_scope() {
    let mut g = people();
    let rows = ok(
        &mut g,
        "WITH 30 AS minAge MATCH (p:Person WHERE p.age > minAge) RETURN p.name",
    );
    assert_eq!(names(&rows), vec!["Andy", "Peter"]);
}

// -- relationship-internal WHERE -----------------------------------

#[test]
fn rel_where_fixed_length() {
    let mut g = people();
    let rows = ok(
        &mut g,
        "MATCH (:Person {name: 'Andy'})-[r:KNOWS WHERE r.since > 2000]->(f) RETURN f.name",
    );
    assert_eq!(names(&rows), vec!["Peter"]);
}

#[test]
fn rel_where_var_length_applies_per_element() {
    // chain with increasing weights; per-element semantics reject any path with a hop failing the
    // predicate
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:N {name: 'a'})-[:R {w: 1}]->(b:N {name: 'b'})-[:R {w: 5}]->(c:N {name: 'c'})",
    );
    let rows = ok(
        &mut g,
        "MATCH (s:N {name: 'a'})-[r:R* WHERE r.w < 3]->(e) RETURN e.name",
    );
    // 1-hop a->b passes (w=1); 2-hop a->b->c contains w=5: rejected
    assert_eq!(names(&rows), vec!["b"]);
}

#[test]
fn rel_where_quantified_pattern() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:N {name: 'a'})-[:R {w: 1}]->(b:N {name: 'b'})-[:R {w: 5}]->(c:N {name: 'c'})",
    );
    let rows = ok(
        &mut g,
        "MATCH (s:N {name: 'a'})-[r:R WHERE r.w < 3]->{1,2}(e) RETURN e.name",
    );
    assert_eq!(names(&rows), vec!["b"]);
}

#[test]
fn where_in_qpp_group_inner_node() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (:C {q: 0})-[:R]->(:C {q: 1})-[:R]->(:C {q: 2})-[:R]->(:C {q: 3})",
    );
    // per iteration: every hop target must satisfy the inline predicate
    let rows = ok(
        &mut g,
        "MATCH (s:C {q: 0})((x)-[r:R]->(y WHERE y.q < 3)){1,3}(e) \
         RETURN size(r) AS hops ORDER BY hops",
    );
    assert_eq!(
        rows.len(),
        2,
        "1 and 2 hops pass; the 3-hop path reaches q=3"
    );
}

// -- contexts + rejections -----------------------------------------

#[test]
fn pattern_where_inside_exists_subquery() {
    let mut g = people();
    let rows = ok(
        &mut g,
        "MATCH (p:Person) WHERE EXISTS { MATCH (p)-[:KNOWS]->(f WHERE f.age < 30) } \
         RETURN p.name",
    );
    assert_eq!(names(&rows), vec!["Andy"]);
}

#[test]
fn pattern_where_in_pattern_comprehension() {
    let mut g = people();
    let rows = ok(
        &mut g,
        "MATCH (a:Person {name: 'Andy'}) \
         RETURN size([(a)-[:KNOWS]->(f WHERE f.age < 30) | f.name]) AS n",
    );
    assert_eq!(rows[0][0], CypherValue::Integer(1));
}

#[test]
fn where_rejected_in_create_and_merge() {
    let mut g = Graph::new();
    let e1 = run(&mut g, "CREATE (n:X WHERE n.a = 1)").unwrap_err();
    assert!(e1.contains("not allowed inside CREATE"), "got: {e1}");
    let e2 = run(&mut g, "MERGE (n:X WHERE n.a = 1)").unwrap_err();
    assert!(e2.contains("not allowed inside MERGE"), "got: {e2}");
}

// -- || concatenation (Cypher 25) ---------------------------------

#[test]
fn concat_strings_and_lists() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "RETURN 'Neo' || '4j' AS s, [1, 2] || [3, 4] AS l, [1, 2] || [3, null] AS ln",
    );
    assert_eq!(rows[0][0], CypherValue::String("Neo4j".into()));
    assert_eq!(
        rows[0][1],
        CypherValue::List(vec![
            CypherValue::Integer(1),
            CypherValue::Integer(2),
            CypherValue::Integer(3),
            CypherValue::Integer(4),
        ])
    );
    // NULL elements are kept (unlike a NULL operand)
    assert!(matches!(&rows[0][2], CypherValue::List(l) if l.len() == 4));
}

#[test]
fn concat_null_operand_propagates() {
    let mut g = Graph::new();
    let rows = ok(&mut g, "RETURN 'a' || null AS s, [1] || null AS l");
    assert_eq!(rows[0][0], CypherValue::Null);
    assert_eq!(rows[0][1], CypherValue::Null);
}

#[test]
fn concat_mixed_kinds_error() {
    let mut g = Graph::new();
    let e = run(&mut g, "RETURN 'a' || [1]").unwrap_err();
    assert!(e.contains("STRING or two LIST"), "got: {e}");
    let e2 = run(&mut g, "RETURN 'a' || 1").unwrap_err();
    assert!(e2.contains("STRING or two LIST"), "got: {e2}");
}
