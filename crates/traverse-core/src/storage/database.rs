//! `Database`: `Graph` + file lock + persistent `PageManager`.
//!
//! `open(path)` creates or loads an `.tvdb` file fully into memory, `flush()`
//! rewrites only dirty entities in place, `compact()` does a full atomic rewrite
//! (temp + rename).

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

#[cfg(feature = "file-locking")]
use fs2::FileExt;

use std::ops::{Deref, DerefMut};

use crate::error::{Error, Result};
use crate::graph::Graph;
use crate::graph::types::{EdgeId, NodeId};
use crate::graph::{CatalogParts, DirtySet};
use crate::storage::codec::{self, RecordSource};
use crate::storage::compression;
use crate::storage::free_space::FreeSpaceMap;
use crate::storage::location::{LocationIndex, RecordLocation};
use crate::storage::overflow;
use crate::storage::page::{PageType, SlottedPage};
use crate::storage::page_manager::PageManager;
use crate::storage::rlog::LogWriter;
use crate::storage::wal;
use crate::sync::WriterGuard;
use crate::tx::owned::Anchor;
use crate::tx::timestamp::HybridTimestamp;
use crate::tx::{Checkpoint, GraphView, MvccStore};
use crate::tx::{OptimisticTx, OwnedOptimisticTx, OwnedReadTx, OwnedWriteTx, ReadTx, WriteTx};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Read access to the live graph of a [`Database`] (see [`Database::graph`]). Derefs to [`Graph`].
pub struct GraphRef<'a> {
    graph: &'a Graph,
    _rw: crate::sync::SlottedReadGuard<'a>,
}

impl Deref for GraphRef<'_> {
    type Target = Graph;
    fn deref(&self) -> &Graph {
        self.graph
    }
}

/// Exclusive write access to the graph of a [`Database`] (see [`Database::graph_mut`]). Derefs
/// (mutably) to [`Graph`].
pub struct GraphRefMut<'a> {
    graph: &'a mut Graph,
    // drop order: the structural lock before the writer lock
    _rw: crate::sync::SlottedWriteGuard<'a>,
    _guard: WriterGuard<'a>,
}

impl Deref for GraphRefMut<'_> {
    type Target = Graph;
    fn deref(&self) -> &Graph {
        self.graph
    }
}

impl DerefMut for GraphRefMut<'_> {
    fn deref_mut(&mut self) -> &mut Graph {
        self.graph
    }
}

/// Embedded graph database backed by one `.tvdb` file.
///
/// Loads the whole graph on open and keeps a `PageManager` for incremental
/// in-place `flush()`; `compact()` rewrites atomically (temp + rename) to defragment.
///
/// `Send + Sync`: share it as an `Arc<Database>` and begin owned transactions on it
/// ([`begin_write_owned`](Database::begin_write_owned) and friends), which hold the `Arc`.
///
/// # Transactions
///
/// [`begin_read`](Database::begin_read) / [`begin_write`](Database::begin_write)
/// give MVCC access with snapshot isolation.
pub struct Database {
    path: PathBuf,
    mvcc: MvccStore,
    /// Page file state for saves; locked after the writer lock. A save from a checkpoint locks it
    /// before the writer lock goes, so saves are written in the order of their checkpoints.
    files: Mutex<Files>,
    /// Sidecar `.lock` file, open for the database's lifetime. With `file-locking`
    /// it holds the exclusive lock (released on close); without, the handle is just kept.
    #[cfg_attr(not(feature = "file-locking"), allow(dead_code))]
    lock_file: File,
    /// Raw metadata page bytes (e.g. auth catalog).
    metadata: Vec<u8>,
    /// Skip flushing in `close()`/`drop()`. Crash-recovery tests use it to simulate dying before a
    /// snapshot.
    suppress_flush: bool,
    /// Transactions the redo log replayed on top of the file at open.
    replayed: usize,
    /// What the last save covered.
    saved: Saved,
}

/// The redo-log position and the commit (encoded) the last save covered: what's newer is unsaved.
#[derive(Debug, Default)]
struct Saved {
    position: AtomicU64,
    commit: AtomicU64,
}

/// What incremental saves carry from one to the next.
struct Files {
    /// `None` before the first save and after a failed one: the next save rewrites the file.
    page_manager: Option<PageManager>,
    locations: LocationIndex,
    /// Data pages with room; built on the first in-place save after the file was opened or
    /// rewritten.
    free_space: Option<FreeSpaceMap>,
}

