use super::*;
use std::collections::BTreeMap;
use traverse_core::graph::Graph;
use traverse_core::graph::property::PropertyValue;
use types::CypherValue;

fn empty_props() -> BTreeMap<String, PropertyValue> {
    BTreeMap::new()
}

fn props(pairs: &[(&str, PropertyValue)]) -> BTreeMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// Node property via the graph's string table.
fn node_prop(
    node: &traverse_core::graph::node::Node,
    graph: &traverse_core::graph::Graph,
    key: &str,
) -> Option<PropertyValue> {
    use traverse_core::graph::types::PropertyKeyId;
    let kid = PropertyKeyId(graph.strings().get(key)?);
    node.properties.get(kid)
}

// -- Scenario 1: MATCH (n) RETURN n -----------------------------
#[test]
fn strip_literals_keeps_qpp_quantifiers() {
    // QPP quantifier bounds are grammar, never stripped
    for (q, expect_kept) in [
        ("MATCH (a)-[:R]->{1,3}(b) RETURN b", "{1,3}"),
        ("MATCH (a)((x)-[:R]->(y)){2}(b) RETURN b", "{2}"),
        ("MATCH (a)-[:R]-{,4}(b) RETURN b", "{,4}"),
        ("MATCH (a)-[:R]->{2,}(b) RETURN b", "{2,}"),
    ] {
        let (stripped, _) = strip_literals(q).unwrap();
        assert!(
            stripped.contains(expect_kept),
            "quantifier {expect_kept} must survive stripping, got: {stripped}"
        );
    }
    // ...while ordinary literals in the same query still strip.
    let (stripped, params) =
        strip_literals("MATCH (a {v: 7})-[:R]->{1,3}(b) WHERE b.x = 9 RETURN b").unwrap();
    assert!(stripped.contains("{1,3}"), "{stripped}");
    assert_eq!(params.len(), 2, "7 and 9 stripped: {stripped}");
    // ...and map literals / subquery braces are untouched by the exemption.
    let (stripped2, params2) = strip_literals("RETURN {a: 5} AS m").unwrap();
    assert!(stripped2.contains("$__sp0"), "{stripped2}");
    assert_eq!(params2.len(), 1);
}

#[test]
fn strip_literals_keeps_fixed_lengths() {
    // `*2` is grammar like `*1..2`; a placeholder there is a syntax error
    for q in [
        "MATCH (a)-[:R*2]->(b) WHERE a.x = 7 RETURN b",
        "MATCH (a)-[r*2 {w: 7}]->(b) RETURN b",
    ] {
        let (stripped, params) = strip_literals(q).unwrap();
        assert!(
            stripped.contains("*2"),
            "fixed length must survive: {stripped}"
        );
        assert_eq!(params.len(), 1, "7 strips: {stripped}");
        crate::parse(&stripped).unwrap_or_else(|e| panic!("{stripped}: {e}"));
    }
    let (stripped, params) = strip_literals("RETURN 3 * 2 AS x").unwrap();
    assert_eq!(params.len(), 2, "arithmetic strips: {stripped}");
}

#[test]
fn strip_literals_keeps_selector_counts() {
    let (s, params) =
        strip_literals("MATCH p = SHORTEST 3 (:A {x: 7})-[:L]->+(:B) RETURN length(p)").unwrap();
    assert!(s.contains("SHORTEST 3"), "selector count must survive: {s}");
    assert!(!s.contains("7"), "data literal must strip: {s}");
    assert_eq!(params.len(), 1);
    let (s2, _) = strip_literals("MATCH p = ANY 2 (:A)-[:L]->+(:B) RETURN p").unwrap();
    assert!(s2.contains("ANY 2"), "{s2}");
}

#[test]
fn scan_all_nodes() {
    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Bob".into()))]),
    );

    let result = execute(&mut g, "MATCH (n) RETURN n").unwrap();
    assert_eq!(result.columns, vec!["n"]);
    assert_eq!(result.rows.len(), 2);
}

// -- Scenario 2: MATCH (n:Person) RETURN n.name -----------------
#[test]
fn label_filter_property_access() {
    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    g.create_node(
        &["Movie"],
        props(&[("title", PropertyValue::String("Matrix".into()))]),
    );

    let result = execute(&mut g, "MATCH (n:Person) RETURN n.name").unwrap();
    assert_eq!(result.columns, vec!["n.name"]);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::String("Alice".into()));
}

// -- Scenario 3: Pattern with edge type -------------------------
#[test]
fn pattern_with_edge_type() {
    let mut g = Graph::new();
    let alice = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    let bob = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Bob".into()))]),
    );
    g.create_edge(alice, bob, "KNOWS", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::String("Alice".into()));
    assert_eq!(result.rows[0][1], CypherValue::String("Bob".into()));
}

// -- Scenario 4: Variable-length path ---------------------------
#[test]
fn variable_length_path() {
    let mut g = Graph::new();
    let a = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );
    let b = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("B".into()))]),
    );
    let c = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("C".into()))]),
    );
    g.create_edge(a, b, "NEXT", empty_props()).unwrap();
    g.create_edge(b, c, "NEXT", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH (a:N {name: 'A'})-[:NEXT*1..3]->(b) RETURN b.name",
    )
    .unwrap();
    // B (1 hop) and C (2 hops)
    assert_eq!(result.rows.len(), 2);
    let names: Vec<&CypherValue> = result.rows.iter().map(|r| &r[0]).collect();
    assert!(names.contains(&&CypherValue::String("B".into())));
    assert!(names.contains(&&CypherValue::String("C".into())));
}

// -- Scenario 4b: Variable-length path with DISTINCT (DistinctExpand) --
#[test]
fn variable_length_path_distinct() {
    let mut g = Graph::new();
    let a = g.create_node(&["User"], props(&[("id", PropertyValue::Int(1))]));
    let b = g.create_node(&["User"], props(&[("id", PropertyValue::Int(2))]));
    let c = g.create_node(&["User"], props(&[("id", PropertyValue::Int(3))]));
    let d = g.create_node(&["User"], props(&[("id", PropertyValue::Int(4))]));
    g.create_edge(a, b, "KNOWS", empty_props()).unwrap();
    g.create_edge(a, c, "KNOWS", empty_props()).unwrap();
    g.create_edge(b, c, "KNOWS", empty_props()).unwrap(); // c reachable via both 1 and 2 hops
    g.create_edge(b, d, "KNOWS", empty_props()).unwrap();
    g.create_edge(c, d, "KNOWS", empty_props()).unwrap(); // d reachable via multiple paths

    // Query: 1..2 hops from user 1, distinct target User IDs
    let result = execute(
        &mut g,
        "MATCH (s:User {id: 1})-[*1..2]->(n:User) RETURN DISTINCT n.id",
    )
    .unwrap();
    // From user 1: hop1 = {2, 3}, hop2 = {3, 4} (from 2->3,4 and 3->4)
    // Distinct targets = {2, 3, 4}
    let mut ids: Vec<i64> = result
        .rows
        .iter()
        .map(|r| match &r[0] {
            CypherValue::Integer(i) => *i,
            _ => panic!("expected integer"),
        })
        .collect();
    ids.sort();
    assert_eq!(ids, vec![2, 3, 4]);
}

// -- Scenario 4c: Multi-hop DISTINCT expansion chains --
#[test]
fn multi_hop_distinct_expand() {
    let mut g = Graph::new();
    let a = g.create_node(&["User"], props(&[("id", PropertyValue::Int(1))]));
    let b = g.create_node(&["User"], props(&[("id", PropertyValue::Int(2))]));
    let c = g.create_node(&["User"], props(&[("id", PropertyValue::Int(3))]));
    let d = g.create_node(&["User"], props(&[("id", PropertyValue::Int(4))]));
    g.create_edge(a, b, "KNOWS", empty_props()).unwrap();
    g.create_edge(a, c, "KNOWS", empty_props()).unwrap();
    g.create_edge(b, d, "KNOWS", empty_props()).unwrap();
    g.create_edge(c, d, "KNOWS", empty_props()).unwrap();

    // 2-hop distinct expansion: (s)-->()-->(n:User) RETURN DISTINCT n.id
    let result = execute(
        &mut g,
        "MATCH (s:User {id: 1})-->()-->(n:User) RETURN DISTINCT n.id",
    )
    .unwrap();
    // From 1: hop1 = {2, 3}, hop2 from 2 = {4}, from 3 = {4}
    // Distinct = {4}
    let mut ids: Vec<i64> = result
        .rows
        .iter()
        .map(|r| match &r[0] {
            CypherValue::Integer(i) => *i,
            _ => panic!("expected integer"),
        })
        .collect();
    ids.sort();
    assert_eq!(ids, vec![4]);
}

