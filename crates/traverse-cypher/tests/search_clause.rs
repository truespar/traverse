//! Cypher 25 `SEARCH` clause (Neo4j 2026.02+) end to end:
//! `MATCH (n) SEARCH n IN (VECTOR INDEX idx FOR qv [WHERE pred] LIMIT k)
//!  [SCORE AS s]`, ANN vector search as a MATCH subclause, replacing the
//! deprecated `db.index.vector.queryNodes` procedure.
//!
//! Mirrors the examples on the Neo4j Cypher manual's SEARCH page.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run_params(
    graph: &mut Graph,
    q: &str,
    params: &HashMap<String, CypherValue>,
) -> Result<Vec<Vec<CypherValue>>, String> {
    execute_with_params(graph, q, params)
        .map(|r| r.rows)
        .map_err(|e| e.to_string())
}

fn run(graph: &mut Graph, q: &str) -> Result<Vec<Vec<CypherValue>>, String> {
    run_params(graph, q, &HashMap::new())
}

fn ok(graph: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    run(graph, q).unwrap_or_else(|e| panic!("query failed: {q}\n{e}"))
}

fn titles(rows: &[Vec<CypherValue>]) -> Vec<String> {
    rows.iter()
        .map(|r| match &r[0] {
            CypherValue::String(s) => s.clone(),
            o => panic!("expected string, got {o:?}"),
        })
        .collect()
}

/// Tiny movie corpus with 4-dim embeddings in distinct directions, so cosine ranking is
/// unambiguous: `alpha` nearly parallel to `alpha2`, `beta`/`gamma` elsewhere.
fn corpus() -> Graph {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE VECTOR INDEX moviePlots FOR (m:Movie) ON (m.embedding) \
         OPTIONS { indexConfig: { 'vector.dimensions': 4, 'vector.similarity_function': 'cosine' } }",
    );
    ok(
        &mut g,
        "CREATE (:Movie {title: 'alpha',  rating: 9.0, year: 2001, embedding: [1.0, 0.0, 0.0, 0.0]})",
    );
    ok(
        &mut g,
        "CREATE (:Movie {title: 'alpha2', rating: 6.0, year: 1985, embedding: [0.9, 0.1, 0.0, 0.0]})",
    );
    ok(
        &mut g,
        "CREATE (:Movie {title: 'beta',   rating: 8.0, year: 1999, embedding: [0.0, 1.0, 0.0, 0.0]})",
    );
    ok(
        &mut g,
        "CREATE (:Movie {title: 'gamma',  rating: 7.0, year: 2010, embedding: [0.0, 0.0, 1.0, 0.0]})",
    );
    ok(&mut g, "CREATE (:Other {title: 'not-a-movie'})");
    g
}

#[test]
fn basic_search_with_list_literal() {
    let mut g = corpus();
    let rows = ok(
        &mut g,
        "MATCH (movie:Movie) \
           SEARCH movie IN (VECTOR INDEX moviePlots FOR [1.0, 0.0, 0.0, 0.0] LIMIT 2) \
         RETURN movie.title AS title",
    );
    let t = titles(&rows);
    assert_eq!(t, vec!["alpha", "alpha2"], "descending similarity order");
}

#[test]
fn search_with_score_alias() {
    let mut g = corpus();
    let rows = ok(
        &mut g,
        "MATCH (movie:Movie) \
           SEARCH movie IN (VECTOR INDEX moviePlots FOR [1.0, 0.0, 0.0, 0.0] LIMIT 3) \
           SCORE AS s \
         RETURN movie.title AS title, s",
    );
    assert_eq!(rows.len(), 3);
    // scores in [0,1], strictly descending here, exact match = 1.0
    let scores: Vec<f64> = rows
        .iter()
        .map(|r| match &r[1] {
            CypherValue::Float(f) => *f,
            o => panic!("expected float score, got {o:?}"),
        })
        .collect();
    assert!(
        scores[0] > 0.999,
        "exact match scores ~1.0, got {}",
        scores[0]
    );
    for w in scores.windows(2) {
        assert!(w[0] >= w[1], "scores must be descending: {scores:?}");
    }
    assert!(scores.iter().all(|s| (0.0..=1.0).contains(s)));
    assert_eq!(rows[0][0], CypherValue::String("alpha".into()));
}

