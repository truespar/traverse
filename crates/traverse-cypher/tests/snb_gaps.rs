//! Query shapes typical of LDBC SNB-style workloads: pattern expressions, bound-endpoint
//! comprehensions, CASE null semantics, subquery UNIONs.

use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

fn run(g: &mut Graph, q: &str) -> Result<Vec<Vec<CypherValue>>, String> {
    traverse_cypher::execute(g, q)
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

fn ok(g: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    run(g, q).unwrap_or_else(|e| panic!("query failed: {q}\n{e}"))
}

fn fixture(g: &mut Graph) {
    ok(
        g,
        "CREATE (a:P {name: 'a'}), (b:P {name: 'b'}), (c:P {name: 'c'}) \
         CREATE (a)-[:KNOWS]->(b) \
         CREATE (a)-[:HAS_CREATOR]->(b), (c)-[:LIKES]->(a)",
    );
}

/// pattern expression under not() in RETURN
#[test]
fn pattern_expr_in_return_boolean_context() {
    let mut g = Graph::new();
    fixture(&mut g);
    let rows = ok(
        &mut g,
        "MATCH (x:P {name: 'a'}), (y:P) WHERE x <> y \
         RETURN y.name AS n, not((x)-[:KNOWS]-(y)) AS isNew ORDER BY n",
    );
    assert_eq!(rows[0][0], CypherValue::String("b".into()));
    assert_eq!(rows[0][1], CypherValue::Bool(false), "a KNOWS b");
    assert_eq!(rows[1][0], CypherValue::String("c".into()));
    assert_eq!(rows[1][1], CypherValue::Bool(true));
    // comparison = VALUE position: rejected (as in Neo4j 5)
    let ec = run(
        &mut g,
        "MATCH (x:P {name: 'a'}), (y:P {name: 'b'}) RETURN (x)-[:KNOWS]-(y) = true AS k",
    )
    .unwrap_err();
    assert!(ec.contains("Pattern expressions"), "got: {ec}");
    let rows3 = ok(
        &mut g,
        "MATCH (x:P {name: 'a'}), (y:P) WHERE x <> y \
         RETURN y.name AS n, CASE WHEN (x)-[:KNOWS]-(y) THEN 1 ELSE 0 END AS k ORDER BY n",
    );
    assert_eq!(rows3[0][1], CypherValue::Integer(1));
    assert_eq!(rows3[1][1], CypherValue::Integer(0));
    // bare value position still errors (Neo4j 5 rule)
    let e = run(
        &mut g,
        "MATCH (x:P), (y:P) RETURN (x)-[:KNOWS]-(y) AS bare LIMIT 1",
    )
    .unwrap_err();
    assert!(e.contains("Pattern expressions"), "got: {e}");
}

/// pattern expression as list-comprehension filter + size()
#[test]
fn pattern_expr_in_list_comprehension_filter() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (p:P {name: 'me'}), (t:T {name: 'tag'}) \
         CREATE (p)-[:HAS_INTEREST]->(t) \
         CREATE (m1:M {id: 1})-[:HAS_TAG]->(t) \
         CREATE (m2:M {id: 2})",
    );
    let rows = ok(
        &mut g,
        "MATCH (p:P {name: 'me'}) \
         MATCH (m:M) WITH p, collect(m) AS posts \
         RETURN size([x IN posts WHERE (x)-[:HAS_TAG]->()<-[:HAS_INTEREST]-(p)]) AS common, \
                size(posts) AS total",
    );
    assert_eq!(rows[0][0], CypherValue::Integer(1));
    assert_eq!(rows[0][1], CypherValue::Integer(2));
}

/// datetime from epochMillis + component accessors
#[test]
fn datetime_epoch_millis_components() {
    let mut g = Graph::new();
    // 1989-05-21T00:00:00Z = 611712000000 ms
    let rows = ok(
        &mut g,
        "WITH datetime({epochMillis: 611712000000}) AS d \
         RETURN d.month AS m, d.day AS day, d.year AS y",
    );
    assert_eq!(rows[0][0], CypherValue::Integer(5));
    assert_eq!(rows[0][1], CypherValue::Integer(21));
    assert_eq!(rows[0][2], CypherValue::Integer(1989));
}