// -- Scenario 5: CREATE -----------------------------------------
#[test]
fn create_node() {
    let mut g = Graph::new();
    let result = execute(&mut g, "CREATE (n:Person {name: 'Alice'}) RETURN n.name").unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::String("Alice".into()));
    assert_eq!(g.node_count(), 1);
}

// -- Scenario 6: Filter + sort + limit --------------------------
#[test]
fn filter_sort_limit() {
    let mut g = Graph::new();
    for age in [25, 35, 45, 20, 30] {
        g.create_node(&["Person"], props(&[("age", PropertyValue::Int(age))]));
    }

    let result = execute(
        &mut g,
        "MATCH (n:Person) WHERE n.age > 30 RETURN n.age ORDER BY n.age DESC LIMIT 10",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], CypherValue::Integer(45));
    assert_eq!(result.rows[1][0], CypherValue::Integer(35));
}

// -- Scenario 7: Aggregation ------------------------------------
#[test]
fn aggregation() {
    let mut g = Graph::new();
    g.create_node(&["Person"], props(&[("age", PropertyValue::Int(30))]));
    g.create_node(&["Person"], props(&[("age", PropertyValue::Int(40))]));
    g.create_node(&["Person"], props(&[("age", PropertyValue::Int(50))]));

    let result = execute(
        &mut g,
        "MATCH (n:Person) RETURN count(n) AS cnt, avg(n.age) AS avg_age",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::Integer(3));
    assert_eq!(result.rows[0][1], CypherValue::Float(40.0));
}

// -- Scenario 8: SET --------------------------------------------
#[test]
fn set_property() {
    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );

    execute(&mut g, "MATCH (n:Person) SET n.updated = true").unwrap();

    let node = g.nodes().next().unwrap();
    assert_eq!(
        node_prop(node, &g, "updated"),
        Some(PropertyValue::Bool(true))
    );
}

// -- Scenario 9: DETACH DELETE ----------------------------------
#[test]
fn detach_delete() {
    let mut g = Graph::new();
    let a = g.create_node(&["Temp"], empty_props());
    let b = g.create_node(&["Keep"], empty_props());
    g.create_edge(a, b, "R", empty_props()).unwrap();

    execute(&mut g, "MATCH (n:Temp) DETACH DELETE n").unwrap();
    assert_eq!(g.node_count(), 1);
    assert_eq!(g.edge_count(), 0);
}

// -- Scenario 10: Multi-part WITH -------------------------------
#[test]
fn with_aggregation() {
    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[("type", PropertyValue::String("A".into()))]),
    );
    g.create_node(
        &["Person"],
        props(&[("type", PropertyValue::String("A".into()))]),
    );
    g.create_node(
        &["Person"],
        props(&[("type", PropertyValue::String("B".into()))]),
    );

    let result = execute(
        &mut g,
        "MATCH (n:Person) WITH n.type AS t, count(*) AS c RETURN t, c ORDER BY c DESC",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 2);
    // "A" count 2 should come first (DESC)
    assert_eq!(result.rows[0][0], CypherValue::String("A".into()));
    assert_eq!(result.rows[0][1], CypherValue::Integer(2));
    assert_eq!(result.rows[1][0], CypherValue::String("B".into()));
    assert_eq!(result.rows[1][1], CypherValue::Integer(1));
}

// -- Additional: UNWIND -----------------------------------------
#[test]
fn unwind_list() {
    let mut g = Graph::new();
    let result = execute(&mut g, "UNWIND [1, 2, 3] AS x RETURN x").unwrap();
    assert_eq!(result.rows.len(), 3);
    assert_eq!(result.rows[0][0], CypherValue::Integer(1));
    assert_eq!(result.rows[1][0], CypherValue::Integer(2));
    assert_eq!(result.rows[2][0], CypherValue::Integer(3));
}

// -- Additional: String functions -------------------------------
#[test]
fn string_functions() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN toUpper('hello') AS upper, toLower('WORLD') AS lower, size('abc') AS sz",
    )
    .unwrap();
    assert_eq!(result.rows[0][0], CypherValue::String("HELLO".into()));
    assert_eq!(result.rows[0][1], CypherValue::String("world".into()));
    assert_eq!(result.rows[0][2], CypherValue::Integer(3));
}

// -- Additional: CASE expression --------------------------------
#[test]
fn case_expression() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN CASE WHEN 1 = 1 THEN 'yes' ELSE 'no' END AS answer",
    )
    .unwrap();
    assert_eq!(result.rows[0][0], CypherValue::String("yes".into()));
}

// -- Additional: Boolean expressions ----------------------------
#[test]
fn boolean_logic() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN true AND false AS a, true OR false AS b, NOT true AS c",
    )
    .unwrap();
    assert_eq!(result.rows[0][0], CypherValue::Bool(false));
    assert_eq!(result.rows[0][1], CypherValue::Bool(true));
    assert_eq!(result.rows[0][2], CypherValue::Bool(false));
}

// -- Additional: Arithmetic -------------------------------------
#[test]
fn arithmetic() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN 1 + 2 AS sum, 10 - 3 AS diff, 4 * 5 AS prod, 10 / 3 AS quot, 10 % 3 AS modulo",
    )
    .unwrap();
    assert_eq!(result.rows[0][0], CypherValue::Integer(3));
    assert_eq!(result.rows[0][1], CypherValue::Integer(7));
    assert_eq!(result.rows[0][2], CypherValue::Integer(20));
    assert_eq!(result.rows[0][3], CypherValue::Integer(3));
    assert_eq!(result.rows[0][4], CypherValue::Integer(1));
}

// -- Additional: List operations --------------------------------
#[test]
fn list_operations() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN [1,2,3] AS list, head([1,2,3]) AS h, tail([1,2,3]) AS t, last([1,2,3]) AS l",
    )
    .unwrap();
    assert_eq!(
        result.rows[0][0],
        CypherValue::List(vec![
            CypherValue::Integer(1),
            CypherValue::Integer(2),
            CypherValue::Integer(3),
        ])
    );
    assert_eq!(result.rows[0][1], CypherValue::Integer(1));
    assert_eq!(
        result.rows[0][2],
        CypherValue::List(vec![CypherValue::Integer(2), CypherValue::Integer(3)])
    );
    assert_eq!(result.rows[0][3], CypherValue::Integer(3));
}

// -- Additional: MERGE ------------------------------------------
#[test]
fn merge_creates_when_missing() {
    let mut g = Graph::new();
    execute(&mut g, "MERGE (n:Person {name: 'Alice'})").unwrap();
    assert_eq!(g.node_count(), 1);
    // Second merge should not create duplicate
    execute(&mut g, "MERGE (n:Person {name: 'Alice'})").unwrap();
    assert_eq!(g.node_count(), 1);
}

// -- Additional: CREATE with relationship -----------------------
#[test]
fn create_with_relationship() {
    let mut g = Graph::new();
    execute(
        &mut g,
        "CREATE (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person {name: 'Bob'})",
    )
    .unwrap();
    assert_eq!(g.node_count(), 2);
    assert_eq!(g.edge_count(), 1);
}

// -- Additional: SKIP -------------------------------------------
#[test]
fn skip_and_limit() {
    let mut g = Graph::new();
    for i in 0..5 {
        g.create_node(&["N"], props(&[("val", PropertyValue::Int(i))]));
    }
    let result = execute(
        &mut g,
        "MATCH (n:N) RETURN n.val ORDER BY n.val SKIP 2 LIMIT 2",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], CypherValue::Integer(2));
    assert_eq!(result.rows[1][0], CypherValue::Integer(3));
}

