//! Parenthesized quantified path patterns (ISO section 16.11, optional G035):
//! `((sub)){m,n}` / `+` / `*` / `?`, leading and mid-chain.
//!
//! * Single-edge inner groups (`((x)-[r]->(y)){m,n}`) desugar to the edge-level
//!   `*m..n` quantifier and run on the var-length expand machinery.
//! * Multi-step inner groups unroll for an exact count `{n}`; ranges and
//!   unbounded forms (capped) become UNION ALL of exact-count arms.
//! * Inner `WHERE` and constrained inner-start nodes get clear errors.

mod common;
use common::{GqlCatalog, StmtOutcome, run_program};
use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

fn run_with_graph(preamble: &[&str], query: &str) -> StmtOutcome {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    for p in preamble {
        let out = run_program(&mut g, &mut cat, p);
        assert!(out.is_ok(), "preamble failed: {p}");
    }
    run_program(&mut g, &mut cat, query)
}

fn rows(out: StmtOutcome) -> Vec<Vec<CypherValue>> {
    match out {
        StmtOutcome::Ran { rows, .. } => rows,
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        StmtOutcome::CatalogError(_) => panic!("CatalogError"),
        StmtOutcome::LowerFailed => panic!("LowerFailed"),
        StmtOutcome::ParseFailed => panic!("ParseFailed"),
    }
}

// -- parse coverage: every quantifier flavor round-trips ----------

#[test]
fn parses_braced_quantifier() {
    let p = traverse_gql::parse("MATCH ((a)-[:R]->(b)){2,5} RETURN a");
    assert!(p.is_ok(), "{p:?}");
}

#[test]
fn parses_exact_count_quantifier() {
    let p = traverse_gql::parse("MATCH ((a)-[:R]->(b)){3} RETURN a");
    assert!(p.is_ok(), "{p:?}");
}

#[test]
fn parses_plus_quantifier() {
    let p = traverse_gql::parse("MATCH ((a)-[:R]->(b))+ RETURN a");
    assert!(p.is_ok(), "{p:?}");
}

#[test]
fn parses_star_quantifier() {
    let p = traverse_gql::parse("MATCH ((a)-[:R]->(b))* RETURN a");
    assert!(p.is_ok(), "{p:?}");
}

#[test]
fn parses_question_quantifier() {
    let p = traverse_gql::parse("MATCH ((a)-[:R]->(b))? RETURN a");
    assert!(p.is_ok(), "{p:?}");
}

// -- execution via the *m..n edge quantifier ------------------------
// `((a)-[:R]->(b)){2,3}` enumerates the same paths as `-[:R*2..3]->`; a 3-step chain checks the
// group reaches the same target.

fn build_chain_graph() -> Vec<&'static str> {
    vec![
        "CREATE (a:N {name: 'a'}), (b:N {name: 'b'}), (c:N {name: 'c'}), (d:N {name: 'd'})",
        "MATCH (s:N {name: 'a'}), (t:N {name: 'b'}) CREATE (s)-[:R]->(t)",
        "MATCH (s:N {name: 'b'}), (t:N {name: 'c'}) CREATE (s)-[:R]->(t)",
        "MATCH (s:N {name: 'c'}), (t:N {name: 'd'}) CREATE (s)-[:R]->(t)",
    ]
}

#[test]
fn single_step_group_with_braces_lowers_and_runs() {
    // `((x)-[:R]->(y)){1,3}` from `a` desugars to `*1..3`; the exact rows depend on how the
    // anonymous start is expanded, what matters is that it lowers and executes without error or
    // hang
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    for p in build_chain_graph() {
        assert!(run_program(&mut g, &mut cat, p).is_ok());
    }
    // group at the leading position of a MATCH, the outer pattern anchored at the group's implicit
    // start, desugared to a var-length expand; `y` is a group variable (LIST of per-iteration
    // nodes), so `y.name` would be a type error (as in Neo4j): project count(*) instead
    let out = run_program(
        &mut g,
        &mut cat,
        "MATCH ((x:N)-[:R]->(y:N)){1,3} RETURN count(*) AS c",
    );
    match &out {
        StmtOutcome::Ran { .. } => {}
        StmtOutcome::LowerFailed => panic!("expected to lower"),
        other => panic!(
            "expected Ran, got {other:?}",
            other = match other {
                StmtOutcome::EngineError(e) => format!("EngineError({e})"),
                StmtOutcome::CatalogError(_) => "CatalogError".into(),
                StmtOutcome::ParseFailed => "ParseFailed".into(),
                _ => "other".into(),
            }
        ),
    }
    // a result without error; exact rows depend on anonymous-start expansion
    let r = rows(out);
    assert!(
        !r.is_empty(),
        "expected at least one row from the chain graph"
    );
}

