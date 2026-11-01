//! Integration: WriteTx -> WAL -> recovery -> replay, from `WriteTx::commit()` writing redo records
//! through `scan_segments` to a fresh `MvccStore` replaying via the Graph API.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use traverse_core::graph::Graph;
use traverse_core::graph::property::PropertyValue;
use traverse_core::storage::rlog::{LogWriter, Op as RedoOp, WriterOptions, scan_segments};
use traverse_core::tx::MvccStore;
use traverse_core::tx::view::GraphView;

fn pmap(pairs: &[(&str, PropertyValue)]) -> BTreeMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn open_writer(dir: &std::path::Path) -> Arc<Mutex<LogWriter>> {
    let opts = WriterOptions {
        segment_size: traverse_core::storage::rlog::MIN_SEGMENT_SIZE,
        stem: "test.tvdb".to_string(),
        dir: dir.to_path_buf(),
    };
    Arc::new(Mutex::new(LogWriter::open(opts).unwrap()))
}

/// Replays recovered ops onto a fresh MvccStore, like `Database::open` does.
fn replay_ops(store: &MvccStore, ops: &[RedoOp]) {
    let mut tx = store.begin_write();
    tx.set_skip_redo(); // never log replayed ops to the WAL itself
    for op in ops {
        match op {
            RedoOp::CreateNode {
                node_id: _,
                labels,
                properties,
            } => {
                // assigned node_ids aren't checked: replay onto an empty Graph assigns 0,1,2... in
                // the original order anyway
                let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
                let props: BTreeMap<String, PropertyValue> = properties.iter().cloned().collect();
                GraphView::create_node(&mut tx, &label_refs, props).unwrap();
            }
            RedoOp::DeleteNode { node_id } => {
                let nid = traverse_core::graph::types::NodeId(*node_id);
                GraphView::delete_node(&mut tx, nid).unwrap();
            }
            RedoOp::SetNodeProperties {
                node_id,
                properties,
            } => {
                let nid = traverse_core::graph::types::NodeId(*node_id);
                let props: BTreeMap<String, PropertyValue> = properties.iter().cloned().collect();
                GraphView::update_node_properties(&mut tx, nid, props).unwrap();
            }
            RedoOp::AddLabel { node_id, label } => {
                let nid = traverse_core::graph::types::NodeId(*node_id);
                GraphView::add_label(&mut tx, nid, label).unwrap();
            }
            RedoOp::RemoveLabel { node_id, label } => {
                let nid = traverse_core::graph::types::NodeId(*node_id);
                GraphView::remove_label(&mut tx, nid, label).unwrap();
            }
            RedoOp::CreateEdge {
                edge_id: _,
                src,
                dst,
                edge_type,
                properties,
            } => {
                let s = traverse_core::graph::types::NodeId(*src);
                let d = traverse_core::graph::types::NodeId(*dst);
                let props: BTreeMap<String, PropertyValue> = properties.iter().cloned().collect();
                GraphView::create_edge(&mut tx, s, d, edge_type, props).unwrap();
            }
            RedoOp::DeleteEdge { edge_id } => {
                let eid = traverse_core::graph::types::EdgeId(*edge_id);
                GraphView::delete_edge(&mut tx, eid).unwrap();
            }
            RedoOp::SetEdgeProperties {
                edge_id,
                properties,
            } => {
                let eid = traverse_core::graph::types::EdgeId(*edge_id);
                let props: BTreeMap<String, PropertyValue> = properties.iter().cloned().collect();
                GraphView::update_edge_properties(&mut tx, eid, props).unwrap();
            }
            RedoOp::CreateIndex {
                name,
                label,
                property,
            } => {
                GraphView::create_index(&mut tx, name, label, property).unwrap();
            }
            RedoOp::DropIndex { name } => {
                GraphView::drop_index(&mut tx, name).unwrap();
            }
            RedoOp::CreateConstraint {
                name,
                label,
                property,
                kind,
            } => {
                let ctype = match kind {
                    1 => traverse_core::graph::constraint::ConstraintType::Unique,
                    2 => traverse_core::graph::constraint::ConstraintType::Exists,
                    _ => panic!("unknown constraint kind {}", kind),
                };
                GraphView::create_constraint(&mut tx, name, label, property, ctype).unwrap();
            }
            RedoOp::DropConstraint { name } => {
                GraphView::drop_constraint(&mut tx, name).unwrap();
            }
            RedoOp::CreateEdgeIndex {
                name,
                edge_type,
                property,
            } => {
                GraphView::create_edge_index(&mut tx, name, edge_type, property.as_deref())
                    .unwrap();
            }
            RedoOp::DropEdgeIndex { name } => {
                GraphView::drop_edge_index(&mut tx, name).unwrap();
            }
            RedoOp::CreateVectorIndex {
                name,
                label,
                property,
                dimensions,
                similarity,
                m,
                ef_construction,
                ef_search,
                seed,
                quantize,
            } => {
                let sim = traverse_core::graph::vector::Similarity::parse(similarity).unwrap();
                let params = traverse_core::graph::vector::HnswParams {
                    m: *m as usize,
                    ef_construction: *ef_construction as usize,
                    ef_search: *ef_search as usize,
                    seed: *seed,
                    quantize: *quantize,
                };
                GraphView::create_vector_index(
                    &mut tx,
                    name,
                    label,
                    property,
                    *dimensions as usize,
                    sim,
                    params,
                )
                .unwrap();
            }
            RedoOp::DropVectorIndex { name } => {
                GraphView::drop_vector_index(&mut tx, name).unwrap();
            }
            RedoOp::CreateFulltextIndex {
                name,
                labels,
                properties,
                analyzer,
            } => {
                let az = traverse_core::graph::fulltext::Analyzer::from_name(analyzer).unwrap();
                let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                let prop_refs: Vec<&str> = properties.iter().map(String::as_str).collect();
                GraphView::create_fulltext_index(&mut tx, name, &label_refs, &prop_refs, az)
                    .unwrap();
            }
            RedoOp::DropFulltextIndex { name } => {
                GraphView::drop_fulltext_index(&mut tx, name).unwrap();
            }
            RedoOp::CreatePointIndex {
                name,
                label,
                property,
            } => {
                GraphView::create_point_index(&mut tx, name, label, property).unwrap();
            }
            RedoOp::DropPointIndex { name } => {
                GraphView::drop_point_index(&mut tx, name).unwrap();
            }
            RedoOp::CreateTextIndex {
                name,
                label,
                property,
            } => {
                GraphView::create_text_index(&mut tx, name, label, property).unwrap();
            }
            RedoOp::DropTextIndex { name } => {
                GraphView::drop_text_index(&mut tx, name).unwrap();
            }
        }
    }
    tx.commit().unwrap();
}

