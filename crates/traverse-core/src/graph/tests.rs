use super::property::{PropertyMap, PropertyValue};
use super::types::Direction;
use super::*;
use crate::error::Error;
use std::collections::BTreeMap;

fn empty_props() -> PropertyMap {
    BTreeMap::new()
}

fn props(pairs: &[(&str, PropertyValue)]) -> PropertyMap {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

#[test]
fn fulltext_index_maintained_across_writes() {
    use crate::graph::fulltext::Analyzer;
    let mut g = Graph::new();
    let doc = |body: &str| props(&[("body", PropertyValue::String(body.to_string()))]);

    // Populate-from-existing: node created BEFORE the index.
    let n0 = g.create_node(&["Doc"], doc("an ancient scroll about foxes"));
    g.create_fulltext_index("ft", &["Doc"], &["body"], Analyzer::English)
        .unwrap();
    // Incremental maintenance: nodes created AFTER the index.
    let n1 = g.create_node(&["Doc"], doc("the quick brown fox jumps"));
    let _n2 = g.create_node(&["Doc"], doc("lazy dogs sleep all day"));

    let ids: Vec<NodeId> = g
        .query_fulltext_index("ft", "fox", 10)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert!(ids.contains(&n0), "existing node indexed");
    assert!(ids.contains(&n1), "new node incrementally indexed");
    assert_eq!(ids.len(), 2, "the dog doc must not match 'fox'");

    // Delete maintenance.
    g.delete_node(n1).unwrap();
    let after: Vec<NodeId> = g
        .query_fulltext_index("ft", "fox", 10)
        .unwrap()
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    assert!(!after.contains(&n1) && after.contains(&n0));

    // Descriptor for SHOW INDEXES.
    let desc = g.fulltext_index_descriptors();
    assert_eq!(desc.len(), 1);
    assert_eq!(desc[0].0, "ft");
    assert_eq!(desc[0].3, "english");
}

// -- Node CRUD --------------------------------------------------

#[test]
fn create_node_assigns_sequential_ids() {
    let mut g = Graph::new();
    let a = g.create_node(&["Person"], empty_props());
    let b = g.create_node(&["Person"], empty_props());
    assert_eq!(a, NodeId(0));
    assert_eq!(b, NodeId(1));
}

#[test]
fn create_node_stores_labels_and_properties() {
    let mut g = Graph::new();
    let id = g.create_node(
        &["Person", "Employee"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    let node = g.get_node(id).unwrap();
    assert_eq!(node.labels.len(), 2);
    assert_eq!(
        g.get_node_property(id, "name").unwrap().unwrap().as_str(),
        Some("Alice")
    );
}

#[test]
fn get_node_not_found() {
    let g = Graph::new();
    assert!(g.get_node(NodeId(999)).is_err());
}

#[test]
fn update_node_properties_replaces_all() {
    let mut g = Graph::new();
    let id = g.create_node(&[], props(&[("a", PropertyValue::Int(1))]));
    g.update_node_properties(id, props(&[("b", PropertyValue::Int(2))]))
        .unwrap();
    assert!(g.get_node_property(id, "a").unwrap().is_none());
    assert_eq!(
        g.get_node_property(id, "b").unwrap().unwrap().as_int(),
        Some(2)
    );
}

#[test]
fn add_label_and_remove_label() {
    let mut g = Graph::new();
    let id = g.create_node(&["Person"], empty_props());
    g.add_label(id, "Admin").unwrap();

    let node = g.get_node(id).unwrap();
    assert_eq!(node.labels.len(), 2);

    g.remove_label(id, "Person").unwrap();
    let node = g.get_node(id).unwrap();
    assert_eq!(node.labels.len(), 1);
}

#[test]
fn add_duplicate_label_fails() {
    let mut g = Graph::new();
    let id = g.create_node(&["Person"], empty_props());
    let err = g.add_label(id, "Person").unwrap_err();
    assert!(matches!(err, Error::DuplicateLabel(_, _)));
}

#[test]
fn remove_nonexistent_label_fails() {
    let mut g = Graph::new();
    let id = g.create_node(&[], empty_props());
    let err = g.remove_label(id, "Ghost").unwrap_err();
    assert!(matches!(err, Error::LabelNotFound(_, _)));
}

#[test]
fn delete_node_succeeds_when_no_edges() {
    let mut g = Graph::new();
    let id = g.create_node(&["Temp"], empty_props());
    g.delete_node(id).unwrap();
    assert!(g.get_node(id).is_err());
    assert_eq!(g.node_count(), 0);
}

#[test]
fn delete_node_fails_when_edges_exist() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    g.create_edge(a, b, "KNOWS", empty_props()).unwrap();
    let err = g.delete_node(a).unwrap_err();
    assert!(matches!(err, Error::NodeHasEdges(_)));
}

#[test]
fn delete_node_clears_label_index() {
    let mut g = Graph::new();
    let id = g.create_node(&["Person"], empty_props());
    g.delete_node(id).unwrap();
    assert!(g.nodes_by_label("Person").is_empty());
}

#[test]
fn delete_node_cascading_removes_node_and_edges() {
    let mut g = Graph::new();
    let a = g.create_node(&["Person"], empty_props());
    let b = g.create_node(&[], empty_props());
    let c = g.create_node(&[], empty_props());
    g.create_edge(a, b, "KNOWS", empty_props()).unwrap();
    g.create_edge(c, a, "FOLLOWS", empty_props()).unwrap();

    g.delete_node_cascading(a).unwrap();

    assert!(g.get_node(a).is_err());
    assert_eq!(g.edge_count(), 0);
    assert_eq!(g.node_count(), 2);
    assert!(g.nodes_by_label("Person").is_empty());
}

#[test]
fn delete_node_cascading_handles_self_loop() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    g.create_edge(a, a, "SELF", empty_props()).unwrap();

    g.delete_node_cascading(a).unwrap();

    assert!(g.get_node(a).is_err());
    assert_eq!(g.edge_count(), 0);
}

// -- Edge CRUD --------------------------------------------------

#[test]
fn create_edge_returns_sequential_ids() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    let e1 = g.create_edge(a, b, "R1", empty_props()).unwrap();
    let e2 = g.create_edge(a, b, "R2", empty_props()).unwrap();
    assert_eq!(e1, EdgeId(0));
    assert_eq!(e2, EdgeId(1));
}

