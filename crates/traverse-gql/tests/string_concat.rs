//! ISO/IEC 39075 section 20.23 string concatenation (`||`) end to end: parse -> lower -> execute on an
//! in-memory graph.

mod common;
use common::{GqlCatalog, StmtOutcome, run_program};
use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

fn run(query: &str) -> StmtOutcome {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    run_program(&mut g, &mut cat, query)
}

fn first_row_first_cell(out: &StmtOutcome) -> CypherValue {
    match out {
        StmtOutcome::Ran { rows, .. } => rows[0][0].clone(),
        other => panic!("expected Ran, got {:?}", debug_kind(other)),
    }
}

fn debug_kind(o: &StmtOutcome) -> &'static str {
    match o {
        StmtOutcome::Ran { .. } => "Ran",
        StmtOutcome::CatalogError(_) => "CatalogError",
        StmtOutcome::EngineError(_) => "EngineError",
        StmtOutcome::LowerFailed => "LowerFailed",
        StmtOutcome::ParseFailed => "ParseFailed",
    }
}

#[test]
fn concat_two_string_literals() {
    let out = run("RETURN 'foo' || 'bar' AS s");
    assert_eq!(
        first_row_first_cell(&out),
        CypherValue::String("foobar".into()),
    );
}

#[test]
fn concat_is_left_associative_three_operands() {
    let out = run("RETURN 'a' || 'b' || 'c' AS s");
    assert_eq!(
        first_row_first_cell(&out),
        CypherValue::String("abc".into()),
    );
}

#[test]
fn concat_with_null_propagates() {
    let out = run("RETURN null || 'x' AS s");
    assert_eq!(first_row_first_cell(&out), CypherValue::Null);
    let out = run("RETURN 'x' || null AS s");
    assert_eq!(first_row_first_cell(&out), CypherValue::Null);
}

#[test]
fn concat_rejects_non_string_operands() {
    // ISO section 20.23: `||` is strict, no numeric coercion, no list append (Cypher's `+` is the lenient
    // form), so integers are a type error
    let out = run("RETURN 1 || 'a' AS s");
    assert!(
        matches!(out, StmtOutcome::EngineError(_)),
        "expected EngineError, got {}",
        debug_kind(&out),
    );
    let out = run("RETURN 'a' || 2 AS s");
    assert!(matches!(out, StmtOutcome::EngineError(_)));
}

#[test]
fn concat_precedence_is_additive() {
    // `||` has additive precedence (ISO), so `'a' || 'b' = 'c'` is `('a' || 'b') = 'c'`: string ||
    // string, then `=` against a string
    let out = run("RETURN 'a' || 'b' = 'ab' AS eq");
    assert_eq!(first_row_first_cell(&out), CypherValue::Bool(true));
}