impl Database {
    /// Opens or creates a database at `path`: loads the graph if the file exists,
    /// else starts empty. Holds an exclusive file lock while open.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_durability(path, true)
    }

    /// Opens a database with or without per-commit redo-log durability. `durable =
    /// true` ([`Database::open`]'s default): each committed `WriteTx` is appended and
    /// fsynced to the rlog, and open replays the committed tail past the snapshot's
    /// `recovery_lsn`. `durable = false`: writes stay in memory until an explicit
    /// flush/save (raw write-throughput benchmarks).
    pub fn open_with_durability(path: impl AsRef<Path>, durable: bool) -> Result<Self> {
        let path = path.as_ref().to_path_buf();

        // exclusive lock via a sidecar .lock file
        let lock_path = path.with_extension("tvdb.lock");
        let lock_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(&lock_path)?;
        #[cfg(feature = "file-locking")]
        lock_file
            .try_lock_exclusive()
            .map_err(|_| Error::DatabaseLocked(path.clone()))?;

        // remove a stale temp file from a previous crash
        let tmp_path = path.with_extension("tvdb.tmp");
        if tmp_path.exists() {
            let _ = std::fs::remove_file(&tmp_path);
        }

        // replay committed WAL from a previous crash, if any
        if path.exists() && std::fs::metadata(&path)?.len() > 0 {
            PageManager::recover(&path)?;
        }

        // remove a stale uncommitted WAL file
        let wal_file = wal::wal_path(&path);
        if wal_file.exists() {
            let _ = std::fs::remove_file(&wal_file);
        }

        if path.exists() && std::fs::metadata(&path)?.len() > 0 {
            let (graph, locations, pm, metadata) = Self::load_graph_with_locations(&path)?;
            let recovery_lsn = pm.header().recovery_lsn;
            let (mvcc, replayed) = Self::init_store(&path, graph, recovery_lsn, durable)?;
            // commits replayed from the log are newer than the file
            let saved = Saved {
                position: AtomicU64::new(recovery_lsn),
                commit: AtomicU64::new(if replayed > 0 {
                    0
                } else {
                    mvcc.last_commit_ts.load(Ordering::Acquire)
                }),
            };
            Ok(Self {
                path,
                mvcc,
                files: Mutex::new(Files {
                    page_manager: Some(pm),
                    locations,
                    free_space: None,
                }),
                lock_file,
                metadata,
                suppress_flush: false,
                replayed,
                saved,
            })
        } else {
            // fresh database: snapshot reflects nothing, replay from LSN 0
            let (mvcc, replayed) = Self::init_store(&path, Graph::new(), 0, durable)?;
            Ok(Self {
                path,
                mvcc,
                files: Mutex::new(Files {
                    page_manager: None,
                    locations: LocationIndex::new(),
                    free_space: None,
                }),
                lock_file,
                metadata: Vec::new(),
                suppress_flush: false,
                replayed,
                saved: Saved::default(),
            })
        }
    }

    /// `MvccStore` for a freshly loaded graph, and how many transactions it replayed. `durable`:
    /// replays the committed rlog tail (`commit_lsn > recovery_lsn`) onto the snapshot and
    /// attaches a resumed `LogWriter` so later commits append + fsync. Otherwise a plain store.
    fn init_store(
        path: &Path,
        graph: Graph,
        recovery_lsn: u64,
        durable: bool,
    ) -> Result<(MvccStore, usize)> {
        use crate::storage::rlog::{WriterOptions, apply_transactions, scan_segments};
        if !durable {
            // Snapshot mode, no writer. A previous run may have crashed in WAL mode,
            // leaving acknowledged commits the snapshot lacks: replay them, remember the
            // floor so the next snapshot save stamps it, and leave the segments (stale once
            // that snapshot exists).
            let store = MvccStore::new(graph);
            if let Ok(options) = WriterOptions::from_tvdb_path(path)
                && let Ok(report) = scan_segments(&options.dir, &options.stem, recovery_lsn)
                && !report.transactions.is_empty()
            {
                apply_transactions(&store, &report.transactions)?;
                store.set_recovery_floor(report.resume_lsn);
                return Ok((store, report.transactions.len()));
            }
            return Ok((store, 0));
        }

        let options = WriterOptions::from_tvdb_path(path)?;
        // One scan: `min_lsn = recovery_lsn` picks the transactions to replay, while the
        // physical resume position is tracked regardless, so one report drives both.
        let report = scan_segments(&options.dir, &options.stem, recovery_lsn)?;

        let store = MvccStore::new(graph);
        // replay the tail the snapshot lacks; skip_redo in apply_transactions keeps it from being
        // re-logged
        if !report.transactions.is_empty() {
            apply_transactions(&store, &report.transactions)?;
        }
        // resume the writer after the tail (creates segment 0 for a fresh log); refuses an
        // unreadable segment header
        let writer = LogWriter::resume(options, &report)?;
        let replayed = report.transactions.len();
        Ok((
            store.with_log_writer(Arc::new(Mutex::new(writer))),
            replayed,
        ))
    }

    /// Redo-log position a save now reflects: the next LSN the writer would assign (everything
    /// below is logged and in the graph). Without a writer, the floor of segments replayed at
    /// open, so the next open doesn't apply them again.
    fn current_recovery_lsn(&self) -> u64 {
        match self.mvcc.log_writer() {
            Some(w) => w.lock().unwrap_or_else(|e| e.into_inner()).next_lsn(),
            None => self.mvcc.recovery_floor(),
        }
    }

    /// After a snapshot advances `recovery_lsn`, deletes closed rlog segments it fully
    /// covers. Best effort: replay filters by `recovery_lsn` anyway, so failure only
    /// wastes disk. The active segment is always kept.
    fn truncate_rlog(&self, watermark_lsn: u64) {
        if watermark_lsn == 0 {
            return;
        }
        if let Some(writer) = self.mvcc.log_writer() {
            let mut w = writer.lock().unwrap_or_else(|e| e.into_inner());
            let _ = w.truncate_at(watermark_lsn);
        }
    }

    /// After the final save: when the file holds every logged commit, the redo log goes, so a
    /// closed database is its `.tvdb` alone. Runs while the file lock is held, so no other
    /// process can be appending to the log it deletes. Keeps the log when it holds anything the
    /// file doesn't (a failed save, commits replayed at open and not saved since), or when
    /// another holder still has the writer (a server that took it with `into_parts`).
    fn retire_log(&mut self) {
        let covered = self.saved.position.load(Ordering::Acquire);
        let Some(writer) = self.mvcc.log_writer.take() else {
            return;
        };
        match Arc::try_unwrap(writer) {
            Ok(writer) => {
                let writer = writer.into_inner().unwrap_or_else(PoisonError::into_inner);
                if let Err(e) = writer.retire(covered) {
                    tracing::warn!("failed to delete the redo log of a closed database: {e}");
                }
            }
            Err(shared) => self.mvcc.log_writer = Some(shared),
        }
    }

    /// The live graph (latest state, not a snapshot; see [`begin_read`](Database::begin_read)).
    /// Holds the structural read lock: writes wait until it drops, so don't write on this thread
    /// while holding it.
    pub fn graph(&self) -> GraphRef<'_> {
        let rw = self.mvcc.graph_rw.read();
        // SAFETY: read-only, under the structural read lock held by the returned guard
        let graph = unsafe { self.mvcc.graph() };
        GraphRef { graph, _rw: rw }
    }

    /// Exclusive, non-transactional access to the graph: holds the writer mutex and the
    /// structural write lock, so readers wait until it drops. Records no versions and no redo:
    /// snapshots open across it see its changes, and a crash before the next save loses them.
    /// Use [`begin_write`](Database::begin_write) for transactions.
    pub fn graph_mut(&mut self) -> GraphRefMut<'_> {
        let guard = self.mvcc.write_lock.lock();
        let rw = self.mvcc.graph_rw.write();
        // SAFETY: we hold the write_lock and the structural write lock.
        let graph = unsafe { self.mvcc.graph_mut() };
        GraphRefMut {
            graph,
            _rw: rw,
            _guard: guard,
        }
    }

    /// Read-only transaction on a consistent snapshot as of its start. Readers
    /// coexist with each other and with one writer.
    pub fn begin_read(&self) -> crate::error::Result<ReadTx<'_>> {
        self.mvcc.begin_read()
    }

    /// Write transaction; takes the single-writer lock (blocks while another writer
    /// is active). [`WriteTx::commit`] publishes changes, [`WriteTx::abort`] drops them.
    pub fn begin_write(&self) -> WriteTx<'_> {
        self.mvcc.begin_write()
    }

    /// Optimistic transaction: reads a snapshot plus its own writes, applies them at commit if no
    /// concurrent commit changed the same objects (else [`TransactionConflict`](crate::error::Error::TransactionConflict):
    /// retry). Holds no lock while open, so long or interactive transactions block no one.
    /// `read_only` refuses every write.
    pub fn begin_optimistic(&self, read_only: bool) -> crate::error::Result<OptimisticTx<'_>> {
        self.mvcc.begin_optimistic(read_only)
    }

    /// [`begin_read`](Self::begin_read) on a shared database: the transaction holds the `Arc`, so
    /// the database outlives it.
    pub fn begin_read_owned(self: &Arc<Self>) -> Result<OwnedReadTx> {
        let tx = self.mvcc.begin_read()?;
        // SAFETY: `tx` borrows the store inside `**self`, which the anchor keeps alive
        Ok(unsafe { OwnedReadTx::new(tx, Arc::clone(self) as Anchor) })
    }

    /// [`begin_write`](Self::begin_write) on a shared database (see
    /// [`begin_read_owned`](Self::begin_read_owned)). `Send`: it may finish on another thread.
    pub fn begin_write_owned(self: &Arc<Self>) -> OwnedWriteTx {
        let tx = self.mvcc.begin_write();
        // SAFETY: as in `begin_read_owned`
        unsafe { OwnedWriteTx::new(tx, Arc::clone(self) as Anchor) }
    }

    /// [`begin_write_owned`](Self::begin_write_owned) that waits at most `timeout` for the writer
    /// lock, then fails with [`Busy`](Error::Busy).
    pub fn try_begin_write_owned(self: &Arc<Self>, timeout: Duration) -> Result<OwnedWriteTx> {
        let tx = self.mvcc.try_begin_write(timeout)?;
        // SAFETY: as in `begin_read_owned`
        Ok(unsafe { OwnedWriteTx::new(tx, Arc::clone(self) as Anchor) })
    }

    /// [`begin_optimistic`](Self::begin_optimistic) on a shared database (see
    /// [`begin_read_owned`](Self::begin_read_owned)).
    pub fn begin_optimistic_owned(self: &Arc<Self>, read_only: bool) -> Result<OwnedOptimisticTx> {
        let tx = self.mvcc.begin_optimistic(read_only)?;
        // SAFETY: as in `begin_read_owned`
        Ok(unsafe { OwnedOptimisticTx::new(tx, Arc::clone(self) as Anchor) })
    }

    /// Bulk load: a write transaction that logs nothing and makes its commit durable by saving
    /// the database (see [`BulkLoadTx`]). Waits at most `timeout` for the writer lock (`None`:
    /// as long as it takes).
    pub fn begin_bulk_load(self: &Arc<Self>, timeout: Option<Duration>) -> Result<BulkLoadTx> {
        let mut tx = match timeout {
            Some(t) => self.try_begin_write_owned(t)?,
            None => self.begin_write_owned(),
        };
        tx.set_skip_redo();
        // no file yet: the commit rewrites it whole, so nothing needs tracking as dirty
        let fresh = self
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .page_manager
            .is_none();
        if fresh {
            tx.set_bulk(true);
        }
        Ok(BulkLoadTx {
            tx,
            db: Arc::clone(self),
        })
    }

    /// Saves the changes since the last save, in place: only changed and deleted records are
    /// rewritten. Works from a checkpoint, so the writer lock is held only while that's taken:
    /// readers and commits carry on while the file is written, and what commits meanwhile is saved
    /// the next time. Waits for an active write transaction to finish before it starts. Rewrites
    /// the whole file instead (as [`compact`](Self::compact)) when it was never written, or after
    /// a failed save.
    pub fn flush(&self) -> Result<()> {
        self.save_changes(None)
    }

    /// [`flush`](Self::flush) that waits at most `timeout` for an active writer, then fails with
    /// [`Busy`](Error::Busy).
    pub fn try_flush(&self, timeout: Duration) -> Result<()> {
        self.save_changes(Some(timeout))
    }

    /// Full atomic rewrite from a checkpoint: the whole graph to a temp file renamed over the
    /// original, reclaiming the space of deleted records. Readers and commits carry on while it's
    /// written. Waits for an active write transaction to finish before it starts.
    pub fn compact(&self) -> Result<()> {
        // the changes taken are in the rewrite
        let (checkpoint, _changes, mut files) = self.begin_save(None)?;
        self.rewrite(
            &mut files,
            &checkpoint,
            checkpoint.catalog(),
            checkpoint.position(),
        )?;
        self.note_saved(checkpoint.position(), checkpoint.commit());
        Ok(())
    }

    /// Redo-log bytes written since the last save.
    pub(crate) fn unsaved_log_bytes(&self) -> u64 {
        self.current_recovery_lsn()
            .saturating_sub(self.saved.position.load(Ordering::Acquire))
    }

    /// Whether anything committed since the last save.
    pub(crate) fn has_unsaved_commits(&self) -> bool {
        self.mvcc.last_commit_ts.load(Ordering::Acquire) > self.saved.commit.load(Ordering::Acquire)
    }

    /// Whether a compaction would give back most of the file: free pages and the bytes of deleted
    /// records over half of it, the file at least `min_bytes`.
    pub(crate) fn compaction_due(&self, min_bytes: u64) -> bool {
        use crate::storage::format::PAGE_SIZE;
        let files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(pm) = &files.page_manager else {
            return false;
        };
        let size = pm.total_pages() * PAGE_SIZE as u64;
        let room = files
            .free_space
            .as_ref()
            .map_or(0, FreeSpaceMap::total_room) as u64;
        let free = pm.free_page_count() as u64 * PAGE_SIZE as u64;
        size >= min_bytes && (room + free) * 2 > size
    }

    fn note_saved(&self, position: u64, commit: u64) {
        self.saved.position.fetch_max(position, Ordering::AcqRel);
        self.saved.commit.fetch_max(commit, Ordering::AcqRel);
    }

    /// [`flush`](Self::flush), waiting at most `timeout` (`None`: as long as it takes) for the
    /// writer lock.
    fn save_changes(&self, timeout: Option<Duration>) -> Result<()> {
        let (checkpoint, changes, mut files) = self.begin_save(timeout)?;
        // on an error below `pm` drops, so the next save rewrites the file whole, with the
        // changes taken here in it
        match files.page_manager.take() {
            None => self.rewrite(
                &mut files,
                &checkpoint,
                checkpoint.catalog(),
                checkpoint.position(),
            )?,
            Some(pm) if changes.is_empty() => files.page_manager = Some(pm),
            Some(pm) => self.write_in_place(
                &mut files,
                pm,
                &changes,
                &checkpoint,
                checkpoint.catalog(),
                checkpoint.position(),
            )?,
        }
        self.note_saved(checkpoint.position(), checkpoint.commit());
        Ok(())
    }

    /// A save's start: a checkpoint, the changes since the last save, and the files, locked while
    /// the writer lock is held. A bulk load saving later under the writer lock waits for the
    /// files, so it can't be overwritten by this older save, whose position would also take the
    /// file back to before the load (which isn't logged).
    fn begin_save(
        &self,
        timeout: Option<Duration>,
    ) -> Result<(Checkpoint<'_>, DirtySet, MutexGuard<'_, Files>)> {
        self.mvcc.checkpoint_taking_changes(timeout, || {
            self.files.lock().unwrap_or_else(PoisonError::into_inner)
        })
    }

    /// [`flush`](Self::flush) for the holder of the writer lock, from the live graph: with the
    /// holder's uncommitted changes in it when a bulk load commits. An error means nothing new is
    /// durable: the failed step was before the page WAL committed.
    pub(crate) fn flush_locked(&self, writer: &WriterGuard<'_>) -> Result<()> {
        debug_assert!(writer.holds(&self.mvcc.write_lock));
        // rlog watermark, taken under the writer lock: the save reflects exactly the commits
        // logged so far
        let position = self.current_recovery_lsn();
        let changes = {
            let _rw = self.mvcc.graph_rw.write();
            // SAFETY: writer lock and structural write lock held
            unsafe { self.mvcc.graph_mut() }.take_dirty()
        };
        // SAFETY: writer lock held: nothing changes the graph while we read it
        let graph = unsafe { self.mvcc.graph() };
        let commit = self.mvcc.last_commit_ts.load(Ordering::Acquire);
        let mut files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
        match files.page_manager.take() {
            None => self.rewrite(&mut files, graph, &graph.catalog_parts(), position)?,
            Some(pm) if changes.is_empty() => files.page_manager = Some(pm),
            Some(pm) => self.write_in_place(
                &mut files,
                pm,
                &changes,
                graph,
                &graph.catalog_parts(),
                position,
            )?,
        }
        self.note_saved(position, commit);
        Ok(())
    }

    /// Writes `changes` over the file in place, reading the records from `records` (a
    /// checkpoint, or the live graph under the writer lock), then the catalog, stamped with
    /// `position`.
    fn write_in_place(
        &self,
        files: &mut Files,
        mut pm: PageManager,
        changes: &DirtySet,
        records: &dyn RecordSource,
        catalog: &CatalogParts,
        position: u64,
    ) -> Result<()> {
        let mut space = match files.free_space.take() {
            Some(space) => space,
            None => FreeSpaceMap::build(&pm)?,
        };
        let locations = &mut files.locations;

        // deleted records give up their place
        for id in &changes.deleted_nodes {
            if let Some(loc) = locations.remove_node(*id) {
                Self::remove_record(&mut pm, &mut space, loc)?;
            }
        }
        for id in &changes.deleted_edges {
            if let Some(loc) = locations.remove_edge(*id) {
                Self::remove_record(&mut pm, &mut space, loc)?;
            }
        }

        // changed and new ones: the old copy goes, the new one goes where there's room (records
        // the source doesn't have were deleted since they changed)
        let nodes: Vec<NodeId> = changes.dirty_nodes.iter().copied().collect();
        records.nodes_by_id(&nodes, &mut |node| {
            if let Some(old) = locations.remove_node(node.id) {
                Self::remove_record(&mut pm, &mut space, old)?;
            }
            let stored = overflow::store(&mut pm, codec::encode_node(node)?)?;
            locations.set_node(node.id, space.insert(&mut pm, &stored, PageType::NodeData)?);
            Ok(())
        })?;
        let edges: Vec<EdgeId> = changes.dirty_edges.iter().copied().collect();
        records.edges_by_id(&edges, &mut |edge| {
            if let Some(old) = locations.remove_edge(edge.id) {
                Self::remove_record(&mut pm, &mut space, old)?;
            }
            let stored = overflow::store(&mut pm, codec::encode_edge(edge)?)?;
            locations.set_edge(edge.id, space.insert(&mut pm, &stored, PageType::EdgeData)?);
            Ok(())
        })?;

        // the catalog, rewritten every time (it's small)
        Self::write_catalog(&mut pm, catalog)?;

        // watermark: this save covers every commit logged below it; the next open's replay skips
        // records below it
        pm.header_mut().recovery_lsn = position;

        match pm.flush() {
            Ok(()) => {
                files.page_manager = Some(pm);
                files.free_space = Some(space);
            }
            // the page WAL committed, so the save is durable; dropping `pm` makes the next save
            // rewrite the file, settling that WAL first
            Err(e) if pm.wal_unapplied() => tracing::warn!(
                "applying a save to {} failed after its page WAL committed (recovery replays it): \
                 {e}",
                self.path.display()
            ),
            Err(e) => return Err(e),
        }

        // reclaim rlog segments the save fully covers
        self.truncate_rlog(position);
        Ok(())
    }

    /// A record's place in the file going away (deleted, or replaced by a new copy): its overflow
    /// pages and its slot are freed, and its page when nothing else is on it.
    fn remove_record(
        pm: &mut PageManager,
        space: &mut FreeSpaceMap,
        loc: RecordLocation,
    ) -> Result<()> {
        overflow::free_slot(pm, loc.page_id, loc.slot_idx)?;
        let buf = pm.get_page_mut(loc.page_id)?;
        let mut page = SlottedPage::new(buf.as_mut(), loc.page_id);
        page.tombstone_slot(loc.slot_idx)?;
        if page.live_slot_count() == 0 {
            pm.free_page(loc.page_id);
            space.forget(loc.page_id);
        } else {
            space.refresh(pm, loc.page_id)?;
        }
        Ok(())
    }

    /// Rewrites the whole file from `records` (a checkpoint, or the live graph under the writer
    /// lock), stamped with `position`: a temp file renamed over the original. An error means
    /// nothing new is durable: the failed step was before the rename.
    fn rewrite(
        &self,
        files: &mut Files,
        records: &dyn RecordSource,
        catalog: &CatalogParts,
        position: u64,
    ) -> Result<()> {
        // drop the old page manager first to release the file handle
        files.page_manager = None;
        files.free_space = None;
        // a page WAL a failed save left committed belongs to the current file: settle it into
        // that file before replacing it, or a crash after the rename would replay it onto the new
        // one
        if wal::wal_path(&self.path).exists() {
            PageManager::recover(&self.path)?;
        }

        let mut locations = LocationIndex::new();
        let (_, pm) = Self::write_file_keep(&self.path, position, |pm| {
            codec::write_records(pm, records, catalog, &self.metadata, Some(&mut locations))
        })?;
        // durable from here on: reclaim rlog segments the new file covers
        self.truncate_rlog(position);

        // the manager that wrote the file keeps serving it. Without one, the file is still
        // saved: the next save just rewrites it again.
        if let Some(pm) = pm {
            files.page_manager = Some(pm);
            files.locations = locations;
        }
        Ok(())
    }

    /// Skip flushing on close/drop. Crash-recovery tests use it to simulate dying before a
    /// snapshot.
    pub fn set_suppress_flush(&mut self, suppress: bool) {
        self.suppress_flush = suppress;
    }

    /// Flushes and closes the database, consuming it.
    pub fn close(self) -> Result<()> {
        if self.suppress_flush {
            return Ok(());
        }
        // SAFETY: `self` is consumed: no transaction or reference is alive
        let has_changes = unsafe { self.mvcc.graph() }.has_changes();
        if has_changes {
            self.flush()?;
        }
        Ok(())
    }

    /// Returns the file path of the database.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the raw metadata bytes loaded from the tvdb file.
    pub fn metadata(&self) -> &[u8] {
        &self.metadata
    }

    /// Transactions the redo log replayed on top of the file when it opened: commits the file
    /// doesn't have yet, so a holder that saves only changed databases saves this one.
    pub fn replayed_transactions(&self) -> usize {
        self.replayed
    }

    /// Consumes the database: returns the in-memory graph, path and metadata, and
    /// releases the file lock. Lets a server move the graph into its own `MvccStore`
    /// and persist later with [`Database::save`].
    pub fn into_parts(mut self) -> (Graph, PathBuf, Vec<u8>, Option<Arc<Mutex<LogWriter>>>, u64) {
        // keep the rlog writer alive past this drop: the server installs it on its own
        // MvccStore so commits stay durable
        let writer = self.mvcc.log_writer();

        // take the graph out of the store (swapping in an empty one) instead of cloning it
        let graph = {
            let _guard = self.mvcc.write_lock.lock();
            // SAFETY: we hold the writer lock and own the database (no reader can exist)
            let g = unsafe { self.mvcc.graph_mut() };
            std::mem::take(g)
        };
        let path = self.path.clone();
        let metadata = std::mem::take(&mut self.metadata);

        // drop the page manager (file handles, page buffers) and the location index before drop
        let files = self.files.get_mut().unwrap_or_else(PoisonError::into_inner);
        files.page_manager = None;
        files.locations = LocationIndex::new();

        let recovery_floor = self.mvcc.recovery_floor();
        // normal drop: the empty graph has no changes so flush is a no-op, and Drop's unlock is
        // idempotent
        drop(self);

        (graph, path, metadata, writer, recovery_floor)
    }

    /// Saves a graph to a `.tvdb` file (create or overwrite): a full compact-style
    /// write. Used to persist an `MvccStore` snapshot on server shutdown. Non-empty
    /// `metadata` gets a metadata page referenced from the header.
    pub fn save(path: &Path, graph: &Graph, metadata: &[u8]) -> Result<()> {
        Self::save_with_recovery_lsn(path, graph, metadata, 0)
    }

    /// [`Database::save`] that stamps the snapshot with an rlog `recovery_lsn`.
    /// Durable callers (server shutdown/checkpoint) pass `LogWriter::next_lsn()` so
    /// the next open skips records the snapshot already has. 0 (via `save`) means
    /// replay everything: only right when there's no rlog.
    pub fn save_with_recovery_lsn(
        path: &Path,
        graph: &Graph,
        metadata: &[u8],
        recovery_lsn: u64,
    ) -> Result<()> {
        Self::save_file(path, recovery_lsn, |pm| {
            codec::write_records(pm, graph, &graph.catalog_parts(), metadata, None)
        })
        .map(|_| ())
    }

    /// Saves a store to `path` (create or overwrite) as of a checkpoint's commit, stamped with its
    /// log position. Reads and commits carry on while it's written. Returns the
    /// `(nodes, relationships)` saved.
    pub fn save_checkpoint(
        path: &Path,
        checkpoint: &Checkpoint<'_>,
        metadata: &[u8],
    ) -> Result<(usize, usize)> {
        let written = Self::save_file(path, checkpoint.position(), |pm| {
            codec::write_records(pm, checkpoint, checkpoint.catalog(), metadata, None)
        })?;
        Ok((written.nodes, written.edges))
    }

    /// [`write_file`](Self::write_file) holding `path`'s lock, for a file no open database holds;
    /// drops the file's page WAL after.
    fn save_file(
        path: &Path,
        recovery_lsn: u64,
        write: impl FnOnce(&mut PageManager) -> Result<codec::Written>,
    ) -> Result<codec::Written> {
        // exclusive lock
        let lock_path = path.with_extension("tvdb.lock");
        let _lock_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(&lock_path)?;
        #[cfg(feature = "file-locking")]
        _lock_file
            .try_lock_exclusive()
            .map_err(|_| Error::DatabaseLocked(path.to_path_buf()))?;

        let written = Self::write_file(path, recovery_lsn, write)?;

        // drop WAL and lock
        let wal_file = wal::wal_path(path);
        if wal_file.exists() {
            let _ = std::fs::remove_file(&wal_file);
        }
        #[cfg(feature = "file-locking")]
        let _ = FileExt::unlock(&_lock_file);
        let _ = std::fs::remove_file(&lock_path);

        Ok(written)
    }

    /// Writes a whole `.tvdb` with `write` into a temp file stamped with `recovery_lsn`, then
    /// renames it over `path` durably. The caller holds `path`'s lock. A failed write leaves `path`
    /// as it was and removes the temp file.
    /// [`write_file`](Self::write_file) that also hands back the page manager over the
    /// finished file, pages still in memory, so nothing has to be read back and verified;
    /// `None` when the file could not be reopened (it is saved either way).
    fn write_file_keep(
        path: &Path,
        recovery_lsn: u64,
        write: impl FnOnce(&mut PageManager) -> Result<codec::Written>,
    ) -> Result<(codec::Written, Option<PageManager>)> {
        let tmp_path = path.with_extension("tvdb.tmp");
        let result = (|| {
            let mut pm = PageManager::create(&tmp_path)?;
            let written = write(&mut pm)?;
            pm.header_mut().recovery_lsn = recovery_lsn;
            pm.flush_new_file()?;
            // the handle must close before the rename
            pm.detach_file();
            Ok((written, pm))
        })();
        let (written, mut pm) = match result {
            Ok(x) => x,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(e);
            }
        };
        super::rename_durable(&tmp_path, path)?;
        match pm.attach_file(path) {
            Ok(()) => Ok((written, Some(pm))),
            Err(e) => {
                tracing::warn!(
                    "reopening {} after a full save failed, the next save rewrites it: {e}",
                    path.display()
                );
                Ok((written, None))
            }
        }
    }

    fn write_file(
        path: &Path,
        recovery_lsn: u64,
        write: impl FnOnce(&mut PageManager) -> Result<codec::Written>,
    ) -> Result<codec::Written> {
        let tmp_path = path.with_extension("tvdb.tmp");
        let written = (|| {
            let mut pm = PageManager::create(&tmp_path)?;
            let written = write(&mut pm)?;
            pm.header_mut().recovery_lsn = recovery_lsn;
            pm.flush_new_file()?;
            Ok(written)
            // pm dropped here: closes the temp file
        })();
        let written = match written {
            Ok(w) => w,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                return Err(e);
            }
        };

        // atomic, durable rename temp -> original
        super::rename_durable(&tmp_path, path)?;
        Ok(written)
    }

    /// Replaces the metadata of an existing `.tvdb` (the server's user catalog) without loading the
    /// graph. Copy on write: the new metadata page (and its overflow pages, when it's larger than a
    /// page) goes after the last page and the header switches to it, so no page of the file a
    /// reader may be copying changes; the old metadata pages stay, unreferenced, until the next
    /// full save rewrites the file. Crash-safe like a flush: the pages and the header go to the
    /// page WAL first, so a crash leaves the old metadata or the new, and the next open applies a
    /// committed log. Holds the file's lock while it writes, like a save.
    pub fn save_metadata(path: &Path, metadata: &[u8]) -> Result<()> {
        use crate::storage::format::{HEADER_SIZE, PAGE_SIZE};
        use std::io::{Read as _, Seek, SeekFrom, Write as _};

        let lock_path = path.with_extension("tvdb.lock");
        let _lock_file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(&lock_path)?;
        #[cfg(feature = "file-locking")]
        _lock_file
            .try_lock_exclusive()
            .map_err(|_| Error::DatabaseLocked(path.to_path_buf()))?;

        // a log left by an interrupted write goes in first
        PageManager::recover(path)?;

        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        let mut hdr_buf = [0u8; HEADER_SIZE];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut hdr_buf)?;
        let mut header = crate::storage::format::FileHeader::from_bytes(&hdr_buf)?;
        let held = file.metadata()?.len().saturating_sub(HEADER_SIZE as u64) / PAGE_SIZE as u64;
        if header.page_count > held {
            return Err(Error::InvalidFormat(format!(
                "header claims {} pages, the file holds {held}",
                header.page_count
            )));
        }
        // the metadata page goes after the last page, its overflow pages after it
        let meta_page_id = header.page_count;
        let record = compression::encode_record(metadata);
        let mut pages = Vec::new();
        let slot = if record.len() <= overflow::MAX_INLINE {
            record
        } else {
            let (stub, chain) = overflow::spill_appended(&record, meta_page_id + 1)?;
            pages = chain;
            header.note_overflow();
            stub
        };
        let mut meta_page = Box::new([0u8; PAGE_SIZE]);
        {
            let mut page = SlottedPage::new(meta_page.as_mut(), meta_page_id);
            page.init(PageType::Metadata);
            page.insert_record(&slot)?;
        }
        pages.insert(0, (meta_page_id, meta_page));
        header.page_count = meta_page_id + pages.len() as u64;
        header.metadata_page_id = meta_page_id;
        header.lsn += 1;
        for (id, buf) in &mut pages {
            let mut page = SlottedPage::new(buf.as_mut(), *id);
            page.set_lsn(header.lsn);
            page.write_checksum();
        }
        let hdr_bytes = header.to_bytes();

        let wal_path = wal::wal_path(path);
        let mut log = wal::WalWriter::create(&wal_path, header.lsn, header.lsn)?;
        for (id, buf) in &pages {
            log.write_page(*id, buf)?;
        }
        log.write_header(&hdr_bytes)?;
        log.commit()?;

        // the pages, then the header that points at them
        for (id, buf) in &pages {
            file.seek(SeekFrom::Start(HEADER_SIZE as u64 + id * PAGE_SIZE as u64))?;
            file.write_all(buf.as_ref())?;
        }
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&hdr_bytes)?;
        file.sync_all()?;
        let _ = std::fs::remove_file(&wal_path);

        #[cfg(feature = "file-locking")]
        let _ = FileExt::unlock(&_lock_file);
        let _ = std::fs::remove_file(&lock_path);
        Ok(())
    }

    // -- Private helpers ----------------------------------------------

    /// Loads the file at `path`: graph, location index, the page manager (kept for incremental
    /// flushes) and the metadata bytes.
    fn load_graph_with_locations(
        path: &Path,
    ) -> Result<(Graph, LocationIndex, PageManager, Vec<u8>)> {
        super::codec::load_graph(PageManager::open(path)?)
    }

    /// Rewrites the catalog page in the PageManager.
    fn write_catalog(pm: &mut PageManager, catalog: &CatalogParts) -> Result<()> {
        let old_catalog_page_id = pm.header().catalog_page_id;
        overflow::free_page_records(pm, old_catalog_page_id)?;
        pm.free_page(old_catalog_page_id);
        codec::write_catalog_page(pm, catalog)?;
        Ok(())
    }
}

