//! Composable SHOW (Cypher 25 / Neo4j 2026.05): `SHOW <thing>` chained with `YIELD` / `WHERE` /
//! `RETURN` / `ORDER BY` / `SKIP` / `LIMIT`, e.g. `SHOW INDEXES WHERE ...`, `SHOW FUNCTIONS YIELD
//! name RETURN count(name)`.

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

fn int(v: &CypherValue) -> i64 {
    match v {
        CypherValue::Integer(i) => *i,
        _ => panic!("expected int, got {v:?}"),
    }
}

fn strings(rows: &[Vec<CypherValue>]) -> Vec<String> {
    rows.iter()
        .map(|r| match &r[0] {
            CypherValue::String(s) => s.clone(),
            other => panic!("expected string, got {other:?}"),
        })
        .collect()
}

#[test]
fn show_functions_yield_return_count() {
    // YIELD + RETURN aggregate
    let mut g = Graph::new();
    let rows = run(&mut g, "SHOW FUNCTIONS YIELD name RETURN count(name) AS c").expect("query");
    assert!(int(&rows[0][0]) > 40);
}

#[test]
fn show_procedures_yield_return_count() {
    let mut g = Graph::new();
    let rows = run(&mut g, "SHOW PROCEDURES YIELD name RETURN count(name) AS c").expect("query");
    assert!(int(&rows[0][0]) >= 8);
}

#[test]
fn show_functions_yield_where_return() {
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "SHOW FUNCTIONS YIELD name, category WHERE category = 'Aggregating' RETURN name",
    )
    .expect("query");
    let names = strings(&rows);
    assert!(names.iter().any(|n| n == "count"));
    assert!(names.iter().any(|n| n == "sum"));
    // non-aggregating functions filtered out
    assert!(!names.iter().any(|n| n == "abs"));
}

#[test]
fn show_functions_bare_where() {
    let mut g = Graph::new();
    // bare WHERE (no YIELD) filters the default columns, which the result keeps
    let rows = run(&mut g, "SHOW FUNCTIONS WHERE category = 'Numeric'").expect("query");
    assert!(!rows.is_empty());
    // every row's category column (index 1) is Numeric
    for r in &rows {
        assert_eq!(r[1], CypherValue::String("Numeric".into()));
    }
}

#[test]
fn show_functions_yield_where_order_by() {
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "SHOW FUNCTIONS YIELD name WHERE name STARTS WITH 'coll.' RETURN name ORDER BY name",
    )
    .expect("query");
    let names = strings(&rows);
    assert!(names.iter().all(|n| n.starts_with("coll.")));
    // sorted ascending
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
}

#[test]
fn show_functions_yield_alias() {
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "SHOW FUNCTIONS YIELD name AS fn RETURN count(fn) AS c",
    )
    .expect("query");
    assert!(int(&rows[0][0]) > 40);
}

#[test]
fn show_indexes_where_by_name() {
    let mut g = Graph::new();
    run(&mut g, "CREATE INDEX ix FOR (p:Person) ON (p.age)").expect("index");
    run(&mut g, "CREATE INDEX iy FOR (p:Person) ON (p.name)").expect("index");
    let rows = run(
        &mut g,
        "SHOW INDEXES YIELD name WHERE name = 'ix' RETURN name",
    )
    .expect("query");
    assert_eq!(strings(&rows), vec!["ix".to_string()]);
}

#[test]
fn show_functions_yield_limit() {
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "SHOW FUNCTIONS YIELD name RETURN name ORDER BY name LIMIT 3",
    )
    .expect("query");
    assert_eq!(rows.len(), 3);
}

#[test]
fn bare_show_still_works() {
    // the no-tail form still returns the full catalog
    let mut g = Graph::new();
    assert!(run(&mut g, "SHOW FUNCTIONS").expect("q").len() > 40);
    run(&mut g, "SHOW CONSTRAINTS").expect("q");
    run(&mut g, "SHOW INDEXES").expect("q");
}
