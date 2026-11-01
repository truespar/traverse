//! ISO/IEC 39075 section 15.4 statement-level conditional: top-level `WHEN cond THEN stmt [ELSE stmt] [END
//! WHEN]`. The condition is evaluated with no bindings (literals, comparisons, boolean
//! combinators), enough for the spec's "evaluable at compile time" intent without a procedural
//! sublanguage.

mod common;
use common::{GqlCatalog, StmtOutcome, run_program};
use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

fn run(query: &str) -> StmtOutcome {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    run_program(&mut g, &mut cat, query)
}

fn first_int(out: StmtOutcome) -> i64 {
    match out {
        StmtOutcome::Ran { rows, .. } if !rows.is_empty() => match &rows[0][0] {
            CypherValue::Integer(v) => *v,
            other => panic!("expected Integer, got {other:?}"),
        },
        StmtOutcome::Ran { .. } => panic!("expected non-empty rows"),
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        StmtOutcome::CatalogError(_) => panic!("CatalogError"),
        StmtOutcome::LowerFailed => panic!("LowerFailed"),
        StmtOutcome::ParseFailed => panic!("ParseFailed"),
    }
}

// -- constant true / false ---------------------------------------

#[test]
fn when_true_runs_then_branch() {
    let v = first_int(run("WHEN true THEN RETURN 1 AS x ELSE RETURN 2 AS x"));
    assert_eq!(v, 1);
}

#[test]
fn when_false_runs_else_branch() {
    let v = first_int(run("WHEN false THEN RETURN 1 AS x ELSE RETURN 2 AS x"));
    assert_eq!(v, 2);
}

#[test]
fn when_false_without_else_emits_empty_result() {
    let out = run("WHEN false THEN RETURN 1 AS x");
    match out {
        StmtOutcome::Ran { rows, .. } => assert!(rows.is_empty()),
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        StmtOutcome::CatalogError(_) => panic!("CatalogError"),
        StmtOutcome::LowerFailed => panic!("LowerFailed"),
        StmtOutcome::ParseFailed => panic!("ParseFailed"),
    }
}

// -- comparisons and combinators in the condition -----------------

#[test]
fn when_equality_comparison() {
    let v = first_int(run("WHEN 1 = 1 THEN RETURN 1 AS x ELSE RETURN 0 AS x"));
    assert_eq!(v, 1);
}

#[test]
fn when_inequality_comparison() {
    let v = first_int(run("WHEN 1 <> 2 THEN RETURN 1 AS x ELSE RETURN 0 AS x"));
    assert_eq!(v, 1);
}

#[test]
fn when_logical_and() {
    let v = first_int(run(
        "WHEN true AND false THEN RETURN 1 AS x ELSE RETURN 2 AS x",
    ));
    assert_eq!(v, 2);
}

#[test]
fn when_logical_or() {
    let v = first_int(run(
        "WHEN false OR true THEN RETURN 1 AS x ELSE RETURN 2 AS x",
    ));
    assert_eq!(v, 1);
}

#[test]
fn when_logical_not() {
    let v = first_int(run("WHEN NOT false THEN RETURN 1 AS x ELSE RETURN 2 AS x"));
    assert_eq!(v, 1);
}

// -- optional `END WHEN` ------------------------------------------

#[test]
fn when_with_end_when_terminator() {
    let v = first_int(run(
        "WHEN true THEN RETURN 42 AS x ELSE RETURN 0 AS x END WHEN",
    ));
    assert_eq!(v, 42);
}

// -- branches with read queries on an existing graph --------------

#[test]
fn when_branch_runs_read_query() {
    // the chosen branch runs as a normal statement: THEN matches existing nodes, so the dispatch
    // reaches the engine and returns rows from the current graph
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    assert!(
        run_program(&mut g, &mut cat, "CREATE (:N {x: 7})").is_ok(),
        "preamble CREATE should succeed",
    );
    let out = run_program(
        &mut g,
        &mut cat,
        "WHEN true THEN MATCH (n:N) RETURN n.x AS v \
         ELSE RETURN -1 AS v",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => {
            assert_eq!(rows.len(), 1);
            assert!(
                matches!(rows[0][0], CypherValue::Integer(7)),
                "expected THEN branch result of 7, got {:?}",
                rows[0][0],
            );
        }
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        StmtOutcome::CatalogError(_) => panic!("CatalogError"),
        StmtOutcome::LowerFailed => panic!("LowerFailed"),
        StmtOutcome::ParseFailed => panic!("ParseFailed"),
    }
}