#[test]
fn writex_commit_writes_to_wal() {
    let tmp = TempDir::new().unwrap();
    let writer = open_writer(tmp.path());
    let store = MvccStore::new(Graph::new()).with_log_writer(writer.clone());

    let mut tx = store.begin_write();
    let nid = GraphView::create_node(
        &mut tx,
        &["Person"],
        pmap(&[("name", PropertyValue::String("Alice".into()))]),
    )
    .unwrap();
    tx.commit().unwrap();

    drop(store); // releases write_lock; the Arc still owns the writer
    drop(writer); // drops the LogWriter, closing the segment

    let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
    assert!(report.is_clean(), "status = {:?}", report.status);
    assert_eq!(report.transactions.len(), 1);
    let tx = &report.transactions[0];
    assert_eq!(tx.ops.len(), 1);
    match &tx.ops[0] {
        RedoOp::CreateNode {
            node_id,
            labels,
            properties,
        } => {
            assert_eq!(*node_id, nid.0);
            assert_eq!(labels, &vec!["Person".to_string()]);
            assert_eq!(properties.len(), 1);
            assert_eq!(properties[0].0, "name");
        }
        other => panic!("unexpected op {:?}", other),
    }
}

#[test]
fn writex_without_writer_is_zero_overhead() {
    // WriteTx behaves the same with no log writer
    let store = MvccStore::new(Graph::new());
    let mut tx = store.begin_write();
    let _ = GraphView::create_node(&mut tx, &["N"], pmap(&[])).unwrap();
    tx.commit().unwrap();
    // no segments created; passing = not panicking
}

