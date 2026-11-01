//! Unbounded `[*]` var-length path search must not run away. An unbounded quantifier means
//! max_depth ~ u32::MAX; with both endpoints unbound (an N^2 sweep over node pairs) the search must
//! stay bounded and honor the query deadline. `SHORTEST k` is GQL-only, so these go through the
//! same `ShortestPathOp` via openCypher `allShortestPaths`; the k-path enumerators are unit-tested
//! in the crate.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run(graph: &mut Graph, q: &str) -> Result<Vec<Vec<CypherValue>>, String> {
    let params: HashMap<String, CypherValue> = HashMap::new();
    match execute_with_params(graph, q, &params) {
        Ok(res) => Ok(res.rows),
        Err(e) => Err(e.to_string()),
    }
}

fn cyclic_graph(n: usize) -> Graph {
    let mut g = Graph::new();
    for i in 0..n {
        run(&mut g, &format!("CREATE (:N {{id: {i}}})")).expect("node");
    }
    for i in 0..n {
        let j = (i + 1) % n;
        run(
            &mut g,
            &format!("MATCH (a:N {{id:{i}}}),(b:N {{id:{j}}}) CREATE (a)-[:R]->(b)"),
        )
        .expect("ring edge");
    }
    for (i, j) in [(0, n / 2), (1, n / 2 + 1), (2, n - 1)] {
        run(
            &mut g,
            &format!("MATCH (a:N {{id:{i}}}),(b:N {{id:{j}}}) CREATE (a)-[:R]->(b)"),
        )
        .expect("chord");
    }
    g
}

#[test]
fn allshortestpaths_unbounded_both_endpoints_terminates() {
    // the pathological shape: both endpoints unbound + unbounded var-length; must return promptly
    // (a loopless path can't exceed the node count)
    let mut g = cyclic_graph(14);
    let rows = run(
        &mut g,
        "MATCH (a:N),(b:N) MATCH p = allShortestPaths((a)-[*]-(b)) RETURN count(p) AS c",
    )
    .expect("must not hang");
    assert!(matches!(rows[0][0], CypherValue::Integer(_)));
}

#[test]
fn allshortestpaths_bounded_correct() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE (:N {id:0})-[:R]->(:N {id:1})-[:R]->(:N {id:2})",
    )
    .expect("chain");
    let rows = run(
        &mut g,
        "MATCH (a:N {id:0}),(b:N {id:2}) MATCH p = allShortestPaths((a)-[*]-(b)) RETURN length(p) AS len",
    )
    .expect("query");
    assert_eq!(rows[0][0], CypherValue::Integer(2));
}
