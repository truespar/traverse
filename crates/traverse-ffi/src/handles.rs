//! Opaque handle types for FFI.
//!
//! The `*Inner` structs are heap-allocated behind raw pointers.  C callers
//! only ever see `*mut TvDatabase` etc., which are zero-sized marker types
//! that cbindgen turns into forward-declared C structs.
//!
//! Transaction and bulk-writer handles hold the database themselves, so they stay valid after
//! their database handle is closed: the close rolls them back, later calls fail with
//! `TV_ERROR_TRANSACTION`, and commit/abort still free them.

use std::collections::HashMap;
use std::ffi::CString;
use std::sync::Arc;

use traverse_core::embedded::{Lease, SharedDatabase, Transaction};
use traverse_core::storage::database::BulkLoadTx;
use traverse_cypher::embedded::{EmbeddedTransaction, StatementCanceller};
use traverse_cypher::plan_cache::PlanCache;
use traverse_cypher::transactions::TransactionHandle;
use traverse_cypher::types::CypherValue;

// ---------------------------------------------------------------------------
// Internal (Rust-side) handle payloads
// ---------------------------------------------------------------------------

/// The database behind a `TvDatabase` handle.
pub(crate) struct DatabaseInner {
    pub db: SharedDatabase,
    /// Plans shared by every statement on the database.
    pub plans: Arc<PlanCache>,
}

/// The bulk load behind a `TvBulkWriter` handle.
pub(crate) struct BulkWriterInner {
    pub lease: Lease<BulkLoadTx>,
}

/// The transaction behind a `TvTransaction` handle.
pub(crate) struct TransactionInner {
    pub lease: Lease<Transaction>,
    /// Its place in the transaction registry and its statement timeout.
    pub embedded: EmbeddedTransaction,
    /// The database's plan cache.
    pub plans: Arc<PlanCache>,
}

/// What a `TvCanceller` stops. Outlives the transaction or statement.
pub(crate) enum CancellerInner {
    /// A transaction's running statement (`traverse_canceller`).
    Transaction(TransactionHandle),
    /// One auto-commit statement, before or while it runs (`traverse_canceller_new`).
    Statement(StatementCanceller),
}

/// Holds the result of a Cypher query for row-by-row iteration.
pub(crate) struct ResultInner {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<CypherValue>>,
    /// Current cursor position.  `-1` means "before the first row".
    pub cursor: i64,
    /// Per-cell string cache for the *current* row, lazily populated by
    /// `traverse_result_get_string`.  Reset on each `result_next`.
    pub string_cache: Vec<Option<CString>>,
    /// Cached CStrings for column names (valid for the lifetime of the result).
    pub column_name_cache: Vec<CString>,
    /// Per-cell string cache for graph accessor functions (node labels, prop
    /// keys/values, edge type names).  Keyed by `(col, sub_index)` where
    /// sub_index disambiguates different strings within the same cell.
    /// Reset on each `result_next`.
    pub graph_string_cache: HashMap<(usize, usize), CString>,
}

// ---------------------------------------------------------------------------
// Opaque C-visible marker types
// ---------------------------------------------------------------------------

/// Opaque handle representing an open Traverse database.
#[repr(C)]
pub struct TvDatabase {
    _opaque: [u8; 0],
}

/// Opaque handle representing an active bulk writer.
#[repr(C)]
pub struct TvBulkWriter {
    _opaque: [u8; 0],
}

/// Opaque handle representing an active write transaction.
#[repr(C)]
pub struct TvTransaction {
    _opaque: [u8; 0],
}

/// Opaque handle representing a query result set.
#[repr(C)]
pub struct TvResult {
    _opaque: [u8; 0],
}

/// Opaque handle that stops a running statement from any thread: a transaction's
/// (`traverse_canceller`) or one auto-commit statement's (`traverse_canceller_new`).
#[repr(C)]
pub struct TvCanceller {
    _opaque: [u8; 0],
}
