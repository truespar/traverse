//! `CALL { ... } IN TRANSACTIONS` (batched-write subquery): the body runs per input row through the
//! write-capable pipeline, unlike the read-only `exec_subquery`.

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

fn count(graph: &mut Graph, pat: &str) -> i64 {
    match run(graph, &format!("MATCH {pat} RETURN count(*) AS c")).expect("count")[0][0] {
        CypherValue::Integer(n) => n,
        ref o => panic!("expected int, got {o:?}"),
    }
}

#[test]
fn bulk_create() {
    let mut g = Graph::new();
    run(
        &mut g,
        "UNWIND range(1, 5) AS i CALL { WITH i CREATE (:N {id: i}) } IN TRANSACTIONS",
    )
    .expect("bulk create");
    assert_eq!(count(&mut g, "(n:N)"), 5);
}

#[test]
fn bulk_create_with_batch_size() {
    let mut g = Graph::new();
    run(
        &mut g,
        "UNWIND range(1, 10) AS i CALL { WITH i CREATE (:N {id: i}) } IN TRANSACTIONS OF 3 ROWS",
    )
    .expect("batched");
    assert_eq!(count(&mut g, "(n:N)"), 10);
}

#[test]
fn bulk_merge_upsert() {
    let mut g = Graph::new();
    // duplicate ids upsert to one node each
    run(
        &mut g,
        "UNWIND [1, 1, 2, 3, 3] AS i CALL { WITH i MERGE (:N {id: i}) } IN TRANSACTIONS OF 2 ROWS",
    )
    .expect("merge");
    assert_eq!(count(&mut g, "(n:N)"), 3);
}

#[test]
fn match_and_set_body() {
    // a body with a MATCH (multi-leaf plan) must still seed correctly
    let mut g = Graph::new();
    run(&mut g, "UNWIND range(1, 4) AS i CREATE (:N {id: i})").expect("seed");
    run(
        &mut g,
        "UNWIND [2, 4] AS i CALL { WITH i MATCH (n:N {id: i}) SET n.seen = true } IN TRANSACTIONS",
    )
    .expect("match+set");
    assert_eq!(count(&mut g, "(n:N {seen: true})"), 2);
}

#[test]
fn returning_subquery() {
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "UNWIND range(1, 3) AS i CALL { WITH i CREATE (n:N {id: i}) RETURN n.id AS id } \
         IN TRANSACTIONS RETURN id ORDER BY id",
    )
    .expect("returning");
    let ids: Vec<i64> = rows
        .iter()
        .map(|r| match r[0] {
            CypherValue::Integer(n) => n,
            ref o => panic!("{o:?}"),
        })
        .collect();
    assert_eq!(ids, vec![1, 2, 3]);
    assert_eq!(count(&mut g, "(n:N)"), 3);
}

#[test]
fn on_error_continue_skips_bad_rows() {
    // division by zero errors on i=0; CONTINUE skips it and goes on
    let mut g = Graph::new();
    run(
        &mut g,
        "UNWIND [1, 0, 2] AS i CALL { WITH i CREATE (:N {v: 10 / i}) } \
         IN TRANSACTIONS OF 1 ROW ON ERROR CONTINUE",
    )
    .expect("on error continue");
    // i=1 and i=2 succeed, i=0 skipped
    assert_eq!(count(&mut g, "(n:N)"), 2);
}

#[test]
fn on_error_fail_propagates() {
    let mut g = Graph::new();
    let err = run(
        &mut g,
        "UNWIND [1, 0] AS i CALL { WITH i CREATE (:N {v: 10 / i}) } IN TRANSACTIONS",
    )
    .expect_err("should fail on divide-by-zero");
    assert!(!err.is_empty());
}

#[test]
fn report_status() {
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "UNWIND range(1, 4) AS i CALL { WITH i CREATE (:N {id: i}) } \
         IN TRANSACTIONS OF 2 ROWS REPORT STATUS AS s RETURN s",
    )
    .expect("report status");
    // 4 rows / batch 2 = 2 batches = 2 status rows
    assert_eq!(rows.len(), 2);
    for r in &rows {
        match &r[0] {
            CypherValue::Map(m) => {
                assert_eq!(m.get("committed"), Some(&CypherValue::Bool(true)));
            }
            o => panic!("expected status map, got {o:?}"),
        }
    }
    assert_eq!(count(&mut g, "(n:N)"), 4);
}

#[test]
fn plain_call_subquery_still_works() {
    // CALL {} without IN TRANSACTIONS is unaffected
    let mut g = Graph::new();
    run(&mut g, "CREATE (:P {age: 30}), (:P {age: 40})").expect("seed");
    let rows = run(
        &mut g,
        "MATCH (p:P) CALL { WITH p RETURN p.age AS a } RETURN a ORDER BY a",
    )
    .expect("plain call");
    assert_eq!(rows.len(), 2);
}

// -- batches as transactions of their own (an auto-commit statement's write transaction) --

