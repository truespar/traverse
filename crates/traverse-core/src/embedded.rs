//! Database handles for language bindings and the C ABI, where nothing ties a transaction's
//! lifetime to its database: a garbage collector drops handles in any order, a caller may close
//! the database first, and handles move between threads.
//!
//! - [`SharedDatabase`] owns the database through an `Arc`. Every transaction holds the `Arc`
//!   too, so the database outlives each of them whatever order they go in.
//! - Every transaction sits in a [`Lease`]. [`SharedDatabase::close`] revokes the leases still
//!   open (their transactions roll back, later calls fail with
//!   [`DatabaseClosed`](Error::DatabaseClosed)), waits until nothing else holds the database,
//!   then saves it and releases the file.
//! - Writer-lock waits are bounded by a busy timeout: a writer left open on a single-threaded
//!   host, or a second `begin` on the thread holding one, fails with [`Busy`](Error::Busy)
//!   instead of hanging.
//! - The database saves itself in the background ([`AutoCheckpoint`]), so the redo log stays
//!   short and a restart replays little, without the host calling `flush`.

use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::storage::Database;
use crate::storage::database::BulkLoadTx;
use crate::tx::{GraphView, OwnedReadTx, OwnedWriteTx};

/// Default wait for the writer lock (SQLite bindings use similar defaults).
pub const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// When a database saves itself, from a background thread: once its redo log has grown by
/// `log_bytes` since the last save, or a commit has waited `interval` unsaved. A save works from a
/// checkpoint (see [`Database::flush`]), so reads and commits carry on meanwhile; one that finds
/// most of a large file reusable compacts it. On by default ([`AutoCheckpoint::default`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AutoCheckpoint {
    /// Redo-log growth that triggers a save.
    pub log_bytes: u64,
    /// Longest a commit stays unsaved; `None`: no time limit.
    pub interval: Option<Duration>,
}

impl Default for AutoCheckpoint {
    /// 64 MiB of log, or five minutes.
    fn default() -> Self {
        Self {
            log_bytes: 64 << 20,
            interval: Some(Duration::from_secs(300)),
        }
    }
}

/// How often the checkpoint thread looks.
const CHECKPOINT_POLL: Duration = Duration::from_millis(250);

/// How long a background save waits for an open writer before trying again later.
const CHECKPOINT_BUSY: Duration = Duration::from_millis(100);

/// Smallest file a background save compacts.
const COMPACT_MIN_BYTES: u64 = 64 << 20;

/// A database shared with handles whose lifetimes Rust can't see (see the module docs). A clone is
/// another handle to the same database: closing through one closes it for all.
#[derive(Clone)]
pub struct SharedDatabase {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
    /// Signalled when `live` drops to zero.
    idle: Condvar,
    /// Stops the checkpoint thread.
    stop: Arc<Stop>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.stop.set();
    }
}

/// A stop flag the checkpoint thread sleeps on.
#[derive(Default)]
struct Stop {
    stopped: Mutex<bool>,
    wake: Condvar,
}

impl Stop {
    fn set(&self) {
        *self.stopped.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.wake.notify_all();
    }

    /// Sleeps up to `timeout`; `true` once stopped.
    fn sleep(&self, timeout: Duration) -> bool {
        let stopped = self.stopped.lock().unwrap_or_else(PoisonError::into_inner);
        if *stopped {
            return true;
        }
        let (stopped, _) = self
            .wake
            .wait_timeout(stopped, timeout)
            .unwrap_or_else(PoisonError::into_inner);
        *stopped
    }
}

struct State {
    /// `None` once closed.
    db: Option<Arc<Database>>,
    /// Holders of the database besides `db`: leased transactions, begins in progress, saves.
    live: usize,
    /// Leases handed out; dead entries are pruned as the list grows.
    leases: Vec<Weak<dyn Revoke>>,
    /// List length that triggers the next prune.
    prune_at: usize,
    busy_timeout: Duration,
    statement_timeout: Option<Duration>,
    /// The file stem, or `traverse`: what `SHOW TRANSACTIONS` lists as the database.
    name: Arc<str>,
    /// `None`: no background saves.
    auto_checkpoint: Option<AutoCheckpoint>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Counts one holder of the database (see `State::live`) until dropped.
struct Ticket(Arc<Inner>);

impl Drop for Ticket {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.live -= 1;
        if state.live == 0 {
            self.0.idle.notify_all();
        }
    }
}