#[test]
fn search_with_parameter_vector() {
    let mut g = corpus();
    let mut params = HashMap::new();
    params.insert(
        "qv".to_string(),
        CypherValue::List(vec![
            CypherValue::Float(0.0),
            CypherValue::Float(1.0),
            CypherValue::Float(0.0),
            CypherValue::Float(0.0),
        ]),
    );
    let rows = run_params(
        &mut g,
        "MATCH (m:Movie) SEARCH m IN (VECTOR INDEX moviePlots FOR $qv LIMIT 1) \
         RETURN m.title",
        &params,
    )
    .expect("param query");
    assert_eq!(titles(&rows), vec!["beta"]);
}

#[test]
fn search_with_property_query_vector_from_prior_match() {
    // query vector from a node bound by an earlier MATCH (correlated search, manual example 4)
    let mut g = corpus();
    let rows = ok(
        &mut g,
        "MATCH (seed:Movie {title: 'alpha'}) \
         MATCH (movie:Movie) \
           SEARCH movie IN (VECTOR INDEX moviePlots FOR seed.embedding LIMIT 2) \
         RETURN movie.title AS title",
    );
    assert_eq!(titles(&rows), vec!["alpha", "alpha2"]);
}

#[test]
fn pattern_property_predicate_post_filters() {
    // manual example 7: {year: ...} post-filters the k results, it does NOT change what the index
    // returns
    let mut g = corpus();
    let rows = ok(
        &mut g,
        "MATCH (movie:Movie {year: 1985}) \
           SEARCH movie IN (VECTOR INDEX moviePlots FOR [1.0, 0.0, 0.0, 0.0] LIMIT 2) \
         RETURN movie.title AS title",
    );
    // top-2 by similarity: alpha + alpha2; only alpha2 has year 1985
    assert_eq!(titles(&rows), vec!["alpha2"]);
}

#[test]
fn where_post_filter_both_positions() {
    // manual example 8: WHERE before or after SEARCH, both post-filter
    let mut g = corpus();
    for q in [
        "MATCH (m:Movie) WHERE m.rating < 7.0 \
           SEARCH m IN (VECTOR INDEX moviePlots FOR [1.0, 0.0, 0.0, 0.0] LIMIT 2) \
         RETURN m.title",
        "MATCH (m:Movie) \
           SEARCH m IN (VECTOR INDEX moviePlots FOR [1.0, 0.0, 0.0, 0.0] LIMIT 2) \
           WHERE m.rating < 7.0 \
         RETURN m.title",
    ] {
        let mut g2 = corpus();
        let rows = ok(&mut g2, q);
        // top-2 = alpha (9.0), alpha2 (6.0); post-filter keeps alpha2
        assert_eq!(titles(&rows), vec!["alpha2"], "{q}");
    }
    let _ = &mut g; // unused
}

#[test]
fn in_index_where_is_filtered_knn() {
    // manual example 9: the in-index WHERE gives the top-k AMONG passing nodes, so k results still
    // come back (unlike a post-filter)
    let mut g = corpus();
    let rows = ok(
        &mut g,
        "MATCH (m:Movie) \
           SEARCH m IN (VECTOR INDEX moviePlots \
                        FOR [1.0, 0.0, 0.0, 0.0] \
                        WHERE m.rating < 8.5 \
                        LIMIT 2) \
         RETURN m.title",
    );
    // alpha (9.0) is filtered out, so top-2 of the rest: alpha2 (closest passing), then beta
    assert_eq!(titles(&rows), vec!["alpha2", "beta"]);
}

