//! Transactions that own a handle on what they borrow, for sessions no borrow can span: server
//! connections, language bindings, the C ABI.
//!
//! Each wraps a borrowing transaction whose lifetime is erased to `'static` next to an [`Anchor`]
//! keeping the borrowed store alive. Sound because the anchor drops after the transaction (field
//! order) and the `'static` lifetime never escapes: everything below borrows through `&self` /
//! `&mut self`, and none of the wrapped types hands out a reference tied to its own lifetime
//! parameter.

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;

use super::MvccStore;
use super::optimistic::OptimisticTx;
use super::read::ReadTx;
use super::timestamp::HybridTimestamp;
use super::view::GraphView;
use super::write::WriteTx;
use crate::error::Result;
#[cfg(not(target_arch = "wasm32"))]
use crate::sync::WriterGuard;

/// Keeps a transaction's store alive: the store itself, or whatever owns it (a `Database`).
pub(crate) type Anchor = Arc<dyn Any + Send + Sync>;

/// [`ReadTx`] that owns a handle on its store.
pub struct OwnedReadTx {
    // field order: the transaction (and its snapshot registration) drops before the anchor
    tx: ReadTx<'static>,
    _anchor: Anchor,
}

impl OwnedReadTx {
    /// Wraps `tx`.
    ///
    /// # Safety
    ///
    /// `tx` borrows only from inside `anchor`'s allocation.
    pub(crate) unsafe fn new(tx: ReadTx<'_>, anchor: Anchor) -> Self {
        // SAFETY: the caller guarantees `anchor` owns what `tx` borrows; see the module docs
        let tx = unsafe { std::mem::transmute::<ReadTx<'_>, ReadTx<'static>>(tx) };
        Self {
            tx,
            _anchor: anchor,
        }
    }

    /// See [`ReadTx::snapshot_ts`].
    pub fn snapshot_ts(&self) -> HybridTimestamp {
        self.tx.snapshot_ts()
    }
}

impl GraphView for OwnedReadTx {
    fn as_sync_ref(&self) -> Option<&(dyn GraphView + Sync)> {
        self.tx.as_sync_ref()
    }

    crate::forward_graph_view_statements!(self => &self.tx, &mut self.tx);
    crate::forward_graph_view_reads!(self => &self.tx);
    crate::forward_graph_view_writes!(self => &mut self.tx);
}

/// [`WriteTx`] that owns a handle on its store. `Send`: it may finish on another thread than the
/// one that began it.
pub struct OwnedWriteTx {
    // field order: the transaction (and its writer lock) drops before the anchor
    tx: WriteTx<'static>,
    _anchor: Anchor,
}

impl OwnedWriteTx {
    /// Wraps `tx`.
    ///
    /// # Safety
    ///
    /// `tx` borrows only from inside `anchor`'s allocation.
    pub(crate) unsafe fn new(tx: WriteTx<'_>, anchor: Anchor) -> Self {
        // SAFETY: the caller guarantees `anchor` owns what `tx` borrows; see the module docs
        let tx = unsafe { std::mem::transmute::<WriteTx<'_>, WriteTx<'static>>(tx) };
        Self {
            tx,
            _anchor: anchor,
        }
    }

    /// See [`WriteTx::commit`].
    pub fn commit(self) -> Result<HybridTimestamp> {
        let Self { tx, _anchor } = self;
        tx.commit()
    }

    /// See [`WriteTx::abort`].
    pub fn abort(self) {
        let Self { tx, _anchor } = self;
        tx.abort();
    }

    /// See [`WriteTx::begin_ts`].
    pub fn begin_ts(&self) -> HybridTimestamp {
        self.tx.begin_ts()
    }

    /// See [`WriteTx::allow_batch_commits`].
    pub fn allow_batch_commits(&mut self) {
        self.tx.allow_batch_commits();
    }

    /// The writer lock this transaction holds.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn writer_guard(&self) -> &WriterGuard<'_> {
        self.tx.writer_guard()
    }

    /// See [`WriteTx::set_skip_redo`].
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn set_skip_redo(&mut self) {
        self.tx.set_skip_redo();
    }

    /// See [`WriteTx::commit_snapshotted`].
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn commit_snapshotted(self) -> HybridTimestamp {
        let Self { tx, _anchor } = self;
        tx.commit_snapshotted()
    }
}

impl GraphView for OwnedWriteTx {
    fn as_sync_ref(&self) -> Option<&(dyn GraphView + Sync)> {
        self.tx.as_sync_ref()
    }

    crate::forward_graph_view_statements!(self => &self.tx, &mut self.tx);
    crate::forward_graph_view_reads!(self => &self.tx);
    crate::forward_graph_view_writes!(self => &mut self.tx);
}

/// [`OptimisticTx`] that owns a handle on its store.
pub struct OwnedOptimisticTx {
    // field order: the transaction (and its snapshot registration) drops before the anchor
    tx: OptimisticTx<'static>,
    _anchor: Anchor,
}

