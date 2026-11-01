//! `MATCH SHORTEST k` (G019, counted shortest path search) via a Yen-style BFS (`bfs_k_shortest`):
//! up to k loopless paths per (source, target), by ascending length, filling from longer tiers when
//! fewer than k share the minimum length. `ALL SHORTEST + LIMIT k` would cap rows without reaching
//! the next tier.

mod common;
use common::{GqlCatalog, StmtOutcome, run_program};
use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

/// Runs the preamble statements, then the query; panics if a preamble fails.
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

/// Integer node count from a `size(nodes(p))` column; nodes = edges + 1, so a 2-edge path has 3.
fn lengths(rows: &[Vec<CypherValue>], col: usize) -> Vec<i64> {
    rows.iter()
        .map(|r| match &r[col] {
            CypherValue::Integer(n) => *n,
            other => panic!("expected Integer, got {other:?}"),
        })
        .collect()
}

// -- single tier: >= k paths at the minimum length -----------------
// graph a -> b1 -> c, a -> b2 -> c, a -> b3 -> c: three length-2 paths a->c, `SHORTEST 2` returns 2 of
// them

#[test]
fn shortest_k_returns_k_when_enough_paths_share_min_length() {
    let out = run_with_graph(
        &[
            "CREATE (a:N {name: 'a'}), (b1:N {name: 'b1'}), (b2:N {name: 'b2'}), (b3:N {name: 'b3'}), (c:N {name: 'c'})",
            "MATCH (a:N {name: 'a'}), (b:N {name: 'b1'}) CREATE (a)-[:R]->(b)",
            "MATCH (a:N {name: 'a'}), (b:N {name: 'b2'}) CREATE (a)-[:R]->(b)",
            "MATCH (a:N {name: 'a'}), (b:N {name: 'b3'}) CREATE (a)-[:R]->(b)",
            "MATCH (b:N {name: 'b1'}), (c:N {name: 'c'}) CREATE (b)-[:R]->(c)",
            "MATCH (b:N {name: 'b2'}), (c:N {name: 'c'}) CREATE (b)-[:R]->(c)",
            "MATCH (b:N {name: 'b3'}), (c:N {name: 'c'}) CREATE (b)-[:R]->(c)",
        ],
        "MATCH (a:N {name: 'a'}), (c:N {name: 'c'}) \
         MATCH SHORTEST 2 p = (a)-[:R*]->(c) \
         RETURN size(nodes(p)) AS len",
    );
    let r = rows(out);
    assert_eq!(r.len(), 2, "expected 2 paths, got {}", r.len());
    let lens = lengths(&r, 0);
    assert!(
        lens.iter().all(|&l| l == 3),
        "expected all node-counts == 3 (= length 2), got {lens:?}"
    );
}

// -- multi-tier: ONE path at min length, more at min+1 --------------
//
// Graph: a -> b -> c       (length 2, shortest)
//        a -> d -> e -> c   (length 3)
//        a -> f -> g -> c   (length 3)
//
// `SHORTEST 1` returns the length-2 path; `SHORTEST 2` and `SHORTEST 3` MUST
// fill in from the length-3 tier (`ALL SHORTEST + LIMIT k` would return only
// the length-2 path for any k).

fn build_multitier_graph() -> Vec<&'static str> {
    vec![
        "CREATE (a:N {name: 'a'}), (b:N {name: 'b'}), (c:N {name: 'c'})",
        "CREATE (d:N {name: 'd'}), (e:N {name: 'e'}), (f:N {name: 'f'}), (g:N {name: 'g'})",
        // length-2 path a -> b -> c
        "MATCH (a:N {name: 'a'}), (b:N {name: 'b'}) CREATE (a)-[:R]->(b)",
        "MATCH (b:N {name: 'b'}), (c:N {name: 'c'}) CREATE (b)-[:R]->(c)",
        // length-3 path a -> d -> e -> c
        "MATCH (a:N {name: 'a'}), (d:N {name: 'd'}) CREATE (a)-[:R]->(d)",
        "MATCH (d:N {name: 'd'}), (e:N {name: 'e'}) CREATE (d)-[:R]->(e)",
        "MATCH (e:N {name: 'e'}), (c:N {name: 'c'}) CREATE (e)-[:R]->(c)",
        // length-3 path a -> f -> g -> c
        "MATCH (a:N {name: 'a'}), (f:N {name: 'f'}) CREATE (a)-[:R]->(f)",
        "MATCH (f:N {name: 'f'}), (g:N {name: 'g'}) CREATE (f)-[:R]->(g)",
        "MATCH (g:N {name: 'g'}), (c:N {name: 'c'}) CREATE (g)-[:R]->(c)",
    ]
}

#[test]
fn shortest_1_returns_the_min_length_path() {
    let out = run_with_graph(
        &build_multitier_graph(),
        "MATCH (a:N {name: 'a'}), (c:N {name: 'c'}) \
         MATCH SHORTEST 1 p = (a)-[:R*]->(c) \
         RETURN size(nodes(p)) AS len",
    );
    let r = rows(out);
    assert_eq!(r.len(), 1);
    // 2-edge path -> 3 nodes
    assert_eq!(lengths(&r, 0), vec![3]);
}

