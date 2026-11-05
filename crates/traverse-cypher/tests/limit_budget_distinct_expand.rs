//! Row budgets under LIMIT and multi-source distinct expansion.
//!
//! A streaming LIMIT hands its remaining rows down as a row budget; an in-place expansion
//! under it starts that small and doubles. Chunk sizes must never change which rows come out
//! or their order, so every `LIMIT k` / `SKIP s LIMIT k` must be the matching slice of the
//! unlimited result. A distinct expansion under a global dedup expands every input source in
//! one walk; it must return what the plain chain does, deduplicated.
//!
//! Each query runs twice: in a one-thread pool (the plan stays sequential, where the budget
//! applies) and on the default pool (the morsel scheduler).

use std::collections::HashSet;

use traverse_core::graph::Graph;
use traverse_cypher::execute;
use traverse_cypher::types::CypherValue;

const N: i64 = 300;
const DEG: i64 = 8;

fn graph() -> Graph {
    let mut g = Graph::new();
    execute(
        &mut g,
        &format!("UNWIND range(0, {}) AS i CREATE (:N {{id: i}})", N - 1),
    )
    .unwrap();
    execute(&mut g, "CREATE INDEX FOR (n:N) ON (n.id)").unwrap();
    execute(
        &mut g,
        &format!(
            "UNWIND range(0, {n1}) AS i UNWIND range(0, {d1}) AS j \
             MATCH (a:N {{id: i}}), (b:N {{id: (i * 7 + j * 13 + 1) % {n}}}) \
             CREATE (a)-[:R]->(b)",
            n = N,
            n1 = N - 1,
            d1 = DEG - 1
        ),
    )
    .unwrap();
    // a self-loop and a parallel edge, for relationship uniqueness along the chains
    execute(&mut g, "MATCH (a:N {id: 0}) CREATE (a)-[:R]->(a)").unwrap();
    execute(
        &mut g,
        "MATCH (a:N {id: 1}), (b:N {id: 2}) CREATE (a)-[:R]->(b)",
    )
    .unwrap();
    g
}

fn rows(g: &mut Graph, q: &str) -> Vec<String> {
    execute(g, q)
        .unwrap_or_else(|e| panic!("{q}: {e}"))
        .rows
        .iter()
        .map(|r| format!("{r:?}"))
        .collect()
}

/// `q` on a one-thread pool (sequential plans) and on the default pool.
fn both(g: &mut Graph, q: &str) -> (Vec<String>, Vec<String>) {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    let seq = pool.install(|| rows(g, q));
    let par = rows(g, q);
    (seq, par)
}

fn plan_has(g: &mut Graph, q: &str, arg: &str) -> bool {
    fn walk(v: &CypherValue, arg: &str) -> bool {
        let CypherValue::Map(m) = v else { return false };
        if let Some(CypherValue::Map(a)) = m.get("args")
            && a.contains_key(arg)
        {
            return true;
        }
        match m.get("children") {
            Some(CypherValue::List(cs)) => cs.iter().any(|c| walk(c, arg)),
            _ => false,
        }
    }
    let r = execute(g, &format!("EXPLAIN {q}")).unwrap();
    walk(r.plan.as_ref().expect("an EXPLAIN plan"), arg)
}

const KS: &[usize] = &[1, 2, 3, 5, 17, 100, 1000, 2047, 2048, 2049, 5000];
const SKIPS: &[(usize, usize)] = &[(1, 1), (3, 2), (100, 7), (2047, 2), (2048, 1), (10, 3000)];

fn assert_limit_slices(g: &mut Graph, q: &str) {
    let (full, full_par) = both(g, q);
    assert_eq!(full, full_par, "sequential vs parallel order: {q}");
    assert!(!full.is_empty(), "the query should return rows: {q}");
    for &k in KS {
        let lq = format!("{q} LIMIT {k}");
        let (seq, par) = both(g, &lq);
        let want = &full[..k.min(full.len())];
        assert_eq!(seq, want, "{lq} (one thread)");
        assert_eq!(par, want, "{lq} (default pool)");
    }
    for &(s, k) in SKIPS {
        let lq = format!("{q} SKIP {s} LIMIT {k}");
        let (seq, par) = both(g, &lq);
        let lo = s.min(full.len());
        let want = &full[lo..(s + k).min(full.len())];
        assert_eq!(seq, want, "{lq} (one thread)");
        assert_eq!(par, want, "{lq} (default pool)");
    }
}

