//! FFI functions for opening, flushing, and closing a Traverse database.

use std::ffi::CStr;
use std::os::raw::c_char;
use std::panic::AssertUnwindSafe;
use std::time::Duration;

use traverse_core::embedded::SharedDatabase;

use crate::error::{self, TvError};
use crate::handles::{DatabaseInner, TvDatabase};

/// Open (or create) a database at `path`.
///
/// Returns a non-null handle on success, or `NULL` on error.
/// On error, call [`traverse_last_error`](crate::error::traverse_last_error)
/// for details.
///
/// # Safety
///
/// `path` must be a valid, NUL-terminated UTF-8 string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_open(path: *const c_char) -> *mut TvDatabase {
    error::clear_last_error();
    crate::catch_panic(
        std::ptr::null_mut(),
        AssertUnwindSafe(|| {
            if path.is_null() {
                error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "path is null");
                return std::ptr::null_mut();
            }
            let c_str = unsafe { CStr::from_ptr(path) };
            let path_str = match c_str.to_str() {
                Ok(s) => s,
                Err(e) => {
                    error::set_last_error(
                        TvError::TV_ERROR_INVALID_ARG,
                        &format!("invalid UTF-8 in path: {e}"),
                    );
                    return std::ptr::null_mut();
                }
            };

            match SharedDatabase::open(path_str) {
                Ok(db) => {
                    let inner = Box::new(DatabaseInner {
                        db,
                        plans: Default::default(),
                    });
                    Box::into_raw(inner) as *mut TvDatabase
                }
                Err(e) => {
                    error::set_core_error(&e);
                    std::ptr::null_mut()
                }
            }
        }),
    )
}

/// Sets how long `traverse_begin`, `traverse_bulk_begin` and `traverse_flush` wait for an
/// active write transaction before failing with `TV_ERROR_BUSY`: `timeout_ms` milliseconds, or as
/// long as it takes when negative. The default is 5000.
///
/// Returns `TV_OK` (0).
///
/// # Safety
///
/// `db` must be a valid handle returned by [`traverse_open`] that has not been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_set_busy_timeout(db: *mut TvDatabase, timeout_ms: i64) -> i32 {
    error::clear_last_error();
    crate::catch_panic(
        TvError::TV_ERROR_INTERNAL as i32,
        AssertUnwindSafe(|| {
            if db.is_null() {
                error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "db handle is null");
                return TvError::TV_ERROR_INVALID_ARG as i32;
            }
            let inner = unsafe { &*(db as *const DatabaseInner) };
            let timeout = u64::try_from(timeout_ms).map_or(Duration::MAX, Duration::from_millis);
            inner.db.set_busy_timeout(timeout);
            TvError::TV_OK as i32
        }),
    )
}

/// Sets how long a statement may run before it stops with `TV_ERROR_INTERRUPTED`: `timeout_ms`
/// milliseconds, or no limit when zero or negative (the default). Applies to transactions begun
/// afterwards.
///
/// Returns `TV_OK` (0).
///
/// # Safety
///
/// `db` must be a valid handle returned by [`traverse_open`] that has not been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_set_statement_timeout(
    db: *mut TvDatabase,
    timeout_ms: i64,
) -> i32 {
    error::clear_last_error();
    crate::catch_panic(
        TvError::TV_ERROR_INTERNAL as i32,
        AssertUnwindSafe(|| {
            if db.is_null() {
                error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "db handle is null");
                return TvError::TV_ERROR_INVALID_ARG as i32;
            }
            let inner = unsafe { &*(db as *const DatabaseInner) };
            let timeout = u64::try_from(timeout_ms)
                .ok()
                .filter(|ms| *ms > 0)
                .map(Duration::from_millis);
            inner.db.set_statement_timeout(timeout);
            TvError::TV_OK as i32
        }),
    )
}

/// Flush dirty pages to disk. Waits at most the busy timeout for an active write transaction.
///
/// Returns `TV_OK` (0) on success, or an error code on failure (`TV_ERROR_BUSY` if a writer stayed
/// active).
///
/// # Safety
///
/// `db` must be a valid handle returned by [`traverse_open`] that has not
/// been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_flush(db: *mut TvDatabase) -> i32 {
    error::clear_last_error();
    crate::catch_panic(
        TvError::TV_ERROR_INTERNAL as i32,
        AssertUnwindSafe(|| {
            if db.is_null() {
                error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "db handle is null");
                return TvError::TV_ERROR_INVALID_ARG as i32;
            }
            let inner = unsafe { &*(db as *const DatabaseInner) };
            match inner.db.flush() {
                Ok(()) => TvError::TV_OK as i32,
                Err(e) => error::set_core_error(&e) as i32,
            }
        }),
    )
}

/// Close the database and free the handle: rolls back transactions and bulk loads still open on
/// it, saves, and releases the file.
///
/// Returns `TV_OK` (0) on success, or an error code on failure.
/// After this call the handle is invalid and must not be used again. Transaction and bulk-writer
/// handles stay valid: their calls fail with `TV_ERROR_TRANSACTION`, and commit/abort free them.
///
/// # Safety
///
/// `db` must be a valid handle returned by [`traverse_open`] that has not
/// been closed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn traverse_close(db: *mut TvDatabase) -> i32 {
    error::clear_last_error();
    crate::catch_panic(
        TvError::TV_ERROR_INTERNAL as i32,
        AssertUnwindSafe(|| {
            if db.is_null() {
                error::set_last_error(TvError::TV_ERROR_INVALID_ARG, "db handle is null");
                return TvError::TV_ERROR_INVALID_ARG as i32;
            }
            // Reclaim the Box so it is dropped at the end of this scope.
            let inner = unsafe { Box::from_raw(db as *mut DatabaseInner) };
            match inner.db.close() {
                Ok(()) => TvError::TV_OK as i32,
                Err(e) => error::set_core_error(&e) as i32,
            }
        }),
    )
}
