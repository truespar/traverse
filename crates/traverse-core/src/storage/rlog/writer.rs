//! Single-threaded redo-log writer.
//!
//! Assigns LSNs, encodes records, rotates full segments.
//!
//! Group commit: `append_batch()` only queues the encoded records in an in-memory
//! log buffer (under the writer mutex); getting them to disk is awaited outside the
//! mutex via [`SyncControl::wait_durable`], where the first waiter writes and
//! fdatasyncs everything queued so far for everyone. `commit_batch()` is the
//! append + wait for single-threaded callers.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use super::record::{CommitPayload, Op, RECORD_ALIGNMENT, RecordKind, encode_record};
use super::recovery::{RecoveryReport, scan_segments};
use super::segment::{
    DEFAULT_SEGMENT_SIZE, MIN_SEGMENT_SIZE, SEGMENT_HEADER_SIZE, SegmentHeader, SegmentWriter,
    discover_segments, remove_segments, segment_path,
};

/// Observability counters. Updated under the caller-side writer mutex, so reads
/// are eventually consistent; no atomics on the hot path.
#[derive(Debug, Default, Clone, Copy)]
pub struct LogWriterMetrics {
    /// Successful `commit_batch` calls.
    pub commits: u64,
    /// Ops written across all commits.
    pub data_records: u64,
    /// `fdatasync` calls issued.
    pub fdatasyncs: u64,
    /// Bytes written (DATA + COMMIT records).
    pub bytes_written: u64,
    /// Segment rotations triggered.
    pub rotations: u64,
}
use crate::error::{Error, Result};

/// Redo-log writer tunables; defaults suit most uses.
#[derive(Debug, Clone)]
pub struct WriterOptions {
    /// Pre-allocated segment size in bytes. Default 32 MiB, at least [`MIN_SEGMENT_SIZE`].
    pub segment_size: u64,
    /// Filename stem, usually the .tvdb file's (`mydb` for `mydb.tvdb`). Segments sit
    /// next to the .tvdb as `<stem>.rlog.NNNNNNNNNN`.
    pub stem: String,
    /// Directory of the .tvdb file; segments go here.
    pub dir: PathBuf,
}

impl WriterOptions {
    /// Stem and directory from a `.tvdb` path.
    pub fn from_tvdb_path(tvdb_path: &Path) -> Result<Self> {
        let dir = tvdb_path
            .parent()
            .ok_or_else(|| {
                Error::InvalidFormat("rlog: tvdb path has no parent directory".to_string())
            })?
            .to_path_buf();
        let file_name = tvdb_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                Error::InvalidFormat("rlog: tvdb path has no valid filename".to_string())
            })?;
        // stem keeps `.tvdb` so `mydb.tvdb` and `mydb.tvdb.rlog.0000000001` sort together
        Ok(Self {
            segment_size: DEFAULT_SEGMENT_SIZE,
            stem: file_name.to_string(),
            dir,
        })
    }
}

// -- Group commit -----------------------------------------------------

/// Pending bytes past which an append writes them out itself, bounding the buffer even when no
/// committer waits.
const MAX_PENDING: usize = 8 << 20;

/// Durability coordinator for group commit.
///
/// Appends only copy their encoded records into an in-memory log buffer, under the
/// `LogWriter` mutex: no file I/O while a commit holds the store's writer lock.
/// Getting them to the file happens outside every lock: the first committer that
/// finds no leader active becomes the leader, writes the WHOLE buffer with one
/// `write`, `fdatasync`s once and wakes every committer it covered; committers
/// arriving meanwhile join the next round. Concurrent commits share one write and
/// one sync, a lone committer pays one of each, and no append ever waits behind a
/// sync on the same file (which some platforms serialize).
///
/// Relies on the WAL being sequential: a sync covering LSN `x` covers every LSN
/// below it, so a later transaction can't become durable before an earlier one.
pub struct SyncControl {
    state: Mutex<SyncState>,
    cv: Condvar,
    /// fdatasyncs issued (group leaders and rotations; merged into writer metrics).
    fdatasyncs: AtomicU64,
    /// Newest commit timestamp whose records are durable: what readers may see in synchronous
    /// WAL mode.
    durable_ts: AtomicU64,
}

struct SyncState {
    /// Records appended but not yet written to the segment, in LSN order.
    pending: Vec<u8>,
    /// LSN just past the last appended byte.
    appended_lsn: u64,
    /// Everything below this LSN is in the segment file (the OS has it).
    written_lsn: u64,
    /// Everything below this LSN is durable.
    synced_lsn: u64,
    /// A leader is writing or syncing; it holds `segment` meanwhile.
    busy: bool,
    /// The active segment, `None` while a leader has it.
    segment: Option<SegmentWriter>,
    /// A write or sync failed: durability is gone, every later wait fails fast.
    /// In-memory state stays usable; write ACKs stop.
    poisoned: Option<String>,
    /// Appended commits not yet durable: `(end LSN, commit timestamp)`, in LSN order.
    commits: VecDeque<(u64, u64)>,
}

fn poisoned_error(msg: &str) -> Error {
    Error::Io(std::io::Error::other(format!(
        "rlog durability poisoned by an earlier write or fdatasync failure: {msg}"
    )))
}

impl SyncControl {
    fn new(segment: SegmentWriter, lsn: u64) -> Self {
        Self {
            state: Mutex::new(SyncState {
                pending: Vec::new(),
                appended_lsn: lsn,
                written_lsn: lsn,
                synced_lsn: lsn,
                busy: false,
                segment: Some(segment),
                poisoned: None,
                commits: VecDeque::new(),
            }),
            cv: Condvar::new(),
            fdatasyncs: AtomicU64::new(0),
            durable_ts: AtomicU64::new(0),
        }
    }