#[test]
fn limit_over_in_place_chains_is_a_prefix() {
    let mut g = graph();
    // the pokec pattern_long shape: four in-place hops from a seed, the last one incoming
    assert_limit_slices(
        &mut g,
        "MATCH (n1:N {id: 3})-[e1]->(n2)-[e2]->(n3)-[e3]->(n4)<-[e4]-(n5) \
         RETURN n1.id, n2.id, n3.id, n4.id, n5.id",
    );
    // through the self-loop and the parallel edge
    assert_limit_slices(
        &mut g,
        "MATCH (a:N {id: 0})-[r1]->(b)-[r2]->(c)-[r3]->(d) RETURN id(r1), id(r2), id(r3), d.id",
    );
    // a filter between the limit and the chain
    assert_limit_slices(
        &mut g,
        "MATCH (a:N {id: 5})-->(b)-->(c)-->(d) WHERE d.id % 3 = 0 RETURN a.id, b.id, c.id, d.id",
    );
    // a scan source: many input chunks feeding the hops
    assert_limit_slices(&mut g, "MATCH (a:N)-->(b)-->(c) RETURN a.id, b.id, c.id");
}

fn as_set(v: &[String]) -> HashSet<String> {
    v.iter().cloned().collect()
}

/// The distinct query against the plain one deduplicated, on both pools.
fn assert_distinct_matches(g: &mut Graph, distinct: &str, plain: &str) {
    let (seq, par) = both(g, distinct);
    let (pseq, _) = both(g, plain);
    let want = as_set(&pseq);
    for (got, pool) in [(&seq, "one thread"), (&par, "default pool")] {
        assert_eq!(
            got.len(),
            as_set(got).len(),
            "duplicates ({pool}): {distinct}"
        );
        assert_eq!(as_set(got), want, "{distinct} ({pool})");
    }
}

#[test]
fn multi_source_distinct_expand_matches_the_plain_chain() {
    let mut g = graph();
    let two_hop = "MATCH (a:N)-[:R]->()-[:R]->(c:N) RETURN DISTINCT c.id";
    assert!(
        plan_has(&mut g, two_hop, "multi_source"),
        "a scan-sourced distinct chain expands all sources at once"
    );
    assert_distinct_matches(
        &mut g,
        two_hop,
        "MATCH (a:N)-[:R]->()-[:R]->(c:N) RETURN c.id",
    );
    assert_distinct_matches(
        &mut g,
        "MATCH (a:N)-[:R*1..3]->(c:N) RETURN DISTINCT c.id",
        "MATCH (a:N)-[:R*1..3]->(c:N) RETURN c.id",
    );
    // a filter on the target alone keeps the multi-source walk
    assert_distinct_matches(
        &mut g,
        "MATCH (a:N)-[:R]->()-[:R]->(c:N) WHERE c.id % 5 = 1 RETURN DISTINCT c.id",
        "MATCH (a:N)-[:R]->()-[:R]->(c:N) WHERE c.id % 5 = 1 RETURN c.id",
    );
    // a filter reading the source must see each source's own row
    assert_distinct_matches(
        &mut g,
        "MATCH (a:N)-[:R]->()-[:R]->(c:N) WHERE c.id > a.id + 250 RETURN DISTINCT c.id",
        "MATCH (a:N)-[:R]->()-[:R]->(c:N) WHERE c.id > a.id + 250 RETURN c.id",
    );
    // sources from a cross product of two groups
    assert_distinct_matches(
        &mut g,
        "MATCH (a:N), (z:N) WHERE a.id < 4 AND z.id < 3 WITH a, z \
         MATCH (a)-[:R]->()-[:R]->(c:N) RETURN DISTINCT c.id",
        "MATCH (a:N), (z:N) WHERE a.id < 4 AND z.id < 3 WITH a, z \
         MATCH (a)-[:R]->()-[:R]->(c:N) RETURN c.id",
    );
    // an empty side of the product: no rows, so no sources
    let (seq, par) = both(
        &mut g,
        "MATCH (a:N), (z:Missing) WITH a, z MATCH (a)-[:R]->()-[:R]->(c:N) RETURN DISTINCT c.id",
    );
    assert!(seq.is_empty() && par.is_empty(), "{seq:?} {par:?}");
    // count(DISTINCT end) over the fused chain
    let distinct_ends =
        as_set(&both(&mut g, "MATCH (a:N)-[:R]->()-[:R]->(c:N) RETURN c.id").0).len();
    let (seq, par) = both(
        &mut g,
        "MATCH (a:N)-[:R]->()-[:R]->(c:N) RETURN count(DISTINCT c) AS k",
    );
    let want = vec![format!(
        "{:?}",
        vec![CypherValue::Integer(distinct_ends as i64)]
    )];
    assert_eq!(seq, want);
    assert_eq!(par, want);
    // a seeded body re-run per outer row (reset between runs)
    assert_distinct_matches(
        &mut g,
        "MATCH (a:N) WHERE a.id < 20 \
         CALL { WITH a MATCH (a)-[:R]->()-[:R]->(c:N) RETURN DISTINCT c.id AS cid } \
         RETURN a.id, cid",
        "MATCH (a:N) WHERE a.id < 20 \
         CALL { WITH a MATCH (a)-[:R]->()-[:R]->(c:N) RETURN c.id AS cid } \
         RETURN a.id, cid",
    );
}