/// pattern comprehension with BOTH endpoints bound, inside reduce, in a WITH projection
#[test]
fn pattern_comprehension_bound_endpoints() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:P {name: 'a'}), (b:P {name: 'b'}) \
         CREATE (a)<-[:HAS_CREATOR]-(:C {id: 1})-[:REPLY_OF]->(:Post {id: 10})-[:HAS_CREATOR]->(b) \
         CREATE (a)<-[:HAS_CREATOR]-(:C {id: 2})-[:REPLY_OF]->(:Post {id: 11})-[:HAS_CREATOR]->(b)",
    );
    let rows = ok(
        &mut g,
        "MATCH (a:P {name: 'a'}), (b:P {name: 'b'}) \
         WITH a, b, [(a)<-[:HAS_CREATOR]-(:C)-[:REPLY_OF]->(:Post)-[:HAS_CREATOR]->(b) | 1.0] AS w \
         RETURN size(w) AS n, reduce(s = 0.0, v IN w | s + v) AS total",
    );
    assert_eq!(
        rows[0][0],
        CypherValue::Integer(2),
        "two reply chains matched"
    );
    assert_eq!(rows[0][1], CypherValue::Float(2.0));
}

/// OPTIONAL MATCH with two bound endpoints + CASE null
#[test]
fn optional_match_bound_endpoints_case_null() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (m:M {id: 1}), (auth:P {name: 'auth'}), (rep:P {name: 'rep'}) \
         CREATE (m)-[:HAS_CREATOR]->(auth) \
         CREATE (c:C {id: 2})-[:REPLY_OF]->(m), (c)-[:HAS_CREATOR]->(rep) \
         CREATE (rep)-[:KNOWS]->(auth)",
    );
    let rows = ok(
        &mut g,
        "MATCH (m:M {id: 1})<-[:REPLY_OF]-(c:C)-[:HAS_CREATOR]->(p:P) \
         MATCH (m)-[:HAS_CREATOR]->(a:P) \
         OPTIONAL MATCH (a)-[r:KNOWS]-(p) \
         RETURN c.id AS cid, CASE r WHEN null THEN false ELSE true END AS knows",
    );
    assert_eq!(rows[0][0], CypherValue::Integer(2));
    assert_eq!(rows[0][1], CypherValue::Bool(true), "rep KNOWS auth");
    // absent edge: r is null and `WHEN null` NEVER matches under Cypher `=`, so it falls to ELSE
    // true (Neo4j too)
    ok(&mut g, "MATCH (rep:P {name: 'rep'})-[k:KNOWS]->() DELETE k");
    let rows2 = ok(
        &mut g,
        "MATCH (m:M {id: 1})<-[:REPLY_OF]-(c:C)-[:HAS_CREATOR]->(p:P) \
         MATCH (m)-[:HAS_CREATOR]->(a:P) \
         OPTIONAL MATCH (a)-[r:KNOWS]-(p) \
         RETURN CASE r WHEN null THEN false ELSE true END AS knows, \
                r IS NOT NULL AS actually_knows",
    );
    assert_eq!(rows2[0][0], CypherValue::Bool(true));
    assert_eq!(rows2[0][1], CypherValue::Bool(false));
}

/// one OPTIONAL MATCH, two-hop pattern from bound m through NEW variable a to BOUND p (undirected)
#[test]
fn optional_match_two_hop_bound_new_bound() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (m:M {id: 1}), (auth:P {name: 'auth'}), (rep:P {name: 'rep'}) \
         CREATE (m)-[:HAS_CREATOR]->(auth) \
         CREATE (c:C {id: 2})-[:REPLY_OF]->(m), (c)-[:HAS_CREATOR]->(rep) \
         CREATE (rep)-[:KNOWS]->(auth)",
    );
    let rows = ok(
        &mut g,
        "MATCH (m:M {id: 1})<-[:REPLY_OF]-(c:C)-[:HAS_CREATOR]->(p:P) \
         OPTIONAL MATCH (m)-[:HAS_CREATOR]->(a:P)-[r:KNOWS]-(p) \
         RETURN c.id, CASE r WHEN null THEN false ELSE true END AS knows",
    );
    assert_eq!(
        rows[0][1],
        CypherValue::Bool(true),
        "two-hop OPTIONAL with bound p"
    );
}

