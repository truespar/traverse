//! `NEXT [YIELD ...]` linear composition (ISO 39075 section 9.3, GQ20): each output row of the prior
//! statement pipes into the next statement's scope; an optional `YIELD` list filters the columns.
//! The lowerer fuses NEXT-chained queries into one Cypher statement (RETURN->WITH at each seam), so
//! the executor gets the cross-product semantics for free.

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

fn rows(out: StmtOutcome) -> Vec<Vec<CypherValue>> {
    match out {
        StmtOutcome::Ran { rows, .. } => rows,
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        StmtOutcome::CatalogError(_) => panic!("CatalogError"),
        StmtOutcome::LowerFailed => panic!("LowerFailed"),
        StmtOutcome::ParseFailed => panic!("ParseFailed"),
    }
}

fn strings(rows: &[Vec<CypherValue>], col: usize) -> Vec<String> {
    rows.iter()
        .map(|r| match &r[col] {
            CypherValue::String(s) => s.clone(),
            other => panic!("expected String in col {col}, got {other:?}"),
        })
        .collect()
}

fn ints(rows: &[Vec<CypherValue>], col: usize) -> Vec<i64> {
    rows.iter()
        .map(|r| match &r[col] {
            CypherValue::Integer(n) => *n,
            other => panic!("expected Integer in col {col}, got {other:?}"),
        })
        .collect()
}

// -- NEXT without YIELD pipes every column ------------------------

#[test]
fn next_without_yield_pipes_all_columns_to_next_statement() {
    // the first statement projects two scalars, the second uses both
    let out = run_with_graph(
        &["CREATE (:P {name: 'A', age: 30})"],
        "MATCH (p:P) RETURN p.name AS who, p.age AS years \
         NEXT \
         RETURN who, years + 1 AS bumped",
    );
    let r = rows(out);
    assert_eq!(r.len(), 1);
    assert_eq!(strings(&r, 0), vec!["A"]);
    assert_eq!(ints(&r, 1), vec![31]);
}

// -- YIELD filters which columns pipe -----------------------------

#[test]
fn next_yield_restricts_propagated_columns() {
    // `years` isn't in the YIELD list, so the second statement can't see it; only `who` propagates
    let out = run_with_graph(
        &["CREATE (:P {name: 'A', age: 30})"],
        "MATCH (p:P) RETURN p.name AS who, p.age AS years \
         NEXT YIELD who \
         RETURN who",
    );
    let r = rows(out);
    assert_eq!(r.len(), 1);
    assert_eq!(strings(&r, 0), vec!["A"]);
}

#[test]
fn next_yield_with_unknown_name_fails_to_lower() {
    // YIELD names something not in the prior RETURN: the fuser refuses to lower
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    let out = run_program(
        &mut g,
        &mut cat,
        "MATCH (p:P) RETURN p.name AS who \
         NEXT YIELD nope \
         RETURN nope",
    );
    assert!(matches!(out, StmtOutcome::LowerFailed));
}

// -- per-row piping: prior rows multiply into the next statement ----

#[test]
fn next_pipes_each_prior_row_into_next_statement() {
    // 3 rows in, a constant projection added: 3 rows out (one per prior row)
    let out = run_with_graph(
        &["CREATE (:P {name: 'a'}), (:P {name: 'b'}), (:P {name: 'c'})"],
        "MATCH (p:P) RETURN p.name AS x \
         NEXT \
         RETURN x, x + '!' AS bang ORDER BY x",
    );
    let r = rows(out);
    assert_eq!(r.len(), 3, "expected 3 piped rows, got {}", r.len());
    assert_eq!(strings(&r, 0), vec!["a", "b", "c"]);
    assert_eq!(strings(&r, 1), vec!["a!", "b!", "c!"]);
}

// -- YIELD AS renames the column for the next statement ---------

#[test]
fn next_yield_alias_renames_propagated_column() {
    // `who AS person`: the second statement sees `person`, not `who`
    let out = run_with_graph(
        &["CREATE (:P {name: 'A'})"],
        "MATCH (p:P) RETURN p.name AS who \
         NEXT YIELD who AS person \
         RETURN person",
    );
    let r = rows(out);
    assert_eq!(r.len(), 1);
    assert_eq!(strings(&r, 0), vec!["A"]);
}

// -- three-statement chain ----------------------------------------

#[test]
fn three_statement_next_chain() {
    let out = run_with_graph(
        &["CREATE (:P {name: 'A'}), (:P {name: 'B'})"],
        "MATCH (p:P) RETURN p.name AS x \
         NEXT \
         RETURN x AS y \
         NEXT \
         RETURN y AS z ORDER BY z",
    );
    let r = rows(out);
    assert_eq!(r.len(), 2);
    assert_eq!(strings(&r, 0), vec!["A", "B"]);
}

// -- aggregation in either statement ------------------------------

#[test]
fn aggregation_in_first_statement_pipes_count_forward() {
    let out = run_with_graph(
        &["CREATE (:P {name: 'a'}), (:P {name: 'b'}), (:P {name: 'c'})"],
        "MATCH (p:P) RETURN count(p) AS n \
         NEXT \
         RETURN n, n * 2 AS doubled",
    );
    let r = rows(out);
    assert_eq!(r.len(), 1);
    assert_eq!(ints(&r, 0), vec![3]);
    assert_eq!(ints(&r, 1), vec![6]);
}
