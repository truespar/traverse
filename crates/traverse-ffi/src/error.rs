//! Thread-local error reporting for the FFI layer.
//!
//! Every exported FFI function calls `clear_last_error` on entry and, on
//! failure, stores an error code + human-readable message via
//! `set_last_error`.  The C caller can retrieve them with
//! [`traverse_last_error`] and [`traverse_last_error_code`], and a statement's
//! GQLSTATUS with [`traverse_last_error_gql_status`].

use std::cell::RefCell;
use std::ffi::CString;
use std::os::raw::c_char;

use traverse_core::error::Error as CoreError;
use traverse_cypher::error::CypherError;
use traverse_cypher::status::GqlStatus;

// ---------------------------------------------------------------------------
// Error codes
// ---------------------------------------------------------------------------

/// Error codes returned by FFI functions and retrievable via
/// [`traverse_last_error_code`].
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(non_camel_case_types)]
pub enum TvError {
    /// No error.
    TV_OK = 0,
    /// An I/O error occurred.
    TV_ERROR_IO = 1,
    /// The database file is corrupt.
    TV_ERROR_CORRUPT = 2,
    /// The database file is locked by another process.
    TV_ERROR_LOCKED = 3,
    /// Cypher syntax error.
    TV_ERROR_SYNTAX = 4,
    /// Cypher type error.
    TV_ERROR_TYPE = 5,
    /// Cypher semantic error.
    TV_ERROR_SEMANTIC = 6,
    /// Graph-level error (node/edge not found, etc.).
    TV_ERROR_GRAPH = 7,
    /// Transaction error (already finished, write-conflict, read-only).
    TV_ERROR_TRANSACTION = 8,
    /// Invalid argument passed to an FFI function.
    TV_ERROR_INVALID_ARG = 9,
    /// An internal error (e.g. a caught panic).
    TV_ERROR_INTERNAL = 10,
    /// A resource limit was exceeded (memory budget, max readers, etc.).
    TV_ERROR_RESOURCE_EXHAUSTED = 11,
    /// Another write transaction held the writer lock for the whole busy timeout (see
    /// `traverse_set_busy_timeout`). Transient: retry.
    TV_ERROR_BUSY = 12,
    /// The statement was stopped before it finished: its time limit passed, it was cancelled,
    /// or its transaction was terminated (`TERMINATE TRANSACTIONS`).
    TV_ERROR_INTERRUPTED = 13,
}

// ---------------------------------------------------------------------------
// Thread-local storage
// ---------------------------------------------------------------------------

/// The last error on this thread.
struct LastError {
    code: TvError,
    message: CString,
    /// A statement's GQLSTATUS.
    gql_status: Option<CString>,
}

thread_local! {
    static LAST_ERROR: RefCell<Option<LastError>> = const { RefCell::new(None) };
}

/// Clear the last error.  Called at the top of every FFI function.
pub(crate) fn clear_last_error() {
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = None;
    });
}

/// Store an error code and a human-readable message.
pub(crate) fn set_last_error(code: TvError, message: &str) {
    let msg = CString::new(message)
        .unwrap_or_else(|_| CString::new("(error message contained a NUL byte)").unwrap());
    LAST_ERROR.with(|e| {
        *e.borrow_mut() = Some(LastError {
            code,
            message: msg,
            gql_status: None,
        });
    });
}

/// Adds the GQLSTATUS to the error just set.
fn set_gql_status(status: GqlStatus) {
    LAST_ERROR.with(|e| {
        if let Some(last) = e.borrow_mut().as_mut() {
            last.gql_status = CString::new(status.code()).ok();
        }
    });
}

