//! A label-less property lookup, `MATCH (n {id: $x})`, seeks through the label indexes on the
//! property when they cover every node: no node without a label, an index on the property for
//! every label in use. Otherwise it scans, and a plan prepared while they covered rechecks when
//! it runs. Every answer is checked against the same query on the same graph with no indexes
//! (the plain scan).

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_core::tx::MvccStore;
use traverse_cypher::execute;
use traverse_cypher::types::CypherValue;

fn build(indexed: bool, extra: &[&str]) -> Graph {
    let mut g = Graph::new();
    for q in [
        "UNWIND range(0, 199) AS i CREATE (:Person {id: i, name: 'p' + i})",
        "UNWIND range(0, 99) AS i CREATE (:Post {id: 'post_' + i, n: i})",
        "UNWIND range(0, 49) AS i CREATE (:Post:Comment {id: 'post_' + (i * 2), n: i})",
        "UNWIND range(0, 9) AS i CREATE (:Tag {id: i * 1.5})",
        "MATCH (p:Person) WHERE p.id < 100 MATCH (m:Post) WHERE m.n = p.id \
         CREATE (m)-[:HAS_CREATOR]->(p)",
    ]
    .iter()
    .chain(extra)
    {
        execute(&mut g, q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    if indexed {
        for label in ["Person", "Post", "Comment", "Tag"] {
            execute(&mut g, &format!("CREATE INDEX FOR (n:{label}) ON (n.id)")).unwrap();
        }
    }
    g
}

fn rows(g: &mut Graph, q: &str) -> Vec<String> {
    let mut out: Vec<String> = execute(g, q)
        .unwrap_or_else(|e| panic!("{q}: {e}"))
        .rows
        .iter()
        .map(|r| format!("{r:?}"))
        .collect();
    out.sort();
    out
}

fn plan(g: &mut Graph, q: &str) -> String {
    format!("{:?}", execute(g, &format!("EXPLAIN {q}")).unwrap().plan)
}

fn uses_seek(g: &mut Graph, q: &str) -> bool {
    plan(g, q).contains("NodeIndexSeekAnyLabel")
}

const QUERIES: &[&str] = &[
    "MATCH (n {id: 5}) RETURN n.id, n.name",
    "MATCH (n {id: 'post_4'}) RETURN labels(n), n.n",
    "MATCH (n {id: 'post_4'})-[:HAS_CREATOR]->(p:Person) RETURN p.id",
    "MATCH (n) WHERE 7 = n.id RETURN n.name",
    "MATCH (n {id: 4.5}) RETURN n.id",
    "MATCH (n {id: 3.0}) RETURN n.id",
    "MATCH (n {id: 3}) RETURN n.id",
    "MATCH (n {id: 'nope'}) RETURN n",
    "MATCH (n {id: null}) RETURN n",
    "MATCH (n {id: 5, name: 'p5'}) RETURN n.id",
    "MATCH (n {id: 5, name: 'p6'}) RETURN n.id",
    "MATCH (n {id: 5}) RETURN count(*)",
];

#[test]
fn the_seek_answers_like_the_scan() {
    let mut plain = build(false, &[]);
    let mut indexed = build(true, &[]);
    for q in QUERIES {
        assert!(!uses_seek(&mut plain, q), "no index, no seek: {q}");
        assert!(uses_seek(&mut indexed, q), "covered, so it seeks: {q}");
        assert_eq!(rows(&mut indexed, q), rows(&mut plain, q), "{q}");
    }
}

/// A label disjunction narrows the scan to one scan per label (a deduplicating union); each takes
/// its own index on the property, with or without a cover, as does a labeled lookup with more
/// conjuncts.
#[test]
fn label_scans_seek_an_indexed_equality_among_other_conjuncts() {
    for extra in [&[][..], &["CREATE ({id: 'post_6', n: -1})"][..]] {
        let mut plain = build(false, extra);
        let mut indexed = build(true, extra);
        for q in [
            "MATCH (n) WHERE n.id = 'post_6' AND (n:Post OR n:Comment) RETURN n.n",
            "MATCH (n) WHERE (n:Comment OR n:Post) AND n.n >= 0 AND n.id = 'post_6' RETURN n.n",
            "MATCH (n:Post) WHERE n.n > 1 AND n.id = 'post_6' RETURN n.n",
            "MATCH (n:Post) WHERE n.n > 9 AND n.id = 'post_6' RETURN n.n",
        ] {
            let p = plan(&mut indexed, q);
            assert!(
                p.contains("NodeIndexScan") && !p.contains("NodeByLabelScan"),
                "{q}: {p}"
            );
            assert_eq!(rows(&mut indexed, q), rows(&mut plain, q), "{q}");
        }
    }
}

#[test]
fn an_unlabeled_node_or_an_unindexed_label_keeps_the_scan() {
    for extra in [
        "CREATE ({id: 5, name: 'loose'})",
        "CREATE (:Loose {id: 'post_4'})",
    ] {
        let mut plain = build(false, &[extra]);
        let mut indexed = build(true, &[extra]);
        for q in QUERIES {
            assert!(!uses_seek(&mut indexed, q), "{extra}: not covered: {q}");
            assert_eq!(rows(&mut indexed, q), rows(&mut plain, q), "{extra}: {q}");
        }
    }
}

/// A plan prepared while the indexes cover the graph keeps answering right after a node
/// without a label (or with an unindexed one) arrives: the scan rechecks the cover each run.
#[test]
fn a_prepared_seek_rechecks_the_cover() {
    let store = MvccStore::new(build(true, &[]));
    let q = "MATCH (n {id: $id}) RETURN n.name AS name";
    let params = HashMap::from([("id".to_string(), CypherValue::Integer(5))]);
    let prepared = {
        let tx = store.begin_read().unwrap();
        traverse_cypher::prepare_query(&tx, q, &params).unwrap()
    };
    assert!(format!("{:?}", prepared.plan_value()).contains("NodeIndexSeekAnyLabel"));
    let names = || {
        let mut tx = store.begin_read().unwrap();
        let mut out: Vec<String> = traverse_cypher::execute_prepared(&mut tx, &prepared, &params)
            .unwrap()
            .rows
            .iter()
            .map(|r| format!("{:?}", r[0]))
            .collect();
        out.sort();
        out
    };
    assert_eq!(names(), [r#"String("p5")"#]);
    for (i, create) in [
        "CREATE ({id: 5, name: 'loose'})",
        "CREATE (:Loose {id: 5, name: 'unindexed'})",
    ]
    .iter()
    .enumerate()
    {
        let mut tx = store.begin_write();
        traverse_cypher::execute(&mut tx, create).unwrap();
        tx.commit().unwrap();
        let got = names();
        assert_eq!(got.len(), 2 + i, "{create}: {got:?}");
        // again once nothing is versioned
        store.gc();
        assert_eq!(names(), got, "{create}");
    }
}
