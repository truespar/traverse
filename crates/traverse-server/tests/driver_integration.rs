//! Bolt server integration tests with a raw Bolt 5.4 client speaking the wire protocol: handshake,
//! authentication, queries, transactions, error recovery.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;

use traverse_core::graph::Graph;
use traverse_core::tx::MvccStore;
use traverse_server::auth::{AuthCatalog, Role};
use traverse_server::framing;
use traverse_server::packstream::{self, Value};
use traverse_server::server::Server;

// -- test helpers ------------------------------------------------------

/// Bolt magic preamble: 60 60 B0 17
const BOLT_MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];

/// Starts a server on a random port, returns its address.
async fn start_server() -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let store = Arc::new(MvccStore::new(Graph::new()));
    let server = Server::with_store(store, "127.0.0.1:0".into());
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let addr = server.run_until_shutdown(shutdown_rx).await.unwrap();
    (addr, shutdown_tx)
}

/// Starts a server with authentication credentials.
async fn start_server_with_auth(
    creds: std::collections::HashMap<String, String>,
) -> (std::net::SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let store = Arc::new(MvccStore::new(Graph::new()));
    let server = Server::with_store(store, "127.0.0.1:0".into());
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    // HashMap credentials -> AuthCatalog
    let mut catalog = AuthCatalog::new();
    for (user, pass) in &creds {
        catalog.add_user(user, pass, Role::Admin).unwrap();
    }
    let addr = server
        .run_until_shutdown_with_auth(catalog, shutdown_rx)
        .await
        .unwrap();
    (addr, shutdown_tx)
}

/// Minimal Bolt 5.4 test client.
struct TestClient {
    reader: BufReader<tokio::net::tcp::OwnedReadHalf>,
    writer: BufWriter<tokio::net::tcp::OwnedWriteHalf>,
}

impl TestClient {
    /// Connects and handshakes only (no HELLO/LOGON).
    async fn connect_raw(addr: std::net::SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (read_half, write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut writer = BufWriter::new(write_half);

        // Send magic
        writer.write_all(&BOLT_MAGIC).await.unwrap();
        // Propose Bolt 5.4
        writer.write_u32(0x0000_0405).await.unwrap();
        writer.write_u32(0).await.unwrap();
        writer.write_u32(0).await.unwrap();
        writer.write_u32(0).await.unwrap();
        writer.flush().await.unwrap();

        let mut resp = [0u8; 4];
        reader.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp, [0, 0, 4, 5]);

        Self { reader, writer }
    }

    /// Connects with the Bolt 5.4 handshake, then authenticates.
    async fn connect(addr: std::net::SocketAddr) -> Self {
        Self::connect_minor(addr, 4).await
    }

    /// Connects over Bolt 5.`minor`.
    async fn connect_minor(addr: std::net::SocketAddr, minor: u8) -> Self {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (read_half, write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut writer = BufWriter::new(write_half);

        // Send magic
        writer.write_all(&BOLT_MAGIC).await.unwrap();

        // propose Bolt 5.<minor> only (range 0)
        writer
            .write_u32(u32::from_be_bytes([0, 0, minor, 5]))
            .await
            .unwrap();
        // Empty proposals 2-4
        writer.write_u32(0).await.unwrap();
        writer.write_u32(0).await.unwrap();
        writer.write_u32(0).await.unwrap();
        writer.flush().await.unwrap();

        // Read back negotiated version
        let mut resp = [0u8; 4];
        reader.read_exact(&mut resp).await.unwrap();
        assert_eq!(resp, [0, 0, minor, 5], "expected Bolt 5.{minor} negotiated");

        let mut client = Self { reader, writer };

        // HELLO
        client
            .send_message(&Value::Structure {
                tag: 0x01, // HELLO
                fields: vec![Value::Map(vec![(
                    "user_agent".into(),
                    Value::String("traverse-test/1.0".into()),
                )])],
            })
            .await;
        let hello_resp = client.recv_message().await;
        assert!(
            matches!(&hello_resp, Value::Structure { tag: 0x70, .. }),
            "expected SUCCESS after HELLO, got {hello_resp:?}"
        );

        // LOGON (Bolt 5.x needs a separate auth step)
        client
            .send_message(&Value::Structure {
                tag: 0x6A, // LOGON
                fields: vec![Value::Map(vec![(
                    "scheme".into(),
                    Value::String("none".into()),
                )])],
            })
            .await;
        let logon_resp = client.recv_message().await;
        assert!(
            matches!(&logon_resp, Value::Structure { tag: 0x70, .. }),
            "expected SUCCESS after LOGON, got {logon_resp:?}"
        );

        client
    }

    async fn send_message(&mut self, value: &Value) {
        let mut buf = Vec::new();
        packstream::encode_value(&mut buf, value);
        framing::write_message(&mut self.writer, &buf)
            .await
            .unwrap();
        self.writer.flush().await.unwrap();
    }

    async fn recv_message(&mut self) -> Value {
        let bytes = framing::read_message(&mut self.reader).await.unwrap();
        let mut pos = 0;
        packstream::decode_value(&bytes, &mut pos).unwrap()
    }

    /// RUN + PULL, collecting every record. Returns (fields_metadata, records, pull_metadata).
    async fn run_query(
        &mut self,
        query: &str,
        params: Vec<(String, Value)>,
    ) -> (Value, Vec<Vec<Value>>, Value) {
        // RUN
        self.send_message(&Value::Structure {
            tag: 0x10, // RUN
            fields: vec![
                Value::String(query.into()),
                Value::Map(params),
                Value::Map(vec![]),
            ],
        })
        .await;

        let run_resp = self.recv_message().await;
        assert!(
            matches!(&run_resp, Value::Structure { tag: 0x70, .. }),
            "expected SUCCESS after RUN, got {run_resp:?}"
        );

        // PULL all
        self.send_message(&Value::Structure {
            tag: 0x3F, // PULL
            fields: vec![Value::Map(vec![("n".into(), Value::Integer(-1))])],
        })
        .await;

        let mut records = Vec::new();
        loop {
            let msg = self.recv_message().await;
            match &msg {
                Value::Structure { tag: 0x71, fields } => {
                    // RECORD: fields[0] is a List of row values
                    if let Some(Value::List(row)) = fields.first() {
                        records.push(row.clone());
                    }
                }
                Value::Structure { tag: 0x70, .. } => {
                    // SUCCESS - done streaming
                    return (run_resp, records, msg);
                }
                Value::Structure { tag: 0x7F, .. } => {
                    // FAILURE
                    return (run_resp, records, msg);
                }
                _ => panic!("unexpected message during PULL: {msg:?}"),
            }
        }
    }

    /// GOODBYE for a clean close.
    async fn goodbye(&mut self) {
        self.send_message(&Value::Structure {
            tag: 0x02, // GOODBYE
            fields: vec![],
        })
        .await;
    }
}

/// Metadata of a SUCCESS response.
fn get_metadata(value: &Value) -> &[(String, Value)] {
    match value {
        Value::Structure { tag: 0x70, fields } => match fields.first() {
            Some(Value::Map(m)) => m,
            _ => &[],
        },
        _ => &[],
    }
}

fn metadata_get<'a>(meta: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    meta.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

// -- tests -------------------------------------------------------------

#[tokio::test]
async fn return_scalar() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    let (run_meta, records, pull_meta) = client.run_query("RETURN 1 AS n", vec![]).await;

    // Check fields metadata
    let meta = get_metadata(&run_meta);
    let fields = metadata_get(meta, "fields").unwrap();
    assert_eq!(fields, &Value::List(vec![Value::String("n".into())]));

    // Check type is "r" (read) in RUN SUCCESS
    let query_type = metadata_get(meta, "type").unwrap();
    assert_eq!(query_type, &Value::String("r".into()));

    // Check t_first is present and >= 0
    let t_first = metadata_get(meta, "t_first").unwrap();
    assert!(matches!(t_first, Value::Integer(v) if *v >= 0));

    // Check PULL SUCCESS has t_last and type
    let pull_meta = get_metadata(&pull_meta);
    let t_last = metadata_get(pull_meta, "t_last").unwrap();
    assert!(matches!(t_last, Value::Integer(v) if *v >= 0));
    let pull_type = metadata_get(pull_meta, "type").unwrap();
    assert_eq!(pull_type, &Value::String("r".into()));

