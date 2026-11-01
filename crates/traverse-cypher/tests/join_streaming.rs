//! Joins must stream: memory bounded by one side, never by the output. Small graphs only; what
//! matters is plan shape and correctness on shapes that would otherwise materialize a full
//! cartesian product, notably under a write.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run(graph: &mut Graph, q: &str) -> Result<Vec<Vec<CypherValue>>, String> {
    execute_with_params(graph, q, &HashMap::new())
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

fn ok(graph: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    run(graph, q).unwrap_or_else(|e| panic!("query failed: {q}\n{e}"))
}

fn int(v: &CypherValue) -> i64 {
    match v {
        CypherValue::Integer(i) => *i,
        other => panic!("expected integer, got {other:?}"),
    }
}

/// Operator types in the EXPLAIN tree, pre-order.
fn plan_operators(graph: &mut Graph, query: &str) -> Vec<String> {
    let res =
        execute_with_params(graph, &format!("EXPLAIN {query}"), &HashMap::new()).expect("explain");
    fn walk(v: &CypherValue, out: &mut Vec<String>) {
        if let CypherValue::Map(m) = v {
            if let Some(CypherValue::String(t)) = m.get("operatorType") {
                out.push(t.clone());
            }
            if let Some(CypherValue::List(children)) = m.get("children") {
                for c in children {
                    walk(c, out);
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(res.plan.as_ref().expect("plan"), &mut out);
    out
}

fn people(n: i64) -> Graph {
    let mut g = Graph::new();
    ok(
        &mut g,
        &format!("UNWIND range(1, {n}) AS i CREATE (:P {{id: i, g: i % 10}})"),
    );
    g
}

// -- write shape: a join feeding CREATE --------------------------------

const WRITE_JOIN: &str =
    "MATCH (a:P) WHERE a.id % 3 = 0 MATCH (b:P {id: a.id + 1}) CREATE (a)-[:R]->(b)";

#[test]
fn write_plan_hash_joins_an_unindexed_computed_equality() {
    let mut g = people(10);
    let ops = plan_operators(&mut g, WRITE_JOIN);
    assert!(
        ops.iter().any(|o| o == "HashJoin"),
        "expected a HashJoin in the write plan, got {ops:?}"
    );
    assert!(
        !ops.iter().any(|o| o == "CartesianProduct"),
        "write plan must not keep a CartesianProduct: {ops:?}"
    );
}

#[test]
fn write_join_creates_exactly_the_matching_edges() {
    // a.id in {3, 6, ..., 2997} with a.id + 1 <= 3000 -> 999 edges
    let mut g = people(3_000);
    ok(&mut g, WRITE_JOIN);
    let rows = ok(
        &mut g,
        "MATCH (a:P)-[:R]->(b:P) RETURN count(*), min(a.id), max(b.id)",
    );
    assert_eq!(int(&rows[0][0]), 999);
    assert_eq!(int(&rows[0][1]), 3);
    assert_eq!(int(&rows[0][2]), 2998);
    let rows = ok(
        &mut g,
        "MATCH (a:P)-[:R]->(b:P) WHERE b.id <> a.id + 1 RETURN count(*)",
    );
    assert_eq!(int(&rows[0][0]), 0, "every edge must join id -> id + 1");
}

// -- read joins ---------------------------------------------------------

#[test]
fn read_hash_join_on_computed_key() {
    let mut g = people(2_000);
    let rows = ok(
        &mut g,
        "MATCH (a:P) MATCH (b:P) WHERE b.id = a.id + 1 RETURN count(*)",
    );
    assert_eq!(int(&rows[0][0]), 1_999);
    let rows = ok(
        &mut g,
        "MATCH (a:P) MATCH (b:P) WHERE b.id = a.id + 1 AND a.g = 0 RETURN a.id, b.id ORDER BY a.id LIMIT 3",
    );
    assert_eq!(rows.len(), 3);
    assert_eq!((int(&rows[0][0]), int(&rows[0][1])), (10, 11));
}

#[test]
fn hash_join_with_unmatched_probe_rows_and_duplicates() {
    let mut g = Graph::new();
    ok(&mut g, "UNWIND [1, 1, 2, 3] AS k CREATE (:L {k: k})");
    ok(&mut g, "UNWIND [1, 3, 3, 4] AS k CREATE (:R {k: k})");
    // 1 matches 2x1, 3 matches 1x2, 2 and 4 match nothing -> 4 rows
    let rows = ok(
        &mut g,
        "MATCH (l:L) MATCH (r:R) WHERE r.k = l.k RETURN l.k, r.k ORDER BY l.k",
    );
    let pairs: Vec<(i64, i64)> = rows.iter().map(|r| (int(&r[0]), int(&r[1]))).collect();
    assert_eq!(pairs, vec![(1, 1), (1, 1), (3, 3), (3, 3)]);
}

#[test]
fn cross_product_streams_every_pair_and_honours_limit() {
    let mut g = Graph::new();
    ok(&mut g, "UNWIND range(1, 700) AS i CREATE (:A {x: i})");
    ok(&mut g, "UNWIND range(1, 700) AS i CREATE (:B {y: i})");
    // a true cartesian product (no equi-key), filtered above it
    let rows = ok(
        &mut g,
        "MATCH (a:A) MATCH (b:B) WHERE a.x < b.y RETURN count(*)",
    );
    assert_eq!(int(&rows[0][0]), 700 * 699 / 2);
    // streaming: LIMIT stops after five rows, not 490 000
    let rows = ok(&mut g, "MATCH (a:A) MATCH (b:B) RETURN a.x, b.y LIMIT 5");
    assert_eq!(rows.len(), 5);
}

#[test]
fn cross_product_with_an_empty_side_is_empty() {
    let mut g = people(50);
    let rows = ok(&mut g, "MATCH (a:P) MATCH (b:Nope) RETURN count(*)");
    assert_eq!(int(&rows[0][0]), 0);
    let rows = ok(&mut g, "MATCH (a:Nope) MATCH (b:P) RETURN count(*)");
    assert_eq!(int(&rows[0][0]), 0);
}

#[test]
fn lopsided_cross_product_keeps_every_pair_in_both_orientations() {
    let mut g = Graph::new();
    ok(&mut g, "UNWIND range(1, 3) AS i CREATE (:S {i: i})");
    ok(&mut g, "UNWIND range(1, 2000) AS j CREATE (:B {j: j})");
    // small x big and big x small: every pair exactly once, both ways
    let rows = ok(
        &mut g,
        "MATCH (s:S) MATCH (b:B) RETURN count(*), sum(s.i * b.j)",
    );
    assert_eq!(int(&rows[0][0]), 6_000);
    assert_eq!(int(&rows[0][1]), (1 + 2 + 3) * (2000 * 2001 / 2));
    let rows = ok(&mut g, "MATCH (b:B) MATCH (s:S) RETURN count(*)");
    assert_eq!(int(&rows[0][0]), 6_000);
}

#[test]
fn optional_match_streams_with_and_without_matches() {
    let mut g = Graph::new();
    ok(&mut g, "UNWIND range(1, 500) AS i CREATE (:N {i: i})");
    ok(
        &mut g,
        "MATCH (n:N) WHERE n.i % 2 = 0 CREATE (n)-[:E]->(:M {i: n.i})",
    );
    ok(
        &mut g,
        "MATCH (n:N) WHERE n.i % 4 = 0 CREATE (n)-[:E]->(:M {i: n.i + 1000})",
    );
    // odd: no match (row kept with null); ==2 mod 4: one match; ==0 mod 4: two
    let rows = ok(
        &mut g,
        "MATCH (n:N) OPTIONAL MATCH (n)-[:E]->(m:M) RETURN count(*), count(m), count(DISTINCT n)",
    );
    assert_eq!(int(&rows[0][0]), 250 + 125 + 125 * 2);
    assert_eq!(int(&rows[0][1]), 125 + 125 * 2);
    assert_eq!(int(&rows[0][2]), 500);
    let rows = ok(
        &mut g,
        "MATCH (n:N) OPTIONAL MATCH (n)-[:E]->(m:M) RETURN n.i, m.i ORDER BY n.i, m.i LIMIT 3",
    );
    assert_eq!(int(&rows[0][0]), 1);
    assert_eq!(rows[0][1], CypherValue::Null);
    assert_eq!((int(&rows[1][0]), int(&rows[1][1])), (2, 2));
    assert_eq!(int(&rows[2][0]), 3);
    assert_eq!(rows[2][1], CypherValue::Null);
}
