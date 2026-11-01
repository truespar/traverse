use std::collections::BTreeMap;

use criterion::{Criterion, criterion_group, criterion_main};
use traverse_core::graph::Graph;
use traverse_core::graph::property::PropertyValue;
use traverse_core::tx::MvccStore;

fn props(pairs: &[(&str, PropertyValue)]) -> BTreeMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// Build a graph with 1,000 Person nodes and 2,000 KNOWS edges.
fn build_graph() -> MvccStore {
    let mut g = Graph::new();
    let mut node_ids = Vec::with_capacity(1_000);
    for i in 0..1_000 {
        let id = g.create_node(
            &["Person"],
            props(&[("name", PropertyValue::String(format!("user_{i}")))]),
        );
        node_ids.push(id);
    }
    for i in 0..2_000 {
        let src = node_ids[i % 1_000];
        let tgt = node_ids[(i + 1) % 1_000];
        g.create_edge(src, tgt, "KNOWS", BTreeMap::new()).unwrap();
    }
    MvccStore::new(g)
}

fn bench_query_match_label(c: &mut Criterion) {
    let store = build_graph();
    c.bench_function("query_match_label", |b| {
        b.iter(|| {
            let mut tx = store.begin_write();
            let result =
                traverse_cypher::execute(&mut tx, "MATCH (n:Person) RETURN count(n)").unwrap();
            assert_eq!(result.rows.len(), 1);
        });
    });
}

fn bench_query_match_property(c: &mut Criterion) {
    let store = build_graph();
    c.bench_function("query_match_property", |b| {
        b.iter(|| {
            let mut tx = store.begin_write();
            let result = traverse_cypher::execute(
                &mut tx,
                "MATCH (n:Person {name: 'user_500'}) RETURN n.name",
            )
            .unwrap();
            assert_eq!(result.rows.len(), 1);
        });
    });
}

fn bench_query_traversal(c: &mut Criterion) {
    let store = build_graph();
    c.bench_function("query_traversal", |b| {
        b.iter(|| {
            let mut tx = store.begin_write();
            let result = traverse_cypher::execute(
                &mut tx,
                "MATCH (a:Person {name: 'user_0'})-[:KNOWS]->(b) RETURN b.name",
            )
            .unwrap();
            assert!(!result.rows.is_empty());
        });
    });
}

criterion_group!(
    benches,
    bench_query_match_label,
    bench_query_match_property,
    bench_query_traversal
);
criterion_main!(benches);
