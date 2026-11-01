//! Composite/dynamic USE references (GQ01), the `IS NOTHING` value-type predicate (GV71), PATH as a
//! first-class type in `IS PATH` (GV55).

use traverse_gql::{Statement, UseReference, parse};

mod common;
use common::{GqlCatalog, StmtOutcome, run_program};
use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

fn parse_one(src: &str) -> Statement {
    let p = parse(src).expect("parse failed");
    assert_eq!(p.statements.len(), 1);
    p.statements.into_iter().next().unwrap()
}

// -- GQ01: composite USE references ------------------------------

#[test]
fn use_simple_identifier() {
    match parse_one("USE social") {
        Statement::Use {
            graph, reference, ..
        } => {
            assert_eq!(graph, "social");
            assert!(matches!(reference, UseReference::Simple));
        }
        other => panic!("expected USE, got {other:?}"),
    }
}

#[test]
fn use_composite_multi_segment() {
    match parse_one("USE myComposite.myGraph") {
        Statement::Use {
            graph, reference, ..
        } => {
            assert_eq!(graph, "myComposite");
            match reference {
                UseReference::Composite { segments } => {
                    assert_eq!(
                        segments,
                        vec!["myComposite".to_string(), "myGraph".to_string()]
                    );
                }
                other => panic!("expected Composite, got {other:?}"),
            }
        }
        other => panic!("expected USE, got {other:?}"),
    }
}

#[test]
fn use_composite_three_segments() {
    match parse_one("USE a.b.c") {
        Statement::Use {
            reference: UseReference::Composite { segments },
            ..
        } => {
            assert_eq!(
                segments,
                vec!["a".to_string(), "b".to_string(), "c".to_string()]
            );
        }
        other => panic!("expected Composite, got {other:?}"),
    }
}

#[test]
fn use_dynamic_function_call() {
    match parse_one("USE graph.byName('social')") {
        Statement::Use {
            reference:
                UseReference::Dynamic {
                    function,
                    arg_spans,
                },
            ..
        } => {
            assert_eq!(function, vec!["graph".to_string(), "byName".to_string()]);
            assert_eq!(arg_spans.len(), 1);
        }
        other => panic!("expected Dynamic, got {other:?}"),
    }
}

#[test]
fn use_dynamic_multiple_args() {
    match parse_one("USE graph.byElementId(1, 2, 3)") {
        Statement::Use {
            reference: UseReference::Dynamic { arg_spans, .. },
            ..
        } => {
            assert_eq!(arg_spans.len(), 3);
        }
        other => panic!("expected Dynamic, got {other:?}"),
    }
}

#[test]
fn use_dynamic_no_args() {
    match parse_one("USE graph.current()") {
        Statement::Use {
            reference: UseReference::Dynamic { arg_spans, .. },
            ..
        } => {
            assert!(arg_spans.is_empty());
        }
        other => panic!("expected Dynamic, got {other:?}"),
    }
}

// -- GV71: IS NOTHING ----------------------------------------------

fn first_cell(out: StmtOutcome) -> CypherValue {
    match out {
        StmtOutcome::Ran { rows, .. } => rows[0][0].clone(),
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        _ => panic!("non-Ran"),
    }
}

fn run(query: &str) -> StmtOutcome {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    run_program(&mut g, &mut cat, query)
}

#[test]
fn is_nothing_predicate_recognises_null() {
    // `IS NOTHING` is GQL's empty-type predicate, equivalent to `IS NULL`
    assert_eq!(
        first_cell(run("RETURN null IS NOTHING AS x")),
        CypherValue::Bool(true),
    );
}

#[test]
fn is_nothing_returns_false_on_value() {
    assert_eq!(
        first_cell(run("RETURN 1 IS NOTHING AS x")),
        CypherValue::Bool(false),
    );
    assert_eq!(
        first_cell(run("RETURN 'a' IS NOTHING AS x")),
        CypherValue::Bool(false),
    );
}

#[test]
fn is_not_nothing_negates() {
    assert_eq!(
        first_cell(run("RETURN 1 IS NOT NOTHING AS x")),
        CypherValue::Bool(true),
    );
    assert_eq!(
        first_cell(run("RETURN null IS NOT NOTHING AS x")),
        CypherValue::Bool(false),
    );
}

// -- GV55: PATH as a first-class type -----------------------------

#[test]
fn is_path_predicate_recognises_match_path_variable() {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    let pre = "CREATE (:N {n: 'A'})-[:R]->(:N {n: 'B'})";
    let out = run_program(&mut g, &mut cat, pre);
    assert!(out.is_ok());
    // `p` is a PATH bound by `=` in MATCH; `IS PATH` gives BOOLEAN true
    let out = run_program(
        &mut g,
        &mut cat,
        "MATCH p = (a:N)-[:R]->(b:N) RETURN p IS PATH AS x",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => {
            assert_eq!(rows[0][0], CypherValue::Bool(true));
        }
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        _ => panic!("non-Ran"),
    }
}

#[test]
fn is_path_returns_false_on_non_path() {
    assert_eq!(
        first_cell(run("RETURN 1 IS PATH AS x")),
        CypherValue::Bool(false),
    );
}
