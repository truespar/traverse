//! Quantified-path-pattern lowering benchmark.
//!
//! Bounded ranges and capped unbounded quantifiers over multi-step groups
//! unroll into UNION ALL of exact-count variants: `{2,5}` gives 4 arms, `+`
//! gives `UNBOUNDED_ITERATION_CAP = 10`. Both `expand_range_quantifiers` and the
//! lowering after it are linear in the arm count; this pins that cost.
//!
//! ```text
//! cargo bench -p traverse-gql --bench quantified_paths
//! ```

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use std::hint::black_box;

fn parse_and_lower(q: &str) {
    let p = traverse_gql::parse(q).expect("benchmark inputs must parse");
    let _ = traverse_gql::lower::lower(&p);
}

fn bench_quantified_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("quantified_paths");

    // -- single-edge inner: desugars to an edge quantifier -- constant in `n` (always one Cypher
    // `*m..n`); the baseline for the multi-step cases
    group.bench_function(BenchmarkId::new("single_edge_inner", "1..10"), |b| {
        b.iter(|| {
            parse_and_lower(black_box("MATCH ((x)-[:R]->(y)){1,10} RETURN y"));
        });
    });

    // -- multi-step exact count {n}: unrolls n times -- linear in n, 2 PatternChain entries per
    // iteration (2-hop inner pattern)
    for n in &[1usize, 3, 5, 8, 10] {
        let q = format!("MATCH ((x)-[:R]->(y)-[:R]->(z)){{{n}}} RETURN z");
        group.bench_with_input(BenchmarkId::new("multi_step_exact", n), &q, |b, q| {
            b.iter(|| parse_and_lower(black_box(q)))
        });
    }

    // -- multi-step bounded range {m,n}: UNION of unrollings -- linear in (n - m + 1) arms, each a
    // copy of the query body with the quantifier set to {k, k}; scales with arm count and body size
    for &(m, n) in &[(1usize, 3), (1, 5), (1, 8), (2, 6), (3, 8)] {
        let q = format!("MATCH ((x)-[:R]->(y)-[:R]->(z)){{{m},{n}}} RETURN z");
        let label = format!("{m}_{n}");
        group.bench_with_input(BenchmarkId::new("multi_step_range", &label), &q, |b, q| {
            b.iter(|| parse_and_lower(black_box(q)))
        });
    }

    // -- multi-step unbounded `+` / `*`, capped at 10 -- same as {1,10} / {0,10}, so both cost a
    // 10-arm explicit range (confirms the cap holds)
    for q in &[
        ("plus", "MATCH ((x)-[:R]->(y)-[:R]->(z))+ RETURN z"),
        ("star", "MATCH ((x)-[:R]->(y)-[:R]->(z))* RETURN z"),
    ] {
        group.bench_with_input(
            BenchmarkId::new("multi_step_unbounded", q.0),
            q.1,
            |b, q| b.iter(|| parse_and_lower(black_box(q))),
        );
    }
    group.finish();
}

criterion_group!(benches, bench_quantified_paths);
criterion_main!(benches);
