//! Parse-throughput benchmarks: wall-clock time of `traverse_gql::parse` over
//! representative query shapes.
//!
//! ```text
//! cargo bench -p traverse-gql --bench parse
//! ```
//!
//! Criterion writes per-iteration timings and summaries to
//! `target/criterion/`. Save a baseline with
//! `cargo bench -- --save-baseline main`; later runs compare against it with
//! `--baseline main` and flag >5% regressions.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;

fn bench_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("parse");

    // `(name, query)` cases; names sort lexicographically in the report, keeping related cases
    // together
    let cases: &[(&str, &str)] = &[
        // -- Trivial baseline --
        ("00_literal_return", "RETURN 1 AS x"),
        // -- Simple MATCH --
        ("10_single_node_match", "MATCH (n) RETURN n"),
        ("11_labeled_node_match", "MATCH (n:Person) RETURN n.name"),
        (
            "12_single_edge_match",
            "MATCH (a)-[r:KNOWS]->(b) RETURN a, b, r",
        ),
        // -- Variable-length edges --
        ("20_var_length_edge", "MATCH (a)-[:R*1..5]->(b) RETURN a, b"),
        // -- quantified path patterns --
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
        // -- Complex query bodies --
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
        // -- Expression-heavy --
        (
            "50_case_expression",
            "RETURN CASE WHEN 1 < 2 THEN 'a' WHEN 2 < 3 THEN 'b' ELSE 'c' END AS r",
        ),
        (
            "51_nested_function_calls",
            "RETURN toupper(substring(lower('Hello'), 1, 3)) AS r",
        ),
        // -- Catalog statements --
        (
            "60_create_graph_type",
            "CREATE GRAPH TYPE /t { NODE Person { name :: STRING, age :: INTEGER NOT NULL } }",
        ),
        // -- NEXT-piped multi-statement programs --
        (
            "70_next_pipe",
            "MATCH (n:N) RETURN n.x AS v NEXT MATCH (m:N) RETURN count(*) AS k",
        ),
        // -- WHEN statement --
        (
            "80_when_statement",
            "WHEN true THEN MATCH (n) RETURN n.name ELSE RETURN null AS n",
        ),
    ];

    for (name, query) in cases {
        group.bench_with_input(BenchmarkId::from_parameter(name), query, |b, q| {
            b.iter(|| {
                let _ = traverse_gql::parse(black_box(q));
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_parse);
criterion_main!(benches);