#[test]
fn create_edge_with_missing_source_fails() {
    let mut g = Graph::new();
    let b = g.create_node(&[], empty_props());
    let err = g
        .create_edge(NodeId(999), b, "R", empty_props())
        .unwrap_err();
    assert!(matches!(err, Error::SourceNodeNotFound(_)));
}

#[test]
fn create_edge_with_missing_target_fails() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let err = g
        .create_edge(a, NodeId(999), "R", empty_props())
        .unwrap_err();
    assert!(matches!(err, Error::TargetNodeNotFound(_)));
}

#[test]
fn get_edge_retrieves_correct_data() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    let eid = g
        .create_edge(
            a,
            b,
            "FOLLOWS",
            props(&[("since", PropertyValue::Int(2024))]),
        )
        .unwrap();
    let edge = g.get_edge(eid).unwrap();
    assert_eq!(edge.source, a);
    assert_eq!(edge.target, b);
    assert_eq!(
        g.get_edge_property(eid, "since").unwrap().unwrap().as_int(),
        Some(2024)
    );
}

#[test]
fn delete_edge_removes_from_indexes() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    let eid = g.create_edge(a, b, "KNOWS", empty_props()).unwrap();
    g.delete_edge(eid).unwrap();
    assert!(g.get_edge(eid).is_err());
    assert_eq!(g.edge_count(), 0);
    assert_eq!(g.degree(a, Direction::Outgoing).unwrap(), 0);
}

#[test]
fn get_edge_mut_allows_property_mutation() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    let eid = g.create_edge(a, b, "KNOWS", empty_props()).unwrap();
    {
        let pid = g.intern_property_key("weight");
        let edge = g.get_edge_mut(eid).unwrap();
        edge.properties.insert(pid, PropertyValue::Float(0.9));
    }
    assert_eq!(
        g.get_edge_property(eid, "weight")
            .unwrap()
            .unwrap()
            .as_float(),
        Some(0.9)
    );
}

