//! DDL breadth: composite (multi-property) b-tree indexes, parameterized DDL names (`CREATE INDEX
//! $name ...`), vector DDL variants, typed `SHOW <TYPE> INDEXES`.

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

fn plan_has_operator(graph: &mut Graph, query: &str, op: &str) -> bool {
    let res =
        execute_with_params(graph, &format!("EXPLAIN {query}"), &HashMap::new()).expect("explain");
    fn walk(v: &CypherValue, op: &str) -> bool {
        if let CypherValue::Map(m) = v {
            if let Some(CypherValue::String(t)) = m.get("operatorType")
                && t == op
            {
                return true;
            }
            if let Some(CypherValue::List(children)) = m.get("children") {
                return children.iter().any(|c| walk(c, op));
            }
        }
        false
    }
    walk(res.plan.as_ref().expect("plan"), op)
}

fn people(indexed: bool) -> Graph {
    let mut g = Graph::new();
    if indexed {
        ok(
            &mut g,
            "CREATE INDEX comp_ac FOR (n:Person) ON (n.age, n.country)",
        );
    }
    for (name, age, country) in [
        ("ann", 30, "se"),
        ("bob", 30, "no"),
        ("cat", 40, "se"),
        ("dan", 30, "se"),
    ] {
        ok(
            &mut g,
            &format!("CREATE (:Person {{name: '{name}', age: {age}, country: '{country}'}})"),
        );
    }
    g
}

// -- composite indexes ---------------------------------------------

#[test]
fn composite_index_matches_scan_and_accelerates() {
    let q = "MATCH (n:Person) WHERE n.age = 30 AND n.country = 'se' \
             RETURN n.name AS n ORDER BY n";
    let mut with = people(true);
    let mut without = people(false);
    assert_eq!(ok(&mut with, q), ok(&mut without, q));
    assert_eq!(ok(&mut with, q).len(), 2, "ann + dan");
    assert!(
        plan_has_operator(&mut with, q, "NodeIndexScan"),
        "composite seek engaged"
    );
    assert!(!plan_has_operator(&mut without, q, "NodeIndexScan"));
}

#[test]
fn composite_index_maintained_on_writes() {
    let mut g = people(true);
    let q = "MATCH (n:Person) WHERE n.age = 30 AND n.country = 'se' RETURN n.name AS n ORDER BY n";
    ok(
        &mut g,
        "CREATE (:Person {name: 'eve', age: 30, country: 'se'})",
    );
    assert_eq!(ok(&mut g, q).len(), 3);
    ok(
        &mut g,
        "MATCH (n:Person {name: 'ann'}) SET n.country = 'dk'",
    );
    assert_eq!(ok(&mut g, q).len(), 2, "ann left the (30, se) tuple");
    ok(&mut g, "MATCH (n:Person {name: 'eve'}) DELETE n");
    assert_eq!(ok(&mut g, q).len(), 1);
}

#[test]
fn composite_index_type_distinction() {
    // (1, "a") vs ("1", "a") must not collide in the tuple encoding
    let mut g = Graph::new();
    ok(&mut g, "CREATE INDEX comp_xy FOR (n:T) ON (n.x, n.y)");
    ok(&mut g, "CREATE (:T {x: 1, y: 'a'}), (:T {x: '1', y: 'a'})");
    let ints = ok(&mut g, "MATCH (n:T) WHERE n.x = 1 AND n.y = 'a' RETURN n");
    assert_eq!(ints.len(), 1);
    let strs = ok(&mut g, "MATCH (n:T) WHERE n.x = '1' AND n.y = 'a' RETURN n");
    assert_eq!(strs.len(), 1);
}

#[test]
fn composite_relationship_index_creates_and_shows() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE INDEX comp_rel FOR ()-[r:BOUGHT]-() ON (r.date, r.amount)",
    );
    let rows = ok(
        &mut g,
        "SHOW INDEXES YIELD name, entityType, properties WHERE name = 'comp_rel' \
         RETURN entityType, properties",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("RELATIONSHIP".into()));
    assert_eq!(rows[0][1], CypherValue::String("date,amount".into()));
}

