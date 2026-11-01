//! `MATCH SHORTEST k GROUPS` (G020, counted shortest group search) via length-tier DFS enumeration
//! (`bfs_k_groups`): *every* loopless path whose length is in one of the k smallest distinct length
//! classes between two bound endpoints.

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

/// Node count from a `size(nodes(p))` column.
fn lengths(rows: &[Vec<CypherValue>], col: usize) -> Vec<i64> {
    rows.iter()
        .map(|r| match &r[col] {
            CypherValue::Integer(n) => *n,
            other => panic!("expected Integer, got {other:?}"),
        })
        .collect()
}

// -- graph: two length-2 paths + two length-3 paths ----------------
//
//      +-> b1 -+
//   a -+       +-> c   (two length-2 paths)
//      +-> b2 -+
//
//      +-> d1 -> d2 -+
//   a -+             +-> c   (two length-3 paths)
//      +-> e1 -> e2 -+
//
// SHORTEST 1 GROUPS = all length-2 paths           (2 paths)
// SHORTEST 2 GROUPS = all length-2 + all length-3  (4 paths)

fn build_two_tier_graph() -> Vec<&'static str> {
    vec![
        "CREATE (a:N {name: 'a'}), (c:N {name: 'c'})",
        "CREATE (b1:N {name: 'b1'}), (b2:N {name: 'b2'})",
        "CREATE (d1:N {name: 'd1'}), (d2:N {name: 'd2'})",
        "CREATE (e1:N {name: 'e1'}), (e2:N {name: 'e2'})",
        // two length-2 paths
        "MATCH (a:N {name: 'a'}), (b:N {name: 'b1'}) CREATE (a)-[:R]->(b)",
        "MATCH (b:N {name: 'b1'}), (c:N {name: 'c'}) CREATE (b)-[:R]->(c)",
        "MATCH (a:N {name: 'a'}), (b:N {name: 'b2'}) CREATE (a)-[:R]->(b)",
        "MATCH (b:N {name: 'b2'}), (c:N {name: 'c'}) CREATE (b)-[:R]->(c)",
        // two length-3 paths
        "MATCH (a:N {name: 'a'}), (d:N {name: 'd1'}) CREATE (a)-[:R]->(d)",
        "MATCH (d:N {name: 'd1'}), (e:N {name: 'd2'}) CREATE (d)-[:R]->(e)",
        "MATCH (d:N {name: 'd2'}), (c:N {name: 'c'}) CREATE (d)-[:R]->(c)",
        "MATCH (a:N {name: 'a'}), (d:N {name: 'e1'}) CREATE (a)-[:R]->(d)",
        "MATCH (d:N {name: 'e1'}), (e:N {name: 'e2'}) CREATE (d)-[:R]->(e)",
        "MATCH (d:N {name: 'e2'}), (c:N {name: 'c'}) CREATE (d)-[:R]->(c)",
    ]
}

#[test]
fn shortest_1_groups_returns_all_min_length_paths() {
    // same as ALL SHORTEST: every path at the minimum length
    let out = run_with_graph(
        &build_two_tier_graph(),
        "MATCH (a:N {name: 'a'}), (c:N {name: 'c'}) \
         MATCH SHORTEST 1 GROUPS p = (a)-[:R*1..5]->(c) \
         RETURN size(nodes(p)) AS len ORDER BY len",
    );
    let r = rows(out);
    let lens = lengths(&r, 0);
    assert_eq!(
        lens.len(),
        2,
        "expected 2 length-2 paths, got {}",
        lens.len()
    );
    assert!(
        lens.iter().all(|&l| l == 3),
        "expected node-counts [3,3], got {lens:?}"
    );
}

#[test]
fn shortest_2_groups_returns_all_two_tiers() {
    // unlike SHORTEST 2: *every* length-2 AND *every* length-3 path, not two in total
    let out = run_with_graph(
        &build_two_tier_graph(),
        "MATCH (a:N {name: 'a'}), (c:N {name: 'c'}) \
         MATCH SHORTEST 2 GROUPS p = (a)-[:R*1..5]->(c) \
         RETURN size(nodes(p)) AS len ORDER BY len",
    );
    let r = rows(out);
    let lens = lengths(&r, 0);
    assert_eq!(
        lens.len(),
        4,
        "expected all 4 paths (2 + 2), got {}",
        lens.len()
    );
    // two of node count 3 (length 2), two of node count 4 (length 3)
    assert_eq!(lens, vec![3, 3, 4, 4]);
}

