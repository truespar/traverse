//! Native FULLTEXT search end to end: `CREATE FULLTEXT INDEX ...` +
//! `db.index.fulltext.queryNodes(...)` -> BM25-ranked nodes.

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

fn strs(rows: &[Vec<CypherValue>]) -> Vec<String> {
    rows.iter()
        .map(|r| match &r[0] {
            CypherValue::String(s) => s.clone(),
            o => panic!("expected string, got {o:?}"),
        })
        .collect()
}

fn corpus() -> Graph {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE FULLTEXT INDEX docs FOR (d:Doc) ON EACH [d.title, d.body]",
    )
    .expect("create index");
    run(
        &mut g,
        "CREATE (:Doc {title: 'The Fox', body: 'the quick brown fox jumps over the lazy dog'})",
    )
    .expect("d1");
    run(
        &mut g,
        "CREATE (:Doc {title: 'Two Foxes', body: 'a quick brown dog outpaces a quick fox'})",
    )
    .expect("d2");
    run(
        &mut g,
        "CREATE (:Doc {title: 'Cats', body: 'lazy cats sleep all day'})",
    )
    .expect("d3");
    g
}

#[test]
fn create_and_query_ranks_by_bm25() {
    let mut g = corpus();
    let rows = run(
        &mut g,
        "CALL db.index.fulltext.queryNodes('docs', 'quick fox') YIELD node, score \
         RETURN node.title AS t ORDER BY score DESC",
    )
    .expect("query");
    let titles = strs(&rows);
    // "Two Foxes" (quickx2, fox) ranks above "The Fox"; "Cats" doesn't match
    assert_eq!(titles.first().map(String::as_str), Some("Two Foxes"));
    assert!(titles.contains(&"The Fox".to_string()));
    assert!(!titles.contains(&"Cats".to_string()));
}

#[test]
fn indexes_existing_nodes_on_create() {
    // nodes created BEFORE the index get indexed by CREATE
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE (:Doc {body: 'an ancient tale of a clever fox'})",
    )
    .expect("seed");
    run(
        &mut g,
        "CREATE FULLTEXT INDEX ft FOR (d:Doc) ON EACH [d.body]",
    )
    .expect("index");
    let rows = run(
        &mut g,
        "CALL db.index.fulltext.queryNodes('ft', 'fox') YIELD node RETURN count(node) AS c",
    )
    .expect("query");
    assert_eq!(rows[0][0], CypherValue::Integer(1));
}

#[test]
fn english_analyzer_stems() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE FULLTEXT INDEX ft FOR (d:Doc) ON EACH [d.body] \
         OPTIONS { indexConfig: { `fulltext.analyzer`: 'english' } }",
    )
    .expect("index");
    run(&mut g, "CREATE (:Doc {body: 'the running foxes jumped'})").expect("seed");
    // "run" must match the stemmed "running"
    let rows = run(
        &mut g,
        "CALL db.index.fulltext.queryNodes('ft', 'run') YIELD node RETURN count(node) AS c",
    )
    .expect("query");
    assert_eq!(rows[0][0], CypherValue::Integer(1));
}

#[test]
fn required_and_prohibited_operators() {
    let mut g = corpus();
    // +fox requires it; only the two fox docs match
    let must = run(
        &mut g,
        "CALL db.index.fulltext.queryNodes('docs', '+fox') YIELD node RETURN count(node) AS c",
    )
    .expect("must");
    assert_eq!(must[0][0], CypherValue::Integer(2));
    // -lazy excludes "The Fox" (which contains lazy), leaving "Two Foxes"
    let not = run(
        &mut g,
        "CALL db.index.fulltext.queryNodes('docs', 'quick -lazy') YIELD node \
         RETURN node.title AS t",
    )
    .expect("not");
    let titles = strs(&not);
    assert!(titles.contains(&"Two Foxes".to_string()));
    assert!(!titles.contains(&"The Fox".to_string()));
}

