use super::*;
use crate::graph::property::PropertyValue;
use crate::graph::types::Direction;
use crate::storage::format::{HEADER_SIZE, PAGE_HEADER_SIZE};
use std::collections::BTreeMap;

fn empty_props() -> BTreeMap<String, PropertyValue> {
    BTreeMap::new()
}

fn props(pairs: &[(&str, PropertyValue)]) -> BTreeMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

/// Helper: look up a property on a Node via the graph's string table.
fn node_prop(
    node: &crate::graph::node::Node,
    graph: &crate::graph::Graph,
    key: &str,
) -> Option<PropertyValue> {
    use crate::graph::types::PropertyKeyId;
    let kid = PropertyKeyId(graph.strings().get(key)?);
    node.properties.get(kid)
}

fn reopened_metadata(path: &Path) -> Vec<u8> {
    Database::open(path).unwrap().into_parts().2
}

#[test]
fn metadata_is_replaced_copy_on_write_and_leaves_no_log() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("m.tvdb");
    let mut g = Graph::new();
    g.create_node(&["N"], empty_props());
    Database::save(&path, &g, &[]).unwrap();
    Database::save_metadata(&path, b"first").unwrap();
    let before = std::fs::read(&path).unwrap();
    assert_eq!(reopened_metadata(&path), b"first");
    Database::save_metadata(&path, b"second").unwrap();
    assert_eq!(reopened_metadata(&path), b"second");
    // one new page after the others, which are untouched
    let after = std::fs::read(&path).unwrap();
    assert_eq!(
        after.len(),
        before.len() + crate::storage::format::PAGE_SIZE
    );
    assert_eq!(after[HEADER_SIZE..before.len()], before[HEADER_SIZE..]);
    assert!(!wal::wal_path(&path).exists());
    // the graph is untouched
    assert_eq!(Database::open(&path).unwrap().graph().node_count(), 1);
}

#[test]
fn a_metadata_write_cut_short_after_its_log_committed_is_completed_on_open() {
    use crate::storage::format::PAGE_SIZE;
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("m.tvdb");
    Database::save(&path, &Graph::new(), b"old").unwrap();
    // what save_metadata logs for "new", committed but never applied to the file
    let mut header = crate::storage::format::FileHeader::from_bytes(
        std::fs::read(&path).unwrap()[..HEADER_SIZE]
            .try_into()
            .unwrap(),
    )
    .unwrap();
    header.lsn += 1;
    let id = header.metadata_page_id;
    let mut page_buf = [0u8; PAGE_SIZE];
    let mut page = SlottedPage::new(&mut page_buf, id);
    page.init(PageType::Metadata);
    page.insert_record(&compression::encode_record(b"new"))
        .unwrap();
    page.set_lsn(header.lsn);
    page.write_checksum();
    let mut log = wal::WalWriter::create(&wal::wal_path(&path), header.lsn, header.lsn).unwrap();
    log.write_page(id, &page_buf).unwrap();
    log.write_header(&header.to_bytes()).unwrap();
    log.commit().unwrap();
    assert_eq!(reopened_metadata(&path), b"new");
}

#[test]
fn create_empty_close_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    {
        let db = Database::open(&path).unwrap();
        assert_eq!(db.graph().node_count(), 0);
        assert_eq!(db.graph().edge_count(), 0);
        db.close().unwrap();
    }

    {
        let db = Database::open(&path).unwrap();
        assert_eq!(db.graph().node_count(), 0);
        assert_eq!(db.graph().edge_count(), 0);
    }
}

#[test]
fn insert_nodes_close_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let alice_id;
    {
        let mut db = Database::open(&path).unwrap();
        alice_id = db.graph_mut().create_node(
            &["Person"],
            props(&[
                ("name", PropertyValue::String("Alice".into())),
                ("age", PropertyValue::Int(30)),
            ]),
        );
        db.graph_mut().create_node(
            &["Person"],
            props(&[("name", PropertyValue::String("Bob".into()))]),
        );
        db.close().unwrap();
    }

    {
        let db = Database::open(&path).unwrap();
        let g = db.graph();
        assert_eq!(g.node_count(), 2);
        let alice = g.get_node(alice_id).unwrap();
        assert_eq!(
            node_prop(alice, &g, "name").unwrap().as_str(),
            Some("Alice")
        );
        assert_eq!(node_prop(alice, &g, "age").unwrap().as_int(), Some(30));
        assert_eq!(g.nodes_by_label("Person").len(), 2);
    }
}

#[test]
fn insert_nodes_and_edges_traversal_works() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let (alice_id, bob_id);
    {
        let mut db = Database::open(&path).unwrap();
        alice_id = db.graph_mut().create_node(&["Person"], empty_props());
        bob_id = db.graph_mut().create_node(&["Person"], empty_props());
        db.graph_mut()
            .create_edge(alice_id, bob_id, "KNOWS", empty_props())
            .unwrap();
        db.close().unwrap();
    }

    {
        let db = Database::open(&path).unwrap();
        assert_eq!(db.graph().node_count(), 2);
        assert_eq!(db.graph().edge_count(), 1);
        let neighbors = db
            .graph()
            .neighbors(alice_id, Direction::Outgoing, Some("KNOWS"))
            .unwrap();
        assert_eq!(neighbors, vec![bob_id]);
    }
}

#[test]
fn all_property_value_variants_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let node_id;
    {
        let mut db = Database::open(&path).unwrap();
        let all_props = props(&[
            ("null", PropertyValue::Null),
            ("bool_t", PropertyValue::Bool(true)),
            ("bool_f", PropertyValue::Bool(false)),
            ("int", PropertyValue::Int(-42)),
            ("float", PropertyValue::Float(1.25)),
            ("string", PropertyValue::String("hello".into())),
            ("bytes", PropertyValue::Bytes(vec![1, 2, 3])),
            (
                "list",
                PropertyValue::List(vec![
                    PropertyValue::Int(1),
                    PropertyValue::String("two".into()),
                ]),
            ),
            (
                "map",
                PropertyValue::Map({
                    let mut m = BTreeMap::new();
                    m.insert("key".into(), PropertyValue::Bool(true));
                    m
                }),
            ),
            ("datetime", PropertyValue::DateTime(1700000000000)),
            ("duration", PropertyValue::Duration(60_000)),
            (
                "point2d",
                PropertyValue::Point2D {
                    x: 1.0,
                    y: 2.0,
                    srid: 4326,
                },
            ),
            (
                "point3d",
                PropertyValue::Point3D {
                    x: 1.0,
                    y: 2.0,
                    z: 3.0,
                    srid: 4326,
                },
            ),
            ("vector", PropertyValue::Vector(vec![0.1, 0.2, 0.3])),
        ]);
        node_id = db.graph_mut().create_node(&[], all_props.clone());
        db.close().unwrap();
    }

    {
        let db = Database::open(&path).unwrap();
        let g = db.graph();
        let node = g.get_node(node_id).unwrap();
        assert!(node_prop(node, &g, "null").unwrap().is_null());
        assert_eq!(node_prop(node, &g, "bool_t").unwrap().as_bool(), Some(true));
        assert_eq!(
            node_prop(node, &g, "bool_f").unwrap().as_bool(),
            Some(false)
        );
        assert_eq!(node_prop(node, &g, "int").unwrap().as_int(), Some(-42));
        assert_eq!(node_prop(node, &g, "float").unwrap().as_float(), Some(1.25));
        assert_eq!(
            node_prop(node, &g, "string").unwrap().as_str(),
            Some("hello")
        );
        assert!(matches!(
            node_prop(node, &g, "bytes").unwrap(),
            PropertyValue::Bytes(ref b) if b == &[1, 2, 3]
        ));
        assert!(matches!(
            node_prop(node, &g, "list").unwrap(),
            PropertyValue::List(ref l) if l.len() == 2
        ));
        assert!(matches!(
            node_prop(node, &g, "map").unwrap(),
            PropertyValue::Map(ref m) if m.len() == 1
        ));
        assert!(matches!(
            node_prop(node, &g, "datetime").unwrap(),
            PropertyValue::DateTime(1700000000000)
        ));
        assert!(matches!(
            node_prop(node, &g, "duration").unwrap(),
            PropertyValue::Duration(60_000)
        ));
        assert!(matches!(
            node_prop(node, &g, "point2d").unwrap(),
            PropertyValue::Point2D { x, y, srid } if x == 1.0 && y == 2.0 && srid == 4326
        ));
        assert!(matches!(
            node_prop(node, &g, "point3d").unwrap(),
            PropertyValue::Point3D { x, y, z, srid }
                if x == 1.0 && y == 2.0 && z == 3.0 && srid == 4326
        ));
        assert!(matches!(
            node_prop(node, &g, "vector").unwrap(),
            PropertyValue::Vector(ref v) if v.len() == 3
        ));
    }
}

