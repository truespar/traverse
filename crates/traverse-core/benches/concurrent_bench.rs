use std::collections::BTreeMap;
use std::sync::Arc;

use criterion::{Criterion, criterion_group, criterion_main};
use traverse_core::graph::Graph;
use traverse_core::graph::property::PropertyValue;
use traverse_core::graph::types::NodeId;
use traverse_core::tx::{GraphView, MvccStore};

fn props(pairs: &[(&str, PropertyValue)]) -> BTreeMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// Store with 10,000 nodes, plus their IDs.
fn build_store() -> (Arc<MvccStore>, Vec<NodeId>) {
    let mut g = Graph::new();
    let mut ids = Vec::with_capacity(10_000);
    for i in 0..10_000 {
        let id = g.create_node(&["Item"], props(&[("index", PropertyValue::Int(i))]));
        ids.push(id);
    }
    (Arc::new(MvccStore::new(g)), ids)
}

fn bench_concurrent_reads_4(c: &mut Criterion) {
    let (store, ids) = build_store();
    let iters_per_thread = 500;

    c.bench_function("concurrent_reads_4_threads", |b| {
        b.iter_custom(|n| {
            let start = std::time::Instant::now();
            for _ in 0..n {
                let mut handles = Vec::with_capacity(4);
                for t in 0..4u64 {
                    let store = Arc::clone(&store);
                    let ids = ids.clone();
                    handles.push(std::thread::spawn(move || {
                        for j in 0..iters_per_thread {
                            let tx = store.begin_read().unwrap();
                            let _ = GraphView::node_count(&tx);
                            let idx = ((t as usize) * iters_per_thread + j) % ids.len();
                            let _ = GraphView::get_node(&tx, ids[idx]);
                        }
                    }));
                }
                for h in handles {
                    h.join().unwrap();
                }
            }
            start.elapsed()
        });
    });
}

fn bench_concurrent_read_write(c: &mut Criterion) {
    let (store, ids) = build_store();
    let read_iters = 200;

    c.bench_function("concurrent_read_write", |b| {
        b.iter_custom(|n| {
            let start = std::time::Instant::now();
            for _ in 0..n {
                let mut handles = Vec::with_capacity(4);

                // 1 writer thread
                {
                    let store = Arc::clone(&store);
                    handles.push(std::thread::spawn(move || {
                        for i in 0..50 {
                            let mut tx = store.begin_write();
                            let _ = GraphView::create_node(
                                &mut tx,
                                &["Temp"],
                                props(&[("w", PropertyValue::Int(i))]),
                            );
                            tx.commit().unwrap();
                        }
                    }));
                }

                // 3 reader threads
                for t in 0..3u64 {
                    let store = Arc::clone(&store);
                    let ids = ids.clone();
                    handles.push(std::thread::spawn(move || {
                        for j in 0..read_iters {
                            let tx = store.begin_read().unwrap();
                            let _ = GraphView::node_count(&tx);
                            let idx = ((t as usize) * read_iters + j) % ids.len();
                            let _ = GraphView::get_node(&tx, ids[idx]);
                        }
                    }));
                }

                for h in handles {
                    h.join().unwrap();
                }
            }
            start.elapsed()
        });
    });
}

criterion_group!(
    benches,
    bench_concurrent_reads_4,
    bench_concurrent_read_write
);
criterion_main!(benches);
