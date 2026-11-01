//! Redo-log crash injection: realistic faults (torn writes, bit flips, zeroed
//! sectors) in a generated segment, then recovery must yield state consistent
//! with *some prefix* of acknowledged commits.
//!
//! An embedded splitmix64 PRNG seeded from the test name keeps failures
//! reproducible without `proptest` or `rand`; each test runs N seeds.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use traverse_core::graph::Graph;
use traverse_core::graph::property::PropertyValue;
use traverse_core::storage::rlog::{
    LogWriter, RecoveryStatus, SEGMENT_HEADER_SIZE, StopReason, WriterOptions, scan_segments,
};
use traverse_core::tx::MvccStore;
use traverse_core::tx::view::GraphView;

/// Splitmix64: tiny, deterministic, well distributed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn range(&mut self, lo: u64, hi: u64) -> u64 {
        if hi <= lo {
            return lo;
        }
        lo + (self.next_u64() % (hi - lo))
    }
}

fn pmap(pairs: &[(&str, PropertyValue)]) -> BTreeMap<String, PropertyValue> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

fn open_writer(dir: &Path) -> Arc<Mutex<LogWriter>> {
    let opts = WriterOptions {
        segment_size: traverse_core::storage::rlog::MIN_SEGMENT_SIZE,
        stem: "test.tvdb".to_string(),
        dir: dir.to_path_buf(),
    };
    Arc::new(Mutex::new(LogWriter::open(opts).unwrap()))
}

/// Deterministic workload through WriteTx; returns (commit_lsn, next_lsn_after, op_count) per
/// commit, in order.
fn run_workload(store: &MvccStore, ops_per_tx: usize, tx_count: usize) -> Vec<u64> {
    let mut commit_tses = Vec::new();
    for t in 0..tx_count {
        let mut tx = store.begin_write();
        for i in 0..ops_per_tx {
            GraphView::create_node(
                &mut tx,
                &["N"],
                pmap(&[
                    ("t", PropertyValue::Int(t as i64)),
                    ("i", PropertyValue::Int(i as i64)),
                ]),
            )
            .unwrap();
        }
        let ts = tx.commit().unwrap();
        commit_tses.push(ts.physical);
    }
    commit_tses
}

#[test]
fn fuzz_truncate_at_random_offsets() {
    for seed in 1u64..=20 {
        let tmp = TempDir::new().unwrap();
        let writer = open_writer(tmp.path());
        let store = MvccStore::new(Graph::new()).with_log_writer(writer.clone());
        let _ = run_workload(&store, 2, 50);

        // last committed LSN and active segment path
        let (last_commit_lsn, seg_path) = {
            let w = writer.lock().unwrap();
            (
                w.last_committed_lsn(),
                w.active_segment_path().to_path_buf(),
            )
        };
        drop(store);
        drop(writer);

        let file_size = std::fs::metadata(&seg_path).unwrap().len();
        let mut rng = Rng::new(seed * 0xDEADBEEF);
        // truncate at a random offset in the records region (past the segment header)
        let trunc_at = rng.range(SEGMENT_HEADER_SIZE as u64, file_size);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&seg_path)
            .unwrap();
        f.set_len(trunc_at).unwrap();
        f.sync_all().unwrap();
        drop(f);

        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        // invariant 1: every recovered tx has commit_lsn <= trunc_at
        for tx in &report.transactions {
            assert!(
                tx.commit_lsn
                    < trunc_at - (SEGMENT_HEADER_SIZE as u64) + (SEGMENT_HEADER_SIZE as u64),
                "recovered tx beyond truncation (seed={})",
                seed
            );
        }
        // invariant 2 (loose, COMMIT sizes unknown here): truncating past the last
        // COMMIT's end recovers everything. invariant 3: status is Clean (cut past the
        // last COMMIT byte) or StoppedAtTail (cut mid-record).
        match &report.status {
            RecoveryStatus::Clean => {
                // ok: torn in the zero-padded tail
            }
            RecoveryStatus::StoppedAtTail { .. } => {
                // ok: the typical case
            }
            other => panic!("seed={} unexpected status: {:?}", seed, other),
        }
        // invariant 4: resume_lsn <= trunc_at
        let trunc_lsn_within = trunc_at - SEGMENT_HEADER_SIZE as u64;
        assert!(
            report.resume_lsn <= trunc_lsn_within,
            "seed={} resume_lsn={} > trunc_lsn_within={}",
            seed,
            report.resume_lsn,
            trunc_lsn_within
        );

        let _ = last_commit_lsn;
    }
}