#[test]
fn corrupt_page_detected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    {
        let mut db = Database::open(&path).unwrap();
        db.graph_mut().create_node(&["Person"], empty_props());
        db.close().unwrap();
    }

    // Corrupt a byte in the first page
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        // Corrupt a data byte in the first data page
        file.seek(SeekFrom::Start(
            HEADER_SIZE as u64 + PAGE_HEADER_SIZE as u64 + 10,
        ))
        .unwrap();
        file.write_all(&[0xFF]).unwrap();
        file.sync_all().unwrap();
    }

    let err = Database::open(&path).unwrap_err();
    assert!(
        matches!(err, Error::ChecksumMismatch { .. }),
        "expected ChecksumMismatch, got: {err:?}"
    );
}

#[test]
fn large_graph_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let node_count = 1000;
    let edge_count = 2000;

    {
        let mut db = Database::open(&path).unwrap();
        {
            let mut g = db.graph_mut();
            let mut node_ids = Vec::with_capacity(node_count);
            for i in 0..node_count {
                let id =
                    g.create_node(&["Node"], props(&[("index", PropertyValue::Int(i as i64))]));
                node_ids.push(id);
            }

            for i in 0..edge_count {
                let src = node_ids[i % node_count];
                let tgt = node_ids[(i + 1) % node_count];
                g.create_edge(src, tgt, "LINK", empty_props()).unwrap();
            }
        }
        db.close().unwrap();
    }

    {
        let db = Database::open(&path).unwrap();
        assert_eq!(db.graph().node_count(), node_count);
        assert_eq!(db.graph().edge_count(), edge_count);
    }
}

#[test]
fn rlog_recovers_writes_without_snapshot() {
    use crate::tx::view::GraphView;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash.tvdb");

    // phase 1: commit through WriteTx (logged), then "crash" without a snapshot
    {
        let mut db = Database::open(&path).unwrap();
        let mut tx = db.begin_write();
        let a = GraphView::create_node(
            &mut tx,
            &["Person"],
            props(&[("name", PropertyValue::String("Alice".into()))]),
        )
        .unwrap();
        let b = GraphView::create_node(&mut tx, &["Person"], empty_props()).unwrap();
        GraphView::create_edge(&mut tx, a, b, "KNOWS", empty_props()).unwrap();
        tx.commit().unwrap();
        db.set_suppress_flush(true); // simulate crash: no snapshot
    }
    // the .tvdb snapshot was never written...
    assert!(!path.exists() || std::fs::metadata(&path).unwrap().len() == 0);

    // phase 2: reopen, state must come from the rlog alone
    {
        let db = Database::open(&path).unwrap();
        let g = db.graph();
        assert_eq!(g.node_count(), 2, "nodes recovered from rlog");
        assert_eq!(g.edge_count(), 1, "edge recovered from rlog");
        assert_eq!(g.nodes_by_label("Person").len(), 2);
        drop(g);
        db_forget(db);
    }
}

#[test]
fn rlog_recovers_tail_after_snapshot() {
    use crate::tx::view::GraphView;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tail.tvdb");

    // batch 1: commit, flush -> snapshot with recovery_lsn watermark
    let (a, b) = {
        let mut db = Database::open(&path).unwrap();
        let (a, b) = {
            let mut tx = db.begin_write();
            let a = GraphView::create_node(&mut tx, &["A"], empty_props()).unwrap();
            let b = GraphView::create_node(&mut tx, &["B"], empty_props()).unwrap();
            tx.commit().unwrap();
            (a, b)
        };
        db.flush().unwrap(); // snapshot reflects batch 1; recovery_lsn advances

        // batch 2: commits after the snapshot (lsn > recovery_lsn), then crash
        {
            let mut tx = db.begin_write();
            let c = GraphView::create_node(&mut tx, &["C"], empty_props()).unwrap();
            GraphView::create_edge(&mut tx, a, c, "R", empty_props()).unwrap();
            tx.commit().unwrap();
        }
        db.set_suppress_flush(true);
        (a, b)
    };

    // reopen: snapshot gives batch 1, rlog tail gives batch 2
    {
        let db = Database::open(&path).unwrap();
        let g = db.graph();
        assert_eq!(g.node_count(), 3, "batch1 (snapshot) + batch2 (rlog tail)");
        assert_eq!(g.edge_count(), 1, "tail edge recovered");
        // IDs survive the snapshot/tail boundary: a,b from the snapshot, c got the next id,
        // edge is a->c
        assert!(g.nodes_by_label("A").contains(&a));
        assert!(g.nodes_by_label("B").contains(&b));
        assert_eq!(g.nodes_by_label("C").len(), 1);
        drop(g);
        db_forget(db);
    }
}

/// Drops a Database without flush-on-drop (crash sim).
fn db_forget(mut db: Database) {
    db.set_suppress_flush(true);
    drop(db);
}

#[test]
fn vector_index_persists_across_reopen() {
    use crate::graph::vector::{HnswParams, Similarity};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vec.tvdb");
    let dim = 16usize;
    // Deterministic per-node vector; node `i` gets a distinct direction.
    let mk = |i: usize| -> PropertyValue {
        PropertyValue::List(
            (0..dim)
                .map(|j| PropertyValue::Float(((i * 7 + j) as f64 * 0.1).sin()))
                .collect(),
        )
    };
    let query0: Vec<f32> = (0..dim).map(|j| (j as f64 * 0.1).sin() as f32).collect();

    {
        let mut db = Database::open(&path).unwrap();
        {
            let mut g = db.graph_mut();
            for i in 0..80 {
                g.create_node(
                    &["Doc"],
                    props(&[("id", PropertyValue::Int(i as i64)), ("emb", mk(i))]),
                );
            }
            g.create_vector_index(
                "docEmb",
                "Doc",
                "emb",
                dim,
                Similarity::Cosine,
                HnswParams::default(),
            )
            .unwrap();
            let hits = g.query_vector_index("docEmb", &query0, 5, None).unwrap();
            assert_eq!(hits.len(), 5);
            assert!(hits[0].1 > 0.99, "node 0 should match its own vector");
        }
        db.close().unwrap();
    }

    // reopen: descriptor restored, HNSW rebuilt from persisted vectors, no CREATE VECTOR INDEX
    {
        let db = Database::open(&path).unwrap();
        let g = db.graph();
        assert!(
            g.has_vector_index("docEmb"),
            "descriptor should persist across restart"
        );
        let descs = g.vector_index_descriptors();
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].0, "docEmb");
        assert_eq!(descs[0].1, "Doc");
        assert_eq!(descs[0].2, "emb");
        assert_eq!(descs[0].3, dim);
        assert_eq!(descs[0].4, "cosine");

        let hits = g.query_vector_index("docEmb", &query0, 5, None).unwrap();
        assert_eq!(hits.len(), 5, "HNSW should be queryable after reload");
        assert!(
            hits[0].1 > 0.99,
            "reloaded index must still find the exact match"
        );
    }
}

