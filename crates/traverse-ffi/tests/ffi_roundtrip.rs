//! Integration tests for the Traverse FFI layer.
//!
//! These tests call the `extern "C"` functions directly from Rust, exercising
//! the full FFI contract: open -> begin -> query -> read results -> commit/abort ->
//! flush -> close.

// every block drives the C ABI with handles and strings this test just created
#![allow(clippy::undocumented_unsafe_blocks)]

use std::ffi::{CStr, CString};

use traverse_ffi::bulk::*;
use traverse_ffi::database::*;
use traverse_ffi::error::*;
use traverse_ffi::handles::{TvDatabase, TvTransaction};
use traverse_ffi::query::*;
use traverse_ffi::result::*;
use traverse_ffi::transaction::*;

/// Helper: read the last error message as a Rust string.
fn last_error_msg() -> Option<String> {
    unsafe {
        let ptr = traverse_last_error();
        if ptr.is_null() {
            None
        } else {
            Some(CStr::from_ptr(ptr).to_string_lossy().into_owned())
        }
    }
}

/// Helper: open a database in a temporary directory.
fn open_temp_db(dir: &tempfile::TempDir) -> *mut traverse_ffi::handles::TvDatabase {
    let path = dir.path().join("test.tvdb");
    let c_path = CString::new(path.to_str().unwrap()).unwrap();
    let db = unsafe { traverse_open(c_path.as_ptr()) };
    assert!(!db.is_null(), "failed to open db: {:?}", last_error_msg());
    db
}

/// Helper: read a string column from the current result row.
unsafe fn read_string(result: *mut traverse_ffi::handles::TvResult, col: i32) -> Option<String> {
    let ptr = unsafe { traverse_result_get_string(result, col) };
    if ptr.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}

/// Helper: read a type name from the current result row.
unsafe fn read_type(result: *const traverse_ffi::handles::TvResult, col: i32) -> Option<String> {
    let ptr = unsafe { traverse_result_get_type(result, col) };
    if ptr.is_null() {
        None
    } else {
        Some(
            unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}

// -------------------------------------------------------------------------
// Test 1: open -> query -> close
// -------------------------------------------------------------------------

#[test]
fn open_query_close() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp_db(&dir);

    // Begin transaction
    let tx = unsafe { traverse_begin(db) };
    assert!(!tx.is_null());

    // CREATE + RETURN
    let q = CString::new("CREATE (n:Person {name: 'Alice'}) RETURN n.name").unwrap();
    let result = unsafe { traverse_query(tx, q.as_ptr()) };
    assert!(!result.is_null(), "query failed: {:?}", last_error_msg());

    // Read result
    unsafe {
        assert_eq!(traverse_result_column_count(result), 1);
        assert_eq!(traverse_result_row_count(result), 1);
        assert!(traverse_result_next(result));
        let name = read_string(result, 0);
        assert_eq!(name.as_deref(), Some("Alice"));
        assert!(!traverse_result_next(result)); // no more rows
        traverse_result_free(result);
    }

    // Commit, flush, close
    unsafe {
        assert_eq!(traverse_commit(tx), TvError::TV_OK as i32);
        assert_eq!(traverse_flush(db), TvError::TV_OK as i32);
        assert_eq!(traverse_close(db), TvError::TV_OK as i32);
    }
}

// -------------------------------------------------------------------------
// Test 2: error on invalid query
// -------------------------------------------------------------------------

#[test]
fn error_on_invalid_query() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp_db(&dir);

    let tx = unsafe { traverse_begin(db) };
    assert!(!tx.is_null());

    let q = CString::new("CRETE (n) RETRUN n").unwrap(); // intentional typos
    let result = unsafe { traverse_query(tx, q.as_ptr()) };
    assert!(result.is_null(), "expected null result for syntax error");

    // Error should be set
    let code = traverse_last_error_code();
    assert_ne!(code, TvError::TV_OK as i32);
    let msg = last_error_msg();
    assert!(msg.is_some(), "expected error message");

    unsafe {
        traverse_abort(tx);
        traverse_close(db);
    }
}

// -------------------------------------------------------------------------
// Test 3: abort discards changes
// -------------------------------------------------------------------------

