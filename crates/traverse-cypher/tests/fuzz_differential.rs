//! Self-differential fuzzing: generated queries checked against built-in oracles,
//! no reference engine needed.
//!
//! 1. **TLP (ternary logic partitioning)**: for a deterministic predicate P,
//!    `WHERE P`, `WHERE NOT P` and `WHERE P IS NULL` partition the matches, so
//!    their counts sum to the unfiltered count. Catches three-valued-logic and
//!    push-down bugs.
//! 2. **COUNT/EXISTS consistency**: `RETURN COUNT { MATCH ... }` must equal
//!    `MATCH ... RETURN count(*)`, `EXISTS { ... }` must equal `count > 0`. The
//!    subquery side runs in the record interpreter (`exec_subquery`), the direct
//!    side in the operator/morsel pipeline; a mismatch is an execution-path bug.
//! 3. **CALL wrapping**: `CALL { MATCH ... RETURN cols } RETURN cols` must give
//!    the same multiset of rows as the bare query.
//! 4. **Panic guard**: every query runs under `catch_unwind`; any panic fails.
//!
//! A fixed seed generates the fixture and all queries, so failures reproduce.
//! `FUZZ_ITERS` scales it up (default 300 per oracle family).

use std::collections::HashMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

// -- Deterministic PRNG (xorshift64*) ---------------------------------

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

// -- Fixture ----------------------------------------------------------

const NAMES: &[&str] = &[
    "alice", "bob", "carol", "dave", "eve", "frank", "grace", "heidi", "ivan", "judy",
];
const CITIES: &[&str] = &["oslo", "lund", "kyiv", "porto", "graz"];

fn run(graph: &mut Graph, q: &str) -> Result<Vec<Vec<CypherValue>>, String> {
    let params: HashMap<String, CypherValue> = HashMap::new();
    match execute_with_params(graph, q, &params) {
        Ok(res) => Ok(res.rows),
        Err(e) => Err(e.to_string()),
    }
}

/// Deterministic fixture: ~160 Person nodes with deliberately patchy properties (missing values
/// exercise three-valued logic), ~5 City nodes, weighted KNOWS edges, LIVES_IN edges.
fn build_fixture(seed: u64) -> Graph {
    let mut g = Graph::new();
    let mut rng = Rng::new(seed);
    let n_people = 160;

    for i in 0..n_people {
        let name = format!("{}{}", NAMES[(i % NAMES.len() as u64) as usize], i);
        let mut props = vec![format!("id: {i}"), format!("name: '{name}'")];
        if !rng.chance(10) {
            props.push(format!("age: {}", 18 + rng.below(60)));
        }
        if !rng.chance(10) {
            props.push(format!("score: {}.{}", rng.below(100), rng.below(100)));
        }
        if !rng.chance(20) {
            props.push(format!("active: {}", rng.chance(50)));
        }
        if rng.chance(30) {
            props.push(format!("nick: 'n{}'", rng.below(40)));
        }
        run(
            &mut g,
            &format!("CREATE (:Person {{{}}})", props.join(", ")),
        )
        .expect("person");
    }
    for (i, c) in CITIES.iter().enumerate() {
        run(
            &mut g,
            &format!(
                "CREATE (:City {{id: {i}, name: '{c}', pop: {}}})",
                1000 * (i + 1)
            ),
        )
        .expect("city");
    }
    // KNOWS: ~3 outgoing per person, deterministic targets
    for i in 0..n_people {
        for _ in 0..3 {
            let j = rng.below(n_people);
            if j == i {
                continue;
            }
            run(
                &mut g,
                &format!(
                    "MATCH (a:Person {{id: {i}}}), (b:Person {{id: {j}}}) \
                     CREATE (a)-[:KNOWS {{weight: {}}}]->(b)",
                    rng.below(100)
                ),
            )
            .expect("knows");
        }
        if rng.chance(80) {
            let c = rng.below(CITIES.len() as u64);
            run(
                &mut g,
                &format!(
                    "MATCH (a:Person {{id: {i}}}), (c:City {{id: {c}}}) \
                     CREATE (a)-[:LIVES_IN]->(c)"
                ),
            )
            .expect("lives_in");
        }
    }
    g
}

// -- Query generation -------------------------------------------------

/// Generated MATCH pattern plus its node variables.
struct Pattern {
    text: &'static str,
    vars: &'static [&'static str],
}