// -- Additional: IS NULL / IS NOT NULL --------------------------
#[test]
fn null_checks() {
    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    g.create_node(&["Person"], empty_props());

    let result = execute(
        &mut g,
        "MATCH (n:Person) WHERE n.name IS NOT NULL RETURN n.name",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::String("Alice".into()));
}

// -- Additional: IN operator ------------------------------------
#[test]
fn in_operator() {
    let mut g = Graph::new();
    let result = execute(&mut g, "RETURN 2 IN [1, 2, 3] AS yes, 4 IN [1, 2, 3] AS no").unwrap();
    assert_eq!(result.rows[0][0], CypherValue::Bool(true));
    assert_eq!(result.rows[0][1], CypherValue::Bool(false));
}

// -- Additional: Coalesce ---------------------------------------
#[test]
fn coalesce_function() {
    let mut g = Graph::new();
    let result = execute(&mut g, "RETURN coalesce(null, null, 42, 99) AS val").unwrap();
    assert_eq!(result.rows[0][0], CypherValue::Integer(42));
}

// -- Additional: String predicates ------------------------------
#[test]
fn string_predicates() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN 'hello world' STARTS WITH 'hello' AS sw, 'hello world' ENDS WITH 'world' AS ew, 'hello world' CONTAINS 'lo wo' AS ct",
    )
    .unwrap();
    assert_eq!(result.rows[0][0], CypherValue::Bool(true));
    assert_eq!(result.rows[0][1], CypherValue::Bool(true));
    assert_eq!(result.rows[0][2], CypherValue::Bool(true));
}

// -- Additional: RETURN * ---------------------------------------
#[test]
fn return_star() {
    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    let result = execute(&mut g, "MATCH (n:Person) RETURN *").unwrap();
    assert_eq!(result.rows.len(), 1);
}

// -- Additional: UNION ------------------------------------------
#[test]
fn union_all() {
    let mut g = Graph::new();
    let result = execute(&mut g, "RETURN 1 AS x UNION ALL RETURN 2 AS x").unwrap();
    assert_eq!(result.rows.len(), 2);
}

// -- Additional: range() function -------------------------------
#[test]
fn range_function() {
    let mut g = Graph::new();
    let result = execute(&mut g, "RETURN range(1, 5) AS r").unwrap();
    assert_eq!(
        result.rows[0][0],
        CypherValue::List(vec![
            CypherValue::Integer(1),
            CypherValue::Integer(2),
            CypherValue::Integer(3),
            CypherValue::Integer(4),
            CypherValue::Integer(5),
        ])
    );
}

// -- Additional: REMOVE property --------------------------------
#[test]
fn remove_property() {
    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[
            ("name", PropertyValue::String("Alice".into())),
            ("age", PropertyValue::Int(30)),
        ]),
    );
    execute(&mut g, "MATCH (n:Person) REMOVE n.age").unwrap();
    let node = g.nodes().next().unwrap();
    assert!(node_prop(node, &g, "age").is_none());
    assert_eq!(
        node_prop(node, &g, "name"),
        Some(PropertyValue::String("Alice".into()))
    );
}

// -- OPTIONAL MATCH -------------------------------------------
#[test]
fn optional_match_with_null() {
    let mut g = Graph::new();
    let alice = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    let _bob = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Bob".into()))]),
    );
    g.create_edge(alice, _bob, "KNOWS", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH (a:Person) OPTIONAL MATCH (a)-[:KNOWS]->(b:Person) RETURN a.name, b.name",
    )
    .unwrap();
    // Alice knows Bob, Bob knows nobody
    assert_eq!(result.rows.len(), 2);
    let mut found_alice = false;
    let mut found_bob_null = false;
    for row in &result.rows {
        if row[0] == CypherValue::String("Alice".into()) {
            assert_eq!(row[1], CypherValue::String("Bob".into()));
            found_alice = true;
        }
        if row[0] == CypherValue::String("Bob".into()) {
            assert_eq!(row[1], CypherValue::Null);
            found_bob_null = true;
        }
    }
    assert!(found_alice, "Alice->Bob relationship should be found");
    assert!(found_bob_null, "Bob should have null for optional match");
}

// OPTIONAL MATCH with an unbound first node and a bound tail must re-root at the bound endpoint
// and expand in reverse, not scan every node per outer row.
#[test]
fn optional_match_unbound_first_bound_tail() {
    let mut g = Graph::new();
    let alice = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    let bob = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Bob".into()))]),
    );
    let carol = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Carol".into()))]),
    );
    g.create_edge(alice, bob, "KNOWS", empty_props()).unwrap();
    g.create_edge(carol, bob, "KNOWS", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH (b:Person {name: 'Bob'}) \
         OPTIONAL MATCH (prev)-[:KNOWS]->(b) \
         RETURN b.name, prev.name ORDER BY prev.name",
    )
    .unwrap();
    // Both Alice and Carol know Bob.
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], CypherValue::String("Bob".into()));
    assert_eq!(result.rows[0][1], CypherValue::String("Alice".into()));
    assert_eq!(result.rows[1][0], CypherValue::String("Bob".into()));
    assert_eq!(result.rows[1][1], CypherValue::String("Carol".into()));
}

// second MATCH with unbound first node and bound tail: same rewrite, no scan of all nodes for
// `prev`.
#[test]
fn second_match_unbound_first_bound_tail() {
    let mut g = Graph::new();
    let alice = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    let bob = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Bob".into()))]),
    );
    g.create_edge(alice, bob, "KNOWS", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH (b:Person {name: 'Bob'}) \
         MATCH (prev)-[:KNOWS]->(b) \
         RETURN prev.name",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::String("Alice".into()));
}

// bound EDGE variable reused in OPTIONAL MATCH: endpoints must come from the bound edge, not a
// full node scan per outer row.
#[test]
fn optional_match_bound_edge_undirected() {
    let mut g = Graph::new();
    let alice = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    let bob = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Bob".into()))]),
    );
    let carol = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Carol".into()))]),
    );
    g.create_edge(alice, bob, "KNOWS", empty_props()).unwrap();
    g.create_edge(bob, carol, "KNOWS", empty_props()).unwrap();

    // r bound by the first MATCH and reused; undirected, so 2 rows per outer row (one per
    // orientation)
    let result = execute(
        &mut g,
        "MATCH (a:Person {name: 'Alice'})-[r:KNOWS]->(b:Person) \
         OPTIONAL MATCH (hub)-[r:KNOWS]-(other) \
         RETURN a.name, b.name, hub.name, other.name ORDER BY hub.name, other.name",
    )
    .unwrap();
    // a=Alice, b=Bob, r = Alice->Bob; undirected: (Alice,Bob) and (Bob,Alice)
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][2], CypherValue::String("Alice".into()));
    assert_eq!(result.rows[0][3], CypherValue::String("Bob".into()));
    assert_eq!(result.rows[1][2], CypherValue::String("Bob".into()));
    assert_eq!(result.rows[1][3], CypherValue::String("Alice".into()));
}

// Same, but with a directed pattern reuse.
#[test]
fn optional_match_bound_edge_directed() {
    let mut g = Graph::new();
    let alice = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    let bob = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Bob".into()))]),
    );
    g.create_edge(alice, bob, "KNOWS", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH (a:Person {name: 'Alice'})-[r:KNOWS]->(b:Person) \
         OPTIONAL MATCH (hub)-[r:KNOWS]->(other) \
         RETURN hub.name, other.name",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::String("Alice".into()));
    assert_eq!(result.rows[0][1], CypherValue::String("Bob".into()));
}

// type mismatch: the bound edge is X, the pattern wants Y. No inner rows; OPTIONAL MATCH still
// emits the outer row with NULLs.
#[test]
fn optional_match_bound_edge_type_mismatch() {
    let mut g = Graph::new();
    let alice = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    let bob = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Bob".into()))]),
    );
    g.create_edge(alice, bob, "KNOWS", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH (a:Person {name: 'Alice'})-[r:KNOWS]->(b:Person) \
         OPTIONAL MATCH (hub)-[r:LIKES]->(other) \
         RETURN a.name, hub.name",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::String("Alice".into()));
    assert_eq!(result.rows[0][1], CypherValue::Null);
}

