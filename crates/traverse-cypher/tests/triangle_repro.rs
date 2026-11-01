//! Cycle closure + bare count(*): the aggregate-pushdown path must honor an already-bound `to`
//! variable, or triangles get over-counted (see operators/aggregate/expand.rs, try_expand_aggregate_pushdown).

use traverse_core::graph::Graph;
use traverse_core::tx::view::GraphView;

fn build_triangle_graph() -> Graph {
    // node 0 on a triangle 0->1->2->0 (the ONLY triangle), plus an open 3-path 0->3->4->5
    let mut g = Graph::new();
    let n: Vec<_> = (0..6)
        .map(|_| g.create_node(&["User"], Default::default()))
        .collect();
    let add = |g: &mut Graph, a: usize, b: usize| {
        g.create_edge(n[a], n[b], "Friend", Default::default())
            .unwrap();
    };
    add(&mut g, 0, 1);
    add(&mut g, 1, 2);
    add(&mut g, 2, 0); // triangle
    add(&mut g, 0, 3);
    add(&mut g, 3, 4);
    add(&mut g, 4, 5); // open 3-path
    g
}

#[test]
fn count_star_over_cycle_closure_matches_actual_triangles() {
    let mut g = build_triangle_graph();
    let q_count = "MATCH (a)-[:Friend]->(b)-[:Friend]->(c)-[:Friend]->(a) \
                   WHERE id(a) = 0 RETURN count(*) AS n";
    let r = traverse_cypher::execute(&mut g as &mut dyn GraphView, q_count).unwrap();
    let n = match r.rows[0][0] {
        traverse_cypher::types::CypherValue::Integer(n) => n,
        _ => panic!(),
    };
    assert_eq!(n, 1, "exactly one triangle through node 0");

    // the same query through a WITH pipe must agree
    let q_with = "MATCH (a)-[:Friend]->(b)-[:Friend]->(c)-[:Friend]->(a) \
                  WHERE id(a) = 0 WITH a, b, c RETURN count(*) AS n";
    let r2 = traverse_cypher::execute(&mut g as &mut dyn GraphView, q_with).unwrap();
    let n2 = match r2.rows[0][0] {
        traverse_cypher::types::CypherValue::Integer(n) => n,
        _ => panic!(),
    };
    assert_eq!(n, n2, "bare count(*) and WITH-then-count must agree");
}
