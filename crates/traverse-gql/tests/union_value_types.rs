//! Dynamic union value types (GV66 open / GV67 closed): `IS T1 | T2 | ...` in IS-predicate form.

mod common;
use common::{GqlCatalog, StmtOutcome, run_program};
use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

fn first_cell(query: &str) -> CypherValue {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    match run_program(&mut g, &mut cat, query) {
        StmtOutcome::Ran { rows, .. } => rows[0][0].clone(),
        StmtOutcome::CatalogError(_) => panic!("CatalogError"),
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        StmtOutcome::LowerFailed => panic!("LowerFailed"),
        StmtOutcome::ParseFailed => panic!("ParseFailed"),
    }
}

#[test]
fn union_two_types_matches_either() {
    // 1 matches INTEGER, not STRING: true
    assert_eq!(
        first_cell("RETURN 1 IS INTEGER | STRING AS x"),
        CypherValue::Bool(true),
    );
    // 'a' matches STRING: true
    assert_eq!(
        first_cell("RETURN 'a' IS INTEGER | STRING AS x"),
        CypherValue::Bool(true),
    );
    // true matches neither: false
    assert_eq!(
        first_cell("RETURN true IS INTEGER | STRING AS x"),
        CypherValue::Bool(false),
    );
}

#[test]
fn union_three_types() {
    assert_eq!(
        first_cell("RETURN 1.5 IS INTEGER | FLOAT | STRING AS x"),
        CypherValue::Bool(true),
    );
    assert_eq!(
        first_cell("RETURN [1,2,3] IS INTEGER | FLOAT | STRING AS x"),
        CypherValue::Bool(false),
    );
}

#[test]
fn union_with_not_negates_or() {
    // NOT (1 is INTEGER OR 1 is STRING) = NOT (true OR false) = false
    assert_eq!(
        first_cell("RETURN 1 IS NOT INTEGER | STRING AS x"),
        CypherValue::Bool(false),
    );
    // NOT (true is INTEGER OR true is STRING) = NOT (false) = true
    assert_eq!(
        first_cell("RETURN true IS NOT INTEGER | STRING AS x"),
        CypherValue::Bool(true),
    );
}

#[test]
fn union_with_temporal_and_scalar() {
    assert_eq!(
        first_cell("RETURN DATE '2024-01-01' IS DATE | STRING AS x"),
        CypherValue::Bool(true),
    );
    assert_eq!(
        first_cell("RETURN 'hi' IS DATE | STRING AS x"),
        CypherValue::Bool(true),
    );
    assert_eq!(
        first_cell("RETURN 1 IS DATE | STRING AS x"),
        CypherValue::Bool(false),
    );
}

#[test]
fn union_with_null_returns_false() {
    // ISO section 19.6: type predicates never return null; each component is false on null, so the OR is
    // false
    assert_eq!(
        first_cell("RETURN null IS INTEGER | STRING AS x"),
        CypherValue::Bool(false),
    );
    // ...so `IS NOT (T1|T2)` on null is true
    assert_eq!(
        first_cell("RETURN null IS NOT INTEGER | STRING AS x"),
        CypherValue::Bool(true),
    );
}

#[test]
fn union_with_any_short_circuits_to_not_null() {
    // ANY matches any non-null value, so `IS INTEGER | ANY` is `IS NOT NULL` for non-null inputs
    assert_eq!(
        first_cell("RETURN 'x' IS INTEGER | ANY AS x"),
        CypherValue::Bool(true),
    );
    assert_eq!(
        first_cell("RETURN null IS INTEGER | ANY AS x"),
        CypherValue::Bool(false),
    );
}

#[test]
fn union_with_nothing_alternative() {
    // `IS T | NOTHING` is true for T values or null
    assert_eq!(
        first_cell("RETURN 1 IS INTEGER | NOTHING AS x"),
        CypherValue::Bool(true),
    );
    assert_eq!(
        first_cell("RETURN null IS INTEGER | NOTHING AS x"),
        CypherValue::Bool(true),
    );
    assert_eq!(
        first_cell("RETURN 'a' IS INTEGER | NOTHING AS x"),
        CypherValue::Bool(false),
    );
}

#[test]
fn union_mixes_with_node_types_via_match() {
    // operands from a pattern match: a `:P` node is a NODE, INTEGER doesn't match
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    assert!(run_program(&mut g, &mut cat, "CREATE (:P {x: 1})").is_ok());
    let out = run_program(
        &mut g,
        &mut cat,
        "MATCH (n:P) RETURN n IS NODE | EDGE AS isObj, n.x IS INTEGER | STRING AS isVal",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => {
            assert_eq!(rows[0][0], CypherValue::Bool(true));
            assert_eq!(rows[0][1], CypherValue::Bool(true));
        }
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        StmtOutcome::CatalogError(_) => panic!("CatalogError"),
        StmtOutcome::LowerFailed => panic!("LowerFailed"),
        StmtOutcome::ParseFailed => panic!("ParseFailed"),
    }
}
