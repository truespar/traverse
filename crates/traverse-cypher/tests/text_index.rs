//! Native TEXT (trigram) index end to end: `CREATE TEXT INDEX ...` plus transparent acceleration of
//! `CONTAINS` / `ENDS WITH` / `STARTS WITH` / string `=`. The bar: the *indexed* path returns
//! exactly what the *scan* path does, so each query runs against two identical graphs, one indexed,
//! and the results are compared. EXPLAIN separately proves the index is used.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run_params(
    graph: &mut Graph,
    q: &str,
    params: &HashMap<String, CypherValue>,
) -> Vec<Vec<CypherValue>> {
    execute_with_params(graph, q, params)
        .unwrap_or_else(|e| panic!("query failed: {q}\n{e}"))
        .rows
}

fn run(graph: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    run_params(graph, q, &HashMap::new())
}

fn names(rows: &[Vec<CypherValue>]) -> Vec<String> {
    let mut v: Vec<String> = rows
        .iter()
        .map(|r| match &r[0] {
            CypherValue::String(s) => s.clone(),
            o => panic!("expected string, got {o:?}"),
        })
        .collect();
    v.sort();
    v
}

fn corpus(indexed: bool) -> Graph {
    let mut g = Graph::new();
    if indexed {
        run(&mut g, "CREATE TEXT INDEX emails FOR (u:User) ON (u.email)");
    }
    for (name, email) in [
        ("alice", "alice@example.com"),
        ("bob", "bob@example.org"),
        ("carol", "carol@test.com"),
        ("dave", "dave.smith@corp.example.com"),
        ("erin", "erin@sub.test.org"),
    ] {
        run(
            &mut g,
            &format!("CREATE (:User {{name: '{name}', email: '{email}'}})"),
        );
    }
    // a node without the property, and one where it isn't a string
    run(&mut g, "CREATE (:User {name: 'noemail'})");
    run(&mut g, "CREATE (:User {name: 'intmail', email: 42})");
    g
}

/// Does the EXPLAIN tree contain an operator of this type?
fn plan_has_operator(graph: &mut Graph, query: &str, op: &str) -> bool {
    let res =
        execute_with_params(graph, &format!("EXPLAIN {query}"), &HashMap::new()).expect("explain");
    fn walk(v: &CypherValue, op: &str) -> bool {
        if let CypherValue::Map(m) = v {
            if let Some(CypherValue::String(t)) = m.get("operatorType")
                && t == op
            {
                return true;
            }
            if let Some(CypherValue::List(children)) = m.get("children") {
                return children.iter().any(|c| walk(c, op));
            }
        }
        false
    }
    walk(res.plan.as_ref().expect("plan present"), op)
}

/// Asserts the indexed and unindexed paths agree and, when `accelerated`, that the plan uses the
/// trigram index.
fn check(q: &str, accelerated: bool) -> Vec<String> {
    let mut with = corpus(true);
    let mut without = corpus(false);
    let a = names(&run(&mut with, q));
    let b = names(&run(&mut without, q));
    assert_eq!(a, b, "indexed and scan paths must agree: {q}");
    assert_eq!(
        plan_has_operator(&mut corpus(true), q, "NodeTextIndexScan"),
        accelerated,
        "acceleration expectation failed for: {q}"
    );
    a
}

#[test]
fn contains_matches_scan_path() {
    let got = check(
        "MATCH (u:User) WHERE u.email CONTAINS 'example' RETURN u.name",
        true,
    );
    assert_eq!(got, vec!["alice", "bob", "dave"]);
}

#[test]
fn ends_with_matches_scan_path() {
    let got = check(
        "MATCH (u:User) WHERE u.email ENDS WITH '.org' RETURN u.name",
        true,
    );
    assert_eq!(got, vec!["bob", "erin"]);
}

#[test]
fn starts_with_matches_scan_path() {
    let got = check(
        "MATCH (u:User) WHERE u.email STARTS WITH 'alice' RETURN u.name",
        true,
    );
    assert_eq!(got, vec!["alice"]);
}

#[test]
fn equality_matches_scan_path() {
    let got = check(
        "MATCH (u:User) WHERE u.email = 'carol@test.com' RETURN u.name",
        true,
    );
    assert_eq!(got, vec!["carol"]);
}