    // Check record
    assert_eq!(records.len(), 1);
    assert_eq!(records[0], vec![Value::Integer(1)]);

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn return_multiple_columns() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    let (_run_meta, records, _) = client
        .run_query("RETURN 'hello' AS greeting, 42 AS answer", vec![])
        .await;

    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0],
        vec![Value::String("hello".into()), Value::Integer(42),]
    );

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn create_and_read_nodes() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // Create a node
    let (run_meta, records, pull_meta) = client
        .run_query(
            "CREATE (n:Person {name: 'Alice', age: 30}) RETURN n",
            vec![],
        )
        .await;

    assert_eq!(records.len(), 1);

    // Check type is "rw" (read-write since it has RETURN + CREATE)
    let meta = get_metadata(&run_meta);
    let query_type = metadata_get(meta, "type").unwrap();
    assert_eq!(query_type, &Value::String("rw".into()));

    // Check stats in PULL SUCCESS
    let pull_m = get_metadata(&pull_meta);
    let nodes_created = metadata_get(pull_m, "nodes-created").unwrap();
    assert_eq!(nodes_created, &Value::Integer(1));
    let props_set = metadata_get(pull_m, "properties-set").unwrap();
    assert_eq!(props_set, &Value::Integer(2));
    let labels_added = metadata_get(pull_m, "labels-added").unwrap();
    assert_eq!(labels_added, &Value::Integer(1));

    // The result should be a Node structure (tag 0x4E)
    match &records[0][0] {
        Value::Structure { tag, fields } => {
            assert_eq!(*tag, 0x4E); // NODE
            // fields: [id, labels, properties, element_id]
            assert_eq!(fields.len(), 4);
            // labels should contain "Person"
            if let Value::List(labels) = &fields[1] {
                assert_eq!(labels, &vec![Value::String("Person".into())]);
            } else {
                panic!("expected labels list");
            }
        }
        other => panic!("expected Node structure, got {other:?}"),
    }

    // Read it back
    let (read_run_meta, records, _) = client
        .run_query("MATCH (n:Person) RETURN n.name, n.age", vec![])
        .await;

    // Read query should have type "r"
    let read_meta = get_metadata(&read_run_meta);
    let read_type = metadata_get(read_meta, "type").unwrap();
    assert_eq!(read_type, &Value::String("r".into()));

    assert_eq!(records.len(), 1);
    assert_eq!(records[0][0], Value::String("Alice".into()));
    assert_eq!(records[0][1], Value::Integer(30));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn explicit_transaction_commit() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // BEGIN
    client
        .send_message(&Value::Structure {
            tag: 0x11, // BEGIN
            fields: vec![Value::Map(vec![])],
        })
        .await;
    let begin_resp = client.recv_message().await;
    assert!(matches!(&begin_resp, Value::Structure { tag: 0x70, .. }));

    // RUN inside transaction
    let (_, records, _) = client
        .run_query("CREATE (n:TxTest {val: 1}) RETURN n.val", vec![])
        .await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0][0], Value::Integer(1));

    // COMMIT
    client
        .send_message(&Value::Structure {
            tag: 0x12, // COMMIT
            fields: vec![],
        })
        .await;
    let commit_resp = client.recv_message().await;
    assert!(matches!(&commit_resp, Value::Structure { tag: 0x70, .. }));

    // Verify data persisted after commit
    let (_, records, _) = client
        .run_query("MATCH (n:TxTest) RETURN n.val", vec![])
        .await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0][0], Value::Integer(1));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn explicit_transaction_rollback() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // BEGIN
    client
        .send_message(&Value::Structure {
            tag: 0x11,
            fields: vec![Value::Map(vec![])],
        })
        .await;
    let begin_resp = client.recv_message().await;
    assert!(matches!(&begin_resp, Value::Structure { tag: 0x70, .. }));

    // RUN inside transaction
    let (_, _, _) = client
        .run_query("CREATE (n:RollbackTest {val: 99})", vec![])
        .await;

    // ROLLBACK
    client
        .send_message(&Value::Structure {
            tag: 0x13, // ROLLBACK
            fields: vec![],
        })
        .await;
    let rollback_resp = client.recv_message().await;
    assert!(matches!(&rollback_resp, Value::Structure { tag: 0x70, .. }));

    // Verify data was NOT persisted
    let (_, records, _) = client
        .run_query("MATCH (n:RollbackTest) RETURN n", vec![])
        .await;
    assert_eq!(records.len(), 0);

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn syntax_error_then_reset() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // Send a query with a syntax error
    client
        .send_message(&Value::Structure {
            tag: 0x10, // RUN
            fields: vec![
                Value::String("THIS IS NOT VALID CYPHER !!!".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;

    let run_resp = client.recv_message().await;
    // Should get FAILURE (0x7F)
    match &run_resp {
        Value::Structure { tag: 0x7F, fields } => {
            if let Some(Value::Map(meta)) = fields.first() {
                let code = meta.iter().find(|(k, _)| k == "code").map(|(_, v)| v);
                assert!(code.is_some(), "FAILURE should include error code");
            }
        }
        _ => panic!("expected FAILURE, got {run_resp:?}"),
    }

    // In Failed state, messages should be IGNORED
    client
        .send_message(&Value::Structure {
            tag: 0x10, // RUN
            fields: vec![
                Value::String("RETURN 1".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let ignored_resp = client.recv_message().await;
    assert!(
        matches!(&ignored_resp, Value::Structure { tag: 0x7E, .. }),
        "expected IGNORED in Failed state, got {ignored_resp:?}"
    );

    // RESET to recover
    client
        .send_message(&Value::Structure {
            tag: 0x0F, // RESET
            fields: vec![],
        })
        .await;
    let reset_resp = client.recv_message().await;
    assert!(matches!(&reset_resp, Value::Structure { tag: 0x70, .. }));

    // Now queries should work again
    let (_, records, _) = client.run_query("RETURN 42 AS n", vec![]).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0][0], Value::Integer(42));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn query_with_parameters() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    let (_, records, _) = client
        .run_query(
            "RETURN $x + $y AS sum",
            vec![
                ("x".into(), Value::Integer(10)),
                ("y".into(), Value::Integer(32)),
            ],
        )
        .await;

    assert_eq!(records.len(), 1);
    assert_eq!(records[0][0], Value::Integer(42));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn pull_with_streaming() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // Create multiple nodes
    client
        .run_query(
            "CREATE (:Item {n: 1}), (:Item {n: 2}), (:Item {n: 3})",
            vec![],
        )
        .await;

    // RUN
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("MATCH (i:Item) RETURN i.n ORDER BY i.n".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let run_resp = client.recv_message().await;
    assert!(matches!(&run_resp, Value::Structure { tag: 0x70, .. }));

    // PULL with n=1 (fetch one at a time)
    client
        .send_message(&Value::Structure {
            tag: 0x3F,
            fields: vec![Value::Map(vec![("n".into(), Value::Integer(1))])],
        })
        .await;

    // Should get 1 RECORD + SUCCESS with has_more=true
    let first_record = client.recv_message().await;
    assert!(matches!(&first_record, Value::Structure { tag: 0x71, .. }));

    let first_success = client.recv_message().await;
    let meta = get_metadata(&first_success);
    let has_more = metadata_get(meta, "has_more");
    assert_eq!(has_more, Some(&Value::Bool(true)));

    // PULL remaining
    client
        .send_message(&Value::Structure {
            tag: 0x3F,
            fields: vec![Value::Map(vec![("n".into(), Value::Integer(-1))])],
        })
        .await;

    // Should get 2 more RECORDs + SUCCESS with has_more=false
    let _record2 = client.recv_message().await;
    let _record3 = client.recv_message().await;
    let final_success = client.recv_message().await;
    let meta = get_metadata(&final_success);
    let has_more = metadata_get(meta, "has_more");
    assert_eq!(has_more, Some(&Value::Bool(false)));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn write_only_query_type() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // Write-only query (no RETURN)
    let (run_meta, records, pull_meta) = client
        .run_query("CREATE (:Animal {species: 'Dog'})", vec![])
        .await;

    assert_eq!(records.len(), 0);

    // RUN SUCCESS should have type "w"
    let meta = get_metadata(&run_meta);
    let query_type = metadata_get(meta, "type").unwrap();
    assert_eq!(query_type, &Value::String("w".into()));

    // PULL SUCCESS should have stats
    let pull_m = get_metadata(&pull_meta);
    let nodes_created = metadata_get(pull_m, "nodes-created").unwrap();
    assert_eq!(nodes_created, &Value::Integer(1));
    let props_set = metadata_get(pull_m, "properties-set").unwrap();
    assert_eq!(props_set, &Value::Integer(1));

    client.goodbye().await;
    let _ = shutdown.send(());
}

// -- keep-alive, metadata, extras, pipelining ---------------------------

#[tokio::test]
async fn noop_keepalive() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // Send a NOOP (empty 0x0000 chunk pair)
    client.writer.write_u16(0).await.unwrap();
    client.writer.flush().await.unwrap();

    // The connection should still be alive - run a query
    let (_, records, _) = client.run_query("RETURN 'alive' AS status", vec![]).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0][0], Value::String("alive".into()));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn bookmark_returned_on_autocommit_write() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    let (_, _, pull_meta) = client
        .run_query("CREATE (:BookmarkTest {val: 1})", vec![])
        .await;

    let meta = get_metadata(&pull_meta);
    let bookmark = metadata_get(meta, "bookmark");
    assert!(
        bookmark.is_some(),
        "auto-commit write should return a bookmark"
    );
    if let Some(Value::String(bk)) = bookmark {
        assert!(
            bk.starts_with("FB:"),
            "bookmark should start with 'FB:': {bk}"
        );
    } else {
        panic!("bookmark should be a string");
    }

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn bookmark_returned_on_explicit_commit() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // BEGIN
    client
        .send_message(&Value::Structure {
            tag: 0x11,
            fields: vec![Value::Map(vec![])],
        })
        .await;
    let _ = client.recv_message().await;

    // RUN + PULL inside transaction
    let _ = client
        .run_query("CREATE (:BookmarkTx {val: 2})", vec![])
        .await;

    // COMMIT
    client
        .send_message(&Value::Structure {
            tag: 0x12,
            fields: vec![],
        })
        .await;
    let commit_resp = client.recv_message().await;
    let meta = get_metadata(&commit_resp);

    let bookmark = metadata_get(meta, "bookmark");
    assert!(bookmark.is_some(), "COMMIT should return a bookmark");
    if let Some(Value::String(bk)) = bookmark {
        assert!(
            bk.starts_with("FB:"),
            "bookmark should start with 'FB:': {bk}"
        );
    }

    let db = metadata_get(meta, "db");
    assert_eq!(db, Some(&Value::String("traverse".into())));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn db_metadata_in_responses() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    let (run_meta, _, pull_meta) = client.run_query("RETURN 1 AS n", vec![]).await;

    // RUN SUCCESS should have db
    let meta = get_metadata(&run_meta);
    let db = metadata_get(meta, "db");
    assert_eq!(db, Some(&Value::String("traverse".into())));

    // PULL SUCCESS should have db
    let pull_m = get_metadata(&pull_meta);
    let db = metadata_get(pull_m, "db");
    assert_eq!(db, Some(&Value::String("traverse".into())));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn tx_timeout_accepted() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // RUN with tx_timeout in extras (generous timeout - should succeed)
    client
        .send_message(&Value::Structure {
            tag: 0x10, // RUN
            fields: vec![
                Value::String("RETURN 1 AS n".into()),
                Value::Map(vec![]),
                Value::Map(vec![("tx_timeout".into(), Value::Integer(60000))]),
            ],
        })
        .await;

    let run_resp = client.recv_message().await;
    assert!(
        matches!(&run_resp, Value::Structure { tag: 0x70, .. }),
        "expected SUCCESS with tx_timeout, got {run_resp:?}"
    );

    // PULL
    client
        .send_message(&Value::Structure {
            tag: 0x3F,
            fields: vec![Value::Map(vec![("n".into(), Value::Integer(-1))])],
        })
        .await;
    let record = client.recv_message().await;
    assert!(matches!(&record, Value::Structure { tag: 0x71, .. }));
    let pull_resp = client.recv_message().await;
    assert!(matches!(&pull_resp, Value::Structure { tag: 0x70, .. }));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn begin_with_tx_timeout() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // BEGIN with tx_timeout
    client
        .send_message(&Value::Structure {
            tag: 0x11,
            fields: vec![Value::Map(vec![(
                "tx_timeout".into(),
                Value::Integer(60000),
            )])],
        })
        .await;
    let begin_resp = client.recv_message().await;
    assert!(matches!(&begin_resp, Value::Structure { tag: 0x70, .. }));

    // RUN inside transaction
    let (_, records, _) = client.run_query("RETURN 42 AS n", vec![]).await;
    assert_eq!(records[0][0], Value::Integer(42));

    // ROLLBACK
    client
        .send_message(&Value::Structure {
            tag: 0x13,
            fields: vec![],
        })
        .await;
    let _ = client.recv_message().await;

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn pipelining_multiple_runs() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // First RUN
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("RETURN 1 AS a".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let run1_resp = client.recv_message().await;
    let meta1 = get_metadata(&run1_resp);
    let qid1 = metadata_get(meta1, "qid").unwrap().as_i64().unwrap();

    // Second RUN (pipelined - sent while first result still pending)
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("RETURN 2 AS b".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let run2_resp = client.recv_message().await;
    let meta2 = get_metadata(&run2_resp);
    let qid2 = metadata_get(meta2, "qid").unwrap().as_i64().unwrap();

    // qids should be different
    assert_ne!(qid1, qid2, "pipelined RUNs should have different qids");

    // PULL first result by qid
    client
        .send_message(&Value::Structure {
            tag: 0x3F,
            fields: vec![Value::Map(vec![
                ("n".into(), Value::Integer(-1)),
                ("qid".into(), Value::Integer(qid1)),
            ])],
        })
        .await;
    let record1 = client.recv_message().await;
    match &record1 {
        Value::Structure { tag: 0x71, fields } => {
            if let Some(Value::List(row)) = fields.first() {
                assert_eq!(row[0], Value::Integer(1));
            }
        }
        _ => panic!("expected RECORD, got {record1:?}"),
    }
    let pull1_success = client.recv_message().await;
    assert!(matches!(&pull1_success, Value::Structure { tag: 0x70, .. }));

    // PULL second result by qid
    client
        .send_message(&Value::Structure {
            tag: 0x3F,
            fields: vec![Value::Map(vec![
                ("n".into(), Value::Integer(-1)),
                ("qid".into(), Value::Integer(qid2)),
            ])],
        })
        .await;
    let record2 = client.recv_message().await;
    match &record2 {
        Value::Structure { tag: 0x71, fields } => {
            if let Some(Value::List(row)) = fields.first() {
                assert_eq!(row[0], Value::Integer(2));
            }
        }
        _ => panic!("expected RECORD, got {record2:?}"),
    }
    let pull2_success = client.recv_message().await;
    assert!(matches!(&pull2_success, Value::Structure { tag: 0x70, .. }));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn notifications_placeholder_in_pull() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    let (_, _, pull_meta) = client.run_query("RETURN 1 AS n", vec![]).await;

    let meta = get_metadata(&pull_meta);
    let notifications = metadata_get(meta, "notifications");
    assert!(
        notifications.is_none() || notifications == Some(&Value::List(vec![])),
        "PULL SUCCESS should omit notifications or have empty list"
    );

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn begin_with_bookmarks_accepted() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // First: create data and get a bookmark
    let (_, _, pull_meta) = client.run_query("CREATE (:BmTest {val: 1})", vec![]).await;
    let meta = get_metadata(&pull_meta);
    let bookmark = metadata_get(meta, "bookmark").unwrap().clone();

    // BEGIN with the bookmark
    client
        .send_message(&Value::Structure {
            tag: 0x11,
            fields: vec![Value::Map(vec![(
                "bookmarks".into(),
                Value::List(vec![bookmark]),
            )])],
        })
        .await;
    let begin_resp = client.recv_message().await;
    assert!(
        matches!(&begin_resp, Value::Structure { tag: 0x70, .. }),
        "BEGIN with bookmarks should succeed"
    );

    // ROLLBACK
    client
        .send_message(&Value::Structure {
            tag: 0x13,
            fields: vec![],
        })
        .await;
    let _ = client.recv_message().await;

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn impersonation_accepted() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // RUN with imp_user in extras
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("RETURN 1 AS n".into()),
                Value::Map(vec![]),
                Value::Map(vec![("imp_user".into(), Value::String("alice".into()))]),
            ],
        })
        .await;

    let run_resp = client.recv_message().await;
    assert!(
        matches!(&run_resp, Value::Structure { tag: 0x70, .. }),
        "RUN with imp_user should succeed"
    );

    // PULL
    client
        .send_message(&Value::Structure {
            tag: 0x3F,
            fields: vec![Value::Map(vec![("n".into(), Value::Integer(-1))])],
        })
        .await;
    let record = client.recv_message().await;
    assert!(matches!(&record, Value::Structure { tag: 0x71, .. }));
    let pull_resp = client.recv_message().await;
    assert!(matches!(&pull_resp, Value::Structure { tag: 0x70, .. }));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn begin_with_impersonation() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // BEGIN with imp_user
    client
        .send_message(&Value::Structure {
            tag: 0x11,
            fields: vec![Value::Map(vec![(
                "imp_user".into(),
                Value::String("bob".into()),
            )])],
        })
        .await;
    let begin_resp = client.recv_message().await;
    assert!(matches!(&begin_resp, Value::Structure { tag: 0x70, .. }));

    // ROLLBACK
    client
        .send_message(&Value::Structure {
            tag: 0x13,
            fields: vec![],
        })
        .await;
    let _ = client.recv_message().await;

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn qid_in_run_metadata() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    let (run_meta, _, _) = client.run_query("RETURN 1 AS n", vec![]).await;

    let meta = get_metadata(&run_meta);
    let qid = metadata_get(meta, "qid");
    assert!(qid.is_some(), "RUN SUCCESS should include qid");
    assert!(matches!(qid, Some(Value::Integer(_))));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn discard_with_qid() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // RUN
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("RETURN 1 AS n".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let run_resp = client.recv_message().await;
    let meta = get_metadata(&run_resp);
    let qid = metadata_get(meta, "qid").unwrap().as_i64().unwrap();

    // DISCARD with qid
    client
        .send_message(&Value::Structure {
            tag: 0x2F,
            fields: vec![Value::Map(vec![
                ("n".into(), Value::Integer(-1)),
                ("qid".into(), Value::Integer(qid)),
            ])],
        })
        .await;
    let discard_resp = client.recv_message().await;
    assert!(
        matches!(&discard_resp, Value::Structure { tag: 0x70, .. }),
        "DISCARD with qid should succeed"
    );

    // Should be back to Ready state - can run another query
    let (_, records, _) = client.run_query("RETURN 42 AS n", vec![]).await;
    assert_eq!(records[0][0], Value::Integer(42));

    client.goodbye().await;
    let _ = shutdown.send(());
}

// -- Authentication tests ---------------------------------------------

#[tokio::test]
async fn auth_basic_success() {
    let mut creds = std::collections::HashMap::new();
    creds.insert("neo4j".to_string(), "password123".to_string());
    let (addr, shutdown) = start_server_with_auth(creds).await;

    let mut client = TestClient::connect_raw(addr).await;

    // HELLO
    client
        .send_message(&Value::Structure {
            tag: 0x01,
            fields: vec![Value::Map(vec![(
                "user_agent".into(),
                Value::String("test/1.0".into()),
            )])],
        })
        .await;
    let hello_resp = client.recv_message().await;
    assert!(matches!(&hello_resp, Value::Structure { tag: 0x70, .. }));

    // LOGON with correct credentials
    client
        .send_message(&Value::Structure {
            tag: 0x6A,
            fields: vec![Value::Map(vec![
                ("scheme".into(), Value::String("basic".into())),
                ("principal".into(), Value::String("neo4j".into())),
                ("credentials".into(), Value::String("password123".into())),
            ])],
        })
        .await;
    let logon_resp = client.recv_message().await;
    assert!(
        matches!(&logon_resp, Value::Structure { tag: 0x70, .. }),
        "valid credentials should succeed, got {logon_resp:?}"
    );

    // Should be able to run queries
    let (_, records, _) = client.run_query("RETURN 1 AS n", vec![]).await;
    assert_eq!(records[0][0], Value::Integer(1));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn auth_basic_wrong_password() {
    let mut creds = std::collections::HashMap::new();
    creds.insert("neo4j".to_string(), "correct_password".to_string());
    let (addr, shutdown) = start_server_with_auth(creds).await;

    let mut client = TestClient::connect_raw(addr).await;

    // HELLO
    client
        .send_message(&Value::Structure {
            tag: 0x01,
            fields: vec![Value::Map(vec![(
                "user_agent".into(),
                Value::String("test/1.0".into()),
            )])],
        })
        .await;
    let _ = client.recv_message().await;

    // LOGON with wrong password
    client
        .send_message(&Value::Structure {
            tag: 0x6A,
            fields: vec![Value::Map(vec![
                ("scheme".into(), Value::String("basic".into())),
                ("principal".into(), Value::String("neo4j".into())),
                ("credentials".into(), Value::String("wrong_password".into())),
            ])],
        })
        .await;
    let logon_resp = client.recv_message().await;

    // Should get FAILURE
    match &logon_resp {
        Value::Structure { tag: 0x7F, fields } => {
            if let Some(Value::Map(meta)) = fields.first() {
                let code = meta.iter().find(|(k, _)| k == "code").map(|(_, v)| v);
                assert_eq!(
                    code,
                    Some(&Value::String(
                        "Neo.ClientError.Security.Unauthorized".into()
                    ))
                );
            } else {
                panic!("FAILURE missing metadata");
            }
        }
        _ => panic!("expected FAILURE for wrong password, got {logon_resp:?}"),
    }

    let _ = shutdown.send(());
}

#[tokio::test]
async fn auth_basic_unknown_user() {
    let mut creds = std::collections::HashMap::new();
    creds.insert("neo4j".to_string(), "password".to_string());
    let (addr, shutdown) = start_server_with_auth(creds).await;

    let mut client = TestClient::connect_raw(addr).await;

    // HELLO
    client
        .send_message(&Value::Structure {
            tag: 0x01,
            fields: vec![Value::Map(vec![(
                "user_agent".into(),
                Value::String("test/1.0".into()),
            )])],
        })
        .await;
    let _ = client.recv_message().await;

    // LOGON with unknown user
    client
        .send_message(&Value::Structure {
            tag: 0x6A,
            fields: vec![Value::Map(vec![
                ("scheme".into(), Value::String("basic".into())),
                ("principal".into(), Value::String("unknown_user".into())),
                ("credentials".into(), Value::String("password".into())),
            ])],
        })
        .await;
    let logon_resp = client.recv_message().await;
    assert!(
        matches!(&logon_resp, Value::Structure { tag: 0x7F, .. }),
        "unknown user should fail, got {logon_resp:?}"
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn auth_none_scheme_rejected_when_auth_configured() {
    let mut creds = std::collections::HashMap::new();
    creds.insert("neo4j".to_string(), "password".to_string());
    let (addr, shutdown) = start_server_with_auth(creds).await;

    let mut client = TestClient::connect_raw(addr).await;

    // HELLO
    client
        .send_message(&Value::Structure {
            tag: 0x01,
            fields: vec![Value::Map(vec![(
                "user_agent".into(),
                Value::String("test/1.0".into()),
            )])],
        })
        .await;
    let _ = client.recv_message().await;

    // LOGON with scheme "none" should be rejected when auth is configured
    client
        .send_message(&Value::Structure {
            tag: 0x6A,
            fields: vec![Value::Map(vec![(
                "scheme".into(),
                Value::String("none".into()),
            )])],
        })
        .await;
    let logon_resp = client.recv_message().await;
    assert!(
        matches!(&logon_resp, Value::Structure { tag: 0x7F, .. }),
        "scheme 'none' should be rejected when auth is configured, got {logon_resp:?}"
    );

    let _ = shutdown.send(());
}

// -- EXPLAIN / PROFILE tests ------------------------------------------

#[tokio::test]
async fn explain_returns_plan_no_rows() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // Create some data first
    client.run_query("CREATE (:Foo {x: 1})", vec![]).await;

    // EXPLAIN query
    let (run_meta, records, _pull_meta) = client
        .run_query("EXPLAIN MATCH (n:Foo) RETURN n", vec![])
        .await;

    // EXPLAIN should produce no rows
    assert_eq!(records.len(), 0, "EXPLAIN should return 0 rows");

    // RUN SUCCESS should contain plan metadata
    let meta = get_metadata(&run_meta);
    let plan = metadata_get(meta, "plan");
    assert!(
        plan.is_some(),
        "EXPLAIN RUN SUCCESS should include plan metadata"
    );

    // Plan should be a map with operatorType
    if let Some(Value::Map(plan_map)) = plan {
        let op_type = plan_map.iter().find(|(k, _)| k == "operatorType");
        assert!(op_type.is_some(), "plan should have operatorType key");
    } else {
        panic!("plan should be a Map, got {plan:?}");
    }

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn profile_returns_plan_with_rows() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // Create some data
    client
        .run_query("CREATE (:Bar {x: 1}), (:Bar {x: 2})", vec![])
        .await;

    // PROFILE query
    let (run_meta, records, pull_meta) = client
        .run_query("PROFILE MATCH (n:Bar) RETURN n.x", vec![])
        .await;

    // PROFILE should produce actual rows
    assert_eq!(records.len(), 2, "PROFILE should execute and return rows");

    // RUN SUCCESS should NOT contain the plan (plan goes in PULL SUCCESS for PROFILE)
    let meta = get_metadata(&run_meta);
    let run_plan = metadata_get(meta, "plan");
    assert!(
        run_plan.is_none(),
        "PROFILE plan should not be in RUN SUCCESS"
    );

    // PULL SUCCESS should contain the profile
    let pull_m = get_metadata(&pull_meta);
    let profile = metadata_get(pull_m, "profile");
    assert!(
        profile.is_some(),
        "PROFILE PULL SUCCESS should include profile metadata"
    );

    if let Some(Value::Map(plan_map)) = profile {
        let op_type = plan_map.iter().find(|(k, _)| k == "operatorType");
        assert!(op_type.is_some(), "profile should have operatorType key");
    }

    client.goodbye().await;
    let _ = shutdown.send(());
}

// -- Deferred execution tests -----------------------------------------

#[tokio::test]
async fn deferred_read_query_executes_on_pull() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // Create data
    client
        .run_query("CREATE (:Deferred {v: 10}), (:Deferred {v: 20})", vec![])
        .await;

    // RUN a read query (should be deferred)
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("MATCH (n:Deferred) RETURN n.v ORDER BY n.v".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;

    let run_resp = client.recv_message().await;
    assert!(
        matches!(&run_resp, Value::Structure { tag: 0x70, .. }),
        "deferred RUN should succeed"
    );

    // Check run metadata has fields and qid
    let meta = get_metadata(&run_resp);
    assert!(metadata_get(meta, "fields").is_some());
    assert!(metadata_get(meta, "qid").is_some());

    // PULL should trigger execution and return rows
    client
        .send_message(&Value::Structure {
            tag: 0x3F,
            fields: vec![Value::Map(vec![("n".into(), Value::Integer(-1))])],
        })
        .await;

    let mut rows = Vec::new();
    loop {
        let msg = client.recv_message().await;
        match &msg {
            Value::Structure { tag: 0x71, fields } => {
                if let Some(Value::List(row)) = fields.first() {
                    rows.push(row.clone());
                }
            }
            Value::Structure { tag: 0x70, .. } => break,
            _ => panic!("unexpected: {msg:?}"),
        }
    }

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Integer(10));
    assert_eq!(rows[1][0], Value::Integer(20));

    client.goodbye().await;
    let _ = shutdown.send(());
}

// -- protocol compliance -----------------------------------------------

#[tokio::test]
async fn reset_in_auth_state_stays_in_auth() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect_raw(addr).await;

    // HELLO
    client
        .send_message(&Value::Structure {
            tag: 0x01,
            fields: vec![Value::Map(vec![(
                "user_agent".into(),
                Value::String("test/1.0".into()),
            )])],
        })
        .await;
    let _ = client.recv_message().await;

    // RESET before LOGON (still in Authentication state)
    client
        .send_message(&Value::Structure {
            tag: 0x0F, // RESET
            fields: vec![],
        })
        .await;
    let reset_resp = client.recv_message().await;
    assert!(matches!(&reset_resp, Value::Structure { tag: 0x70, .. }));

    // Should still need LOGON - RUN should fail
    client
        .send_message(&Value::Structure {
            tag: 0x10, // RUN
            fields: vec![
                Value::String("RETURN 1".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let run_resp = client.recv_message().await;
    assert!(
        matches!(&run_resp, Value::Structure { tag: 0x7F, .. }),
        "RUN after RESET in auth state should fail, got {run_resp:?}"
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn logoff_cleans_up_transaction() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // BEGIN a transaction
    client
        .send_message(&Value::Structure {
            tag: 0x11,
            fields: vec![Value::Map(vec![])],
        })
        .await;
    let _ = client.recv_message().await;

    // RUN inside transaction
    let _ = client
        .run_query("CREATE (:LogoffTest {val: 1})", vec![])
        .await;

    // LOGOFF (should abort tx)
    client
        .send_message(&Value::Structure {
            tag: 0x6B, // LOGOFF
            fields: vec![],
        })
        .await;
    let logoff_resp = client.recv_message().await;
    assert!(matches!(&logoff_resp, Value::Structure { tag: 0x70, .. }));

    // Re-authenticate
    client
        .send_message(&Value::Structure {
            tag: 0x6A, // LOGON
            fields: vec![Value::Map(vec![(
                "scheme".into(),
                Value::String("none".into()),
            )])],
        })
        .await;
    let _ = client.recv_message().await;

    // Data should NOT have been committed
    let (_, records, _) = client
        .run_query("MATCH (n:LogoffTest) RETURN n", vec![])
        .await;
    assert_eq!(
        records.len(),
        0,
        "LOGOFF should have aborted the transaction"
    );

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn hello_returns_unique_connection_id_and_hints() {
    let (addr, shutdown) = start_server().await;

    // Connect two clients, check they get different connection_ids
    let mut client1 = TestClient::connect_raw(addr).await;
    let mut client2 = TestClient::connect_raw(addr).await;

    // HELLO for client 1
    client1
        .send_message(&Value::Structure {
            tag: 0x01,
            fields: vec![Value::Map(vec![(
                "user_agent".into(),
                Value::String("test/1.0".into()),
            )])],
        })
        .await;
    let hello1 = client1.recv_message().await;

    // HELLO for client 2
    client2
        .send_message(&Value::Structure {
            tag: 0x01,
            fields: vec![Value::Map(vec![(
                "user_agent".into(),
                Value::String("test/2.0".into()),
            )])],
        })
        .await;
    let hello2 = client2.recv_message().await;

    let meta1 = get_metadata(&hello1);
    let meta2 = get_metadata(&hello2);

    // Connection IDs should be present and different
    let id1 = metadata_get(meta1, "connection_id");
    let id2 = metadata_get(meta2, "connection_id");
    assert!(id1.is_some(), "HELLO should include connection_id");
    assert!(id2.is_some(), "HELLO should include connection_id");
    assert_ne!(id1, id2, "connection_ids should be unique");

    // Check hints are present
    let hints = metadata_get(meta1, "hints");
    assert!(hints.is_some(), "HELLO should include hints map");
    if let Some(Value::Map(hints_map)) = hints {
        let timeout = hints_map
            .iter()
            .find(|(k, _)| k == "connection.recv_timeout_seconds");
        assert!(
            timeout.is_some(),
            "hints should include connection.recv_timeout_seconds"
        );
    }

    let _ = shutdown.send(());
}

#[tokio::test]
async fn partial_discard() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // Create 3 items
    client
        .run_query("CREATE (:PD {n: 1}), (:PD {n: 2}), (:PD {n: 3})", vec![])
        .await;

    // RUN
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("MATCH (i:PD) RETURN i.n ORDER BY i.n".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let _ = client.recv_message().await; // RUN SUCCESS

    // DISCARD 1 record (partial)
    client
        .send_message(&Value::Structure {
            tag: 0x2F, // DISCARD
            fields: vec![Value::Map(vec![("n".into(), Value::Integer(1))])],
        })
        .await;
    let discard1 = client.recv_message().await;
    let meta1 = get_metadata(&discard1);
    let has_more1 = metadata_get(meta1, "has_more");
    assert_eq!(
        has_more1,
        Some(&Value::Bool(true)),
        "partial discard should have has_more=true"
    );

    // DISCARD remaining
    client
        .send_message(&Value::Structure {
            tag: 0x2F,
            fields: vec![Value::Map(vec![("n".into(), Value::Integer(-1))])],
        })
        .await;
    let discard2 = client.recv_message().await;
    let meta2 = get_metadata(&discard2);
    let has_more2 = metadata_get(meta2, "has_more");
    assert_eq!(
        has_more2,
        Some(&Value::Bool(false)),
        "full discard should have has_more=false"
    );

    // Should be back in Ready state
    let (_, records, _) = client.run_query("RETURN 42 AS n", vec![]).await;
    assert_eq!(records[0][0], Value::Integer(42));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn error_classification_syntax() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // Send a query with syntax error
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("NOT VALID CYPHER!!!".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let run_resp = client.recv_message().await;

    match &run_resp {
        Value::Structure { tag: 0x7F, fields } => {
            if let Some(Value::Map(meta)) = fields.first() {
                let code = meta.iter().find(|(k, _)| k == "code").map(|(_, v)| v);
                assert_eq!(
                    code,
                    Some(&Value::String(
                        "Neo.ClientError.Statement.SyntaxError".into()
                    )),
                    "syntax errors should be classified as SyntaxError"
                );
            }
        }
        _ => panic!("expected FAILURE, got {run_resp:?}"),
    }

    let _ = shutdown.send(());
}

#[tokio::test]
async fn struct_8_roundtrip() {
    // Test that STRUCT_8 encode/decode works (structures with >15 fields)
    use traverse_server::packstream::{self, Value};

    let fields: Vec<Value> = (0..20).map(Value::Integer).collect();
    let original = Value::Structure {
        tag: 0xFF,
        fields: fields.clone(),
    };

    let mut buf = Vec::new();
    packstream::encode_value(&mut buf, &original);

    // Should use STRUCT_8 marker (0xDC)
    assert_eq!(buf[0], 0xDC, "should use STRUCT_8 for 20 fields");
    assert_eq!(buf[1], 20, "field count should be 20");
    assert_eq!(buf[2], 0xFF, "tag should be 0xFF");

    let mut pos = 0;
    let decoded = packstream::decode_value(&buf, &mut pos).unwrap();
    assert_eq!(decoded, original);
    assert_eq!(pos, buf.len());
}

#[tokio::test]
async fn begin_with_mode() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // BEGIN with mode="r" (read intent for routing)
    client
        .send_message(&Value::Structure {
            tag: 0x11,
            fields: vec![Value::Map(vec![("mode".into(), Value::String("r".into()))])],
        })
        .await;
    let begin_resp = client.recv_message().await;
    assert!(
        matches!(&begin_resp, Value::Structure { tag: 0x70, .. }),
        "BEGIN with mode should succeed"
    );

    // ROLLBACK
    client
        .send_message(&Value::Structure {
            tag: 0x13,
            fields: vec![],
        })
        .await;
    let _ = client.recv_message().await;

    client.goodbye().await;
    let _ = shutdown.send(());
}

// -- Notification tests -----------------------------------------------

#[tokio::test]
async fn notification_unknown_label() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // Query for a label that doesn't exist in the graph
    let (_, _, pull_meta) = client
        .run_query("MATCH (n:NonExistentLabel) RETURN n", vec![])
        .await;

    let meta = get_metadata(&pull_meta);
    let notifications = metadata_get(meta, "notifications");
    assert!(notifications.is_some(), "should have notifications field");

    if let Some(Value::List(notifs)) = notifications {
        // Should have at least one notification about unknown label
        assert!(
            !notifs.is_empty(),
            "should have notification for unknown label"
        );
        // Check first notification structure
        if let Value::Map(n) = &notifs[0] {
            let code = n.iter().find(|(k, _)| k == "code").map(|(_, v)| v);
            assert_eq!(
                code,
                Some(&Value::String(
                    "Neo.ClientNotification.Statement.UnknownLabelWarning".into()
                ))
            );
            let severity = n.iter().find(|(k, _)| k == "severity").map(|(_, v)| v);
            assert_eq!(severity, Some(&Value::String("WARNING".into())));
            let category = n.iter().find(|(k, _)| k == "category").map(|(_, v)| v);
            assert_eq!(category, Some(&Value::String("HINT".into())));
        } else {
            panic!("notification should be a Map");
        }
    } else {
        panic!("notifications should be a List");
    }

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn notification_cartesian_product() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // Create some data first so labels exist
    client
        .run_query("CREATE (:A {x: 1}), (:B {y: 2})", vec![])
        .await;

    // Cartesian product query: two disconnected MATCH patterns
    let (_, _, pull_meta) = client
        .run_query("MATCH (a:A) MATCH (b:B) RETURN a, b", vec![])
        .await;

    let meta = get_metadata(&pull_meta);
    let notifications = metadata_get(meta, "notifications");

    if let Some(Value::List(notifs)) = notifications {
        let has_cartesian = notifs.iter().any(|n| {
            if let Value::Map(m) = n {
                m.iter().any(|(k, v)| {
                    k == "code"
                        && v == &Value::String(
                            "Neo.ClientNotification.Statement.CartesianProductWarning".into(),
                        )
                })
            } else {
                false
            }
        });
        assert!(
            has_cartesian,
            "should have CartesianProductWarning notification"
        );
    } else {
        panic!("notifications should be a List");
    }

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn notification_no_warnings_for_simple_query() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // Simple query with no issues
    let (_, _, pull_meta) = client.run_query("RETURN 1 AS n", vec![]).await;

    let meta = get_metadata(&pull_meta);
    let notifications = metadata_get(meta, "notifications");
    assert!(
        notifications.is_none() || notifications == Some(&Value::List(vec![])),
        "simple query should have no notifications"
    );

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn notification_settings_disable_all() {
    let (addr, shutdown) = start_server().await;

    // Connect raw and send HELLO with notifications_minimum_severity: OFF
    let mut client = TestClient::connect_raw(addr).await;

    // HELLO with notifications OFF
    client
        .send_message(&Value::Structure {
            tag: 0x01,
            fields: vec![Value::Map(vec![
                ("user_agent".into(), Value::String("test/1.0".into())),
                (
                    "notifications_minimum_severity".into(),
                    Value::String("OFF".into()),
                ),
            ])],
        })
        .await;
    let _ = client.recv_message().await;

    // LOGON
    client
        .send_message(&Value::Structure {
            tag: 0x6A,
            fields: vec![Value::Map(vec![(
                "scheme".into(),
                Value::String("none".into()),
            )])],
        })
        .await;
    let _ = client.recv_message().await;

    // Query with unknown label (would normally generate a notification)
    let (_, _, pull_meta) = client
        .run_query("MATCH (n:DisabledNotifLabel) RETURN n", vec![])
        .await;

    let meta = get_metadata(&pull_meta);
    let notifications = metadata_get(meta, "notifications");
    assert!(
        notifications.is_none() || notifications == Some(&Value::List(vec![])),
        "notifications should be empty when severity is OFF"
    );

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn notification_bookmark_enforcement() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // Write data and get a bookmark
    let (_, _, pull_meta) = client
        .run_query("CREATE (:BkEnforce {val: 1})", vec![])
        .await;
    let meta = get_metadata(&pull_meta);
    let bookmark = metadata_get(meta, "bookmark").unwrap().clone();

    // BEGIN with the bookmark - should succeed (causal consistency satisfied)
    client
        .send_message(&Value::Structure {
            tag: 0x11,
            fields: vec![Value::Map(vec![(
                "bookmarks".into(),
                Value::List(vec![bookmark]),
            )])],
        })
        .await;
    let begin_resp = client.recv_message().await;
    assert!(
        matches!(&begin_resp, Value::Structure { tag: 0x70, .. }),
        "BEGIN with valid bookmark should succeed"
    );

    // Verify the data is visible
    let (_, records, _) = client
        .run_query("MATCH (n:BkEnforce) RETURN n.val", vec![])
        .await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0][0], Value::Integer(1));

    // ROLLBACK
    client
        .send_message(&Value::Structure {
            tag: 0x13,
            fields: vec![],
        })
        .await;
    let _ = client.recv_message().await;

    client.goodbye().await;
    let _ = shutdown.send(());
}

// -- Protocol compliance tests ----------------------------------------

#[tokio::test]
async fn hello_includes_patch_bolt() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect_raw(addr).await;

    // HELLO
    client
        .send_message(&Value::Structure {
            tag: 0x01,
            fields: vec![Value::Map(vec![(
                "user_agent".into(),
                Value::String("test/1.0".into()),
            )])],
        })
        .await;
    let hello_resp = client.recv_message().await;
    let meta = get_metadata(&hello_resp);
    let patch_bolt = metadata_get(meta, "patch_bolt");
    assert_eq!(
        patch_bolt,
        Some(&Value::List(vec![])),
        "HELLO SUCCESS should include patch_bolt"
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn db_ping_returns_empty_result() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // RUN db.ping()
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("CALL db.ping()".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let run_resp = client.recv_message().await;
    assert!(
        matches!(&run_resp, Value::Structure { tag: 0x70, .. }),
        "db.ping() RUN should succeed, got {run_resp:?}"
    );
    let run_meta = get_metadata(&run_resp);
    let fields = metadata_get(run_meta, "fields");
    assert_eq!(
        fields,
        Some(&Value::List(vec![])),
        "db.ping() should return empty fields"
    );

    // PULL
    client
        .send_message(&Value::Structure {
            tag: 0x3F,
            fields: vec![Value::Map(vec![("n".into(), Value::Integer(-1))])],
        })
        .await;
    let pull_resp = client.recv_message().await;
    assert!(
        matches!(&pull_resp, Value::Structure { tag: 0x70, .. }),
        "db.ping() PULL should succeed (no records), got {pull_resp:?}"
    );

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn unknown_db_rejected() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // RUN with unknown database name
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("RETURN 1".into()),
                Value::Map(vec![]),
                Value::Map(vec![("db".into(), Value::String("nonexistent".into()))]),
            ],
        })
        .await;
    let run_resp = client.recv_message().await;
    match &run_resp {
        Value::Structure { tag: 0x7F, fields } => {
            if let Some(Value::Map(meta)) = fields.first() {
                let code = meta.iter().find(|(k, _)| k == "code").map(|(_, v)| v);
                assert_eq!(
                    code,
                    Some(&Value::String(
                        "Neo.ClientError.Database.DatabaseNotFound".into()
                    )),
                    "unknown db should produce DatabaseNotFound"
                );
            }
        }
        _ => panic!("expected FAILURE for unknown db, got {run_resp:?}"),
    }

    let _ = shutdown.send(());
}

#[tokio::test]
async fn contains_updates_in_write_pull() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // Write query
    let (_, _, pull_meta) = client
        .run_query("CREATE (:ContainsUpdates {v: 1})", vec![])
        .await;
    let meta = get_metadata(&pull_meta);
    let cu = metadata_get(meta, "contains_updates");
    assert_eq!(
        cu,
        Some(&Value::Bool(true)),
        "write query PULL should have contains_updates=true"
    );

    // Read query should not have contains_updates
    let (_, _, pull_meta2) = client.run_query("RETURN 42", vec![]).await;
    let meta2 = get_metadata(&pull_meta2);
    let cu2 = metadata_get(meta2, "contains_updates");
    assert!(
        cu2.is_none(),
        "read query should not have contains_updates, got {cu2:?}"
    );

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn logon_failure_sets_failed_state() {
    let mut creds = std::collections::HashMap::new();
    creds.insert("neo4j".to_string(), "password".to_string());
    let (addr, shutdown) = start_server_with_auth(creds).await;

    let mut client = TestClient::connect_raw(addr).await;

    // HELLO
    client
        .send_message(&Value::Structure {
            tag: 0x01,
            fields: vec![Value::Map(vec![(
                "user_agent".into(),
                Value::String("test/1.0".into()),
            )])],
        })
        .await;
    let _ = client.recv_message().await;

    // LOGON with wrong password -> FAILURE
    client
        .send_message(&Value::Structure {
            tag: 0x6A,
            fields: vec![Value::Map(vec![
                ("scheme".into(), Value::String("basic".into())),
                ("principal".into(), Value::String("neo4j".into())),
                ("credentials".into(), Value::String("wrong".into())),
            ])],
        })
        .await;
    let logon_resp = client.recv_message().await;
    assert!(
        matches!(&logon_resp, Value::Structure { tag: 0x7F, .. }),
        "wrong password should fail"
    );

    // Subsequent RUN should be IGNORED (Failed state)
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("RETURN 1".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let run_resp = client.recv_message().await;
    assert!(
        matches!(&run_resp, Value::Structure { tag: 0x7E, .. }),
        "RUN after auth failure should be IGNORED, got {run_resp:?}"
    );

    // RESET should recover
    client
        .send_message(&Value::Structure {
            tag: 0x0F,
            fields: vec![],
        })
        .await;
    let reset_resp = client.recv_message().await;
    assert!(
        matches!(&reset_resp, Value::Structure { tag: 0x70, .. }),
        "RESET should succeed after auth failure"
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn is_write_query_word_boundaries() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // "PRESET" contains "SET" but isn't a write: is_write_query must match on word boundaries
    let (_, records, _) = client.run_query("RETURN 'PRESET' AS x", vec![]).await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0][0], Value::String("PRESET".into()));

    client.goodbye().await;
    let _ = shutdown.send(());
}

// -- state machine -----------------------------------------------------

#[tokio::test]
async fn commit_from_tx_streaming() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // BEGIN
    client
        .send_message(&Value::Structure {
            tag: 0x11,
            fields: vec![Value::Map(vec![])],
        })
        .await;
    let _ = client.recv_message().await;

    // RUN inside transaction (don't PULL - stay in TxStreaming)
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("CREATE (:TxStreamCommit {val: 1})".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let run_resp = client.recv_message().await;
    assert!(matches!(&run_resp, Value::Structure { tag: 0x70, .. }));

    // COMMIT directly from TxStreaming (without PULL)
    client
        .send_message(&Value::Structure {
            tag: 0x12, // COMMIT
            fields: vec![],
        })
        .await;
    let commit_resp = client.recv_message().await;
    assert!(
        matches!(&commit_resp, Value::Structure { tag: 0x70, .. }),
        "COMMIT from TxStreaming should succeed, got {commit_resp:?}"
    );

    // Verify data persisted
    let (_, records, _) = client
        .run_query("MATCH (n:TxStreamCommit) RETURN n.val", vec![])
        .await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0][0], Value::Integer(1));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn rollback_from_tx_streaming() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // BEGIN
    client
        .send_message(&Value::Structure {
            tag: 0x11,
            fields: vec![Value::Map(vec![])],
        })
        .await;
    let _ = client.recv_message().await;

    // RUN inside transaction (don't PULL - stay in TxStreaming)
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("CREATE (:TxStreamRollback {val: 1})".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let _ = client.recv_message().await;

    // ROLLBACK directly from TxStreaming
    client
        .send_message(&Value::Structure {
            tag: 0x13, // ROLLBACK
            fields: vec![],
        })
        .await;
    let rollback_resp = client.recv_message().await;
    assert!(
        matches!(&rollback_resp, Value::Structure { tag: 0x70, .. }),
        "ROLLBACK from TxStreaming should succeed, got {rollback_resp:?}"
    );

    // Verify data was NOT persisted
    let (_, records, _) = client
        .run_query("MATCH (n:TxStreamRollback) RETURN n", vec![])
        .await;
    assert_eq!(records.len(), 0);

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn reset_after_auth_failure_stays_in_auth() {
    let mut creds = std::collections::HashMap::new();
    creds.insert("neo4j".to_string(), "password".to_string());
    let (addr, shutdown) = start_server_with_auth(creds).await;

    let mut client = TestClient::connect_raw(addr).await;

    // HELLO
    client
        .send_message(&Value::Structure {
            tag: 0x01,
            fields: vec![Value::Map(vec![(
                "user_agent".into(),
                Value::String("test/1.0".into()),
            )])],
        })
        .await;
    let _ = client.recv_message().await;

    // LOGON with wrong password -> FAILURE
    client
        .send_message(&Value::Structure {
            tag: 0x6A,
            fields: vec![Value::Map(vec![
                ("scheme".into(), Value::String("basic".into())),
                ("principal".into(), Value::String("neo4j".into())),
                ("credentials".into(), Value::String("wrong".into())),
            ])],
        })
        .await;
    let logon_resp = client.recv_message().await;
    assert!(matches!(&logon_resp, Value::Structure { tag: 0x7F, .. }));

    // RESET should go back to Authentication (not Ready)
    client
        .send_message(&Value::Structure {
            tag: 0x0F,
            fields: vec![],
        })
        .await;
    let reset_resp = client.recv_message().await;
    assert!(matches!(&reset_resp, Value::Structure { tag: 0x70, .. }));

    // Should NOT be in Ready - RUN should fail (still in Authentication)
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("RETURN 1".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let run_resp = client.recv_message().await;
    assert!(
        matches!(&run_resp, Value::Structure { tag: 0x7F, .. }),
        "RUN after RESET from auth failure should fail (still in Authentication), got {run_resp:?}"
    );

    // RESET again to clear Failed state from the RUN rejection
    client
        .send_message(&Value::Structure {
            tag: 0x0F,
            fields: vec![],
        })
        .await;
    let _ = client.recv_message().await;

    // Now LOGON with correct credentials should work
    client
        .send_message(&Value::Structure {
            tag: 0x6A,
            fields: vec![Value::Map(vec![
                ("scheme".into(), Value::String("basic".into())),
                ("principal".into(), Value::String("neo4j".into())),
                ("credentials".into(), Value::String("password".into())),
            ])],
        })
        .await;
    let logon_resp2 = client.recv_message().await;
    assert!(
        matches!(&logon_resp2, Value::Structure { tag: 0x70, .. }),
        "LOGON with correct credentials after RESET should succeed, got {logon_resp2:?}"
    );

    // Now queries should work
    let (_, records, _) = client.run_query("RETURN 42 AS n", vec![]).await;
    assert_eq!(records[0][0], Value::Integer(42));

    client.goodbye().await;
    let _ = shutdown.send(());
}

// -- Handshake first-match test ---------------------------------------

#[tokio::test]
async fn handshake_first_match_priority() {
    let (addr, shutdown) = start_server().await;

    let stream = TcpStream::connect(addr).await.unwrap();
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(write_half);

    // Send magic
    writer.write_all(&BOLT_MAGIC).await.unwrap();

    // Propose Bolt 5.4 first (even though 6.0 is also offered later)
    writer.write_u32(0x0000_0405).await.unwrap();
    writer.write_u32(0x0000_0006).await.unwrap();
    writer.write_u32(0).await.unwrap();
    writer.write_u32(0).await.unwrap();
    writer.flush().await.unwrap();

    // Should negotiate 5.4 (first match) not 6.0 (highest)
    let mut resp = [0u8; 4];
    reader.read_exact(&mut resp).await.unwrap();
    assert_eq!(
        resp,
        [0, 0, 4, 5],
        "should pick first-match (5.4), not highest (6.0)"
    );

    let _ = shutdown.send(());
}

// -- Procedure call tests ---------------------------------------------

#[tokio::test]
async fn call_dbms_components() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    let (run_meta, records, _) = client.run_query("CALL dbms.components()", vec![]).await;

    let meta = get_metadata(&run_meta);
    let fields = metadata_get(meta, "fields").unwrap();
    assert_eq!(
        fields,
        &Value::List(vec![
            Value::String("name".into()),
            Value::String("versions".into()),
            Value::String("edition".into()),
        ])
    );

    assert_eq!(records.len(), 1);
    assert_eq!(records[0][0], Value::String("Traverse".into()));
    match &records[0][1] {
        Value::List(v) => assert!(!v.is_empty()),
        _ => panic!("versions should be a List"),
    }
    assert_eq!(records[0][2], Value::String("community".into()));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn call_db_labels() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    client
        .run_query("CREATE (:LabelA {x: 1}), (:LabelB {y: 2})", vec![])
        .await;

    let (run_meta, records, _) = client.run_query("CALL db.labels()", vec![]).await;

    let meta = get_metadata(&run_meta);
    let fields = metadata_get(meta, "fields").unwrap();
    assert_eq!(fields, &Value::List(vec![Value::String("label".into())]));

    let labels: Vec<&Value> = records.iter().map(|r| &r[0]).collect();
    assert!(labels.contains(&&Value::String("LabelA".into())));
    assert!(labels.contains(&&Value::String("LabelB".into())));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn call_db_relationship_types() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    client
        .run_query("CREATE (:X)-[:KNOWS]->(:Y), (:X)-[:LIKES]->(:Y)", vec![])
        .await;

    let (_, records, _) = client
        .run_query("CALL db.relationshipTypes()", vec![])
        .await;

    let types: Vec<&Value> = records.iter().map(|r| &r[0]).collect();
    assert!(types.contains(&&Value::String("KNOWS".into())));
    assert!(types.contains(&&Value::String("LIKES".into())));

    client.goodbye().await;
    let _ = shutdown.send(());
}

#[tokio::test]
async fn call_db_property_keys() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    client
        .run_query("CREATE (:PropTest {name: 'Alice', age: 30})", vec![])
        .await;

    let (_, records, _) = client.run_query("CALL db.propertyKeys()", vec![]).await;

    let keys: Vec<&Value> = records.iter().map(|r| &r[0]).collect();
    assert!(keys.contains(&&Value::String("name".into())));
    assert!(keys.contains(&&Value::String("age".into())));

    client.goodbye().await;
    let _ = shutdown.send(());
}

// -- Mode enforcement test --------------------------------------------

#[tokio::test]
async fn mode_r_rejects_writes() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // BEGIN with mode:"r"
    client
        .send_message(&Value::Structure {
            tag: 0x11,
            fields: vec![Value::Map(vec![("mode".into(), Value::String("r".into()))])],
        })
        .await;
    let begin_resp = client.recv_message().await;
    assert!(matches!(&begin_resp, Value::Structure { tag: 0x70, .. }));

    // Write query should be rejected
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("CREATE (:ModeRTest {val: 1})".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let run_resp = client.recv_message().await;
    assert!(
        matches!(&run_resp, Value::Structure { tag: 0x7F, .. }),
        "write query in mode:r should be rejected, got {run_resp:?}"
    );

    // RESET to recover
    client
        .send_message(&Value::Structure {
            tag: 0x0F,
            fields: vec![],
        })
        .await;
    let _ = client.recv_message().await;

    let (_, records, _) = client.run_query("RETURN 1 AS n", vec![]).await;
    assert_eq!(records[0][0], Value::Integer(1));

    client.goodbye().await;
    let _ = shutdown.send(());
}

// -- ROUTE db validation test -----------------------------------------

#[tokio::test]
async fn route_validates_db() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // ROUTE with valid db
    client
        .send_message(&Value::Structure {
            tag: 0x66,
            fields: vec![
                Value::Map(vec![]),
                Value::List(vec![]),
                Value::String("traverse".into()),
            ],
        })
        .await;
    let route_resp = client.recv_message().await;
    assert!(
        matches!(&route_resp, Value::Structure { tag: 0x70, .. }),
        "ROUTE with valid db should succeed, got {route_resp:?}"
    );

    // ROUTE with unknown db
    client
        .send_message(&Value::Structure {
            tag: 0x66,
            fields: vec![
                Value::Map(vec![]),
                Value::List(vec![]),
                Value::String("nonexistent".into()),
            ],
        })
        .await;
    let route_resp2 = client.recv_message().await;
    assert!(
        matches!(&route_resp2, Value::Structure { tag: 0x7F, .. }),
        "ROUTE with unknown db should fail, got {route_resp2:?}"
    );

    let _ = shutdown.send(());
}

// -- Per-query notification scoping test ------------------------------

#[tokio::test]
async fn per_query_notification_settings_dont_persist() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // First query: override notifications to OFF
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("MATCH (n:NonExistentScopeTest) RETURN n".into()),
                Value::Map(vec![]),
                Value::Map(vec![(
                    "notifications_minimum_severity".into(),
                    Value::String("OFF".into()),
                )]),
            ],
        })
        .await;
    let _ = client.recv_message().await;

    // PULL
    client
        .send_message(&Value::Structure {
            tag: 0x3F,
            fields: vec![Value::Map(vec![("n".into(), Value::Integer(-1))])],
        })
        .await;
    let pull1 = client.recv_message().await;
    let meta1 = get_metadata(&pull1);
    let notifs1 = metadata_get(meta1, "notifications");
    assert!(
        notifs1.is_none() || notifs1 == Some(&Value::List(vec![])),
        "per-query OFF should suppress notifications"
    );

    // Second query: no notification override - connection-level default should apply
    let (_, _, pull2_meta) = client
        .run_query("MATCH (n:NonExistentScopeTest2) RETURN n", vec![])
        .await;
    let meta2 = get_metadata(&pull2_meta);
    let notifs2 = metadata_get(meta2, "notifications");
    if let Some(Value::List(notif_list)) = notifs2 {
        assert!(
            !notif_list.is_empty(),
            "connection-level notification settings should be restored after per-query override"
        );
    } else {
        panic!("should have notifications for unknown label after override expires");
    }

    client.goodbye().await;
    let _ = shutdown.send(());
}

