//! End-to-end smoke tests for the procedure adapter.
//!
//! A trivial "constant-1" centrality algorithm driven through the executor in
//! all three modes:
//!
//! * `traverse.constantOne.stream` yields one row per node, score 1.0
//! * `traverse.constantOne.stats` yields one row of summary stats
//! * `traverse.constantOne.write` returns one row with `nodePropertiesWritten`
//!
//! No real algorithm involved; it pins the procedure surface itself. Real
//! built-ins are covered further down.

#![cfg(test)]

use std::cell::Cell;
use std::collections::BTreeMap;

use traverse_core::graph::Graph;

use super::projection::AlgorithmGraph;
use super::result::NodeScalarResult;
use super::{
    AlgorithmError, AlgorithmMetadata, BaseConfig, CancellationToken, Category, ConfigParam,
    GraphAlgorithm, OutputKind, Stability,
};
use crate::execute;

struct ConstantOne;

impl GraphAlgorithm for ConstantOne {
    type Config = ();
    type Output = NodeScalarResult;

    fn metadata(&self) -> AlgorithmMetadata {
        AlgorithmMetadata {
            name: "constantOne",
            display_name: "Constant One",
            category: Category::Centrality,
            output_kind: OutputKind::NodeScalar,
            stability: Stability::Experimental,
            description: "Test: assigns 1.0 to every active node.",
            params: Vec::<ConfigParam>::new(),
            output_columns: OutputKind::NodeScalar.default_columns().unwrap(),
            stats_columns: OutputKind::NodeScalar.default_stats_columns().unwrap(),
            write_columns: OutputKind::NodeScalar.default_write_columns().unwrap(),
        }
    }

    fn parse_config(
        &self,
        _params: &BTreeMap<String, crate::types::CypherValue>,
    ) -> Result<(), AlgorithmError> {
        Ok(())
    }

    fn run(
        &self,
        graph: &dyn AlgorithmGraph,
        _cfg: &(),
        _cancel: &CancellationToken,
    ) -> Result<NodeScalarResult, AlgorithmError> {
        let cap = graph.highest_node_id() as usize;
        let mut values = vec![f64::NAN; cap];
        for nid in graph.active_nodes() {
            values[nid.0 as usize] = 1.0;
        }
        Ok(NodeScalarResult::new("score", values))
    }
}

thread_local! {
    /// Per-thread guard: the algorithm registry is thread-local and cargo runs tests in parallel,
    /// so each test thread registers on its own.
    static REGISTERED: Cell<bool> = const { Cell::new(false) };
}

fn ensure_registered() {
    REGISTERED.with(|r| {
        if !r.get() {
            super::procedure::register(ConstantOne);
            r.set(true);
        }
    });
    let _ = BaseConfig::default(); // touch the import
}

fn three_node_graph() -> Graph {
    let mut g = Graph::new();
    let a = g.create_node(&["Person"], Default::default());
    let b = g.create_node(&["Person"], Default::default());
    let _c = g.create_node(&["Person"], Default::default());
    g.create_edge(a, b, "KNOWS", Default::default()).unwrap();
    g
}

#[test]
fn stream_mode_yields_one_row_per_active_node() {
    ensure_registered();
    let mut g = three_node_graph();
    let r = execute(
        &mut g,
        "CALL traverse.constantOne.stream({}) YIELD nodeId, score RETURN nodeId, score",
    )
    .unwrap();
    assert_eq!(r.rows.len(), 3);
    for row in &r.rows {
        let score = match row.get(1) {
            Some(crate::types::CypherValue::Float(f)) => *f,
            other => panic!("expected float, got {:?}", other),
        };
        assert!((score - 1.0).abs() < 1e-9);
    }
}

#[test]
fn stats_mode_yields_one_summary_row() {
    ensure_registered();
    let mut g = three_node_graph();
    let r = execute(
        &mut g,
        "CALL traverse.constantOne.stats({}) YIELD nodeCount, min, max, mean, computeMillis \
         RETURN nodeCount, min, max, mean, computeMillis",
    )
    .unwrap();
    assert_eq!(r.rows.len(), 1, "expected exactly one stats row");
    assert_eq!(
        r.rows[0][0],
        crate::types::CypherValue::Integer(3),
        "nodeCount should be 3"
    );
    assert_eq!(
        r.rows[0][1],
        crate::types::CypherValue::Float(1.0),
        "min should be 1.0"
    );
    assert_eq!(
        r.rows[0][2],
        crate::types::CypherValue::Float(1.0),
        "max should be 1.0"
    );
}