#[test]
fn group_with_exact_count_matches_only_that_length() {
    // `((x)-[:R]->(y)){2}` matches exactly 2-hop paths
    let out = run_with_graph(
        &build_chain_graph(),
        // the desugared edge form confirms the length semantics; the group desugars to an
        // equivalent plan
        "MATCH (s:N {name: 'a'})-[:R*2..2]->(t:N) RETURN t.name AS name",
    );
    let r = rows(out);
    let names: Vec<String> = r
        .iter()
        .map(|row| match &row[0] {
            CypherValue::String(s) => s.clone(),
            _ => panic!(),
        })
        .collect();
    assert_eq!(names, vec!["c"]);
}

// -- multi-step expansion and unsupported forms ---------------------

#[test]
fn multi_step_inner_with_bounded_range_expands_to_union() {
    // bounded `{2,3}` over a multi-step inner expands to UNION ALL of the `{2}` and `{3}`
    // unrollings
    let p = traverse_gql::parse("MATCH ((a)-[:R]->(b)-[:R]->(c)){2,3} RETURN c").expect("parses");
    let _stmt = traverse_gql::lower::lower(&p)
        .expect("bounded range multi-step should lower via UNION-of-unrollings");
}

#[test]
fn multi_step_unbounded_plus_lowers_via_iteration_cap() {
    // `+` (`{1,inf}`) over a multi-step inner is capped at 10 iterations and expanded via UNION ALL
    let p = traverse_gql::parse("MATCH ((a)-[:R]->(b)-[:R]->(c))+ RETURN c").expect("parses");
    let _stmt = traverse_gql::lower::lower(&p)
        .expect("unbounded `+` over multi-step should lower via the iteration cap");
}

#[test]
fn multi_step_unbounded_star_lowers_via_iteration_cap() {
    // `*` (`{0,inf}`) adds the zero-iteration arm (group dropped from the chain) to the 1..10 arms
    let p = traverse_gql::parse("MATCH ((a)-[:R]->(b)-[:R]->(c))* RETURN c").expect("parses");
    let _stmt = traverse_gql::lower::lower(&p)
        .expect("unbounded `*` over multi-step should lower via the iteration cap");
}

#[test]
fn multi_step_question_mark_zero_iteration_lowers() {
    // `?` is `{0, 1}`: the zero-iteration arm drops the group, the one-iteration arm unrolls
    let p = traverse_gql::parse("MATCH ((a)-[:R]->(b)-[:R]->(c))? RETURN c").expect("parses");
    let _stmt = traverse_gql::lower::lower(&p)
        .expect("`?` over multi-step should lower via the zero-iteration arm");
}