/// Map a [`traverse_core::error::Error`] to an [`TvError`] and store it.
pub(crate) fn set_core_error(err: &CoreError) -> TvError {
    let code = match err {
        CoreError::Io(_) => TvError::TV_ERROR_IO,
        CoreError::DatabaseLocked(_) => TvError::TV_ERROR_LOCKED,
        CoreError::InvalidFormat(_)
        | CoreError::UnsupportedVersion(_)
        | CoreError::ChecksumMismatch { .. }
        | CoreError::HeaderChecksumMismatch
        | CoreError::WalCorrupted(_)
        | CoreError::WalRecoveryFailed(_) => TvError::TV_ERROR_CORRUPT,
        CoreError::ReadOnlyTransaction
        | CoreError::TransactionFinished
        | CoreError::TransactionFailed
        | CoreError::DatabaseClosed
        | CoreError::TransactionConflict(_) => TvError::TV_ERROR_TRANSACTION,
        CoreError::Busy(_) => TvError::TV_ERROR_BUSY,
        CoreError::TooManyReaders(_)
        | CoreError::MemoryLimitExceeded { .. }
        | CoreError::AllocationFailed(_) => TvError::TV_ERROR_RESOURCE_EXHAUSTED,
        _ => TvError::TV_ERROR_GRAPH,
    };
    set_last_error(code, &err.to_string());
    set_gql_status(GqlStatus::of_core(err));
    code
}

/// Map a [`CypherError`] to an [`TvError`] and store it.
pub(crate) fn set_cypher_error(err: &CypherError) -> TvError {
    let code = set_cypher_error_code(err);
    set_gql_status(err.status());
    code
}

fn set_cypher_error_code(err: &CypherError) -> TvError {
    match err {
        CypherError::SyntaxError { .. } => {
            set_last_error(TvError::TV_ERROR_SYNTAX, &err.to_string());
            TvError::TV_ERROR_SYNTAX
        }
        CypherError::TypeError { .. } => {
            set_last_error(TvError::TV_ERROR_TYPE, &err.to_string());
            TvError::TV_ERROR_TYPE
        }
        CypherError::SemanticError { .. } => {
            set_last_error(TvError::TV_ERROR_SEMANTIC, &err.to_string());
            TvError::TV_ERROR_SEMANTIC
        }
        CypherError::RuntimeError { .. } => {
            set_last_error(TvError::TV_ERROR_INTERNAL, &err.to_string());
            TvError::TV_ERROR_INTERNAL
        }
        CypherError::GraphError(core_err) => set_core_error(core_err),
        CypherError::Interrupted { .. } => {
            set_last_error(TvError::TV_ERROR_INTERRUPTED, &err.to_string());
            TvError::TV_ERROR_INTERRUPTED
        }
    }
}

// ---------------------------------------------------------------------------
// FFI exports
// ---------------------------------------------------------------------------

/// Returns a pointer to the last error message, or `NULL` if no error has
/// occurred since the last FFI call.  The pointer is valid until the next
/// FFI call **on the same thread**.
#[unsafe(no_mangle)]
pub extern "C" fn traverse_last_error() -> *const c_char {
    LAST_ERROR.with(|e| {
        let borrow = e.borrow();
        match borrow.as_ref() {
            Some(last) => last.message.as_ptr(),
            None => std::ptr::null(),
        }
    })
}

/// Returns the last error code, or `TV_OK` (0) if no error has occurred.
#[unsafe(no_mangle)]
pub extern "C" fn traverse_last_error_code() -> i32 {
    LAST_ERROR.with(|e| {
        let borrow = e.borrow();
        match borrow.as_ref() {
            Some(last) => last.code as i32,
            None => TvError::TV_OK as i32,
        }
    })
}

/// Returns the last error's GQLSTATUS (five characters, ISO/IEC 39075: `22012` division by zero,
/// `42N62` variable not defined, `50N42` for an unexpected engine error, ...), or `NULL` when the
/// error has none (an invalid argument). The pointer is valid until the next FFI call **on the
/// same thread**.
#[unsafe(no_mangle)]
pub extern "C" fn traverse_last_error_gql_status() -> *const c_char {
    LAST_ERROR.with(|e| {
        let borrow = e.borrow();
        match borrow.as_ref().and_then(|last| last.gql_status.as_ref()) {
            Some(status) => status.as_ptr(),
            None => std::ptr::null(),
        }
    })
}
