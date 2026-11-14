//! A pattern that starts at a node sought by a correlated key, `UNWIND $ids AS id MATCH
//! (a:L {id: id})-[e]-(b)`, seeks that node through the index (an index lookup join) and expands
//! from it, as it does when the node stands in a MATCH of its own. It used to scan the whole label,
//! expand every node and hash-join the result against the driving rows. Every answer is checked
//! against the same query on the same graph with no indexes (the plain scan).

use traverse_core::graph::Graph;
use traverse_cypher::execute;

fn build(indexed: bool) -> Graph {
    let mut g = Graph::new();
    for q in [
        "UNWIND range(0, 199) AS i CREATE (:Person {id: i, marked: i % 5 = 0, roles: i % 13})",
        "UNWIND range(0, 39) AS i CREATE (:Company {id: i, score: i * 37 % 1500})",
        "UNWIND range(0, 9) AS i CREATE (:Other {id: i})",
        // every person sits in two companies; every seventh person's first role is an auditor's
        "MATCH (p:Person) MATCH (c:Company) WHERE c.id = p.id % 40 \
         CREATE (p)-[:REPRESENTATIVE_OF {positionType: CASE WHEN p.id % 7 = 0 THEN 'REV' ELSE 'LE' END}]->(c)",
        "MATCH (p:Person) MATCH (c:Company) WHERE c.id = (p.id * 3 + 1) % 40 \
         CREATE (p)-[:REPRESENTATIVE_OF {positionType: 'SU'}]->(c)",
        "MATCH (p:Person) WHERE p.id < 10 MATCH (o:Other {id: p.id}) CREATE (p)-[:KNOWS]->(o)",
    ] {
        execute(&mut g, q).unwrap_or_else(|e| panic!("{q}: {e}"));
    }
    if indexed {
        for label in ["Person", "Company"] {
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

/// Patterns starting at the sought node: the indexed graph seeks it per driving row
const SEEKING: &[&str] = &[
    // the people's companies, one role type left out (the step of a company network walk)
    "UNWIND [3, 7, 14, 50, 199, 1000] AS pid \
     MATCH (p:Person {id: pid})-[r:REPRESENTATIVE_OF]-(c:Company) \
     WHERE NOT r.positionType IN ['REVH', 'REVS', 'REV'] \
     RETURN DISTINCT c.id, c.score",
    "UNWIND [3, 7, 14] AS pid MATCH (p:Person {id: pid})-[:REPRESENTATIVE_OF]->(c:Company) RETURN p.id, c.id",
    "UNWIND [0, 5, 39] AS cid MATCH (c:Company {id: cid})<-[r:REPRESENTATIVE_OF]-(p:Person) \
     RETURN c.id, p.id, r.positionType",
    // a key from a map, and a second hop
    "UNWIND [{a: 2}, {a: 9}] AS row MATCH (p:Person {id: row.a})-[:REPRESENTATIVE_OF]-(c:Company) RETURN row.a, c.id",
    "UNWIND [1, 2] AS pid MATCH (p:Person {id: pid})-[:REPRESENTATIVE_OF]-(c:Company)-[:REPRESENTATIVE_OF]-(q:Person) \
     WHERE q.marked RETURN DISTINCT q.id",
    // a filter on the sought node and an aggregate above
    "UNWIND range(0, 30) AS pid MATCH (p:Person {id: pid})-[:REPRESENTATIVE_OF]-(c:Company) \
     WHERE p.roles <= 6 RETURN count(DISTINCT c.id), sum(c.score)",
    // a correlated key from an earlier MATCH
    "MATCH (o:Other) WHERE o.id < 4 MATCH (p:Person {id: o.id * 2})-[:REPRESENTATIVE_OF]-(c:Company) RETURN o.id, c.id",
];

/// The same answers whatever the plan: the sought node at the far end of the pattern, an
/// unindexed label, a key that uses the sought node itself
const ANSWERS_ONLY: &[&str] = &[
    "UNWIND [3, 7] AS pid MATCH (c:Company)-[:REPRESENTATIVE_OF]-(p:Person {id: pid}) RETURN c.id, p.id",
    "UNWIND [1, 2] AS oid MATCH (o:Other {id: oid})<-[:KNOWS]-(p:Person) RETURN o.id, p.id",
    "UNWIND [3] AS pid MATCH (p:Person)-[:REPRESENTATIVE_OF]-(c:Company) WHERE p.id = pid + p.roles - p.roles RETURN c.id",
    "UNWIND [] AS pid MATCH (p:Person {id: pid})-[:REPRESENTATIVE_OF]-(c:Company) RETURN c.id",
    "UNWIND [null, 4] AS pid MATCH (p:Person {id: pid})-[:REPRESENTATIVE_OF]-(c:Company) RETURN c.id",
];

#[test]
fn a_pattern_from_a_correlated_key_seeks_its_start_node() {
    let mut plain = build(false);
    let mut indexed = build(true);
    for q in SEEKING {
        let p = plan(&mut indexed, q);
        assert!(p.contains("NodeIndexLookupJoin"), "seeks: {q}\n{p}");
        assert!(
            !p.contains("HashJoin"),
            "no hash join over the label: {q}\n{p}"
        );
        assert_eq!(rows(&mut indexed, q), rows(&mut plain, q), "{q}");
        assert!(!rows(&mut indexed, q).is_empty(), "the graph answers: {q}");
    }
}

#[test]
fn other_patterns_answer_as_the_scan() {
    let mut plain = build(false);
    let mut indexed = build(true);
    for q in ANSWERS_ONLY {
        assert_eq!(rows(&mut indexed, q), rows(&mut plain, q), "{q}");
    }
}
