//! ISO/IEC 39075 type predicates: section 19.6 (scalar / structural type tests), section 19.7 (normalization),
//! G111 (`IS LABELED`), through the full GQL pipeline on an in-memory graph.

mod common;
use common::{GqlCatalog, StmtOutcome, run_program};
use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

fn first_cell(query: &str) -> CypherValue {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    match run_program(&mut g, &mut cat, query) {
        StmtOutcome::Ran { rows, .. } => rows[0][0].clone(),
        StmtOutcome::CatalogError(_) => panic!("expected Ran, got CatalogError"),
        StmtOutcome::EngineError(e) => panic!("expected Ran, got EngineError: {e}"),
        StmtOutcome::LowerFailed => panic!("expected Ran, got LowerFailed"),
        StmtOutcome::ParseFailed => panic!("expected Ran, got ParseFailed"),
    }
}

fn first_cell_with_graph(preamble: &[&str], query: &str) -> CypherValue {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    for p in preamble {
        let out = run_program(&mut g, &mut cat, p);
        assert!(out.is_ok(), "preamble failed: {p}");
    }
    match run_program(&mut g, &mut cat, query) {
        StmtOutcome::Ran { rows, .. } => rows[0][0].clone(),
        StmtOutcome::CatalogError(_) => panic!("expected Ran, got CatalogError"),
        StmtOutcome::EngineError(e) => panic!("expected Ran, got EngineError: {e}"),
        StmtOutcome::LowerFailed => panic!("expected Ran, got LowerFailed"),
        StmtOutcome::ParseFailed => panic!("expected Ran, got ParseFailed"),
    }
}

// -- scalar type tests --------------------------------------------

#[test]
fn is_boolean_matches_only_booleans() {
    assert_eq!(
        first_cell("RETURN true IS BOOLEAN AS x"),
        CypherValue::Bool(true)
    );
    assert_eq!(
        first_cell("RETURN 1 IS BOOLEAN AS x"),
        CypherValue::Bool(false)
    );
    assert_eq!(
        first_cell("RETURN 'a' IS BOOLEAN AS x"),
        CypherValue::Bool(false)
    );
    // ISO section 19.6: type predicates never return null; null input still gives a boolean
    assert_eq!(
        first_cell("RETURN null IS BOOLEAN AS x"),
        CypherValue::Bool(false)
    );
}

#[test]
fn is_string_synonyms() {
    // `STRING` and `VARCHAR` are interchangeable (ISO section 4.16)
    assert_eq!(
        first_cell("RETURN 'a' IS STRING AS x"),
        CypherValue::Bool(true)
    );
    assert_eq!(
        first_cell("RETURN 'a' IS VARCHAR AS x"),
        CypherValue::Bool(true)
    );
    assert_eq!(
        first_cell("RETURN 1 IS STRING AS x"),
        CypherValue::Bool(false)
    );
}

#[test]
fn is_integer_synonyms() {
    assert_eq!(
        first_cell("RETURN 1 IS INTEGER AS x"),
        CypherValue::Bool(true)
    );
    assert_eq!(first_cell("RETURN 1 IS INT AS x"), CypherValue::Bool(true));
    assert_eq!(
        first_cell("RETURN 1 IS SIGNED INTEGER AS x"),
        CypherValue::Bool(true)
    );
    assert_eq!(
        first_cell("RETURN 1.5 IS INTEGER AS x"),
        CypherValue::Bool(false)
    );
}

#[test]
fn is_number_covers_int_and_float() {
    assert_eq!(
        first_cell("RETURN 1 IS NUMBER AS x"),
        CypherValue::Bool(true)
    );
    assert_eq!(
        first_cell("RETURN 1.5 IS NUMBER AS x"),
        CypherValue::Bool(true)
    );
    assert_eq!(
        first_cell("RETURN 'x' IS NUMBER AS x"),
        CypherValue::Bool(false)
    );
}

#[test]
fn is_not_negates() {
    assert_eq!(
        first_cell("RETURN 1 IS NOT BOOLEAN AS x"),
        CypherValue::Bool(true)
    );
    assert_eq!(
        first_cell("RETURN true IS NOT BOOLEAN AS x"),
        CypherValue::Bool(false)
    );
    // `IS NOT BOOLEAN` on null is true (null isn't a boolean)
    assert_eq!(
        first_cell("RETURN null IS NOT BOOLEAN AS x"),
        CypherValue::Bool(true)
    );
}

#[test]
fn is_list_and_map() {
    assert_eq!(
        first_cell("RETURN [1,2,3] IS LIST AS x"),
        CypherValue::Bool(true)
    );
    assert_eq!(
        first_cell("RETURN {a: 1} IS MAP AS x"),
        CypherValue::Bool(true)
    );
    assert_eq!(
        first_cell("RETURN [1] IS MAP AS x"),
        CypherValue::Bool(false)
    );
}

#[test]
fn is_any_is_not_null() {
    assert_eq!(first_cell("RETURN 1 IS ANY AS x"), CypherValue::Bool(true));
    assert_eq!(
        first_cell("RETURN 'x' IS ANY AS x"),
        CypherValue::Bool(true)
    );
    assert_eq!(
        first_cell("RETURN null IS ANY AS x"),
        CypherValue::Bool(false)
    );
}