/// pattern comprehension whose WHERE references the ENCLOSING list-comprehension variable through
/// startNode()/endNode()
#[test]
fn pattern_comprehension_where_refs_outer_comprehension_var() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:Person {id: 1}), (b:Person {id: 2}) \
         CREATE (a)-[:KNOWS]->(b) \
         CREATE (a)<-[:HAS_CREATOR]-(:Comment)-[:REPLY_OF]->(:Post)-[:HAS_CREATOR]->(b)",
    );
    let rows = ok(
        &mut g,
        "MATCH path = allShortestPaths((p1:Person {id: 1})-[:KNOWS*0..]-(p2:Person {id: 2})) \
         WITH relationships(path) AS rels \
         WITH [r IN rels | \
             reduce(w = 0.0, v IN [ \
                 (x:Person)<-[:HAS_CREATOR]-(:Comment)-[:REPLY_OF]->(:Post)-[:HAS_CREATOR]->(y:Person) \
                 WHERE (x.id = startNode(r).id AND y.id = endNode(r).id) \
                    OR (x.id = endNode(r).id AND y.id = startNode(r).id) \
                 | 1.0] | w + v) \
         ] AS weights \
         RETURN reduce(s = 0.0, v IN weights | s + v) AS total",
    );
    assert_eq!(
        rows[0][0],
        CypherValue::Float(1.0),
        "one reply chain across the KNOWS edge"
    );
}

/// simple CASE uses Cypher `=`: `WHEN null` never matches (null = null is null), so a null operand
/// falls to ELSE
#[test]
fn simple_case_when_null_never_matches() {
    let mut g = Graph::new();
    let rows = ok(
        &mut g,
        "WITH null AS r \
         RETURN CASE r WHEN null THEN 'matched' ELSE 'else' END AS a, \
                CASE WHEN r IS NULL THEN 'isnull' ELSE 'else' END AS b, \
                CASE 1 WHEN null THEN 'x' WHEN 1 THEN 'one' ELSE 'else' END AS c",
    );
    assert_eq!(rows[0][0], CypherValue::String("else".into()));
    assert_eq!(rows[0][1], CypherValue::String("isnull".into()));
    assert_eq!(rows[0][2], CypherValue::String("one".into()));
}

/// UNION ALL inside a CALL subquery
#[test]
fn union_inside_call_subquery() {
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:P {x: 1}), (:P {x: 2}), (:Q {x: 10})");
    let rows = ok(
        &mut g,
        "CALL { MATCH (p:P) RETURN p.x AS v UNION ALL MATCH (q:Q) RETURN q.x AS v } \
         RETURN v ORDER BY v",
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], CypherValue::Integer(1));
    assert_eq!(rows[2][0], CypherValue::Integer(10));
    // UNION distinct dedups
    let rows2 = ok(
        &mut g,
        "CALL { MATCH (p:P) RETURN 1 AS v UNION MATCH (q:Q) RETURN 1 AS v } RETURN count(v)",
    );
    assert_eq!(rows2[0][0], CypherValue::Integer(1));
    // correlated: imports visible in every branch
    let rows3 = ok(
        &mut g,
        "UNWIND [1, 10] AS base \
         CALL (base) { MATCH (p:P) WHERE p.x >= base RETURN p.x AS v \
                       UNION ALL MATCH (q:Q) WHERE q.x >= base RETURN q.x AS v } \
         RETURN base, count(v) ORDER BY base",
    );
    assert_eq!(rows3[0][1], CypherValue::Integer(3), "base 1: 1,2,10");
    assert_eq!(rows3[1][1], CypherValue::Integer(1), "base 10: just 10");
}

/// string+number concat and case-insensitive epoch keys
#[test]
fn string_number_concat_and_epoch_key_case() {
    let mut g = Graph::new();
    let r = ok(
        &mut g,
        "RETURN 'e: ' + 42 + '!' AS s, 1 + 'x' AS t, 'f' + 1.5 AS u",
    );
    assert_eq!(r[0][0], CypherValue::String("e: 42!".into()));
    assert_eq!(r[0][1], CypherValue::String("1x".into()));
    assert_eq!(r[0][2], CypherValue::String("f1.5".into()));
    let d = ok(
        &mut g,
        "RETURN datetime({epochmillis: 611712000000}).month AS m",
    );
    assert_eq!(d[0][0], CypherValue::Integer(5));
}

