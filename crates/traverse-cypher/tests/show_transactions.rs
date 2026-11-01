//! `SHOW TRANSACTIONS` and `TERMINATE TRANSACTIONS` over the process-wide registry
//! (`traverse_cypher::transactions`): default and full columns, id filters, chaining, who sees
//! what, and terminating a running statement from another thread. Tests run in parallel against
//! one registry, so each registers under its own database name and filters on it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use traverse_core::graph::Graph;
use traverse_cypher::error::CypherError;
use traverse_cypher::query::{Interrupt, Principal, QueryContext, QueryOptions};
use traverse_cypher::transactions::{self, Transaction, TransactionInfo};
use traverse_cypher::types::CypherValue;
use traverse_cypher::{execute, execute_with_params};

fn run(graph: &mut Graph, q: &str) -> Result<(Vec<String>, Vec<Vec<CypherValue>>), String> {
    execute(graph, q)
        .map(|r| (r.columns, r.rows))
        .map_err(|e| e.to_string())
}

fn rows(graph: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    run(graph, q).unwrap_or_else(|e| panic!("{q}: {e}")).1
}

fn s(v: &str) -> CypherValue {
    CypherValue::String(v.into())
}

/// 2,000 nodes: a three-way cross product over them (8e9 rows) runs for minutes, streaming.
fn big_graph() -> Graph {
    let mut g = Graph::new();
    execute(&mut g, "UNWIND range(1, 2000) AS i CREATE (:N {i: i})").unwrap();
    g
}

const LONG: &str = "MATCH (a:N), (b:N), (c:N) RETURN count(*)";

fn register(database: &str, user: &str) -> Transaction {
    transactions::register(TransactionInfo {
        database: database.into(),
        user: user.into(),
        protocol: "bolt",
        connection_id: "bolt-1".into(),
        client_address: "127.0.0.1:50000".into(),
        ..TransactionInfo::default()
    })
}

#[test]
fn bare_show_has_the_default_columns_and_the_caller() {
    let mut g = Graph::new();
    let (columns, rows) = run(&mut g, "SHOW TRANSACTIONS").unwrap();
    assert_eq!(columns, transactions::DEFAULT_COLUMNS);
    // the calling statement is listed even though nothing registered it
    assert!(
        rows.iter().any(
            |r| matches!(&r[1], CypherValue::String(id) if id.starts_with("transaction-"))
                && r[8] == s("Running")
        ),
        "{rows:?}"
    );
}

#[test]
fn yield_star_has_every_column() {
    let mut g = Graph::new();
    let (columns, _) = run(&mut g, "SHOW TRANSACTIONS YIELD *").unwrap();
    assert_eq!(columns, transactions::ALL_COLUMNS);
}

#[test]
fn registered_transactions_are_listed_with_their_details() {
    let tx = register("show_details", "alice");
    let query = QueryContext::new(tx.query_options(QueryOptions::default()));
    let _running = tx.run(&query, "MATCH (n) RETURN n", None);
    let mut g = Graph::new();
    let rows = rows(
        &mut g,
        "SHOW TRANSACTIONS YIELD database, transactionId, username, currentQuery, \
         currentQueryId, protocol, clientAddress, status \
         WHERE database = 'show_details'",
    );
    assert_eq!(rows.len(), 1, "{rows:?}");
    let r = &rows[0];
    assert_eq!(r[1], s(tx.display_id()));
    assert_eq!(r[2], s("alice"));
    assert_eq!(r[3], s("MATCH (n) RETURN n"));
    assert_eq!(r[4], s(&format!("query-{}", query.id())));
    assert_eq!(r[5], s("bolt"));
    assert_eq!(r[6], s("127.0.0.1:50000"));
    assert_eq!(r[7], s("Running"));
}

#[test]
fn ids_filter_the_listing() {
    let a = register("show_ids", "");
    let b = register("show_ids", "");
    let _c = register("show_ids", "");
    let mut g = Graph::new();
    let one = format!("SHOW TRANSACTIONS '{}' YIELD transactionId", a.display_id());
    assert_eq!(rows(&mut g, &one), vec![vec![s(a.display_id())]]);
    let two = format!(
        "SHOW TRANSACTIONS '{}', '{}' YIELD transactionId ORDER BY transactionId",
        a.display_id(),
        b.display_id()
    );
    assert_eq!(rows(&mut g, &two).len(), 2);
    let mut params = HashMap::new();
    params.insert(
        "ids".to_string(),
        CypherValue::List(vec![s(b.display_id())]),
    );
    let by_param = execute_with_params(
        &mut g,
        "SHOW TRANSACTIONS $ids YIELD transactionId",
        &params,
    )
    .unwrap();
    assert_eq!(by_param.rows, vec![vec![s(b.display_id())]]);
}

#[test]
fn where_and_count_compose() {
    let _a = register("show_where", "");
    let _b = register("show_where", "");
    let mut g = Graph::new();
    let n = rows(
        &mut g,
        "SHOW TRANSACTIONS YIELD database WHERE database = 'show_where' RETURN count(*) AS c",
    );
    assert_eq!(n, vec![vec![CypherValue::Integer(2)]]);
    let filtered = rows(&mut g, "SHOW TRANSACTIONS WHERE database = 'show_where'");
    assert_eq!(filtered.len(), 2);
    assert_eq!(filtered[0].len(), transactions::DEFAULT_COLUMNS.len());
}