impl SharedDatabase {
    /// Opens (or creates) the database at `path` with per-commit redo-log durability.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::new(Database::open(path)?))
    }

    /// Shares `db`.
    pub fn new(db: Database) -> Self {
        let name: Arc<str> = db
            .path()
            .file_stem()
            .and_then(|s| s.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("traverse")
            .into();
        let inner = Arc::new(Inner {
            state: Mutex::new(State {
                db: Some(Arc::new(db)),
                live: 0,
                leases: Vec::new(),
                prune_at: 64,
                busy_timeout: DEFAULT_BUSY_TIMEOUT,
                statement_timeout: None,
                name,
                auto_checkpoint: Some(AutoCheckpoint::default()),
            }),
            idle: Condvar::new(),
            stop: Arc::new(Stop::default()),
        });
        // holds the database only while it saves: dropping every handle still frees it
        let (weak, stop) = (Arc::downgrade(&inner), Arc::clone(&inner.stop));
        if let Err(e) = std::thread::Builder::new()
            .name("traverse-checkpoint".into())
            .spawn(move || checkpoints(weak, stop))
        {
            tracing::warn!("no background saves: the checkpoint thread didn't start: {e}");
        }
        Self { inner }
    }

    /// When the database saves itself (see [`AutoCheckpoint`]); `None`: only on
    /// [`flush`](Self::flush) and [`close`](Self::close).
    pub fn auto_checkpoint(&self) -> Option<AutoCheckpoint> {
        self.inner.lock().auto_checkpoint
    }

    /// See [`auto_checkpoint`](Self::auto_checkpoint).
    pub fn set_auto_checkpoint(&self, policy: Option<AutoCheckpoint>) {
        self.inner.lock().auto_checkpoint = policy;
    }

    /// How long a write waits for the writer lock before failing with [`Busy`](Error::Busy).
    pub fn busy_timeout(&self) -> Duration {
        self.inner.lock().busy_timeout
    }

    /// See [`busy_timeout`](Self::busy_timeout). `Duration::MAX` waits as long as it takes.
    pub fn set_busy_timeout(&self, timeout: Duration) {
        self.inner.lock().busy_timeout = timeout;
    }

    /// How long a statement may run before it stops with a timeout; `None` (the default): no
    /// limit. Applies to statements started afterwards.
    pub fn statement_timeout(&self) -> Option<Duration> {
        self.inner.lock().statement_timeout
    }

    /// See [`statement_timeout`](Self::statement_timeout).
    pub fn set_statement_timeout(&self, timeout: Option<Duration>) {
        self.inner.lock().statement_timeout = timeout;
    }

    /// The database's name: its file stem.
    pub fn name(&self) -> Arc<str> {
        Arc::clone(&self.inner.lock().name)
    }

    /// Whether [`close`](Self::close) ran.
    pub fn is_closed(&self) -> bool {
        self.inner.lock().db.is_none()
    }

    /// Snapshot transaction: sees the database as of now, never blocks, refuses writes.
    pub fn begin_read(&self) -> Result<Lease<Transaction>> {
        let (db, ticket, _) = self.enter()?;
        let tx = db
            .begin_read_owned()
            .map(|tx| Transaction::new(Kind::Read(tx)));
        drop(db);
        self.lease(tx, ticket)
    }

    /// Write transaction: holds the writer lock until it finishes, waiting at most the busy
    /// timeout for it.
    pub fn begin_write(&self) -> Result<Lease<Transaction>> {
        let (db, ticket, timeout) = self.enter()?;
        let tx = db
            .try_begin_write_owned(timeout)
            .map(|tx| Transaction::new(Kind::Write(tx)));
        drop(db);
        self.lease(tx, ticket)
    }

    /// The write transaction of one auto-commit statement: [`begin_write`](Self::begin_write),
    /// except that the statement may commit batches in it (`CALL { ... } IN TRANSACTIONS`), and
    /// that while it waits for the writer lock (at most the busy timeout) `check` runs every 20
    /// ms, so a statement stopped meanwhile gives up at once with `check`'s error.
    pub fn begin_auto_commit<E: From<Error>>(
        &self,
        mut check: impl FnMut() -> std::result::Result<(), E>,
    ) -> std::result::Result<Lease<Transaction>, E> {
        const POLL: Duration = Duration::from_millis(20);
        let (db, ticket, timeout) = self.enter()?;
        // `Duration::MAX`: as long as it takes
        let deadline = Instant::now().checked_add(timeout);
        let tx = loop {
            let wait = deadline.map_or(POLL, |d| {
                d.saturating_duration_since(Instant::now()).min(POLL)
            });
            match db.try_begin_write_owned(wait) {
                Ok(mut tx) => {
                    tx.allow_batch_commits();
                    break Ok(Transaction::new(Kind::Write(tx)));
                }
                Err(Error::Busy(_)) if deadline.is_none_or(|d| Instant::now() < d) => check()?,
                Err(Error::Busy(_)) => break Err(Error::Busy(timeout)),
                Err(e) => break Err(e),
            }
        };
        drop(db);
        Ok(self.lease(tx, ticket)?)
    }

    /// Bulk-load transaction ([`BulkLoadTx`]), waiting at most the busy timeout for the writer
    /// lock.
    pub fn begin_bulk_load(&self) -> Result<Lease<BulkLoadTx>> {
        let (db, ticket, timeout) = self.enter()?;
        let tx = db.begin_bulk_load(Some(timeout));
        drop(db);
        self.lease(tx, ticket)
    }

    /// Saves changes to the file (see [`Database::flush`]), waiting at most the busy timeout for
    /// an active writer to finish.
    pub fn flush(&self) -> Result<()> {
        let (db, _ticket, timeout) = self.enter()?;
        db.try_flush(timeout)
    }

    /// Closes the database: rolls back the transactions still open, waits for begins and saves in
    /// progress, saves, and releases the file. Later calls on the database and on its
    /// transactions fail with [`DatabaseClosed`](Error::DatabaseClosed). Closing twice is a no-op.
    pub fn close(&self) -> Result<()> {
        let (db, leases) = {
            let mut state = self.inner.lock();
            let Some(db) = state.db.take() else {
                return Ok(());
            };
            (db, std::mem::take(&mut state.leases))
        };
        self.inner.stop.set();
        for lease in leases.iter().filter_map(Weak::upgrade) {
            lease.revoke();
        }
        // begins still waiting for the writer lock, and saves in progress, hold the database too:
        // the begins find it closed and roll back
        {
            let mut state = self.inner.lock();
            while state.live > 0 {
                state = self
                    .inner
                    .idle
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
        // every other holder had a ticket, so this is the last handle
        match Arc::try_unwrap(db) {
            Ok(db) => db.close(),
            Err(_) => unreachable!("database still held after every ticket was returned"),
        }
    }

    /// The open database, the ticket that keeps a close waiting for us, and the busy timeout.
    fn enter(&self) -> Result<(Arc<Database>, Ticket, Duration)> {
        let mut state = self.inner.lock();
        let db = Arc::clone(state.db.as_ref().ok_or(Error::DatabaseClosed)?);
        state.live += 1;
        Ok((db, Ticket(Arc::clone(&self.inner)), state.busy_timeout))
    }

    /// Hands `value` out in a lease, unless the database closed while it was being made (then it
    /// rolls back).
    fn lease<T: Send + 'static>(&self, value: Result<T>, ticket: Ticket) -> Result<Lease<T>> {
        let value = value?;
        let mut state = self.inner.lock();
        if state.db.is_none() {
            drop(state);
            drop(value);
            return Err(Error::DatabaseClosed);
        }
        let lease = Lease {
            slot: Arc::new(LeaseSlot {
                state: Mutex::new(Slot::Open(value, ticket)),
            }),
        };
        if state.leases.len() >= state.prune_at {
            state.leases.retain(|l| l.strong_count() > 0);
            state.prune_at = (state.leases.len() * 2).max(64);
        }
        let weak: Weak<LeaseSlot<T>> = Arc::downgrade(&lease.slot);
        state.leases.push(weak);
        Ok(lease)
    }
}

/// The checkpoint thread: every [`CHECKPOINT_POLL`], saves the database when its
/// [`AutoCheckpoint`] says so, until it closes or every handle is gone.
fn checkpoints(inner: Weak<Inner>, stop: Arc<Stop>) {
    // when commits were first seen unsaved
    let mut unsaved_since: Option<Instant> = None;
    while !stop.sleep(CHECKPOINT_POLL) {
        let Some(inner) = inner.upgrade() else {
            return;
        };
        let (db, ticket, policy) = {
            let mut state = inner.lock();
            let Some(db) = state.db.as_ref().map(Arc::clone) else {
                return;
            };
            let Some(policy) = state.auto_checkpoint else {
                unsaved_since = None;
                continue;
            };
            // a close waits for this save like for any other holder
            state.live += 1;
            (db, Ticket(Arc::clone(&inner)), policy)
        };
        drop(inner);

        let unsaved = db.has_unsaved_commits();
        let since = match (unsaved, unsaved_since) {
            (false, _) => None,
            (true, None) => Some(Instant::now()),
            (true, since) => since,
        };
        let waited = since.is_some_and(|t| policy.interval.is_some_and(|i| t.elapsed() >= i));
        if unsaved && (waited || db.unsaved_log_bytes() >= policy.log_bytes) {
            match db.try_flush(CHECKPOINT_BUSY) {
                Ok(()) => {
                    unsaved_since = None;
                    if db.compaction_due(COMPACT_MIN_BYTES)
                        && let Err(e) = db.compact()
                    {
                        tracing::warn!(
                            "background compaction of {} failed: {e}",
                            db.path().display()
                        );
                    }
                }
                // a writer is open: next time
                Err(Error::Busy(_)) => unsaved_since = since,
                Err(e) => {
                    unsaved_since = since;
                    tracing::warn!("background save of {} failed: {e}", db.path().display());
                }
            }
        } else {
            unsaved_since = since;
        }
        drop(db);
        drop(ticket);
    }
}

impl std::fmt::Debug for SharedDatabase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.inner.lock();
        f.debug_struct("SharedDatabase")
            .field("closed", &state.db.is_none())
            .field("live", &state.live)
            .finish()
    }
}

