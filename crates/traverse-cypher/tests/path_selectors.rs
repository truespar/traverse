//! ISO section 14.6 path selectors (`SHORTEST k`, `ALL SHORTEST`, `ANY [k]`, `SHORTEST k GROUPS`), section 14.5
//! path modes (`WALK`/`TRAIL`/`ACYCLIC`) and section 14.4 match modes (`REPEATABLE ELEMENTS`) in the Bolt
//! dialect, on the same engine machinery as the GQL dialect.

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

/// Diamond with a long tail: a->b->d, a->c->d (two 2-hop paths), a->e->f->d (one 3-hop). Between a and d:
/// SHORTEST 1 -> one 2-hop path; ALL SHORTEST -> both 2-hop; SHORTEST 3 -> both 2-hop + the 3-hop;
/// SHORTEST 2 GROUPS -> all three (lengths 2 and 3).
fn diamond() -> Graph {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:S {name: 'a'}), (b:S {name: 'b'}), (c:S {name: 'c'}), \
                (d:S {name: 'd'}), (e:S {name: 'e'}), (f:S {name: 'f'}) \
         CREATE (a)-[:L]->(b)-[:L]->(d), (a)-[:L]->(c)-[:L]->(d), \
                (a)-[:L]->(e)-[:L]->(f)-[:L]->(d)",
    );
    g
}

#[test]
fn shortest_k_selector() {
    let mut g = diamond();
    let one = ok(
        &mut g,
        "MATCH p = SHORTEST 1 (:S {name: 'a'})-[:L]->+(:S {name: 'd'}) RETURN length(p)",
    );
    assert_eq!(one.len(), 1);
    assert_eq!(one[0][0], CypherValue::Integer(2));
    let three = ok(
        &mut g,
        "MATCH p = SHORTEST 3 (:S {name: 'a'})-[:L]->+(:S {name: 'd'}) \
         RETURN length(p) ORDER BY length(p)",
    );
    assert_eq!(three.len(), 3);
    assert_eq!(
        three[2][0],
        CypherValue::Integer(3),
        "third path fills from the next tier"
    );
}

#[test]
fn all_shortest_selector() {
    let mut g = diamond();
    let rows = ok(
        &mut g,
        "MATCH p = ALL SHORTEST (:S {name: 'a'})-[:L]->+(:S {name: 'd'}) RETURN length(p)",
    );
    assert_eq!(rows.len(), 2, "both 2-hop paths, not the 3-hop one");
}

#[test]
fn shortest_k_groups_selector() {
    let mut g = diamond();
    let rows = ok(
        &mut g,
        "MATCH p = SHORTEST 2 GROUPS (:S {name: 'a'})-[:L]->+(:S {name: 'd'}) \
         RETURN length(p) ORDER BY length(p)",
    );
    assert_eq!(
        rows.len(),
        3,
        "all paths in the two smallest length classes"
    );
}

#[test]
fn any_shortest_and_any_k() {
    let mut g = diamond();
    let one = ok(
        &mut g,
        "MATCH p = ANY SHORTEST (:S {name: 'a'})-[:L]->+(:S {name: 'd'}) RETURN length(p)",
    );
    assert_eq!(one.len(), 1);
    assert_eq!(one[0][0], CypherValue::Integer(2));
    // ANY k: at most k paths (top-level LIMIT semantics, as in the GQL dialect)
    let two = ok(
        &mut g,
        "MATCH p = ANY 2 (:S {name: 'a'})-[:L]->+(:S {name: 'd'}) RETURN length(p)",
    );
    assert_eq!(two.len(), 2);
    let any = ok(
        &mut g,
        "MATCH p = ANY (:S {name: 'a'})-[:L]->+(:S {name: 'd'}) RETURN length(p)",
    );
    assert_eq!(any.len(), 1);
}

#[test]
fn selector_noise_words_and_unnamed() {
    let mut g = diamond();
    let rows = ok(
        &mut g,
        "MATCH SHORTEST 1 PATHS (s:S {name: 'a'})-[:L]->+(d:S {name: 'd'}) RETURN d.name",
    );
    assert_eq!(rows.len(), 1);
    let rows2 = ok(
        &mut g,
        "MATCH p = ALL SHORTEST PATHS (:S {name: 'a'})-[:L]->+(:S {name: 'd'}) RETURN length(p)",
    );
    assert_eq!(rows2.len(), 2);
}

