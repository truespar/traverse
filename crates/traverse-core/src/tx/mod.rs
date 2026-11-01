//! MVCC transactions: snapshot isolation, single writer, many readers.
//!
//! - [`MvccStore`]: `Graph` with multi-version concurrency control
//! - [`ReadTx`]: read-only snapshot transaction
//! - [`WriteTx`]: single-writer transaction, commits atomically
//! - [`GraphView`]: trait over `Graph`, `ReadTx` and `WriteTx`
//! - [`Checkpoint`]: the store as of one commit, written to a file while it keeps running

mod checkpoint;
mod forward;
mod gc;
pub mod mvcc;
mod optimistic;
#[cfg(test)]
mod optimistic_tests;
pub(crate) mod owned;
mod read;
#[cfg(test)]
mod snapshot_tests;
pub mod timestamp;
pub(crate) mod versions;
pub mod view;
mod write;

use std::cell::UnsafeCell;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::graph::Graph;
use crate::sync::{SlottedRwLock, WriterLock};

#[cfg(not(target_arch = "wasm32"))]
use crate::storage::rlog::LogWriter;

use self::timestamp::{HlcClock, HybridTimestamp};

// -- Lock-free reader registry ---------------------------------------

/// Reader slots per registry chunk.
const READER_CHUNK: usize = 64;
/// Chunks, allocated on demand.
const READER_CHUNKS: usize = 64;
/// Maximum concurrent snapshots (read transactions plus open explicit transactions).
pub const MAX_READERS: usize = READER_CHUNK * READER_CHUNKS;

/// Empty slot.
const SLOT_EMPTY: u64 = 0;

/// `HybridTimestamp` -> u64 for atomic storage: `(physical << 16) | logical`.
/// 48 bits of ms since epoch cover ~8900 years; 16 bits of logical counter is plenty.
pub(crate) fn encode_ts(ts: HybridTimestamp) -> u64 {
    debug_assert!(ts.physical < (1u64 << 48), "physical exceeds 48 bits");
    debug_assert!(ts.logical < (1u32 << 16), "logical exceeds 16 bits");
    (ts.physical << 16) | (ts.logical as u64)
}

/// u64 -> `HybridTimestamp`.
fn decode_ts(val: u64) -> HybridTimestamp {
    HybridTimestamp {
        physical: val >> 16,
        logical: (val & 0xFFFF) as u32,
    }
}

/// Lock-free registry of active snapshot timestamps (the GC watermark): `AtomicU64` slots
/// claimed by CAS, in chunks of 64 allocated on first need, so registration is wait-free under
/// low contention and capacity grows with the number of open transactions.
pub(crate) struct ReaderRegistry {
    chunks: [std::sync::OnceLock<Box<[AtomicU64; READER_CHUNK]>>; READER_CHUNKS],
}

impl ReaderRegistry {
    pub fn new() -> Self {
        Self {
            chunks: std::array::from_fn(|_| std::sync::OnceLock::new()),
        }
    }

    /// Allocated chunks, in order (a chunk only exists once every earlier one filled up).
    fn allocated(&self) -> impl Iterator<Item = &[AtomicU64; READER_CHUNK]> {
        self.chunks.iter().map_while(|c| c.get().map(|b| &**b))
    }

    /// Registers an encoded snapshot timestamp; returns its slot, `None` at [`MAX_READERS`].
    ///
    /// SeqCst like every registry access: GC reads the commit timestamp and then scans the
    /// slots, a reader stores its slot and then re-reads the commit timestamp, so one of the two
    /// always sees the other (see [`MvccStore::begin_read`]).
    pub fn register(&self, encoded: u64) -> Option<usize> {
        debug_assert!(encoded != SLOT_EMPTY, "encoded timestamp must be non-zero");
        for (ci, chunk) in self.chunks.iter().enumerate() {
            // free slots in existing chunks first; a new chunk only when they're all taken
            let chunk =
                chunk.get_or_init(|| Box::new(std::array::from_fn(|_| AtomicU64::new(SLOT_EMPTY))));
            for (i, slot) in chunk.iter().enumerate() {
                if slot
                    .compare_exchange(SLOT_EMPTY, encoded, Ordering::SeqCst, Ordering::Relaxed)
                    .is_ok()
                {
                    return Some(ci * READER_CHUNK + i);
                }
            }
        }
        None
    }

