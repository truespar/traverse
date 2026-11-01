//! ISO/IEC 39075 section 19.4 `EXISTS { query }` existential subquery (optional feature GQ22).

mod common;
use common::{GqlCatalog, StmtOutcome, run_program};
use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

fn run_with_graph(preamble: &[&str], query: &str) -> StmtOutcome {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    for p in preamble {
        let out = run_program(&mut g, &mut cat, p);
        assert!(out.is_ok(), "preamble failed: {p}");
    }
    run_program(&mut g, &mut cat, query)
}

fn rows_of(out: StmtOutcome) -> Vec<Vec<CypherValue>> {
    match out {
        StmtOutcome::Ran { rows, .. } => rows,
        StmtOutcome::CatalogError(_) => panic!("CatalogError"),
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        StmtOutcome::LowerFailed => panic!("LowerFailed"),
        StmtOutcome::ParseFailed => panic!("ParseFailed"),
    }
}

#[test]
fn exists_subquery_with_pattern_shortcut() {
    let out = run_with_graph(
        &[
            "CREATE (alice:Person {name: 'Alice'})",
            "CREATE (alice:Person {name: 'AliceTwo'})-[:KNOWS]->(bob:Person {name: 'Bob'})",
        ],
        "MATCH (p:Person) \
         RETURN p.name, EXISTS { (p)-[:KNOWS]->() } AS hasFriend",
    );
    let rows = rows_of(out);
    // rows are [name, has_friend]: the Alice without a KNOWS edge and Bob are false, the other
    // Alice true
    let pairs: Vec<(String, bool)> = rows
        .into_iter()
        .map(|r| match (&r[0], &r[1]) {
            (CypherValue::String(s), CypherValue::Bool(b)) => (s.clone(), *b),
            other => panic!("unexpected row {other:?}"),
        })
        .collect();
    // one friendless Alice (false), one Alice with a friend (true), Bob without outgoing KNOWS
    // (false)
    let true_count = pairs.iter().filter(|(_, b)| *b).count();
    assert_eq!(
        true_count, 1,
        "exactly one row should have hasFriend=true; got {pairs:?}"
    );
}

#[test]
fn exists_subquery_with_full_match() {
    let out = run_with_graph(
        &["CREATE (a:City {name: 'A'})-[:ROAD]->(b:City {name: 'B'})"],
        "MATCH (c:City) \
         RETURN c.name, EXISTS { MATCH (c)-[:ROAD]->(other) } AS leavesCity",
    );
    let rows = rows_of(out);
    let pairs: Vec<(String, bool)> = rows
        .into_iter()
        .map(|r| match (&r[0], &r[1]) {
            (CypherValue::String(s), CypherValue::Bool(b)) => (s.clone(), *b),
            other => panic!("unexpected row {other:?}"),
        })
        .collect();
    if let Some((_, b)) = pairs.iter().find(|(n, _)| n == "A") {
        assert!(*b, "A leaves to B");
    }
    if let Some((_, b)) = pairs.iter().find(|(n, _)| n == "B") {
        assert!(!*b, "B has no outgoing");
    }
}

#[test]
fn exists_subquery_in_where_clause() {
    let out = run_with_graph(
        &[
            "CREATE (alice:Person {name: 'Alice'})",
            "CREATE (bob:Person {name: 'Bob'})-[:KNOWS]->(carol:Person {name: 'Carol'})",
        ],
        "MATCH (p:Person) \
         WHERE EXISTS { (p)-[:KNOWS]->() } \
         RETURN p.name AS name",
    );
    let rows = rows_of(out);
    let names: Vec<String> = rows
        .into_iter()
        .map(|r| match &r[0] {
            CypherValue::String(s) => s.clone(),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(names, vec!["Bob".to_string()]);
}

#[test]
fn exists_subquery_negated_with_not() {
    let out = run_with_graph(
        &[
            "CREATE (alice:Person {name: 'Alice'})",
            "CREATE (bob:Person {name: 'Bob'})-[:KNOWS]->(carol:Person {name: 'Carol'})",
        ],
        "MATCH (p:Person) \
         WHERE NOT EXISTS { (p)-[:KNOWS]->() } \
         RETURN p.name AS name \
         ORDER BY name",
    );
    let rows = rows_of(out);
    let names: Vec<String> = rows
        .into_iter()
        .map(|r| match &r[0] {
            CypherValue::String(s) => s.clone(),
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    // Alice and Carol have no outgoing KNOWS
    assert_eq!(names, vec!["Alice".to_string(), "Carol".to_string()]);
}

#[test]
fn exists_subquery_returns_boolean_literal_form() {
    // EXISTS yields BOOLEAN whatever the inner RETURN is (ISO section 19.4), not the subquery's rows
    let out = run_with_graph(
        &["CREATE (a:Person {name: 'A'})"],
        "MATCH (p:Person) RETURN EXISTS { MATCH (p) RETURN p.name } AS x",
    );
    let rows = rows_of(out);
    assert_eq!(rows[0][0], CypherValue::Bool(true));
}