/// A transaction from a [`SharedDatabase`]: a snapshot reader or the writer.
///
/// Statements run through [`statement`](Self::statement). One that fails after changing data
/// leaves the transaction failed, as in Neo4j and PostgreSQL: its partial changes can't be kept,
/// so later statements and [`commit`](Self::commit) are refused with
/// [`Error::TransactionFailed`] and only a rollback ends it.
pub struct Transaction {
    kind: Kind,
    failed: bool,
}

enum Kind {
    /// See [`SharedDatabase::begin_read`].
    Read(OwnedReadTx),
    /// See [`SharedDatabase::begin_write`].
    Write(OwnedWriteTx),
}

impl Transaction {
    fn new(kind: Kind) -> Self {
        Self {
            kind,
            failed: false,
        }
    }

    fn view(&mut self) -> &mut dyn GraphView {
        match &mut self.kind {
            Kind::Read(tx) => tx,
            Kind::Write(tx) => tx,
        }
    }

    /// Runs a statement: `f` gets the graph as this transaction sees it (its own writes
    /// included). Refused once the transaction failed; a statement that fails after changing
    /// data fails the transaction.
    pub fn statement<T, E: From<Error>>(
        &mut self,
        f: impl FnOnce(&mut dyn GraphView) -> std::result::Result<T, E>,
    ) -> std::result::Result<T, E> {
        if self.failed {
            return Err(Error::TransactionFailed.into());
        }
        let view = self.view();
        let before = view.write_count();
        let result = f(view);
        if result.is_err() && self.view().write_count() != before {
            self.failed = true;
        }
        result
    }

