//! Recovery: scans redo-log segments into replayable transactions in commit order.
//!
//! # Algorithm
//!
//! Per segment, in segment_id order:
//!
//! 1. Validate the segment header (magic, version, CRC).
//! 2. Walk records in order:
//!    - Decode the header; a bad header CRC or length ends the valid region.
//!    - Verify the per-record CRC over `header + payload + pad`.
//!    - Buffer DATA records per transaction.
//!    - On COMMIT: verify the batch CRC over the bytes since the previous
//!      COMMIT. Both good -> emit a [`ReplayTransaction`], clear the buffer.
//!      Either bad -> **stop this segment and all later ones** (tail-only leniency).
//! 3. Stop at invalid bytes, the zero-padded region, or the end of the records.
//!
//! # Tail vs middle
//!
//! **Strict in the middle, lenient at the tail.** After the first failed record,
//! everything later in this and later segments is dropped, even if it would
//! validate on its own. Same as RocksDB's `kPointInTimeRecovery`; avoids
//! replaying a transaction whose causal predecessors were lost.
//!
//! A failed segment-header CRC isn't tail corruption, the file is unreadable:
//! recovery returns what it scanned so far plus a `RecoveryStatus::StoppedAtSegment` status
//! showing where, and the caller decides whether to refuse to open.

use std::path::{Path, PathBuf};

use super::record::{
    COMMIT_PAYLOAD_SIZE, CommitPayload, Op, RECORD_HEADER_SIZE, RecordKind, decode_record,
};
use super::segment::{SegmentHeader, SegmentReader, discover_segments};
use crate::error::{Error, Result};

/// One recovered transaction, ready to replay onto the in-memory graph.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplayTransaction {
    /// Transaction ID from the COMMIT record.
    pub tx_id: u64,
    /// Encoded `HybridTimestamp` from the COMMIT record.
    pub commit_ts: u64,
    /// Global LSN of this transaction's COMMIT record (its first byte); update `recovery_lsn` with
    /// it once applied.
    pub commit_lsn: u64,
    /// Ops to replay, in log order.
    pub ops: Vec<Op>,
}

/// Why the scan stopped early, if it did.
#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    /// Natural end of the readable region (zero-padded tail).
    EndOfData,
    /// Malformed record header (bad kind tag, oversized payload_len, ...).
    InvalidHeader(String),
    /// Per-record CRC mismatch.
    RecordCrcMismatch,
    /// COMMIT batch CRC doesn't match the preceding DATA records.
    BatchCrcMismatch,
    /// Record length runs past the readable region.
    Truncated,
    /// Unreadable segment header; segment skipped.
    BadSegmentHeader(String),
}

/// Recovery scan status.
#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryStatus {
    /// Every byte of every segment parsed cleanly.
    Clean,
    /// Stopped before the natural end because of corruption. `tail_lsn` is the
    /// first invalid byte; the writer resumes after the last valid COMMIT, *not* there.
    StoppedAtTail {
        /// Why it stopped.
        reason: StopReason,
        /// Global LSN where the scan stopped.
        tail_lsn: u64,
        /// Segment where it stopped (diagnostics).
        segment_path: PathBuf,
    },
    /// Unreadable segment header. Valid transactions from earlier segments still replay.
    StoppedAtSegment {
        /// Why it stopped.
        reason: StopReason,
        /// Offending segment.
        segment_path: PathBuf,
    },
}

/// Output of [`scan_segments`]: replayable transactions plus status.
#[derive(Debug, Clone)]
pub struct RecoveryReport {
    /// Transactions to replay, in commit order across segments.
    pub transactions: Vec<ReplayTransaction>,
    /// Why the scan stopped (or that it finished cleanly).
    pub status: RecoveryStatus,
    /// LSN to resume appending at: just past the last fully valid COMMIT. 0 if nothing recovered.
    pub resume_lsn: u64,
    /// Segment holding the resume point, if any; the writer opens it for append.
    pub resume_segment: Option<PathBuf>,
    /// Write offset in `resume_segment`: `resume_lsn - segment.start_lsn`.
    pub resume_segment_offset: Option<u64>,
    /// Highest segment_id found; seeds the writer's `next_segment_id`.
    pub highest_segment_id: u64,
    /// The scan's `min_lsn`: what the snapshot already holds. The writer appends past it even
    /// when the segments end lower or are missing (a file copied without its log), or recovery
    /// would skip the new commits.
    pub floor_lsn: u64,
}