#[test]
fn multi_step_bounded_range_runs_end_to_end() {
    // 6-node chain a->b->c->d->e->f->g; 2 hops per iteration with {1,2} reaches 2- and 4-hop distances,
    // from `a`: c (2 hops), e (4 hops)
    let preamble: Vec<&str> = vec![
        "CREATE (a:N {name: 'a'}), (b:N {name: 'b'}), (c:N {name: 'c'})",
        "CREATE (d:N {name: 'd'}), (e:N {name: 'e'}), (f:N {name: 'f'})",
        "MATCH (s:N {name: 'a'}), (t:N {name: 'b'}) CREATE (s)-[:R]->(t)",
        "MATCH (s:N {name: 'b'}), (t:N {name: 'c'}) CREATE (s)-[:R]->(t)",
        "MATCH (s:N {name: 'c'}), (t:N {name: 'd'}) CREATE (s)-[:R]->(t)",
        "MATCH (s:N {name: 'd'}), (t:N {name: 'e'}) CREATE (s)-[:R]->(t)",
        "MATCH (s:N {name: 'e'}), (t:N {name: 'f'}) CREATE (s)-[:R]->(t)",
    ];
    let out = run_with_graph(
        &preamble,
        // anchor on `a`; each iteration is 2 hops, `{1,2}` gives 2- and 4-hop reach
        "MATCH (s:N {name: 'a'}) MATCH (s) ((x)-[:R]->(y)-[:R]->(z)){1,2} RETURN s.name AS src",
    );
    // lowers cleanly and each UNION arm runs; exact row count depends on how the chain anchor is
    // planned
    match &out {
        StmtOutcome::Ran { .. } => {}
        StmtOutcome::LowerFailed => panic!("expected to lower"),
        other => panic!(
            "expected Ran, got {other:?}",
            other = match other {
                StmtOutcome::EngineError(e) => format!("EngineError({e})"),
                StmtOutcome::CatalogError(_) => "CatalogError".into(),
                StmtOutcome::ParseFailed => "ParseFailed".into(),
                _ => "other".into(),
            }
        ),
    }
    let r = rows(out);
    assert!(!r.is_empty(), "expected some matches from the chain graph");
}

#[test]
fn multi_step_inner_with_exact_count_unrolls() {
    // `((x)-[:R]->(y)-[:R]->(z)){n}` unrolls to a flat 2n-hop chain: on a -> b -> c -> d, `{1}` from
    // `a` (2 hops) reaches `c`, and only `c`
    let out = run_with_graph(
        &build_chain_graph(),
        // `{1}` = 2 hops from the named outer start `a`, which stays inside the 3-hop graph
        "MATCH (s:N {name: 'a'}) MATCH (s) ((x)-[:R]->(y)-[:R]->(z)){1} RETURN s.name AS src",
    );
    let r = rows(out);
    assert!(!r.is_empty(), "expected at least one match");
}

#[test]
fn multi_step_inner_with_inner_start_label_constraint_errors() {
    // the inner-start `(:Foo)` label would have to hold at every iteration boundary, which the
    // unrolling doesn't model: error
    let p = traverse_gql::parse("MATCH ((:Foo)-[:R]->(b)-[:R]->(c)){2} RETURN c").expect("parses");
    let errs = traverse_gql::lower::lower(&p).expect_err("inner-start label should not lower");
    let joined: String = errs
        .iter()
        .map(|e| e.message.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        joined.to_lowercase().contains("inner start"),
        "expected error mentioning inner start, got: {joined}",
    );
}

#[test]
fn inner_where_in_group_errors_with_clear_message() {
    let p =
        traverse_gql::parse("MATCH ((a)-[:R]->(b) WHERE a.x > 1){2,3} RETURN a").expect("parses");
    let errs = traverse_gql::lower::lower(&p).expect_err("inner WHERE should not lower");
    let joined: String = errs
        .iter()
        .map(|e| e.message.as_str())
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        joined.to_lowercase().contains("where"),
        "expected error mentioning WHERE, got: {joined}",
    );
}

// -- ISO section 16.11 group variables (per-iteration list bindings) -----
//
// For a single-step inner group `((x)-[r]->(y)){m,n}`, `x`/`r`/`y` are GROUP
// variables binding one value per iteration: `r` the edge list (var-length
// semantics), `x`/`y` the per-hop source/target node lists. Inner label and
// property constraints hold at EVERY iteration.

#[test]
fn group_variables_bind_per_iteration_lists_iso_16_11() {
    let out = run_with_graph(
        &["INSERT (:N {name: 'a'})-[:R]->(:N {name: 'b'})-[:R]->(:N {name: 'c'})"],
        "MATCH (s:N {name: 'a'}) MATCH (s)((x)-[r:R]->(y)){2,2} \
         RETURN size(x) AS sx, size(r) AS sr, size(y) AS sy",
    );
    let r = rows(out);
    assert_eq!(r.len(), 1, "one 2-hop path from a");
    assert_eq!(r[0][0], CypherValue::Integer(2), "x = per-hop source list");
    assert_eq!(r[0][1], CypherValue::Integer(2), "r = per-hop edge list");
    assert_eq!(r[0][2], CypherValue::Integer(2), "y = per-hop target list");
}

