//! Lowering-throughput benchmarks: wall-clock time of `traverse_gql::lower::lower`
//! over representative queries, each parsed once outside the loop so only
//! lowering is measured.
//!
//! ```text
//! cargo bench -p traverse-gql --bench lower
//! ```

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;

fn bench_lower(c: &mut Criterion) {
    let mut group = c.benchmark_group("lower");

    // the parse.rs shapes minus catalog statements, which intentionally don't lower (they go
    // through a catalog)
    let cases: &[(&str, &str)] = &[
        ("00_literal_return", "RETURN 1 AS x"),
        ("10_single_node_match", "MATCH (n) RETURN n"),
        ("11_labeled_node_match", "MATCH (n:Person) RETURN n.name"),
        (
            "12_single_edge_match",
            "MATCH (a)-[r:KNOWS]->(b) RETURN a, b, r",
        ),
        ("20_var_length_edge", "MATCH (a)-[:R*1..5]->(b) RETURN a, b"),
        (
            "30_quantified_exact_count",
            "MATCH ((x)-[:R]->(y)-[:R]->(z)){3} RETURN z",
        ),
        (
            "31_quantified_bounded_range",
            "MATCH ((x)-[:R]->(y)-[:R]->(z)){2,4} RETURN z",
        ),
        (
            "32_quantified_unbounded",
            "MATCH ((x)-[:R]->(y)-[:R]->(z))+ RETURN z",
        ),
        (
            "40_with_clause_chain",
            "MATCH (n) WITH n, count(*) AS c WHERE c > 1 RETURN n, c ORDER BY c DESC LIMIT 10",
        ),
        (
            "41_call_subquery",
            "MATCH (o:Org) CALL (o) { MATCH (p:Person)-[:IN]->(o) RETURN count(p) AS members } \
             RETURN o.name AS org, members",
        ),
        (
            "42_union",
            "MATCH (n:A) RETURN n.x AS v UNION ALL MATCH (n:B) RETURN n.x AS v",
        ),
        (
            "50_case_expression",
            "RETURN CASE WHEN 1 < 2 THEN 'a' WHEN 2 < 3 THEN 'b' ELSE 'c' END AS r",
        ),
        (
            "51_nested_function_calls",
            "RETURN toupper(substring(lower('Hello'), 1, 3)) AS r",
        ),
        (
            "70_next_pipe",
            "MATCH (n:N) RETURN n.x AS v NEXT MATCH (m:N) RETURN count(*) AS k",
        ),
    ];

    for (name, query) in cases {
        // parse outside the measured loop
        let program = match traverse_gql::parse(query) {
            Ok(p) => p,
            Err(_) => continue,
        };
        group.bench_with_input(BenchmarkId::from_parameter(name), &program, |b, p| {
            b.iter(|| {
                let _ = traverse_gql::lower::lower(black_box(p));
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_lower);
criterion_main!(benches);