    /// Newest commit timestamp whose records are durable (0 before the first).
    pub fn durable_ts(&self) -> u64 {
        self.durable_ts.load(Ordering::SeqCst)
    }

    /// Raises the durable timestamp to at least `ts` (a store attaching the writer: everything it
    /// already holds counts as durable).
    pub fn note_durable_ts(&self, ts: u64) {
        self.durable_ts.fetch_max(ts, Ordering::SeqCst);
    }

    /// After `synced_lsn` advanced: commits it now covers become durable, under the state lock
    /// and before waking anyone, so a committer returning from its wait finds its timestamp there.
    fn settle(&self, st: &mut SyncState) {
        let mut newest = 0;
        while let Some(&(end, ts)) = st.commits.front() {
            if end > st.synced_lsn {
                break;
            }
            newest = newest.max(ts);
            st.commits.pop_front();
        }
        if newest > 0 {
            self.durable_ts.fetch_max(newest, Ordering::SeqCst);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, SyncState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Writer (under its mutex): queues a commit's encoded records, ending at `end_lsn`.
    fn buffer(&self, bytes: &[u8], end_lsn: u64, commit_ts: u64) {
        let mut st = self.lock();
        st.pending.extend_from_slice(bytes);
        st.appended_lsn = end_lsn;
        st.commits.push_back((end_lsn, commit_ts));
        if st.pending.len() > MAX_PENDING && !st.busy && st.poisoned.is_none() {
            drop(self.lead(st, false));
        }
    }

    /// Leads one round: writes every pending byte (one `write`), syncs if `sync`, publishes the
    /// new marks and wakes the waiters. Called with the state locked and no leader active.
    fn lead<'s>(
        &'s self,
        mut st: std::sync::MutexGuard<'s, SyncState>,
        sync: bool,
    ) -> std::sync::MutexGuard<'s, SyncState> {
        st.busy = true;
        let bytes = std::mem::take(&mut st.pending);
        let target = st.appended_lsn;
        let mut segment = st.segment.take().expect("active segment while no leader");
        drop(st);
        let mut res = Ok(());
        if !bytes.is_empty() {
            res = segment.append(&bytes).map(|_| ());
        }
        if res.is_ok() && sync {
            res = segment.fdatasync();
            self.fdatasyncs.fetch_add(1, Ordering::Relaxed);
        }
        let mut st = self.lock();
        st.segment = Some(segment);
        st.busy = false;
        match res {
            Ok(()) => {
                st.written_lsn = st.written_lsn.max(target);
                if sync {
                    st.synced_lsn = st.synced_lsn.max(target);
                    self.settle(&mut st);
                }
                // hand the (cleared) allocation back for the next round
                if st.pending.is_empty() {
                    let mut bytes = bytes;
                    bytes.clear();
                    st.pending = bytes;
                }
            }
            Err(e) => st.poisoned = Some(e.to_string()),
        }
        self.cv.notify_all();
        st
    }

    /// Blocks until everything up to `lsn` is durable (or durability is poisoned).
    pub fn wait_durable(&self, lsn: u64) -> Result<()> {
        let mut st = self.lock();
        loop {
            if let Some(msg) = &st.poisoned {
                return Err(poisoned_error(msg));
            }
            if st.synced_lsn >= lsn {
                return Ok(());
            }
            st = if st.busy {
                self.cv.wait(st).unwrap_or_else(|e| e.into_inner())
            } else {
                self.lead(st, true)
            };
        }
    }

    /// Blocks until everything up to `lsn` is in the segment file, without syncing: async-WAL
    /// commits, which a process crash then doesn't lose (an OS crash still can, up to the next
    /// background sync).
    pub fn wait_written(&self, lsn: u64) -> Result<()> {
        let mut st = self.lock();
        loop {
            if let Some(msg) = &st.poisoned {
                return Err(poisoned_error(msg));
            }
            if st.written_lsn >= lsn {
                return Ok(());
            }
            st = if st.busy {
                self.cv.wait(st).unwrap_or_else(|e| e.into_inner())
            } else {
                self.lead(st, false)
            };
        }
    }

    /// Forces everything appended so far to disk (snapshot watermarks, graceful shutdown,
    /// the async-WAL background sync).
    pub fn sync_now(&self) -> Result<()> {
        let target = self.lock().appended_lsn;
        self.wait_durable(target)
    }

    /// Writes and syncs everything pending into the active segment, then installs the segment
    /// `next` returns: rotation. Waits for any leader first.
    fn rotate(&self, next: impl FnOnce() -> Result<SegmentWriter>) -> Result<()> {
        let mut st = self.lock();
        while st.busy {
            st = self.cv.wait(st).unwrap_or_else(|e| e.into_inner());
        }
        if let Some(msg) = &st.poisoned {
            return Err(poisoned_error(msg));
        }
        st.busy = true;
        let bytes = std::mem::take(&mut st.pending);
        let target = st.appended_lsn;
        let mut old = st.segment.take().expect("active segment while no leader");
        drop(st);
        // the closed segment goes to disk whole: recovery's "a closed segment is complete"
        let res = (|| {
            if !bytes.is_empty() {
                old.append(&bytes)?;
            }
            old.fdatasync()?;
            self.fdatasyncs.fetch_add(1, Ordering::Relaxed);
            next()
        })();
        let mut st = self.lock();
        st.busy = false;
        let out = match res {
            Ok(new) => {
                st.segment = Some(new);
                st.written_lsn = target;
                st.synced_lsn = target;
                self.settle(&mut st);
                Ok(())
            }
            Err(e) => {
                st.segment = Some(old);
                st.poisoned = Some(e.to_string());
                Err(e)
            }
        };
        self.cv.notify_all();
        out
    }