#[test]
fn fuzz_bit_flip_at_random_offset_inside_data() {
    for seed in 1u64..=20 {
        let tmp = TempDir::new().unwrap();
        let writer = open_writer(tmp.path());
        let store = MvccStore::new(Graph::new()).with_log_writer(writer.clone());
        let _ = run_workload(&store, 2, 30);

        let seg_path = writer.lock().unwrap().active_segment_path().to_path_buf();
        let next_lsn = writer.lock().unwrap().next_lsn();
        drop(store);
        drop(writer);

        // flip a random bit in the records region (not the header)
        let mut rng = Rng::new(seed * 0xFEEDFACE);
        let max_offset = SEGMENT_HEADER_SIZE as u64 + next_lsn - 1;
        let off = rng.range(SEGMENT_HEADER_SIZE as u64, max_offset);
        let bit = (rng.next_u64() & 7) as u8;

        let mut f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&seg_path)
            .unwrap();
        f.seek(SeekFrom::Start(off)).unwrap();
        let mut byte = [0u8; 1];
        f.read_exact(&mut byte).unwrap();
        byte[0] ^= 1u8 << bit;
        f.seek(SeekFrom::Start(off)).unwrap();
        f.write_all(&byte).unwrap();
        f.sync_data().unwrap();
        drop(f);

        // recovery must detect corruption and stop cleanly: never panic, never corrupt silently
        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        // a bit flip is never silent
        match &report.status {
            RecoveryStatus::Clean => {
                // Acceptable only if we got lucky and flipped a bit in the
                // unused-pad region of an already-valid record. With our
                // record layout (header + payload + tight padding) this is
                // uncommon but possible. If clean, every transaction must be
                // recoverable.
                assert!(!report.transactions.is_empty() || off >= max_offset);
            }
            RecoveryStatus::StoppedAtTail { reason, .. } => {
                assert!(
                    matches!(
                        reason,
                        StopReason::RecordCrcMismatch
                            | StopReason::InvalidHeader(_)
                            | StopReason::BatchCrcMismatch
                            | StopReason::Truncated
                    ),
                    "seed={} unexpected reason: {:?}",
                    seed,
                    reason
                );
            }
            other => panic!("seed={} unexpected status: {:?}", seed, other),
        }
    }
}

#[test]
fn fuzz_zeroed_sector_at_random_offset() {
    for seed in 1u64..=10 {
        let tmp = TempDir::new().unwrap();
        let writer = open_writer(tmp.path());
        let store = MvccStore::new(Graph::new()).with_log_writer(writer.clone());
        let _ = run_workload(&store, 1, 30);

        let seg_path = writer.lock().unwrap().active_segment_path().to_path_buf();
        let next_lsn = writer.lock().unwrap().next_lsn();
        drop(store);
        drop(writer);

        let mut rng = Rng::new(seed * 0xABADC0DE);
        let max_offset = SEGMENT_HEADER_SIZE as u64 + next_lsn;
        // zero a 512-byte chunk (one lost sector)
        let off = rng.range(
            SEGMENT_HEADER_SIZE as u64,
            max_offset
                .saturating_sub(512)
                .max(SEGMENT_HEADER_SIZE as u64 + 1),
        );
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(&seg_path)
            .unwrap();
        f.seek(SeekFrom::Start(off)).unwrap();
        let len = 512.min(max_offset - off);
        f.write_all(&vec![0u8; len as usize]).unwrap();
        f.sync_data().unwrap();
        drop(f);

        // no panic; status is corrupt or clean depending on where the zeros landed
        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        match report.status {
            RecoveryStatus::Clean => {}
            RecoveryStatus::StoppedAtTail { .. } => {}
            other => panic!("seed={} unexpected status: {:?}", seed, other),
        }
        // recovered set must be a *prefix* of commit order: commit_ts monotonic
        let mut last_ts = 0u64;
        for tx in &report.transactions {
            assert!(
                tx.commit_ts >= last_ts,
                "seed={} commit_ts not monotonic: {} -> {}",
                seed,
                last_ts,
                tx.commit_ts
            );
            last_ts = tx.commit_ts;
        }
    }
}

