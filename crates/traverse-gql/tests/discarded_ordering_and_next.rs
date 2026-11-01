//! GA07 (ordering by discarded binding variables) and GQ20 (NEXT as a statement separator, NEXT
//! YIELD).

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

// -- GA07: ORDER BY may reference variables not projected ----------

#[test]
fn order_by_discarded_binding_variable() {
    // projects `n.name` but orders by `m.age`: `m` is matched but dropped by the projection, yet
    // must stay in scope for ORDER BY (ISO section 16.6 / GA07)
    let out = run_with_graph(
        &[
            "CREATE (a:P {name: 'A'})-[:R]->(:Q {age: 30})",
            "MATCH (a:P {name: 'A'}) CREATE (a)-[:R]->(:Q {age: 10})",
            "MATCH (a:P {name: 'A'}) CREATE (a)-[:R]->(:Q {age: 20})",
        ],
        // three rows, same `n` (A), three different `m.age`; only `n.name` projected, ordered by
        // `m.age` ascending
        "MATCH (n:P)-[:R]->(m:Q) RETURN n.name AS x ORDER BY m.age",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => {
            assert_eq!(rows.len(), 3, "expected 3 rows");
            // every `x` is 'A', but the rows were sorted by the discarded `m.age`; dropping `m`
            // from scope before ORDER BY would error
            for r in &rows {
                assert_eq!(r[0], CypherValue::String("A".into()));
            }
        }
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        _ => panic!("expected Ran"),
    }
}

#[test]
fn order_by_discarded_relationship_variable() {
    // same, with the discarded variable a relationship
    let out = run_with_graph(
        &[
            "CREATE (a:P {name: 'A'})-[:R {w: 5}]->(:Q)",
            "MATCH (a:P {name: 'A'}) CREATE (a)-[:R {w: 1}]->(:Q)",
        ],
        "MATCH (n:P)-[r:R]->(:Q) RETURN n.name AS x ORDER BY r.w",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => assert_eq!(rows.len(), 2),
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        _ => panic!("non-Ran"),
    }
}

// -- GQ20: NEXT as a statement separator -------------------------

#[test]
fn next_separates_statements_at_program_level() {
    // GQL `NEXT` separates statements in a composite query, like `;`
    let out = run_with_graph(
        &["CREATE (:P {name: 'A'})"],
        "MATCH (n:P) RETURN n.name AS x NEXT MATCH (n:P) RETURN count(*) AS y",
    );
    match out {
        StmtOutcome::Ran { rows, columns, .. } => {
            // the **last** statement's result: `count(*) = 1`
            assert_eq!(columns, vec!["y".to_string()]);
            assert_eq!(rows[0][0], CypherValue::Integer(1));
        }
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        _ => panic!("non-Ran"),
    }
}

#[test]
fn next_yield_clause_parses_and_runs() {
    // `NEXT YIELD a, b` restricts which names reach the next statement; must parse and run end to
    // end
    let out = run_with_graph(
        &["CREATE (:P {name: 'A'})"],
        "MATCH (n:P) RETURN n.name AS x \
         NEXT YIELD x \
         MATCH (n:P) RETURN count(*) AS y",
    );
    match out {
        StmtOutcome::Ran { rows, columns, .. } => {
            assert_eq!(columns, vec!["y".to_string()]);
            assert_eq!(rows[0][0], CypherValue::Integer(1));
        }
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        _ => panic!("non-Ran"),
    }
}

#[test]
fn next_yield_with_alias_parses() {
    let out = run_with_graph(
        &["CREATE (:P {name: 'A'})"],
        "MATCH (n:P) RETURN n.name AS x \
         NEXT YIELD x AS firstName \
         MATCH (n:P) RETURN count(*) AS y",
    );
    assert!(matches!(out, StmtOutcome::Ran { .. }));
}
