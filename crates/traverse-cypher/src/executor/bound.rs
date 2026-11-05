//! Bound-entity capture, the provenance behind graph views: `CypherResult::bound` answers "which
//! entities did the patterns bind" even when only properties were returned. Bindings die at every
//! projection boundary, so a capture operator below each one records them.
//!
//! Scoped per query, not per process: concurrent queries (Bolt handlers, parallel tests) each arm
//! their own handle in a thread-local, and the morsel pipeline clones it onto rayon workers like
//! query params and the deadline.

use traverse_core::graph::types::{EdgeId, NodeId};

use crate::types::CypherValue;

/// Most entities one query captures.
const BOUND_CAP: usize = 1024;

pub(crate) struct BoundState {
    // FxHash: ids are plain integers, and every captured row hashes one
    nodes_seen: rustc_hash::FxHashSet<NodeId>,
    edges_seen: rustc_hash::FxHashSet<EdgeId>,
    out: Vec<CypherValue>,
}

pub(crate) type BoundHandle = std::sync::Arc<std::sync::Mutex<BoundState>>;

thread_local! {
    static BOUND_HANDLE: std::cell::RefCell<Option<BoundHandle>> =
        const { std::cell::RefCell::new(None) };
    /// The capture reached its cap on this thread: no more scanning.
    static BOUND_FULL: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Armed capture for one query. `finish()` restores the thread's previous handle
/// (a trigger-fired inner query must not swallow the outer capture) and returns
/// the collected refs.
pub(crate) struct BoundGuard {
    handle: BoundHandle,
    prev: Option<BoundHandle>,
}

pub(crate) fn arm_bound_capture() -> BoundGuard {
    let handle: BoundHandle = std::sync::Arc::new(std::sync::Mutex::new(BoundState {
        nodes_seen: Default::default(),
        edges_seen: Default::default(),
        out: Vec::new(),
    }));
    let prev = BOUND_HANDLE.with(|h| h.borrow_mut().replace(handle.clone()));
    BOUND_FULL.set(false);
    BoundGuard { handle, prev }
}

impl BoundGuard {
    pub(crate) fn finish(self) -> Vec<CypherValue> {
        BOUND_HANDLE.with(|h| *h.borrow_mut() = self.prev.clone());
        BOUND_FULL.set(false);
        let mut st = self.handle.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut st.out)
    }
}

/// Is a capture armed on this thread and still taking entities?
#[inline]
pub(crate) fn bound_capture_armed() -> bool {
    !BOUND_FULL.get() && BOUND_HANDLE.with(|h| h.borrow().is_some())
}

/// Morsel pipeline: copy the query thread's handle onto a worker (cleared after; rayon threads are
/// pooled).
pub(crate) fn clone_bound_handle() -> Option<BoundHandle> {
    BOUND_HANDLE.with(|h| h.borrow().clone())
}

pub(crate) fn set_bound_handle(handle: Option<BoundHandle>) {
    BOUND_HANDLE.with(|h| *h.borrow_mut() = handle);
    BOUND_FULL.set(false);
}

/// Merges a batch of entity refs into the query's capture (distinct, capped). Batched so it stays
/// off the hot path: one lock per operator or morsel, never per frame.
pub(crate) fn flush_bound(batch: Vec<CypherValue>) {
    if batch.is_empty() {
        return;
    }
    let Some(handle) = clone_bound_handle() else {
        return;
    };
    let mut st = handle.lock().unwrap_or_else(|e| e.into_inner());
    for cv in batch {
        if st.out.len() >= BOUND_CAP {
            BOUND_FULL.set(true);
            break;
        }
        let fresh = match &cv {
            CypherValue::NodeRef(id) => st.nodes_seen.insert(*id),
            CypherValue::EdgeRef { id, .. } | CypherValue::Edge { id, .. } => {
                st.edges_seen.insert(*id)
            }
            _ => false,
        };
        if fresh {
            st.out.push(cv);
        }
    }
}
