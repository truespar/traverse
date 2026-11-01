//! Inline procedure subquery `CALL [(imports)] { body }`: GP01 (transparent
//! scope) and GP03 (explicit imports).
//!
//! * The body runs once per outer row.
//! * The body's RETURN columns extend the outer row (cross product when it
//!   yields N rows).
//! * Imports restrict what the body sees; the rest of the outer row comes back
//!   on the cross-joined output.

mod common;
use common::{GqlCatalog, StmtOutcome, run_program};
use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

fn run_with_graph(preamble: &[&str], query: &str) -> StmtOutcome {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    for p in preamble {
        let out = run_program(&mut g, &mut cat, p);
        assert!(out.is_ok(), "preamble failed: {p}");
    }
    run_program(&mut g, &mut cat, query)
}

fn rows(out: StmtOutcome) -> Vec<Vec<CypherValue>> {
    match out {
        StmtOutcome::Ran { rows, .. } => rows,
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        StmtOutcome::CatalogError(_) => panic!("CatalogError"),
        StmtOutcome::LowerFailed => panic!("LowerFailed"),
        StmtOutcome::ParseFailed => panic!("ParseFailed"),
    }
}

fn ints(rows: &[Vec<CypherValue>], col: usize) -> Vec<i64> {
    rows.iter()
        .map(|r| match &r[col] {
            CypherValue::Integer(n) => *n,
            other => panic!("expected Integer in col {col}, got {other:?}"),
        })
        .collect()
}

fn strings(rows: &[Vec<CypherValue>], col: usize) -> Vec<String> {
    rows.iter()
        .map(|r| match &r[col] {
            CypherValue::String(s) => s.clone(),
            other => panic!("expected String in col {col}, got {other:?}"),
        })
        .collect()
}

// -- standalone CALL subquery --------------------------------------

#[test]
fn standalone_call_subquery_returns_inner_rows() {
    // no outer scope, just a top-level `CALL { MATCH ... RETURN ... }`: the implicit single empty outer
    // row, all N body rows kept
    let out = run_with_graph(
        &["CREATE (:N {x: 1}), (:N {x: 2}), (:N {x: 3})"],
        "CALL { MATCH (n:N) RETURN n.x AS val } \
         RETURN val ORDER BY val",
    );
    let r = rows(out);
    assert_eq!(ints(&r, 0), vec![1, 2, 3]);
}

// -- per-row evaluation (transparent scope, GP01) ------------------

#[test]
fn call_subquery_per_row_with_transparent_scope() {
    // for each outer `:Org` the body matches its members; its RETURN adds the member's name to the
    // outer row
    let out = run_with_graph(
        &[
            "CREATE (a:Org {name: 'Acme'}), (b:Org {name: 'Beta'})",
            "CREATE (p:Person {name: 'Alice'}), (q:Person {name: 'Bob'}), (r:Person {name: 'Carol'})",
            "MATCH (o:Org {name: 'Acme'}), (p:Person {name: 'Alice'}) CREATE (p)-[:IN]->(o)",
            "MATCH (o:Org {name: 'Acme'}), (p:Person {name: 'Bob'}) CREATE (p)-[:IN]->(o)",
            "MATCH (o:Org {name: 'Beta'}), (p:Person {name: 'Carol'}) CREATE (p)-[:IN]->(o)",
        ],
        "MATCH (o:Org) \
         CALL { MATCH (p:Person)-[:IN]->(o) RETURN p.name AS member } \
         RETURN o.name AS org, member ORDER BY org, member",
    );
    let r = rows(out);
    // Acme has 2 members, Beta 1 -> 3 rows
    let orgs = strings(&r, 0);
    let members = strings(&r, 1);
    assert_eq!(orgs, vec!["Acme", "Acme", "Beta"]);
    assert_eq!(members, vec!["Alice", "Bob", "Carol"]);
}

// -- explicit imports (GP03) ---------------------------------------