#[test]
fn shortest_2_fills_from_next_tier_when_only_one_min_length_path_exists() {
    // the case `ALL SHORTEST + LIMIT k` gets wrong
    let out = run_with_graph(
        &build_multitier_graph(),
        "MATCH (a:N {name: 'a'}), (c:N {name: 'c'}) \
         MATCH SHORTEST 2 p = (a)-[:R*]->(c) \
         RETURN size(nodes(p)) AS len ORDER BY len",
    );
    let r = rows(out);
    assert_eq!(r.len(), 2, "expected 2 paths, got {}", r.len());
    let lens = lengths(&r, 0);
    // one length-2 path (3 nodes), one length-3 path (4 nodes)
    assert_eq!(lens, vec![3, 4], "expected [3, 4], got {lens:?}");
}

#[test]
fn shortest_3_returns_all_three_paths_across_two_tiers() {
    let out = run_with_graph(
        &build_multitier_graph(),
        "MATCH (a:N {name: 'a'}), (c:N {name: 'c'}) \
         MATCH SHORTEST 3 p = (a)-[:R*]->(c) \
         RETURN size(nodes(p)) AS len ORDER BY len",
    );
    let r = rows(out);
    assert_eq!(r.len(), 3, "expected 3 paths, got {}", r.len());
    let lens = lengths(&r, 0);
    assert_eq!(
        lens,
        vec![3, 4, 4],
        "expected [3, 4, 4] node-counts, got {lens:?}"
    );
}

#[test]
fn shortest_k_returns_at_most_available_paths() {
    // asking for more paths than exist returns those that exist, no error
    let out = run_with_graph(
        &build_multitier_graph(),
        "MATCH (a:N {name: 'a'}), (c:N {name: 'c'}) \
         MATCH SHORTEST 10 p = (a)-[:R*]->(c) \
         RETURN size(nodes(p)) AS len ORDER BY len",
    );
    let r = rows(out);
    assert_eq!(r.len(), 3, "graph has 3 paths total, got {}", r.len());
    assert_eq!(lengths(&r, 0), vec![3, 4, 4]);
}

// -- no path ------------------------------------------------------

#[test]
fn shortest_k_returns_empty_when_no_path_exists() {
    let out = run_with_graph(
        &[
            "CREATE (a:N {name: 'a'}), (b:N {name: 'b'})",
            // no edges between a and b
        ],
        "MATCH (a:N {name: 'a'}), (b:N {name: 'b'}) \
         MATCH SHORTEST 3 p = (a)-[:R*]->(b) \
         RETURN size(nodes(p)) AS len",
    );
    let r = rows(out);
    assert!(r.is_empty(), "expected no rows, got {}", r.len());
}

// -- per-pair semantics --------------------------------------------
// with several endpoint pairs, SHORTEST k applies per pair, so total rows can exceed k

#[test]
fn shortest_k_caps_per_pair_not_total() {
    // two sources, each reaching `c` two ways; SHORTEST 1 -> one path per (source, c) = 2 rows
    let out = run_with_graph(
        &[
            "CREATE (a1:S {name: 'a1'}), (a2:S {name: 'a2'}), (c:N {name: 'c'})",
            "CREATE (m1:M {name: 'm1'}), (m2:M {name: 'm2'})",
            // a1 -> m1 -> c (length 2)
            "MATCH (a:S {name: 'a1'}), (m:M {name: 'm1'}) CREATE (a)-[:R]->(m)",
            "MATCH (m:M {name: 'm1'}), (c:N {name: 'c'}) CREATE (m)-[:R]->(c)",
            // a1 -> m2 -> c (length 2)
            "MATCH (a:S {name: 'a1'}), (m:M {name: 'm2'}) CREATE (a)-[:R]->(m)",
            "MATCH (m:M {name: 'm2'}), (c:N {name: 'c'}) CREATE (m)-[:R]->(c)",
            // a2 -> m1 -> c (length 2)
            "MATCH (a:S {name: 'a2'}), (m:M {name: 'm1'}) CREATE (a)-[:R]->(m)",
            // a2 -> m2 -> c (length 2)
            "MATCH (a:S {name: 'a2'}), (m:M {name: 'm2'}) CREATE (a)-[:R]->(m)",
        ],
        "MATCH (a:S), (c:N {name: 'c'}) \
         MATCH SHORTEST 1 p = (a)-[:R*]->(c) \
         RETURN a.name AS src",
    );
    let r = rows(out);
    // one shortest path per source, 2 rows (a top-level LIMIT 1 would give 1)
    assert_eq!(
        r.len(),
        2,
        "expected 2 rows (one per source), got {}",
        r.len()
    );
    let mut names: Vec<String> = r
        .iter()
        .map(|row| match &row[0] {
            CypherValue::String(s) => s.clone(),
            other => panic!("expected String, got {other:?}"),
        })
        .collect();
    names.sort();
    assert_eq!(names, vec!["a1", "a2"]);
}
