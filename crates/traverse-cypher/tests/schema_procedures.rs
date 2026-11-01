//! Schema-introspection procedures `db.labels()`, `db.propertyKeys()`, `db.relationshipTypes()`,
//! which drivers, BI tools and Studio call for autocomplete / schema.

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

fn strings(rows: &[Vec<CypherValue>]) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|r| match &r[0] {
            CypherValue::String(s) => s.clone(),
            other => panic!("expected string, got {other:?}"),
        })
        .collect();
    out.sort();
    out
}

fn seed() -> Graph {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:Person {name: 'a', age: 30})").expect("seed");
    run(&mut g, "CREATE (:Company {name: 'b', founded: 1999})").expect("seed");
    run(
        &mut g,
        "CREATE (:Person {name: 'c'})-[:WORKS_AT {since: 2020}]->(:Company {name: 'd'})",
    )
    .expect("seed");
    g
}

#[test]
fn db_labels_yield() {
    let mut g = seed();
    let rows = run(&mut g, "CALL db.labels() YIELD label RETURN label").expect("query");
    assert_eq!(
        strings(&rows),
        vec!["Company".to_string(), "Person".to_string()]
    );
}

#[test]
fn db_labels_standalone_no_yield() {
    let mut g = seed();
    let rows = run(&mut g, "CALL db.labels()").expect("query");
    assert_eq!(
        strings(&rows),
        vec!["Company".to_string(), "Person".to_string()]
    );
}

#[test]
fn db_relationship_types() {
    let mut g = seed();
    let rows = run(
        &mut g,
        "CALL db.relationshipTypes() YIELD relationshipType RETURN relationshipType",
    )
    .expect("query");
    assert_eq!(strings(&rows), vec!["WORKS_AT".to_string()]);
}

#[test]
fn db_property_keys() {
    let mut g = seed();
    let rows = run(
        &mut g,
        "CALL db.propertyKeys() YIELD propertyKey RETURN propertyKey",
    )
    .expect("query");
    let keys = strings(&rows);
    for want in ["age", "founded", "name", "since"] {
        assert!(
            keys.contains(&want.to_string()),
            "missing {want} in {keys:?}"
        );
    }
}

#[test]
fn db_labels_yield_with_alias() {
    let mut g = seed();
    let rows = run(
        &mut g,
        "CALL db.labels() YIELD label AS l RETURN l ORDER BY l",
    )
    .expect("query");
    assert_eq!(
        strings(&rows),
        vec!["Company".to_string(), "Person".to_string()]
    );
}

#[test]
fn db_labels_in_query_cross_join() {
    // in-query CALL: each input row cross-joins with every label
    let mut g = seed();
    let rows = run(
        &mut g,
        "UNWIND [1, 2] AS n CALL db.labels() YIELD label RETURN n, label",
    )
    .expect("query");
    // 2 input rows x 2 labels = 4 rows
    assert_eq!(rows.len(), 4);
}

#[test]
fn db_labels_empty_graph() {
    let mut g = Graph::new();
    let rows = run(&mut g, "CALL db.labels() YIELD label RETURN label").expect("query");
    assert_eq!(rows.len(), 0);
}

#[test]
fn db_schema_visualization_meta_graph() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:Person)-[:KNOWS]->(:Person)").expect("seed knows");
    run(&mut g, "CREATE (:Person)-[:WORKS_AT]->(:Company)").expect("seed works");
    let rows = run(
        &mut g,
        "CALL db.schema.visualization() YIELD nodes, relationships RETURN nodes, relationships",
    )
    .expect("schema viz");
    assert_eq!(rows.len(), 1);
    // nodes: one virtual node per label (Person, Company)
    let node_labels: Vec<String> = match &rows[0][0] {
        CypherValue::List(items) => items
            .iter()
            .map(|n| match n {
                CypherValue::Node { labels, .. } => labels[0].clone(),
                other => panic!("expected node, got {other:?}"),
            })
            .collect(),
        other => panic!("expected list, got {other:?}"),
    };
    assert!(node_labels.contains(&"Person".to_string()));
    assert!(node_labels.contains(&"Company".to_string()));
    // relationships: one virtual rel per (srcLabel, type, dstLabel)
    let rel_types: Vec<String> = match &rows[0][1] {
        CypherValue::List(items) => items
            .iter()
            .map(|e| match e {
                CypherValue::Edge { edge_type, .. } => edge_type.clone(),
                other => panic!("expected edge, got {other:?}"),
            })
            .collect(),
        other => panic!("expected list, got {other:?}"),
    };
    assert!(rel_types.contains(&"KNOWS".to_string()));
    assert!(rel_types.contains(&"WORKS_AT".to_string()));
}

#[test]
fn db_schema_visualization_empty_graph() {
    let mut g = Graph::new();
    let rows = run(&mut g, "CALL db.schema.visualization()").expect("schema viz");
    assert_eq!(rows.len(), 1);
    assert!(matches!(&rows[0][0], CypherValue::List(v) if v.is_empty()));
    assert!(matches!(&rows[0][1], CypherValue::List(v) if v.is_empty()));
}