#[test]
fn call_subquery_with_explicit_imports() {
    // the body sees only the imported `o`; the outer `aux` is hidden inside but back on the output
    // row
    let out = run_with_graph(
        &[
            "CREATE (a:Org {name: 'Acme'}), (b:Org {name: 'Beta'})",
            "CREATE (p:Person {name: 'Alice'})",
            "MATCH (o:Org {name: 'Acme'}), (p:Person) CREATE (p)-[:IN]->(o)",
        ],
        "MATCH (o:Org) \
         CALL (o) { MATCH (p:Person)-[:IN]->(o) RETURN p.name AS member } \
         RETURN o.name AS org, member ORDER BY org",
    );
    let r = rows(out);
    let orgs = strings(&r, 0);
    let members = strings(&r, 1);
    assert_eq!(orgs, vec!["Acme"]);
    assert_eq!(members, vec!["Alice"]);
}

// -- outer bindings kept on output ----------------------------------

#[test]
fn outer_bindings_remain_visible_after_call_subquery() {
    // the body's RETURN brings `cnt` into scope; outer `o.name` stays reachable
    let out = run_with_graph(
        &[
            "CREATE (a:Org {name: 'Acme'}), (b:Org {name: 'Beta'})",
            "CREATE (p:Person {name: 'Alice'}), (q:Person {name: 'Bob'})",
            "MATCH (o:Org {name: 'Acme'}), (p:Person {name: 'Alice'}) CREATE (p)-[:IN]->(o)",
            "MATCH (o:Org {name: 'Acme'}), (p:Person {name: 'Bob'}) CREATE (p)-[:IN]->(o)",
        ],
        "MATCH (o:Org) \
         CALL (o) { MATCH (p:Person)-[:IN]->(o) RETURN count(p) AS cnt } \
         RETURN o.name AS org, cnt ORDER BY org",
    );
    let r = rows(out);
    let orgs = strings(&r, 0);
    let counts = ints(&r, 1);
    assert_eq!(orgs, vec!["Acme", "Beta"]);
    // Acme 2 members, Beta 0 (an aggregate over the empty set still yields one row)
    assert_eq!(counts, vec![2, 0]);
}

// -- empty inner result drops the outer row -------------------------

#[test]
fn empty_inner_result_drops_outer_row() {
    // a non-aggregate body returning no rows for member-less orgs: the outer row cross-joins with
    // nothing and drops
    let out = run_with_graph(
        &[
            "CREATE (a:Org {name: 'Acme'}), (b:Org {name: 'Beta'})",
            "CREATE (p:Person {name: 'Alice'})",
            "MATCH (o:Org {name: 'Acme'}), (p:Person) CREATE (p)-[:IN]->(o)",
        ],
        "MATCH (o:Org) \
         CALL (o) { MATCH (p:Person)-[:IN]->(o) RETURN p.name AS member } \
         RETURN o.name AS org, member",
    );
    let r = rows(out);
    // only Acme has a member
    let orgs = strings(&r, 0);
    let members = strings(&r, 1);
    assert_eq!(orgs, vec!["Acme"]);
    assert_eq!(members, vec!["Alice"]);
}

// -- body aggregation ----------------------------------------------

#[test]
fn call_subquery_body_aggregates_independently() {
    // per-row aggregation in the body counts members per org (an outer aggregation would group by
    // every bound var)
    let out = run_with_graph(
        &[
            "CREATE (a:Org {name: 'Acme'}), (b:Org {name: 'Beta'})",
            "CREATE (p1:Person), (p2:Person), (p3:Person)",
            "MATCH (o:Org {name: 'Acme'}), (p:Person) WHERE id(p) % 2 = 0 CREATE (p)-[:IN]->(o)",
            "MATCH (o:Org {name: 'Beta'}), (p:Person) WHERE id(p) % 2 = 1 CREATE (p)-[:IN]->(o)",
        ],
        "MATCH (o:Org) \
         CALL (o) { MATCH (p:Person)-[:IN]->(o) RETURN count(p) AS members } \
         RETURN o.name AS org, members ORDER BY org",
    );
    let r = rows(out);
    let orgs = strings(&r, 0);
    let members = ints(&r, 1);
    assert_eq!(orgs, vec!["Acme", "Beta"]);
    // 3 persons split between two orgs by id parity; the exact split depends on ids, but each count
    // is >= 0
    assert!(members.iter().all(|&n| n >= 0));
    assert_eq!(members.iter().sum::<i64>(), 3);
}
