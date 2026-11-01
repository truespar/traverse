//! ISO section 16.11 / Neo4j **group variables** in quantified path-pattern groups: in `MATCH
//! (s)((x)-[r]->(y)){m,n}(e)` the inner `x`/`r`/`y` bind per-iteration LISTS (`r` via normal
//! var-length semantics, `x`/`y` via traversal reconstruction), and inner label/property
//! constraints hold at EVERY iteration.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run(graph: &mut Graph, q: &str) -> Result<Vec<Vec<CypherValue>>, String> {
    let params: HashMap<String, CypherValue> = HashMap::new();
    execute_with_params(graph, q, &params)
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

fn ok(graph: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    run(graph, q).unwrap_or_else(|e| panic!("query failed: {q}\n{e}"))
}

/// n0 -> n1 -> n2 -> n3 chain of :P nodes ("n0".."n3"), plus a detour n1 -> q -> n3 whose middle
/// node is NOT :P.
fn chain() -> Graph {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (n0:P {name: 'n0', v: 0}), (n1:P {name: 'n1', v: 1}), \
                (n2:P {name: 'n2', v: 2}), (n3:P {name: 'n3', v: 3}), \
                (q:Q {name: 'q'}) \
         CREATE (n0)-[:R]->(n1), (n1)-[:R]->(n2), (n2)-[:R]->(n3), \
                (n1)-[:R]->(q), (q)-[:R]->(n3)",
    );
    g
}

/// `name` properties of a LIST<NODE> column value.
fn list_names(g: &mut Graph, v: &CypherValue) -> Vec<String> {
    // reconstruction binds NodeRef values, so resolve them with a follow-up query
    match v {
        CypherValue::List(items) => items
            .iter()
            .map(|n| match n {
                CypherValue::NodeRef(id) | CypherValue::Node { id, .. } => {
                    let rows = ok(
                        g,
                        &format!("MATCH (m) WHERE id(m) = {} RETURN m.name", id.0),
                    );
                    match &rows[0][0] {
                        CypherValue::String(s) => s.clone(),
                        o => panic!("expected name string, got {o:?}"),
                    }
                }
                o => panic!("expected node in group list, got {o:?}"),
            })
            .collect(),
        o => panic!("expected LIST group variable, got {o:?}"),
    }
}

#[test]
fn group_variables_bind_per_iteration_lists() {
    let mut g = chain();
    // exactly 2 hops from n0: n0->n1->n2 (chain) and n0->n1->q (detour)
    let rows = ok(
        &mut g,
        "MATCH (s:P {name: 'n0'})((x)-[r:R]->(y)){2,2}(e) \
         RETURN x, y, e.name, size(r)",
    );
    assert_eq!(rows.len(), 2, "two 2-hop paths from n0");
    for row in &rows {
        assert_eq!(row[3], CypherValue::Integer(2));
        assert!(
            matches!(&row[0], CypherValue::List(l) if l.len() == 2),
            "x is a 2-list"
        );
        assert!(
            matches!(&row[1], CypherValue::List(l) if l.len() == 2),
            "y is a 2-list"
        );
    }
}

#[test]
fn group_lists_content_and_alignment() {
    let mut g = chain();
    // 2 hops from n0 ending at n2 (endpoint label+prop excludes the q detour)
    let rows = ok(
        &mut g,
        "MATCH (s:P {name: 'n0'})((x)-[r:R]->(y)){2,2}(e:P {name: 'n2'}) \
         RETURN x, y, size(r)",
    );
    assert_eq!(rows.len(), 1);
    let x = rows[0][0].clone();
    let y = rows[0][1].clone();
    assert_eq!(
        rows[0][2],
        CypherValue::Integer(2),
        "r list has one edge per iteration"
    );
    assert_eq!(
        list_names(&mut g, &x),
        vec!["n0", "n1"],
        "x = per-hop sources"
    );
    assert_eq!(
        list_names(&mut g, &y),
        vec!["n1", "n2"],
        "y = per-hop targets"
    );
}

#[test]
fn size_invariants_hold() {
    let mut g = chain();
    let rows = ok(
        &mut g,
        "MATCH (s:P {name: 'n0'})((x)-[r:R]->(y)){1,3}(e) \
         RETURN size(x) AS sx, size(r) AS sr, size(y) AS sy",
    );
    assert!(!rows.is_empty());
    for row in &rows {
        assert_eq!(row[0], row[1], "size(x) == size(r)");
        assert_eq!(row[1], row[2], "size(r) == size(y)");
    }
}

