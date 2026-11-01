//! Garbage collection of version state: cuts delta chains at the oldest open
//! snapshot, drops the runs and ghosts no snapshot needs, frees retired deltas.

use std::sync::atomic::Ordering;

use super::MvccStore;
use super::mvcc::{Delta, TXN_ID_SENTINEL};

/// Cuts a chain at the first delta committed at or before `watermark` (chains are commit-ordered,
/// so everything older is too); `true` if the chain is now empty.
fn cut_chain(head: &std::sync::atomic::AtomicPtr<Delta>, watermark: u64) -> bool {
    let mut prev: *mut Delta = std::ptr::null_mut();
    let mut current = head.load(Ordering::Acquire);
    while !current.is_null() {
        // SAFETY: GC holds the writer and structural write locks: the chain is stable and alive
        let delta = unsafe { &*current };
        let ts = delta.commit_ts();
        if ts != TXN_ID_SENTINEL && ts <= watermark {
            if prev.is_null() {
                head.store(std::ptr::null_mut(), Ordering::Release);
            } else {
                // SAFETY: prev is a live delta we just walked past
                unsafe { &*prev }
                    .next
                    .store(std::ptr::null_mut(), Ordering::Release);
            }
            break;
        }
        prev = current;
        current = delta.next();
    }
    head.load(Ordering::Relaxed).is_null()
}

impl MvccStore {
    /// Collects version state no open snapshot needs and compacts large adjacency overlays.
    /// Takes the writer lock.
    pub fn gc(&self) {
        let _guard = self.write_lock.lock();
        self.collect_locked(true);
        let _rw = self.graph_rw.write();
        // SAFETY: we hold the writer lock and the structural write lock
        unsafe { self.graph_mut() }.compact_adjacency();
    }

    /// Cheap collection after a commit (writer lock held): everything when no snapshot is open,
    /// else only once the oldest snapshot has moved since the last collection.
    pub(crate) fn collect_after_commit(&self) {
        self.collect_locked(false);
    }

    /// Collects with the writer lock held. `force`: collect even if the watermark hasn't moved.
    pub(crate) fn collect_locked(&self, force: bool) {
        // visible timestamp FIRST, then the slots (SeqCst both): a reader the scan misses re-reads
        // a visible timestamp at least this one and needs nothing older
        let visible = self.visible_commit();
        let oldest = self.reader_registry.min_encoded();
        let _rw = self.graph_rw.write();
        // SAFETY: writer lock + structural write lock
        let graph = unsafe { self.graph_mut() };
        let everything_visible = visible == self.last_commit_ts.load(Ordering::SeqCst);
        if oldest.is_none() && everything_visible {
            // no snapshot open and none can start below the newest commit: drop it all
            graph.node_delta_heads.clear();
            graph.edge_delta_heads.clear();
            graph.versions_mut().clear();
            self.collected_up_to.store(visible, Ordering::Relaxed);
            return;
        }
        // new snapshots start at `visible` (commits past it aren't durable yet), open ones at
        // `oldest`
        let watermark = oldest.map_or(visible, |o| o.min(visible));
        if !force && watermark <= self.collected_up_to.load(Ordering::Relaxed) {
            return;
        }
        graph
            .node_delta_heads
            .retain(|_, head| !cut_chain(head, watermark));
        graph
            .edge_delta_heads
            .retain(|_, head| !cut_chain(head, watermark));
        graph.versions_mut().collect(watermark);
        self.collected_up_to.store(watermark, Ordering::Relaxed);
    }
}
