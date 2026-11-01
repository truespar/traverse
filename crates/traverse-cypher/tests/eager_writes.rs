//! Streaming writes and the eagerness analysis. CREATE / SET / REMOVE / MERGE apply one row at a
//! time; the optimizer adds an `Eager` barrier only where a read could otherwise see a write out of
//! clause order. Tests pin both halves: import shapes stream (no `Eager`), conflicting shapes get a
//! barrier, and every result matches clause-at-a-time semantics.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn ok(graph: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    execute_with_params(graph, q, &HashMap::new())
        .unwrap_or_else(|e| panic!("query failed: {q}\n{e}"))
        .rows
}

fn int(graph: &mut Graph, q: &str) -> i64 {
    match &ok(graph, q)[0][0] {
        CypherValue::Integer(i) => *i,
        other => panic!("expected integer, got {other:?}"),
    }
}

/// Operator types in the EXPLAIN tree, pre-order.
fn plan_operators(graph: &mut Graph, query: &str) -> Vec<String> {
    let res = execute_with_params(graph, &format!("EXPLAIN {query}"), &HashMap::new())
        .unwrap_or_else(|e| panic!("explain failed: {query}\n{e}"));
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

fn has_eager(graph: &mut Graph, query: &str) -> bool {
    plan_operators(graph, query).iter().any(|o| o == "Eager")
}

fn people(n: i64) -> Graph {
    let mut g = Graph::new();
    ok(
        &mut g,
        &format!("UNWIND range(1, {n}) AS i CREATE (:P {{id: i, g: i % 10}})"),
    );
    g
}

// -- import shapes stream: no barrier ----------------------------------

#[test]
fn import_shapes_have_no_eager() {
    let mut g = people(20);
    for q in [
        "UNWIND range(1, 100) AS i CREATE (:Q {id: i})",
        "UNWIND $rows AS row CREATE (:Q {id: row.id, name: row.name})",
        "UNWIND $rows AS row MERGE (:Q {id: row.id})",
        "MATCH (a:P) MATCH (b:P) WHERE b.id = a.id + 1 CREATE (a)-[:R]->(b)",
        "MATCH (a:P {id: 1}) MATCH (b:P {id: 2}) MERGE (a)-[:R]->(b)",
        "MATCH (n:P) SET n.visited = true",
        "MATCH (n:P) WHERE n.g = 3 REMOVE n.flag",
        "MATCH (n:P) SET n:Seen",
        // a read on label P can't see a node created with label Q, even though both touch `id`
        "MATCH (a:P {id: 1}) CREATE (b:Q {id: a.id + 1})",
        "MATCH (a:P) CREATE (a)-[:R {w: a.id}]->(:Q)",
        // two MERGEs on different labels, the usual node-pair import
        "UNWIND $rows AS row MERGE (a:Q {id: row.id}) MERGE (b:S {id: row.name})",
        // MERGE between two bound nodes only creates the relationship, so even unlabeled scans
        // below are unaffected
        "MATCH (a) MATCH (b) WHERE a.id = 1 AND b.id = 2 MERGE (a)-[:R]->(b)",
    ] {
        assert!(
            !has_eager(&mut g, q),
            "unexpected Eager in plan for: {q}\n{:?}",
            plan_operators(&mut g, q)
        );
    }
}

// -- conflicting shapes get a barrier, and the results prove it --------

#[test]
fn create_same_label_as_scan_is_eager_and_doubles_exactly() {
    let mut g = people(50);
    let q = "MATCH (n:P) CREATE (:P)";
    assert!(has_eager(&mut g, q), "{:?}", plan_operators(&mut g, q));
    ok(&mut g, q);
    assert_eq!(int(&mut g, "MATCH (n:P) RETURN count(n)"), 100);
}

#[test]
fn create_under_unlabeled_scan_is_eager() {
    let mut g = people(30);
    let q = "MATCH (n) CREATE (:Q)";
    assert!(has_eager(&mut g, q));
    ok(&mut g, q);
    assert_eq!(int(&mut g, "MATCH (n) RETURN count(n)"), 60);
}

#[test]
fn create_same_rel_type_as_expand_is_eager_and_doubles_exactly() {
    let mut g = people(30);
    ok(
        &mut g,
        "MATCH (a:P) MATCH (b:P) WHERE b.id = a.id + 1 CREATE (a)-[:R]->(b)",
    );
    assert_eq!(int(&mut g, "MATCH ()-[r:R]->() RETURN count(r)"), 29);
    let q = "MATCH (a:P)-[:R]->(b) CREATE (a)-[:R]->(b)";
    assert!(has_eager(&mut g, q));
    ok(&mut g, q);
    assert_eq!(int(&mut g, "MATCH ()-[r:R]->() RETURN count(r)"), 58);
    // a different type streams
    assert!(!has_eager(
        &mut g,
        "MATCH (a:P)-[:R]->(b) CREATE (a)-[:S]->(b)"
    ));
}

#[test]
fn set_of_a_property_read_below_is_eager() {
    let mut g = people(20);
    let q = "MATCH (n:P) WHERE n.g = 1 SET n.g = 2";
    assert!(has_eager(&mut g, q));
    ok(&mut g, q);
    assert_eq!(int(&mut g, "MATCH (n:P) WHERE n.g = 2 RETURN count(n)"), 4);
    assert_eq!(int(&mut g, "MATCH (n:P) WHERE n.g = 1 RETURN count(n)"), 0);
    // writing an unrelated property streams; reading and removing the same one conflicts (the
    // variable may repeat across rows)
    assert!(!has_eager(
        &mut g,
        "MATCH (n:P) WHERE n.g = 1 SET n.flag = true"
    ));
    assert!(has_eager(&mut g, "MATCH (n:P) WHERE n.g = 3 REMOVE n.g"));
}

#[test]
fn merge_then_create_on_the_same_label_keeps_clause_order() {
    // clause at a time: MERGE for both rows (P1, P2), then CREATE for both (P2, P3) -> 4 nodes; row
    // by row the second MERGE would find the first row's CREATE -> 3
    let mut g = Graph::new();
    let q = "UNWIND [1, 2] AS x MERGE (a:P {id: x}) CREATE (b:P {id: x + 1})";
    assert!(has_eager(&mut g, q));
    ok(&mut g, q);
    assert_eq!(int(&mut g, "MATCH (n:P) RETURN count(n)"), 4);
}

#[test]
fn create_then_merge_on_the_same_label_keeps_clause_order() {
    // CREATE for both rows first (P2, P1), then MERGE matches both -> 2 nodes; streamed, row 1's
    // MERGE (P1) would run before row 2's CREATE (P1) -> 3
    let mut g = Graph::new();
    let q = "UNWIND [1, 2] AS x CREATE (a:P {id: 3 - x}) MERGE (b:P {id: x})";
    assert!(has_eager(&mut g, q));
    ok(&mut g, q);
    assert_eq!(int(&mut g, "MATCH (n:P) RETURN count(n)"), 2);
}

#[test]
fn reader_above_a_streaming_write_sees_every_write() {
    // the expand reads R while the CREATE below writes R: for row (a1, b2) the expand must already
    // see the edge created for (a1, b1)
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:A {id: 1}), (:B {id: 1}), (:B {id: 2})");
    let q =
        "MATCH (a:A) MATCH (b:B) CREATE (a)-[:R]->(:Q) WITH a MATCH (a)-[:R]->(q) RETURN count(q)";
    assert!(has_eager(&mut g, q));
    assert_eq!(int(&mut g, q), 4);
}

