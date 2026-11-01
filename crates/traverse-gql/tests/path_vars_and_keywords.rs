//! Function/keyword polish and path variables: section 16.9 path variables (G004), section 20.7 nullif, GV39/GV40
//! parameterless temporal keywords, G002/G003 match modes.

mod common;
use common::{GqlCatalog, StmtOutcome, run_program};
use traverse_core::graph::Graph;
use traverse_cypher::types::CypherValue;

fn run(query: &str) -> StmtOutcome {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    run_program(&mut g, &mut cat, query)
}

fn run_with_graph(preamble: &[&str], query: &str) -> StmtOutcome {
    let mut g = Graph::new();
    let mut cat = GqlCatalog::default();
    for p in preamble {
        let out = run_program(&mut g, &mut cat, p);
        assert!(out.is_ok(), "preamble failed: {p}");
    }
    run_program(&mut g, &mut cat, query)
}

fn first_cell(out: StmtOutcome) -> CypherValue {
    match out {
        StmtOutcome::Ran { rows, .. } => rows[0][0].clone(),
        StmtOutcome::CatalogError(_) => panic!("CatalogError"),
        StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
        StmtOutcome::LowerFailed => panic!("LowerFailed"),
        StmtOutcome::ParseFailed => panic!("ParseFailed"),
    }
}

// -- nullif (ISO section 20.7) --------------------------------------------

#[test]
fn nullif_returns_null_on_equal() {
    assert_eq!(
        first_cell(run("RETURN nullif(1, 1) AS x")),
        CypherValue::Null
    );
    assert_eq!(
        first_cell(run("RETURN nullif('a', 'a') AS x")),
        CypherValue::Null,
    );
}

#[test]
fn nullif_returns_lhs_when_not_equal() {
    assert_eq!(
        first_cell(run("RETURN nullif(1, 2) AS x")),
        CypherValue::Integer(1),
    );
    assert_eq!(
        first_cell(run("RETURN nullif('a', 'b') AS x")),
        CypherValue::String("a".into()),
    );
}

#[test]
fn nullif_propagates_null_lhs() {
    // ISO section 20.7: `nullif(NULL, x)` is null, the comparison can never be true
    assert_eq!(
        first_cell(run("RETURN nullif(null, 1) AS x")),
        CypherValue::Null,
    );
}

#[test]
fn nullif_returns_lhs_when_rhs_is_null() {
    // comparing with NULL is UNKNOWN -> "not equal" -> the left side unchanged
    assert_eq!(
        first_cell(run("RETURN nullif(1, null) AS x")),
        CypherValue::Integer(1),
    );
}

// -- parameterless temporal keywords (section GV39 / section GV40) ---------------

#[test]
fn current_date_returns_a_date() {
    // just a DATE-typed value, not the exact day (that would race the clock)
    let v = first_cell(run("RETURN CURRENT_DATE AS x"));
    assert!(matches!(v, CypherValue::Date(_)));
}

#[test]
fn current_timestamp_returns_a_datetime() {
    let v = first_cell(run("RETURN CURRENT_TIMESTAMP AS x"));
    assert!(matches!(v, CypherValue::DateTime(_)));
}

#[test]
fn current_time_returns_a_time() {
    let v = first_cell(run("RETURN CURRENT_TIME AS x"));
    assert!(matches!(v, CypherValue::Time(_)));
}

#[test]
fn local_timestamp_returns_a_local_datetime() {
    let v = first_cell(run("RETURN LOCAL_TIMESTAMP AS x"));
    assert!(matches!(v, CypherValue::LocalDateTime(_)));
}

#[test]
fn parameterless_keyword_with_parens_still_works() {
    // `CURRENT_TIMESTAMP()` with an explicit empty arg list resolves to the same impl
    let v = first_cell(run("RETURN CURRENT_TIMESTAMP() AS x"));
    assert!(matches!(v, CypherValue::DateTime(_)));
}

