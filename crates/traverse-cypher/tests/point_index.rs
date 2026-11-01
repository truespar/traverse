//! Native POINT (spatial R-tree) index end to end: `CREATE POINT INDEX ...` plus transparent
//! acceleration of `point.distance` / `point.withinBBox` / `ORDER BY point.distance ... LIMIT k`. The
//! bar: the *indexed* path returns exactly what the *scan* path does, so each query runs against
//! two identical graphs, one indexed, and the results are compared. EXPLAIN separately proves the
//! index is used.

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

fn run(graph: &mut Graph, q: &str) -> Vec<Vec<CypherValue>> {
    let params: HashMap<String, CypherValue> = HashMap::new();
    execute_with_params(graph, q, &params)
        .unwrap_or_else(|e| panic!("query failed: {q}\n{e}"))
        .rows
}

/// Sorted first-column strings (order-insensitive comparison of the two paths).
fn names(rows: &[Vec<CypherValue>]) -> Vec<String> {
    let mut v: Vec<String> = rows
        .iter()
        .map(|r| match &r[0] {
            CypherValue::String(s) => s.clone(),
            o => panic!("expected string, got {o:?}"),
        })
        .collect();
    v.sort();
    v
}

/// Grid of `Place` nodes named "x_y" at cartesian point (x, y).
fn grid(indexed: bool) -> Graph {
    let mut g = Graph::new();
    if indexed {
        run(&mut g, "CREATE POINT INDEX places FOR (p:Place) ON (p.loc)");
    }
    for x in 0..10 {
        for y in 0..10 {
            let q = format!("CREATE (:Place {{name: '{x}_{y}', loc: point({{x: {x}, y: {y}}})}})");
            run(&mut g, &q);
        }
    }
    g
}

/// Does the EXPLAIN tree contain an operator of this type?
fn plan_has_operator(graph: &mut Graph, query: &str, op: &str) -> bool {
    let params: HashMap<String, CypherValue> = HashMap::new();
    let res = execute_with_params(graph, &format!("EXPLAIN {query}"), &params).expect("explain");
    fn walk(v: &CypherValue, op: &str) -> bool {
        if let CypherValue::Map(m) = v {
            if let Some(CypherValue::String(t)) = m.get("operatorType")
                && t == op
            {
                return true;
            }
            if let Some(CypherValue::List(children)) = m.get("children") {
                return children.iter().any(|c| walk(c, op));
            }
        }
        false
    }
    walk(res.plan.as_ref().expect("plan present"), op)
}

#[test]
fn distance_filter_matches_scan_path() {
    let q = "MATCH (p:Place) WHERE point.distance(p.loc, point({x: 5, y: 5})) < 2.5 \
             RETURN p.name AS n";
    let mut with = grid(true);
    let mut without = grid(false);
    let a = names(&run(&mut with, q));
    let b = names(&run(&mut without, q));
    assert_eq!(a, b, "indexed and scan paths must agree");
    assert!(!a.is_empty(), "some places are within 2.5 of (5,5)");
    // (5,5) itself is in range, (0,0) isn't
    assert!(a.contains(&"5_5".to_string()));
    assert!(!a.contains(&"0_0".to_string()));
}

#[test]
fn distance_filter_uses_the_index() {
    let mut g = grid(true);
    let q = "MATCH (p:Place) WHERE point.distance(p.loc, point({x: 5, y: 5})) < 2.5 \
             RETURN p.name AS n";
    assert!(plan_has_operator(&mut g, q, "NodePointIndexScan"));
    // without the index the same query must fall back to a plain scan
    let mut plain = grid(false);
    assert!(!plan_has_operator(&mut plain, q, "NodePointIndexScan"));
}

#[test]
fn distance_filter_commuted_and_inclusive() {
    // commuted (`r > distance`) and inclusive (`<=`) forms both accelerate
    for q in [
        "MATCH (p:Place) WHERE 2.5 > point.distance(p.loc, point({x: 5, y: 5})) RETURN p.name AS n",
        "MATCH (p:Place) WHERE point.distance(p.loc, point({x: 5, y: 5})) <= 2.0 RETURN p.name AS n",
    ] {
        let mut with = grid(true);
        let mut without = grid(false);
        assert_eq!(
            names(&run(&mut with, q)),
            names(&run(&mut without, q)),
            "{q}"
        );
        assert!(
            plan_has_operator(&mut grid(true), q, "NodePointIndexScan"),
            "{q}"
        );
    }
}

#[test]
fn within_bbox_matches_scan_path() {
    let q = "MATCH (p:Place) \
             WHERE point.withinBBox(p.loc, point({x: 2, y: 2}), point({x: 4, y: 4})) \
             RETURN p.name AS n";
    let mut with = grid(true);
    let mut without = grid(false);
    let a = names(&run(&mut with, q));
    let b = names(&run(&mut without, q));
    assert_eq!(a, b);
    // 3x3 block from (2,2) to (4,4)
    assert_eq!(a.len(), 9);
    assert!(a.contains(&"3_3".to_string()));
    assert!(plan_has_operator(&mut grid(true), q, "NodePointIndexScan"));
}

