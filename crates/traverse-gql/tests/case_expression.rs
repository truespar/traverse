//! ISO/IEC 39075 `CASE` expression, simple and searched forms, end to end: parse -> lower -> execute
//! on an in-memory graph.

mod common;
use common::{GqlCatalog, StmtOutcome, run_program};
use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

fn first_cell(query: &str) -> CypherValue {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    match run_program(&mut g, &mut cat, query) {
        StmtOutcome::Ran { rows, .. } => rows[0][0].clone(),
        StmtOutcome::CatalogError(_) => panic!("expected Ran, got CatalogError"),
        StmtOutcome::EngineError(e) => panic!("expected Ran, got EngineError: {e}"),
        StmtOutcome::LowerFailed => panic!("expected Ran, got LowerFailed"),
        StmtOutcome::ParseFailed => panic!("expected Ran, got ParseFailed"),
    }
}

#[test]
fn simple_case_matches_on_equality() {
    // operand 2: the second WHEN arm matches
    let v = first_cell(
        "RETURN CASE 2 \
           WHEN 1 THEN 'one' \
           WHEN 2 THEN 'two' \
           WHEN 3 THEN 'three' \
         END AS x",
    );
    assert_eq!(v, CypherValue::String("two".into()));
}

#[test]
fn simple_case_else_branch() {
    let v = first_cell(
        "RETURN CASE 9 \
           WHEN 1 THEN 'one' \
           WHEN 2 THEN 'two' \
           ELSE 'other' \
         END AS x",
    );
    assert_eq!(v, CypherValue::String("other".into()));
}

#[test]
fn simple_case_no_match_no_else_returns_null() {
    let v = first_cell(
        "RETURN CASE 9 \
           WHEN 1 THEN 'one' \
           WHEN 2 THEN 'two' \
         END AS x",
    );
    assert_eq!(v, CypherValue::Null);
}

#[test]
fn searched_case_first_truthy_arm_wins() {
    let v = first_cell(
        "RETURN CASE \
           WHEN 1 > 2 THEN 'a' \
           WHEN 3 > 2 THEN 'b' \
           WHEN 4 > 2 THEN 'c' \
         END AS x",
    );
    assert_eq!(v, CypherValue::String("b".into()));
}

#[test]
fn searched_case_else_branch() {
    let v = first_cell(
        "RETURN CASE \
           WHEN false THEN 'a' \
           WHEN false THEN 'b' \
           ELSE 'c' \
         END AS x",
    );
    assert_eq!(v, CypherValue::String("c".into()));
}

#[test]
fn case_nested_in_arithmetic() {
    // CASE is an expression, usable wherever a value is
    let v = first_cell("RETURN 10 + CASE WHEN true THEN 5 ELSE 0 END AS x");
    assert_eq!(v, CypherValue::Integer(15));
}

#[test]
fn case_with_string_concat() {
    // combined with `||`
    let v = first_cell("RETURN 'value: ' || CASE 1 WHEN 1 THEN 'one' ELSE 'other' END AS x");
    assert_eq!(v, CypherValue::String("value: one".into()));
}