/// Runs `q` as an auto-commit statement: a write transaction that may commit batches, committed
/// when the statement succeeds and rolled back when it fails, as the server runs it.
fn auto_commit(
    store: &traverse_core::tx::MvccStore,
    q: &str,
) -> Result<Vec<Vec<CypherValue>>, String> {
    let mut tx = store.begin_write();
    tx.allow_batch_commits();
    match execute_with_params(&mut tx, q, &HashMap::new()) {
        Ok(res) => {
            tx.commit().map_err(|e| e.to_string())?;
            Ok(res.rows)
        }
        Err(e) => {
            tx.abort();
            Err(e.to_string())
        }
    }
}

fn store_count(store: &traverse_core::tx::MvccStore, q: &str) -> i64 {
    let mut read = store.begin_read().unwrap();
    match execute_with_params(&mut read, q, &HashMap::new())
        .unwrap()
        .rows[0][0]
    {
        CypherValue::Integer(n) => n,
        ref o => panic!("expected int, got {o:?}"),
    }
}

#[test]
fn a_failed_batch_leaves_nothing_behind() {
    let store = traverse_core::tx::MvccStore::new(Graph::new());
    // batches of 2: [1, 2] commits, [0, 3] fails on 10 / 0 after creating the node for... nothing
    // (0 comes first) and [4, 0] fails after creating the node for 4, which must roll back
    let rows = auto_commit(
        &store,
        "UNWIND [1, 2, 0, 3, 4, 0] AS i CALL { WITH i CREATE (:N {v: 10 / i}) } \
         IN TRANSACTIONS OF 2 ROWS ON ERROR CONTINUE REPORT STATUS AS s \
         RETURN s.committed AS committed",
    )
    .expect("on error continue");
    assert_eq!(
        rows,
        vec![
            vec![CypherValue::Bool(true)],
            vec![CypherValue::Bool(false)],
            vec![CypherValue::Bool(false)],
        ]
    );
    // only the committed batch's nodes exist: 4's node rolled back with its batch
    assert_eq!(store_count(&store, "MATCH (n:N) RETURN count(n)"), 2);
}

#[test]
fn committed_batches_stay_when_a_later_one_fails_the_statement() {
    let store = traverse_core::tx::MvccStore::new(Graph::new());
    let err = auto_commit(
        &store,
        "UNWIND [1, 2, 3, 0] AS i CALL { WITH i CREATE (:N {v: 10 / i}) } \
         IN TRANSACTIONS OF 2 ROWS",
    )
    .expect_err("ON ERROR FAIL fails the statement");
    assert!(!err.is_empty());
    // the first batch committed on its own; the failing one rolled back with the statement
    assert_eq!(store_count(&store, "MATCH (n:N) RETURN count(n)"), 2);
}

#[test]
fn a_failed_batch_returns_no_rows() {
    let store = traverse_core::tx::MvccStore::new(Graph::new());
    let rows = auto_commit(
        &store,
        "UNWIND [1, 0, 2] AS i CALL { WITH i CREATE (n:N {v: 10 / i}) RETURN n.v AS v } \
         IN TRANSACTIONS OF 1 ROW ON ERROR CONTINUE RETURN v",
    )
    .expect("on error continue");
    assert_eq!(
        rows,
        vec![
            vec![CypherValue::Integer(10)],
            vec![CypherValue::Integer(5)]
        ]
    );
}

#[test]
fn explicit_transactions_refuse_batches() {
    let store = std::sync::Arc::new(traverse_core::tx::MvccStore::new(Graph::new()));
    // a client's write transaction (no batch commits)
    let mut tx = store.begin_write();
    let err = execute_with_params(
        &mut tx,
        "UNWIND [1] AS i CALL { WITH i CREATE (:N) } IN TRANSACTIONS",
        &HashMap::new(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("auto-commit"), "{err}");
    tx.abort();
    // an optimistic one too
    let mut otx = store.begin_optimistic(false).unwrap();
    let err = execute_with_params(
        &mut otx,
        "UNWIND [1] AS i CALL { WITH i CREATE (:N) } IN TRANSACTIONS",
        &HashMap::new(),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("auto-commit"), "{err}");
}

#[test]
fn batches_cant_follow_writes_in_the_same_statement() {
    let store = traverse_core::tx::MvccStore::new(Graph::new());
    let err = auto_commit(
        &store,
        "CREATE (:Before) WITH 1 AS x UNWIND [1, 2] AS i \
         CALL { WITH i CREATE (:N) } IN TRANSACTIONS",
    )
    .unwrap_err();
    assert!(err.contains("can't follow writes"), "{err}");
    assert_eq!(store_count(&store, "MATCH (n) RETURN count(n)"), 0);
}

#[test]
fn read_transactions_run_batches_without_commits() {
    let store = traverse_core::tx::MvccStore::new(Graph::new());
    auto_commit(&store, "UNWIND range(1, 3) AS i CREATE (:N {i: i})").unwrap();
    let mut read = store.begin_read().unwrap();
    let rows = execute_with_params(
        &mut read,
        "UNWIND [1, 2, 3] AS x CALL { WITH x MATCH (n:N {i: x}) RETURN n.i AS i } \
         IN TRANSACTIONS OF 2 ROWS RETURN sum(i) AS total",
        &HashMap::new(),
    )
    .unwrap()
    .rows;
    assert_eq!(rows, vec![vec![CypherValue::Integer(6)]]);
}
