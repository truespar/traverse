use crate::graph::types::{EdgeId, NodeId};

/// Graph and storage errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    // -- Graph errors --------------------------------------------------
    /// Node not found.
    #[error("node {0} not found")]
    NodeNotFound(NodeId),

    /// Edge not found.
    #[error("edge {0} not found")]
    EdgeNotFound(EdgeId),

    /// Can't delete a node that still has edges.
    #[error("node {0} still has edges attached")]
    NodeHasEdges(NodeId),

    /// Node already has this label.
    #[error("node {0} already has label \"{1}\"")]
    DuplicateLabel(NodeId, String),

    /// Node doesn't have this label.
    #[error("node {0} does not have label \"{1}\"")]
    LabelNotFound(NodeId, String),

    /// Edge source node not found.
    #[error("source node {0} not found")]
    SourceNodeNotFound(NodeId),

    /// Edge target node not found.
    #[error("target node {0} not found")]
    TargetNodeNotFound(NodeId),

    // -- Storage errors ------------------------------------------------
    /// I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Not a valid TVDB header.
    #[error("invalid format: {0}")]
    InvalidFormat(String),

    /// File format version unsupported by this build.
    #[error("unsupported format version: {0}")]
    UnsupportedVersion(u32),

    /// Page CRC32 doesn't match its data.
    #[error("checksum mismatch on page {page_id}: expected {expected:#010x}, got {actual:#010x}")]
    ChecksumMismatch {
        /// Page that failed verification.
        page_id: u64,
        /// Checksum stored in the page header.
        expected: u32,
        /// Checksum computed from the data.
        actual: u32,
    },

    /// File header CRC32 mismatch.
    #[error("file header checksum mismatch")]
    HeaderChecksumMismatch,

    /// Not enough free space in the page for the record.
    #[error("page {0} is full")]
    PageFull(u64),

    /// Page ID doesn't exist in the file.
    #[error("page {0} not found")]
    PageNotFound(u64),

    /// (De)serialization error.
    #[error("serialization error: {0}")]
    Serialization(String),

    /// Decompression error.
    #[error("decompression error: {0}")]
    Decompression(String),

    /// Database file locked by another process.
    #[error("database is locked: {}", _0.display())]
    DatabaseLocked(std::path::PathBuf),

    /// Record exceeds the max page capacity.
    #[error("record too large: {size} bytes exceeds page capacity of {capacity} bytes")]
    RecordTooLarge {
        /// Record size.
        size: usize,
        /// Max record capacity per page.
        capacity: usize,
    },

    /// Slot is tombstoned (deleted).
    #[error("slot {slot_idx} on page {page_id} is tombstoned")]
    SlotTombstoned {
        /// Page holding the slot.
        page_id: u64,
        /// Tombstoned slot index.
        slot_idx: u16,
    },

    /// Corrupt WAL (bad magic, checksum, or truncated).
    #[error("WAL corrupted: {0}")]
    WalCorrupted(String),

    /// WAL replay into the main file failed.
    #[error("WAL recovery failed: {0}")]
    WalRecoveryFailed(String),

    /// Index operation failed.
    #[error("index error: {0}")]
    IndexError(String),

    /// Constraint operation failed.
    #[error("constraint error: {0}")]
    ConstraintError(String),

    /// Mutation violates a constraint.
    #[error("constraint violation: {message}")]
    ConstraintViolation {
        /// Which rule it broke.
        kind: ViolationKind,
        /// What happened.
        message: String,
    },

    // -- Transaction errors ----------------------------------------
    /// Write on a read-only transaction.
    #[error("read-only transaction")]
    ReadOnlyTransaction,

    /// Transaction already committed or aborted.
    #[error("transaction already finished")]
    TransactionFinished,

    /// A concurrent transaction committed a conflicting change first. Transient: retrying the
    /// transaction on a fresh snapshot can succeed.
    #[error("transaction conflict: {0}")]
    TransactionConflict(String),

    /// Another write transaction kept the writer lock for the whole wait. Transient: retry later
    /// or wait longer.
    #[error("database is busy: another write transaction held the writer lock for {} ms", .0.as_millis())]
    Busy(std::time::Duration),

    /// The database was closed; transactions still open on it were rolled back.
    #[error("database is closed")]
    DatabaseClosed,

    /// Every reader registry slot is taken.
    #[error("too many concurrent readers (max {0})")]
    TooManyReaders(usize),

    /// An allocation sized by data from outside (a file) couldn't be made.
    #[error("could not allocate {0} bytes")]
    AllocationFailed(usize),

    /// A statement in this transaction failed after changing data. Its partial changes can't be
    /// kept, so the transaction can only be rolled back.
    #[error("the transaction failed: a statement in it failed after changing data; roll it back")]
    TransactionFailed,

    /// Not available on this kind of transaction.
    #[error("{0}")]
    Unsupported(&'static str),

    /// Operation would exceed the memory budget.
    #[error("memory limit exceeded: using {}, limit is {}", format_bytes(*.current), format_bytes(*.limit))]
    MemoryLimitExceeded {
        /// Current estimated usage in bytes.
        current: usize,
        /// Configured limit in bytes.
        limit: usize,
    },
}

fn format_bytes(bytes: usize) -> String {
    const GB: usize = 1024 * 1024 * 1024;
    const MB: usize = 1024 * 1024;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    }
}

/// The rule an [`Error::ConstraintViolation`] broke.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViolationKind {
    /// Uniqueness (or a key): another node or relationship has the same values.
    Uniqueness,
    /// Existence (or a key): a required property is missing.
    Existence,
    /// A property type constraint.
    Type,
}

impl Error {
    /// A [`ConstraintViolation`](Error::ConstraintViolation).
    pub fn constraint_violation(kind: ViolationKind, message: impl Into<String>) -> Self {
        Error::ConstraintViolation {
            kind,
            message: message.into(),
        }
    }
}

/// `Result` for graph and storage operations.
pub type Result<T> = std::result::Result<T, Error>;
