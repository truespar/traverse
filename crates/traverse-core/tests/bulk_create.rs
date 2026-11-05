//! Bulk creation on a bulk load: the batched paths build the same graph a record at a time
//! does, commit it whole, and roll back cleanly.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::{Direction, PropertyKeyId};
use traverse_core::storage::Database;
use traverse_core::tx::GraphView;

fn temp_db(tag: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join(format!("{tag}.tvdb"));
    (dir, p)
}

fn props(i: i64) -> BTreeMap<String, PropertyValue> {
    let mut m = BTreeMap::new();
    m.insert("i".to_string(), PropertyValue::Int(i));
    m
}

#[test]
fn bulk_nodes_and_edges_commit_like_single_creations() {
    let (_dir, path) = temp_db("commit");
    let db = Arc::new(Database::open(&path).unwrap());
    let mut bulk = db.begin_bulk_load(None).unwrap();
    let first = bulk
        .bulk_nodes(&["Item"], (0..5).map(props).collect())
        .unwrap();
    let n = |k: u64| traverse_core::graph::types::NodeId(first.0 + k);
    // a chain, a second type, and a self-loop
    let e0 = bulk
        .bulk_edges(
            "NEXT",
            &[(n(0), n(1)), (n(1), n(2)), (n(2), n(3)), (n(3), n(4))],
        )
        .unwrap();
    let e1 = bulk.bulk_edges("LOOP", &[(n(2), n(2))]).unwrap();
    assert_eq!(e1.0, e0.0 + 4, "consecutive ids across calls");
    bulk.commit().unwrap();
    drop(db);

    let db = Database::open(&path).unwrap();
    let tx = db.begin_read().unwrap();
    assert_eq!(tx.node_count(), 5);
    assert_eq!(tx.edge_count(), 5);
    assert_eq!(tx.count_edges_by_type("NEXT"), 4);
    assert_eq!(tx.count_edges_by_type("LOOP"), 1);
    assert_eq!(tx.self_loops(), Some(1));
    let out = tx
        .edges_with_neighbor(n(2), Direction::Outgoing, None)
        .unwrap();
    let mut targets: Vec<u64> = out.iter().map(|e| e.neighbor.0).collect();
    targets.sort_unstable();
    assert_eq!(targets, vec![n(2).0, n(3).0]);
    assert_eq!(
        tx.edges_with_neighbor(n(0), Direction::Incoming, None)
            .unwrap()
            .len(),
        0
    );
    let key = PropertyKeyId(tx.strings().get("i").unwrap());
    assert_eq!(
        tx.get_node(n(4)).unwrap().properties.get(key),
        Some(PropertyValue::Int(4))
    );
    drop(tx);
    drop(db);
}

#[test]
fn bulk_load_abort_leaves_nothing() {
    let (_dir, path) = temp_db("abort");
    let db = Arc::new(Database::open(&path).unwrap());
    let mut bulk = db.begin_bulk_load(None).unwrap();
    let first = bulk
        .bulk_nodes(&["Item"], (0..3).map(props).collect())
        .unwrap();
    let n = |k: u64| traverse_core::graph::types::NodeId(first.0 + k);
    bulk.bulk_edges("NEXT", &[(n(0), n(1)), (n(1), n(2))])
        .unwrap();
    bulk.abort();
    let tx = db.begin_read().unwrap();
    assert_eq!(tx.node_count(), 0);
    assert_eq!(tx.edge_count(), 0);
    drop(tx);
    // and the database still takes ordinary writes afterwards, tracked for the next save
    let mut w = db.begin_write();
    let a = w.create_node(&["Item"], props(7)).unwrap();
    let b = w.create_node(&["Item"], props(8)).unwrap();
    w.create_edge(a, b, "NEXT", BTreeMap::new()).unwrap();
    w.commit().unwrap();
    db.flush().unwrap();
    drop(db);
    let db = Database::open(&path).unwrap();
    let tx = db.begin_read().unwrap();
    assert_eq!(tx.node_count(), 2);
    assert_eq!(tx.edge_count(), 1);
    drop(tx);
    drop(db);
}
