use std::collections::BTreeMap;

use criterion::{Criterion, criterion_group, criterion_main};
use traverse_core::graph::property::PropertyValue;
use traverse_core::storage::Database;
use traverse_core::tx::GraphView;

fn props(pairs: &[(&str, PropertyValue)]) -> BTreeMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn bench_insert_1k_nodes(c: &mut Criterion) {
    c.bench_function("insert_1k_nodes", |b| {
        b.iter(|| {
            let dir = tempfile::tempdir().unwrap();
            let db = Database::open(dir.path().join("bench.tvdb")).unwrap();
            let mut tx = db.begin_write();
            for i in 0..1_000 {
                let _ = GraphView::create_node(
                    &mut tx,
                    &["Person"],
                    props(&[
                        ("name", PropertyValue::String(format!("user_{i}"))),
                        ("age", PropertyValue::Int(i)),
                    ]),
                );
            }
            tx.commit().unwrap();
        });
    });
}

fn bench_insert_1k_edges(c: &mut Criterion) {
    c.bench_function("insert_1k_edges", |b| {
        b.iter(|| {
            let dir = tempfile::tempdir().unwrap();
            let db = Database::open(dir.path().join("bench.tvdb")).unwrap();

            // Pre-populate 1,000 nodes
            let mut tx = db.begin_write();
            let mut node_ids = Vec::with_capacity(1_000);
            for i in 0..1_000 {
                let id = GraphView::create_node(
                    &mut tx,
                    &["Person"],
                    props(&[("name", PropertyValue::String(format!("user_{i}")))]),
                )
                .unwrap();
                node_ids.push(id);
            }
            tx.commit().unwrap();

            // Insert 1,000 edges
            let mut tx = db.begin_write();
            for i in 0..1_000 {
                let src = node_ids[i];
                let tgt = node_ids[(i + 1) % 1_000];
                GraphView::create_edge(
                    &mut tx,
                    src,
                    tgt,
                    "KNOWS",
                    props(&[("weight", PropertyValue::Int(i as i64))]),
                )
                .unwrap();
            }
            tx.commit().unwrap();
        });
    });
}

fn bench_insert_bulk_10k(c: &mut Criterion) {
    c.bench_function("insert_bulk_10k_nodes", |b| {
        b.iter(|| {
            let dir = tempfile::tempdir().unwrap();
            let db = Database::open(dir.path().join("bench.tvdb")).unwrap();
            let mut tx = db.begin_write();
            for i in 0..10_000 {
                let _ = GraphView::create_node(
                    &mut tx,
                    &["Item"],
                    props(&[
                        ("index", PropertyValue::Int(i)),
                        ("data", PropertyValue::String(format!("payload_{i}"))),
                    ]),
                );
            }
            tx.commit().unwrap();
        });
    });
}

criterion_group!(
    benches,
    bench_insert_1k_nodes,
    bench_insert_1k_edges,
    bench_insert_bulk_10k
);
criterion_main!(benches);