#[test]
fn shortest_3_groups_returns_two_tiers_when_only_two_exist() {
    // more groups than exist (only 2 length classes): all 4 paths
    let out = run_with_graph(
        &build_two_tier_graph(),
        "MATCH (a:N {name: 'a'}), (c:N {name: 'c'}) \
         MATCH SHORTEST 3 GROUPS p = (a)-[:R*1..5]->(c) \
         RETURN size(nodes(p)) AS len ORDER BY len",
    );
    let r = rows(out);
    assert_eq!(r.len(), 4);
    assert_eq!(lengths(&r, 0), vec![3, 3, 4, 4]);
}

// -- group != path count: SHORTEST k vs SHORTEST k GROUPS -------------

#[test]
fn k_groups_differs_from_plain_k_when_min_tier_is_larger_than_k() {
    // plain SHORTEST 1 returns one path, SHORTEST 1 GROUPS ALL min-length paths (2 here): the
    // distinction the GQL grammar draws
    let out_plain = run_with_graph(
        &build_two_tier_graph(),
        "MATCH (a:N {name: 'a'}), (c:N {name: 'c'}) \
         MATCH SHORTEST 1 p = (a)-[:R*1..5]->(c) \
         RETURN size(nodes(p)) AS len",
    );
    let plain = rows(out_plain);
    assert_eq!(plain.len(), 1, "SHORTEST 1 returns exactly one path");

    let out_groups = run_with_graph(
        &build_two_tier_graph(),
        "MATCH (a:N {name: 'a'}), (c:N {name: 'c'}) \
         MATCH SHORTEST 1 GROUPS p = (a)-[:R*1..5]->(c) \
         RETURN size(nodes(p)) AS len",
    );
    let groups = rows(out_groups);
    assert_eq!(
        groups.len(),
        2,
        "SHORTEST 1 GROUPS returns BOTH min-length paths"
    );
}

// -- no path -----------------------------------------------------

#[test]
fn k_groups_returns_empty_when_no_path_exists() {
    let out = run_with_graph(
        &[
            "CREATE (a:N {name: 'a'}), (b:N {name: 'b'})",
            // no edges
        ],
        "MATCH (a:N {name: 'a'}), (b:N {name: 'b'}) \
         MATCH SHORTEST 3 GROUPS p = (a)-[:R*1..5]->(b) \
         RETURN size(nodes(p)) AS len",
    );
    let r = rows(out);
    assert!(r.is_empty());
}

// -- per-pair semantics ------------------------------------------

#[test]
fn k_groups_caps_per_pair_not_total() {
    // two sources, each reaching `c` by two length-2 paths: SHORTEST 1 GROUPS per pair = 2 per
    // source = 4 rows
    let out = run_with_graph(
        &[
            "CREATE (a1:S {name: 'a1'}), (a2:S {name: 'a2'}), (c:N {name: 'c'})",
            "CREATE (m1:M {name: 'm1'}), (m2:M {name: 'm2'})",
            "MATCH (a:S {name: 'a1'}), (m:M {name: 'm1'}) CREATE (a)-[:R]->(m)",
            "MATCH (m:M {name: 'm1'}), (c:N {name: 'c'}) CREATE (m)-[:R]->(c)",
            "MATCH (a:S {name: 'a1'}), (m:M {name: 'm2'}) CREATE (a)-[:R]->(m)",
            "MATCH (m:M {name: 'm2'}), (c:N {name: 'c'}) CREATE (m)-[:R]->(c)",
            "MATCH (a:S {name: 'a2'}), (m:M {name: 'm1'}) CREATE (a)-[:R]->(m)",
            "MATCH (a:S {name: 'a2'}), (m:M {name: 'm2'}) CREATE (a)-[:R]->(m)",
        ],
        "MATCH (a:S), (c:N {name: 'c'}) \
         MATCH SHORTEST 1 GROUPS p = (a)-[:R*1..5]->(c) \
         RETURN a.name AS src ORDER BY src",
    );
    let r = rows(out);
    // 2 sources x 2 length-2 paths = 4 rows
    assert_eq!(r.len(), 4, "expected 4 rows total, got {}", r.len());
    let names: Vec<String> = r
        .iter()
        .map(|row| match &row[0] {
            CypherValue::String(s) => s.clone(),
            other => panic!("expected String, got {other:?}"),
        })
        .collect();
    // each source twice (one per path)
    assert_eq!(names, vec!["a1", "a1", "a2", "a2"]);
}
