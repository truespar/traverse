//! Long-tail grammar: CYPHER prelude after PROFILE, ON ERROR RETRY, CALL (*), map-projection entry
//! kinds, mid-query WHEN (read-only prefix duplication), leading USE, unnamed indexes, IS :: VECTOR
//! and `| NULL` type unions.

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
fn cypher_prelude_after_profile() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:P {x: 1})");
    let rows = ok(
        &mut g,
        "PROFILE CYPHER runtime=parallel MATCH (n:P) RETURN n.x",
    );
    assert_eq!(rows.len(), 1);
    let rows2 = ok(&mut g, "CYPHER 25 PROFILE MATCH (n:P) RETURN n.x");
    assert_eq!(rows2.len(), 1);
}

#[test]
fn on_error_retry_variants() {
    let mut g = Graph::new();
    for q in [
        "UNWIND range(1, 4) AS i CALL (i) { CREATE (:R1 {v: i}) } IN TRANSACTIONS OF 2 ROWS ON ERROR RETRY",
        "UNWIND range(1, 4) AS i CALL (i) { CREATE (:R2 {v: i}) } IN TRANSACTIONS ON ERROR RETRY FOR 3 SECONDS",
        "UNWIND range(1, 4) AS i CALL (i) { CREATE (:R3 {v: i}) } IN TRANSACTIONS ON ERROR RETRY THEN CONTINUE",
    ] {
        ok(&mut g, q);
    }
    let rows = ok(&mut g, "MATCH (n:R3) RETURN count(n)");
    assert_eq!(rows[0][0], CypherValue::Integer(4));
}

#[test]
fn call_star_imports_all() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:T {name: 'x'})");
    let rows = ok(
        &mut g,
        "MATCH (t:T) CALL (*) { RETURN t.name AS n } RETURN n",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("x".into()));
}

#[test]
fn map_projection_all_entry_kinds() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:Person {name: 'Keanu', born: 1964})");
    let rows = ok(
        &mut g,
        "MATCH (k:Person) WITH k, 3 AS totalMovies \
         RETURN k {.name, totalMovies, age: 2026 - k.born, .*} AS m",
    );
    let CypherValue::Map(m) = &rows[0][0] else {
        panic!("expected map")
    };
    assert_eq!(m.get("name"), Some(&CypherValue::String("Keanu".into())));
    assert_eq!(m.get("totalMovies"), Some(&CypherValue::Integer(3)));
    assert_eq!(m.get("age"), Some(&CypherValue::Integer(62)));
    assert_eq!(
        m.get("born"),
        Some(&CypherValue::Integer(1964)),
        ".* included"
    );
}

#[test]
fn map_projection_on_map_variable() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "WITH {a: 10, b: 20} AS map RETURN map {.a, doubled: map.b * 2} AS m",
    );
    let CypherValue::Map(m) = &rows[0][0] else {
        panic!("expected map")
    };
    assert_eq!(m.get("a"), Some(&CypherValue::Integer(10)));
    assert_eq!(m.get("doubled"), Some(&CypherValue::Integer(40)));
}

#[test]
fn mid_query_when_routes_per_row() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (:E {name: 'a', v: 1}), (:E {name: 'b', v: 10})",
    );
    let rows = ok(
        &mut g,
        "MATCH (e:E) \
         WHEN e.v > 5 THEN RETURN e.name AS n, 'big' AS size \
         ELSE RETURN e.name AS n, 'small' AS size",
    );
    assert_eq!(rows.len(), 2);
    let mut sorted: Vec<(String, String)> = rows
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (CypherValue::String(a), CypherValue::String(b)) => (a.clone(), b.clone()),
            _ => panic!(),
        })
        .collect();
    sorted.sort();
    assert_eq!(sorted[0], ("a".into(), "small".into()));
    assert_eq!(sorted[1], ("b".into(), "big".into()));
}

#[test]
fn when_after_next_segment() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "UNWIND [1, 9] AS x RETURN x \
         NEXT WHEN x > 5 THEN RETURN x * 100 AS r ELSE RETURN x AS r",
    );
    let mut vals: Vec<i64> = rows
        .iter()
        .map(|r| match &r[0] {
            CypherValue::Integer(i) => *i,
            _ => panic!(),
        })
        .collect();
    vals.sort();
    assert_eq!(vals, vec![1, 900]);
}

#[test]
fn when_after_write_prefix_rejected() {
    let mut g = Graph::new();
    let e = run(
        &mut g,
        "CREATE (n:W) WHEN true THEN RETURN 1 AS x ELSE RETURN 2 AS x",
    )
    .unwrap_err();
    assert!(e.contains("not supported yet"), "got: {e}");
}

#[test]
fn leading_use_is_tolerated() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:U {x: 1})");
    // embedded = single graph: USE is accepted and refers to it
    let rows = ok(&mut g, "USE neo4j MATCH (n:U) RETURN n.x");
    assert_eq!(rows.len(), 1);
    let rows2 = ok(&mut g, "USE composite.member MATCH (n:U) RETURN n.x");
    assert_eq!(rows2.len(), 1);
}

#[test]
fn unnamed_index_gets_generated_name() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE RANGE INDEX FOR (s:Scientist) ON (s.born)");
    let rows = ok(&mut g, "SHOW INDEXES YIELD name, type RETURN name, type");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0],
        CypherValue::String("index_Scientist_born".into())
    );
}

#[test]
fn vector_and_nullable_type_constraints() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE CONSTRAINT emb_vec FOR (n:Doc) REQUIRE n.emb IS :: VECTOR<FLOAT32>(3)",
    );
    ok(&mut g, "CREATE (:Doc {emb: [1.0, 2.0, 3.0]})");
    let e = run(&mut g, "CREATE (:Doc {emb: 'nope'})").unwrap_err();
    assert!(
        !e.is_empty(),
        "non-list must violate the VECTOR (list) constraint"
    );
    ok(
        &mut g,
        "CREATE CONSTRAINT tag_nullable FOR (n:M) REQUIRE n.tagline IS :: STRING | NULL",
    );
    ok(&mut g, "CREATE (:M {tagline: 'ok'}), (:M)");
    let e2 = run(&mut g, "CREATE (:M {tagline: 4})").unwrap_err();
    assert!(!e2.is_empty());
}