#[test]
fn fulltext_index_persists_across_reopen() {
    use crate::graph::fulltext::Analyzer;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ft.tvdb");

    {
        let mut db = Database::open(&path).unwrap();
        {
            let mut g = db.graph_mut();
            g.create_node(
                &["Doc"],
                props(&[("body", PropertyValue::String("the quick brown fox".into()))]),
            );
            g.create_node(
                &["Doc"],
                props(&[("body", PropertyValue::String("lazy dogs sleep".into()))]),
            );
            g.create_fulltext_index("docs", &["Doc"], &["body"], Analyzer::English)
                .unwrap();
            let hits = g.query_fulltext_index("docs", "fox", 10).unwrap();
            assert_eq!(hits.len(), 1, "one doc mentions fox");
        }
        db.close().unwrap();
    }

    // reopen: descriptor restored, inverted index rebuilt from persisted strings, no CREATE
    // FULLTEXT
    {
        let db = Database::open(&path).unwrap();
        let g = db.graph();
        assert!(
            g.has_fulltext_index("docs"),
            "descriptor should persist across restart"
        );
        let descs = g.fulltext_index_descriptors();
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].0, "docs");
        assert_eq!(descs[0].3, "english");
        let hits = g.query_fulltext_index("docs", "fox", 10).unwrap();
        assert_eq!(
            hits.len(),
            1,
            "inverted index must be queryable after reload"
        );
    }
}

#[test]
fn point_index_persists_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pt.tvdb");

    {
        let mut db = Database::open(&path).unwrap();
        {
            let mut g = db.graph_mut();
            g.create_node(
                &["Place"],
                props(&[(
                    "loc",
                    PropertyValue::Point2D {
                        x: 0.0,
                        y: 0.0,
                        srid: 7203,
                    },
                )]),
            );
            g.create_node(
                &["Place"],
                props(&[(
                    "loc",
                    PropertyValue::Point2D {
                        x: 10.0,
                        y: 10.0,
                        srid: 7203,
                    },
                )]),
            );
            g.create_point_index("places", "Place", "loc").unwrap();
            let near = g
                .point_index_knn("Place", "loc", [0.0, 0.0, 0.0], 1, 7203)
                .unwrap();
            assert_eq!(near.len(), 1);
        }
        db.close().unwrap();
    }

    // reopen: descriptor restored, R-tree rebuilt from persisted points, no CREATE POINT INDEX
    {
        let db = Database::open(&path).unwrap();
        let g = db.graph();
        assert!(
            g.has_point_index("places"),
            "descriptor should persist across restart"
        );
        let descs = g.point_index_descriptors();
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].0, "places");
        assert_eq!(descs[0].1, "Place");
        assert_eq!(descs[0].2, "loc");
        let near = g
            .point_index_knn("Place", "loc", [0.5, 0.5, 0.0], 1, 7203)
            .unwrap();
        assert_eq!(near.len(), 1, "R-tree must be queryable after reload");
        // The (0,0) point is nearest to (0.5,0.5).
        let bbox = g
            .point_index_within_bbox("Place", "loc", [-1.0, -1.0, 0.0], [5.0, 5.0, 0.0], 7203)
            .unwrap();
        assert_eq!(bbox.len(), 1, "only (0,0) falls in the box");
    }
}

#[test]
fn text_index_persists_across_reopen() {
    use crate::graph::text::TextQueryKind;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tx.tvdb");

    {
        let mut db = Database::open(&path).unwrap();
        {
            let mut g = db.graph_mut();
            g.create_node(
                &["User"],
                props(&[("email", PropertyValue::String("alice@example.com".into()))]),
            );
            g.create_node(
                &["User"],
                props(&[("email", PropertyValue::String("bob@test.org".into()))]),
            );
            g.create_text_index("emails", "User", "email").unwrap();
            let cand = g
                .text_index_candidates("User", "email", TextQueryKind::Contains, "example")
                .unwrap();
            assert_eq!(cand.len(), 1);
        }
        db.close().unwrap();
    }

    // reopen: descriptor restored, trigrams rebuilt from persisted strings, no CREATE TEXT
    // INDEX
    {
        let db = Database::open(&path).unwrap();
        let g = db.graph();
        assert!(
            g.has_text_index("emails"),
            "descriptor should persist across restart"
        );
        let descs = g.text_index_descriptors();
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].0, "emails");
        assert_eq!(descs[0].1, "User");
        assert_eq!(descs[0].2, "email");
        let cand = g
            .text_index_candidates("User", "email", TextQueryKind::EndsWith, ".org")
            .unwrap();
        assert_eq!(
            cand.len(),
            1,
            "trigram index must be queryable after reload"
        );
    }
}

#[test]
fn composite_constraint_persists_across_reopen() {
    use crate::graph::constraint::ConstraintType;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ck.tvdb");

    {
        let mut db = Database::open(&path).unwrap();
        {
            let mut g = db.graph_mut();
            g.create_node(
                &["P"],
                props(&[
                    ("a", PropertyValue::String("x".into())),
                    ("b", PropertyValue::Int(1)),
                ]),
            );
            g.create_composite_constraint(
                "ck",
                "P",
                &["a".to_string(), "b".to_string()],
                ConstraintType::NodeKey,
                false,
            )
            .unwrap();
        }
        db.close().unwrap();
    }

    {
        let db = Database::open(&path).unwrap();
        let g = db.graph();
        let desc = g
            .constraint_registry()
            .get_by_name("ck")
            .expect("composite constraint survives reopen");
        assert_eq!(desc.extra_property_keys.len(), 1, "second key restored");
        assert!(matches!(desc.constraint_type, ConstraintType::NodeKey));
    }
}

#[test]
fn multiple_flush_with_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let mut db = Database::open(&path).unwrap();
    db.graph_mut().create_node(&["A"], empty_props());
    db.flush().unwrap();

    db.graph_mut().create_node(&["B"], empty_props());
    db.flush().unwrap();

    db.graph_mut().create_node(&["C"], empty_props());
    db.close().unwrap();

    let db = Database::open(&path).unwrap();
    assert_eq!(db.graph().node_count(), 3);
    assert_eq!(db.graph().nodes_by_label("A").len(), 1);
    assert_eq!(db.graph().nodes_by_label("B").len(), 1);
    assert_eq!(db.graph().nodes_by_label("C").len(), 1);
}

#[test]
fn drop_persists_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    {
        let mut db = Database::open(&path).unwrap();
        db.graph_mut().create_node(&["Dropped"], empty_props());
        // no close() on purpose: rely on Drop
    }

    let db = Database::open(&path).unwrap();
    assert_eq!(db.graph().node_count(), 1);
    assert_eq!(db.graph().nodes_by_label("Dropped").len(), 1);
}

#[test]
fn open_garbage_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("garbage.tvdb");
    std::fs::write(&path, b"this is not an tvdb file at all!!!!!!!!!!!!!!!!!").unwrap();
    let err = Database::open(&path).unwrap_err();
    assert!(
        matches!(err, Error::InvalidFormat(_)),
        "expected InvalidFormat, got: {err:?}"
    );
}

#[test]
fn concurrent_open_returns_locked_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let _db1 = Database::open(&path).unwrap();
    let err = Database::open(&path).unwrap_err();
    assert!(
        matches!(err, Error::DatabaseLocked(_)),
        "expected DatabaseLocked, got: {err:?}"
    );
}

// -- Incremental flush tests ------------------------------------