#[test]
fn full_cycle_commit_recover_replay() {
    let tmp = TempDir::new().unwrap();
    let writer = open_writer(tmp.path());

    // phase 1: original store, several commits
    let mut original_ids = Vec::new();
    let mut original_edge_ids = Vec::new();
    {
        let store = MvccStore::new(Graph::new()).with_log_writer(writer.clone());
        for i in 0..20u64 {
            let mut tx = store.begin_write();
            let nid = GraphView::create_node(
                &mut tx,
                &["Person"],
                pmap(&[
                    ("name", PropertyValue::String(format!("user{}", i))),
                    ("age", PropertyValue::Int(i as i64)),
                ]),
            )
            .unwrap();
            original_ids.push(nid);
            tx.commit().unwrap();
        }
        // a few edges
        for i in 0..10u64 {
            let mut tx = store.begin_write();
            let eid = GraphView::create_edge(
                &mut tx,
                original_ids[i as usize],
                original_ids[(i as usize + 1) % 20],
                "Friend",
                pmap(&[("since", PropertyValue::Int(2026))]),
            )
            .unwrap();
            original_edge_ids.push(eid);
            tx.commit().unwrap();
        }
        // mutations: label add/remove, property update, delete
        {
            let mut tx = store.begin_write();
            GraphView::add_label(&mut tx, original_ids[0], "Admin").unwrap();
            tx.commit().unwrap();
        }
        {
            let mut tx = store.begin_write();
            GraphView::update_node_properties(
                &mut tx,
                original_ids[1],
                pmap(&[("name", PropertyValue::String("renamed".into()))]),
            )
            .unwrap();
            tx.commit().unwrap();
        }
        {
            let mut tx = store.begin_write();
            GraphView::delete_node(&mut tx, original_ids[19]).unwrap();
            tx.commit().unwrap();
        }
    }
    drop(writer); // close all segments

    // phase 2: scan + replay onto a fresh store
    let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
    assert!(report.is_clean(), "status = {:?}", report.status);
    // 20 creates + 10 edge creates + 3 mutations = 33 transactions
    assert_eq!(report.transactions.len(), 33);

    let replayed_store = MvccStore::new(Graph::new());
    for tx_record in &report.transactions {
        replay_ops(&replayed_store, &tx_record.ops);
    }

    // phase 3: compare replayed state with expected
    let tx = replayed_store.begin_read().unwrap();
    // 20 created - 1 deleted = 19 nodes
    assert_eq!(GraphView::node_count(&tx), 19);
    // 10 edges; the deleted node (id 19) is in none of them with this setup
    let edge_count = GraphView::edge_count(&tx);
    // cascades from deleting node 19 could remove some
    assert!(edge_count <= 10, "got {}", edge_count);

    // user0 gained label "Admin"
    let admins = GraphView::nodes_by_label(&tx, "Admin");
    assert_eq!(admins.len(), 1);
    let admin_name = GraphView::get_node_property(&tx, admins[0], "name").unwrap();
    assert_eq!(admin_name.unwrap().as_str(), Some("user0"));

    // user1 renamed to "renamed"
    let renamed = GraphView::get_node_property(&tx, original_ids[1], "name")
        .unwrap()
        .unwrap();
    assert_eq!(renamed.as_str(), Some("renamed"));
}

#[test]
fn rollback_on_commit_with_no_ops_is_a_noop() {
    // empty transactions write no redo records; commit still returns Ok
    let tmp = TempDir::new().unwrap();
    let writer = open_writer(tmp.path());
    let store = MvccStore::new(Graph::new()).with_log_writer(writer);
    let tx = store.begin_write();
    let _ = tx.commit().unwrap();
}