#[test]
fn combined_in_index_and_post_filter() {
    // manual example 10: both filters compose
    let mut g = corpus();
    let rows = ok(
        &mut g,
        "MATCH (m:Movie) \
           SEARCH m IN (VECTOR INDEX moviePlots \
                        FOR [1.0, 0.0, 0.0, 0.0] \
                        WHERE m.rating < 8.5 \
                        LIMIT 2) \
           WHERE m.year > 1990 \
         RETURN m.title",
    );
    // filtered top-2 = alpha2 (1985), beta (1999); post-filter keeps beta
    assert_eq!(titles(&rows), vec!["beta"]);
}

#[test]
fn null_query_vector_yields_no_rows() {
    // manual example 16: null query vector -> no results, no error
    let mut g = corpus();
    let rows = ok(
        &mut g,
        "MATCH (seed:Movie {title: 'alpha'}) \
         MATCH (m:Movie) \
           SEARCH m IN (VECTOR INDEX moviePlots FOR seed.noSuchProp LIMIT 4) \
         RETURN m.title",
    );
    assert!(rows.is_empty());
}

#[test]
fn null_query_vector_optional_match_yields_null_row() {
    // manual example 17: with OPTIONAL MATCH the row survives with nulls
    let mut g = corpus();
    let rows = ok(
        &mut g,
        "MATCH (seed:Movie {title: 'alpha'}) \
         OPTIONAL MATCH (m:Movie) \
           SEARCH m IN (VECTOR INDEX moviePlots FOR seed.noSuchProp LIMIT 4) \
         RETURN seed.title, m.title",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("alpha".into()));
    assert_eq!(rows[0][1], CypherValue::Null);
}

#[test]
fn dimension_mismatch_errors() {
    // manual example 15 (error 51N65 shape)
    let mut g = corpus();
    let err = run(
        &mut g,
        "MATCH (m:Movie) SEARCH m IN (VECTOR INDEX moviePlots FOR [1.0, 2.0] LIMIT 4) \
         RETURN m.title",
    )
    .unwrap_err();
    assert!(
        err.contains("dimensionality") || err.contains("dimension"),
        "expected a dimension error, got: {err}"
    );
}

#[test]
fn unknown_index_errors() {
    let mut g = corpus();
    let err = run(
        &mut g,
        "MATCH (m:Movie) SEARCH m IN (VECTOR INDEX nope FOR [1.0, 0.0, 0.0, 0.0] LIMIT 4) \
         RETURN m.title",
    )
    .unwrap_err();
    assert!(err.contains("no such vector"), "got: {err}");
}

#[test]
fn self_referencing_query_vector_errors() {
    // error 42I75: the query vector can't depend on the binding variable
    let mut g = corpus();
    let err = run(
        &mut g,
        "MATCH (m:Movie) SEARCH m IN (VECTOR INDEX moviePlots FOR m.embedding LIMIT 4) \
         RETURN m.title",
    )
    .unwrap_err();
    assert!(err.contains("may not depend"), "got: {err}");
}

#[test]
fn undefined_binding_variable_errors() {
    // error 42N62
    let mut g = corpus();
    let err = run(&mut g,
        "MATCH (m:Movie) SEARCH other IN (VECTOR INDEX moviePlots FOR [1.0, 0.0, 0.0, 0.0] LIMIT 4) \
         RETURN m.title")
        .unwrap_err();
    assert!(
        err.contains("not defined") || err.contains("must be the variable"),
        "got: {err}"
    );
}

#[test]
fn relationship_pattern_rejected() {
    let mut g = corpus();
    let err = run(
        &mut g,
        "MATCH (m:Movie)-[r]->(x) \
           SEARCH m IN (VECTOR INDEX moviePlots FOR [1.0, 0.0, 0.0, 0.0] LIMIT 4) \
         RETURN m.title",
    )
    .unwrap_err();
    assert!(err.contains("single-node"), "got: {err}");
}