/// Auto-commit RUN with mode:"r" rejects writes.
#[tokio::test]
async fn auto_commit_mode_r_rejects_writes() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;

    // RUN CREATE with mode:"r" in extras (auto-commit)
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("CREATE (n:Test) RETURN n".into()),
                Value::Map(vec![]),
                Value::Map(vec![("mode".into(), Value::String("r".into()))]),
            ],
        })
        .await;
    let resp = client.recv_message().await;
    // Should be FAILURE
    match &resp {
        Value::Structure { tag, fields } => {
            assert_eq!(*tag, 0x7F, "expected FAILURE for write in mode:r");
            let meta = match fields.first() {
                Some(Value::Map(m)) => m.as_slice(),
                _ => &[],
            };
            let code = metadata_get(meta, "code").and_then(|v| v.as_str()).unwrap();
            assert!(
                code.contains("AccessMode"),
                "expected AccessMode error, got: {code}"
            );
        }
        _ => panic!("expected structure"),
    }

    // RESET to clear failed state
    client
        .send_message(&Value::Structure {
            tag: 0x0F,
            fields: vec![],
        })
        .await;
    let _ = client.recv_message().await;

    // Auto-commit RUN with mode:"r" for a read query should succeed
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("RETURN 1 AS n".into()),
                Value::Map(vec![]),
                Value::Map(vec![("mode".into(), Value::String("r".into()))]),
            ],
        })
        .await;
    let run_resp = client.recv_message().await;
    match &run_resp {
        Value::Structure { tag, .. } => {
            assert_eq!(*tag, 0x70, "expected SUCCESS for read in mode:r");
        }
        _ => panic!("expected structure"),
    }

    // PULL the result
    client
        .send_message(&Value::Structure {
            tag: 0x3F,
            fields: vec![Value::Map(vec![("n".into(), Value::Integer(-1))])],
        })
        .await;
    let _ = client.recv_message().await; // RECORD
    let _ = client.recv_message().await; // SUCCESS

    // after the auto-commit completes mode:"r" is cleared, so a later write without it succeeds
    let (_, _, _) = client
        .run_query("CREATE (n:ModeRCleanup) RETURN n", vec![])
        .await;

    client.goodbye().await;
    let _ = shutdown.send(());
}