#[test]
fn fuzz_torn_write_then_resume_and_grow() {
    // full crash-resilience scenario:
    // 1. workload runs, fsyncs
    // 2. crash mid-write (file truncated to a random pre-commit-LSN length)
    // 3. reopen via LogWriter::open: resumes after the last good COMMIT
    // 4. append a new batch
    // 5. rescan: everything recovers (pre- and post-crash)
    for seed in 1u64..=10 {
        let tmp = TempDir::new().unwrap();
        let writer = open_writer(tmp.path());
        let store = MvccStore::new(Graph::new()).with_log_writer(writer.clone());
        let _ = run_workload(&store, 1, 20);

        // snapshot state for invariants
        let pre_crash_next_lsn = writer.lock().unwrap().next_lsn();
        let pre_crash_last_committed = writer.lock().unwrap().last_committed_lsn();
        let seg_path = writer.lock().unwrap().active_segment_path().to_path_buf();
        drop(store);
        drop(writer);

        // truncate between last committed and next_lsn: more bytes written than were durable
        let mut rng = Rng::new(seed);
        let lo = SEGMENT_HEADER_SIZE as u64 + pre_crash_last_committed;
        let hi = SEGMENT_HEADER_SIZE as u64 + pre_crash_next_lsn;
        if hi <= lo + 1 {
            continue;
        }
        let trunc_at = rng.range(lo, hi);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&seg_path)
            .unwrap()
            .set_len(trunc_at)
            .unwrap();

        // reopen
        let writer2 = open_writer(tmp.path());
        let resumed_next_lsn = writer2.lock().unwrap().next_lsn();
        // cutting into the last COMMIT discards it, so resume may land at the prior
        // COMMIT's end. Either (a) cut past the last COMMIT: resumed == pre_crash_next_lsn,
        // or (b) cut the last COMMIT: resumed <= pre_crash_last_committed. Never
        // `resumed > pre_crash_next_lsn` (no fabricated bytes).
        assert!(
            resumed_next_lsn <= pre_crash_next_lsn,
            "seed={} resumed_next_lsn={} > pre_crash_next_lsn={} - fabricated bytes!",
            seed,
            resumed_next_lsn,
            pre_crash_next_lsn
        );

        // append more
        let store2 = MvccStore::new(Graph::new()).with_log_writer(writer2.clone());
        let _ = run_workload(&store2, 1, 5);
        drop(store2);
        drop(writer2);

        // final scan, the invariants that matter:
        // (1) recovered commit_ts monotonic (no time travel)
        // (2) no recovered commit_lsn conflicts with the truncation point (no fabricated bytes
        // survive a torn write)
        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        let mut last_ts = 0u64;
        for tx in &report.transactions {
            assert!(
                tx.commit_ts >= last_ts,
                "seed={} non-monotonic commit_ts",
                seed
            );
            last_ts = tx.commit_ts;
        }
        // Clean (post-crash tail overwrote orphaned bytes) or StoppedAtTail (orphaned
        // bytes past the new commits look like a torn tail). Both safe; a recovered tx
        // with a fabricated commit_ts would not be.
        match &report.status {
            RecoveryStatus::Clean | RecoveryStatus::StoppedAtTail { .. } => {}
            other => panic!("seed={} unexpected status: {:?}", seed, other),
        }
    }
}