    fn slot(&self, slot_id: usize) -> &AtomicU64 {
        &self.chunks[slot_id / READER_CHUNK]
            .get()
            .expect("slot from register()")[slot_id % READER_CHUNK]
    }

    /// Moves a registered reader to a later timestamp.
    pub fn update(&self, slot_id: usize, encoded: u64) {
        self.slot(slot_id).store(encoded, Ordering::SeqCst);
    }

    /// Unregisters a reader by clearing its slot.
    pub fn unregister(&self, slot_id: usize) {
        self.slot(slot_id).store(SLOT_EMPTY, Ordering::SeqCst);
    }

    /// Oldest registered snapshot (encoded), or `None` without readers.
    pub fn min_encoded(&self) -> Option<u64> {
        self.allocated()
            .flatten()
            .map(|slot| slot.load(Ordering::SeqCst))
            .filter(|&v| v != SLOT_EMPTY)
            .min()
    }
}

pub use self::checkpoint::Checkpoint;
pub use self::optimistic::OptimisticTx;
pub use self::owned::{OwnedOptimisticTx, OwnedReadTx, OwnedWriteTx};
pub use self::read::{ReadTx, ReadTx as ReadTransaction, UnregisteredReadView};
pub use self::timestamp::HybridTimestamp as Timestamp;
pub use self::view::GraphView;
pub use self::write::{WriteTx, WriteTx as WriteTransaction};

/// Multi-version concurrency control store around a `Graph`: readers take
/// consistent snapshots while one writer mutates.
///
/// # Concurrency
///
/// - **One writer, many readers:** the writer lock allows one `WriteTx` at a time; `ReadTx`s
///   coexist, up to [`MAX_READERS`] open snapshots.
/// - **Snapshot isolation:** a `ReadTx` sees every transaction committed before it
///   started and nothing else (no uncommitted state, no later commit).
/// - **Version state:** the graph holds the latest state in place; per-object chains of
///   pre-transaction images, runs of created ids and ghost memberships give older
///   snapshots their view (see `mvcc`, `versions`).
///
/// # Safety
///
/// `UnsafeCell<Graph>` gives readers shared access. Sound because:
/// - writers are serialized by `write_lock`
/// - structural changes take `graph_rw` for writing, every read takes it for reading
/// - an object a writer changes in place is imaged first; readers resolve it through
///   the image and never read the live copy
/// - GC takes `write_lock` and `graph_rw` before cutting chains or freeing deltas
pub struct MvccStore {
    /// The shared graph, accessed unsafely via `graph()` / `graph_mut()`.
    pub(crate) graph: UnsafeCell<Graph>,
    pub(crate) clock: HlcClock,
    pub(crate) reader_registry: ReaderRegistry,
    pub(crate) write_lock: WriterLock,
    /// Cache-padded per-slot RW lock for map structure changes: ReadTx takes a slot read lock,
    /// WriteTx the write lock around insert/remove/resize.
    pub(crate) graph_rw: SlottedRwLock,
    /// Encoded timestamp of the latest commit; published after its deltas, so a snapshot at it
    /// always finds them committed.
    pub(crate) last_commit_ts: AtomicU64,
    /// Watermark of the last collection (versions at or before it are gone).
    collected_up_to: AtomicU64,
    /// The graph's id counters: optimistic transactions reserve ids here without the writer lock.
    pub(crate) ids: std::sync::Arc<crate::graph::IdCounters>,
    memory_budget: Option<usize>,
    /// Optional redo-log writer for per-commit durability: every `WriteTx::commit()`
    /// appends its logical ops to the WAL before returning. `None` (default; wasm and
    /// in-process tests): commits live in memory only.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) log_writer: Option<Arc<Mutex<LogWriter>>>,
    /// The writer's durability coordinator, for lock-free reads of the durable watermark.
    #[cfg(not(target_arch = "wasm32"))]
    durability: Option<Arc<crate::storage::rlog::SyncControl>>,
    /// With a log writer: `true` (default) acks each commit after fdatasync.
    /// `false` is async WAL (Postgres `synchronous_commit=off` / Redis
    /// `appendfsync everysec`): append per commit, ack at once, a background task
    /// fsyncs on an interval that bounds the crash-loss window. Commits still fail
    /// fast once a background fsync failure poisons the sync control.
    pub(crate) sync_on_commit: std::sync::atomic::AtomicBool,
    /// LSN up to which rlog segments were REPLAYED at open WITHOUT a writer (snapshot
    /// mode recovering an earlier WAL run's tail). Stamped into the next snapshot so
    /// the following boot doesn't re-apply them. 0 if nothing was replayed that way.
    pub(crate) recovery_floor: std::sync::atomic::AtomicU64,
    /// Set by [`CommitPause::close`]: commits fail with
    /// [`DatabaseClosed`](crate::error::Error::DatabaseClosed).
    closed: std::sync::atomic::AtomicBool,
    /// One [`Checkpoint`] or [`CommitPause`] at a time, so saved files are installed in commit
    /// order. Taken before the writer lock.
    checkpoint_lock: std::sync::Mutex<()>,
}