#[test]
fn knn_order_by_distance_limit() {
    // an off-grid query point makes every distance distinct, so top-k is unambiguous (no ties at
    // the LIMIT boundary)
    let q = "MATCH (p:Place) RETURN p.name AS n \
             ORDER BY point.distance(p.loc, point({x: 0.3, y: 0.1})) LIMIT 3";
    let mut with = grid(true);
    let mut without = grid(false);
    // kNN keeps order: compare ordered results directly
    let a: Vec<Vec<CypherValue>> = run(&mut with, q);
    let b: Vec<Vec<CypherValue>> = run(&mut without, q);
    assert_eq!(a, b, "indexed kNN must match the sort+limit path");
    assert_eq!(a.len(), 3);
    // nearest three to (0.3, 0.1): (0,0), (1,0), (0,1), in that order
    assert_eq!(a[0][0], CypherValue::String("0_0".to_string()));
    assert_eq!(a[1][0], CypherValue::String("1_0".to_string()));
    assert_eq!(a[2][0], CypherValue::String("0_1".to_string()));
    assert!(plan_has_operator(&mut grid(true), q, "NodePointIndexScan"));
}

#[test]
fn index_maintained_on_writes() {
    // a node created AFTER the index must be found, a moved node must leave its old neighborhood
    let mut g = grid(true);
    run(
        &mut g,
        "CREATE (:Place {name: 'new', loc: point({x: 5, y: 5})})",
    );
    let q = "MATCH (p:Place) WHERE point.distance(p.loc, point({x: 5, y: 5})) < 0.5 \
             RETURN p.name AS n";
    let found = names(&run(&mut g, q));
    assert!(found.contains(&"new".to_string()));
    assert!(found.contains(&"5_5".to_string()));
    // move '5_5' far away; it must drop out
    run(
        &mut g,
        "MATCH (p:Place {name: '5_5'}) SET p.loc = point({x: 99, y: 99})",
    );
    let after = names(&run(&mut g, q));
    assert!(!after.contains(&"5_5".to_string()));
    assert!(after.contains(&"new".to_string()));
}

#[test]
fn geographic_distance() {
    // WGS-84 (lon/lat) points, haversine distances in meters
    let mut with = Graph::new();
    run(
        &mut with,
        "CREATE POINT INDEX cities FOR (c:City) ON (c.loc)",
    );
    let mut without = Graph::new();
    for g in [&mut with, &mut without] {
        run(
            g,
            "CREATE (:City {name: 'Paris', loc: point({longitude: 2.35, latitude: 48.85})})",
        );
        run(
            g,
            "CREATE (:City {name: 'Versailles', loc: point({longitude: 2.13, latitude: 48.80})})",
        );
        run(
            g,
            "CREATE (:City {name: 'Berlin', loc: point({longitude: 13.4, latitude: 52.52})})",
        );
    }
    // within 50 km of Paris: Paris + Versailles, not Berlin
    let q = "MATCH (c:City) \
             WHERE point.distance(c.loc, point({longitude: 2.35, latitude: 48.85})) < 50000 \
             RETURN c.name AS n";
    let a = names(&run(&mut with, q));
    let b = names(&run(&mut without, q));
    assert_eq!(a, b);
    assert_eq!(a, vec!["Paris".to_string(), "Versailles".to_string()]);
    assert!(plan_has_operator(&mut with, q, "NodePointIndexScan"));
}

#[test]
fn show_indexes_reports_point_type() {
    let mut g = grid(true);
    let rows = run(
        &mut g,
        "SHOW INDEXES YIELD name, type, labelsOrTypes, properties \
         WHERE type = 'POINT' RETURN name, labelsOrTypes, properties",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("places".to_string()));
}

#[test]
fn drop_index_falls_back_to_scan() {
    let mut g = grid(true);
    run(&mut g, "DROP INDEX places");
    let q = "MATCH (p:Place) WHERE point.distance(p.loc, point({x: 5, y: 5})) < 2.5 \
             RETURN p.name AS n";
    // still correct, just not accelerated
    assert!(!plan_has_operator(&mut g, q, "NodePointIndexScan"));
    let rows = names(&run(&mut g, q));
    assert!(rows.contains(&"5_5".to_string()));
}

#[test]
fn relationship_point_index_form_still_edge_index() {
    // `CREATE POINT INDEX ... FOR ()-[r:T]-() ON (r.p)` routes to the edge b-tree index (the R-tree
    // is node-only)
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE POINT INDEX rel_pt FOR ()-[r:STREET]-() ON (r.intersection)",
    );
    let rows = run(
        &mut g,
        "SHOW INDEXES YIELD name, entityType WHERE name = 'rel_pt' RETURN entityType",
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], CypherValue::String("RELATIONSHIP".to_string()));
}