/// The fused chain keeps relationships unique: node 0's self-loop must not take it back to
/// itself in two hops (that would use the loop twice).
#[test]
fn fixed_length_distinct_chain_keeps_relationships_unique() {
    let mut g = graph();
    assert_distinct_matches(
        &mut g,
        "MATCH (a:N {id: 0})-[:R]->()-[:R]->(c:N) RETURN DISTINCT c.id",
        "MATCH (a:N {id: 0})-[:R]->()-[:R]->(c:N) RETURN c.id",
    );
    assert_distinct_matches(
        &mut g,
        "MATCH (a:N)-[:R]->()-[:R]->(c:N) WHERE a.id = 0 OR a.id > 290 RETURN DISTINCT c.id",
        "MATCH (a:N)-[:R]->()-[:R]->(c:N) WHERE a.id = 0 OR a.id > 290 RETURN c.id",
    );
}

/// Every relationship in both directions along a path (two-cycles everywhere), self-loops on
/// every seventh node, chords, a parallel edge, and a second type: walks that reuse a
/// relationship reach targets no trail does, at every length and in every direction.
fn reuse_graph() -> Graph {
    let mut g = Graph::new();
    let stmts = [
        "UNWIND range(0, 29) AS i CREATE (:M {id: i})",
        "MATCH (a:M), (b:M) WHERE b.id = a.id + 1 CREATE (a)-[:R]->(b), (b)-[:R]->(a)",
        "MATCH (a:M) WHERE a.id % 7 = 0 CREATE (a)-[:R]->(a)",
        "MATCH (a:M), (b:M) WHERE a.id % 3 = 0 AND b.id = (a.id * 5 + 3) % 30 CREATE (a)-[:R]->(b)",
        "MATCH (a:M {id: 4}), (b:M {id: 5}) CREATE (a)-[:R]->(b)",
        "MATCH (a:M), (b:M) WHERE a.id % 4 = 0 AND b.id = (a.id + 2) % 30 CREATE (a)-[:S]->(b)",
        // small components where the only way on reuses a relationship: an isolated two-cycle,
        // a self-loop with one way out, a single relationship (undirected: there and back), a
        // triangle with a two-cycle leaf
        "CREATE (x:M {id: 100})-[:R]->(y:M {id: 101}), (y)-[:R]->(x)",
        "CREATE (z:M {id: 102})-[:R]->(z), (z)-[:R]->(:M {id: 103})",
        "CREATE (:M {id: 104})-[:R]->(:M {id: 105})",
        "CREATE (a:M {id: 106})-[:R]->(b:M {id: 107})-[:R]->(c:M {id: 108})-[:R]->(a), \
         (c)-[:R]->(d:M {id: 109}), (d)-[:R]->(c)",
    ];
    for q in stmts {
        execute(&mut g, q).unwrap();
    }
    g
}

fn chain(hop: &str, k: usize) -> String {
    let mut c = String::new();
    for _ in 1..k {
        c.push_str(hop);
        c.push_str("()");
    }
    c.push_str(hop);
    c
}

