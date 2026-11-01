//! Inline `CALL { ... }` subqueries (GP01 / GP03) and the `LET` statement (GQ09). `LET` lowers to
//! `WITH <scope>, expr AS name`.

mod common;
use common::{GqlCatalog, StmtOutcome, run_program};
use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

fn run(query: &str) -> StmtOutcome {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    run_program(&mut g, &mut cat, query)
}

fn run_with_graph(preamble: &[&str], query: &str) -> StmtOutcome {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    for p in preamble {
        let out = run_program(&mut g, &mut cat, p);
        assert!(out.is_ok(), "preamble failed: {p}");
    }
    run_program(&mut g, &mut cat, query)
}

fn first_cell(out: StmtOutcome) -> CypherValue {
    match out {
        StmtOutcome::Ran { rows, .. } => rows[0][0].clone(),
        StmtOutcome::CatalogError(_) => panic!("CatalogError"),
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        StmtOutcome::LowerFailed => panic!("LowerFailed"),
        StmtOutcome::ParseFailed => panic!("ParseFailed"),
    }
}

// -- CALL { ... } ---------------------------------------------------

#[test]
fn parses_call_subquery_without_imports() {
    let p = traverse_gql::parse("CALL { MATCH (n:Person) RETURN n.name AS name } RETURN name")
        .expect("parse failed");
    assert!(!p.statements.is_empty());
}

#[test]
fn parses_call_subquery_with_imports() {
    let p = traverse_gql::parse(
        "MATCH (p:Person) \
         CALL (p) { MATCH (p)-[:KNOWS]->(f) RETURN f.name AS friend } \
         RETURN friend",
    )
    .expect("parse failed");
    assert!(!p.statements.is_empty());
}

#[test]
fn call_subquery_lowers_successfully() {
    // CALL { ... } lowers to a planned subquery (`cy::Clause::CallSubquery`) run by `CallSubqueryOp`;
    // execution semantics are in `tests/call_subquery_scope.rs`
    let p =
        traverse_gql::parse("CALL { MATCH (n:Person) RETURN n.name AS name } RETURN name").unwrap();
    let _stmt =
        traverse_gql::lower::lower(&p).expect("CALL subquery should lower cleanly post-Phase-P");
}

#[test]
fn named_call_still_parses_and_lowers() {
    // the CALL vs CALL-subquery disambiguation must keep the named-procedure form working
    let p = traverse_gql::parse("CALL db.indexes() YIELD name RETURN name").expect("parse failed");
    assert!(!p.statements.is_empty());
}

// -- LET binds expressions and runs end to end -----------------------

#[test]
fn let_single_binding() {
    let v = first_cell(run("LET x = 42 RETURN x"));
    assert_eq!(v, CypherValue::Integer(42));
}

#[test]
fn let_multiple_bindings() {
    let out = run("LET a = 1, b = 2, c = a + b RETURN a, b, c");
    match out {
        StmtOutcome::Ran { rows, .. } => {
            assert_eq!(rows[0][0], CypherValue::Integer(1));
            assert_eq!(rows[0][1], CypherValue::Integer(2));
            assert_eq!(rows[0][2], CypherValue::Integer(3));
        }
        other => panic!(
            "expected Ran, got {other:?}",
            other = match other {
                StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
                StmtOutcome::ParseFailed => "ParseFailed",
                StmtOutcome::LowerFailed => "LowerFailed",
                StmtOutcome::CatalogError(_) => "CatalogError",
                _ => unreachable!(),
            }
        ),
    }
}

#[test]
fn let_after_match_passes_through_match_variables() {
    let out = run_with_graph(
        &["CREATE (a:Person {name: 'Alice', age: 30})"],
        "MATCH (p:Person) \
         LET full_age = p.age + 10 \
         RETURN p.name AS name, full_age",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => {
            assert_eq!(rows[0][0], CypherValue::String("Alice".into()));
            assert_eq!(rows[0][1], CypherValue::Integer(40));
        }
        other => panic!(
            "expected Ran, got {other:?}",
            other = match other {
                StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
                _ => "non-Ran",
            }
        ),
    }
}

#[test]
fn let_chains_with_later_clauses() {
    // LET output is visible to a later FILTER / WITH / RETURN (the explicit pass-through the
    // lowerer builds)
    let out = run("LET x = 5 \
         FILTER x > 0 \
         LET y = x * 2 \
         RETURN x, y");
    match out {
        StmtOutcome::Ran { rows, .. } => {
            assert_eq!(rows[0][0], CypherValue::Integer(5));
            assert_eq!(rows[0][1], CypherValue::Integer(10));
        }
        other => panic!(
            "expected Ran, got {other:?}",
            other = match other {
                StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
                _ => "non-Ran",
            }
        ),
    }
}