#[test]
fn incremental_flush_modify_single_node() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let (alice_id, bob_id);
    {
        let mut db = Database::open(&path).unwrap();
        alice_id = db.graph_mut().create_node(
            &["Person"],
            props(&[("name", PropertyValue::String("Alice".into()))]),
        );
        bob_id = db.graph_mut().create_node(
            &["Person"],
            props(&[("name", PropertyValue::String("Bob".into()))]),
        );
        db.flush().unwrap();

        // Modify only Alice
        db.graph_mut()
            .update_node_properties(
                alice_id,
                props(&[("name", PropertyValue::String("Alice Updated".into()))]),
            )
            .unwrap();
        db.flush().unwrap();
    }

    let db = Database::open(&path).unwrap();
    let g = db.graph();
    assert_eq!(g.node_count(), 2);
    let alice = g.get_node(alice_id).unwrap();
    assert_eq!(
        node_prop(alice, &g, "name").unwrap().as_str(),
        Some("Alice Updated")
    );
    let bob = g.get_node(bob_id).unwrap();
    assert_eq!(node_prop(bob, &g, "name").unwrap().as_str(), Some("Bob"));
}

#[test]
fn incremental_flush_delete_entity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let bob_id;
    {
        let mut db = Database::open(&path).unwrap();
        let alice_id = db.graph_mut().create_node(&["Person"], empty_props());
        bob_id = db.graph_mut().create_node(&["Person"], empty_props());
        db.graph_mut()
            .create_edge(alice_id, bob_id, "KNOWS", empty_props())
            .unwrap();
        db.flush().unwrap();

        // Delete Alice and her edge via cascading delete
        db.graph_mut().delete_node_cascading(alice_id).unwrap();
        db.flush().unwrap();
    }

    let db = Database::open(&path).unwrap();
    assert_eq!(db.graph().node_count(), 1);
    assert_eq!(db.graph().edge_count(), 0);
    assert!(db.graph().get_node(bob_id).is_ok());
}

#[test]
fn incremental_flush_add_new_entity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    {
        let mut db = Database::open(&path).unwrap();
        db.graph_mut().create_node(&["A"], empty_props());
        db.flush().unwrap();

        // Add a second node incrementally
        db.graph_mut().create_node(&["B"], empty_props());
        db.flush().unwrap();
    }

    let db = Database::open(&path).unwrap();
    assert_eq!(db.graph().node_count(), 2);
    assert_eq!(db.graph().nodes_by_label("A").len(), 1);
    assert_eq!(db.graph().nodes_by_label("B").len(), 1);
}

#[test]
fn incremental_flush_mixed_operations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let (bob_id, charlie_id);
    {
        let mut db = Database::open(&path).unwrap();
        let alice_id = db.graph_mut().create_node(
            &["Person"],
            props(&[("name", PropertyValue::String("Alice".into()))]),
        );
        bob_id = db.graph_mut().create_node(
            &["Person"],
            props(&[("name", PropertyValue::String("Bob".into()))]),
        );
        db.graph_mut()
            .create_edge(alice_id, bob_id, "KNOWS", empty_props())
            .unwrap();
        db.flush().unwrap();

        // Mixed: delete Alice, modify Bob, add Charlie
        db.graph_mut().delete_node_cascading(alice_id).unwrap();
        db.graph_mut()
            .update_node_properties(
                bob_id,
                props(&[("name", PropertyValue::String("Bob Updated".into()))]),
            )
            .unwrap();
        charlie_id = db.graph_mut().create_node(
            &["Person"],
            props(&[("name", PropertyValue::String("Charlie".into()))]),
        );
        db.flush().unwrap();
    }

    let db = Database::open(&path).unwrap();
    let g = db.graph();
    assert_eq!(g.node_count(), 2);
    assert_eq!(g.edge_count(), 0);
    let bob = g.get_node(bob_id).unwrap();
    assert_eq!(
        node_prop(bob, &g, "name").unwrap().as_str(),
        Some("Bob Updated")
    );
    let charlie = g.get_node(charlie_id).unwrap();
    assert_eq!(
        node_prop(charlie, &g, "name").unwrap().as_str(),
        Some("Charlie")
    );
}

#[test]
fn no_changes_flush_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let mut db = Database::open(&path).unwrap();
    db.graph_mut().create_node(&["A"], empty_props());
    db.flush().unwrap();

    // Flush again with no changes - should be a no-op
    let file_size_before = std::fs::metadata(&path).unwrap().len();
    db.flush().unwrap();
    let file_size_after = std::fs::metadata(&path).unwrap().len();
    assert_eq!(file_size_before, file_size_after);
}

#[test]
fn compact_after_deletes_reclaims_space() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let mut db = Database::open(&path).unwrap();
    // Create many nodes with large-ish properties to span multiple pages
    let mut ids = Vec::new();
    for i in 0..500 {
        let id = db.graph_mut().create_node(
            &["Node"],
            props(&[
                ("index", PropertyValue::Int(i)),
                ("data", PropertyValue::String(format!("payload-{i:0>100}"))),
            ]),
        );
        ids.push(id);
    }
    db.flush().unwrap();

    // Delete most of them
    for &id in &ids[1..] {
        db.graph_mut().delete_node(id).unwrap();
    }
    db.flush().unwrap();
    let size_after_delete = std::fs::metadata(&path).unwrap().len();

    // Compact should reclaim space
    db.compact().unwrap();
    let size_after_compact = std::fs::metadata(&path).unwrap().len();
    assert!(
        size_after_compact < size_after_delete,
        "compact should reduce file size: {size_after_compact} >= {size_after_delete}"
    );

    // Data should still be intact
    drop(db);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.graph().node_count(), 1);
    assert!(db.graph().get_node(ids[0]).is_ok());
}

#[test]
fn multiple_incremental_flushes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let mut db = Database::open(&path).unwrap();

    // Flush 1: Create node A
    let a = db.graph_mut().create_node(&["A"], empty_props());
    db.flush().unwrap();

    // Flush 2: Create node B and edge
    let b = db.graph_mut().create_node(&["B"], empty_props());
    db.graph_mut()
        .create_edge(a, b, "R", empty_props())
        .unwrap();
    db.flush().unwrap();

    // Flush 3: Delete edge, modify A
    let edges: Vec<_> = db.graph().all_edges().map(|e| e.id).collect();
    for eid in edges {
        db.graph_mut().delete_edge(eid).unwrap();
    }
    db.graph_mut()
        .update_node_properties(a, props(&[("x", PropertyValue::Int(1))]))
        .unwrap();
    db.flush().unwrap();

    drop(db);
    let db = Database::open(&path).unwrap();
    let g = db.graph();
    assert_eq!(g.node_count(), 2);
    assert_eq!(g.edge_count(), 0);
    let node_a = g.get_node(a).unwrap();
    assert_eq!(node_prop(node_a, &g, "x").unwrap().as_int(), Some(1));
}

// -- WAL / crash-safety tests -------------------------------------

#[test]
fn crash_after_wal_commit_recovers_data() {
    use crate::storage::checksum;
    use crate::storage::wal;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    // Create DB with one node
    let alice_id;
    {
        let mut db = Database::open(&path).unwrap();
        alice_id = db.graph_mut().create_node(
            &["Person"],
            props(&[("name", PropertyValue::String("Alice".into()))]),
        );
        db.close().unwrap();
    }

    // simulate crash: committed WAL with a modified page, never applied to the main file
    {
        let pm = PageManager::open(&path).unwrap();

        // Build a modified version of page 0 with an additional record
        let mut modified_page = *pm.get_page(0).unwrap();
        {
            let mut page = SlottedPage::new(&mut modified_page, 0);
            // Modify by stamping a new LSN
            page.set_lsn(2);
            checksum::write_page_checksum(&mut modified_page);
        }

        let mut hdr = pm.header().clone();
        hdr.lsn = 2;
        let hdr_bytes = hdr.to_bytes();

        let wal_path = wal::wal_path(&path);
        let mut w = wal::WalWriter::create(&wal_path, 2, 2).unwrap();
        w.write_page(0, &modified_page).unwrap();
        w.write_header(&hdr_bytes).unwrap();
        w.commit().unwrap();
        // Drop pm without applying - simulates crash
    }

    // Reopen should recover via WAL replay
    let db = Database::open(&path).unwrap();
    let g = db.graph();
    assert_eq!(g.node_count(), 1);
    let alice = g.get_node(alice_id).unwrap();
    assert_eq!(
        node_prop(alice, &g, "name").unwrap().as_str(),
        Some("Alice")
    );

    // WAL should be cleaned up
    assert!(!wal::wal_path(&path).exists());
}