/// HELLO's server version is CARGO_PKG_VERSION.
#[tokio::test]
async fn hello_server_version_matches_cargo_version() {
    let (addr, shutdown) = start_server().await;

    // Connect raw and do HELLO manually to inspect the response.
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.set_nodelay(true).unwrap();
    let (reader, writer) = tokio::io::split(stream);
    let mut reader = tokio::io::BufReader::new(reader);
    let mut writer = tokio::io::BufWriter::new(writer);

    // Handshake
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    writer.write_all(&[0x60, 0x60, 0xB0, 0x17]).await.unwrap();
    writer.write_u32(0x0000_0006).await.unwrap();
    writer.write_u32(0x0004_0805).await.unwrap();
    writer.write_u32(0).await.unwrap();
    writer.write_u32(0).await.unwrap();
    writer.flush().await.unwrap();
    let mut ver = [0u8; 4];
    reader.read_exact(&mut ver).await.unwrap();

    // Send HELLO
    let hello = Value::Structure {
        tag: 0x01,
        fields: vec![Value::Map(vec![(
            "user_agent".into(),
            Value::String("version-test/1.0".into()),
        )])],
    };
    let mut buf = Vec::new();
    traverse_server::packstream::encode_value(&mut buf, &hello);
    writer.write_u16(buf.len() as u16).await.unwrap();
    writer.write_all(&buf).await.unwrap();
    writer.write_u16(0).await.unwrap();
    writer.flush().await.unwrap();

    // Read response
    let mut msg = Vec::new();
    loop {
        let chunk_size = reader.read_u16().await.unwrap() as usize;
        if chunk_size == 0 {
            break;
        }
        let start = msg.len();
        msg.resize(start + chunk_size, 0);
        reader.read_exact(&mut msg[start..]).await.unwrap();
    }
    let mut pos = 0;
    let resp = traverse_server::packstream::decode_value(&msg, &mut pos).unwrap();

    let meta = get_metadata(&resp);
    let server = metadata_get(meta, "server")
        .and_then(|v| v.as_str())
        .expect("missing server in HELLO response");

    let expected = concat!("Traverse/", env!("CARGO_PKG_VERSION"));
    assert_eq!(
        server, expected,
        "server version should match CARGO_PKG_VERSION"
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn bolt_5_1_handshake_negotiation() {
    let (addr, shutdown) = start_server().await;

    let stream = TcpStream::connect(addr).await.unwrap();
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(write_half);

    // Send magic
    writer.write_all(&BOLT_MAGIC).await.unwrap();
    // Propose Bolt 5.1 only: major=5, minor=1, range=0
    writer.write_u32(0x0000_0105).await.unwrap();
    writer.write_u32(0).await.unwrap();
    writer.write_u32(0).await.unwrap();
    writer.write_u32(0).await.unwrap();
    writer.flush().await.unwrap();

    // Read back negotiated version - should be 5.1
    let mut resp = [0u8; 4];
    reader.read_exact(&mut resp).await.unwrap();
    assert_eq!(resp, [0, 0, 1, 5], "expected Bolt 5.1 negotiated");

    // HELLO should succeed on 5.1
    let hello = Value::Structure {
        tag: 0x01,
        fields: vec![Value::Map(vec![(
            "user_agent".into(),
            Value::String("bolt51-test/1.0".into()),
        )])],
    };
    let mut buf = Vec::new();
    packstream::encode_value(&mut buf, &hello);
    framing::write_message(&mut writer, &buf).await.unwrap();
    writer.flush().await.unwrap();

    let resp_bytes = framing::read_message(&mut reader).await.unwrap();
    let mut pos = 0;
    let resp_val = packstream::decode_value(&resp_bytes, &mut pos).unwrap();
    assert!(
        matches!(&resp_val, Value::Structure { tag: 0x70, .. }),
        "expected SUCCESS after HELLO on Bolt 5.1, got {resp_val:?}"
    );

    let _ = shutdown.send(());
}

#[tokio::test]
async fn bolt_5_2_handshake_negotiation() {
    let (addr, shutdown) = start_server().await;

    let stream = TcpStream::connect(addr).await.unwrap();
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(write_half);

    // Send magic
    writer.write_all(&BOLT_MAGIC).await.unwrap();
    // Propose Bolt 5.2 only: major=5, minor=2, range=0
    writer.write_u32(0x0000_0205).await.unwrap();
    writer.write_u32(0).await.unwrap();
    writer.write_u32(0).await.unwrap();
    writer.write_u32(0).await.unwrap();
    writer.flush().await.unwrap();

    // Read back negotiated version - should be 5.2
    let mut resp = [0u8; 4];
    reader.read_exact(&mut resp).await.unwrap();
    assert_eq!(resp, [0, 0, 2, 5], "expected Bolt 5.2 negotiated");

    // HELLO + LOGON should succeed on 5.2
    let hello = Value::Structure {
        tag: 0x01,
        fields: vec![Value::Map(vec![(
            "user_agent".into(),
            Value::String("bolt52-test/1.0".into()),
        )])],
    };
    let mut buf = Vec::new();
    packstream::encode_value(&mut buf, &hello);
    framing::write_message(&mut writer, &buf).await.unwrap();
    writer.flush().await.unwrap();

    let resp_bytes = framing::read_message(&mut reader).await.unwrap();
    let mut pos = 0;
    let resp_val = packstream::decode_value(&resp_bytes, &mut pos).unwrap();
    assert!(
        matches!(&resp_val, Value::Structure { tag: 0x70, .. }),
        "expected SUCCESS after HELLO on Bolt 5.2, got {resp_val:?}"
    );

    // LOGON
    let logon = Value::Structure {
        tag: 0x6A,
        fields: vec![Value::Map(vec![(
            "scheme".into(),
            Value::String("none".into()),
        )])],
    };
    buf.clear();
    packstream::encode_value(&mut buf, &logon);
    framing::write_message(&mut writer, &buf).await.unwrap();
    writer.flush().await.unwrap();

    let resp_bytes = framing::read_message(&mut reader).await.unwrap();
    pos = 0;
    let resp_val = packstream::decode_value(&resp_bytes, &mut pos).unwrap();
    assert!(
        matches!(&resp_val, Value::Structure { tag: 0x70, .. }),
        "expected SUCCESS after LOGON on Bolt 5.2, got {resp_val:?}"
    );

    let _ = shutdown.send(());
}

// -- Optimistic explicit transactions ---------------------------------

impl TestClient {
    async fn begin(&mut self, extra: Vec<(String, Value)>) {
        self.send_message(&Value::Structure {
            tag: 0x11,
            fields: vec![Value::Map(extra)],
        })
        .await;
        let resp = self.recv_message().await;
        assert!(
            matches!(&resp, Value::Structure { tag: 0x70, .. }),
            "BEGIN: {resp:?}"
        );
    }

    async fn commit(&mut self) -> Value {
        self.send_message(&Value::Structure {
            tag: 0x12,
            fields: vec![],
        })
        .await;
        self.recv_message().await
    }

    async fn reset(&mut self) {
        self.send_message(&Value::Structure {
            tag: 0x0F,
            fields: vec![],
        })
        .await;
        let _ = self.recv_message().await;
    }
}

/// `code` of a FAILURE message.
fn failure_code(v: &Value) -> Option<String> {
    match v {
        Value::Structure { tag: 0x7F, fields } => match fields.first() {
            Some(Value::Map(m)) => metadata_get(m, "code").and_then(|c| match c {
                Value::String(s) => Some(s.clone()),
                _ => None,
            }),
            _ => None,
        },
        _ => None,
    }
}

#[tokio::test]
async fn concurrent_commits_on_one_node_conflict_and_retry_wins() {
    let (addr, shutdown) = start_server().await;
    let mut setup = TestClient::connect(addr).await;
    setup.run_query("CREATE (:Counter {n: 0})", vec![]).await;
    let mut a = TestClient::connect(addr).await;
    let mut b = TestClient::connect(addr).await;
    a.begin(vec![]).await;
    b.begin(vec![]).await;
    a.run_query("MATCH (c:Counter) SET c.n = c.n + 1", vec![])
        .await;
    b.run_query("MATCH (c:Counter) SET c.n = c.n + 1", vec![])
        .await;
    let ca = a.commit().await;
    assert!(matches!(&ca, Value::Structure { tag: 0x70, .. }), "{ca:?}");
    let cb = b.commit().await;
    assert_eq!(
        failure_code(&cb).as_deref(),
        Some("Neo.TransientError.Transaction.Outdated"),
        "{cb:?}"
    );
    // what a driver does with a transient error: reset, retry the whole transaction
    b.reset().await;
    b.begin(vec![]).await;
    b.run_query("MATCH (c:Counter) SET c.n = c.n + 1", vec![])
        .await;
    let cb = b.commit().await;
    assert!(matches!(&cb, Value::Structure { tag: 0x70, .. }), "{cb:?}");
    let (_, rows, _) = setup
        .run_query("MATCH (c:Counter) RETURN c.n", vec![])
        .await;
    assert_eq!(rows[0][0], Value::Integer(2), "no update lost");
    let _ = shutdown.send(());
}

#[tokio::test]
async fn read_only_transaction_refuses_writes() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;
    client
        .begin(vec![("mode".into(), Value::String("r".into()))])
        .await;
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("CREATE (:X)".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let resp = client.recv_message().await;
    assert_eq!(
        failure_code(&resp).as_deref(),
        Some("Neo.ClientError.Statement.AccessMode"),
        "{resp:?}"
    );
    client.reset().await;
    let (_, rows, _) = client.run_query("MATCH (n) RETURN count(n)", vec![]).await;
    assert_eq!(rows[0][0], Value::Integer(0));
    let _ = shutdown.send(());
}

#[tokio::test]
async fn open_transaction_blocks_no_writer_and_keeps_its_snapshot() {
    let (addr, shutdown) = start_server().await;
    let mut a = TestClient::connect(addr).await;
    let mut b = TestClient::connect(addr).await;
    a.begin(vec![]).await;
    a.run_query("CREATE (:Mine)", vec![]).await;
    let (_, rows, _) = a.run_query("MATCH (n) RETURN count(n)", vec![]).await;
    assert_eq!(rows[0][0], Value::Integer(1), "reads its own write");
    // another connection writes while the transaction is open, without waiting for it
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        b.run_query("CREATE (:Theirs)", vec![]),
    )
    .await
    .expect("auto-commit write blocked behind an open transaction");
    let (_, rows, _) = a.run_query("MATCH (n) RETURN count(n)", vec![]).await;
    assert_eq!(
        rows[0][0],
        Value::Integer(1),
        "the transaction keeps its snapshot"
    );
    let resp = a.commit().await;
    assert!(
        matches!(&resp, Value::Structure { tag: 0x70, .. }),
        "{resp:?}"
    );
    let (_, rows, _) = b.run_query("MATCH (n) RETURN count(n)", vec![]).await;
    assert_eq!(rows[0][0], Value::Integer(2));
    let _ = shutdown.send(());
}

// -- stopping statements: TERMINATE TRANSACTIONS, RESET, tx_timeout, disconnects --

/// A three-way cross product over 2,000 nodes: 8e9 rows, minutes of work unless stopped. Tests run
/// in parallel against one process-wide transaction registry, so each names its own column and
/// finds its own statement by text.
fn long_query(tag: &str) -> String {
    format!("MATCH (a:Big), (b:Big), (c:Big) RETURN count(*) AS {tag}")
}

/// A server with 2,000 `:Big` nodes and a client connected to it.
async fn server_with_big_graph() -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Sender<()>,
    TestClient,
) {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;
    let (_, _, done) = client
        .run_query("UNWIND range(1, 2000) AS i CREATE (:Big {i: i})", vec![])
        .await;
    assert!(
        matches!(&done, Value::Structure { tag: 0x70, .. }),
        "{done:?}"
    );
    (addr, shutdown, client)
}