    /// Test support: poison as if a write or fdatasync had failed.
    pub fn poison_for_test(&self, msg: &str) {
        self.lock().poisoned = Some(msg.to_string());
        self.cv.notify_all();
    }

    /// Cheap poison probe: fail fast after a write or fdatasync failure, no waiting.
    pub fn check_poisoned(&self) -> Result<()> {
        match &self.lock().poisoned {
            Some(msg) => Err(poisoned_error(msg)),
            None => Ok(()),
        }
    }
}

/// Single-threaded writer owning the LSN space, encoding records and rotating segments; the
/// active segment file itself is driven by [`SyncControl`].
///
/// LSNs are global byte offsets across the whole WAL stream; they don't reset per
/// segment. Each segment header has its own `start_lsn` and records store their
/// global LSN. LSN 0 means "no records yet"; the first record's LSN is segment 0's
/// `start_lsn`.
pub struct LogWriter {
    options: WriterOptions,
    /// Active segment: path, first LSN, and the LSN just past its capacity.
    active_path: PathBuf,
    active_start_lsn: u64,
    active_end_lsn: u64,
    next_segment_id: u64,
    /// Global LSN of the next byte to append.
    next_lsn: AtomicU64,
    /// LSN of the last appended COMMIT; 0 = none yet in this writer's life.
    last_committed_lsn: AtomicU64,
    /// Transaction-ID counter, monotonic per writer lifetime; recovery groups records by it.
    next_tx_id: AtomicU64,
    /// Counters, read via [`metrics`](Self::metrics).
    metrics: LogWriterMetrics,
    /// Group-commit coordinator, shared with committers via [`sync_handle`](Self::sync_handle).
    sync: Arc<SyncControl>,
}

/// The LSN just past what `segment` can hold.
fn end_lsn_of(segment: &SegmentWriter) -> u64 {
    segment.next_lsn() + segment.remaining()
}

impl LogWriter {
    /// Opens or creates a redo-log writer, for when there's no recovery state to
    /// reapply. Same as:
    /// ```ignore
    /// let report = scan_segments(&options.dir, &options.stem, 0)?;
    /// LogWriter::resume(options, &report)
    /// ```
    ///
    /// It DOES find existing segments and resume after them. To replay unapplied
    /// transactions, call [`scan_segments`] first and feed
    /// `RecoveryReport::transactions` into your graph.
    pub fn open(options: WriterOptions) -> Result<Self> {
        let report = scan_segments(&options.dir, &options.stem, 0)?;
        Self::resume(options, &report)
    }

    /// Opens a writer resuming from a [`RecoveryReport`].
    ///
    /// Clean or torn tail: reopens the last segment for append at the post-COMMIT
    /// offset. Torn bytes past it get overwritten; any leftovers from a previous
    /// incarnation fail the CRC/LSN check on re-read.
    ///
    /// The writer never assigns an LSN the snapshot already covers
    /// ([`RecoveryReport::floor_lsn`]): with no segments, or segments that end below it, it
    /// starts a new segment there.
    ///
    /// Segment-header failure: refuses to open, since continuing could silently
    /// abandon recoverable transactions.
    pub fn resume(options: WriterOptions, report: &RecoveryReport) -> Result<Self> {
        if options.segment_size < MIN_SEGMENT_SIZE {
            return Err(Error::InvalidFormat(format!(
                "rlog: segment_size {} below minimum {}",
                options.segment_size, MIN_SEGMENT_SIZE
            )));
        }
        std::fs::create_dir_all(&options.dir)?;

        if let super::recovery::RecoveryStatus::StoppedAtSegment {
            reason,
            segment_path,
        } = &report.status
        {
            return Err(Error::InvalidFormat(format!(
                "rlog: refusing to open - segment {} has unreadable header: {:?}",
                segment_path.display(),
                reason
            )));
        }

        // recovery skips everything at or below what the snapshot holds, so no new record may get
        // an LSN there
        let floor = report.floor_lsn.next_multiple_of(RECORD_ALIGNMENT as u64);
        let fresh_segment = |segment_id: u64| {
            let path = segment_path(&options.dir, &options.stem, segment_id);
            let header = SegmentHeader {
                segment_id,
                start_lsn: floor,
                created_ns: now_ns(),
                salt: random_salt(),
            };
            SegmentWriter::create(path, header, options.segment_size)
        };
        let (active, next_segment_id, next_lsn, last_committed_lsn, next_tx_id) =
            match report.resume_segment.clone() {
                // no segments: segment 0, starting where the snapshot ends
                None => (fresh_segment(0)?, 1, floor, 0, 1),
                // the segments end below what the snapshot holds (another copy's log, or one cut
                // short): a new segment past it; replay already skips the old ones
                Some(_) if report.resume_lsn < floor => {
                    let id = report.highest_segment_id + 1;
                    (fresh_segment(id)?, id + 1, floor, 0, 1)
                }
                // resume into the segment holding the last valid COMMIT
                Some(path) => {
                    let offset = report.resume_segment_offset.unwrap_or(0);
                    let active = SegmentWriter::open_for_append(path, offset)?;
                    let last = report.transactions.last();
                    (
                        active,
                        report.highest_segment_id + 1,
                        report.resume_lsn,
                        last.map(|tx| tx.commit_lsn).unwrap_or(0),
                        // tx_id counts per writer lifetime; a restart continues from the highest
                        // committed + 1 so IDs stay unique in the log (debugging aid)
                        last.map(|tx| tx.tx_id + 1).unwrap_or(1),
                    )
                }
            };
        Ok(Self {
            options,
            active_path: active.path().to_path_buf(),
            active_start_lsn: active.header().start_lsn,
            active_end_lsn: end_lsn_of(&active),
            next_segment_id,
            next_lsn: AtomicU64::new(next_lsn),
            last_committed_lsn: AtomicU64::new(last_committed_lsn),
            next_tx_id: AtomicU64::new(next_tx_id),
            metrics: LogWriterMetrics::default(),
            sync: Arc::new(SyncControl::new(active, next_lsn)),
        })
    }