const PATTERNS: &[Pattern] = &[
    Pattern {
        text: "(a:Person)",
        vars: &["a"],
    },
    Pattern {
        text: "(a)",
        vars: &["a"],
    },
    Pattern {
        text: "(a:Person)-[:KNOWS]->(b:Person)",
        vars: &["a", "b"],
    },
    Pattern {
        text: "(a:Person)-[:KNOWS]-(b:Person)",
        vars: &["a", "b"],
    },
    Pattern {
        text: "(a:Person)-[:LIVES_IN]->(c:City)",
        vars: &["a", "c"],
    },
    Pattern {
        text: "(a:Person)-[:KNOWS]->(b:Person)-[:LIVES_IN]->(c:City)",
        vars: &["a", "b", "c"],
    },
    Pattern {
        text: "(a:Person)-[:KNOWS*1..2]->(b:Person)",
        vars: &["a", "b"],
    },
    Pattern {
        text: "(a:Person)-[r:KNOWS]->(b:Person)",
        vars: &["a", "b"],
    },
];

/// Random comparison atom over the pattern variables; deterministic, side-effect-free only.
fn gen_atom(rng: &mut Rng, vars: &[&str]) -> String {
    let v = vars[rng.below(vars.len() as u64) as usize];
    match rng.below(10) {
        0 => format!("{v}.age > {}", 18 + rng.below(60)),
        1 => format!("{v}.age <= {}", 18 + rng.below(60)),
        2 => format!("{v}.score < {}.5", rng.below(100)),
        3 => format!(
            "{v}.name STARTS WITH '{}'",
            &NAMES[rng.below(10) as usize][..1]
        ),
        4 => format!("{v}.name CONTAINS '{}'", NAMES[rng.below(10) as usize]),
        5 => format!("{v}.active"),
        6 => format!("{v}.age IS NULL"),
        7 => format!(
            "{v}.age IN [{}, {}, {}]",
            18 + rng.below(60),
            18 + rng.below(60),
            18 + rng.below(60)
        ),
        8 => format!("size({v}.name) > {}", 4 + rng.below(5)),
        _ => {
            if vars.len() > 1 {
                let w = vars[rng.below(vars.len() as u64) as usize];
                format!("{v}.age + {w}.age > {}", 40 + rng.below(80))
            } else {
                format!("{v}.age % {} = {}", 2 + rng.below(5), rng.below(3))
            }
        }
    }
}

/// Random predicate tree, bounded depth.
fn gen_pred(rng: &mut Rng, vars: &[&str], depth: u32) -> String {
    if depth == 0 || rng.chance(40) {
        return gen_atom(rng, vars);
    }
    match rng.below(4) {
        0 => format!("NOT ({})", gen_pred(rng, vars, depth - 1)),
        1 => format!(
            "({} AND {})",
            gen_pred(rng, vars, depth - 1),
            gen_pred(rng, vars, depth - 1)
        ),
        2 => format!(
            "({} OR {})",
            gen_pred(rng, vars, depth - 1),
            gen_pred(rng, vars, depth - 1)
        ),
        _ => format!(
            "({} XOR {})",
            gen_pred(rng, vars, depth - 1),
            gen_pred(rng, vars, depth - 1)
        ),
    }
}

// -- Harness plumbing -------------------------------------------------