    /// Whether writes are refused.
    pub fn is_read_only(&self) -> bool {
        matches!(self.kind, Kind::Read(_))
    }

    /// Whether a statement failed after changing data: only a rollback ends the transaction now.
    pub fn is_failed(&self) -> bool {
        self.failed
    }

    /// Commits a write transaction; ends a read one. A failed transaction is rolled back and
    /// [`Error::TransactionFailed`] returned.
    pub fn commit(self) -> Result<()> {
        if self.failed {
            self.abort();
            return Err(Error::TransactionFailed);
        }
        match self.kind {
            Kind::Read(_) => Ok(()),
            Kind::Write(tx) => tx.commit().map(drop),
        }
    }

    /// Rolls back a write transaction; ends a read one.
    pub fn abort(self) {
        if let Kind::Write(tx) = self.kind {
            tx.abort();
        }
    }
}

/// A transaction (or bulk load) handed out by a [`SharedDatabase`]: open until it finishes or the
/// database closes. Dropping the last handle on an open lease rolls its transaction back.
///
/// `Send + Sync`: calls from several threads take turns.
pub struct Lease<T: Send + 'static> {
    slot: Arc<LeaseSlot<T>>,
}

struct LeaseSlot<T> {
    state: Mutex<Slot<T>>,
}