#[test]
fn composite_index_partial_tuple_not_indexed() {
    // entities missing a component aren't in the index but scans still find them (the Filter on top
    // keeps semantics exact)
    let mut g = Graph::new();
    ok(&mut g, "CREATE INDEX comp_pq FOR (n:U) ON (n.p, n.q)");
    ok(&mut g, "CREATE (:U {p: 1}), (:U {p: 1, q: 2})");
    let rows = ok(&mut g, "MATCH (n:U) WHERE n.p = 1 AND n.q = 2 RETURN n");
    assert_eq!(rows.len(), 1);
    let all = ok(&mut g, "MATCH (n:U) WHERE n.p = 1 RETURN n");
    assert_eq!(
        all.len(),
        2,
        "single-prop query is not composite-indexed, full results"
    );
}

// -- parameterized DDL names ---------------------------------------

#[test]
fn create_and_drop_index_with_param_name() {
    let mut g = Graph::new();
    let mut params = HashMap::new();
    params.insert("name".to_string(), CypherValue::String("dyn_idx".into()));
    execute_with_params(&mut g, "CREATE INDEX $name FOR (n:P) ON (n.k)", &params)
        .expect("create with $name");
    let rows = ok(
        &mut g,
        "SHOW INDEXES YIELD name WHERE name = 'dyn_idx' RETURN name",
    );
    assert_eq!(rows.len(), 1);
    execute_with_params(&mut g, "DROP INDEX $name", &params).expect("drop with $name");
    let rows2 = ok(
        &mut g,
        "SHOW INDEXES YIELD name WHERE name = 'dyn_idx' RETURN name",
    );
    assert!(rows2.is_empty());
}

#[test]
fn param_name_missing_or_wrong_type_errors() {
    let mut g = Graph::new();
    let e = run(&mut g, "CREATE INDEX $nope FOR (n:P) ON (n.k)").unwrap_err();
    assert!(e.contains("$nope"), "got: {e}");
    let mut params = HashMap::new();
    params.insert("n".to_string(), CypherValue::Integer(7));
    let e2 = execute_with_params(&mut g, "CREATE INDEX $n FOR (n:P) ON (n.k)", &params)
        .unwrap_err()
        .to_string();
    assert!(e2.contains("STRING"), "got: {e2}");
}

// -- vector DDL variants -------------------------------------------

#[test]
fn vector_index_parenless_on() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE VECTOR INDEX v1 IF NOT EXISTS FOR (m:Movie) ON m.embedding \
         OPTIONS { indexConfig: { `vector.dimensions`: 3, `vector.similarity_function`: 'cosine' } }",
    );
    let rows = ok(&mut g, "SHOW VECTOR INDEXES YIELD name RETURN name");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("v1".into()));
}

#[test]
fn vector_multi_label_clear_error() {
    let mut g = Graph::new();
    let e = run(&mut g, "CREATE VECTOR INDEX v2 FOR (m:A|B) ON m.emb").unwrap_err();
    assert!(e.contains("multi-label"), "got: {e}");
}

#[test]
fn fulltext_relationship_form_clear_error() {
    let mut g = Graph::new();
    let e = run(
        &mut g,
        "CREATE FULLTEXT INDEX ft FOR ()-[r:REVIEWED|EMAILED]-() ON EACH [r.message]",
    )
    .unwrap_err();
    assert!(e.contains("node-only"), "got: {e}");
}

// -- typed SHOW ----------------------------------------------------

#[test]
fn typed_show_indexes_filters() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE INDEX ridx FOR (n:A) ON (n.x)");
    ok(&mut g, "CREATE TEXT INDEX tidx FOR (n:A) ON (n.y)");
    ok(&mut g, "CREATE POINT INDEX pidx FOR (n:A) ON (n.z)");
    ok(&mut g, "CREATE FULLTEXT INDEX fidx FOR (n:A) ON EACH [n.w]");

    let range = ok(&mut g, "SHOW RANGE INDEXES YIELD name RETURN name");
    assert_eq!(range.len(), 1);
    assert_eq!(range[0][0], CypherValue::String("ridx".into()));
    let text = ok(&mut g, "SHOW TEXT INDEXES YIELD name RETURN name");
    assert_eq!(text.len(), 1);
    let point = ok(&mut g, "SHOW POINT INDEXES YIELD name RETURN name");
    assert_eq!(point.len(), 1);
    let ft = ok(&mut g, "SHOW FULLTEXT INDEXES");
    assert_eq!(ft.len(), 1);
    let lookup = ok(&mut g, "SHOW LOOKUP INDEXES YIELD name RETURN name");
    assert!(lookup.is_empty());
    let all = ok(
        &mut g,
        "SHOW ALL INDEXES YIELD name RETURN count(name) AS c",
    );
    assert_eq!(all[0][0], CypherValue::Integer(4));
}