#[test]
fn crash_before_wal_commit_discards() {
    use crate::storage::wal;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let alice_id;
    {
        let mut db = Database::open(&path).unwrap();
        alice_id = db.graph_mut().create_node(
            &["Person"],
            props(&[("name", PropertyValue::String("Alice".into()))]),
        );
        db.close().unwrap();
    }

    // Write an uncommitted WAL (partial write before crash)
    {
        let wal_path = wal::wal_path(&path);
        let mut w = wal::WalWriter::create(&wal_path, 99, 99).unwrap();
        w.write_page(0, &[0xFFu8; crate::storage::format::PAGE_SIZE])
            .unwrap();
        // Drop without commit - simulates crash before commit
    }

    // Reopen: uncommitted WAL discarded, original data intact
    let db = Database::open(&path).unwrap();
    let g = db.graph();
    assert_eq!(g.node_count(), 1);
    let alice = g.get_node(alice_id).unwrap();
    assert_eq!(
        node_prop(alice, &g, "name").unwrap().as_str(),
        Some("Alice")
    );
    assert!(!wal::wal_path(&path).exists());
}

#[test]
fn normal_flush_leaves_no_wal() {
    use crate::storage::wal;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let mut db = Database::open(&path).unwrap();
    db.graph_mut().create_node(&["A"], empty_props());
    db.flush().unwrap();

    assert!(!wal::wal_path(&path).exists());

    db.graph_mut().create_node(&["B"], empty_props());
    db.flush().unwrap();

    assert!(!wal::wal_path(&path).exists());
}

#[test]
fn recovery_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    {
        let mut db = Database::open(&path).unwrap();
        db.graph_mut().create_node(&["A"], empty_props());
        db.close().unwrap();
    }

    // Running recovery multiple times on a clean DB is safe
    PageManager::recover(&path).unwrap();
    PageManager::recover(&path).unwrap();

    let db = Database::open(&path).unwrap();
    assert_eq!(db.graph().node_count(), 1);
}

#[test]
fn lsn_increments_across_flushes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let mut db = Database::open(&path).unwrap();
    db.graph_mut().create_node(&["A"], empty_props());
    db.flush().unwrap(); // compact (first write) -> creates file

    db.graph_mut().create_node(&["B"], empty_props());
    db.flush().unwrap(); // incremental flush -> LSN 1

    db.graph_mut().create_node(&["C"], empty_props());
    db.flush().unwrap(); // incremental flush -> LSN 2

    drop(db);

    // Reopen and check header LSN
    let pm = PageManager::open(&path).unwrap();
    assert!(pm.lsn() >= 2, "LSN should be at least 2, got {}", pm.lsn());
}

#[test]
fn compact_resets_lsn() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let mut db = Database::open(&path).unwrap();
    db.graph_mut().create_node(&["A"], empty_props());
    db.flush().unwrap();

    db.graph_mut().create_node(&["B"], empty_props());
    db.flush().unwrap();

    // Compact produces a fresh file with LSN 0
    db.compact().unwrap();
    drop(db);

    let pm = PageManager::open(&path).unwrap();
    // compact creates a new file via PageManager::create and flushes once, so LSN is 0 or 1
    assert!(
        pm.lsn() <= 1,
        "LSN after compact should be 0 or 1, got {}",
        pm.lsn()
    );
}

// -- Transaction tests -----------------------------------------

#[test]
fn database_begin_write_commit() {
    use crate::tx::view::GraphView;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let db = Database::open(&path).unwrap();
    {
        let mut tx = db.begin_write();
        GraphView::create_node(
            &mut tx,
            &["Person"],
            props(&[("name", PropertyValue::String("Alice".into()))]),
        )
        .unwrap();
        tx.commit().unwrap();
    }

    assert_eq!(db.graph().node_count(), 1);
    db.flush().unwrap();

    // Reopen and verify persistence
    drop(db);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.graph().node_count(), 1);
}

#[test]
fn database_begin_write_abort() {
    use crate::tx::view::GraphView;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let db = Database::open(&path).unwrap();
    {
        let mut tx = db.begin_write();
        GraphView::create_node(&mut tx, &["Person"], empty_props()).unwrap();
        tx.abort();
    }

    assert_eq!(db.graph().node_count(), 0);
}

#[test]
fn database_snapshot_isolation() {
    use crate::tx::view::GraphView;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let mut db = Database::open(&path).unwrap();
    db.graph_mut().create_node(
        &["Person"],
        props(&[("name", PropertyValue::String("Alice".into()))]),
    );

    // Start read before write
    let read_tx = db.begin_read().unwrap();

    // Write through transaction
    {
        let mut write_tx = db.begin_write();
        GraphView::create_node(
            &mut write_tx,
            &["Person"],
            props(&[("name", PropertyValue::String("Bob".into()))]),
        )
        .unwrap();
        write_tx.commit().unwrap();
    }

    // Read tx should still see only Alice
    assert_eq!(GraphView::node_count(&read_tx), 1);

    drop(read_tx);

    // New read should see both
    assert_eq!(db.graph().node_count(), 2);
}

// -- Additional crash / group-commit tests ----------------------

#[test]
fn crash_mid_compact_stale_tmp() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    // Create DB with data
    {
        let mut db = Database::open(&path).unwrap();
        db.graph_mut().create_node(
            &["Person"],
            props(&[("name", PropertyValue::String("Alice".into()))]),
        );
        db.close().unwrap();
    }

    // Simulate a crash that left a stale .tmp file
    let tmp_path = path.with_extension("tvdb.tmp");
    std::fs::write(&tmp_path, b"stale compact temp data").unwrap();
    assert!(tmp_path.exists());

    // Reopen should clean up the .tmp and load data intact
    let db = Database::open(&path).unwrap();
    assert!(!tmp_path.exists(), ".tmp file should be cleaned up on open");
    let g = db.graph();
    assert_eq!(g.node_count(), 1);
    let nodes: Vec<_> = g.nodes_by_label("Person");
    assert_eq!(nodes.len(), 1);
}

#[test]
fn crash_with_truncated_wal_entry() {
    use std::io::Write as _;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    // Create DB with one node
    {
        let mut db = Database::open(&path).unwrap();
        db.graph_mut().create_node(
            &["Person"],
            props(&[("name", PropertyValue::String("Alice".into()))]),
        );
        db.close().unwrap();
    }

    // v2 WAL header claiming 1 entry, followed by a truncated entry
    {
        let wal_file = wal::wal_path(&path);
        let mut file = std::fs::File::create(&wal_file).unwrap();

        let mut hdr = [0u8; 32];
        hdr[0..4].copy_from_slice(b"TVWL");
        hdr[4..8].copy_from_slice(&2u32.to_le_bytes()); // version 2
        hdr[8..16].copy_from_slice(&99u64.to_le_bytes()); // lsn
        hdr[16..20].copy_from_slice(&1u32.to_le_bytes()); // entry_count = 1
        hdr[20..28].copy_from_slice(&99u64.to_le_bytes()); // tx_id
        let crc = crc32fast::hash(&hdr[0..28]);
        hdr[28..32].copy_from_slice(&crc.to_le_bytes());
        file.write_all(&hdr).unwrap();

        // Write only 5 bytes of a "page entry" - way too short
        file.write_all(&[0x01, 0x00, 0x00, 0x00, 0x00]).unwrap();
        file.sync_all().unwrap();
    }

    // Reopen: truncated WAL should be discarded, original data intact
    let db = Database::open(&path).unwrap();
    let g = db.graph();
    assert_eq!(g.node_count(), 1);
    let nodes: Vec<_> = g.nodes_by_label("Person");
    assert_eq!(nodes.len(), 1);
    assert!(!wal::wal_path(&path).exists());
}

