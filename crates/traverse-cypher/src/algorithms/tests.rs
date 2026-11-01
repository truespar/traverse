//! Projection-level tests against a real `Graph`, independent of any algorithm.

#![cfg(test)]

use std::collections::BTreeMap;

use traverse_core::graph::Graph;
use traverse_core::graph::property::PropertyValue;
use traverse_core::tx::view::GraphView;

use super::projection::{AlgorithmGraph, AlgorithmGraphView, WeightConfig};
use super::{BaseConfig, Orientation};

fn props(pairs: &[(&str, PropertyValue)]) -> BTreeMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn empty_props() -> BTreeMap<String, PropertyValue> {
    BTreeMap::new()
}

/// Three-node line graph a -[T]-> b -[T]-> c, all :Person.
fn line_graph() -> Graph {
    let mut g = Graph::new();
    let a = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("a".into()))]),
    );
    let b = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("b".into()))]),
    );
    let c = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("c".into()))]),
    );
    g.create_edge(a, b, "T", empty_props()).unwrap();
    g.create_edge(b, c, "T", empty_props()).unwrap();
    g
}

#[test]
fn open_projection_sees_all_nodes_and_edges() {
    let g = line_graph();
    let view: &(dyn GraphView + Sync) = &g;
    let proj = AlgorithmGraphView::open(view).expect("snapshot");

    assert_eq!(proj.node_count(), 3);
    assert_eq!(proj.edge_count(), 2);
    let nodes = proj.active_nodes();
    assert_eq!(nodes.len(), 3);
    assert_eq!(proj.out_degree(nodes[0]), 1);
    assert_eq!(proj.out_degree(nodes[1]), 1);
    assert_eq!(proj.out_degree(nodes[2]), 0);
}

#[test]
fn natural_orientation_yields_outgoing_only() {
    let g = line_graph();
    let view: &(dyn GraphView + Sync) = &g;
    let proj = AlgorithmGraphView::open(view).expect("snapshot");
    let nodes = proj.active_nodes();

    let mut seen = Vec::new();
    proj.for_each_out(nodes[0], &mut |neighbor, _eid, w| {
        seen.push((neighbor.0, w));
    });
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0, nodes[1].0);
    assert!((seen[0].1 - 1.0).abs() < 1e-9, "unit weight expected");
}

#[test]
fn reverse_orientation_swaps_in_out() {
    let g = line_graph();
    let cfg = BaseConfig {
        orientation: Orientation::Reverse,
        ..Default::default()
    };
    let view: &(dyn GraphView + Sync) = &g;
    let proj = AlgorithmGraphView::new(view, &cfg).expect("snapshot");

    let nodes = proj.active_nodes();
    // under REVERSE, a's "out" is empty (no in-edges in natural form)
    let mut a_out: Vec<u64> = Vec::new();
    proj.for_each_out(nodes[0], &mut |n, _, _| a_out.push(n.0));
    assert!(a_out.is_empty());
    // c's "out" is b (incoming T from b in natural form)
    let mut c_out: Vec<u64> = Vec::new();
    proj.for_each_out(nodes[2], &mut |n, _, _| c_out.push(n.0));
    assert_eq!(c_out, vec![nodes[1].0]);
}

#[test]
fn undirected_orientation_unions_in_and_out() {
    let g = line_graph();
    let cfg = BaseConfig {
        orientation: Orientation::Undirected,
        ..Default::default()
    };
    let view: &(dyn GraphView + Sync) = &g;
    let proj = AlgorithmGraphView::new(view, &cfg).expect("snapshot");

    let nodes = proj.active_nodes();
    let mut b_out: Vec<u64> = Vec::new();
    proj.for_each_out(nodes[1], &mut |n, _, _| b_out.push(n.0));
    b_out.sort();
    let mut expected = vec![nodes[0].0, nodes[2].0];
    expected.sort();
    assert_eq!(b_out, expected);
    assert_eq!(proj.out_degree(nodes[1]), 2);
}

#[test]
fn edge_type_filter_drops_mismatched_edges() {
    let mut g = Graph::new();
    let a = g.create_node(&["A"], empty_props());
    let b = g.create_node(&["A"], empty_props());
    g.create_edge(a, b, "TYPE_X", empty_props()).unwrap();
    g.create_edge(a, b, "TYPE_Y", empty_props()).unwrap();

    let cfg = BaseConfig {
        relationship_types: Some(vec!["TYPE_X".to_string()]),
        ..Default::default()
    };
    let view: &(dyn GraphView + Sync) = &g;
    let proj = AlgorithmGraphView::new(view, &cfg).expect("snapshot");

    assert_eq!(proj.out_degree(a), 1);
    let mut neighbors = Vec::new();
    proj.for_each_out(a, &mut |n, _, _| neighbors.push(n.0));
    assert_eq!(neighbors, vec![b.0]);
}

#[test]
fn node_label_filter_excludes_other_labels() {
    let mut g = Graph::new();
    let p1 = g.create_node(&["Person"], empty_props());
    let p2 = g.create_node(&["Person"], empty_props());
    let c1 = g.create_node(&["Company"], empty_props());
    g.create_edge(p1, p2, "KNOWS", empty_props()).unwrap();
    g.create_edge(p1, c1, "WORKS_AT", empty_props()).unwrap();

    let cfg = BaseConfig {
        node_labels: Some(vec!["Person".to_string()]),
        ..Default::default()
    };
    let view: &(dyn GraphView + Sync) = &g;
    let proj = AlgorithmGraphView::new(view, &cfg).expect("snapshot");

    assert!(proj.is_active(p1));
    assert!(proj.is_active(p2));
    assert!(!proj.is_active(c1));
    // the edge p1 -> c1 must NOT be visited: c1 is filtered out
    let mut visited = Vec::new();
    proj.for_each_out(p1, &mut |n, _, _| visited.push(n.0));
    assert_eq!(visited, vec![p2.0]);
}

#[test]
fn weight_property_resolution() {
    let mut g = Graph::new();
    let a = g.create_node(&["A"], empty_props());
    let b = g.create_node(&["A"], empty_props());
    g.create_edge(a, b, "T", props(&[("weight", PropertyValue::Float(3.5))]))
        .unwrap();

    let cfg = BaseConfig {
        relationship_weight_property: Some("weight".to_string()),
        ..Default::default()
    };
    let view: &(dyn GraphView + Sync) = &g;
    let proj = AlgorithmGraphView::new(view, &cfg).expect("snapshot");

    let mut seen_weight = 0.0;
    proj.for_each_out(a, &mut |_, _, w| seen_weight = w);
    assert!((seen_weight - 3.5).abs() < 1e-9);
    let _ = WeightConfig::Unit; // keep the import live
}

#[test]
fn unknown_label_is_an_error() {
    let g = line_graph();
    let cfg = BaseConfig {
        node_labels: Some(vec!["NotInGraph".to_string()]),
        ..Default::default()
    };
    let view: &(dyn GraphView + Sync) = &g;
    match AlgorithmGraphView::new(view, &cfg) {
        Ok(_) => panic!("expected UnknownLabel error"),
        Err(err) => assert!(format!("{err}").contains("NotInGraph")),
    }
}

#[test]
fn snapshot_guard_passes_for_clean_graph() {
    let g = line_graph();
    let view: &(dyn GraphView + Sync) = &g;
    assert!(AlgorithmGraphView::open(view).is_ok());
}
