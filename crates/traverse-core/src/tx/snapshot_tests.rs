//! Snapshot isolation across every read path, abort, collection and concurrency.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::{GraphView, MvccStore, ReadTx};
use crate::graph::Graph;
use crate::graph::constraint::ConstraintType;
use crate::graph::property::{IndexKey, PropertyValue};
use crate::graph::types::{Direction, EdgeId, LabelId, NodeId};

fn props(pairs: &[(&str, PropertyValue)]) -> BTreeMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn int(n: i64) -> PropertyValue {
    PropertyValue::Int(n)
}

fn sorted<T: Ord>(mut v: Vec<T>) -> Vec<T> {
    v.sort();
    v
}

fn version_state(store: &MvccStore) -> bool {
    // SAFETY: tests read with no writer active
    unsafe { store.graph() }.has_active_deltas()
}

/// Person nodes 0..n with `v = i`, an index on `:Person(v)`, a chain of KNOWS edges.
fn people(n: i64) -> (MvccStore, Vec<NodeId>, Vec<EdgeId>) {
    let mut g = Graph::new();
    let ids: Vec<NodeId> = (0..n)
        .map(|i| g.create_node(&["Person"], props(&[("v", int(i))])))
        .collect();
    let edges: Vec<EdgeId> = ids
        .windows(2)
        .map(|w| {
            g.create_edge(w[0], w[1], "KNOWS", props(&[("w", int(1))]))
                .unwrap()
        })
        .collect();
    g.create_index("pv", "Person", "v").unwrap();
    g.create_edge_index("kw", "KNOWS", Some("w")).unwrap();
    (MvccStore::new(g), ids, edges)
}

fn person_v(r: &ReadTx<'_>, v: i64) -> Vec<NodeId> {
    sorted(r.index_lookup("Person", "v", &IndexKey::Int(v)).unwrap())
}

#[test]
fn uncommitted_writes_are_invisible() {
    let (store, ids, _) = people(3);
    let mut w = store.begin_write();
    GraphView::update_node_properties(&mut w, ids[0], props(&[("v", int(100))])).unwrap();
    let created = GraphView::create_node(&mut w, &["Person"], props(&[("v", int(7))])).unwrap();
    GraphView::delete_node_cascading(&mut w, ids[2]).unwrap();
    let r = store.begin_read().unwrap();
    assert_eq!(r.get_node_property(ids[0], "v").unwrap(), Some(int(0)));
    assert!(r.get_node(created).is_err());
    assert!(r.get_node(ids[2]).is_ok());
    assert_eq!(r.node_count(), 3);
    assert_eq!(r.edge_count(), 2);
    assert_eq!(person_v(&r, 0), vec![ids[0]]);
    assert!(person_v(&r, 100).is_empty());
    assert!(person_v(&r, 7).is_empty());
    assert_eq!(sorted(r.nodes_by_label("Person")), sorted(ids.clone()));
    assert_eq!(
        r.neighbors(ids[1], Direction::Outgoing, None).unwrap(),
        vec![ids[2]]
    );
    w.commit().unwrap();
    // still the same snapshot after the commit
    assert_eq!(r.get_node_property(ids[0], "v").unwrap(), Some(int(0)));
    assert_eq!(r.node_count(), 3);
    let r2 = store.begin_read().unwrap();
    assert_eq!(r2.get_node_property(ids[0], "v").unwrap(), Some(int(100)));
    assert_eq!(r2.node_count(), 3);
    assert!(r2.get_node(ids[2]).is_err());
}