#[test]
fn update_edge_properties_replaces_all() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    let eid = g
        .create_edge(a, b, "KNOWS", props(&[("a", PropertyValue::Int(1))]))
        .unwrap();
    g.update_edge_properties(eid, props(&[("b", PropertyValue::Int(2))]))
        .unwrap();
    assert!(g.get_edge_property(eid, "a").unwrap().is_none());
    assert_eq!(
        g.get_edge_property(eid, "b").unwrap().unwrap().as_int(),
        Some(2)
    );
}

#[test]
fn self_loop_allowed() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let eid = g.create_edge(a, a, "SELF", empty_props()).unwrap();
    assert!(g.get_edge(eid).is_ok());
    // A self-loop appears in both outgoing and incoming.
    assert_eq!(g.degree(a, Direction::Both).unwrap(), 2);
}

#[test]
fn multi_edges_allowed() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    g.create_edge(a, b, "KNOWS", empty_props()).unwrap();
    g.create_edge(a, b, "FOLLOWS", empty_props()).unwrap();
    assert_eq!(g.degree(a, Direction::Outgoing).unwrap(), 2);
}

// -- Traversal --------------------------------------------------

#[test]
fn neighbors_outgoing() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    let c = g.create_node(&[], empty_props());
    g.create_edge(a, b, "KNOWS", empty_props()).unwrap();
    g.create_edge(a, c, "KNOWS", empty_props()).unwrap();

    let out = g.neighbors(a, Direction::Outgoing, None).unwrap();
    assert_eq!(out.len(), 2);
    assert!(out.contains(&b));
    assert!(out.contains(&c));
}

#[test]
fn neighbors_incoming() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    g.create_edge(a, b, "KNOWS", empty_props()).unwrap();

    let inc = g.neighbors(b, Direction::Incoming, None).unwrap();
    assert_eq!(inc, vec![a]);
}

#[test]
fn neighbors_with_edge_type_filter() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    let c = g.create_node(&[], empty_props());
    g.create_edge(a, b, "KNOWS", empty_props()).unwrap();
    g.create_edge(a, c, "FOLLOWS", empty_props()).unwrap();

    let knows = g.neighbors(a, Direction::Outgoing, Some("KNOWS")).unwrap();
    assert_eq!(knows, vec![b]);
}

#[test]
fn neighbors_filter_unknown_type_returns_empty() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    g.create_edge(a, b, "KNOWS", empty_props()).unwrap();

    let none = g
        .neighbors(a, Direction::Outgoing, Some("NONEXISTENT"))
        .unwrap();
    assert!(none.is_empty());
}

#[test]
fn edges_of_returns_correct_edges() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    let eid = g.create_edge(a, b, "KNOWS", empty_props()).unwrap();

    let edges = g.edges_of(a, Direction::Outgoing, None).unwrap();
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].id, eid);
}

#[test]
fn degree_counts_correctly() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    let c = g.create_node(&[], empty_props());
    g.create_edge(a, b, "R", empty_props()).unwrap();
    g.create_edge(a, c, "R", empty_props()).unwrap();
    g.create_edge(b, a, "R", empty_props()).unwrap();

    assert_eq!(g.degree(a, Direction::Outgoing).unwrap(), 2);
    assert_eq!(g.degree(a, Direction::Incoming).unwrap(), 1);
    assert_eq!(g.degree(a, Direction::Both).unwrap(), 3);
}

// -- Queries ----------------------------------------------------

#[test]
fn node_and_edge_count() {
    let mut g = Graph::new();
    assert_eq!(g.node_count(), 0);
    assert_eq!(g.edge_count(), 0);

    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    g.create_edge(a, b, "R", empty_props()).unwrap();

    assert_eq!(g.node_count(), 2);
    assert_eq!(g.edge_count(), 1);
}

#[test]
fn nodes_by_label_returns_matching() {
    let mut g = Graph::new();
    g.create_node(&["Person"], empty_props());
    g.create_node(&["Person"], empty_props());
    g.create_node(&["Movie"], empty_props());

    assert_eq!(g.nodes_by_label("Person").len(), 2);
    assert_eq!(g.nodes_by_label("Movie").len(), 1);
    assert!(g.nodes_by_label("Ghost").is_empty());
}

