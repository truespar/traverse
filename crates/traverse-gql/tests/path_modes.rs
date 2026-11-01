//! ISO/IEC 39075 section 14.5 path modes (WALK / TRAIL / ACYCLIC) and section 14.6 selector limits (ANY / ANY k /
//! SHORTEST k) at execution time. The matcher honors the mode via a thread-local the GQL surface
//! installs from each query's AST.

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

fn count_only(out: StmtOutcome) -> i64 {
    match out {
        StmtOutcome::Ran { rows, .. } => match &rows[0][0] {
            CypherValue::Integer(n) => *n,
            other => panic!("expected Integer count, got {other:?}"),
        },
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        StmtOutcome::CatalogError(_) => panic!("CatalogError"),
        StmtOutcome::LowerFailed => panic!("LowerFailed"),
        StmtOutcome::ParseFailed => panic!("ParseFailed"),
    }
}

// -- ACYCLIC: node uniqueness on variable-length paths -------------

#[test]
fn acyclic_drops_paths_that_revisit_nodes() {
    // A->B->A cycle: without ACYCLIC, A->B->A is a valid 2-hop path (relationship isomorphism allows
    // it, the two edges have different ids); ACYCLIC rejects it since A appears twice
    let out = run_with_graph(
        &[
            "CREATE (a:Node {n: 'A'})-[:R]->(b:Node {n: 'B'})",
            "MATCH (a:Node {n: 'A'}), (b:Node {n: 'B'}) CREATE (b)-[:R]->(a)",
        ],
        "MATCH ACYCLIC (a:Node {n: 'A'})-[:R*1..3]->(b:Node) \
         RETURN count(*) AS n",
    );
    // reachable from A under ACYCLIC: just B (1 hop); A->B->A and A->B->A->B revisit nodes. Exactly 1
    // row.
    assert_eq!(count_only(out), 1);
}

#[test]
fn trail_default_allows_node_revisits() {
    // same data, TRAIL (the default) revisits nodes freely as long as each edge is used once: A->B->A
    // is allowed (edges e1, e2), A->B->A->B too if the two A->B edges have distinct ids
    let out = run_with_graph(
        &[
            "CREATE (a:Node {n: 'A'})-[:R]->(b:Node {n: 'B'})",
            "MATCH (a:Node {n: 'A'}), (b:Node {n: 'B'}) CREATE (b)-[:R]->(a)",
        ],
        "MATCH (a:Node {n: 'A'})-[:R*1..3]->(b:Node) RETURN count(*) AS n",
    );
    // 1-hop A->B, plus A->B->A (ends at A, which matches `(b:Node)`); 3-hop A->B->A->B reuses the A->B
    // edge, which TRAIL forbids. 2 rows.
    assert_eq!(count_only(out), 2);
}

// -- WALK: edges may repeat ----------------------------------------

#[test]
fn walk_allows_edge_repeats() {
    // single edge A->B: TRAIL gives 1 path; WALK allows any number of hops over it (capped at *1..3)
    let out = run_with_graph(
        &["CREATE (a:Node {n: 'A'})-[:R]->(b:Node {n: 'B'})"],
        "MATCH WALK (a:Node {n: 'A'})-[:R*1..3]->(b:Node) RETURN count(*) AS n",
    );
    // directed edge only (no B->A), so nothing chains past B: exactly 1 row under WALK too; edge
    // repeats need an undirected pattern or a back edge (next test)
    assert_eq!(count_only(out), 1);
}

#[test]
fn walk_undirected_allows_back_and_forth() {
    // single undirected edge A--B: TRAIL bans reusing it, WALK allows A->B->A->B etc.
    let out = run_with_graph(
        &["CREATE (a:Node {n: 'A'})-[:R]->(b:Node {n: 'B'})"],
        // undirected, back over the same edge
        "MATCH WALK (a:Node {n: 'A'})-[:R*1..3]-(b:Node) RETURN count(*) AS n",
    );
    // WALK reusing the single edge: A->B (ends B), A->B->A (ends A), A->B->A->B (ends B); 3 rows, TRAIL
    // would give only the first
    let n = count_only(out);
    assert!(n >= 3, "WALK should admit re-traversal; got {n} rows");
}

#[test]
fn trail_blocks_undirected_edge_reuse() {
    // same data and pattern as walk_undirected, but TRAIL forbids reusing the edge: only the 1-hop
    // A->B
    let out = run_with_graph(
        &["CREATE (a:Node {n: 'A'})-[:R]->(b:Node {n: 'B'})"],
        "MATCH (a:Node {n: 'A'})-[:R*1..3]-(b:Node) RETURN count(*) AS n",
    );
    assert_eq!(count_only(out), 1);
}

// -- ANY / ANY k: row limit on the outermost projection -------------

#[test]
fn any_selector_limits_to_one_row() {
    // three outgoing edges A->{B,C,D}: count would be 3, ANY makes the lowerer add LIMIT 1
    let out = run_with_graph(
        &[
            "CREATE (a:Node {n: 'A'})-[:R]->(:Node {n: 'B'})",
            "MATCH (a:Node {n: 'A'}) CREATE (a)-[:R]->(:Node {n: 'C'})",
            "MATCH (a:Node {n: 'A'}) CREATE (a)-[:R]->(:Node {n: 'D'})",
        ],
        // all 3 without ANY, at most 1 with it
        "MATCH ANY (a:Node {n: 'A'})-[:R]->(x:Node) RETURN x.n AS name",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => {
            assert_eq!(rows.len(), 1, "ANY should cap to 1 row, got {}", rows.len());
        }
        other => panic!(
            "expected Ran, got {other:?}",
            other = match &other {
                StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
                _ => "non-Ran",
            }
        ),
    }
}

#[test]
fn any_k_selector_limits_to_k_rows() {
    let out = run_with_graph(
        &[
            "CREATE (a:Node {n: 'A'})-[:R]->(:Node {n: 'B'})",
            "MATCH (a:Node {n: 'A'}) CREATE (a)-[:R]->(:Node {n: 'C'})",
            "MATCH (a:Node {n: 'A'}) CREATE (a)-[:R]->(:Node {n: 'D'})",
        ],
        "MATCH ANY 2 (a:Node {n: 'A'})-[:R]->(x:Node) RETURN x.n AS name",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => {
            assert_eq!(
                rows.len(),
                2,
                "ANY 2 should cap to 2 rows, got {}",
                rows.len()
            );
        }
        other => panic!(
            "expected Ran: {other:?}",
            other = match &other {
                StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
                _ => "non-Ran",
            }
        ),
    }
}

#[test]
fn user_limit_overrides_selector_limit() {
    // ANY 5, but the user wrote LIMIT 2: the user's LIMIT wins (only an empty slot gets one)
    let out = run_with_graph(
        &[
            "CREATE (a:Node {n: 'A'})-[:R]->(:Node {n: 'B'})",
            "MATCH (a:Node {n: 'A'}) CREATE (a)-[:R]->(:Node {n: 'C'})",
            "MATCH (a:Node {n: 'A'}) CREATE (a)-[:R]->(:Node {n: 'D'})",
        ],
        "MATCH ANY 5 (a:Node {n: 'A'})-[:R]->(x:Node) RETURN x.n LIMIT 2",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => {
            assert_eq!(rows.len(), 2, "user LIMIT 2 should win, got {}", rows.len());
        }
        other => panic!(
            "expected Ran: {other:?}",
            other = match &other {
                StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
                _ => "non-Ran",
            }
        ),
    }
}