/// Sends RUN + PULL without waiting for the replies.
async fn start_pipelined(client: &mut TestClient, query: &str) {
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String(query.into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    client
        .send_message(&Value::Structure {
            tag: 0x3F,
            fields: vec![Value::Map(vec![("n".into(), Value::Integer(-1))])],
        })
        .await;
}

/// The id of the transaction running `query` (as SHOW TRANSACTIONS reports it), once it shows up.
async fn running_transaction(observer: &mut TestClient, query: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let (_, rows, _) = observer
            .run_query(
                "SHOW TRANSACTIONS YIELD transactionId, currentQuery \
                 WHERE currentQuery = $q RETURN transactionId",
                vec![("q".into(), Value::String(query.into()))],
            )
            .await;
        if let Some(Value::String(id)) = rows.first().and_then(|r| r.first()) {
            return id.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the statement never showed up in SHOW TRANSACTIONS"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// The next message, failing the test if none comes within 20 s (a statement that wasn't stopped).
async fn recv_within(client: &mut TestClient) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(20), client.recv_message())
        .await
        .expect("no reply: the statement wasn't stopped")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn terminate_transactions_stops_another_sessions_statement() {
    let query = long_query("terminate_me");
    let (addr, shutdown, mut observer) = server_with_big_graph().await;
    let mut victim = TestClient::connect(addr).await;
    start_pipelined(&mut victim, &query).await;

    let id = running_transaction(&mut observer, &query).await;
    let (_, rows, _) = observer
        .run_query(
            "TERMINATE TRANSACTIONS $id YIELD message",
            vec![("id".into(), Value::String(id))],
        )
        .await;
    assert_eq!(
        rows,
        vec![vec![Value::String("Transaction terminated.".into())]]
    );

    // RUN's SUCCESS goes out with the PULL's reply
    let run = recv_within(&mut victim).await;
    assert!(
        matches!(&run, Value::Structure { tag: 0x70, .. }),
        "{run:?}"
    );
    let pulled = recv_within(&mut victim).await;
    assert_eq!(
        failure_code(&pulled).as_deref(),
        Some("Neo.TransientError.Transaction.Terminated"),
        "{pulled:?}"
    );
    // the connection carries on after a RESET
    victim.reset().await;
    let (_, rows, _) = victim.run_query("RETURN 1 AS n", vec![]).await;
    assert_eq!(rows, vec![vec![Value::Integer(1)]]);
    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reset_interrupts_the_running_statement() {
    let query = long_query("reset_me");
    let (_addr, shutdown, mut client) = server_with_big_graph().await;
    start_pipelined(&mut client, &query).await;
    // let the statement get going, then interrupt it
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    client
        .send_message(&Value::Structure {
            tag: 0x0F,
            fields: vec![],
        })
        .await;
    // RUN's SUCCESS goes out with the PULL's reply: the interrupted PULL is IGNORED (the client
    // asked for that), then RESET succeeds
    let run = recv_within(&mut client).await;
    assert!(
        matches!(&run, Value::Structure { tag: 0x70, .. }),
        "{run:?}"
    );
    let pulled = recv_within(&mut client).await;
    assert!(
        matches!(&pulled, Value::Structure { tag: 0x7E, .. }),
        "{pulled:?}"
    );
    let reset = recv_within(&mut client).await;
    assert!(
        matches!(&reset, Value::Structure { tag: 0x70, .. }),
        "{reset:?}"
    );
    let (_, rows, _) = client.run_query("RETURN 2 AS n", vec![]).await;
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_tx_timeout_stops_an_explicit_transaction() {
    let query = long_query("time_me_out");
    let (_addr, shutdown, mut client) = server_with_big_graph().await;
    client
        .begin(vec![("tx_timeout".into(), Value::Integer(300))])
        .await;
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String(query.clone()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let run = recv_within(&mut client).await;
    assert_eq!(
        failure_code(&run).as_deref(),
        Some("Neo.ClientError.Transaction.TransactionTimedOutClientConfiguration"),
        "{run:?}"
    );
    client.reset().await;
    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_terminated_idle_transaction_fails_its_next_statement() {
    let (addr, shutdown) = start_server().await;
    let mut owner = TestClient::connect(addr).await;
    let mut admin = TestClient::connect(addr).await;
    owner
        .begin(vec![(
            "tx_metadata".into(),
            Value::Map(vec![("app".into(), Value::String("terminate-idle".into()))]),
        )])
        .await;
    owner.run_query("CREATE (:Pending)", vec![]).await;

    // the transaction is listed with its metadata
    let (_, rows, _) = admin
        .run_query(
            "SHOW TRANSACTIONS YIELD transactionId, metaData, protocol \
             WHERE metaData.app = 'terminate-idle' RETURN transactionId, protocol",
            vec![],
        )
        .await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0][1], Value::String("bolt".into()));
    let id = rows[0][0].clone();
    let (_, out, _) = admin
        .run_query(
            "TERMINATE TRANSACTIONS $id YIELD message",
            vec![("id".into(), id)],
        )
        .await;
    assert_eq!(out[0][0], Value::String("Transaction terminated.".into()));

    // its next statement fails and the write is gone
    owner
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("RETURN 1".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let run = recv_within(&mut owner).await;
    assert_eq!(
        failure_code(&run).as_deref(),
        Some("Neo.TransientError.Transaction.Terminated"),
        "{run:?}"
    );
    owner.reset().await;
    let (_, rows, _) = admin
        .run_query("MATCH (n:Pending) RETURN count(n)", vec![])
        .await;
    assert_eq!(rows[0][0], Value::Integer(0));
    let _ = shutdown.send(());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_the_connection_stops_its_statement() {
    let query = long_query("orphan_me");
    let (addr, shutdown, mut observer) = server_with_big_graph().await;
    let mut client = TestClient::connect(addr).await;
    start_pipelined(&mut client, &query).await;
    let _ = running_transaction(&mut observer, &query).await;
    drop(client);
    // the statement stops and its transaction leaves the registry
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let (_, rows, _) = observer
            .run_query(
                "SHOW TRANSACTIONS YIELD currentQuery WHERE currentQuery = $q \
                 RETURN count(*)",
                vec![("q".into(), Value::String(query.clone()))],
            )
            .await;
        if rows[0][0] == Value::Integer(0) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the statement kept running after its client went away"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let _ = shutdown.send(());
}

// -- CALL { ... } IN TRANSACTIONS: batches commit on their own --

#[tokio::test]
async fn call_in_transactions_rolls_back_a_failed_batch() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;
    let (_, rows, done) = client
        .run_query(
            "UNWIND [1, 0, 2, 3] AS i CALL { WITH i CREATE (:B {v: 10 / i}) } \
             IN TRANSACTIONS OF 1 ROW ON ERROR CONTINUE REPORT STATUS AS s \
             RETURN s.committed AS committed",
            vec![],
        )
        .await;
    assert!(
        matches!(&done, Value::Structure { tag: 0x70, .. }),
        "{done:?}"
    );
    assert_eq!(
        rows,
        vec![
            vec![Value::Bool(true)],
            vec![Value::Bool(false)],
            vec![Value::Bool(true)],
            vec![Value::Bool(true)],
        ]
    );
    let (_, rows, _) = client
        .run_query("MATCH (n:B) RETURN count(n)", vec![])
        .await;
    assert_eq!(rows[0][0], Value::Integer(3));
    let _ = shutdown.send(());
}

#[tokio::test]
async fn call_in_transactions_is_refused_in_an_explicit_transaction() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect(addr).await;
    client.begin(vec![]).await;
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String("UNWIND [1] AS i CALL { WITH i CREATE (:B) } IN TRANSACTIONS".into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let run = client.recv_message().await;
    assert!(failure_code(&run).is_some(), "{run:?}");
    client.reset().await;
    let _ = shutdown.send(());
}

// -- GQLSTATUS in FAILURE (Bolt 5.7+) --

/// The FAILURE metadata of `query` (at RUN or at PULL), then a RESET.
async fn failure_of(client: &mut TestClient, query: &str) -> Vec<(String, Value)> {
    client
        .send_message(&Value::Structure {
            tag: 0x10,
            fields: vec![
                Value::String(query.into()),
                Value::Map(vec![]),
                Value::Map(vec![]),
            ],
        })
        .await;
    let mut msg = client.recv_message().await;
    if matches!(&msg, Value::Structure { tag: 0x70, .. }) {
        client
            .send_message(&Value::Structure {
                tag: 0x3F,
                fields: vec![Value::Map(vec![("n".into(), Value::Integer(-1))])],
            })
            .await;
        loop {
            msg = client.recv_message().await;
            if !matches!(&msg, Value::Structure { tag: 0x71, .. }) {
                break;
            }
        }
    }
    let meta = match msg {
        Value::Structure { tag: 0x7F, fields } => match fields.into_iter().next() {
            Some(Value::Map(m)) => m,
            other => panic!("FAILURE without metadata: {other:?}"),
        },
        other => panic!("expected FAILURE for `{query}`, got {other:?}"),
    };
    client.reset().await;
    meta
}

#[tokio::test]
async fn failures_carry_the_gql_status_of_their_condition() {
    let (addr, shutdown) = start_server().await;
    let mut client = TestClient::connect_minor(addr, 7).await;
    for (query, neo4j, gql) in [
        (
            "RETURN 1 / 0",
            "Neo.ClientError.Statement.ArithmeticError",
            "22012",
        ),
        ("RETURN x", "Neo.ClientError.Statement.SyntaxError", "42N62"),
        (
            "CALL no.such.procedure()",
            "Neo.ClientError.Procedure.ProcedureNotFound",
            "42N08",
        ),
        (
            "RETURN $missing",
            "Neo.ClientError.Statement.ParameterMissing",
            "42N51",
        ),
    ] {
        let meta = failure_of(&mut client, query).await;
        assert_eq!(
            metadata_get(&meta, "neo4j_code"),
            Some(&Value::String(neo4j.into())),
            "{query}: {meta:?}"
        );
        assert_eq!(
            metadata_get(&meta, "gql_status"),
            Some(&Value::String(gql.into())),
            "{query}: {meta:?}"
        );
        assert!(
            matches!(metadata_get(&meta, "description"), Some(Value::String(d)) if d.starts_with("error: ")),
            "{meta:?}"
        );
    }
    let _ = shutdown.send(());
}
