//! Relationship uniqueness: one MATCH binds a relationship at most once, across its
//! comma-separated parts, the two walks the planner splits a pattern into at a bound middle
//! node, and relationships an earlier clause bound that the pattern names again. Separate
//! clauses may bind the same relationship, and so may a `REPEATABLE ELEMENTS` clause; the
//! rewrites that rebuild or fuse an expand chain (direction reversal, the distinct expansion)
//! must keep exactly the pairs the plan kept apart, no more.
//!
//! Every expected value is Neo4j 2026.01's answer on the same graph. Each query runs on a
//! one-thread pool (sequential plans) and on the default pool (the morsel scheduler).

use traverse_core::graph::Graph;
use traverse_cypher::execute;
use traverse_cypher::types::CypherValue;

/// Paths 0..29 both ways, self-loops on multiples of 7, skips and a parallel edge; a small
/// 100..109 component with a 2-cycle, a self-loop and a triangle.
fn graph() -> Graph {
    let mut g = Graph::new();
    for q in [
        "UNWIND range(0, 29) + range(100, 109) AS i CREATE (:Reuse {id: i})",
        "MATCH (a:Reuse), (b:Reuse) WHERE a.id < 29 AND b.id = a.id + 1 \
         CREATE (a)-[:R]->(b), (b)-[:R]->(a)",
        "MATCH (a:Reuse) WHERE a.id < 30 AND a.id % 7 = 0 CREATE (a)-[:R]->(a)",
        "MATCH (a:Reuse), (b:Reuse) WHERE a.id < 27 AND a.id % 5 = 0 AND b.id = a.id + 3 \
         CREATE (a)-[:R]->(b)",
        "MATCH (a:Reuse {id: 4}), (b:Reuse {id: 5}) CREATE (a)-[:R]->(b)",
        "MATCH (a:Reuse), (b:Reuse) WHERE a.id < 30 AND a.id % 4 = 0 AND b.id = a.id + 2 \
         CREATE (a)-[:S]->(b)",
        "UNWIND [[100,101],[101,100],[102,102],[102,103],[104,105],[106,107],[107,108],\
         [108,106],[108,109],[109,108]] AS e \
         MATCH (a:Reuse {id: e[0]}), (b:Reuse {id: e[1]}) CREATE (a)-[:R]->(b)",
    ] {
        execute(&mut g, q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    g
}

/// The single value of a one-row, one-column result: an integer, or a list of integers
/// (sorted).
fn value(g: &mut Graph, q: &str) -> String {
    let r = execute(g, q).unwrap_or_else(|e| panic!("{q}: {e}"));
    assert_eq!(r.rows.len(), 1, "{q}: {:?}", r.rows);
    match &r.rows[0][0] {
        CypherValue::Integer(i) => i.to_string(),
        CypherValue::List(items) => {
            let mut ints: Vec<i64> = items
                .iter()
                .map(|v| match v {
                    CypherValue::Integer(i) => *i,
                    other => panic!("{q}: {other:?}"),
                })
                .collect();
            ints.sort_unstable();
            format!("{ints:?}")
        }
        other => panic!("{q}: {other:?}"),
    }
}

fn check(cases: &[(&str, &str)]) {
    let mut g = graph();
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(1)
        .build()
        .unwrap();
    let mut wrong = Vec::new();
    for (q, want) in cases {
        let seq = pool.install(|| value(&mut g, q));
        let par = value(&mut g, q);
        if seq != *want || par != *want {
            wrong.push(format!(
                "{q}\n  want {want}, sequential {seq}, parallel {par}"
            ));
        }
    }
    assert!(wrong.is_empty(), "\n{}", wrong.join("\n"));
}

#[test]
fn comma_parts_never_share_a_relationship() {
    check(&[
        // disconnected parts, cross-joined
        (
            "MATCH (a:Reuse {id: 100})-[r1:R]->(b), (c:Reuse {id: 100})-[r2:R]->(d) \
             RETURN count(*)",
            "0",
        ),
        (
            "MATCH (a:Reuse)-[r1:R]->(b), (c:Reuse)-[r2:R]->(d) \
             WHERE a.id >= 100 AND c.id >= 100 RETURN count(*)",
            "90",
        ),
        (
            "MATCH (a:Reuse {id: 106})-[rs:R*2]->(b), (c:Reuse)-[r2:R]->(d) \
             WHERE c.id >= 106 RETURN count(*)",
            "3",
        ),
        (
            "MATCH (a:Reuse {id: 106})-[r1:R*1..2]->(b), (c:Reuse {id: 107})-[r2:R*1..2]->(d) \
             RETURN count(*)",
            "3",
        ),
        (
            "MATCH (a:Reuse {id: 4})-[r:R*1..3]-(b), (c:Reuse {id: 5})-[s:R*1..3]-(d) \
             RETURN count(*)",
            "6447",
        ),
        // an undirected self-loop is one relationship
        (
            "MATCH (a:Reuse {id: 102})-[r1:R]-(b), (c:Reuse {id: 102})-[r2:R]-(d) \
             RETURN count(*)",
            "2",
        ),
        (
            "MATCH p = (a:Reuse {id: 100})-[:R]->(b), q = (c:Reuse {id: 100})-[:R]->(d) \
             RETURN count(*)",
            "0",
        ),
        (
            "MATCH (a:Reuse)-[r1:R]->(b), (c)-[r2:R]->(d), (e)-[r3:R]->(f) \
             WHERE a.id = 106 AND c.id = 107 AND e.id = 108 RETURN count(*)",
            "2",
        ),
        // parts chained on a shared node
        (
            "MATCH (a:Reuse {id: 101})-[r1:R]-(b), (a)-[r2:R]-(c) RETURN count(*)",
            "2",
        ),
        (
            "MATCH (a:Reuse)-[r1:R]->(b), (b)-[r2:R]->(c) WHERE a.id >= 100 RETURN count(*)",
            "10",
        ),
        (
            "MATCH (a:Reuse)-[r1:R]-(b), (b)-[r2:R]-(c) WHERE a.id >= 100 RETURN count(*)",
            "24",
        ),
        (
            "MATCH (a:Reuse)-[r1:R]-(b), (b)-[r2:R]-(c), (c)-[r3:R]-(a) \
             WHERE a.id >= 100 RETURN count(*)",
            "6",
        ),
        (
            "MATCH (a:Reuse)-[r1:R]->(b)-[r2:R]->(c), (c)-[r3:R]->(d) \
             WHERE a.id = 106 RETURN count(*)",
            "2",
        ),
        (
            "MATCH (a:Reuse)-[r:R]->(b), (a)-[s:R]->(b) WHERE a.id = 7 RETURN count(*)",
            "0",
        ),
        (
            "MATCH (a:Reuse)-[r:R]-(b), (a)-[s:R]-(b) WHERE a.id = 7 RETURN count(*)",
            "4",
        ),
        (
            "MATCH (a:Reuse)-[r:R]-(b), (b)-[s:R]-(a) WHERE a.id < 10 RETURN count(*)",
            "46",
        ),
        (
            "MATCH (a:Reuse)-[r:R*1..3]-(b), (b)-[s:R]-(a) WHERE a.id = 4 RETURN count(*)",
            "48",
        ),
        (
            "MATCH p = (a:Reuse {id: 106})-[:R*1..3]->(b), q = (b)-[:R*1..3]->(c) \
             RETURN count(*)",
            "10",
        ),
        (
            "MATCH (a:Reuse {id: 5})-[:R]-(m), (m)-[:R*1..2]-(c) \
             RETURN collect(DISTINCT c.id)",
            "[0, 2, 3, 4, 5, 6, 7, 8, 9, 10]",
        ),
        (
            "MATCH (a:Reuse {id: 3})-[r:R]-(b), (c:Reuse {id: 3})-[s:R]-(d) \
             WHERE id(b) <= id(d) RETURN count(*)",
            "12",
        ),
    ]);
}

#[test]
fn parts_from_bound_nodes_never_share_a_relationship() {
    check(&[
        (
            "MATCH (x:Reuse {id: 101}) MATCH (x)-[r1:R]-(a), (x)-[r2:R]-(y) RETURN count(*)",
            "2",
        ),
        // two parts from different bound nodes; the count runs per source lane, the first
        // part's relationship sits in another group
        (
            "MATCH (x:Reuse {id: 101}) MATCH (y:Reuse {id: 100}) \
             MATCH (x)-[r1:R]-(a), (y)-[r2:R]-(b) RETURN count(*)",
            "2",
        ),
        (
            "MATCH (a:Reuse {id: 4}), (c:Reuse {id: 5}) \
             MATCH (a)-[r:R*1..2]-(b), (c)-[s:R*1..2]-(d) RETURN count(*)",
            "547",
        ),
        (
            "MATCH (x:Reuse {id: 106}) MATCH (a)-[r1:R]->(x), (x)-[r2:R*1..3]->(b) \
             RETURN count(*)",
            "3",
        ),
        (
            "MATCH (x:Reuse {id: 106}) MATCH (a)-[r1:R]-(x), (x)-[r2:R*1..3]-(b) \
             RETURN count(*)",
            "10",
        ),
        (
            "MATCH (a:Reuse {id: 106})-[r1:R]->(b) MATCH (a)-[r2:R]->(b), (b)<-[r3:R]-(a) \
             RETURN count(*)",
            "0",
        ),
    ]);
}

#[test]
fn a_pattern_split_at_a_bound_node_keeps_both_walks_apart() {
    check(&[
        (
            "MATCH (x:Reuse) WHERE x.id = 101 \
             MATCH (a:Reuse)-[r1:R]-(x)-[r2:R]-(y)-[r3:R]-(b) RETURN count(*)",
            "0",
        ),
        (
            "MATCH (x:Reuse {id: 1}) MATCH (a)-[r1:R]-(x)-[r2:R]-(b) RETURN count(*)",
            "12",
        ),
        (
            "MATCH (x:Reuse {id: 107}) MATCH (a)-[r1:R*1..2]-(x)-[r2:R*1..2]-(b) \
             RETURN count(*)",
            "14",
        ),
        (
            "MATCH (x:Reuse {id: 3}) MATCH (a:Reuse)-[:R*1..2]-(x)-[:R*1..2]-(b:Reuse) \
             RETURN count(*)",
            "344",
        ),
        // the right walk would fuse into a distinct expansion if it didn't keep off the left's
        (
            "MATCH (x:Reuse) WHERE x.id = 1 OR x.id = 2 OR x.id = 101 \
             MATCH (a:Reuse)-[:R]-(x)-[:R]-()-[:R]-(b) RETURN collect(DISTINCT b.id)",
            "[0, 1, 2, 3, 4]",
        ),
    ]);
}

#[test]
fn relationships_bound_by_earlier_clauses_count_in_the_pattern() {
    check(&[
        (
            "MATCH (:Reuse {id: 100})-[r:R]->() WITH r \
             MATCH (a)-[r]->(b), (c:Reuse)-[r2:R]->(d) WHERE c.id >= 100 RETURN count(*)",
            "9",
        ),
        (
            "MATCH (a:Reuse)-[r:R]->(b) WHERE a.id = 7 MATCH (c)-[r]->(d), (a)-[s:R]->(e) \
             RETURN count(*)",
            "6",
        ),
        // a bound relationship inside a part chained from a bound node must be that one
        (
            "MATCH (:Reuse {id: 108})-[r:R]->(:Reuse {id: 109}) WITH r \
             MATCH (x:Reuse {id: 107}) MATCH (x)-[s:R]->(y)-[r]->(z) RETURN count(*)",
            "1",
        ),
        (
            "MATCH (:Reuse {id: 108})-[r:R]->(:Reuse {id: 109}) WITH r \
             MATCH (x:Reuse {id: 108}) MATCH (x)-[s:R]-(y), (y)-[r]-(z) RETURN count(*)",
            "1",
        ),
    ]);
}

#[test]
fn optional_and_subquery_patterns_keep_their_parts_apart() {
    check(&[
        (
            "MATCH (x:Reuse {id: 101}) OPTIONAL MATCH (x)-[r1:R]->(a), (x)-[r2:R]->(b) \
             RETURN count(r2)",
            "0",
        ),
        (
            "MATCH (a:Reuse)-[r:R]->(b) WHERE a.id = 102 \
             OPTIONAL MATCH (a)-[s:R]->(c), (a)-[t:R]->(d) RETURN count(s)",
            "4",
        ),
        // a bound relationship or node an OPTIONAL MATCH names again must be that one
        (
            "MATCH (:Reuse {id: 108})-[r:R]->(:Reuse {id: 109}) WITH r \
             OPTIONAL MATCH (x:Reuse {id: 107})-[s:R]->(y)-[r]->(z) RETURN count(z)",
            "1",
        ),
        (
            "MATCH (a:Reuse {id: 1}) OPTIONAL MATCH (a) RETURN count(*)",
            "1",
        ),
        (
            "MATCH (x:Reuse) WHERE x.id >= 100 \
             AND EXISTS { MATCH (x)-[r1:R]->(), (x)-[r2:R]->() } RETURN collect(x.id)",
            "[102, 108]",
        ),
        (
            "MATCH (x:Reuse) WHERE x.id >= 100 \
             RETURN sum(COUNT { MATCH (x)-[r1:R]-(), (x)-[r2:R]-() })",
            "24",
        ),
    ]);
}

/// A pattern of one bound node is planned from its binding: no scan, only the checks that it is a
/// node, has the labels and properties written, and (under OPTIONAL MATCH) is the same row.
#[test]
fn a_lone_bound_node_checks_its_binding() {
    check(&[
        ("MATCH (a:Reuse) WITH a MATCH (a) RETURN count(*)", "40"),
        (
            "MATCH (a:Reuse) WITH a MATCH (a), (a) RETURN count(*)",
            "40",
        ),
        ("MATCH (a:Reuse {id: 1}) MATCH (a:S) RETURN count(*)", "0"),
        (
            "MATCH (a:Reuse {id: 1}) MATCH (a:Reuse {id: 1}) RETURN count(*)",
            "1",
        ),
        (
            "MATCH (a:Reuse {id: 1}) MATCH (a {id: 2}) RETURN count(*)",
            "0",
        ),
        (
            "MATCH (a:Reuse {id: 1}) OPTIONAL MATCH (a:Missing) RETURN count(*)",
            "1",
        ),
        (
            "OPTIONAL MATCH (a:Reuse {id: 1000}) WITH a MATCH (a) RETURN count(*)",
            "0",
        ),
        (
            "OPTIONAL MATCH (a:Reuse {id: 1000}) WITH a OPTIONAL MATCH (a) RETURN count(*)",
            "1",
        ),
        (
            "MATCH (a:Reuse) WHERE a.id < 3 WITH a MATCH p = (a) RETURN count(p)",
            "3",
        ),
        (
            "MATCH (a:Reuse)-[r:R]->(b) WHERE a.id = 7 WITH a, b MATCH (a), (b) RETURN count(*)",
            "3",
        ),
    ]);
}

#[test]
fn separate_clauses_may_share_a_relationship() {
    check(&[
        (
            "MATCH (a:Reuse)-[r1:R]->(b) MATCH (c)-[r2:R]->(d) \
             WHERE a.id = 100 AND c.id = 100 RETURN count(*)",
            "1",
        ),
        (
            "MATCH (a:Reuse)-[r1:R]->(b) MATCH (b)-[r2:R]->(c) RETURN count(*)",
            "178",
        ),
        // the selective target reverses the chain: it must not keep the clauses apart
        (
            "MATCH (a:Reuse)-[r1:R]->(b) MATCH (b)-[r2:R]->(c) WHERE c.id = 102 \
             RETURN count(*)",
            "1",
        ),
        // nor may the distinct expansion, which walks a trail
        (
            "MATCH (a:Reuse {id: 102})-[:R]->(b) MATCH (b)-[:R]->(c) \
             RETURN collect(DISTINCT c.id)",
            "[102, 103]",
        ),
        (
            "MATCH (a:Reuse) WHERE a.id = 100 OR a.id = 102 \
             MATCH (a)-[:R]->(b) MATCH (b)-[:R]->(c) RETURN count(DISTINCT c)",
            "3",
        ),
        (
            "MATCH (a:Reuse) WHERE a.id < 10 MATCH (a)-[:R]-(b) MATCH (b)-[:R]-(c) \
             RETURN count(DISTINCT c)",
            "13",
        ),
        (
            "MATCH (a:Reuse)-[:R]->(b) MATCH (b)-[:R]->(c) \
             WITH c, count(*) AS n RETURN sum(n * c.id)",
            "3591",
        ),
        (
            "MATCH (a:Reuse)-[:R]-(b) MATCH (c)-[:R]-(b) \
             WITH a, count(*) AS n RETURN sum(n * a.id)",
            "13440",
        ),
        // the same within one clause keeps them apart
        (
            "MATCH (a:Reuse)-[:R]-(b), (c)-[:R]-(b) WITH a, count(*) AS n RETURN sum(n * a.id)",
            "9517",
        ),
        (
            "MATCH (a:Reuse)-[:R]->(b), (b)-[:R]->(c) WITH a, count(*) AS n \
             RETURN sum(n * a.id)",
            "3337",
        ),
    ]);
}

#[test]
fn repeatable_elements_may_reuse_relationships() {
    check(&[
        (
            "MATCH REPEATABLE ELEMENTS (a:Reuse {id: 100})-[r1:R]->(b)<-[r2:R]-(a) \
             RETURN count(*)",
            "1",
        ),
        (
            "MATCH REPEATABLE ELEMENTS (a:Reuse {id: 100})-[r1:R]->(b), \
             (c:Reuse {id: 100})-[r2:R]->(d) RETURN count(*)",
            "1",
        ),
        (
            "MATCH REPEATABLE ELEMENTS (x:Reuse {id: 101})-[r1:R]-(a), (x)-[r2:R]-(y) \
             RETURN count(*)",
            "4",
        ),
        // split at the bound node, both walks stay repeatable
        (
            "MATCH (x:Reuse) WHERE x.id = 101 \
             MATCH REPEATABLE ELEMENTS (a:Reuse)-[r1:R]-(x)-[r2:R]-(y)-[r3:R]-(b) \
             RETURN count(*)",
            "8",
        ),
        (
            "MATCH (x:Reuse) WHERE x.id = 101 \
             MATCH REPEATABLE ELEMENTS (a:Reuse)-[r1:R]-(m)-[r2:R]-(x)-[r3:R]-(b) \
             RETURN count(*)",
            "8",
        ),
        (
            "MATCH REPEATABLE ELEMENTS (a:Reuse)-[:R]->(b)-[:R]->(c) WHERE c.id = 102 \
             RETURN count(*)",
            "1",
        ),
        (
            "MATCH REPEATABLE ELEMENTS (a:Reuse {id: 102})-[:R]->(b)-[:R]->(c) \
             RETURN collect(DISTINCT c.id)",
            "[102, 103]",
        ),
        (
            "MATCH REPEATABLE ELEMENTS (a:Reuse {id: 102})-[:R*2]->(c) \
             RETURN collect(DISTINCT c.id)",
            "[102, 103]",
        ),
        (
            "MATCH REPEATABLE ELEMENTS (a:Reuse {id: 100})-[:R*1..3]-(c) \
             RETURN collect(DISTINCT c.id)",
            "[100, 101]",
        ),
    ]);
}

/// A filter over a cartesian product that no join covers (a quantifier's own variable reads as
/// one more input) stays on top instead of disappearing.
#[test]
fn quantified_filters_over_cartesian_products_are_kept() {
    check(&[
        (
            "MATCH (a:Reuse {id: 106})-[r1:R*1..2]->(b) MATCH (c:Reuse {id: 107})-[r2:R*1..2]->(d) \
             WHERE none(x IN r1 WHERE x IN r2) RETURN count(*)",
            "3",
        ),
        (
            "MATCH (a:Reuse {id: 106}), (c:Reuse {id: 107}), (e:Reuse {id: 108}) \
             MATCH (a)-[r1:R*1..2]->(b) MATCH (c)-[r2:R*1..2]->(d) \
             WHERE any(x IN r1 WHERE x IN r2) RETURN count(*)",
            "3",
        ),
    ]);
}

/// The check keeping two cross-joined parts' relationships apart reads both sides, so it stays
/// above the join; the clause's WHERE sinks past it to each side's id seek instead of filtering
/// the whole product.
#[test]
fn where_filters_sink_past_the_uniqueness_check() {
    fn operators(v: &CypherValue, out: &mut Vec<String>) {
        let CypherValue::Map(m) = v else { return };
        if let Some(CypherValue::String(op)) = m.get("operatorType") {
            out.push(op.to_string());
        }
        if let Some(CypherValue::List(cs)) = m.get("children") {
            for c in cs.iter() {
                operators(c, out);
            }
        }
    }
    let mut g = graph();
    let q = "MATCH (a:Reuse)-[r1:R]->(b), (c:Reuse)-[r2:R]->(d) \
             WHERE id(a) = 3 AND id(c) = 4 RETURN count(*)";
    let r = execute(&mut g, &format!("EXPLAIN {q}")).unwrap();
    let mut ops = Vec::new();
    operators(r.plan.as_ref().expect("an EXPLAIN plan"), &mut ops);
    let seeks = ops.iter().filter(|o| o.starts_with("NodeByIdSeek")).count();
    assert_eq!(seeks, 2, "{ops:?}");
    assert!(!ops.iter().any(|o| o.contains("Scan")), "{ops:?}");
    // two nodes' outgoing relationships are never the same one: the plain product, as with the
    // parts in separate clauses
    assert_eq!(
        value(&mut g, q),
        value(&mut g, &q.replace(", (c:", " MATCH (c:"))
    );
}