    /// Snapshot of the counters. Last-write-wins (no atomic ordering); read from the context
    /// driving commits.
    pub fn metrics(&self) -> LogWriterMetrics {
        let mut m = self.metrics;
        m.fdatasyncs += self.sync.fdatasyncs.load(Ordering::Relaxed);
        m
    }

    /// New transaction ID for the COMMIT record's `tx_id`. Monotonic per writer
    /// lifetime, NOT durable across restarts (`commit_ts` is).
    pub fn allocate_tx_id(&self) -> u64 {
        self.next_tx_id.fetch_add(1, Ordering::Relaxed)
    }

    /// LSN of the last appended COMMIT record.
    pub fn last_committed_lsn(&self) -> u64 {
        self.last_committed_lsn.load(Ordering::Acquire)
    }

    /// LSN the next record byte gets.
    pub fn next_lsn(&self) -> u64 {
        self.next_lsn.load(Ordering::Acquire)
    }

    /// Active segment path (testing/debugging).
    pub fn active_segment_path(&self) -> &Path {
        &self.active_path
    }

    /// Commits `ops` as one transaction: one `Data` record per op (LSNs in order),
    /// then a `Commit` with the tx_id and batch CRC, and waits until it's durable;
    /// `last_committed_lsn` then points at the COMMIT record.
    ///
    /// `commit_ts` is the encoded [`HybridTimestamp`](crate::tx::timestamp::HybridTimestamp) of the
    /// calling transaction.
    pub fn commit_batch(&mut self, ops: &[Op], commit_ts: u64) -> Result<CommitOutcome> {
        let outcome = self.append_batch(ops, commit_ts)?;
        self.sync.wait_durable(outcome.next_lsn)?;
        Ok(outcome)
    }

    /// Appends a transaction's records WITHOUT waiting for durability (the fast half
    /// of group commit). Before acking, the caller must
    /// [`SyncControl::wait_durable`] on `outcome.next_lsn` (via
    /// [`sync_handle`](Self::sync_handle)), ideally AFTER releasing the writer mutex
    /// so concurrent committers share the write and the sync.
    pub fn append_batch(&mut self, ops: &[Op], commit_ts: u64) -> Result<CommitOutcome> {
        if ops.is_empty() {
            return Err(Error::InvalidFormat(
                "rlog: refuse to commit empty batch".to_string(),
            ));
        }
        let tx_id = self.allocate_tx_id();
        self.append_batch_with_tx_id(ops, tx_id, commit_ts)
    }

    /// Group-commit durability handle, waitable outside the writer mutex.
    pub fn sync_handle(&self) -> Arc<SyncControl> {
        Arc::clone(&self.sync)
    }

    /// Forces everything appended so far to disk (snapshot watermarks, graceful shutdown).
    pub fn sync_now(&self) -> Result<()> {
        self.sync.sync_now()
    }

    /// Append-only half with a caller-supplied `tx_id`.
    pub fn append_batch_with_tx_id(
        &mut self,
        ops: &[Op],
        tx_id: u64,
        commit_ts: u64,
    ) -> Result<CommitOutcome> {
        // 1. encode all DATA records into one buffer, then the COMMIT; batch_crc covers the DATA
        //    bytes
        let start_lsn = self.next_lsn.load(Ordering::Acquire);
        let mut batch = Vec::with_capacity(estimate_batch_size(ops));
        let mut record_lsns: Vec<u64> = Vec::with_capacity(ops.len());
        let mut cursor_lsn = start_lsn;
        for op in ops {
            let payload = op.to_payload_bytes()?;
            record_lsns.push(cursor_lsn);
            let size = encode_record(&mut batch, RecordKind::Data, cursor_lsn, &payload)?;
            cursor_lsn += size as u64;
        }
        let commit_payload = CommitPayload {
            prev_commit_lsn: self.last_committed_lsn.load(Ordering::Acquire),
            tx_id,
            commit_ts,
            batch_crc: crc32fast::hash(&batch),
        };
        let commit_lsn = cursor_lsn;
        let commit_size = encode_record(
            &mut batch,
            RecordKind::Commit,
            commit_lsn,
            &commit_payload.to_bytes(),
        )?;
        let next_lsn_after = commit_lsn + commit_size as u64;

        // 2. a transaction's records stay in one segment: rotate first if they don't fit (LSNs
        //    continue, so the encoding stands). One larger than a segment gets a segment sized to
        //    hold it; the rotation after it goes back to the configured size
        if next_lsn_after > self.active_end_lsn {
            let needed = SEGMENT_HEADER_SIZE as u64 + batch.len() as u64;
            self.rotate_segment_sized(needed.max(self.options.segment_size))?;
        }

        // 3. queue (written + synced by the group-commit leader) and publish
        self.sync.buffer(&batch, next_lsn_after, commit_ts);
        self.next_lsn.store(next_lsn_after, Ordering::Release);
        self.last_committed_lsn.store(commit_lsn, Ordering::Release);
        self.metrics.commits += 1;
        self.metrics.data_records += ops.len() as u64;
        self.metrics.bytes_written += batch.len() as u64;

        Ok(CommitOutcome {
            tx_id,
            commit_lsn,
            data_record_lsns: record_lsns,
            next_lsn: next_lsn_after,
        })
    }

    /// Closes the current segment (everything pending written and synced into it) and opens a new
    /// one; LSNs continue without a gap.
    pub fn rotate_segment(&mut self) -> Result<()> {
        self.rotate_segment_sized(self.options.segment_size)
    }

