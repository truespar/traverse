//! Bulk loads: no redo records, durable through the save their commit makes, rolled back on
//! abort or a failed save.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use traverse_core::graph::property::PropertyValue;
use traverse_core::storage::Database;
use traverse_core::storage::rlog::{WriterOptions, scan_segments};
use traverse_core::tx::GraphView;

fn load(tx: &mut dyn GraphView, n: i64) {
    let mut prev = None;
    for i in 0..n {
        let mut props = BTreeMap::new();
        props.insert("i".to_string(), PropertyValue::Int(i));
        let id = tx.create_node(&["Item"], props).unwrap();
        if let Some(p) = prev {
            tx.create_edge(p, id, "NEXT", BTreeMap::new()).unwrap();
        }
        prev = Some(id);
    }
    tx.create_index("item_i", "Item", "i").unwrap();
}

fn logged_transactions(path: &Path) -> usize {
    let options = WriterOptions::from_tvdb_path(path).unwrap();
    scan_segments(&options.dir, &options.stem, 0)
        .unwrap()
        .transactions
        .len()
}

/// Drops the database without saving, like a crash.
fn crash(db: Arc<Database>) {
    let mut db = Arc::try_unwrap(db).expect("last handle");
    db.set_suppress_flush(true);
    drop(db);
}

#[test]
fn bulk_commit_is_durable_without_redo_records() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.tvdb");
    let db = Arc::new(Database::open(&path).unwrap());

    let mut bulk = db.begin_bulk_load(None).unwrap();
    load(&mut bulk, 1_000);
    bulk.commit().unwrap();
    assert_eq!(logged_transactions(&path), 0, "a bulk load writes no redo");
    // visible at once, synchronous WAL mode included
    assert_eq!(db.begin_read().unwrap().node_count(), 1_000);

    // an ordinary commit after it is logged and replays on top of the bulk load's save
    let mut tx = db.begin_write();
    let a = tx.create_node(&["After"], BTreeMap::new()).unwrap();
    tx.create_edge(
        a,
        traverse_core::graph::types::NodeId(0),
        "TO",
        BTreeMap::new(),
    )
    .unwrap();
    tx.commit().unwrap();
    assert_eq!(logged_transactions(&path), 1);
    crash(db);

    let db = Database::open(&path).unwrap();
    let g = db.graph();
    assert_eq!(g.node_count(), 1_001);
    assert_eq!(g.edge_count(), 1_000);
    assert!(g.index_registry().catalog().has_index_by_name("item_i"));
}

#[test]
fn bulk_abort_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.tvdb");
    let db = Arc::new(Database::open(&path).unwrap());
    let mut bulk = db.begin_bulk_load(None).unwrap();
    load(&mut bulk, 100);
    bulk.abort();
    assert_eq!(db.begin_read().unwrap().node_count(), 0);
    // dropping one rolls back too
    let mut bulk = db.begin_bulk_load(None).unwrap();
    load(&mut bulk, 100);
    drop(bulk);
    assert_eq!(db.begin_read().unwrap().node_count(), 0);
    drop(db);
    assert_eq!(Database::open(&path).unwrap().graph().node_count(), 0);
}

#[test]
fn a_failed_bulk_save_rolls_the_load_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.tvdb");
    let db = Arc::new(Database::open(&path).unwrap());
    // the first save of a new database writes `t.tvdb.tmp`: a directory there makes it fail
    let blocker = dir.path().join("t.tvdb.tmp");
    std::fs::create_dir(&blocker).unwrap();

    let mut bulk = db.begin_bulk_load(None).unwrap();
    load(&mut bulk, 100);
    assert!(bulk.commit().is_err());
    assert_eq!(
        db.begin_read().unwrap().node_count(),
        0,
        "nothing published"
    );

    std::fs::remove_dir(&blocker).unwrap();
    let mut tx = db.begin_write();
    tx.create_node(&["Kept"], BTreeMap::new()).unwrap();
    tx.commit().unwrap();
    db.flush().unwrap();
    crash(db);
    assert_eq!(Database::open(&path).unwrap().graph().node_count(), 1);
}

#[test]
fn bulk_load_in_snapshot_mode_is_durable_at_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.tvdb");
    let db = Arc::new(Database::open_with_durability(&path, false).unwrap());
    let mut bulk = db.begin_bulk_load(None).unwrap();
    load(&mut bulk, 500);
    bulk.commit().unwrap();
    // later changes are in memory only in this mode; a crash keeps exactly the bulk load
    let mut tx = db.begin_write();
    tx.create_node(&["Unsaved"], BTreeMap::new()).unwrap();
    tx.commit().unwrap();
    crash(db);
    assert_eq!(
        Database::open_with_durability(&path, false)
            .unwrap()
            .graph()
            .node_count(),
        500
    );
}

#[test]
fn incremental_saves_follow_a_bulk_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.tvdb");
    let db = Arc::new(Database::open(&path).unwrap());
    let mut bulk = db.begin_bulk_load(None).unwrap();
    load(&mut bulk, 200);
    bulk.commit().unwrap();
    // a second load on an existing file takes the incremental path
    let mut bulk = db.begin_bulk_load(None).unwrap();
    for _ in 0..50 {
        bulk.create_node(&["More"], BTreeMap::new()).unwrap();
    }
    bulk.commit().unwrap();
    crash(db);
    assert_eq!(Database::open(&path).unwrap().graph().node_count(), 250);
}
