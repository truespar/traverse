//! Neo4j-style dynamic writes, end to end against an in-process graph:
//!
//!   CREATE (n:$($label))
//!   CREATE (a)-[:$($type)]->(b)
//!   SET n:$($label)
//!   REMOVE n:$($label)
//!   SET n[$key] = value

use std::collections::HashMap;

use traverse_core::graph::Graph;
use traverse_cypher::execute_with_params;
use traverse_cypher::types::CypherValue;

type Row = HashMap<String, CypherValue>;

fn run(graph: &mut Graph, q: &str, params: &[(&str, CypherValue)]) -> Vec<Row> {
    let map: HashMap<String, CypherValue> = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    let res = execute_with_params(graph, q, &map).expect("query should succeed");
    res.rows
        .into_iter()
        .map(|row| {
            res.columns
                .iter()
                .zip(row)
                .map(|(c, v)| (c.clone(), v))
                .collect()
        })
        .collect()
}

#[test]
fn create_node_with_dynamic_label() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE (n:$($lbl) {id: 1})",
        &[("lbl", CypherValue::String("DynA".into()))],
    );
    let r = run(&mut g, "MATCH (n:DynA) RETURN n.id AS id", &[]);
    assert_eq!(r.len(), 1, "node created with dynamic label should match");
    assert_eq!(r[0].get("id").unwrap(), &CypherValue::Integer(1));
}

#[test]
fn create_node_combines_static_and_dynamic_labels() {
    let mut g = Graph::new();
    run(
        &mut g,
        "CREATE (n:Static:$($lbl) {id: 2})",
        &[("lbl", CypherValue::String("Dyn".into()))],
    );
    let r = run(&mut g, "MATCH (n:Static:Dyn) RETURN n.id AS id", &[]);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].get("id").unwrap(), &CypherValue::Integer(2));
}

#[test]
fn set_dynamic_label_adds_to_existing_node() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (n:Person {id: 1})", &[]);
    run(
        &mut g,
        "MATCH (n:Person {id: 1}) SET n:$($extra)",
        &[("extra", CypherValue::String("VIP".into()))],
    );
    let r = run(&mut g, "MATCH (n:Person:VIP) RETURN n.id AS id", &[]);
    assert_eq!(r.len(), 1);
}

#[test]
fn remove_dynamic_label_strips_from_existing_node() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (n:Person:VIP {id: 1})", &[]);
    run(
        &mut g,
        "MATCH (n:Person {id: 1}) REMOVE n:$($extra)",
        &[("extra", CypherValue::String("VIP".into()))],
    );
    let still_vip = run(&mut g, "MATCH (n:VIP) RETURN n.id AS id", &[]);
    assert!(still_vip.is_empty(), "VIP label should have been removed");
    let person = run(&mut g, "MATCH (n:Person) RETURN n.id AS id", &[]);
    assert_eq!(person.len(), 1);
}

#[test]
fn set_dynamic_property_assigns_value() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (n:Person {id: 1})", &[]);
    run(
        &mut g,
        "MATCH (n:Person {id: 1}) SET n[$key] = $val",
        &[
            ("key", CypherValue::String("age".into())),
            ("val", CypherValue::Integer(42)),
        ],
    );
    let r = run(&mut g, "MATCH (n:Person {id: 1}) RETURN n.age AS age", &[]);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].get("age").unwrap(), &CypherValue::Integer(42));
}

#[test]
fn create_relationship_with_dynamic_type() {
    let mut g = Graph::new();
    run(&mut g, "CREATE (a:Node {id: 1}), (b:Node {id: 2})", &[]);
    run(
        &mut g,
        "MATCH (a:Node {id: 1}), (b:Node {id: 2}) CREATE (a)-[r:$($t) {w: 9}]->(b)",
        &[("t", CypherValue::String("DynLink".into()))],
    );
    let r = run(
        &mut g,
        "MATCH (a:Node)-[r:DynLink]->(b:Node) RETURN a.id AS a, b.id AS b, r.w AS w",
        &[],
    );
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].get("a").unwrap(), &CypherValue::Integer(1));
    assert_eq!(r[0].get("b").unwrap(), &CypherValue::Integer(2));
    assert_eq!(r[0].get("w").unwrap(), &CypherValue::Integer(9));
}

