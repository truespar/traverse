//! Neo4j core-procedure compatibility: the operational / introspection procedures from Neo4j
//! 2026.05's SHOW PROCEDURES.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run(graph: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    execute_with_params(graph, q, &HashMap::new())
        .unwrap_or_else(|e| panic!("query failed: {q}\n{e}"))
        .rows
}

#[test]
fn noops_succeed() {
    let mut g = Graph::new();
    for q in [
        "CALL db.prepareForReplanning()",
        "CALL db.resampleIndex('x')",
        "CALL db.resampleOutdatedIndexes()",
        "CALL db.index.fulltext.awaitEventuallyConsistentIndexRefresh()",
        "CALL db.createLabel('L')",
        "CALL db.createProperty('p')",
        "CALL db.createRelationshipType('T')",
    ] {
        run(&mut g, q);
    }
}

#[test]
fn info_and_upgrade_rows() {
    let mut g = Graph::new();
    let rows = run(&mut g, "CALL db.info() YIELD id, name RETURN id, name");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], CypherValue::String("traverse".into()));
    let rows2 = run(
        &mut g,
        "CALL dbms.upgradeStatus() YIELD status RETURN status",
    );
    assert_eq!(rows2[0][0], CypherValue::String("CURRENT".into()));
    let rows3 = run(
        &mut g,
        "CALL db.clearQueryCaches() YIELD value RETURN value",
    );
    assert_eq!(rows3.len(), 1);
}

#[test]
fn list_config_reads_settings_registry() {
    traverse_cypher::settings::register_setting("test.proc_compat.k", "v", "d", "desc", false);
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "CALL dbms.listConfig('proc_compat') YIELD name, value RETURN name, value",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][1], CypherValue::String("v".into()));
}

#[test]
fn fulltext_analyzers_listed() {
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "CALL db.index.fulltext.listAvailableAnalyzers() YIELD analyzer \
         RETURN analyzer ORDER BY analyzer",
    );
    let names: Vec<&CypherValue> = rows.iter().map(|r| &r[0]).collect();
    assert_eq!(rows.len(), 5);
    assert_eq!(*names[0], CypherValue::String("english".into()));
}

#[test]
fn schema_type_properties() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE (:Person {name: 'a', age: 1})-[:KNOWS {since: 2020}]->(:Person {name: 'b'})",
    );
    let rows = run(
        &mut g,
        "CALL db.schema.nodeTypeProperties() YIELD nodeType, propertyName, mandatory \
         RETURN nodeType, propertyName, mandatory ORDER BY propertyName",
    );
    // Person group: age (on 1 of 2, not mandatory), name (on 2 of 2, mandatory)
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], CypherValue::String("age".into()));
    assert_eq!(rows[0][2], CypherValue::Bool(false));
    assert_eq!(rows[1][1], CypherValue::String("name".into()));
    assert_eq!(rows[1][2], CypherValue::Bool(true));
    let rels = run(
        &mut g,
        "CALL db.schema.relTypeProperties() YIELD relType, propertyName, mandatory \
         RETURN relType, propertyName, mandatory",
    );
    assert_eq!(rels.len(), 1);
    assert_eq!(rels[0][0], CypherValue::String(":`KNOWS`".into()));
    assert_eq!(rels[0][2], CypherValue::Bool(true));
}

#[test]
fn stats_and_connections() {
    let mut g = Graph::new();
    let rows = run(
        &mut g,
        "CALL db.stats.collect('GRAPH COUNTS') YIELD section, success RETURN section, success",
    );
    assert_eq!(rows[0][0], CypherValue::String("GRAPH COUNTS".into()));
    assert_eq!(rows[0][1], CypherValue::Bool(true));
    let rows2 = run(
        &mut g,
        "CALL db.stats.retrieve('GRAPH COUNTS') YIELD section RETURN section",
    );
    assert_eq!(rows2.len(), 1);
    let conns = run(
        &mut g,
        "CALL dbms.listConnections() YIELD connectionId, connector RETURN connectionId, connector",
    );
    assert_eq!(conns.len(), 1);
    assert_eq!(conns[0][1], CypherValue::String("bolt".into()));
    let killed = run(
        &mut g,
        "CALL dbms.killConnection('bolt-99') YIELD message RETURN message",
    );
    assert_eq!(
        killed[0][0],
        CypherValue::String("Connection not found".into())
    );
}

#[test]
fn rel_index_queries_return_empty() {
    let mut g = Graph::new();
    let r1 = run(
        &mut g,
        "CALL db.index.fulltext.queryRelationships('x', 'q') YIELD relationship RETURN relationship",
    );
    assert!(r1.is_empty());
    let r2 = run(
        &mut g,
        "CALL db.index.vector.queryRelationships('x', 3, [1.0]) YIELD relationship RETURN relationship",
    );
    assert!(r2.is_empty());
}

#[test]
fn procedural_vector_index_and_setters() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (:Doc {title: 'a'}), (:Doc {title: 'b'})");
    // the old procedural creation API delegates to the real HNSW index
    run(
        &mut g,
        "CALL db.index.vector.createNodeIndex('docVec', 'Doc', 'emb', 3, 'cosine')",
    );
    // the setter maintains the index incrementally
    run(
        &mut g,
        "MATCH (d:Doc {title: 'a'}) CALL db.create.setNodeVectorProperty(d, 'emb', [1.0, 0.0, 0.0]) RETURN d",
    );
    run(
        &mut g,
        "MATCH (d:Doc {title: 'b'}) CALL db.create.setNodeVectorProperty(d, 'emb', [0.0, 1.0, 0.0]) RETURN d",
    );
    let hits = run(
        &mut g,
        "CALL db.index.vector.queryNodes('docVec', 1, [0.9, 0.1, 0.0]) YIELD node RETURN node.title",
    );
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0][0], CypherValue::String("a".into()));
    // the deprecated variant returns the node
    let ret = run(
        &mut g,
        "MATCH (d:Doc {title: 'b'}) CALL db.create.setVectorProperty(d, 'emb', [0.0, 0.9, 0.1]) YIELD node RETURN node.title",
    );
    assert_eq!(ret[0][0], CypherValue::String("b".into()));
    // relationship setter
    run(
        &mut g,
        "MATCH (a:Doc {title: 'a'}), (b:Doc {title: 'b'}) CREATE (a)-[:REL]->(b)",
    );
    run(
        &mut g,
        "MATCH ()-[r:REL]->() CALL db.create.setRelationshipVectorProperty(r, 'emb', [1.0, 1.0]) RETURN r",
    );
    let rprop = run(&mut g, "MATCH ()-[r:REL]->() RETURN r.emb");
    assert!(matches!(&rprop[0][0], CypherValue::List(l) if l.len() == 2));
}
