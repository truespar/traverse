//! Quantified path patterns in the Bolt/openCypher dialect: a Neo4j quantifier on a relationship
//! (`-[:R]->{m,n}` / `->+` / `->*`), semantically a variable-length relationship, lowered to the
//! same var-length machinery as the GQL dialect.

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

// directed chain n0 -> n1 -> n2 -> n3
fn chain() -> Graph {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE (:N {id:0})-[:R]->(:N {id:1})-[:R]->(:N {id:2})-[:R]->(:N {id:3})",
    )
    .expect("chain");
    g
}

fn ids(graph: &mut Graph, q: &str) -> Vec<i64> {
    let mut v: Vec<i64> = run(graph, q)
        .expect("query")
        .iter()
        .map(|r| match r[0] {
            CypherValue::Integer(n) => n,
            ref o => panic!("expected int, got {o:?}"),
        })
        .collect();
    v.sort();
    v
}

#[test]
fn bounded_range_quantifier() {
    let mut g = chain();
    let got = ids(
        &mut g,
        "MATCH (a:N {id:0})-[:R]->{1,3}(b) RETURN b.id AS id",
    );
    assert_eq!(got, vec![1, 2, 3]);
}

#[test]
fn exact_count_quantifier() {
    let mut g = chain();
    let got = ids(&mut g, "MATCH (a:N {id:0})-[:R]->{2}(b) RETURN b.id AS id");
    assert_eq!(got, vec![2]);
}

#[test]
fn plus_quantifier_one_or_more() {
    let mut g = chain();
    let got = ids(&mut g, "MATCH (a:N {id:0})-[:R]->+(b) RETURN b.id AS id");
    assert_eq!(got, vec![1, 2, 3]);
}

#[test]
fn star_quantifier_zero_or_more() {
    let mut g = chain();
    // `*` is 0+: the zero-hop match binds b = a (id 0)
    let got = ids(&mut g, "MATCH (a:N {id:0})-[:R]->*(b) RETURN b.id AS id");
    assert_eq!(got, vec![0, 1, 2, 3]);
}

#[test]
fn open_ended_min() {
    let mut g = chain();
    // `{2,}` -> 2 or more
    let got = ids(&mut g, "MATCH (a:N {id:0})-[:R]->{2,}(b) RETURN b.id AS id");
    assert_eq!(got, vec![2, 3]);
}

#[test]
fn open_ended_max() {
    let mut g = chain();
    // `{,2}` -> 0..2 (includes the zero-hop self-match)
    let got = ids(&mut g, "MATCH (a:N {id:0})-[:R]->{,2}(b) RETURN b.id AS id");
    assert_eq!(got, vec![0, 1, 2]);
}

#[test]
fn quantifier_matches_varlength_exactly() {
    // the quantifier form must match the classic var-length spelling exactly
    let mut g = chain();
    let qpp = ids(
        &mut g,
        "MATCH (a:N {id:0})-[:R]->{1,3}(b) RETURN b.id AS id",
    );
    let varlen = ids(
        &mut g,
        "MATCH (a:N {id:0})-[:R*1..3]->(b) RETURN b.id AS id",
    );
    assert_eq!(qpp, varlen);
}

#[test]
fn quantifier_binds_edge_list() {
    // as with var-length, the relationship variable binds the edge list
    let mut g = chain();
    let rows = run(
        &mut g,
        "MATCH (a:N {id:0})-[r:R]->{2}(b) RETURN size(r) AS hops, b.id AS id",
    )
    .expect("query");
    assert_eq!(rows[0][0], CypherValue::Integer(2));
    assert_eq!(rows[0][1], CypherValue::Integer(2));
}

#[test]
fn range_and_quantifier_conflict_errors() {
    let mut g = chain();
    let err = run(&mut g, "MATCH (a:N)-[:R*1..2]->{1,3}(b) RETURN b")
        .expect_err("both range and quantifier");
    assert!(err.to_lowercase().contains("quantifier") || err.to_lowercase().contains("range"));
}

// -- parenthesized group form: ((a)-[r]->(b)){m,n} ------------------------

#[test]
fn group_bounded_range() {
    let mut g = chain();
    let got = ids(
        &mut g,
        "MATCH (a:N {id:0}) ((n)-[:R]->(m)){1,3} (b) RETURN b.id AS id",
    );
    assert_eq!(got, vec![1, 2, 3]);
}

#[test]
fn group_plus() {
    let mut g = chain();
    let got = ids(
        &mut g,
        "MATCH (a:N {id:0}) (()-[:R]->())+ (b) RETURN b.id AS id",
    );
    assert_eq!(got, vec![1, 2, 3]);
}

#[test]
fn group_star_zero_or_more() {
    let mut g = chain();
    let got = ids(
        &mut g,
        "MATCH (a:N {id:0}) ((n)-[:R]->(m))* (b) RETURN b.id AS id",
    );
    assert_eq!(got, vec![0, 1, 2, 3]);
}

#[test]
fn group_matches_rel_quantifier() {
    let mut g = chain();
    let group = ids(
        &mut g,
        "MATCH (a:N {id:0}) ((n)-[:R]->(m)){1,3} (b) RETURN b.id AS id",
    );
    let rel = ids(
        &mut g,
        "MATCH (a:N {id:0})-[:R]->{1,3}(b) RETURN b.id AS id",
    );
    assert_eq!(group, rel);
}

#[test]
fn group_with_edge_var_binds_list() {
    let mut g = chain();
    let rows = run(
        &mut g,
        "MATCH (a:N {id:0}) ((n)-[r:R]->(m)){2} (b) RETURN size(r) AS hops",
    )
    .expect("query");
    assert_eq!(rows[0][0], CypherValue::Integer(2));
}

#[test]
fn multi_rel_group_is_clear_error() {
    let mut g = chain();
    let err = run(
        &mut g,
        "MATCH (a:N {id:0}) ((x)-[:R]->(y)-[:R]->(z)){1,2} (b) RETURN b",
    )
    .expect_err("multi-rel group unsupported");
    assert!(err.to_lowercase().contains("multi-relationship"));
}

#[test]
fn constrained_inner_node_applies_per_iteration() {
    // inner-node constraints hold at every iteration (ISO section 16.11 group semantics; full coverage in
    // tests/qpp_group_vars.rs)
    let mut g = chain();
    // no node carries :Extra, so any 1..2-hop group requiring it matches nothing
    let rows = run(
        &mut g,
        "MATCH (a:N {id:0}) ((n:Extra)-[:R]->(m)){1,2} (b) RETURN b",
    )
    .expect("constrained inner nodes are now supported");
    assert!(rows.is_empty());
    // a satisfiable constraint brings the paths back
    let rows2 = run(
        &mut g,
        "MATCH (a:N {id:0}) ((n:N)-[:R]->(m:N)){1,2} (b) RETURN b",
    )
    .expect("query");
    assert_eq!(rows2.len(), 2, "1-hop and 2-hop paths from id 0");
}
