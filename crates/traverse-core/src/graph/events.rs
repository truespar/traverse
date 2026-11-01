//! Graph mutation events: the capture layer behind `apoc.trigger.*`.
//!
//! Capture is THREAD-LOCAL and armed by the query layer only while a trigger is
//! installed; otherwise free (every hook first checks a plain `Cell<bool>`).
//!
//! Event sources (one choke point each):
//! * `Graph::create_node` / `create_edge`: creations.
//! * `Graph::delete_node(_cascading)` / `delete_edge`: deletions, with a
//!   PRE-DELETE snapshot (labels + properties) so triggers can still inspect the
//!   entity as a virtual value.
//! * `Graph::add_label` / `remove_label`: label changes.
//! * The `unindex_node -> mutate -> index_node` bracket around every in-place
//!   property write: `unindex` snapshots the map, `index` diffs and emits
//!   per-property (old, new) events. Covers ALL property writes (SET, REMOVE,
//!   `+=`, APOC setters, atomics, merges) without instrumenting each.

use std::cell::{Cell, RefCell};

use super::property::PropertyValue;
use super::types::{EdgeId, NodeId};

/// One captured mutation.
#[derive(Debug, Clone)]
pub enum GraphEvent {
    /// A node was created.
    NodeCreated(NodeId),
    /// An edge was created.
    EdgeCreated(EdgeId),
    /// Node deleted; labels + properties are the pre-delete snapshot.
    NodeDeleted {
        /// Deleted node id.
        id: NodeId,
        /// Labels at deletion time.
        labels: Vec<String>,
        /// Properties at deletion time.
        properties: Vec<(String, PropertyValue)>,
    },
    /// Edge deleted; fields are the pre-delete snapshot.
    EdgeDeleted {
        /// Deleted edge id.
        id: EdgeId,
        /// Type name at deletion.
        edge_type: String,
        /// Source node.
        source: NodeId,
        /// Target node.
        target: NodeId,
        /// Properties at deletion time.
        properties: Vec<(String, PropertyValue)>,
    },
    /// Label added to a node.
    LabelAdded {
        /// The node.
        node: NodeId,
        /// The label name.
        label: String,
    },
    /// Label removed from a node.
    LabelRemoved {
        /// The node.
        node: NodeId,
        /// The label name.
        label: String,
    },
    /// Node property changed (old = None: newly set; new = None: removed).
    NodePropChanged {
        /// The node.
        node: NodeId,
        /// Property key.
        key: String,
        /// Previous value, if any.
        old: Option<PropertyValue>,
        /// New value, if any.
        new: Option<PropertyValue>,
    },
    /// Edge property changed.
    EdgePropChanged {
        /// The edge.
        edge: EdgeId,
        /// Property key.
        key: String,
        /// Previous value, if any.
        old: Option<PropertyValue>,
        /// New value, if any.
        new: Option<PropertyValue>,
    },
}

thread_local! {
    static CAPTURING: Cell<bool> = const { Cell::new(false) };
    static LOG: RefCell<Vec<GraphEvent>> = const { RefCell::new(Vec::new()) };
    /// Pre-mutation property snapshots by entity id, taken at `unindex_*` while capturing (diffed +
    /// cleared at `index_*`).
    static NODE_SNAPSHOTS: RefCell<Vec<(NodeId, Vec<(String, PropertyValue)>)>> =
        const { RefCell::new(Vec::new()) };
    static EDGE_SNAPSHOTS: RefCell<Vec<(EdgeId, Vec<(String, PropertyValue)>)>> =
        const { RefCell::new(Vec::new()) };
}

/// Arms/disarms capture on this thread; returns the previous state.
pub fn set_capture(on: bool) -> bool {
    CAPTURING.with(|c| c.replace(on))
}

/// Capture armed? Cheap; hooks check it first.
#[inline]
pub fn capturing() -> bool {
    CAPTURING.with(Cell::get)
}

/// Records an event (no-op unless capturing).
#[inline]
pub fn record(ev: GraphEvent) {
    if capturing() {
        LOG.with(|l| l.borrow_mut().push(ev));
    }
}

/// Takes all captured events, clearing the log and pending snapshots.
pub fn drain() -> Vec<GraphEvent> {
    NODE_SNAPSHOTS.with(|s| s.borrow_mut().clear());
    EDGE_SNAPSHOTS.with(|s| s.borrow_mut().clear());
    LOG.with(|l| std::mem::take(&mut *l.borrow_mut()))
}

/// Stores a node's pre-mutation snapshot (from `unindex_node` while capturing).
pub fn snapshot_node(id: NodeId, props: Vec<(String, PropertyValue)>) {
    NODE_SNAPSHOTS.with(|s| s.borrow_mut().push((id, props)));
}

/// Diffs a node's properties against its snapshot, emitting per-property events (from `index_node`
/// while capturing).
pub fn diff_node(id: NodeId, current: Vec<(String, PropertyValue)>) {
    let before = NODE_SNAPSHOTS.with(|s| {
        let mut v = s.borrow_mut();
        v.iter()
            .rposition(|(nid, _)| *nid == id)
            .map(|i| v.remove(i).1)
    });
    let Some(before) = before else { return };
    emit_prop_diffs(before, current, |key, old, new| {
        GraphEvent::NodePropChanged {
            node: id,
            key,
            old,
            new,
        }
    });
}

/// [`snapshot_node`] for edges.
pub fn snapshot_edge(id: EdgeId, props: Vec<(String, PropertyValue)>) {
    EDGE_SNAPSHOTS.with(|s| s.borrow_mut().push((id, props)));
}

/// [`diff_node`] for edges.
pub fn diff_edge(id: EdgeId, current: Vec<(String, PropertyValue)>) {
    let before = EDGE_SNAPSHOTS.with(|s| {
        let mut v = s.borrow_mut();
        v.iter()
            .rposition(|(eid, _)| *eid == id)
            .map(|i| v.remove(i).1)
    });
    let Some(before) = before else { return };
    emit_prop_diffs(before, current, |key, old, new| {
        GraphEvent::EdgePropChanged {
            edge: id,
            key,
            old,
            new,
        }
    });
}

fn emit_prop_diffs(
    before: Vec<(String, PropertyValue)>,
    after: Vec<(String, PropertyValue)>,
    mk: impl Fn(String, Option<PropertyValue>, Option<PropertyValue>) -> GraphEvent,
) {
    for (k, new_v) in &after {
        match before.iter().find(|(bk, _)| bk == k) {
            None => record(mk(k.clone(), None, Some(new_v.clone()))),
            Some((_, old_v)) if old_v != new_v => {
                record(mk(k.clone(), Some(old_v.clone()), Some(new_v.clone())));
            }
            _ => {}
        }
    }
    for (k, old_v) in &before {
        if !after.iter().any(|(ak, _)| ak == k) {
            record(mk(k.clone(), Some(old_v.clone()), None));
        }
    }
}
