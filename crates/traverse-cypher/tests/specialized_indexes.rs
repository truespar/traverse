//! Specialized index DDL: `CREATE RANGE INDEX` maps to the b-tree index; `TEXT` (trigram), `POINT`
//! (R-tree) and `FULLTEXT` (BM25) are native indexes; `LOOKUP` gets a clear error (label/type
//! lookups are always available) instead of a confusing parse failure.

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
fn range_index_creates_and_accelerates() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE RANGE INDEX person_age FOR (p:Person) ON (p.age)",
    )
    .expect("create range");
    for a in [5, 10, 15] {
        run(&mut g, &format!("CREATE (:Person {{age: {a}}})")).expect("seed");
    }
    let rows = run(
        &mut g,
        "MATCH (p:Person) WHERE p.age = 10 RETURN p.age AS a",
    )
    .expect("query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::Integer(10));
}

#[test]
fn range_index_appears_in_show_indexes() {
    let mut g = Graph::new();
    run(&mut g, "CREATE RANGE INDEX ix FOR (p:Person) ON (p.age)").expect("create");
    let rows = run(&mut g, "SHOW INDEXES").expect("show");
    assert!(!rows.is_empty(), "expected the range index to be listed");
}

#[test]
fn range_index_if_not_exists() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE RANGE INDEX ix IF NOT EXISTS FOR (p:Person) ON (p.age)",
    )
    .expect("create");
}

#[test]
fn text_index_accepted() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE TEXT INDEX person_name FOR (p:Person) ON (p.name)",
    )
    .expect("create text");
    run(&mut g, "CREATE (:Person {name: 'alice'})").expect("seed");
    // CONTAINS over the text-indexed property
    let rows = run(
        &mut g,
        "MATCH (p:Person) WHERE p.name CONTAINS 'lic' RETURN p.name AS n",
    )
    .expect("query");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("alice".into()));
}

#[test]
fn point_index_accepted() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE POINT INDEX place_loc FOR (p:Place) ON (p.location)",
    )
    .expect("create point");
}

#[test]
fn fulltext_index_now_supported() {
    // FULLTEXT is a native BM25 index (see tests/fulltext_index.rs)
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE FULLTEXT INDEX ft FOR (d:Doc) ON EACH [d.body]",
    )
    .expect("create fulltext");
}

#[test]
fn lookup_index_clear_error() {
    let mut g = Graph::new();
    let err = run(&mut g, "CREATE LOOKUP INDEX look FOR (n) ON EACH labels(n)")
        .expect_err("lookup not needed");
    assert!(err.to_uppercase().contains("LOOKUP"), "got: {err}");
}

#[test]
fn plain_create_index_still_works() {
    // plain CREATE INDEX (both spellings) still works
    let mut g = Graph::new();
    run(&mut g, "CREATE INDEX ix FOR (p:Person) ON (p.age)").expect("plain create");
    run(&mut g, "CREATE INDEX ON :Company(name)").expect("memgraph-style create");
}
