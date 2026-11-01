//! Deep-recursion safety: statement and expression shapes that drive recursion depth in the parser,
//! planner, optimizer, evaluator and the pull-based operator chain must run, or fail cleanly, on a
//! small stack. A Rust stack overflow aborts the process, so every case runs on a 1 MiB thread
//! (half the server's 2 MiB) to prove the growth / dedicated-stack machinery in
//! `traverse_cypher::stack` works, instead of trusting whatever stack the harness gives.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::stack::{DEEP_PLAN_DEPTH, MAX_CHAIN_LENGTH, MAX_NESTING_DEPTH};
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

fn run(graph: &mut Graph, q: &str) -> Result<Vec<Vec<CypherValue>>, String> {
    execute_with_params(graph, q, &HashMap::new())
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

fn ok(graph: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    run(graph, q).unwrap_or_else(|e| panic!("query failed: {e}"))
}

fn int(v: &CypherValue) -> i64 {
    match v {
        CypherValue::Integer(i) => *i,
        other => panic!("expected integer, got {other:?}"),
    }
}

// -- plan depth (one level per clause) ----------------------------------

#[test]
fn thousands_of_chained_create_clauses() {
    on_small_stack("chained_create", || {
        let mut g = Graph::new();
        let n = 3_000;
        let q: String = (0..n)
            .map(|i| format!("CREATE (:N {{i: {i}}})\n"))
            .collect();
        ok(&mut g, &q);
        let rows = ok(&mut g, "MATCH (n:N) RETURN count(n)");
        assert_eq!(int(&rows[0][0]), n);
    });
}

#[test]
fn thousands_of_chained_with_clauses_stream_a_value_through() {
    on_small_stack("chained_with", || {
        let mut g = Graph::new();
        let n = 2_000;
        let mut q = String::from("WITH 0 AS x\n");
        for _ in 0..n {
            q.push_str("WITH x + 1 AS x\n");
        }
        q.push_str("RETURN x");
        let rows = ok(&mut g, &q);
        assert_eq!(int(&rows[0][0]), n);
    });
}

#[test]
fn deep_plan_with_a_read_returning_many_rows() {
    // deep *and* wide: a long WITH chain over an UNWIND, so the pull chain runs once per row at
    // full depth (streaming would have to materialize)
    on_small_stack("deep_and_wide", || {
        let mut g = Graph::new();
        let mut q = String::from("UNWIND range(1, 500) AS i WITH i AS x\n");
        for _ in 0..(DEEP_PLAN_DEPTH * 4) {
            q.push_str("WITH x + 1 AS x\n");
        }
        q.push_str("RETURN sum(x)");
        let rows = ok(&mut g, &q);
        let expected: i64 = (1..=500).map(|i| i + DEEP_PLAN_DEPTH as i64 * 4).sum();
        assert_eq!(int(&rows[0][0]), expected);
    });
}

#[test]
fn bundled_movies_dataset_loads_on_a_small_stack() {
    // 210 CREATE clauses in one statement (movie-graph script size); overflows a 2 MiB thread
    // without the dedicated execution stack
    on_small_stack("movies", || {
        let mut g = Graph::new();
        let rows = ok(
            &mut g,
            "CALL apoc.example.movies() YIELD nodes, relationships RETURN nodes, relationships",
        );
        assert_eq!(int(&rows[0][0]), 169);
        assert_eq!(int(&rows[0][1]), 250);
    });
}

// -- expression shape ----------------------------------------------------

#[test]
fn long_or_chain_predicate() {
    on_small_stack("or_chain", || {
        let mut g = Graph::new();
        let terms = 4_000;
        let pred: Vec<String> = (1..=terms).map(|i| format!("i = {i}")).collect();
        let q = format!(
            "UNWIND range(1, 100) AS i WITH i WHERE {} RETURN count(i)",
            pred.join(" OR ")
        );
        let rows = ok(&mut g, &q);
        assert_eq!(int(&rows[0][0]), 100);
    });
}

#[test]
fn long_addition_chain_evaluates() {
    on_small_stack("addition_chain", || {
        let mut g = Graph::new();
        let terms = 4_000;
        let q = format!("RETURN {}", vec!["1"; terms].join(" + "));
        let rows = ok(&mut g, &q);
        assert_eq!(int(&rows[0][0]), terms as i64);
    });
}

#[test]
fn long_not_chain_is_parsed_iteratively() {
    on_small_stack("not_chain", || {
        let mut g = Graph::new();
        let q = format!("RETURN {}true", "NOT ".repeat(4_001));
        let rows = ok(&mut g, &q);
        assert_eq!(rows[0][0], CypherValue::Bool(false));
    });
}

#[test]
fn long_negation_chain_is_parsed_iteratively() {
    on_small_stack("negation_chain", || {
        let mut g = Graph::new();
        // spaced so the lexer sees 4001 minus tokens, not a pattern's dashes
        let q = format!("RETURN {}7", "- ".repeat(4_001));
        let rows = ok(&mut g, &q);
        assert_eq!(int(&rows[0][0]), -7);
    });
}

#[test]
fn nested_parentheses_within_the_limit() {
    on_small_stack("parens", || {
        let mut g = Graph::new();
        let depth = (MAX_NESTING_DEPTH - 8) as usize;
        let q = format!("RETURN {}42{}", "(".repeat(depth), ")".repeat(depth));
        let rows = ok(&mut g, &q);
        assert_eq!(int(&rows[0][0]), 42);
    });
}

#[test]
fn nested_list_literals_within_the_limit() {
    on_small_stack("lists", || {
        let mut g = Graph::new();
        let depth = (MAX_NESTING_DEPTH - 8) as usize;
        let q = format!("RETURN size({}1{})", "[".repeat(depth), "]".repeat(depth));
        let rows = ok(&mut g, &q);
        assert_eq!(int(&rows[0][0]), 1);
    });
}

#[test]
fn nested_function_calls_within_the_limit() {
    // each call level is a parser nesting level and a validate / plan / evaluate recursion level
    // (nested `CALL { CALL { ... } }` would be the subquery analogue, but isn't supported)
    on_small_stack("function_calls", || {
        let mut g = Graph::new();
        let depth = (MAX_NESTING_DEPTH - 8) as usize;
        let q = format!("RETURN {}-1{}", "abs(".repeat(depth), ")".repeat(depth));
        let rows = ok(&mut g, &q);
        assert_eq!(int(&rows[0][0]), 1);
    });
}

#[test]
fn nested_exists_subqueries() {
    on_small_stack("exists_subqueries", || {
        let mut g = Graph::new();
        ok(&mut g, "CREATE (:A {v: 1})-[:R]->(:B {v: 2})");
        let rows = ok(
            &mut g,
            "MATCH (a:A) WHERE EXISTS { MATCH (a)-->(b) WHERE EXISTS { MATCH (b) WHERE b.v = 2 } } RETURN a.v",
        );
        assert_eq!(int(&rows[0][0]), 1);
    });
}

// -- hard limits fail cleanly --------------------------------------------

#[test]
fn nesting_beyond_the_limit_is_a_syntax_error_not_a_crash() {
    on_small_stack("nesting_limit", || {
        let mut g = Graph::new();
        let depth = (MAX_NESTING_DEPTH + 200) as usize;
        let q = format!("RETURN {}42{}", "(".repeat(depth), ")".repeat(depth));
        let err = run(&mut g, &q).expect_err("must be rejected");
        assert!(err.contains("nesting too deep"), "unexpected error: {err}");
    });
}

#[test]
fn chain_beyond_the_limit_is_a_syntax_error_not_a_crash() {
    on_small_stack("chain_limit", || {
        let mut g = Graph::new();
        let q = format!("RETURN {}", vec!["1"; MAX_CHAIN_LENGTH + 10].join(" + "));
        let err = run(&mut g, &q).expect_err("must be rejected");
        assert!(
            err.contains("expression too long"),
            "unexpected error: {err}"
        );
    });
}
