//! FFI functions for transactions.

use std::panic::AssertUnwindSafe;

use std::sync::Arc;

use traverse_core::embedded::{Lease, SharedDatabase, Transaction};
use traverse_core::error::Result as CoreResult;
use traverse_cypher::embedded::{EmbeddedTransaction, StatementCanceller};

use crate::error::{self, TvError};
use crate::handles::{
    CancellerInner, DatabaseInner, TransactionInner, TvCanceller, TvDatabase, TvTransaction,
};

/// Boxes a transaction from `begin` on `db` into a handle; `NULL` on error.
unsafe fn begin_with(
    db: *mut TvDatabase,
    begin: impl FnOnce(&SharedDatabase) -> CoreResult<Lease<Transaction>>,
) -> *mut TvTransaction {
    if db.is_null() {
        error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "db handle is null");
        return std::ptr::null_mut();
    }
    let db_inner = unsafe { &*(db as *const DatabaseInner) };
    match begin(&db_inner.db) {
        Ok(lease) => Box::into_raw(Box::new(TransactionInner {
            lease,
            embedded: EmbeddedTransaction::on(&db_inner.db),
            plans: Arc::clone(&db_inner.plans),
        })) as *mut TvTransaction,
        Err(e) => {
            error::set_core_error(&e);
            std::ptr::null_mut()
        }
    }
}

/// Begin a write transaction on `db`.
///
/// Returns a non-null handle on success, or `NULL` on error. One write transaction is active at
/// a time: this waits for the active one at most the busy timeout (see
/// `traverse_set_busy_timeout`), then fails with `TV_ERROR_BUSY`.
///
/// # Safety
///
/// `db` must be a valid, non-null [`TvDatabase`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_begin(db: *mut TvDatabase) -> *mut TvTransaction {
    error::clear_last_error();
    crate::catch_panic(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| unsafe { begin_with(db, SharedDatabase::begin_write) }),
    )
}

/// Begin a read-only transaction on `db`: a snapshot of the database as of now. Never waits,
/// runs alongside the writer and other readers; writes fail with `TV_ERROR_TRANSACTION`.
/// Finish it with [`traverse_commit`] or [`traverse_abort`] (same effect).
///
/// Returns a non-null handle on success, or `NULL` on error.
///
/// # Safety
///
/// `db` must be a valid, non-null [`TvDatabase`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_begin_read(db: *mut TvDatabase) -> *mut TvTransaction {
    error::clear_last_error();
    crate::catch_panic(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| unsafe { begin_with(db, SharedDatabase::begin_read) }),
    )
}

/// Commit the transaction and free the handle.
///
/// Returns `TV_OK` (0) on success.  After this call the handle is invalid.
///
/// # Safety
///
/// `tx` must be a valid handle returned by [`traverse_begin`] or [`traverse_begin_read`] that has
/// not been committed or aborted, and no other call may be using it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_commit(tx: *mut TvTransaction) -> i32 {
    error::clear_last_error();
    crate::catch_panic(
        TvError::TV_ERROR_INTERNAL as i32,
        AssertUnwindSafe(|| {
            if tx.is_null() {
                error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "tx handle is null");
                return TvError::TV_ERROR_INVALID_ARG as i32;
            }
            let inner = unsafe { Box::from_raw(tx as *mut TransactionInner) };
            // terminated (TERMINATE TRANSACTIONS): rolled back instead
            if let Err(e) = inner.embedded.check_live() {
                let _ = inner.lease.finish(Transaction::abort);
                return error::set_cypher_error(&e) as i32;
            }
            match inner.lease.finish(Transaction::commit).and_then(|r| r) {
                Ok(()) => TvError::TV_OK as i32,
                Err(e) => error::set_core_error(&e) as i32,
            }
        }),
    )
}