// middle-bound `(a)-[r1]->(b)-[r2]->(c)` with `b` bound earlier: split into two parts so
// neither side scans the graph.
#[test]
fn optional_match_middle_bound() {
    let mut g = Graph::new();
    let alice = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    let bob = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Bob".into()))]),
    );
    let carol = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Carol".into()))]),
    );
    g.create_edge(alice, bob, "KNOWS", empty_props()).unwrap();
    g.create_edge(bob, carol, "KNOWS", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH (b:Person {name: 'Bob'}) \
         OPTIONAL MATCH (a)-[:KNOWS]->(b)-[:KNOWS]->(c) \
         RETURN a.name, b.name, c.name",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::String("Alice".into()));
    assert_eq!(result.rows[0][1], CypherValue::String("Bob".into()));
    assert_eq!(result.rows[0][2], CypherValue::String("Carol".into()));
}

// -- Regex match (=~) -----------------------------------------
#[test]
fn regex_match() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN 'hello world' =~ 'hello.*' AS yes, 'abc' =~ '^xyz$' AS no",
    )
    .unwrap();
    assert_eq!(result.rows[0][0], CypherValue::Bool(true));
    assert_eq!(result.rows[0][1], CypherValue::Bool(false));
}

#[test]
fn regex_match_with_nodes() {
    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Bob".into()))]),
    );
    g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alex".into()))]),
    );

    let result = execute(
        &mut g,
        "MATCH (n:Person) WHERE n.name =~ 'Al.*' RETURN n.name ORDER BY n.name",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], CypherValue::String("Alex".into()));
    assert_eq!(result.rows[1][0], CypherValue::String("Alice".into()));
}

#[test]
fn regex_match_and_apoc_text_keep_their_own_compiled_patterns() {
    // `=~` matches the whole string, apoc.text.split searches: the same pattern text compiled
    // for one must not be handed to the other, whichever runs first (the caches outlive a
    // query)
    let mut g = Graph::new();
    let s = |v: &str| CypherValue::String(v.into());
    for _ in 0..2 {
        let r = execute(
            &mut g,
            "RETURN 'a,b' =~ ',' AS full, apoc.text.split('a,b', ',') AS parts",
        )
        .unwrap();
        assert_eq!(r.rows[0][0], CypherValue::Bool(false));
        assert_eq!(r.rows[0][1], CypherValue::List(vec![s("a"), s("b")]));
        let r = execute(
            &mut g,
            "RETURN apoc.text.split('x;y', ';') AS parts, 'x;y' =~ ';' AS full",
        )
        .unwrap();
        assert_eq!(r.rows[0][0], CypherValue::List(vec![s("x"), s("y")]));
        assert_eq!(r.rows[0][1], CypherValue::Bool(false));
    }
}

// -- List comprehension ---------------------------------------
#[test]
fn list_comprehension_basic() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN [x IN [1, 2, 3, 4, 5] WHERE x > 2] AS filtered",
    )
    .unwrap();
    assert_eq!(
        result.rows[0][0],
        CypherValue::List(vec![
            CypherValue::Integer(3),
            CypherValue::Integer(4),
            CypherValue::Integer(5),
        ])
    );
}

#[test]
fn list_comprehension_with_map() {
    let mut g = Graph::new();
    let result = execute(&mut g, "RETURN [x IN [1, 2, 3] | x * 2] AS doubled").unwrap();
    assert_eq!(
        result.rows[0][0],
        CypherValue::List(vec![
            CypherValue::Integer(2),
            CypherValue::Integer(4),
            CypherValue::Integer(6),
        ])
    );
}

#[test]
fn list_comprehension_filter_and_map() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN [x IN range(1, 10) WHERE x % 2 = 0 | x * x] AS squares",
    )
    .unwrap();
    assert_eq!(
        result.rows[0][0],
        CypherValue::List(vec![
            CypherValue::Integer(4),
            CypherValue::Integer(16),
            CypherValue::Integer(36),
            CypherValue::Integer(64),
            CypherValue::Integer(100),
        ])
    );
}

// -- Existential subquery -------------------------------------
#[test]
fn exists_subquery() {
    let mut g = Graph::new();
    let alice = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    let bob = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Bob".into()))]),
    );
    g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Charlie".into()))]),
    );
    g.create_edge(alice, bob, "KNOWS", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH (n:Person) WHERE EXISTS { MATCH (n)-[:KNOWS]->() } RETURN n.name",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::String("Alice".into()));
}

// -- COUNT subquery -------------------------------------------
#[test]
fn count_subquery() {
    let mut g = Graph::new();
    let alice = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    let bob = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Bob".into()))]),
    );
    let charlie = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Charlie".into()))]),
    );
    g.create_edge(alice, bob, "KNOWS", empty_props()).unwrap();
    g.create_edge(alice, charlie, "KNOWS", empty_props())
        .unwrap();

    let result = execute(
        &mut g,
        "MATCH (n:Person) WHERE COUNT { MATCH (n)-[:KNOWS]->() } > 1 RETURN n.name",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::String("Alice".into()));
}

// -- Named paths ----------------------------------------------
#[test]
fn named_path() {
    let mut g = Graph::new();
    let alice = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    let bob = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Bob".into()))]),
    );
    g.create_edge(alice, bob, "KNOWS", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH p = (a:Person {name: 'Alice'})-[:KNOWS]->(b:Person) RETURN p, length(p)",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 1);
    // p should be a Path
    match &result.rows[0][0] {
        CypherValue::Path { nodes, edges } => {
            assert_eq!(nodes.len(), 2);
            assert_eq!(edges.len(), 1);
        }
        other => panic!("expected Path, got {other:?}"),
    }
    // length(p) should be 1
    assert_eq!(result.rows[0][1], CypherValue::Integer(1));
}

#[test]
fn named_path_with_nodes_function() {
    let mut g = Graph::new();
    let a = g.create_node(&["N"], props(&[("v", PropertyValue::Int(1))]));
    let b = g.create_node(&["N"], props(&[("v", PropertyValue::Int(2))]));
    let c = g.create_node(&["N"], props(&[("v", PropertyValue::Int(3))]));
    g.create_edge(a, b, "NEXT", empty_props()).unwrap();
    g.create_edge(b, c, "NEXT", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH p = (a:N {v: 1})-[:NEXT*1..3]->(c) RETURN length(p) ORDER BY length(p)",
    )
    .unwrap();
    // Should find paths of length 1 (a->b) and length 2 (a->b->c)
    assert_eq!(result.rows.len(), 2);
}

// -- RETURN DISTINCT ------------------------------------------
#[test]
fn return_distinct() {
    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[("city", PropertyValue::String("NYC".into()))]),
    );
    g.create_node(
        &["Person"],
        props(&[("city", PropertyValue::String("NYC".into()))]),
    );
    g.create_node(
        &["Person"],
        props(&[("city", PropertyValue::String("LA".into()))]),
    );

    let result = execute(
        &mut g,
        "MATCH (n:Person) RETURN DISTINCT n.city AS city ORDER BY city",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], CypherValue::String("LA".into()));
    assert_eq!(result.rows[1][0], CypherValue::String("NYC".into()));
}

// -- WITH WHERE -----------------------------------------------
#[test]
fn with_where_filter() {
    let mut g = Graph::new();
    for i in [1, 2, 3, 4, 5] {
        g.create_node(&["N"], props(&[("val", PropertyValue::Int(i))]));
    }

    let result = execute(
        &mut g,
        "MATCH (n:N) WITH n.val AS v WHERE v > 3 RETURN v ORDER BY v",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 2);
    assert_eq!(result.rows[0][0], CypherValue::Integer(4));
    assert_eq!(result.rows[1][0], CypherValue::Integer(5));
}