impl RecoveryReport {
    /// Every byte of every segment was valid.
    pub fn is_clean(&self) -> bool {
        matches!(self.status, RecoveryStatus::Clean)
    }
}

/// Scans all redo-log segments for `stem` in `dir`: replayable transactions plus
/// a [`RecoveryReport`].
///
/// Transactions with `commit_lsn <= min_lsn` are skipped (the snapshot already
/// has them); `0` recovers everything. Doesn't open the log for append; that's
/// the caller's (usually `LogWriter::resume`) next step.
pub fn scan_segments(dir: &Path, stem: &str, min_lsn: u64) -> Result<RecoveryReport> {
    let segments = discover_segments(dir, stem)?;
    let mut report = RecoveryReport {
        transactions: Vec::new(),
        status: RecoveryStatus::Clean,
        resume_lsn: 0,
        resume_segment: None,
        resume_segment_offset: None,
        highest_segment_id: 0,
        floor_lsn: min_lsn,
    };

    if segments.is_empty() {
        return Ok(report);
    }

    // resume info across segments: updated per good COMMIT; a torn tail or middle corruption
    // freezes it
    let mut last_valid_commit: Option<(PathBuf, u64, u64)> = None; // (path, end_lsn, segment_start_lsn)
    let mut stopped = false;

    for segment_path in &segments {
        if stopped {
            break;
        }

        let mut reader = match SegmentReader::open(segment_path) {
            Ok(r) => r,
            Err(e) => {
                report.status = RecoveryStatus::StoppedAtSegment {
                    reason: StopReason::BadSegmentHeader(e.to_string()),
                    segment_path: segment_path.clone(),
                };
                stopped = true;
                continue;
            }
        };

        let header = *reader.header();
        report.highest_segment_id = report.highest_segment_id.max(header.segment_id);

        let region = match reader.read_all_records() {
            Ok(r) => r,
            Err(e) => {
                report.status = RecoveryStatus::StoppedAtSegment {
                    reason: StopReason::BadSegmentHeader(e.to_string()),
                    segment_path: segment_path.clone(),
                };
                stopped = true;
                continue;
            }
        };

        let scan = scan_one_segment(&region, &header, min_lsn)?;

        // merge this segment's transactions
        report.transactions.extend(scan.transactions);

        // segment ended with a clean tail: track the resume position
        if let Some((commit_lsn, end_lsn)) = scan.last_commit_end_lsn {
            last_valid_commit = Some((segment_path.clone(), end_lsn, header.start_lsn));
            // commit_lsn is already in the transactions list
            let _ = commit_lsn;
        }

        if let Some(stop) = scan.stop {
            report.status = RecoveryStatus::StoppedAtTail {
                reason: stop.reason,
                tail_lsn: stop.tail_lsn,
                segment_path: segment_path.clone(),
            };
            stopped = true;
        }
    }

    // resume position
    if let Some((path, end_lsn, seg_start)) = last_valid_commit {
        report.resume_lsn = end_lsn;
        report.resume_segment = Some(path);
        report.resume_segment_offset = Some(end_lsn - seg_start);
    } else if !segments.is_empty() {
        // segments exist but no valid commit anywhere: resume at the first segment's start so the
        // writer can append, keeping old segments for forensics
        if let Ok(reader) = SegmentReader::open(&segments[0]) {
            report.resume_lsn = reader.header().start_lsn;
            report.resume_segment = Some(segments[0].clone());
            report.resume_segment_offset = Some(0);
        }
    }

    Ok(report)
}