// SAFETY: shareable across threads:
// 1. `graph` (UnsafeCell) is only mutated by WriteTx under the mutex
// 2. ReadTx reads through a shared reference
// 3. delta chain heads are AtomicPtr (lock-free reads)
// 4. every other field is already Send+Sync
unsafe impl Send for MvccStore {}
// SAFETY: see `Send` above; shared access never hands out `&mut Graph` without the mutex
unsafe impl Sync for MvccStore {}

impl MvccStore {
    /// Shared graph reference.
    ///
    /// # Safety
    ///
    /// Fine from ReadTx (concurrent readers): WriteTx holds the mutex so there's one
    /// writer, entity reads are independent, and delta chains rebuild snapshots.
    #[inline]
    pub(crate) unsafe fn graph(&self) -> &Graph {
        // SAFETY: forwarded to the caller, see # Safety
        unsafe { &*self.graph.get() }
    }

    /// Mutable graph reference.
    ///
    /// # Safety
    ///
    /// Caller holds the `write_lock` mutex or otherwise has exclusive access.
    #[inline]
    #[allow(clippy::mut_from_ref)] // exclusivity is the caller's contract, see # Safety
    pub(crate) unsafe fn graph_mut(&self) -> &mut Graph {
        // SAFETY: forwarded to the caller, see # Safety
        unsafe { &mut *self.graph.get() }
    }

    /// New MVCC store around `graph`.
    pub fn new(graph: Graph) -> Self {
        let clock = HlcClock::new();
        // the loaded graph is the first committed state: snapshots start here, never at 0 (the
        // registry's empty marker)
        let boot = encode_ts(clock.now());
        let ids = graph.id_counters();
        Self {
            ids,
            graph: UnsafeCell::new(graph),
            clock,
            reader_registry: ReaderRegistry::new(),
            write_lock: WriterLock::new(),
            graph_rw: SlottedRwLock::new(),
            last_commit_ts: AtomicU64::new(boot),
            collected_up_to: AtomicU64::new(0),
            memory_budget: None,
            #[cfg(not(target_arch = "wasm32"))]
            log_writer: None,
            #[cfg(not(target_arch = "wasm32"))]
            durability: None,
            sync_on_commit: std::sync::atomic::AtomicBool::new(true),
            recovery_floor: std::sync::atomic::AtomicU64::new(0),
            closed: std::sync::atomic::AtomicBool::new(false),
            checkpoint_lock: std::sync::Mutex::new(()),
        }
    }

