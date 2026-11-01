use std::collections::BTreeMap;

use criterion::{Criterion, criterion_group, criterion_main};
use traverse_core::graph::Graph;
use traverse_core::graph::property::{IndexKey, PropertyValue};
use traverse_core::graph::types::PropertyKeyId;

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

fn bench_scan_no_index(c: &mut Criterion) {
    let g = make_graph(100_000);
    let target = "user_50000";
    let name_key = PropertyKeyId(g.strings().get("name").unwrap());
    c.bench_function("scan_100k_no_index", |b| {
        b.iter(|| {
            let mut found = Vec::new();
            for node in g.nodes() {
                if let Some(PropertyValue::String(name)) = node.properties.get(name_key)
                    && name == target
                {
                    found.push(node.id);
                }
            }
            found
        });
    });
}

fn bench_index_lookup(c: &mut Criterion) {
    let mut g = make_graph(100_000);
    g.create_index("idx_name", "Person", "name").unwrap();
    let key = IndexKey::String("user_50000".to_string());
    c.bench_function("index_100k_lookup", |b| {
        b.iter(|| g.index_lookup("Person", "name", &key));
    });
}

criterion_group!(benches, bench_scan_no_index, bench_index_lookup);
criterion_main!(benches);