#[test]
fn inner_label_constraint_applies_every_iteration() {
    let mut g = chain();
    // length-3 paths n0->...->n3: the :P chain (n0,n1,n2,n3) and the q detour (n0,n1,q,n3); y covers
    // the intermediate nodes AND the last hop's target, so requiring :P on every y drops the detour
    let rows = ok(
        &mut g,
        "MATCH (s:P {name: 'n0'})((x:P)-[r:R]->(y:P)){3,3}(e {name: 'n3'}) \
         RETURN size(r)",
    );
    assert_eq!(
        rows.len(),
        1,
        "only the all-:P chain survives; the q detour is filtered"
    );
    // without the constraint both 3-hop paths match
    let rows2 = ok(
        &mut g,
        "MATCH (s:P {name: 'n0'})((x)-[r:R]->(y)){3,3}(e {name: 'n3'}) \
         RETURN size(r)",
    );
    assert_eq!(rows2.len(), 2);
}

#[test]
fn inner_property_constraint_applies_every_iteration() {
    let mut g = chain();
    // v < 3 on every per-hop source: the 3-hop chain has x = [n0(0), n1(1), n2(2)] passes; y.v = 1 at
    // every hop only holds for 1-hop paths ending at n1
    let rows = ok(
        &mut g,
        "MATCH (s:P {name: 'n0'})((x)-[r:R]->(y {v: 1})){1,3}(e) RETURN e.name",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("n1".into()));
}

#[test]
fn zero_length_binds_empty_lists() {
    let mut g = chain();
    // {0,1}: the zero-iteration arm keeps s itself, x/y = []
    let rows = ok(
        &mut g,
        "MATCH (s:P {name: 'n3'})((x)-[r:R]->(y)){0,1}(e) \
         RETURN size(x), size(y), e.name",
    );
    // n3 has no outgoing :R, so only the zero arm matches: e = n3, sizes 0
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::Integer(0));
    assert_eq!(rows[0][1], CypherValue::Integer(0));
    assert_eq!(rows[0][2], CypherValue::String("n3".into()));
}

#[test]
fn incoming_direction_group() {
    let mut g = chain();
    // 2 hops INCOMING from n2: n2 <- n1 <- n0; x = per-hop source in traversal order = [n2, n1],
    // y = [n1, n0]
    let rows = ok(
        &mut g,
        "MATCH (s:P {name: 'n2'})((x)<-[r:R]-(y)){2,2}(e:P {name: 'n0'}) \
         RETURN x, y",
    );
    assert_eq!(rows.len(), 1);
    let x = rows[0][0].clone();
    let y = rows[0][1].clone();
    assert_eq!(list_names(&mut g, &x), vec!["n2", "n1"]);
    assert_eq!(list_names(&mut g, &y), vec!["n1", "n0"]);
}

#[test]
fn group_vars_usable_downstream() {
    let mut g = chain();
    // group lists flow through WITH / UNWIND like any list
    let rows = ok(
        &mut g,
        "MATCH (s:P {name: 'n0'})((x)-[r:R]->(y:P)){2,2}(e:P {name: 'n2'}) \
         UNWIND y AS hop RETURN hop.name ORDER BY hop.name",
    );
    let names: Vec<String> = rows
        .iter()
        .map(|r| match &r[0] {
            CypherValue::String(s) => s.clone(),
            o => panic!("{o:?}"),
        })
        .collect();
    assert_eq!(names, vec!["n1", "n2"]);
}

#[test]
fn anonymous_groups_unchanged() {
    // fully anonymous groups still work
    let mut g = chain();
    let rows = ok(
        &mut g,
        "MATCH (s:P {name: 'n0'})(()-[:R]->()){1,3}(e {name: 'n3'}) RETURN count(*)",
    );
    assert_eq!(
        rows[0][0],
        CypherValue::Integer(2),
        "chain + q-detour 3-hop paths"
    );
}

#[test]
fn multi_relationship_group_still_clear_error() {
    let mut g = chain();
    let err = run(
        &mut g,
        "MATCH (s)((a)-[r:R]->(b)-[t:R]->(c)){1,2}(e) RETURN s",
    )
    .unwrap_err();
    assert!(err.contains("multi-relationship"), "got: {err}");
}