#[test]
fn crash_mid_main_apply_wal_replay() {
    use std::io::{Seek, SeekFrom, Write as _};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    // enough nodes with large properties for 2+ data pages
    {
        let mut db = Database::open(&path).unwrap();
        for i in 0..50 {
            db.graph_mut().create_node(
                &["Node"],
                props(&[
                    ("index", PropertyValue::Int(i)),
                    ("data", PropertyValue::String(format!("payload-{i:0>200}"))),
                ]),
            );
        }
        db.close().unwrap();
    }

    // Read the current state to build a WAL that modifies two pages
    let (modified_page_0, modified_page_1, hdr_bytes);
    {
        let pm = PageManager::open(&path).unwrap();
        assert!(pm.total_pages() >= 2, "need at least 2 pages");

        // Modify page 0 by stamping a new LSN
        let mut page0 = *pm.get_page(0).unwrap();
        {
            let mut page = SlottedPage::new(&mut page0, 0);
            page.set_lsn(99);
            page.write_checksum();
        }
        modified_page_0 = page0;

        // Modify page 1 by stamping a new LSN
        let mut page1 = *pm.get_page(1).unwrap();
        {
            let mut page = SlottedPage::new(&mut page1, 1);
            page.set_lsn(99);
            page.write_checksum();
        }
        modified_page_1 = page1;

        let mut hdr = pm.header().clone();
        hdr.lsn = 99;
        hdr_bytes = hdr.to_bytes();
    }

    // partial apply: page 0 reaches the main file, page 1 and header don't; committed WAL left
    // behind
    {
        // Write page 0 to main file (partial apply)
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let offset_0 = HEADER_SIZE as u64;
        file.seek(SeekFrom::Start(offset_0)).unwrap();
        file.write_all(&modified_page_0).unwrap();
        file.sync_all().unwrap();
        // Page 1 and header NOT written - simulates crash mid-apply
    }

    // Write committed WAL with both pages + header
    {
        let wal_file = wal::wal_path(&path);
        let mut w = wal::WalWriter::create(&wal_file, 99, 99).unwrap();
        w.write_page(0, &modified_page_0).unwrap();
        w.write_page(1, &modified_page_1).unwrap();
        w.write_header(&hdr_bytes).unwrap();
        w.commit().unwrap();
    }

    // reopen: recovery replays the whole WAL, both pages and header end up correct
    let db = Database::open(&path).unwrap();
    let g = db.graph();
    assert_eq!(g.node_count(), 50);
    // Verify all nodes are intact
    for i in 0..50 {
        let nodes: Vec<_> = g
            .nodes()
            .filter(|n| node_prop(n, &g, "index").and_then(|v| v.as_int()) == Some(i))
            .collect();
        assert_eq!(nodes.len(), 1, "node with index={i} should exist");
    }
    assert!(!wal::wal_path(&path).exists());
}

#[test]
fn group_commit_batches_transactions() {
    use crate::tx::view::GraphView;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.tvdb");

    let db = Database::open(&path).unwrap();

    // Tx 1: create Alice
    {
        let mut tx = db.begin_write();
        GraphView::create_node(
            &mut tx,
            &["Person"],
            props(&[("name", PropertyValue::String("Alice".into()))]),
        )
        .unwrap();
        tx.commit().unwrap();
    }

    // Tx 2: create Bob
    {
        let mut tx = db.begin_write();
        GraphView::create_node(
            &mut tx,
            &["Person"],
            props(&[("name", PropertyValue::String("Bob".into()))]),
        )
        .unwrap();
        tx.commit().unwrap();
    }

    // A single flush() persists both transactions
    db.flush().unwrap();

    // Reopen and verify both are persisted
    drop(db);
    let db = Database::open(&path).unwrap();
    let g = db.graph();
    assert_eq!(g.node_count(), 2);
    let names: Vec<String> = g
        .nodes()
        .filter_map(|n| {
            node_prop(n, &g, "name").and_then(|v| match v {
                PropertyValue::String(s) => Some(s),
                _ => None,
            })
        })
        .collect();
    assert!(names.contains(&"Alice".to_string()));
    assert!(names.contains(&"Bob".to_string()));
}

/// Hex text that doesn't compress: catalogs built from it are as large as they look.
fn noise(i: u64) -> String {
    let a = i.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let b = a.rotate_left(29).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    format!("{a:016x}{b:016x}")
}

#[test]
fn a_checkpoint_saves_while_a_writer_holds_the_lock() {
    use std::time::Duration;
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("cp.tvdb");
    let mut g = Graph::new();
    g.create_node(&["N"], empty_props());
    let store = MvccStore::new(g);
    let checkpoint = store.checkpoint().unwrap();
    let mut open = store.begin_write();
    GraphView::create_node(&mut open, &["N"], empty_props()).unwrap();
    let (saved, wait) = std::sync::mpsc::channel();
    std::thread::scope(|s| {
        s.spawn(|| saved.send(Database::save_checkpoint(&path, &checkpoint, b"meta")));
        let counts = wait
            .recv_timeout(Duration::from_secs(30))
            .expect("the save doesn't wait for the writer")
            .unwrap();
        assert_eq!(counts, (1, 0));
    });
    open.abort();
    let db = Database::open(&path).unwrap();
    assert_eq!(db.graph().node_count(), 1);
    assert_eq!(db.metadata(), b"meta");
}

/// The version a `.tvdb` says it is.
fn file_version(path: &Path) -> u32 {
    let bytes = std::fs::read(path).unwrap();
    u32::from_le_bytes(bytes[4..8].try_into().unwrap())
}

#[test]
fn a_catalog_larger_than_its_page_keeps_every_entry() {
    use crate::graph::constraint::ConstraintType;
    use crate::storage::catalog::StatsCatalog;
    use crate::storage::compression;
    use crate::storage::format::PAGE_DATA_CAPACITY;
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("stats.tvdb");
    let mut g = Graph::new();
    for i in 0..300u64 {
        let p: BTreeMap<String, PropertyValue> = (0..30u64)
            .map(|k| (format!("p{k}"), PropertyValue::String(noise(i * 31 + k))))
            .collect();
        g.create_node(&["P"], p);
    }
    g.create_index("p1_idx", "P", "p1").unwrap();
    GraphView::create_constraint(&mut g, "p0_unique", "P", "p0", ConstraintType::Unique).unwrap();
    GraphView::analyze_statistics(&mut g, &[]);
    let stats = g.statistics().unwrap().clone();
    let stats_size = compression::encode_record(&StatsCatalog { stats }.to_bytes().unwrap()).len();
    assert!(
        stats_size > PAGE_DATA_CAPACITY,
        "the statistics need {stats_size} bytes"
    );

    Database::save(&path, &g, &[]).unwrap();
    assert_eq!(file_version(&path), 7);
    let db = Database::open(&path).unwrap();
    let parts = db.graph().as_parts();
    let stats = parts.stats.expect("the statistics are kept");
    assert_eq!(stats.property_stats.len(), 30);
    let mut indexes: Vec<&str> = parts
        .index_definitions
        .iter()
        .map(|d| d.0.as_str())
        .collect();
    indexes.sort_unstable();
    let constraints: Vec<&str> = parts
        .constraint_definitions
        .iter()
        .map(|d| d.0.as_str())
        .collect();
    // the unique constraint keeps its backing index
    assert_eq!(indexes, ["__constraint_p0_unique", "p1_idx"]);
    assert_eq!(constraints, ["p0_unique"]);
    assert_eq!(parts.nodes.len(), 300);
}