enum Slot<T> {
    // field order: the value drops before its ticket releases a waiting close
    Open(T, Ticket),
    Finished,
    Revoked,
}

impl<T> LeaseSlot<T> {
    fn lock(&self) -> MutexGuard<'_, Slot<T>> {
        // a call that panicked mid-way leaves the value as it was; rolling it back still works
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn closed_or_finished<T>(slot: &Slot<T>) -> Error {
    match slot {
        Slot::Revoked => Error::DatabaseClosed,
        _ => Error::TransactionFinished,
    }
}

impl<T: Send + 'static> Lease<T> {
    /// Runs `f` on the open value.
    pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> Result<R> {
        let mut slot = self.slot.lock();
        match &mut *slot {
            Slot::Open(value, _) => Ok(f(value)),
            other => Err(closed_or_finished(other)),
        }
    }

    /// Takes the value out and runs `f` on it (commit, abort); the lease is finished after.
    /// A close waits for `f`.
    pub fn finish<R>(&self, f: impl FnOnce(T) -> R) -> Result<R> {
        let mut slot = self.slot.lock();
        match std::mem::replace(&mut *slot, Slot::Finished) {
            Slot::Open(value, ticket) => {
                let out = f(value);
                drop(ticket);
                Ok(out)
            }
            other => {
                let err = closed_or_finished(&other);
                *slot = other;
                Err(err)
            }
        }
    }

    /// Whether the value is still there (not finished, database not closed).
    pub fn is_open(&self) -> bool {
        matches!(*self.slot.lock(), Slot::Open(..))
    }
}

/// A lease a close can take back.
trait Revoke: Send + Sync {
    fn revoke(&self);
}

impl<T: Send> Revoke for LeaseSlot<T> {
    fn revoke(&self) {
        let taken = {
            let mut slot = self.lock();
            match std::mem::replace(&mut *slot, Slot::Revoked) {
                open @ Slot::Open(..) => Some(open),
                done => {
                    // a finished lease stays finished
                    *slot = done;
                    None
                }
            }
        };
        // rolls back, then returns the ticket
        drop(taken);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn node(tx: &mut dyn GraphView, label: &str) {
        tx.create_node(&[label], BTreeMap::new()).unwrap();
    }

    fn count(path: &Path) -> usize {
        Database::open(path).unwrap().graph().node_count()
    }

    fn assert_send_sync<T: Send + Sync>() {}

    /// Commits `n` nodes labelled `label`, one transaction each.
    fn commit_nodes(db: &SharedDatabase, label: &str, n: usize) {
        for _ in 0..n {
            let tx = db.begin_write().unwrap();
            tx.with(|tx| node(tx.view(), label)).unwrap();
            tx.finish(Transaction::commit).unwrap().unwrap();
        }
    }

    /// Waits up to 10 s for `done`.
    fn eventually(done: impl Fn() -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(10);
        while Instant::now() < until {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        done()
    }

    #[test]
    fn the_database_saves_itself_once_its_log_grows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.tvdb");
        let db = SharedDatabase::open(&path).unwrap();
        db.set_auto_checkpoint(Some(AutoCheckpoint {
            log_bytes: 16 << 10,
            interval: None,
        }));
        commit_nodes(&db, "N", 500);
        // only a save writes the file
        assert!(eventually(|| path.exists()), "no background save");
        // the save covered the log: a crash now (no further save) loses nothing
        drop(db);
        assert_eq!(count(&path), 500);
    }

    #[test]
    fn the_database_saves_a_commit_left_unsaved_for_its_interval() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.tvdb");
        let db = SharedDatabase::open(&path).unwrap();
        db.set_auto_checkpoint(Some(AutoCheckpoint {
            log_bytes: u64::MAX,
            interval: Some(Duration::from_millis(300)),
        }));
        commit_nodes(&db, "N", 1);
        assert!(eventually(|| path.exists()), "no background save");
        db.close().unwrap();
        assert_eq!(count(&path), 1);
    }

