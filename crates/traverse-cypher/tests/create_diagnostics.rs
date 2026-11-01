//! CREATE-shape diagnostics: a big multi-clause CREATE script fails as ONE statement and the AST
//! has no source spans, so semantic errors must name what they rejected (not just "position 0").

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn err(q: &str) -> String {
    let mut g = Graph::new();
    let params: HashMap<String, CypherValue> = HashMap::new();
    execute_with_params(&mut g, q, &params)
        .expect_err("query should be rejected")
        .to_string()
}

#[test]
fn undirected_create_names_the_relationship_type() {
    let e = err("CREATE (a {name: 'x'})
CREATE (b {name: 'y'})
CREATE (a)-[:CONNECTS_ALL]-(b)");
    assert!(
        e.contains("CONNECTS_ALL"),
        "error must name the offending type so the clause is findable: {e}"
    );
    assert!(
        e.contains("-[:CONNECTS_ALL]->"),
        "error must show the fix: {e}"
    );
}

#[test]
fn incoming_direction_is_accepted_in_create() {
    // `<-[:R]-` is directed (endpoints swap at execute time); only the truly undirected `-[:R]-` is
    // rejected
    let mut g = Graph::new();
    let params: HashMap<String, CypherValue> = HashMap::new();
    execute_with_params(
        &mut g,
        "CREATE (a {name: 'x'})
CREATE (b {name: 'y'})
CREATE (a)<-[:REPORTS_TO]-(b)",
        &params,
    )
    .expect("incoming-direction CREATE must execute");
}
