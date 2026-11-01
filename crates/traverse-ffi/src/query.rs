//! FFI functions for executing queries (openCypher and GQL).

use std::collections::HashMap;
use std::ffi::{CStr, CString};
use std::os::raw::c_char;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use traverse_cypher::error::CypherError;
use traverse_cypher::executor::CypherResult;

use crate::error::{self, TvError};
use crate::handles::{
    CancellerInner, DatabaseInner, ResultInner, TransactionInner, TvCanceller, TvDatabase,
    TvResult, TvTransaction,
};

/// The query text, or `None` with the error set.
unsafe fn query_text<'a>(query: *const c_char) -> Option<&'a str> {
    if query.is_null() {
        error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "query string is null");
        return None;
    }
    match unsafe { CStr::from_ptr(query) }.to_str() {
        Ok(s) => Some(s),
        Err(e) => {
            error::set_last_error(
                TvError::TV_ERROR_INVALID_ARG,
                &format!("invalid UTF-8 in query: {e}"),
            );
            None
        }
    }
}

/// A result handle for `result`, or `NULL` with the error set.
fn box_result(result: Result<CypherResult, CypherError>) -> *mut TvResult {
    match result {
        Ok(result) => {
            let col_count = result.columns.len();
            let column_name_cache = result
                .columns
                .iter()
                .map(|c| CString::new(c.as_str()).unwrap_or_default())
                .collect();
            let inner = Box::new(ResultInner {
                columns: result.columns,
                rows: result.rows,
                cursor: -1,
                string_cache: vec![None; col_count],
                column_name_cache,
                graph_string_cache: HashMap::new(),
            });
            Box::into_raw(inner) as *mut TvResult
        }
        Err(e) => {
            error::set_cypher_error(&e);
            std::ptr::null_mut()
        }
    }
}

/// Shared body of `traverse_query` / `traverse_query_gql` - validate
/// the handles, run the query in the requested dialect, box the result.
unsafe fn run_query(tx: *mut TvTransaction, query: *const c_char, use_gql: bool) -> *mut TvResult {
    if tx.is_null() {
        error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "tx handle is null");
        return std::ptr::null_mut();
    }
    let Some(query_str) = (unsafe { query_text(query) }) else {
        return std::ptr::null_mut();
    };
    let tx_inner = unsafe { &*(tx as *const TransactionInner) };
    let params = Arc::new(HashMap::new());
    let result = tx_inner.lease.with(|tx| {
        tx.statement(|view| {
            // listed, limited by the statement timeout, stoppable by `traverse_cancel`
            tx_inner.embedded.run(query_str, None, |_| {
                if use_gql {
                    traverse_gql::execute_gql_on(
                        view,
                        tx_inner.embedded.database(),
                        query_str,
                        &params,
                    )
                } else {
                    traverse_cypher::plan_cache::execute(&tx_inner.plans, view, query_str, &params)
                }
            })
        })
    });
    match result {
        Ok(result) => box_result(result),
        Err(e) => {
            error::set_core_error(&e);
            std::ptr::null_mut()
        }
    }
}

/// Shared body of `traverse_execute` / `traverse_execute_gql`.
unsafe fn run_auto_commit(
    db: *mut TvDatabase,
    query: *const c_char,
    canceller: *const TvCanceller,
    use_gql: bool,
) -> *mut TvResult {
    if db.is_null() {
        error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "db handle is null");
        return std::ptr::null_mut();
    }
    let Some(query_str) = (unsafe { query_text(query) }) else {
        return std::ptr::null_mut();
    };
    let canceller = if canceller.is_null() {
        None
    } else {
        match unsafe { &*(canceller as *const CancellerInner) } {
            CancellerInner::Statement(statement) => Some(statement),
            CancellerInner::Transaction(_) => {
                error::set_last_error(
                    TvError::TV_ERROR_INVALID_ARG,
                    "a transaction's canceller can't stop an auto-commit statement: make one with \
                     traverse_canceller_new",
                );
                return std::ptr::null_mut();
            }
        }
    };
    let db_inner = unsafe { &*(db as *const DatabaseInner) };
    let params = Arc::new(HashMap::new());
    box_result(if use_gql {
        traverse_cypher::embedded::auto_commit(
            &db_inner.db,
            query_str,
            None,
            canceller,
            |_| traverse_gql::query_writes(query_str),
            |view| traverse_gql::execute_gql_on(view, &db_inner.db.name(), query_str, &params),
        )
    } else {
        traverse_cypher::embedded::execute_auto_commit(
            &db_inner.db,
            &db_inner.plans,
            query_str,
            &params,
            canceller,
        )
    })
}

/// Runs an openCypher query as a transaction of its own (auto-commit): a read on a snapshot, so
/// it never waits; a write in a write transaction that commits when the query succeeds and rolls
/// back when it fails, waiting at most the busy timeout for the writer lock, in which
/// `CALL { ... } IN TRANSACTIONS` commits each batch. `canceller` (`NULL`, or a handle from
/// [`traverse_canceller_new`](crate::transaction::traverse_canceller_new)) stops it from another
/// thread.
///
/// Returns a non-null [`TvResult`] handle on success, or `NULL` on error.
///
/// # Safety
///
/// - `db` must be a valid, non-null [`TvDatabase`] handle.
/// - `query` must be a valid, NUL-terminated UTF-8 string.
/// - `canceller` must be `NULL` or a canceller handle not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_execute(
    db: *mut TvDatabase,
    query: *const c_char,
    canceller: *const TvCanceller,
) -> *mut TvResult {
    error::clear_last_error();
    crate::catch_panic(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| unsafe { run_auto_commit(db, query, canceller, false) }),
    )
}

/// [`traverse_execute`] for an ISO/IEC 39075 GQL query.
///
/// Returns a non-null [`TvResult`] handle on success, or `NULL` on error.
///
/// # Safety
///
/// As for [`traverse_execute`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_execute_gql(
    db: *mut TvDatabase,
    query: *const c_char,
    canceller: *const TvCanceller,
) -> *mut TvResult {
    error::clear_last_error();
    crate::catch_panic(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| unsafe { run_auto_commit(db, query, canceller, true) }),
    )
}

/// Execute an openCypher query within the given transaction.
///
/// Returns a non-null [`TvResult`] handle on success, or `NULL` on error.
///
/// # Safety
///
/// - `tx` must be a valid, non-null [`TvTransaction`] handle.
/// - `query` must be a valid, NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_query(
    tx: *mut TvTransaction,
    query: *const c_char,
) -> *mut TvResult {
    error::clear_last_error();
    crate::catch_panic(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| unsafe { run_query(tx, query, false) }),
    )
}

/// Execute an ISO/IEC 39075 GQL query within the given transaction.
///
/// The query is parsed by the GQL front-end and lowered into the same
/// engine `traverse_query` uses - identical planning and execution,
/// only the surface language differs.
///
/// Returns a non-null [`TvResult`] handle on success, or `NULL` on error.
///
/// # Safety
///
/// - `tx` must be a valid, non-null [`TvTransaction`] handle.
/// - `query` must be a valid, NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_query_gql(
    tx: *mut TvTransaction,
    query: *const c_char,
) -> *mut TvResult {
    error::clear_last_error();
    crate::catch_panic(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| unsafe { run_query(tx, query, true) }),
    )
}