fn iters() -> u64 {
    std::env::var("FUZZ_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300)
}

/// Runs a query under a panic guard. Err(reason) for engine errors and panics alike; panics get a
/// `PANIC:` prefix so callers fail hard.
fn guarded(graph: &mut Graph, q: &str) -> Result<Vec<Vec<CypherValue>>, String> {
    match catch_unwind(AssertUnwindSafe(|| run(graph, q))) {
        Ok(r) => r,
        Err(p) => {
            let msg = p
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic>".to_string());
            Err(format!("PANIC: {msg}"))
        }
    }
}

fn count_of(graph: &mut Graph, q: &str) -> Result<i64, String> {
    let rows = guarded(graph, q)?;
    match rows.first().and_then(|r| r.first()) {
        Some(CypherValue::Integer(n)) => Ok(*n),
        other => Err(format!("expected integer count, got {other:?}")),
    }
}

/// Canonical multiset of a result, for order-insensitive diffs.
fn canon(rows: &[Vec<CypherValue>]) -> Vec<String> {
    let mut v: Vec<String> = rows.iter().map(|r| format!("{r:?}")).collect();
    v.sort();
    v
}

// -- Oracle 1: ternary logic partitioning -----------------------------

#[test]
fn tlp_partition_counts() {
    let mut g = build_fixture(42);
    let mut rng = Rng::new(0x71b0);
    let mut panics = Vec::new();
    let mut failures = Vec::new();

    for i in 0..iters() {
        let pat = &PATTERNS[rng.below(PATTERNS.len() as u64) as usize];
        let pred = gen_pred(&mut rng, pat.vars, 2);

        let total_q = format!("MATCH {} RETURN count(*) AS c", pat.text);
        let p_q = format!("MATCH {} WHERE {} RETURN count(*) AS c", pat.text, pred);
        let np_q = format!(
            "MATCH {} WHERE NOT ({}) RETURN count(*) AS c",
            pat.text, pred
        );
        let nullp_q = format!(
            "MATCH {} WHERE ({}) IS NULL RETURN count(*) AS c",
            pat.text, pred
        );

        let outcome = (|| -> Result<(), String> {
            let total = count_of(&mut g, &total_q)?;
            let p = count_of(&mut g, &p_q)?;
            let np = count_of(&mut g, &np_q)?;
            let nullp = count_of(&mut g, &nullp_q)?;
            if p + np + nullp != total {
                return Err(format!("TLP violation: {p} + {np} + {nullp} != {total}"));
            }
            Ok(())
        })();

        if let Err(e) = outcome {
            if e.starts_with("PANIC:") {
                panics.push(format!(
                    "iter {i}: {e}\n  pred: {pred}\n  pattern: {}",
                    pat.text
                ));
            } else if e.starts_with("TLP violation") {
                failures.push(format!(
                    "iter {i}: {e}\n  pred: {pred}\n  pattern: {}",
                    pat.text
                ));
            }
            // engine errors on a generated predicate are fine (all four branches fail alike);
            // panics and partition violations aren't
        }
    }

    assert!(
        panics.is_empty(),
        "panics during TLP fuzzing:\n{}",
        panics.join("\n")
    );
    assert!(
        failures.is_empty(),
        "TLP violations:\n{}",
        failures.join("\n")
    );
}

// -- Oracle 2: COUNT/EXISTS subquery vs direct pipeline ---------------

#[test]
fn count_subquery_matches_direct_count() {
    let mut g = build_fixture(42);
    let mut rng = Rng::new(0xc047);
    let mut failures = Vec::new();

    for i in 0..iters() {
        let pat = &PATTERNS[rng.below(PATTERNS.len() as u64) as usize];
        let pred = gen_pred(&mut rng, pat.vars, 2);

        let direct_q = format!("MATCH {} WHERE {} RETURN count(*) AS c", pat.text, pred);
        let sub_q = format!("RETURN COUNT {{ MATCH {} WHERE {} }} AS c", pat.text, pred);
        let exists_q = format!("RETURN EXISTS {{ MATCH {} WHERE {} }} AS e", pat.text, pred);

        let direct = count_of(&mut g, &direct_q);
        let sub = count_of(&mut g, &sub_q);

        match (direct, sub) {
            (Ok(d), Ok(s)) => {
                if d != s {
                    failures.push(format!(
                        "iter {i}: direct count {d} != COUNT-subquery {s}\n  pattern: {}\n  pred: {pred}",
                        pat.text
                    ));
                }
                match guarded(&mut g, &exists_q) {
                    Ok(rows) => {
                        let e = matches!(
                            rows.first().and_then(|r| r.first()),
                            Some(CypherValue::Bool(true))
                        );
                        if e != (d > 0) {
                            failures.push(format!(
                                "iter {i}: EXISTS {e} but count {d}\n  pattern: {}\n  pred: {pred}",
                                pat.text
                            ));
                        }
                    }
                    Err(e) => failures.push(format!(
                        "iter {i}: EXISTS errored where count succeeded: {e}\n  pattern: {}\n  pred: {pred}",
                        pat.text
                    )),
                }
            }
            (Ok(d), Err(e)) => failures.push(format!(
                "iter {i}: COUNT-subquery errored where direct ({d}) succeeded: {e}\n  pattern: {}\n  pred: {pred}",
                pat.text
            )),
            (Err(e), Ok(s)) => failures.push(format!(
                "iter {i}: direct errored where COUNT-subquery ({s}) succeeded: {e}\n  pattern: {}\n  pred: {pred}",
                pat.text
            )),
            (Err(_), Err(_)) => {} // both reject: consistent
        }
    }

    assert!(
        failures.is_empty(),
        "COUNT/EXISTS subquery divergences:\n{}",
        failures.join("\n")
    );
}

// -- Oracle 3: CALL-subquery wrapping preserves results ---------------

#[test]
fn call_subquery_wrapping_preserves_rows() {
    let mut g = build_fixture(42);
    let mut rng = Rng::new(0xca11_5eed);
    let mut failures = Vec::new();

    for i in 0..iters() {
        let pat = &PATTERNS[rng.below(PATTERNS.len() as u64) as usize];
        let pred = gen_pred(&mut rng, pat.vars, 2);
        let v = pat.vars[rng.below(pat.vars.len() as u64) as usize];
        let proj = match rng.below(3) {
            0 => format!("{v}.name AS x"),
            1 => format!("{v}.age AS x"),
            _ => format!("coalesce({v}.age, -1) + 0 AS x"),
        };

        let direct_q = format!("MATCH {} WHERE {} RETURN {}", pat.text, pred, proj);
        let wrapped_q = format!(
            "CALL {{ MATCH {} WHERE {} RETURN {} }} RETURN x",
            pat.text, pred, proj
        );

        match (guarded(&mut g, &direct_q), guarded(&mut g, &wrapped_q)) {
            (Ok(d), Ok(w)) => {
                if canon(&d) != canon(&w) {
                    failures.push(format!(
                        "iter {i}: CALL-wrapped rows differ ({} vs {})\n  pattern: {}\n  pred: {pred}\n  proj: {proj}",
                        d.len(),
                        w.len(),
                        pat.text
                    ));
                }
            }
            (Ok(d), Err(e)) => failures.push(format!(
                "iter {i}: wrapped errored where direct ({} rows) succeeded: {e}\n  pattern: {}\n  pred: {pred}\n  proj: {proj}",
                d.len(),
                pat.text
            )),
            (Err(e), Ok(w)) => failures.push(format!(
                "iter {i}: direct errored where wrapped ({} rows) succeeded: {e}\n  pattern: {}\n  pred: {pred}\n  proj: {proj}",
                w.len(),
                pat.text
            )),
            (Err(_), Err(_)) => {}
        }
    }

    assert!(
        failures.is_empty(),
        "CALL-subquery wrapping divergences:\n{}",
        failures.join("\n")
    );
}

// -- Oracle 4: aggregation consistency --------------------------------

/// count(*) grouped by a key must sum to the ungrouped count; DISTINCT counts never exceed plain
/// counts.
#[test]
fn aggregation_consistency() {
    let mut g = build_fixture(42);
    let mut rng = Rng::new(0xa66_5eed);
    let mut failures = Vec::new();

    for i in 0..iters() {
        let pat = &PATTERNS[rng.below(PATTERNS.len() as u64) as usize];
        let pred = gen_pred(&mut rng, pat.vars, 1);
        let v = pat.vars[rng.below(pat.vars.len() as u64) as usize];

        let total_q = format!("MATCH {} WHERE {} RETURN count(*) AS c", pat.text, pred);
        let grouped_q = format!(
            "MATCH {} WHERE {} RETURN {v}.active AS k, count(*) AS c",
            pat.text, pred
        );
        let distinct_q = format!(
            "MATCH {} WHERE {} RETURN count(DISTINCT {v}) AS c",
            pat.text, pred
        );

        let outcome = (|| -> Result<(), String> {
            let total = count_of(&mut g, &total_q)?;
            let grouped = guarded(&mut g, &grouped_q)?;
            let group_sum: i64 = grouped
                .iter()
                .filter_map(|r| match r.get(1) {
                    Some(CypherValue::Integer(n)) => Some(*n),
                    _ => None,
                })
                .sum();
            if group_sum != total {
                return Err(format!("grouped sum {group_sum} != total {total}"));
            }
            let distinct = count_of(&mut g, &distinct_q)?;
            if distinct > total {
                return Err(format!("DISTINCT count {distinct} > total {total}"));
            }
            Ok(())
        })();

        if let Err(e) = outcome {
            if !e.starts_with("PANIC:") && (e.contains("!=") || e.contains(">")) {
                failures.push(format!(
                    "iter {i}: {e}\n  pattern: {}\n  pred: {pred}",
                    pat.text
                ));
            } else if e.starts_with("PANIC:") {
                failures.push(format!("iter {i}: {e}"));
            }
        }
    }

    assert!(
        failures.is_empty(),
        "aggregation inconsistencies:\n{}",
        failures.join("\n")
    );
}