#[test]
fn old_snapshot_keeps_every_read_path() {
    let (store, ids, edges) = people(4);
    let r = store.begin_read().unwrap();

    let mut w = store.begin_write();
    // value change on an indexed property, a label removal, an edge and a node deletion, a new
    // node and edge
    GraphView::update_node_properties(&mut w, ids[0], props(&[("v", int(50))])).unwrap();
    GraphView::remove_label(&mut w, ids[1], "Person").unwrap();
    GraphView::delete_edge(&mut w, edges[2]).unwrap();
    GraphView::delete_node(&mut w, ids[3]).unwrap();
    let fresh = GraphView::create_node(&mut w, &["Person"], props(&[("v", int(0))])).unwrap();
    GraphView::create_edge(&mut w, fresh, ids[0], "KNOWS", props(&[("w", int(1))])).unwrap();
    w.commit().unwrap();

    // point reads
    assert_eq!(r.get_node_property(ids[0], "v").unwrap(), Some(int(0)));
    assert!(r.get_node(ids[3]).is_ok());
    assert!(r.get_node(fresh).is_err());
    assert!(
        r.has_label(ids[1], LabelId(r.strings().get("Person").unwrap()))
            .unwrap()
    );
    // scans and counts
    assert_eq!(r.node_count(), 4);
    assert_eq!(r.edge_count(), 3);
    assert_eq!(sorted(r.nodes_vec()), sorted(ids.clone()));
    assert_eq!(sorted(r.nodes_by_label("Person")), sorted(ids.clone()));
    assert_eq!(sorted(r.all_edges_vec()), sorted(edges.clone()));
    assert_eq!(sorted(r.edges_by_type("KNOWS")), sorted(edges.clone()));
    // index lookups by the old value find the node, not the new one
    assert_eq!(person_v(&r, 0), vec![ids[0]]);
    assert!(person_v(&r, 50).is_empty());
    assert_eq!(person_v(&r, 1), vec![ids[1]]);
    let range = sorted(
        r.index_range_lookup(
            "Person",
            "v",
            std::ops::Bound::Included(IndexKey::Int(0)),
            std::ops::Bound::Included(IndexKey::Int(3)),
        )
        .unwrap(),
    );
    assert_eq!(range, sorted(ids.clone()));
    assert_eq!(
        sorted(
            r.edge_index_lookup("KNOWS", "w", &IndexKey::Int(1))
                .unwrap()
        ),
        sorted(edges.clone())
    );
    // adjacency includes the deleted edge, excludes the new one
    assert_eq!(
        r.neighbors(ids[2], Direction::Outgoing, None).unwrap(),
        vec![ids[3]]
    );
    assert_eq!(
        r.edges_of(ids[3], Direction::Incoming, None).unwrap(),
        vec![edges[2]]
    );
    assert_eq!(r.degree(ids[0], Direction::Both).unwrap(), 1);
    assert!(
        r.edges_with_neighbor(ids[0], Direction::Incoming, None)
            .unwrap()
            .is_empty()
    );

    // a new snapshot sees the new state through the same paths
    let now = store.begin_read().unwrap();
    assert_eq!(now.node_count(), 4);
    assert_eq!(sorted(person_v(&now, 0)), vec![fresh]);
    assert_eq!(person_v(&now, 50), vec![ids[0]]);
    assert!(person_v(&now, 1).is_empty());
    assert!(now.get_node(ids[3]).is_err());
    assert_eq!(
        now.edges_of(ids[0], Direction::Incoming, None)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn in_place_mutation_is_imaged_before_it_happens() {
    let (store, ids, _) = people(2);
    let r = store.begin_read().unwrap();
    let mut w = store.begin_write();
    let kid = GraphView::intern_property_key(&mut w, "v");
    GraphView::unindex_node(&mut w, ids[0]);
    GraphView::get_node_mut(&mut w, ids[0])
        .unwrap()
        .properties
        .insert(kid, int(9));
    GraphView::index_node(&mut w, ids[0]);
    // mid-transaction and after commit, the old snapshot keeps v = 0
    assert_eq!(r.get_node_property(ids[0], "v").unwrap(), Some(int(0)));
    w.commit().unwrap();
    assert_eq!(r.get_node_property(ids[0], "v").unwrap(), Some(int(0)));
    assert_eq!(person_v(&r, 0), vec![ids[0]]);
    assert!(person_v(&r, 9).is_empty());
    let now = store.begin_read().unwrap();
    assert_eq!(person_v(&now, 9), vec![ids[0]]);
}

#[test]
fn replaced_property_maps_restore_dropped_keys() {
    let mut g = Graph::new();
    let a = g.create_node(&["P"], props(&[("v", int(1)), ("w", int(5))]));
    let store = MvccStore::new(g);
    let r = store.begin_read().unwrap();
    let mut w = store.begin_write();
    GraphView::update_node_properties(&mut w, a, props(&[("v", int(2))])).unwrap();
    w.commit().unwrap();
    assert_eq!(r.get_node_property(a, "w").unwrap(), Some(int(5)));
    assert_eq!(r.get_node_property(a, "v").unwrap(), Some(int(1)));
}

#[test]
fn abort_restores_data_and_indexes() {
    let (store, ids, edges) = people(3);
    {
        let mut w = store.begin_write();
        GraphView::update_node_properties(&mut w, ids[0], props(&[("v", int(42))])).unwrap();
        GraphView::add_label(&mut w, ids[1], "Admin").unwrap();
        let kid = GraphView::intern_property_key(&mut w, "v");
        GraphView::unindex_node(&mut w, ids[1]);
        GraphView::get_node_mut(&mut w, ids[1])
            .unwrap()
            .properties
            .insert(kid, int(43));
        GraphView::index_node(&mut w, ids[1]);
        GraphView::delete_node_cascading(&mut w, ids[2]).unwrap();
        GraphView::create_node(&mut w, &["Person"], props(&[("v", int(0))])).unwrap();
        w.abort();
    }
    assert!(!version_state(&store), "abort left version state behind");
    let r = store.begin_read().unwrap();
    assert_eq!(r.node_count(), 3);
    assert_eq!(r.edge_count(), 2);
    assert_eq!(person_v(&r, 0), vec![ids[0]]);
    assert_eq!(person_v(&r, 1), vec![ids[1]]);
    assert_eq!(person_v(&r, 2), vec![ids[2]]);
    assert!(person_v(&r, 42).is_empty());
    assert!(person_v(&r, 43).is_empty());
    assert!(r.nodes_by_label("Admin").is_empty());
    assert_eq!(
        sorted(
            r.edge_index_lookup("KNOWS", "w", &IndexKey::Int(1))
                .unwrap()
        ),
        sorted(edges.clone())
    );
    assert_eq!(
        r.neighbors(ids[1], Direction::Outgoing, None).unwrap(),
        vec![ids[2]]
    );
}

#[test]
fn schema_changes_roll_back_exactly() {
    let mut g = Graph::new();
    g.create_node(&["A"], props(&[("k", int(1)), ("j", int(1))]));
    let x = g.create_node(&["A"], BTreeMap::new());
    let y = g.create_node(&["A"], BTreeMap::new());
    g.create_edge(x, y, "R", props(&[("p", int(1))])).unwrap();
    g.create_relationship_constraint("rel_u", "R", "p", ConstraintType::Unique)
        .unwrap();
    g.create_composite_constraint(
        "comp",
        "A",
        &["k".to_string(), "j".to_string()],
        ConstraintType::Unique,
        false,
    )
    .unwrap();
    let store = MvccStore::new(g);
    {
        let mut w = store.begin_write();
        GraphView::drop_constraint(&mut w, "rel_u").unwrap();
        GraphView::drop_constraint(&mut w, "comp").unwrap();
        GraphView::create_point_index(&mut w, "pt", "A", "loc").unwrap();
        GraphView::create_index(&mut w, "ak", "A", "k").unwrap();
        w.abort();
    }
    let r = store.begin_read().unwrap();
    let reg = r.constraint_registry();
    let rel = reg
        .get_by_name("rel_u")
        .expect("relationship constraint back");
    assert_eq!(
        rel.entity,
        crate::graph::constraint::ConstraintEntity::Relationship
    );
    let comp = reg.get_by_name("comp").expect("composite constraint back");
    assert_eq!(comp.extra_property_keys.len(), 1);
    assert!(!r.has_point_index("pt"));
    assert!(!r.index_catalog().has_index_by_name("ak"));
}

#[test]
fn collection_frees_versions_once_no_snapshot_needs_them() {
    let (store, ids, _) = people(3);
    let mut w = store.begin_write();
    GraphView::update_node_properties(&mut w, ids[0], props(&[("v", int(9))])).unwrap();
    w.commit().unwrap();
    // no reader was open: everything collected at commit
    assert!(!version_state(&store));

    let r = store.begin_read().unwrap();
    let mut w = store.begin_write();
    GraphView::update_node_properties(&mut w, ids[1], props(&[("v", int(8))])).unwrap();
    w.commit().unwrap();
    assert!(version_state(&store), "the open snapshot needs the image");
    store.gc();
    assert!(version_state(&store));
    assert_eq!(r.get_node_property(ids[1], "v").unwrap(), Some(int(1)));
    drop(r);
    store.gc();
    assert!(!version_state(&store));
}

#[test]
fn vector_fulltext_and_point_queries_at_a_snapshot() {
    let mut g = Graph::new();
    let a = g.create_node(
        &["Doc"],
        props(&[
            ("emb", PropertyValue::Vector(vec![1.0, 0.0])),
            (
                "text",
                PropertyValue::String("graph database engine".into()),
            ),
            (
                "loc",
                PropertyValue::Point2D {
                    x: 0.0,
                    y: 0.0,
                    srid: 7203,
                },
            ),
        ]),
    );
    let b = g.create_node(
        &["Doc"],
        props(&[
            ("emb", PropertyValue::Vector(vec![0.0, 1.0])),
            ("text", PropertyValue::String("cooking recipes".into())),
            (
                "loc",
                PropertyValue::Point2D {
                    x: 10.0,
                    y: 10.0,
                    srid: 7203,
                },
            ),
        ]),
    );
    g.create_vector_index(
        "emb",
        "Doc",
        "emb",
        2,
        crate::graph::vector::Similarity::Cosine,
        Default::default(),
    )
    .unwrap();
    g.create_fulltext_index(
        "txt",
        &["Doc"],
        &["text"],
        crate::graph::fulltext::Analyzer::Standard,
    )
    .unwrap();
    g.create_point_index("pt", "Doc", "loc").unwrap();
    let store = MvccStore::new(g);
    let r = store.begin_read().unwrap();

    // a now points where b was and talks about cooking; b is deleted
    let mut w = store.begin_write();
    GraphView::update_node_properties(
        &mut w,
        a,
        props(&[
            ("emb", PropertyValue::Vector(vec![0.0, 1.0])),
            ("text", PropertyValue::String("cooking at home".into())),
            (
                "loc",
                PropertyValue::Point2D {
                    x: 10.0,
                    y: 10.0,
                    srid: 7203,
                },
            ),
        ]),
    )
    .unwrap();
    GraphView::delete_node(&mut w, b).unwrap();
    w.commit().unwrap();

    let top = r.query_vector_index("emb", &[1.0, 0.0], 1, None).unwrap();
    assert_eq!(top[0].0, a, "old snapshot: a still points along x");
    assert_eq!(
        r.query_vector_index("emb", &[0.0, 1.0], 1, None).unwrap()[0].0,
        b
    );
    let hits = r.query_fulltext_index("txt", "graph", 10).unwrap();
    assert_eq!(hits.iter().map(|h| h.0).collect::<Vec<_>>(), vec![a]);
    let hits = r.query_fulltext_index("txt", "cooking", 10).unwrap();
    assert_eq!(hits.iter().map(|h| h.0).collect::<Vec<_>>(), vec![b]);
    let near = r
        .point_index_within_distance("Doc", "loc", [0.0, 0.0, 0.0], 1.0, 7203)
        .unwrap();
    assert_eq!(near.iter().map(|h| h.0).collect::<Vec<_>>(), vec![a]);
    let knn = r
        .point_index_knn("Doc", "loc", [10.0, 10.0, 0.0], 1, 7203)
        .unwrap();
    assert_eq!(knn[0].0, b);

    let now = store.begin_read().unwrap();
    assert_eq!(
        now.query_vector_index("emb", &[0.0, 1.0], 1, None).unwrap()[0].0,
        a
    );
    assert!(
        now.query_fulltext_index("txt", "graph", 10)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn algorithm_adjacency_matches_the_snapshot() {
    let (store, ids, edges) = people(4);
    let r = store.begin_read().unwrap();
    let mut w = store.begin_write();
    GraphView::delete_edge(&mut w, edges[0]).unwrap();
    GraphView::create_edge(&mut w, ids[3], ids[0], "KNOWS", BTreeMap::new()).unwrap();
    w.commit().unwrap();
    // version state present: the reader builds a projection of its snapshot
    assert!(r.adjacency_snapshot().is_none());
    let adj = crate::graph::adjacency::AdjacencySnapshot::of(&r).unwrap();
    assert_eq!(adj.outgoing(ids[0]).len(), 1);
    assert_eq!(adj.outgoing(ids[0])[0].neighbor(), ids[1]);
    assert!(adj.outgoing(ids[3]).is_empty());
    drop(r);
    store.gc();
    let now = store.begin_read().unwrap();
    let adj = crate::graph::adjacency::AdjacencySnapshot::of(&now).unwrap();
    assert!(adj.outgoing(ids[0]).is_empty());
    assert_eq!(adj.outgoing(ids[3])[0].neighbor(), ids[0]);
}

/// Writers move value between accounts (in place, by replacement, by delete + recreate) while
/// readers check, through scans, index lookups and adjacency, that every snapshot sees the same
/// total and the same number of accounts.
#[test]
fn concurrent_readers_always_see_a_consistent_total() {
    const ACCOUNTS: i64 = 40;
    const TOTAL: i64 = ACCOUNTS * 100;
    let mut g = Graph::new();
    let hub = g.create_node(&["Hub"], BTreeMap::new());
    for _ in 0..ACCOUNTS {
        let a = g.create_node(&["Account"], props(&[("bal", int(100))]));
        g.create_edge(hub, a, "HOLDS", BTreeMap::new()).unwrap();
    }
    g.create_index("bal", "Account", "bal").unwrap();
    let store = Arc::new(MvccStore::new(g));
    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let (store, stop) = (Arc::clone(&store), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut i: u64 = 0;
            while !stop.load(Ordering::Relaxed) {
                i += 1;
                let mut w = store.begin_write();
                let accts = w.nodes_by_label("Account");
                let (a, b) = (
                    accts[(i as usize) % accts.len()],
                    accts[(i as usize * 7 + 3) % accts.len()],
                );
                if a == b {
                    continue;
                }
                let bal = |w: &super::WriteTx<'_>, n| match w.get_node_property(n, "bal").unwrap() {
                    Some(PropertyValue::Int(v)) => v,
                    _ => panic!("no balance"),
                };
                let (ba, bb) = (bal(&w, a), bal(&w, b));
                let amt = (i % 7) as i64;
                match i % 3 {
                    0 => {
                        GraphView::update_node_properties(
                            &mut w,
                            a,
                            props(&[("bal", int(ba - amt))]),
                        )
                        .unwrap();
                        GraphView::update_node_properties(
                            &mut w,
                            b,
                            props(&[("bal", int(bb + amt))]),
                        )
                        .unwrap();
                    }
                    1 => {
                        let kid = GraphView::intern_property_key(&mut w, "bal");
                        for (n, v) in [(a, ba - amt), (b, bb + amt)] {
                            GraphView::unindex_node(&mut w, n);
                            GraphView::get_node_mut(&mut w, n)
                                .unwrap()
                                .properties
                                .insert(kid, int(v));
                            GraphView::index_node(&mut w, n);
                        }
                    }
                    _ => {
                        // replace `a` by a new account holding its balance minus the amount
                        GraphView::delete_node_cascading(&mut w, a).unwrap();
                        let n = GraphView::create_node(
                            &mut w,
                            &["Account"],
                            props(&[("bal", int(ba - amt))]),
                        )
                        .unwrap();
                        GraphView::create_edge(&mut w, hub, n, "HOLDS", BTreeMap::new()).unwrap();
                        GraphView::update_node_properties(
                            &mut w,
                            b,
                            props(&[("bal", int(bb + amt))]),
                        )
                        .unwrap();
                    }
                }
                if i.is_multiple_of(11) {
                    w.abort();
                } else {
                    w.commit().unwrap();
                }
                if i.is_multiple_of(50) {
                    store.gc();
                }
            }
        })
    };

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (store, stop) = (Arc::clone(&store), Arc::clone(&stop));
            std::thread::spawn(move || {
                let mut checks = 0;
                while !stop.load(Ordering::Relaxed) {
                    let r = store.begin_read().unwrap();
                    let accts = r.nodes_by_label("Account");
                    assert_eq!(accts.len() as i64, ACCOUNTS, "label scan count");
                    let sum: i64 = accts
                        .iter()
                        .map(|&n| match r.get_node_property(n, "bal").unwrap() {
                            Some(PropertyValue::Int(v)) => v,
                            _ => panic!("no balance"),
                        })
                        .sum();
                    assert_eq!(sum, TOTAL, "label scan total");
                    let held = r.neighbors(hub, Direction::Outgoing, None).unwrap();
                    assert_eq!(sorted(held), sorted(accts.clone()), "adjacency");
                    let all = r
                        .index_range_lookup(
                            "Account",
                            "bal",
                            std::ops::Bound::Unbounded,
                            std::ops::Bound::Unbounded,
                        )
                        .unwrap();
                    assert_eq!(all.len() as i64, ACCOUNTS, "index range count");
                    let via_index: i64 = all
                        .into_iter()
                        .map(|n| match r.get_node_property(n, "bal").unwrap() {
                            Some(PropertyValue::Int(v)) => v,
                            _ => panic!("no balance"),
                        })
                        .sum();
                    assert_eq!(via_index, TOTAL, "index total");
                    assert_eq!(r.node_count() as i64, ACCOUNTS + 1, "count");
                    checks += 1;
                }
                checks
            })
        })
        .collect();

    std::thread::sleep(std::time::Duration::from_millis(1500));
    stop.store(true, Ordering::Relaxed);
    writer.join().unwrap();
    let checks: i32 = readers.into_iter().map(|h| h.join().unwrap()).sum();
    assert!(checks > 0);
}

