//! Cypher 25 VECTOR functions (Neo4j 2025.10): `vector()`, `vector_distance()`, `vector_norm()`,
//! `vector_dimension_count()`. A VECTOR is a numeric LIST here (as in storage, the HNSW index and
//! `vector.similarity.*`), so they work on number lists.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run(graph: &mut Graph, q: &str) -> Result<Vec<Vec<CypherValue>>, String> {
    let params: HashMap<String, CypherValue> = HashMap::new();
    match execute_with_params(graph, q, &params) {
        Ok(res) => Ok(res.rows),
        Err(e) => Err(e.to_string()),
    }
}

fn one(q: &str) -> CypherValue {
    let mut g = Graph::new();
    run(&mut g, &format!("RETURN {q} AS r")).expect("query")[0][0].clone()
}

fn floats(vs: &[f64]) -> CypherValue {
    CypherValue::List(vs.iter().map(|f| CypherValue::Float(*f)).collect())
}

fn approx(v: CypherValue, want: f64) {
    match v {
        CypherValue::Float(f) => assert!((f - want).abs() < 1e-6, "got {f}, want {want}"),
        other => panic!("expected float, got {other:?}"),
    }
}

#[test]
fn vector_constructor() {
    assert_eq!(one("vector([1.0, 2.0, 3.0])"), floats(&[1.0, 2.0, 3.0]));
    // dimension + coordinate-type keyword (bare FLOAT32)
    assert_eq!(
        one("vector([1, 2, 3], 3, FLOAT32)"),
        floats(&[1.0, 2.0, 3.0])
    );
}

#[test]
fn vector_constructor_dimension_mismatch_errors() {
    let mut g = Graph::new();
    let err = run(&mut g, "RETURN vector([1, 2], 3) AS r").expect_err("dim mismatch");
    assert!(err.contains("dimension"), "got: {err}");
}

#[test]
fn vector_dimension_count() {
    assert_eq!(
        one("vector_dimension_count([1.0, 2.0, 3.0, 4.0])"),
        CypherValue::Integer(4)
    );
}

#[test]
fn vector_norm() {
    approx(one("vector_norm([3.0, 4.0])"), 5.0);
    approx(one("vector_norm([1.0, 2.0, 2.0])"), 3.0);
}

#[test]
fn vector_distance_euclidean() {
    approx(
        one("vector_distance([1.0, 0.0], [0.0, 1.0], 'EUCLIDEAN')"),
        std::f64::consts::SQRT_2,
    );
    approx(
        one("vector_distance([0.0, 0.0], [3.0, 4.0], 'EUCLIDEAN')"),
        5.0,
    );
}

#[test]
fn vector_distance_cosine() {
    // orthogonal -> cosine similarity 0 -> distance 1
    approx(
        one("vector_distance([1.0, 0.0], [0.0, 1.0], 'COSINE')"),
        1.0,
    );
    // same direction -> distance 0
    approx(
        one("vector_distance([1.0, 2.0, 3.0], [1.0, 2.0, 3.0], 'COSINE')"),
        0.0,
    );
}

#[test]
fn vector_distance_dot_product() {
    approx(
        one("vector_distance([1.0, 2.0], [3.0, 4.0], 'DOT_PRODUCT')"),
        -11.0,
    );
}

#[test]
fn vector_distance_unknown_metric_errors() {
    let mut g = Graph::new();
    let err = run(
        &mut g,
        "RETURN vector_distance([1.0], [2.0], 'MANHATTAN') AS r",
    )
    .expect_err("unknown metric");
    assert!(err.to_uppercase().contains("METRIC"), "got: {err}");
}

#[test]
fn vector_functions_on_stored_property() {
    // a vector stored as a list property flows through unchanged
    let mut g = Graph::new();
    run(&mut g, "CREATE (:Doc {embedding: [0.1, 0.2, 0.3, 0.4]})").expect("seed");
    let rows = run(
        &mut g,
        "MATCH (d:Doc) RETURN vector_dimension_count(d.embedding) AS dim",
    )
    .expect("query");
    assert_eq!(rows[0][0], CypherValue::Integer(4));
}

#[test]
fn vector_null_propagation() {
    assert_eq!(one("vector_norm(null)"), CypherValue::Null);
    assert_eq!(
        one("vector_distance(null, [1.0], 'COSINE')"),
        CypherValue::Null
    );
    assert_eq!(one("vector_dimension_count(null)"), CypherValue::Null);
}
