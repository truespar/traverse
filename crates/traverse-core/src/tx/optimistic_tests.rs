//! Optimistic transactions: read-your-writes, isolation, conflicts, constraints, schema.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::{GraphView, MvccStore, OwnedOptimisticTx};
use crate::error::Error;
use crate::graph::Graph;
use crate::graph::constraint::ConstraintType;
use crate::graph::property::{IndexKey, PropertyValue};
use crate::graph::types::{Direction, NodeId};

fn props(pairs: &[(&str, PropertyValue)]) -> BTreeMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn int(n: i64) -> PropertyValue {
    PropertyValue::Int(n)
}

fn get_int(view: &dyn GraphView, id: NodeId, key: &str) -> i64 {
    match view.get_node_property(id, key).unwrap() {
        Some(PropertyValue::Int(v)) => v,
        other => panic!("{key} = {other:?}"),
    }
}

fn counter_store() -> (MvccStore, NodeId) {
    let mut g = Graph::new();
    let c = g.create_node(&["Counter"], props(&[("n", int(0))]));
    g.create_index("cn", "Counter", "n").unwrap();
    (MvccStore::new(g), c)
}

#[test]
fn second_writer_of_the_same_object_conflicts() {
    let (store, c) = counter_store();
    let mut a = store.begin_optimistic(false).unwrap();
    let mut b = store.begin_optimistic(false).unwrap();
    let va = get_int(&a, c, "n");
    let vb = get_int(&b, c, "n");
    a.update_node_properties(c, props(&[("n", int(va + 1))]))
        .unwrap();
    b.update_node_properties(c, props(&[("n", int(vb + 1))]))
        .unwrap();
    a.commit().unwrap();
    match b.commit() {
        Err(Error::TransactionConflict(_)) => {}
        other => panic!("expected a conflict, got {other:?}"),
    }
    let r = store.begin_read().unwrap();
    assert_eq!(get_int(&r, c, "n"), 1);
}

#[test]
fn concurrent_increments_with_retry_lose_nothing() {
    let (store, c) = counter_store();
    let store = Arc::new(store);
    let threads = 8;
    let per = 50;
    let handles: Vec<_> = (0..threads)
        .map(|_| {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                let mut conflicts = 0;
                for _ in 0..per {
                    loop {
                        let mut tx = OwnedOptimisticTx::begin(&store, false).unwrap();
                        let v = get_int(&tx, c, "n");
                        tx.update_node_properties(c, props(&[("n", int(v + 1))]))
                            .unwrap();
                        match tx.commit() {
                            Ok(_) => break,
                            Err(Error::TransactionConflict(_)) => conflicts += 1,
                            Err(e) => panic!("{e}"),
                        }
                    }
                }
                conflicts
            })
        })
        .collect();
    let conflicts: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    let r = store.begin_read().unwrap();
    assert_eq!(get_int(&r, c, "n"), (threads * per) as i64);
    // the index followed every committed value
    assert_eq!(
        r.index_lookup("Counter", "n", &IndexKey::Int((threads * per) as i64))
            .unwrap(),
        vec![c]
    );
    // with 8 threads on one row, some must have raced
    assert!(conflicts > 0, "no conflict detected under contention");
}