#[test]
fn distinct_chains_are_trails_on_a_reuse_heavy_graph() {
    let mut g = reuse_graph();
    for hop in ["-[:R]->", "<-[:R]-", "-[:R]-", "-->", "--"] {
        for k in 2..=5 {
            let c = chain(hop, k);
            for src in [
                "MATCH (a:M {id: 0})",
                "MATCH (a:M {id: 7})",
                "MATCH (a:M {id: 15})",
                "MATCH (a:M {id: 29})",
                "MATCH (a:M {id: 100})",
                "MATCH (a:M {id: 102})",
                "MATCH (a:M {id: 104})",
                "MATCH (a:M {id: 106})",
                "MATCH (a:M) WHERE a.id >= 100",
                "MATCH (a:M)",
            ] {
                let d = format!("{src} MATCH (a){c}(t:M) RETURN DISTINCT t.id");
                let plain = format!("{src} MATCH (a){c}(t:M) RETURN t.id");
                assert!(plan_has_op(&mut g, &d, "DistinctExpand"), "fused: {d}");
                assert_distinct_matches(&mut g, &d, &plain);
                let filtered =
                    format!("{src} MATCH (a){c}(t:M) WHERE t.id % 3 = 1 RETURN DISTINCT t.id");
                let filtered_plain =
                    format!("{src} MATCH (a){c}(t:M) WHERE t.id % 3 = 1 RETURN t.id");
                assert_distinct_matches(&mut g, &filtered, &filtered_plain);
            }
        }
    }
}

/// Without self-loops two directed hops take the walk as is (only a self-loop reuses a
/// relationship within two directed hops): the shortcut must agree with the plain chain.
#[test]
fn two_directed_hops_without_self_loops() {
    let mut g = reuse_graph();
    execute(&mut g, "MATCH (a)-[r]->(a) DELETE r").unwrap();
    for hop in ["-[:R]->", "<-[:R]-", "-->"] {
        let c = chain(hop, 2);
        for src in [
            "MATCH (a:M {id: 0})",
            "MATCH (a:M {id: 100})",
            "MATCH (a:M)",
        ] {
            assert_distinct_matches(
                &mut g,
                &format!("{src} MATCH (a){c}(t:M) RETURN DISTINCT t.id"),
                &format!("{src} MATCH (a){c}(t:M) RETURN t.id"),
            );
        }
    }
}

#[test]
fn distinct_var_length_ranges_are_trails_on_a_reuse_heavy_graph() {
    let mut g = reuse_graph();
    for rel in ["-[:R*{r}]->", "<-[:R*{r}]-", "-[:R*{r}]-", "-[*{r}]->"] {
        for range in ["0..2", "1..1", "1..3", "2..2", "2..3", "3..4", "4..5"] {
            let r = rel.replace("{r}", range);
            for src in [
                "MATCH (a:M {id: 0})",
                "MATCH (a:M {id: 14})",
                "MATCH (a:M {id: 100})",
                "MATCH (a:M {id: 104})",
                "MATCH (a:M {id: 109})",
                "MATCH (a:M) WHERE a.id >= 100",
                "MATCH (a:M)",
            ] {
                let d = format!("{src} MATCH (a){r}(t:M) RETURN DISTINCT t.id");
                let plain = format!("{src} MATCH (a){r}(t:M) RETURN t.id");
                assert_distinct_matches(&mut g, &d, &plain);
                let counted = format!("{src} MATCH (a){r}(t:M) RETURN count(DISTINCT t) AS k");
                let want = as_set(&both(&mut g, &plain).0).len();
                let (seq, par) = both(&mut g, &counted);
                let want = vec![format!("{:?}", vec![CypherValue::Integer(want as i64)])];
                assert_eq!(seq, want, "{counted} (one thread)");
                assert_eq!(par, want, "{counted} (default pool)");
            }
        }
    }
}