/// Bulk-load transaction (see [`Database::begin_bulk_load`]): a write transaction that writes no
/// redo records. Its commit saves the database with the load in it, under the writer lock, and
/// publishes the load only once that save is durable, so the data reaches disk once instead of
/// twice (log, then snapshot) and nothing is buffered for the log. Readers see none of it until
/// commit; [`abort`](Self::abort), dropping it, or a failed commit roll it back.
pub struct BulkLoadTx {
    // field order: the transaction rolls back (on drop) before the database handle goes
    tx: OwnedWriteTx,
    db: Arc<Database>,
}

impl BulkLoadTx {
    /// Creates one node per property map, every one with `labels`; consecutive ids from the
    /// returned one.
    pub fn bulk_nodes(
        &mut self,
        labels: &[&str],
        props: Vec<crate::graph::property::PropertyMap>,
    ) -> Result<NodeId> {
        self.tx.bulk_nodes(labels, props)
    }

    /// Creates a property-less edge of `edge_type` per `(source, target)` pair; consecutive ids
    /// from the returned one. Every endpoint must exist.
    pub fn bulk_edges(
        &mut self,
        edge_type: &str,
        pairs: &[(NodeId, NodeId)],
    ) -> Result<crate::graph::types::EdgeId> {
        self.tx.bulk_edges(edge_type, pairs)
    }

