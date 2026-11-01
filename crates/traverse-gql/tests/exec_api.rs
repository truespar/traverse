//! The embedded entry point `traverse_gql::execute_gql`, shared by the language bindings (PyO3 /
//! JNI / napi / FFI) and the server's dialect route: the full parse -> lower -> path-mode -> execute
//! pipeline on an in-memory graph.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

#[test]
fn gql_only_syntax_executes() {
    let mut g = Graph::new();
    let params = HashMap::new();
    // `||` and `IS INTEGER` are GQL-only (the openCypher parser rejects both), so success means the
    // GQL front end handled it
    let r = traverse_gql::execute_gql(
        &mut g,
        "RETURN 'foo' || 'bar' AS s, 1 IS INTEGER AS i",
        &params,
    )
    .unwrap();
    assert_eq!(r.columns, vec!["s", "i"]);
    assert_eq!(
        r.rows[0],
        vec![
            CypherValue::String("foobar".into()),
            CypherValue::Bool(true)
        ]
    );
}

#[test]
fn parameters_reach_the_engine() {
    let mut g = Graph::new();
    let mut params = HashMap::new();
    params.insert("x".to_string(), CypherValue::Integer(41));
    let r = traverse_gql::execute_gql(&mut g, "RETURN $x + 1 AS n", &params).unwrap();
    assert_eq!(r.rows[0][0], CypherValue::Integer(42));
}

#[test]
fn writes_and_reads_round_trip() {
    let mut g = Graph::new();
    let params = HashMap::new();
    traverse_gql::execute_gql(
        &mut g,
        "INSERT (:Person {name: 'Ada', age: 36}), (:Person {name: 'Bob', age: 3})",
        &params,
    )
    .unwrap();
    let r = traverse_gql::execute_gql(
        &mut g,
        "MATCH (p:Person) FILTER p.age > 10 RETURN p.name AS name",
        &params,
    )
    .unwrap();
    assert_eq!(r.rows, vec![vec![CypherValue::String("Ada".into())]]);
}

#[test]
fn parse_errors_carry_span_and_message() {
    let mut g = Graph::new();
    let params = HashMap::new();
    let err = traverse_gql::execute_gql(&mut g, "RETURN )(", &params).unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("GQL:"), "unexpected error shape: {msg}");
}

fn rows(g: &mut Graph, q: &str, params: &HashMap<String, CypherValue>) -> Vec<Vec<CypherValue>> {
    traverse_gql::execute_gql(g, q, params)
        .unwrap_or_else(|e| panic!("{q}: {e}"))
        .rows
}

fn text(s: &str) -> CypherValue {
    CypherValue::String(s.into())
}

fn node_count(g: &mut Graph) -> CypherValue {
    rows(g, "MATCH (n) RETURN count(n) AS n", &HashMap::new())[0][0].clone()
}

#[test]
fn when_runs_the_branch_its_condition_picks() {
    let mut g = Graph::new();
    let none = HashMap::new();
    let q = |cond: &str| {
        format!("WHEN {cond} THEN RETURN 'then' AS b ELSE RETURN 'else' AS b END WHEN")
    };
    assert_eq!(rows(&mut g, &q("1 < 2"), &none), vec![vec![text("then")]]);
    assert_eq!(rows(&mut g, &q("1 > 2"), &none), vec![vec![text("else")]]);
    // null is not true: ELSE
    assert_eq!(rows(&mut g, &q("null"), &none), vec![vec![text("else")]]);
    // evaluated by the engine, so functions and parameters work, not just constants
    assert_eq!(
        rows(&mut g, &q("size('ab') = 2"), &none),
        vec![vec![text("then")]]
    );
    let mut params = HashMap::new();
    params.insert("flag".to_string(), CypherValue::Bool(false));
    assert_eq!(rows(&mut g, &q("$flag"), &params), vec![vec![text("else")]]);
    // false without ELSE: nothing runs
    let r = traverse_gql::execute_gql(&mut g, "WHEN false THEN RETURN 1 AS x", &none).unwrap();
    assert!(r.columns.is_empty() && r.rows.is_empty());
}

