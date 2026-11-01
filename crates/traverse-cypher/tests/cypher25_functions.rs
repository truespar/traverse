//! Cypher 25 function-library additions: `coll.*` (2025.11), `string.*` (2026.05), `*OrNull`
//! coercions, the `replace()` limit arg (2025.06), and the `path_length` / `duration_between` GQL
//! aliases (2026.02).

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

fn ints(vs: &[i64]) -> CypherValue {
    CypherValue::List(vs.iter().map(|i| CypherValue::Integer(*i)).collect())
}

// -- coll.* --------------------------------------------------------------

#[test]
fn coll_distinct() {
    assert_eq!(one("coll.distinct([1, 1, 2, 3, 3, 2])"), ints(&[1, 2, 3]));
}

#[test]
fn coll_flatten() {
    assert_eq!(
        one("coll.flatten([[1, 2], [3], [4, 5]])"),
        ints(&[1, 2, 3, 4, 5])
    );
}

#[test]
fn coll_index_of() {
    assert_eq!(
        one("coll.indexOf([10, 20, 30], 20)"),
        CypherValue::Integer(1)
    );
    assert_eq!(
        one("coll.indexOf([10, 20, 30], 99)"),
        CypherValue::Integer(-1)
    );
}

#[test]
fn coll_min_max() {
    assert_eq!(one("coll.min([3, 1, 2])"), CypherValue::Integer(1));
    assert_eq!(one("coll.max([3, 1, 2])"), CypherValue::Integer(3));
}

#[test]
fn coll_sort() {
    assert_eq!(one("coll.sort([3, 1, 2])"), ints(&[1, 2, 3]));
}

#[test]
fn coll_insert() {
    assert_eq!(one("coll.insert([1, 2, 3], 1, 99)"), ints(&[1, 99, 2, 3]));
}

#[test]
fn coll_remove() {
    assert_eq!(one("coll.remove([1, 2, 3, 4], 1)"), ints(&[1, 3, 4]));
    assert_eq!(one("coll.remove([1, 2, 3, 4], 1, 2)"), ints(&[1, 4]));
}

// -- string.* ------------------------------------------------------------

#[test]
fn string_join() {
    assert_eq!(
        one("string.join(['a', 'b', 'c'], '-')"),
        CypherValue::String("a-b-c".into())
    );
    assert_eq!(
        one("string.join(['a', 'b', 'c'])"),
        CypherValue::String("abc".into())
    );
}

#[test]
fn string_index_of() {
    assert_eq!(one("string.indexOf('hello', 'l')"), CypherValue::Integer(2));
    assert_eq!(
        one("string.indexOf('hello', 'l', 3)"),
        CypherValue::Integer(3)
    );
    assert_eq!(
        one("string.indexOf('hello', 'z')"),
        CypherValue::Integer(-1)
    );
}

#[test]
fn string_regex_replace() {
    assert_eq!(
        one("string.regexReplace('a1b2c3', '[0-9]', '#')"),
        CypherValue::String("a#b#c#".into())
    );
}

// -- *OrNull -------------------------------------------------------------

#[test]
fn to_integer_or_null() {
    assert_eq!(one("toIntegerOrNull('42')"), CypherValue::Integer(42));
    assert_eq!(one("toIntegerOrNull('abc')"), CypherValue::Null);
    assert_eq!(one("toIntegerOrNull([1, 2])"), CypherValue::Null);
}

#[test]
fn to_float_boolean_string_or_null() {
    assert_eq!(one("toFloatOrNull('1.5')"), CypherValue::Float(1.5));
    assert_eq!(one("toFloatOrNull('x')"), CypherValue::Null);
    assert_eq!(one("toBooleanOrNull('true')"), CypherValue::Bool(true));
    assert_eq!(one("toBooleanOrNull('nope')"), CypherValue::Null);
    assert_eq!(one("toStringOrNull(42)"), CypherValue::String("42".into()));
}

// -- replace() limit -----------------------------------------------------