// -- Reconstruction ---------------------------------------------

#[test]
fn from_parts_roundtrip_preserves_graph() {
    let mut g = Graph::new();
    let alice = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    let bob = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Bob".into()))]),
    );
    let movie = g.create_node(&["Movie"], empty_props());
    g.create_edge(alice, bob, "KNOWS", empty_props()).unwrap();
    g.create_edge(alice, movie, "WATCHED", empty_props())
        .unwrap();

    let parts = g.into_parts();
    let g2 = Graph::from_parts(parts);

    assert_eq!(g2.node_count(), 3);
    assert_eq!(g2.edge_count(), 2);
    assert_eq!(g2.nodes_by_label("Person").len(), 2);
    assert_eq!(g2.nodes_by_label("Movie").len(), 1);

    let alice_knows = g2
        .neighbors(alice, Direction::Outgoing, Some("KNOWS"))
        .unwrap();
    assert_eq!(alice_knows, vec![bob]);

    let alice_watched = g2
        .neighbors(alice, Direction::Outgoing, Some("WATCHED"))
        .unwrap();
    assert_eq!(alice_watched, vec![movie]);

    assert_eq!(
        g2.get_node_property(alice, "name")
            .unwrap()
            .unwrap()
            .as_str(),
        Some("Alice")
    );
}

#[test]
fn from_parts_preserves_id_counters() {
    let mut g = Graph::new();
    g.create_node(&[], empty_props());
    g.create_node(&[], empty_props());

    let parts = g.into_parts();
    let mut g2 = Graph::from_parts(parts);

    // next ID continues from the original
    let next = g2.create_node(&[], empty_props());
    assert_eq!(next, NodeId(2));
}

#[test]
fn as_parts_does_not_consume_graph() {
    let mut g = Graph::new();
    g.create_node(&["Person"], empty_props());
    let parts = g.as_parts();
    // Original graph is still usable
    assert_eq!(g.node_count(), 1);
    // Parts are valid
    assert_eq!(parts.nodes.len(), 1);
}

// -- Dirty tracking ---------------------------------------------

#[test]
fn create_node_marks_dirty() {
    let mut g = Graph::new();
    let id = g.create_node(&["Person"], empty_props());
    assert!(g.has_changes());
    assert!(g.dirty_nodes.contains(&id));
    assert!(g.strings_dirty);
}

#[test]
fn create_node_no_labels_strings_not_dirty() {
    let mut g = Graph::new();
    g.create_node(&[], empty_props());
    assert!(g.has_changes());
    assert!(!g.strings_dirty);
}

#[test]
fn update_node_properties_marks_dirty() {
    let mut g = Graph::new();
    let id = g.create_node(&[], empty_props());
    g.clear_dirty();
    g.update_node_properties(id, props(&[("x", PropertyValue::Int(1))]))
        .unwrap();
    assert!(g.dirty_nodes.contains(&id));
}

#[test]
fn get_node_mut_marks_dirty() {
    let mut g = Graph::new();
    let id = g.create_node(&[], empty_props());
    g.clear_dirty();
    let _ = g.get_node_mut(id).unwrap();
    assert!(g.dirty_nodes.contains(&id));
}

#[test]
fn delete_node_marks_deleted_removes_dirty() {
    let mut g = Graph::new();
    let id = g.create_node(&[], empty_props());
    assert!(g.dirty_nodes.contains(&id));
    g.delete_node(id).unwrap();
    assert!(!g.dirty_nodes.contains(&id));
    assert!(g.deleted_nodes.contains(&id));
}

#[test]
fn create_edge_marks_dirty() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    g.clear_dirty();
    let eid = g.create_edge(a, b, "KNOWS", empty_props()).unwrap();
    assert!(g.dirty_edges.contains(&eid));
    assert!(g.strings_dirty);
}