    /// Attaches a redo-log writer for per-commit durability. Shared
    /// (`Arc<Mutex<...>>`) so snapshot/flush can truncate segments after persisting.
    ///
    /// The caller must replay pending log transactions BEFORE attaching: a writer
    /// opened past pending records silently overwrites them.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_log_writer(mut self, writer: Arc<Mutex<LogWriter>>) -> Self {
        let sync = writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sync_handle();
        // what the store holds now is the durable baseline
        sync.note_durable_ts(self.last_commit_ts.load(Ordering::SeqCst));
        self.durability = Some(sync);
        self.log_writer = Some(writer);
        self
    }

    /// Newest commit snapshots may see (encoded): the newest published one, and in synchronous
    /// WAL mode no newer than the newest durable one, so no reader observes a commit a crash could
    /// still lose. (A commit's log records can be synced before its deltas are published, hence
    /// the min: a snapshot never includes an unpublished commit.)
    pub(crate) fn visible_commit(&self) -> u64 {
        let published = self.last_commit_ts.load(Ordering::SeqCst);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(sync) = &self.durability
            && self.sync_on_commit()
        {
            return sync.durable_ts().min(published);
        }
        published
    }

    /// Newest commit a new snapshot sees: bookmarks up to it are satisfied.
    pub fn visible_ts(&self) -> HybridTimestamp {
        decode_ts(self.visible_commit())
    }

    /// See [`recovery_floor`](Self::recovery_floor).
    pub fn set_recovery_floor(&self, lsn: u64) {
        self.recovery_floor
            .store(lsn, std::sync::atomic::Ordering::Release);
    }

    /// LSN already replayed from on-disk segments without a writer (snapshot mode); 0 otherwise.
    pub fn recovery_floor(&self) -> u64 {
        self.recovery_floor
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// Marks commits up to `encoded` durable by other means than the redo log (a snapshot that
    /// holds them), so synchronous-WAL snapshots may see them.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn note_durable(&self, encoded: u64) {
        if let Some(sync) = &self.durability {
            sync.note_durable_ts(encoded);
        }
    }

    /// Closed to writes? See [`CommitPause::close`].
    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The attached log writer, if any.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn log_writer(&self) -> Option<Arc<Mutex<LogWriter>>> {
        self.log_writer.as_ref().map(Arc::clone)
    }

    /// Whether commits block on fdatasync (see the field docs). Only matters with a log writer.
    pub fn set_sync_on_commit(&self, sync: bool) {
        self.sync_on_commit
            .store(sync, std::sync::atomic::Ordering::Release);
    }

    /// Whether commits wait for the redo log to reach disk (see `set_sync_on_commit`).
    pub fn sync_on_commit(&self) -> bool {
        self.sync_on_commit
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// New MVCC store with an optional memory budget.
    pub fn with_budget(graph: Graph, budget: Option<usize>) -> Self {
        let mut store = Self::new(graph);
        store.memory_budget = budget;
        store
    }

    /// Memory budget, if set.
    pub fn memory_budget(&self) -> Option<usize> {
        self.memory_budget
    }

    /// Current memory estimate.
    pub fn estimated_memory(&self) -> usize {
        let _guard = self.graph_rw.read();
        // SAFETY: under the structural read lock
        unsafe { self.graph().estimated_memory() }
    }

    /// Schema metadata: (labels, edge types, property keys with sampled types).
    pub fn schema_summary(&self) -> (Vec<String>, Vec<String>, Vec<(String, &'static str)>) {
        let _guard = self.graph_rw.read();
        // SAFETY: under the structural read lock
        let graph = unsafe { self.graph() };
        (
            graph.schema_labels(),
            graph.schema_edge_types(),
            graph.schema_property_types(),
        )
    }

    /// Per-label and per-edge-type property maps, by sampling.
    pub fn schema_properties_by_owner(
        &self,
    ) -> (
        Vec<(String, Vec<(String, &'static str)>)>,
        Vec<(String, Vec<(String, &'static str)>)>,
    ) {
        let _guard = self.graph_rw.read();
        // SAFETY: under the structural read lock
        let graph = unsafe { self.graph() };
        graph.schema_properties_by_owner()
    }

    /// Read-only snapshot transaction at the newest visible commit ([`Self::visible_ts`]).
    /// Lock-free: claims a registry slot.
    pub fn begin_read(&self) -> crate::error::Result<ReadTx<'_>> {
        let (snapshot, slot) = self.register_snapshot()?;
        Ok(ReadTx::new(self, decode_ts(snapshot), snapshot, slot))
    }

    /// Registers a snapshot at the newest visible commit ([`Self::visible_ts`]); returns
    /// `(encoded ts, slot)`.
    ///
    /// Store-then-recheck: once the slot holds `ts` and the visible timestamp still reads `ts`,
    /// any GC that missed the slot read a visible timestamp `<= ts`, so it can't have freed
    /// anything this snapshot needs. A commit in between moves the slot forward and rechecks.
    fn register_snapshot(&self) -> crate::error::Result<(u64, usize)> {
        let mut ts = self.visible_commit();
        let slot = self
            .reader_registry
            .register(ts)
            .ok_or(crate::error::Error::TooManyReaders(MAX_READERS))?;
        loop {
            let now = self.visible_commit();
            if now == ts {
                return Ok((ts, slot));
            }
            ts = now;
            self.reader_registry.update(slot, ts);
        }
    }

    /// Lightweight unregistered read view for metadata: unlike `begin_read()` it
    /// doesn't register (doesn't hold back GC) or track a snapshot. For index hints,
    /// string table and statistics.
    pub fn unregistered_read(&self) -> UnregisteredReadView<'_> {
        UnregisteredReadView::new(self)
    }

    /// Write transaction; takes the single-writer lock (blocks while another writer
    /// is active) for its whole life, mutating the committed graph in place with an
    /// undo log for rollback.
    pub fn begin_write(&self) -> WriteTx<'_> {
        let guard = self.write_lock.lock();
        let begin_ts = self.clock.now();
        WriteTx::new(self, begin_ts, guard)
    }

    /// [`begin_write`](Self::begin_write) that waits at most `timeout` for the writer lock, then
    /// fails with [`Busy`](crate::error::Error::Busy).
    pub fn try_begin_write(&self, timeout: Duration) -> crate::error::Result<WriteTx<'_>> {
        let guard = self
            .write_lock
            .try_lock_for(timeout)
            .ok_or(crate::error::Error::Busy(timeout))?;
        let begin_ts = self.clock.now();
        Ok(WriteTx::new(self, begin_ts, guard))
    }

    /// Timestamp of the latest commit (doesn't advance the clock).
    pub fn last_commit_ts(&self) -> HybridTimestamp {
        let encoded = self.last_commit_ts.load(Ordering::Acquire);
        if encoded == 0 {
            HybridTimestamp::ZERO
        } else {
            decode_ts(encoded)
        }
    }

    /// Runs `f` on the committed graph, with commits held off and readers waiting until it
    /// returns: reading its schema without a copy. Keep `f` short; a whole-graph pass belongs
    /// on a [`Checkpoint`] or a snapshot.
    pub fn with_committed_graph<R>(&self, f: impl FnOnce(&Graph) -> R) -> R {
        let _guard = self.write_lock.lock();
        let _rw = self.graph_rw.write();
        // SAFETY: the writer lock rules out writers and graph_rw the structural changes; `f` only
        // reads
        f(unsafe { self.graph() })
    }

    /// Holds off commits until the returned pause drops: for moving a database's files, where every
    /// commit must land either in a checkpoint taken during the pause or in the log after it. Waits
    /// for a checkpoint that's being written.
    pub fn pause_commits(&self) -> CommitPause<'_> {
        let serial = self
            .checkpoint_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        CommitPause {
            store: self,
            _guard: self.write_lock.lock(),
            _serial: serial,
        }
    }

    /// Where replay resumes for a file holding every commit so far (writer lock held): the redo
    /// log's next record, or without a log writer the floor of segments replayed at open (see
    /// [`Self::recovery_floor`]).
    fn log_position(&self) -> u64 {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(w) = &self.log_writer {
            return w.lock().unwrap_or_else(|e| e.into_inner()).next_lsn();
        }
        self.recovery_floor()
    }
}