// -- List indexing and slicing ---------------------------------
#[test]
fn list_indexing_and_slicing() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN [1,2,3,4,5][0] AS first, [1,2,3,4,5][-1] AS last, [1,2,3,4,5][1..3] AS slice",
    )
    .unwrap();
    assert_eq!(result.rows[0][0], CypherValue::Integer(1));
    assert_eq!(result.rows[0][1], CypherValue::Integer(5));
    assert_eq!(
        result.rows[0][2],
        CypherValue::List(vec![CypherValue::Integer(2), CypherValue::Integer(3)])
    );
}
// -- XOR ------------------------------------------------------
#[test]
fn xor_operator() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN true XOR false AS a, true XOR true AS b, false XOR false AS c",
    )
    .unwrap();
    assert_eq!(result.rows[0][0], CypherValue::Bool(true));
    assert_eq!(result.rows[0][1], CypherValue::Bool(false));
    assert_eq!(result.rows[0][2], CypherValue::Bool(false));
}

// -- Power operator -------------------------------------------
#[test]
fn power_operator() {
    let mut g = Graph::new();
    let result = execute(&mut g, "RETURN 2 ^ 10 AS val").unwrap();
    assert_eq!(result.rows[0][0], CypherValue::Float(1024.0));
}

// -- Map literal ----------------------------------------------
#[test]
fn map_literal() {
    let mut g = Graph::new();
    let result = execute(&mut g, "RETURN {name: 'Alice', age: 30} AS m").unwrap();
    match &result.rows[0][0] {
        CypherValue::Map(m) => {
            assert_eq!(m.get("name"), Some(&CypherValue::String("Alice".into())));
            assert_eq!(m.get("age"), Some(&CypherValue::Integer(30)));
        }
        other => panic!("expected Map, got {other:?}"),
    }
}

// -- Multiple aggregation functions ----------------------------
#[test]
fn multiple_aggregations() {
    let mut g = Graph::new();
    for v in [10, 20, 30, 40, 50] {
        g.create_node(&["N"], props(&[("v", PropertyValue::Int(v))]));
    }

    let result = execute(
        &mut g,
        "MATCH (n:N) RETURN sum(n.v) AS s, min(n.v) AS mn, max(n.v) AS mx, collect(n.v) AS c",
    )
    .unwrap();
    assert_eq!(result.rows[0][0], CypherValue::Integer(150));
    assert_eq!(result.rows[0][1], CypherValue::Integer(10));
    assert_eq!(result.rows[0][2], CypherValue::Integer(50));
    if let CypherValue::List(items) = &result.rows[0][3] {
        assert_eq!(items.len(), 5);
    } else {
        panic!("expected list for collect()");
    }
}

// -- Chained comparison ---------------------------------------
#[test]
fn chained_comparison() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN 1 < 2 AS a, 3 > 2 AS b, 1 <= 1 AS c, 5 >= 6 AS d, 1 <> 2 AS e",
    )
    .unwrap();
    assert_eq!(result.rows[0][0], CypherValue::Bool(true));
    assert_eq!(result.rows[0][1], CypherValue::Bool(true));
    assert_eq!(result.rows[0][2], CypherValue::Bool(true));
    assert_eq!(result.rows[0][3], CypherValue::Bool(false));
    assert_eq!(result.rows[0][4], CypherValue::Bool(true));
}

// -- Conversion functions -------------------------------------
#[test]
fn conversion_functions() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN toString(42) AS s, toInteger('123') AS i, toFloat('1.25') AS f, toBoolean('true') AS b",
    )
    .unwrap();
    assert_eq!(result.rows[0][0], CypherValue::String("42".into()));
    assert_eq!(result.rows[0][1], CypherValue::Integer(123));
    assert_eq!(result.rows[0][2], CypherValue::Float(1.25));
    assert_eq!(result.rows[0][3], CypherValue::Bool(true));
}

// -- Numeric functions ----------------------------------------
#[test]
fn numeric_functions() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN abs(-5) AS a, ceil(2.3) AS b, floor(2.7) AS c, round(2.5) AS d, sign(-3) AS e",
    )
    .unwrap();
    assert_eq!(result.rows[0][0], CypherValue::Integer(5));
    assert_eq!(result.rows[0][1], CypherValue::Float(3.0));
    assert_eq!(result.rows[0][2], CypherValue::Float(2.0));
    assert_eq!(result.rows[0][3], CypherValue::Float(3.0));
    assert_eq!(result.rows[0][4], CypherValue::Integer(-1));
}

// -- String functions extended ---------------------------------
#[test]
fn string_functions_extended() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN trim('  hello  ') AS t, replace('hello', 'l', 'r') AS r, substring('hello', 1, 3) AS s, left('hello', 2) AS l, right('hello', 2) AS ri, split('a,b,c', ',') AS sp, reverse('abc') AS rev",
    )
    .unwrap();
    assert_eq!(result.rows[0][0], CypherValue::String("hello".into()));
    assert_eq!(result.rows[0][1], CypherValue::String("herro".into()));
    assert_eq!(result.rows[0][2], CypherValue::String("ell".into()));
    assert_eq!(result.rows[0][3], CypherValue::String("he".into()));
    assert_eq!(result.rows[0][4], CypherValue::String("lo".into()));
    assert_eq!(
        result.rows[0][5],
        CypherValue::List(vec![
            CypherValue::String("a".into()),
            CypherValue::String("b".into()),
            CypherValue::String("c".into()),
        ])
    );
    assert_eq!(result.rows[0][6], CypherValue::String("cba".into()));
}

// -- Node identity and type functions --------------------------
#[test]
fn identity_and_type_functions() {
    let mut g = Graph::new();
    let nid = g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    let eid = g.create_edge(nid, nid, "SELF", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH (n:Person)-[r:SELF]->(m) RETURN id(n), labels(n), type(r), keys(n)",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::Integer(nid.0 as i64));
    assert_eq!(
        result.rows[0][1],
        CypherValue::List(vec![CypherValue::String("Person".into())])
    );
    assert_eq!(result.rows[0][2], CypherValue::String("SELF".into()));
    assert_eq!(
        result.rows[0][3],
        CypherValue::List(vec![CypherValue::String("name".into())])
    );
    let _ = eid; // used above via the query
}

// -- Simple CASE ----------------------------------------------
#[test]
fn simple_case_expression() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN CASE 2 WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'other' END AS val",
    )
    .unwrap();
    assert_eq!(result.rows[0][0], CypherValue::String("two".into()));
}

// -- Undirected relationship match ----------------------------
#[test]
fn undirected_relationship() {
    let mut g = Graph::new();
    let a = g.create_node(&["N"], props(&[("v", PropertyValue::Int(1))]));
    let b = g.create_node(&["N"], props(&[("v", PropertyValue::Int(2))]));
    g.create_edge(a, b, "R", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH (x:N)-[:R]-(y:N) RETURN x.v, y.v ORDER BY x.v, y.v",
    )
    .unwrap();
    // Undirected: should match both directions
    assert_eq!(result.rows.len(), 2);
}

// -- Multiple pattern parts -----------------------------------
#[test]
fn multiple_match_patterns() {
    let mut g = Graph::new();
    g.create_node(&["A"], props(&[("v", PropertyValue::Int(1))]));
    g.create_node(&["B"], props(&[("v", PropertyValue::Int(2))]));

    let result = execute(&mut g, "MATCH (a:A), (b:B) RETURN a.v, b.v").unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::Integer(1));
    assert_eq!(result.rows[0][1], CypherValue::Integer(2));
}

// -- SET label ------------------------------------------------
#[test]
fn set_label() {
    let mut g = Graph::new();
    g.create_node(&["Person"], empty_props());

    execute(&mut g, "MATCH (n:Person) SET n:Employee").unwrap();

    let node = g.nodes().next().unwrap();
    let labels: Vec<String> = node
        .labels
        .iter()
        .map(|lid| g.strings().resolve(lid.0).to_string())
        .collect();
    assert!(labels.contains(&"Person".to_string()));
    assert!(labels.contains(&"Employee".to_string()));
}

// -- REMOVE label ---------------------------------------------
#[test]
fn remove_label() {
    let mut g = Graph::new();
    let nid = g.create_node(&["Person"], empty_props());
    let _ = g.add_label(nid, "Temp");

    execute(&mut g, "MATCH (n:Person) REMOVE n:Temp").unwrap();

    let node = g.get_node(nid).unwrap();
    let labels: Vec<String> = node
        .labels
        .iter()
        .map(|lid| g.strings().resolve(lid.0).to_string())
        .collect();
    assert!(labels.contains(&"Person".to_string()));
    assert!(!labels.contains(&"Temp".to_string()));
}