#[test]
fn distinct_chains_keep_off_relationships_bound_elsewhere_in_the_pattern() {
    let mut g = reuse_graph();
    for (d, plain) in [
        (
            "MATCH (a:M {id: 3})-[r:R]->(b)-[:R*1..2]->(t:M) RETURN DISTINCT t.id",
            "MATCH (a:M {id: 3})-[r:R]->(b)-[:R*1..2]->(t:M) RETURN t.id",
        ),
        (
            "MATCH (a:M {id: 3})<-[r:R]-(b)-[:R]->()-[:R]->(t:M) RETURN DISTINCT t.id",
            "MATCH (a:M {id: 3})<-[r:R]-(b)-[:R]->()-[:R]->(t:M) RETURN t.id",
        ),
        (
            "MATCH (a:M {id: 3})-[r:R]->(b), (b)-[:R]->()-[:R]->(t:M) RETURN DISTINCT t.id",
            "MATCH (a:M {id: 3})-[r:R]->(b), (b)-[:R]->()-[:R]->(t:M) RETURN t.id",
        ),
        (
            "MATCH (a:M)-[r:R]-(b)-[:R*2..3]-(t:M) RETURN DISTINCT t.id",
            "MATCH (a:M)-[r:R]-(b)-[:R*2..3]-(t:M) RETURN t.id",
        ),
        // filters on what the chain binds inside: an inner node, an inner relationship
        (
            "MATCH (a:M {id: 0})-[:R]->(x)-[:R]->(t:M) WHERE x.id > 3 RETURN DISTINCT t.id",
            "MATCH (a:M {id: 0})-[:R]->(x)-[:R]->(t:M) WHERE x.id > 3 RETURN t.id",
        ),
        (
            "MATCH (a:M)-[:R]->()-[q:R]->()-[:R]->(t:M) WHERE id(q) % 2 = 0 RETURN DISTINCT t.id",
            "MATCH (a:M)-[:R]->()-[q:R]->()-[:R]->(t:M) WHERE id(q) % 2 = 0 RETURN t.id",
        ),
        // a filter on the source, rejecting the sources that reach each target first
        (
            "MATCH (a:M)-[:R]->()-[:R]->(t:M) WHERE a.id > 20 AND t.id <> a.id RETURN DISTINCT t.id",
            "MATCH (a:M)-[:R]->()-[:R]->(t:M) WHERE a.id > 20 AND t.id <> a.id RETURN t.id",
        ),
    ] {
        assert_distinct_matches(&mut g, d, plain);
    }
}

fn plan_has_op(g: &mut Graph, q: &str, op: &str) -> bool {
    fn walk(v: &CypherValue, op: &str) -> bool {
        let CypherValue::Map(m) = v else { return false };
        if matches!(m.get("operatorType"), Some(CypherValue::String(s)) if s.as_str() == op) {
            return true;
        }
        match m.get("children") {
            Some(CypherValue::List(cs)) => cs.iter().any(|c| walk(c, op)),
            _ => false,
        }
    }
    let r = execute(g, &format!("EXPLAIN {q}")).unwrap();
    walk(r.plan.as_ref().expect("an EXPLAIN plan"), op)
}

/// `(key, target)` rows of `q` on the one-thread pool.
fn pairs(g: &mut Graph, q: &str) -> Vec<(String, String)> {
    execute(g, q)
        .unwrap_or_else(|e| panic!("{q}: {e}"))
        .rows
        .iter()
        .map(|r| (format!("{:?}", r[0]), format!("{:?}", r[1])))
        .collect()
}

/// `{key} AS k, count(DISTINCT t)` over `pattern` against the plain rows, distinct targets
/// counted per key in the test; `fused` says whether the plan must hold a DistinctExpand.
fn assert_grouped_count(g: &mut Graph, pattern: &str, key: &str, fused: bool) {
    let mut want: std::collections::BTreeMap<String, HashSet<String>> = Default::default();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    let plain = format!("{pattern} RETURN {key} AS k, t AS t");
    for (k, t) in pool.install(|| pairs(g, &plain)) {
        want.entry(k).or_default().insert(t);
    }
    let want: std::collections::BTreeMap<String, String> = want
        .into_iter()
        .map(|(k, ts)| (k, format!("{:?}", CypherValue::Integer(ts.len() as i64))))
        .collect();
    let q = format!("{pattern} RETURN {key} AS k, count(DISTINCT t) AS c");
    assert_eq!(plan_has_op(g, &q, "DistinctExpand"), fused, "fused: {q}");
    for (got, label) in [
        (pool.install(|| pairs(g, &q)), "one thread"),
        (pairs(g, &q), "default pool"),
    ] {
        assert_eq!(got.len(), want.len(), "groups ({label}): {q}");
        let got: std::collections::BTreeMap<String, String> = got.into_iter().collect();
        assert_eq!(got, want, "{q} ({label})");
    }
}