#[test]
fn acyclic_path_mode() {
    // triangle a->b->c->a: TRAIL allows a->b->c->a (edge-unique), ACYCLIC forbids revisiting a
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:T {name: 'a'})-[:R]->(b:T {name: 'b'})-[:R]->(c:T {name: 'c'})-[:R]->(a)",
    );
    let trail = ok(
        &mut g,
        "MATCH p = (s:T {name: 'a'})-[:R*3]->(s) RETURN length(p)",
    );
    assert_eq!(
        trail.len(),
        1,
        "TRAIL (default) permits the closed triangle"
    );
    let acyclic = ok(
        &mut g,
        "MATCH p = ACYCLIC (s:T {name: 'a'})-[:R*3]->(s) RETURN length(p)",
    );
    assert!(
        acyclic.is_empty(),
        "ACYCLIC forbids revisiting the start node"
    );
}

#[test]
fn walk_mode_and_repeatable_elements() {
    // one edge each way between two nodes: a->b->a->b needs the a->b edge twice, impossible under
    // TRAIL, fine under WALK / REPEATABLE ELEMENTS
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE (a:W {name: 'a'})-[:R]->(b:W {name: 'b'}) CREATE (b)-[:R]->(a)",
    );
    let trail = ok(
        &mut g,
        "MATCH p = (s:W {name: 'a'})-[:R*3]->(t:W {name: 'b'}) RETURN length(p)",
    );
    assert!(trail.is_empty(), "TRAIL: the 3-hop walk reuses an edge");
    let walk = ok(
        &mut g,
        "MATCH p = WALK (s:W {name: 'a'})-[:R*3]->(t:W {name: 'b'}) RETURN length(p)",
    );
    assert_eq!(walk.len(), 1, "WALK permits edge reuse");
    let rep = ok(
        &mut g,
        "MATCH REPEATABLE ELEMENTS p = (s:W {name: 'a'})-[:R*3]->(t:W {name: 'b'}) RETURN length(p)",
    );
    assert_eq!(rep.len(), 1, "REPEATABLE ELEMENTS = WALK semantics");
    // explicitly spelling the default is accepted
    let diff = ok(
        &mut g,
        "MATCH DIFFERENT RELATIONSHIPS p = (s:W {name: 'a'})-[:R*2]->(s) RETURN length(p)",
    );
    assert_eq!(diff.len(), 1);
}

#[test]
fn selector_errors() {
    let mut g = diamond();
    let e = run(&mut g, "MATCH p = SHORTEST 0 (:S)-[:L]->+(:S) RETURN p").unwrap_err();
    assert!(e.contains("positive"), "got: {e}");
    let e2 = run(
        &mut g,
        "MATCH p = SHORTEST 1 shortestPath((:S)-[:L*]->(:S)) RETURN p",
    )
    .unwrap_err();
    assert!(e2.contains("can't be combined"), "got: {e2}");
    let e3 = run(
        &mut g,
        "MATCH p = SHORTEST 2 (:S {name: 'a'})-[:L]->(:S) RETURN p",
    )
    .unwrap_err();
    assert!(e3.contains("variable-length"), "got: {e3}");
}

#[test]
fn plain_patterns_unaffected() {
    // variables named like the keywords still work
    let mut g = Graph::new();
    ok(&mut g, "CREATE (:X {name: 'n1'})");
    let rows = ok(&mut g, "MATCH (walk:X) RETURN walk.name");
    assert_eq!(rows.len(), 1);
    let rows2 = ok(
        &mut g,
        "MATCH (shortest:X) WHERE shortest.name = 'n1' RETURN shortest.name",
    );
    assert_eq!(rows2.len(), 1);
    let rows3 = ok(&mut g, "WITH 1 AS any RETURN any + 1");
    assert_eq!(rows3[0][0], CypherValue::Integer(2));
}