#[test]
fn abort_discards_changes() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp_db(&dir);

    // Create a node and abort
    let tx = unsafe { traverse_begin(db) };
    let q = CString::new("CREATE (n:Ghost {val: 1})").unwrap();
    let result = unsafe { traverse_query(tx, q.as_ptr()) };
    assert!(!result.is_null(), "query failed: {:?}", last_error_msg());
    unsafe { traverse_result_free(result) };
    unsafe { traverse_abort(tx) };

    // Begin new transaction - the node should not exist
    let tx2 = unsafe { traverse_begin(db) };
    let q2 = CString::new("MATCH (n:Ghost) RETURN n").unwrap();
    let result2 = unsafe { traverse_query(tx2, q2.as_ptr()) };
    assert!(!result2.is_null());
    unsafe {
        assert_eq!(traverse_result_row_count(result2), 0);
        traverse_result_free(result2);
        traverse_abort(tx2);
        traverse_close(db);
    }
}

// -------------------------------------------------------------------------
// Test 4: null pointer safety
// -------------------------------------------------------------------------

#[test]
fn null_pointer_safety() {
    unsafe {
        // Database functions
        assert!(traverse_open(std::ptr::null()).is_null());
        assert_eq!(
            traverse_flush(std::ptr::null_mut()),
            TvError::TV_ERROR_INVALID_ARG as i32
        );
        assert_eq!(
            traverse_close(std::ptr::null_mut()),
            TvError::TV_ERROR_INVALID_ARG as i32
        );

        // Transaction functions
        assert!(traverse_begin(std::ptr::null_mut()).is_null());
        assert_eq!(
            traverse_commit(std::ptr::null_mut()),
            TvError::TV_ERROR_INVALID_ARG as i32
        );
        assert_eq!(
            traverse_abort(std::ptr::null_mut()),
            TvError::TV_ERROR_INVALID_ARG as i32
        );

        // Query function
        assert!(traverse_query(std::ptr::null_mut(), std::ptr::null()).is_null());

        // Result functions
        assert_eq!(traverse_result_column_count(std::ptr::null()), 0);
        assert!(traverse_result_column_name(std::ptr::null(), 0).is_null());
        assert_eq!(traverse_result_row_count(std::ptr::null()), 0);
        assert!(!traverse_result_next(std::ptr::null_mut()));
        assert!(traverse_result_get_string(std::ptr::null_mut(), 0).is_null());
        assert_eq!(traverse_result_get_int(std::ptr::null(), 0), 0);
        assert_eq!(traverse_result_get_double(std::ptr::null(), 0), 0.0);
        assert!(!traverse_result_get_bool(std::ptr::null(), 0));
        assert!(traverse_result_is_null(std::ptr::null(), 0));
        assert!(traverse_result_get_type(std::ptr::null(), 0).is_null());

        // traverse_result_free with null is a safe no-op
        traverse_result_free(std::ptr::null_mut());
    }
}

// -------------------------------------------------------------------------
// Test 5: result type accessors
// -------------------------------------------------------------------------

#[test]
fn result_type_accessors() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp_db(&dir);
    let tx = unsafe { traverse_begin(db) };

    let q = CString::new("RETURN 42 AS i, 1.5 AS f, true AS b, null AS n, 'hello' AS s").unwrap();
    let result = unsafe { traverse_query(tx, q.as_ptr()) };
    assert!(!result.is_null(), "query failed: {:?}", last_error_msg());

    unsafe {
        assert!(traverse_result_next(result));

        // Integer
        assert_eq!(traverse_result_get_int(result, 0), 42);
        assert_eq!(read_type(result, 0).as_deref(), Some("INTEGER"));

        // Float
        assert!((traverse_result_get_double(result, 1) - 1.5).abs() < 1e-10);
        assert_eq!(read_type(result, 1).as_deref(), Some("FLOAT"));

        // Bool
        assert!(traverse_result_get_bool(result, 2));
        assert_eq!(read_type(result, 2).as_deref(), Some("BOOLEAN"));

        // Null
        assert!(traverse_result_is_null(result, 3));
        assert_eq!(read_type(result, 3).as_deref(), Some("NULL"));

        // String
        let s = read_string(result, 4);
        assert_eq!(s.as_deref(), Some("hello"));
        assert_eq!(read_type(result, 4).as_deref(), Some("STRING"));

        traverse_result_free(result);
    }

    unsafe {
        traverse_abort(tx);
        traverse_close(db);
    }
}

// -------------------------------------------------------------------------
// Test 6: multiple queries in same transaction
// -------------------------------------------------------------------------

