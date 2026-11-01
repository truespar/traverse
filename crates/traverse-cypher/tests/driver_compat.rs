//! Driver/tool compatibility: the `CYPHER <version>|runtime=|planner=` query prefix, the `USING
//! JOIN` hint, `dbms.components()` / `db.awaitIndexes()`, and the `n {.*}` all-properties map
//! projection.

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
fn cypher_version_prefix_ignored() {
    let mut g = Graph::new();
    assert_eq!(
        run(&mut g, "CYPHER 25 RETURN 1 AS r").expect("v25")[0][0],
        CypherValue::Integer(1)
    );
    assert_eq!(
        run(&mut g, "CYPHER 5 RETURN 2 AS r").expect("v5")[0][0],
        CypherValue::Integer(2)
    );
}

#[test]
fn cypher_runtime_planner_prefix_ignored() {
    let mut g = Graph::new();
    assert_eq!(
        run(&mut g, "CYPHER runtime=parallel RETURN 3 AS r").expect("runtime")[0][0],
        CypherValue::Integer(3)
    );
    assert_eq!(
        run(&mut g, "CYPHER planner=cost RETURN 4 AS r").expect("planner")[0][0],
        CypherValue::Integer(4)
    );
    assert_eq!(
        run(&mut g, "CYPHER 25 runtime=pipelined RETURN 5 AS r").expect("combo")[0][0],
        CypherValue::Integer(5)
    );
}

#[test]
fn using_join_hint_accepted() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:A)-[:R]->(:B)").expect("seed");
    // USING JOIN ON b is advisory: must parse and run
    let rows = run(
        &mut g,
        "MATCH (a)-->(b) USING JOIN ON b RETURN count(*) AS c",
    )
    .expect("join hint");
    assert_eq!(rows[0][0], CypherValue::Integer(1));
}

#[test]
fn using_typed_index_hint_accepted() {
    let mut g = Graph::new();
    run(&mut g, "CREATE RANGE INDEX ix FOR (p:P) ON (p.age)").expect("index");
    run(&mut g, "CREATE (:P {age: 5})").expect("seed");
    let rows = run(
        &mut g,
        "MATCH (p:P) USING RANGE INDEX p:P(age) WHERE p.age=5 RETURN p.age AS a",
    )
    .expect("typed index hint");
    assert_eq!(rows[0][0], CypherValue::Integer(5));
}

#[test]
fn dbms_components_returns_version() {
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "CALL dbms.components() YIELD name, versions, edition RETURN name, versions, edition",
    )
    .expect("components");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("Traverse".into()));
    assert!(matches!(&rows[0][1], CypherValue::List(v) if !v.is_empty()));
    assert!(matches!(&rows[0][2], CypherValue::String(_)));
}

#[test]
fn db_await_indexes_is_noop_success() {
    let mut g = Graph::new();
    // void procedure: succeeds, no rows
    let rows = run(&mut g, "CALL db.awaitIndexes(1)").expect("awaitIndexes");
    assert_eq!(rows.len(), 0);
    run(&mut g, "CALL db.awaitIndex('x', 1)").expect("awaitIndex");
}

#[test]
fn map_projection_all_properties() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:Doc {name: 'a', size: 3})").expect("seed");
    let rows = run(&mut g, "MATCH (n:Doc) RETURN n {.*} AS m").expect("star projection");
    match &rows[0][0] {
        CypherValue::Map(m) => {
            assert_eq!(m.get("name"), Some(&CypherValue::String("a".into())));
            assert_eq!(m.get("size"), Some(&CypherValue::Integer(3)));
        }
        other => panic!("expected map, got {other:?}"),
    }
}

#[test]
fn map_projection_star_plus_explicit() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:Doc {name: 'a', size: 3})").expect("seed");
    // `.*` plus an explicit extra key still works
    let rows = run(&mut g, "MATCH (n:Doc) RETURN n {.*, .name} AS m").expect("mixed");
    match &rows[0][0] {
        CypherValue::Map(m) => assert_eq!(m.get("name"), Some(&CypherValue::String("a".into()))),
        other => panic!("expected map, got {other:?}"),
    }
}

#[test]
fn show_functions_lists_catalog() {
    let mut g = Graph::new();
    let rows = run(&mut g, "SHOW FUNCTIONS").expect("show functions");
    assert!(
        rows.len() > 40,
        "expected a sizeable function catalog, got {}",
        rows.len()
    );
    // row shape: name, category, description, signature, aggregating
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| match &r[0] {
            CypherValue::String(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    for want in ["abs", "count", "coll.sort", "point.distance", "elementId"] {
        assert!(names.iter().any(|n| n == want), "missing function {want}");
    }
    // `count` must be flagged aggregating
    let count_row = rows
        .iter()
        .find(|r| matches!(&r[0], CypherValue::String(s) if s=="count"))
        .unwrap();
    assert_eq!(count_row[4], CypherValue::Bool(true));
}

#[test]
fn show_procedures_lists_catalog() {
    let mut g = Graph::new();
    let rows = run(&mut g, "SHOW PROCEDURES").expect("show procedures");
    let names: Vec<String> = rows
        .iter()
        .filter_map(|r| match &r[0] {
            CypherValue::String(s) => Some(s.clone()),
            _ => None,
        })
        .collect();
    for want in ["db.labels", "dbms.components", "db.index.vector.queryNodes"] {
        assert!(
            names.iter().any(|n| n == want),
            "missing procedure {want}: {names:?}"
        );
    }
}