/// Synchronous WAL: a commit published in memory but not yet durable stays invisible to new
/// snapshots until the log reaches disk; async WAL shows it at once.
#[cfg(not(target_arch = "wasm32"))]
#[test]
fn snapshots_wait_for_durability_in_synchronous_wal_mode() {
    use crate::storage::rlog::{LogWriter, MIN_SEGMENT_SIZE, Op, WriterOptions};
    let dir = tempfile::TempDir::new().unwrap();
    let writer = Arc::new(std::sync::Mutex::new(
        LogWriter::open(WriterOptions {
            segment_size: MIN_SEGMENT_SIZE,
            stem: "v.tvdb".to_string(),
            dir: dir.path().to_path_buf(),
        })
        .unwrap(),
    ));
    let store = MvccStore::new(Graph::new()).with_log_writer(Arc::clone(&writer));
    let mut w = store.begin_write();
    GraphView::create_node(&mut w, &["A"], BTreeMap::new()).unwrap();
    let durable = super::encode_ts(w.commit().unwrap());
    assert_eq!(
        store.visible_commit(),
        durable,
        "an acknowledged commit is visible"
    );

    // a later commit appended and published, its sync still pending
    let later = super::encode_ts(store.clock.now());
    writer
        .lock()
        .unwrap()
        .append_batch(&[Op::DeleteNode { node_id: 99 }], later)
        .unwrap();
    store.last_commit_ts.store(later, Ordering::SeqCst);
    assert_eq!(store.visible_commit(), durable, "not durable: not visible");
    assert_eq!(
        super::encode_ts(store.begin_read().unwrap().snapshot_ts()),
        durable
    );
    store.set_sync_on_commit(false);
    assert_eq!(
        store.visible_commit(),
        later,
        "async WAL: visible when published"
    );
    store.set_sync_on_commit(true);

    writer.lock().unwrap().sync_now().unwrap();
    assert_eq!(store.visible_commit(), later, "durable now");
}