#[test]
fn delete_edge_marks_deleted_removes_dirty() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    let eid = g.create_edge(a, b, "KNOWS", empty_props()).unwrap();
    assert!(g.dirty_edges.contains(&eid));
    g.delete_edge(eid).unwrap();
    assert!(!g.dirty_edges.contains(&eid));
    assert!(g.deleted_edges.contains(&eid));
}

#[test]
fn get_edge_mut_marks_dirty() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    let eid = g.create_edge(a, b, "R", empty_props()).unwrap();
    g.clear_dirty();
    let _ = g.get_edge_mut(eid).unwrap();
    assert!(g.dirty_edges.contains(&eid));
}

#[test]
fn cascading_delete_tracks_all() {
    let mut g = Graph::new();
    let a = g.create_node(&[], empty_props());
    let b = g.create_node(&[], empty_props());
    let e1 = g.create_edge(a, b, "R", empty_props()).unwrap();
    let e2 = g.create_edge(b, a, "R", empty_props()).unwrap();
    g.clear_dirty();
    g.delete_node_cascading(a).unwrap();
    assert!(g.deleted_nodes.contains(&a));
    assert!(g.deleted_edges.contains(&e1));
    assert!(g.deleted_edges.contains(&e2));
}

#[test]
fn take_dirty_clears_state() {
    let mut g = Graph::new();
    g.create_node(&["Person"], empty_props());
    let dirty = g.take_dirty();
    assert!(!dirty.is_empty());
    assert!(!g.has_changes());
}

#[test]
fn from_parts_starts_clean() {
    let mut g = Graph::new();
    g.create_node(&["Person"], empty_props());
    let parts = g.into_parts();
    let g2 = Graph::from_parts(parts);
    assert!(!g2.has_changes());
}

// -- Integration ------------------------------------------------

#[test]
fn social_graph_scenario() {
    let mut g = Graph::new();

    // Create people
    let alice = g.create_node(
        &["Person"],
        props(&[
            ("name", PropertyValue::String("Alice".into())),
            ("age", PropertyValue::Int(30)),
        ]),
    );
    let bob = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Bob".into()))]),
    );
    let charlie = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Charlie".into()))]),
    );
    let movie = g.create_node(
        &["Movie"],
        props(&[("title", PropertyValue::String("GraphDB: The Movie".into()))]),
    );

    // Create relationships
    g.create_edge(
        alice,
        bob,
        "KNOWS",
        props(&[("since", PropertyValue::Int(2020))]),
    )
    .unwrap();
    g.create_edge(alice, charlie, "KNOWS", empty_props())
        .unwrap();
    g.create_edge(bob, charlie, "KNOWS", empty_props()).unwrap();
    g.create_edge(alice, movie, "WATCHED", empty_props())
        .unwrap();
    g.create_edge(bob, movie, "WATCHED", empty_props()).unwrap();

    // Verify counts
    assert_eq!(g.node_count(), 4);
    assert_eq!(g.edge_count(), 5);

    // Alice knows 2 people
    let alice_knows = g
        .neighbors(alice, Direction::Outgoing, Some("KNOWS"))
        .unwrap();
    assert_eq!(alice_knows.len(), 2);

    // Alice watched 1 movie
    let alice_watched = g
        .neighbors(alice, Direction::Outgoing, Some("WATCHED"))
        .unwrap();
    assert_eq!(alice_watched.len(), 1);
    assert_eq!(alice_watched[0], movie);

    // Movie has 2 incoming WATCHED edges
    let watchers = g
        .neighbors(movie, Direction::Incoming, Some("WATCHED"))
        .unwrap();
    assert_eq!(watchers.len(), 2);

    // Delete an edge and verify
    let alice_edges = g
        .edges_of(alice, Direction::Outgoing, Some("KNOWS"))
        .unwrap();
    let first_edge_id = alice_edges[0].id;
    g.delete_edge(first_edge_id).unwrap();
    assert_eq!(g.edge_count(), 4);

    // Label queries
    assert_eq!(g.nodes_by_label("Person").len(), 3);
    assert_eq!(g.nodes_by_label("Movie").len(), 1);

    // String table was populated
    assert!(g.strings().get("Person").is_some());
    assert!(g.strings().get("KNOWS").is_some());
}