#[test]
fn unknown_algorithm_name_is_a_clear_error() {
    let mut g = three_node_graph();
    let err = execute(
        &mut g,
        "CALL traverse.notARealAlgo.stream({}) YIELD nodeId RETURN nodeId",
    )
    .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("notARealAlgo") || msg.contains("ProcedureNotFound"),
        "unexpected error: {msg}"
    );
}

// -- end to end for real built-in algorithms ---------------------------

fn ensure_builtins() {
    super::register_builtins();
    let _ = BaseConfig::default();
}

#[test]
fn builtin_degree_via_cypher_call() {
    ensure_builtins();
    let mut g = traverse_core::graph::Graph::new();
    let center = g.create_node(&["N"], Default::default());
    for _ in 0..3 {
        let l = g.create_node(&["N"], Default::default());
        g.create_edge(center, l, "T", Default::default()).unwrap();
    }
    let r = execute(
        &mut g,
        "CALL traverse.degree.stream({}) YIELD nodeId, score \
         RETURN nodeId, score ORDER BY score DESC LIMIT 1",
    )
    .unwrap();
    assert_eq!(r.rows.len(), 1);
    // center is the highest-degree node (3)
    assert_eq!(
        r.rows[0][0],
        crate::types::CypherValue::Integer(center.0 as i64)
    );
    assert_eq!(r.rows[0][1], crate::types::CypherValue::Float(3.0));
}

#[test]
fn builtin_pagerank_via_cypher_call() {
    ensure_builtins();
    let mut g = traverse_core::graph::Graph::new();
    let hub = g.create_node(&["N"], Default::default());
    for _ in 0..4 {
        let l = g.create_node(&["N"], Default::default());
        g.create_edge(l, hub, "T", Default::default()).unwrap();
    }
    let r = execute(
        &mut g,
        "CALL traverse.pageRank.stream({maxIterations: 30, tolerance: 0.0000001}) \
         YIELD nodeId, score RETURN nodeId, score ORDER BY score DESC LIMIT 1",
    )
    .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(
        r.rows[0][0],
        crate::types::CypherValue::Integer(hub.0 as i64)
    );
}

#[test]
fn builtin_wcc_via_cypher_call() {
    ensure_builtins();
    let mut g = traverse_core::graph::Graph::new();
    let a = g.create_node(&["N"], Default::default());
    let b = g.create_node(&["N"], Default::default());
    let c = g.create_node(&["N"], Default::default());
    let d = g.create_node(&["N"], Default::default());
    g.create_edge(a, b, "T", Default::default()).unwrap();
    g.create_edge(c, d, "T", Default::default()).unwrap();
    let r = execute(
        &mut g,
        "CALL traverse.wcc.stream({}) YIELD nodeId, communityId \
         RETURN communityId, count(*) AS n ORDER BY communityId",
    )
    .unwrap();
    // two components of size 2
    assert_eq!(r.rows.len(), 2);
    for row in &r.rows {
        assert_eq!(row[1], crate::types::CypherValue::Integer(2));
    }
}

#[test]
fn builtin_triangle_count_via_cypher_call() {
    ensure_builtins();
    // K_3: 1 triangle, seen once by each vertex
    let mut g = traverse_core::graph::Graph::new();
    let a = g.create_node(&["N"], Default::default());
    let b = g.create_node(&["N"], Default::default());
    let c = g.create_node(&["N"], Default::default());
    g.create_edge(a, b, "T", Default::default()).unwrap();
    g.create_edge(b, c, "T", Default::default()).unwrap();
    g.create_edge(c, a, "T", Default::default()).unwrap();
    let r = execute(
        &mut g,
        "CALL traverse.triangleCount.stream({}) YIELD nodeId, triangleCount, localClusteringCoefficient \
         RETURN triangleCount, localClusteringCoefficient",
    )
    .unwrap();
    assert_eq!(r.rows.len(), 3);
    for row in &r.rows {
        assert_eq!(row[0], crate::types::CypherValue::Float(1.0));
        assert_eq!(row[1], crate::types::CypherValue::Float(1.0));
    }
}

#[test]
fn config_passes_through_label_filter() {
    ensure_registered();
    let mut g = Graph::new();
    let p1 = g.create_node(&["Person"], Default::default());
    let _c1 = g.create_node(&["Company"], Default::default());
    let _p2 = g.create_node(&["Person"], Default::default());
    g.create_edge(p1, _c1, "WORKS_AT", Default::default())
        .unwrap();

    let r = execute(
        &mut g,
        "CALL traverse.constantOne.stream({nodeLabels: ['Person']}) \
         YIELD nodeId, score RETURN nodeId, score",
    )
    .unwrap();
    assert_eq!(
        r.rows.len(),
        2,
        "label filter should keep only the 2 :Person nodes"
    );
}