#[test]
fn label_change_under_label_scan_is_eager() {
    let mut g = people(10);
    ok(&mut g, "MATCH (n:P) WHERE n.id <= 5 SET n:Old");
    let q = "MATCH (n:Old) REMOVE n:Old SET n:P";
    assert!(has_eager(&mut g, q));
    ok(&mut g, q);
    assert_eq!(int(&mut g, "MATCH (n:Old) RETURN count(n)"), 0);
    // adding a label nothing scans streams
    assert!(!has_eager(&mut g, "MATCH (n:P) SET n:Fresh"));
}

#[test]
fn explain_shows_the_eager_operator() {
    let mut g = people(5);
    let ops = plan_operators(&mut g, "MATCH (n:P) CREATE (:P)");
    assert!(ops.contains(&"Eager".to_string()), "{ops:?}");
    assert!(ops.contains(&"CreateNode".to_string()), "{ops:?}");
}

// -- semantics that don't depend on a barrier -------------------------

#[test]
fn limit_after_a_write_still_applies_every_write() {
    let mut g = people(40);
    let rows = ok(&mut g, "MATCH (n:P) SET n.flag = true RETURN n.id LIMIT 1");
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&mut g, "MATCH (n:P) WHERE n.flag RETURN count(n)"), 40);
}

