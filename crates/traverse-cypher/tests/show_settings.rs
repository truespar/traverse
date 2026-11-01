//! `SHOW SETTINGS`: the process settings registry, composable like the other SHOW commands (`YIELD`
//! / `WHERE` / `RETURN`). The server registers its resolved config at startup (secrets redacted);
//! the engine adds a couple of built-ins so it's never empty when embedded.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run(graph: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    execute_with_params(graph, q, &HashMap::new())
        .unwrap_or_else(|e| panic!("query failed: {q}\n{e}"))
        .rows
}

#[test]
fn bare_show_settings_lists_builtins() {
    let mut g = Graph::new();
    let rows = run(&mut g, "SHOW SETTINGS");
    assert!(!rows.is_empty(), "built-ins keep SHOW SETTINGS non-empty");
    // columns: name, value, isDynamic, defaultValue, description
    assert_eq!(rows[0].len(), 5);
    let names: Vec<String> = rows
        .iter()
        .map(|r| match &r[0] {
            CypherValue::String(s) => s.clone(),
            o => panic!("expected name string, got {o:?}"),
        })
        .collect();
    assert!(names.iter().any(|n| n == "internal.engine.version"));
    // sorted by name
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
}

#[test]
fn composable_yield_where_return() {
    traverse_cypher::settings::register_setting(
        "test.show_settings.flag",
        "on",
        "off",
        "test-only flag",
        true,
    );
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "SHOW SETTINGS YIELD name, value, isDynamic \
         WHERE name = 'test.show_settings.flag' \
         RETURN name, value, isDynamic",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0],
        CypherValue::String("test.show_settings.flag".into())
    );
    assert_eq!(rows[0][1], CypherValue::String("on".into()));
    assert_eq!(rows[0][2], CypherValue::Bool(true));
}

#[test]
fn where_only_form_filters() {
    let mut g = Graph::new();
    let rows = run(&mut g, "SHOW SETTINGS WHERE name CONTAINS 'engine.version'");
    assert_eq!(rows.len(), 1);
}

#[test]
fn singular_spelling_accepted() {
    let mut g = Graph::new();
    let rows = run(&mut g, "SHOW SETTING YIELD name RETURN count(name) AS c");
    assert!(matches!(rows[0][0], CypherValue::Integer(n) if n >= 2));
}

#[test]
fn registered_value_upserts() {
    traverse_cypher::settings::register_setting("test.show_settings.upsert", "v1", "", "d", false);
    traverse_cypher::settings::register_setting("test.show_settings.upsert", "v2", "", "d", false);
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "SHOW SETTINGS YIELD name, value WHERE name = 'test.show_settings.upsert' RETURN value",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("v2".into()));
}