#[test]
fn terminate_reports_and_flags() {
    let tx = register("terminate_one", "bob");
    let query = QueryContext::new(tx.query_options(QueryOptions::default()));
    let _running = tx.run(&query, "MATCH (n) RETURN n", None);
    let mut g = Graph::new();
    let (columns, out) = run(
        &mut g,
        &format!("TERMINATE TRANSACTIONS '{}'", tx.display_id()),
    )
    .unwrap();
    assert_eq!(columns, transactions::TERMINATE_COLUMNS);
    assert_eq!(
        out,
        vec![vec![
            s(tx.display_id()),
            s("bob"),
            s("Transaction terminated.")
        ]]
    );
    assert_eq!(tx.terminated(), Some(Interrupt::Terminated));
    assert_eq!(query.interrupt(), Some(Interrupt::Terminated));
    let status = rows(
        &mut g,
        &format!("SHOW TRANSACTIONS '{}' YIELD status", tx.display_id()),
    );
    assert_eq!(
        status,
        vec![vec![s(
            "Terminated with reason: explicitly terminated by the user"
        )]]
    );
}

#[test]
fn unknown_ids_are_not_found() {
    let mut g = Graph::new();
    let out = rows(
        &mut g,
        "TERMINATE TRANSACTIONS 'neo4j-transaction-999999999'",
    );
    assert_eq!(
        out,
        vec![vec![
            s("neo4j-transaction-999999999"),
            CypherValue::Null,
            s("Transaction not found.")
        ]]
    );
}

#[test]
fn show_and_terminate_chain() {
    let a = register("chain", "carol");
    let b = register("chain", "carol");
    let mut g = Graph::new();
    let out = rows(
        &mut g,
        "SHOW TRANSACTIONS YIELD transactionId AS id, database WHERE database = 'chain' \
         TERMINATE TRANSACTIONS id YIELD message \
         RETURN id, message ORDER BY id",
    );
    assert_eq!(out.len(), 2, "{out:?}");
    assert!(out.iter().all(|r| r[1] == s("Transaction terminated.")));
    assert_eq!(a.terminated(), Some(Interrupt::Terminated));
    assert_eq!(b.terminated(), Some(Interrupt::Terminated));
}

#[test]
fn users_see_and_terminate_only_their_own() {
    let theirs = register("visibility", "dave");
    let mine = register("visibility", "erin");
    let me = QueryContext::new(QueryOptions {
        principal: Some(Principal {
            user: "erin".into(),
            admin: false,
        }),
        ..QueryOptions::default()
    });
    let scope = me.enter();
    let mut g = Graph::new();
    let seen = rows(
        &mut g,
        "SHOW TRANSACTIONS YIELD transactionId, database WHERE database = 'visibility' \
         RETURN transactionId",
    );
    assert_eq!(seen, vec![vec![s(mine.display_id())]]);
    let out = rows(
        &mut g,
        &format!("TERMINATE TRANSACTIONS '{}'", theirs.display_id()),
    );
    assert_eq!(out[0][2], s("Transaction not found."));
    assert_eq!(theirs.terminated(), None);
    drop(scope);
}

#[test]
fn malformed_commands_are_rejected() {
    let mut g = Graph::new();
    let err = run(&mut g, "TERMINATE TRANSACTIONS").unwrap_err();
    assert!(err.contains("needs the ids"), "{err}");
    let err = run(&mut g, "SHOW TRANSACTIONS TERMINATE TRANSACTIONS 'x'").unwrap_err();
    assert!(err.contains("needs a YIELD"), "{err}");
    let err = run(
        &mut g,
        "SHOW TRANSACTIONS YIELD transactionId TERMINATE TRANSACTIONS transactionId",
    )
    .unwrap_err();
    assert!(err.contains("already declared"), "{err}");
    let err = run(&mut g, "TERMINATE TRANSACTIONS 42").unwrap_err();
    assert!(err.contains("STRING or LIST<STRING>"), "{err}");
}

#[test]
fn terminating_stops_a_running_statement() {
    let tx = Arc::new(register("terminate_running", "frank"));
    let id = tx.display_id().to_string();
    let worker = {
        let tx = Arc::clone(&tx);
        std::thread::spawn(move || {
            let query = QueryContext::new(tx.query_options(QueryOptions::default()));
            let _running = tx.run(&query, "long", None);
            let mut g = big_graph();
            let _scope = query.enter();
            execute(&mut g, LONG).map(|_| ())
        })
    };
    // wait for the statement to show up, then terminate it from here
    let mut g = Graph::new();
    let started = Instant::now();
    loop {
        let r = rows(
            &mut g,
            &format!("SHOW TRANSACTIONS '{id}' YIELD currentQuery"),
        );
        if r.first().is_some_and(|row| row[0] == s("long")) {
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(10), "never started");
        std::thread::sleep(Duration::from_millis(5));
    }
    let out = rows(&mut g, &format!("TERMINATE TRANSACTIONS '{id}'"));
    assert_eq!(out[0][2], s("Transaction terminated."));
    let result = worker.join().unwrap();
    assert!(
        matches!(
            result,
            Err(CypherError::Interrupted {
                reason: Interrupt::Terminated,
                ..
            })
        ),
        "{result:?}"
    );
}

#[test]
fn a_query_deadline_stops_execution() {
    let query = QueryContext::new(QueryOptions {
        timeout: Some(Duration::from_millis(50)),
        ..QueryOptions::default()
    });
    let mut g = big_graph();
    let _scope = query.enter();
    let started = Instant::now();
    let err = execute(&mut g, LONG).unwrap_err();
    assert!(
        matches!(
            err,
            CypherError::Interrupted {
                reason: Interrupt::TimedOut,
                ..
            }
        ),
        "{err}"
    );
    assert!(err.to_string().contains("timed out after 50ms"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(5));
}