// -- Properties function --------------------------------------
#[test]
fn properties_function() {
    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[
            ("name", PropertyValue::String("Alice".into())),
            ("age", PropertyValue::Int(30)),
        ]),
    );

    let result = execute(&mut g, "MATCH (n:Person) RETURN properties(n) AS p").unwrap();
    match &result.rows[0][0] {
        CypherValue::Map(m) => {
            assert_eq!(m.get("name"), Some(&CypherValue::String("Alice".into())));
            assert_eq!(m.get("age"), Some(&CypherValue::Integer(30)));
        }
        other => panic!("expected Map, got {other:?}"),
    }
}

// -- MERGE with ON MATCH SET ----------------------------------
#[test]
fn merge_on_match_set() {
    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );

    execute(
        &mut g,
        "MERGE (n:Person {name: 'Alice'}) ON MATCH SET n.found = true",
    )
    .unwrap();
    assert_eq!(g.node_count(), 1);
    let node = g.nodes().next().unwrap();
    assert_eq!(
        node_prop(node, &g, "found"),
        Some(PropertyValue::Bool(true))
    );
}

// -- SET with replace all properties --------------------------
#[test]
fn set_all_properties() {
    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[
            ("name", PropertyValue::String("Alice".into())),
            ("age", PropertyValue::Int(30)),
        ]),
    );

    execute(&mut g, "MATCH (n:Person) SET n = {name: 'Bob'}").unwrap();
    let node = g.nodes().next().unwrap();
    assert_eq!(
        node_prop(node, &g, "name"),
        Some(PropertyValue::String("Bob".into()))
    );
    // age should be gone (replaced all)
    assert!(node_prop(node, &g, "age").is_none());
}

// -- SET with merge properties --------------------------------
#[test]
fn set_merge_properties() {
    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[
            ("name", PropertyValue::String("Alice".into())),
            ("age", PropertyValue::Int(30)),
        ]),
    );

    execute(&mut g, "MATCH (n:Person) SET n += {city: 'NYC'}").unwrap();
    let node = g.nodes().next().unwrap();
    assert_eq!(
        node_prop(node, &g, "name"),
        Some(PropertyValue::String("Alice".into()))
    );
    assert_eq!(node_prop(node, &g, "age"), Some(PropertyValue::Int(30)));
    assert_eq!(
        node_prop(node, &g, "city"),
        Some(PropertyValue::String("NYC".into()))
    );
}

// -- String concatenation -------------------------------------
#[test]
fn string_concatenation() {
    let mut g = Graph::new();
    let result = execute(&mut g, "RETURN 'hello' + ' ' + 'world' AS s").unwrap();
    assert_eq!(result.rows[0][0], CypherValue::String("hello world".into()));
}

// -- List concatenation ---------------------------------------
#[test]
fn list_concatenation() {
    let mut g = Graph::new();
    let result = execute(&mut g, "RETURN [1, 2] + [3, 4] AS l").unwrap();
    assert_eq!(
        result.rows[0][0],
        CypherValue::List(vec![
            CypherValue::Integer(1),
            CypherValue::Integer(2),
            CypherValue::Integer(3),
            CypherValue::Integer(4),
        ])
    );
}

// -- Null propagation in arithmetic ---------------------------
#[test]
fn null_propagation() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "RETURN null + 1 AS a, null = null AS b, null <> null AS c",
    )
    .unwrap();
    assert_eq!(result.rows[0][0], CypherValue::Null);
    // Cypher null = null is null (PartialEq says equal, the comparison yields Null)
}

// -- exists() function (scalar) -------------------------------
#[test]
fn exists_function() {
    let mut g = Graph::new();
    let result = execute(&mut g, "RETURN exists(42) AS a, exists(null) AS b").unwrap();
    assert_eq!(result.rows[0][0], CypherValue::Bool(true));
    assert_eq!(result.rows[0][1], CypherValue::Bool(false));
}

// -- Relationship with multiple types -------------------------
#[test]
fn multi_type_relationship() {
    let mut g = Graph::new();
    let a = g.create_node(&["N"], empty_props());
    let b = g.create_node(&["N"], empty_props());
    let c = g.create_node(&["N"], empty_props());
    g.create_edge(a, b, "KNOWS", empty_props()).unwrap();
    g.create_edge(a, c, "LIKES", empty_props()).unwrap();

    let result = execute(&mut g, "MATCH (a:N)-[:KNOWS|LIKES]->(b) RETURN b").unwrap();
    assert_eq!(result.rows.len(), 2);
}

#[test]
fn datetime_tz_name_propagation() {
    let mut g = Graph::new();
    let result = execute(
        &mut g,
        "WITH date({year: 1984, month: 10, day: 11}) AS otherDate, datetime({year: 1984, month: 10, day: 11, hour: 12, timezone: 'Europe/Stockholm'}) AS otherTime\nRETURN datetime({date: otherDate, time: otherTime}) AS result",
    ).unwrap();
    let val = &result.rows[0][0];
    assert_eq!(format!("{val}"), "1984-10-11T12:00+01:00[Europe/Stockholm]");
}

#[test]
fn datetime_tz_name_roundtrip() {
    // Test direct parsing roundtrip
    let parsed = crate::temporal::parse_datetime_string(
        "1984-11-11T12:31:14.645876123+01:00[Europe/Stockholm]",
    )
    .unwrap();
    eprintln!("Parsed: {:?}", parsed);
    if let types::CypherValue::DateTime(dt) = &parsed {
        assert_eq!(
            dt.tz_name.as_deref(),
            Some("Europe/Stockholm"),
            "tz_name after parsing"
        );
    } else {
        panic!("expected DateTime");
    }

    // Test storage roundtrip
    let mut g = Graph::new();
    execute(
        &mut g,
        "CREATE (:Val {date: datetime({year: 1984, month: 11, day: 11, hour: 12, minute: 31, second: 14, nanosecond: 645876123, timezone: 'Europe/Stockholm'})})",
    ).unwrap();
    // Without WITH: works because v.date.timezone is a chained property access
    let r1 = execute(&mut g, "MATCH (v:Val) RETURN v.date.timezone").unwrap();
    eprintln!("v.date.timezone = {:?}", r1.rows[0][0]);

    // With WITH: this is where tz_name gets lost
    let r2 = execute(&mut g, "MATCH (v:Val) WITH v.date AS d RETURN d").unwrap();
    eprintln!("d after WITH = {:?}", r2.rows[0][0]);
    let result = execute(&mut g, "MATCH (v:Val) WITH v.date AS d RETURN d.timezone").unwrap();
    let val = &result.rows[0][0];
    assert_eq!(
        val,
        &crate::types::CypherValue::String("Europe/Stockholm".to_string())
    );
}

// -- Index DDL and IndexScan ---------------------------------
#[test]
fn create_and_drop_index() {
    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );

    execute(
        &mut g,
        "CREATE INDEX idx_person_name FOR (n:Person) ON (n.name)",
    )
    .unwrap();
    assert!(g.index_catalog().has_index_by_name("idx_person_name"));

    execute(&mut g, "DROP INDEX idx_person_name").unwrap();
    assert!(!g.index_catalog().has_index_by_name("idx_person_name"));
}

#[test]
fn index_scan_returns_correct_results() {
    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Bob".into()))]),
    );
    g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Charlie".into()))]),
    );

    // Without index: full scan
    let r1 = execute(&mut g, "MATCH (n:Person {name: 'Bob'}) RETURN n.name").unwrap();
    assert_eq!(r1.rows.len(), 1);
    assert_eq!(r1.rows[0][0], CypherValue::String("Bob".into()));

    // Create index
    execute(&mut g, "CREATE INDEX idx_name FOR (n:Person) ON (n.name)").unwrap();

    // With index: should use IndexScan and get same result
    let r2 = execute(&mut g, "MATCH (n:Person {name: 'Bob'}) RETURN n.name").unwrap();
    assert_eq!(r2.rows.len(), 1);
    assert_eq!(r2.rows[0][0], CypherValue::String("Bob".into()));
}