#[test]
fn grouped_distinct_counts_are_per_row_trails() {
    let mut g = reuse_graph();
    let mut hops: Vec<String> = Vec::new();
    for hop in ["-[:R]->", "<-[:R]-", "-[:R]-", "--"] {
        for k in [2, 3, 5] {
            hops.push(chain(hop, k));
        }
    }
    for rel in ["-[:R*{r}]->", "<-[:R*{r}]-", "-[:R*{r}]-"] {
        for range in ["0..2", "1..3", "2..2", "2..4", "4..5"] {
            hops.push(rel.replace("{r}", range));
        }
    }
    for h in &hops {
        for src in [
            "MATCH (a:M)",
            "MATCH (a:M) WHERE a.id >= 100 OR a.id % 7 = 0",
        ] {
            for target in ["(t)", "(t:M)"] {
                let pattern = format!("{src} MATCH (a){h}{target}");
                // one source per key, sources sharing a key (the count must stay distinct across
                // them), the node itself, a constant
                for key in ["a.id", "a.id % 3", "a", "1"] {
                    assert_grouped_count(&mut g, &pattern, key, true);
                }
                // a target filter reading the source
                let filtered = format!("{pattern} WHERE t.id <> a.id");
                assert_grouped_count(&mut g, &filtered, "a.id % 4", true);
            }
        }
    }
}

#[test]
fn grouped_distinct_counts_keyed_off_the_chain_stay_plain() {
    let mut g = reuse_graph();
    for (pattern, key) in [
        // an inner node, the chain's relationships
        ("MATCH (a:M)-[:R]->(x)-[:R]->(t)", "x.id"),
        ("MATCH (a:M)-[:R]->(x)-[:R]->()-[:R]->(t)", "x.id % 2"),
        ("MATCH (a:M)-[r:R*2..3]-(t)", "size(r)"),
        ("MATCH (a:M)-[q:R]->()-[:R]->(t)", "id(q) % 3"),
    ] {
        assert_grouped_count(&mut g, pattern, key, false);
    }
}

/// The plain `(a.id, t.id)` rows of `pattern`, one per trail, on the one-thread pool.
fn trail_ends(g: &mut Graph, pattern: &str) -> Vec<(i64, i64)> {
    let q = format!("{pattern} RETURN a.id AS a, t.id AS t");
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    let r = pool
        .install(|| execute(g, &q))
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    r.rows
        .iter()
        .map(|row| match (&row[0], &row[1]) {
            (CypherValue::Integer(a), CypherValue::Integer(t)) => (*a, *t),
            other => panic!("{q}: {other:?}"),
        })
        .collect()
}

/// `q`'s rows on both pools, each as its debug cells, list cells sorted (collect order is
/// unspecified), the rows sorted.
fn canonical_rows(g: &mut Graph, q: &str) -> [Vec<Vec<String>>; 2] {
    fn canon(rows: &[Vec<CypherValue>]) -> Vec<Vec<String>> {
        let mut out: Vec<Vec<String>> = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|c| match c {
                        CypherValue::List(items) => {
                            let mut v: Vec<String> =
                                items.iter().map(|i| format!("{i:?}")).collect();
                            v.sort();
                            v.join(",")
                        }
                        other => format!("{other:?}"),
                    })
                    .collect()
            })
            .collect();
        out.sort();
        out
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    let seq = pool
        .install(|| execute(g, q))
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    let par = execute(g, q).unwrap_or_else(|e| panic!("{q}: {e}"));
    [canon(&seq.rows), canon(&par.rows)]
}

fn int(v: i64) -> String {
    format!("{:?}", CypherValue::Integer(v))
}