/// Commits paused; see [`MvccStore::pause_commits`].
pub struct CommitPause<'a> {
    store: &'a MvccStore,
    _guard: crate::sync::WriterGuard<'a>,
    _serial: std::sync::MutexGuard<'a, ()>,
}

impl CommitPause<'_> {
    /// A checkpoint of the paused store: nothing commits until the pause ends, so it holds every
    /// commit so far.
    pub fn checkpoint(&self) -> crate::error::Result<Checkpoint<'_>> {
        self.store.checkpoint_locked(None)
    }

    /// Ends the pause with the store closed to writes: every later commit fails with
    /// [`DatabaseClosed`](crate::error::Error::DatabaseClosed). For a database that's going away
    /// (dropped, unloaded) while sessions may still hold it; reads keep working.
    pub fn close(self) {
        self.store
            .closed
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

impl std::fmt::Debug for MvccStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let _guard = self.graph_rw.read();
        // SAFETY: under the structural read lock
        let graph = unsafe { self.graph() };
        f.debug_struct("MvccStore")
            .field("nodes", &graph.node_count())
            .field("edges", &graph.edge_count())
            .field("node_deltas", &graph.node_delta_heads.len())
            .field("edge_deltas", &graph.edge_delta_heads.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::property::PropertyValue;
    use std::collections::BTreeMap;

    fn empty_props() -> BTreeMap<String, PropertyValue> {
        BTreeMap::new()
    }

    #[test]
    fn reader_registry_grows_past_one_chunk_and_reuses_slots() {
        let store = MvccStore::new(Graph::new());
        let readers: Vec<_> = (0..200).map(|_| store.begin_read().unwrap()).collect();
        let oldest = encode_ts(readers[0].snapshot_ts());
        assert_eq!(store.reader_registry.min_encoded(), Some(oldest));
        drop(readers);
        assert_eq!(store.reader_registry.min_encoded(), None);
        // freed slots come back first: no further chunk gets allocated
        let again: Vec<_> = (0..200).map(|_| store.begin_read().unwrap()).collect();
        assert_eq!(store.reader_registry.allocated().count(), 4);
        drop(again);
    }

    #[test]
    fn reader_registry_has_a_ceiling() {
        let registry = ReaderRegistry::new();
        let ts = 1;
        for _ in 0..MAX_READERS {
            assert!(registry.register(ts).is_some());
        }
        assert_eq!(registry.register(ts), None);
    }

    fn props(pairs: &[(&str, PropertyValue)]) -> BTreeMap<String, PropertyValue> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn basic_write_commit() {
        let g = Graph::new();
        let store = MvccStore::new(g);

        let mut tx = store.begin_write();
        let nid = GraphView::create_node(
            &mut tx,
            &["Person"],
            props(&[("name", PropertyValue::String("Alice".into()))]),
        )
        .unwrap();
        tx.commit().unwrap();

        // SAFETY: no concurrent access in tests
        let graph = unsafe { store.graph() };
        assert_eq!(graph.node_count(), 1);
        assert_eq!(
            graph
                .get_node_property(nid, "name")
                .unwrap()
                .unwrap()
                .as_str(),
            Some("Alice")
        );
    }

    #[test]
    fn abort_discards_changes() {
        let g = Graph::new();
        let store = MvccStore::new(g);

        let mut tx = store.begin_write();
        GraphView::create_node(&mut tx, &["Person"], empty_props()).unwrap();
        tx.abort();

        // SAFETY: no transaction is open
        let graph = unsafe { store.graph() };
        assert_eq!(graph.node_count(), 0);
    }

    #[test]
    fn snapshot_isolation() {
        let mut g = Graph::new();
        g.create_node(
            &["Person"],
            props(&[("name", PropertyValue::String("Alice".into()))]),
        );
        let store = MvccStore::new(g);

        // Start a read transaction (snapshot before write)
        let read_tx = store.begin_read().unwrap();

        // Write transaction creates a new node
        let mut write_tx = store.begin_write();
        GraphView::create_node(
            &mut write_tx,
            &["Person"],
            props(&[("name", PropertyValue::String("Bob".into()))]),
        )
        .unwrap();
        write_tx.commit().unwrap();

        // Read transaction should still see only Alice
        assert_eq!(GraphView::node_count(&read_tx), 1);

        drop(read_tx);

        // New read should see both
        let read_tx2 = store.begin_read().unwrap();
        assert_eq!(GraphView::node_count(&read_tx2), 2);
    }

    #[test]
    fn read_your_own_writes() {
        let g = Graph::new();
        let store = MvccStore::new(g);

        let mut tx = store.begin_write();
        let nid = GraphView::create_node(
            &mut tx,
            &["Person"],
            props(&[("name", PropertyValue::String("Alice".into()))]),
        )
        .unwrap();
        // visible immediately within the transaction
        assert_eq!(
            GraphView::get_node_property(&tx, nid, "name")
                .unwrap()
                .unwrap()
                .as_str(),
            Some("Alice")
        );
        assert_eq!(GraphView::node_count(&tx), 1);
        tx.commit().unwrap();
    }

    #[test]
    fn multiple_concurrent_readers() {
        let mut g = Graph::new();
        g.create_node(&["Person"], empty_props());
        let store = MvccStore::new(g);

        let r1 = store.begin_read().unwrap();
        let r2 = store.begin_read().unwrap();

        assert_eq!(GraphView::node_count(&r1), 1);
        assert_eq!(GraphView::node_count(&r2), 1);

        drop(r1);
        drop(r2);
    }

    #[test]
    fn gc_cleans_old_deltas() {
        let g = Graph::new();
        let store = MvccStore::new(g);

        // Write something
        let mut tx = store.begin_write();
        GraphView::create_node(&mut tx, &["A"], empty_props()).unwrap();
        tx.commit().unwrap();

        // Write again (creates deltas)
        let mut tx = store.begin_write();
        GraphView::create_node(&mut tx, &["B"], empty_props()).unwrap();
        tx.commit().unwrap();

        // no active readers: GC cleans everything
        store.gc();

        // SAFETY: no concurrent access in tests
        let graph = unsafe { store.graph() };
        assert!(graph.node_delta_heads.is_empty());
        assert!(graph.edge_delta_heads.is_empty());
    }

    #[test]
    fn probe_uncommitted_update_hidden() {
        let mut g = Graph::new();
        let a = g.create_node(&["P"], props(&[("v", PropertyValue::Int(1))]));
        let store = MvccStore::new(g);
        let mut w = store.begin_write();
        GraphView::update_node_properties(&mut w, a, props(&[("v", PropertyValue::Int(2))]))
            .unwrap();
        let r = store.begin_read().unwrap();
        assert_eq!(
            GraphView::get_node_property(&r, a, "v").unwrap(),
            Some(PropertyValue::Int(1)),
            "dirty read via update_node_properties"
        );
    }

    #[test]
    fn probe_in_place_set_hidden() {
        let mut g = Graph::new();
        let a = g.create_node(&["P"], props(&[("v", PropertyValue::Int(1))]));
        let store = MvccStore::new(g);
        let r = store.begin_read().unwrap();
        let mut w = store.begin_write();
        let kid = GraphView::intern_property_key(&mut w, "v");
        GraphView::get_node_mut(&mut w, a)
            .unwrap()
            .properties
            .insert(kid, PropertyValue::Int(3));
        w.commit().unwrap();
        assert_eq!(
            GraphView::get_node_property(&r, a, "v").unwrap(),
            Some(PropertyValue::Int(1)),
            "older snapshot sees an in-place SET"
        );
    }

    #[test]
    fn probe_replaced_key_restored() {
        let mut g = Graph::new();
        let a = g.create_node(
            &["P"],
            props(&[("v", PropertyValue::Int(1)), ("w", PropertyValue::Int(5))]),
        );
        let store = MvccStore::new(g);
        let r = store.begin_read().unwrap();
        let mut w = store.begin_write();
        GraphView::update_node_properties(&mut w, a, props(&[("v", PropertyValue::Int(2))]))
            .unwrap();
        w.commit().unwrap();
        assert_eq!(
            GraphView::get_node_property(&r, a, "w").unwrap(),
            Some(PropertyValue::Int(5)),
            "older snapshot lost a key the replacement dropped"
        );
    }

    #[test]
    fn snapshot_sees_deletion_undo() {
        let mut g = Graph::new();
        let nid = g.create_node(
            &["Person"],
            props(&[("name", PropertyValue::String("Alice".into()))]),
        );
        let store = MvccStore::new(g);

        // Start read before delete
        let read_tx = store.begin_read().unwrap();

        // Delete in write tx
        let mut write_tx = store.begin_write();
        GraphView::delete_node(&mut write_tx, nid).unwrap();
        write_tx.commit().unwrap();

        // Read tx should still see Alice
        assert_eq!(
            GraphView::get_node_property(&read_tx, nid, "name")
                .unwrap()
                .unwrap()
                .as_str(),
            Some("Alice")
        );
    }
}