#[test]
fn index_scan_with_multiple_properties() {
    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[
            ("name", PropertyValue::String("Alice".into())),
            ("age", PropertyValue::Int(30)),
        ]),
    );
    g.create_node(
        &["Person"],
        props(&[
            ("name", PropertyValue::String("Alice".into())),
            ("age", PropertyValue::Int(25)),
        ]),
    );

    execute(&mut g, "CREATE INDEX idx_name FOR (n:Person) ON (n.name)").unwrap();

    // Index on name, additional filter on age
    let r = execute(
        &mut g,
        "MATCH (n:Person {name: 'Alice', age: 30}) RETURN n.age",
    )
    .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], CypherValue::Integer(30));
}

#[test]
fn index_maintained_on_create_delete() {
    let mut g = Graph::new();
    execute(&mut g, "CREATE INDEX idx_name FOR (n:Person) ON (n.name)").unwrap();

    // Create via Cypher (index should auto-maintain)
    execute(&mut g, "CREATE (n:Person {name: 'Alice'})").unwrap();
    let r1 = execute(&mut g, "MATCH (n:Person {name: 'Alice'}) RETURN n.name").unwrap();
    assert_eq!(r1.rows.len(), 1);

    // Delete the node
    execute(&mut g, "MATCH (n:Person {name: 'Alice'}) DETACH DELETE n").unwrap();
    let r2 = execute(&mut g, "MATCH (n:Person {name: 'Alice'}) RETURN n.name").unwrap();
    assert_eq!(r2.rows.len(), 0);
}

// -- Cypher through MVCC WriteTx ------------------------------
#[test]
fn cypher_through_write_tx() {
    use traverse_core::tx::MvccStore;
    use traverse_core::tx::view::GraphView;

    let store = MvccStore::new(Graph::new());

    // Execute Cypher through a WriteTx
    let mut tx = store.begin_write();
    execute(&mut tx, "CREATE (n:Person {name: 'Alice'})").unwrap();

    // Read-your-own-writes within the transaction
    let r = execute(&mut tx, "MATCH (n:Person) RETURN n.name").unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0], CypherValue::String("Alice".into()));

    tx.commit().unwrap();

    // Verify committed state is visible to a new read
    let read_tx = store.begin_read().unwrap();
    assert_eq!(GraphView::node_count(&read_tx), 1);
}

#[test]
fn cypher_tx_abort_discards() {
    use traverse_core::tx::MvccStore;
    use traverse_core::tx::view::GraphView;

    let store = MvccStore::new(Graph::new());

    let mut tx = store.begin_write();
    execute(&mut tx, "CREATE (n:Person {name: 'Alice'})").unwrap();
    tx.abort();

    // Nothing committed
    let read_tx = store.begin_read().unwrap();
    assert_eq!(GraphView::node_count(&read_tx), 0);
}

#[test]
fn cypher_snapshot_isolation() {
    use traverse_core::tx::MvccStore;
    use traverse_core::tx::view::GraphView;

    let mut g = Graph::new();
    g.create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );
    let store = MvccStore::new(g);

    // Start read before write
    let read_tx = store.begin_read().unwrap();

    // Write a new node and commit
    let mut write_tx = store.begin_write();
    execute(&mut write_tx, "CREATE (n:Person {name: 'Bob'})").unwrap();
    write_tx.commit().unwrap();

    // Read transaction should still see only Alice
    assert_eq!(GraphView::node_count(&read_tx), 1);

    drop(read_tx);

    // New read should see both
    let read_tx2 = store.begin_read().unwrap();
    assert_eq!(GraphView::node_count(&read_tx2), 2);
}

// -- shortestPath / allShortestPaths ---------------------------

#[test]
fn shortest_path_basic() {
    // A->B->C->D and A->D direct: shortestPath should return the 1-hop path
    let mut g = Graph::new();
    let a = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );
    let b = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("B".into()))]),
    );
    let c = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("C".into()))]),
    );
    let d = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("D".into()))]),
    );
    g.create_edge(a, b, "NEXT", empty_props()).unwrap();
    g.create_edge(b, c, "NEXT", empty_props()).unwrap();
    g.create_edge(c, d, "NEXT", empty_props()).unwrap();
    g.create_edge(a, d, "NEXT", empty_props()).unwrap(); // direct

    let result = execute(
        &mut g,
        "MATCH (a:N {name: 'A'}), (d:N {name: 'D'}), p = shortestPath((a)-[:NEXT*]->(d)) RETURN length(p)",
    ).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::Integer(1)); // direct A->D
}

#[test]
fn shortest_path_no_path() {
    // Disconnected nodes: shortestPath returns no rows
    let mut g = Graph::new();
    g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );
    g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("B".into()))]),
    );

    let result = execute(
        &mut g,
        "MATCH (a:N {name: 'A'}), (b:N {name: 'B'}), p = shortestPath((a)-[*]->(b)) RETURN p",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 0);
}

#[test]
fn all_shortest_paths_diamond() {
    // Diamond: A->B->D, A->C->D. allShortestPaths returns 2 paths of length 2.
    let mut g = Graph::new();
    let a = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );
    let b = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("B".into()))]),
    );
    let c = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("C".into()))]),
    );
    let d = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("D".into()))]),
    );
    g.create_edge(a, b, "R", empty_props()).unwrap();
    g.create_edge(a, c, "R", empty_props()).unwrap();
    g.create_edge(b, d, "R", empty_props()).unwrap();
    g.create_edge(c, d, "R", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH (a:N {name: 'A'}), (d:N {name: 'D'}), p = allShortestPaths((a)-[*]->(d)) RETURN length(p)",
    ).unwrap();
    assert_eq!(result.rows.len(), 2);
    // Both paths should have length 2
    for row in &result.rows {
        assert_eq!(row[0], CypherValue::Integer(2));
    }
}

#[test]
fn shortest_path_edge_type_filter() {
    // A-[:KNOWS]->B-[:KNOWS]->C and A-[:LIKES]->C
    // shortestPath with [:KNOWS*] should skip the LIKES edge
    let mut g = Graph::new();
    let a = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );
    let b = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("B".into()))]),
    );
    let c = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("C".into()))]),
    );
    g.create_edge(a, b, "KNOWS", empty_props()).unwrap();
    g.create_edge(b, c, "KNOWS", empty_props()).unwrap();
    g.create_edge(a, c, "LIKES", empty_props()).unwrap(); // would be shorter

    let result = execute(
        &mut g,
        "MATCH (a:N {name: 'A'}), (c:N {name: 'C'}), p = shortestPath((a)-[:KNOWS*]->(c)) RETURN length(p)",
    ).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::Integer(2)); // A->B->C via KNOWS
}

#[test]
fn shortest_path_incoming() {
    // A->B->C: shortestPath((c)<-[*]-(a)) should find path C<-B<-A
    let mut g = Graph::new();
    let a = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );
    let b = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("B".into()))]),
    );
    let c = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("C".into()))]),
    );
    g.create_edge(a, b, "R", empty_props()).unwrap();
    g.create_edge(b, c, "R", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH (c:N {name: 'C'}), (a:N {name: 'A'}), p = shortestPath((c)<-[*]-(a)) RETURN length(p)",
    ).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::Integer(2));
}

#[test]
fn shortest_path_max_depth() {
    // A->B->C: shortestPath with *..1 should NOT find path of length 2
    let mut g = Graph::new();
    let a = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );
    let b = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("B".into()))]),
    );
    let c = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("C".into()))]),
    );
    g.create_edge(a, b, "R", empty_props()).unwrap();
    g.create_edge(b, c, "R", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH (a:N {name: 'A'}), (c:N {name: 'C'}), p = shortestPath((a)-[*..1]->(c)) RETURN p",
    )
    .unwrap();
    assert_eq!(result.rows.len(), 0); // No path of length <= 1
}

