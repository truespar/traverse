//! ISO 39075 section 20.2 typed temporal literals (`DATE '2024-01-01'`, `DATETIME '...'`, `DURATION '...'`,
//! ...), optional feature GV39.

mod common;
use common::{GqlCatalog, StmtOutcome, run_program};
use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

fn first_cell(out: StmtOutcome) -> CypherValue {
    match out {
        StmtOutcome::Ran { rows, .. } => rows[0][0].clone(),
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        StmtOutcome::CatalogError(_) => panic!("CatalogError"),
        StmtOutcome::LowerFailed => panic!("LowerFailed"),
        StmtOutcome::ParseFailed => panic!("ParseFailed"),
    }
}

fn run(query: &str) -> StmtOutcome {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    run_program(&mut g, &mut cat, query)
}

#[test]
fn date_typed_literal_returns_a_date() {
    let v = first_cell(run("RETURN DATE '2024-01-15' AS d"));
    assert!(
        matches!(v, CypherValue::Date(_)),
        "expected Date, got {v:?}"
    );
}

#[test]
fn datetime_typed_literal_returns_a_datetime() {
    let v = first_cell(run("RETURN DATETIME '2024-01-15T12:00:00Z' AS d"));
    assert!(
        matches!(v, CypherValue::DateTime(_)),
        "expected DateTime, got {v:?}",
    );
}

#[test]
fn zoned_datetime_typed_literal_returns_a_datetime() {
    let v = first_cell(run("RETURN ZONED_DATETIME '2024-01-15T12:00:00Z' AS d"));
    assert!(
        matches!(v, CypherValue::DateTime(_)),
        "expected DateTime, got {v:?}",
    );
}

#[test]
fn local_datetime_typed_literal_returns_a_local_datetime() {
    let v = first_cell(run("RETURN LOCAL_DATETIME '2024-01-15T12:00:00' AS d"));
    assert!(
        matches!(v, CypherValue::LocalDateTime(_)),
        "expected LocalDateTime, got {v:?}",
    );
}

#[test]
fn time_typed_literal_returns_a_time() {
    let v = first_cell(run("RETURN TIME '12:00:00Z' AS t"));
    assert!(
        matches!(v, CypherValue::Time(_)),
        "expected Time, got {v:?}"
    );
}

#[test]
fn local_time_typed_literal_returns_a_local_time() {
    let v = first_cell(run("RETURN LOCAL_TIME '12:00:00' AS t"));
    assert!(
        matches!(v, CypherValue::LocalTime(_)),
        "expected LocalTime, got {v:?}",
    );
}

#[test]
fn duration_typed_literal_returns_a_duration() {
    let v = first_cell(run("RETURN DURATION 'P1D' AS d"));
    assert!(
        matches!(v, CypherValue::Duration(_)),
        "expected Duration, got {v:?}",
    );
}

#[test]
fn date_typed_literal_round_trips_value() {
    // the literal is a real Date: equal to the constructor call
    let v = first_cell(run("RETURN DATE '2024-01-15' = date('2024-01-15') AS eq"));
    assert_eq!(v, CypherValue::Bool(true));
}

#[test]
fn function_call_form_still_works() {
    // `DATE(...)` with parens stays a function call, not shadowed by the typed-literal arm
    let v = first_cell(run("RETURN date('2024-01-15') AS d"));
    assert!(matches!(v, CypherValue::Date(_)));
}
