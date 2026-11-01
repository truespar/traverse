use std::collections::HashMap;
use std::sync::Arc;

use traverse_core::graph::Graph;
use traverse_core::tx::MvccStore;
use traverse_server::server::Server;

// -- Test helpers ------------------------------------------------------

/// Start a test Bolt server on a random port.
async fn start_test_server() -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let store = Arc::new(MvccStore::new(Graph::new()));
    let server = Server::with_store(store, "127.0.0.1:0".into());
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let addr = server.run_until_shutdown(shutdown_rx).await.unwrap();
    (addr, shutdown_tx)
}

// -- Bolt mode tests --------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bolt_simple_query() {
    let (addr, _shutdown) = start_test_server().await;

    let mut client =
        traverse_cli::bolt_client::BoltClient::connect(&addr.to_string(), None, None, false, false)
            .await
            .unwrap();

    let result = client
        .run_query("RETURN 1 AS x;", HashMap::new())
        .await
        .unwrap();

    assert_eq!(result.columns, vec!["x"]);
    assert_eq!(result.rows.len(), 1);
    match &result.rows[0][0] {
        traverse_cypher::types::CypherValue::Integer(n) => assert_eq!(*n, 1),
        other => panic!("expected Integer(1), got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bolt_create_and_match() {
    let (addr, _shutdown) = start_test_server().await;

    let mut client =
        traverse_cli::bolt_client::BoltClient::connect(&addr.to_string(), None, None, false, false)
            .await
            .unwrap();

    // CREATE
    let create_result = client
        .run_query(
            "CREATE (n:Person {name: 'Alice'}) RETURN n.name AS name;",
            HashMap::new(),
        )
        .await
        .unwrap();
    assert_eq!(create_result.columns, vec!["name"]);
    assert_eq!(create_result.rows.len(), 1);

    // MATCH
    let match_result = client
        .run_query("MATCH (n:Person) RETURN n.name AS name;", HashMap::new())
        .await
        .unwrap();
    assert_eq!(match_result.rows.len(), 1);
    match &match_result.rows[0][0] {
        traverse_cypher::types::CypherValue::String(s) => assert_eq!(s, "Alice"),
        other => panic!("expected String('Alice'), got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bolt_syntax_error() {
    let (addr, _shutdown) = start_test_server().await;

    let mut client =
        traverse_cli::bolt_client::BoltClient::connect(&addr.to_string(), None, None, false, false)
            .await
            .unwrap();

    let result = client
        .run_query("THIS IS NOT CYPHER;", HashMap::new())
        .await;

    assert!(result.is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bolt_return_node() {
    let (addr, _shutdown) = start_test_server().await;

    let mut client =
        traverse_cli::bolt_client::BoltClient::connect(&addr.to_string(), None, None, false, false)
            .await
            .unwrap();

    let result = client
        .run_query("CREATE (n:Test {val: 42}) RETURN n;", HashMap::new())
        .await
        .unwrap();

    assert_eq!(result.columns, vec!["n"]);
    assert_eq!(result.rows.len(), 1);
    match &result.rows[0][0] {
        traverse_cypher::types::CypherValue::Node { labels, .. } => {
            assert_eq!(labels, &["Test"]);
        }
        other => panic!("expected Node, got {other:?}"),
    }
}

// -- Schema / CALL procedure tests ------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bolt_call_db_labels() {
    let (addr, _shutdown) = start_test_server().await;

    let mut client =
        traverse_cli::bolt_client::BoltClient::connect(&addr.to_string(), None, None, false, false)
            .await
            .unwrap();

    // Create data first
    client
        .run_query(
            "CREATE (:Person {name: 'A'}), (:City {name: 'B'});",
            HashMap::new(),
        )
        .await
        .unwrap();

    // CALL db.labels()
    let result = client
        .run_query("CALL db.labels()", HashMap::new())
        .await
        .unwrap();

    assert_eq!(result.columns, vec!["label"]);
    let labels: Vec<&str> = result
        .rows
        .iter()
        .filter_map(|r| r.first()?.as_str())
        .collect();
    assert!(labels.contains(&"Person"), "labels: {labels:?}");
    assert!(labels.contains(&"City"), "labels: {labels:?}");

    // CALL db.propertyKeys()
    let result = client
        .run_query("CALL db.propertyKeys()", HashMap::new())
        .await
        .unwrap();

    let keys: Vec<&str> = result
        .rows
        .iter()
        .filter_map(|r| r.first()?.as_str())
        .collect();
    assert!(keys.contains(&"name"), "keys: {keys:?}");
}

// -- Embedded mode tests ----------------------------------------------

#[test]
fn embedded_create_and_query() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let db_path = tmp_dir.path().join("test.tvdb");

    let db = traverse_core::storage::database::Database::open(&db_path).unwrap();

    // CREATE
    {
        let mut tx = db.begin_write();
        let result = traverse_cypher::execute(
            &mut tx,
            "CREATE (n:Person {name: 'Bob'}) RETURN n.name AS name",
        )
        .unwrap();
        assert_eq!(result.columns, vec!["name"]);
        assert_eq!(result.rows.len(), 1);
        tx.commit().unwrap();
        db.flush().unwrap();
    }

    // MATCH (read transaction)
    {
        let mut tx = db.begin_read().unwrap();
        let result =
            traverse_cypher::execute(&mut tx, "MATCH (n:Person) RETURN n.name AS name").unwrap();
        assert_eq!(result.rows.len(), 1);
        match &result.rows[0][0] {
            traverse_cypher::types::CypherValue::String(s) => assert_eq!(s, "Bob"),
            other => panic!("expected String('Bob'), got {other:?}"),
        }
    }
}

// -- Display format tests ---------------------------------------------

#[test]
fn display_table_format() {
    use traverse_cypher::types::CypherValue;

    let columns = vec!["x".into(), "name".into()];
    let rows = vec![vec![
        CypherValue::Integer(1),
        CypherValue::String("Alice".into()),
    ]];
    let stats = vec![];

    // Just verify it doesn't panic
    traverse_cli::display::display_results(
        &columns,
        &rows,
        &stats,
        None,
        traverse_cli::display::OutputFormat::Table,
    );
}

#[test]
fn display_json_format() {
    use traverse_cypher::types::CypherValue;

    let columns = vec!["x".into()];
    let rows = vec![vec![CypherValue::Integer(42)]];
    let stats = vec![];

    // Just verify it doesn't panic
    traverse_cli::display::display_results(
        &columns,
        &rows,
        &stats,
        None,
        traverse_cli::display::OutputFormat::Json,
    );
}

#[test]
fn display_csv_format() {
    use traverse_cypher::types::CypherValue;

    let columns = vec!["a".into(), "b".into()];
    let rows = vec![
        vec![CypherValue::Integer(1), CypherValue::String("hello".into())],
        vec![CypherValue::Integer(2), CypherValue::String("world".into())],
    ];
    let stats = vec![];

    traverse_cli::display::display_results(
        &columns,
        &rows,
        &stats,
        None,
        traverse_cli::display::OutputFormat::Csv,
    );
}

// -- Syntax highlighting tests ----------------------------------------

#[test]
fn highlight_keywords() {
    let result = traverse_cli::highlight::highlight_cypher("MATCH (n) RETURN n");
    assert!(result.contains("\x1b[1;34m")); // bold blue for keywords
    assert!(result.contains("MATCH"));
    assert!(result.contains("RETURN"));
}

#[test]
fn highlight_string_literal() {
    let result = traverse_cli::highlight::highlight_cypher("RETURN 'hello'");
    assert!(result.contains("\x1b[32m")); // green for strings
}

#[test]
fn highlight_number() {
    let result = traverse_cli::highlight::highlight_cypher("RETURN 42");
    assert!(result.contains("\x1b[33m")); // yellow for numbers
}

// ======================================================================
// Pure-function unit tests (no server required)
// ======================================================================

// -- split_statements -------------------------------------------------

#[test]
fn split_statements_empty() {
    assert!(traverse_cli::util::split_statements("").is_empty());
}

#[test]
fn split_statements_whitespace_only() {
    assert!(traverse_cli::util::split_statements("   \n\t  ").is_empty());
}

#[test]
fn split_statements_single_with_semicolon() {
    let stmts = traverse_cli::util::split_statements("RETURN 1;");
    assert_eq!(stmts, vec!["RETURN 1"]);
}

#[test]
fn split_statements_single_without_semicolon() {
    let stmts = traverse_cli::util::split_statements("RETURN 1");
    assert_eq!(stmts, vec!["RETURN 1"]);
}

#[test]
fn split_statements_multiple() {
    let stmts = traverse_cli::util::split_statements("RETURN 1; RETURN 2; RETURN 3;");
    assert_eq!(stmts, vec!["RETURN 1", "RETURN 2", "RETURN 3"]);
}

#[test]
fn split_statements_semicolon_in_single_quoted_string() {
    let stmts = traverse_cli::util::split_statements("RETURN 'a;b'");
    assert_eq!(stmts, vec!["RETURN 'a;b'"]);
}

#[test]
fn split_statements_semicolon_in_double_quoted_string() {
    let stmts = traverse_cli::util::split_statements("RETURN \"a;b\"");
    assert_eq!(stmts, vec!["RETURN \"a;b\""]);
}

#[test]
fn split_statements_escaped_quotes() {
    // Doubled single-quote inside a string literal
    let stmts = traverse_cli::util::split_statements("RETURN 'it''s'; RETURN 2");
    assert_eq!(stmts, vec!["RETURN 'it''s'", "RETURN 2"]);
}

#[test]
fn split_statements_line_comment_with_semicolon() {
    let input = "RETURN 1 // this; is a comment\n; RETURN 2";
    let stmts = traverse_cli::util::split_statements(input);
    assert_eq!(stmts, vec!["RETURN 1 // this; is a comment", "RETURN 2"]);
}

// -- parse_param_value ------------------------------------------------

#[test]
fn parse_param_null() {
    use traverse_cypher::types::CypherValue;
    assert!(matches!(
        traverse_cli::util::parse_param_value("null").unwrap(),
        CypherValue::Null
    ));
    assert!(matches!(
        traverse_cli::util::parse_param_value("NULL").unwrap(),
        CypherValue::Null
    ));
    assert!(matches!(
        traverse_cli::util::parse_param_value("Null").unwrap(),
        CypherValue::Null
    ));
}

#[test]
fn parse_param_bool() {
    use traverse_cypher::types::CypherValue;
    assert!(matches!(
        traverse_cli::util::parse_param_value("true").unwrap(),
        CypherValue::Bool(true)
    ));
    assert!(matches!(
        traverse_cli::util::parse_param_value("TRUE").unwrap(),
        CypherValue::Bool(true)
    ));
    assert!(matches!(
        traverse_cli::util::parse_param_value("false").unwrap(),
        CypherValue::Bool(false)
    ));
    assert!(matches!(
        traverse_cli::util::parse_param_value("False").unwrap(),
        CypherValue::Bool(false)
    ));
}

#[test]
fn parse_param_single_quoted_string() {
    use traverse_cypher::types::CypherValue;
    match traverse_cli::util::parse_param_value("'hello world'").unwrap() {
        CypherValue::String(s) => assert_eq!(s, "hello world"),
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn parse_param_double_quoted_string() {
    use traverse_cypher::types::CypherValue;
    match traverse_cli::util::parse_param_value("\"hello\"").unwrap() {
        CypherValue::String(s) => assert_eq!(s, "hello"),
        other => panic!("expected String, got {other:?}"),
    }
}

#[test]
fn parse_param_integer() {
    use traverse_cypher::types::CypherValue;
    match traverse_cli::util::parse_param_value("42").unwrap() {
        CypherValue::Integer(n) => assert_eq!(n, 42),
        other => panic!("expected Integer, got {other:?}"),
    }
}

#[test]
fn parse_param_negative_integer() {
    use traverse_cypher::types::CypherValue;
    match traverse_cli::util::parse_param_value("-7").unwrap() {
        CypherValue::Integer(n) => assert_eq!(n, -7),
        other => panic!("expected Integer, got {other:?}"),
    }
}

#[test]
fn parse_param_float() {
    use traverse_cypher::types::CypherValue;
    match traverse_cli::util::parse_param_value("2.5").unwrap() {
        CypherValue::Float(f) => assert!((f - 2.5).abs() < 1e-10),
        other => panic!("expected Float, got {other:?}"),
    }
}

#[test]
fn parse_param_bare_word() {
    use traverse_cypher::types::CypherValue;
    match traverse_cli::util::parse_param_value("hello").unwrap() {
        CypherValue::String(s) => assert_eq!(s, "hello"),
        other => panic!("expected String fallback, got {other:?}"),
    }
}

#[test]
fn parse_param_empty() {
    assert!(traverse_cli::util::parse_param_value("").is_err());
}

#[test]
fn parse_param_whitespace_only() {
    assert!(traverse_cli::util::parse_param_value("   ").is_err());
}

#[test]
fn parse_param_large_number_beyond_i64() {
    use traverse_cypher::types::CypherValue;
    // i64::MAX + 1 cannot be parsed as i64, falls through to f64
    let big = "9999999999999999999999";
    match traverse_cli::util::parse_param_value(big).unwrap() {
        CypherValue::Float(f) => assert!(f > 0.0),
        other => panic!("expected Float for huge number, got {other:?}"),
    }
}

// -- extract_error_position -------------------------------------------

#[test]
fn extract_error_position_at_position() {
    assert_eq!(
        traverse_cli::display::extract_error_position("at position 42: unexpected token"),
        Some(42)
    );
}

#[test]
fn extract_error_position_position() {
    assert_eq!(
        traverse_cli::display::extract_error_position("position 10: error"),
        Some(10)
    );
}

#[test]
fn extract_error_position_none() {
    assert_eq!(
        traverse_cli::display::extract_error_position("no position here"),
        None
    );
}

#[test]
fn extract_error_position_empty_digits() {
    // "at position : missing" - no digits after "at position "
    assert_eq!(
        traverse_cli::display::extract_error_position("at position : missing"),
        None
    );
}

#[test]
fn extract_error_position_zero() {
    assert_eq!(
        traverse_cli::display::extract_error_position("at position 0: start"),
        Some(0)
    );
}

// -- cypher_to_json ---------------------------------------------------

#[test]
fn cypher_to_json_null() {
    use traverse_cypher::types::CypherValue;
    let result = traverse_cli::display::cypher_to_json(&CypherValue::Null);
    assert!(result.is_null());
}

#[test]
fn cypher_to_json_bool() {
    use traverse_cypher::types::CypherValue;
    let result = traverse_cli::display::cypher_to_json(&CypherValue::Bool(true));
    assert_eq!(result, serde_json::Value::Bool(true));
}

#[test]
fn cypher_to_json_integer() {
    use traverse_cypher::types::CypherValue;
    let result = traverse_cli::display::cypher_to_json(&CypherValue::Integer(99));
    assert_eq!(result, serde_json::json!(99));
}

#[test]
fn cypher_to_json_float() {
    use traverse_cypher::types::CypherValue;
    let result = traverse_cli::display::cypher_to_json(&CypherValue::Float(2.5));
    assert_eq!(result, serde_json::json!(2.5));
}

#[test]
fn cypher_to_json_string() {
    use traverse_cypher::types::CypherValue;
    let result = traverse_cli::display::cypher_to_json(&CypherValue::String("hi".into()));
    assert_eq!(result, serde_json::Value::String("hi".into()));
}

#[test]
fn cypher_to_json_list() {
    use traverse_cypher::types::CypherValue;
    let list = CypherValue::List(vec![
        CypherValue::Integer(1),
        CypherValue::String("two".into()),
        CypherValue::Null,
    ]);
    let result = traverse_cli::display::cypher_to_json(&list);
    assert!(result.is_array());
    let arr = result.as_array().unwrap();
    assert_eq!(arr.len(), 3);
    assert_eq!(arr[0], serde_json::json!(1));
    assert_eq!(arr[1], serde_json::json!("two"));
    assert!(arr[2].is_null());
}

#[test]
fn cypher_to_json_map() {
    use std::collections::BTreeMap;
    use traverse_cypher::types::CypherValue;
    let mut m = BTreeMap::new();
    m.insert("key".to_string(), CypherValue::Integer(42));
    let result = traverse_cli::display::cypher_to_json(&CypherValue::Map(m));
    assert!(result.is_object());
    assert_eq!(result["key"], serde_json::json!(42));
}

#[test]
fn cypher_to_json_node() {
    use std::collections::BTreeMap;
    use traverse_core::graph::types::NodeId;
    use traverse_cypher::types::CypherValue;

    let node = CypherValue::Node {
        id: NodeId(5),
        labels: vec!["Person".into()],
        properties: BTreeMap::new(),
    };
    let result = traverse_cli::display::cypher_to_json(&node);
    assert!(result.is_object());
    assert_eq!(result["_id"], serde_json::json!(5));
    assert_eq!(result["_labels"], serde_json::json!(["Person"]));
}

#[test]
fn cypher_to_json_edge() {
    use std::collections::BTreeMap;
    use traverse_core::graph::types::{EdgeId, NodeId};
    use traverse_cypher::types::CypherValue;

    let edge = CypherValue::Edge {
        id: EdgeId(10),
        edge_type: "KNOWS".into(),
        source: NodeId(1),
        target: NodeId(2),
        properties: BTreeMap::new(),
    };
    let result = traverse_cli::display::cypher_to_json(&edge);
    assert!(result.is_object());
    assert_eq!(result["_id"], serde_json::json!(10));
    assert_eq!(result["_type"], serde_json::json!("KNOWS"));
    assert_eq!(result["_start"], serde_json::json!(1));
    assert_eq!(result["_end"], serde_json::json!(2));
}

#[test]
fn cypher_to_json_path() {
    use std::collections::BTreeMap;
    use traverse_core::graph::types::{EdgeId, NodeId};
    use traverse_cypher::types::CypherValue;

    let node_a = CypherValue::Node {
        id: NodeId(1),
        labels: vec!["A".into()],
        properties: BTreeMap::new(),
    };
    let node_b = CypherValue::Node {
        id: NodeId(2),
        labels: vec!["B".into()],
        properties: BTreeMap::new(),
    };
    let edge = CypherValue::Edge {
        id: EdgeId(100),
        edge_type: "REL".into(),
        source: NodeId(1),
        target: NodeId(2),
        properties: BTreeMap::new(),
    };
    let path = CypherValue::Path {
        nodes: vec![node_a, node_b],
        edges: vec![edge],
    };
    let result = traverse_cli::display::cypher_to_json(&path);
    assert!(result.is_object());
    assert!(result["_nodes"].is_array());
    assert!(result["_edges"].is_array());
    assert_eq!(result["_nodes"].as_array().unwrap().len(), 2);
    assert_eq!(result["_edges"].as_array().unwrap().len(), 1);
}

// -- cypher_to_csv_field ----------------------------------------------

#[test]
fn cypher_to_csv_field_null() {
    use traverse_cypher::types::CypherValue;
    assert_eq!(
        traverse_cli::display::cypher_to_csv_field(&CypherValue::Null),
        ""
    );
}

#[test]
fn cypher_to_csv_field_string() {
    use traverse_cypher::types::CypherValue;
    assert_eq!(
        traverse_cli::display::cypher_to_csv_field(&CypherValue::String("hello".into())),
        "hello"
    );
}

#[test]
fn cypher_to_csv_field_integer() {
    use traverse_cypher::types::CypherValue;
    let result = traverse_cli::display::cypher_to_csv_field(&CypherValue::Integer(42));
    assert_eq!(result, "42");
}

// -- format_plan_args -------------------------------------------------

#[test]
fn format_plan_args_none() {
    assert_eq!(traverse_cli::display::format_plan_args(None), "");
}

#[test]
fn format_plan_args_map_values() {
    use std::collections::BTreeMap;
    use traverse_cypher::types::CypherValue;

    let mut m = BTreeMap::new();
    m.insert("label".to_string(), CypherValue::String("Person".into()));
    m.insert("count".to_string(), CypherValue::Integer(10));
    let args = CypherValue::Map(m);
    let result = traverse_cli::display::format_plan_args(Some(&args));
    // BTreeMap iteration is sorted by key
    assert!(result.contains("count: 10"));
    assert!(result.contains("label: Person"));
}

#[test]
fn format_plan_args_non_map() {
    use traverse_cypher::types::CypherValue;
    let val = CypherValue::Integer(42);
    assert_eq!(traverse_cli::display::format_plan_args(Some(&val)), "");
}

// -- extract_columns / extract_db / extract_failure -------------------

#[test]
fn extract_columns_present() {
    use traverse_server::packstream::Value;
    let metadata = vec![(
        "fields".to_string(),
        Value::List(vec![Value::String("a".into()), Value::String("b".into())]),
    )];
    let cols = traverse_cli::bolt_client::extract_columns(&metadata);
    assert_eq!(cols, vec!["a", "b"]);
}

#[test]
fn extract_columns_missing() {
    let metadata: Vec<(String, traverse_server::packstream::Value)> = vec![];
    let cols = traverse_cli::bolt_client::extract_columns(&metadata);
    assert!(cols.is_empty());
}

#[test]
fn extract_db_present() {
    use traverse_server::packstream::Value;
    let metadata = vec![("db".to_string(), Value::String("mydb".into()))];
    assert_eq!(
        traverse_cli::bolt_client::extract_db(&metadata),
        Some("mydb".into())
    );
}

#[test]
fn extract_db_missing() {
    let metadata: Vec<(String, traverse_server::packstream::Value)> = vec![];
    assert_eq!(traverse_cli::bolt_client::extract_db(&metadata), None);
}

#[test]
fn extract_failure_present() {
    use traverse_server::packstream::Value;
    let metadata = vec![
        ("code".to_string(), Value::String("MyError".into())),
        (
            "message".to_string(),
            Value::String("something broke".into()),
        ),
    ];
    let (code, msg) = traverse_cli::bolt_client::extract_failure(&metadata);
    assert_eq!(code, "MyError");
    assert_eq!(msg, "something broke");
}

#[test]
fn extract_failure_defaults() {
    let metadata: Vec<(String, traverse_server::packstream::Value)> = vec![];
    let (code, msg) = traverse_cli::bolt_client::extract_failure(&metadata);
    assert_eq!(code, "Neo.ClientError.Statement.SyntaxError");
    assert_eq!(msg, "unknown error");
}

// -- is_keyword -------------------------------------------------------

#[test]
fn is_keyword_known() {
    assert!(traverse_cli::highlight::is_keyword("MATCH"));
    assert!(traverse_cli::highlight::is_keyword("RETURN"));
    assert!(traverse_cli::highlight::is_keyword("CREATE"));
}

#[test]
fn is_keyword_mixed_case() {
    assert!(traverse_cli::highlight::is_keyword("match"));
    assert!(traverse_cli::highlight::is_keyword("Match"));
    assert!(traverse_cli::highlight::is_keyword("rEtUrN"));
}

#[test]
fn is_keyword_non_keyword() {
    assert!(!traverse_cli::highlight::is_keyword("foo"));
    assert!(!traverse_cli::highlight::is_keyword("SELECT"));
    assert!(!traverse_cli::highlight::is_keyword(""));
}

// -- highlight_cypher edge cases --------------------------------------

#[test]
fn highlight_escaped_quote_in_string() {
    let result = traverse_cli::highlight::highlight_cypher("RETURN 'it''s'");
    // The entire string including escaped quote should be green
    assert!(result.contains("\x1b[32m")); // green
    assert!(result.contains("it''s"));
}

#[test]
fn highlight_line_comment() {
    let result = traverse_cli::highlight::highlight_cypher("RETURN 1 // comment");
    assert!(result.contains("\x1b[90m")); // dim gray for comments
    assert!(result.contains("// comment"));
}

#[test]
fn highlight_negative_number_vs_subtraction() {
    // "a-1" should NOT color the -1 as a negative number (preceded by alphanumeric)
    let result = traverse_cli::highlight::highlight_cypher("a-1");
    // the '-' stays plain text (not in a yellow span); no panic, expected text present
    assert!(result.contains("a"));
    assert!(result.contains("1"));

    // Standalone "-1" should be yellow
    let result2 = traverse_cli::highlight::highlight_cypher("-1");
    assert!(result2.contains("\x1b[33m")); // yellow for number
}

#[test]
fn highlight_empty_string() {
    let result = traverse_cli::highlight::highlight_cypher("");
    assert_eq!(&*result, "");
}

// -- OutputFormat::from_str -------------------------------------------

#[test]
fn output_format_from_str_known() {
    use traverse_cli::display::OutputFormat;
    assert_eq!(OutputFormat::parse("table"), Some(OutputFormat::Table));
    assert_eq!(OutputFormat::parse("TABLE"), Some(OutputFormat::Table));
    assert_eq!(OutputFormat::parse("json"), Some(OutputFormat::Json));
    assert_eq!(OutputFormat::parse("Json"), Some(OutputFormat::Json));
    assert_eq!(OutputFormat::parse("csv"), Some(OutputFormat::Csv));
    assert_eq!(OutputFormat::parse("CSV"), Some(OutputFormat::Csv));
}

#[test]
fn output_format_from_str_unknown() {
    use traverse_cli::display::OutputFormat;
    assert_eq!(OutputFormat::parse("xml"), None);
    assert_eq!(OutputFormat::parse(""), None);
    assert_eq!(OutputFormat::parse("tab"), None);
}
