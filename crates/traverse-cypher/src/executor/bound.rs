//! Bound-entity capture, the provenance behind graph views: `CypherResult::bound` answers "which
//! entities did the patterns bind" even when only properties were returned. Bindings die at every
//! projection boundary (ProjectOp builds fresh frames; the fused scan never emits its scan
//! variable), so capture happens where they die.
//!
//! Scoped per query, not per process: concurrent queries (Bolt handlers, parallel tests) each arm
//! their own handle in a thread-local, and the morsel pipeline clones it onto rayon workers like
//! query params and the deadline.

use traverse_core::graph::types::{EdgeId, NodeId};

use crate::frame::{Frame, SlotValue};
use crate::types::CypherValue;

/// Most entities one query captures.
const BOUND_CAP: usize = 1024;

pub(crate) struct BoundState {
    nodes_seen: std::collections::HashSet<NodeId>,
    edges_seen: std::collections::HashSet<EdgeId>,
    out: Vec<CypherValue>,
}

pub(crate) type BoundHandle = std::sync::Arc<std::sync::Mutex<BoundState>>;

thread_local! {
    static BOUND_HANDLE: std::cell::RefCell<Option<BoundHandle>> =
        const { std::cell::RefCell::new(None) };
}

/// Armed capture for one query. `finish()` restores the thread's previous handle
/// (a trigger-fired inner query must not swallow the outer capture) and returns
/// the collected refs.
pub(super) struct BoundGuard {
    handle: BoundHandle,
    prev: Option<BoundHandle>,
}

pub(super) fn arm_bound_capture() -> BoundGuard {
    let handle: BoundHandle = std::sync::Arc::new(std::sync::Mutex::new(BoundState {
        nodes_seen: Default::default(),
        edges_seen: Default::default(),
        out: Vec::new(),
    }));
    let prev = BOUND_HANDLE.with(|h| h.borrow_mut().replace(handle.clone()));
    BoundGuard { handle, prev }
}

impl BoundGuard {
    pub(super) fn finish(self) -> Vec<CypherValue> {
        BOUND_HANDLE.with(|h| *h.borrow_mut() = self.prev.clone());
        let mut st = self.handle.lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut st.out)
    }
}

#[inline]
pub(crate) fn bound_capture_armed() -> bool {
    BOUND_HANDLE.with(|h| h.borrow().is_some())
}

/// Morsel pipeline: copy the query thread's handle onto a worker (cleared after; rayon threads are
/// pooled).
pub(crate) fn clone_bound_handle() -> Option<BoundHandle> {
    BOUND_HANDLE.with(|h| h.borrow().clone())
}

pub(crate) fn set_bound_handle(handle: Option<BoundHandle>) {
    BOUND_HANDLE.with(|h| *h.borrow_mut() = handle);
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

/// Appends every entity binding in `frame` to `batch`.
pub(crate) fn collect_entity_slots(frame: &Frame, batch: &mut Vec<CypherValue>) {
    for sv in frame.iter() {
        match sv {
            SlotValue::NodeRef(id) => batch.push(CypherValue::NodeRef(*id)),
            SlotValue::EdgeRef {
                id,
                edge_type_id,
                source,
                target,
            } => {
                batch.push(CypherValue::EdgeRef {
                    id: *id,
                    edge_type_id: *edge_type_id,
                    source: *source,
                    target: *target,
                });
            }
            SlotValue::SingleHopPath {
                start,
                end,
                edge_id,
                edge_type,
                outgoing,
            } => {
                batch.push(CypherValue::NodeRef(*start));
                batch.push(CypherValue::NodeRef(*end));
                let (source, target) = if *outgoing {
                    (*start, *end)
                } else {
                    (*end, *start)
                };
                batch.push(CypherValue::EdgeRef {
                    id: *edge_id,
                    edge_type_id: *edge_type,
                    source,
                    target,
                });
            }
            SlotValue::Heap(cv) => match &**cv {
                CypherValue::Node { id, .. } | CypherValue::NodeRef(id) => {
                    batch.push(CypherValue::NodeRef(*id));
                }
                CypherValue::EdgeRef { .. } | CypherValue::Edge { .. } => {
                    batch.push((**cv).clone());
                }
                _ => {}
            },
            _ => {}
        }
    }
}
