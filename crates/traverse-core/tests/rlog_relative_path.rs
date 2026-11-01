//! A database opened by a relative path (`Database::open("registry.tvdb")`) keeps its redo log in
//! the working directory: recovery finds it after a crash and a clean close removes it. One test,
//! since the working directory belongs to the whole process.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use traverse_core::graph::property::PropertyValue;
use traverse_core::storage::Database;
use traverse_core::tx::GraphView;

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

fn files_in_working_dir() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(".")
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

/// Restores the working directory when dropped, before the temp dir goes (Windows can't delete
/// the current directory).
struct WorkingDir(PathBuf);

impl Drop for WorkingDir {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

#[test]
fn a_relative_path_keeps_its_redo_log_working() {
    let dir = tempfile::tempdir().unwrap();
    let _restore = WorkingDir(std::env::current_dir().unwrap());
    std::env::set_current_dir(dir.path()).unwrap();
    let path = Path::new("rel.tvdb");

    let db = Database::open(path).unwrap();
    commit_node(&db, "A");
    commit_node(&db, "B");
    crash(db);
    let db = Database::open(path).unwrap();
    assert_eq!(
        db.begin_read().unwrap().node_count(),
        2,
        "recovery replays the log"
    );

    commit_node(&db, "C");
    db.close().unwrap();
    assert_eq!(
        files_in_working_dir(),
        vec!["rel.tvdb"],
        "a clean close removes the log"
    );

    let db = Database::open(path).unwrap();
    commit_node(&db, "D");
    crash(db);
    let db = Database::open(path).unwrap();
    assert_eq!(
        db.begin_read().unwrap().node_count(),
        4,
        "commits after a clean close survive"
    );
    db.close().unwrap();
    assert_eq!(files_in_working_dir(), vec!["rel.tvdb"]);
}