#[test]
fn shortest_path_variable_binding() {
    // Verify path variable works with nodes(p) and length(p)
    let mut g = Graph::new();
    let a = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );
    let b = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("B".into()))]),
    );
    g.create_edge(a, b, "R", empty_props()).unwrap();

    let result = execute(
        &mut g,
        "MATCH (a:N {name: 'A'}), (b:N {name: 'B'}), p = shortestPath((a)-[*]->(b)) RETURN nodes(p), length(p)",
    ).unwrap();
    assert_eq!(result.rows.len(), 1);
    // nodes(p) should be a list of 2 nodes
    if let CypherValue::List(ref nodes) = result.rows[0][0] {
        assert_eq!(nodes.len(), 2);
    } else {
        panic!("Expected list for nodes(p)");
    }
    assert_eq!(result.rows[0][1], CypherValue::Integer(1));
}

// -- wShortest (weighted shortest path / Dijkstra) ------------

#[test]
fn wshortest_basic() {
    // A->B(1)->C(1) vs A->C(10): should find A->B->C with cost 2.0
    let mut g = Graph::new();
    let a = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );
    let b = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("B".into()))]),
    );
    let c = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("C".into()))]),
    );
    g.create_edge(a, b, "ROAD", props(&[("cost", PropertyValue::Float(1.0))]))
        .unwrap();
    g.create_edge(b, c, "ROAD", props(&[("cost", PropertyValue::Float(1.0))]))
        .unwrap();
    g.create_edge(a, c, "ROAD", props(&[("cost", PropertyValue::Float(10.0))]))
        .unwrap();

    let result = execute(
        &mut g,
        "MATCH (a:N {name: 'A'}), (c:N {name: 'C'}), p = (a)-[r:ROAD *wShortest (r, n | r.cost) total_cost]->(c) RETURN length(p), total_cost",
    ).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::Integer(2)); // 2 hops
    assert_eq!(result.rows[0][1], CypherValue::Float(2.0)); // cost 2.0
}

#[test]
fn wshortest_no_path() {
    // Disconnected nodes: no path
    let mut g = Graph::new();
    g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );
    g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("B".into()))]),
    );

    let result = execute(
        &mut g,
        "MATCH (a:N {name: 'A'}), (b:N {name: 'B'}), p = (a)-[r *wShortest (r, n | r.cost) tc]->(b) RETURN p, tc",
    ).unwrap();
    assert_eq!(result.rows.len(), 0);
}

#[test]
fn wshortest_same_node() {
    // start == target: cost 0.0, empty path
    let mut g = Graph::new();
    g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );

    let result = execute(
        &mut g,
        "MATCH (a:N {name: 'A'}), (b:N {name: 'A'}), p = (a)-[r *wShortest (r, n | r.cost) tc]->(b) RETURN length(p), tc",
    ).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::Integer(0));
    assert_eq!(result.rows[0][1], CypherValue::Float(0.0));
}

#[test]
fn wshortest_integer_weights() {
    // Integer weights should work and cost returned as Float
    let mut g = Graph::new();
    let a = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );
    let b = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("B".into()))]),
    );
    g.create_edge(a, b, "ROAD", props(&[("cost", PropertyValue::Int(5))]))
        .unwrap();

    let result = execute(
        &mut g,
        "MATCH (a:N {name: 'A'}), (b:N {name: 'B'}), p = (a)-[r *wShortest (r, n | r.cost) tc]->(b) RETURN tc",
    ).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::Float(5.0));
}

#[test]
fn wshortest_edge_type_filter() {
    // Only traverse ROAD edges, not FOOTPATH
    let mut g = Graph::new();
    let a = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );
    let b = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("B".into()))]),
    );
    let c = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("C".into()))]),
    );
    g.create_edge(a, b, "ROAD", props(&[("cost", PropertyValue::Float(1.0))]))
        .unwrap();
    g.create_edge(
        b,
        c,
        "FOOTPATH",
        props(&[("cost", PropertyValue::Float(1.0))]),
    )
    .unwrap();
    g.create_edge(
        a,
        c,
        "ROAD",
        props(&[("cost", PropertyValue::Float(100.0))]),
    )
    .unwrap();

    let result = execute(
        &mut g,
        "MATCH (a:N {name: 'A'}), (c:N {name: 'C'}), p = (a)-[r:ROAD *wShortest (r, n | r.cost) tc]->(c) RETURN length(p), tc",
    ).unwrap();
    // Only ROAD edges: A->C direct with cost 100
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::Integer(1));
    assert_eq!(result.rows[0][1], CypherValue::Float(100.0));
}

#[test]
fn wshortest_missing_weight_skipped() {
    // Edges without weight property are skipped
    let mut g = Graph::new();
    let a = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );
    let b = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("B".into()))]),
    );
    let c = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("C".into()))]),
    );
    // A->B has no cost property, B->C has cost, A->C has cost
    g.create_edge(a, b, "ROAD", empty_props()).unwrap();
    g.create_edge(b, c, "ROAD", props(&[("cost", PropertyValue::Float(1.0))]))
        .unwrap();
    g.create_edge(a, c, "ROAD", props(&[("cost", PropertyValue::Float(5.0))]))
        .unwrap();

    let result = execute(
        &mut g,
        "MATCH (a:N {name: 'A'}), (c:N {name: 'C'}), p = (a)-[r:ROAD *wShortest (r, n | r.cost) tc]->(c) RETURN length(p), tc",
    ).unwrap();
    // A->B is skipped (no cost), so must take A->C direct
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::Integer(1));
    assert_eq!(result.rows[0][1], CypherValue::Float(5.0));
}

#[test]
fn wshortest_without_cost_var() {
    // Syntax without cost variable still works
    let mut g = Graph::new();
    let a = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );
    let b = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("B".into()))]),
    );
    g.create_edge(a, b, "ROAD", props(&[("dist", PropertyValue::Float(3.0))]))
        .unwrap();

    let result = execute(
        &mut g,
        "MATCH (a:N {name: 'A'}), (b:N {name: 'B'}), p = (a)-[r *wShortest (r, n | r.dist)]->(b) RETURN length(p)",
    ).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::Integer(1));
}

#[test]
fn wshortest_path_variable() {
    // Path variable `p` is bound correctly
    let mut g = Graph::new();
    let a = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );
    let b = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("B".into()))]),
    );
    g.create_edge(a, b, "ROAD", props(&[("cost", PropertyValue::Float(1.0))]))
        .unwrap();

    let result = execute(
        &mut g,
        "MATCH (a:N {name: 'A'}), (b:N {name: 'B'}), p = (a)-[r *wShortest (r, n | r.cost) tc]->(b) RETURN nodes(p), length(p)",
    ).unwrap();
    assert_eq!(result.rows.len(), 1);
    if let CypherValue::List(ref nodes) = result.rows[0][0] {
        assert_eq!(nodes.len(), 2);
    } else {
        panic!("Expected list for nodes(p)");
    }
    assert_eq!(result.rows[0][1], CypherValue::Integer(1));
}

#[test]
fn wshortest_diamond() {
    // Diamond: A->B(1), A->C(2), B->D(1), C->D(1)
    // Minimum cost A->D: A->B->D = 2.0 (not A->C->D = 3.0)
    let mut g = Graph::new();
    let a = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("A".into()))]),
    );
    let b = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("B".into()))]),
    );
    let c = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("C".into()))]),
    );
    let d = g.create_node(
        &["N"],
        props(&[("name", PropertyValue::String("D".into()))]),
    );
    g.create_edge(a, b, "ROAD", props(&[("cost", PropertyValue::Float(1.0))]))
        .unwrap();
    g.create_edge(a, c, "ROAD", props(&[("cost", PropertyValue::Float(2.0))]))
        .unwrap();
    g.create_edge(b, d, "ROAD", props(&[("cost", PropertyValue::Float(1.0))]))
        .unwrap();
    g.create_edge(c, d, "ROAD", props(&[("cost", PropertyValue::Float(1.0))]))
        .unwrap();

    let result = execute(
        &mut g,
        "MATCH (a:N {name: 'A'}), (d:N {name: 'D'}), p = (a)-[r:ROAD *wShortest (r, n | r.cost) tc]->(d) RETURN length(p), tc",
    ).unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], CypherValue::Integer(2)); // 2 hops A->B->D
    assert_eq!(result.rows[0][1], CypherValue::Float(2.0));
}
