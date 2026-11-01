//! Every error carries the GQLSTATUS of its condition: the code a client branches on, the same
//! over every protocol and binding.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_core::tx::MvccStore;
use traverse_cypher::execute_with_params;
use traverse_cypher::status::GqlStatus;

fn status_of(graph: &mut Graph, query: &str) -> &'static str {
    match execute_with_params(graph, query, &HashMap::new()) {
        Ok(_) => panic!("expected `{query}` to fail"),
        Err(e) => e.status().code(),
    }
}

#[test]
fn syntax_and_scope() {
    let mut g = Graph::new();
    assert_eq!(status_of(&mut g, "RETURN ("), "42001");
    assert_eq!(status_of(&mut g, "MATCH (n) RETURN x"), "42N62");
    assert_eq!(status_of(&mut g, "MATCH (n)-[n]->() RETURN n"), "42N59");
    assert_eq!(status_of(&mut g, "CREATE (a) CREATE (a)"), "42N78");
    assert_eq!(status_of(&mut g, "RETURN nosuch(1)"), "42N48");
    assert_eq!(status_of(&mut g, "CALL no.such.procedure()"), "42N08");
    assert_eq!(status_of(&mut g, "RETURN $missing"), "42N51");
    assert_eq!(
        status_of(&mut g, "MATCH (n) WHERE count(n) > 1 RETURN n"),
        "42I24"
    );
    assert_eq!(status_of(&mut g, "RETURN 1 LIMIT -1"), "22N02");
}

#[test]
fn data_exceptions() {
    let mut g = Graph::new();
    assert_eq!(status_of(&mut g, "RETURN 1 / 0"), "22012");
    assert_eq!(status_of(&mut g, "RETURN 9223372036854775807 + 1"), "22003");
    assert_eq!(status_of(&mut g, "RETURN date('2020-13-45')"), "22007");
    // caught statically it's a syntax error, at run time a data exception
    assert_eq!(status_of(&mut g, "RETURN 1 + true"), "42001");
    assert_eq!(
        status_of(&mut g, "UNWIND [true] AS b RETURN 1 + b"),
        "22G03"
    );
}

#[test]
fn constraints_and_dependent_objects() {
    let mut g = Graph::new();
    let ok = |g: &mut Graph, q: &str| {
        execute_with_params(g, q, &HashMap::new()).unwrap();
    };
    ok(
        &mut g,
        "CREATE CONSTRAINT u FOR (n:U) REQUIRE n.k IS UNIQUE",
    );
    ok(
        &mut g,
        "CREATE CONSTRAINT e FOR (n:E) REQUIRE n.k IS NOT NULL",
    );
    ok(
        &mut g,
        "CREATE CONSTRAINT t FOR (n:T) REQUIRE n.k IS :: INTEGER",
    );
    ok(&mut g, "CREATE (:U {k: 1})");
    assert_eq!(status_of(&mut g, "CREATE (:U {k: 1})"), "22N79");
    assert_eq!(status_of(&mut g, "CREATE (:E)"), "22N77");
    assert_eq!(status_of(&mut g, "CREATE (:T {k: 'x'})"), "22N78");
    ok(&mut g, "CREATE (:A)-[:R]->(:B)");
    assert_eq!(status_of(&mut g, "MATCH (a:A) DELETE a"), "G1001");
}

#[test]
fn transaction_states() {
    let store = MvccStore::new(Graph::new());
    // a snapshot refuses writes
    let mut read = store.begin_read().unwrap();
    let e = execute_with_params(&mut read, "CREATE ()", &HashMap::new()).unwrap_err();
    assert_eq!(e.status(), GqlStatus::WriteInReadAccessMode);
    assert_eq!(e.status().code(), "42N02");
    // a transaction that isn't the statement's own refuses batch commits
    let mut write = store.begin_write();
    let e = execute_with_params(
        &mut write,
        "UNWIND [1] AS i CALL { WITH i CREATE (:N) } IN TRANSACTIONS",
        &HashMap::new(),
    )
    .unwrap_err();
    assert_eq!(e.status().code(), "25N17");
}

#[test]
fn every_status_has_its_neo4j_code() {
    let mut g = Graph::new();
    let e = execute_with_params(&mut g, "RETURN 1 / 0", &HashMap::new()).unwrap_err();
    assert_eq!(
        e.status().neo4j_code(),
        "Neo.ClientError.Statement.ArithmeticError"
    );
    assert_eq!(
        e.status().description(),
        "error: data exception - division by zero"
    );
    let e = execute_with_params(&mut g, "CALL no.such.procedure()", &HashMap::new()).unwrap_err();
    assert_eq!(
        e.status().neo4j_code(),
        "Neo.ClientError.Procedure.ProcedureNotFound"
    );
}