#[test]
fn multiple_queries_same_tx() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp_db(&dir);
    let tx = unsafe { traverse_begin(db) };

    // First query: create a node
    let q1 = CString::new("CREATE (n:Item {name: 'Widget'})").unwrap();
    let r1 = unsafe { traverse_query(tx, q1.as_ptr()) };
    assert!(!r1.is_null());
    unsafe { traverse_result_free(r1) };

    // Second query: match the node we just created
    let q2 = CString::new("MATCH (n:Item) RETURN n.name").unwrap();
    let r2 = unsafe { traverse_query(tx, q2.as_ptr()) };
    assert!(!r2.is_null(), "second query failed: {:?}", last_error_msg());

    unsafe {
        assert_eq!(traverse_result_row_count(r2), 1);
        assert!(traverse_result_next(r2));
        let name = read_string(r2, 0);
        assert_eq!(name.as_deref(), Some("Widget"));
        traverse_result_free(r2);
    }

    unsafe {
        traverse_commit(tx);
        traverse_close(db);
    }
}

// -------------------------------------------------------------------------
// Test 7: persistence across reopen
// -------------------------------------------------------------------------

#[test]
fn persistence_across_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("persist.tvdb");
    let c_path = CString::new(path.to_str().unwrap()).unwrap();

    // Open, write, commit, flush, close
    unsafe {
        let db = traverse_open(c_path.as_ptr());
        assert!(!db.is_null(), "open failed: {:?}", last_error_msg());

        let tx = traverse_begin(db);
        let q = CString::new("CREATE (n:Durable {val: 99})").unwrap();
        let r = traverse_query(tx, q.as_ptr());
        assert!(!r.is_null());
        traverse_result_free(r);
        assert_eq!(traverse_commit(tx), TvError::TV_OK as i32);
        assert_eq!(traverse_flush(db), TvError::TV_OK as i32);
        assert_eq!(traverse_close(db), TvError::TV_OK as i32);
    }

    // Reopen and verify
    unsafe {
        let db = traverse_open(c_path.as_ptr());
        assert!(!db.is_null(), "reopen failed: {:?}", last_error_msg());

        let tx = traverse_begin(db);
        let q = CString::new("MATCH (n:Durable) RETURN n.val").unwrap();
        let r = traverse_query(tx, q.as_ptr());
        assert!(!r.is_null(), "query failed: {:?}", last_error_msg());

        assert_eq!(traverse_result_row_count(r), 1);
        assert!(traverse_result_next(r));
        assert_eq!(traverse_result_get_int(r, 0), 99);
        traverse_result_free(r);

        traverse_abort(tx);
        traverse_close(db);
    }
}

// -------------------------------------------------------------------------
// Handle lifetimes, read transactions, busy timeout, bulk loads
// -------------------------------------------------------------------------

/// Runs `query` on `tx` and returns the first column of the first row as an integer.
unsafe fn query_int(tx: *mut TvTransaction, query: &str) -> i64 {
    let q = CString::new(query).unwrap();
    let result = unsafe { traverse_query(tx, q.as_ptr()) };
    assert!(!result.is_null(), "{query}: {:?}", last_error_msg());
    unsafe {
        assert!(traverse_result_next(result));
        let v = traverse_result_get_int(result, 0);
        traverse_result_free(result);
        v
    }
}

unsafe fn count_nodes(db: *mut TvDatabase) -> i64 {
    unsafe {
        let tx = traverse_begin_read(db);
        assert!(!tx.is_null(), "{:?}", last_error_msg());
        let n = query_int(tx, "MATCH (n) RETURN count(n)");
        assert_eq!(traverse_commit(tx), TvError::TV_OK as i32);
        n
    }
}

#[test]
fn transactions_outlive_a_closed_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp_db(&dir);
    unsafe {
        let tx = traverse_begin(db);
        let q = CString::new("CREATE (:Lost)").unwrap();
        let r = traverse_query(tx, q.as_ptr());
        assert!(!r.is_null());
        traverse_result_free(r);
        let reader = traverse_begin_read(db);
        assert!(!reader.is_null());

        // closing rolls both back; their handles stay valid until freed
        assert_eq!(traverse_close(db), TvError::TV_OK as i32);
        assert!(traverse_query(tx, q.as_ptr()).is_null());
        assert_eq!(
            traverse_last_error_code(),
            TvError::TV_ERROR_TRANSACTION as i32
        );
        assert!(last_error_msg().unwrap().contains("closed"));
        assert_eq!(traverse_commit(tx), TvError::TV_ERROR_TRANSACTION as i32);
        assert_eq!(traverse_abort(reader), TvError::TV_OK as i32);
    }
    // the file was released and holds nothing
    let db = open_temp_db(&dir);
    unsafe {
        assert_eq!(count_nodes(db), 0);
        traverse_close(db);
    }
}