#[test]
fn when_condition_is_a_boolean_without_bindings() {
    let mut g = Graph::new();
    let none = HashMap::new();
    let err = traverse_gql::execute_gql(&mut g, "WHEN 1 THEN RETURN 1 AS x", &none).unwrap_err();
    assert!(err.to_string().contains("BOOLEAN"), "{err}");
    // a variable has nothing to bind to
    assert!(traverse_gql::execute_gql(&mut g, "WHEN x THEN RETURN 1 AS x", &none).is_err());
}

#[test]
fn when_branch_can_write() {
    let mut g = Graph::new();
    let none = HashMap::new();
    rows(&mut g, "WHEN true THEN INSERT (:A) END WHEN", &none);
    rows(
        &mut g,
        "WHEN false THEN INSERT (:A) ELSE INSERT (:B) END WHEN",
        &none,
    );
    assert_eq!(
        rows(
            &mut g,
            "MATCH (n) RETURN labels(n)[0] AS l ORDER BY l",
            &none
        ),
        vec![vec![text("A")], vec![text("B")]]
    );
}

#[test]
fn transaction_block_runs_its_statements_in_order() {
    let mut g = Graph::new();
    let none = HashMap::new();
    let r = traverse_gql::execute_gql(
        &mut g,
        "START TRANSACTION
           INSERT (:A {v: 1});
           INSERT (:A {v: 2});
           MATCH (a:A) RETURN count(a) AS n
         COMMIT",
        &none,
    )
    .unwrap();
    // the last statement's rows, every statement's changes
    assert_eq!(r.rows, vec![vec![CypherValue::Integer(2)]]);
    assert_eq!(r.stats.nodes_created, 2);
    assert_eq!(r.stats.properties_set, 2);
}

#[test]
fn read_only_block_refuses_writes_before_running_any() {
    let mut g = Graph::new();
    let none = HashMap::new();
    let err = traverse_gql::execute_gql(
        &mut g,
        "START TRANSACTION READ ONLY
           MATCH (n) RETURN count(n) AS n;
           INSERT (:A)
         COMMIT",
        &none,
    )
    .unwrap_err();
    assert!(err.to_string().contains("READ ONLY"), "{err}");
    assert_eq!(node_count(&mut g), CypherValue::Integer(0));
    // reads are fine
    let r = rows(
        &mut g,
        "START TRANSACTION READ ONLY MATCH (n) RETURN count(n) AS n COMMIT",
        &none,
    );
    assert_eq!(r, vec![vec![CypherValue::Integer(0)]]);
}

#[test]
fn rollback_block_never_keeps_changes() {
    let mut g = Graph::new();
    let none = HashMap::new();
    // writes can't be discarded within one call, so the block is refused before anything runs
    let err = traverse_gql::execute_gql(&mut g, "START TRANSACTION INSERT (:A) ROLLBACK", &none)
        .unwrap_err();
    assert!(err.to_string().contains("ROLLBACK"), "{err}");
    assert_eq!(node_count(&mut g), CypherValue::Integer(0));
    let r = rows(
        &mut g,
        "START TRANSACTION MATCH (n) RETURN count(n) AS n ROLLBACK",
        &none,
    );
    assert_eq!(r, vec![vec![CypherValue::Integer(0)]]);
}