#[test]
fn limit_sees_writes_under_any_operator() {
    // the writes sit under an OPTIONAL MATCH and a FOREACH: LIMIT still can't cut them short
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "UNWIND range(1, 10) AS i CREATE (a:N {i: i}) WITH a \
         OPTIONAL MATCH (a)-[:R]->(b) RETURN a.i LIMIT 1",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&mut g, "MATCH (n:N) RETURN count(n)"), 10);
    let rows = ok(
        &mut g,
        "UNWIND range(1, 5) AS i FOREACH (x IN [i] | CREATE (:F {x: x})) \
         WITH i MATCH (n:N {i: i}) RETURN n.i LIMIT 1",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(int(&mut g, "MATCH (n:F) RETURN count(n)"), 5);
}

#[test]
fn streaming_merge_is_row_sequential() {
    let mut g = Graph::new();
    ok(&mut g, "UNWIND [1, 1, 2] AS x MERGE (:P {id: x})");
    assert_eq!(int(&mut g, "MATCH (n:P) RETURN count(n)"), 2);
    ok(
        &mut g,
        "UNWIND [1, 2, 2] AS x MERGE (a:P {id: x}) MERGE (b:Q {id: x})",
    );
    assert_eq!(int(&mut g, "MATCH (n:P) RETURN count(n)"), 2);
    assert_eq!(int(&mut g, "MATCH (n:Q) RETURN count(n)"), 2);
    // a relationship MERGE matching several edges fans out per row
    ok(
        &mut g,
        "MATCH (a:P {id: 1}) MATCH (b:Q {id: 1}) CREATE (a)-[:R]->(b), (a)-[:R]->(b)",
    );
    let rows = ok(
        &mut g,
        "MATCH (a:P {id: 1}) MATCH (b:Q {id: 1}) MERGE (a)-[r:R]->(b) RETURN r",
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(int(&mut g, "MATCH ()-[r:R]->() RETURN count(r)"), 2);
}

#[test]
fn write_then_read_in_a_later_clause_sees_the_write() {
    let mut g = people(10);
    assert_eq!(
        int(
            &mut g,
            "CREATE (:P {id: 100}) WITH 1 AS x MATCH (p:P) RETURN count(p)"
        ),
        11
    );
    assert_eq!(
        int(
            &mut g,
            "MATCH (n:P) SET n.s = 1 WITH count(*) AS c MATCH (m:P) WHERE m.s = 1 RETURN count(m)"
        ),
        11
    );
}

#[test]
fn delete_remains_a_barrier() {
    let mut g = people(10);
    ok(
        &mut g,
        "MATCH (a:P) MATCH (b:P) WHERE b.id = a.id + 1 CREATE (a)-[:R]->(b)",
    );
    // every relationship of every row is deleted before any node
    ok(&mut g, "MATCH (a:P)-[r:R]->(b:P) DELETE r, a, b");
    assert_eq!(int(&mut g, "MATCH (n:P) RETURN count(n)"), 0);
}

#[test]
fn foreach_bodies_still_apply_clause_by_clause() {
    let mut g = Graph::new();
    ok(&mut g, "FOREACH (x IN [1, 2, 3] | CREATE (:F {id: x}))");
    assert_eq!(int(&mut g, "MATCH (f:F) RETURN count(f)"), 3);
    ok(
        &mut g,
        "MATCH (f:F) FOREACH (_ IN CASE WHEN f.id > 1 THEN [1] ELSE [] END | SET f.big = true)",
    );
    assert_eq!(int(&mut g, "MATCH (f:F) WHERE f.big RETURN count(f)"), 2);
}

#[test]
fn write_results_and_stats_are_unchanged() {
    let mut g = people(5);
    let res = execute_with_params(
        &mut g,
        "MATCH (a:P) CREATE (a)-[:R]->(b:Q {id: a.id}) RETURN b.id ORDER BY b.id",
        &HashMap::new(),
    )
    .unwrap();
    assert_eq!(res.rows.len(), 5);
    assert_eq!(res.stats.nodes_created, 5);
    assert_eq!(res.stats.relationships_created, 5);
    assert_eq!(res.stats.properties_set, 5);
}