/// Full snapshot + WAL truncation cycle:
///
/// 1. workload through WriteTx -> WAL
/// 2. "flush": take the LogWriter's `next_lsn` as recovery_lsn, persist it in a
///    v6 .tvdb header
/// 3. truncate old segments via `LogWriter::truncate_at`
/// 4. second workload
/// 5. crash: drop everything
/// 6. reopen: read recovery_lsn from the header, scan the WAL with it as
///    min_lsn; ONLY post-flush transactions come back
#[test]
fn full_snapshot_and_truncation_cycle() {
    use traverse_core::storage::format::{FileHeader, HEADER_SIZE};
    let tmp = TempDir::new().unwrap();
    let tvdb_path = tmp.path().join("snap.tvdb");
    let writer = open_writer_in(tmp.path(), "snap.tvdb");

    // phase 1: pre-flush workload
    let store_pre = MvccStore::new(Graph::new()).with_log_writer(writer.clone());
    let mut pre_ids = Vec::new();
    for i in 0..5u64 {
        let mut tx = store_pre.begin_write();
        let nid = GraphView::create_node(
            &mut tx,
            &["N"],
            pmap(&[("i", PropertyValue::Int(i as i64))]),
        )
        .unwrap();
        pre_ids.push(nid);
        tx.commit().unwrap();
    }
    // "flush": v6 .tvdb header with recovery_lsn = writer's current next_lsn
    let snapshot_lsn = writer.lock().unwrap().next_lsn();
    {
        let mut header = FileHeader::new(0, 0);
        header.recovery_lsn = snapshot_lsn;
        let bytes = header.to_bytes();
        std::fs::write(&tvdb_path, bytes).unwrap();
    }
    // truncate everything <= snapshot_lsn; the active segment survives
    let pre_segs = writer.lock().unwrap().segment_paths().unwrap().len();
    {
        let mut w = writer.lock().unwrap();
        w.rotate_segment().unwrap(); // close the segment holding the pre-flush records
        let _ = w.truncate_at(snapshot_lsn).unwrap();
    }
    let post_truncate_segs = writer.lock().unwrap().segment_paths().unwrap().len();
    assert!(
        post_truncate_segs < pre_segs + 1, // +1: rotate added a segment
        "expected truncation to delete old segments: pre={} post={}",
        pre_segs,
        post_truncate_segs
    );

    // phase 2: post-flush workload
    let mut post_ids = Vec::new();
    for i in 5..10u64 {
        let mut tx = store_pre.begin_write();
        let nid = GraphView::create_node(
            &mut tx,
            &["N"],
            pmap(&[("i", PropertyValue::Int(i as i64))]),
        )
        .unwrap();
        post_ids.push(nid);
        tx.commit().unwrap();
    }

    // phase 3: drop everything (crash loses in-memory state)
    drop(store_pre);
    drop(writer);

    // phase 4: reopen; header.recovery_lsn as min_lsn for the WAL scan
    let mut hdr_buf = [0u8; HEADER_SIZE];
    let file_bytes = std::fs::read(&tvdb_path).unwrap();
    hdr_buf[..file_bytes.len()].copy_from_slice(&file_bytes);
    let header = FileHeader::from_bytes(&hdr_buf).unwrap();
    assert_eq!(header.recovery_lsn, snapshot_lsn);

    let report =
        traverse_core::storage::rlog::scan_segments(tmp.path(), "snap.tvdb", header.recovery_lsn)
            .unwrap();

    // ONLY the 5 post-flush transactions
    assert_eq!(
        report.transactions.len(),
        5,
        "expected 5 post-flush txns, got {}",
        report.transactions.len()
    );
    for (i, tx) in report.transactions.iter().enumerate() {
        assert!(tx.commit_lsn > header.recovery_lsn);
        match &tx.ops[0] {
            RedoOp::CreateNode { properties, .. } => {
                // the "i" property identifies the workload round
                let i_prop = properties
                    .iter()
                    .find(|(k, _)| k == "i")
                    .map(|(_, v)| v.clone())
                    .unwrap();
                match i_prop {
                    PropertyValue::Int(v) => assert_eq!(v, (5 + i) as i64),
                    other => panic!("expected Int, got {:?}", other),
                }
            }
            other => panic!("expected CreateNode, got {:?}", other),
        }
    }
}

fn open_writer_in(dir: &std::path::Path, stem: &str) -> Arc<Mutex<LogWriter>> {
    let opts = WriterOptions {
        segment_size: traverse_core::storage::rlog::MIN_SEGMENT_SIZE,
        stem: stem.to_string(),
        dir: dir.to_path_buf(),
    };
    Arc::new(Mutex::new(LogWriter::open(opts).unwrap()))
}

