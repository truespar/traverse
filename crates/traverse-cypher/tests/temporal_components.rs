//! Temporal values built from maps: a component of the wrong type is an error, not a
//! silently ignored field.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run(q: &str) -> Result<CypherValue, String> {
    let mut g = Graph::new();
    let params: HashMap<String, CypherValue> = HashMap::new();
    match execute_with_params(&mut g, &format!("RETURN {q} AS r"), &params) {
        Ok(res) => Ok(res.rows[0][0].clone()),
        Err(e) => Err(e.to_string()),
    }
}

#[test]
fn wrong_typed_components_are_errors() {
    for q in [
        "localdatetime({date: 5})",
        "date({year: 5, month: 'x'})",
        "date({year: '2020'})",
        "time({hour: 1, timezone: 1})",
        "datetime({epochSeconds: 'now'})",
        "duration({days: '1'})",
        "localtime({hour: [1]})",
    ] {
        let err = run(q).expect_err(q);
        assert!(err.contains("cannot construct"), "{q}: {err}");
    }
}

#[test]
fn well_typed_components_still_build() {
    assert_eq!(
        run("toString(date({year: 2020, month: 2, day: 3}))").unwrap(),
        CypherValue::String("2020-02-03".into())
    );
    assert_eq!(
        run("localdatetime({date: date('2020-01-02'), hour: 3}).hour").unwrap(),
        CypherValue::Integer(3)
    );
    assert_eq!(
        run("toString(date({date: '2021-05-06'}))").unwrap(),
        CypherValue::String("2021-05-06".into())
    );
    assert_eq!(
        run("duration({days: 1.5}).days").unwrap(),
        CypherValue::Integer(1)
    );
    assert_eq!(
        run("datetime({epochSeconds: 0, timezone: 'UTC'}).year").unwrap(),
        CypherValue::Integer(1970)
    );
}
