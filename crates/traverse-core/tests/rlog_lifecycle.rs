//! The redo log across a database file's life: a clean close leaves only the `.tvdb`, and a file
//! opened without its log (copied, uploaded, built elsewhere) keeps every commit made after.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use traverse_core::graph::property::PropertyValue;
use traverse_core::storage::Database;
use traverse_core::tx::GraphView;

/// Redo-log segment files next to the database at `path`.
fn log_segments(path: &Path) -> Vec<String> {
    let stem = path.file_name().unwrap().to_string_lossy().to_string();
    let mut names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with(&stem) && n.contains(".rlog."))
        .collect();
    names.sort();
    names
}

fn commit_node(db: &Database, label: &str) {
    let mut props = BTreeMap::new();
    props.insert("label".to_string(), PropertyValue::String(label.into()));
    let mut tx = db.begin_write();
    tx.create_node(&[label], props).unwrap();
    tx.commit().unwrap();
}

/// Drops the database without saving, like a crash.
fn crash(mut db: Database) {
    db.set_suppress_flush(true);
    drop(db);
}

fn node_count(path: &Path) -> usize {
    Database::open(path)
        .unwrap()
        .begin_read()
        .unwrap()
        .node_count()
}

#[test]
fn a_clean_close_leaves_only_the_database_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.tvdb");
    let db = Database::open(&path).unwrap();
    commit_node(&db, "A");
    commit_node(&db, "B");
    db.close().unwrap();
    assert_eq!(log_segments(&path), Vec::<String>::new());
    assert_eq!(node_count(&path), 2);
}

#[test]
fn opening_and_closing_without_writes_leaves_no_log() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.tvdb");
    Database::open(&path).unwrap().close().unwrap();
    assert_eq!(log_segments(&path), Vec::<String>::new());
}

#[test]
fn a_bulk_load_leaves_only_the_database_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.tvdb");
    let db = Arc::new(Database::open(&path).unwrap());
    let mut bulk = db.begin_bulk_load(None).unwrap();
    for i in 0..100 {
        let mut props = BTreeMap::new();
        props.insert("i".to_string(), PropertyValue::Int(i));
        bulk.create_node(&["Item"], props).unwrap();
    }
    bulk.commit().unwrap();
    Arc::try_unwrap(db).expect("last handle").close().unwrap();
    assert_eq!(log_segments(&path), Vec::<String>::new());
    assert_eq!(node_count(&path), 100);
}

#[test]
fn commits_after_a_clean_close_survive_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.tvdb");
    let db = Database::open(&path).unwrap();
    commit_node(&db, "A");
    db.close().unwrap();

    // the log starts again past what the file holds, so recovery replays the new commit
    let db = Database::open(&path).unwrap();
    commit_node(&db, "B");
    crash(db);
    assert_eq!(node_count(&path), 2);
}

#[test]
fn commits_replayed_at_open_survive_the_close() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.tvdb");
    let db = Database::open(&path).unwrap();
    commit_node(&db, "A");
    crash(db); // A is only in the log

    // replayed at open; the close saves it or keeps the log, either way nothing is lost
    Database::open(&path).unwrap().close().unwrap();
    let db = Database::open(&path).unwrap();
    commit_node(&db, "B");
    crash(db);
    assert_eq!(node_count(&path), 2);
}

#[test]
fn a_stale_log_from_another_copy_is_not_reused() {
    // a file saved with three commits
    let a = tempfile::tempdir().unwrap();
    let saved = a.path().join("t.tvdb");
    let db = Database::open(&saved).unwrap();
    for label in ["A", "B", "C"] {
        commit_node(&db, label);
    }
    db.flush().unwrap();
    crash(db);

    // a directory whose own database crashed with one commit, leaving a shorter log behind
    let b = tempfile::tempdir().unwrap();
    let path = b.path().join("t.tvdb");
    let db = Database::open(&path).unwrap();
    commit_node(&db, "Stale");
    crash(db);
    assert!(!log_segments(&path).is_empty());

    // the saved file replaces that database: the old log's commit isn't replayed into it, and
    // new commits go past the file's position
    std::fs::copy(&saved, &path).unwrap();
    let db = Database::open(&path).unwrap();
    assert_eq!(db.begin_read().unwrap().node_count(), 3);
    commit_node(&db, "D");
    crash(db);
    assert_eq!(node_count(&path), 4);
}

#[test]
fn a_file_copied_without_its_log_keeps_later_commits() {
    let src = tempfile::tempdir().unwrap();
    let path = src.path().join("t.tvdb");
    let db = Database::open(&path).unwrap();
    commit_node(&db, "A");
    commit_node(&db, "B");
    // a save while the database stays open: the file records a log position past 0, and the
    // log segments stay behind in the source directory
    db.flush().unwrap();
    crash(db);

    let dst = tempfile::tempdir().unwrap();
    let copy = dst.path().join("t.tvdb");
    std::fs::copy(&path, &copy).unwrap();
    assert_eq!(log_segments(&copy), Vec::<String>::new());

    let db = Database::open(&copy).unwrap();
    commit_node(&db, "C");
    crash(db);
    assert_eq!(
        node_count(&copy),
        3,
        "the commit made on the copy was lost in the crash"
    );
}