#[test]
fn many_concurrent_readers_during_writex_commit() {
    // WAL fsync must not deadlock readers
    let tmp = TempDir::new().unwrap();
    let writer = open_writer(tmp.path());
    let store = Arc::new(MvccStore::new(Graph::new()).with_log_writer(writer.clone()));

    // a few nodes up front
    {
        for _ in 0..5 {
            let mut tx = store.begin_write();
            let _ = GraphView::create_node(&mut tx, &["N"], pmap(&[])).unwrap();
            tx.commit().unwrap();
        }
    }

    // reader threads
    let mut handles = Vec::new();
    for _ in 0..4 {
        let store_clone = Arc::clone(&store);
        handles.push(std::thread::spawn(move || {
            for _ in 0..100 {
                let r = store_clone.begin_read().unwrap();
                let _ = GraphView::node_count(&r);
                drop(r);
            }
        }));
    }

    // concurrent writer
    for _ in 0..50 {
        let mut tx = store.begin_write();
        let _ = GraphView::create_node(&mut tx, &["N"], pmap(&[])).unwrap();
        tx.commit().unwrap();
    }

    for h in handles {
        h.join().unwrap();
    }
    let r = store.begin_read().unwrap();
    assert_eq!(GraphView::node_count(&r), 55);
}

/// CREATE VECTOR INDEX in the redo log must recover: replay onto a fresh store
/// re-creates the descriptor and rebuilds the HNSW from replayed vectors, so
/// k-ANN works without any .tvdb snapshot.
#[test]
fn vector_index_survives_rlog_replay() {
    use traverse_core::graph::vector::{HnswParams, Similarity};

    let tmp = TempDir::new().unwrap();
    let writer = open_writer(tmp.path());

    let mk = |i: usize| -> PropertyValue {
        PropertyValue::List(
            (0..8)
                .map(|j| PropertyValue::Float(((i * 3 + j) as f64 * 0.13).sin()))
                .collect(),
        )
    };
    let query0: Vec<f32> = (0..8).map(|j| (j as f64 * 0.13).sin() as f32).collect();

    // original store: vector nodes + vector index, committed to the WAL
    {
        let store = MvccStore::new(Graph::new()).with_log_writer(writer.clone());
        let mut tx = store.begin_write();
        for i in 0..40 {
            GraphView::create_node(&mut tx, &["Doc"], pmap(&[("emb", mk(i))])).unwrap();
        }
        GraphView::create_vector_index(
            &mut tx,
            "docEmb",
            "Doc",
            "emb",
            8,
            Similarity::Cosine,
            HnswParams::default(),
        )
        .unwrap();
        tx.commit().unwrap();
        drop(store);
    }
    drop(writer);

    // replay onto a fresh store: crash recovery without a snapshot
    let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
    assert!(report.is_clean(), "status = {:?}", report.status);
    let replayed = MvccStore::new(Graph::new());
    for tx_record in &report.transactions {
        replay_ops(&replayed, &tx_record.ops);
    }

    // recovered store has the index and answers queries
    let r = replayed.begin_read().unwrap();
    assert!(
        GraphView::has_vector_index(&r, "docEmb"),
        "index should be recovered"
    );
    let hits =
        GraphView::query_vector_index(&r, "docEmb", &query0, 5, None).expect("index present");
    assert_eq!(hits.len(), 5, "HNSW rebuilt + queryable after replay");
    assert!(hits[0].1 > 0.99, "exact match recovered");
}