#[test]
fn is_nothing_is_only_null() {
    assert_eq!(
        first_cell("RETURN null IS NOTHING AS x"),
        CypherValue::Bool(true)
    );
    assert_eq!(
        first_cell("RETURN 0 IS NOTHING AS x"),
        CypherValue::Bool(false)
    );
}

// -- structural type tests ----------------------------------------

#[test]
fn is_node_on_a_node() {
    let v = first_cell_with_graph(
        &["CREATE (a:Person {name: 'A'})"],
        "MATCH (a) RETURN a IS NODE AS x",
    );
    assert_eq!(v, CypherValue::Bool(true));
}

#[test]
fn is_any_node_synonym() {
    let v = first_cell_with_graph(
        &["CREATE (a:Person)"],
        "MATCH (a) RETURN a IS ANY NODE AS x",
    );
    assert_eq!(v, CypherValue::Bool(true));
}

#[test]
fn is_vertex_synonym_for_node() {
    let v = first_cell_with_graph(&["CREATE (a:Person)"], "MATCH (a) RETURN a IS VERTEX AS x");
    assert_eq!(v, CypherValue::Bool(true));
}

#[test]
fn is_edge_on_an_edge() {
    let v = first_cell_with_graph(
        &["CREATE (a:A)-[:R]->(b:B)"],
        "MATCH ()-[r]->() RETURN r IS EDGE AS x",
    );
    assert_eq!(v, CypherValue::Bool(true));
}

#[test]
fn is_relationship_synonym_for_edge() {
    let v = first_cell_with_graph(
        &["CREATE (a:A)-[:R]->(b:B)"],
        "MATCH ()-[r]->() RETURN r IS RELATIONSHIP AS x",
    );
    assert_eq!(v, CypherValue::Bool(true));
}

// -- IS LABELED (G111) --------------------------------------------

#[test]
fn is_labeled_single_label() {
    let v = first_cell_with_graph(
        &["CREATE (a:Person {name: 'A'})"],
        "MATCH (a) RETURN a IS LABELED Person AS x",
    );
    assert_eq!(v, CypherValue::Bool(true));
    let v = first_cell_with_graph(
        &["CREATE (a:Person)"],
        "MATCH (a) RETURN a IS LABELED Other AS x",
    );
    assert_eq!(v, CypherValue::Bool(false));
}

#[test]
fn is_labeled_conjunction() {
    // ISO 39075 label conjunction is `&`, not Cypher's `:A:B`; patterns and `IS LABELED` share the
    // section 14.7 label-expression grammar
    let v = first_cell_with_graph(
        &["CREATE (a:Person&Student {name: 'A'})"],
        "MATCH (a:Person) RETURN a IS LABELED Person & Student AS x",
    );
    assert_eq!(v, CypherValue::Bool(true));
    let v = first_cell_with_graph(
        &["CREATE (a:Person {name: 'A'})"],
        "MATCH (a) RETURN a IS LABELED Person & Student AS x",
    );
    assert_eq!(v, CypherValue::Bool(false));
}

#[test]
fn is_labeled_disjunction() {
    let v = first_cell_with_graph(
        &["CREATE (a:Person {name: 'A'})"],
        "MATCH (a) RETURN a IS LABELED Person | Student AS x",
    );
    assert_eq!(v, CypherValue::Bool(true));
}

#[test]
fn is_not_labeled_negates_whole_expression() {
    // per G111's Neo4j note, `IS NOT LABELED` negates the whole label expression (unlike `!`, which
    // negates inside it), so `a IS NOT LABELED Person & Student` is true if either label is missing
    let v = first_cell_with_graph(
        &["CREATE (a:Person)"],
        "MATCH (a) RETURN a IS NOT LABELED Person & Student AS x",
    );
    assert_eq!(v, CypherValue::Bool(true));
}

// -- IS NORMALIZED (section 19.7) ----------------------------------------

#[test]
fn is_normalized_default_is_nfc() {
    // ASCII is trivially NFC
    assert_eq!(
        first_cell("RETURN 'hello' IS NORMALIZED AS x"),
        CypherValue::Bool(true),
    );
}

#[test]
fn is_normalized_form_argument() {
    // ISO 39075 Unicode escapes in string literals use the brace form `\u{XXXX}` (the only one the
    // lexer accepts); composed "é" (U+00E9) is NFC
    assert_eq!(
        first_cell(r"RETURN 'caf\u{00e9}' IS NORMALIZED NFC AS x"),
        CypherValue::Bool(true),
    );
    // decomposed "é" (e + combining acute, U+0065 U+0301) is NOT NFC
    assert_eq!(
        first_cell(r"RETURN 'cafe\u{0301}' IS NORMALIZED NFC AS x"),
        CypherValue::Bool(false),
    );
    // ...but it IS NFD
    assert_eq!(
        first_cell(r"RETURN 'cafe\u{0301}' IS NORMALIZED NFD AS x"),
        CypherValue::Bool(true),
    );
}

#[test]
fn is_normalized_null_is_false() {
    // ISO section 19.6/section 19.7: type predicates don't propagate null
    assert_eq!(
        first_cell("RETURN null IS NORMALIZED AS x"),
        CypherValue::Bool(false),
    );
}