    /// Saves the database with the load in it, then publishes the load. On error nothing is
    /// published and the load is rolled back.
    pub fn commit(self) -> Result<HybridTimestamp> {
        let Self { tx, db } = self;
        // `tx` rolls back as it drops on the error path
        db.flush_locked(tx.writer_guard())?;
        Ok(tx.commit_snapshotted())
    }

    /// Rolls the load back.
    pub fn abort(self) {
        self.tx.abort();
    }
}

impl GraphView for BulkLoadTx {
    fn as_sync_ref(&self) -> Option<&(dyn GraphView + Sync)> {
        self.tx.as_sync_ref()
    }

    crate::forward_graph_view_statements!(self => &self.tx, &mut self.tx);
    crate::forward_graph_view_reads!(self => &self.tx);
    crate::forward_graph_view_writes!(self => &mut self.tx);
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let graph = self.graph();
        f.debug_struct("Database")
            .field("path", &self.path)
            .field("nodes", &graph.node_count())
            .field("edges", &graph.edge_count())
            .field("has_changes", &graph.has_changes())
            .finish()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        if !self.suppress_flush {
            // SAFETY: read-only check during drop.
            let has_changes = unsafe { self.mvcc.graph() }.has_changes();
            if has_changes && let Err(e) = self.flush() {
                tracing::warn!("failed to flush database on drop: {e}");
            }
            self.retire_log();
        }
        let lock_path = self.path.with_extension("tvdb.lock");
        #[cfg(feature = "file-locking")]
        let _ = FileExt::unlock(&self.lock_file);
        let _ = std::fs::remove_file(lock_path);
    }
}

#[cfg(test)]
mod tests;