#[test]
fn thousands_of_names_save_and_load() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("names.tvdb");
    let mut g = Graph::new();
    g.create_node(&["N"], empty_props());
    for i in 0..2000 {
        GraphView::intern_property_key(&mut g, &noise(i));
    }
    Database::save(&path, &g, &[]).unwrap();
    let db = Database::open(&path).unwrap();
    let graph = db.graph();
    assert!((0..2000).all(|i| graph.strings().get(&noise(i)).is_some()));
}

#[test]
fn files_without_large_records_or_compact_relationships_stay_version_6() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("small.tvdb");
    let mut g = Graph::new();
    let a = g.create_node(
        &["N"],
        props(&[("s", PropertyValue::String("x".repeat(9000)))]),
    );
    let b = g.create_node(&["N"], empty_props());
    g.create_edge(a, b, "R", props(&[("w", PropertyValue::Int(1))]))
        .unwrap();
    Database::save(&path, &g, b"users").unwrap();
    assert_eq!(file_version(&path), 6, "earlier releases read it");
    let mut db = Database::open(&path).unwrap();
    db.graph_mut().create_node(&["N"], empty_props());
    db.flush().unwrap();
    drop(db);
    assert_eq!(file_version(&path), 6);
}

#[test]
fn files_with_property_less_relationships_are_version_8() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("compact.tvdb");
    let mut g = Graph::new();
    let a = g.create_node(&["N"], empty_props());
    let b = g.create_node(&["N"], empty_props());
    g.create_edge(a, b, "R", empty_props()).unwrap();
    Database::save(&path, &g, b"users").unwrap();
    assert_eq!(file_version(&path), 8);
    // a v6 file gains one such relationship through a flush and becomes v8 too
    let path6 = dir.path().join("grown.tvdb");
    let mut g6 = Graph::new();
    let a6 = g6.create_node(&["N"], empty_props());
    let b6 = g6.create_node(&["N"], empty_props());
    g6.create_edge(a6, b6, "R", props(&[("w", PropertyValue::Int(1))]))
        .unwrap();
    Database::save(&path6, &g6, b"users").unwrap();
    assert_eq!(file_version(&path6), 6);
    let mut db = Database::open(&path6).unwrap();
    db.graph_mut()
        .create_edge(a6, b6, "R", empty_props())
        .unwrap();
    db.flush().unwrap();
    drop(db);
    assert_eq!(file_version(&path6), 8);
    let db = Database::open(&path6).unwrap();
    assert_eq!(db.graph().edge_count(), 2);
}

#[test]
fn records_larger_than_a_page_save_load_and_flush_in_place() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("big.tvdb");
    let big = |seed: u64, kb: u64| -> PropertyValue {
        PropertyValue::String(
            (0..kb * 1024 / 32)
                .map(|i| noise(seed * 100_000 + i))
                .collect(),
        )
    };
    // a 4096-dimension embedding alone is a page of floats
    let embedding = PropertyValue::Vector((0..4096).map(|i| (i as f32 * 1.618).sin()).collect());
    let (doc, other, rel) = {
        let mut g = Graph::new();
        let doc = g.create_node(
            &["Doc"],
            props(&[("text", big(1, 40)), ("v", embedding.clone())]),
        );
        let other = g.create_node(&["Doc"], props(&[("text", big(2, 20))]));
        let rel = g
            .create_edge(doc, other, "CITES", props(&[("note", big(3, 30))]))
            .unwrap();
        Database::save(&path, &g, &[]).unwrap();
        (doc, other, rel)
    };
    assert_eq!(file_version(&path), 7);

    let check = |text: &PropertyValue, note: Option<&PropertyValue>| {
        let db = Database::open(&path).unwrap();
        let g = db.graph();
        let node = g.get_node(doc).unwrap();
        assert_eq!(node_prop(node, &g, "text").as_ref(), Some(text));
        assert_eq!(node_prop(node, &g, "v").as_ref(), Some(&embedding));
        let key = crate::graph::types::PropertyKeyId(g.strings().get("note").unwrap());
        match note {
            Some(note) => {
                assert_eq!(
                    g.get_edge(rel).unwrap().properties.get(key).as_ref(),
                    Some(note)
                )
            }
            None => assert!(g.get_edge(rel).is_err()),
        }
    };
    check(&big(1, 40), Some(&big(3, 30)));

    // in place: a new value for the big node, the relationship deleted, then the same again
    let pages_after = |rounds: u64, delete_rel: bool| {
        for round in 0..rounds {
            let mut db = Database::open(&path).unwrap();
            {
                let mut g = db.graph_mut();
                // replaces every property: the embedding goes back too
                g.update_node_properties(
                    doc,
                    props(&[("text", big(10 + round, 40)), ("v", embedding.clone())]),
                )
                .unwrap();
                if delete_rel && round == 0 {
                    g.delete_edge(rel).unwrap();
                }
            }
            db.flush().unwrap();
        }
        let len = std::fs::metadata(&path).unwrap().len();
        (len - HEADER_SIZE as u64) / crate::storage::format::PAGE_SIZE as u64
    };
    let first = pages_after(1, true);
    check(&big(10, 40), None);
    // freed overflow pages are reused: rewriting the value again and again doesn't grow the file
    let later = pages_after(5, false);
    assert!(later <= first + 1, "{first} pages, then {later}");
    check(&big(14, 40), None);
    assert!(
        Database::open(&path)
            .unwrap()
            .graph()
            .get_node(other)
            .is_ok()
    );
}

#[test]
fn metadata_larger_than_a_page_is_written_copy_on_write() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("meta.tvdb");
    let mut g = Graph::new();
    g.create_node(&["N"], empty_props());
    Database::save(&path, &g, b"first").unwrap();
    let before = std::fs::read(&path).unwrap();

    let large: Vec<u8> = (0..3000).flat_map(|i| noise(i).into_bytes()).collect();
    Database::save_metadata(&path, &large).unwrap();
    let after = std::fs::read(&path).unwrap();
    // every page the file had is untouched: the new metadata went after them
    assert_eq!(after[HEADER_SIZE..before.len()], before[HEADER_SIZE..]);
    assert!(after.len() > before.len() + large.len() / 2);
    assert_eq!(reopened_metadata(&path), large);
    assert_eq!(file_version(&path), 7);

    Database::save_metadata(&path, b"small again").unwrap();
    assert_eq!(reopened_metadata(&path), b"small again");
    assert!(!wal::wal_path(&path).exists());
}

#[test]
fn a_failed_save_keeps_the_file_and_leaves_no_temp_file() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("keep.tvdb");
    let mut g = Graph::new();
    g.create_node(&["N"], empty_props());
    Database::save(&path, &g, &[]).unwrap();
    let before = std::fs::read(&path).unwrap();
    let err = Database::save_file(&path, 0, |pm| {
        pm.allocate(PageType::NodeData);
        Err(Error::InvalidFormat("cut short".into()))
    })
    .unwrap_err();
    assert!(err.to_string().contains("cut short"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert!(!path.with_extension("tvdb.tmp").exists());
}

/// Nodes labelled `N` in a reopened `path`.
fn reopened_count(path: &Path, label: &str) -> usize {
    let db = Database::open(path).unwrap();
    db.graph().nodes_by_label(label).len()
}

fn commit_node(db: &Database, label: &str) {
    let mut w = db.begin_write();
    GraphView::create_node(&mut w, &[label], empty_props()).unwrap();
    w.commit().unwrap();
}

#[test]
fn a_save_works_from_its_checkpoint_while_commits_carry_on() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("cp.tvdb");
    {
        let db = Database::open(&path).unwrap();
        commit_node(&db, "Before");
        db.flush().unwrap();
        commit_node(&db, "Saved");

        // a save starts: its checkpoint and the changes it rewrites are taken
        let (checkpoint, changes, mut files) = db.begin_save(None).unwrap();
        // a commit lands while it writes, and a writer is open as it writes
        commit_node(&db, "Later");
        let mut open = db.begin_write();
        GraphView::create_node(&mut open, &["Open"], empty_props()).unwrap();
        {
            let pm = files.page_manager.take().unwrap();
            db.write_in_place(
                &mut files,
                pm,
                &changes,
                &checkpoint,
                checkpoint.catalog(),
                checkpoint.position(),
            )
            .unwrap();
        }
        drop((checkpoint, files));
        open.commit().unwrap();

        // the file has the save's commits; the later ones are in the log past its position, so
        // dying now (no further save) loses nothing
        let mut db = db;
        db.set_suppress_flush(true);
    }
    assert_eq!(reopened_count(&path, "Before"), 1);
    assert_eq!(reopened_count(&path, "Saved"), 1);
    assert_eq!(reopened_count(&path, "Later"), 1);
    assert_eq!(reopened_count(&path, "Open"), 1);

    // and the next save writes them into the file
    let db = Database::open(&path).unwrap();
    db.flush().unwrap();
    drop(db);
    for path in std::fs::read_dir(dir.path()).unwrap() {
        let path = path.unwrap().path();
        if path.to_string_lossy().contains(".rlog.") {
            // segments the save covers are gone; whatever is left replays nothing new
            let _ = std::fs::remove_file(path);
        }
    }
    assert_eq!(reopened_count(&path, "Later"), 1);
    assert_eq!(reopened_count(&path, "Open"), 1);
}