#[test]
fn limit_zero_yields_no_rows() {
    let mut g = corpus();
    let rows = ok(
        &mut g,
        "MATCH (m:Movie) SEARCH m IN (VECTOR INDEX moviePlots FOR [1.0, 0.0, 0.0, 0.0] LIMIT 0) \
         RETURN m.title",
    );
    assert!(rows.is_empty());
}

#[test]
fn explain_shows_vector_search() {
    let mut g = corpus();
    let params: HashMap<String, CypherValue> = HashMap::new();
    let res = execute_with_params(
        &mut g,
        "EXPLAIN MATCH (m:Movie) \
           SEARCH m IN (VECTOR INDEX moviePlots FOR [1.0, 0.0, 0.0, 0.0] LIMIT 2) \
         RETURN m.title",
        &params,
    )
    .expect("explain");
    fn walk(v: &CypherValue) -> bool {
        if let CypherValue::Map(m) = v {
            if let Some(CypherValue::String(t)) = m.get("operatorType")
                && t == "VectorSearch"
            {
                return true;
            }
            if let Some(CypherValue::List(children)) = m.get("children") {
                return children.iter().any(walk);
            }
        }
        false
    }
    assert!(
        walk(res.plan.as_ref().expect("plan")),
        "plan must contain VectorSearch"
    );
}

#[test]
fn score_flows_through_aggregation_and_order() {
    // score is a real variable: usable in ORDER BY and expressions
    let mut g = corpus();
    let rows = ok(
        &mut g,
        "MATCH (m:Movie) \
           SEARCH m IN (VECTOR INDEX moviePlots FOR [1.0, 0.0, 0.0, 0.0] LIMIT 4) \
           SCORE AS s \
         RETURN m.title, round(s * 10) / 10 AS bucket ORDER BY s ASC LIMIT 1",
    );
    assert_eq!(rows.len(), 1);
    // ascending -> the LEAST similar of the 4
    assert_ne!(rows[0][0], CypherValue::String("alpha".into()));
}

#[test]
fn search_survives_vector_updates_and_deletes() {
    let mut g = Graph::new();
    ok(
        &mut g,
        "CREATE VECTOR INDEX pts FOR (p:Pt) ON (p.v) \
         OPTIONS { indexConfig: { 'vector.dimensions': 3, 'vector.similarity_function': 'euclidean' } }",
    );
    ok(
        &mut g,
        "UNWIND range(0, 299) AS i CREATE (:Pt {i: i, v: [toFloat(i), toFloat(i % 7), 1.0]})",
    );
    // every vector replaced, then a third of the nodes deleted: the index repairs in place
    ok(
        &mut g,
        "MATCH (p:Pt) SET p.v = [p.i + 0.5, toFloat(p.i % 7), 1.0]",
    );
    ok(&mut g, "MATCH (p:Pt) WHERE p.i % 3 = 0 DETACH DELETE p");
    let nearest = |g: &mut Graph, q: &str| -> Vec<i64> {
        ok(
            g,
            &format!("MATCH (p:Pt) SEARCH p IN (VECTOR INDEX pts FOR {q} LIMIT 2) RETURN p.i AS i"),
        )
        .into_iter()
        .map(|r| match r[0] {
            CypherValue::Integer(i) => i,
            ref o => panic!("expected integer, got {o:?}"),
        })
        .collect()
    };
    assert_eq!(nearest(&mut g, "[100.5, 2.0, 1.0]")[0], 100);
    // near a deleted node: still answers with live neighbors
    let around_99 = nearest(&mut g, "[99.5, 1.0, 1.0]");
    assert_eq!(around_99.len(), 2);
    assert!(
        around_99.iter().all(|i| i % 3 != 0),
        "deleted node returned: {around_99:?}"
    );
}