/// Scan result of one segment's record region.
struct SegmentScan {
    transactions: Vec<ReplayTransaction>,
    /// `(commit_lsn, end_lsn_of_commit_record)` of the segment's last fully valid COMMIT.
    last_commit_end_lsn: Option<(u64, u64)>,
    /// Set when corruption stopped the segment early.
    stop: Option<SegmentStop>,
}

struct SegmentStop {
    reason: StopReason,
    tail_lsn: u64,
}

fn scan_one_segment(region: &[u8], header: &SegmentHeader, min_lsn: u64) -> Result<SegmentScan> {
    let mut transactions = Vec::new();
    let mut pending_ops: Vec<Op> = Vec::new();
    let mut pending_batch_start: usize = 0; // where the current batch's DATA records start
    let mut last_commit_end_lsn: Option<(u64, u64)> = None;
    let mut cursor = 0usize;

    let segment_start_lsn = header.start_lsn;

    loop {
        // end of region: success
        if cursor >= region.len() {
            break;
        }

        // zero kind byte: we're in the pre-allocated zero region past the written data. Normal end,
        // not corruption.
        if region[cursor] == 0 {
            break;
        }

        // need at least a header
        if cursor + RECORD_HEADER_SIZE > region.len() {
            return Ok(SegmentScan {
                transactions,
                last_commit_end_lsn,
                stop: Some(SegmentStop {
                    reason: StopReason::Truncated,
                    tail_lsn: segment_start_lsn + cursor as u64,
                }),
            });
        }

        // peek the header for the claimed record size
        let header_buf: &[u8; RECORD_HEADER_SIZE] = region[cursor..cursor + RECORD_HEADER_SIZE]
            .try_into()
            .unwrap();
        let rec_header = match super::record::RecordHeader::from_bytes(header_buf) {
            Ok(h) => h,
            Err(e) => {
                return Ok(SegmentScan {
                    transactions,
                    last_commit_end_lsn,
                    stop: Some(SegmentStop {
                        reason: StopReason::InvalidHeader(e.to_string()),
                        tail_lsn: segment_start_lsn + cursor as u64,
                    }),
                });
            }
        };

        let total = super::record::record_size(rec_header.payload_len as usize);
        if cursor + total > region.len() {
            return Ok(SegmentScan {
                transactions,
                last_commit_end_lsn,
                stop: Some(SegmentStop {
                    reason: StopReason::Truncated,
                    tail_lsn: segment_start_lsn + cursor as u64,
                }),
            });
        }

        // verify record CRC + decode
        let (decoded_header, payload) = match decode_record(&region[cursor..cursor + total]) {
            Ok(p) => p,
            Err(Error::HeaderChecksumMismatch) => {
                return Ok(SegmentScan {
                    transactions,
                    last_commit_end_lsn,
                    stop: Some(SegmentStop {
                        reason: StopReason::RecordCrcMismatch,
                        tail_lsn: segment_start_lsn + cursor as u64,
                    }),
                });
            }
            Err(e) => {
                return Ok(SegmentScan {
                    transactions,
                    last_commit_end_lsn,
                    stop: Some(SegmentStop {
                        reason: StopReason::InvalidHeader(e.to_string()),
                        tail_lsn: segment_start_lsn + cursor as u64,
                    }),
                });
            }
        };

        // on-disk LSN must match our byte walk from the segment header; a mismatch means
        // misordered/recycled bytes, stop replaying
        let expected_lsn = segment_start_lsn + cursor as u64;
        if decoded_header.lsn != expected_lsn {
            return Ok(SegmentScan {
                transactions,
                last_commit_end_lsn,
                stop: Some(SegmentStop {
                    reason: StopReason::InvalidHeader(format!(
                        "lsn mismatch: header={} expected={}",
                        decoded_header.lsn, expected_lsn
                    )),
                    tail_lsn: expected_lsn,
                }),
            });
        }

        match decoded_header.kind {
            RecordKind::Data => {
                let op = match Op::from_payload_bytes(payload) {
                    Ok(op) => op,
                    Err(e) => {
                        return Ok(SegmentScan {
                            transactions,
                            last_commit_end_lsn,
                            stop: Some(SegmentStop {
                                reason: StopReason::InvalidHeader(e.to_string()),
                                tail_lsn: expected_lsn,
                            }),
                        });
                    }
                };
                if pending_ops.is_empty() {
                    pending_batch_start = cursor;
                }
                pending_ops.push(op);
            }
            RecordKind::Commit => {
                if payload.len() != COMMIT_PAYLOAD_SIZE {
                    return Ok(SegmentScan {
                        transactions,
                        last_commit_end_lsn,
                        stop: Some(SegmentStop {
                            reason: StopReason::InvalidHeader(format!(
                                "COMMIT payload len {} != {}",
                                payload.len(),
                                COMMIT_PAYLOAD_SIZE
                            )),
                            tail_lsn: expected_lsn,
                        }),
                    });
                }
                let commit_payload = CommitPayload::from_bytes(payload.try_into().unwrap());

                // batch CRC over the DATA records from pending_batch_start up to this COMMIT
                let batch_bytes = &region[pending_batch_start..cursor];
                let computed = crc32fast::hash(batch_bytes);
                if computed != commit_payload.batch_crc {
                    return Ok(SegmentScan {
                        transactions,
                        last_commit_end_lsn,
                        stop: Some(SegmentStop {
                            reason: StopReason::BatchCrcMismatch,
                            tail_lsn: expected_lsn,
                        }),
                    });
                }

                // min_lsn filter: skip transactions already in the snapshot
                if expected_lsn > min_lsn {
                    transactions.push(ReplayTransaction {
                        tx_id: commit_payload.tx_id,
                        commit_ts: commit_payload.commit_ts,
                        commit_lsn: expected_lsn,
                        ops: std::mem::take(&mut pending_ops),
                    });
                } else {
                    pending_ops.clear();
                }
                last_commit_end_lsn =
                    Some((expected_lsn, segment_start_lsn + (cursor + total) as u64));
                pending_batch_start = cursor + total;
            }
            RecordKind::NoopPad => {
                // skip padding records (segment tail before rotation)
                pending_batch_start = cursor + total;
            }
        }

        cursor += total;
    }

    // leftover pending_ops without a COMMIT are an incomplete transaction: never acked, never
    // replayed
    let _ = pending_ops;

    Ok(SegmentScan {
        transactions,
        last_commit_end_lsn,
        stop: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::property::PropertyValue;
    use crate::storage::rlog::segment::{MIN_SEGMENT_SIZE, SegmentWriter, segment_path};
    use crate::storage::rlog::writer::{LogWriter, WriterOptions};
    use tempfile::TempDir;

    fn opts(dir: &Path) -> WriterOptions {
        WriterOptions {
            segment_size: MIN_SEGMENT_SIZE,
            stem: "test.tvdb".to_string(),
            dir: dir.to_path_buf(),
        }
    }

    fn make_ops(n: u64) -> Vec<Op> {
        (0..n)
            .map(|i| Op::CreateNode {
                node_id: i,
                labels: vec!["N".to_string()],
                properties: vec![("idx".to_string(), PropertyValue::Int(i as i64))],
            })
            .collect()
    }

    #[test]
    fn scan_empty_dir_returns_empty_report() {
        let tmp = TempDir::new().unwrap();
        let report = scan_segments(tmp.path(), "missing", 0).unwrap();
        assert!(report.transactions.is_empty());
        assert!(report.is_clean());
        assert_eq!(report.resume_lsn, 0);
        assert!(report.resume_segment.is_none());
    }

    #[test]
    fn scan_one_commit_replays_cleanly() {
        let tmp = TempDir::new().unwrap();
        let outcome;
        {
            let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
            outcome = writer.commit_batch(&make_ops(3), 42).unwrap();
        } // writer drops, segment on disk

        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        assert!(report.is_clean(), "status = {:?}", report.status);
        assert_eq!(report.transactions.len(), 1);
        let tx = &report.transactions[0];
        assert_eq!(tx.tx_id, outcome.tx_id);
        assert_eq!(tx.commit_ts, 42);
        assert_eq!(tx.commit_lsn, outcome.commit_lsn);
        assert_eq!(tx.ops.len(), 3);
        for (i, op) in tx.ops.iter().enumerate() {
            match op {
                Op::CreateNode { node_id, .. } => assert_eq!(*node_id, i as u64),
                _ => panic!("wrong op kind"),
            }
        }
        // resume LSN just past the COMMIT
        assert!(report.resume_lsn > outcome.commit_lsn);
        assert_eq!(report.resume_lsn, outcome.next_lsn);
    }

    #[test]
    fn scan_many_commits_in_order() {
        let tmp = TempDir::new().unwrap();
        let mut commit_lsns = Vec::new();
        {
            let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
            for i in 0..50u64 {
                let outcome = writer
                    .commit_batch(&[Op::DeleteNode { node_id: i }], i)
                    .unwrap();
                commit_lsns.push(outcome.commit_lsn);
            }
        }
        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        assert!(report.is_clean());
        assert_eq!(report.transactions.len(), 50);
        for (i, tx) in report.transactions.iter().enumerate() {
            assert_eq!(tx.commit_lsn, commit_lsns[i]);
            assert_eq!(tx.commit_ts, i as u64);
            assert_eq!(tx.ops.len(), 1);
            match &tx.ops[0] {
                Op::DeleteNode { node_id } => assert_eq!(*node_id, i as u64),
                _ => panic!(),
            }
        }
    }

    #[test]
    fn min_lsn_filter_skips_old_transactions() {
        let tmp = TempDir::new().unwrap();
        let mut outcomes = Vec::new();
        {
            let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
            for i in 0..10u64 {
                let outcome = writer
                    .commit_batch(&[Op::DeleteNode { node_id: i }], i)
                    .unwrap();
                outcomes.push(outcome);
            }
        }
        // filter at the 5th commit's LSN
        let report = scan_segments(tmp.path(), "test.tvdb", outcomes[4].commit_lsn).unwrap();
        assert!(report.is_clean());
        assert_eq!(report.transactions.len(), 5);
        assert_eq!(report.transactions[0].commit_lsn, outcomes[5].commit_lsn);
    }

    #[test]
    fn scan_detects_torn_tail_after_last_commit() {
        let tmp = TempDir::new().unwrap();
        let outcome;
        let segment_path_buf;
        {
            let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
            outcome = writer.commit_batch(&make_ops(3), 100).unwrap();
            segment_path_buf = writer.active_segment_path().to_path_buf();
            // garbage after the COMMIT simulates a torn, unflushed record
            use std::io::{Seek, SeekFrom, Write};
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .open(&segment_path_buf)
                .unwrap();
            // right after the last COMMIT
            f.seek(SeekFrom::Start(
                super::super::segment::SEGMENT_HEADER_SIZE as u64 + outcome.next_lsn,
            ))
            .unwrap();
            f.write_all(&[0xFFu8; 32]).unwrap(); // header-shaped, won't decode
        }

        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        // the one clean transaction still recovers
        assert_eq!(report.transactions.len(), 1);
        assert_eq!(report.transactions[0].commit_lsn, outcome.commit_lsn);
        // status reports the bad tail
        match &report.status {
            RecoveryStatus::StoppedAtTail {
                reason, tail_lsn, ..
            } => {
                assert!(*tail_lsn >= outcome.next_lsn);
                // InvalidHeader (bad kind byte) or RecordCrcMismatch
                match reason {
                    StopReason::InvalidHeader(_) | StopReason::RecordCrcMismatch => {}
                    other => panic!("unexpected stop reason: {:?}", other),
                }
            }
            other => panic!("expected StoppedAtTail, got {:?}", other),
        }
        // resume at the post-COMMIT position, NOT the torn tail
        assert_eq!(report.resume_lsn, outcome.next_lsn);
    }

    #[test]
    fn scan_detects_corrupted_record_inside_batch() {
        let tmp = TempDir::new().unwrap();
        let segment_path_buf;
        {
            let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
            // two commits, corrupt record 1: expect 0 recovered (lenient only at tail)
            let _ = writer.commit_batch(&make_ops(1), 100).unwrap();
            let _ = writer.commit_batch(&make_ops(2), 200).unwrap();
            segment_path_buf = writer.active_segment_path().to_path_buf();
        } // writer drops here, fd closed

        // corrupt the first record's payload byte (offset SEGMENT_HEADER_SIZE +
        // RECORD_HEADER_SIZE + 1)
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&segment_path_buf)
            .unwrap();
        let corrupt_offset =
            super::super::segment::SEGMENT_HEADER_SIZE as u64 + RECORD_HEADER_SIZE as u64 + 1;
        // XOR a bit instead of writing 0xFF, which could equal the current byte
        f.seek(SeekFrom::Start(corrupt_offset)).unwrap();
        let mut byte = [0u8; 1];
        f.read_exact(&mut byte).unwrap();
        byte[0] ^= 0x01;
        f.seek(SeekFrom::Start(corrupt_offset)).unwrap();
        f.write_all(&byte).unwrap();
        f.sync_data().unwrap();
        drop(f);

        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        // first record is bad and leniency is tail-only, so nothing recovers
        assert_eq!(
            report.transactions.len(),
            0,
            "expected 0 recovered transactions, got {}",
            report.transactions.len()
        );
        match &report.status {
            RecoveryStatus::StoppedAtTail { reason, .. } => {
                assert!(
                    matches!(reason, StopReason::RecordCrcMismatch),
                    "got {:?}",
                    reason
                );
            }
            other => panic!("expected StoppedAtTail, got {:?}", other),
        }
    }

    #[test]
    fn scan_detects_batch_crc_mismatch() {
        let tmp = TempDir::new().unwrap();
        let outcome;
        let segment_path_buf;
        {
            let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
            outcome = writer.commit_batch(&make_ops(3), 100).unwrap();
            segment_path_buf = writer.active_segment_path().to_path_buf();
        }

        // break the COMMIT's batch_crc, then fix the record CRC so only the batch check fails
        use std::io::{Read, Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&segment_path_buf)
            .unwrap();
        let commit_record_start =
            super::super::segment::SEGMENT_HEADER_SIZE as u64 + outcome.commit_lsn;
        // COMMIT record: 16 B header + 28 B payload + 4 B pad + 4 B CRC; CRC at 48,
        // batch_crc is the payload's last 4 bytes at 16 + 24 = 40
        let batch_crc_offset = commit_record_start + 16 + 24;
        f.seek(SeekFrom::Start(batch_crc_offset)).unwrap();
        f.write_all(&[0u8; 4]).unwrap(); // zero the batch_crc

        // recompute the record CRC over header + payload + pad and rewrite the trailer
        let total_size = super::super::record::record_size(COMMIT_PAYLOAD_SIZE);
        let mut record_buf = vec![0u8; total_size];
        f.seek(SeekFrom::Start(commit_record_start)).unwrap();
        f.read_exact(&mut record_buf).unwrap();
        let new_crc = crc32fast::hash(&record_buf[..total_size - 4]);
        f.seek(SeekFrom::Start(
            commit_record_start + (total_size - 4) as u64,
        ))
        .unwrap();
        f.write_all(&new_crc.to_le_bytes()).unwrap();
        drop(f);

        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        // DATA records pass their CRC, the COMMIT batch_crc doesn't
        assert_eq!(report.transactions.len(), 0);
        match &report.status {
            RecoveryStatus::StoppedAtTail { reason, .. } => {
                assert_eq!(*reason, StopReason::BatchCrcMismatch);
            }
            other => panic!("expected StoppedAtTail BatchCrcMismatch, got {:?}", other),
        }
    }

    #[test]
    fn scan_across_multiple_segments() {
        let tmp = TempDir::new().unwrap();
        let mut commit_count = 0;
        {
            let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
            // large transactions to force a rotation
            let big_str = "z".repeat(20_000);
            for i in 0..6u64 {
                let ops = vec![Op::CreateNode {
                    node_id: i,
                    labels: vec!["N".to_string()],
                    properties: vec![("data".to_string(), PropertyValue::String(big_str.clone()))],
                }];
                writer.commit_batch(&ops, i).unwrap();
                commit_count += 1;
            }
            assert!(
                writer.segment_paths().unwrap().len() >= 2,
                "expected rotation"
            );
        }

        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        assert!(report.is_clean(), "status = {:?}", report.status);
        assert_eq!(report.transactions.len(), commit_count);
        // commit_ts monotonic in scan order
        for (i, tx) in report.transactions.iter().enumerate() {
            assert_eq!(tx.commit_ts, i as u64);
        }
    }

    #[test]
    fn scan_returns_resume_position_pointing_at_segment_offset() {
        let tmp = TempDir::new().unwrap();
        let outcome;
        let segment_path_buf;
        {
            let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
            outcome = writer.commit_batch(&make_ops(2), 1).unwrap();
            segment_path_buf = writer.active_segment_path().to_path_buf();
        }
        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        assert_eq!(report.resume_lsn, outcome.next_lsn);
        assert_eq!(
            report.resume_segment.as_deref(),
            Some(segment_path_buf.as_path())
        );
        // segment 0 starts at LSN 0, so offset == resume LSN
        assert_eq!(report.resume_segment_offset, Some(outcome.next_lsn));
    }

    /// 1000 commits, crash via appended garbage, recover: all transactions in order, correct resume
    /// position.
    #[test]
    fn end_to_end_1000_commits_then_simulated_crash() {
        let tmp = TempDir::new().unwrap();
        let mut expected_commit_lsns: Vec<u64> = Vec::new();
        let mut final_outcome = None;
        {
            let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
            for i in 0..1000u64 {
                let ops = vec![
                    Op::CreateNode {
                        node_id: i,
                        labels: vec!["Person".to_string()],
                        properties: vec![
                            (
                                "name".to_string(),
                                PropertyValue::String(format!("user{}", i)),
                            ),
                            ("age".to_string(), PropertyValue::Int((i % 100) as i64)),
                        ],
                    },
                    Op::CreateEdge {
                        edge_id: i,
                        src: i,
                        dst: (i + 1) % 1000,
                        edge_type: "Friend".to_string(),
                        properties: vec![],
                    },
                ];
                let outcome = writer.commit_batch(&ops, i).unwrap();
                expected_commit_lsns.push(outcome.commit_lsn);
                final_outcome = Some(outcome);
            }
        } // writer drops, data on disk

        let final_outcome = final_outcome.unwrap();

        // torn write: garbage past the last COMMIT in the active segment
        let segments = discover_segments(tmp.path(), "test.tvdb").unwrap();
        let last_seg = segments.last().unwrap();
        let last_reader = SegmentReader::open(last_seg).unwrap();
        let last_header = *last_reader.header();
        // at end_lsn_of_last_commit, i.e. the next record slot
        let file_offset = super::super::segment::SEGMENT_HEADER_SIZE as u64
            + (final_outcome.next_lsn - last_header.start_lsn);
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(last_seg)
            .unwrap();
        f.seek(SeekFrom::Start(file_offset)).unwrap();
        // 256 bytes: valid kind tag, payload that fails CRC
        let mut garbage = vec![0u8; 256];
        garbage[0] = RecordKind::Data as u8;
        garbage[2..6].copy_from_slice(&64u32.to_le_bytes()); // claims a 64-byte payload
        f.write_all(&garbage).unwrap();
        f.sync_data().unwrap();
        drop(f);

        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        assert_eq!(report.transactions.len(), 1000, "expected all 1000 txns");
        for (i, tx) in report.transactions.iter().enumerate() {
            assert_eq!(tx.commit_ts, i as u64);
            assert_eq!(tx.commit_lsn, expected_commit_lsns[i]);
            assert_eq!(tx.ops.len(), 2);
            match &tx.ops[0] {
                Op::CreateNode { node_id, .. } => assert_eq!(*node_id, i as u64),
                _ => panic!("wrong op kind at txn {}", i),
            }
            match &tx.ops[1] {
                Op::CreateEdge { src, dst, .. } => {
                    assert_eq!(*src, i as u64);
                    assert_eq!(*dst, ((i + 1) % 1000) as u64);
                }
                _ => panic!("wrong op kind at txn {}", i),
            }
        }
        // tail status shows the torn write
        match &report.status {
            RecoveryStatus::StoppedAtTail { .. } => {}
            other => panic!("expected StoppedAtTail, got {:?}", other),
        }
        // resume exactly after the last good COMMIT
        assert_eq!(report.resume_lsn, final_outcome.next_lsn);
    }

    /// After recovery, new commits give a self-consistent log.
    #[test]
    fn writer_can_continue_appending_after_torn_tail_recovery() {
        let tmp = TempDir::new().unwrap();
        let first_outcome;
        let last_seg;
        {
            let mut w = LogWriter::open(opts(tmp.path())).unwrap();
            first_outcome = w.commit_batch(&make_ops(2), 100).unwrap();
            last_seg = w.active_segment_path().to_path_buf();
        }
        // torn-tail garbage
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&last_seg)
            .unwrap();
        let off = super::super::segment::SEGMENT_HEADER_SIZE as u64 + first_outcome.next_lsn;
        f.seek(SeekFrom::Start(off)).unwrap();
        f.write_all(&[0xAB; 64]).unwrap();
        f.sync_data().unwrap();
        drop(f);

        // reopen: resumes at first_outcome.next_lsn
        let mut w2 = LogWriter::open(opts(tmp.path())).unwrap();
        assert_eq!(w2.next_lsn(), first_outcome.next_lsn);

        let second_outcome = w2.commit_batch(&make_ops(1), 200).unwrap();
        assert_eq!(second_outcome.data_record_lsns[0], first_outcome.next_lsn);

        drop(w2);

        // rescan: BOTH transactions, no tail corruption
        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        assert_eq!(report.transactions.len(), 2);
        assert_eq!(report.transactions[0].commit_lsn, first_outcome.commit_lsn);
        assert_eq!(report.transactions[1].commit_lsn, second_outcome.commit_lsn);
        // the new commit overwrote the garbage, so the tail is clean
        assert!(report.is_clean(), "status = {:?}", report.status);
    }

    #[test]
    fn scan_detects_segment_header_corruption() {
        let tmp = TempDir::new().unwrap();
        let path = segment_path(tmp.path(), "test.tvdb", 0);
        // real segment, then trash its magic
        let header = SegmentHeader {
            segment_id: 0,
            start_lsn: 0,
            created_ns: 0,
            salt: [0u8; 16],
        };
        let _seg = SegmentWriter::create(path.clone(), header, MIN_SEGMENT_SIZE).unwrap();
        drop(_seg);

        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(b"XXXX").unwrap();
        drop(f);

        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        match &report.status {
            RecoveryStatus::StoppedAtSegment { reason, .. } => {
                assert!(matches!(reason, StopReason::BadSegmentHeader(_)));
            }
            other => panic!("expected StoppedAtSegment, got {:?}", other),
        }
    }
}