#[test]
fn a_compaction_keeps_commits_made_while_it_writes() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("compact.tvdb");
    {
        let db = Database::open(&path).unwrap();
        commit_node(&db, "Before");
        db.flush().unwrap();
        let (checkpoint, _changes, mut files) = db.begin_save(None).unwrap();
        commit_node(&db, "During");
        db.rewrite(
            &mut files,
            &checkpoint,
            checkpoint.catalog(),
            checkpoint.position(),
        )
        .unwrap();
        drop((checkpoint, files));
        // the commit made meanwhile is still marked changed: the next save writes it
        db.flush().unwrap();
    }
    assert_eq!(reopened_count(&path, "Before"), 1);
    assert_eq!(reopened_count(&path, "During"), 1);
}

#[test]
fn a_bulk_load_committing_during_a_save_is_written_after_it() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("bulk_during_save.tvdb");
    let v = |v: i64| BTreeMap::from([("v".to_string(), PropertyValue::Int(v))]);
    let db = Arc::new(Database::open(&path).unwrap());
    let mut w = db.begin_write();
    let id = GraphView::create_node(&mut w, &["N"], v(0)).unwrap();
    w.commit().unwrap();
    db.flush().unwrap();
    let mut w = db.begin_write();
    w.update_node_properties(id, v(1)).unwrap();
    w.commit().unwrap();

    // a save starts; a bulk load changes the same node and commits, saving as it does
    let (checkpoint, changes, mut files) = db.begin_save(None).unwrap();
    let bulk = std::thread::spawn({
        let db = Arc::clone(&db);
        move || {
            let mut tx = db.begin_bulk_load(None).unwrap();
            tx.update_node_properties(id, v(2)).unwrap();
            tx.commit().unwrap();
        }
    });
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        !bulk.is_finished(),
        "the bulk load saved ahead of an older save"
    );
    let pm = files.page_manager.take().unwrap();
    db.write_in_place(
        &mut files,
        pm,
        &changes,
        &checkpoint,
        checkpoint.catalog(),
        checkpoint.position(),
    )
    .unwrap();
    drop((checkpoint, files));
    bulk.join().unwrap();

    // the file has the load (no log record has it), not the older save's value
    let mut db = Arc::try_unwrap(db).ok().unwrap();
    db.set_suppress_flush(true);
    drop(db);
    let db = Database::open(&path).unwrap();
    let r = db.begin_read().unwrap();
    assert_eq!(
        r.get_node_property(id, "v").unwrap(),
        Some(PropertyValue::Int(2))
    );
}

#[test]
fn in_place_saves_reuse_the_space_of_records_they_replace() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("churn.tvdb");
    let db = Database::open(&path).unwrap();
    let mut w = db.begin_write();
    let ids: Vec<NodeId> = (0..2_000u64)
        .map(|i| {
            GraphView::create_node(
                &mut w,
                &["N"],
                props(&[("s", PropertyValue::String(noise(i)))]),
            )
            .unwrap()
        })
        .collect();
    w.commit().unwrap();
    db.flush().unwrap();
    let size = || std::fs::metadata(&path).unwrap().len();
    let first = size();
    // every record rewritten, a bit larger and smaller in turn, many times over
    for round in 0..10u64 {
        let mut w = db.begin_write();
        for (i, &id) in ids.iter().enumerate() {
            let len = if (round + i as u64).is_multiple_of(2) {
                1
            } else {
                3
            };
            let text: String = (0..len)
                .map(|k| noise(round * 10_000 + i as u64 * 3 + k))
                .collect();
            GraphView::update_node_properties(
                &mut w,
                id,
                props(&[("s", PropertyValue::String(text))]),
            )
            .unwrap();
        }
        w.commit().unwrap();
        db.flush().unwrap();
    }
    assert!(
        size() < first * 4,
        "{first} bytes, then {} after ten rounds of rewrites",
        size()
    );
    drop(db);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.graph().node_count(), 2_000);
}

#[test]
fn a_file_mostly_reusable_space_is_due_a_compaction() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("sparse.tvdb");
    let db = Database::open(&path).unwrap();
    let mut w = db.begin_write();
    let ids: Vec<NodeId> = (0..3_000u64)
        .map(|i| {
            GraphView::create_node(
                &mut w,
                &["N"],
                props(&[("s", PropertyValue::String(noise(i)))]),
            )
            .unwrap()
        })
        .collect();
    w.commit().unwrap();
    db.flush().unwrap();
    assert!(!db.compaction_due(0));

    let mut w = db.begin_write();
    for &id in &ids[..2_500] {
        GraphView::delete_node(&mut w, id).unwrap();
    }
    w.commit().unwrap();
    db.flush().unwrap();
    assert!(db.compaction_due(0));
    assert!(!db.compaction_due(u64::MAX), "too small to bother");

    let before = std::fs::metadata(&path).unwrap().len();
    db.compact().unwrap();
    assert!(std::fs::metadata(&path).unwrap().len() < before / 2);
    assert!(!db.compaction_due(0));
    drop(db);
    assert_eq!(Database::open(&path).unwrap().graph().node_count(), 500);
}

/// A bulk load leaves no version state behind: its commit is durable (the save) only after it
/// publishes, so it collects again then, as a synced commit does. Otherwise every reader after a
/// load takes the versioned paths until some later write's commit collects.
#[test]
fn a_bulk_load_leaves_no_version_state() {
    let dir = tempfile::tempdir().unwrap();
    for (name, saved_first) in [("fresh.tvdb", false), ("existing.tvdb", true)] {
        let path = dir.path().join(name);
        let db = Arc::new(Database::open(&path).unwrap());
        if saved_first {
            let mut w = db.begin_write();
            GraphView::create_node(&mut w, &["Seed"], empty_props()).unwrap();
            w.commit().unwrap();
            db.flush().unwrap();
        }
        let mut tx = db.begin_bulk_load(None).unwrap();
        let ids: Vec<NodeId> = (0..100)
            .map(|i| {
                GraphView::create_node(&mut tx, &["N"], props(&[("v", PropertyValue::Int(i))]))
                    .unwrap()
            })
            .collect();
        for pair in ids.windows(2) {
            GraphView::create_edge(&mut tx, pair[0], pair[1], "R", empty_props()).unwrap();
        }
        tx.bulk_nodes(&["M"], vec![empty_props(); 50]).unwrap();
        tx.commit().unwrap();
        assert!(
            !db.graph().has_active_deltas(),
            "{name}: version state left after the bulk load"
        );
        let r = db.begin_read().unwrap();
        assert_eq!(r.node_count(), 150 + usize::from(saved_first));
        assert_eq!(r.edge_count(), 99);
    }
}
