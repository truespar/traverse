//! Deep-recursion safety for the GQL front end: the recursive-descent parser and the lowering to
//! the Cypher AST must handle long chains and deep nesting on a small stack, and reject input past
//! the shared limits cleanly. See `traverse_cypher::stack`.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::stack::{MAX_CHAIN_LENGTH, MAX_NESTING_DEPTH};
use traverse_cypher::types::CypherValue;

const SMALL_STACK: usize = 1024 * 1024;

/// Runs `f` on a 1 MiB thread named after the test, so an overflow (which aborts the process) is
/// attributable in the harness output.
fn on_small_stack<T: Send + 'static>(name: &str, f: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::Builder::new()
        .name(format!("small-stack::{name}"))
        .stack_size(SMALL_STACK)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("test thread panicked")
}

fn run(q: &str) -> Result<Vec<Vec<CypherValue>>, String> {
    let mut g = Graph::new();
    traverse_gql::execute_gql(&mut g, q, &HashMap::new())
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

fn int(v: &CypherValue) -> i64 {
    match v {
        CypherValue::Integer(i) => *i,
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn long_addition_chain_lowers_and_evaluates() {
    on_small_stack("addition_chain", || {
        let q = format!("RETURN {} AS s", vec!["1"; 4_000].join(" + "));
        let rows = run(&q).unwrap();
        assert_eq!(int(&rows[0][0]), 4_000);
    });
}

#[test]
fn long_or_chain_filter() {
    on_small_stack("or_chain", || {
        let pred: Vec<String> = (1..=4_000).map(|i| format!("i = {i}")).collect();
        let q = format!(
            "UNWIND [1, 2, 3] AS i FILTER {} RETURN count(i) AS c",
            pred.join(" OR ")
        );
        let rows = run(&q).unwrap();
        assert_eq!(int(&rows[0][0]), 3);
    });
}

#[test]
fn long_not_chain_is_parsed_iteratively() {
    on_small_stack("not_chain", || {
        let q = format!("RETURN {}TRUE AS b", "NOT ".repeat(4_001));
        let rows = run(&q).unwrap();
        assert_eq!(rows[0][0], CypherValue::Bool(false));
    });
}

#[test]
fn long_negation_chain_is_parsed_iteratively() {
    on_small_stack("negation_chain", || {
        // spaced: `--` opens a comment in GQL
        let q = format!("RETURN {}7 AS n", "- ".repeat(4_001));
        let rows = run(&q).unwrap();
        assert_eq!(int(&rows[0][0]), -7);
    });
}

#[test]
fn nested_parentheses_within_the_limit() {
    on_small_stack("parens", || {
        let depth = (MAX_NESTING_DEPTH - 8) as usize;
        let q = format!("RETURN {}42{} AS n", "(".repeat(depth), ")".repeat(depth));
        let rows = run(&q).unwrap();
        assert_eq!(int(&rows[0][0]), 42);
    });
}

#[test]
fn nesting_beyond_the_limit_is_an_error_not_a_crash() {
    on_small_stack("nesting_limit", || {
        let depth = (MAX_NESTING_DEPTH + 200) as usize;
        let q = format!("RETURN {}42{} AS n", "(".repeat(depth), ")".repeat(depth));
        let err = run(&q).expect_err("must be rejected");
        assert!(err.contains("nesting too deep"), "unexpected error: {err}");
    });
}

#[test]
fn chain_beyond_the_limit_is_an_error_not_a_crash() {
    on_small_stack("chain_limit", || {
        let q = format!(
            "RETURN {} AS s",
            vec!["1"; MAX_CHAIN_LENGTH + 10].join(" + ")
        );
        let err = run(&q).expect_err("must be rejected");
        assert!(
            err.contains("expression too long"),
            "unexpected error: {err}"
        );
    });
}
