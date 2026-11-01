//! Cypher 25 / GQL clauses in the Bolt dialect: `FILTER` (standalone WHERE), `LET` (additive
//! per-row bindings), `FINISH` (terminal, no result columns).

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
fn filter_standalone() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:P {age: 20}), (:P {age: 40})");
    let rows = ok(&mut g, "MATCH (p:P) FILTER p.age > 30 RETURN p.age");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::Integer(40));
}

#[test]
fn filter_chains_and_keeps_all_variables() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (:P {age: 20, name: 'a'}), (:P {age: 40, name: 'b'}), (:P {age: 50, name: 'c'})",
    );
    let rows = ok(
        &mut g,
        "MATCH (p:P) FILTER p.age > 30 FILTER p.name <> 'c' RETURN p.name, p.age",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("b".into()));
}

#[test]
fn filter_after_unwind() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "UNWIND [1, 2, 3, 4] AS x FILTER x % 2 = 0 RETURN x ORDER BY x",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], CypherValue::Integer(2));
}

#[test]
fn let_binds_additively() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:C {first: 'Ada', last: 'Lovelace'})");
    let rows = ok(
        &mut g,
        "MATCH (c:C) LET fullName = c.first + ' ' + c.last RETURN fullName, c.first",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("Ada Lovelace".into()));
    // c stays in scope (additive, unlike WITH)
    assert_eq!(rows[0][1], CypherValue::String("Ada".into()));
}

#[test]
fn let_multiple_bindings_and_chaining() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "UNWIND [1, 2] AS x LET double = x * 2, square = x * x LET both = double + square \
         RETURN x, double, square, both ORDER BY x",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1][1], CypherValue::Integer(4));
    assert_eq!(rows[1][2], CypherValue::Integer(4));
    assert_eq!(rows[1][3], CypherValue::Integer(8));
}

#[test]
fn let_binds_lists_and_nulls_verbatim() {
    let mut g = Graph::new();
    let rows = ok(&mut g, "LET l = [1, 2], n = null RETURN l, n");
    assert_eq!(rows.len(), 1);
    assert!(matches!(&rows[0][0], CypherValue::List(v) if v.len() == 2));
    assert_eq!(rows[0][1], CypherValue::Null);
}

#[test]
fn finish_runs_side_effects_no_result() {
    let mut g = Graph::new();
    let rows = ok(&mut g, "CREATE (:P {name: 'made'}) FINISH");
    assert!(rows.is_empty());
    let check = ok(&mut g, "MATCH (p:P) RETURN p.name");
    assert_eq!(check.len(), 1, "FINISH must still execute side effects");
}

#[test]
fn finish_on_pure_read() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:P)");
    let rows = ok(&mut g, "MATCH (p:P) FINISH");
    assert!(rows.is_empty());
}

#[test]
fn finish_rejects_trailing_clauses() {
    let mut g = Graph::new();
    assert!(run(&mut g, "MATCH (p:P) FINISH RETURN p").is_err());
}