#[test]
fn short_patterns_still_exact() {
    // 1-2 char patterns can't form a trigram: the index falls back to its doc set and the filter
    // refines; results must still be exact
    let got = check(
        "MATCH (u:User) WHERE u.email CONTAINS 'o' RETURN u.name",
        true,
    );
    assert_eq!(got, vec!["alice", "bob", "carol", "dave", "erin"]);
    let got2 = check(
        "MATCH (u:User) WHERE u.email ENDS WITH 'm' RETURN u.name",
        true,
    );
    assert_eq!(got2, vec!["alice", "carol", "dave"]);
}

#[test]
fn parameter_pattern_accelerates() {
    let mut params = HashMap::new();
    params.insert("s".to_string(), CypherValue::String("test".into()));
    let q = "MATCH (u:User) WHERE u.email CONTAINS $s RETURN u.name";
    let mut with = corpus(true);
    let mut without = corpus(false);
    let a = names(&run_params(&mut with, q, &params));
    let b = names(&run_params(&mut without, q, &params));
    assert_eq!(a, b);
    assert_eq!(a, vec!["carol", "erin"]);
}

#[test]
fn null_pattern_yields_no_rows() {
    // CONTAINS null -> null predicate -> no rows, no error, same as scan
    let q = "MATCH (u:User) WHERE u.email CONTAINS null RETURN u.name";
    let mut with = corpus(true);
    let mut without = corpus(false);
    assert_eq!(run(&mut with, q), run(&mut without, q));
    assert!(run(&mut with, q).is_empty());
}

#[test]
fn unindexed_property_not_accelerated() {
    // the index covers u.email, not u.name: no acceleration, still correct
    let got = check(
        "MATCH (u:User) WHERE u.name CONTAINS 'ali' RETURN u.name",
        false,
    );
    assert_eq!(got, vec!["alice"]);
}

#[test]
fn index_maintained_on_writes() {
    let mut g = corpus(true);
    run(
        &mut g,
        "CREATE (:User {name: 'frank', email: 'frank@example.com'})",
    );
    let rows = names(&run(
        &mut g,
        "MATCH (u:User) WHERE u.email CONTAINS 'example' RETURN u.name",
    ));
    assert!(
        rows.contains(&"frank".to_string()),
        "new node must be indexed"
    );
    // change alice's email; she must leave the 'example' results
    run(
        &mut g,
        "MATCH (u:User {name: 'alice'}) SET u.email = 'alice@other.net'",
    );
    let rows2 = names(&run(
        &mut g,
        "MATCH (u:User) WHERE u.email CONTAINS 'example' RETURN u.name",
    ));
    assert!(!rows2.contains(&"alice".to_string()));
    let rows3 = names(&run(
        &mut g,
        "MATCH (u:User) WHERE u.email ENDS WITH '.net' RETURN u.name",
    ));
    assert_eq!(rows3, vec!["alice"]);
    // delete bob; he must vanish
    run(&mut g, "MATCH (u:User {name: 'bob'}) DETACH DELETE u");
    let rows4 = names(&run(
        &mut g,
        "MATCH (u:User) WHERE u.email ENDS WITH '.org' RETURN u.name",
    ));
    assert_eq!(rows4, vec!["erin"]);
}

#[test]
fn show_indexes_reports_text_type() {
    let mut g = corpus(true);
    let rows = run(
        &mut g,
        "SHOW INDEXES YIELD name, type, labelsOrTypes, properties \
         WHERE type = 'TEXT' RETURN name, labelsOrTypes, properties",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("emails".to_string()));
    assert_eq!(rows[0][1], CypherValue::String("User".to_string()));
    assert_eq!(rows[0][2], CypherValue::String("email".to_string()));
}

#[test]
fn drop_index_falls_back_to_scan() {
    let mut g = corpus(true);
    run(&mut g, "DROP INDEX emails");
    let q = "MATCH (u:User) WHERE u.email CONTAINS 'example' RETURN u.name";
    assert!(!plan_has_operator(&mut g, q, "NodeTextIndexScan"));
    assert_eq!(names(&run(&mut g, q)), vec!["alice", "bob", "dave"]);
}

#[test]
fn relationship_text_index_form_still_edge_index() {
    // `CREATE TEXT INDEX ... FOR ()-[r:T]-() ON (r.p)` routes to the edge b-tree index (the trigram
    // index is node-only)
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE TEXT INDEX rel_idx FOR ()-[r:REL]-() ON (r.kind)",
    );
    let rows = run(
        &mut g,
        "SHOW INDEXES YIELD name, entityType WHERE name = 'rel_idx' RETURN entityType",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("RELATIONSHIP".to_string()));
}