    /// [`rotate_segment`](Self::rotate_segment) into a segment of `capacity` bytes.
    fn rotate_segment_sized(&mut self, capacity: u64) -> Result<()> {
        let segment_id = self.next_segment_id;
        let start_lsn = self.next_lsn.load(Ordering::Acquire);
        let path = segment_path(&self.options.dir, &self.options.stem, segment_id);
        let header = SegmentHeader {
            segment_id,
            start_lsn,
            created_ns: now_ns(),
            salt: random_salt(),
        };
        let new_path = path.clone();
        self.sync
            .rotate(move || SegmentWriter::create(path, header, capacity))?;
        self.active_path = new_path;
        self.active_start_lsn = start_lsn;
        self.active_end_lsn = start_lsn + capacity - SEGMENT_HEADER_SIZE as u64;
        self.next_segment_id += 1;
        self.metrics.rotations += 1;
        Ok(())
    }

    /// Segment paths owned by this writer, by segment_id.
    pub fn segment_paths(&self) -> Result<Vec<PathBuf>> {
        discover_segments(&self.options.dir, &self.options.stem)
    }

    /// Moves the log to the name of another `.tvdb` (a renamed database): rotates into a fresh
    /// segment named after `tvdb_path`, LSNs continuing. Segments under the old name stay, all
    /// closed; delete them once a snapshot of the database at its new path covers them.
    pub fn retarget(&mut self, tvdb_path: &Path) -> Result<()> {
        let options = WriterOptions {
            segment_size: self.options.segment_size,
            ..WriterOptions::from_tvdb_path(tvdb_path)?
        };
        let previous = std::mem::replace(&mut self.options, options);
        let rotated = self.rotate_segment();
        if rotated.is_err() {
            self.options = previous;
        }
        rotated
    }

    /// Deletes every segment whose LSN range is at or below `watermark_lsn`; returns
    /// how many.
    ///
    /// **Never** deletes the active segment, even if fully below the watermark: we
    /// need one to append to. Call [`Self::rotate_segment`] first to free it too.
    ///
    /// Fsyncs the directory afterwards so the deletions are durable. Without it a
    /// crash could resurrect a segment; replay would skip it (lsn <= recovery_lsn),
    /// which is harmless, but the fsync keeps post-crash state exact.
    pub fn truncate_at(&mut self, watermark_lsn: u64) -> Result<usize> {
        use crate::storage::rlog::segment::SegmentReader;

        // every segment with its start_lsn; a closed segment ends where the next one starts
        let segment_paths = discover_segments(&self.options.dir, &self.options.stem)?;
        let mut headers: Vec<(PathBuf, u64)> = Vec::with_capacity(segment_paths.len());
        for path in &segment_paths {
            let reader = SegmentReader::open(path)?;
            headers.push((path.clone(), reader.header().start_lsn));
        }
        headers.sort_by_key(|h| h.1);

        let mut deleted = 0;
        for i in 0..headers.len() {
            let path = &headers[i].0;
            if path == &self.active_path {
                continue; // never the active segment
            }
            let end_lsn = if i + 1 < headers.len() {
                headers[i + 1].1.saturating_sub(1)
            } else {
                // closed segment with no successor: shouldn't happen (nothing after the active
                // segment). Be conservative and treat it as ending at active.start_lsn - 1.
                self.active_start_lsn.saturating_sub(1)
            };
            if end_lsn <= watermark_lsn {
                std::fs::remove_file(path)?;
                deleted += 1;
            }
        }

        // fsync the directory so the deletions stick
        if deleted > 0
            && let Ok(dir) = std::fs::File::open(&self.options.dir)
        {
            let _ = dir.sync_all();
        }

        Ok(deleted)
    }

    /// Closes the log and deletes all of its segments, the active one included, when the
    /// database file holds everything in them: `covered_lsn`, the position the file was saved
    /// at, is at or past [`next_lsn`](Self::next_lsn). A clean close then leaves the `.tvdb`
    /// alone; the next open starts a new log past the file's position. Returns whether it
    /// deleted the log (`false`: something logged isn't in the file, so it stays).
    pub fn retire(self, covered_lsn: u64) -> Result<bool> {
        if self.next_lsn() > covered_lsn {
            return Ok(false);
        }
        self.sync_now()?;
        let (dir, stem) = (self.options.dir.clone(), self.options.stem.clone());
        drop(self); // closes the active segment
        remove_segments(&discover_segments(&dir, &stem)?)?;
        Ok(true)
    }
}

impl Drop for LogWriter {
    /// A writer going away cleanly leaves everything it appended on disk.
    fn drop(&mut self) {
        let _ = self.sync.sync_now();
    }
}

/// Result of a successful commit.
#[derive(Debug, Clone)]
pub struct CommitOutcome {
    /// Transaction ID assigned to this batch.
    pub tx_id: u64,
    /// Global LSN of the COMMIT record.
    pub commit_lsn: u64,
    /// Global LSNs of the DATA records, in input order.
    pub data_record_lsns: Vec<u64>,
    /// Global LSN of the next record byte.
    pub next_lsn: u64,
}

/// Rough encoded size of a batch, only to size the buffer; `encode_record` decides the real size.
fn estimate_batch_size(ops: &[Op]) -> usize {
    // ~64 bytes per op on average (small deletes under, big creates over) + room for the COMMIT
    ops.len() * 64 + 64
}

