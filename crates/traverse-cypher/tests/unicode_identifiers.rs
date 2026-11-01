//! Unicode identifiers: openCypher identifiers take Unicode letters, as Neo4j does (`förkortning`,
//! `TILLHÖR` unquoted), for labels, property keys, relationship types and variables, not just
//! string values.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run(graph: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    let params: HashMap<String, CypherValue> = HashMap::new();
    execute_with_params(graph, q, &params).expect(q).rows
}

#[test]
fn swedish_identifiers_everywhere() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE (:Parti {namn: 'Socialdemokraterna', förkortning: 'S'})",
    );
    run(&mut g, "CREATE (:Ledamot {namn: 'Åsa Öström-Läärä'})");
    run(
        &mut g,
        "MATCH (l:Ledamot {namn: 'Åsa Öström-Läärä'}), (p:Parti) CREATE (l)-[:TILLHÖR]->(p)",
    );
    let rows = run(
        &mut g,
        "MATCH (ledamöter:Ledamot)-[:TILLHÖR]->(p:Parti) RETURN ledamöter.namn, p.förkortning",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("Åsa Öström-Läärä".into()));
    assert_eq!(rows[0][1], CypherValue::String("S".into()));
}

#[test]
fn unicode_labels_and_backticks_agree() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:Åtgärd {typ: 'granskning'})");
    let plain = run(&mut g, "MATCH (a:Åtgärd) RETURN a.typ");
    let ticked = run(&mut g, "MATCH (a:`Åtgärd`) RETURN a.`typ`");
    assert_eq!(plain, ticked);
    assert_eq!(plain.len(), 1);
}

#[test]
fn punctuation_still_terminates_identifiers() {
    let mut g = Graph::new();
    // an em-dash isn't an identifier character: it must ERROR, not merge silently into a token
    let params: HashMap<String, CypherValue> = HashMap::new();
    let err = execute_with_params(&mut g, "RETURN a\u{2014}b", &params).unwrap_err();
    assert!(err.to_string().contains("unexpected character"), "{err}");
}
