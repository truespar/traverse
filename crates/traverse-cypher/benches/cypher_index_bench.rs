use std::collections::BTreeMap;

use criterion::{Criterion, criterion_group, criterion_main};
use traverse_core::graph::Graph;
use traverse_core::graph::property::PropertyValue;

fn make_graph(n: usize) -> Graph {
    let mut g = Graph::new();
    for i in 0..n {
        g.create_node(
            &["Person"],
            BTreeMap::from([(
                "name".to_string(),
                PropertyValue::String(format!("user_{i}")),
            )]),
        );
    }
    g
}

fn bench_cypher_no_index(c: &mut Criterion) {
    let mut g = make_graph(10_000);
    c.bench_function("cypher_10k_no_index", |b| {
        b.iter(|| {
            traverse_cypher::execute(&mut g, "MATCH (n:Person {name: 'user_5000'}) RETURN n.name")
                .unwrap()
        });
    });
}

fn bench_cypher_with_index(c: &mut Criterion) {
    let mut g = make_graph(10_000);
    g.create_index("idx_name", "Person", "name").unwrap();
    c.bench_function("cypher_10k_with_index", |b| {
        b.iter(|| {
            traverse_cypher::execute(&mut g, "MATCH (n:Person {name: 'user_5000'}) RETURN n.name")
                .unwrap()
        });
    });
}

criterion_group!(benches, bench_cypher_no_index, bench_cypher_with_index);
criterion_main!(benches);