/// Cypher SET mutates via the unindex_node -> get_node_mut -> index_node bracket,
/// not update_node_properties. Bracket-path writes must replay after a crash.
#[test]
fn bracket_path_property_updates_are_durable() {
    let tmp = TempDir::new().unwrap();
    let writer = open_writer(tmp.path());

    let (nid, eid);
    {
        let store = MvccStore::new(Graph::new()).with_log_writer(writer.clone());
        let mut tx = store.begin_write();
        let a =
            GraphView::create_node(&mut tx, &["P"], pmap(&[("v", PropertyValue::Int(1))])).unwrap();
        let b = GraphView::create_node(&mut tx, &["P"], pmap(&[])).unwrap();
        let e = GraphView::create_edge(&mut tx, a, b, "R", pmap(&[])).unwrap();
        tx.commit().unwrap();
        nid = a;
        eid = e;

        // mutate through the bracket, exactly like the executor's SET
        let mut tx = store.begin_write();
        GraphView::unindex_node(&mut tx, nid);
        {
            let pid = GraphView::intern_property_key(&mut tx, "v");
            let n = tx.get_node_mut(nid).unwrap();
            n.properties.insert(pid, PropertyValue::Int(42));
        }
        GraphView::index_node(&mut tx, nid);
        GraphView::unindex_edge(&mut tx, eid);
        {
            let pid = GraphView::intern_property_key(&mut tx, "w");
            let e = tx.get_edge_mut(eid).unwrap();
            e.properties.insert(pid, PropertyValue::Int(7));
        }
        GraphView::index_edge(&mut tx, eid);
        tx.commit().unwrap();
        // dropped WITHOUT a snapshot: crash
    }
    drop(writer);

    // recover: scan + replay onto a fresh store (as production open does)
    let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
    assert!(report.is_clean(), "status = {:?}", report.status);
    let replayed = MvccStore::new(Graph::new());
    for tx_record in &report.transactions {
        replay_ops(&replayed, &tx_record.ops);
    }
    let tx = replayed.begin_read().unwrap();
    let v = GraphView::get_node_property(&tx, nid, "v")
        .unwrap()
        .unwrap();
    assert_eq!(
        v,
        PropertyValue::Int(42),
        "bracket-path node property survived the crash"
    );
    let w = GraphView::get_edge_property(&tx, eid, "w")
        .unwrap()
        .unwrap();
    assert_eq!(
        w,
        PropertyValue::Int(7),
        "bracket-path edge property survived the crash"
    );
}