#[test]
fn readers_run_alongside_the_writer_on_their_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp_db(&dir);
    unsafe {
        let writer = traverse_begin(db);
        let q = CString::new("CREATE (:N)").unwrap();
        traverse_result_free(traverse_query(writer, q.as_ptr()));
        let reader = traverse_begin_read(db);
        assert_eq!(query_int(reader, "MATCH (n) RETURN count(n)"), 0);
        // writes are refused in a read transaction
        assert!(traverse_query(reader, q.as_ptr()).is_null());
        assert_eq!(traverse_commit(writer), TvError::TV_OK as i32);
        assert_eq!(
            query_int(reader, "MATCH (n) RETURN count(n)"),
            0,
            "snapshot"
        );
        assert_eq!(traverse_commit(reader), TvError::TV_OK as i32);
        assert_eq!(count_nodes(db), 1);
        traverse_close(db);
    }
}

#[test]
fn a_second_writer_fails_busy_after_the_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp_db(&dir);
    unsafe {
        assert_eq!(traverse_set_busy_timeout(db, 20), TvError::TV_OK as i32);
        let writer = traverse_begin(db);
        assert!(!writer.is_null());
        assert!(traverse_begin(db).is_null());
        assert_eq!(traverse_last_error_code(), TvError::TV_ERROR_BUSY as i32);
        assert!(traverse_bulk_begin(db).is_null());
        assert_eq!(traverse_last_error_code(), TvError::TV_ERROR_BUSY as i32);
        assert_eq!(traverse_flush(db), TvError::TV_ERROR_BUSY as i32);
        assert_eq!(traverse_abort(writer), TvError::TV_OK as i32);
        let again = traverse_begin(db);
        assert!(!again.is_null());
        assert_eq!(traverse_commit(again), TvError::TV_OK as i32);
        traverse_close(db);
    }
}

#[test]
fn a_transaction_commits_on_another_thread() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp_db(&dir);
    let tx = unsafe { traverse_begin(db) } as usize;
    std::thread::spawn(move || unsafe {
        let tx = tx as *mut TvTransaction;
        let q = CString::new("CREATE (:N)").unwrap();
        traverse_result_free(traverse_query(tx, q.as_ptr()));
        assert_eq!(traverse_commit(tx), TvError::TV_OK as i32);
    })
    .join()
    .unwrap();
    unsafe {
        assert_eq!(count_nodes(db), 1);
        traverse_close(db);
    }
}

#[test]
fn bulk_load_is_invisible_until_commit_and_abort_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let db = open_temp_db(&dir);
    let label = CString::new("Item").unwrap();
    let labels = [label.as_ptr()];
    let key = CString::new("i").unwrap();
    let keys = [key.as_ptr()];
    let next = CString::new("NEXT").unwrap();
    unsafe {
        let load = |n: i64| {
            let w = traverse_bulk_begin(db);
            assert!(!w.is_null(), "{:?}", last_error_msg());
            let mut prev = -1;
            for i in 0..n {
                let v = CString::new(i.to_string()).unwrap();
                let vals = [v.as_ptr()];
                let id = traverse_bulk_create_node(
                    w,
                    labels.as_ptr(),
                    1,
                    keys.as_ptr(),
                    vals.as_ptr(),
                    1,
                );
                assert!(id >= 0, "{:?}", last_error_msg());
                if prev >= 0 {
                    let e = traverse_bulk_create_edge(
                        w,
                        prev,
                        id,
                        next.as_ptr(),
                        std::ptr::null(),
                        std::ptr::null(),
                        0,
                    );
                    assert!(e >= 0, "{:?}", last_error_msg());
                }
                prev = id;
            }
            w
        };

        let w = load(50);
        assert_eq!(count_nodes(db), 0, "invisible before commit");
        assert_eq!(traverse_bulk_abort(w), TvError::TV_OK as i32);
        assert_eq!(count_nodes(db), 0, "rolled back");

        let w = load(50);
        assert_eq!(traverse_bulk_commit(w), TvError::TV_OK as i32);
        assert_eq!(count_nodes(db), 50);

        // a bulk writer left open when the database closes rolls back
        let w = load(10);
        assert_eq!(traverse_close(db), TvError::TV_OK as i32);
        assert_eq!(
            traverse_bulk_create_node(w, labels.as_ptr(), 1, std::ptr::null(), std::ptr::null(), 0),
            -1
        );
        assert_eq!(
            traverse_last_error_code(),
            TvError::TV_ERROR_TRANSACTION as i32
        );
        assert_eq!(traverse_bulk_abort(w), TvError::TV_OK as i32);
    }
    let db = open_temp_db(&dir);
    unsafe {
        assert_eq!(count_nodes(db), 50);
        let tx = traverse_begin_read(db);
        assert_eq!(query_int(tx, "MATCH ()-[r:NEXT]->() RETURN count(r)"), 49);
        traverse_commit(tx);
        traverse_close(db);
    }
}