fn now_ns() -> u64 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Per-segment salt. Not cryptographic; just makes the segment distinct from recycled bytes in the
/// pre-allocated region.
fn random_salt() -> [u8; 16] {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // splat a 64-bit splitmix into 16 bytes
    let mut x = ns as u64;
    let mut out = [0u8; 16];
    for chunk in out.as_chunks_mut::<8>().0 {
        x = x.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^= z >> 31;
        chunk.copy_from_slice(&z.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::property::PropertyValue;
    use crate::storage::rlog::record::peek_next_record;
    use crate::storage::rlog::segment::{SegmentReader, remove_segments, segments_of};
    use tempfile::TempDir;

    fn opts(dir: &Path) -> WriterOptions {
        WriterOptions {
            segment_size: MIN_SEGMENT_SIZE,
            stem: "test.tvdb".to_string(),
            dir: dir.to_path_buf(),
        }
    }

    #[test]
    fn unwaited_appends_reach_disk_on_drop() {
        let tmp = TempDir::new().unwrap();
        let outcomes: Vec<CommitOutcome> = {
            let mut w = LogWriter::open(opts(tmp.path())).unwrap();
            let outcomes = (0..50)
                .map(|i| {
                    w.append_batch(&[Op::DeleteNode { node_id: i }], 100 + i)
                        .unwrap()
                })
                .collect();
            // nothing waited: nothing written yet
            assert_eq!(w.metrics().fdatasyncs, 0);
            outcomes
            // the drop writes and syncs everything appended
        };
        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        assert!(report.is_clean(), "status = {:?}", report.status);
        assert_eq!(report.transactions.len(), 50);
        assert_eq!(
            report.transactions.last().unwrap().commit_lsn,
            outcomes.last().unwrap().commit_lsn
        );
    }

    #[test]
    fn one_wait_covers_every_earlier_append() {
        let tmp = TempDir::new().unwrap();
        let mut w = LogWriter::open(opts(tmp.path())).unwrap();
        let mut last = 0;
        for i in 0..10 {
            last = w
                .append_batch(&[Op::DeleteNode { node_id: i }], 100 + i)
                .unwrap()
                .next_lsn;
        }
        let sync = w.sync_handle();
        sync.wait_durable(last).unwrap();
        // one leader wrote and synced all ten
        assert_eq!(w.metrics().fdatasyncs, 1);
        sync.wait_durable(last).unwrap();
        assert_eq!(w.metrics().fdatasyncs, 1, "already durable: no second sync");
    }

    #[test]
    fn writer_open_creates_first_segment() {
        let tmp = TempDir::new().unwrap();
        let writer = LogWriter::open(opts(tmp.path())).unwrap();
        assert!(writer.active_segment_path().exists());
        assert_eq!(writer.next_lsn(), 0);
        assert_eq!(writer.last_committed_lsn(), 0);
    }

    #[test]
    fn writer_resumes_after_clean_close() {
        let tmp = TempDir::new().unwrap();
        let first_outcome;
        {
            let mut w = LogWriter::open(opts(tmp.path())).unwrap();
            first_outcome = w
                .commit_batch(
                    &[Op::CreateNode {
                        node_id: 1,
                        labels: vec!["N".to_string()],
                        properties: vec![],
                    }],
                    100,
                )
                .unwrap();
        }
        // reopen: resumes at the post-COMMIT offset, doesn't refuse
        let mut w2 = LogWriter::open(opts(tmp.path())).unwrap();
        assert_eq!(w2.next_lsn(), first_outcome.next_lsn);
        assert_eq!(w2.last_committed_lsn(), first_outcome.commit_lsn);
        // tx_id continues where we left off
        assert_eq!(w2.allocate_tx_id(), first_outcome.tx_id + 1);

        // next commit appends cleanly
        let second_outcome = w2
            .commit_batch(&[Op::DeleteNode { node_id: 1 }], 200)
            .unwrap();
        assert!(second_outcome.commit_lsn > first_outcome.commit_lsn);
    }

    #[test]
    fn writer_resume_into_empty_dir_creates_segment_0() {
        let tmp = TempDir::new().unwrap();
        let w = LogWriter::open(opts(tmp.path())).unwrap();
        assert!(w.active_segment_path().exists());
        let name = w
            .active_segment_path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        assert!(name.ends_with("0000000000"), "name = {}", name);
    }

    #[test]
    fn commit_batch_advances_lsn_and_writes_to_disk() {
        let tmp = TempDir::new().unwrap();
        let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
        let ops = vec![
            Op::CreateNode {
                node_id: 1,
                labels: vec!["N".to_string()],
                properties: vec![],
            },
            Op::CreateNode {
                node_id: 2,
                labels: vec!["N".to_string()],
                properties: vec![("name".to_string(), PropertyValue::String("x".to_string()))],
            },
        ];
        let outcome = writer.commit_batch(&ops, 1000).unwrap();
        assert_eq!(outcome.tx_id, 1);
        assert_eq!(outcome.data_record_lsns.len(), 2);
        assert!(outcome.commit_lsn > outcome.data_record_lsns[1]);
        assert_eq!(outcome.next_lsn, writer.next_lsn());
        assert_eq!(outcome.commit_lsn, writer.last_committed_lsn());

        // read back: 2 DATA + 1 COMMIT
        let mut reader = SegmentReader::open(writer.active_segment_path()).unwrap();
        let region = reader.read_all_records().unwrap();

        let mut cursor = 0usize;
        let mut data_count = 0;
        let mut commit_count = 0;
        while let Some((header, _payload, size)) = peek_next_record(&region[cursor..]).unwrap() {
            match header.kind {
                RecordKind::Data => data_count += 1,
                RecordKind::Commit => commit_count += 1,
                RecordKind::NoopPad => {}
            }
            cursor += size;
            // stop at the end, or at the zero region after the records (kind 0 fails validation)
            if cursor >= region.len() || region[cursor] == 0 {
                break;
            }
        }
        assert_eq!(data_count, 2);
        assert_eq!(commit_count, 1);
    }

    #[test]
    fn commit_batch_rejects_empty() {
        let tmp = TempDir::new().unwrap();
        let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
        let err = writer.commit_batch(&[], 0).unwrap_err();
        assert!(matches!(err, Error::InvalidFormat(_)));
    }

    #[test]
    fn rotate_segment_creates_new_file_and_continues_lsn() {
        let tmp = TempDir::new().unwrap();
        let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
        let ops = vec![Op::DeleteNode { node_id: 1 }];
        let o1 = writer.commit_batch(&ops, 1).unwrap();
        let lsn_before_rotate = writer.next_lsn();
        writer.rotate_segment().unwrap();
        let new_segment = writer.active_segment_path().to_path_buf();
        assert_ne!(new_segment, segment_path(tmp.path(), "test.tvdb", 0));

        let o2 = writer.commit_batch(&ops, 2).unwrap();
        assert!(o2.tx_id > o1.tx_id);
        assert!(o2.data_record_lsns[0] >= lsn_before_rotate);

        // both segments exist on disk
        let segments = writer.segment_paths().unwrap();
        assert_eq!(segments.len(), 2);
    }

    #[test]
    fn retarget_logs_on_under_the_new_name() {
        let tmp = TempDir::new().unwrap();
        let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
        let ops = vec![Op::DeleteNode { node_id: 1 }];
        writer.commit_batch(&ops, 1).unwrap();
        let moved_at = writer.next_lsn();
        writer.retarget(&tmp.path().join("renamed.tvdb")).unwrap();
        let after = writer.commit_batch(&ops, 2).unwrap();
        assert!(after.commit_lsn >= moved_at, "LSNs continue");

        let old = segments_of(&tmp.path().join("test.tvdb")).unwrap();
        let new = segments_of(&tmp.path().join("renamed.tvdb")).unwrap();
        assert_eq!((old.len(), new.len()), (1, 1));
        // a snapshot taken at the move covers the old name's segments; the new name has the rest
        let report = scan_segments(tmp.path(), "renamed.tvdb", moved_at).unwrap();
        assert_eq!(report.transactions.len(), 1);
        assert_eq!(report.transactions[0].commit_lsn, after.commit_lsn);
        remove_segments(&old).unwrap();
        assert!(
            segments_of(&tmp.path().join("test.tvdb"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn rotate_on_segment_full() {
        let tmp = TempDir::new().unwrap();
        let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
        // one CreateNode with a huge string, so the next commit has to rotate
        let big_str = "x".repeat(40_000);
        let ops = vec![Op::CreateNode {
            node_id: 1,
            labels: vec!["N".to_string()],
            properties: vec![("data".to_string(), PropertyValue::String(big_str))],
        }];
        // first commit fits in segment 0
        let _o1 = writer.commit_batch(&ops, 1).unwrap();
        // later commits overflow and rotate
        let mut prev_segments = writer.segment_paths().unwrap().len();
        let mut total_iters = 0;
        loop {
            let _ = writer.commit_batch(&ops, 2).unwrap();
            let cur = writer.segment_paths().unwrap().len();
            if cur > prev_segments {
                prev_segments = cur;
                break;
            }
            total_iters += 1;
            assert!(total_iters < 50, "rotation never triggered");
        }
        assert!(prev_segments >= 2);
    }

    #[test]
    fn a_transaction_larger_than_a_segment_gets_one_of_its_own() {
        let tmp = TempDir::new().unwrap();
        let node = |id: u64, data: String| Op::CreateNode {
            node_id: id,
            labels: vec![],
            properties: vec![("data".to_string(), PropertyValue::String(data))],
        };
        {
            let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
            writer.commit_batch(&[node(1, String::new())], 1).unwrap();
            // three segments' worth in one transaction
            let huge: Vec<Op> = (0..3)
                .map(|i| node(10 + i, "y".repeat(MIN_SEGMENT_SIZE as usize)))
                .collect();
            writer.commit_batch(&huge, 2).unwrap();
            writer.commit_batch(&[node(2, String::new())], 3).unwrap();
            let sizes: Vec<u64> = writer
                .segment_paths()
                .unwrap()
                .iter()
                .map(|p| std::fs::metadata(p).unwrap().len())
                .collect();
            assert_eq!(sizes.len(), 3);
            assert!(
                sizes[1] > 3 * MIN_SEGMENT_SIZE,
                "holds the transaction whole"
            );
            assert_eq!(
                sizes[2], MIN_SEGMENT_SIZE,
                "then back to the configured size"
            );
        }
        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        assert!(report.is_clean());
        let commits: Vec<u64> = report.transactions.iter().map(|t| t.commit_ts).collect();
        assert_eq!(commits, [1, 2, 3]);
        assert_eq!(report.transactions[1].ops.len(), 3);

        // the writer resumes after it
        let mut writer = LogWriter::resume(opts(tmp.path()), &report).unwrap();
        writer.commit_batch(&[node(3, String::new())], 4).unwrap();
        drop(writer);
        assert_eq!(
            scan_segments(tmp.path(), "test.tvdb", 0)
                .unwrap()
                .transactions
                .len(),
            4
        );
    }

    #[test]
    fn a_value_larger_than_sixteen_mib_commits_and_replays() {
        let tmp = TempDir::new().unwrap();
        let text = PropertyValue::String("z".repeat(20 << 20));
        {
            let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
            let op = Op::CreateNode {
                node_id: 1,
                labels: vec![],
                properties: vec![("t".to_string(), text.clone())],
            };
            writer.commit_batch(&[op], 1).unwrap();
        }
        let report = scan_segments(tmp.path(), "test.tvdb", 0).unwrap();
        assert!(report.is_clean());
        match &report.transactions[0].ops[0] {
            Op::CreateNode { properties, .. } => assert_eq!(properties[0].1, text),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn commit_batch_durably_advances_committed_lsn() {
        let tmp = TempDir::new().unwrap();
        let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
        let ops1 = vec![Op::CreateNode {
            node_id: 1,
            labels: vec![],
            properties: vec![],
        }];
        let ops2 = vec![Op::DeleteNode { node_id: 1 }];
        let o1 = writer.commit_batch(&ops1, 100).unwrap();
        assert_eq!(writer.last_committed_lsn(), o1.commit_lsn);
        let o2 = writer.commit_batch(&ops2, 200).unwrap();
        assert_eq!(writer.last_committed_lsn(), o2.commit_lsn);
        assert!(o2.commit_lsn > o1.commit_lsn);
    }

    #[test]
    fn from_tvdb_path_extracts_stem_and_dir() {
        let opts = WriterOptions::from_tvdb_path(Path::new("/tmp/foo/bar.tvdb")).unwrap();
        assert_eq!(opts.stem, "bar.tvdb");
        assert_eq!(opts.dir, Path::new("/tmp/foo"));
    }

    #[test]
    fn truncate_at_deletes_closed_segments_below_watermark() {
        let tmp = TempDir::new().unwrap();
        let mut w = LogWriter::open(opts(tmp.path())).unwrap();
        // three rotations -> 4 segments (0..=3, 3 active)
        let big = "z".repeat(20_000);
        let ops = vec![Op::CreateNode {
            node_id: 1,
            labels: vec!["N".to_string()],
            properties: vec![("data".to_string(), PropertyValue::String(big.clone()))],
        }];
        let mut commit_lsns = Vec::new();
        // loop until 4 segments
        loop {
            let outcome = w.commit_batch(&ops, 0).unwrap();
            commit_lsns.push(outcome.commit_lsn);
            if w.segment_paths().unwrap().len() >= 4 {
                break;
            }
            assert!(
                commit_lsns.len() < 200,
                "rotation never produced 4 segments"
            );
        }

        // watermark inside segment 1, from its start_lsn
        let segs = w.segment_paths().unwrap();
        assert!(segs.len() >= 4);
        // watermark just below segment 2: only segment 0 fully reflected
        let seg2_reader = crate::storage::rlog::segment::SegmentReader::open(&segs[2]).unwrap();
        let watermark = seg2_reader.header().start_lsn - 1;

        let n = w.truncate_at(watermark).unwrap();
        assert!(n >= 1, "expected at least one deletion, got {}", n);

        let remaining = w.segment_paths().unwrap();
        // segs[0] gone
        assert!(!remaining.contains(&segs[0]));
        // active segment must still be present.
        assert!(remaining.contains(&w.active_segment_path().to_path_buf()));
    }

    #[test]
    fn truncate_at_never_removes_active_segment_even_if_watermark_is_high() {
        let tmp = TempDir::new().unwrap();
        let mut w = LogWriter::open(opts(tmp.path())).unwrap();
        let outcome = w.commit_batch(&[Op::DeleteNode { node_id: 1 }], 0).unwrap();
        // Watermark past everything we've written.
        let n = w.truncate_at(outcome.next_lsn + 1000).unwrap();
        // Active segment kept, so zero deletions.
        assert_eq!(n, 0);
        assert!(w.active_segment_path().exists());
    }

    #[test]
    fn rotate_then_truncate_can_clear_everything_old() {
        let tmp = TempDir::new().unwrap();
        let mut w = LogWriter::open(opts(tmp.path())).unwrap();
        let _ = w.commit_batch(&[Op::DeleteNode { node_id: 1 }], 0).unwrap();
        let before = w.segment_paths().unwrap().len();
        assert_eq!(before, 1);

        // Rotate so the old segment is closed.
        w.rotate_segment().unwrap();
        let active_after_rotate = w.active_segment_path().to_path_buf();
        let watermark = w
            .active_segment_path()
            .is_file()
            .then(|| {
                let r = crate::storage::rlog::segment::SegmentReader::open(&active_after_rotate)
                    .unwrap();
                r.header().start_lsn.saturating_sub(1)
            })
            .unwrap();

        let n = w.truncate_at(watermark).unwrap();
        assert_eq!(n, 1, "expected the just-closed segment to be deleted");
        assert_eq!(w.segment_paths().unwrap().len(), 1);
        // The active segment is still there.
        assert!(w.active_segment_path().exists());
    }

    /// Smoke check: header records carry the LSN we expect.
    #[test]
    fn record_lsns_match_writer_assignments() {
        let tmp = TempDir::new().unwrap();
        let mut writer = LogWriter::open(opts(tmp.path())).unwrap();
        let ops = vec![
            Op::DeleteNode { node_id: 1 },
            Op::DeleteNode { node_id: 2 },
            Op::DeleteNode { node_id: 3 },
        ];
        let outcome = writer.commit_batch(&ops, 0).unwrap();

        let mut reader = SegmentReader::open(writer.active_segment_path()).unwrap();
        let region = reader.read_all_records().unwrap();

        let mut cursor = 0usize;
        let mut data_lsns = Vec::new();
        let mut commit_lsn = None;
        loop {
            if cursor >= region.len() || region[cursor] == 0 {
                break;
            }
            let (header, _payload, size) = peek_next_record(&region[cursor..]).unwrap().unwrap();
            match header.kind {
                RecordKind::Data => data_lsns.push(header.lsn),
                RecordKind::Commit => commit_lsn = Some(header.lsn),
                _ => {}
            }
            cursor += size;
        }
        assert_eq!(data_lsns, outcome.data_record_lsns);
        assert_eq!(commit_lsn, Some(outcome.commit_lsn));
    }
}