#[test]
fn reads_see_own_writes_on_every_path() {
    let mut g = Graph::new();
    let a = g.create_node(&["P"], props(&[("k", int(1))]));
    let b = g.create_node(&["P"], props(&[("k", int(2))]));
    let e = g.create_edge(a, b, "R", props(&[("w", int(1))])).unwrap();
    g.create_index("pk", "P", "k").unwrap();
    g.create_edge_index("rw", "R", Some("w")).unwrap();
    let store = MvccStore::new(g);
    let mut tx = store.begin_optimistic(false).unwrap();

    let c = tx.create_node(&["P"], props(&[("k", int(3))])).unwrap();
    let f = tx.create_edge(b, c, "R", props(&[("w", int(2))])).unwrap();
    tx.update_node_properties(a, props(&[("k", int(10))]))
        .unwrap();
    tx.delete_edge(e).unwrap();
    tx.add_label(b, "Q").unwrap();

    assert_eq!(tx.node_count(), 3);
    assert_eq!(tx.edge_count(), 1);
    let mut by_label = tx.nodes_by_label("P");
    by_label.sort();
    assert_eq!(by_label, vec![a, b, c]);
    assert_eq!(tx.nodes_by_label("Q"), vec![b]);
    assert_eq!(
        tx.index_lookup("P", "k", &IndexKey::Int(3)).unwrap(),
        vec![c]
    );
    assert_eq!(
        tx.index_lookup("P", "k", &IndexKey::Int(10)).unwrap(),
        vec![a]
    );
    assert!(
        tx.index_lookup("P", "k", &IndexKey::Int(1))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        tx.edge_index_lookup("R", "w", &IndexKey::Int(2)).unwrap(),
        vec![f]
    );
    assert!(
        tx.edge_index_lookup("R", "w", &IndexKey::Int(1))
            .unwrap()
            .is_empty()
    );
    assert!(
        tx.neighbors(a, Direction::Outgoing, None)
            .unwrap()
            .is_empty()
    );
    assert_eq!(tx.neighbors(b, Direction::Outgoing, None).unwrap(), vec![c]);
    assert_eq!(
        tx.edges_of(c, Direction::Incoming, Some("R")).unwrap(),
        vec![f]
    );
    assert_eq!(tx.degree(b, Direction::Both).unwrap(), 1);
    let mut all = tx.nodes_vec();
    all.sort();
    assert_eq!(all, vec![a, b, c]);
    assert_eq!(tx.all_edges_vec(), vec![f]);

    // in-place change through the index bracket
    let kid = tx.intern_property_key("k");
    tx.unindex_node(b);
    tx.get_node_mut(b).unwrap().properties.insert(kid, int(20));
    // stale until re-indexed, still found by its current value
    assert_eq!(
        tx.index_lookup("P", "k", &IndexKey::Int(20)).unwrap(),
        vec![b]
    );
    tx.index_node(b);
    assert_eq!(
        tx.index_lookup("P", "k", &IndexKey::Int(20)).unwrap(),
        vec![b]
    );
    assert!(
        tx.index_lookup("P", "k", &IndexKey::Int(2))
            .unwrap()
            .is_empty()
    );

    // nothing visible outside before commit
    let r = store.begin_read().unwrap();
    assert_eq!(r.node_count(), 2);
    assert!(r.get_node(c).is_err());
    drop(r);

    tx.commit().unwrap();
    let r = store.begin_read().unwrap();
    // ids are final: what the transaction handed out is what's committed
    assert_eq!(get_int(&r, c, "k"), 3);
    assert_eq!(r.get_edge(f).unwrap().target, c);
    assert_eq!(get_int(&r, a, "k"), 10);
    assert_eq!(get_int(&r, b, "k"), 20);
    assert!(r.get_edge(e).is_err());
    assert_eq!(r.nodes_by_label("Q"), vec![b]);
    assert_eq!(
        r.index_lookup("P", "k", &IndexKey::Int(20)).unwrap(),
        vec![b]
    );
}

#[test]
fn snapshot_isolation_and_non_blocking() {
    let (store, c) = counter_store();
    let tx = store.begin_optimistic(false).unwrap();
    // an open optimistic transaction doesn't hold the writer lock
    let mut w = store.begin_write();
    GraphView::update_node_properties(&mut w, c, props(&[("n", int(5))])).unwrap();
    let extra = GraphView::create_node(&mut w, &["Counter"], props(&[("n", int(9))])).unwrap();
    w.commit().unwrap();
    assert_eq!(get_int(&tx, c, "n"), 0);
    assert!(tx.get_node(extra).is_err());
    assert_eq!(tx.nodes_by_label("Counter"), vec![c]);
    // reading only: commits fine
    tx.commit().unwrap();
}

#[test]
fn deleting_a_node_that_gained_an_edge_conflicts() {
    let mut g = Graph::new();
    let a = g.create_node(&["P"], BTreeMap::new());
    let b = g.create_node(&["P"], BTreeMap::new());
    let store = MvccStore::new(g);
    let mut tx = store.begin_optimistic(false).unwrap();
    tx.delete_node(a).unwrap();
    let mut w = store.begin_write();
    GraphView::create_edge(&mut w, b, a, "R", BTreeMap::new()).unwrap();
    w.commit().unwrap();
    assert!(matches!(tx.commit(), Err(Error::TransactionConflict(_))));
    let r = store.begin_read().unwrap();
    assert!(r.get_node(a).is_ok());
}

#[test]
fn edge_to_a_node_deleted_concurrently_conflicts() {
    let mut g = Graph::new();
    let a = g.create_node(&["P"], BTreeMap::new());
    let b = g.create_node(&["P"], BTreeMap::new());
    let store = MvccStore::new(g);
    let mut tx = store.begin_optimistic(false).unwrap();
    tx.create_edge(a, b, "R", BTreeMap::new()).unwrap();
    let mut w = store.begin_write();
    GraphView::delete_node(&mut w, b).unwrap();
    w.commit().unwrap();
    assert!(matches!(tx.commit(), Err(Error::TransactionConflict(_))));
    assert_eq!(store.begin_read().unwrap().edge_count(), 0);
}