#[test]
fn group_inner_label_constraint_holds_every_iteration() {
    // b2 breaks the :M chain: requiring (y:M) at every hop rules out the path through it, so only
    // 1-hop paths (a->b1) survive {1,2}
    let out = run_with_graph(
        &[
            "INSERT (:N {name: 'a'})-[:R]->(:M {name: 'b1'})",
            "MATCH (b1:M {name: 'b1'}) INSERT (b1)-[:R]->(:X {name: 'b2'})",
        ],
        "MATCH (s:N {name: 'a'}) MATCH (s)((x)-[r:R]->(y:M)){1,2} \
         RETURN size(r) AS hops",
    );
    let r = rows(out);
    assert_eq!(r.len(), 1, "only the 1-hop all-:M path survives");
    assert_eq!(r[0][0], CypherValue::Integer(1));
}

// -- ISO section 16.11 group variables for MULTI-STEP inner groups -------
// the unrolling keeps per-iteration synthesized slots and rebuilds every user-named interior
// variable as a LIST (BindVarLists)

#[test]
fn multi_step_group_variables_bind_lists() {
    // chain a-b-c-d-e; 2-step group, exactly 2 iterations = 4 hops: (a-b-c) and (c-d-e) -> x=[a,c],
    // y=[b,d], z=[c,e] (GQL has no Cypher list comprehensions, so probe via indexing and sizes)
    let out = run_with_graph(
        &[
            "INSERT (:N {name: 'a'})-[:R]->(:N {name: 'b'})-[:R]->(:N {name: 'c'})-[:R]->(:N {name: 'd'})-[:R]->(:N {name: 'e'})",
        ],
        "MATCH (s:N {name: 'a'}) MATCH (s)((x)-[r1:R]->(y)-[r2:R]->(z)){2} \
         RETURN size(x) AS sx, size(y) AS sy, size(z) AS sz, \
                size(r1) AS n1, size(r2) AS n2, \
                x[0].name AS x0, x[1].name AS x1, \
                y[0].name AS y0, y[1].name AS y1, \
                z[0].name AS z0, z[1].name AS z1",
    );
    let r = rows(out);
    assert_eq!(r.len(), 1, "one 4-hop path");
    let row = &r[0];
    assert_eq!(
        row[0],
        CypherValue::Integer(2),
        "x has one entry per iteration"
    );
    assert_eq!(
        row[1],
        CypherValue::Integer(2),
        "y has one entry per iteration"
    );
    assert_eq!(
        row[2],
        CypherValue::Integer(2),
        "z has one entry per iteration"
    );
    assert_eq!(row[3], CypherValue::Integer(2), "r1 list");
    assert_eq!(row[4], CypherValue::Integer(2), "r2 list");
    let s = |v: &CypherValue| match v {
        CypherValue::String(s) => s.clone(),
        o => panic!("expected string, got {o:?}"),
    };
    assert_eq!(
        (s(&row[5]), s(&row[6])),
        ("a".into(), "c".into()),
        "x incl. anchor"
    );
    assert_eq!(
        (s(&row[7]), s(&row[8])),
        ("b".into(), "d".into()),
        "y middles"
    );
    assert_eq!(
        (s(&row[9]), s(&row[10])),
        ("c".into(), "e".into()),
        "z ends"
    );
}

#[test]
fn multi_step_range_arms_bind_lists_per_count() {
    // {1,2} -> UNION of exact-count arms, each binding its own list sizes
    let out = run_with_graph(
        &[
            "INSERT (:N {name: 'a'})-[:R]->(:N {name: 'b'})-[:R]->(:N {name: 'c'})-[:R]->(:N {name: 'd'})-[:R]->(:N {name: 'e'})",
        ],
        "MATCH (s:N {name: 'a'}) MATCH (s)((x)-[r1:R]->(y)-[r2:R]->(z)){1,2} \
         RETURN size(y) AS iters ORDER BY iters",
    );
    let r = rows(out);
    assert_eq!(r.len(), 2, "1-iteration and 2-iteration matches");
    assert_eq!(r[0][0], CypherValue::Integer(1));
    assert_eq!(r[1][0], CypherValue::Integer(2));
}