#[test]
fn batches_commit_and_roll_back_on_their_own() {
    let store = MvccStore::new(Graph::new());
    let mut tx = store.begin_write();
    // a client's transaction until the host says it's an auto-commit statement's
    assert!(GraphView::is_explicit_transaction(&tx));
    assert!(GraphView::commit_batch(&mut tx).is_err());
    tx.allow_batch_commits();
    assert!(!GraphView::is_explicit_transaction(&tx));
    tx.create_node(&["A"], BTreeMap::new()).unwrap();
    GraphView::commit_batch(&mut tx).unwrap();
    // committed while the transaction carries on
    assert_eq!(store.begin_read().unwrap().node_count(), 1);
    tx.create_node(&["B"], BTreeMap::new()).unwrap();
    GraphView::rollback_batch(&mut tx).unwrap();
    tx.create_node(&["C"], BTreeMap::new()).unwrap();
    GraphView::commit_batch(&mut tx).unwrap();
    tx.create_node(&["D"], BTreeMap::new()).unwrap();
    // aborting drops only what came after the last batch
    tx.abort();
    let r = store.begin_read().unwrap();
    assert_eq!(r.node_count(), 2);
    assert_eq!(r.nodes_by_label("A").len(), 1);
    assert_eq!(r.nodes_by_label("C").len(), 1);
    assert!(r.nodes_by_label("B").is_empty());
}