impl OwnedOptimisticTx {
    /// Begins on `store` (read-only refuses every write).
    pub fn begin(store: &Arc<MvccStore>, read_only: bool) -> Result<Self> {
        store.begin_optimistic_owned(read_only)
    }

    /// Wraps `tx`.
    ///
    /// # Safety
    ///
    /// `tx` borrows only from inside `anchor`'s allocation.
    pub(crate) unsafe fn new(tx: OptimisticTx<'_>, anchor: Anchor) -> Self {
        // SAFETY: the caller guarantees `anchor` owns what `tx` borrows; see the module docs
        let tx = unsafe { std::mem::transmute::<OptimisticTx<'_>, OptimisticTx<'static>>(tx) };
        Self {
            tx,
            _anchor: anchor,
        }
    }

    /// See [`OptimisticTx::commit`].
    pub fn commit(self) -> Result<HybridTimestamp> {
        let Self { tx, _anchor } = self;
        tx.commit()
    }

    /// See [`OptimisticTx::snapshot_ts`].
    pub fn snapshot_ts(&self) -> HybridTimestamp {
        self.tx.snapshot_ts()
    }

    /// See [`OptimisticTx::is_read_only`].
    pub fn is_read_only(&self) -> bool {
        self.tx.is_read_only()
    }

    /// See [`OptimisticTx::has_writes`].
    pub fn has_writes(&self) -> bool {
        self.tx.has_writes()
    }
}

impl GraphView for OwnedOptimisticTx {
    fn as_sync_ref(&self) -> Option<&(dyn GraphView + Sync)> {
        self.tx.as_sync_ref()
    }

    crate::forward_graph_view_statements!(self => &self.tx, &mut self.tx);
    crate::forward_graph_view_reads!(self => &self.tx);
    crate::forward_graph_view_writes!(self => &mut self.tx);
}

impl MvccStore {
    /// [`begin_read`](Self::begin_read) holding the `Arc`.
    pub fn begin_read_owned(self: &Arc<Self>) -> Result<OwnedReadTx> {
        let tx = self.begin_read()?;
        // SAFETY: `tx` borrows `**self`, which the anchor keeps alive
        Ok(unsafe { OwnedReadTx::new(tx, Arc::clone(self) as Anchor) })
    }

    /// [`begin_write`](Self::begin_write) holding the `Arc`.
    pub fn begin_write_owned(self: &Arc<Self>) -> OwnedWriteTx {
        let tx = self.begin_write();
        // SAFETY: as in `begin_read_owned`
        unsafe { OwnedWriteTx::new(tx, Arc::clone(self) as Anchor) }
    }

    /// [`try_begin_write`](Self::try_begin_write) holding the `Arc`.
    pub fn try_begin_write_owned(self: &Arc<Self>, timeout: Duration) -> Result<OwnedWriteTx> {
        let tx = self.try_begin_write(timeout)?;
        // SAFETY: as in `begin_read_owned`
        Ok(unsafe { OwnedWriteTx::new(tx, Arc::clone(self) as Anchor) })
    }

    /// [`begin_optimistic`](Self::begin_optimistic) holding the `Arc`.
    pub fn begin_optimistic_owned(self: &Arc<Self>, read_only: bool) -> Result<OwnedOptimisticTx> {
        let tx = self.begin_optimistic(read_only)?;
        // SAFETY: as in `begin_read_owned`
        Ok(unsafe { OwnedOptimisticTx::new(tx, Arc::clone(self) as Anchor) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::Graph;
    use std::collections::BTreeMap;

    fn assert_send<T: Send>() {}

    #[test]
    fn owned_transactions_are_send() {
        assert_send::<OwnedReadTx>();
        assert_send::<OwnedWriteTx>();
        assert_send::<OwnedOptimisticTx>();
    }

    #[test]
    fn owned_write_commits_on_another_thread_and_outlives_the_callers_handle() {
        let store = Arc::new(MvccStore::new(Graph::new()));
        let mut tx = store.begin_write_owned();
        tx.create_node(&["N"], BTreeMap::new()).unwrap();
        let reader = store.begin_read_owned().unwrap();
        // the only handle left on the store is inside the transactions
        drop(store);
        std::thread::spawn(move || tx.commit().unwrap())
            .join()
            .unwrap();
        assert_eq!(reader.node_count(), 0, "snapshot from before the commit");
    }

    #[test]
    fn try_begin_write_reports_busy() {
        let store = Arc::new(MvccStore::new(Graph::new()));
        let held = store.begin_write_owned();
        let err = store
            .try_begin_write_owned(Duration::from_millis(10))
            .err()
            .expect("writer lock is held");
        assert!(matches!(err, crate::error::Error::Busy(_)), "{err}");
        held.abort();
        store
            .try_begin_write_owned(Duration::from_millis(10))
            .unwrap()
            .abort();
    }
}