#[test]
fn replace_with_limit() {
    assert_eq!(
        one("replace('aaaa', 'a', 'b', 2)"),
        CypherValue::String("bbaa".into())
    );
    // 3-arg form still works
    assert_eq!(
        one("replace('aaaa', 'a', 'b')"),
        CypherValue::String("bbbb".into())
    );
}

// -- aliases -------------------------------------------------------------

#[test]
fn path_length_alias() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:A)-[:R]->(:B)-[:R]->(:C)").expect("seed");
    let rows = run(
        &mut g,
        "MATCH p = (a:A)-[:R*2]->(c:C) RETURN path_length(p) AS len",
    )
    .expect("query");
    assert_eq!(rows[0][0], CypherValue::Integer(2));
}

// -- aggregate aliases (Neo4j 2026.02) -----------------------------------

fn agg(q: &str) -> CypherValue {
    let mut g = Graph::new();
    run(&mut g, q).expect("query")[0][0].clone()
}

#[test]
fn collect_list_alias() {
    assert_eq!(
        agg("UNWIND [1, 2, 3] AS x RETURN collect_list(x) AS r"),
        ints(&[1, 2, 3])
    );
}

#[test]
fn percentile_cont_alias() {
    match agg("UNWIND [1, 2, 3, 4] AS x RETURN percentile_cont(x, 0.5) AS r") {
        CypherValue::Float(f) => assert!((f - 2.5).abs() < 1e-9, "got {f}"),
        other => panic!("expected float, got {other:?}"),
    }
}

#[test]
fn percentile_disc_alias() {
    // discrete percentile returns an actual element of the set
    match agg("UNWIND [1, 2, 3, 4] AS x RETURN percentile_disc(x, 0.5) AS r") {
        CypherValue::Integer(i) => assert!(i == 2 || i == 3, "got {i}"),
        other => panic!("expected integer, got {other:?}"),
    }
}

#[test]
fn stdev_samp_and_pop_aliases() {
    match agg("UNWIND [1, 2, 3, 4] AS x RETURN stdev_samp(x) AS r") {
        CypherValue::Float(f) => assert!((f - 1.290_994).abs() < 1e-4, "sample stdev, got {f}"),
        other => panic!("expected float, got {other:?}"),
    }
    match agg("UNWIND [1, 2, 3, 4] AS x RETURN stdev_pop(x) AS r") {
        CypherValue::Float(f) => assert!((f - 1.118_034).abs() < 1e-4, "pop stdev, got {f}"),
        other => panic!("expected float, got {other:?}"),
    }
}

// -- format() (Neo4j 2025.09) --------------------------------------------

#[test]
fn format_date() {
    assert_eq!(
        one("format(date('2024-01-15'), 'yyyy-MM-dd')"),
        CypherValue::String("2024-01-15".into())
    );
    assert_eq!(
        one("format(date('2024-01-15'), 'dd/MM/yyyy')"),
        CypherValue::String("15/01/2024".into())
    );
}

#[test]
fn format_datetime() {
    assert_eq!(
        one("format(datetime('2024-01-15T13:45:30'), 'yyyy-MM-dd HH:mm:ss')"),
        CypherValue::String("2024-01-15 13:45:30".into())
    );
}

// -- temporal pattern-arg parsing (Neo4j 2025.10) ------------------------

#[test]
fn date_pattern_arg() {
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "RETURN date('15/01/2024', 'dd/MM/yyyy') = date('2024-01-15') AS same",
    )
    .expect("query");
    assert_eq!(rows[0][0], CypherValue::Bool(true));
}

#[test]
fn datetime_pattern_arg() {
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "RETURN localdatetime('15/01/2024 13:45', 'dd/MM/yyyy HH:mm') = \
         localdatetime('2024-01-15T13:45:00') AS same",
    )
    .expect("query");
    assert_eq!(rows[0][0], CypherValue::Bool(true));
}

#[test]
fn duration_between_alias_matches_namespaced() {
    // the alias returns the same as `duration.between`
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "RETURN duration_between(date('2020-01-01'), date('2020-03-01')) = \
         duration.between(date('2020-01-01'), date('2020-03-01')) AS same",
    )
    .expect("query");
    assert_eq!(rows[0][0], CypherValue::Bool(true));
}
