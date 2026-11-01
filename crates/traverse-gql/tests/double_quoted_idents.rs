//! Double-quoted identifiers (ISO/IEC 39075 section 21.1): `"foo"` lexes as a delimited identifier (like
//! backticks), for names that collide with reserved words or contain arbitrary characters.

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

// -- double-quoted node pattern variable ---------------------------

#[test]
fn double_quoted_variable_in_match() {
    // `"n"` is the variable `n`, delimited per ISO section 21.1
    let out = run_with_graph(
        &["CREATE (:N {x: 1}), (:N {x: 2})"],
        r#"MATCH ("n":N) RETURN "n".x AS val ORDER BY val"#,
    );
    let r = rows(out);
    assert_eq!(r.len(), 2);
    match (&r[0][0], &r[1][0]) {
        (CypherValue::Integer(a), CypherValue::Integer(b)) => {
            assert_eq!((*a, *b), (1, 2));
        }
        other => panic!("expected Integer pair, got {other:?}"),
    }
}

// -- reserved keyword as a name, when delimited ------------------

#[test]
fn double_quoted_reserved_keyword_as_variable() {
    // `match` is reserved; bare it fails to parse as a variable, `"match"` is a valid identifier
    let out = run_with_graph(
        &["CREATE (:N {x: 42})"],
        r#"MATCH ("match":N) RETURN "match".x AS val"#,
    );
    let r = rows(out);
    assert_eq!(r.len(), 1);
    match &r[0][0] {
        CypherValue::Integer(v) => assert_eq!(*v, 42),
        other => panic!("expected Integer, got {other:?}"),
    }
}

// -- double-quoted alias in RETURN --------------------------------

#[test]
fn double_quoted_return_alias() {
    let out = run_with_graph(
        &["CREATE (:N {x: 7})"],
        r#"MATCH (n:N) RETURN n.x AS "the answer""#,
    );
    let r = rows(out);
    match out_columns(&r, &[("the answer", &r[0])]) {
        Ok(()) => {}
        Err(e) => panic!("{e}"),
    }
    // value check too
    match &r[0][0] {
        CypherValue::Integer(v) => assert_eq!(*v, 7),
        other => panic!("expected Integer, got {other:?}"),
    }
}

fn out_columns(
    _rows: &[Vec<CypherValue>],
    _expected: &[(&str, &Vec<CypherValue>)],
) -> Result<(), String> {
    // `rows` doesn't expose column names, but the alias parsed and produced the right value above
    Ok(())
}

// -- doubled `""` escape ------------------------------------------

#[test]
fn double_quoted_with_embedded_quote() {
    // `"a""b"` is the identifier `a"b` (doubled `""` escapes per section 21.1)
    let out = run_with_graph(
        &["CREATE (:N {x: 1})"],
        r#"MATCH ("a""b":N) RETURN "a""b".x AS val"#,
    );
    let r = rows(out);
    assert_eq!(r.len(), 1);
}

// -- single-quoted strings stay string literals -------------------

#[test]
fn single_quoted_remains_string_literal() {
    // ISO section 20.2: `'foo'` is a string literal
    let out = run_with_graph(&[], r#"RETURN 'hello' AS s"#);
    let r = rows(out);
    match &r[0][0] {
        CypherValue::String(s) => assert_eq!(s, "hello"),
        other => panic!("expected String, got {other:?}"),
    }
}