#[test]
fn repeat_blind_aggregates_and_distinct_projections_fuse_per_row() {
    use std::collections::{BTreeMap, BTreeSet};
    let mut g = reuse_graph();
    let mut hops: Vec<String> = Vec::new();
    for hop in ["-[:R]->", "-[:R]-", "<-[:R]-"] {
        for k in [2, 3, 4] {
            hops.push(chain(hop, k));
        }
    }
    for rel in ["-[:R*{r}]->", "-[:R*{r}]-"] {
        for range in ["0..2", "2..3", "4..5"] {
            hops.push(rel.replace("{r}", range));
        }
    }
    for h in &hops {
        for src in [
            "MATCH (a:M)",
            "MATCH (a:M) WHERE a.id >= 100 OR a.id % 7 = 0",
        ] {
            let pattern = format!("{src} MATCH (a){h}(t:M)");
            let ends = trail_ends(&mut g, &pattern);
            // keys one per source (1000), shared across sources (3), one for all (1)
            for m in [1000, 3, 1] {
                let mut per: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
                let mut pairs: BTreeSet<(i64, i64)> = BTreeSet::new();
                for &(a, t) in &ends {
                    per.entry(a % m).or_default().insert(t);
                    pairs.insert((a % m, t));
                }
                let sorted = |rows: Vec<Vec<String>>| {
                    let mut rows = rows;
                    rows.sort();
                    rows
                };
                let cases: Vec<(String, Vec<Vec<String>>)> = vec![
                    (
                        format!("RETURN a.id % {m} AS k, count(DISTINCT t.id) AS c"),
                        per.iter()
                            .map(|(k, ts)| vec![int(*k), int(ts.len() as i64)])
                            .collect(),
                    ),
                    (
                        format!("RETURN a.id % {m} AS k, collect(DISTINCT t.id) AS c"),
                        per.iter()
                            .map(|(k, ts)| {
                                let mut v: Vec<String> = ts.iter().map(|t| int(*t)).collect();
                                v.sort();
                                vec![int(*k), v.join(",")]
                            })
                            .collect(),
                    ),
                    (
                        format!("RETURN a.id % {m} AS k, min(t.id) AS lo, max(t.id) AS hi"),
                        per.iter()
                            .map(|(k, ts)| {
                                vec![
                                    int(*k),
                                    int(*ts.iter().next().unwrap()),
                                    int(*ts.iter().last().unwrap()),
                                ]
                            })
                            .collect(),
                    ),
                    (
                        format!("RETURN a.id % {m} AS k, sum(DISTINCT t.id % 5) AS s"),
                        per.iter()
                            .map(|(k, ts)| {
                                let s: BTreeSet<i64> = ts.iter().map(|t| t % 5).collect();
                                vec![int(*k), int(s.iter().sum())]
                            })
                            .collect(),
                    ),
                    (
                        format!("RETURN DISTINCT a.id % {m} AS k, t.id AS t"),
                        pairs.iter().map(|(k, t)| vec![int(*k), int(*t)]).collect(),
                    ),
                    (
                        format!("WITH DISTINCT a.id % {m} AS k, t RETURN k, count(*) AS c"),
                        per.iter()
                            .map(|(k, ts)| vec![int(*k), int(ts.len() as i64)])
                            .collect(),
                    ),
                ];
                for (ret, want) in cases {
                    let q = format!("{pattern} {ret}");
                    let want = sorted(want);
                    assert!(plan_has_op(&mut g, &q, "DistinctExpand"), "fused: {q}");
                    let [seq, par] = canonical_rows(&mut g, &q);
                    assert_eq!(seq, want, "{q} (one thread)");
                    assert_eq!(par, want, "{q} (default pool)");
                }
            }
            // keyed by the target alone: one dedup across every row
            let mut by_parity: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();
            for &(_, t) in &ends {
                by_parity.entry(t % 2).or_default().insert(t);
            }
            let q = format!("{pattern} RETURN t.id % 2 AS k, count(DISTINCT t) AS c");
            let mut want: Vec<Vec<String>> = by_parity
                .iter()
                .map(|(k, ts)| vec![int(*k), int(ts.len() as i64)])
                .collect();
            want.sort();
            assert!(plan_has(&mut g, &q, "multi_source"), "one dedup: {q}");
            let [seq, par] = canonical_rows(&mut g, &q);
            assert_eq!(seq, want, "{q} (one thread)");
            assert_eq!(par, want, "{q} (default pool)");
        }
    }
}

