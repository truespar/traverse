//! ISO/IEC 39075 path-search prefixes: section 14.5 path modes and section 14.6 path selectors (optional features
//! G005 / G010 / G011 / G013 / G016 / G017 / G018 / G019 / G020).

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

fn assert_parses(query: &str) {
    let p = traverse_gql::parse(query)
        .unwrap_or_else(|errs| panic!("parse failed for `{query}`: {errs:?}"));
    assert!(
        !p.statements.is_empty(),
        "no statements parsed from `{query}`"
    );
}

#[test]
fn shortest_path_returns_one_path() {
    let out = run_with_graph(
        &[
            "CREATE (a:City {name: 'A'})-[:ROAD]->(b:City {name: 'B'})",
            "MATCH (a:City {name: 'A'}), (b:City {name: 'B'}) \
             CREATE (a)-[:ROAD]->(c:City {name: 'C'})-[:ROAD]->(b)",
        ],
        "MATCH SHORTEST (a:City {name: 'A'})-[:ROAD*]->(b:City {name: 'B'}) \
         RETURN count(*) AS n",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => {
            // exactly one shortest path between A and B (the direct edge)
            assert_eq!(rows[0][0], CypherValue::Integer(1));
        }
        other => panic!(
            "expected Ran, got {other:?}",
            other = match &other {
                StmtOutcome::CatalogError(_) => "CatalogError",
                StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
                StmtOutcome::LowerFailed => "LowerFailed",
                StmtOutcome::ParseFailed => "ParseFailed",
                StmtOutcome::Ran { .. } => unreachable!(),
            }
        ),
    }
}

#[test]
fn any_shortest_is_a_synonym_for_shortest() {
    let out = run_with_graph(
        &["CREATE (a:City {name: 'A'})-[:ROAD]->(b:City {name: 'B'})"],
        "MATCH ANY SHORTEST (a:City)-[:ROAD*]->(b:City) RETURN count(*) AS n",
    );
    assert!(matches!(out, StmtOutcome::Ran { .. }));
}

#[test]
fn all_shortest_returns_all_minimum_length_paths() {
    let out = run_with_graph(
        &[
            "CREATE (a:City {name: 'A'})-[:ROAD]->(b:City {name: 'B'})",
            "MATCH (a:City {name: 'A'}), (b:City {name: 'B'}) \
             CREATE (a)-[:ROAD]->(b)",
        ],
        "MATCH ALL SHORTEST (a:City {name: 'A'})-[:ROAD*]->(b:City {name: 'B'}) \
         RETURN count(*) AS n",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => {
            // two distinct length-1 paths between A and B (two ROAD edges)
            assert_eq!(rows[0][0], CypherValue::Integer(2));
        }
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        other => panic!(
            "expected Ran, got {other:?}",
            other = match other {
                StmtOutcome::CatalogError(_) => "CatalogError",
                StmtOutcome::LowerFailed => "LowerFailed",
                StmtOutcome::ParseFailed => "ParseFailed",
                _ => unreachable!(),
            }
        ),
    }
}

#[test]
fn trail_is_the_default_and_passes_through() {
    let out = run_with_graph(
        &["CREATE (a:Person)-[:KNOWS]->(b:Person)"],
        "MATCH TRAIL (a:Person)-[:KNOWS]->(b:Person) RETURN count(*) AS n",
    );
    assert!(matches!(out, StmtOutcome::Ran { .. }));
}

// -- parse coverage for the selectors -------------------------------

#[test]
fn parses_walk_mode() {
    // the grammar accepts the surface syntax
    assert_parses("MATCH WALK (a)-[:R]->(b) RETURN a");
}

#[test]
fn parses_acyclic_mode() {
    assert_parses("MATCH ACYCLIC (a)-[:R]->(b) RETURN a");
}

#[test]
fn parses_shortest_n() {
    assert_parses("MATCH SHORTEST 3 (a)-[:R*]->(b) RETURN a");
}

#[test]
fn parses_shortest_groups() {
    assert_parses("MATCH SHORTEST 3 GROUPS (a)-[:R*]->(b) RETURN a");
}

#[test]
fn parses_any_n() {
    assert_parses("MATCH ANY 5 (a)-[:R]->(b) RETURN a");
}

// -- lowering of WALK / SHORTEST k -----------------------------------

#[test]
fn walk_mode_lowers_cleanly() {
    // WALK reaches the executor via the path-mode thread-local; the lowerer just records the mode
    // on each pattern part
    let p = traverse_gql::parse("MATCH WALK (a)-[:R]->(b) RETURN a").unwrap();
    let stmt = traverse_gql::lower::lower(&p).expect("WALK should now lower");
    assert!(matches!(
        traverse_gql::lower::dominant_path_mode(&p),
        traverse_cypher::ast::PathMode::Walk,
    ));
    // the single MatchClause carries Walk on its PatternPart
    if let traverse_cypher::ast::Query::Regular(ref rq) = stmt.query
        && let Some(c) = rq.single.final_part.clauses.first()
        && let traverse_cypher::ast::Clause::Match(m) = c
    {
        assert_eq!(
            m.pattern.parts[0].path_mode,
            traverse_cypher::ast::PathMode::Walk
        );
        return;
    }
    panic!("expected a Match clause in the lowered statement");
}

#[test]
fn shortest_n_lowers_to_counted_path_mode() {
    // SHORTEST k becomes `Counted(k)` on the path pattern, not a result-set LIMIT: `bfs_k_shortest`
    // (Yen-style BFS) keeps per-pair semantics, and a top-level LIMIT would over-restrict queries
    // with several endpoint pairs
    let p = traverse_gql::parse("MATCH SHORTEST 3 (a)-[:R*]->(b) RETURN a").unwrap();
    let _stmt = traverse_gql::lower::lower(&p).expect("SHORTEST k should lower");
    assert_eq!(
        traverse_gql::lower::dominant_path_selector_limit(&p),
        None,
        "SHORTEST k is per-pair - no top-level LIMIT injection",
    );
}