    #[test]
    fn without_auto_checkpoints_only_flush_and_close_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.tvdb");
        let db = SharedDatabase::open(&path).unwrap();
        db.set_auto_checkpoint(None);
        commit_nodes(&db, "N", 500);
        std::thread::sleep(Duration::from_millis(1_000));
        assert!(!path.exists());
        db.close().unwrap();
        assert_eq!(count(&path), 500);
    }

    #[test]
    fn handles_are_send_and_sync() {
        assert_send_sync::<SharedDatabase>();
        assert_send_sync::<Lease<Transaction>>();
        assert_send_sync::<Lease<BulkLoadTx>>();
    }

    #[test]
    fn close_rolls_back_open_transactions_and_fails_their_later_calls() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.tvdb");
        let db = SharedDatabase::open(&path).unwrap();
        let committed = db.begin_write().unwrap();
        committed.with(|tx| node(tx.view(), "Kept")).unwrap();
        committed.finish(Transaction::commit).unwrap().unwrap();

        let open = db.begin_write().unwrap();
        open.with(|tx| node(tx.view(), "Lost")).unwrap();
        let reader = db.begin_read().unwrap();
        db.close().unwrap();

        assert!(matches!(open.with(|_| ()), Err(Error::DatabaseClosed)));
        assert!(matches!(
            open.finish(Transaction::commit),
            Err(Error::DatabaseClosed)
        ));
        assert!(matches!(reader.with(|_| ()), Err(Error::DatabaseClosed)));
        assert!(matches!(
            committed.with(|_| ()),
            Err(Error::TransactionFinished)
        ));
        assert!(matches!(db.begin_read(), Err(Error::DatabaseClosed)));
        db.close().unwrap();
        // the file is released and holds only the committed node
        assert_eq!(count(&path), 1);
    }

    #[test]
    fn a_statement_that_fails_after_writing_fails_the_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.tvdb");
        let db = SharedDatabase::open(&path).unwrap();
        let tx = db.begin_write().unwrap();
        tx.with(|tx| {
            tx.statement(|v| {
                node(v, "Kept");
                Ok::<_, Error>(())
            })
        })
        .unwrap()
        .unwrap();
        // writes, then fails: its partial change can't be committed
        let failed = tx
            .with(|tx| {
                tx.statement(|v| {
                    node(v, "Partial");
                    Err::<(), _>(Error::NodeNotFound(crate::graph::types::NodeId(999)))
                })
            })
            .unwrap();
        assert!(failed.is_err());
        assert!(tx.with(|tx| tx.is_failed()).unwrap());
        let later = tx
            .with(|tx| {
                tx.statement(|v| {
                    node(v, "Later");
                    Ok::<_, Error>(())
                })
            })
            .unwrap();
        assert!(matches!(later, Err(Error::TransactionFailed)));
        let committed = tx.finish(Transaction::commit).unwrap();
        assert!(matches!(committed, Err(Error::TransactionFailed)));
        db.close().unwrap();
        assert_eq!(count(&path), 0, "a failed transaction is rolled back");
    }

    #[test]
    fn a_statement_that_fails_before_writing_leaves_the_transaction_usable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.tvdb");
        let db = SharedDatabase::open(&path).unwrap();
        let tx = db.begin_write().unwrap();
        let failed = tx
            .with(|tx| tx.statement(|_| Err::<(), _>(Error::ReadOnlyTransaction)))
            .unwrap();
        assert!(failed.is_err());
        tx.with(|tx| {
            tx.statement(|v| {
                node(v, "N");
                Ok::<_, Error>(())
            })
        })
        .unwrap()
        .unwrap();
        tx.finish(Transaction::commit).unwrap().unwrap();
        db.close().unwrap();
        assert_eq!(count(&path), 1);
    }

    #[test]
    fn handles_go_in_any_order_and_the_last_one_saves() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.tvdb");
        let db = SharedDatabase::open(&path).unwrap();
        let tx = db.begin_write().unwrap();
        // the database handle goes first; the transaction keeps the database alive
        drop(db);
        tx.with(|tx| node(tx.view(), "N")).unwrap();
        tx.finish(Transaction::commit).unwrap().unwrap();
        drop(tx);
        assert_eq!(count(&path), 1);
    }

    #[test]
    fn a_second_writer_gets_busy_while_readers_proceed() {
        let dir = tempfile::tempdir().unwrap();
        let db = SharedDatabase::open(dir.path().join("t.tvdb")).unwrap();
        db.set_busy_timeout(Duration::from_millis(20));
        let writer = db.begin_write().unwrap();
        writer.with(|tx| node(tx.view(), "N")).unwrap();
        assert!(matches!(db.begin_write(), Err(Error::Busy(_))));
        assert!(matches!(db.begin_bulk_load(), Err(Error::Busy(_))));
        assert!(matches!(db.flush(), Err(Error::Busy(_))));
        let reader = db.begin_read().unwrap();
        assert_eq!(reader.with(|tx| tx.view().node_count()).unwrap(), 0);
        writer.finish(Transaction::commit).unwrap().unwrap();
        db.begin_write()
            .unwrap()
            .finish(Transaction::abort)
            .unwrap();
        db.flush().unwrap();
    }

    #[test]
    fn a_transaction_finishes_on_another_thread() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.tvdb");
        let db = SharedDatabase::open(&path).unwrap();
        let tx = db.begin_write().unwrap();
        tx.with(|tx| node(tx.view(), "N")).unwrap();
        std::thread::spawn(move || tx.finish(Transaction::commit).unwrap().unwrap())
            .join()
            .unwrap();
        db.close().unwrap();
        assert_eq!(count(&path), 1);
    }

    #[test]
    fn close_waits_out_a_begin_blocked_on_the_writer_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.tvdb");
        let db = Arc::new(SharedDatabase::open(&path).unwrap());
        db.set_busy_timeout(Duration::MAX);
        let holder = db.begin_write().unwrap();
        let waiting = {
            let db = Arc::clone(&db);
            std::thread::spawn(move || db.begin_write().map(drop))
        };
        std::thread::sleep(Duration::from_millis(50));
        // revokes `holder`, which lets the waiter in; the waiter then finds the database closed
        db.close().unwrap();
        assert!(matches!(
            waiting.join().unwrap(),
            Err(Error::DatabaseClosed)
        ));
        assert!(matches!(holder.with(|_| ()), Err(Error::DatabaseClosed)));
        assert_eq!(count(&path), 0, "file released");
    }

    #[test]
    fn close_races_cleanly_with_busy_threads() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.tvdb");
        let db = Arc::new(SharedDatabase::open(&path).unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let workers: Vec<_> = (0..4)
            .map(|i| {
                let (db, stop) = (Arc::clone(&db), Arc::clone(&stop));
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let lease = if i % 2 == 0 {
                            db.begin_write()
                        } else {
                            db.begin_read()
                        };
                        let Ok(lease) = lease else {
                            continue;
                        };
                        let _ = lease.with(|tx| {
                            if !tx.is_read_only() {
                                node(tx.view(), "N");
                            }
                        });
                        let _ = lease.finish(Transaction::commit);
                    }
                })
            })
            .collect();
        std::thread::sleep(Duration::from_millis(100));
        db.close().unwrap();
        stop.store(true, Ordering::Relaxed);
        for w in workers {
            w.join().unwrap();
        }
        // whatever committed before the close is in the file, which is released
        let saved = count(&path);
        assert!(saved > 0, "some writes committed");
    }

    #[test]
    fn bulk_load_is_isolated_and_rolls_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.tvdb");
        let db = SharedDatabase::open(&path).unwrap();
        let bulk = db.begin_bulk_load().unwrap();
        bulk.with(|tx| {
            for _ in 0..100 {
                node(tx, "B");
            }
        })
        .unwrap();
        let reader = db.begin_read().unwrap();
        assert_eq!(reader.with(|tx| tx.view().node_count()).unwrap(), 0);
        bulk.finish(BulkLoadTx::abort).unwrap();
        let after = db.begin_read().unwrap();
        assert_eq!(after.with(|tx| tx.view().node_count()).unwrap(), 0);
        db.close().unwrap();
        assert_eq!(count(&path), 0);
    }
}
