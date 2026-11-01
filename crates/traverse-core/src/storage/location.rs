//! Entity ID -> on-disk page/slot index.

use rustc_hash::FxHashMap;

use crate::graph::types::{EdgeId, NodeId};

/// On-disk location of a record in a slotted page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordLocation {
    /// Page holding the record.
    pub page_id: u64,
    /// Slot in the page.
    pub slot_idx: u16,
}

/// Entity ID -> on-disk record location.
#[derive(Debug, Default)]
pub struct LocationIndex {
    nodes: FxHashMap<NodeId, RecordLocation>,
    edges: FxHashMap<EdgeId, RecordLocation>,
}

impl LocationIndex {
    /// Empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Index from pre-built maps.
    pub fn from_maps(
        nodes: FxHashMap<NodeId, RecordLocation>,
        edges: FxHashMap<EdgeId, RecordLocation>,
    ) -> Self {
        Self { nodes, edges }
    }

    /// Sets a node's location.
    pub fn set_node(&mut self, id: NodeId, loc: RecordLocation) {
        self.nodes.insert(id, loc);
    }

    /// Sets an edge's location.
    pub fn set_edge(&mut self, id: EdgeId, loc: RecordLocation) {
        self.edges.insert(id, loc);
    }

    /// Node location, if known.
    pub fn get_node(&self, id: NodeId) -> Option<RecordLocation> {
        self.nodes.get(&id).copied()
    }

    /// Edge location, if known.
    pub fn get_edge(&self, id: EdgeId) -> Option<RecordLocation> {
        self.edges.get(&id).copied()
    }

    /// Removes and returns a node's location.
    pub fn remove_node(&mut self, id: NodeId) -> Option<RecordLocation> {
        self.nodes.remove(&id)
    }

    /// Removes and returns an edge's location.
    pub fn remove_edge(&mut self, id: EdgeId) -> Option<RecordLocation> {
        self.edges.remove(&id)
    }

    /// Clears everything.
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.edges.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_and_get_node() {
        let mut idx = LocationIndex::new();
        let loc = RecordLocation {
            page_id: 1,
            slot_idx: 3,
        };
        idx.set_node(NodeId(42), loc);
        assert_eq!(idx.get_node(NodeId(42)), Some(loc));
        assert_eq!(idx.get_node(NodeId(99)), None);
    }

    #[test]
    fn set_and_get_edge() {
        let mut idx = LocationIndex::new();
        let loc = RecordLocation {
            page_id: 2,
            slot_idx: 0,
        };
        idx.set_edge(EdgeId(7), loc);
        assert_eq!(idx.get_edge(EdgeId(7)), Some(loc));
    }

    #[test]
    fn remove_node() {
        let mut idx = LocationIndex::new();
        let loc = RecordLocation {
            page_id: 0,
            slot_idx: 1,
        };
        idx.set_node(NodeId(1), loc);
        assert_eq!(idx.remove_node(NodeId(1)), Some(loc));
        assert_eq!(idx.get_node(NodeId(1)), None);
    }

    #[test]
    fn clear_removes_all() {
        let mut idx = LocationIndex::new();
        idx.set_node(
            NodeId(0),
            RecordLocation {
                page_id: 0,
                slot_idx: 0,
            },
        );
        idx.set_edge(
            EdgeId(0),
            RecordLocation {
                page_id: 1,
                slot_idx: 0,
            },
        );
        idx.clear();
        assert_eq!(idx.get_node(NodeId(0)), None);
        assert_eq!(idx.get_edge(EdgeId(0)), None);
    }
}