#[test]
fn incremental_maintenance_and_delete() {
    let mut g = corpus();
    // a new matching doc after creation
    run(
        &mut g,
        "CREATE (:Doc {title: 'New', body: 'another quick fox tale'})",
    )
    .expect("add");
    let after_add = run(
        &mut g,
        "CALL db.index.fulltext.queryNodes('docs', 'fox') YIELD node RETURN count(node) AS c",
    )
    .expect("q");
    assert_eq!(after_add[0][0], CypherValue::Integer(3));
    // deleted docs leave the index
    run(&mut g, "MATCH (d:Doc {title: 'New'}) DELETE d").expect("del");
    let after_del = run(
        &mut g,
        "CALL db.index.fulltext.queryNodes('docs', 'fox') YIELD node RETURN count(node) AS c",
    )
    .expect("q");
    assert_eq!(after_del[0][0], CypherValue::Integer(2));
}

#[test]
fn unknown_analyzer_errors() {
    let mut g = Graph::new();
    let err = run(
        &mut g,
        "CREATE FULLTEXT INDEX ft FOR (d:Doc) ON EACH [d.body] \
         OPTIONS { indexConfig: { `fulltext.analyzer`: 'klingon' } }",
    )
    .expect_err("unknown analyzer");
    assert!(err.to_lowercase().contains("analyzer"));
}

#[test]
fn missing_index_errors() {
    let mut g = Graph::new();
    let err = run(
        &mut g,
        "CALL db.index.fulltext.queryNodes('nope', 'x') YIELD node RETURN node",
    )
    .expect_err("no such index");
    assert!(err.to_lowercase().contains("fulltext"));
}

#[test]
fn multi_label_index() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE FULLTEXT INDEX ml FOR (n:Article|Post) ON EACH [n.text]",
    )
    .expect("index");
    run(
        &mut g,
        "CREATE (:Article {text: 'breaking news about a fox'})",
    )
    .expect("a");
    run(&mut g, "CREATE (:Post {text: 'my pet fox is lovely'})").expect("p");
    let rows = run(
        &mut g,
        "CALL db.index.fulltext.queryNodes('ml', 'fox') YIELD node RETURN count(node) AS c",
    )
    .expect("q");
    assert_eq!(rows[0][0], CypherValue::Integer(2));
}

// -- Lucene query syntax: phrase / prefix / fuzzy ---------------------

#[test]
fn phrase_query_end_to_end() {
    let mut g = corpus();
    // "quick brown" is adjacent in both fox docs, "brown quick" nowhere
    let rows = run(
        &mut g,
        "CALL db.index.fulltext.queryNodes('docs', '\"quick brown\"') YIELD node \
         RETURN node.title ORDER BY node.title",
    )
    .expect("phrase query");
    let titles = strs(&rows);
    assert_eq!(titles, vec!["The Fox", "Two Foxes"]);
    let rows2 = run(
        &mut g,
        "CALL db.index.fulltext.queryNodes('docs', '\"brown quick\"') YIELD node RETURN node.title",
    )
    .expect("reversed phrase");
    assert!(rows2.is_empty());
}

#[test]
fn prefix_query_end_to_end() {
    let mut g = corpus();
    let rows = run(
        &mut g,
        "CALL db.index.fulltext.queryNodes('docs', 'outp*') YIELD node RETURN node.title",
    )
    .expect("prefix query");
    assert_eq!(strs(&rows), vec!["Two Foxes"]);
}

#[test]
fn fuzzy_query_end_to_end() {
    let mut g = corpus();
    // "lazi" is the stem of lazy; "lazu" is 1 edit away
    let rows = run(
        &mut g,
        "CALL db.index.fulltext.queryNodes('docs', 'lazu~1') YIELD node \
         RETURN node.title ORDER BY node.title",
    )
    .expect("fuzzy query");
    let titles = strs(&rows);
    assert!(titles.contains(&"The Fox".to_string()));
    assert!(titles.contains(&"Cats".to_string()));
}

#[test]
fn required_phrase_with_prohibited_term() {
    let mut g = corpus();
    let rows = run(
        &mut g,
        "CALL db.index.fulltext.queryNodes('docs', '+\"quick brown\" -outpaces') \
         YIELD node RETURN node.title",
    )
    .expect("composed query");
    assert_eq!(strs(&rows), vec!["The Fox"]);
}