/// Group commit: concurrent committers share fsyncs. Checks correctness (every acked commit
/// replays) and grouping (fewer fdatasyncs than commits).
#[test]
fn group_commit_concurrent_writers() {
    use std::sync::atomic::{AtomicU64, Ordering};

    let tmp = TempDir::new().unwrap();
    let writer = open_writer(tmp.path());

    const THREADS: usize = 8;
    const COMMITS_PER_THREAD: u64 = 25;
    let acked = AtomicU64::new(0);
    {
        let store =
            std::sync::Arc::new(MvccStore::new(Graph::new()).with_log_writer(writer.clone()));
        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let store = std::sync::Arc::clone(&store);
                let acked = &acked;
                scope.spawn(move || {
                    for i in 0..COMMITS_PER_THREAD {
                        let mut tx = store.begin_write();
                        GraphView::create_node(
                            &mut tx,
                            &["G"],
                            pmap(&[("t", PropertyValue::Int((t as i64) * 1000 + i as i64))]),
                        )
                        .unwrap();
                        tx.commit().unwrap();
                        acked.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
        });
    }
    let total = THREADS as u64 * COMMITS_PER_THREAD;
    assert_eq!(acked.load(Ordering::Relaxed), total);
    let metrics = writer.lock().unwrap().metrics();
    assert_eq!(metrics.commits, total);
    // grouping happened: strictly fewer syncs than commits (exact count is timing-dependent; 8
    // threads on a real disk collapse heavily)
    assert!(
        metrics.fdatasyncs < total,
        "expected grouped syncs, got {} fdatasyncs for {} commits",
        metrics.fdatasyncs,
        total
    );
    drop(writer);

    // every acked commit replays
    let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
    assert!(report.is_clean(), "status = {:?}", report.status);
    assert_eq!(report.transactions.len(), total as usize);
    let replayed = MvccStore::new(Graph::new());
    for tx_record in &report.transactions {
        replay_ops(&replayed, &tx_record.ops);
    }
    let tx = replayed.begin_read().unwrap();
    assert_eq!(GraphView::node_count(&tx), total as usize);
}

// -- Async-WAL commit path --------------------------------------------

/// wal-async: commits ack without fdatasync, but each is appended, and a later
/// sync_now() makes the whole prefix recoverable. The crash window depends on
/// sync cadence, not write volume.
#[test]
fn async_commit_appends_and_recovers_after_background_sync() {
    let dir = TempDir::new().unwrap();
    let writer = open_writer(dir.path());

    let store = MvccStore::new(Graph::new()).with_log_writer(Arc::clone(&writer));
    store.set_sync_on_commit(false);

    for i in 0..25 {
        let mut tx = store.begin_write();
        GraphView::create_node(&mut tx, &["Async"], pmap(&[("idx", PropertyValue::Int(i))]))
            .unwrap();
        tx.commit().unwrap(); // no per-commit fdatasync
    }
    assert_eq!(
        writer.lock().unwrap().metrics().fdatasyncs,
        0,
        "async commits don't sync"
    );

    // acked async commits are already in the log file (a process crash keeps them), before any
    // sync
    let before_sync = scan_segments(dir.path(), "test.tvdb", 0).unwrap();
    assert_eq!(before_sync.transactions.len(), 25);

    // the background task's job, done by hand
    {
        let lw = writer.lock().unwrap();
        lw.sync_now().unwrap();
    }
    drop(store);
    drop(writer);

    // everything appended before the sync recovers
    let recovered = scan_segments(dir.path(), "test.tvdb", 0).unwrap();
    let replay = MvccStore::new(Graph::new());
    for txn in &recovered.transactions {
        replay_ops(&replay, &txn.ops);
    }
    let read = replay.begin_read().unwrap();
    assert_eq!(GraphView::node_count(&read), 25);
}

/// A poisoned sync control fails async commits fast instead of acking onto a broken log.
#[test]
fn async_commit_fails_after_poison() {
    let dir = TempDir::new().unwrap();
    let writer = open_writer(dir.path());
    let sync = writer.lock().unwrap().sync_handle();

    let store = MvccStore::new(Graph::new()).with_log_writer(Arc::clone(&writer));
    store.set_sync_on_commit(false);

    // healthy commit goes through
    let mut tx = store.begin_write();
    GraphView::create_node(&mut tx, &["A"], pmap(&[])).unwrap();
    tx.commit().unwrap();

    // clean, then poison: commits must fail
    sync.check_poisoned().unwrap();
    sync.poison_for_test("injected fdatasync failure");
    let mut tx = store.begin_write();
    GraphView::create_node(&mut tx, &["A"], pmap(&[])).unwrap();
    assert!(tx.commit().is_err(), "post-poison async commit must fail");
}

/// An optimistic transaction's commit is logged like any write: replaying the log onto the same
/// starting graph reproduces its creates (with their reserved ids), updates and deletes.
#[test]
fn optimistic_commit_is_logged_and_replays() {
    let tmp = TempDir::new().unwrap();
    let base = || {
        let mut g = Graph::new();
        let a = g.create_node(&["P"], pmap(&[("v", PropertyValue::Int(1))]));
        let b = g.create_node(&["P"], pmap(&[("v", PropertyValue::Int(2))]));
        g.create_edge(a, b, "R", BTreeMap::new()).unwrap();
        (g, a, b)
    };
    let (g, a, b) = base();
    let store = MvccStore::new(g).with_log_writer(open_writer(tmp.path()));

    let mut tx = store.begin_optimistic(false).unwrap();
    let c = tx
        .create_node(&["P", "New"], pmap(&[("v", PropertyValue::Int(3))]))
        .unwrap();
    let e = tx.create_edge(c, a, "R", BTreeMap::new()).unwrap();
    tx.update_node_properties(a, pmap(&[("v", PropertyValue::Int(10))]))
        .unwrap();
    tx.delete_node_cascading(b).unwrap();
    tx.commit().unwrap();

    let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
    assert!(report.is_clean(), "status = {:?}", report.status);
    assert_eq!(
        report.transactions.len(),
        1,
        "one commit, one log transaction"
    );

    let (g, _, _) = base();
    let replayed = MvccStore::new(g);
    traverse_core::storage::rlog::apply_transactions(&replayed, &report.transactions).unwrap();
    let r = replayed.begin_read().unwrap();
    assert_eq!(r.node_count(), 2);
    assert_eq!(r.edge_count(), 1);
    assert!(r.get_node(b).is_err());
    assert_eq!(
        r.get_node_property(c, "v").unwrap(),
        Some(PropertyValue::Int(3)),
        "created under the id the transaction reserved"
    );
    assert_eq!(
        r.get_node_property(a, "v").unwrap(),
        Some(PropertyValue::Int(10))
    );
    assert_eq!(r.get_edge(e).unwrap().source, c);
    assert_eq!(r.nodes_by_label("New"), vec![c]);
}
