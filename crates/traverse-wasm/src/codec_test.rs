//! Round-trip test for the bytes-only tvdb codec.
//!
//! These tests run on native - they exercise the same code path the
//! wasm-bindgen `exportTvdb` / `loadTvdb` methods use, so a passing
//! native test is strong evidence the WASM behavior is correct
//! (modulo the wasm-bindgen marshalling, which is trivial JSON).
//!
//! The tvdb byte format the WASM crate produces is byte-for-byte
//! compatible with what `Database::save` writes on disk, since both
//! go through `traverse_core::storage::codec::write_graph`.

#![cfg(test)]

use traverse_core::graph::Graph;
use traverse_core::graph::property::{PropertyMap, PropertyValue};
use traverse_core::storage::codec;
use traverse_core::storage::page_manager::PageManager;

fn props(items: &[(&str, PropertyValue)]) -> PropertyMap {
    let mut m = PropertyMap::new();
    for (k, v) in items {
        m.insert((*k).to_string(), v.clone());
    }
    m
}

/// Build a small graph, serialize to tvdb bytes via the codec, then
/// deserialize back and assert the structure survived intact.
///
/// This is the exact round-trip a WASM tab does: in-memory Graph ->
/// `exportTvdb()` -> bytes (e.g. saved to OPFS) -> `loadTvdb(bytes)` ->
/// in-memory Graph.
#[test]
fn tvdb_round_trip_preserves_graph_structure() {
    // Build the source graph.
    let mut g = Graph::new();
    let alice = g.create_node(
        &["Person"],
        props(&[
            ("name", PropertyValue::String("Alice".into())),
            ("age", PropertyValue::Int(30)),
            ("active", PropertyValue::Bool(true)),
            ("rating", PropertyValue::Float(4.2)),
        ]),
    );
    let bob = g.create_node(&["Person", "Admin"], props(&[]));
    g.create_edge(
        alice,
        bob,
        "KNOWS",
        props(&[("since", PropertyValue::Int(2020))]),
    )
    .unwrap();

    // Serialize -> bytes.
    let mut pm_out = PageManager::create_in_memory();
    codec::write_graph(&mut pm_out, &g, &[]).expect("write_graph");
    let bytes = pm_out.to_bytes();
    assert!(!bytes.is_empty(), "tvdb output is empty");
    // tvdb magic: first 4 bytes should be "TVDB"
    assert_eq!(&bytes[..4], b"TVDB", "tvdb header magic mismatch");

    // Bytes -> Graph (the WASM load path).
    let pm_in = PageManager::open_from_bytes(bytes).expect("open_from_bytes");
    let (loaded, _locations, _pm, _metadata) = codec::load_graph(pm_in).expect("load_graph");

    // Structural checks.
    assert_eq!(loaded.node_count(), 2, "node count");
    assert_eq!(loaded.edge_count(), 1, "edge count");

    // Property fidelity - find Alice by name and check her props.
    let alice_reloaded = loaded
        .nodes()
        .find(|n| {
            let p = loaded.resolve_property_map(&n.properties);
            matches!(p.get("name"), Some(PropertyValue::String(s)) if s == "Alice")
        })
        .expect("alice should be present");
    let alice_props = loaded.resolve_property_map(&alice_reloaded.properties);
    assert_eq!(alice_props.get("age"), Some(&PropertyValue::Int(30)));
    assert_eq!(alice_props.get("active"), Some(&PropertyValue::Bool(true)));
    match alice_props.get("rating") {
        Some(PropertyValue::Float(f)) => assert!((*f - 4.2).abs() < 1e-9),
        other => panic!("rating not a float: {other:?}"),
    }
}

#[test]
fn tvdb_round_trip_handles_complex_property_types() {
    let mut g = Graph::new();
    let mut nested = std::collections::BTreeMap::new();
    nested.insert("k".to_string(), PropertyValue::Int(7));
    nested.insert("v".to_string(), PropertyValue::String("nested".into()));

    let _ = g.create_node(
        &["N"],
        props(&[
            (
                "list",
                PropertyValue::List(vec![
                    PropertyValue::String("a".into()),
                    PropertyValue::Int(1),
                    PropertyValue::Bool(false),
                ]),
            ),
            ("map", PropertyValue::Map(nested)),
            ("when", PropertyValue::DateTime(1_700_000_000_000)),
            ("dur", PropertyValue::Duration(5_000)),
        ]),
    );

    let mut pm_out = PageManager::create_in_memory();
    codec::write_graph(&mut pm_out, &g, &[]).expect("write_graph");
    let bytes = pm_out.to_bytes();

    let pm_in = PageManager::open_from_bytes(bytes).expect("open_from_bytes");
    let (loaded, _, _, _) = codec::load_graph(pm_in).expect("load_graph");

    assert_eq!(loaded.node_count(), 1);
    let n = loaded.nodes().next().unwrap();
    let p = loaded.resolve_property_map(&n.properties);
    assert!(matches!(p.get("list"), Some(PropertyValue::List(items)) if items.len() == 3));
    assert!(matches!(p.get("map"), Some(PropertyValue::Map(_))));
    assert_eq!(
        p.get("when"),
        Some(&PropertyValue::DateTime(1_700_000_000_000))
    );
    assert_eq!(p.get("dur"), Some(&PropertyValue::Duration(5_000)));
}

#[test]
fn tvdb_metadata_round_trip() {
    let g = Graph::new();
    let metadata = b"auth catalog v1: alice = admin\n";

    let mut pm_out = PageManager::create_in_memory();
    codec::write_graph(&mut pm_out, &g, metadata).expect("write_graph");
    let bytes = pm_out.to_bytes();

    let pm_in = PageManager::open_from_bytes(bytes).expect("open_from_bytes");
    let (_, _, _, recovered) = codec::load_graph(pm_in).expect("load_graph");
    assert_eq!(&recovered[..], metadata);
}
