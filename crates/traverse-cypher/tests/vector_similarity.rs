//! `vector.similarity.cosine` / `vector.similarity.euclidean`. Cosine returns `(1 + cos theta) / 2`
//! (equal -> 1.0, orthogonal -> 0.5, opposite -> 0.0); Euclidean `1 / (1 + sqrt(sum(a-b)^2))` (equal ->
//! 1.0, decaying toward 0 with distance). Both take equal-length numeric lists and propagate NULL.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run_one(q: &str) -> CypherValue {
    let mut g = Graph::new();
    let params: HashMap<String, CypherValue> = HashMap::new();
    let res = execute_with_params(&mut g, q, &params).expect("query");
    assert_eq!(res.rows.len(), 1, "expected single row");
    res.rows
        .into_iter()
        .next()
        .unwrap()
        .into_iter()
        .next()
        .unwrap()
}

fn approx(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn cosine_identical_vectors() {
    let v = run_one("RETURN vector.similarity.cosine([1.0, 2.0, 3.0], [1.0, 2.0, 3.0]) AS s");
    match v {
        CypherValue::Float(f) => approx(f, 1.0),
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn cosine_orthogonal_vectors() {
    let v = run_one("RETURN vector.similarity.cosine([1.0, 0.0], [0.0, 1.0]) AS s");
    match v {
        CypherValue::Float(f) => approx(f, 0.5),
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn cosine_opposite_vectors() {
    let v = run_one("RETURN vector.similarity.cosine([1.0, 0.0], [-1.0, 0.0]) AS s");
    match v {
        CypherValue::Float(f) => approx(f, 0.0),
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn euclidean_identical_vectors() {
    let v = run_one("RETURN vector.similarity.euclidean([1.0, 2.0, 3.0], [1.0, 2.0, 3.0]) AS s");
    match v {
        CypherValue::Float(f) => approx(f, 1.0),
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn euclidean_unit_distance() {
    // sqrt((1-0)^2) = 1 -> 1/(1+1) = 0.5
    let v = run_one("RETURN vector.similarity.euclidean([1.0], [0.0]) AS s");
    match v {
        CypherValue::Float(f) => approx(f, 0.5),
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn cosine_null_propagates() {
    let v = run_one("RETURN vector.similarity.cosine(NULL, [1.0, 2.0]) AS s");
    assert_eq!(v, CypherValue::Null);
    let v = run_one("RETURN vector.similarity.euclidean([1.0, 2.0], NULL) AS s");
    assert_eq!(v, CypherValue::Null);
}

#[test]
fn cosine_length_mismatch_errors() {
    let mut g = Graph::new();
    let params: HashMap<String, CypherValue> = HashMap::new();
    let res = execute_with_params(
        &mut g,
        "RETURN vector.similarity.cosine([1.0, 2.0], [1.0, 2.0, 3.0]) AS s",
        &params,
    );
    assert!(res.is_err(), "expected length-mismatch error");
}

#[test]
fn cosine_integer_lists_accepted() {
    // INTEGER lists coerce like FLOAT lists
    let v = run_one("RETURN vector.similarity.cosine([1, 2, 3], [1, 2, 3]) AS s");
    match v {
        CypherValue::Float(f) => approx(f, 1.0),
        other => panic!("expected Float, got {other:?}"),
    }
}