#[test]
fn trim_functions_follow_iso() {
    let mut g = Graph::new();
    let none = HashMap::new();
    let r = rows(
        &mut g,
        "RETURN TRIM(LEADING 'x' FROM 'xxhixx') AS l, TRIM(TRAILING 'x' FROM 'xxhixx') AS t, \
         TRIM(BOTH 'x' FROM 'xxhixx') AS b, TRIM('x' FROM 'xxhixx') AS c, \
         TRIM(FROM '  hi  ') AS d, TRIM(BOTH FROM '  hi  ') AS e, trim('  hi  ') AS f",
        &none,
    );
    assert_eq!(
        r[0],
        vec![
            text("hixx"),
            text("xxhi"),
            text("hi"),
            text("hi"),
            text("hi"),
            text("hi"),
            text("hi")
        ]
    );
    // multi-character trims strip any of the given characters; with one argument, spaces only
    let r = rows(
        &mut g,
        "RETURN btrim('xyhixy', 'yx') AS b, ltrim('xyhixy', 'xy') AS l, rtrim('xyhixy', 'xy') AS r, \
         btrim('  hi  ') AS s",
        &none,
    );
    assert_eq!(
        r[0],
        vec![text("hi"), text("hixy"), text("xyhi"), text("hi")]
    );
    // ISO: one trim character only (22027)
    let err = traverse_gql::execute_gql(&mut g, "RETURN TRIM(BOTH 'xy' FROM 'xyhi') AS x", &none)
        .unwrap_err();
    assert!(err.to_string().contains("22027"), "{err}");
    // `both` is still a name outside the TRIM syntax
    assert_eq!(
        rows(
            &mut g,
            "LET both = 'xxhixx' RETURN trim(both) AS t, btrim(both, 'x') AS u",
            &none
        )[0],
        vec![text("xxhixx"), text("hi")]
    );
}

#[test]
fn use_names_the_graph_a_host_routes_to() {
    let none = HashMap::new();
    let target = |q: &str| traverse_gql::use_target(q, &none);
    assert_eq!(target("MATCH (n) RETURN n").unwrap(), None);
    assert_eq!(
        target("USE social MATCH (n) RETURN n").unwrap().as_deref(),
        Some("social")
    );
    assert_eq!(target("USE a.b RETURN 1").unwrap().as_deref(), Some("a.b"));
    assert_eq!(
        target("USE graph.byName('people') RETURN 1")
            .unwrap()
            .as_deref(),
        Some("people")
    );
    let with_param = HashMap::from([("g".to_string(), CypherValue::String("films".into()))]);
    assert_eq!(
        traverse_gql::use_target("USE graph.byName($g) RETURN 1", &with_param)
            .unwrap()
            .as_deref(),
        Some("films")
    );
    // one graph per request
    let err = target("USE a RETURN 1 NEXT USE b RETURN 2").unwrap_err();
    assert_eq!(err.status().code(), "25G04", "{err}");
    assert_eq!(
        target("USE a RETURN 1 NEXT USE a RETURN 2")
            .unwrap()
            .as_deref(),
        Some("a")
    );
}

#[test]
fn use_runs_on_the_graph_it_names() {
    let mut g = Graph::new();
    let none = HashMap::new();
    traverse_gql::execute_gql(&mut g, "INSERT (:N {v: 1})", &none).unwrap();
    // a host that routed the request: USE is done
    let r = traverse_gql::execute_gql(&mut g, "USE whatever MATCH (n:N) RETURN n.v AS v", &none)
        .unwrap();
    assert_eq!(r.rows, vec![vec![CypherValue::Integer(1)]]);
    // a single-graph host: USE must name it
    let r = traverse_gql::execute_gql_on(
        &mut g,
        "mydb",
        "USE mydb MATCH (n:N) RETURN n.v AS v",
        &none,
    )
    .unwrap();
    assert_eq!(r.rows.len(), 1);
    let err = traverse_gql::execute_gql_on(&mut g, "mydb", "USE other MATCH (n) RETURN n", &none)
        .unwrap_err();
    assert_eq!(err.status().code(), "42NA7", "{err}");
    // USE alone does nothing
    let r = traverse_gql::execute_gql_on(&mut g, "mydb", "USE mydb", &none).unwrap();
    assert!(r.rows.is_empty() && r.columns.is_empty());
    // in a transaction block too
    let r = traverse_gql::execute_gql_on(
        &mut g,
        "mydb",
        "START TRANSACTION USE mydb MATCH (n:N) RETURN count(n) AS c COMMIT",
        &none,
    )
    .unwrap();
    assert_eq!(r.rows, vec![vec![CypherValue::Integer(1)]]);
}