#[test]
fn repeat_sensitive_aggregates_stay_plain() {
    let mut g = reuse_graph();
    for q in [
        "MATCH (a:M)-[:R*2..3]-(t) RETURN a.id AS k, count(t) AS c",
        "MATCH (a:M)-[:R*2..3]-(t) RETURN a.id AS k, count(*) AS c",
        "MATCH (a:M)-[:R*2..3]-(t) RETURN a.id AS k, collect(t.id) AS c",
        "MATCH (a:M)-[:R*2..3]-(t) RETURN a.id AS k, sum(t.id) AS c",
        "MATCH (a:M)-[:R*2..3]-(t) RETURN a.id AS k, count(DISTINCT t) + 1 AS c",
        "MATCH (a:M)-[:R*2..3]-(t) RETURN a.id AS k, count(DISTINCT t), count(t) AS c",
        "MATCH (a:M)-[:R]-()-[:R]-(t) RETURN a.id AS k, apoc.agg.first(t.id) AS c",
    ] {
        assert!(!plan_has_op(&mut g, q, "DistinctExpand"), "plain: {q}");
    }
    // and their values: one per trail
    let ends = trail_ends(&mut g, "MATCH (a:M)-[:R*2..3]-(t:M)");
    let mut per: std::collections::BTreeMap<i64, (i64, i64)> = Default::default();
    for &(a, t) in &ends {
        let e = per.entry(a).or_default();
        e.0 += 1;
        e.1 += t;
    }
    let q = "MATCH (a:M)-[:R*2..3]-(t:M) RETURN a.id AS k, count(*) AS c, sum(t.id) AS s";
    let mut want: Vec<Vec<String>> = per
        .iter()
        .map(|(k, (c, s))| vec![int(*k), int(*c), int(*s)])
        .collect();
    want.sort();
    let [seq, par] = canonical_rows(&mut g, q);
    assert_eq!(seq, want, "{q} (one thread)");
    assert_eq!(par, want, "{q} (default pool)");
}

#[test]
fn distinct_chains_into_bound_nodes() {
    let mut g = reuse_graph();
    for (d, plain, fused) in [
        // both ends bound before the chain
        (
            "MATCH (a:M {id: 0}), (t:M) WHERE t.id IN [1, 3, 5, 100] MATCH (a)-[:R*2..3]-(t) RETURN DISTINCT t.id",
            "MATCH (a:M {id: 0}), (t:M) WHERE t.id IN [1, 3, 5, 100] MATCH (a)-[:R*2..3]-(t) RETURN t.id",
            true,
        ),
        (
            "MATCH (a:M {id: 0}), (t:M) WHERE t.id IN [1, 3, 5] MATCH (a)-[:R]-()-[:R]-()-[:R]-(t) RETURN DISTINCT t.id",
            "MATCH (a:M {id: 0}), (t:M) WHERE t.id IN [1, 3, 5] MATCH (a)-[:R]-()-[:R]-()-[:R]-(t) RETURN t.id",
            true,
        ),
        (
            "MATCH (a:M), (t:M {id: 3}) MATCH (a)-[:R]->()-[:R]->(t) RETURN DISTINCT a.id % 2, t.id",
            "MATCH (a:M), (t:M {id: 3}) MATCH (a)-[:R]->()-[:R]->(t) RETURN a.id % 2, t.id",
            true,
        ),
        // a cycle back to the source
        (
            "MATCH (a:M)-[:R*2..3]-(a) RETURN DISTINCT a.id",
            "MATCH (a:M)-[:R*2..3]-(a) RETURN a.id",
            true,
        ),
        (
            "MATCH (a:M)-[:R]->()-[:R]->()-[:R]->(a) RETURN DISTINCT a.id",
            "MATCH (a:M)-[:R]->()-[:R]->()-[:R]->(a) RETURN a.id",
            true,
        ),
        // an inner node bound before the chain: never walked through unbound
        (
            "MATCH (x:M) WHERE x.id IN [1, 2, 101] MATCH (a:M)-[:R]-(x)-[:R]-()-[:R]-(t:M) RETURN DISTINCT t.id",
            "MATCH (x:M) WHERE x.id IN [1, 2, 101] MATCH (a:M)-[:R]-(x)-[:R]-()-[:R]-(t:M) RETURN t.id",
            false,
        ),
    ] {
        assert_distinct_matches(&mut g, d, plain);
        // a bound end (or the source again) fuses to a fresh end kept to it by the filter
        if fused {
            assert!(plan_has_op(&mut g, d, "DistinctExpand"), "fused: {d}");
        }
    }
    let q = "MATCH (a:M {id: 0}), (t:M {id: 3}) MATCH (a)-[:R]-()-[:R]-()-[:R]-(t) RETURN count(DISTINCT t) AS c";
    let want = as_set(
        &both(
            &mut g,
            "MATCH (a:M {id: 0}), (t:M {id: 3}) MATCH (a)-[:R]-()-[:R]-()-[:R]-(t) RETURN t.id",
        )
        .0,
    )
    .len();
    let (seq, par) = both(&mut g, q);
    let want = vec![format!("{:?}", vec![CypherValue::Integer(want as i64)])];
    assert_eq!(seq, want, "{q} (one thread)");
    assert_eq!(par, want, "{q} (default pool)");
}