#[test]
fn racing_unique_creates_conflict_not_duplicate() {
    let mut g = Graph::new();
    g.create_constraint("uk", "User", "email", ConstraintType::Unique)
        .unwrap();
    let store = MvccStore::new(g);
    let email = || props(&[("email", PropertyValue::String("a@x".into()))]);
    let mut t1 = store.begin_optimistic(false).unwrap();
    let mut t2 = store.begin_optimistic(false).unwrap();
    // both MERGE-check against their snapshot (no such user yet) and create
    assert!(
        t1.index_lookup("User", "email", &IndexKey::String("a@x".into()))
            .unwrap()
            .is_empty()
    );
    assert!(
        t2.index_lookup("User", "email", &IndexKey::String("a@x".into()))
            .unwrap()
            .is_empty()
    );
    t1.create_node(&["User"], email()).unwrap();
    t2.create_node(&["User"], email()).unwrap();
    t1.commit().unwrap();
    match t2.commit() {
        Err(Error::TransactionConflict(m)) => assert!(m.contains("concurrent"), "{m}"),
        other => panic!("expected a conflict, got {other:?}"),
    }
    assert_eq!(store.begin_read().unwrap().nodes_by_label("User").len(), 1);
}

#[test]
fn own_constraint_violation_is_not_a_conflict() {
    let mut g = Graph::new();
    g.create_constraint("ex", "User", "email", ConstraintType::Exists)
        .unwrap();
    let store = MvccStore::new(g);
    let mut tx = store.begin_optimistic(false).unwrap();
    tx.create_node(&["User"], BTreeMap::new()).unwrap();
    assert!(matches!(
        tx.commit(),
        Err(Error::ConstraintViolation { .. })
    ));
    assert_eq!(store.begin_read().unwrap().node_count(), 0);
}

#[test]
fn read_only_refuses_writes() {
    let (store, c) = counter_store();
    let mut tx = store.begin_optimistic(true).unwrap();
    assert!(matches!(
        tx.update_node_properties(c, props(&[("n", int(1))])),
        Err(Error::ReadOnlyTransaction)
    ));
    assert!(matches!(
        tx.create_node(&["X"], BTreeMap::new()),
        Err(Error::ReadOnlyTransaction)
    ));
    assert!(tx.get_node_mut(c).is_err());
    let ts = tx.commit().unwrap();
    assert!(ts.physical > 0);
}

#[test]
fn every_write_counts() {
    let store = MvccStore::new(Graph::new());
    let mut tx = store.begin_optimistic(false).unwrap();
    assert_eq!(GraphView::write_count(&tx), 0);
    let n = tx.create_node(&["A"], BTreeMap::new()).unwrap();
    assert_eq!(GraphView::write_count(&tx), 1);
    tx.add_label(n, "B").unwrap();
    // a refused write counts as an attempt too
    assert!(tx.add_label(n, "B").is_err());
    assert_eq!(GraphView::write_count(&tx), 3);
    assert!(GraphView::is_explicit_transaction(&tx));
}

#[test]
fn rollback_leaves_nothing_and_burns_ids() {
    let (store, _) = counter_store();
    let mut tx = store.begin_optimistic(false).unwrap();
    let id = tx.create_node(&["X"], BTreeMap::new()).unwrap();
    tx.rollback();
    let r = store.begin_read().unwrap();
    assert!(r.get_node(id).is_err());
    assert_eq!(r.node_count(), 1);
    drop(r);
    let mut w = store.begin_write();
    let next = GraphView::create_node(&mut w, &["X"], BTreeMap::new()).unwrap();
    w.commit().unwrap();
    assert!(next.0 > id.0, "ids are never reused");
}

#[test]
fn schema_changes_apply_at_commit() {
    let (store, _) = counter_store();
    let mut tx = store.begin_optimistic(false).unwrap();
    tx.create_index("xk", "X", "k").unwrap();
    // duplicate names are refused at once, also against pending changes
    assert!(tx.create_index("xk", "X", "j").is_err());
    assert!(tx.drop_index("missing").is_err());
    tx.create_constraint("xu", "X", "k", ConstraintType::Unique)
        .unwrap();
    tx.create_node(&["X"], props(&[("k", int(1))])).unwrap();
    // the snapshot's schema until commit
    assert!(!tx.index_catalog().has_index_by_name("xk"));
    tx.commit().unwrap();
    let r = store.begin_read().unwrap();
    assert!(r.index_catalog().has_index_by_name("xk"));
    assert!(r.constraint_registry().get_by_name("xu").is_some());
    assert_eq!(
        r.index_lookup("X", "k", &IndexKey::Int(1)).unwrap().len(),
        1
    );
}

#[test]
fn created_then_deleted_objects_leave_no_trace() {
    let (store, c) = counter_store();
    let mut tx = store.begin_optimistic(false).unwrap();
    let x = tx.create_node(&["X"], BTreeMap::new()).unwrap();
    let e = tx.create_edge(c, x, "R", BTreeMap::new()).unwrap();
    tx.delete_node_cascading(x).unwrap();
    assert!(tx.get_edge(e).is_err());
    assert_eq!(tx.node_count(), 1);
    assert!(
        tx.neighbors(c, Direction::Outgoing, None)
            .unwrap()
            .is_empty()
    );
    tx.commit().unwrap();
    let r = store.begin_read().unwrap();
    assert_eq!(r.node_count(), 1);
    assert_eq!(r.edge_count(), 0);
}
