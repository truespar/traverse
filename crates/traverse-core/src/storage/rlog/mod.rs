//! Logical redo log (rlog): per-commit durability.
//!
//! The page WAL ([`crate::storage::wal`]) logs full **page after-images** to
//! make `Database::flush()` torn-write safe. The redo log instead records the
//! **logical operations** of each `WriteTx::commit()`: `CreateNode`,
//! `SetProperty`, `CreateEdge`, schema changes, ...
//!
//! # Why two logs
//!
//! Traverse is in-memory: without the redo log a committed transaction is only
//! durable once the next `flush()` writes it, so a `kill -9` between flushes
//! loses every commit since the last one. With it, each commit appends a small
//! batch to the active segment and is acked once that's synced; startup replays
//! every record with `lsn > recovery_lsn` onto the loaded `.tvdb`.
//!
//! # Design
//!
//! - 16 B record header + rkyv payload + per-record CRC trailer.
//! - Per-batch CRC in a fixed-format COMMIT record at every commit boundary.
//! - Pre-allocated 32 MiB segments; rotate on size, truncate on flush.
//! - Group commit: concurrent commits share one `fdatasync`.
//! - 8-byte record alignment; LSNs are global byte offsets across the stream.
//! - Payload strings carried verbatim; recovery re-interns through the normal
//!   Graph mutation APIs (no StringTable replay).
//!
//! ## .tvdb compatibility
//!
//! The log is anchored by `recovery_lsn` in the v6 `.tvdb` header.
//!
//! - **v5 -> v6**: automatic on the first flush. v5 reads as `recovery_lsn = 0`
//!   (replay everything) and is written back as v6.
//! - **v6 -> v5**: not supported; v5 readers reject the v6 version.
//! - **v1-v5 files on a v6 binary**: supported, `recovery_lsn` reads as 0 and
//!   any rlog segments replay from the start.
//!
//! ## Observability
//!
//! [`LogWriterMetrics`]: commits, data records, fdatasyncs, bytes written,
//! segment rotations. Read via [`LogWriter::metrics`].

pub mod record;
pub mod recovery;
#[cfg(not(target_arch = "wasm32"))]
pub mod replay;
pub mod segment;
pub mod writer;

#[cfg(not(target_arch = "wasm32"))]
pub use replay::apply_transactions;

pub use record::{
    COMMIT_PAYLOAD_SIZE, CommitPayload, MAX_PAYLOAD_SIZE, Op, RECORD_ALIGNMENT, RECORD_CRC_SIZE,
    RECORD_HEADER_SIZE, RecordHeader, RecordKind, decode_record, encode_record, record_padding,
    record_size,
};
pub use recovery::{RecoveryReport, RecoveryStatus, ReplayTransaction, StopReason, scan_segments};
pub use segment::{
    DEFAULT_SEGMENT_SIZE, MIN_SEGMENT_SIZE, SEGMENT_HEADER_SIZE, SEGMENT_MAGIC, SEGMENT_VERSION,
    SegmentHeader, SegmentReader, SegmentWriter, discover_segments, remove_segments,
    segment_filename, segment_path, segments_of,
};
pub use writer::{CommitOutcome, LogWriter, LogWriterMetrics, SyncControl, WriterOptions};