/// GDS stream procedure inside a CALL subquery with a UNION ALL fallback branch
#[test]
fn gds_procedure_inside_call_subquery_union() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:V {id: 0}), (b:V {id: 1}) CREATE (a)-[:R {w: 3.0}]->(b)",
    );
    ok(
        &mut g,
        "CALL gds.graph.project('subq', 'V', {R: {type: 'R', properties: 'w'}})",
    );
    let rows = ok(
        &mut g,
        "CALL { \
           MATCH (x:V {id: 0}), (y:V {id: 1}) \
           CALL gds.shortestPath.dijkstra.stream('subq', \
                {sourceNode: x, targetNode: y, relationshipWeightProperty: 'w'}) \
           YIELD totalCost \
           RETURN totalCost \
           UNION ALL \
           RETURN -1.0 AS totalCost \
         } \
         RETURN max(totalCost) AS totalCost",
    );
    assert_eq!(rows[0][0], CypherValue::Float(3.0));
    // fallback branch wins when the MATCH is empty
    let rows2 = ok(
        &mut g,
        "CALL { \
           MATCH (x:V {id: 77}), (y:V {id: 1}) \
           CALL gds.shortestPath.dijkstra.stream('subq', \
                {sourceNode: x, targetNode: y, relationshipWeightProperty: 'w'}) \
           YIELD totalCost \
           RETURN totalCost \
           UNION ALL \
           RETURN -1.0 AS totalCost \
         } \
         RETURN max(totalCost) AS totalCost",
    );
    assert_eq!(rows2[0][0], CypherValue::Float(-1.0));
    // write-mode procedures still refuse clearly
    let e = run(
        &mut g,
        "CALL { MATCH (x:V) CALL gds.pageRank.write('subq', {writeProperty: 'pr'}) \
         YIELD nodePropertiesWritten RETURN nodePropertiesWritten } RETURN 1",
    )
    .unwrap_err();
    assert!(e.contains("not yet supported"), "got: {e}");
    ok(&mut g, "CALL gds.graph.drop('subq')");
}

/// DISTINCT dedups BEFORE LIMIT (WITH and RETURN)
#[test]
fn distinct_before_limit() {
    let mut g = Graph::new();
    // 10 distinct values, 5 copies each: 50 rows
    ok(&mut g, "UNWIND range(0, 49) AS i CREATE (:D {v: i % 10})");
    let r = ok(&mut g, "MATCH (d:D) RETURN DISTINCT d.v AS v LIMIT 8");
    assert_eq!(
        r.len(),
        8,
        "8 distinct values, not distinct-of-first-8-rows"
    );
    let w = ok(
        &mut g,
        "MATCH (d:D) WITH DISTINCT d.v AS v LIMIT 8 RETURN count(v)",
    );
    assert_eq!(w[0][0], CypherValue::Integer(8));
    // DISTINCT + ORDER BY + LIMIT: ordered dedup, then paginate
    let o = ok(
        &mut g,
        "MATCH (d:D) WITH DISTINCT d.v AS v ORDER BY v DESC LIMIT 3 RETURN collect(v)",
    );
    assert_eq!(
        o[0][0],
        CypherValue::List(vec![
            CypherValue::Integer(9),
            CypherValue::Integer(8),
            CypherValue::Integer(7)
        ])
    );
    // SKIP composes too
    let s = ok(
        &mut g,
        "MATCH (d:D) RETURN DISTINCT d.v AS v ORDER BY v SKIP 2 LIMIT 2",
    );
    assert_eq!(s[0][0], CypherValue::Integer(2));
    assert_eq!(s[1][0], CypherValue::Integer(3));
}

/// Neo4j's multi-arg round(): precision and rounding modes
#[test]
fn round_with_precision() {
    let mut g = Graph::new();
    let r = ok(
        &mut g,
        "RETURN round(1.23456, 2) AS a, round(2.5, 0) AS b, \
                round(1.249, 1, 'CEILING') AS c, round(-1.35, 1) AS d",
    );
    assert_eq!(r[0][0], CypherValue::Float(1.23));
    assert_eq!(r[0][1], CypherValue::Float(3.0));
    assert_eq!(r[0][2], CypherValue::Float(1.3));
    assert_eq!(r[0][3], CypherValue::Float(-1.4));
}

/// ORDER BY on FLOAT node properties via the batched sort-key path (needs enough rows to batch;
/// floats aren't "missing")
#[test]
fn order_by_float_property_batched() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "UNWIND range(0, 499) AS i CREATE (:F {id: i, score: toFloat(i % 100) / 7.0})",
    );
    let rows = ok(
        &mut g,
        "MATCH (f:F) RETURN f.score AS s ORDER BY f.score DESC LIMIT 500",
    );
    let vals: Vec<f64> = rows
        .iter()
        .map(|r| match r[0] {
            CypherValue::Float(f) => f,
            _ => panic!("expected float"),
        })
        .collect();
    for w in vals.windows(2) {
        assert!(w[0] >= w[1], "non-monotonic: {} < {}", w[0], w[1]);
    }
    // mixed int+float+null keys order correctly (nulls last DESC)
    ok(&mut g, "CREATE (:F {id: 1000}), (:F {id: 1001, score: 3})");
    let tail = ok(
        &mut g,
        "MATCH (f:F) RETURN f.score ORDER BY f.score DESC LIMIT 502",
    );
    assert_eq!(tail[501][0], CypherValue::Null);
}