// -- DIFFERENT RELATIONSHIPS / REPEATABLE ELEMENTS (G002 / G003) --

#[test]
fn different_relationships_parses_and_runs() {
    // DIFFERENT RELATIONSHIPS is no-op sugar: the default match semantics already match the spec
    let out = run_with_graph(
        &["CREATE (a:Person)-[:KNOWS]->(b:Person)"],
        "MATCH DIFFERENT RELATIONSHIPS (a)-[:KNOWS]->(b) RETURN count(*) AS n",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => assert_eq!(rows[0][0], CypherValue::Integer(1)),
        other => panic!(
            "expected Ran, got {other:?}",
            other = match other {
                StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
                _ => "non-Ran",
            }
        ),
    }
}

#[test]
fn repeatable_elements_lowers_to_walk_mode() {
    // REPEATABLE ELEMENTS lowers as `PathMode::Walk`; the matcher's thread-local then drops edge
    // uniqueness in variable-length expansion
    let p = traverse_gql::parse("MATCH REPEATABLE ELEMENTS (a)-[:R]->(b) RETURN a").unwrap();
    let _stmt =
        traverse_gql::lower::lower(&p).expect("REPEATABLE ELEMENTS should now lower as WALK");
    assert!(matches!(
        traverse_gql::lower::dominant_path_mode(&p),
        traverse_cypher::ast::PathMode::Walk,
    ));
}

// -- path variables (section 16.9 / G004) --------------------------------

#[test]
fn path_variable_binds_a_path() {
    let out = run_with_graph(
        &["CREATE (a:City {name: 'A'})-[:ROAD]->(b:City {name: 'B'})"],
        // `end` is reserved (terminates CASE), hence `dst`
        "MATCH p = (src:City)-[:ROAD]->(dst:City) \
         RETURN src.name AS s, dst.name AS e",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => {
            // PATH values have no literal string form here, but the path-variable scope must bind
            // both endpoints
            assert_eq!(rows[0][0], CypherValue::String("A".into()));
            assert_eq!(rows[0][1], CypherValue::String("B".into()));
        }
        other => panic!(
            "expected Ran, got {other:?}",
            other = match other {
                StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
                _ => "non-Ran",
            }
        ),
    }
}

// -- SESSION_USER (section 20.3) ------------------------------------------

#[test]
fn session_user_returns_null_outside_session() {
    // no session guard -> null, per the executor contract (the HTTP layer installs one per request)
    assert_eq!(
        first_cell(run("RETURN SESSION_USER AS u")),
        CypherValue::Null
    );
}

#[test]
fn session_user_returns_configured_value_inside_guard() {
    let _g = traverse_cypher::session::set_session_user("alice");
    assert_eq!(
        first_cell(run("RETURN SESSION_USER AS u")),
        CypherValue::String("alice".into()),
    );
}

#[test]
fn session_user_with_parens_is_equivalent() {
    let _g = traverse_cypher::session::set_session_user("bob");
    assert_eq!(
        first_cell(run("RETURN SESSION_USER() AS u")),
        CypherValue::String("bob".into()),
    );
}

#[test]
fn path_variable_is_usable_in_return() {
    let out = run_with_graph(
        &["CREATE (a:Person {name: 'A'})-[:KNOWS]->(b:Person {name: 'B'})"],
        // `p` carries the whole path; `nodes(p)` lists the nodes traversed
        "MATCH p = (a:Person)-[:KNOWS]->(b:Person) \
         RETURN size(nodes(p)) AS n",
    );
    match out {
        StmtOutcome::Ran { rows, .. } => {
            assert_eq!(rows[0][0], CypherValue::Integer(2));
        }
        other => panic!(
            "expected Ran, got {other:?}",
            other = match other {
                StmtOutcome::EngineError(e) => panic!("EngineError: {e}"),
                _ => "non-Ran",
            }
        ),
    }
}