/// Abort the transaction, discarding all changes, and free the handle.
///
/// Returns `TV_OK` (0).  After this call the handle is invalid.
///
/// # Safety
///
/// `tx` must be a valid handle returned by [`traverse_begin`] or [`traverse_begin_read`] that has
/// not been committed or aborted, and no other call may be using it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_abort(tx: *mut TvTransaction) -> i32 {
    error::clear_last_error();
    crate::catch_panic(
        TvError::TV_ERROR_INTERNAL as i32,
        AssertUnwindSafe(|| {
            if tx.is_null() {
                error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "tx handle is null");
                return TvError::TV_ERROR_INVALID_ARG as i32;
            }
            let inner = unsafe { Box::from_raw(tx as *mut TransactionInner) };
            // finished or rolled back by a close already: nothing left to undo
            let _ = inner.lease.finish(Transaction::abort);
            TvError::TV_OK as i32
        }),
    )
}

/// A canceller for `tx`: stops the statement `tx` is running, from any thread, through
/// [`traverse_cancel`]. It stays valid after the transaction ends (a cancel then does nothing);
/// free it with [`traverse_canceller_free`].
///
/// Returns a non-null handle, or `NULL` on error.
///
/// # Safety
///
/// `tx` must be a valid, open [`TvTransaction`] handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_canceller(tx: *mut TvTransaction) -> *mut TvCanceller {
    error::clear_last_error();
    crate::catch_panic(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| {
            if tx.is_null() {
                error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "tx handle is null");
                return std::ptr::null_mut();
            }
            let inner = unsafe { &*(tx as *const TransactionInner) };
            Box::into_raw(Box::new(CancellerInner::Transaction(
                inner.embedded.handle(),
            ))) as *mut TvCanceller
        }),
    )
}

/// A canceller for one auto-commit statement: pass it to [`traverse_execute`] and call
/// [`traverse_cancel`] from another thread to stop that statement, whether it already runs or
/// starts later. It stays cancelled; free it with [`traverse_canceller_free`].
///
/// Returns a non-null handle.
///
/// [`traverse_execute`]: crate::query::traverse_execute
#[unsafe(no_mangle)]
pub extern "C" fn traverse_canceller_new() -> *mut TvCanceller {
    error::clear_last_error();
    Box::into_raw(Box::new(CancellerInner::Statement(
        StatementCanceller::new(),
    ))) as *mut TvCanceller
}

/// Stops the statement the canceller targets: its transaction's running statement
/// ([`traverse_canceller`]), or its auto-commit statement ([`traverse_canceller_new`]), which
/// fails with `TV_ERROR_INTERRUPTED` at its next check (within milliseconds). A statement that had
/// already changed data leaves its explicit transaction failed, to be rolled back; an auto-commit
/// statement rolls back. Thread-safe; a transaction's canceller does nothing when no statement
/// runs.
///
/// Returns `TV_OK` (0).
///
/// # Safety
///
/// `canceller` must be a valid handle from [`traverse_canceller`] not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_cancel(canceller: *const TvCanceller) -> i32 {
    error::clear_last_error();
    crate::catch_panic(
        TvError::TV_ERROR_INTERNAL as i32,
        AssertUnwindSafe(|| {
            if canceller.is_null() {
                error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "canceller handle is null");
                return TvError::TV_ERROR_INVALID_ARG as i32;
            }
            match unsafe { &*(canceller as *const CancellerInner) } {
                CancellerInner::Transaction(handle) => traverse_cypher::embedded::cancel(handle),
                CancellerInner::Statement(statement) => statement.cancel(),
            }
            TvError::TV_OK as i32
        }),
    )
}

/// Frees a canceller. `NULL` is a no-op.
///
/// # Safety
///
/// `canceller` must be a handle from [`traverse_canceller`] not yet freed, and no other call may
/// be using it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_canceller_free(canceller: *mut TvCanceller) {
    if canceller.is_null() {
        return;
    }
    crate::catch_panic(
        (),
        AssertUnwindSafe(|| {
            drop(unsafe { Box::from_raw(canceller as *mut CancellerInner) });
        }),
    )
}