#[test]
fn load_csv_file_and_linenumber_helpers() {
    // `file()` / `linenumber()`: the resolved file path and 1-based row number during LOAD CSV,
    // NULL otherwise; the context is published in `LoadCsvOp::next` just before each frame
    use std::io::Write;
    let dir = std::env::temp_dir();
    let path = dir.join("p1f_csv.csv");
    let mut f = std::fs::File::create(&path).expect("create tempfile");
    writeln!(f, "name").unwrap();
    writeln!(f, "Alice").unwrap();
    writeln!(f, "Bob").unwrap();
    writeln!(f, "Carol").unwrap();
    drop(f);
    // `file://` + absolute path: `file:///tmp/x` on Unix, `file://C:/x` on Windows (forward
    // slashes, no escapes in the Cypher string)
    let url = format!("file://{}", path.display().to_string().replace('\\', "/"));

    let mut g = Graph::new();
    let q = format!(
        "LOAD CSV WITH HEADERS FROM '{url}' AS line \
         RETURN line.name AS name, file() AS source, linenumber() AS line_num"
    );
    let rows = run(&mut g, &q, &[]);
    assert_eq!(rows.len(), 3);

    // names in CSV order; line_num 1-based; file() the same path for every row
    let names: Vec<_> = rows
        .iter()
        .map(|r| r.get("name").unwrap().clone())
        .collect();
    assert_eq!(
        names,
        vec![
            CypherValue::String("Alice".into()),
            CypherValue::String("Bob".into()),
            CypherValue::String("Carol".into()),
        ]
    );
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(
            row.get("line_num").unwrap(),
            &CypherValue::Integer((i + 1) as i64),
            "row {i} line_num"
        );
        // file() is a String path
        match row.get("source").unwrap() {
            CypherValue::String(s) => assert!(s.ends_with("p1f_csv.csv"), "got {s:?}"),
            other => panic!("expected String, got {other:?}"),
        }
    }

    // once the LOAD CSV scope ends, file() / linenumber() are NULL again
    let after = run(&mut g, "RETURN file() AS f, linenumber() AS n", &[]);
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].get("f").unwrap(), &CypherValue::Null);
    assert_eq!(after[0].get("n").unwrap(), &CypherValue::Null);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn load_csv_create_with_dynamic_label_from_row() {
    // LOAD CSV with dynamic labels: the CSV row is just another in-scope variable for the label
    // expression; each created node must carry the right label
    use std::io::Write;
    let dir = std::env::temp_dir();
    let path = dir.join("p05d_dynamic_csv.csv");
    let mut f = std::fs::File::create(&path).expect("create tempfile");
    writeln!(f, "label,name").unwrap();
    writeln!(f, "Person,Alice").unwrap();
    writeln!(f, "Animal,Bessie").unwrap();
    drop(f);
    // `file://` + absolute path: `file:///tmp/x` on Unix, `file://C:/x` on Windows (forward
    // slashes, no escapes in the Cypher string)
    let url = format!("file://{}", path.display().to_string().replace('\\', "/"));

    let mut g = Graph::new();
    let q = format!(
        "LOAD CSV WITH HEADERS FROM '{url}' AS line CREATE (n:Loaded:$(line.label) {{name: line.name}})"
    );
    run(&mut g, &q, &[]);

    let person = run(&mut g, "MATCH (n:Loaded:Person) RETURN n.name AS name", &[]);
    assert_eq!(person.len(), 1);
    assert_eq!(
        person[0].get("name").unwrap(),
        &CypherValue::String("Alice".into())
    );
    let animal = run(&mut g, "MATCH (n:Loaded:Animal) RETURN n.name AS name", &[]);
    assert_eq!(animal.len(), 1);
    assert_eq!(
        animal[0].get("name").unwrap(),
        &CypherValue::String("Bessie".into())
    );

    // remove the tempfile so later runs aren't stale
    let _ = std::fs::remove_file(&path);
}
